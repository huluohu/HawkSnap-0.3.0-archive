/**
 * Popup：三种捕获模式的选择入口。
 *
 * 只发一条 'hawksnap:capture' 给 service worker（捕获逻辑全在 SW），显式带上
 * 当前活动标签的 tabId/windowId —— 弹层关闭瞬间焦点可能未回归，依赖
 * lastFocusedWindow 有竞态（SW 侧同样有兜底）。
 */

type CaptureMode = 'full' | 'visible' | 'region';

async function currentTab(): Promise<{ tabId?: number; windowId?: number }> {
  try {
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    return { tabId: tab?.id, windowId: tab?.windowId };
  } catch {
    return {};
  }
}

async function trigger(mode: CaptureMode): Promise<void> {
  const { tabId, windowId } = await currentTab();
  // 回调形式：popup 即将关闭，不等 Promise。
  chrome.runtime.sendMessage({ type: 'hawksnap:capture', mode, tabId, windowId }, () => {
    void chrome.runtime.lastError;
  });
  window.close();
}

function bind(id: string, mode: CaptureMode): void {
  document.getElementById(id)?.addEventListener('click', () => {
    void trigger(mode);
  });
}

bind('full', 'full');
bind('visible', 'visible');
bind('region', 'region');
