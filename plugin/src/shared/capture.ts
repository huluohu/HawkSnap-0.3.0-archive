/**
 * 捕获载荷类型（SW ↔ 编辑器 的交接契约）。
 *
 * 仅类型（import type，构建时擦除）——service worker 与 editor 页各自打包，
 * 不能从 service-worker.ts 导值（那会把 SW 的 chrome.* 注册逻辑带进编辑器）。
 */

export type CaptureMode = 'full' | 'visible' | 'region';

export type CapturePayload = {
  captureId: string;
  /** PNG 的 base64（无 data: 前缀）；空串 = 捕获失败，note 携带原因。 */
  pngBase64: string;
  /** 真实像素（协议 §2.1：capture 帧也用这对值；0 = 未知）。 */
  width: number;
  height: number;
  source: CaptureMode;
  /** 状态提示（降采样/裁剪/失败原因），编辑器状态栏展示。 */
  note?: string;
};
