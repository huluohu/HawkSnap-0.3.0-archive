/**
 * MV3 service worker：捕获流程编排。
 *
 * 三种模式（popup 选择 / 快捷键触发 action）：
 *  - full    整页：chrome.debugger attach → 懒加载有界滚动 →
 *            Page.getLayoutMetrics 量尺寸 → computeCapturePolicy 定缩放 →
 *            Emulation.setDeviceMetricsOverride + Page.captureScreenshot
 *            (captureBeyondViewport) → detach（finally，保证 infobar 消失）。
 *  - visible 可见区：chrome.tabs.captureVisibleTab（activeTab 授权后无需 host 权限）。
 *  - region  区域：注入 content-script 画选区浮层 → 选完先隐藏浮层再
 *            captureVisibleTab → offscreen document 裁剪 → 编辑器。
 *
 * 数据交接给编辑器：captureId → chrome.storage.session（SW 可能被回收，
 * 内存不可靠；session 区域配额 10MB，超限时退回内存映射——编辑器在加载后
 * 立即拉取，此时 SW 刚完成捕获仍在存活窗口内）。
 */

import { computeCapturePolicy, imageSettleDecision, planLazyScroll } from './shared/size-policy.js';
import { uuid } from './shared/protocol.js';
import type { CaptureMode, CapturePayload } from './shared/capture.js';

export type { CaptureMode, CapturePayload };

/** 区域选择等待表：content script → SW 的选中回调（按 tabId）。 */
type RegionSelection = {
  rect: { x: number; y: number; width: number; height: number };
  devicePixelRatio: number;
};
const regionWaiters = new Map<number, ((sel: RegionSelection | null) => void) | undefined>();

/** 内存映射：storage.session 超配额时的兜底（SW 存活窗口内有效）。 */
const memoryCaptures = new Map<string, CapturePayload>();
/** 内存映射只保留最近几份，防止长会话内存增长。 */
const MEMORY_KEEP = 4;

async function storeCapture(payload: CapturePayload): Promise<void> {
  memoryCaptures.set(payload.captureId, payload);
  while (memoryCaptures.size > MEMORY_KEEP) {
    const oldest = memoryCaptures.keys().next().value;
    if (oldest === undefined) break;
    memoryCaptures.delete(oldest);
  }
  try {
    await chrome.storage.session.set({ [`cap:${payload.captureId}`]: payload });
  } catch (err) {
    // 超出 session 配额（整页大图）：内存兜底已记下，编辑器会立即拉取。
    console.warn('[hawksnap] storage.session 存放失败，走内存兜底', err);
  }
}

async function loadCapture(captureId: string): Promise<CapturePayload | null> {
  const mem = memoryCaptures.get(captureId);
  if (mem) return mem;
  const key = `cap:${captureId}`;
  const bag = await chrome.storage.session.get(key);
  const found = bag[key] as CapturePayload | undefined;
  return found ?? null;
}

// —— 徽标反馈（不引 notifications 权限，错误就地提示） ——

function flashBadge(tabId: number | undefined, text: string, ok: boolean): void {
  void chrome.action.setBadgeBackgroundColor({ color: ok ? '#2f9e44' : '#e03131', tabId });
  void chrome.action.setBadgeText({ text, tabId });
  setTimeout(() => {
    void chrome.action.setBadgeText({ text: '', tabId });
  }, 2500);
}

// —— 懒加载：有界滚动 + 有界等待（决策纯函数在 shared/size-policy） ——

/** 页内执行的表达式：滚到 y 并回报图片就绪度。 */
function scrollProbeExpression(y: number): string {
  return `(() => {
    window.scrollTo(0, ${y});
    const imgs = Array.from(document.images || []);
    const settled = imgs.filter((img) => img.complete).length;
    return { total: imgs.length, settled };
  })()`;
}

function statsProbeExpression(): string {
  return `(() => {
    const imgs = Array.from(document.images || []);
    const settled = imgs.filter((img) => img.complete).length;
    return { total: imgs.length, settled };
  })()`;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

type ImageStats = { total: number; settled: number };

async function probeStats(tabId: number, expression: string): Promise<ImageStats> {
  const res = (await chrome.debugger.sendCommand({ tabId }, 'Runtime.evaluate', {
    expression,
    returnByValue: true,
  })) as { result?: { value?: ImageStats } };
  return res.result?.value ?? { total: 0, settled: 0 };
}

/**
 * 懒加载遍历：逐步下滚（触发 loading=lazy），每步等图片就绪（有界），
 * 最后回顶。总预算 8s（LAZY_TOTAL_BUDGET_MS），超时放弃等待直接截图。
 */
async function runLazyLoadPass(
  tabId: number,
  pageHeightCss: number,
  viewportHeightCss: number,
): Promise<void> {
  const started = Date.now();
  const positions = planLazyScroll(pageHeightCss, viewportHeightCss);
  for (const y of positions) {
    if (Date.now() - started > 8_000) break;
    try {
      const stats = await probeStats(tabId, scrollProbeExpression(y));
      const stepStarted = Date.now();
      // 单步等待循环：wait → 稍候再查；continue → 下一步；give-up → 整体放弃。
      for (;;) {
        const decision = imageSettleDecision({
          totalImages: stats.total,
          settledImages: stats.settled,
          stepElapsedMs: Date.now() - stepStarted,
          totalElapsedMs: Date.now() - started,
        });
        if (decision !== 'wait') break;
        await sleep(100);
        const s2 = await probeStats(tabId, statsProbeExpression());
        stats.total = s2.total;
        stats.settled = s2.settled;
      }
    } catch (err) {
      // 页面上下文异常（iframe 策略等）不阻塞截图主流程。
      console.warn('[hawksnap] 懒加载探测失败，跳过等待', err);
      break;
    }
    await sleep(120); // LAZY_STEP_DELAY_MS：给新触发的请求一点启动时间
  }
  // 回顶（截图从顶部开始；触发过的图已在缓存里）。
  try {
    await chrome.debugger.sendCommand(
      { tabId },
      'Runtime.evaluate',
      { expression: 'window.scrollTo(0, 0)', returnByValue: true },
    );
  } catch {
    // 忽略
  }
  await sleep(250);
}

// —— 整页捕获 ——

async function captureFullPage(tab: chrome.tabs.Tab): Promise<CapturePayload> {
  const tabId = tab.id as number;
  const target = { tabId };
  await chrome.debugger.attach(target, '1.3');
  let note: string | undefined;
  try {
    await chrome.debugger.sendCommand(target, 'Page.enable');
    await chrome.debugger.sendCommand(target, 'Runtime.enable');

    const metrics = (await chrome.debugger.sendCommand(target, 'Page.getLayoutMetrics')) as {
      cssContentSize?: { width: number; height: number };
      contentSize?: { width: number; height: number };
      cssVisualViewport?: { clientWidth: number; clientHeight: number };
    };
    const content = metrics.cssContentSize ?? metrics.contentSize;
    if (!content || content.width <= 0 || content.height <= 0) {
      throw new Error('无法读取页面内容尺寸');
    }
    const vp = metrics.cssVisualViewport;
    const dprEval = (await chrome.debugger.sendCommand(target, 'Runtime.evaluate', {
      expression: 'window.devicePixelRatio',
      returnByValue: true,
    })) as { result?: { value?: number } };
    const dpr = dprEval.result?.value && dprEval.result.value > 0 ? dprEval.result.value : 1;

    await runLazyLoadPass(tabId, content.height, vp?.clientHeight ?? 600);

    const policy = computeCapturePolicy({
      cssWidth: content.width,
      cssHeight: content.height,
      devicePixelRatio: dpr,
    });
    if (policy.cropped) {
      note = `页面过大：已降采样并裁剪到安全上限（输出 ${policy.outWidth}×${policy.outHeight}）`;
    } else if (policy.downscaled) {
      note = `页面超出整页保真上限：已整体降采样至 ${policy.outWidth}×${policy.outHeight}`;
    }

    await chrome.debugger.sendCommand(target, 'Emulation.setDeviceMetricsOverride', {
      width: Math.round(policy.captureCssWidth),
      height: Math.round(policy.captureCssHeight),
      deviceScaleFactor: policy.deviceScaleFactor,
      mobile: false,
    });
    const shot = (await chrome.debugger.sendCommand(target, 'Page.captureScreenshot', {
      format: 'png',
      captureBeyondViewport: true,
      optimizeForSpeed: true,
    })) as { data?: string };
    await chrome.debugger
      .sendCommand(target, 'Emulation.clearDeviceMetricsOverride')
      .catch(() => undefined);
    if (!shot.data) throw new Error('captureScreenshot 未返回数据');

    return {
      captureId: uuid(),
      pngBase64: shot.data,
      width: policy.outWidth,
      height: policy.outHeight,
      source: 'full',
      note,
    };
  } finally {
    await chrome.debugger.detach(target).catch(() => undefined);
  }
}

// —— 可见区 / 区域捕获 ——

async function captureVisible(windowId: number | undefined): Promise<string> {
  return chrome.tabs.captureVisibleTab(windowId, { format: 'png' });
}

/** dataUrl → 纯 base64（去掉 data:image/png;base64, 前缀）。 */
function stripDataUrl(dataUrl: string): string {
  const idx = dataUrl.indexOf(',');
  return idx >= 0 ? dataUrl.slice(idx + 1) : dataUrl;
}

/** 请 offscreen document 做画布裁剪（SW 无 DOM）。dataUrl 进、dataUrl 出。 */
function offscreenCropOnce(
  dataUrl: string,
  sx: number,
  sy: number,
  sw: number,
  sh: number,
): Promise<{ ok: boolean; dataUrl?: string; error?: string }> {
  return new Promise((resolve) => {
    chrome.runtime.sendMessage(
      { type: 'hawksnap:offscreen-crop', dataUrl, sx, sy, sw, sh },
      (res: unknown) => {
        if (chrome.runtime.lastError) {
          resolve({ ok: false, error: chrome.runtime.lastError.message });
          return;
        }
        const r = res as { ok: boolean; dataUrl?: string; error?: string } | undefined;
        resolve(r ?? { ok: false, error: 'offscreen 无响应' });
      },
    );
  });
}

/**
 * createDocument 的 promise 在文档创建时即 resolve，offscreen.js 的消息监听器
 * 可能尚未注册 → 首发消息报 "Receiving end does not exist"。对这类瞬态错误
 * 有界重试（3 次 × 150ms），真实裁剪错误原样返回不重试。
 */
async function offscreenCrop(
  dataUrl: string,
  sx: number,
  sy: number,
  sw: number,
  sh: number,
): Promise<{ ok: boolean; dataUrl?: string; error?: string }> {
  const TRANSIENT = /receiving end does not exist|message port closed/i;
  let last = { ok: false, error: 'offscreen 无响应' } as { ok: boolean; dataUrl?: string; error?: string };
  for (let attempt = 0; attempt < 3; attempt++) {
    last = await offscreenCropOnce(dataUrl, sx, sy, sw, sh);
    if (last.ok || !TRANSIENT.test(last.error ?? '')) return last;
    await sleep(150);
  }
  return last;
}

async function ensureOffscreen(): Promise<void> {
  const contexts = await chrome.runtime.getContexts({ contextType: 'OFFSCREEN_DOCUMENT' });
  if (contexts.length > 0) return;
  await chrome.offscreen.createDocument({
    url: 'offscreen.html',
    reasons: ['DOM_SCRAPING'],
    justification: '用 canvas 对可见区截图做区域裁剪（SW 无 DOM/canvas）',
  });
}

function captureRegion(tab: chrome.tabs.Tab): Promise<CapturePayload> {
  const tabId = tab.id as number;
  return (async () => {
    const dataUrl = await captureVisible(tab.windowId);
    // 选区 rect 是 CSS px（content script 上报），截图是设备像素 → 按 dpr 放大。
    // devicePixelRatio 由 content script 一并上报（含浏览器缩放，与截图一致）。
    return new Promise<CapturePayload>((resolve, reject) => {
      regionWaiters.set(tabId, (sel) => {
        void (async () => {
          try {
            if (!sel) {
              reject(new Error('已取消区域选择'));
              return;
            }
            await ensureOffscreen();
            const dpr = sel.devicePixelRatio > 0 ? sel.devicePixelRatio : 1;
            const sx = Math.max(0, Math.round(sel.rect.x * dpr));
            const sy = Math.max(0, Math.round(sel.rect.y * dpr));
            const sw = Math.max(1, Math.round(sel.rect.width * dpr));
            const sh = Math.max(1, Math.round(sel.rect.height * dpr));
            const cropped = await offscreenCrop(dataUrl, sx, sy, sw, sh);
            if (!cropped.ok || !cropped.dataUrl) {
              throw new Error(cropped.error ?? '裁剪失败');
            }
            resolve({
              captureId: uuid(),
              pngBase64: stripDataUrl(cropped.dataUrl),
              width: sw,
              height: sh,
              source: 'region',
            });
          } catch (err) {
            reject(err instanceof Error ? err : new Error(String(err)));
          }
        })();
      });
      // 注入选区浮层（浮层出现 → 用户拖拽 → mouseup 先隐藏浮层再回报）。
      chrome.scripting
        .executeScript({ target: { tabId }, files: ['content-script.js'] })
        .catch((err) => {
          regionWaiters.delete(tabId);
          reject(err instanceof Error ? err : new Error(String(err)));
        });
    });
  })();
}

// —— 编辑器打开 ——

async function openEditor(payload: CapturePayload): Promise<void> {
  await storeCapture(payload);
  await chrome.tabs.create({
    url: `${chrome.runtime.getURL('editor/editor.html')}?id=${encodeURIComponent(payload.captureId)}`,
  });
}

// —— 主流程 ——

function isCapturableUrl(url: string | undefined): boolean {
  return (
    !!url &&
    (url.startsWith('http://') || url.startsWith('https://') || url.startsWith('file://'))
  );
}

async function runCapture(mode: CaptureMode, tab: chrome.tabs.Tab): Promise<void> {
  if (!isCapturableUrl(tab.url)) {
    flashBadge(tab.id, '×', false);
    console.warn('[hawksnap] 该页面类型不支持截图（仅 http/https/file）:', tab.url);
    return;
  }
  try {
    const payload =
      mode === 'full'
        ? await captureFullPage(tab)
        : mode === 'visible'
          ? {
              captureId: uuid(),
              pngBase64: stripDataUrl(await captureVisible(tab.windowId)),
              // 可见区尺寸交给编辑器按图片实际像素回填（协议允许 0=未知，
              // 但编辑器解码后总是能拿到真实值）。
              width: 0,
              height: 0,
              source: 'visible' as const,
            }
          : await captureRegion(tab);
    flashBadge(tab.id, '✓', true);
    await openEditor(payload);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    if (message.includes('已取消')) return; // 用户主动取消，不打扰
    console.error('[hawksnap] 捕获失败:', err);
    flashBadge(tab.id, '!', false);
    // 打开编辑器错误态，让失败原因可见（而不是只留在 SW 日志里）。
    await openEditor({
      captureId: uuid(),
      pngBase64: '',
      width: 0,
      height: 0,
      source: mode,
      note: `捕获失败：${message}`,
    });
  }
}

// —— 消息路由 ——

chrome.runtime.onMessage.addListener((msg: unknown, sender, sendResponse) => {
  const m = msg as { type?: string } & Record<string, unknown>;
  if (!m || typeof m.type !== 'string') return;

  if (m.type === 'hawksnap:capture') {
    void (async () => {
      // popup 显式带上 tabId/windowId（弹出层关闭瞬间焦点可能未回归，
      // 依赖 lastFocusedWindow 有竞态）；缺省时回退到当前聚焦窗口的活动标签。
      let tab: chrome.tabs.Tab | null = null;
      if (typeof m.tabId === 'number') {
        tab = (await chrome.tabs.query({})).find((t) => t.id === m.tabId) ?? null;
      }
      if (!tab) {
        const [active] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
        tab = active ?? null;
      }
      if (!tab || tab.id === undefined) {
        console.warn('[hawksnap] 找不到目标标签页');
        return;
      }
      await runCapture(m.mode as CaptureMode, tab);
    })();
    return; // fire-and-forget，不占用 sendResponse
  }

  if (m.type === 'hawksnap:region-selected' && sender.tab?.id !== undefined) {
    const waiter = regionWaiters.get(sender.tab.id);
    regionWaiters.delete(sender.tab.id);
    waiter?.(
      m.cancelled === true
        ? null
        : {
            rect: m.rect as { x: number; y: number; width: number; height: number },
            devicePixelRatio: typeof m.devicePixelRatio === 'number' ? m.devicePixelRatio : 1,
          },
    );
    return; // 不回应
  }

  if (m.type === 'hawksnap:get-capture') {
    void (async () => {
      const payload = await loadCapture(String(m.id));
      sendResponse({ type: 'hawksnap:capture-payload', found: !!payload, payload });
    })();
    return true; // 异步 sendResponse
  }

  // 'hawksnap:offscreen-crop' 由 offscreen.js 处理；SW 不回应（避免抢占 sendResponse）。
});
