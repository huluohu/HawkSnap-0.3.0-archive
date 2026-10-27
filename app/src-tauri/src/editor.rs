//! 标注编辑器窗口（M2 Task M1）：label "editor" 的 webview 窗口，?window=editor 路由。
//!
//! 单编辑器模型：整个应用只有一个编辑器窗口。`editor_open(path, origin)` 建窗（首次）
//! 或复用并 show/focus（已在），随后 emit `hawk://editor-load { path, origin }`；
//! **编辑中再开新图 = 前端用新 path 替换当前图 + 清空撤销栈**（形状数组随旧图作废，
//! 语义见前端 index.tsx 的 loadFromPath）。
//!
//! ## 来源契约（M2 Task M3）
//!
//! `origin` 随 editor-load 下发、前端保存时原样回传 editor_save（`Option`，None =
//! 历史重新编辑等无副作用来源）。三个入口：
//! - 历史「重新编辑」：`origin=None`——保存只覆盖原文件（历史路径本身在 save_dir，
//!   无需重复入库）。
//! - 贴图双击：`origin=Pin { pin_id }`——编辑器装载 `$TMPDIR/hawksnap-pin-{id}-{ts}.png`
//!   （app 层 pins.rs 落盘）；保存覆盖临时文件后，把导出的 RGBA 回写贴图窗口
//!   （`pins::pin_set_image_rgba` → PinWindow::set_image 换图，窗口位置/缩放保持；
//!   贴图已被关闭的竞态静默忽略）。下一次双击装载的就是新图。
//! - 覆盖层「标注」（commands::annotate_crop）：`origin=Capture`——编辑器装载
//!   `$TMPDIR/hawksnap-annotate-{ms}-{seq}.png`；保存（裁定细化）= ①自动复制到剪贴板
//!   （spec「来源=截图 → 自动复制到剪贴板」）；②另存 PNG 副本到 save_dir 并入库
//!   （record_history，与普通截图同一产品模型；恒 PNG——编辑器导出即 PNG，重编码
//!   JPEG 破坏 F5 像素一致，output_format 设置不适用于此路径）。剪贴板失败向上报错
//!   （用户面核心语义），入库失败旁路降级（与 finish_capture 的历史旁路口径一致）。
//!
//! ## 并发契约（M1 ledgered race fix）
//!
//! 两次快速 editor_open 可能同见「窗口不存在」→ 都走建窗，后者撞 duplicate label
//! 报错。修复：建窗失败一律**再查一次窗口**——已存在（并发对手建好了）→ show+focus
//! 走与常规复用完全相同的单一代码路径（[`ensure_editor_window`]），不当失败抛出。
//!
//! 线程纪律（与 panel.rs 同款，**与贴图 pin 相反**）：wry webview 的建窗/关窗禁止
//! 在主线程做，`editor_open` 为 `#[tauri::command(async)]`——命令体在运行时线程执行，
//! 建窗合法；show/set_focus 是投递给主线程事件循环的消息，非主线程调用安全。贴图
//! 双击入口（pins.rs）据此把 editor_open 放在独立线程。

use base64::Engine;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

/// 编辑器窗口 label（与 capabilities/editor.json 的 `windows: ["editor"]` 对应）。
pub const EDITOR_LABEL: &str = "editor";

/// 编辑器来源（M2 Task M3，见模块注「来源契约」）。serde：`{"kind":"pin","pinId":7}`
/// / `{"kind":"capture"}`（tag=kind；字段 camelCase 对齐前端 invoke 载荷）。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EditorOrigin {
    /// 来源=贴图（双击）：保存后回写该贴图窗口的图像。
    #[serde(rename_all = "camelCase")]
    Pin { pin_id: u64 },
    /// 来源=截图（覆盖层「标注」）：保存后自动复制剪贴板 + 存 save_dir 入历史。
    #[serde(rename_all = "camelCase")]
    Capture,
}

/// hawk://editor-load 载荷（serde camelCase 对齐前端 `e.payload.path/origin`）。
#[derive(Clone, serde::Serialize)]
struct EditorLoadPayload {
    path: String,
    origin: Option<EditorOrigin>,
}

/// M2 Task M1：打开标注编辑器装载指定 PNG。流程：校验（存在 + 可解码）→ 建窗或
/// 复用（含并发竞态兜底，见模块注）→ emit editor-load。emit 三连发（150ms 间隔，
/// overlay-ready 同款冷加载竞态契约）：首建的 webview 冷加载期早于前端 listen 注册
/// 时首条 emit 必丢，前端以**收到的最后一条**为准；补发在独立线程，命令立即返回。
#[tauri::command(async)]
pub fn editor_open(app: AppHandle, path: String, origin: Option<EditorOrigin>) -> Result<(), String> {
    // 校验：文件存在 + PNG 可解码（ Hawksnap 编辑器只吃自家保存的 PNG；解码顺带
    // 验证非空/非损坏。几 MB 级解码在命令线程，不卡事件循环）。
    if path.trim().is_empty() {
        return Err("路径为空".into());
    }
    let png = std::fs::read(&path).map_err(|e| format!("读取截图文件失败: {e}"))?;
    hawksnap_core::crop::png_to_rgba(&png).map_err(|e| format!("截图文件解码失败: {e}"))?;

    // M2 Task M3：贴图/标注来源装载的是 $TMPDIR 临时文件——静态 asset scope 只有
    // `$CACHE/**`（tauri.conf），save_dir 由 M0 运行期放行，临时目录刻意不整段放行
    // （asset 面最小化）。装载前按文件精确 allow_file：命中 → 前端走 asset 协议快路；
    // 失败仅 eprintln——前端 onerror 自降级 editor_image_dataurl（base64 IPC 慢路，
    // 语义不变）。
    if let Err(e) = app.asset_protocol_scope().allow_file(std::path::Path::new(&path)) {
        eprintln!("[hawksnap] asset 协议放行编辑器源文件失败: {e}");
    }

    ensure_editor_window(&app)?;

    // emit 三连发：首条 + 独立线程 2 条补发 @150ms（覆盖层同款，无代数守卫——编辑器
    // 无流程代数概念，最后到的 path 即生效，重复 emit 同 path 幂等）。
    let payload = EditorLoadPayload { path, origin };
    let app2 = app.clone();
    let repeats = payload.clone();
    std::thread::spawn(move || {
        for _ in 0..2 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let _ = app2.emit("hawk://editor-load", repeats.clone());
        }
    });
    app.emit("hawk://editor-load", payload).map_err(|e| e.to_string())?;
    Ok(())
}

/// 建窗或复用（M1 ledgered race fix，单一代码路径）：已在 → show+focus；不在 → 建窗，
/// **建窗失败一律再查一次**——两次快速 open 同见 None 时，后者撞 duplicate label，
/// 此时窗口已被并发对手建好，show+focus 复用之（不当失败抛出）；再查仍无才是真失败。
fn ensure_editor_window(app: &AppHandle) -> Result<(), String> {
    if show_focus_existing(app) {
        return Ok(());
    }
    match WebviewWindowBuilder::new(
        app,
        EDITOR_LABEL,
        WebviewUrl::App("editor?window=editor".into()),
    )
    .title("HawkSnap 编辑器") // 窗口标题静态（原生 titlebar 不走 t()；G6 范围外，注记）
    .inner_size(960.0, 640.0)
    .min_inner_size(720.0, 480.0)
    .resizable(true)
    .visible(true)
    .build()
    {
        Ok(_) => Ok(()),
        Err(build_err) => {
            if show_focus_existing(app) {
                Ok(())
            } else {
                Err(format!("编辑器窗口创建失败: {build_err}"))
            }
        }
    }
}

/// 复用路径（建窗竞态兜底共用）：窗口存在 → show + focus，返回 true。
fn show_focus_existing(app: &AppHandle) -> bool {
    match app.get_webview_window(EDITOR_LABEL) {
        Some(w) => {
            let _ = w.show();
            let _ = w.set_focus();
            true
        }
        None => false,
    }
}

/// 降级装载（上下文裁定：仅当 asset 协议加载失败时前端调用）：读 PNG → base64
/// dataURL。8MP PNG → ~10MB 字符串过 IPC，故只作降级路径，不作常规装载。
#[tauri::command(async)]
pub fn editor_image_dataurl(path: String) -> Result<String, String> {
    if path.trim().is_empty() {
        return Err("路径为空".into());
    }
    let png = std::fs::read(&path).map_err(|e| format!("读取截图文件失败: {e}"))?;
    hawksnap_core::crop::png_to_rgba(&png).map_err(|e| format!("截图文件解码失败: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    Ok(format!("data:image/png;base64,{b64}"))
}

/// PNG 文件魔数（RFC 2083 8 字节签名）：editor_save 的载荷校验（前端 canvas.toBlob
/// 产物必带；防误写其他字节流）。
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// 保存编辑结果（M2 Task M2 + M3 origin 副作用）：前端 exportToPng（F5 单路径栅格化）
/// 产出的 PNG 字节以 base64 过 IPC → 校验（目标为已存在的 .png 文件 + PNG 魔数）→
/// 覆盖写回原路径 → 按 [`EditorOrigin`] 施加来源副作用（见模块注「来源契约」）。
///
/// 路径安全裁定（上下文预授权）：path 只来自编辑器装载过的历史/临时文件路径
/// （hawk://editor-load 原样传回），属第一方受控输入——不做目录白名单校验，仅校验
/// 「文件已存在 + 扩展名 .png（大小写不敏感）」，杜绝借编辑器写任意路径/任意格式。
#[tauri::command(async)]
pub fn editor_save(
    app: AppHandle,
    path: String,
    png_base64: String,
    origin: Option<EditorOrigin>,
) -> Result<(), String> {
    let (rgba, w, h) = save_png_bytes(&path, &png_base64)?;
    match origin {
        None => {} // 历史重新编辑：只覆盖原文件（路径已在 save_dir，无重复入库）
        Some(EditorOrigin::Pin { pin_id }) => {
            // 贴图回写（主线程交付在 pins.rs）：贴图已被关闭 → pin_set_image_rgba
            // 静默 Ok；platform/交付失败 eprintln 降级（保存本身已成功，编辑器不因
            // 回写失败报错——临时文件路径对用户不可见，报错无从处置）。
            if let Err(e) = crate::pins::pin_set_image_rgba(&app, pin_id, rgba, w, h) {
                eprintln!("[hawksnap] 贴图回写失败（忽略）: {e}");
            }
        }
        Some(EditorOrigin::Capture) => {
            // 裁定：来源=截图 → 自动复制到剪贴板（用户面核心语义，失败向上报错）。
            crate::commands::copy_rgba_to_clipboard(&rgba, w, h)?;
            // 另存 save_dir + 入库（与普通截图同一产品模型）。历史是旁路：失败
            // eprintln 降级，不回滚剪贴板、不影响保存成败（与 finish_capture 同口径）。
            if let Err(e) = save_annotate_to_history(&app, &rgba, w, h) {
                eprintln!("[hawksnap] 标注截图入库失败（忽略）: {e}");
            }
        }
    }
    Ok(())
}

/// editor_save 的纯落盘半程（AppHandle-free，可测）：校验路径/扩展名/魔数 → 原子
/// 覆盖写（同目录 tmp + rename，见函数体注）→ 解码写盘字节返回 `(rgba, w, h)`
/// （origin 副作用直接复用，免二次读盘）。
fn save_png_bytes(path: &str, png_base64: &str) -> Result<(Vec<u8>, u32, u32), String> {
    if path.trim().is_empty() {
        return Err("路径为空".into());
    }
    let p = std::path::Path::new(path);
    let is_png_ext = p
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("png"));
    if !is_png_ext {
        return Err("编辑器仅支持保存 PNG 文件".into());
    }
    if !p.is_file() {
        return Err(format!("目标文件不存在: {path}"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png_base64.trim())
        .map_err(|e| format!("PNG 数据解码失败: {e}"))?;
    if bytes.len() < PNG_MAGIC.len() || bytes[..PNG_MAGIC.len()] != PNG_MAGIC {
        return Err("不是有效的 PNG 数据".into());
    }
    // 原子落盘（终审修复，数据丢失类）：先写同目录 `<path>.tmp` 再 rename 覆盖——
    // 同目录 rename 是 POSIX 原子操作，覆盖瞬间无第三态；直接 fs::write 覆盖会在
    // 进程中途被杀/磁盘满时留下截断的半张 PNG（原图已损，且历史/贴图双消费方都
    // 读同一路径）。写失败/rename 失败时清理 tmp 残留后上抛（原文件保持完好）。
    let tmp: std::path::PathBuf = {
        let mut s = p.as_os_str().to_os_string();
        s.push(".tmp");
        s.into()
    };
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("写入截图文件失败: {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp, p) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("写入截图文件失败: {e}"));
    }
    hawksnap_core::crop::png_to_rgba(&bytes).map_err(|e| format!("截图文件解码失败: {e}"))
}

/// origin=Capture 的入库半程（M2 Task M3 裁定细化）：编辑后的图另存 PNG 副本到
/// save_dir（save_path_for 命名，app 字段暂记 "HawkSnap"——与 finish_capture 同口径）
/// 并 record_history 入库。恒 PNG（F5 一致性，见模块注）；入库含缩略图/prune 的完整
/// 链复用 commands::record_history（旁路降级语义一致）。
fn save_annotate_to_history(
    app: &AppHandle,
    rgba: &[u8],
    w: u32,
    h: u32,
) -> Result<(), String> {
    let settings = crate::settings::Settings::load(app);
    let dir = crate::commands::resolve_save_dir(&settings).ok_or("no picture dir")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = crate::commands::save_path_for(&dir, "HawkSnap", "png");
    let png = hawksnap_core::crop::rgba_to_png(rgba, w, h)
        .map_err(|e| format!("标注截图编码失败: {e}"))?;
    std::fs::write(&path, &png).map_err(|e| format!("写入截图文件失败: {e}"))?;
    crate::commands::record_history(
        app,
        &path,
        &dir,
        rgba,
        &hawksnap_core::geometry::Rect { x: 0, y: 0, w, h },
    )
}

#[cfg(test)]
mod editor_tests {
    use super::*;

    /// 每测试独立临时目录（进程 id + 用例 tag 隔离，免共享状态互踩）。
    fn temp_png(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hawksnap-editor-save-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("shot.png")
    }

    #[test]
    fn writes_valid_png_bytes_over_the_original() {
        let path = temp_png("roundtrip");
        // 起始文件必须存在（编辑器只覆盖自家装载过的路径）
        std::fs::write(&path, b"stale-bytes").unwrap();
        let png = hawksnap_core::crop::rgba_to_png(&[10, 20, 30, 255, 40, 50, 60, 255], 2, 1).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let (rgba, w, h) = save_png_bytes(path.to_string_lossy().as_ref(), &b64).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), png); // 原样落盘（不再转码——F5 像素一致）
        assert_eq!((w, h), (2, 1), "返回 dims 供 origin 副作用复用");
        assert_eq!(rgba, vec![10, 20, 30, 255, 40, 50, 60, 255]);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// 原子落盘（终审修复）：成功保存后同目录无 `.tmp` 残留；校验失败（魔数不过）
    /// 时原文件保持原样且同样无残留——半成品只会存在于写盘中途，rename 后要么
    /// 完整新图、要么完整旧图。
    #[test]
    fn atomic_write_leaves_no_tmp_residue() {
        let path = temp_png("atomic");
        std::fs::write(&path, b"stale-bytes").unwrap();
        let dir = path.parent().unwrap();
        let tmp = {
            let mut s = path.as_os_str().to_os_string();
            s.push(".tmp");
            std::path::PathBuf::from(s)
        };
        // 预置一个陈旧 tmp（模拟上次异常退出残留）：本次成功保存应把它消费掉（rename 覆盖）。
        std::fs::write(&tmp, b"leftover").unwrap();
        let png = hawksnap_core::crop::rgba_to_png(&[0, 0, 0, 255], 1, 1).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        save_png_bytes(path.to_string_lossy().as_ref(), &b64).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), png);
        assert!(!tmp.exists(), "成功保存后不得残留 .tmp");
        // 失败路径（非 PNG 魔数）：原文件原样、无新增 tmp。
        let not_png = base64::engine::general_purpose::STANDARD.encode(b"JPEG data, honestly");
        assert!(save_png_bytes(path.to_string_lossy().as_ref(), &not_png).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), png);
        let leftovers: Vec<_> = std::fs::read_dir(dir).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "失败路径不得残留 .tmp: {leftovers:?}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rejects_bad_base64() {
        let path = temp_png("badb64");
        std::fs::write(&path, b"stale").unwrap();
        let err = save_png_bytes(path.to_string_lossy().as_ref(), "!!!not-base64!!!").unwrap_err();
        assert!(err.contains("PNG 数据解码失败"), "got: {err}");
        // 失败不落盘（原文件保持原样）
        assert_eq!(std::fs::read(&path).unwrap(), b"stale");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn rejects_payload_without_png_magic() {
        let path = temp_png("magic");
        std::fs::write(&path, b"stale").unwrap();
        let not_png = base64::engine::general_purpose::STANDARD.encode(b"JPEG data, honestly");
        let err = save_png_bytes(path.to_string_lossy().as_ref(), &not_png).unwrap_err();
        assert!(err.contains("不是有效的 PNG 数据"), "got: {err}");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn rejects_missing_target_and_non_png_extension() {
        let dir = std::env::temp_dir().join(format!("hawksnap-editor-save-{}-missing", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("gone.png");
        let err = save_png_bytes(missing.to_string_lossy().as_ref(), "").unwrap_err();
        assert!(err.contains("目标文件不存在"), "got: {err}");
        // 已存在但扩展名非 .png：拒绝（防借编辑器写任意格式路径）
        let txt = dir.join("shot.txt");
        std::fs::write(&txt, b"x").unwrap();
        let png = hawksnap_core::crop::rgba_to_png(&[0, 0, 0, 255], 1, 1).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let err = save_png_bytes(txt.to_string_lossy().as_ref(), &b64).unwrap_err();
        assert!(err.contains("仅支持保存 PNG"), "got: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_empty_path() {
        assert_eq!(save_png_bytes("  ", "").unwrap_err(), "路径为空");
    }

    /// M2 Task M3：origin 契约的 serde 形状（前端 invoke 载荷 ↔ Rust 枚举）。
    #[test]
    fn editor_origin_serializes_tagged_camel_case() {
        // Pin：tag=kind + 字段 camelCase（pinId）
        let s = serde_json::to_string(&EditorOrigin::Pin { pin_id: 7 }).unwrap();
        assert_eq!(s, r#"{"kind":"pin","pinId":7}"#, "前端 payload 形状契约");
        let back: EditorOrigin = serde_json::from_str(&s).unwrap();
        assert_eq!(back, EditorOrigin::Pin { pin_id: 7 });
        // Capture：仅 tag
        let s = serde_json::to_string(&EditorOrigin::Capture).unwrap();
        assert_eq!(s, r#"{"kind":"capture"}"#);
        let back: EditorOrigin = serde_json::from_str(&s).unwrap();
        assert_eq!(back, EditorOrigin::Capture);
        // Option：null（历史重新编辑路径传 origin: null）→ None
        let none: Option<EditorOrigin> = serde_json::from_str("null").unwrap();
        assert_eq!(none, None);
    }
}
