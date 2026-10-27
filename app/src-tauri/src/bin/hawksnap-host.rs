// M4 P1 —— Native Messaging 宿主（hawksnap-host）。
//
// Chrome 读 manifest → **拉起本进程** → 与浏览器之间是一对新鲜的管道
// （stdin/stdout）→ 本进程把 JSON 帧转发给**运行中的** HawkSnap.app
// （127.0.0.1 回环 + 一次性令牌鉴权）。
//
// 为什么必须是独立进程（而不是让 Tauri app 自己当宿主）：Chrome 拉起宿主时
// 期待的是「一个刚起来的进程 + 一对新鲜管道」；已在运行的 app 的 stdin 不是
// 管道，而拉起 app 会变成第二实例（被 single_instance 守卫挡掉）。详见
// `src/native_messaging.rs` 文件头注。
//
// 与桌面端共享同一份协议实现：下面 `#[path]` 直接编译桌面端那份源码
// （`src/native_protocol.rs`）——**不**链 Tauri（宿主进程要轻），也**不**复制
// 协议逻辑（避免两侧漂移）。
//
// 构建（feature 门控，不进默认构建——第二个默认 bin 会让 `tauri build` 找错
// 主程序，M3 N4 教训）：
//   cargo build --release --features native-host --bin hawksnap-host
//
// 环境变量：
//   HAWKSNAP_NATIVE_FRAMING=lines|chrome  强制 stdin 帧格式（默认自动识别）
//   HAWKSNAP_NATIVE_HOST_CONFIG=<路径>    覆盖宿主配置文件路径（默认按平台约定）

// 宿主只用协议层的一部分（桌面端写配置等在此无人调用）→ 就地放行，
// 不做 crate 级 blanket allow（会吞掉未来的真实告警）。
#[allow(dead_code)]
#[path = "../native_protocol.rs"]
mod native_protocol;

use base64::Engine;
use native_protocol::{Frame, HostConfig, Reply};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

/// 与桌面端通信的超时（截图入库要解码 PNG + 写盘 + 入库，给足时间）。
const IO_TIMEOUT: Duration = Duration::from_secs(30);

fn main() {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[hawksnap-host] {e}");
            std::process::exit(1);
        }
    };

    let stdin = std::io::stdin();
    let mut reader = FrameReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut framing: Option<Framing> = match std::env::var("HAWKSNAP_NATIVE_FRAMING").as_deref() {
        Ok("lines") => Some(Framing::Lines),
        Ok("chrome") => Some(Framing::Chrome),
        _ => None,
    };

    loop {
        match reader.next_payload(&mut framing) {
            // EOF：浏览器关闭管道 → 宿主退出。
            Ok(None) => break,
            Ok(Some(payload)) => {
                let reply = handle(&cfg, &payload);
                let out = serde_json::to_string(&reply).unwrap_or_else(|e| {
                    format!(r#"{{"id":"","ok":false,"error":"回执序列化失败: {e}"}}"#)
                });
                let mode = framing.unwrap_or(Framing::Lines);
                if let Err(e) = write_payload(&mut writer, mode, &out) {
                    eprintln!("[hawksnap-host] 写回执失败: {e}");
                    break;
                }
            }
            Err(e) => {
                eprintln!("[hawksnap-host] 读取帧失败: {e}");
                break;
            }
        }
    }
}

/// 读宿主配置（端口 + 令牌）——桌面端启动时写的 0600 文件。
fn load_config() -> Result<HostConfig, String> {
    if let Ok(path) = std::env::var("HAWKSNAP_NATIVE_HOST_CONFIG") {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("读取宿主配置失败 ({path}): {e}"))?;
        return serde_json::from_str(&text).map_err(|e| format!("宿主配置解析失败: {e}"));
    }
    native_protocol::read_host_config()
}

/// 一帧一答复。
fn handle(cfg: &HostConfig, payload: &str) -> Reply {
    let frame = match native_protocol::parse_frame(payload) {
        Ok(f) => f,
        Err(e) => return Reply::err("", e),
    };
    match frame.cmd.as_str() {
        "save" => save_payload(&frame),
        "capture" => capture(cfg, &frame),
        other => Reply::err(&frame.id, format!("不支持的命令: {other}")),
    }
}

/// `capture`：校验路径 → 经回环 socket 转发给桌面端 → 原样返回桌面端回执。
fn capture(cfg: &HostConfig, frame: &Frame) -> Reply {
    // 宿主侧也校验一遍（纵深防御：桌面端还会再校验一次）。
    if let Err(e) = native_protocol::validate_capture_path(&frame.path) {
        return Reply::err(&frame.id, e);
    }
    match relay(cfg, frame) {
        Ok(text) => serde_json::from_str::<Reply>(&text)
            .unwrap_or_else(|e| Reply::err(&frame.id, format!("回执解析失败: {e}"))),
        Err(e) => Reply::err(&frame.id, e),
    }
}

/// 转发一帧给桌面端并读回执。
fn relay(cfg: &HostConfig, frame: &Frame) -> Result<String, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", cfg.port)).map_err(|e| {
        format!(
            "无法连接 HawkSnap 桌面端 (127.0.0.1:{})：{e}（请先启动 HawkSnap.app）",
            cfg.port
        )
    })?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
    let req = Frame {
        cmd: "capture".to_string(),
        id: frame.id.clone(),
        path: frame.path.clone(),
        width: frame.width,
        height: frame.height,
        token: Some(cfg.token.clone()),
        data: None,
    };
    let line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(b"\n").map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).map_err(|e| e.to_string())?;
    if resp.trim().is_empty() {
        return Err("桌面端未返回回执".to_string());
    }
    Ok(resp.trim().to_string())
}

/// `save`：把插件传来的 base64 PNG 落进**宿主私有收件箱**（系统临时目录下的
/// 0700 子目录），文件名固定前缀 —— 桌面端的路径校验（临时目录 + `.png` +
/// `hawksnap-plugin-*.png`）由此满足。
///
/// 为什么需要它：浏览器扩展**无法**直接把文件写进系统临时目录（扩展沙箱只
/// 能经 downloads API 写下载目录）。宿主是原生进程，能写临时目录——于是插件
/// 把图片交给宿主落盘，再拿回路径发 `capture` 帧。协议详见
/// `docs/superpowers/protocols/native-messaging.md`。
fn save_payload(frame: &Frame) -> Reply {
    let data = match frame.data.as_deref() {
        Some(d) if !d.is_empty() => d,
        _ => return Reply::err(&frame.id, "save 帧缺少 data"),
    };
    let bytes = match base64::engine::general_purpose::STANDARD.decode(data.as_bytes()) {
        Ok(b) => b,
        Err(e) => return Reply::err(&frame.id, format!("base64 解码失败: {e}")),
    };
    let inbox = match inbox_dir() {
        Ok(d) => d,
        Err(e) => return Reply::err(&frame.id, e),
    };
    let path = inbox.join(format!(
        "{}{}.png",
        native_protocol::PLUGIN_FILE_PREFIX,
        sanitize_id(&frame.id)
    ));
    if let Err(e) = std::fs::write(&path, &bytes) {
        return Reply::err(&frame.id, format!("写入临时文件失败: {e}"));
    }
    Reply::saved(&frame.id, path.to_string_lossy().into_owned())
}

/// 收件箱目录：`<系统临时目录>/hawksnap-native-inbox`（0700，仅本人可读写）。
fn inbox_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("hawksnap-native-inbox");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建收件箱失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

/// id 消毒：只保留 [A-Za-z0-9_-]，其余替换为 '_'（文件名安全）。
fn sanitize_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

// —— 帧读取（Chrome 长度前缀 / 裸 JSON 行 自动识别）——

/// 帧格式：Chrome 的 native messaging = 4 字节小端长度 + 载荷；裸 JSON 行 =
/// 一行一个 JSON 文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Chrome 原生消息格式（长度前缀）。
    Chrome,
    /// 裸 JSON 行（`{...}\n`）——测试与开发工具用。
    Lines,
}

/// 帧读取器：按字节流读，自动识别首帧格式（`next_payload` 首次调用时判定，
/// 之后整个流沿用同一格式）。
struct FrameReader<R: Read> {
    inner: R,
    buf: Vec<u8>,
    eof: bool,
}

impl<R: Read> FrameReader<R> {
    fn new(inner: R) -> Self {
        FrameReader {
            inner,
            buf: Vec::new(),
            eof: false,
        }
    }

    /// 尽量把缓冲区填到 `want` 字节（EOF 则尽力而为）。
    fn fill(&mut self, want: usize) -> std::io::Result<usize> {
        while self.buf.len() < want && !self.eof {
            let mut chunk = [0u8; 8192];
            let k = self.inner.read(&mut chunk)?;
            if k == 0 {
                self.eof = true;
                break;
            }
            self.buf.extend_from_slice(&chunk[..k]);
        }
        Ok(self.buf.len())
    }

    /// 读下一帧载荷（`None` = EOF）。
    fn next_payload(&mut self, framing: &mut Option<Framing>) -> Result<Option<String>, String> {
        if framing.is_none() {
            let n = self.fill(4).map_err(|e| e.to_string())?;
            if n == 0 {
                return Ok(None);
            }
            let head = &self.buf[..n.min(4)];
            // 判据：裸 JSON 行必以 '{' 开头，且其后紧跟的字节不可能是 NUL
            // （JSON 文本不含 NUL）。故「首字节 '{' 且后三字节为 0」只可能是
            // Chrome 的长度前缀（长度低字节 = 0x7B）。其余以 '{' 开头的按行解析。
            let is_json_line = head[0] == b'{'
                && !(head.len() >= 4 && head[1] == 0 && head[2] == 0 && head[3] == 0);
            *framing = Some(if is_json_line {
                Framing::Lines
            } else {
                Framing::Chrome
            });
        }
        match framing.unwrap_or(Framing::Lines) {
            Framing::Chrome => {
                let n = self.fill(4).map_err(|e| e.to_string())?;
                if n == 0 {
                    return Ok(None);
                }
                if n < 4 {
                    return Err("帧头不完整".to_string());
                }
                let len =
                    u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
                self.buf.drain(..4);
                if len > native_protocol::MAX_FRAME_BYTES {
                    return Err("帧过大".to_string());
                }
                let got = self.fill(len).map_err(|e| e.to_string())?;
                if got < len {
                    return Err("帧不完整".to_string());
                }
                // 取走前 len 字节（split_off + mem::replace：避免 drain(..).collect()
                // 的 clippy 告警，且不额外分配）。
                let tail = self.buf.split_off(len);
                let payload = std::mem::replace(&mut self.buf, tail);
                Ok(Some(
                    String::from_utf8(payload).map_err(|_| "帧不是合法 UTF-8".to_string())?,
                ))
            }
            Framing::Lines => loop {
                if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                    // 取走含换行的那一段（剩余部分留在缓冲里）。
                    let tail = self.buf.split_off(pos + 1);
                    let line = std::mem::replace(&mut self.buf, tail);
                    return Ok(Some(String::from_utf8_lossy(&line).trim().to_string()));
                }
                if self.buf.len() > native_protocol::MAX_FRAME_BYTES {
                    return Err("帧过长".to_string());
                }
                if self.eof {
                    if self.buf.is_empty() {
                        return Ok(None);
                    }
                    let line = std::mem::take(&mut self.buf);
                    return Ok(Some(String::from_utf8_lossy(&line).trim().to_string()));
                }
                let want = self.buf.len() + 8192;
                self.fill(want).map_err(|e| e.to_string())?;
            },
        }
    }
}

/// 写回执（格式与输入一致）。
fn write_payload<W: Write>(w: &mut W, framing: Framing, payload: &str) -> std::io::Result<()> {
    match framing {
        Framing::Chrome => {
            let bytes = payload.as_bytes();
            w.write_all(&(bytes.len() as u32).to_le_bytes())?;
            w.write_all(bytes)?;
        }
        Framing::Lines => {
            w.write_all(payload.as_bytes())?;
            w.write_all(b"\n")?;
        }
    }
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn reader(bytes: &[u8]) -> FrameReader<Cursor<Vec<u8>>> {
        FrameReader::new(Cursor::new(bytes.to_vec()))
    }

    #[test]
    fn reads_json_lines() {
        let mut r = reader(b"{\"cmd\":\"capture\"}\n{\"cmd\":\"save\"}\n");
        let mut framing = None;
        let a = r.next_payload(&mut framing).unwrap().unwrap();
        assert_eq!(a, r#"{"cmd":"capture"}"#);
        assert_eq!(framing, Some(Framing::Lines));
        let b = r.next_payload(&mut framing).unwrap().unwrap();
        assert_eq!(b, r#"{"cmd":"save"}"#);
        assert!(r.next_payload(&mut framing).unwrap().is_none());
    }

    #[test]
    fn reads_chrome_length_prefixed_frames() {
        let p1 = r#"{"cmd":"capture","id":"a"}"#;
        let p2 = r#"{"cmd":"save","id":"b"}"#;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(p1.len() as u32).to_le_bytes());
        bytes.extend_from_slice(p1.as_bytes());
        bytes.extend_from_slice(&(p2.len() as u32).to_le_bytes());
        bytes.extend_from_slice(p2.as_bytes());
        let mut r = reader(&bytes);
        let mut framing = None;
        assert_eq!(r.next_payload(&mut framing).unwrap().unwrap(), p1);
        assert_eq!(framing, Some(Framing::Chrome));
        assert_eq!(r.next_payload(&mut framing).unwrap().unwrap(), p2);
        assert!(r.next_payload(&mut framing).unwrap().is_none());
    }

    #[test]
    fn chrome_frame_with_brace_length_is_not_mistaken_for_json() {
        // 长度 = 0x7B('{') 的 Chrome 帧：其后 3 字节为 0 → 必须判成 Chrome，
        // 否则整条流会错位（这是本模块最易踩的判据坑）。
        let payload = "x".repeat(123);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload.as_bytes());
        let mut r = reader(&bytes);
        let mut framing = None;
        assert_eq!(r.next_payload(&mut framing).unwrap().unwrap(), payload);
        assert_eq!(framing, Some(Framing::Chrome));
    }

    #[test]
    fn write_payload_matches_framing() {
        let mut out = Vec::new();
        write_payload(&mut out, Framing::Lines, "{}").unwrap();
        assert_eq!(out, b"{}\n".to_vec());
        let mut out = Vec::new();
        write_payload(&mut out, Framing::Chrome, "{}").unwrap();
        assert_eq!(out, vec![2, 0, 0, 0, b'{', b'}']);
    }

    #[test]
    fn sanitize_id_keeps_safe_chars() {
        assert_eq!(sanitize_id("abc-123_x"), "abc-123_x");
        // 路径分隔符与点号都被替换（文件名安全：收件箱里的文件名不可带路径成分）
        assert_eq!(sanitize_id("../../etc"), "______etc");
        assert_eq!(sanitize_id(""), "unknown");
    }
}
