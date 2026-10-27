/**
 * Native Messaging 客户端（插件侧）。
 *
 * 每次发送操作新建一条 connectNative 连接（Chrome 为每条连接拉起一个新的
 * 宿主进程）——天然实现协议 §2.2 的「onDisconnect → 重连即可」：宿主 5 分钟
 * 静默退出 / 被杀，下一次操作都是全新连接，不存在陈旧端口。
 *
 * 发送链路（协议 §3.1 / §4 推荐链路）：
 *   save（base64 PNG）→ 回执 path → capture（path + 真实像素）→ 回执 savedPath。
 *
 * 失败语义（调用点据此降级到本地下载）：
 *   - too-large：save 载荷超浏览器单消息 4 MB（发送前预检，不发起连接）；
 *   - host-missing：onDisconnect 且从未收到任何消息（宿主未注册/未安装）；
 *   - timeout：REPLY_TIMEOUT_MS 内无回执（对齐宿主↔桌面端 30s 超时）；
 *   - error：宿主回执 ok=false（含桌面端拒收的中文原因，直接透传）。
 */

import {
  buildCaptureFrame,
  buildSaveFrame,
  bytesToBase64,
  CHROME_MAX_MESSAGE_BYTES,
  HOST_NAME,
  MESSAGE_OVERHEAD_BYTES,
  parseReply,
  REPLY_TIMEOUT_MS,
  type HostReply,
} from './protocol.js';

export type SendToDesktopResult =
  | { status: 'saved'; savedPath: string }
  | { status: 'too-large'; message: string }
  | { status: 'host-missing'; message: string }
  | { status: 'error'; message: string };

export type SendToDesktopInput = {
  /** PNG 字节。 */
  pngBytes: Uint8Array;
  /** 真实像素宽（§2.1：真实像素或 0）。 */
  width: number;
  /** 真实像素高。 */
  height: number;
  /** id 种子（宿主只保留 [A-Za-z0-9_-]；不传用默认前缀）。 */
  idSeed?: string;
};

type Pending = {
  resolve: (reply: HostReply) => void;
  timer: ReturnType<typeof setTimeout>;
};

/**
 * 底层连接封装：发帧、按 id 配对回执、超时与断连。
 * 内部用；对外入口是 sendToDesktop() / probeDesktop()。
 */
class NativeConnection {
  private port: chrome.runtime.Port | null = null;
  private pending = new Map<string, Pending>();

  private ensurePort(): chrome.runtime.Port {
    if (this.port) return this.port;
    const port = chrome.runtime.connectNative(HOST_NAME);
    let receivedAny = false;
    port.onMessage.addListener((msg: unknown) => {
      receivedAny = true;
      const reply = parseReply(msg);
      const p = this.pending.get(reply.id);
      if (p) {
        clearTimeout(p.timer);
        this.pending.delete(reply.id);
        p.resolve(reply);
      }
      // 无法按 id 配对的回执：契约只保证 id 配对，静默丢弃。
    });
    port.onDisconnect.addListener(() => {
      // 从未收到任何消息的断连 → 宿主没能起来（未注册 / 二进制缺失）。
      // lastError 在此回调内可读（如 "Specified native messaging host not found"），
      // 读掉它也避免 Chrome 记Unchecked runtime.lastError 控制台噪音。
      const lastErr = chrome.runtime.lastError?.message;
      const message = lastErr
        ? `无法连接 Native Messaging 宿主：${lastErr}`
        : receivedAny
          ? '与宿主的连接中断（宿主可能超时退出），请重试'
          : '无法启动 Native Messaging 宿主（未注册或未安装）';
      for (const [, p] of this.pending) {
        clearTimeout(p.timer);
        p.resolve({ kind: 'error', id: '', error: message });
      }
      this.pending.clear();
      this.port = null;
    });
    this.port = port;
    return port;
  }

  /** 发一帧并等待同 id 回执。永不 reject。 */
  request(frame: { id: string } & Record<string, unknown>): Promise<HostReply> {
    const port = this.ensurePort();
    return new Promise<HostReply>((resolve) => {
      const timer = setTimeout(() => {
        this.pending.delete(frame.id);
        resolve({
          kind: 'error',
          id: frame.id,
          error: `等待宿主回执超时（${Math.round(REPLY_TIMEOUT_MS / 1000)}s）`,
        });
      }, REPLY_TIMEOUT_MS);
      this.pending.set(frame.id, { resolve, timer });
      try {
        port.postMessage(frame);
      } catch (err) {
        // 同步抛错（最典型：消息超过浏览器 4 MB 单消息上限）。
        clearTimeout(timer);
        this.pending.delete(frame.id);
        this.port = null;
        port.disconnect();
        resolve({
          kind: 'error',
          id: frame.id,
          error: err instanceof Error ? err.message : '发送帧失败',
        });
      }
    });
  }

  close(): void {
    for (const [, p] of this.pending) clearTimeout(p.timer);
    this.pending.clear();
    this.port?.disconnect();
    this.port = null;
  }
}

/** save 载荷是否在浏览器单消息上限内（含帧包装开销的保守余量）。 */
export function savePayloadFits(base64Length: number): boolean {
  return base64Length + MESSAGE_OVERHEAD_BYTES <= CHROME_MAX_MESSAGE_BYTES;
}

const TOO_LARGE_MESSAGE =
  '截图超过浏览器→宿主单消息 4 MB 上限（协议分片本版未实现），已改为下载到本地';

/** 1×1 透明 PNG（70 字节）——探测帧载荷（tests/protocol.test.ts 校验其头部合法）。 */
export const PROBE_PNG_BASE64 =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==';

/**
 * 一次「发送到桌面端」完整链路。永不 reject——结果以判别联合返回，
 * 调用点按 status 分支处理（成功展示路径，其余降级本地下载）。
 */
export async function sendToDesktop(input: SendToDesktopInput): Promise<SendToDesktopResult> {
  const conn = new NativeConnection();
  const idBase = `${input.idSeed ?? 'shot'}-${Date.now().toString(36)}`.replace(
    /[^A-Za-z0-9_-]/g,
    '',
  );
  try {
    // 预检浏览器侧 4 MB（§4）——超限直接降级，不发起连接。
    const base64 = bytesToBase64(input.pngBytes);
    if (!savePayloadFits(base64.length)) {
      return { status: 'too-large', message: TOO_LARGE_MESSAGE };
    }

    const saveReply = await conn.request(buildSaveFrame(`${idBase}-s`, base64));
    if (saveReply.kind !== 'save-ok') {
      const error = saveReply.kind === 'error' ? saveReply.error : 'save 回执缺少 path';
      return { status: 'host-missing', message: error };
    }

    // capture 帧：path 必须是 save 回执给的真实临时路径；宽高填真实像素。
    const captureFrame = buildCaptureFrame(
      `${idBase}-c`,
      saveReply.path,
      input.width,
      input.height,
    );
    const captureReply = await conn.request(captureFrame);
    if (captureReply.kind === 'capture-ok') {
      return { status: 'saved', savedPath: captureReply.savedPath };
    }
    const error =
      captureReply.kind === 'error' ? captureReply.error : 'capture 回执缺少 savedPath';
    return { status: 'error', message: error };
  } catch (err) {
    // buildSaveFrame 抛错（载荷预检后被并发改动等边界）或 connectNative 本身抛错。
    const message = err instanceof Error ? err.message : String(err);
    return { status: 'error', message };
  } finally {
    conn.close();
  }
}

/**
 * 探测宿主可用性（options 页用）：发一个 1×1 PNG 的 save 帧。
 * save-ok → 宿主链路正常（宿主进程能拉起、能落盘）；其余 → 返回原因。
 * save 只落临时目录（§4），不进桌面端历史——探测不会污染用户数据。
 */
export async function probeDesktop(): Promise<{ ok: boolean; message: string }> {
  const conn = new NativeConnection();
  try {
    const reply = await conn.request(
      buildSaveFrame(`probe${Date.now().toString(36)}`, PROBE_PNG_BASE64),
    );
    if (reply.kind === 'save-ok') return { ok: true, message: '宿主链路正常（探测帧已落盘）' };
    const error = reply.kind === 'error' ? reply.error : 'save 回执缺少 path';
    return { ok: false, message: error };
  } catch (err) {
    return { ok: false, message: err instanceof Error ? err.message : String(err) };
  } finally {
    conn.close();
  }
}
