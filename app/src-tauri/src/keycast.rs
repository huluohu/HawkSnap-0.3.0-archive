//! 按键显示（keycast，M3 Task N4）：录制期间的键盘组合键悬浮显示。
//!
//! ## 链路
//!
//! 悬浮条开关 → [`keycast_set`]（仅录制中可开）→ CGEventTap **listen-only** 键盘
//! tap（专用线程 + 私有 CFRunLoop，录屏会话维度启停）→ keyDown 解析 keycode+flags →
//! [`format_key_event`]（修饰符序 Cmd⇧⌥⌃ + 键名 [`keycode_display_name`]）→ 100ms 同
//! 组合合并节流（[`ComboThrottle`]：**滑窗**语义——同组合的连续重复合并为一次显示
//! 刷新；长按自动重复期间只显示一次，停手后再次按下才重新显示）→ `hawk://keycast`
//! {combo, ts} → keycast 小窗（320×88 透明、点击穿透、**不可获 key 焦点**、底部
//! 居中）大字显示 + 1.5s CSS 淡出。
//!
//! ## Tap API 选型（任务裁定）
//!
//! **CGEventTap（CoreGraphics 裸 extern，ax.rs 同款）而非 NSEvent 全局监听**：
//! NSEvent 全局监听对按键类事件（keyDown/keyUp）**不投递其他应用**的按键（只有
//! systemDefined 等特殊事件流例外——媒体键路径正是利用该例外，见 overlay/hotkey.rs
//! 选型注）；CGEventTap listen-only + kCGSessionEventTap 是监听全局键盘的**正典
//! API**，辅助功能授权（本应用既有门槛：热键/悬停/媒体键同款）下可靠工作。
//! listen-only 保证 tap 绝不吞改事件——F6 停止热键照常触发，keycast 只是旁路旁观
//! （顺带把 F6 按键也如实显示，诚实语义）。修饰符**单独**按下（flagsChanged）
//! M3 不显示——只显示完整组合键（keyDown 时刻的 flags 快照），flagsChanged 不装
//! 掩码、行为有据可查。**修饰键自身**的 keyDown 会显示其符号**一次**（旗标快照
//! 已含该修饰符 → 不重复打印键名，见 [`format_key_event`]）。
//!
//! ## 生命周期与泄漏守卫
//!
//! tap **只在「录制进行中 AND 开关打开」**存活：开关关闭（[`keycast_set`] false）与
//! 录制停止（recording.rs 的 record_stop 收口调用 [`keycast_disable`]）都会停 tap
//! 线程 + 移除 runloop source + `CFMachPortInvalidate` 销毁 tap——绝不残留系统级
//! 监听。小窗随开关懒建/关（与悬浮条 bar-close 同款模式，webview 不常驻）。
//! 启停全路径经 [`LIFECYCLE`] 锁串行（spawn 竞态互斥 + TAP_HANDLER 单槽一致性）。
//!
//! ## 权限
//!
//! 辅助功能未授予 → [`keycast_set`] 返回 Err（中文文案，悬浮条瞬态错误显示，与
//! pip_update 的 Err 回流同款）；CGEventTapCreate 失败（NULL）同路径降级。
//! 录制主链路零依赖 keycast——任何失败都不影响录制。
//!
//! ## 小窗的 ACL 与焦点纪律（评审修复）
//!
//! - **ACL 按窗口标签授权**：keycast 小窗要收 `hawk://keycast`（JS `listen`），
//!   必须有覆盖标签 `keycast` 的 capability（`app/src-tauri/capabilities/keycast.json`
//!   ——`core:default` → `core:event:default` → `allow-listen`）。缺了它 listen 的
//!   promise 在运行时被 ACL 拒绝 → 窗口永远收不到事件（Rust 侧 tap 正常、窗口也
//!   开着，但小窗空白）。守卫见 `app/src-tauri/tests/keycast_acl.rs`（走真实编译期
//!   ACL 的 `resolve_access` 判定，非「文件存在」弱断言）。
//! - **绝不获 key 焦点**：非交互浮层不得吞用户按键 → 建窗 `focusable(false)`
//!   （tao 覆写 `canBecomeKeyWindow` → 本窗永不能成为 key window）+ `focused(false)`；
//!   `set_ignore_cursor_events` 只管鼠标、键盘靠 focusable 兜。实证见
//!   `src/bin/keycast-focus-probe.rs`（可见 + `canBecomeKeyWindow=false`）。

// —— 非 macOS 平台放行约定（M4 P0 评审修复轮）——
//
// 本文件含 macOS 专属能力（录屏 / 画中画 / 贴图 / 按键显示 / 长截图 等）。非 macOS
// 构建下这些项**无人调用**——这是「能力缺席」的正常结果，不是代码缺陷。故按项逐个
// 放行：`#[cfg_attr(not(target_os = "macos"), allow(dead_code))]`（仅 macOS 分支才用
// 的局部变量用 `unused_variables`，仅 macOS 才用的导入用 `unused_imports`）。
//
// **逐项**放行而非 crate 级 blanket allow：crate 级 `allow(dead_code, unused_*)`
// 会把**未来新增**的同类问题一并吞掉；逐项放行后，新增项若在非 macOS 上真的无人用，
// 依然照报。macOS 构建下这些属性恒不生效（`not(target_os = "macos")` 为假）——
// macOS 的 lint 纪律零变化。

use serde::Serialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};
#[cfg_attr(not(target_os = "macos"), allow(unused_imports))]
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

// ---------------------------------------------------------------------------
// 纯逻辑（可测）：键名表 + 组合格式化 + flags 解析 + 节流 + 生命周期决策
// ---------------------------------------------------------------------------

/// macOS 虚拟键码 → 显示名（通用符号，无 i18n——键名本就跨语言）。表覆盖：字母
/// A-Z、数字 0-9、F1-F12、空格/回车/Tab/Esc/删除/前删、四向箭头、Home/End/
/// PageUp/PageDown、常用标点、修饰键本体（⌘⇧⌥⌃⇪fn，keyDown 形式到达时如实显示）、
/// 音量三键。**未知键码 → "Key{code}"**（F 键区段外与媒体键优雅兜底——契约裁定）。
/// 纯函数：单 match 表驱动，测试见 keycast_tests。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn keycode_display_name(code: u16) -> String {
    let named: Option<&str> = match code {
        // 字母（kVK_ANSI_A..Z；键码非字母序，QWERTY 物理布局）
        0x00 => Some("A"),
        0x01 => Some("S"),
        0x02 => Some("D"),
        0x03 => Some("F"),
        0x04 => Some("H"),
        0x05 => Some("G"),
        0x06 => Some("Z"),
        0x07 => Some("X"),
        0x08 => Some("C"),
        0x09 => Some("V"),
        0x0B => Some("B"),
        0x0C => Some("Q"),
        0x0D => Some("W"),
        0x0E => Some("E"),
        0x0F => Some("R"),
        0x10 => Some("Y"),
        0x11 => Some("T"),
        0x1F => Some("O"),
        0x20 => Some("U"),
        0x22 => Some("I"),
        0x23 => Some("P"),
        0x25 => Some("L"),
        0x26 => Some("J"),
        0x28 => Some("K"),
        0x2D => Some("N"),
        0x2E => Some("M"),
        // 数字行
        0x12 => Some("1"),
        0x13 => Some("2"),
        0x14 => Some("3"),
        0x15 => Some("4"),
        0x16 => Some("6"),
        0x17 => Some("5"),
        0x19 => Some("9"),
        0x1A => Some("7"),
        0x1C => Some("8"),
        0x1D => Some("0"),
        // F 键（kVK_F1=0x7A … kVK_F12=0x6F，非连续物理段）
        0x7A => Some("F1"),
        0x78 => Some("F2"),
        0x63 => Some("F3"),
        0x76 => Some("F4"),
        0x60 => Some("F5"),
        0x61 => Some("F6"),
        0x62 => Some("F7"),
        0x64 => Some("F8"),
        0x65 => Some("F9"),
        0x6D => Some("F10"),
        0x67 => Some("F11"),
        0x6F => Some("F12"),
        // 控制键（通用符号）
        0x24 => Some("↩"), // return
        0x30 => Some("⇥"), // tab
        0x31 => Some("␣"), // space（␣=显式空格符号，避免「看不见的显示」）
        0x35 => Some("⎋"), // escape
        0x33 => Some("⌫"), // delete（退格）
        0x75 => Some("⌦"), // forward delete
        0x7B => Some("←"),
        0x7C => Some("→"),
        0x7D => Some("↓"),
        0x7E => Some("↑"),
        0x73 => Some("↖"), // home
        0x77 => Some("↘"), // end
        0x74 => Some("⇞"), // page up
        0x79 => Some("⇟"), // page down
        // 标点（US 布局刻字）
        0x18 => Some("="),
        0x1B => Some("-"),
        0x21 => Some("["),
        0x1E => Some("]"),
        0x29 => Some(";"),
        0x27 => Some("'"),
        0x2B => Some(","),
        0x2F => Some("."),
        0x2C => Some("/"),
        0x2A => Some("\\"),
        0x32 => Some("`"),
        // 修饰键本体（正常经 flagsChanged 而非 keyDown 到达——M3 不显示修饰符单独
        // 按下；此处兜底映射，个別键盘/驱动以 keyDown 形式投递也显示合理符号）
        0x37 | 0x36 => Some("⌘"), // command / right command
        0x38 | 0x3C => Some("⇧"), // shift / right shift
        0x3A | 0x3D => Some("⌥"), // option / right option
        0x3B | 0x3E => Some("⌃"), // control / right control
        0x39 => Some("⇪"),        // caps lock
        0x3F => Some("fn"),       // function（地球键）
        // 音量三键（fnState 开启时物理键发键码；关闭时走 systemDefined 不经本路径）
        0x48 => Some("🔊"),
        0x49 => Some("🔉"),
        0x4A => Some("🔇"),
        _ => None,
    };
    named.map_or_else(|| format!("Key{code}"), str::to_owned)
}

/// CGEventGetFlags 的修饰符位解析（纯函数）。只取四个显示用修饰符；AlphaShift
/// （大写锁定激活态）与 Function（fn 修饰——F 键语义随它翻转但显示仍按 F 键名）
/// 不参与组合显示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct Mods {
    pub cmd: bool,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// kCGEventFlagMaskCommand / Shift / Alternate / Control（CGEventTypes.h）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const FLAG_CMD: u64 = 1 << 20;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const FLAG_SHIFT: u64 = 1 << 17;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const FLAG_ALT: u64 = 1 << 19;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const FLAG_CTRL: u64 = 1 << 18;

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn parse_flags(flags: u64) -> Mods {
    Mods {
        cmd: flags & FLAG_CMD != 0,
        shift: flags & FLAG_SHIFT != 0,
        alt: flags & FLAG_ALT != 0,
        ctrl: flags & FLAG_CTRL != 0,
    }
}

/// 修饰键角色（纯函数）：**修饰键自身的键码** → 它对应的 flags 位。用于「修饰键
/// 自身按下」的显示去重（见 [`format_key_event`]）——旗标前缀已含该修饰符时，
/// 键名位置不再重复打印同一符号（否则 ⌘ 键按下显示 "⌘⌘"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum ModifierRole {
    Cmd,
    Shift,
    Alt,
    Ctrl,
}

/// 键码 → 修饰键角色（非修饰键 → None）。左右修饰键同角色（0x36 右 ⌘ 等）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn modifier_role(code: u16) -> Option<ModifierRole> {
    match code {
        0x37 | 0x36 => Some(ModifierRole::Cmd), // command / right command
        0x38 | 0x3C => Some(ModifierRole::Shift), // shift / right shift
        0x3A | 0x3D => Some(ModifierRole::Alt), // option / right option
        0x3B | 0x3E => Some(ModifierRole::Ctrl), // control / right control
        _ => None,
    }
}

/// 组合格式化（纯函数，契约裁定修饰符序 **Cmd ⇧ ⌥ ⌃**）：无修饰符 → 键名原样；
/// 有修饰符 → 按序前缀（例：Ctrl+Alt+A → "⌥⌃A"，全修饰 → "⌘⇧⌥⌃A"）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn format_combo(mods: Mods, key: &str) -> String {
    let mut s = String::new();
    if mods.cmd {
        s.push('⌘');
    }
    if mods.shift {
        s.push('⇧');
    }
    if mods.alt {
        s.push('⌥');
    }
    if mods.ctrl {
        s.push('⌃');
    }
    s.push_str(key);
    s
}

/// 一次按键事件的显示串（纯函数，评审修复 Minor 6 的落点）。
///
/// 规则：普通键 → [`format_combo`]（旗标前缀 + 键名）。**修饰键自身**按下 → 旗标
/// 快照通常已含该修饰符（按下即生效），前缀会再打一遍同一符号（"⌘⌘"）——此时
/// 抑制该位的前缀、由键名位置打一次（"⌘"）；其余旗标保留（⌘+⇧ 期间按 ⌘ → "⇧⌘"）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn format_key_event(mods: Mods, code: u16) -> String {
    let name = keycode_display_name(code);
    let Some(role) = modifier_role(code) else {
        return format_combo(mods, &name);
    };
    let mut s = String::new();
    if mods.cmd && role != ModifierRole::Cmd {
        s.push('⌘');
    }
    if mods.shift && role != ModifierRole::Shift {
        s.push('⇧');
    }
    if mods.alt && role != ModifierRole::Alt {
        s.push('⌥');
    }
    if mods.ctrl && role != ModifierRole::Ctrl {
        s.push('⌃');
    }
    s.push_str(&name);
    s
}

/// 100ms 同组合合并节流（纯逻辑）。**滑窗语义**（评审修复 Minor 3）：同组合的
/// 重复在窗内被合并为**一次**显示刷新（plan 契约原文「identical combo repeats →
/// single display refresh」）。
///
/// 基准时间在**抑制时也刷新**（滑窗）——长按自动重复（~30Hz）期间每次重复都把
/// 基准推后，连续重复永不越过窗口 → **长按只显示一次**；停手后再次按下（距上次
/// 尝试 >100ms）才重新显示。不同组合立即放行（快速连打零延迟）。
#[derive(Debug)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct ComboThrottle {
    window: Duration,
    last: Option<(String, Instant)>,
}

impl ComboThrottle {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn new(window: Duration) -> Self {
        Self { window, last: None }
    }

    /// 该组合此刻是否应显示（是 → 记录新基准；否 → 抑制但**仍刷新基准**）。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn allow(&mut self, combo: &str, now: Instant) -> bool {
        let dup = matches!(
            &self.last,
            Some((c, t)) if *c == combo && now.duration_since(*t) < self.window
        );
        self.last = Some((combo.to_owned(), now));
        !dup
    }
}

/// 100ms 合并窗（契约值；常量便于测试引用）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const THROTTLE_WINDOW: Duration = Duration::from_millis(100);

/// keycast 生命周期决策（纯函数可测）：`active` = tap 当前是否装机；`recording` =
/// 录制是否进行中；`enable` = 请求开关。返回 Ok(Some(true))=需启动 tap、
/// Ok(Some(false))=需停止、Ok(None)=幂等空转、Err=录制未进行（拒绝开启）。
/// 语义：tap 只在「录制中 AND 开」存活（任务契约）；关闭永远放行（录屏收口
/// best-effort 调用幂等）。
pub fn keycast_action(active: bool, recording: bool, enable: bool) -> Result<Option<bool>, String> {
    match (enable, recording, active) {
        (true, false, _) => Err("没有进行中的录屏".into()),
        (true, true, false) => Ok(Some(true)),
        (true, true, true) => Ok(None),
        (false, _, true) => Ok(Some(false)),
        (false, _, false) => Ok(None),
    }
}

/// hawk://keycast 载荷（plan：{combo, ts}；ts = Unix 毫秒，前端当前不用、留作
/// 语义完整——显示时长由 CSS 动画单源控制）。
#[derive(Serialize, Clone)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct KeycastEvent {
    pub combo: String,
    pub ts: i64,
}

// ---------------------------------------------------------------------------
// CGEventTap 裸 extern（CoreGraphics + 补充的 CFRunLoopStop，ax.rs 同款；其余 CF
// 类型托管走 core-foundation 0.10 安全包装——已在依赖树，不引新依赖）
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod ffi {
    use std::os::raw::c_void;

    pub type CGEventRef = *mut c_void;
    pub type CGEventTapProxy = *mut c_void;
    pub type CGEventTapCallBack =
        unsafe extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;

    // CGEventTypes.h 常量（core-graphics 0.25 的安全 API 未暴露 EventTap 面）
    pub const K_CG_SESSION_EVENT_TAP: u32 = 1; // kCGSessionEventTap
    pub const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0; // kCGHeadInsertEventTap
    pub const K_CG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1; // kCGEventTapOptionListenOnly
    pub const K_CG_EVENT_KEY_DOWN: u32 = 10; // kCGEventKeyDown
    pub const K_CG_KEYBOARD_EVENT_KEYCODE: u32 = 9; // kCGKeyboardEventKeycode
    /// 掩码只装 keyDown：修饰符单独按下（flagsChanged=12）与 keyUp（11）不投递
    /// ——M3 裁定只显示完整组合键（模块注）。
    pub const EVENT_MASK_KEY_DOWN: u64 = 1 << 10;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        /// 建键盘 tap。辅助功能未授予（或系统拒绝）→ 返回 NULL。
        /// 返回 +1 引用的 CFMachPortRef（调用方负责 invalidate + release）。
        pub fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: u64,
            callback: CGEventTapCallBack,
            user_info: *mut c_void,
        ) -> *mut c_void;
        /// 销毁 tap（泄漏守卫的执行体：port 失效后系统级监听即停）。
        pub fn CFMachPortInvalidate(port: *mut c_void);
        pub fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
        /// CGEventFlags（u64 位掩码；事件生命周期内有效）。
        pub fn CGEventGetFlags(event: CGEventRef) -> u64;
    }
    // 注：CFRunLoopStop 不在此声明——core-foundation 0.10 的 CFRunLoop::stop()
    // 安全包装已覆盖（跨线程安全，CFRunLoop.h 明示），TapHandle::stop 走包装。
}

// ---------------------------------------------------------------------------
// tap 线程（媒体键 monitor 的线程化对应物：回调只解析+投递，绝不建窗/关窗）
// ---------------------------------------------------------------------------

/// tap 回调分发的处理闭包（进程级单槽：keycast tap 进程内至多一个——LIFECYCLE
/// 锁保证 spawn 串行，单槽与装机一一对应）。槽带代号（TAP_GENERATION）：超时弃单
/// 的线程异步收尾时只清**自己代号**的槽——防止晚到的旧线程 teardown 擦掉重试装机的
/// 新 handler。
#[cfg(target_os = "macos")]
type KeyHandler = Box<dyn Fn(u16, u64) + Send>;

#[cfg(target_os = "macos")]
static TAP_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(target_os = "macos")]
static TAP_HANDLER: Mutex<Option<(u64, KeyHandler)>> = Mutex::new(None);

/// 清槽（仅当槽内仍是自己代号的 handler——旧线程晚到收尾不误伤新装机）。
#[cfg(target_os = "macos")]
fn clear_handler_if_mine(gen: u64) {
    let mut slot = TAP_HANDLER.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|(g, _)| *g == gen) {
        *slot = None;
    }
}

/// CGEventTapCallBack 本体：C 函数指针不能捕获环境 → 经 TAP_HANDLER 单槽转发。
/// 回调线程 = tap 线程（CFRunLoop 投递），保持最小工作（读字段 + 调闭包）。
#[cfg(target_os = "macos")]
unsafe extern "C" fn tap_callback(
    _proxy: *mut std::os::raw::c_void,
    etype: u32,
    event: *mut std::os::raw::c_void,
    _user_info: *mut std::os::raw::c_void,
) -> *mut std::os::raw::c_void {
    // workspace lint unsafe-op-in-unsafe-fn：unsafe fn 体内仍需显式 unsafe 块。
    unsafe {
        if etype == ffi::K_CG_EVENT_KEY_DOWN && !event.is_null() {
            let code = ffi::CGEventGetIntegerValueField(event, ffi::K_CG_KEYBOARD_EVENT_KEYCODE);
            let flags = ffi::CGEventGetFlags(event);
        if let Ok(guard) = TAP_HANDLER.lock() {
            if let Some((_, f)) = guard.as_ref() {
                // 键码域 0-127（HID 界定）；防御性截断到 u16。
                f((code & 0xFFFF) as u16, flags);
            }
        }
        }
    }
    event // listen-only：原样返回（返回值被系统忽略，形式上必须回传）
}

/// tap 句柄：stop() 发 CFRunLoopStop 信号 → join 收尾线程（线程内移除 source +
/// invalidate tap + 释放引用）。CFRunLoop 在 core-foundation 0.10 标注 Send+Sync
/// （runloop 对象由 CF 全局持有、跨线程 stop 安全），句柄可存 static。
#[cfg(target_os = "macos")]
pub(crate) struct TapHandle {
    runloop: core_foundation::runloop::CFRunLoop,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl TapHandle {
    fn stop(mut self) {
        self.runloop.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // drop(self.runloop) 释放句柄侧引用（对象随最后一个引用释放）。
    }
}

/// 装键盘 tap（专用线程 + 私有 CFRunLoop——媒体键模式把监听挂主线程 AppKit 循环，
/// CGEventTap 则自备 runloop：tauri 主循环归 wry，不可注入外部 source）。
/// `on_key` 在 tap 线程被调（键码, flags）。创建失败（NULL：辅助功能未授予）→ Err。
#[cfg(target_os = "macos")]
pub(crate) fn spawn_tap(on_key: KeyHandler) -> Result<TapHandle, String> {
    use core_foundation::base::TCFType;
    use core_foundation::mach_port::CFMachPort;
    use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
    use std::os::raw::c_void;
    use std::ptr;

    // 单槽换装（LIFECYCLE 锁下无并发 spawn；代号防旧线程晚到清槽误伤，见
    // TAP_GENERATION 注）。防御性 replace 覆盖理论残留。
    let gen = TAP_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    *TAP_HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = Some((gen, on_key));

    let (tx, rx) = std::sync::mpsc::channel::<Result<CFRunLoop, String>>();
    let thread = std::thread::Builder::new()
        .name("keycast-tap".into())
        .spawn(move || {
            let tap_ref = unsafe {
                ffi::CGEventTapCreate(
                    ffi::K_CG_SESSION_EVENT_TAP,
                    ffi::K_CG_HEAD_INSERT_EVENT_TAP,
                    ffi::K_CG_EVENT_TAP_OPTION_LISTEN_ONLY,
                    ffi::EVENT_MASK_KEY_DOWN,
                    tap_callback,
                    ptr::null_mut(),
                )
            };
            if tap_ref.is_null() {
                clear_handler_if_mine(gen);
                let _ = tx.send(Err("CGEventTap 创建失败（辅助功能权限可能未授予）".into()));
                return;
            }
            // +1 引用接管（CGEventTapCreate 返回 create 规则引用；*mut c_void 经
            // .cast() 归位 CFMachPortRef）。
            let tap = unsafe { CFMachPort::wrap_under_create_rule(tap_ref.cast()) };
            let Ok(source) = tap.create_runloop_source(0) else {
                // source 建失败：tap 先 invalidate 再随 wrapper drop 释放（未挂
                // source 未 run，无事件可投递）。
                unsafe { ffi::CFMachPortInvalidate(tap.as_concrete_TypeRef() as *mut c_void) };
                clear_handler_if_mine(gen);
                let _ = tx.send(Err("tap runloop source 创建失败".into()));
                return;
            };
            let runloop = CFRunLoop::get_current();
            runloop.add_source(&source, unsafe { kCFRunLoopCommonModes });
            // 装机完成信号（句柄侧持有同一 runloop 的独立引用；此后 run_current
            // 阻塞直到 stop() 的 CFRunLoopStop）。
            let _ = tx.send(Ok(runloop.clone()));
            CFRunLoop::run_current();
            // —— 收尾（泄漏守卫）：移除 source → invalidate tap（系统级监听即停）
            // → wrapper drop 释放引用。顺序保证停机窗口内不再有事件路由。
            runloop.remove_source(&source, unsafe { kCFRunLoopCommonModes });
            unsafe { ffi::CFMachPortInvalidate(tap.as_concrete_TypeRef() as *mut c_void) };
            drop(source);
            drop(tap);
            clear_handler_if_mine(gen);
        })
        .map_err(|e| format!("keycast tap 线程启动失败: {e}"))?;

    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(runloop)) => Ok(TapHandle {
            runloop,
            thread: Some(thread),
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            // 装机超时（>2s，理论病态）：弃单守卫——reaper 线程接手通道，后续线程
            // 若最终装机成功（发出 runloop 引用）则立即停掉其 runloop（线程收尾
            // 退出）；失败路径 recv 返 Err，reaper 直接退出。均无泄漏。
            std::thread::spawn(move || {
                if let Ok(Ok(rl)) = rx.recv() {
                    rl.stop();
                }
            });
            Err("keycast tap 装机超时（runloop 未就绪）".into())
        }
    }
}

// ---------------------------------------------------------------------------
// 运行态 + 命令（keycast_set）+ 窗口
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
static ACTIVE: Mutex<Option<TapHandle>> = Mutex::new(None);
#[cfg(not(target_os = "macos"))]
static ACTIVE: Mutex<bool> = Mutex::new(false);

/// 启停串行锁：spawn/stop 全路径互斥——TAP_HANDLER 单槽一致性 + ACTIVE 双检免竞态。
static LIFECYCLE: Mutex<()> = Mutex::new(());

#[cfg(target_os = "macos")]
fn active_now() -> bool {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}
#[cfg(not(target_os = "macos"))]
fn active_now() -> bool {
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner())
}

const KEYCAST_LABEL: &str = "keycast";
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const KEYCAST_W: f64 = 320.0;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const KEYCAST_H: f64 = 88.0;

/// 按键显示开关（悬浮条按钮入口）。enabled=true：录制未进行 → Err（悬浮条只在
/// 录制中存在，此为 API 防御）；辅助功能未授予 → Err（瞬态错误文案）；tap/窗口
/// 失败 → Err 且不残留半装机。幂等：重复同向开关为 Ok（良性竞态，pip 同口径）。
#[tauri::command(async)]
pub fn keycast_set(app: AppHandle, enabled: bool) -> Result<(), String> {
    let action = keycast_action(active_now(), crate::recording::recording_active(), enabled)?;
    match action {
        // 幂等空转（keycast_action 已 Err 分流——此处模式保证穷尽）。
        None => Ok(()),
        Some(true) => {
            let _guard = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
            keycast_enable(app)
        }
        Some(false) => {
            let _guard = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
            keycast_stop_tap();
            close_keycast_window(&app);
            Ok(())
        }
    }
}

/// 开启：辅助功能预检（错误文案面向悬浮条瞬态显示）→ tap 装机 → 小窗懒建。
/// 顺序先 tap 后窗：tap 失败不留窗；窗失败回收 tap。调用方持 LIFECYCLE 锁。
#[cfg(target_os = "macos")]
fn keycast_enable(app: AppHandle) -> Result<(), String> {
    if !hawksnap_platform_mac::ax::ax_trusted() {
        return Err("需辅助功能权限：系统设置→隐私与安全性→辅助功能 中添加 HawkSnap".into());
    }
    // LIFECYCLE 锁下 ACTIVE 必空（enable/disable 串行）——双检防御理论残留。
    if active_now() {
        return Ok(());
    }
    let handler_app = app.clone();
    let handle = spawn_tap(Box::new(move |code, flags| {
        on_key_down(&handler_app, code, flags);
    }))?;
    if let Err(e) = show_keycast_window(&app) {
        handle.stop();
        return Err(e);
    }
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn keycast_enable(_app: AppHandle) -> Result<(), String> {
    Err("仅 macOS 支持按键显示".into())
}

/// 停 tap（幂等；录屏收口与开关关闭共用）。cfg 门控兜底非 macOS 编译。
pub(crate) fn keycast_stop_tap() {
    #[cfg(target_os = "macos")]
    if let Some(handle) = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take() {
        handle.stop();
    }
    #[cfg(not(target_os = "macos"))]
    {
        *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = false;
    }
}

/// 录屏停止收口（recording.rs record_stop 调用，best-effort）：停 tap + 关小窗。
/// 「tap 只在录制中存活」不变量的执行点——用户忘关开关也会随录屏停止一并收掉。
pub(crate) fn keycast_disable(app: &AppHandle) {
    let _guard = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
    keycast_stop_tap();
    close_keycast_window(app);
}

/// keyDown 处理（tap 线程）：键名 → 组合 → 节流 → 事件。节流态为进程级单例
/// （tap 由 LIFECYCLE 锁保证单例 ⇒ 节流单例等价语义）；锁持有时长 = 纯内存运算 +
/// emit 投递，远小于 100ms 窗，不构成回调堆积。
#[cfg(target_os = "macos")]
fn on_key_down(app: &AppHandle, code: u16, flags: u64) {
    static THROTTLE: Mutex<Option<ComboThrottle>> = Mutex::new(None);
    let mut slot = THROTTLE.lock().unwrap_or_else(|e| e.into_inner());
    let throttle = slot.get_or_insert_with(|| ComboThrottle::new(THROTTLE_WINDOW));
    let combo = format_key_event(parse_flags(flags), code);
    if throttle.allow(&combo, Instant::now()) {
        let _ = app.emit(
            "hawk://keycast",
            KeycastEvent {
                combo,
                ts: unix_millis(),
            },
        );
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// keycast 小窗懒建（bar-close 同款模式；非主线程调用——命令线程 ✓）。
/// 320×88 透明 + always_on_top + 点击穿透（set_ignore_cursor_events——录屏中
/// 用户点击必须落到前台应用）。位置：主显示器**底部居中**（悬浮条在顶部居中，
/// 两窗不相交；底部上收 120 物理 px ≈ 80 逻辑 px 让开 Dock）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn show_keycast_window(app: &AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(KEYCAST_LABEL) {
        position_keycast(&w);
        let _ = w.show();
        return Ok(());
    }
    let win = WebviewWindowBuilder::new(
        app,
        KEYCAST_LABEL,
        WebviewUrl::App("keycast?window=keycast".into()),
    )
    .inner_size(KEYCAST_W, KEYCAST_H)
    .decorations(false)
    .always_on_top(true)
    .resizable(false)
    .skip_taskbar(true)
    // 绝不抢键盘焦点（评审修复 Important）：tao 的 focusable(false) 覆写
    // `canBecomeKeyWindow` → 本窗**永远不能成为 key window** → show()（tao 走
    // makeKeyAndOrderFront）只把它排到最前、不接管键盘，用户的按键仍落在被录
    // 应用上。与 overlay/flow.rs「绝不 set_focus 抢焦点」同纪律。focused(false)
    // 另确保建窗瞬间不做 makeKeyAndOrderFront。
    .focusable(false)
    .focused(false)
    .transparent(true) // 透明窗：页面自绘组合键胶囊 + CSS 淡出（overlay 同款能力）
    .visible(false)    // 先定位再 show，杜绝默认位置闪现（bar 同款）
    .build()
    .map_err(|e| e.to_string())?;
    let _ = win.set_ignore_cursor_events(true);
    position_keycast(&win);
    let _ = win.show();
    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn position_keycast(w: &tauri::WebviewWindow) {
    if let Some(m) = w.primary_monitor().ok().flatten() {
        let size = w
            .outer_size()
            .unwrap_or(tauri::PhysicalSize::new(KEYCAST_W as u32, KEYCAST_H as u32));
        let x = m.position().x + ((m.size().width as i32 - size.width as i32) / 2).max(0);
        // 底边上收：窗口底缘距屏底 120 物理 px（≈80 逻辑 px，Dock 常驻高度之上）。
        let y = m.position().y + m.size().height as i32 - size.height as i32 - 120;
        let _ = w.set_position(tauri::PhysicalPosition::new(x, y.max(0)));
    } else {
        let _ = w.center();
    }
}

/// 关小窗（best-effort，bar-close 同款）：close 语义，下次开启懒建分支重建。
fn close_keycast_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(KEYCAST_LABEL) {
        let _ = w.close();
    }
}

// ---------------------------------------------------------------------------
// 测试：键名表 / 组合格式 / flags / 节流 / 生命周期决策（纯逻辑）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod keycast_tests {
    use super::*;

    #[test]
    fn letters_map_by_physical_layout() {
        // QWERTY 键码非字母序的抽样：A=0x00 S=0x01 D=0x02 Z=0x06 Q=0x0C M=0x2E。
        assert_eq!(keycode_display_name(0x00), "A");
        assert_eq!(keycode_display_name(0x01), "S");
        assert_eq!(keycode_display_name(0x02), "D");
        assert_eq!(keycode_display_name(0x06), "Z");
        assert_eq!(keycode_display_name(0x0C), "Q");
        assert_eq!(keycode_display_name(0x2E), "M");
    }

    #[test]
    fn digits_map_to_digits() {
        assert_eq!(keycode_display_name(0x12), "1");
        assert_eq!(keycode_display_name(0x1D), "0");
        assert_eq!(keycode_display_name(0x16), "6"); // 6=0x16 在 5 之前（物理序）
        assert_eq!(keycode_display_name(0x17), "5");
    }

    #[test]
    fn f_key_range_is_fully_covered() {
        // F1-F12 键码（kVK_F1=122 … 非连续物理段）：122,120,99,118,96,97,98,100,101,109,103,111
        let fkeys = [
            (0x7A, "F1"), (0x78, "F2"), (0x63, "F3"), (0x76, "F4"),
            (0x60, "F5"), (0x61, "F6"), (0x62, "F7"), (0x64, "F8"),
            (0x65, "F9"), (0x6D, "F10"), (0x67, "F11"), (0x6F, "F12"),
        ];
        for (code, name) in fkeys {
            assert_eq!(keycode_display_name(code), name, "code={code}");
        }
    }

    #[test]
    fn control_keys_use_universal_symbols() {
        assert_eq!(keycode_display_name(0x31), "␣"); // space
        assert_eq!(keycode_display_name(0x24), "↩"); // return
        assert_eq!(keycode_display_name(0x30), "⇥"); // tab
        assert_eq!(keycode_display_name(0x35), "⎋"); // escape
        assert_eq!(keycode_display_name(0x33), "⌫"); // delete
        assert_eq!(keycode_display_name(0x75), "⌦"); // forward delete
        assert_eq!(keycode_display_name(0x7B), "←");
        assert_eq!(keycode_display_name(0x7E), "↑");
    }

    #[test]
    fn unknown_code_falls_back_to_key_prefix() {
        // 契约：未知键码 → "Key{code}"（十进制）——F 键区段外/媒体键优雅兜底。
        assert_eq!(keycode_display_name(0x99), "Key153");
        assert_eq!(keycode_display_name(0xF0), "Key240");
    }

    #[test]
    fn parse_flags_extracts_four_modifiers() {
        assert_eq!(parse_flags(0), Mods { cmd: false, shift: false, alt: false, ctrl: false });
        assert!(parse_flags(FLAG_CMD).cmd);
        assert!(
            parse_flags(FLAG_SHIFT | FLAG_ALT).shift && parse_flags(FLAG_SHIFT | FLAG_ALT).alt
        );
        // 大写锁定（AlphaShift 1<<16）与 fn（1<<23）不计入显示。
        let noisy = FLAG_CMD | (1 << 16) | (1 << 23);
        let m = parse_flags(noisy);
        assert!(m.cmd && !m.shift && !m.alt && !m.ctrl);
    }

    #[test]
    fn combo_format_orders_cmd_shift_alt_ctrl() {
        let none = Mods { cmd: false, shift: false, alt: false, ctrl: false };
        assert_eq!(format_combo(none, "A"), "A");
        let cmd = Mods { cmd: true, ..none };
        assert_eq!(format_combo(cmd, "A"), "⌘A");
        let cmd_shift = Mods { cmd: true, shift: true, ..none };
        assert_eq!(format_combo(cmd_shift, "S"), "⌘⇧S");
        let ctrl_alt = Mods { alt: true, ctrl: true, ..none };
        assert_eq!(format_combo(ctrl_alt, "A"), "⌥⌃A"); // 序：⌥ 在 ⌃ 前
        let all = Mods { cmd: true, shift: true, alt: true, ctrl: true };
        assert_eq!(format_combo(all, "␣"), "⌘⇧⌥⌃␣");
    }

    #[test]
    fn throttle_merges_identical_repeats_within_window() {
        // 滑窗语义（评审修复 Minor 3）：同组合的重复在窗内合并为**一次**显示刷新，
        // 且**抑制时基准也前移**（长按自动重复永不越过窗口 → 只显示一次）。
        let mut t = ComboThrottle::new(Duration::from_millis(100));
        let t0 = Instant::now();
        assert!(t.allow("A", t0)); // 首次放行
        assert!(!t.allow("A", t0 + Duration::from_millis(50))); // 窗内同组合合并
        assert!(t.allow("B", t0 + Duration::from_millis(60))); // 不同组合立即放行
        assert!(!t.allow("B", t0 + Duration::from_millis(80)));
        // 连续重复（长按）→ 基准持续前移 → 始终不越过窗口 → 只显示一次。
        assert!(!t.allow("B", t0 + Duration::from_millis(150)));
        assert!(!t.allow("B", t0 + Duration::from_millis(200)));
        // 停手后再次按下（距上次尝试 >100ms）→ 重新显示。
        assert!(t.allow("B", t0 + Duration::from_millis(350)));
    }

    #[test]
    fn throttle_allows_same_combo_after_quiet_gap() {
        // 静默间隔 > 窗口 → 同一组合重新显示（滑窗不等于「永久抑制」）。
        let mut t = ComboThrottle::new(Duration::from_millis(100));
        let t0 = Instant::now();
        assert!(t.allow("A", t0));
        assert!(!t.allow("A", t0 + Duration::from_millis(40)));
        // 静默 200ms（无任何尝试）→ 基准仍是 t0+40ms → 已超窗 → 放行。
        assert!(t.allow("A", t0 + Duration::from_millis(240)));
    }

    /// 评审修复 Minor 6：修饰键自身按下不重复打印符号（"⌘⌘" → "⌘"）。
    #[test]
    fn modifier_key_press_shows_single_symbol() {
        let none = Mods { cmd: false, shift: false, alt: false, ctrl: false };
        // ⌘ 键自身按下：旗标快照通常已含 cmd → 只显示一个 ⌘。
        let cmd = Mods { cmd: true, ..none };
        assert_eq!(format_key_event(cmd, 0x37), "⌘"); // 左 ⌘
        assert_eq!(format_key_event(cmd, 0x36), "⌘"); // 右 ⌘
        // 其余旗标保留：⌘+⇧ 期间按 ⌘ → "⇧⌘"（不重复 ⌘）。
        let cmd_shift = Mods { cmd: true, shift: true, ..none };
        assert_eq!(format_key_event(cmd_shift, 0x37), "⇧⌘");
        // 旗标未含该修饰符时（个别驱动）：键名位置出符号，仍只一个。
        assert_eq!(format_key_event(none, 0x37), "⌘");
        // 修饰键角色表：非修饰键 → None。
        assert_eq!(modifier_role(0x37), Some(ModifierRole::Cmd));
        assert_eq!(modifier_role(0x38), Some(ModifierRole::Shift));
        assert_eq!(modifier_role(0x3A), Some(ModifierRole::Alt));
        assert_eq!(modifier_role(0x3B), Some(ModifierRole::Ctrl));
        assert_eq!(modifier_role(0x00), None); // A
    }

    /// 普通键的显示仍是「旗标前缀 + 键名」（format_key_event 走 format_combo 分支）。
    #[test]
    fn ordinary_key_event_keeps_combo_format() {
        let none = Mods { cmd: false, shift: false, alt: false, ctrl: false };
        assert_eq!(format_key_event(none, 0x00), "A");
        let cmd = Mods { cmd: true, ..none };
        assert_eq!(format_key_event(cmd, 0x00), "⌘A");
        let all = Mods { cmd: true, shift: true, alt: true, ctrl: true };
        assert_eq!(format_key_event(all, 0x00), "⌘⇧⌥⌃A");
    }

    #[test]
    fn throttle_window_constant_is_100ms() {
        // 契约值守护：100ms 合并窗（长按自动重复 ~30Hz 全部落窗内 → 单次显示）。
        assert_eq!(THROTTLE_WINDOW, Duration::from_millis(100));
    }

    #[test]
    fn lifecycle_decisions_follow_recording_and_state() {
        // 开：仅录制中且未装机 → Start；已装机 → 幂等 None。
        assert_eq!(keycast_action(false, true, true), Ok(Some(true)));
        assert_eq!(keycast_action(true, true, true), Ok(None));
        // 开：未录制 → Err（悬浮条只在录制中存在，API 防御）。
        assert!(keycast_action(false, false, true).is_err());
        assert!(keycast_action(true, false, true).is_err());
        // 关：装机中 → Stop；未装机 → 幂等 None（录屏收口 best-effort 语义）。
        assert_eq!(keycast_action(true, true, false), Ok(Some(false)));
        assert_eq!(keycast_action(false, false, false), Ok(None));
        assert_eq!(keycast_action(false, true, false), Ok(None));
    }

    /// N4 载荷契约（plan：{combo, ts}，serde 默认字段名）。
    #[test]
    fn keycast_event_serializes_combo_and_ts() {
        let s = serde_json::to_string(&KeycastEvent { combo: "⌘⇧A".into(), ts: 1234567 }).unwrap();
        assert_eq!(s, r#"{"combo":"⌘⇧A","ts":1234567}"#);
    }
}

// ---------------------------------------------------------------------------
// 真实 tap 集成测试（#[ignore]：依赖运行环境的辅助功能授权——终端/se 域已授权时
// 可用；SE 注入的键盘事件**对 listen-only tap 可见**（与 F 键 HID 缺口不同，
// CGEventTap 收得到系统合成事件），故可脚本化验证）。
// ---------------------------------------------------------------------------

#[cfg(all(test, target_os = "macos"))]
mod tap_integration {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 真实 tap 事件到达验证：装机 → System Events 注入 Esc（key code 53）→
    /// 探针计数递增 → 收尾。运行：`cargo test -p app --lib keycast -- --ignored
    /// --nocapture`（要求注入方——终端/测试宿主链路的辅助功能已授予；未授予时
    /// CGEventTapCreate 返回 NULL，测试以明确断言失败而非静默通过）。
    #[test]
    #[ignore = "真实 CGEventTap + System Events 键注入：需运行环境辅助功能授权"]
    fn real_tap_receives_injected_esc_key() {
        let hits = Arc::new(AtomicUsize::new(0));
        let probe = hits.clone();
        let handle = spawn_tap(Box::new(move |code, _flags| {
            if code == 0x35 {
                probe.fetch_add(1, Ordering::SeqCst);
            }
        }))
        .expect("CGEventTap 装机失败（辅助功能未授予？）");

        // SE 注入 Esc：需要**注入方**（终端/测试宿主）有辅助功能授权（任务注）。
        let injected = std::process::Command::new("osascript")
            .args(["-e", "tell application \"System Events\" to key code 53"])
            .output()
            .expect("osascript 启动失败");
        assert!(
            injected.status.success(),
            "System Events 注入失败（终端缺辅助功能授权？）: {}",
            String::from_utf8_lossy(&injected.stderr)
        );

        // tap 投递异步：宽限 2s 轮询。
        let deadline = Instant::now() + Duration::from_secs(2);
        while hits.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        handle.stop();
        assert!(
            hits.load(Ordering::SeqCst) >= 1,
            "listen-only tap 未收到注入的 Esc（keycode 53）事件"
        );
    }
}
