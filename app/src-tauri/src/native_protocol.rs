// M4 P1 —— Native Messaging 帧协议（纯逻辑，**零 Tauri 依赖**）。
//
// 本文件被**两处**编译（同一份源码 → 帧结构与校验逻辑零漂移）：
//   ① 桌面端 lib：`app/src-tauri/src/native_messaging.rs` 经 `mod native_protocol;`
//      （在 lib.rs 声明）使用；
//   ② 宿主 bin：`app/src-tauri/src/bin/hawksnap-host.rs` 经
//      `#[path = "../native_protocol.rs"] mod native_protocol;` 使用。
//
// 为什么不让宿主 bin 直接依赖 lib（`use app_lib::…`）：lib 链着整个 Tauri
// （wry/WebKit/GTK…）——Chrome 拉起的宿主进程本应是「几 MB 的小工具」，
// 链接 Tauri 会让它变成几十 MB 且在 macOS 上拖着 WebKit 运行时起来（拖慢
// 每次插件通信、可能与主应用的 webview 抢资源）。故共享层只留**纯逻辑**
// （serde + 文件 + 路径），宿主 bin 因此只依赖 serde/serde_json/base64。
//
// 协议契约见 `docs/superpowers/protocols/native-messaging.md`（P2 的 TypeScript
// 必须逐字对齐该文档）。

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Native Messaging 宿主名（Chrome manifest 的 `name` 字段；与应用 bundle id 同名）。
pub const HOST_NAME: &str = "com.hawksnap.app";

/// 桌面端监听端口（首选）。被占用时向上扫描 [`PORT_SCAN_TRIES`] 个端口，
/// 实际端口写入宿主配置文件——端口不硬编码到协议里，宿主从配置读。
pub const PREFERRED_PORT: u16 = 38471;

/// 端口回退扫描范围（首选端口 + 0..16）。
pub const PORT_SCAN_TRIES: u16 = 16;

/// 插件临时截图文件名约束：必须是 `<系统临时目录>/hawksnap-plugin-*.png`。
///
/// 为什么限定文件名：桌面端只接受「插件自己写的临时文件」——固定前缀让
/// 「任意临时文件被诱导导入」这件事在协议层就不可表达（纵深防御：路径根 +
/// 扩展名 + 文件名前缀三重约束，见 [`validate_capture_path`]）。
pub const PLUGIN_FILE_PREFIX: &str = "hawksnap-plugin-";

/// 单帧上限（8 MiB）——防内存耗尽（宿主侧 base64 图片载荷也在此限内）。
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// 插件临时截图的**文件体积**上限（64 MiB）——入库前先看文件大小，避免把一个
/// 超大文件整个读进内存（DoS 面）。
pub const MAX_CAPTURE_BYTES: u64 = 64 * 1024 * 1024;

/// 插件临时截图的**像素数**上限（40 MP）——按 PNG 的 IHDR 读真实宽高（不解码
/// 全图）即可判定；超限直接拒，避免解码时的内存尖峰。
///
/// 口径说明：1920×10000 的整页长截图 ≈ 19.2 MP，在限内；40 MP 之上属异常。
pub const MAX_CAPTURE_PIXELS: u64 = 40_000_000;

/// 宿主配置文件名（位于用户配置目录，见 [`config_dir`]）。
pub const CONFIG_FILE_NAME: &str = "native-host.json";

/// 帧尺寸上限（防止离谱值；0 = 未知，桌面端以 PNG 实际像素为准）。
const MAX_DIMENSION: u32 = 100_000;

// —— 帧结构 ——

/// 插件 → 宿主 → 桌面端 的统一帧。
///
/// 插件侧发出的 `capture` 帧**不带** `token`（浏览器侧无从得知桌面端令牌）；
/// 宿主 bin 转发给桌面端时补上 `token`——这是「桌面端只信本机已注册宿主」
/// （计划 Global Constraints）在协议层的落点。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Frame {
    /// "capture"（截图入库）| "save"（宿主侧落盘，见协议文档扩展节）。
    pub cmd: String,
    /// 请求 id（插件生成的 uuid）——回执原样带回，供插件配对请求与回执。
    pub id: String,
    /// 临时 PNG 的绝对路径（`capture` 帧必填；`save` 帧可空）。
    #[serde(default)]
    pub path: String,
    /// 截图宽（像素；0 = 未知）。
    #[serde(default)]
    pub width: u32,
    /// 截图高（像素；0 = 未知）。
    #[serde(default)]
    pub height: u32,
    /// 桌面端令牌（宿主 bin 转发时补；插件帧不带）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// 可选：base64 PNG 载荷（仅宿主侧 `save` 命令使用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// 回执（宿主 → 插件）。成功：`{id, ok:true, savedPath}`；失败：`{id, ok:false, error}`。
///
/// `path` 只出现在宿主侧 `save` 的回执里（回传临时文件落点）；`capture` 回执
/// **不含** `path`（协议要求成功回执只有 `savedPath`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Reply {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Reply {
    /// 成功回执（capture：`savedPath`）。
    pub fn ok(id: &str, saved_path: impl Into<String>) -> Self {
        Reply {
            id: id.to_string(),
            ok: true,
            saved_path: Some(saved_path.into()),
            error: None,
            path: None,
        }
    }

    /// 失败回执。
    pub fn err(id: &str, error: impl Into<String>) -> Self {
        Reply {
            id: id.to_string(),
            ok: false,
            saved_path: None,
            error: Some(error.into()),
            path: None,
        }
    }

    /// 宿主侧 `save` 回执（回传临时文件路径）。
    ///
    /// 桌面端 lib 不用它（只有宿主 bin 的 `save` 分支用）——但本文件在 lib 与
    /// bin 两处各自编译，lib 侧看着就是「无人调用」→ 就地放行（不做 crate 级
    /// blanket allow，见 lib.rs 头部纪律）。
    #[allow(dead_code)]
    pub fn saved(id: &str, path: impl Into<String>) -> Self {
        Reply {
            id: id.to_string(),
            ok: true,
            saved_path: None,
            error: None,
            path: Some(path.into()),
        }
    }
}

// —— 纯函数：帧解析与路径校验 ——

/// 解析一行 JSON 帧。非法 JSON / 未知命令 / 缺字段 → Err(人类可读原因)。
///
/// 桌面端与宿主 bin 共用同一份解析（同一文件编译两处）——插件侧发来的帧在
/// 「进桌面端之前」与「进桌面端之后」用的是同一把尺子。
pub fn parse_frame(line: &str) -> Result<Frame, String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err("空帧".to_string());
    }
    let frame: Frame = serde_json::from_str(trimmed).map_err(|e| format!("帧解析失败: {e}"))?;
    if frame.cmd != "capture" && frame.cmd != "save" {
        return Err(format!("不支持的命令: {}", frame.cmd));
    }
    if frame.id.trim().is_empty() {
        return Err("帧缺少 id".to_string());
    }
    if frame.cmd == "capture" && frame.path.trim().is_empty() {
        return Err("帧缺少 path".to_string());
    }
    if frame.width > MAX_DIMENSION || frame.height > MAX_DIMENSION {
        return Err("帧尺寸超出上限".to_string());
    }
    Ok(frame)
}

/// 校验插件临时截图路径，返回**规范化**（canonical）绝对路径。
///
/// 三重约束（任一不满足即拒）：
///   ① 必须是绝对路径；
///   ② 扩展名必须是 `.png`；
///   ③ 必须位于**系统临时目录**内，且文件名形如 `hawksnap-plugin-*.png`。
///
/// 规范化（`canonicalize`）后再判「位于临时目录内」——`../` 穿越与指向目录外
/// 的符号链接都会在这一步现形（解析后的真实路径不在临时目录 → 拒）。
/// 文件必须已存在（插件刚写完）：不存在 → 拒（避免把「即将出现的路径」当
/// 作可信输入）。
pub fn validate_capture_path(path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err("路径必须是绝对路径".to_string());
    }
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !ext.eq_ignore_ascii_case("png") {
        return Err("只接受 .png 文件".to_string());
    }
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if !name.starts_with(PLUGIN_FILE_PREFIX) || !name.to_ascii_lowercase().ends_with(".png") {
        return Err(format!("文件名必须形如 {PLUGIN_FILE_PREFIX}*.png"));
    }
    let canonical = p
        .canonicalize()
        .map_err(|_| "临时文件不存在或不可访问".to_string())?;
    let tmp = std::env::temp_dir()
        .canonicalize()
        .map_err(|e| format!("无法定位系统临时目录: {e}"))?;
    if !canonical.starts_with(&tmp) {
        return Err("路径不在系统临时目录内".to_string());
    }
    Ok(canonical)
}

// —— 宿主配置文件（端口 + 令牌）——

/// 宿主配置文件路径：`<用户配置目录>/com.hawksnap.app/native-host.json`。
///
/// **不**用 Tauri 的 `app_data_dir()`：桌面端与宿主 bin 必须算出**同一个**路径，
/// 而宿主 bin 没有 Tauri app 上下文。故此处按平台约定自行推导（与 Tauri 的
/// `app_data_dir` 约定一致：macOS `~/Library/Application Support/<id>`、
/// Windows `%APPDATA%\<id>`、Linux `$XDG_CONFIG_HOME/<id>`）。
pub fn config_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "无法定位用户主目录".to_string())?;
    #[cfg(target_os = "macos")]
    {
        Ok(home.join("Library").join("Application Support").join(HOST_NAME))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(home.join("AppData").join("Roaming").join(HOST_NAME))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        Ok(base.join(HOST_NAME))
    }
}

/// 宿主配置文件全路径。
pub fn config_path() -> Result<PathBuf, String> {
    Ok(config_dir()?.join(CONFIG_FILE_NAME))
}

/// 宿主配置内容（桌面端写、宿主 bin 读）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HostConfig {
    /// 桌面端实际监听的端口（首选端口被占时回退扫描后的结果）。
    pub port: u16,
    /// 本次运行的一次性令牌（桌面端每次启动重新生成）。
    pub token: String,
    /// 写配置的进程 id（诊断用）。
    pub pid: u32,
    /// 宿主名（诊断用）。
    pub host_name: String,
}

/// 读宿主配置（宿主 bin 启动时用）。
pub fn read_host_config() -> Result<HostConfig, String> {
    let path = config_path()?;
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("读取宿主配置失败 ({}): {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("宿主配置解析失败: {e}"))
}

/// 写宿主配置（桌面端启动时用；文件权限 0600）。
pub fn write_host_config(path: &Path, cfg: &HostConfig) -> Result<(), String> {
    let json = serde_json::to_string(cfg).map_err(|e| e.to_string())?;
    write_secret_file(path, &json)
}

/// 写「仅本人可读」的文件（0600）。Unix 下用 `mode(0o600)` 建文件并显式
/// `set_permissions` 复核（文件已存在时 `mode` 不生效——必须复核）。
///
/// **写临时文件再 rename**（原子替换）：直接截断重写会让「恰好在重写中途读到
/// 配置文件」的宿主读到半截 JSON（解析失败 → 宿主误判桌面端没起）。rename 在
/// 同一文件系统内是原子的 → 读者只会看到「完整的旧内容」或「完整的新内容」。
pub fn write_secret_file(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        // 建目录 + 收紧权限（0700）都在 set_dir_mode 里（新建时才收紧）。
        set_dir_mode(parent)?;
    }
    // 临时名带 pid：同目录内唯一，避免并发写互相覆盖。
    let tmp = path.with_extension(format!(
        "tmp-{}",
        std::process::id()
    ));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("打开文件失败 ({}): {e}", tmp.display()))?;
        f.write_all(contents.as_bytes())
            .map_err(|e| format!("写入失败 ({}): {e}", tmp.display()))?;
        f.sync_all().ok();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置权限失败 ({}): {e}", tmp.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(&tmp, contents).map_err(|e| format!("写入失败 ({}): {e}", tmp.display()))?;
    }
    // Windows 的 rename 不覆盖已存在文件 → 先删旧的再改名（非原子，但 Windows
    // 上本任务只需「能用」；Unix 走上面的原子路径）。
    #[cfg(target_os = "windows")]
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("替换文件失败 ({}): {e}", path.display())
    })?;
    // rename 后再复核目标权限（保险：目标可能是旧文件，rename 覆盖后权限随之替换）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 配置目录权限 0700（目录里放着令牌文件）。已存在的目录**不改**权限（可能是
/// 与其它产物共用的应用数据目录），只在**本次新建**时收紧。
fn set_dir_mode(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let is_new = !dir.exists();
        if is_new {
            fs::create_dir_all(dir).map_err(|e| format!("创建目录失败 ({}): {e}", dir.display()))?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("设置目录权限失败 ({}): {e}", dir.display()))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// 校验插件临时截图**文件**（体积 + 真实像素数），返回 PNG 的宽高。
///
/// 两道闸都在**解码之前**：
///   ① `metadata().len()` ≤ [`MAX_CAPTURE_BYTES`]（64 MiB）——防「超大文件被整个
///      读进内存」；
///   ② 读 PNG 的 IHDR（头 24 字节）拿真实宽高，宽高积 ≤ [`MAX_CAPTURE_PIXELS`]
///      （40 MP）——防「解码时的内存尖峰」。
///
/// 顺带确认「这确实是 PNG」（签名 + IHDR）——不是 PNG 的文件直接拒。
pub fn validate_capture_file(path: &Path) -> Result<(u32, u32), String> {
    let meta = fs::metadata(path).map_err(|e| format!("读取文件属性失败: {e}"))?;
    if meta.len() > MAX_CAPTURE_BYTES {
        return Err(format!(
            "文件过大（{} 字节 > {} 上限）",
            meta.len(),
            MAX_CAPTURE_BYTES
        ));
    }
    let mut head = [0u8; 24];
    {
        use std::io::Read;
        let mut f = fs::File::open(path).map_err(|e| format!("打开文件失败: {e}"))?;
        f.read_exact(&mut head)
            .map_err(|_| "文件过短（不是有效的 PNG）".to_string())?;
    }
    const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if head[..8] != PNG_SIG || &head[12..16] != b"IHDR" {
        return Err("不是有效的 PNG 文件".to_string());
    }
    let w = u32::from_be_bytes([head[16], head[17], head[18], head[19]]);
    let h = u32::from_be_bytes([head[20], head[21], head[22], head[23]]);
    if w == 0 || h == 0 {
        return Err("图片尺寸非法".to_string());
    }
    if (w as u64) * (h as u64) > MAX_CAPTURE_PIXELS {
        return Err(format!(
            "图片像素数过大（{w}×{h} > {} 上限）",
            MAX_CAPTURE_PIXELS
        ));
    }
    Ok((w, h))
}

/// 生成一次性令牌（32 位十六进制 = 128 bit）。
///
/// 首选 `/dev/urandom`（Unix）；非 Unix 或读取失败时退回「进程内随机 + 时间 +
/// pid」混合（熵弱于前者，但令牌文件本身是 0600 + 仅本机可读，威胁模型是
/// 「同机其他用户 / 其他进程」，此退守口径可接受；失败时 eprintln 由调用方补）。
pub fn random_token() -> String {
    match random_bytes(16) {
        Some(bytes) => bytes.iter().map(|b| format!("{b:02x}")).collect(),
        None => fallback_token(),
    }
}

fn random_bytes(n: usize) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::io::Read;
        let mut f = fs::File::open("/dev/urandom").ok()?;
        let mut buf = vec![0u8; n];
        f.read_exact(&mut buf).ok()?;
        Some(buf)
    }
    #[cfg(not(unix))]
    {
        let _ = n;
        None
    }
}

/// 退守令牌：`RandomState`（进程内由 OS 播种）+ 纳秒时间 + pid 混合。
fn fallback_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u64(std::process::id() as u64);
    h.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
    );
    let a = h.finish();
    let b = h.finish();
    format!("{a:016x}{b:016x}")
}

/// 常量时间比较（长度不同直接 false——长度本身不是秘密）。
pub fn tokens_equal(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_capture_frame() {
        let f = parse_frame(
            r#"{"cmd":"capture","id":"abc-123","path":"/tmp/hawksnap-plugin-abc-123.png","width":1920,"height":1080}"#,
        )
        .expect("应解析成功");
        assert_eq!(f.cmd, "capture");
        assert_eq!(f.id, "abc-123");
        assert_eq!(f.width, 1920);
        assert_eq!(f.height, 1080);
        assert!(f.token.is_none(), "插件帧不带令牌");
        assert!(f.data.is_none(), "插件 capture 帧不带 data");
    }

    #[test]
    fn rejects_malformed_json() {
        let e = parse_frame("{not json").unwrap_err();
        assert!(e.contains("帧解析失败"), "实际: {e}");
    }

    #[test]
    fn rejects_empty_frame() {
        assert!(parse_frame("   ").is_err());
    }

    #[test]
    fn rejects_unknown_command() {
        let e = parse_frame(r#"{"cmd":"exec","id":"x","path":"/tmp/a.png"}"#).unwrap_err();
        assert!(e.contains("不支持的命令"), "实际: {e}");
    }

    #[test]
    fn rejects_frame_without_id() {
        let e = parse_frame(r#"{"cmd":"capture","id":"","path":"/tmp/a.png"}"#).unwrap_err();
        assert!(e.contains("id"), "实际: {e}");
    }

    #[test]
    fn rejects_capture_without_path() {
        let e = parse_frame(r#"{"cmd":"capture","id":"x"}"#).unwrap_err();
        assert!(e.contains("path"), "实际: {e}");
    }

    #[test]
    fn reply_serializes_camel_case_contract() {
        let ok = serde_json::to_string(&Reply::ok("i-1", "/pics/browser 2026-10-27 101010.png")).unwrap();
        assert_eq!(ok, r#"{"id":"i-1","ok":true,"savedPath":"/pics/browser 2026-10-27 101010.png"}"#);
        let err = serde_json::to_string(&Reply::err("i-2", "路径不在系统临时目录内")).unwrap();
        assert_eq!(err, r#"{"id":"i-2","ok":false,"error":"路径不在系统临时目录内"}"#);
    }

    #[test]
    fn tokens_equal_is_exact() {
        assert!(tokens_equal("abc", "abc"));
        assert!(!tokens_equal("abc", "abd"));
        assert!(!tokens_equal("abc", "ab"));
        assert!(!tokens_equal("", "x"));
    }

    #[test]
    fn random_token_is_hex_and_unique() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    // —— 文件闸（体积 / 像素数）——

    /// 造一个「头信息齐全」的 PNG 头（够 validate_capture_file 读 IHDR 即可）。
    fn png_head(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        v.extend_from_slice(&13u32.to_be_bytes()); // IHDR 长度
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]); // 位深/颜色类型/…
        v
    }

    #[test]
    fn capture_file_reads_real_dimensions() {
        let dir = std::env::temp_dir().join(format!("hawksnap-np-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("hawksnap-plugin-dim.png");
        std::fs::write(&p, png_head(1920, 1080)).unwrap();
        let (w, h) = validate_capture_file(&p).unwrap();
        assert_eq!((w, h), (1920, 1080));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_file_rejects_oversized_pixels() {
        let dir = std::env::temp_dir().join(format!("hawksnap-np-px-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("hawksnap-plugin-huge.png");
        // 100000×100000 = 10^10 像素 → 超 40 MP 上限
        std::fs::write(&p, png_head(100_000, 100_000)).unwrap();
        let e = validate_capture_file(&p).unwrap_err();
        assert!(e.contains("像素数过大"), "实际: {e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_file_rejects_non_png() {
        let dir = std::env::temp_dir().join(format!("hawksnap-np-np-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("hawksnap-plugin-bad.png");
        std::fs::write(&p, b"not a png at all, just text").unwrap();
        let e = validate_capture_file(&p).unwrap_err();
        assert!(e.contains("PNG"), "实际: {e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
