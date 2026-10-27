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

use hawksnap_core::capture::ScreenGrabber;
use hawksnap_core::geometry::Rect;
use hawksnap_core::{crop::{crop_rgba, rgba_to_png}, naming::{expand, NamingCtx}};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DisplayPayload {
    pub display_id: u64, pub x: i32, pub y: i32,
    pub width: u32, pub height: u32, pub scale: f64, pub frame_url: String,
    /// 截图流程代数（终审 wave）：start_capture_flow 每轮分配（FLOW_SEQ 单调递增）
    /// 并覆写——capture_screens 先填 0 占位，emit 前统一改为本轮代数。前端按 flow
    /// 丢弃旧代迟到补发（overlay-ready 300ms 补发线程 vs 新一轮的竞态）。
    pub flow: u64,
    /// N1：本轮覆盖层用途（"capture" | "record" 小写单词，CaptureIntent::as_payload_str
    /// 派生）。capture_screens 先填 "capture" 占位，flow 侧按本轮意图覆写——前端按
    /// record 切换提示横幅/隐藏动作浮条/把确认路由到 record_start_region。
    pub intent: &'static str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishReq { pub display_id: u64, pub rect: Rect, pub copy: bool, pub save: bool }

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureOutcome { pub copied: bool, pub saved_to: Option<String> }

pub fn to_physical_rect(logical: &Rect, scale: f64) -> Rect {
    Rect {
        x: (logical.x as f64 * scale).round() as i32,
        y: (logical.y as f64 * scale).round() as i32,
        w: (logical.w as f64 * scale).round() as u32,
        h: (logical.h as f64 * scale).round() as u32,
    }
}

pub fn save_path_for(dir: &std::path::Path, app_name: &str, ext: &str) -> std::path::PathBuf {
    let now = chrono::Local::now();
    let ctx = NamingCtx {
        app: app_name.to_string(),
        date: now.format("%Y-%m-%d").to_string(),
        time: now.format("%H%M%S").to_string(),
    };
    let name = expand(&format!("{{app}} {{date}} {{time}}"), &ctx, |n| {
        dir.join(format!("{n}.{ext}")).exists()
    });
    dir.join(format!("{name}.{ext}"))
}

#[tauri::command]
pub async fn capture_screens(app: AppHandle) -> Result<Vec<DisplayPayload>, String> {
    let mut grabber = hawksnap_platform_mac::XcapGrabber::new();
    let frames = grabber.grab_all().map_err(|e| e.to_string())?;
    let cache = app.path().cache_dir().map_err(|e| e.to_string())?.join("hawksnap/grabs");
    std::fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(frames.len());
    let mut captures = CAPTURES.lock().unwrap_or_else(|e| e.into_inner());
    for f in frames {
        // Plan B Task 1 cache-bust：覆盖层窗口常驻复用后页面不再重载，同路径覆盖写会命中
        // Webview 的 URL 缓存而显示上一轮旧帧，故文件名带毫秒时间戳强制每轮重新加载
        // （asset 协议对 query 参数不可靠，版本化文件名是确定性方案；仍在 $CACHE/** scope 内）。
        // 写入前删除上一轮同 display 的冻结帧，缓存目录不随轮数增长。
        if let Some(prev) = captures.get(&f.display_id) {
            let _ = std::fs::remove_file(&prev.frame_path);
        }
        // Task 2b 一次性兼容：清掉 Task 2a 及之前版本化方案写入的遗留 {id}-*.png，
        // 前端已不再经 asset 协议加载 PNG，留着只会占缓存目录。
        let legacy_prefix = format!("{}-", f.display_id);
        if let Ok(entries) = std::fs::read_dir(&cache) {
            for e in entries.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&legacy_prefix) && name.ends_with(".png") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        // Task 2b（前端原始帧直通）：冻结帧不再编码 PNG，直接落盘整帧物理 RGBA，
        // 前端 fetch 后经 putImageData 画出——省掉整帧 PNG 编码 + asset 协议侧 PNG 解码，
        // h2o（created→shown）从 ~108ms 量级再砍一截。grabber 层的 ScreenFrame.png 编码
        // 仍保留（core 契约 + 集成测试依赖），流程已不消费——是否从抓取路径移除属后续
        // core 契约决策，此处仅注记。
        let file = cache.join(format!(
            "{}-{}.raw",
            f.display_id,
            chrono::Local::now().timestamp_millis()
        ));
        std::fs::write(&file, &f.rgba).map_err(|e| e.to_string())?;
        // raw-RGBA 捕获缓存（Plan B Task 2）：物理尺寸按 (width*scale).round() 计算，
        // finish_capture 直通行复制裁剪与剪贴板，全程不再读冻结帧文件。frame_path 仅供
        // 下一轮删除上一轮冻结帧。
        let (pw, ph) = (
            (f.width as f64 * f.scale).round() as u32,
            (f.height as f64 * f.scale).round() as u32,
        );
        captures.insert(f.display_id, CaptureMeta {
            x: f.x, y: f.y, width: f.width, height: f.height,
            scale: f.scale, width_px: pw, height_px: ph,
            rgba: std::sync::Arc::new(f.rgba.clone()), frame_path: file.clone(),
        });
        out.push(DisplayPayload {
            display_id: f.display_id, x: f.x, y: f.y,
            width: f.width, height: f.height, scale: f.scale,
            frame_url: format!("hawk-grab://{}", file.display()),
            flow: 0, // 占位：start_capture_flow 在 emit 前统一覆写为本轮代数（见结构体注）
            intent: "capture", // 占位：flow 侧按本轮 CaptureIntent 覆写（N1）
        });
    }
    Ok(out)
}

/// RGBA → 剪贴板（finish_capture 直通路径与编辑器「来源=截图」保存共用——M2 Task
/// M3 从 finish_capture 抽取为单一 helper）。premultiplied RGBA 直送 arboard，零编码。
pub fn copy_rgba_to_clipboard(rgba: &[u8], w: u32, h: u32) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_image(arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: rgba.to_vec().into(),
    })
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn finish_capture(app: AppHandle, req: FinishReq) -> Result<CaptureOutcome, String> {
    // Task 11 评审 Finding 1：前端 invoke 即确认时刻，在函数首行打点（stage: confirm）。
    // G3 的确认→剪贴板耗时 = clipboard_done - confirm；旧口径 overlay_shown→clipboard_done
    // 混入了人工框选时间，无法作为 ≤100ms 目标的证据。
    crate::perf::mark("confirm");
    // raw-RGBA 直通（Plan B Task 2）：不再读冻结 PNG。锁内只做查找 + Arc clone 并立即
    // 释放；行复制裁剪与剪贴板全程无锁（单用户场景总耗时 <50ms，无需 spawn_blocking）。
    let meta = { CAPTURES.lock().unwrap_or_else(|e| e.into_inner()).get(&req.display_id).cloned() }
        .ok_or("display not captured")?;
    let physical = to_physical_rect(&req.rect, meta.scale);
    let cropped = crop_rgba(&meta.rgba, meta.width_px, meta.height_px, &physical)
        .map_err(|e| e.to_string())?;
    // 3) 剪贴板：直接送 RGBA，省去裁剪 PNG 重编码 + 再解码两个环节（helper 与编辑器
    // 「来源=截图」保存共用，M2 Task M3 抽取）
    let mut copied = false;
    if req.copy {
        copy_rgba_to_clipboard(&cropped, physical.w, physical.h)?;
        copied = true;
        // Task 11 埋点：clipboard_done 仅在 req.copy=true 路径打点（M1 流程恒为 true）。
        // 若未来支持“仅保存”，confirm 与 clipboard_done 行数将不等，bench 长度校验会
        // 报“数据不齐”——属预期守护而非缺陷。mark_clipboard_done = CSV 追加 +
        // 进程内计数自增（bench_mode 的 c2c 计时门控），二者严格 1:1。
        crate::perf::mark_clipboard_done();
    }
    // 4) 保存：编码只发生在保存路径；格式/质量/目录均来自设置（C2 + C7 接线）。每次读
    // 小 JSON 文件（µs 级）——裁定 2：不为缓存引状态，profile 有实测成本再缓存。
    let mut saved_to = None;
    if req.save {
        let settings = crate::settings::Settings::load(&app);
        // C7 保存目录接线（控制器追加）：settings.save_dir Some(非空) → 用户目录；
        // None → 默认 ~/Pictures/HawkSnap（与 C2 之前行为一致）。
        let dir = resolve_save_dir(&settings).ok_or("no picture dir")?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let (bytes, ext) = if settings.output_format == "jpeg" {
            (
                hawksnap_core::crop::rgba_to_jpeg(
                    &cropped, physical.w, physical.h, settings.jpeg_quality,
                )
                .map_err(|e| e.to_string())?,
                "jpeg",
            )
        } else {
            (
                rgba_to_png(&cropped, physical.w, physical.h).map_err(|e| e.to_string())?,
                "png",
            )
        };
        let path = save_path_for(&dir, &frontmost_app_name().unwrap_or_else(|| "HawkSnap".into()), ext);
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        saved_to = Some(path.to_string_lossy().into_owned());
        // C4 保存即入库：缩略图落盘 + history::record。历史是旁路（裁定）：整条链任一步
        // 失败只 eprintln，绝不影响保存本身的成败——闭包包住全部步骤统一降级。
        // 剪贴板-only 捕获（copy=true, save=false）无落盘文件，不入库（完整版再议
        // 临时文件方案）；w/h 记物理像素（physical），app 暂记 "HawkSnap"——frontmost
        // 检测尚未接入（见 frontmost_app_name 注），接入后同步替换此处。
        if let Err(e) = record_history(&app, &path, &dir, &cropped, &physical) {
            eprintln!("[hawksnap] 历史记录失败: {e}");
        }
    }
    Ok(CaptureOutcome { copied, saved_to })
}

/// 覆盖层「标注」入口（M2 Task M3）：当前选区裁剪直接落盘临时 PNG → editor_open
/// （origin=Capture）→ 返回。**不复制剪贴板、不保存、不入库**（裁定：标注中截图先
/// 临时存盘再编辑，确认后才复制剪贴板 + 存 save_dir 入库——保存副作用契约见 editor.rs
/// 模块注「来源契约」）。req 复用 FinishReq（copy/save 字段忽略——语义由编辑器保存
/// 路径接管，保留字段只为前端载荷同构）。编辑器打开失败 → Err 上抛（覆盖层保持，
/// 用户可重试）。
///
/// 隐藏覆盖层**不在本命令内**（M3 评审 Finding 1）：命令侧无条件 hide_all_overlays
/// （含 release_captures）会把旧链在途期（冷编辑器建窗 ~100-600ms）新启动一轮刚 show
/// 的覆盖层藏掉并释放新一轮 CAPTURES，下一轮 confirm 即「display not captured」。hide
/// 半程拆为 [`hide_overlays_after_annotate`]，由前端在 flow 代数守卫**之后**调用
/// （守卫权威在前端，与 emit_capture_done 同款）。
#[tauri::command]
pub async fn annotate_crop(app: AppHandle, req: FinishReq) -> Result<(), String> {
    // 与 finish_capture 同款锁纪律：锁内只做查找 + Arc clone，裁剪全程无锁。
    let meta = { CAPTURES.lock().unwrap_or_else(|e| e.into_inner()).get(&req.display_id).cloned() }
        .ok_or("display not captured")?;
    let physical = to_physical_rect(&req.rect, meta.scale);
    let cropped = crop_rgba(&meta.rgba, meta.width_px, meta.height_px, &physical)
        .map_err(|e| e.to_string())?;
    let path = write_annotate_png(&cropped, physical.w, physical.h)?;
    crate::editor::editor_open(
        app,
        path.to_string_lossy().into_owned(),
        Some(crate::editor::EditorOrigin::Capture),
    )
}

/// 标注链的隐藏半程（M3 评审 Finding 1 拆分，见 [`annotate_crop`] 注）：前端在 flow
/// 代数守卫之后调用——本轮未被新流程作废才执行 hide_all_overlays（含 release_captures，
/// 与 emit_capture_done 收尾口径一致）。旧链迟到调用已被前端守卫拦截；前端守卫与调用
/// 之间仍有的极窄窗口与 emit_capture_done 相同（同款可接受竞态，新一轮启动自愈）。
#[tauri::command]
pub async fn hide_overlays_after_annotate(app: AppHandle) -> Result<(), String> {
    crate::overlay::hide_all_overlays(&app)
}

/// 标注临时 PNG 落盘（M2 Task M3，纯函数可测）：`$TMPDIR/hawksnap-annotate-{ms}-{seq}.png`。
/// 命名必须**保证**不互撞：毫秒时间戳单独用并不够——同一毫秒内的两次标注（小图 +
/// 快机器）会撞名，后写覆盖先写（编辑器可能拿到别轮的图）。故追加进程内单调序号
/// （`fetch_add` 原子，跨线程唯一）；文件无人主动清理（临时目录系统管理，编辑器
/// 打开期间必须存活，保存后由用户在编辑器语义内处置）。
fn write_annotate_png(rgba: &[u8], w: u32, h: u32) -> Result<std::path::PathBuf, String> {
    let png = rgba_to_png(rgba, w, h).map_err(|e| e.to_string())?;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "hawksnap-annotate-{}-{seq}.png",
        chrono::Local::now().timestamp_millis()
    ));
    std::fs::write(&path, &png).map_err(|e| format!("临时文件写入失败: {e}"))?;
    Ok(path)
}

use std::sync::{LazyLock, Mutex};
use std::collections::HashMap;
// raw-RGBA 捕获缓存（Plan B Task 2，取代 FRAME_META）：每 display 存冻结帧的 scale、
// 物理尺寸与整帧 RGBA（Arc 共享，finish_capture 锁外裁剪）。frame_path 仅供下一轮
// capture_screens 删除上一轮冻结帧文件（Task 2b 起为版本化 .raw 文件，Task 1 的
// 版本化文件名 cache-bust 方案仍生效）。
// Plan B Task 7：补存逻辑 x/y/width/height（类型与语义同 DisplayPayload，ScreenFrame
// 直传）——悬停回退链第三级（显示器全帧）的候选来源，经 captured_display_frames 取出。
// HashMap::new 非 const fn，static 用 LazyLock 包装。
static CAPTURES: LazyLock<Mutex<HashMap<u64, CaptureMeta>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
#[derive(Clone)]
struct CaptureMeta {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale: f64,
    width_px: u32,
    height_px: u32,
    rgba: std::sync::Arc<Vec<u8>>,
    frame_path: std::path::PathBuf, // 仅 capture_screens 删除上一轮冻结帧用
}

/// 显示器全帧（全局逻辑坐标，f64 域，与 RectLog/AxFrame 同形）：悬停回退链第三级
/// （全屏模式）的候选。几何快照，刻意不含 rgba/frame_path 等重数据。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayFrame { pub x: f64, pub y: f64, pub w: f64, pub h: f64 }

/// 悬停回退链第三级的显示器候选（Plan B Task 7）：锁内只克隆几何字段（rgba 是 Arc
/// 不碰、frame_path 不取），锁绝不跨帧持有。try_lock 而非 lock：调用方是主线程上的
/// hover handler（~60Hz），而 capture_screens 会持同一把锁跨越整帧 RGBA 落盘（数十
/// ms）——阻塞等待会卡主线程事件循环；锁被占用时返回空候选，该 tick 降级为无显示器
/// 回退（回退是增强非关键路径，下一 tick 重试自愈）。坐标系 = 全局逻辑（ScreenFrame
/// 同源）。CAPTURES 按轮覆盖写同 display_id，读到的总是最近一轮捕获的几何；陈旧
/// display（用户拔掉显示器后未再触发新一轮）可能残留，但覆盖层不可见时悬停总闸早已
/// 短路，残留帧至多影响下一轮首帧前的窗口期，属可接受降级。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn captured_display_frames() -> Vec<DisplayFrame> {
    let Ok(captures) = CAPTURES.try_lock() else { return Vec::new() };
    captures
        .values()
        .map(|m| DisplayFrame {
            x: m.x as f64, y: m.y as f64, w: m.width as f64, h: m.height as f64,
        })
        .collect()
}

/// 指定显示器的全局几何快照（长截图把 display 本地选区换算成全局 Quartz 坐标用）。
///
/// 与 [`captured_display_frames`] 同源（CAPTURES 几何条目），区别只在于按
/// display_id 精确取一份而非全部。释放整帧（release_captures）后几何仍在——
/// 长截图启动时覆盖层尚未收口，正是要读它。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn captured_display_geometry(display_id: u64) -> Option<DisplayFrame> {
    let captures = CAPTURES.lock().unwrap_or_else(|e| e.into_inner());
    captures.get(&display_id).map(|m| DisplayFrame {
        x: m.x as f64, y: m.y as f64, w: m.width as f64, h: m.height as f64,
    })
}

/// 主显示器快照（bench_mode 确认链用）：display_id 与全帧几何取**同一条目**、一次锁内
/// 产出——bench 要向 finish_capture 传该显示器的 id 与其内部 rect，二者必须同源。
/// DisplayFrame 刻意不含 display_id（hover 的相等性断言依赖纯几何，见结构体与 hover
/// 测试注），故单开此访问器而非扩字段。「主显示器」口径沿用 primary_display_center：
/// 含原点 (0,0) 的帧优先，否则取面积最大者。锁内只做几何拷贝。
pub(crate) fn primary_display() -> Option<(u64, DisplayFrame)> {
    let captures = CAPTURES.lock().unwrap_or_else(|e| e.into_inner());
    let frame = |id: &u64, m: &CaptureMeta| (*id, DisplayFrame {
        x: m.x as f64, y: m.y as f64, w: m.width as f64, h: m.height as f64,
    });
    captures
        .iter()
        .find(|(_, m)| m.x <= 0 && m.y <= 0)
        .map(|(id, m)| frame(id, m))
        .or_else(|| {
            captures
                .iter()
                .max_by_key(|(_, m)| m.width as u64 * m.height as u64)
                .map(|(id, m)| frame(id, m))
        })
}

/// 贴图 pin 的图像来源第一级（C5）：CAPTURES 最近一轮的非空整帧。display_id 取最大者
/// （多显示器无主次概念时稳定取一个）；release_captures 后 rgba 全为空 → None，调用方
/// 落到剪贴板来源。锁内只做 Arc clone + 几何拷贝，不跨 IO。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct LatestCapture {
    pub rgba: std::sync::Arc<Vec<u8>>,
    pub w_px: u32,
    pub h_px: u32,
    pub scale: f64,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn latest_capture_rgba() -> Option<LatestCapture> {
    let captures = CAPTURES.lock().unwrap_or_else(|e| e.into_inner());
    captures
        .iter()
        .filter(|(_, m)| !m.rgba.is_empty())
        .max_by_key(|(id, _)| *id)
        .map(|(_, m)| LatestCapture {
            rgba: m.rgba.clone(),
            w_px: m.width_px,
            h_px: m.height_px,
            scale: m.scale,
        })
}

/// 流程结束释放整帧 RGBA（终审 wave，G2）：hide_all_overlays 在全部覆盖层隐藏后调用
/// ——把每个条目的 rgba 换成空 Arc，释放 ~24-59MB/屏的整帧钉住内存（此前它常驻至
/// 下一轮 capture_screens 覆盖，是空载残余内存的主力，抵消了 G2 缓解措施）。
/// frame_path 刻意保留：下一轮 capture_screens 删除上一轮冻结帧文件仍需要它。
/// 释放后的条目再走 finish_capture 会因空 rgba 裁剪失败，报「display not captured」
/// 式错误——可接受：本函数恰在没有任何覆盖层能确认的时刻执行（能确认的覆盖层都已
/// 隐藏），确认链在该时刻必然已经完成或已越过代际守卫被作废。锁内只做 Arc 替换
/// （纯赋值、无 IO），锁窗口极短，不与 try_lock 的 hover 主线程路径冲突。
pub fn release_captures() {
    let mut captures = CAPTURES.lock().unwrap_or_else(|e| e.into_inner());
    for meta in captures.values_mut() {
        meta.rgba = std::sync::Arc::new(Vec::new());
    }
}

/// C4：保存成功后的历史入库链（缩略图生成 → thumbs/<stem>.png 落盘 → history::record，
/// prune 已内置于 record——M3 继承项①单点化，所有 kind 平权）。独立成函数便于整体
/// `?` 短路：任一步失败即 Err，由调用方 eprintln 降级（历史是旁路，不回滚、不重试、
/// 不影响 CaptureOutcome）。pub(crate)：编辑器「来源=截图」保存入库复用（M2 Task M3，
/// 语义同链）。kind="region"（截图；duration=None——时长列只对 recording 有意义）。
pub(crate) fn record_history(
    app: &AppHandle,
    path: &std::path::Path,
    dir: &std::path::Path,
    cropped: &[u8],
    physical: &Rect,
) -> Result<(), String> {
    let (thumb_rgba, tw, th) =
        hawksnap_core::crop::rgba_thumbnail(cropped, physical.w, physical.h, 320)
            .map_err(|e| e.to_string())?;
    let thumbs_dir = dir.join("thumbs");
    std::fs::create_dir_all(&thumbs_dir).map_err(|e| e.to_string())?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shot".into());
    let thumb_path = thumbs_dir.join(format!("{stem}.png"));
    let thumb_png = rgba_to_png(&thumb_rgba, tw, th).map_err(|e| e.to_string())?;
    std::fs::write(&thumb_path, &thumb_png).map_err(|e| e.to_string())?;
    crate::history::record(
        app,
        &path.to_string_lossy(),
        &thumb_path.to_string_lossy(),
        physical.w as i64,
        physical.h as i64,
        "HawkSnap",
        "region",
        None,
    )?;
    Ok(())
}

fn dirs_picture_dir() -> Option<std::path::PathBuf> {
    // Windows 无 HOME（用户域为 USERPROFILE）：三平台 CI 矩阵（plan D2）bring-up 修复。
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| std::path::PathBuf::from(h).join("Pictures/HawkSnap"))
}

/// C7：保存目录解析——settings.save_dir 优先（sanitized 已把空白串归一为 None），
/// None → 默认 ~/Pictures/HawkSnap。AppHandle-free，独立成函数便于单测。pub(crate)：
/// 编辑器「来源=截图」入库复用（M2 Task M3）。
pub(crate) fn resolve_save_dir(settings: &crate::settings::Settings) -> Option<std::path::PathBuf> {
    match settings.save_dir.as_deref() {
        Some(dir) if !dir.trim().is_empty() => Some(std::path::PathBuf::from(dir)),
        _ => dirs_picture_dir(),
    }
}

/// M0：把保存目录动态放行给 asset 协议（历史网格缩略图经前端 convertFileSrc 加载）。
/// tauri.conf 的静态 scope 仅 `$CACHE/**`（覆盖层冻结帧所在），而缩略图落在保存目录
/// （默认 ~/Pictures/HawkSnap，用户可自定义任意路径）——静态 glob 无法覆盖自定义目录，
/// 启动与 save_dir 变更（set_settings 成功后）时运行期 allow_directory(recursive)。
/// 重复放行同目录幂等；失败仅 eprintln 降级（网格显示占位块，历史仍是旁路功能，
/// 不值得让初始化/保存失败）。
pub fn allow_asset_scope(app: &AppHandle) {
    let settings = crate::settings::Settings::load(app);
    if let Some(dir) = resolve_save_dir(&settings) {
        use tauri::Manager;
        if let Err(e) = app.asset_protocol_scope().allow_directory(&dir, true) {
            eprintln!("[hawksnap] asset 协议放行保存目录失败 ({}): {e}", dir.display());
        }
    }
}

/// 前台应用名：恒 None（占位）。原「Plan B 接 window 检测后替换」的说法已过时——
/// window/AX 检测在 Plan B 落地时只做了悬停高亮与磁吸候选，未包含 frontmost 名称
/// 查询；保存文件名与历史 app 字段（C4）暂用 "HawkSnap"。接入点：CGWindowListCopyWindowInfo
/// 的 kCGWindowOwnerName（与 Task 4 磁吸候选同源），接入后此处替换、历史入库同步取真名。
fn frontmost_app_name() -> Option<String> { None }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_rect_maps_to_physical() {
        // 逻辑 (10,20,100,50) @2x → 物理 (20,40,200,100)
        let p = to_physical_rect(
            &hawksnap_core::geometry::Rect { x: 10, y: 20, w: 100, h: 50 }, 2.0);
        assert_eq!((p.x, p.y, p.w, p.h), (20, 40, 200, 100));
    }

    #[test]
    fn save_uses_template_and_avoids_collision() {
        let dir = std::env::temp_dir().join(format!("hs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = save_path_for(&dir, "Safari", "png");
        assert!(first.to_string_lossy().contains("Safari"));
        assert!(first.to_string_lossy().ends_with(".png"));
        std::fs::write(&first, b"1").unwrap();
        let second = save_path_for(&dir, "Safari", "png");
        assert_ne!(first, second, "重名时必须生成不同文件名");
        let jpeg = save_path_for(&dir, "Safari", "jpeg");
        assert!(jpeg.to_string_lossy().ends_with(".jpeg"), "ext 参数决定扩展名");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn display_payload_serializes_frame_url() {
        // Task 2b：冻结帧改发原始 RGBA，前端可见契约 pngUrl → frameUrl（唯一变更）
        let p = DisplayPayload {
            display_id: 7, x: 0, y: 0, width: 100, height: 50, scale: 2.0,
            frame_url: "hawk-grab:///tmp/a.raw".into(),
            flow: 1,
            intent: "capture",
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"frameUrl\":\"hawk-grab:///tmp/a.raw\""));
        assert!(!s.contains("pngUrl"), "旧字段不得残留");
        // N1：intent 字段随载荷在场（capture_screens 占位值）
        assert!(s.contains("\"intent\":\"capture\""));
    }

    #[test]
    fn save_dir_resolves_from_settings() {
        // C7 保存目录接线（控制器追加）：settings.save_dir Some(非空) → 用户目录；
        // None → 默认 ~/Pictures/HawkSnap；空白串已被 sanitized 归一为 None → 默认。
        let custom =
            crate::settings::Settings { save_dir: Some("/tmp/hs-custom".into()), ..Default::default() };
        assert_eq!(
            resolve_save_dir(&custom),
            Some(std::path::PathBuf::from("/tmp/hs-custom")),
            "自定义目录优先于默认"
        );
        // Windows 无 HOME：与 dirs_picture_dir 同口径取 USERPROFILE（D2 bring-up）。
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .expect("测试环境必须有 HOME/USERPROFILE");
        let default_dir =
            Some(std::path::PathBuf::from(home).join("Pictures/HawkSnap"));
        assert_eq!(resolve_save_dir(&crate::settings::Settings::default()), default_dir);
        let blank = crate::settings::Settings { save_dir: Some("   ".into()), ..Default::default() }
            .sanitized();
        assert_eq!(blank.save_dir, None, "空白 save_dir 必须被 sanitize 归一为 None");
        assert_eq!(resolve_save_dir(&blank), default_dir);
    }

    #[test]
    fn release_captures_drops_rgba_keeps_frame_path() {
        // 终审 wave（G2）：release_captures 把每个条目的 rgba 换成空 Arc（内存释放），
        // frame_path 原样保留（下一轮 capture_screens 删除上一轮冻结帧仍需要）。
        // 直操 CAPTURES：测试模块同文件，私有静态可达；测试尾部清空，不留跨测污染。
        CAPTURES.lock().unwrap().insert(
            42,
            CaptureMeta {
                x: 0, y: 0, width: 100, height: 50, scale: 2.0,
                width_px: 200, height_px: 100,
                rgba: std::sync::Arc::new(vec![1, 2, 3]),
                frame_path: std::path::PathBuf::from("/tmp/hs-test.raw"),
            },
        );
        release_captures();
        {
            let captures = CAPTURES.lock().unwrap();
            let meta = captures.get(&42).unwrap();
            assert!(meta.rgba.is_empty(), "rgba 必须被释放为空 Arc");
            assert_eq!(meta.frame_path, std::path::PathBuf::from("/tmp/hs-test.raw"));
        }
        CAPTURES.lock().unwrap().remove(&42);
    }

    #[test]
    fn annotate_temp_png_roundtrips_with_dims_and_unique_names() {
        // M2 Task M3「标注」入口的落盘半程：$TMPDIR/hawksnap-annotate-{ms}-{seq}.png，
        // 字节可解回同 dims（编辑器按 asset 协议装载该文件），两次落盘文件名不撞。
        let p1 = write_annotate_png(&[9, 8, 7, 255, 6, 5, 4, 255], 2, 1).unwrap();
        assert!(p1.to_string_lossy().contains("hawksnap-annotate-"));
        assert_eq!(p1.extension().and_then(|e| e.to_str()), Some("png"));
        let (rgba, w, h) = hawksnap_core::crop::png_to_rgba(&std::fs::read(&p1).unwrap()).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgba, vec![9, 8, 7, 255, 6, 5, 4, 255]);
        let p2 = write_annotate_png(&[1, 1, 1, 255], 1, 1).unwrap();
        assert_ne!(p1, p2, "毫秒时间戳命名必须保证两次标注不互撞");
        std::fs::remove_file(p1).ok();
        std::fs::remove_file(p2).ok();
    }
}
