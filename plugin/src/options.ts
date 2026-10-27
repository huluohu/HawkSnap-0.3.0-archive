/**
 * Options 页：扩展 ID 展示（注册宿主 manifest 的 allowed_origins 需要）+
 * 桌面端宿主链路探测（probeDesktop：save 1×1 帧，只落临时目录不进历史）。
 */

import { HOST_NAME } from './shared/protocol.js';
import { probeDesktop } from './shared/native-client.js';

const extId = document.getElementById('extId') as HTMLElement | null;
const hostName = document.getElementById('hostName') as HTMLElement | null;
const probeResult = document.getElementById('probeResult') as HTMLElement | null;
const probeBtn = document.getElementById('probe') as HTMLButtonElement | null;

if (extId) {
  extId.textContent = `chrome-extension://${chrome.runtime.id}/`;
}
if (hostName) {
  hostName.textContent = HOST_NAME; // connectNative 的参数（shared/protocol），页面展示同一常量
}

document.getElementById('copyId')?.addEventListener('click', () => {
  void navigator.clipboard
    .writeText(`chrome-extension://${chrome.runtime.id}/`)
    .then(() => {
      if (probeResult) {
        probeResult.textContent = '已复制';
        probeResult.className = 'ok';
      }
    })
    .catch(() => undefined);
});

probeBtn?.addEventListener('click', () => {
  if (!probeResult || !probeBtn) return;
  probeBtn.disabled = true;
  probeResult.textContent = '检测中…';
  probeResult.className = 'muted';
  void probeDesktop().then((r) => {
    probeResult.textContent = r.message;
    probeResult.className = r.ok ? 'ok' : 'bad';
    probeBtn.disabled = false;
  });
});
