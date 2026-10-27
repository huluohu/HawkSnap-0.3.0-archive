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
//   HAWKSNAP_NATIVE_FRAMING=lines|chrome  强制 stdin 帧格式（默认自动识别，
//                                           但**默认按 Chrome 长度前缀**解析）
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
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

/// 与桌面端通信的超时（截图入库要解码 PNG + 写盘 + 入库，给足时间）。
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// 等待下一帧的静默上限——**兜底**：帧格式误判 / 流被截断时，宿主不能无限期
/// 挂着（浏览器会一直等回执）。超时后宿主写一条错误回执并退出（插件侧会收到
/// `onDisconnect` → 重连即由浏览器重新拉起宿主）。
const READ_TIMEOUT: Duration = Duration::from_secs(300);

/// 读宿主配置的重试（桌面端可能比宿主晚起 → 配置文件还没写出来）。
const CONFIG_RETRY: usize = 5;
const CONFIG_RETRY_DELAY: Duration = Duration::from_millis(300);

fn main() {
    let stdin = std::io::stdin();
    let forced = match std::env::var("HAWKSNAP_NATIVE_FRAMING").as_deref() {
        Ok("lines") => Some(Framing::Lines),
        Ok("chrome") => Some(Framing::Chrome),
        _ => None,
    };
    // **错误回执**（读帧失败 / 静默超时）的帧格式（评审修复轮 P4）：
    // 只有**显式**要求 lines（HAWKSNAP_NATIVE_FRAMING=lines）时才用裸 JSON 行，
    // 其余一律 Chrome 长度前缀。
    //
    // 为什么不能沿用「读到的帧格式」：流一旦被误判成 Lines（Chrome 帧的长度
    // 低字节恰好是 '{'），按行格式回执 → Chrome 读到的是**没有长度前缀**的
    // 字节 → 它把回执的前 4 字节当长度 → 读出垃圾 → "Invalid message" →
    // 端口关闭 → 插件只看到 onDisconnect（拿不到任何错误原因）。
    let error_framing = error_reply_framing(forced);

    // 读帧放到独立线程：主线程用 `recv_timeout` 卡住 → 静默超时能变成一条
    // 错误回执，而不是「永远等下去」。
    let (tx, rx) = mpsc::channel::<FrameMsg>();
    std::thread::spawn(move || {
        let mut reader = FrameReader::new(stdin.lock());
        let mut framing = forced;
        loop {
            match reader.next_payload(&mut framing) {
                Ok(Some(payload)) => {
                    let mode = framing.unwrap_or(Framing::Chrome);
                    if tx.send(FrameMsg::Payload(payload, mode)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(FrameMsg::Eof);
                    break;
                }
                Err(e) => {
                    let _ = tx.send(FrameMsg::Error(e));
                    break;
                }
            }
        }
    });

    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    loop {
        match rx.recv_timeout(READ_TIMEOUT) {
            // EOF：浏览器关闭管道 → 宿主退出。
            Ok(FrameMsg::Eof) => break,
            Ok(FrameMsg::Payload(payload, mode)) => {
                let reply = handle(&payload);
                let out = serde_json::to_string(&reply).unwrap_or_else(|e| {
                    format!(r#"{{"id":"","ok":false,"error":"回执序列化失败: {e}"}}"#)
                });
                if let Err(e) = write_payload(&mut writer, mode, &out) {
                    eprintln!("[hawksnap-host] 写回执失败: {e}");
                    break;
                }
            }
            Ok(FrameMsg::Error(e)) => {
                // 读帧出错 → 回一条错误回执（让插件知道是失败而不是「无响应」）。
                // 帧格式用 error_framing（Chrome 长度前缀，除非显式 lines）。
                eprintln!("[hawksnap-host] 读取帧失败: {e}");
                let reply = Reply::err("", format!("读取帧失败: {e}"));
                let out = serde_json::to_string(&reply).unwrap_or_default();
                let _ = write_payload(&mut writer, error_framing, &out);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                eprintln!("[hawksnap-host] 静默超时（{READ_TIMEOUT:?}），退出");
                let reply = Reply::err("", "宿主静默超时");
                let out = serde_json::to_string(&reply).unwrap_or_default();
                let _ = write_payload(&mut writer, error_framing, &out);
                break;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// 读帧线程 → 主线程的消息。
enum FrameMsg {
    /// 一帧载荷 + 该流的帧格式。
    Payload(String, Framing),
    /// 输入结束（EOF）。
    Eof,
    /// 读帧出错。
    Error(String),
}

/// 读宿主配置（端口 + 令牌）——桌面端启动时写的 0600 文件。
///
/// **每次转发前都重读**（桌面端重启会换令牌/端口；宿主是 Chrome 每次通信拉起
/// 的短命进程，缓存配置 = 拿着过期令牌去敲门）。
fn load_config() -> Result<HostConfig, String> {
    let mut last = String::new();
    for attempt in 0..CONFIG_RETRY {
        match read_config_once() {
            Ok(cfg) => return Ok(cfg),
            Err(e) => {
                last = e;
                if attempt + 1 < CONFIG_RETRY {
                    std::thread::sleep(CONFIG_RETRY_DELAY);
                }
            }
        }
    }
    Err(last)
}

fn read_config_once() -> Result<HostConfig, String> {
    if let Ok(path) = std::env::var("HAWKSNAP_NATIVE_HOST_CONFIG") {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("读取宿主配置失败 ({path}): {e}"))?;
        return serde_json::from_str(&text).map_err(|e| format!("宿主配置解析失败: {e}"));
    }
    native_protocol::read_host_config()
}

/// 一帧一答复。
fn handle(payload: &str) -> Reply {
    let frame = match native_protocol::parse_frame(payload) {
        Ok(f) => f,
        Err(e) => return Reply::err("", e),
    };
    match frame.cmd.as_str() {
        "save" => save_payload(&frame),
        "capture" => capture(&frame),
        other => Reply::err(&frame.id, format!("不支持的命令: {other}")),
    }
}

/// `capture`：校验路径 → 经回环 socket 转发给桌面端 → 原样返回桌面端回执。
fn capture(frame: &Frame) -> Reply {
    // 宿主侧也校验一遍（纵深防御：桌面端还会再校验一次）。
    if let Err(e) = native_protocol::validate_capture_path(&frame.path) {
        return Reply::err(&frame.id, e);
    }
    // 每次转发前重读配置（桌面端可能已重启 → 令牌/端口都变了）。
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => return Reply::err(&frame.id, e),
    };
    match relay(&cfg, frame) {
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
            // **默认按 Chrome 长度前缀解析**；只有「首字节 '{' 且次字节看起来像
            // JSON 文本」才按裸 JSON 行解析。
            //
            // 为什么这样判（评审修复轮）：Chrome 帧的头 4 字节是长度（小端），
            // 低字节可能恰好是 '{'(0x7B)。旧判据（「后三字节全 0 才算 Chrome」）
            // 会把长度 ≡ 123 (mod 256) 且 ≥ 256 的帧（如 379/635/891）误判成
            // JSON 行 → 宿主等一个永远不来的 '\n' → **请求永不回执**。新判据把
            // 默认放在 Chrome 上，只有「看起来确实像一行 JSON」才走行模式。
            // 残留的极小歧义（Chrome 帧长度低字节为 0x7B 且次字节像 JSON）由
            // 行模式的**两级兜底**兜住（见下）：① Chrome 长度前缀识别、② JSON
            // 值兜底（take_json_value）——都不会挂死，也不会让流错位。
            let is_json_line = head[0] == b'{' && head.len() >= 2 && is_json_second_char(head[1]);
            *framing = Some(if is_json_line {
                Framing::Lines
            } else {
                Framing::Chrome
            });
        }
        loop {
            match framing.unwrap_or(Framing::Chrome) {
                Framing::Chrome => {
                    let n = self.fill(4).map_err(|e| e.to_string())?;
                    if n == 0 {
                        return Ok(None);
                    }
                    if n < 4 {
                        return Err("帧头不完整".to_string());
                    }
                    let len =
                        u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
                            as usize;
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
                    return Ok(Some(
                        String::from_utf8(payload).map_err(|_| "帧不是合法 UTF-8".to_string())?,
                    ));
                }
                Framing::Lines => {
                    // 跳过行首空白（上一帧残留的换行等）。
                    let skip = self
                        .buf
                        .iter()
                        .position(|b| !b.is_ascii_whitespace())
                        .unwrap_or(self.buf.len());
                    if skip > 0 {
                        self.buf.drain(..skip);
                    }
                    // 长度前缀判定需要 4 字节；不够就先补（EOF 则按现有内容继续）。
                    if self.buf.len() < 4 && !self.eof {
                        self.fill(4).map_err(|e| e.to_string())?;
                        continue;
                    }
                    // ① **Chrome 长度前缀兜底**（评审修复轮 P3）。
                    //
                    // 为什么必须放在「找换行」之前：Chrome 帧长度 ≡ 123 (mod 256)
                    // 且长度落在 640–704 KiB 时，长度前缀（小端）的**第三个字节
                    // 恰好是 0x0A**（'\n'）——例如长度 672123 = 0x000A417B → 前缀
                    // 字节 7B 41 0A 00。旧顺序「先扫换行」会把长度前缀当成一行
                    // → 垃圾帧 → 流错位 → 宿主报错退出。
                    //
                    // 判据：缓冲区开头 4 字节按小端读出的长度落在 [2, 帧上限]
                    // → 这条流**就是** Chrome 帧流（帧格式被误判）→ 纠正格式，
                    // 回到 Chrome 分支按长度前缀读（不会再错位）。
                    // 误伤面：裸 JSON 行的前 4 字节是 `{"xx` 一类 → 小端读出的
                    // 「长度」高达 GB 级 → 不会命中（JSON 文本里也没有 NUL 字节）。
                    if self.chrome_prefix_len().is_some() {
                        *framing = Some(Framing::Chrome);
                        continue;
                    }
                    // ② **完整 JSON 值**（比换行更可靠的边界）——同样必须**先于**
                    // 换行判定：缓冲区里若已有一个能解析出的完整 JSON 值，它就是
                    // 更可信的帧边界（换行可能只是长度前缀里的 0x0A）。
                    // 这让「长度前缀被误判成行模式」的 Chrome 流也能恢复（并且把
                    // 误判残留的长度前缀字节一并丢掉，流不会错位）。
                    if let Some((value, had_garbage)) = take_json_value(&mut self.buf) {
                        if had_garbage {
                            // 候选位置之前有**非空白**字节 = Chrome 的长度前缀残留
                            // → 这条流其实是 Chrome 帧流 → 纠正格式（后续帧按长度
                            // 前缀读，不会错位）。
                            *framing = Some(Framing::Chrome);
                        }
                        return Ok(Some(value));
                    }
                    // ③ 换行边界（缓冲区里既没有完整 JSON 值，也没有长度前缀）。
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
                }
            }
        }
    }

    /// 缓冲区开头是否像一个**合法的 Chrome 长度前缀**（小端 u32，落在
    /// [2, `MAX_FRAME_BYTES`]）。
    ///
    /// 用于「行模式其实是 Chrome 帧流」的兜底纠正（评审修复轮 P3）：Chrome 帧
    /// 的长度前缀被误判成行模式时，靠它把格式纠正回来（并避免把长度前缀里的
    /// 0x0A 当成行尾 → 垃圾帧 + 流错位）。
    fn chrome_prefix_len(&self) -> Option<usize> {
        if self.buf.len() < 4 {
            return None;
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if !(2..=native_protocol::MAX_FRAME_BYTES).contains(&len) {
            return None;
        }
        Some(len)
    }
}

/// 错误回执 / 静默超时回执的帧格式（评审修复轮 P4）。
///
/// 只有**显式**要求行格式（`HAWKSNAP_NATIVE_FRAMING=lines`）时才用裸 JSON 行，
/// 其余一律 Chrome 长度前缀。
///
/// 为什么不能沿用「读到的帧格式」：流一旦被误判成 Lines（Chrome 帧的长度低字节
/// 恰好是 '{'），按行格式回执 → Chrome 读到的是**没有长度前缀**的字节 → 它把
/// 回执的头 4 字节当长度 → 读出垃圾 → "Invalid message" → 端口关闭 → 插件只
/// 看到 onDisconnect（拿不到任何错误原因）。
fn error_reply_framing(forced: Option<Framing>) -> Framing {
    match forced {
        Some(Framing::Lines) => Framing::Lines,
        _ => Framing::Chrome,
    }
}

/// JSON 文本的第 2 个字符：可打印 ASCII（`"` / `}` / 字母数字 / 标点）**或
/// 空白**。
///
/// ⚠️ 旧注释说「空白与控制字符都不可能是 JSON 文本的第二字节」——**这是错的**：
/// JSON 允许 `{` 与后续内容之间有空白（如 `{ "cmd": "capture" }`），空白**就是**
/// 合法的第二字节。旧判据把这种裸 JSON 行当成 Chrome 长度前缀 → 整条流错位
/// → 宿主报错退出（评审修复轮 P5）。故这里把空白也算作「像 JSON」。
///
/// 控制字符（< 0x20 且非空白）仍然排除：JSON 文本里不允许裸控制字符。
fn is_json_second_char(b: u8) -> bool {
    (0x21..=0x7e).contains(&b) || b.is_ascii_whitespace()
}

/// 行模式兜底：从缓冲区里取出一个**完整的 JSON 值**（`{` 起）。
///
/// 逐个尝试候选 `{` 位置（帧格式误判时，长度前缀里可能也有 `{`），用
/// `serde_json` 的流式解析器取出**第一个** JSON 值及其消耗字节数；候选之前的
/// 字节（行模式的空白 / 误判残留的长度前缀）一并丢弃 → 流保持同步。
///
/// 返回 `(值, 候选之前是否有非空白字节)`：后者为真说明前面是 Chrome 的长度
/// 前缀残留 → 调用方据此把帧格式纠正回 Chrome。
fn take_json_value(buf: &mut Vec<u8>) -> Option<(String, bool)> {
    let candidates: Vec<usize> = buf
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'{')
        .map(|(i, _)| i)
        .take(64)
        .collect();
    for start in candidates {
        let Some((consumed, _)) = parse_first_json(&buf[start..]) else {
            continue;
        };
        let value = String::from_utf8_lossy(&buf[start..start + consumed]).trim().to_string();
        let had_garbage = buf[..start].iter().any(|b| !b.is_ascii_whitespace());
        buf.drain(..start + consumed);
        return Some((value, had_garbage));
    }
    None
}

/// 解析字节切片里的**第一个** JSON 值，返回 (消耗字节数, 值)。
fn parse_first_json(bytes: &[u8]) -> Option<(usize, serde_json::Value)> {
    let mut de = serde_json::Deserializer::from_slice(bytes).into_iter::<serde_json::Value>();
    match de.next() {
        Some(Ok(v)) => Some((de.byte_offset(), v)),
        _ => None,
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

    /// 评审修复轮（I1）：Chrome 帧长度 ≡ 123 (mod 256) 且 ≥ 256 时，旧判据会
    /// 误判成「JSON 行」→ 宿主等一个永远不来的 '\n' → **请求永不回执**。
    /// 这里逐个覆盖评审点名的 379 / 635 / 891 / 1147。
    #[test]
    fn chrome_frames_with_brace_low_byte_are_read_as_chrome() {
        for len in [379usize, 635, 891, 1147, 1675] {
            let payload = "y".repeat(len);
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(payload.as_bytes());
            let mut r = reader(&bytes);
            let mut framing = None;
            assert_eq!(
                r.next_payload(&mut framing).unwrap().unwrap(),
                payload,
                "长度 {len} 的 Chrome 帧必须被正确读出"
            );
            assert_eq!(framing, Some(Framing::Chrome), "长度 {len} 必须判成 Chrome 格式");
        }
    }

    #[test]
    fn chrome_frames_with_printable_second_length_byte_are_recovered() {
        // 长度 = 0x227B = 8827 → 长度前缀 = 7B 22 00 00（次字节可打印）→ 判成
        // 行模式；此时靠**行模式的 JSON 值兜底**把帧捞出来（并丢掉残留的长度
        // 字节 → 流不错位，且格式被纠正回 Chrome）。
        let pad = "z".repeat(8827 - 10);
        let payload = format!("{{\"pad\":\"{pad}\"}}");
        assert_eq!(payload.len(), 8827);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload.as_bytes());
        let mut r = reader(&bytes);
        let mut framing = None;
        let got = r.next_payload(&mut framing).unwrap().unwrap();
        assert_eq!(got, payload, "兜底必须把整帧捞出来");
        // 残留的长度前缀字节已被丢弃 → 下一帧仍能正确读出（格式已纠正为 Chrome）。
        assert_eq!(framing, Some(Framing::Chrome));
        let p2 = "w".repeat(10);
        let mut more = Vec::new();
        more.extend_from_slice(&(p2.len() as u32).to_le_bytes());
        more.extend_from_slice(p2.as_bytes());
        r.inner = Cursor::new(more);
        r.eof = false; // 换了一条流 → EOF 标志要复位
        assert_eq!(r.next_payload(&mut framing).unwrap().unwrap(), p2);
    }

    #[test]
    fn bare_json_line_stream_is_read_as_lines() {
        let mut r = reader(b"{\"cmd\":\"capture\",\"id\":\"a\"}\n{\"cmd\":\"save\",\"id\":\"b\"}\n");
        let mut framing = None;
        assert_eq!(
            r.next_payload(&mut framing).unwrap().unwrap(),
            r#"{"cmd":"capture","id":"a"}"#
        );
        assert_eq!(framing, Some(Framing::Lines));
        assert_eq!(
            r.next_payload(&mut framing).unwrap().unwrap(),
            r#"{"cmd":"save","id":"b"}"#
        );
        assert!(r.next_payload(&mut framing).unwrap().is_none());
    }

    #[test]
    fn forced_lines_framing_reads_bare_json_without_newline() {
        // 显式强制行模式（HAWKSNAP_NATIVE_FRAMING=lines）：没有换行也能靠
        // JSON 值兜底出帧。
        let mut r = reader(b"{\"cmd\":\"capture\",\"id\":\"a\"}");
        let mut framing = Some(Framing::Lines);
        assert_eq!(
            r.next_payload(&mut framing).unwrap().unwrap(),
            r#"{"cmd":"capture","id":"a"}"#
        );
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

    /// 评审修复轮 P3：Chrome 帧长度 ≡ 123 (mod 256) 且长度落在 640–704 KiB 时，
    /// 长度前缀（小端）的**第三个字节恰好是 0x0A**（'\n'）——672123 = 0x000A417B
    /// → 前缀字节 `7B 41 0A 00`。旧顺序（先扫换行）会把长度前缀当成一行 →
    /// 垃圾帧 → 流错位 → 宿主报错退出。这里必须整帧读出且格式纠正为 Chrome。
    #[test]
    fn chrome_frame_with_newline_inside_length_prefix_is_read_as_chrome() {
        let len = 672_123usize; // ≡ 123 (mod 256)，落在 640–704 KiB 区间
        assert_eq!(len % 256, 123);
        let payload = "y".repeat(len);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload.as_bytes());
        // 前缀里确实藏着 '\n'（否则本测试就失去意义）。
        assert_eq!(&bytes[..4], &[0x7B, 0x41, 0x0A, 0x00]);
        let mut r = reader(&bytes);
        let mut framing = None;
        assert_eq!(
            r.next_payload(&mut framing).unwrap().unwrap(),
            payload,
            "长度前缀里的 0x0A 不能被当成行尾"
        );
        assert_eq!(framing, Some(Framing::Chrome), "必须纠正回 Chrome 格式");
    }

    /// 评审修复轮 P5：裸 JSON 行的第 2 个字节可以是**空白**（JSON 允许 `{` 之后
    /// 有空白）——旧判据只认「可打印 ASCII」→ 把 `{ "cmd": … }` 当成 Chrome 长度
    /// 前缀 → 整条流错位。
    #[test]
    fn bare_json_line_with_whitespace_second_char_is_read_as_lines() {
        let mut r = reader(b"{ \"cmd\": \"capture\" }\n");
        let mut framing = None;
        assert_eq!(
            r.next_payload(&mut framing).unwrap().unwrap(),
            r#"{ "cmd": "capture" }"#
        );
        assert_eq!(framing, Some(Framing::Lines));
    }

    /// 评审修复轮 P4：错误回执的帧格式——只有**显式** lines 才用行格式。
    #[test]
    fn error_reply_framing_defaults_to_chrome() {
        assert_eq!(error_reply_framing(None), Framing::Chrome);
        assert_eq!(error_reply_framing(Some(Framing::Chrome)), Framing::Chrome);
        assert_eq!(error_reply_framing(Some(Framing::Lines)), Framing::Lines);
    }

    #[test]
    fn sanitize_id_keeps_safe_chars() {
        assert_eq!(sanitize_id("abc-123_x"), "abc-123_x");
        // 路径分隔符与点号都被替换（文件名安全：收件箱里的文件名不可带路径成分）
        assert_eq!(sanitize_id("../../etc"), "______etc");
        assert_eq!(sanitize_id(""), "unknown");
    }
}

