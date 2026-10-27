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
use std::time::Duration;
use tauri::AppHandle;

use super::flow::start_capture_flow;
#[cfg(target_os = "macos")]
use super::hover;

// 热键注册失败的 pull 通道：setup 阶段的 hawk://hotkey-error emit 早于前端 listen 注册，
// 必丢；错误同时落入此静态，前端挂载时经 hotkey_error 命令拉取补显（读取即清除）。
static HOTKEY_ERROR: Mutex<Option<HotkeyError>> = Mutex::new(None);

/// Plan B Task 5：智能吸附 AX 悬停探测注册。
///
/// 辅助功能未授予 → 整条链路缺席（裁定 1）：eprintln + `Ok(())`，不装监听、不弹授权窗，
/// 桌面上不留任何提示；拖拽框选与窗口级磁吸不受影响。
/// 必须在主线程调用（事件监听挂在 AppKit 事件循环上，与 register_media_key_f1 同）。
#[cfg(target_os = "macos")]
pub fn register_hover_probe(app: &AppHandle) -> Result<(), String> {
    if !hawksnap_platform_mac::ax::ax_trusted() {
        eprintln!("[hawksnap] 未授予辅助功能：悬停元素高亮不可用（拖拽框选不受影响）");
        return Ok(());
    }
    hover::install_hover_monitors(app);
    Ok(())
}

pub fn register_hotkeys(app: &AppHandle) -> Result<(), HotkeyError> {
    // C7：热键自设置读取（settings.json 缺失/损坏 → Settings::default = F1/F2/F6），
    // 注册失败语义与拆分前一致：Err 上抛 + 落 pull 通道（HOTKEY_ERROR，首错优先），
    // lib.rs 的 emit/拉面板逻辑不变。
    let settings = crate::settings::Settings::load(app);
    reregister_from_settings(app, &settings).inspect_err(|err| {
        let mut slot = HOTKEY_ERROR.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            // 首错优先（通道槽位只有一个）：register_in_order 返回的即注册序首个错误
            *slot = Some(err.clone());
        }
    })
}

/// 热键角色：注册序 capture → pin → record 下决定组合键触发哪个处理器。
/// 小写序列化（"capture"/"pin"/"record"）：hotkey-error 载荷的 kind 字段，前端据此
/// 归因横幅（截图/贴图/录屏——D0-⑤）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HotkeyKind {
    Capture,
    Pin,
    Record,
}

/// 热键注册失败（D0-⑤）：首错的消息 + 角色归因。同时是 hawk://hotkey-error 事件与
/// hotkey_error 命令的载荷（serde camelCase → `{kind, message}`）；Display 只出 message
/// ——lib.rs 的 eprintln 日志口径与拆分前一致（组合键本就写在 message 里）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyError {
    pub kind: HotkeyKind,
    pub message: String,
}

impl std::fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// 按注册序（capture → pin → record）逐项调用 register_fn；全部尝试（前项失败不阻断
/// 后项——拆分前 F1 被占用时 F2 仍注册成功的行为保留），任一失败 → 返回首个错误
/// （带角色归因，D0-⑤）。与 AppHandle 解耦（app 由 register_fn 闭包捕获）：settings.rs
/// 的保存管线与单测共用。
pub(crate) fn register_in_order(
    settings: &crate::settings::Settings,
    mut register_fn: impl FnMut(&str, HotkeyKind) -> Result<(), String>,
) -> Result<(), HotkeyError> {
    let jobs = [
        (settings.hotkey_capture.as_str(), HotkeyKind::Capture),
        (settings.hotkey_pin.as_str(), HotkeyKind::Pin),
        (settings.hotkey_record.as_str(), HotkeyKind::Record),
    ];
    let mut first_err: Option<HotkeyError> = None;
    for (combo, kind) in jobs {
        if let Err(e) = register_fn(combo, kind) {
            if first_err.is_none() {
                first_err = Some(HotkeyError { kind, message: e });
            }
        }
    }
    first_err.map_or(Ok(()), Err)
}

/// 注册单个组合并挂对应处理器（生产 register_fn）。Shortcut::from_str 是插件的热键
/// 字符串解析器（"F1"、"Cmd+Shift+A"，大小写不敏感）。on_shortcut 注册快捷键并挂处理器
/// （勿再单独 register：同一组合二次注册会被 macOS 以 eventHotKeyExistsErr 拒绝）。
/// 热键回调在主线程（Carbon 事件）触发，而窗口建窗/关窗类操作禁止在主线程做
/// （tauri-runtime-wry 在主线程处理 Close 消息会直接 panic），故处理器只 spawn 独立
/// 线程执行，热键处理立即返回。
pub fn register_one(app: &AppHandle, combo: &str, kind: HotkeyKind) -> Result<(), String> {
    use std::str::FromStr;
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
    let shortcut =
        Shortcut::from_str(combo).map_err(|e| format!("热键 {combo} 无法解析: {e}"))?;
    let gs = app.global_shortcut();
    let app2 = app.clone();
    // 拥有化组合串：Pin 探针的闭包要求 'static（&str 参数活不过本函数体）。
    let combo_owned = combo.to_string();
    match kind {
        HotkeyKind::Capture => gs
            .on_shortcut(shortcut, move |_app, _shortcut, event| {
                if event.state == ShortcutState::Pressed {
                    let app = app2.clone();
                    std::thread::spawn(move || trigger_capture_flow(&app));
                }
            })
            .map_err(|e| format!("{combo} 注册失败（可能被占用）: {e}")),
        HotkeyKind::Pin => gs
            .on_shortcut(shortcut, move |_app, _shortcut, event| {
                if event.state == ShortcutState::Pressed {
                    // 探针（常驻，廉价）：E2E 判别「pin 回调没进来」vs「进来了但没出窗」。
                    // 注意 global-hotkey 0.8 的 Code::F2 → Carbon 0x78=120（99 是 kVK_F3）。
                    // D0-①：探针不再写死 F2，回显本次实际注册的组合（combo 即设置值）。
                    eprintln!("[hawksnap] {combo_owned} 热键触发（pin_latest）");
                    let app = app2.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = crate::pins::pin_latest(app.clone()) {
                            let _ = tauri::Emitter::emit(&app, "hawk://capture-error", e);
                        }
                    });
                }
            })
            .map_err(|e| format!("{combo} 注册失败（可能被占用）: {e}")),
        HotkeyKind::Record => gs
            .on_shortcut(shortcut, move |_app, _shortcut, event| {
                if event.state == ShortcutState::Pressed {
                    eprintln!("[hawksnap] {combo_owned} 热键触发（record）");
                    let app = app2.clone();
                    std::thread::spawn(move || {
                        // bench 轮次保护（D0-③，与 F1/托盘入口同款）：基准模式进行中
                        // 忽略外部触发——录制选区覆盖层是全屏 UI，混入 bench 轮会污染
                        // 时延样本（M2 的 F6 toggle 无此守卫；N1 起覆盖层参与，补齐）。
                        if crate::bench_mode::is_running() {
                            return;
                        }
                        // N1：F6 完整路由（intent::f6_route 纯函数，热键侧分派）——
                        // 录制/暂停中 → 停止（M6 toggle 的停止路径原样保留，即便覆盖层
                        // 可见也放行去停：两态经对向守卫后不再共存，此处防御性放行）；
                        // 空闲 → 进入录制选区模式（**旧「直接全屏录制」语义废除**，plan
                        // Interfaces 裁定：F6 统一进入选区模式，拖选=区域、Enter/双击=
                        // 全屏快捷确认、Esc=取消；record_start API 保留给兼容）；
                        // 覆盖层可见（截图或录制选区进行中）→ 拒绝横幅（M2 守卫 a：
                        // 两套全屏 UI 不叠屏；反馈走 capture-error 通道，与 M2 同口径）。
                        use super::intent::{f6_route, F6Route};
                        let overlay_visible = super::flow::ANY_OVERLAY_VISIBLE
                            .load(std::sync::atomic::Ordering::SeqCst);
                        match f6_route(
                            crate::recording::recording_active(),
                            overlay_visible,
                        ) {
                            F6Route::StopRecording => {
                                if let Err(e) = crate::recording::record_stop(app.clone()) {
                                    // 错误横幅可见化（show_record_error，非主线程合法），
                                    // 拉起失败仅 eprintln，横幅退化为 push 通道尽力投递。
                                    crate::panel::show_record_error(&app, e);
                                }
                            }
                            F6Route::StartRecordSelection => {
                                if let Err(e) =
                                    super::flow::start_record_selection_flow(&app)
                                {
                                    crate::panel::show_record_error(&app, e);
                                }
                            }
                            F6Route::Reject(msg) => {
                                crate::panel::show_capture_error(&app, msg.to_string());
                            }
                        }
                    });
                }
            })
            .map_err(|e| format!("{combo} 注册失败（可能被占用）: {e}")),
    }
}

/// C7：按设置重注册全部热键——先 unregister_all 旧组合，再按 capture → pin → record
/// 注册新组合。任一失败 → Err（调用方中止保存；Err 携带角色归因，D0-⑤）。注意：失败时
/// 旧组合已注销、新组合半数可能已注册（运行态半新半旧），设置文件保持旧值——重启或下次
/// 成功保存自愈（计划裁定语义，不做旧热键回滚注册）。注册经插件 run_main_thread 交付
/// 主线程（2.4.1 register_internal/unregister_all 均已自带），任意线程可调——
/// set_settings(async) 从 tokio 线程调用安全。
pub fn reregister_from_settings(
    app: &AppHandle,
    settings: &crate::settings::Settings,
) -> Result<(), HotkeyError> {
    reregister_with(app, settings, register_one)
}

/// reregister 的可注入缝（计划 C7 裁定）：register_fn 由调用方注入，测试注入 fake
/// （AppHandle 不可造，热键字符串经 settings.rs 的 register_in_order 间接验证）。
pub fn reregister_with(
    app: &AppHandle,
    settings: &crate::settings::Settings,
    register_fn: impl Fn(&AppHandle, &str, HotkeyKind) -> Result<(), String>,
) -> Result<(), HotkeyError> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    app.global_shortcut()
        .unregister_all()
        // 注销失败发生在任何角色注册之前、无从归因到单个组合——按注册序归首个角色
        //（capture），仅为满足 HotkeyError 的 kind 字段；文案保持原样。
        .map_err(|e| HotkeyError {
            kind: HotkeyKind::Capture,
            message: format!("注销旧热键失败: {e}"),
        })?;
    register_in_order(settings, |combo, kind| register_fn(app, combo, kind))
}

/// F1（Carbon）与媒体键（NSEvent 亮度下）两条触发通道共用的入口：300ms 去重窗内只启动
/// 一次截图流程（个别键盘/系统组合下两条事件可能同时到达）。任意线程可调：函数体只做
/// 原子置位 + spawn，start_capture_flow 永远跑在 spawn 出的非主线程上（主线程建窗/关窗
/// 会 panic）。
pub(crate) fn trigger_capture_flow(app: &AppHandle) {
    // bench 守卫（D0-③，Plan C 终审）：基准模式进行中忽略外部触发（防一轮被污染）——
    // bench 轮次直接调 start_capture_flow、不经本入口，自身不受影响。托盘/面板入口
    // 各有同款守卫（lib.rs / panel.rs），F1 与媒体键两通道在此统一拦截。
    if crate::bench_mode::is_running() {
        return;
    }
    if !media::arm_once(&media::TRIGGER_INFLIGHT) {
        return; // 去重窗口内已有触发在跑
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        if let Err(e) = start_capture_flow(&app2) {
            // F1/媒体键失败横幅可见化：主面板懒创建，未开面板时 push 事件无窗可显——
            // emit 前先拉起（show_capture_error 内 ensure_main_panel，本线程非主线程合法）
            crate::panel::show_capture_error(&app2, e);
        }
    });
    // 去重窗按时长解除（而非按流程结束）：恰好挡住同一次按键的双通道竞态
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        media::disarm(&media::TRIGGER_INFLIGHT);
    });
}

// —— Task 13：媒体键（F1 亮度键）兼容 ——

/// 媒体键路径：macOS 默认 `com.apple.keyboard.fnState` 未开启，物理 F1 不发 keycode 122，
/// 而是发 systemDefined 的亮度下事件（NX_KEYTYPE_BRIGHTNESS_DOWN=22）。此处经 NSEvent
/// 全局监听该事件，走与 F1 完全相同的截图流程。监听不可用（无辅助功能权限等）→
/// eprintln + `Ok(())`，绝不阻塞启动；标准 F1（Carbon）路径不受影响。
/// 必须在主线程调用（全局事件监听挂在 AppKit 事件循环上）。
#[cfg(target_os = "macos")]
pub fn register_media_key_f1(app: &AppHandle) -> Result<(), String> {
    // D0-①：hint 文案的热键名从设置读取（默认 F1，可改配——文案随之回显实际组合）。
    // 一次性读（注册期调用），非每事件读。
    let settings = crate::settings::Settings::load(app);
    let fnstate = media::read_fnstate();
    let monitor_ok = media::install_media_key_monitor(app);
    // macOS 的全局监听对按键类事件仅在被授予「辅助功能」时投递（NSEvent.h：见
    // AXIsProcessTrusted）——监听建立成功但未授权同样是"亮度键路径不通"。
    let usable = monitor_ok && media::ax_is_process_trusted();
    if !usable {
        eprintln!(
            "[hawksnap] 媒体键监听不可用（monitor={monitor_ok}，辅助功能授权未确认），F1 亮度键路径降级"
        );
        // 截图热键真失联（fnState 是媒体键模式 且 监听不可用）才提示；走 hotkey-error
        // 横幅通道，带「提示：」前缀与注册失败文案区分；已有真实注册错误时保留错误、
        // 仅 eprintln。载荷带 kind=capture（D0-⑤），前端归因为「截图」。
        if let Some(hint) = media::fnstate_hint(fnstate, &settings.hotkey_capture) {
            eprintln!("[hawksnap] 提示: {hint}");
            let mut slot = HOTKEY_ERROR.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                let err = HotkeyError {
                    kind: HotkeyKind::Capture,
                    message: format!("提示：{hint}"),
                };
                *slot = Some(err.clone());
                drop(slot);
                use tauri::Emitter;
                let _ = app.emit("hawk://hotkey-error", err);
            }
        }
    }
    Ok(())
}

/// 纯逻辑 + 媒体键运行时（macOS 专属部分见下方 cfg 门控）。
pub(crate) mod media {
    /// NSEvent systemDefined 的 data1：高 16 位 = NX 键类型；低 16 位 = flags
    /// （0x0A00=按下 / 0x0B00=释放，详见 parse_media_key 注）。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub const NX_KEYTYPE_BRIGHTNESS_DOWN: i64 = 22;

    #[derive(Debug, PartialEq)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub enum MediaKey {
        BrightnessDown,
    }

    /// 解析 systemDefined 事件的 data1；仅识别亮度下键的按下沿。
    ///
    /// data1 高 16 位 = NX 键类型；低 16 位 = flags。keyDown 有两种等价编码：
    /// `0x0A00`（真实硬件/系统事件，SPMediaKeyTap/Chromium/MonitorControl 一致采用：
    /// `((flags & 0xFF00) >> 8) == 0xA`）与 `0x000A`（本模块测试契约）。两者都接受；
    /// keyUp（`0x0B00` / `0x000B`）与其余键类型一律 None。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn parse_media_key(data1: i64) -> Option<MediaKey> {
        let key = (data1 & 0xFFFF_0000) >> 16;
        let flags = data1 & 0x0000_FFFF;
        let pressed = matches!(flags, 0x0A00 | 0x000A);
        if pressed && key == NX_KEYTYPE_BRIGHTNESS_DOWN {
            Some(MediaKey::BrightnessDown)
        } else {
            None
        }
    }

    /// fnState 提示：true=标准功能键（无需提示）；None=未设置（默认媒体键模式）；
    /// false=媒体键模式。`capture_combo` 是当前设置的截图热键（D0-① 单一来源，默认 F1），
    /// 只用于句首回显实际组合；后半句是 macOS 系统设置的原文案（Apple UI 字符串，不随
    /// 配置变化）。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn fnstate_hint(fnstate: Option<bool>, capture_combo: &str) -> Option<String> {
        match fnstate {
            Some(true) => None,
            _ => Some(format!(
                "{capture_combo} 当前是亮度键：可在 系统设置→键盘 开启「将 F1、F2 等键用作标准功能键」，\
                     或在 系统设置→隐私与安全性→辅助功能 中添加 HawkSnap（推荐，开箱即用）。"
            )),
        }
    }

    /// 300ms 去重闸门（进程级）：F1 与媒体键双通道竞态时只启动一次流程。
    /// `arm_once` 是纯状态转换（可测）；窗口内重复触发返回 false。
    pub(crate) fn arm_once(flag: &std::sync::atomic::AtomicBool) -> bool {
        use std::sync::atomic::Ordering;
        !flag.swap(true, Ordering::SeqCst)
    }

    pub(crate) fn disarm(flag: &std::sync::atomic::AtomicBool) {
        use std::sync::atomic::Ordering;
        flag.store(false, Ordering::SeqCst);
    }

    pub(crate) static TRIGGER_INFLIGHT: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// fnState 读取：`defaults read -g com.apple.keyboard.fnState`。
    /// 退出码 0 且输出 "1" → Some(true)（标准功能键）；退出码非 0（键不存在）→ None
    /// （默认媒体键模式）；其余输出 → Some(false)。
    #[cfg(target_os = "macos")]
    pub(crate) fn read_fnstate() -> Option<bool> {
        let out = std::process::Command::new("defaults")
            .args(["read", "-g", "com.apple.keyboard.fnState"])
            .output()
            .ok()?; // 命令执行失败 → 按未设置（默认媒体键模式）处理
        match out.status.code() {
            Some(0) => Some(String::from_utf8_lossy(&out.stdout).trim() == "1"),
            _ => None,
        }
    }

    /// 本进程是否已被授予「辅助功能」权限（公开 API，仅查询，不弹权限窗）。
    #[cfg(target_os = "macos")]
    pub(crate) fn ax_is_process_trusted() -> bool {
        #[link(name = "ApplicationServices", kind = "framework")]
        extern "C" {
            fn AXIsProcessTrusted() -> bool;
        }
        unsafe { AXIsProcessTrusted() }
    }

    /// 安装 NSEvent 全局监听（仅 systemDefined 事件）。为何不用 CGEventTap：项目依赖树里
    /// 的 core-graphics 0.25 的 CGEventType 枚举缺 SystemDefined(14) 变体，安全 API 无法
    /// 构造掩码，且实测现代 macOS 对合成 systemDefined 事件丢弃 kCGMouseEvent* 槽位写入
    /// （data1 无法从 CGEvent 字段可靠还原）；NSEvent 全局监听则把 data1 作为一等属性
    /// 暴露，语义由系统保证。监听 token 进程生命周期持有（泄漏 = 常驻，永不 remove）。
    /// 失败（返回 nil）→ false，由调用方降级处理。
    #[cfg(target_os = "macos")]
    pub(crate) fn install_media_key_monitor(app: &super::AppHandle) -> bool {
        use block2::RcBlock;
        use objc2_app_kit::NSEvent;
        use std::ptr::NonNull;

        // NSEventMaskSystemDefined = 1 << 14；监听对"其他应用收到的事件"生效——截图场景
        // 正是别的应用在前台，无需 local monitor。
        const NSEVENT_MASK_SYSTEM_DEFINED: u64 = 1 << 14;

        let app2 = app.clone();
        let handler = RcBlock::new(move |event: NonNull<NSEvent>| {
            // 回调由 AppKit 投递（线程不保证），保持最小：解析 + spawn，绝不建窗/关窗。
            // 亮度下键 → 与 F1 同一去重入口（trigger_capture_flow 内部再 spawn 流程线程）。
            // subtype==8 = NX_SUBTYPE_AUX_CONTROL_BUTTONS（媒体键家族，亮度键在此流），
            // 与 SPMediaKeyTap / Chromium / MonitorControl-MediaKeyTap 的判据一致。
            let ev = unsafe { event.as_ref() };
            if ev.subtype().0 == 8 {
                let data1 = ev.data1() as i64;
                if parse_media_key(data1).is_some() {
                    super::trigger_capture_flow(&app2);
                }
            }
        });
        // objc2 0.6 生成的绑定将此方法标为 safe（监听仅观察、不修改事件），直接调用
        let token = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
            objc2_app_kit::NSEventMask(NSEVENT_MASK_SYSTEM_DEFINED),
            &handler,
        );
        match token {
            Some(t) => {
                // 泄漏监听 token：与进程同寿命（tap/handler 引用需进程生命周期存活）
                std::mem::forget(t);
                true
            }
            None => false,
        }
    }
}

/// 拉取并清除最近一次热键注册错误（None = 无错误）；补上 setup 期早于前端 listen 的丢失。
/// 载荷 `{kind, message}`（camelCase，D0-⑤）：前端按 kind 归因横幅。
#[tauri::command]
pub fn hotkey_error() -> Option<HotkeyError> {
    HOTKEY_ERROR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

// —— Task 13：媒体键纯逻辑测试 ——
#[cfg(test)]
mod media_tests {
    use super::media::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn parses_brightness_down_from_system_defined() {
        // NSEvent systemDefined: data1 高 16 位为 NX 键类型，低 16 位为按下/释放标志
        let data1 = (22i64 << 16) | 0x000A; // NX_KEYTYPE_BRIGHTNESS_DOWN=22, keyDown flag=0xA
        assert_eq!(parse_media_key(data1), Some(MediaKey::BrightnessDown));
    }

    #[test]
    fn ignores_other_media_keys_and_releases() {
        assert_eq!(parse_media_key((23i64 << 16) | 0x000A), None); // BRIGHTNESS_UP
        assert_eq!(parse_media_key((22i64 << 16) | 0x000B), None); // keyUp flag
    }

    #[test]
    fn hint_only_when_fnstate_not_standard() {
        assert!(fnstate_hint(Some(false), "F1").is_some()); // 媒体键模式 → 提示
        assert!(fnstate_hint(None, "F1").is_some());        // 未设置（默认）→ 提示
        assert!(fnstate_hint(Some(true), "F1").is_none());  // 标准功能键 → 无提示
    }

    #[test]
    fn hint_echoes_configured_capture_combo() {
        // D0-①：句首热键名随设置回显；系统设置原文案保持不变
        let h = fnstate_hint(Some(false), "Cmd+Shift+A").unwrap();
        assert!(h.starts_with("Cmd+Shift+A 当前是亮度键"), "{h}");
        assert!(h.contains("「将 F1、F2 等键用作标准功能键」"), "{h}");
    }

    #[test]
    fn dedupe_arms_once() {
        // 去重闸门纯逻辑：首次置位成功，窗口内（未 disarm 前）再触发被拒
        let flag = AtomicBool::new(false);
        assert!(arm_once(&flag));
        assert!(!arm_once(&flag));
        disarm(&flag);
        assert!(arm_once(&flag));
    }
}
