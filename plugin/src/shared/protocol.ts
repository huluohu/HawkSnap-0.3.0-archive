/**
 * Native Messaging 协议（P2 插件侧实现）。
 *
 * 契约来源：docs/superpowers/protocols/native-messaging.md（冻结），
 * 桌面端实现：app/src-tauri/src/native_protocol.rs + app/src-tauri/src/bin/hawksnap-host.rs。
 * 字段名 / 类型 / 语义必须逐字对齐该文档——本模块是唯一出帧、收帧的地方。
 *
 * 链路（§1.1 / §3.1 推荐链路）：
 *   1. 插件 → 宿主：{"cmd":"save","id":"<uuid>","data":"<base64 PNG，无 data: 前缀>"}
 *      宿主落盘到 <tmp>/hawksnap-native-inbox/hawksnap-plugin-<id>.png
 *      回执：{"id":"...","ok":true,"path":"<临时文件路径>"}
 *   2. 插件 → 宿主：{"cmd":"capture","id":"<uuid>","path":"<上一步的 path>","width":N,"height":N}
 *      桌面端落盘 save_dir + 历史入库 kind="browser"
 *      回执：{"id":"...","ok":true,"savedPath":"<桌面端落盘路径>"}
 *
 * 失败回执：{"id":"...","ok":false,"error":"<中文原因>"}（ok=true 不含 error，
 * capture 成功不含 path、只有 savedPath；save 成功只有 path——见 §2.1/§4）。
 */

/** 宿主名（§1，浏览器 manifest 的 name，connectNative 的参数）。 */
export const HOST_NAME = 'com.hawksnap.app';

/**
 * Chrome 浏览器 → 宿主 的单消息上限（§4）：4 MB。
 * 宿主侧自身的单帧上限是 8 MiB（base64 后），但浏览器侧的 4 MB 更紧，
 * 插件按更紧的一道闸预检——超限直接降级本地下载，不发起连接。
 */
export const CHROME_MAX_MESSAGE_BYTES = 4 * 1024 * 1024;

/** 出帧 JSON 包装开销的保守余量（键名 + 长度前缀等），预检时从上限里扣掉。 */
export const MESSAGE_OVERHEAD_BYTES = 256;

/** 桌面端单帧读写超时 15s / 宿主↔桌面端 30s（§6）——插件等待回执取更宽的 30s。 */
export const REPLY_TIMEOUT_MS = 30_000;

/** 宿主 5 分钟静默即退出（§2.2）——插件每次发送都新建连接，天然规避陈旧端口。 */

/** capture 帧的 path 约束（§3）：文件名前缀。 */
export const PLUGIN_FILE_PREFIX = 'hawksnap-plugin-';

/** 帧命令（§2.1 / §4）。 */
export type CaptureFrame = {
  cmd: 'capture';
  id: string;
  path: string;
  width: number;
  height: number;
};

export type SaveFrame = {
  cmd: 'save';
  id: string;
  data: string;
};

/** 宿主回执的判别联合（§2.1 / §4：capture 成功带 savedPath，save 成功带 path）。 */
export type HostReply =
  | { kind: 'capture-ok'; id: string; savedPath: string }
  | { kind: 'save-ok'; id: string; path: string }
  | { kind: 'error'; id: string; error: string };

/** 抛错 = 调用点代码有 bug（帧构造不合法）；协议失败走 ok:false 回执。 */

/** 校验请求 id：非空字符串（宿主对空 id 拒收）。 */
export function assertValidId(id: string): void {
  if (typeof id !== 'string' || id.trim().length === 0) {
    throw new Error('帧缺少 id');
  }
}

/**
 * 校验 capture 帧的 path（§3 的插件侧预检，宿主/桌面端还会各校验一次）：
 * 绝对路径（POSIX `/…` 或 Windows 盘符 `C:\…`/`C:/…`）、.png 扩展名、
 * hawksnap-plugin-* 文件名。path 来自宿主 save 回执，插件预检不应比宿主更严。
 */
export function validateCapturePath(path: string): void {
  if (typeof path !== 'string' || path.length === 0) {
    throw new Error('帧缺少 path');
  }
  const isAbsolute = path.startsWith('/') || /^[A-Za-z]:[\\/]/.test(path);
  if (!isAbsolute) {
    throw new Error('路径必须是绝对路径');
  }
  if (!path.toLowerCase().endsWith('.png')) {
    throw new Error('扩展名必须是 .png');
  }
  const base = path.slice(Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\')) + 1);
  if (!base.startsWith(PLUGIN_FILE_PREFIX)) {
    throw new Error(`文件名必须形如 ${PLUGIN_FILE_PREFIX}*.png`);
  }
}

/**
 * 校验 width/height（§2.1）：**真实像素或 0**（0 = 未知，桌面端以 PNG 实际像素为准）。
 * 填 CSS 像素或估算值、与真实值差超 4 倍会被桌面端判「帧在撒谎」拒收；
 * 非负整数（Rust 侧 u32）。
 */
export function assertValidDimensions(width: number, height: number): void {
  for (const [name, v] of [
    ['width', width],
    ['height', height],
  ] as const) {
    if (!Number.isInteger(v) || v < 0) {
      throw new Error(`${name} 必须是非负整数（真实像素或 0）`);
    }
  }
}

/** 构造 `save` 帧（§4）：data 是**不含 data: 前缀**的 base64 PNG。 */
export function buildSaveFrame(id: string, base64Png: string): SaveFrame {
  assertValidId(id);
  if (typeof base64Png !== 'string' || base64Png.length === 0) {
    throw new Error('save 帧缺少 data');
  }
  const payloadBytes = base64Png.length + MESSAGE_OVERHEAD_BYTES;
  if (payloadBytes > CHROME_MAX_MESSAGE_BYTES) {
    // 浏览器侧单消息 4 MB 上限（§4）——超限帧 Chrome 会直接拒发/断连，
    // 必须在发送前拦截并降级（调用点负责降级到本地下载）。
    throw new Error(
      `save 载荷超出浏览器单消息上限（${payloadBytes} > ${CHROME_MAX_MESSAGE_BYTES} 字节）`,
    );
  }
  return { cmd: 'save', id, data: base64Png };
}

/** 构造 `capture` 帧（§2.1）。width/height 传真实像素，拿不到就传 0。 */
export function buildCaptureFrame(
  id: string,
  path: string,
  width: number,
  height: number,
): CaptureFrame {
  assertValidId(id);
  validateCapturePath(path);
  assertValidDimensions(width, height);
  return { cmd: 'capture', id, path, width, height };
}

/**
 * 解析宿主回执（§2.1 / §4）。按出现字段判别：
 *   ok=true + savedPath → capture-ok；ok=true + path → save-ok；ok=false + error → error。
 * 字段类型不符 / 两种成功字段同时出现 / 都不出现 → error（携带人类可读原因）。
 * 解析失败时 id 尽力保留（契约：帧无法解析时回执 id 为 ""，这里镜像该行为）。
 */
export function parseReply(raw: unknown): HostReply {
  let obj: Record<string, unknown> | null = null;
  try {
    obj =
      typeof raw === 'string'
        ? (JSON.parse(raw) as Record<string, unknown>)
        : raw !== null && typeof raw === 'object'
          ? (raw as Record<string, unknown>)
          : null;
  } catch {
    obj = null;
  }
  const id = typeof obj?.id === 'string' ? obj.id : '';
  if (!obj || typeof obj.ok !== 'boolean') {
    return { kind: 'error', id, error: '回执格式非法（缺少 ok 布尔字段）' };
  }
  if (!obj.ok) {
    const error = typeof obj.error === 'string' && obj.error.length > 0 ? obj.error : '未知错误';
    return { kind: 'error', id, error };
  }
  const savedPath = obj.savedPath;
  const path = obj.path;
  const savedPathOk = typeof savedPath === 'string' && savedPath.length > 0;
  const pathOk = typeof path === 'string' && path.length > 0;
  if (savedPathOk && !pathOk) {
    return { kind: 'capture-ok', id, savedPath: savedPath as string };
  }
  if (pathOk && !savedPathOk) {
    return { kind: 'save-ok', id, path: path as string };
  }
  return {
    kind: 'error',
    id,
    error: savedPathOk
      ? '回执字段冲突（同时出现 savedPath 与 path）'
      : '成功回执缺少路径字段（savedPath 或 path）',
  };
}

/**
 * 从 PNG 字节读 IHDR 真实宽高（§2.1 要求帧里填真实像素——生成侧直接量字节，
 * 与 canvas 尺寸互为印证；不是 PNG / 长度不足 → null（调用点改填 0））。
 */
export function readPngSize(bytes: Uint8Array): { width: number; height: number } | null {
  // 8 字节签名 + 4 长度 + 4 "IHDR" + 8 宽高
  const SIGNATURE = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
  if (bytes.length < 24) return null;
  for (let i = 0; i < 8; i++) {
    if (bytes[i] !== SIGNATURE[i]) return null;
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return { width: view.getUint32(16), height: view.getUint32(20) };
}

/**
 * 字节 → base64（无 data: 前缀）。分块转换，避免几 MB 的数组触发
 * Function.prototype.apply 的参数栈上限。btoa 在浏览器与 node ≥16 均可用。
 */
export function bytesToBase64(bytes: Uint8Array): string {
  let binary = '';
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    binary += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return btoa(binary);
}

/** 生成请求 id：RFC 4122 v4（crypto.getRandomValues，MV3 环境可用）。 */
export function uuid(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID();
  }
  const buf = new Uint8Array(16);
  crypto.getRandomValues(buf);
  buf[6] = (buf[6] & 0x0f) | 0x40;
  buf[8] = (buf[8] & 0x3f) | 0x80;
  const hex = [...buf].map((b) => b.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
