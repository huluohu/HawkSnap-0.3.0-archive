//! 录屏 app 层：状态机 + 命令 + 悬浮条窗口 + tick 广播 + 历史入库。
//!
//! ## 范围演进
//!
//! M6 首版 = F6 直接录制整个主显示器（无选区，区域录制 scope cut 归 M3）。**M3 N1
//! 兑现区域录制**：F6 统一进入覆盖层录制选区模式（拖选=区域、Enter/双击=整屏快捷
//! 确认、Esc=取消，旧「直接全屏」语义废除），确认路径走 [`record_start_region`]；
//! 全屏路径 [`record_start`] 命令**保留给 API 兼容**（plan Interfaces 裁定）。
//! 悬浮条 320×56 常驻复用（与 countdown 同款 show/hide；M6 首版 280 宽，N4 加
//! 按键显示开关后 280 容不下——顶部居中定位不变，宽度自适应内容），区域录制在条上追加
//! 「区域 W×H」标题（选区逻辑尺寸，与覆盖层选区标签同口径）。**N2 输出格式**：
//! 覆盖层录制横幅的 MP4/GIF 分段开关 → 会话 `format`（中途不可切）；GIF 会话 =
//! MP4 管线原样 + 旁路抽帧（平台侧 GifLane 32MB 有界队列），stop 时编码 .gif
//! 入库（kind 同 recording），编码失败降级保留 MP4（见 [`record_stop`] 注）。
//!
//! ## 状态机（纯逻辑可测）
//!
//! `Idle →(start) Recording ⇄(pause/resume) Paused →(stop) Idle`；非法转换返回 Err
//! （前端按钮与 F6 toggle 都先查 `record_status`，双保险）。计时 = 挂钟 − 暂停累计
//! （暂停中含当段），由 Rust 每秒广播 `hawk://record-tick {seconds, paused}`，悬浮条
//! 纯渲染——时间语义单源在 Rust（前端不做本地计时，规避 webview 节流）。
//!
//! ## 线程纪律
//!
//! 命令全部 `#[tauri::command(async)]`（record_stop 内部阻塞收尾 ≤25s：stopCapture
//! 10s + finishWriting 15s——async 命令在运行时线程，不卡事件循环/主线程）。悬浮条
//! 建窗禁主线程（与 countdown 同款），全部入口 spawn/async 保证非主线程。
//!
//! ## 音频裁定（ladder 首版口径）
//!
//! 系统声（SCStream `capturesAudio`）进 AAC 音轨；**麦克风 M6 不做**（AVAudioEngine
//! tap + 双路混音过深，计划预授权 ladder 的该项裁定，列 M2.1/M3）。

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

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use hawksnap_core::geometry::Rect;
// 输出路径生成只在 macOS 分支里被调用（record_start / record_start_region 的
// 录制体整段 `#[cfg(target_os = "macos")]`），非 macOS 构建下本导入无人使用 →
// 按 target 门控，避免占用 crate 级 unused_imports 放行额度。
#[cfg(target_os = "macos")]
use hawksnap_platform_mac::record::recording_output_path;

#[cfg(target_os = "macos")]
use hawksnap_platform_mac::record::{PipLane, PipUpdate};

// ---------------------------------------------------------------------------
// 纯逻辑（可测）：状态机 + 计时
// ---------------------------------------------------------------------------

/// 录制输出格式（N2）：overlay 录制模式横幅的分段开关（MP4 | GIF，默认 MP4）→
/// record_start / record_start_region 的 `format` 参数 → 会话。MP4 = 既有管线
/// 零变化（无抽帧热路径成本）；GIF = MP4 管线原样 + 旁路抽帧通道（gif.rs 编码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordFormat {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Mp4,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Gif,
}

impl RecordFormat {
    /// tick/status 载荷值（camelCase 契约为单个小写词；悬浮条据此显示模式标签）。
    pub fn as_payload_str(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Gif => "gif",
        }
    }
}

/// 解析命令入参（纯函数可测）：大小写不敏感；非法值显式拒绝（不静默降级——
/// 用户选了 GIF 却录出 MP4 是错误行为）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn parse_record_format(s: &str) -> Result<RecordFormat, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "mp4" => Ok(RecordFormat::Mp4),
        "gif" => Ok(RecordFormat::Gif),
        _ => Err(format!("输出格式非法: {s}（仅支持 mp4 / gif）")),
    }
}

/// 录屏阶段。与平台 record.rs 的 phase 数字（0/1/2）解耦——这里是用户可见语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Recording,
    Paused,
}

/// 用户动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecAction {
    /// 启动。N1 起 F6 空闲路径改走覆盖层录制选区流程（不经理 Start 装机），Start
    /// 在生产中暂无构造点——仍属状态机纯契约（转移矩阵测试构造之），并是
    /// record_start 兼容命令未来重连状态机的既有落点。
    #[cfg_attr(not(test), allow(dead_code))]
    Start,
    Pause,
    Resume,
    Stop,
}

/// 状态机转移：非法转换 → Err（消息面向 F6/悬浮条按钮的错误横幅）。
pub fn transition(phase: Phase, action: RecAction) -> Result<Phase, String> {
    match (phase, action) {
        (Phase::Idle, RecAction::Start) => Ok(Phase::Recording),
        (Phase::Recording, RecAction::Pause) => Ok(Phase::Paused),
        (Phase::Paused, RecAction::Resume) => Ok(Phase::Recording),
        (Phase::Recording | Phase::Paused, RecAction::Stop) => Ok(Phase::Idle),
        (Phase::Idle, RecAction::Pause | RecAction::Resume) => {
            Err("没有进行中的录屏".into())
        }
        (Phase::Idle, RecAction::Stop) => Err("没有进行中的录屏".into()),
        (Phase::Recording, RecAction::Start) => Err("已有录屏进行中".into()),
        (Phase::Paused, RecAction::Start) => Err("录屏已暂停（先恢复或停止）".into()),
        (Phase::Recording, RecAction::Resume) | (Phase::Paused, RecAction::Pause) => {
            // 幂等方向：F6 双通道/悬浮条双击的良性竞态——语义上无害，直接照办。
            Ok(phase)
        }
    }
}

/// 计时秒数：挂钟活跃时长 − 暂停累计（暂停中含当段）。纯 Duration 运算可测。
pub fn elapsed_secs(active: Duration, paused_total: Duration, current_pause: Option<Duration>) -> u64 {
    let paused = paused_total + current_pause.unwrap_or_default();
    active.saturating_sub(paused).as_secs()
}

/// 录制时长（秒，f64 亚秒精度）：与 [`elapsed_secs`] 同一语义（挂钟活跃时长 −
/// 暂停累计，暂停中含当段），仅精度不同——入库 duration 列（M3 继承项①）取
/// REAL 秒，stop 收尾前调用（stop 阻塞 ≤25s 不得计入）。纯 Duration 运算可测。
pub fn recorded_secs(active: Duration, paused_total: Duration, current_pause: Option<Duration>) -> f64 {
    active
        .saturating_sub(paused_total + current_pause.unwrap_or_default())
        .as_secs_f64()
}

// ---------------------------------------------------------------------------
// 运行态（static Mutex<Option<...>>：None = Idle；与 HOTKEY_ERROR/DELAY_ACTIVE 同款）
// ---------------------------------------------------------------------------

struct RecRuntime {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    session: u64,
    phase: Phase,
    started_at: Instant,
    paused_total: Duration,
    pause_started: Option<Instant>,
    #[cfg(target_os = "macos")]
    recorder: hawksnap_platform_mac::record::ScreenRecorder,
    output_path: std::path::PathBuf,
    /// N1：区域录制的选区逻辑尺寸（悬浮条「区域 W×H」标题的数据源，与覆盖层选区
    /// 标签同口径——逻辑 px，非物理）；None = 全屏录制（record_start 兼容路径）。
    region: Option<(u32, u32)>,
    /// N2：输出格式（会话维度定死——录制中途不可切换；悬浮条标签的数据源）。
    format: RecordFormat,
    /// N2：GIF 旁路通道（format=GIF 时与平台 ScreenRecorder 共享同一 Arc）。
    /// stop 时由 record_stop drain（编码在 gif.rs）；MP4 会话恒 None。
    #[cfg(target_os = "macos")]
    gif_lane: Option<std::sync::Arc<hawksnap_platform_mac::record::GifLane>>,
    /// N3：画中画共享道（每会话恒 Some——悬浮条 pip_update 经它实时改状态；PiP
    /// 默认 visible=false，未开启时热路径零变化）。平台 ScreenRecorder 持同一 Arc。
    #[cfg(target_os = "macos")]
    pip_lane: Option<std::sync::Arc<PipLane>>,
}

static REC: Mutex<Option<RecRuntime>> = Mutex::new(None);

/// 会话单调计数（M7）：每次**成功启动**（REC 安装时）取号 +1，随 tick 广播。前端以
/// session 变化识别「新会话」并清除上一会话的行内错误（M6 遗留 minor：失败文案跨
/// 会话残留）。Relaxed 足够：仅需唯一 + 单调，取号在 REC 锁内（启动天然串行），
/// 载荷同步由事件通道自身保证。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
static SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn lock_rec() -> std::sync::MutexGuard<'static, Option<RecRuntime>> {
    REC.lock().unwrap_or_else(|e| e.into_inner())
}

/// 互斥守卫（终审 wave）：录屏是否进行中（Recording / Paused 均算，含收尾阻塞窗）。
/// F6 截图侧（start_capture_flow）据此拒绝在录屏进行中启动截图流程。
pub fn recording_active() -> bool {
    lock_rec().is_some()
}

/// F6 启动路径的守卫文案（纯函数可测）：截图覆盖层可见（一轮截图进行中）时返回
/// Some(拒绝文案)，否则放行 None。热键处理线程据此先拒后启（互斥守卫 a）。
pub fn f6_start_guard_msg(overlay_visible: bool) -> Option<&'static str> {
    if overlay_visible {
        Some("请先完成或取消当前截图再录屏")
    } else {
        None
    }
}

/// hawk://record-tick 载荷（悬浮条渲染输入；单词字段无需 rename）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct RecordTick {
    /// 会话号（record_start/record_start_region 单调取号）：前端据此清上一会话的错误残留。
    session: u64,
    seconds: u64,
    paused: bool,
    /// N1：区域录制选区逻辑尺寸（条上「区域 W×H」标题）；None = 全屏（序列化时
    /// 整字段缺席——前端以 regionW in payload 判别，不与 null 纠缠）。
    #[serde(skip_serializing_if = "Option::is_none")]
    region_w: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region_h: Option<u32>,
    /// N2：输出格式（"mp4"|"gif"；悬浮条模式标签数据源，每会话定死）。
    format: &'static str,
}

/// pull 通道载荷（悬浮条 mount 拉取补显——push 早于 listen 必丢，同 countdown）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordStatus {
    pub seconds: u64,
    pub paused: bool,
    pub active: bool,
    /// N1：区域录制选区逻辑尺寸（同 RecordTick 语义；skip 序列化，前端可选读取）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region_w: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region_h: Option<u32>,
    /// N2：输出格式（"mp4"|"gif"，同 RecordTick）。
    pub format: &'static str,
}

#[tauri::command]
pub fn record_status() -> Option<RecordStatus> {
    let guard = lock_rec();
    let rt = guard.as_ref()?;
    let current_pause = rt
        .pause_started
        .map(|t| t.elapsed());
    Some(RecordStatus {
        seconds: elapsed_secs(rt.started_at.elapsed(), rt.paused_total, current_pause),
        paused: rt.phase == Phase::Paused,
        active: true,
        region_w: rt.region.map(|r| r.0),
        region_h: rt.region.map(|r| r.1),
        format: rt.format.as_payload_str(),
    })
}

// ---------------------------------------------------------------------------
// 输出目录：~/Movies/HawkSnap（spec §3.4；dirs_picture_dir 同款跨平台 HOME 口径）
// ---------------------------------------------------------------------------

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn movies_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| std::path::PathBuf::from(h).join("Movies/HawkSnap"))
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

/// F6 / 悬浮条入口：启动全屏（主显示器）录制 + 拉起悬浮条 + 启动 tick 广播。
/// N2：`format` = "mp4"|"gif"（None/缺省 = mp4——API 兼容路径的既有调用方零变化）。
#[tauri::command(async)]
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn record_start(app: AppHandle, format: Option<String>) -> Result<(), String> {
    // 平台门槛（范围裁定）：录屏管线仅 macOS；其余平台显式拒绝（不静默）。
    #[cfg(not(target_os = "macos"))]
    return Err("仅 macOS 支持录屏（跨平台适配归后续计划）".into());

    #[cfg(target_os = "macos")]
    {
    let fmt = match format.as_deref() {
        None => RecordFormat::Mp4,
        Some(s) => parse_record_format(s)?,
    };
    // 独占检查：构造（ScreenRecorder::start 阻塞百 ms 级）在锁外完成后装入；
    // 并发第二请求在锁上排队后走 is_some 拒绝 + 半成品清理。
    {
        let guard = lock_rec();
        if guard.is_some() {
            return Err("已有录屏进行中".into());
        }
    }

    // 屏幕录制权限预检（Plan D4 同款：拒绝 → 深链系统设置 + Err 文案）。
    #[cfg(target_os = "macos")]
    if !hawksnap_platform_mac::permissions::screen_recording_prechecked() {
        if let Err(e) = hawksnap_platform_mac::permissions::open_screen_recording_settings() {
            eprintln!("[hawksnap] 打开屏幕录制权限设置失败: {e}");
        }
        return Err("屏幕录制权限未授予：已为你打开系统设置，授权后重新按 F6 即可".into());
    }

    let base = movies_dir().ok_or_else(|| "无法定位用户目录（HOME/USERPROFILE 均缺失）".to_string())?;
    let path = recording_output_path(&base, chrono::Local::now())?;

    // **先建悬浮条窗口，再启动流**（终审修复的自排除前置，platform 侧 start() 注②）：
    // SCShareableContent.applications 只列出「名下有窗口」的进程——若先 start 再建窗，
    // 枚举瞬间本进程可能还零窗口 → excludingApplications 降级为空 → 悬浮条被录进
    // MP4（正是本修复要杀的 bug）。窗口 build 返回时 NSWindow 已在主线程落成，
    // 此后 ScreenRecorder::start 内的枚举必然能按 PID 匹配到本进程。
    show_recording_bar(&app)?;

    // N2：gif 会话先建旁路通道，与 ScreenRecorder 共享同一 Arc（回调生产 / stop 消费）。
    // N3：画中画道每会话必建（PiP 默认关；悬浮条 pip_update 经共享 Arc 实时改）。
    let gif_lane = match fmt {
        RecordFormat::Gif => Some(std::sync::Arc::new(
            hawksnap_platform_mac::record::GifLane::new(),
        )),
        RecordFormat::Mp4 => None,
    };
    #[cfg(target_os = "macos")]
    let pip_lane = Some(std::sync::Arc::new(PipLane::new()));
    // 构造录屏器（阻塞至 startCapture 完成，~百 ms 级；async 命令线程，不卡主线程）。
    let recorder = match match fmt {
        RecordFormat::Gif => hawksnap_platform_mac::record::ScreenRecorder::start_gif(
            path.clone(),
            gif_lane.clone().expect("gif 分支已建通道"),
            pip_lane.clone(),
        ),
        RecordFormat::Mp4 => {
            hawksnap_platform_mac::record::ScreenRecorder::start(path.clone(), pip_lane.clone())
        }
    } {
        Ok(r) => r,
        Err(e) => {
            // 启动失败：回收刚拉起的悬浮条（无录制不残留 UI；与成功停止路径的
            // close_recording_bar 同款 best-effort）。并发守卫（M3 继承项⑥）：
            // 若此刻已有对手装机（REC 非空），条已属于对手——**不碰**（关闭会杀掉
            // 胜者正在用的条）；无对手时条是本请求自建的，照常回收。
            if lock_rec().is_none() {
                close_recording_bar(&app);
            }
            return Err(e);
        }
    };

    {
        let mut guard = lock_rec();
        if guard.is_some() {
            // 并发败者路径（M3 继承项⑥，理论不可达：锁外无 await 点）：第二请求
            // 落在锁检查与装机之间。**只清自己的半成品**（stop + 删临时文件），
            // 绝不触碰悬浮条——bar 是进程单例，此刻属于先装机的胜者，此前的
            // close_recording_bar 会把胜者正在使用的条关掉（0.2.0 缺陷）。败者
            // 前面的 show_recording_bar 对同一窗口是幂等 show，无残留需要回收；
            // 胜者停止时自会关条。返回 Err 后败者侧由 record-error 横幅呈现。
            let _ = recorder.stop();
            let _ = std::fs::remove_file(&path);
            return Err("已有录屏进行中".into());
        }
        // 会话取号在安装锁内：只有成功启动才消耗序号（失败路径不跳号不影响单调）。
        let session = SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        guard.replace(RecRuntime {
            session,
            phase: Phase::Recording,
            started_at: Instant::now(),
            paused_total: Duration::ZERO,
            pause_started: None,
            #[cfg(target_os = "macos")]
            recorder,
            output_path: path,
            region: None, // 全屏录制（兼容路径）：无区域标题
            format: fmt,
            #[cfg(target_os = "macos")]
            gif_lane,
            #[cfg(target_os = "macos")]
            pip_lane,
        });
    }

    // 立即广播首 tick：条新建分支 mount 时 pull 早于装机（record_status 返 None），
    // 推通道补上 0 秒起点；条复用分支同步清上一会话的「区域」标题残留（N1）。
    emit_initial_tick(&app);
    // 悬浮条已在启动前拉起（上方，自排除前置）；此处补 tick 广播线程。
    spawn_tick_thread(app);
    Ok(())
    }
}

/// N1 区域录制启动（覆盖层 record 模式确认路径）：F6 拖选（或 Enter/双击整屏快捷
/// 确认，rect = 显示器全边界）→ 本命令。TCC 预检 → 悬浮条先建（自排除前置，同
/// record_start）→ ScreenRecorder::start_region(display_id, 逻辑 rect) → 装机 +
/// tick 线程 + 隐藏**全部**覆盖层（多显示器语义：只录选区所在屏，他屏覆盖层随
/// hide_all_overlays 一并收口；意图槽同步清除）。
///
/// 成功时覆盖层由本命令收口（前端无需再 hide）；失败时覆盖层**保留**（可重试/
/// Esc 退出），Err 回流 → 前端解锁在途守卫 + 行内错误。rect 为 display 本地逻辑
/// 坐标（与 finish_capture 的 FinishReq.rect 同系同源），物理换算与 sourceRect
/// 坐标裁定在平台侧（record.rs region_stream_params，points 口径）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionReq {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub display_id: u64,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub rect: Rect,
    /// N2：输出格式（"mp4"|"gif"）。serde default = "mp4"——旧载荷（N1 契约）
    /// 缺字段时按 MP4 走，前端可渐进接入。
    #[serde(default = "default_region_format")]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub format: String,
}

fn default_region_format() -> String {
    "mp4".into()
}

#[tauri::command(async)]
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn record_start_region(app: AppHandle, req: RegionReq) -> Result<(), String> {
    // 平台门槛（同 record_start）：录屏管线仅 macOS。
    #[cfg(not(target_os = "macos"))]
    return Err("仅 macOS 支持录屏（跨平台适配归后续计划）".into());

    #[cfg(target_os = "macos")]
    {
    if lock_rec().is_some() {
        return Err("已有录屏进行中".into());
    }
    if req.rect.w < 1 || req.rect.h < 1 {
        return Err(format!("选区尺寸无效: {}x{}", req.rect.w, req.rect.h));
    }
    let display_id = u32::try_from(req.display_id)
        .map_err(|_| format!("显示器 id 非法: {}", req.display_id))?;
    let fmt = parse_record_format(&req.format)?;

    // 屏幕录制权限预检（同 record_start：拒绝 → 深链系统设置 + Err 文案）。
    if !hawksnap_platform_mac::permissions::screen_recording_prechecked() {
        if let Err(e) = hawksnap_platform_mac::permissions::open_screen_recording_settings() {
            eprintln!("[hawksnap] 打开屏幕录制权限设置失败: {e}");
        }
        return Err("屏幕录制权限未授予：已为你打开系统设置，授权后重新按 F6 即可".into());
    }

    let base = movies_dir().ok_or_else(|| "无法定位用户目录（HOME/USERPROFILE 均缺失）".to_string())?;
    let path = recording_output_path(&base, chrono::Local::now())?;

    // **先建悬浮条窗口，再启动流**（自排除前置，record_start 同款论证：SCShareable
    // Content.applications 只列出「名下有窗口」的进程，先 start 再建窗会让排除降级
    // 为空、录制条入画）。区域录制同受此约束——覆盖层虽也是本应用窗口且此刻可见，
    // 但排除按整个应用生效，与条的可见性互不影响。
    show_recording_bar(&app)?;

    let region = hawksnap_platform_mac::record::LogicalRect {
        x: req.rect.x as f64,
        y: req.rect.y as f64,
        w: req.rect.w as f64,
        h: req.rect.h as f64,
    };
    // N2：gif 会话先建旁路通道（与 record_start 同款共享 Arc）；N3：pip 道每会话必建。
    let gif_lane = match fmt {
        RecordFormat::Gif => Some(std::sync::Arc::new(
            hawksnap_platform_mac::record::GifLane::new(),
        )),
        RecordFormat::Mp4 => None,
    };
    #[cfg(target_os = "macos")]
    let pip_lane = Some(std::sync::Arc::new(PipLane::new()));
    let recorder = match match fmt {
        RecordFormat::Gif => hawksnap_platform_mac::record::ScreenRecorder::start_region_gif(
            path.clone(),
            display_id,
            region,
            gif_lane.clone().expect("gif 分支已建通道"),
            pip_lane.clone(),
        ),
        RecordFormat::Mp4 => hawksnap_platform_mac::record::ScreenRecorder::start_region(
            path.clone(),
            display_id,
            region,
            pip_lane.clone(),
        ),
    } {
        Ok(r) => r,
        Err(e) => {
            // 启动失败：回收刚拉起的悬浮条（并发守卫同 record_start——条只属于
            // 无对手的本请求）；覆盖层不 hide（保留选区供重试）。
            if lock_rec().is_none() {
                close_recording_bar(&app);
            }
            return Err(e);
        }
    };

    {
        let mut guard = lock_rec();
        if guard.is_some() {
            // 并发败者路径（同 record_start）：只清自己的半成品，不碰悬浮条/覆盖层。
            let _ = recorder.stop();
            let _ = std::fs::remove_file(&path);
            return Err("已有录屏进行中".into());
        }
        let session = SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        guard.replace(RecRuntime {
            session,
            phase: Phase::Recording,
            started_at: Instant::now(),
            paused_total: Duration::ZERO,
            pause_started: None,
            #[cfg(target_os = "macos")]
            recorder,
            output_path: path,
            // 悬浮条标题用选区**逻辑**尺寸（与覆盖层选区标签同口径，用户所见一致）。
            region: Some((req.rect.w, req.rect.h)),
            format: fmt,
            #[cfg(target_os = "macos")]
            gif_lane,
            #[cfg(target_os = "macos")]
            pip_lane,
        });
    }

    // 首 tick（0 秒起点 + 区域标题立即上条，同 record_start 论证）+ tick 线程。
    emit_initial_tick(&app);
    spawn_tick_thread(app.clone());

    // 录制已开始 → 全部覆盖层收口（他屏覆盖层一并隐藏；CAPTURES 整帧内存释放；
    // CaptureIntent 清槽——确认出口与 cancel/emit_capture_done 走同一收口函数）。
    if let Err(e) = crate::overlay::hide_all_overlays(&app) {
        eprintln!("[hawksnap] 区域录制启动后隐藏覆盖层失败: {e}");
    }
    Ok(())
    }
}

/// 暂停：SCStream 回调丢帧 + 计时冻结；恢复时时间戳平移（平台侧实现）。
#[tauri::command(async)]
pub fn record_pause() -> Result<(), String> {
    let mut guard = lock_rec();
    let rt = guard.as_mut().ok_or_else(|| "没有进行中的录屏".to_string())?;
    rt.phase = transition(rt.phase, RecAction::Pause)?;
    if rt.pause_started.is_none() {
        rt.pause_started = Some(Instant::now());
    }
    #[cfg(target_os = "macos")]
    rt.recorder.pause();
    Ok(())
}

/// 恢复：时间戳平移在下一帧兑现（平台侧 PauseClock）。
#[tauri::command(async)]
pub fn record_resume() -> Result<(), String> {
    let mut guard = lock_rec();
    let rt = guard.as_mut().ok_or_else(|| "没有进行中的录屏".to_string())?;
    rt.phase = transition(rt.phase, RecAction::Resume)?;
    if let Some(t) = rt.pause_started.take() {
        rt.paused_total += t.elapsed();
    }
    #[cfg(target_os = "macos")]
    rt.recorder.resume();
    Ok(())
}

/// N3 画中画实时调整（悬浮条开关；未来拖拽同通道）：部分更新语义——缺席字段保持
/// 现值。visible=true 时平台侧完成 TCC 预检 + AVCaptureSession 启动；相机不可用/
/// 权限拒绝 → Err 回流（悬浮条瞬态错误提示），**visible 回落 false、录制继续**
/// （LADDER：相机缺席 = PiP 静默缺席，绝不阻塞录制）。x/y/size 只改共享状态——
/// SCStream 视频回调逐帧读取，下一帧生效（≤33ms @30fps）。暂停期间同样可调
/// （回调丢帧中，恢复后下一帧生效）。
#[tauri::command(async)]
pub fn pip_update(
    x: Option<Option<f64>>,
    y: Option<Option<f64>>,
    size: Option<f64>,
    visible: Option<bool>,
) -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (x, y, size, visible);
        return Err("仅 macOS 支持画中画".into());
    }
    #[cfg(target_os = "macos")]
    {
        // 只在锁内做「取 lane 引用」——**drop(guard) 后再 update**（评审 Important 修复）：
        // update 内部的首次开启会走 TCC 同步等待（≤15s）+ startRunning（百 ms-1s 级），
        // 持 REC 锁跨这段会让悬浮条计时/暂停/停止整窗无响应。PipLane 内部自同步
        // （状态/相机各一把 Mutex），锁外调用安全；REC 取出（stop 路径 take）与
        // update 的竞态由 Arc 存活语义兜底——lane 归 stop 后的收口线程拥有，update
        // 只碰其中自同步的状态/相机锁，无悬垂。
        let lane = {
            let guard = lock_rec();
            guard
                .as_ref()
                .ok_or_else(|| "没有进行中的录屏".to_string())?
                .pip_lane
                .clone()
                .ok_or_else(|| "本会话不支持画中画".to_string())?
        };
        lane.update(PipUpdate { x, y, size, visible })
    }
}

/// 停止：writer 收尾（阻塞 ≤25s）→（gif 会话）drain 旁路队列 gifski 编码（硬超时
/// 60s，正常 ≤10s——F8 预算）→ 历史入库（kind=recording，thumb 留空——M0 网格对
/// 空 thumbPath 已有占位渲染）→ 关闭悬浮条窗口（终审修复，见函数体注；失败路径
/// 条保持可见展示行内错误）。返回产物路径（gif 会话 = .gif；编码失败降级 = MP4）。
///
/// N2 gif 分支语义：MP4 管线始终收尾（抽帧依赖它的时间线）；GIF 编码成功后删除
/// MP4 中间产物（用户选定格式 = GIF，目录不残留隐形文件），历史行指向 .gif
/// （duration = 末帧归零 PTS——GIF 时间线）；编码失败/无帧 → 保留 MP4 原样入库
/// （「录制必有可用产物」不变量，GIF 静默缺席）。
#[tauri::command(async)]
pub fn record_stop(app: AppHandle) -> Result<String, String> {
    // N4 按键显示随录停止（不变量「tap 只在录制中存活」的执行点，best-effort）：
    // **放在状态机 take/转移之前**（评审修复 Minor 8）——转移失败（未来扩展的非法
    // 态）等早退路径也必须让 tap 死掉，绝不留系统级监听过夜。stop 收尾（≤25s
    // 阻塞）之前完成：录屏一停 UI 即收，不等 writer。
    crate::keycast::keycast_disable(&app);

    let rt = {
        let mut guard = lock_rec();
        let rt = guard.take().ok_or_else(|| "没有进行中的录屏".to_string())?;
        // 转移合法性（Idle 态 take 不到 → 已覆盖；此处防未来扩展）。
        transition(rt.phase, RecAction::Stop)?;
        rt
    };

    // 悬浮条回收（终审修复，webview 泄漏）：成功收尾 + 历史入库后**关闭**（而非隐藏）
    // 悬浮条窗口——常驻隐藏的 webview 永不回收（每屏一份 WKWebView 常驻内存），
    // 与 overlay 的复用/销毁取舍同款：下次 record_start 走 show_recording_bar 的
    // 懒建分支冷加载重建（~300ms，每次录制一次，可接受）。失败路径**不关**（错误
    // 行内必须可见）；失败残留的条被下次启动 show 复用、在下次**成功**停止时关闭，
    // 若下次也失败则继续留着（正确——错误仍可见）。
    // close 须非主线程（record_stop 为 async 命令，运行时线程 ✓）；不入 idle_reaper
    //（close 语义更干净，reaper 是「隐藏满 10 分钟」的兜底，此处有明确生命周期点）。

    // 录制时长（M3 继承项①，入库 duration 列）：stop 收尾**前**取时刻——
    // recorder.stop() 阻塞 ≤25s 不得计入；口径与 tick/record_status 一致。
    // 注意：gif 会话的实际入库时长换成 GIF 时间线（末帧 PTS），此处值只作降级兜底。
    let duration_secs = recorded_secs(
        rt.started_at.elapsed(),
        rt.paused_total,
        rt.pause_started.map(|t| t.elapsed()),
    );
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    let format = rt.format;
    #[cfg(target_os = "macos")]
    let gif_lane = rt.gif_lane.clone(); // Arc 克隆：lane 本体仍随 rt 存活到 stop 完成

    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    let (path, info) = {
        #[cfg(target_os = "macos")]
        {
            let info = rt.recorder.stop()?;
            (rt.output_path, info)
        }
        #[cfg(not(target_os = "macos"))]
        {
            (rt.output_path, ())
        }
    };

    // N2：gif 会话 → drain 旁路队列编码 .gif（成功 = 删 MP4 中间产物、产物换
    // .gif、时长换 GIF 时间线；失败 = eprintln + MP4 降级，产物与入库均不动）。
    #[cfg(target_os = "macos")]
    let (artifact, w, h, dur) = if format == RecordFormat::Gif {
        match crate::gif::finish_gif_recording(gif_lane.as_ref(), &path) {
            Ok(g) => {
                // GIF 已成型 → MP4 中间产物删除（best-effort：删除失败仅留痕，
                // 不失败整个 stop——产物已入库，多余文件不是错误）。
                if let Err(e) = std::fs::remove_file(&path) {
                    eprintln!("[hawksnap] 删除 GIF 会话的 MP4 中间产物失败: {e}");
                }
                (g.path, g.width as i64, g.height as i64, g.duration)
            }
            Err(e) => {
                eprintln!("[hawksnap] GIF 编码失败，保留 MP4 产物: {e}");
                (path.clone(), info.width as i64, info.height as i64, duration_secs)
            }
        }
    } else {
        (path.clone(), info.width as i64, info.height as i64, duration_secs)
    };
    #[cfg(not(target_os = "macos"))]
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    let (artifact, w, h, dur) = (path.clone(), 0i64, 0i64, duration_secs);

    // 历史入库（旁路语义：失败仅记录，不影响录制产物返回——与历史库总裁定一致）。
    // kind="recording" + duration=录制秒数（M3 继承项①：kind 列语义修正——此前
    // "recording" 塞在 app 列冒充 kind；prune 平权由 history::record 内置裁剪兑现，
    // 录屏不再绕过 500 条保留策略）。app 列保持 "recording"（网格展示口径不变）。
    // N2：gif 产物同 kind（spec 分组语义）、w/h/duration 取 GIF 实际值。
    #[cfg(target_os = "macos")]
    if let Err(e) = crate::history::record(
        &app,
        &artifact.to_string_lossy(),
        "",
        w,
        h,
        "recording",
        "recording",
        Some(dur),
    ) {
        eprintln!("[hawksnap] 录屏历史入库失败: {e}");
    }

    close_recording_bar(&app);

    Ok(artifact.to_string_lossy().into_owned())
}

/// F6 toggle 旧入口已随 N1 移除：F6 完整语义改由 overlay::intent::f6_route 分派
/// （录制中 → record_stop；空闲 → start_record_selection_flow 进入选区模式），
/// 全屏 record_start 命令保留给 API 兼容。

// ---------------------------------------------------------------------------
// tick 广播线程：每秒 hawk://record-tick；Idle（None）即退（每次启动新线程）。
// ---------------------------------------------------------------------------

/// 装机后立即广播首 tick（0 秒起点）：条新建分支 mount 时 pull 早于装机（record_status
/// 返 None），推通道补上起点；条复用分支同步刷新区域标题（N1：清上一会话残留）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn emit_initial_tick(app: &AppHandle) {
    let tick = {
        let guard = lock_rec();
        let Some(rt) = guard.as_ref() else { return };
        RecordTick {
            session: rt.session,
            seconds: 0,
            paused: false,
            region_w: rt.region.map(|r| r.0),
            region_h: rt.region.map(|r| r.1),
            format: rt.format.as_payload_str(),
        }
    };
    let _ = app.emit("hawk://record-tick", tick);
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn spawn_tick_thread(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        let tick = {
            let guard = lock_rec();
            let Some(rt) = guard.as_ref() else { break };
            let current_pause = rt.pause_started.map(|t| t.elapsed());
            RecordTick {
                session: rt.session,
                seconds: elapsed_secs(rt.started_at.elapsed(), rt.paused_total, current_pause),
                paused: rt.phase == Phase::Paused,
                region_w: rt.region.map(|r| r.0),
                region_h: rt.region.map(|r| r.1),
                format: rt.format.as_payload_str(),
            }
        };
        let _ = app.emit("hawk://record-tick", tick);
    });
}

// ---------------------------------------------------------------------------
// 悬浮条窗口（常驻复用，与 countdown 同款 show/hide；320×56 深色实心——N4 加按键
// 显示开关后由 280 加宽，见模块注）
// ---------------------------------------------------------------------------

/// 位置裁定：主显示器顶部居中（录制条贴着屏幕上缘不遮挡内容区）；
/// 主显示器不可得 → center() 兜底（OS 当前屏居中，零假设）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn show_recording_bar(app: &AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("recording-bar") {
        position_bar(&w);
        let _ = w.show();
        return Ok(());
    }
    let win = WebviewWindowBuilder::new(
        app,
        "recording-bar",
        WebviewUrl::App("recording-bar?window=recording-bar".into()),
    )
    .inner_size(320.0, 56.0)
    .decorations(false)
    .always_on_top(true)
    .resizable(false)
    .skip_taskbar(true)
    .transparent(false) // 深色实心底（plan：非透明）
    .visible(false)     // 先定位再 show，杜绝默认位置闪现（countdown 同款）
    .build()
    .map_err(|e| e.to_string())?;
    position_bar(&win);
    let _ = win.show();
    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn position_bar(w: &tauri::WebviewWindow) {
    if let Some(m) = w.primary_monitor().ok().flatten() {
        let size = w.outer_size().unwrap_or(tauri::PhysicalSize::new(320, 56));
        let x = m.position().x + ((m.size().width as i32 - size.width as i32) / 2).max(0);
        // 顶部安全区下移一点（刘海屏）：24 逻辑 px。
        let y = m.position().y + 24;
        let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
    } else {
        let _ = w.center();
    }
}

/// 成功停止后关闭悬浮条窗口（终审修复，见 record_stop 注）：close 走 tauri 默认
/// destroy，下次 show_recording_bar 的懒建分支重建。best-effort（关失败仅忽略——
/// 窗口最多退化为常驻隐藏的旧行为，不影响录制产物）。非主线程调用（命令线程 ✓）。
fn close_recording_bar(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("recording-bar") {
        let _ = w.close();
    }
}

// ---------------------------------------------------------------------------
// 测试：状态机转移 + 计时扣除（纯逻辑）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_happy_path() {
        assert_eq!(transition(Phase::Idle, RecAction::Start), Ok(Phase::Recording));
        assert_eq!(transition(Phase::Recording, RecAction::Pause), Ok(Phase::Paused));
        assert_eq!(transition(Phase::Paused, RecAction::Resume), Ok(Phase::Recording));
        assert_eq!(transition(Phase::Recording, RecAction::Stop), Ok(Phase::Idle));
        assert_eq!(transition(Phase::Paused, RecAction::Stop), Ok(Phase::Idle));
    }

    #[test]
    fn transitions_reject_invalid() {
        assert!(transition(Phase::Idle, RecAction::Stop).is_err());
        assert!(transition(Phase::Idle, RecAction::Pause).is_err());
        assert!(transition(Phase::Idle, RecAction::Resume).is_err());
        assert!(transition(Phase::Recording, RecAction::Start).is_err());
        assert!(transition(Phase::Paused, RecAction::Start).is_err());
    }

    #[test]
    fn transitions_idempotent_directions_are_benign() {
        // F6 双通道/悬浮条双击竞态：同方向重复操作语义无害。
        assert_eq!(transition(Phase::Recording, RecAction::Resume), Ok(Phase::Recording));
        assert_eq!(transition(Phase::Paused, RecAction::Pause), Ok(Phase::Paused));
    }

    #[test]
    fn elapsed_excludes_completed_and_running_pauses() {
        // 录 100s、暂停 20s（已恢复）→ 80s。
        assert_eq!(
            elapsed_secs(Duration::from_secs(100), Duration::from_secs(20), None),
            80
        );
        // 录 100s、历史暂停 20s、暂停中 30s → 50s（当前段也冻结）。
        assert_eq!(
            elapsed_secs(
                Duration::from_secs(100),
                Duration::from_secs(20),
                Some(Duration::from_secs(30))
            ),
            50
        );
        // 未暂停 → 原值。
        assert_eq!(elapsed_secs(Duration::from_secs(59), Duration::ZERO, None), 59);
        // 防御：暂停累计超过活跃时长（时钟异常）不 panic 不回绕。
        assert_eq!(
            elapsed_secs(Duration::from_secs(5), Duration::from_secs(20), None),
            0
        );
    }

    /// 录制时长（M3 继承项①，duration 列口径）：与 elapsed_secs 同语义的 f64 版
    /// ——亚秒精度入库；暂停扣除/时钟异常防御同口径。
    #[test]
    fn recorded_secs_keeps_subsecond_precision_and_excludes_pauses() {
        assert_eq!(recorded_secs(Duration::from_millis(1500), Duration::ZERO, None), 1.5);
        assert_eq!(
            recorded_secs(Duration::from_secs(100), Duration::from_secs(20), None),
            80.0
        );
        assert_eq!(
            recorded_secs(
                Duration::from_secs(100),
                Duration::from_secs(20),
                Some(Duration::from_secs(30))
            ),
            50.0
        );
        // 时钟异常（暂停 > 活跃）不回绕：saturating 到 0。
        assert_eq!(recorded_secs(Duration::from_secs(5), Duration::from_secs(20), None), 0.0);
    }

    #[test]
    fn movies_dir_uses_home_or_userprofile() {
        // 不假设 CI 环境；仅验证函数可调且（若返回）路径尾段正确。
        if let Some(d) = movies_dir() {
            assert!(d.ends_with("Movies/HawkSnap"), "{d:?}");
        }
    }

    /// tick 载荷契约（M7）：session/seconds/paused 三字段（前端按 session 变化清错）。
    #[test]
    fn record_tick_serializes_with_session() {
        let s = serde_json::to_string(&RecordTick {
            session: 7, seconds: 65, paused: false, region_w: None, region_h: None,
            format: "mp4",
        })
        .unwrap();
        assert_eq!(s, r#"{"session":7,"seconds":65,"paused":false,"format":"mp4"}"#);
    }

    /// N1：区域录制 tick 载荷契约——regionW/regionH 以 camelCase 在场（悬浮条
    /// 「区域 W×H」标题数据源）；全屏录制（None）整字段缺席。
    #[test]
    fn record_tick_serializes_region_dims_camel_case() {
        let s = serde_json::to_string(&RecordTick {
            session: 7, seconds: 0, paused: false, region_w: Some(1600), region_h: Some(1200),
            format: "gif",
        })
        .unwrap();
        assert_eq!(
            s,
            r#"{"session":7,"seconds":0,"paused":false,"regionW":1600,"regionH":1200,"format":"gif"}"#
        );
    }

    /// N2：RecordFormat 载荷与解析契约——小写单词、大小写不敏感解析、非法值拒绝。
    #[test]
    fn record_format_parse_and_payload() {
        assert_eq!(RecordFormat::Mp4.as_payload_str(), "mp4");
        assert_eq!(RecordFormat::Gif.as_payload_str(), "gif");
        assert_eq!(parse_record_format("mp4"), Ok(RecordFormat::Mp4));
        assert_eq!(parse_record_format("GIF"), Ok(RecordFormat::Gif));
        assert_eq!(parse_record_format(" gif "), Ok(RecordFormat::Gif));
        // 非法值显式拒绝（不静默降级为 MP4——用户选择必须被尊重或报错）。
        assert!(parse_record_format("webm").is_err());
        assert!(parse_record_format("").is_err());
    }

    /// N1：RegionReq 反序列化契约（覆盖层 invoke 载荷：camelCase 外壳 + rect 的
    /// {x,y,w,h} 小写字段，与 FinishReq 同款纪律）。
    #[test]
    fn region_req_deserializes_camel_case() {
        let req: RegionReq = serde_json::from_str(
            r#"{"displayId":2,"rect":{"x":10,"y":20,"w":800,"h":600}}"#,
        )
        .unwrap();
        assert_eq!(req.display_id, 2);
        assert_eq!((req.rect.x, req.rect.y, req.rect.w, req.rect.h), (10, 20, 800, 600));
        // N2：format 缺省 = mp4（N1 旧载荷契约向后兼容）。
        assert_eq!(req.format, "mp4");
        // 显式 gif 直传。
        let req: RegionReq = serde_json::from_str(
            r#"{"displayId":2,"rect":{"x":0,"y":0,"w":1,"h":1},"format":"gif"}"#,
        )
        .unwrap();
        assert_eq!(req.format, "gif");
    }

    /// 会话计数单调：连续取号严格递增（Relaxed 下单线程语义即可保证）。
    #[test]
    fn session_counter_is_monotonic() {
        let a = SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let b = SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert!(b > a, "会话号必须单调递增：{a} → {b}");
    }

    /// 互斥守卫（终审 wave）纯逻辑：覆盖层可见 → F6 启动拒绝文案；不可见 → 放行。
    #[test]
    fn f6_guard_only_when_overlay_visible() {
        assert_eq!(
            f6_start_guard_msg(true).unwrap(),
            "请先完成或取消当前截图再录屏"
        );
        assert_eq!(f6_start_guard_msg(false), None);
    }

    /// recording_active 与 REC 槽位一致（空闲基线；启动/停止路径由集成行为覆盖）。
    /// 注意先取后放（is_none 的临时 guard 在语句末释放）再调 recording_active，
    /// 避免 std::Mutex 同线程重入死锁。
    #[test]
    fn recording_active_false_when_idle() {
        let idle = lock_rec().is_none();
        if idle {
            assert!(!recording_active());
        }
    }
}
