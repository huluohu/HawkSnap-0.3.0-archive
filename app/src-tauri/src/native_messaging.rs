// M4 P1 —— Native Messaging 桥（桌面端）。
//
// 架构（**浏览器拉起宿主**的标准模型，见计划 Global Constraints / Ruling 2）：
//
//   Chrome ──stdio(JSON)──▶ hawksnap-host（独立小进程，Chrome 按 manifest 拉起）
//                              │  TCP 127.0.0.1:<port>（令牌鉴权）
//                              ▼
//                          HawkSnap.app（本模块：监听 + 入库）
//
// 为什么不能是「Tauri app 自己当宿主」：Chrome 拉起的是**独立进程**，它要的是
// 一对新鲜的 stdin/stdout 管道；已在运行的 app 的 stdin 不是管道（拉起 app 会
// 变成第二个实例，被 single_instance 守卫挡掉）。故宿主必须是独立二进制，由
// 它把帧转发给运行中的 app——运行中的 app 在 127.0.0.1 上开一个回环监听，
// 用「每次启动重新生成的一次性令牌 + 0600 配置文件」鉴权。
//
// 安全边界：
//   • 监听**只绑 127.0.0.1**（无外网出口，插件与桌面端之间不过网）；
//   • 每帧必须带令牌（宿主从 0600 配置文件读，插件无从得知）；
//   • 路径三重约束：系统临时目录内 + `.png` + `hawksnap-plugin-*.png` 前缀
//     （见 native_protocol::validate_capture_path）；
//   • 帧长上限 8 MiB、连接读超时——防资源耗尽。
//
// 协议契约见 `docs/superpowers/protocols/native-messaging.md`（P2 照抄）。

use crate::native_protocol::{
    config_path, random_token, tokens_equal, validate_capture_path, write_host_config, Frame,
    HostConfig, Reply, HOST_NAME, MAX_FRAME_BYTES, PORT_SCAN_TRIES, PREFERRED_PORT,
};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// 监听是否在跑（`native_messaging_status` 命令的返回源）。
static LISTENING: AtomicBool = AtomicBool::new(false);

/// 单连接读超时——防止半开连接占住线程。
const CONN_TIMEOUT: Duration = Duration::from_secs(15);

/// 浏览器截图入库后的推事件（前端横幅用）。
pub const BROWSER_CAPTURE_EVENT: &str = "hawk://browser-capture";

/// 浏览器截图入库事件载荷。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserCapturePayload {
    /// 落盘后的绝对路径。
    pub path: String,
    /// 图片宽（像素，以 PNG 实际像素为准）。
    pub width: u32,
    /// 图片高（像素）。
    pub height: u32,
}

/// 入库结果（落盘 + 历史）。
#[derive(Debug, Clone)]
pub struct IngestOutcome {
    pub saved_path: PathBuf,
    pub thumb_path: PathBuf,
    pub width: u32,
    pub height: u32,
}

/// 桌面端监听是否已启动。
pub fn is_listening() -> bool {
    LISTENING.load(Ordering::SeqCst)
}

/// 命令：Native Messaging 桥是否就绪（前端「浏览器插件」分区展示）。
#[tauri::command]
pub fn native_messaging_status() -> bool {
    is_listening()
}

/// 启动 Native Messaging 回环监听（应用启动时调用；失败**不**阻塞启动）。
///
/// 流程：绑定 127.0.0.1:<首选端口>（被占则向上回退扫描）→ 生成一次性令牌 →
/// 写宿主配置文件（0600，含端口与令牌）→ 起后台线程 accept 循环。
pub fn start_listener(app: AppHandle) -> Result<(), String> {
    let cfg_path = config_path()?;
    let handler_app = app.clone();
    start_listener_with(cfg_path, PREFERRED_PORT, move |frame: &Frame| {
        let app = handler_app.clone();
        let token = frame.token.clone().unwrap_or_default();
        match native_ingest(
            app,
            frame.path.clone(),
            frame.width,
            frame.height,
            Some(token),
        ) {
            Ok(saved) => Reply::ok(&frame.id, saved),
            Err(e) => Reply::err(&frame.id, e),
        }
    })
}

/// 监听骨架（配置路径与端口可注入 → 单测可跑在临时目录/临时端口上）。
fn start_listener_with<F>(cfg_path: PathBuf, preferred_port: u16, handler: F) -> Result<(), String>
where
    F: Fn(&Frame) -> Reply + Send + Sync + 'static,
{
    let (listener, port) = bind_listener(preferred_port)?;
    let token = random_token();
    let cfg = HostConfig {
        port,
        token: token.clone(),
        pid: std::process::id(),
        host_name: HOST_NAME.to_string(),
    };
    write_host_config(&cfg_path, &cfg)?;
    let handler = Arc::new(handler);
    std::thread::Builder::new()
        .name("hawksnap-native-host".to_string())
        .spawn(move || accept_loop(listener, handler, token))
        .map_err(|e| format!("启动监听线程失败: {e}"))?;
    // 线程起来了才算「监听已就绪」（命令 native_messaging_status 的返回源）。
    LISTENING.store(true, Ordering::SeqCst);
    Ok(())
}

/// 绑定监听：首选端口 → 向上回退扫描（端口冲突不致命，换一个即可）。
///
/// 端口 0 = 让 OS 分配临时端口（单测用）——此时以 `local_addr()` 取实际端口。
fn bind_listener(preferred: u16) -> Result<(TcpListener, u16), String> {
    let mut last = String::new();
    for offset in 0..PORT_SCAN_TRIES {
        let port = preferred.saturating_add(offset);
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => {
                let actual = l.local_addr().map(|a| a.port()).unwrap_or(port);
                return Ok((l, actual));
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!(
        "无法监听 127.0.0.1:{}~{}: {last}",
        preferred,
        preferred.saturating_add(PORT_SCAN_TRIES - 1)
    ))
}

/// accept 循环：每连接一线程（连接是「一问一答」的短请求，独立线程最简）。
fn accept_loop<F>(listener: TcpListener, handler: Arc<F>, token: String)
where
    F: Fn(&Frame) -> Reply + Send + Sync + 'static,
{
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let handler = Arc::clone(&handler);
                let token = token.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_conn(s, handler, &token) {
                        eprintln!("[hawksnap] Native Messaging 连接处理失败: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[hawksnap] Native Messaging accept 失败: {e}"),
        }
    }
}

/// 单连接：读一帧 → 校验令牌 → 交给 handler → 回写一行 JSON。
fn handle_conn<F>(stream: TcpStream, handler: Arc<F>, token: &str) -> Result<(), String>
where
    F: Fn(&Frame) -> Reply + Send + Sync + 'static,
{
    stream
        .set_read_timeout(Some(CONN_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(CONN_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let line = read_frame_line(&mut reader)?;

    let reply = match crate::native_protocol::parse_frame(&line) {
        Err(e) => Reply::err("", e),
        Ok(frame) => match frame.token.as_deref() {
            // 令牌不对 = 不是本机已注册宿主 → 拒（不回显任何路径信息）。
            Some(t) if tokens_equal(t, token) => handler(&frame),
            _ => Reply::err(&frame.id, "未授权的请求（令牌无效）"),
        },
    };

    let out = serde_json::to_string(&reply).map_err(|e| e.to_string())?;
    writer.write_all(out.as_bytes()).map_err(|e| e.to_string())?;
    writer.write_all(b"\n").map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// 读一行（上限 [`MAX_FRAME_BYTES`]）——超限即断连（流已失同步）。
///
/// 手写 `fill_buf`/`consume` 而非 `read_until`：后者在无换行的恶意流上会无限
/// 增长（内存耗尽）；这里逐段读取并显式封顶。
fn read_frame_line<R: BufRead>(reader: &mut R) -> Result<String, String> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        // ① 窥视：找换行位置 + 本段可用长度（借用在块内结束 → 之后可 consume）
        let (nl_pos, avail_len) = {
            let available = reader.fill_buf().map_err(|e| e.to_string())?;
            if available.is_empty() {
                (None, 0)
            } else {
                (
                    available.iter().position(|&b| b == b'\n'),
                    available.len(),
                )
            }
        };
        if avail_len == 0 {
            // EOF：无残留 → 连接关闭；有残留 → 当作最后一帧（无换行结尾）
            if buf.is_empty() {
                return Err("连接已关闭".to_string());
            }
            break;
        }
        let take_len = nl_pos.map(|p| p + 1).unwrap_or(avail_len);
        // ② 拷贝本段（借用结束后再 consume）
        let seg: Vec<u8> = {
            let available = reader.fill_buf().map_err(|e| e.to_string())?;
            available[..take_len].to_vec()
        };
        reader.consume(take_len);
        buf.extend_from_slice(&seg);
        if nl_pos.is_some() {
            break;
        }
        if buf.len() > MAX_FRAME_BYTES {
            return Err("帧过长".to_string());
        }
    }
    String::from_utf8(buf).map_err(|_| "帧不是合法 UTF-8".to_string())
}

/// 内部命令：把插件写到临时目录的 PNG 入库（落 save_dir + 历史 + 推事件）。
///
/// 令牌必带（`token`）——这是「只信本机已注册宿主」的落点；令牌与宿主配置文件
/// （0600）里的一致才继续。
///
/// 返回值是**落盘后的绝对路径**（协议回执的 `savedPath` 需要它）。
#[tauri::command]
pub fn native_ingest(
    app: AppHandle,
    path: String,
    width: u32,
    height: u32,
    token: Option<String>,
) -> Result<String, String> {
    let token = token.unwrap_or_default();
    let cfg = crate::native_protocol::read_host_config()?;
    if !tokens_equal(&cfg.token, &token) {
        return Err("未授权的请求（令牌无效）".to_string());
    }
    let src = validate_capture_path(&path)?;

    let settings = crate::settings::Settings::load(&app);
    let save_dir = crate::commands::resolve_save_dir(&settings)
        .ok_or_else(|| "未配置保存目录".to_string())?;
    std::fs::create_dir_all(&save_dir).map_err(|e| e.to_string())?;

    let db = crate::history::db_path(&app);
    let outcome = ingest_capture_at(&src, &save_dir, Some(&db))?;

    // 帧里的 width/height 只作参考：**以 PNG 实际像素为准**（插件可能谎报或
    // 省略）；不一致时留一条线索（不静默）。
    if width > 0 && height > 0 && (width != outcome.width || height != outcome.height) {
        eprintln!(
            "[hawksnap] 浏览器截图尺寸与帧声明不一致（帧 {width}x{height}，实际 {}x{}），以实际为准",
            outcome.width, outcome.height
        );
    }

    let payload = BrowserCapturePayload {
        path: outcome.saved_path.to_string_lossy().into_owned(),
        width: outcome.width,
        height: outcome.height,
    };
    let _ = app.emit(BROWSER_CAPTURE_EVENT, payload);
    Ok(outcome.saved_path.to_string_lossy().into_owned())
}

/// 落盘 + 缩略图 + 入库（**无 AppHandle 依赖** → 可单测）。
///
/// - 原文件按字节复制到 save_dir（命名模板 `{app} {date} {time}`，app 段固定
///   "browser"）；不重编码（插件给的是 PNG，字节级保真）；
/// - 尺寸以 **PNG 实际像素**为准（帧里的 width/height 只作参考，插件可能
///   谎报或省略）；
/// - 缩略图 320 长边（与既有截图入库口径一致）→ 落 `save_dir/thumbs/`；
/// - 历史入库 kind="browser"、app="browser"（前端按 kind 门控动作）。
pub(crate) fn ingest_capture_at(
    src: &Path,
    save_dir: &Path,
    db: Option<&Path>,
) -> Result<IngestOutcome, String> {
    let bytes = std::fs::read(src).map_err(|e| format!("读取临时文件失败: {e}"))?;
    let (rgba, w, h) = hawksnap_core::crop::png_to_rgba(&bytes).map_err(|e| e.to_string())?;
    if w == 0 || h == 0 {
        return Err("图片尺寸非法".to_string());
    }

    let saved_path = crate::commands::save_path_for(save_dir, "browser", "png");
    // 保存目录兜底创建（生产路径在调用前已建；此处再建一次 → 幂等，且让
    // 「落盘 + 入库」这个核心函数自带目录保证，测试可直接调用）。
    std::fs::create_dir_all(save_dir).map_err(|e| e.to_string())?;
    std::fs::write(&saved_path, &bytes).map_err(|e| e.to_string())?;

    let thumbs_dir = save_dir.join("thumbs");
    std::fs::create_dir_all(&thumbs_dir).map_err(|e| e.to_string())?;
    let stem = saved_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shot".to_string());
    let thumb_path = thumbs_dir.join(format!("{stem}.png"));
    let (thumb_rgba, tw, th) =
        hawksnap_core::crop::rgba_thumbnail(&rgba, w, h, 320).map_err(|e| e.to_string())?;
    let thumb_png = hawksnap_core::crop::rgba_to_png(&thumb_rgba, tw, th).map_err(|e| e.to_string())?;
    std::fs::write(&thumb_path, &thumb_png).map_err(|e| e.to_string())?;

    if let Some(db) = db {
        crate::history::record_at(
            db,
            &saved_path.to_string_lossy(),
            &thumb_path.to_string_lossy(),
            w as i64,
            h as i64,
            "browser",
            "browser",
            None,
        )?;
    }

    Ok(IngestOutcome {
        saved_path,
        thumb_path,
        width: w,
        height: h,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_protocol::{parse_frame, Frame};

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hawksnap-native-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 造一张真 PNG（1×1 像素）。
    fn tiny_png() -> Vec<u8> {
        hawksnap_core::crop::rgba_to_png(&[255, 0, 0, 255], 1, 1).unwrap()
    }

    #[test]
    fn path_validation_rejects_outside_temp_dir() {
        let e = validate_capture_path("/etc/hawksnap-plugin-x.png").unwrap_err();
        assert!(e.contains("临时文件不存在") || e.contains("临时目录"), "实际: {e}");
    }

    #[test]
    fn path_validation_rejects_non_png() {
        let d = temp_dir("ext");
        let p = d.join("hawksnap-plugin-x.txt");
        std::fs::write(&p, b"x").unwrap();
        let e = validate_capture_path(&p.to_string_lossy()).unwrap_err();
        assert!(e.contains(".png"), "实际: {e}");
    }

    #[test]
    fn path_validation_rejects_foreign_prefix() {
        let d = temp_dir("prefix");
        let p = d.join("other-file.png");
        std::fs::write(&p, tiny_png()).unwrap();
        let e = validate_capture_path(&p.to_string_lossy()).unwrap_err();
        assert!(e.contains("hawksnap-plugin-"), "实际: {e}");
    }

    #[test]
    fn path_validation_accepts_plugin_file_in_temp_dir() {
        let d = temp_dir("ok");
        let p = d.join("hawksnap-plugin-abc.png");
        std::fs::write(&p, tiny_png()).unwrap();
        let canonical = validate_capture_path(&p.to_string_lossy()).unwrap();
        assert!(canonical.exists());
        assert!(canonical.ends_with("hawksnap-plugin-abc.png"));
    }

    #[test]
    fn path_validation_rejects_relative_path() {
        let e = validate_capture_path("hawksnap-plugin-x.png").unwrap_err();
        assert!(e.contains("绝对路径"), "实际: {e}");
    }

    #[test]
    fn token_file_is_0600_and_roundtrips() {
        let d = temp_dir("token");
        let cfg_path = d.join("native-host.json");
        let cfg = HostConfig {
            port: 38471,
            token: crate::native_protocol::random_token(),
            pid: 1,
            host_name: HOST_NAME.to_string(),
        };
        write_host_config(&cfg_path, &cfg).unwrap();
        let text = std::fs::read_to_string(&cfg_path).unwrap();
        let parsed: HostConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.port, cfg.port);
        assert_eq!(parsed.token, cfg.token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "宿主配置文件必须是 0600，实际: {mode:o}");
        }
    }

    #[test]
    fn loopback_capture_lands_in_save_dir_and_history() {
        // 端到端回环：起监听（临时端口 + 临时配置）→ 连 127.0.0.1 → 发合法帧
        // → 断言落盘 + 历史入库。整个链路只有 TCP + 文件 IO → 跨平台可跑。
        let root = temp_dir("loopback");
        let cfg_path = root.join("native-host.json");
        let save_dir = root.join("save");
        let db = root.join("history.db");

        let save_dir_c = save_dir.clone();
        let db_c = db.clone();
        start_listener_with(cfg_path.clone(), 0, move |frame: &Frame| {
            // 与生产路径同款：令牌校验已在 accept 循环完成，这里直接走落盘入库。
            match ingest_capture_at(
                Path::new(&frame.path),
                &save_dir_c,
                Some(&db_c),
            ) {
                Ok(o) => Reply::ok(&frame.id, o.saved_path.to_string_lossy().into_owned()),
                Err(e) => Reply::err(&frame.id, e),
            }
        })
        .expect("监听应启动成功");

        let cfg: HostConfig =
            serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        assert!(cfg.port > 0);

        // 插件写的临时 PNG（必须在系统临时目录内 → 校验才放行）。
        let plugin_dir = std::env::temp_dir().join(format!(
            "hawksnap-native-test-{}-plugin",
            std::process::id()
        ));
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let plugin_png = plugin_dir.join("hawksnap-plugin-loopback.png");
        std::fs::write(&plugin_png, tiny_png()).unwrap();

        // —— 缺令牌 → 拒 ——
        let mut s = TcpStream::connect(("127.0.0.1", cfg.port)).unwrap();
        s.write_all(
            br#"{"cmd":"capture","id":"no-token","path":"PLACEHOLDER","width":1,"height":1}
"#,
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(&mut s).read_line(&mut line).unwrap();
        let reply: Reply = serde_json::from_str(line.trim()).unwrap();
        assert!(!reply.ok, "缺令牌必须被拒");
        assert!(reply.error.unwrap_or_default().contains("令牌"), "实际: {line}");

        // —— 令牌错误 → 拒 ——
        let mut s = TcpStream::connect(("127.0.0.1", cfg.port)).unwrap();
        let bad = format!(
            r#"{{"cmd":"capture","id":"bad-token","path":"{}","width":1,"height":1,"token":"wrong"}}
"#,
            plugin_png.to_string_lossy()
        );
        s.write_all(bad.as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(&mut s).read_line(&mut line).unwrap();
        let reply: Reply = serde_json::from_str(line.trim()).unwrap();
        assert!(!reply.ok, "错误令牌必须被拒");

        // —— 合法帧 → 落盘 + 入库 ——
        let mut s = TcpStream::connect(("127.0.0.1", cfg.port)).unwrap();
        let good = format!(
            r#"{{"cmd":"capture","id":"ok-1","path":"{}","width":1,"height":1,"token":"{}"}}
"#,
            plugin_png.to_string_lossy(),
            cfg.token
        );
        s.write_all(good.as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(&mut s).read_line(&mut line).unwrap();
        let reply: Reply = serde_json::from_str(line.trim()).unwrap();
        assert!(reply.ok, "合法帧必须成功，实际: {line}");
        let saved = reply.saved_path.clone().unwrap();
        assert!(Path::new(&saved).exists(), "落盘文件必须存在: {saved}");
        let save_dir_s = save_dir.to_string_lossy().into_owned();
        assert!(saved.starts_with(&save_dir_s), "必须落在 save_dir: {saved}");
        assert!(saved.ends_with(".png"));

        // 历史入库一行 kind="browser"、app="browser"。
        let rows = crate::history::list_at(&db, 10, 0).unwrap();
        assert_eq!(rows.len(), 1, "应有且仅有一行历史，实际: {rows:?}");
        assert_eq!(rows[0].kind, "browser");
        assert_eq!(rows[0].app, "browser");
        assert_eq!(rows[0].path, saved);

        // 缩略图也落了盘。
        let thumbs = save_dir.join("thumbs");
        assert!(thumbs.exists(), "缩略图目录应存在");
        assert!(std::fs::read_dir(&thumbs).unwrap().count() >= 1);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&plugin_dir);
    }

    #[test]
    fn parse_frame_rejects_malformed_over_the_wire() {
        // 纯函数层的畸形帧拒绝（不经 socket）——协议契约的第二道断言。
        assert!(parse_frame("{oops").is_err());
    }
}
