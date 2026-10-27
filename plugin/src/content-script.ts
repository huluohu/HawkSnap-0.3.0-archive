/**
 * 内容脚本：区域选择的浮层（region 模式）。
 *
 * 由 service worker 经 chrome.scripting.executeScript 注入。注入时可见区截图
 * 已经拍好（截图在前、浮层在后 → 浮层不会出现在截图里），这里只负责：
 *   全屏 crosshair 浮层 → 拖拽出矩形（含尺寸提示）→ mouseup 上报。
 *
 * 上报消息：{type:'hawksnap:region-selected', rect:{x,y,width,height}（CSS px，
 * 相对视口）, devicePixelRatio} 或 {type:'hawksnap:region-selected', cancelled:true}。
 * 坐标系：截图（captureVisibleTab）是设备像素，SW 按 dpr 放大 rect；dpr 在这里
 * 一并上报（含浏览器缩放），保证与截图一致。
 *
 * 重复注入：先拆掉上一次的浮层再建新的（executeScript files 会整段重跑）。
 * Esc 取消；过小的拖拽（< 4px，annotate-geometry 的 minSize）按取消处理。
 */

interface RegionGlobal {
  __hawksnapRegion?: {
    root: HTMLDivElement;
    keydown: (e: KeyboardEvent) => void;
  };
}

const MIN_SELECTION_PX = 4;
const OVERLAY_BG = 'rgba(12, 22, 36, 0.35)';
const BORDER_COLOR = '#FFB020';

type Point = { x: number; y: number };

function cleanupPrevious(): void {
  const g = window as unknown as RegionGlobal;
  if (g.__hawksnapRegion) {
    g.__hawksnapRegion.root.remove();
    window.removeEventListener('keydown', g.__hawksnapRegion.keydown, true);
    g.__hawksnapRegion = undefined;
  }
}

function report(payload: Record<string, unknown>): void {
  // 不期望回执：回调形式 + 静默 lastError（无监听时 Chrome 会报 port closed）。
  try {
    chrome.runtime.sendMessage({ type: 'hawksnap:region-selected', ...payload }, () => {
      void chrome.runtime.lastError;
    });
  } catch {
    // 扩展上下文失效（SW 已回收且重载）→ 用户重试即可。
  }
}

function buildOverlay(): {
  root: HTMLDivElement;
  box: HTMLDivElement;
  label: HTMLDivElement;
} {
  const root = document.createElement('div');
  root.style.cssText = [
    'position:fixed',
    'inset:0',
    'z-index:2147483647',
    'cursor:crosshair',
    'background:transparent',
    'user-select:none',
    '-webkit-user-select:none',
  ].join(';');

  const box = document.createElement('div');
  box.style.cssText = [
    'position:fixed',
    'display:none',
    `border:2px solid ${BORDER_COLOR}`,
    `box-shadow:0 0 0 100000px ${OVERLAY_BG}`, // 外部压暗的标准技巧
    'pointer-events:none',
  ].join(';');

  const label = document.createElement('div');
  label.style.cssText = [
    'position:fixed',
    'display:none',
    'padding:2px 6px',
    'border-radius:4px',
    'background:rgba(12,22,36,0.85)',
    `color:${BORDER_COLOR}`,
    'font:12px/1.4 system-ui, sans-serif',
    'pointer-events:none',
    'white-space:nowrap',
  ].join(';');

  root.appendChild(box);
  root.appendChild(label);
  document.documentElement.appendChild(root);
  return { root, box, label };
}

function main(): void {
  cleanupPrevious();
  const { root, box, label } = buildOverlay();

  let start: Point | null = null;
  let dragging = false;
  let done = false;

  const showRect = (r: { x: number; y: number; width: number; height: number }): void => {
    box.style.display = 'block';
    box.style.left = `${r.x}px`;
    box.style.top = `${r.y}px`;
    box.style.width = `${r.width}px`;
    box.style.height = `${r.height}px`;
    label.style.display = 'block';
    label.style.left = `${r.x}px`;
    label.style.top = `${Math.max(0, r.y - 22)}px`;
    label.textContent = `${Math.round(r.width)} × ${Math.round(r.height)}`;
  };

  const finish = (cancelled: boolean, rect?: { x: number; y: number; width: number; height: number }): void => {
    if (done) return;
    done = true;
    cleanupListeners();
    // 先隐藏浮层再上报：给 captureVisibleTab 留干净画面（虽然截图早已拍好，
    // 浮层消失也要即时，避免用户看到残影）。
    root.remove();
    if (cancelled || !rect) {
      report({ cancelled: true });
      return;
    }
    report({
      rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
      devicePixelRatio: window.devicePixelRatio,
    });
  };

  function onPointerDown(e: PointerEvent): void {
    if (e.button !== 0) {
      finish(true); // 右键/中键 = 取消
      return;
    }
    dragging = true;
    start = { x: e.clientX, y: e.clientY };
    e.preventDefault();
  }

  function onPointerMove(e: PointerEvent): void {
    if (!dragging || !start) return;
    // normalizeRect 的运行时实现（纯函数版在 annotate-geometry，供测试）。
    const x = Math.min(start.x, e.clientX);
    const y = Math.min(start.y, e.clientY);
    const width = Math.abs(e.clientX - start.x);
    const height = Math.abs(e.clientY - start.y);
    showRect({ x, y, width, height });
    e.preventDefault();
  }

  function onPointerUp(e: PointerEvent): void {
    if (!dragging || !start) return;
    dragging = false;
    e.preventDefault();
    const rect = {
      x: Math.min(start.x, e.clientX),
      y: Math.min(start.y, e.clientY),
      width: Math.abs(e.clientX - start.x),
      height: Math.abs(e.clientY - start.y),
    };
    if (rect.width < MIN_SELECTION_PX || rect.height < MIN_SELECTION_PX) {
      finish(true); // 误触（单击/极小拖拽）→ 按取消处理，不打扰
      return;
    }
    finish(false, rect);
  }

  const keydown = (e: KeyboardEvent): void => {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      finish(true);
    }
  };

  function cleanupListeners(): void {
    root.removeEventListener('pointerdown', onPointerDown, true);
    root.removeEventListener('pointermove', onPointerMove, true);
    root.removeEventListener('pointerup', onPointerUp, true);
    window.removeEventListener('keydown', keydown, true);
  }

  root.addEventListener('pointerdown', onPointerDown, true);
  root.addEventListener('pointermove', onPointerMove, true);
  root.addEventListener('pointerup', onPointerUp, true);
  window.addEventListener('keydown', keydown, true);

  const g = window as unknown as RegionGlobal;
  g.__hawksnapRegion = { root, keydown };
}

main();
