/**
 * Offscreen document：画布裁剪（MV3 service worker 无 DOM/canvas）。
 *
 * 只做一件事：收到 SW 的 'hawksnap:offscreen-crop'（dataUrl + 目标矩形，
 * 设备像素），解码 → drawImage 取区 → toDataURL('image/png') → 回传。
 * 不做标注/合成（那是编辑器页的事），保持单一职责，离开即释放。
 *
 * 裁剪矩形越界时夹到图像边界（drawImage 对超界源矩形会静默输出空图）。
 */

type CropRequest = {
  type: 'hawksnap:offscreen-crop';
  dataUrl: string;
  sx: number;
  sy: number;
  sw: number;
  sh: number;
};

function clamp(v: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, v));
}

async function cropToDataUrl(req: CropRequest): Promise<{ ok: boolean; dataUrl?: string; error?: string }> {
  const img = new Image();
  await new Promise<void>((resolve, reject) => {
    img.onload = () => resolve();
    img.onerror = () => reject(new Error('可见区截图解码失败'));
    img.src = req.dataUrl;
  });
  if (!img.naturalWidth || !img.naturalHeight) {
    return { ok: false, error: '可见区截图尺寸为 0' };
  }

  const sx = clamp(Math.round(req.sx), 0, img.naturalWidth - 1);
  const sy = clamp(Math.round(req.sy), 0, img.naturalHeight - 1);
  const sw = clamp(Math.round(req.sw), 1, img.naturalWidth - sx);
  const sh = clamp(Math.round(req.sh), 1, img.naturalHeight - sy);

  const canvas = document.createElement('canvas');
  canvas.width = sw;
  canvas.height = sh;
  const ctx = canvas.getContext('2d');
  if (!ctx) return { ok: false, error: '无法创建 canvas 上下文' };
  ctx.drawImage(img, sx, sy, sw, sh, 0, 0, sw, sh);
  return { ok: true, dataUrl: canvas.toDataURL('image/png') };
}

chrome.runtime.onMessage.addListener((msg: unknown, _sender, sendResponse) => {
  const m = msg as CropRequest | null;
  if (!m || m.type !== 'hawksnap:offscreen-crop') return; // 不参与其它消息
  void (async () => {
    try {
      sendResponse(await cropToDataUrl(m));
    } catch (err) {
      sendResponse({ ok: false, error: err instanceof Error ? err.message : String(err) });
    }
  })();
  return true; // 异步 sendResponse
});
