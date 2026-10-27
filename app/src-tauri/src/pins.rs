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

use std::collections::HashMap;
#[cfg_attr(not(target_os = "macos"), allow(unused_imports))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use tauri::AppHandle;

use base64::Engine as _;

use crate::commands;

// —— 贴图 Pin 窗口句柄（平台门控） ——

/// macOS：platform-mac 的原生 NSWindow 句柄（无 webview）。非 macOS：map 类型退化为
/// `HashMap<u64, ()>` 且所有操作返回 Err——贴图的 Windows/Linux 适配归 Plan D CI 矩阵
/// （plan Global Constraints）。
#[cfg(target_os = "macos")]
type PinHandle = hawksnap_platform_mac::pin::PinWindow;
#[cfg(not(target_os = "macos"))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
type PinHandle = ();

#[cfg(target_os = "macos")]
static PINS: LazyLock<Mutex<HashMap<u64, PinHandle>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(not(target_os = "macos"))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
static PINS: LazyLock<Mutex<HashMap<u64, PinHandle>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// 贴图 id 分配器（也作级联序号：第 n 个贴图偏移 (n%8)*24）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
static PIN_SEQ: AtomicU64 = AtomicU64::new(0);

/// 进程级 AppHandle（M2 Task M3，双击进编辑器用）：双击回调是 `fn(u64)` fn 指针
/// （platform-mac 不反向依赖 app crate），回调体内需要 AppHandle 才能开编辑器窗口
/// ——首个贴图创建时（spawn_pin_window）存入。OnceLock：首建后不再变；未建贴图时
/// 双击不可能发生（无窗口），取不到 handle 的分支仅防御。仅 macOS（双击钩子只在
/// macOS 建窗路径注入）。
#[cfg(target_os = "macos")]
static APP: std::sync::OnceLock<AppHandle> = std::sync::OnceLock::new();

// —— 纯逻辑（可测；窗口路径经 PinWindow 句柄） ——

/// 级联偏移（C5 测试契约）：第 n 个贴图相对基准点（主屏中心 − 半尺寸）的对角偏移。
/// `(n % 8) * 24`：相邻两贴图错开 24pt，8 个一循环——9+ 贴图不会级联出屏。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn next_pin_offset(n: u64) -> (f64, f64) {
    let k = (n % 8) as f64 * 24.0;
    (k, k)
}

/// 缩放目标 clamp：当前显示比 × factor 后截到 [`hawksnap_core::geometry`] 的贴图范围
/// （0.1..=3.2 = 10%–3200%，F3 验收口径；常量下沉后与 platform-mac 滚轮路径同一来源）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn clamp_zoom(cur: f64, factor: f64) -> f64 {
    use hawksnap_core::geometry::{PIN_ZOOM_MAX, PIN_ZOOM_MIN};
    (cur * factor).clamp(PIN_ZOOM_MIN, PIN_ZOOM_MAX)
}

/// 透明度 clamp：0.2..=1.0（20%–100%，F3 验收口径；常量单一来源同上）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn clamp_opacity(opacity: f64) -> f64 {
    use hawksnap_core::geometry::{PIN_OPACITY_MAX, PIN_OPACITY_MIN};
    opacity.clamp(PIN_OPACITY_MIN, PIN_OPACITY_MAX)
}

// —— 尺寸上限（M3 继承项③） ——
/// 单贴图长边像素上限（M3 继承项③，M2 终审遗留「per-pin rgba 上限」）：贴图窗口
/// 常驻整帧 RGBA（w×h×4 字节，与 webview 无关、不参与 idle 回收），>4K 源（6K
/// 全屏截图、长截图、剪贴板巨图）按原样建窗会击穿预算裁定（~24MB/贴图量级）。
/// 超限 → [`hawksnap_core::crop::rgba_thumbnail`] 等比降采样到长边 4096（Lanczos3，
/// 比例保持）再建窗；贴图语义（参考/对照）下 4096 长边的信息量足够，滚轮缩放
/// （F3，0.1–3.2×）不受影响——缩放是显示比，不重建位图。降采样失败（理论不可达：
/// RGBA 尺寸合法）→ 原样放行，由建窗路径自有兜底。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const PIN_MAX_EDGE_PX: u32 = 4096;

/// 尺寸上限裁剪（纯函数可测）：长边 ≤ 上限原样返回；> 上限降采样并返回新尺寸。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn cap_pin_rgba(
    rgba: std::sync::Arc<Vec<u8>>,
    w: u32,
    h: u32,
) -> (std::sync::Arc<Vec<u8>>, u32, u32) {
    if w.max(h) <= PIN_MAX_EDGE_PX {
        return (rgba, w, h);
    }
    match hawksnap_core::crop::rgba_thumbnail(&rgba, w, h, PIN_MAX_EDGE_PX) {
        Ok((data, nw, nh)) => {
            // 探针（常驻，廉价，超限贴图一行）：降采样可观测。
            eprintln!("[hawksnap] pin: 超限降采样 {w}×{h} → {nw}×{nh}（上限 {PIN_MAX_EDGE_PX}px）");
            (std::sync::Arc::new(data), nw, nh)
        }
        Err(_) => (rgba, w, h),
    }
}

/// pin 图像来源（C5）：CAPTURES 最近一轮非空整帧，或剪贴板图像（scale=1.0）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct PinSource {
    /// 来源标签（探针/E2E 判别用）："captures" | "clipboard"。
    source: &'static str,
    rgba: std::sync::Arc<Vec<u8>>,
    w_px: u32,
    h_px: u32,
    scale: f64,
}

/// 来源顺序（plan C5）：① CAPTURES 最后一轮的 rgba（未释放时——release_captures 后为
/// 空 Arc，跳过）；② arboard 剪贴板图像（用户刚复制的任意图像）。两者皆无 → Err
/// 「剪贴板没有图像且无最近截图」。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn resolve_source() -> Result<PinSource, String> {
    if let Some(c) = commands::latest_capture_rgba() {
        return Ok(PinSource { source: "captures", rgba: c.rgba, w_px: c.w_px, h_px: c.h_px, scale: c.scale });
    }
    let img = arboard::Clipboard::new()
        .and_then(|mut cb| cb.get_image())
        .map_err(|e| format!("剪贴板没有图像且无最近截图: {e}"))?;
    let (w, h) = (img.width as u32, img.height as u32);
    if w == 0 || h == 0 {
        return Err("剪贴板没有图像且无最近截图: empty image".into());
    }
    Ok(PinSource { source: "clipboard", rgba: std::sync::Arc::new(img.bytes.into_owned()), w_px: w, h_px: h, scale: 1.0 })
}

// —— 命令 —--

/// F2 / 托盘：把最近截图或剪贴板图像钉成贴图窗口。async 命令（涉及窗口操作，plan
/// 线程模型纪律）：图像解析在调用线程，窗口创建经 run_on_main_thread 交付主线程
/// （原生 NSWindow 必须 main thread；与 wry 窗口的「禁止主线程」相反——那是 webview
/// 窗口的消息循环约束，AppKit 原生窗口恰恰相反）。
#[tauri::command(async)]
pub fn pin_latest(app: AppHandle) -> Result<(), String> {
    do_pin_latest(&app)
}

#[cfg(target_os = "macos")]
fn do_pin_latest(app: &AppHandle) -> Result<(), String> {
    let src = resolve_source()?;
    spawn_pin_window(app, src.source, src.rgba, src.w_px, src.h_px, src.scale)?;
    Ok(())
}

/// 共用建窗管道（M0 重构：pin_latest 与 pin_from_path 同路）：Esc 钩子注入 → 主显示器
/// 中心 − 半尺寸 + 级联偏移定位 → run_on_main_thread 建 PinWindow 入册。返回贴图 id
/// （PIN_SEQ 分配；建窗失败仅 eprintln 降级，与既有 pin_latest 口径一致）。
#[cfg(target_os = "macos")]
fn spawn_pin_window(
    app: &AppHandle,
    source: &str,
    rgba: std::sync::Arc<Vec<u8>>,
    w_px: u32,
    h_px: u32,
    scale: f64,
) -> Result<u64, String> {
    // 尺寸上限（M3 继承项③）：所有建窗来源（captures/剪贴板/历史文件）的单一
    // 咽喉——>4K 源在此降采样后才进入定位与 PinWindow::create（预算裁定见
    // PIN_MAX_EDGE_PX 注）。
    let (rgba, w_px, h_px) = cap_pin_rgba(rgba, w_px, h_px);
    // Esc 钩子（幂等注入）：贴图按键 Esc → remove_pin_on_main（主线程回调内直接执行）
    hawksnap_platform_mac::pin::set_on_escape(remove_pin_on_main);
    // 双击钩子（M2 Task M3，幂等注入）：贴图双击 → open_pin_editor_on_main（主线程
    // 回调内取快照，开编辑器在独立线程）
    hawksnap_platform_mac::pin::set_on_dblclick(open_pin_editor_on_main);
    // AppHandle 存档（双击回调无参携带，见 APP 注）；首建后 set 失败为良性（同值）。
    let _ = APP.set(app.clone());

    // 位置：主显示器中心 − 半尺寸 + 级联偏移。captured_display_frames 为全局逻辑坐标
    // （左上原点，ScreenFrame 同源）；无捕获几何（如纯剪贴板/历史文件来源）→ 1512×982
    // 逻辑分辨率中心的回退值。
    let (cw, ch) = primary_display_center();
    let logical_w = w_px as f64 / scale;
    let logical_h = h_px as f64 / scale;
    let id = PIN_SEQ.fetch_add(1, Ordering::SeqCst);
    let (ox, oy) = next_pin_offset(id);
    let top_left = (cw - logical_w * 0.5 + ox, ch - logical_h * 0.5 + oy);

    // 探针（常驻，廉价，每次贴图一行）：E2E 判别来源/落点是否合理。
    eprintln!(
        "[hawksnap] pin: id={id} src={source} {w_px}×{h_px} scale={scale} top_left=({:.0},{:.0})",
        top_left.0, top_left.1
    );
    app.run_on_main_thread(move || {
        match hawksnap_platform_mac::pin::PinWindow::create(rgba, w_px, h_px, top_left, scale, id) {
            Ok(win) => {
                PINS.lock().unwrap_or_else(|e| e.into_inner()).insert(id, win);
            }
            Err(e) => eprintln!("[hawksnap] 贴图窗口创建失败: {e}"),
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(id)
}

/// 非 macOS：无原生贴图窗口（Plan D CI 矩阵），统一 Err——与 do_pin_latest 同口径。
#[cfg(not(target_os = "macos"))]
fn spawn_pin_window(
    _app: &AppHandle,
    _source: &str,
    _rgba: std::sync::Arc<Vec<u8>>,
    _w_px: u32,
    _h_px: u32,
    _scale: f64,
) -> Result<u64, String> {
    Err("unsupported platform".into())
}

/// 主显示器中心（全局逻辑坐标，左上原点）：captured_display_frames 里含原点 (0,0) 的
/// 帧视为主显示器（xcap/CGDisplayBounds 与 NSScreen 同一原点口径），否则取第一个；
/// 全空（未捕获过）→ (756.0, 491.0)（1512×982 逻辑分辨率的一半，主流 Mac 回退值）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn primary_display_center() -> (f64, f64) {
    let frames = commands::captured_display_frames();
    let f = frames
        .iter()
        .find(|f| f.x <= 0.0 && f.y <= 0.0)
        .or_else(|| frames.first());
    match f {
        Some(f) => (f.x + f.w / 2.0, f.y + f.h / 2.0),
        None => (756.0, 491.0),
    }
}

/// Esc 钩子（仅主线程调用——AppKit keyDown 回调内）：移除贴图。Drop（含 close）
/// 发生在本调用栈上 = 主线程（PinWindow 的 AppKit 纪律，见 pin.rs SAFETY 注）。
#[cfg(target_os = "macos")]
fn remove_pin_on_main(id: u64) {
    if let Some(p) = PINS.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
        p.close(); // 幂等：orderOut + close 后 closed 置位，Drop 不再触碰 AppKit
    }
}

/// 双击钩子（M2 Task M3，仅主线程调用——AppKit mouseUp 回调内）：贴图当前帧落盘
/// 临时 PNG → editor_open（origin=Pin）。线程纪律：主线程栈内只做「取 Arc 克隆 +
/// 锁内查表」（image_rgba 访问器是纯数据读取，无 AppKit 消息以外的代价；PNG 编码
/// 数十 ms 不得占主线程）→ 编码/落盘/建编辑器窗全部丢独立线程（wry 建窗禁主线程，
/// 与托盘 shot 入口同一纪律）。贴图已不在册（竞态：双击与 Esc 关闭相撞）→ 静默返回。
#[cfg(target_os = "macos")]
fn open_pin_editor_on_main(id: u64) {
    let snap = {
        let pins = PINS.lock().unwrap_or_else(|e| e.into_inner());
        pins.get(&id).map(|p| p.image_rgba())
    };
    let Some((rgba, w_px, h_px)) = snap else {
        return; // 贴图已关闭（双击与 Esc 相撞的良性竞态）
    };
    let app = APP.get().cloned();
    // 探针（常驻，廉价，每次双击一行）：E2E 判别双击链路已进入 Rust 侧。
    eprintln!("[hawksnap] pin dblclick: id={id} {w_px}×{h_px}");
    std::thread::spawn(move || {
        if let Err(e) = open_pin_editor(app, rgba, w_px, h_px, id) {
            eprintln!("[hawksnap] 贴图双击打开编辑器失败: {e}");
        }
    });
}

/// 贴图双击临时 PNG 落盘路径（纯函数，生产与测试同源——M3 评审 Finding 2）：
/// `$TMPDIR/hawksnap-pin-{id}-{ts}.png`，毫秒时间戳段（同贴图重复双击不互撞）。
/// 仅 macOS（唯一调用方 open_pin_editor 与平台同门控）。
#[cfg(target_os = "macos")]
fn pin_temp_path(id: u64) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "hawksnap-pin-{id}-{}.png",
        chrono::Local::now().timestamp_millis()
    ))
}

/// 双击的独立线程半程（M2 Task M3）：RGBA → PNG（hawksnap_core 纯函数）→ 临时文件
/// [`pin_temp_path`] → editor_open（origin=Pin，保存回写贴图窗口——契约见 editor.rs
/// 模块注）。app 缺席（无贴图创建即双击不可能发生，防御）→ Err。
#[cfg(target_os = "macos")]
fn open_pin_editor(
    app: Option<AppHandle>,
    rgba: std::sync::Arc<Vec<u8>>,
    w_px: u32,
    h_px: u32,
    id: u64,
) -> Result<(), String> {
    let app = app.ok_or("AppHandle 未就绪".to_string())?;
    let png = hawksnap_core::crop::rgba_to_png(&rgba, w_px, h_px)
        .map_err(|e| format!("贴图帧编码失败: {e}"))?;
    let path = pin_temp_path(id);
    std::fs::write(&path, &png).map_err(|e| format!("临时文件写入失败: {e}"))?;
    crate::editor::editor_open(
        app,
        path.to_string_lossy().into_owned(),
        Some(crate::editor::EditorOrigin::Pin { pin_id: id }),
    )
}

#[cfg(not(target_os = "macos"))]
fn do_pin_latest(_app: &AppHandle) -> Result<(), String> {
    Err("unsupported platform".into())
}

/// M0 历史操作「贴图」：把历史条目的 PNG 文件钉成贴图窗口。async 命令：读文件 +
/// 解码（core png_to_rgba，几 MB 级）在运行时线程，建窗经 spawn_pin_window 交付
/// 主线程（同 pin_latest 纪律）。落盘文件即物理像素 → scale=1.0；返回贴图 id。
#[tauri::command(async)]
pub fn pin_from_path(app: AppHandle, path: String) -> Result<u64, String> {
    do_pin_from_path(&app, &path)
}

fn do_pin_from_path(app: &AppHandle, path: &str) -> Result<u64, String> {
    if path.trim().is_empty() {
        return Err("路径为空".into());
    }
    let png = std::fs::read(path).map_err(|e| format!("读取截图文件失败: {e}"))?;
    let (rgba, w_px, h_px) = hawksnap_core::crop::png_to_rgba(&png)
        .map_err(|e| format!("截图文件解码失败: {e}"))?;
    if w_px == 0 || h_px == 0 {
        return Err("截图文件为空图".into());
    }
    spawn_pin_window(app, "history", std::sync::Arc::new(rgba), w_px, h_px, 1.0)
}

#[tauri::command(async)]
pub fn pin_set_opacity(app: AppHandle, id: u64, opacity: f64) -> Result<(), String> {
    do_pin_set_opacity(&app, id, opacity)
}

#[cfg(target_os = "macos")]
fn do_pin_set_opacity(app: &AppHandle, id: u64, opacity: f64) -> Result<(), String> {
    let op = clamp_opacity(opacity);
    app.run_on_main_thread(move || {
        if let Some(p) = PINS.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
            p.set_opacity(op);
        }
    })
    .map_err(|e| e.to_string())
}

#[cfg(not(target_os = "macos"))]
fn do_pin_set_opacity(_app: &AppHandle, _id: u64, _opacity: f64) -> Result<(), String> {
    Err("unsupported platform".into())
}

#[tauri::command(async)]
pub fn pin_zoom(app: AppHandle, id: u64, factor: f64) -> Result<(), String> {
    do_pin_zoom(&app, id, factor)
}

#[cfg(target_os = "macos")]
fn do_pin_zoom(app: &AppHandle, id: u64, factor: f64) -> Result<(), String> {
    app.run_on_main_thread(move || {
        if let Some(p) = PINS.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
            let target = clamp_zoom(p.scale(), factor);
            p.zoom_to(target);
        }
    })
    .map_err(|e| e.to_string())
}

#[cfg(not(target_os = "macos"))]
fn do_pin_zoom(_app: &AppHandle, _id: u64, _factor: f64) -> Result<(), String> {
    Err("unsupported platform".into())
}

#[tauri::command(async)]
pub fn pin_close(app: AppHandle, id: u64) -> Result<(), String> {
    do_pin_close(&app, id)
}

#[cfg(target_os = "macos")]
fn do_pin_close(app: &AppHandle, id: u64) -> Result<(), String> {
    app.run_on_main_thread(move || remove_pin_on_main(id)).map_err(|e| e.to_string())
}

#[cfg(not(target_os = "macos"))]
fn do_pin_close(_app: &AppHandle, _id: u64) -> Result<(), String> {
    Err("unsupported platform".into())
}

#[tauri::command(async)]
pub fn pin_close_all(app: AppHandle) -> Result<(), String> {
    do_pin_close_all(&app)
}

#[cfg(target_os = "macos")]
fn do_pin_close_all(app: &AppHandle) -> Result<(), String> {
    // 托盘入口线程派发后到此处已在异步线程；close 必须主线程 → run_on_main_thread。
    // 先取 id 快照（锁不跨 run_on_main_thread 闭包），逐个 remove（闭包内主线程执行）。
    let ids: Vec<u64> = {
        let pins = PINS.lock().unwrap_or_else(|e| e.into_inner());
        pins.keys().copied().collect()
    };
    for id in ids {
        app.run_on_main_thread(move || remove_pin_on_main(id)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn do_pin_close_all(_app: &AppHandle) -> Result<(), String> {
    Err("unsupported platform".into())
}

// —— 编辑器保存回写贴图（M2 Task M3） —--

/// PNG base64 → (rgba, w, h)（pin_set_image 的载荷解码，纯函数可测）。校验链与
/// editor_save 同源：base64 解码 → PNG 解码（尺寸合法性由 png_to_rgba 兜底）→
/// 空图拒绝。
fn decode_pin_png(png_base64: &str) -> Result<(Vec<u8>, u32, u32), String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png_base64.trim())
        .map_err(|e| format!("PNG 数据解码失败: {e}"))?;
    let (rgba, w, h) =
        hawksnap_core::crop::png_to_rgba(&bytes).map_err(|e| format!("截图文件解码失败: {e}"))?;
    if w == 0 || h == 0 {
        return Err("截图文件为空图".into());
    }
    Ok((rgba, w, h))
}

/// 编辑器保存回写（M2 Task M3，origin=Pin）：把编辑后的整帧 RGBA 换进贴图窗口。
/// 主线程交付（set_image 触碰 AppKit）；贴图已关闭（编辑期间用户 Esc 掉贴图的竞态）
/// → 静默 Ok（任务裁定：ignore silently）。非 macOS：Err（Plan D CI 矩阵口径）。
pub(crate) fn pin_set_image_rgba(
    app: &AppHandle,
    id: u64,
    rgba: Vec<u8>,
    w_px: u32,
    h_px: u32,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        app.run_on_main_thread(move || {
            if let Some(p) = PINS.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
                if let Err(e) = p.set_image(std::sync::Arc::new(rgba), w_px, h_px) {
                    eprintln!("[hawksnap] 贴图回写失败: {e}");
                }
            }
        })
        .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, id, rgba, w_px, h_px);
        Err("unsupported platform".into())
    }
}

/// 贴图回写命令（M2 Task M3 契约面）：editor_save 内部直调
/// [`pin_set_image_rgba`]（同进程，无 IPC）；本命令提供等价的显式调用面（png base64
/// 载荷与 editor_save 同款传输），当前无前端调用方，注册以固定契约。
#[tauri::command(async)]
pub fn pin_set_image(app: AppHandle, id: u64, png_base64: String) -> Result<(), String> {
    let (rgba, w, h) = decode_pin_png(&png_base64)?;
    pin_set_image_rgba(&app, id, rgba, w, h)
}

#[cfg(test)]
mod pins_tests {
    use super::*;

    #[test]
    fn pin_offset_cascades_deterministically() {
        // 确定性：同 n 同结果
        assert_eq!(next_pin_offset(3), next_pin_offset(3));
        // 级联：前两个偏移不同，且按 24 递增
        let (x0, y0) = next_pin_offset(0);
        let (x1, y1) = next_pin_offset(1);
        assert_ne!((x0, y0), (x1, y1), "相邻贴图必须错开");
        assert_eq!((x1 - x0, y1 - y0), (24.0, 24.0));
        // 8 个一循环（9+ 贴图不级联出屏）
        assert_eq!(next_pin_offset(8), next_pin_offset(0));
        assert_eq!(next_pin_offset(17), next_pin_offset(1));
    }

    #[test]
    fn zoom_clamps_to_10_3200_percent() {
        assert_eq!(clamp_zoom(0.2, 0.5), 0.1, "下限 0.1（10%）");
        assert_eq!(clamp_zoom(2.0, 2.0), 3.2, "上限 3.2（3200%）");
        assert_eq!(clamp_zoom(1.0, 1.0), 1.0, "中性缩放不变");
        assert_eq!(clamp_zoom(0.5, 2.0), 1.0, "正常区间内直接乘");
    }

    #[test]
    fn opacity_clamps_to_20_100_percent() {
        assert_eq!(clamp_opacity(0.1), 0.2, "下限 0.2");
        assert_eq!(clamp_opacity(1.5), 1.0, "上限 1.0");
        assert_eq!(clamp_opacity(0.5), 0.5, "区间内不变");
    }

    /// M3 继承项③：长边 >4096 的源等比降采样到 4096；≤4096 原样（尺寸与字节不动）。
    #[test]
    fn pin_rgba_cap_downsamples_over_4096_and_passes_through_below() {
        // 4097×2（长边超限 1px，体量小便于测试）→ 4096×2：长边钳到上限、比例保持。
        let rgba = vec![7u8; 4097 * 2 * 4];
        let (data, nw, nh) = cap_pin_rgba(std::sync::Arc::new(rgba), 4097, 2);
        assert_eq!((nw, nh), (4096, 2), "长边钳到上限、短边等比");
        assert_eq!(data.len(), 4096 * 2 * 4);
        // 恰在上限：原样放行。
        let edge = vec![1u8; 4096 * 2 * 4];
        let (data, nw, nh) = cap_pin_rgba(std::sync::Arc::new(edge), 4096, 2);
        assert_eq!((nw, nh), (4096, 2), "恰在上限不降采样");
        assert_eq!(data[..8], [1u8; 8], "原样返回（字节不动）");
        // 远小于上限：原样。
        let (data, nw, nh) = cap_pin_rgba(std::sync::Arc::new(vec![3u8; 100 * 50 * 4]), 100, 50);
        assert_eq!((nw, nh), (100, 50));
        assert_eq!(data.len(), 100 * 50 * 4);
    }

    #[test]
    fn decode_pin_png_roundtrips_and_rejects_garbage() {
        // 正路：2×1 PNG → rgba 长度 8 + dims 保真（回写贴图的载荷契约）
        let png = hawksnap_core::crop::rgba_to_png(&[1, 2, 3, 255, 4, 5, 6, 255], 2, 1).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let (rgba, w, h) = decode_pin_png(&b64).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgba.len(), 8);
        // 脏载荷：非 base64 / 非 PNG / 空图
        let err = decode_pin_png("!!!not-base64!!!").unwrap_err();
        assert!(err.contains("PNG 数据解码失败"), "got: {err}");
        let not_png = base64::engine::general_purpose::STANDARD.encode(b"JPEG data, honestly");
        let err = decode_pin_png(&not_png).unwrap_err();
        assert!(err.contains("截图文件解码失败"), "got: {err}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pin_editor_temp_path_matches_contract() {
        // M3 评审 Finding 2：直呼生产函数 pin_temp_path（此前测试用本地闭包复刻格式串
        // ——同义反复，生产格式变更仍恒绿）。契约：$TMPDIR/hawksnap-pin-{id}-{ts}.png，
        // 时间戳段为可解析毫秒数（同贴图重复双击不互撞）。
        let p = pin_temp_path(7);
        assert_eq!(
            p.parent().map(|d| d.to_path_buf()),
            Some(std::env::temp_dir()),
            "落盘 $TMPDIR"
        );
        let name = p.file_name().and_then(|n| n.to_str()).expect("UTF-8 文件名");
        let ts = name
            .strip_prefix("hawksnap-pin-7-")
            .and_then(|s| s.strip_suffix(".png"))
            .expect("文件名契约 hawksnap-pin-{id}-{ts}.png");
        assert!(ts.parse::<i64>().is_ok(), "时间戳段必须可解析（毫秒）: {ts}");
    }
}
