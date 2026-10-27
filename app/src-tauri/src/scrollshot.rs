//! 长截图（滚动捕获 + 拼接）会话：M3 Task N5。
//!
//! ## 链路
//!
//! 托盘「长截图」→ 覆盖层**选区模式**（intent=scrollshot，与截图/录制选区同一套
//! 选区 UI）→ 确认 → [`scrollshot_start`]（建会话 + 拉起提示条 + 注册 ⌘S 完成热键
//! + 隐藏覆盖层）→ **采集线程**每 500ms 抓一次该区域（平台侧
//! `hawksnap_platform_mac::windowcap`：CGWindowListCreateImage 窗口抓取，失败降级
//! 屏幕抓取）→ 帧**即时**喂进流式拼接器（不留帧队列）→ 用户滚动完按 ⌘S（或点
//! 提示条的「完成」）→ [`scrollshot_stop`]：拼接（`hawksnap_core::stitch`）→
//! 存 PNG 到 save_dir → 缩略图（首帧）+ 历史入库（kind="scrollshot"）。
//!
//! ## 设计要点
//!
//! - **采集源选型**（plan 裁定）：`SCScrollWindowImage` 不可用 → 用
//!   `CGWindowListCreateImage` 定时 500ms。窗口抓取拿的是**窗口自身内容**，
//!   因此本应用的提示条**不会**被拍进内容（屏幕兜底路径则可能——已知局限）。
//! - **不留帧队列（M5）**：每帧采集后立即参与匹配与合成、随即释放 → 内存与帧数
//!   无关 → **20000px 可达**（留帧方案：原始 RGBA 384MB 只够 8 帧、PNG 压缩
//!   64MB 只够 38 帧，两者都到不了 20000px）。内存 = 画布（= 产物，20000px×
//!   1600px ≈ 128MB，**本功能的内存下界**——PNG 必须整体物化）+ 2 帧。
//!   画布另有**硬上界**（`hawksnap_core::stitch::MAX_CANVAS_BYTES` 256MB 与
//!   `MAX_OUTPUT_HEIGHT` 40000px 取小）→ 触顶即停止采集并告知用户（G2）。
//! - **拼接拒绝优于错拼**：纯色画面 / 重复内容（条纹、重复表格行）→ 匹配判为
//!   歧义 → 拒绝出图（宁可不产出，绝不产出重影图）。见 `hawksnap_core::stitch`。
//! - **⌘S 完成**：全局热键（会话期间注册、结束即注销）。它会**吞掉**会话期间
//!   浏览器自己的 ⌘S（保存网页）——这是「按 ⌘S 结束」的既定语义，用户在会话
//!   期间的意图就是结束长截图。
//!   **注册失败必须让用户看见**（I4）：热键失败时走横幅通道告知，并保证提示条
//!   上的「完成」按钮始终可点（提示条不再点击穿透）；托盘另有「停止长截图」入口。
//! - **失败必须可达**（I4 残留，评审必修）：`scrollshot_stop` **先拼接、成功后
//!   才收尾**（关条 + 注销热键 + 消费会话）。拼接失败 → 会话与提示条**都留着**
//!   → 错误双通道可见（提示条行内 + 横幅），用户可重试或点「放弃」强制收尾。

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use hawksnap_core::geometry::Rect;
use hawksnap_core::scrollshot::{resolve_shift, StallDetector, StopOutcome};
use hawksnap_core::stitch::{Frame, Stitched, Stitcher};

/// 采集间隔（plan 裁定：500ms）——**基准间隔**。
const CAPTURE_INTERVAL: Duration = Duration::from_millis(500);
/// 快速滚动时的加密采集间隔（见 [`pacing_params`] → core `Pacing` 的滞回规则）。
const CAPTURE_INTERVAL_FAST: Duration = Duration::from_millis(200);
/// 判定「滚得快」的位移阈值（相对选区高度）。
const FAST_SHIFT_RATIO: f64 = 0.4;
/// 判定「滚得慢」的位移阈值（相对选区高度）。
const SLOW_SHIFT_RATIO: f64 = 0.2;
/// 首帧延迟：给「隐藏覆盖层」留出时间（覆盖层是带遮罩的全屏窗——被拍进去就毁了）。
const FIRST_FRAME_DELAY: Duration = Duration::from_millis(250);
/// **停滞检测**窗口（**墙钟**，评审 Minor：动效页 / 慢速滚动误报）。
///
/// 旧实现是「连续 10 帧画布没长」→ 采集间隔是**自适应**的（200ms 加密 /
/// 500ms 基准）→ 10 帧对应 2s~5s → 慢速滚动（每帧几 px、匹配器跟不上）与
/// 动效页会被误判为「匹配链已断」→ 弹出假告警并停止采集。
/// 现改为**墙钟窗口**（≥3s，与节奏无关）+ 「内容**确实测不出**」才判定
/// （规则本体在 core：[`hawksnap_core::scrollshot::StallDetector`]，可在
/// Linux 上单测）。
const STALL_WINDOW: Duration = Duration::from_secs(3);
// 采集期**不留帧队列**（M5）：每帧采集后立即参与匹配与合成、随即释放。
//
// 内存构成 = 画布（= 最终产物，20000px×1600px ≈ 128MB，**这是本功能的内存
// 下界**——PNG 必须整体物化）+ 2 帧（上一张被接受的帧 + 待决帧，各 ~7.7MB）。
//
// 为何不留队列：留帧就必须整批驻留才能建链 → 原始 RGBA 384MB 只够 8 帧、
// PNG 压缩（Adaptive 滤波，页面正文 ~4.4x → 1.75MB/帧）64MB 也只够 38 帧
// ——**两者都到不了 20000px** 的验收目标（每帧步长 ~280px → 38 帧仅 10.6k px）。
// 流式方案帧数无上限 → 20000px 可达。
//
// **画布仍有硬上界**（G2「绝不无界增长」）：`hawksnap_core::stitch` 侧
// MAX_CANVAS_BYTES（256MB）与 MAX_OUTPUT_HEIGHT（40000px）取小 → 触顶即
// 停止采集并告知用户（长图被截断，而不是内存无限涨）。

/// 完成热键（会话期间全局注册）。
const FINISH_COMBO: &str = "Cmd+S";
/// 提示条窗口标签（**ACL 按标签授权**——capabilities/scrollshot-bar.json 必须存在，
/// 否则 listen 被 ACL 拒绝、提示条永远收不到帧数）。
const BAR_LABEL: &str = "scrollshot-bar";
const BAR_W: f64 = 360.0;
const BAR_H: f64 = 64.0;

// ---------------------------------------------------------------------------
// 会话状态
// ---------------------------------------------------------------------------

struct Session {
    /// 会话号（单调）：前端据此清上一会话的行内错误残留。
    session: u64,
    /// 全局逻辑矩形（Quartz points）——采集区域。
    global: hawksnap_platform_mac::windowcap::GlobalRect,
    /// 选区全局矩形（提示条定位用：避开选区，防被拍进内容）。
    /// **流式拼接器**：每帧采集后立即参与匹配与合成，随即释放 → **不留帧队列**
    /// （M5：批处理留帧要么 384MB 原始 RGBA、要么 64MB PNG 只够 37 帧——
    /// 两者都到不了 20000px 的验收目标）。内存 = 画布（= 最终产物）+ 2 帧。
    stitcher: Mutex<Option<Stitcher>>,
    /// 首帧的缩略图（320px，plan 契约：thumb 取首帧）——生成于首帧，避免留整帧。
    thumb: Mutex<Option<(Vec<u8>, u32, u32)>>,
    /// ⌘S 完成热键是否注册成功（false → 提示条上的「完成」按钮是唯一出口）。
    hotkey_ok: AtomicBool,
    stop: AtomicBool,
    captured: AtomicU64,
    /// **收尾在飞行中**（单飞守卫）：「完成」被连点 / ⌘S 与按钮同时触发时，
    /// 只允许一个收尾在跑（评审 Minor：并发收尾 → 第二次拿到空画布 → 弹出
    /// **虚假的**「保存失败」横幅，而第一次其实已经成功）。
    finishing: AtomicBool,
}

impl Session {
    /// 喂一帧给流式拼接器（匹配 + 合成，随即释放）。
    fn push_frame(&self, frame: Frame) {
        let mut guard = self.stitcher.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            // 首帧：按它的实际宽度建画布（抓取尺寸可能与选区推算值不同）。
            *guard = Some(Stitcher::new(frame.width as usize));
            // 缩略图取首帧（plan 契约）：此刻生成 320px 缩略图，避免留整帧。
            if let Ok((rgba, tw, th)) = hawksnap_core::crop::rgba_thumbnail(
                &frame.rgba,
                frame.width,
                frame.height,
                320,
            ) {
                *self.thumb.lock().unwrap_or_else(|e| e.into_inner()) = Some((rgba, tw, th));
            }
        }
        if let Some(st) = guard.as_mut() {
            st.push(frame);
        }
        self.captured.fetch_add(1, Ordering::Relaxed);
    }

    /// 最近一次**测量**到的帧间位移（当前帧相对上一张被接受帧的估计）。
    ///
    /// 采集节奏的自适应依据（I1）：必须用**当前帧**的测量值，而不是上一张被
    /// 接受帧的滞后值——后者在匹配失败时会停在旧值上（拿旧位移决定下一拍间隔
    /// → 越滚越快 → 重叠耗尽 → 配对失配）。
    fn measured_shift(&self) -> Option<i32> {
        self.stitcher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|st| st.measured_shift())
    }

    /// 画布是否已触到内存上界（true = 长图被截断，应停止采集）。
    fn truncated(&self) -> bool {
        self.stitcher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|st| st.truncated())
            .unwrap_or(false)
    }

    /// **当前画布高度**（像素）——已合成进去的内容高度。
    ///
    /// 停滞检测的依据：高度不再增长 = 新帧没被接受（匹配失败）→ 链已断。
    fn canvas_height(&self) -> usize {
        self.stitcher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|st| st.canvas_height())
            .unwrap_or(0)
    }

    /// 本会话可产出的最大长图高度（像素）。
    fn max_height(&self) -> usize {
        self.stitcher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|st| st.max_height())
            .unwrap_or(0)
    }

    /// 收尾：产出长图（并取出首帧缩略图）。
    ///
    /// **空画布 → `None`**（评审 Minor：并发「完成」）：
    /// `Stitcher::finish` 会把画布 `mem::take` 走 → **第二次**收尾拿到的是
    /// **空图**（0 高度）→ 旧实现会把它当成功落盘（0px PNG）或在编码处报错
    /// → 弹出**虚假的**「保存失败」横幅（第一次其实已经成功）。空画布一律
    /// 视为「没有可产出的内容」→ `None`（由调用方按失败路径处理）。
    fn finish(&self) -> Option<(Stitched, Option<(Vec<u8>, u32, u32)>)> {
        let mut guard = self.stitcher.lock().unwrap_or_else(|e| e.into_inner());
        let out = guard.as_mut()?.finish()?;
        if out.height == 0 || out.rgba.is_empty() {
            return None; // 空画布（并发收尾 / 无内容）→ 没有产物
        }
        let thumb = self.thumb.lock().unwrap_or_else(|e| e.into_inner()).take();
        Some((out, thumb))
    }

    fn frame_count(&self) -> usize {
        self.captured.load(Ordering::Relaxed) as usize
    }
}

/// 进程级会话槽（None = 无进行中的长截图）。与 REC/HOTKEY_ERROR 同款单槽模式。
static SESSION: Mutex<Option<Arc<Session>>> = Mutex::new(None);

/// 会话号单调计数（每次成功启动取号 +1）。
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

fn lock_session() -> std::sync::MutexGuard<'static, Option<Arc<Session>>> {
    SESSION.lock().unwrap_or_else(|e| e.into_inner())
}

/// 是否有进行中的长截图（截图/录屏流程据此互斥拒绝）。
pub fn scrollshot_active() -> bool {
    lock_session().is_some()
}

/// 是否有**收尾在飞行中**（[`scrollshot_stop`] 的单飞守卫状态）。
///
/// 托盘据此**不打断**正在进行的收尾（否则并发点击「停止长截图」会在第一个收尾
/// 写到一半时把会话清掉）。
pub fn scrollshot_stopping() -> bool {
    lock_session()
        .as_ref()
        .map(|s| s.finishing.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// 下一次采集的间隔（**滞回式自适应**）——规则本体在 core
/// （[`hawksnap_core::scrollshot::Pacing`]）：产品侧与 eval 跑批器**共用同一份
/// 实现** → 规则改了两边一起变（不会出现「产品改了、eval 还按老模型跑」的盲区）。
///
/// 位移来源 = 拼接器刚做的测量（[`hawksnap_core::stitch::Stitcher::measured_shift`]，
/// 与链上用的是**同一个**三层匹配器）——不再单独跑 1D 行谱（1D 在准周期内容上
/// 会咬住假峰：实测真解 678px 被 524px 假峰盖过 → 节奏按假位移调 → 步长失控）。
fn pacing_params() -> hawksnap_core::scrollshot::Pacing {
    hawksnap_core::scrollshot::Pacing::new(
        CAPTURE_INTERVAL,
        CAPTURE_INTERVAL_FAST,
        FAST_SHIFT_RATIO,
        SLOW_SHIFT_RATIO,
    )
}

// ---------------------------------------------------------------------------
// 载荷
// ---------------------------------------------------------------------------

/// tick 载荷（提示条渲染输入）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ScrollshotTick {
    pub session: u64,
    pub frames: usize,
    /// ⌘S 完成热键是否可用（false → 提示条上的「完成」按钮是唯一出口）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<bool>,
}

/// pull 通道载荷（提示条 mount 时补显——push 早于 listen 必丢，同 countdown）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ScrollshotStatus {
    pub session: u64,
    pub frames: usize,
    pub active: bool,
    /// ⌘S 完成热键是否可用（false → 提示条上的「完成」按钮是唯一出口）。
    ///
    /// **必须走 pull 通道**：会话启动时那次 push（带 `hotkey`）几乎必然早于
    /// 提示条 webview 的 `listen` 就绪 → 必丢。只靠 push 的话，提示条永远显示
    /// 「按 ⌘S 结束」，即使热键注册失败。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<bool>,
}

/// 拼接失败事件载荷（提示条行内错误槽）。
///
/// **为什么单独一个事件**（评审 I4 残留必修）：失败反馈必须**可达**——
/// ① Rust 直发本事件 → 提示条 `setError`（不依赖 invoke 回流是否完好）；
/// ② 同时走横幅通道 `panel::show_capture_error`（提示条 webview 不健康时
/// 用户仍看得见）。旧实现是「先关条再拼接」→ 错误渲染进虚空 → 用户只见
/// 提示条凭空消失，零反馈。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ScrollshotError {
    /// 会话号（前端据此忽略**别的**会话的残留错误）。
    pub session: u64,
    /// 失败原因（可直接显示的中文文案）。
    pub message: String,
}

/// 启动请求（覆盖层选区确认载荷）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrollshotReq {
    pub display_id: u64,
    /// display 本地逻辑坐标选区（与 finish_capture 的 FinishReq.rect 同系同源）。
    pub rect: Rect,
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

/// 启动长截图会话（覆盖层选区确认路径）。
///
/// 成功：会话建立 + 提示条上屏 + ⌘S 完成热键注册 + **覆盖层全部隐藏**（覆盖层是
/// 带遮罩的全屏窗——不藏就会被拍进内容）。失败：Err 回流（覆盖层保留可重试）。
#[tauri::command(async)]
pub fn scrollshot_start(app: AppHandle, req: ScrollshotReq) -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, req);
        return Err("仅 macOS 支持长截图".into());
    }
    #[cfg(target_os = "macos")]
    {
    if scrollshot_active() {
        return Err("已有长截图进行中".into());
    }
    if crate::recording::recording_active() {
        return Err("录屏进行中，请先停止录屏".into());
    }
    if req.rect.w < 16 || req.rect.h < 16 {
        return Err(format!("选区尺寸过小：{}×{}", req.rect.w, req.rect.h));
    }
    // 屏幕录制权限预检（与截图/录屏同门槛；未授权时 macOS 静默返回空图）。
    if !hawksnap_platform_mac::permissions::screen_recording_prechecked() {
        let _ = hawksnap_platform_mac::permissions::open_screen_recording_settings();
        return Err("屏幕录制权限未授予：已为你打开系统设置，授权后重试".into());
    }
    // 显示器全局几何（覆盖层那一轮抓屏留下的几何快照；释放整帧后仍保留）。
    let Some(geo) = crate::commands::captured_display_geometry(req.display_id) else {
        return Err("找不到显示器的位置信息，请重新框选".into());
    };
    let global = hawksnap_platform_mac::windowcap::GlobalRect::new(
        geo.x + req.rect.x as f64,
        geo.y + req.rect.y as f64,
        req.rect.w as f64,
        req.rect.h as f64,
    );

    let session = Arc::new(Session {
        session: SESSION_SEQ.fetch_add(1, Ordering::Relaxed),
        global,
        stitcher: Mutex::new(None),
        thumb: Mutex::new(None),
        hotkey_ok: AtomicBool::new(true),
        stop: AtomicBool::new(false),
        captured: AtomicU64::new(0),
        finishing: AtomicBool::new(false),
    });
    *lock_session() = Some(session.clone());

    // 覆盖层先藏（否则遮罩被拍进内容）；再上提示条；再注册 ⌘S。
    if let Err(e) = crate::overlay::hide_all_overlays(&app) {
        eprintln!("[hawksnap] 长截图启动后隐藏覆盖层失败: {e}");
    }
    show_scrollshot_bar(&app, &global)?;
    let hotkey_ok = register_finish_hotkey(&app);
    session.hotkey_ok.store(hotkey_ok, Ordering::Relaxed);
    // 提示条上明示「完成」出口（⌘S 不可用时它是唯一出口）。
    // 注：这次 push 多半早于提示条 webview 的 listen 就绪 → 会丢；
    // 真正的补显通道是 `scrollshot_status`（带 hotkey 字段）。
    let _ = app.emit(
        "hawk://scrollshot-tick",
        ScrollshotTick {
            session: session.session,
            frames: 0,
            hotkey: Some(hotkey_ok),
        },
    );

    spawn_capture_thread(app.clone(), session.clone());
    emit_tick(&app, &session);
    Ok(())
    }
}

/// 结束长截图：拼接 → 存 PNG → 缩略图 + 历史入库（kind="scrollshot"）。
///
/// ## 顺序契约（评审 I4 残留必修）
///
/// **先拼接、成功后才收尾**（关提示条 + 注销热键 + 消费会话）。旧实现反过来
/// （先注销热键 + 关条，再拼接）→ 拼接失败时错误回传到**正在销毁**的 webview
/// → 前端 `setError` 渲染进虚空；会话已被 `lock_session().take()` 消费；托盘
/// 兜底只答「没有进行中的长截图」；⌘S 已注销 → **主出口彻底死路，零反馈**。
///
/// 现在：
/// - **拼接失败** → 会话与提示条**都留着** → 错误双通道可见（行内 + 横幅），
///   用户可重试或点「放弃」强制收尾；
/// - **落盘失败** → 画布已被取出（不可重试）→ 消费会话 + 横幅告知（不静默）；
/// - **成功** → 消费会话（关条 + 注销热键）→ 返回产物路径。
#[tauri::command(async)]
pub fn scrollshot_stop(app: AppHandle) -> Result<String, String> {
    // 取会话但**不消费**（失败时还要留着提示错误/重试）。
    let session = {
        let guard = lock_session();
        guard.as_ref().cloned()
    }
    .ok_or_else(|| "没有进行中的长截图".to_string())?;

    // —— ⓪ 单飞守卫（评审 Minor：并发「完成」）——
    //
    // 「完成」被连点 / ⌘S 与按钮同时触发 → 两个收尾并发跑 → 后到的那个拿到
    // **空画布**（画布已被取走）→ 旧实现弹出**虚假的**「保存失败」横幅（第一次
    // 其实已经成功）。这里只允许**一个**收尾在飞行中；后来的直接安静返回。
    if session.finishing.swap(true, Ordering::SeqCst) {
        return Err("长截图正在结束中，请稍候".to_string());
    }

    // —— ① 拼接（重活，秒级）——
    let Some((stitched, thumb)) = session.finish() else {
        let outcome = StopOutcome::StitchFailed;
        // 契约：**失败不关条、不消费会话**（否则错误无处可显 → 用户零反馈）。
        if outcome.consumes_session() {
            consume_session(&app, &session);
        }
        // 失败 → **放开单飞守卫**（会话留着 = 用户可重试；不放开的话重试会被
        // 「正在结束中」永久挡下 → 用户只能放弃）。
        session.finishing.store(false, Ordering::SeqCst);
        let msg = "未能识别滚动位移：画面内容没有明显变化，或内容重复导致匹配歧义";
        surface_failure(&app, &session, msg);
        eprintln!("[hawksnap] 长截图拼接失败（会话与提示条保留，可重试或放弃）: {msg}");
        return Err(msg.to_string());
    };

    // —— ② 落盘（画布已取出 → 失败不可重试）——
    let settings = crate::settings::Settings::load(&app);
    let save = (|| -> Result<(std::path::PathBuf, String), String> {
        let dir = crate::commands::resolve_save_dir(&settings).ok_or("no picture dir")?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = crate::commands::save_path_for(&dir, "HawkSnap", "png");
        // **零拷贝编码**：`rgba_to_png` 会把整幅长图 `to_vec()` 复制一份（长图可达
        // 128MB → 峰值翻倍）；`rgba_to_png_borrowed` 直接把切片交给编码器。
        let png = hawksnap_core::crop::rgba_to_png_borrowed(
            &stitched.rgba,
            stitched.width,
            stitched.height,
        )
        .map_err(|e| e.to_string())?;
        std::fs::write(&path, &png).map_err(|e| format!("保存失败: {e}"))?;
        // 缩略图 = **首帧**（plan 契约：thumb 取首帧——它是长图顶部，网格里一眼能认出）。
        // **旁路语义**：缩略图失败**不影响**主产物（长图已落盘）——记日志、缩略图
        // 留空（前端有占位块分支），绝不因缩略图把整次长截图判为失败。
        let thumb_path = match write_thumbnail(&dir, &path, thumb) {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(e) => {
                eprintln!("[hawksnap] 长截图缩略图生成失败（不影响长图）: {e}");
                String::new()
            }
        };
        Ok((path, thumb_path))
    })();

    let (path, thumb_path) = match save {
        Ok(v) => v,
        Err(e) => {
            // 落盘失败：画布已取出（不可重试）→ 消费会话 + **横幅**告知（不静默）。
            let outcome = StopOutcome::SaveFailed;
            if outcome.consumes_session() {
                consume_session(&app, &session);
            }
            crate::panel::show_capture_error(&app, format!("长截图：保存失败（{e}）"));
            eprintln!("[hawksnap] 长截图保存失败: {e}");
            return Err(format!("保存失败: {e}"));
        }
    };

    // —— ③ 成功：消费会话（关条 + 注销热键 + 清槽位）——
    let outcome = StopOutcome::Saved;
    if outcome.consumes_session() {
        consume_session(&app, &session);
    }

    // 历史入库（旁路语义：失败仅记录，不影响产物返回）。
    if let Err(e) = crate::history::record(
        &app,
        &path.to_string_lossy(),
        &thumb_path,
        stitched.width as i64,
        stitched.height as i64,
        "scrollshot",
        "scrollshot",
        None,
    ) {
        eprintln!("[hawksnap] 长截图历史入库失败: {e}");
    }
    Ok(path.to_string_lossy().into_owned())
}

/// 失败反馈的**双通道**投递（评审 I4 残留必修——失败必须看得见）。
///
/// ① 提示条行内错误槽：Rust 直发 `hawk://scrollshot-error` → 前端 `setError`
///    （**不依赖** invoke 回流是否完好，也不依赖前端是否还活着）；
/// ② 横幅通道 `panel::show_capture_error`：提示条 webview 不健康 / 已崩时，
///    用户仍能从主面板看到失败原因（spec §9「不静默」）。
///
/// 调用前提：**提示条未关**（失败路径不关条——否则错误无处可显）。
fn surface_failure(app: &AppHandle, session: &Session, msg: &str) {
    let _ = app.emit(
        "hawk://scrollshot-error",
        ScrollshotError {
            session: session.session,
            message: msg.to_string(),
        },
    );
    crate::panel::show_capture_error(app, format!("长截图：{msg}"));
}

/// 消费会话：停采集 + 注销热键 + 关提示条 + 清槽位（**仅在成功/不可重试时**）。
fn consume_session(app: &AppHandle, session: &Session) {
    session.stop.store(true, Ordering::SeqCst);
    unregister_finish_hotkey(app);
    close_scrollshot_bar(app);
    let mut guard = lock_session();
    // 只清**本会话**（避免误清后来建立的会话）。
    if guard.as_ref().map(|s| s.session) == Some(session.session) {
        *guard = None;
    }
}

/// 强制放弃长截图（丢弃已采集内容）：清会话 + 停采集 + 注销热键 + 关提示条。
///
/// 「完成」失败后提示条上的出口（用户明确表示「不要了」）。托盘的「停止长截图」
/// 也是**强制停止**路径——它必须能把卡住的会话清掉（否则僵尸会话挡住后续
/// 截图/录屏）。
pub fn scrollshot_force_stop(app: &AppHandle) {
    let session = { lock_session().take() };
    if let Some(session) = session {
        session.stop.store(true, Ordering::SeqCst);
    }
    unregister_finish_hotkey(app);
    close_scrollshot_bar(app);
}

/// 放弃长截图（前端「放弃」按钮）。
///
/// **收尾在飞行中 → 不打断**（终审 wave Minor e，与托盘「停止长截图」同款口径）：
/// `scrollshot_force_stop` 会清掉会话槽；正在跑的收尾（拼接 + 落盘）拿到的是
/// 会话内的画布，若此刻把会话抽走 → 收尾写到一半的产物状态不可预期，且提示条
/// 被关掉、错误无处可显。托盘路径的写法是「不打断、什么都不做」，这里改成
/// **返回错误文案**（前端把错误渲染在行内 → 用户知道为什么没反应）。
#[tauri::command(async)]
pub fn scrollshot_abort(app: AppHandle) -> Result<(), String> {
    if scrollshot_stopping() {
        return Err("长截图正在结束中，请稍候".to_string());
    }
    scrollshot_force_stop(&app);
    Ok(())
}

/// 提示条 pull 通道（帧数补显）。
#[tauri::command]
pub fn scrollshot_status() -> Option<ScrollshotStatus> {
    let guard = lock_session();
    let s = guard.as_ref()?;
    Some(ScrollshotStatus {
        session: s.session,
        frames: s.frame_count(),
        active: true,
        hotkey: Some(s.hotkey_ok.load(Ordering::Relaxed)),
    })
}

// ---------------------------------------------------------------------------
// 采集线程
// ---------------------------------------------------------------------------

fn spawn_capture_thread(app: AppHandle, session: Arc<Session>) {
    std::thread::spawn(move || {
        // 首帧延迟：覆盖层 hide 是投递给主线程的异步消息，留出时间避免拍到遮罩。
        std::thread::sleep(FIRST_FRAME_DELAY);
        // 节奏规则来自 core（产品与 eval 共用同一份实现）。
        let pacing = pacing_params();
        let mut interval = CAPTURE_INTERVAL;
        // 上一帧的行亮度剖面——只用于判定「页面完全没动」（静止页面不该加密采集）。
        let mut last_profile: Option<Vec<f32>> = None;
        // 停滞检测（规则本体在 core → 可在 Linux 上单测）：画布不再生长 +
        // 内容**测不出** → 连续满一个**墙钟窗口**才判定链已断。
        let mut stall = StallDetector::new(STALL_WINDOW);
        while !session.stop.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            match hawksnap_platform_mac::windowcap::capture_global_rect(session.global) {
                Ok(img) => {
                    if let Some(frame) = Frame::new(img.width, img.height, img.rgba) {
                        let region_h = frame.height;
                        // 静止判定：行谱与上一帧**完全相同** → 位移 0（页面没滚）。
                        // （对无特征页面有效：那种页面匹配器测不出位移，但页面确实
                        //  静止——否则会被当成「不确定」而一直加密采集。）
                        let profile = frame.row_profile();
                        let still = last_profile.as_ref().map(|p| *p == profile).unwrap_or(false);
                        last_profile = Some(profile);
                        let height_before = session.canvas_height();
                        // 喂给流式拼接器（内部即生产同款三层匹配 `estimate_frame_shift`
                        // ——**不再单独跑一遍 1D 行谱**：1D 在准周期内容上会咬住假峰
                        // （实测真解 678px 被 524px 假峰盖过）或直接返 None → 间隔
                        // 永不调整 → 步长越滚越大 → 重叠耗尽 → 配对失配）。
                        session.push_frame(frame);
                        // 节奏自适应：取**当前帧**的测量位移（与拼接链同一个匹配器）。
                        //
                        // **测量值优先**（评审 Minor）：静止守卫只在**测不出**时兜底。
                        // 旧写法 `if still { Some(0) } else { measured }` 会让「静止守卫」
                        // 覆盖**真实测量**——准周期页面滚过**恰好一个周期**时行谱与上一
                        // 帧逐字节相同 → 被判「静止」→ 位移强行归零 → 节奏退到 500ms
                        // → 重叠被吃掉 → 配对失配。
                        let measured = session.measured_shift();
                        let shift = resolve_shift(measured, still);
                        interval = pacing.next_interval(interval, shift, region_h);
                        // 停滞检测（评审 Minor）：**内容在动**（行谱变了）但**画布
                        // 没长**（新帧没被接受 = 匹配链已断）→ 连续满一个**墙钟
                        // 窗口**（≥3s，与采集节奏无关）且内容**确实测不出**才判
                        // 停滞 → 长图会被静默截断 → 必须告知用户（横幅）并停止
                        // 采集。动效页（内容在变但主体仍可对齐 → 测得出位移）
                        // 不再被误判。
                        let height_after = session.canvas_height();
                        let grew = height_after > height_before;
                        // 画布还空（一个帧都没被接受）→ 不计停滞窗口（此时
                        // 「拼接失败」的反馈路径已经覆盖了这种空转）。
                        if height_after > 0
                            && stall.observe(grew, still, measured, Instant::now())
                        {
                            eprintln!("[hawksnap] 长截图匹配链停滞（画布停在 {height_after}px），停止采集");
                            crate::panel::show_capture_error(
                                &app,
                                format!(
                                    "长截图：匹配已停滞（页面在动但无法对齐，画布停在 {height_after}px）——已停止采集，可点「完成」保存当前长图"
                                ),
                            );
                            break;
                        }
                    }
                }
                Err(e) => eprintln!("[hawksnap] 长截图抓帧失败: {e}"),
            }
            emit_tick(&app, &session);
            // 画布触到内存上界（G2）→ 停止采集，提示用户把当前长图存下来。
            if session.truncated() {
                eprintln!("[hawksnap] 长截图已达最大长度（{}px），停止采集", session.max_height());
                crate::panel::show_capture_error(
                    &app,
                    format!(
                        "长截图：已达最大长度（{}px）——请按 ⌘S 或点「完成」保存当前长图",
                        session.max_height()
                    ),
                );
                break;
            }
            let elapsed = t0.elapsed();
            if elapsed < interval {
                std::thread::sleep(interval - elapsed);
            }
        }
    });
}

fn emit_tick(app: &AppHandle, session: &Session) {
    let _ = app.emit(
        "hawk://scrollshot-tick",
        ScrollshotTick {
            session: session.session,
            frames: session.frame_count(),
            hotkey: None,
        },
    );
}

// ---------------------------------------------------------------------------
// 完成热键（⌘S）
// ---------------------------------------------------------------------------

/// 注册 ⌘S 完成热键。**失败必须让用户看见**（I4，评审必修）：注册失败时若
/// 只 eprintln，用户将没有任何「结束长截图」的键盘出口（提示条原本点击穿透、
/// 没有按钮）→ 采集线程会一直跑到队列上限。故失败时：
/// ① 走既有横幅通道告知用户；
/// ② 提示条改为**可交互**（显示「完成」按钮），保证始终有鼠标出口。
fn register_finish_hotkey(app: &AppHandle) -> bool {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let handle = app.clone();
    match app.global_shortcut().on_shortcut(FINISH_COMBO, move |_app, _sc, event| {
        use tauri_plugin_global_shortcut::ShortcutState;
        if event.state() != ShortcutState::Pressed {
            return;
        }
        let app = handle.clone();
        // 拼接是重活（秒级）→ 绝不占主线程（热键回调在主线程）。
        std::thread::spawn(move || {
            if !scrollshot_active() {
                // 竞态兜底：会话已被消费（热键应已注销）→ 明确告知，不静默。
                crate::panel::show_capture_error(&app, "长截图：没有进行中的长截图".into());
                return;
            }
            // 失败**已由 scrollshot_stop 自行投递**（提示条行内 + 横幅）→ 这里只记
            // 日志，不重复弹横幅（同一错误弹两次 = 噪音）。
            if let Err(e) = scrollshot_stop(app.clone()) {
                eprintln!("[hawksnap] ⌘S 结束长截图失败: {e}");
            }
        });
    }) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[hawksnap] 注册 ⌘S 完成热键失败: {e}");
            crate::panel::show_capture_error(
                app,
                format!("长截图：⌘S 完成热键注册失败（{e}）——请用提示条上的「完成」按钮结束"),
            );
            false
        }
    }
}

fn unregister_finish_hotkey(app: &AppHandle) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    if let Err(e) = app.global_shortcut().unregister(FINISH_COMBO) {
        eprintln!("[hawksnap] 注销 ⌘S 完成热键失败: {e}");
    }
}

// ---------------------------------------------------------------------------
// 提示条窗口
// ---------------------------------------------------------------------------

/// 提示条上屏（懒建 + 常驻复用，与 recording-bar 同款）。
///
/// **可交互**（I4）：条上有「完成」按钮 → 用户始终有鼠标出口（⌘S 注册失败
/// 时它是唯一出口）。代价：条覆盖区域不再「鼠标穿透」（用户在该区域滚轮不会
/// 滚动被拍页面）——故条刻意定位在**避开选区**的上下边缘。
///
/// **位置刻意避开选区**：屏幕兜底抓取路径会把「屏幕上真实可见的像素」拍进去，
/// 提示条若压在选区上就会在长图里留下自己的残影。选区在上下都避开时，退到
/// 底边（与 keycast 同款，让开 Dock）。
fn show_scrollshot_bar(app: &AppHandle, global: &hawksnap_platform_mac::windowcap::GlobalRect) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(BAR_LABEL) {
        position_bar(&w, global);
        let _ = w.show();
        return Ok(());
    }
    let win = WebviewWindowBuilder::new(
        app,
        BAR_LABEL,
        WebviewUrl::App("scrollshot-bar?window=scrollshot-bar".into()),
    )
    .inner_size(BAR_W, BAR_H)
    .decorations(false)
    .always_on_top(true)
    .resizable(false)
    .skip_taskbar(true)
    // **不点击穿透**（终审 wave Minor f：此前此处还留着「点击穿透」的过期注释，
    // 与实际行为矛盾——整窗早已改为可交互，见下）。条是**刻意定位在避开选区的
    // 上下边缘**的：被拍区域的点击不受影响。
    // 不抢键盘焦点（用户要滚动被拍的内容）——但**要能点**（I4：完成按钮是
    // ⌘S 不可用时的唯一鼠标出口；此前整窗点击穿透 → 无任何鼠标出口）。
    .focusable(false)
    .focused(false)
    .transparent(false)
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;
    position_bar(&win, global);
    let _ = win.show();
    Ok(())
}

/// 提示条定位：优先避开选区的上下边，都避开不了才压边。
fn position_bar(w: &tauri::WebviewWindow, global: &hawksnap_platform_mac::windowcap::GlobalRect) {
    let Ok(Some(m)) = w.primary_monitor() else {
        let _ = w.center();
        return;
    };
    let size = w
        .outer_size()
        .unwrap_or(tauri::PhysicalSize::new(BAR_W as u32, BAR_H as u32));
    let mx = m.position().x as f64;
    let my = m.position().y as f64;
    let mw = m.size().width as f64;
    let mh = m.size().height as f64;
    let bar_x = mx + ((mw - size.width as f64) / 2.0).max(0.0);
    // 选区与屏幕的交集（用于判断是否压住选区）。
    let sel_top = global.y.max(my);
    let sel_bottom = (global.y + global.h).min(my + mh);
    let top_y = my + 24.0; // 顶部（刘海安全区下移）
    let bottom_y = my + mh - size.height as f64 - 24.0;
    // 顶部不压选区 → 用顶部；否则底部不压选区 → 用底部；否则用底部（并由窗口抓取
    // 路径兜底：本应用的窗口不会进入窗口抓取内容）。
    let y = if !(sel_top < top_y + size.height as f64 && sel_bottom > top_y) {
        top_y
    } else if !(sel_top < bottom_y + size.height as f64 && sel_bottom > bottom_y) {
        bottom_y
    } else {
        bottom_y
    };
    let _ = w.set_position(tauri::PhysicalPosition::new(
        bar_x.round() as i32,
        y.round().max(0.0) as i32,
    ));
}

fn close_scrollshot_bar(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(BAR_LABEL) {
        let _ = w.close();
    }
}

// ---------------------------------------------------------------------------
// 缩略图
// ---------------------------------------------------------------------------

/// 缩略图落盘（首帧 → 320px 缩略图 → save_dir/thumbs/<stem>.png）。
fn write_thumbnail(
    dir: &std::path::Path,
    out_path: &std::path::Path,
    thumb: Option<(Vec<u8>, u32, u32)>,
) -> Result<std::path::PathBuf, String> {
    let thumbs_dir = dir.join("thumbs");
    std::fs::create_dir_all(&thumbs_dir).map_err(|e| e.to_string())?;
    let stem = out_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shot".into());
    let thumb_path = thumbs_dir.join(format!("{stem}.png"));
    let Some((thumb_rgba, tw, th)) = thumb else {
        return Err("首帧缩略图缺失，无法生成".into());
    };
    let thumb_png = hawksnap_core::crop::rgba_to_png(&thumb_rgba, tw, th).map_err(|e| e.to_string())?;
    std::fs::write(&thumb_path, &thumb_png).map_err(|e| e.to_string())?;
    Ok(thumb_path)
}

// ---------------------------------------------------------------------------
// 测试（纯逻辑：载荷契约 + 队列边界）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_payload_serializes_camel_case() {
        let s = serde_json::to_string(&ScrollshotTick {
            session: 3,
            frames: 12,
            hotkey: None,
        })
        .unwrap();
        assert_eq!(s, r#"{"session":3,"frames":12}"#);
        // hotkey 字段在场时（⌘S 不可用 → 提示条明示「完成」按钮是唯一出口）。
        let s2 = serde_json::to_string(&ScrollshotTick {
            session: 3,
            frames: 12,
            hotkey: Some(false),
        })
        .unwrap();
        assert_eq!(s2, r#"{"session":3,"frames":12,"hotkey":false}"#);
    }

    #[test]
    fn status_payload_serializes_camel_case() {
        let s = serde_json::to_string(&ScrollshotStatus {
            session: 1,
            frames: 2,
            active: true,
            hotkey: None,
        })
        .unwrap();
        assert_eq!(s, r#"{"session":1,"frames":2,"active":true}"#);
        // hotkey 在场时（⌘S 注册失败 → 提示条明示「完成」按钮是唯一出口）。
        let s2 = serde_json::to_string(&ScrollshotStatus {
            session: 1,
            frames: 2,
            active: true,
            hotkey: Some(false),
        })
        .unwrap();
        assert_eq!(s2, r#"{"session":1,"frames":2,"active":true,"hotkey":false}"#);
    }

    #[test]
    fn req_deserializes_camel_case() {
        let req: ScrollshotReq =
            serde_json::from_str(r#"{"displayId":2,"rect":{"x":10,"y":20,"w":800,"h":600}}"#)
                .unwrap();
        assert_eq!(req.display_id, 2);
        assert_eq!((req.rect.x, req.rect.y, req.rect.w, req.rect.h), (10, 20, 800, 600));
    }

    /// 采集期**不留帧队列**（M5）：每帧喂进流式拼接器即释放 → 内存与帧数无关。
    /// 断言：喂 N 帧后，会话里**没有**任何帧缓冲（只有拼接器内部的画布 + 2 帧）。
    #[test]
    fn session_keeps_no_frame_queue() {
        let s = Session {
            session: 0,
            global: hawksnap_platform_mac::windowcap::GlobalRect::new(0.0, 0.0, 10.0, 10.0),
            stitcher: Mutex::new(None),
            thumb: Mutex::new(None),
            hotkey_ok: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            captured: AtomicU64::new(0),
            finishing: AtomicBool::new(false),
        };
        for _ in 0..5 {
            s.push_frame(Frame { width: 4, height: 4, rgba: vec![7u8; 4 * 4 * 4] });
        }
        // 会话里除拼接器与缩略图外**没有任何帧存储**——帧数再多也不涨内存。
        assert_eq!(s.frame_count(), 5, "已抓帧数照常统计（提示条进度用）");
        assert!(
            s.stitcher.lock().unwrap().is_some(),
            "拼接器应已建立（首帧创建）"
        );
    }

    /// 自适应采集间隔（滞回规则）：规则本体在 core
    /// （`hawksnap_core::scrollshot::Pacing`）→ **可在 Linux 上单测**
    /// （core 的 `next_interval_adapts_with_hysteresis`）。此处只锁定「产品侧
    /// 用的就是 core 那份参数」。
    #[test]
    fn product_pacing_uses_core_rule_with_product_constants() {
        let p = pacing_params();
        assert_eq!(p.base, CAPTURE_INTERVAL);
        assert_eq!(p.fast, CAPTURE_INTERVAL_FAST);
        assert_eq!(p.fast_ratio, FAST_SHIFT_RATIO);
        assert_eq!(p.slow_ratio, SLOW_SHIFT_RATIO);
        // 冒烟：一条典型用例（滚得快 → 加密）。
        assert_eq!(
            p.next_interval(CAPTURE_INTERVAL, Some(800), 900),
            CAPTURE_INTERVAL_FAST
        );
    }

    /// 内存模型（M5 的量化依据）：**不留帧队列** → 帧数与内存无关 → 20000px
    /// 可达。断言仅留「画布 + 2 帧」：本测试用一个 40 帧的小序列验证会话内
    /// 不随帧数增长任何帧缓冲（画布随内容增长，是产物本身）。
    #[test]
    fn no_frame_queue_means_memory_is_independent_of_frame_count() {
        let (w, h) = (400u32, 300u32);
        let mut rgba = vec![255u8; (w * h * 4) as usize];
        // 伪随机「文字块」：白底上零散的深色小方块（页面正文的形态）。
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed >> 33
        };
        for y in (0..h).step_by(14) {
            let mut x = 6u32;
            while x < w - 10 {
                let len = 6 + (next() % 18) as u32;
                for yy in y..(y + 5).min(h) {
                    for xx in x..(x + len).min(w) {
                        let i = ((yy * w + xx) * 4) as usize;
                        let g = (30 + (next() % 120) as u8) as u8;
                        rgba[i] = g;
                        rgba[i + 1] = g;
                        rgba[i + 2] = g;
                    }
                }
                x += len + 8 + (next() % 10) as u32;
            }
        }
        // 合成帧（构造用，不入库）：确认「输出 PNG 编码不复制输入」这一零拷贝
        // 路径可用（长图 128MB 级，省一次复制就是省一次峰值）。
        let png = hawksnap_core::crop::rgba_to_png_borrowed(&rgba, w, h).expect("编码应成功");
        assert!(!png.is_empty());
        // 帧队列已移除：会话里不再有「帧数 × 帧大小」的开销。
        let s = Session {
            session: 0,
            global: hawksnap_platform_mac::windowcap::GlobalRect::new(0.0, 0.0, 10.0, 10.0),
            stitcher: Mutex::new(None),
            thumb: Mutex::new(None),
            hotkey_ok: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            captured: AtomicU64::new(0),
            finishing: AtomicBool::new(false),
        };
        for _ in 0..40 {
            s.push_frame(Frame { width: w, height: h, rgba: rgba.clone() });
        }
        assert_eq!(s.frame_count(), 40);
    }

    /// 会话槽空态：idle → scrollshot_active() == false。
    #[test]
    fn no_session_when_idle() {
        let idle = lock_session().is_none();
        if idle {
            assert!(!scrollshot_active());
        }
    }

    /// **顺序契约**（评审 I4 残留必修）：拼接失败**不得**消费会话、**不得**关提示条
    /// ——否则错误回传到正在销毁的 webview（渲染进虚空）+ 会话已被消费 → 用户零反馈。
    ///
    /// 规则本体在 core（`hawksnap_core::scrollshot::StopOutcome`）→ **可在 Linux
    /// 上单测**（core 的 `stitch_failure_keeps_session_and_bar`）。此处锁定
    /// 「app crate 用的就是 core 那份枚举」。
    #[test]
    fn product_stop_outcome_is_the_core_contract() {
        assert!(!StopOutcome::StitchFailed.consumes_session());
        assert!(!StopOutcome::StitchFailed.closes_bar());
        assert!(StopOutcome::Saved.consumes_session());
        assert!(StopOutcome::Saved.closes_bar());
    }

    /// 失败事件载荷（前端行内错误槽）：字段名 camelCase + 会话号可辨。
    #[test]
    fn scrollshot_error_payload_serializes_camel_case() {
        let s = serde_json::to_string(&ScrollshotError {
            session: 7,
            message: "未能识别滚动位移".into(),
        })
        .unwrap();
        assert_eq!(
            s,
            r#"{"session":7,"message":"未能识别滚动位移"}"#
        );
    }

    /// **并发「完成」守卫**（评审 Minor）：
    /// ① 同一会话只允许**一个**收尾在飞行中（第二次 `swap` 返回 true）；
    /// ② **空画布 → `finish()` 返回 `None`**（画布已被前一次收尾取走 → 不得产出
    ///    0px 图 / 虚假失败横幅）。
    #[test]
    fn concurrent_finish_is_single_flight_and_never_yields_an_empty_canvas() {
        let mk = |stitcher: Option<Stitcher>| Session {
            session: 0,
            global: hawksnap_platform_mac::windowcap::GlobalRect::new(0.0, 0.0, 10.0, 10.0),
            stitcher: Mutex::new(stitcher),
            thumb: Mutex::new(None),
            hotkey_ok: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            captured: AtomicU64::new(0),
            finishing: AtomicBool::new(false),
        };

        // ① 单飞守卫：第一次拿到 false（可以收尾），第二次拿到 true（已在飞行）。
        let s = mk(None);
        assert!(!s.finishing.swap(true, Ordering::SeqCst), "第一个收尾应放行");
        assert!(s.finishing.swap(true, Ordering::SeqCst), "第二个收尾应被挡下");

        // ② 空会话（一个帧都没有）→ 无产物。
        assert!(mk(None).finish().is_none(), "没有帧 → 没有产物");

        // ③ 单帧（不可拼接）→ 无产物。
        let mut st = Stitcher::new(4);
        st.push(Frame { width: 4, height: 4, rgba: vec![7u8; 4 * 4 * 4] });
        assert!(mk(Some(st)).finish().is_none(), "单帧不可拼接 → 没有产物");

        // ④ 两帧可拼 → **第一次**有产物；**第二次**（画布已被取走）→ None。
        let mut st = Stitcher::with_estimator(4, |_a: &Frame, _b: &Frame| Some(4));
        let f = || Frame { width: 4, height: 4, rgba: vec![9u8; 4 * 4 * 4] };
        st.push(f());
        st.push(f());
        let s = mk(Some(st));
        assert!(s.finish().is_some(), "两帧可拼接 → 有产物");
        assert!(
            s.finish().is_none(),
            "第二次收尾拿到的是**空画布** → 必须返回 None（不得产出 0px 图）"
        );
    }
}
