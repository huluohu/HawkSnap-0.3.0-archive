/**
 * 编辑器页（extension tab）：标注 + 复制 / 下载 / 发送到桌面端。
 *
 * 双画布结构（性能：整页图可达 40 MP，基图重绘每帧不可接受）：
 *   #board  基图 + 已提交的标注 —— 只在提交/撤销/清空时重绘；
 *   #draft  进行中的形状 —— 拖拽期间只重绘这一层（同尺寸叠加在上）。
 * 导出直接用 #board（提交时已包含全部形状）。
 *
 * 几何全部在 shared/annotate-geometry（纯函数，独立测试）；协议字段构造在
 * shared/protocol + shared/native-client（发送到桌面端 = save → capture 链路，
 * 失败按任务要求降级为本地下载）。
 */

import type { CapturePayload } from './shared/capture.js';
import {
  arrowGeometry,
  clampPointToRect,
  DEFAULT_TEXT_SIZE,
  normalizeRect,
  toCanvasPoint,
  type Point,
  type Rect,
} from './shared/annotate-geometry.js';
import { readPngSize } from './shared/protocol.js';
import { sendToDesktop } from './shared/native-client.js';

type Tool = 'rect' | 'arrow' | 'pen' | 'highlighter' | 'text';

type Shape =
  | { kind: 'rect'; rect: Rect; color: string; width: number }
  | { kind: 'arrow'; from: Point; to: Point; color: string; width: number }
  | { kind: 'pen'; points: Point[]; color: string; width: number }
  | { kind: 'highlighter'; points: Point[]; color: string; width: number }
  | { kind: 'text'; at: Point; text: string; color: string; size: number };

const TOOL_WIDTH: Record<Tool, number> = {
  rect: 3,
  arrow: 4,
  pen: 3,
  highlighter: 18,
  text: DEFAULT_TEXT_SIZE,
};

const SOURCE_LABEL: Record<string, string> = {
  full: 'fullpage',
  visible: 'visible',
  region: 'region',
};

// —— DOM ——
const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`缺少元素 #${id}`);
  return el as T;
};
const bar = $<HTMLElement>('bar');
const stage = $<HTMLElement>('stage');
const errorBox = $<HTMLElement>('error');
const status = $<HTMLElement>('status');
const board = $<HTMLCanvasElement>('board');
const draft = document.createElement('canvas');
draft.id = 'draft';
draft.style.cssText = 'position:absolute;inset:0;touch-action:none;';
stage.appendChild(draft);
const boardCtx = board.getContext('2d') as CanvasRenderingContext2D;
const draftCtx = draft.getContext('2d') as CanvasRenderingContext2D;

// —— 状态 ——
let image: HTMLImageElement | null = null;
let payload: CapturePayload | null = null;
let shapes: Shape[] = [];
let tool: Tool = 'rect';
let color = '#e03131';
let drawing = false;
let dragStart: Point | null = null;
let draftShape: Shape | null = null;
let busy = false;

function setStatus(html: string, cls = ''): void {
  status.innerHTML = cls ? `<span class="${cls}">${html}</span>` : html;
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c] as string);
}

// —— 绘制 —— 

function paintShape(ctx: CanvasRenderingContext2D, s: Shape): void {
  ctx.save();
  switch (s.kind) {
    case 'rect': {
      ctx.strokeStyle = s.color;
      ctx.lineWidth = s.width;
      ctx.strokeRect(s.rect.x, s.rect.y, s.rect.width, s.rect.height);
      break;
    }
    case 'arrow': {
      ctx.strokeStyle = s.color;
      ctx.fillStyle = s.color;
      ctx.lineWidth = s.width;
      ctx.lineCap = 'round';
      ctx.beginPath();
      ctx.moveTo(s.from.x, s.from.y);
      ctx.lineTo(s.to.x, s.to.y);
      ctx.stroke();
      const head = arrowGeometry(s.from, s.to, s.width);
      ctx.beginPath();
      ctx.moveTo(head.tip.x, head.tip.y);
      ctx.lineTo(head.baseA.x, head.baseA.y);
      ctx.lineTo(head.baseB.x, head.baseB.y);
      ctx.closePath();
      ctx.fill();
      break;
    }
    case 'pen':
    case 'highlighter': {
      if (s.points.length === 0) break;
      ctx.strokeStyle = s.color;
      ctx.lineWidth = s.width;
      ctx.lineJoin = 'round';
      ctx.lineCap = 'round';
      if (s.kind === 'highlighter') ctx.globalAlpha = 0.4;
      ctx.beginPath();
      ctx.moveTo(s.points[0].x, s.points[0].y);
      for (let i = 1; i < s.points.length; i++) ctx.lineTo(s.points[i].x, s.points[i].y);
      if (s.points.length === 1) ctx.lineTo(s.points[0].x + 0.01, s.points[0].y);
      ctx.stroke();
      break;
    }
    case 'text': {
      ctx.fillStyle = s.color;
      ctx.font = `${s.size}px system-ui, "PingFang SC", sans-serif`;
      ctx.textBaseline = 'top';
      ctx.fillText(s.text, s.at.x, s.at.y);
      break;
    }
  }
  ctx.restore();
}

function repaintBase(): void {
  boardCtx.clearRect(0, 0, board.width, board.height);
  if (image) boardCtx.drawImage(image, 0, 0);
  for (const s of shapes) paintShape(boardCtx, s);
}

function repaintDraft(): void {
  draftCtx.clearRect(0, 0, draft.width, draft.height);
  if (draftShape) paintShape(draftCtx, draftShape);
}

function refit(): void {
  if (!image) return;
  // 显示比例 = 可视区与原图的较小者（原图小于可视区时不放大，保持 1:1）。
  const availW = Math.max(320, window.innerWidth - 64);
  const availH = Math.max(240, window.innerHeight - 150);
  const s = Math.min(1, availW / image.naturalWidth, availH / image.naturalHeight);
  const cssW = Math.max(1, Math.round(image.naturalWidth * s));
  board.style.width = `${cssW}px`;
  draft.style.width = `${cssW}px`;
}

window.addEventListener('resize', refit);

// —— 指针 → 画布真实像素 —— 

function canvasPoint(e: PointerEvent): Point {
  const rect = draft.getBoundingClientRect();
  return toCanvasPoint(e.clientX, e.clientY, { x: rect.x, y: rect.y, width: rect.width, height: rect.height }, board.width);
}

function onPointerDown(e: PointerEvent): void {
  if (busy || !image || e.button !== 0) return;
  draft.setPointerCapture(e.pointerId);
  drawing = true;
  dragStart = canvasPoint(e);
  if (tool === 'pen' || tool === 'highlighter') {
    draftShape = { kind: tool, points: [clampPointToRect(dragStart, board.width, board.height)], color, width: TOOL_WIDTH[tool] };
  } else {
    draftShape = null; // rect/arrow 拖出第一个 move 再显示；text 点击即成
  }
  repaintDraft();
}

function onPointerMove(e: PointerEvent): void {
  if (!drawing || !dragStart) return;
  const p = canvasPoint(e);
  if (tool === 'pen' || tool === 'highlighter') {
    const shape = draftShape as { kind: 'pen' | 'highlighter'; points: Point[] } | null;
    shape?.points.push(clampPointToRect(p, board.width, board.height));
  } else if (tool === 'rect') {
    draftShape = { kind: 'rect', rect: normalizeRect(dragStart, p), color, width: TOOL_WIDTH.rect };
  } else if (tool === 'arrow') {
    draftShape = { kind: 'arrow', from: dragStart, to: p, color, width: TOOL_WIDTH.arrow };
  }
  repaintDraft();
  e.preventDefault();
}

function onPointerUp(e: PointerEvent): void {
  if (!drawing || !dragStart) return;
  drawing = false;
  const p = canvasPoint(e);
  if (tool === 'text') {
    const text = window.prompt('标注文本：');
    if (text && text.trim().length > 0) {
      shapes.push({ kind: 'text', at: clampPointToRect(p, board.width, board.height), text: text.trim(), color, size: TOOL_WIDTH.text });
      repaintBase();
    }
  } else {
    const s = draftShape;
    if (s) {
      const big =
        s.kind === 'rect'
          ? s.rect.width >= 2 && s.rect.height >= 2
          : s.kind === 'arrow'
            ? Math.hypot(s.to.x - s.from.x, s.to.y - s.from.y) >= 2
            : (s.kind === 'pen' || s.kind === 'highlighter') && s.points.length >= 2;
      if (big) shapes.push(s); // 过小的形状视为误触，丢弃
    }
    repaintBase();
  }
  draftShape = null;
  dragStart = null;
  repaintDraft();
}

draft.addEventListener('pointerdown', onPointerDown);
draft.addEventListener('pointermove', onPointerMove);
draft.addEventListener('pointerup', onPointerUp);

// —— 工具 / 颜色 / 撤销 —— 

for (const btn of document.querySelectorAll<HTMLButtonElement>('#tools .tool')) {
  btn.addEventListener('click', () => {
    tool = (btn.dataset.tool as Tool) ?? 'rect';
    for (const b of document.querySelectorAll('#tools .tool')) b.classList.toggle('active', b === btn);
  });
}
document.querySelector<HTMLButtonElement>('#tools .tool')?.classList.add('active');

for (const btn of document.querySelectorAll<HTMLButtonElement>('#colors .swatch')) {
  btn.addEventListener('click', () => {
    color = btn.dataset.color ?? '#e03131';
    for (const b of document.querySelectorAll('#colors .swatch')) b.classList.toggle('active', b === btn);
  });
}
document.querySelector<HTMLButtonElement>('#colors .swatch')?.classList.add('active');

function undo(): void {
  if (shapes.length === 0) return;
  shapes.pop();
  repaintBase();
}
function clearAll(): void {
  if (shapes.length === 0) return;
  shapes = [];
  repaintBase();
}
$('undo').addEventListener('click', undo);
$('clear').addEventListener('click', clearAll);

window.addEventListener('keydown', (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'z') {
    e.preventDefault();
    undo();
  } else if (e.key === 'Escape' && drawing) {
    drawing = false;
    draftShape = null;
    dragStart = null;
    repaintDraft();
  }
});

// —— 导出（#board 已含全部已提交形状） —— 

async function exportBlob(): Promise<Blob> {
  return new Promise<Blob>((resolve, reject) => {
    board.toBlob((b) => (b ? resolve(b) : reject(new Error('画布导出失败'))), 'image/png');
  });
}

function downloadBlob(blob: Blob): Promise<string> {
  const ts = new Date()
    .toISOString()
    .replace(/[-:]/g, '')
    .replace('T', '-')
    .slice(0, 15); // 20261027-153045
  const name = `HawkSnap-${SOURCE_LABEL[payload?.source ?? ''] ?? 'shot'}-${ts}.png`;
  const url = URL.createObjectURL(blob);
  return chrome.downloads
    .download({ url, filename: name, saveAs: false })
    .then((id) => {
      setTimeout(() => URL.revokeObjectURL(url), 60_000); // 下载启动后再回收
      return id >= 0 ? name : Promise.reject(new Error('下载任务创建失败'));
    })
    .catch((err) => {
      URL.revokeObjectURL(url);
      throw err;
    });
}

$('download').addEventListener('click', () => {
  void (async () => {
    if (busy) return;
    busy = true;
    setActionsDisabled(true);
    try {
      const name = await downloadBlob(await exportBlob());
      setStatus(`已下载：${escapeHtml(name)}`, 'ok');
    } catch (err) {
      setStatus(`下载失败：${escapeHtml(err instanceof Error ? err.message : String(err))}`, 'bad');
    } finally {
      busy = false;
      setActionsDisabled(false);
    }
  })();
});

$('copy').addEventListener('click', () => {
  void (async () => {
    if (busy) return;
    busy = true;
    setActionsDisabled(true);
    try {
      if (!document.hasFocus()) {
        setStatus('请先点击页面任意处（剪贴板要求文档聚焦）再复制', 'bad');
        return;
      }
      const blob = await exportBlob();
      await navigator.clipboard.write([new ClipboardItem({ 'image/png': blob })]);
      setStatus('已复制到剪贴板', 'ok');
    } catch (err) {
      setStatus(`复制失败：${escapeHtml(err instanceof Error ? err.message : String(err))}`, 'bad');
    } finally {
      busy = false;
      setActionsDisabled(false);
    }
  })();
});

// —— 发送到桌面端（Native Messaging：save → capture，失败降级本地下载） —— 

$('send').addEventListener('click', () => {
  void (async () => {
    if (busy) return;
    busy = true;
    setActionsDisabled(true);
    const send = $('send') as HTMLButtonElement;
    const prevLabel = send.textContent;
    send.textContent = '发送中…';
    try {
      const blob = await exportBlob();
      const pngBytes = new Uint8Array(await blob.arrayBuffer());
      const pngSize = readPngSize(pngBytes); // 协议 §2.1：width/height 填真实像素（或 0）
      const result = await sendToDesktop({
        pngBytes,
        width: pngSize?.width ?? 0,
        height: pngSize?.height ?? 0,
        idSeed: payload?.captureId,
      });
      if (result.status === 'saved') {
        setStatus(`已发送到 HawkSnap 桌面端：${escapeHtml(result.savedPath)}`, 'ok');
      } else {
        // 失败（宿主缺失 / 4MB 上限 / 断连 / 桌面端拒收）→ 降级本地下载。
        setStatus(`${escapeHtml(result.message)} —— 已降级为本地下载`, 'bad');
        const name = await downloadBlob(blob);
        setStatus(`${escapeHtml(result.message)} —— 已降级为本地下载：${escapeHtml(name)}`, 'bad');
      }
    } catch (err) {
      setStatus(`发送失败：${escapeHtml(err instanceof Error ? err.message : String(err))}`, 'bad');
    } finally {
      send.textContent = prevLabel;
      busy = false;
      setActionsDisabled(false);
    }
  })();
});

function setActionsDisabled(disabled: boolean): void {
  for (const id of ['copy', 'download', 'send', 'undo', 'clear']) {
    ($(id) as HTMLButtonElement).disabled = disabled;
  }
}

// —— 载入 —— 

function showError(message: string): void {
  bar.style.display = 'none';
  stage.style.display = 'none';
  errorBox.style.display = 'block';
  errorBox.textContent = message;
}

function showImage(img: HTMLImageElement): void {
  bar.style.display = 'flex';
  errorBox.style.display = 'none';
  stage.style.display = 'inline-block';
  board.width = img.naturalWidth;
  board.height = img.naturalHeight;
  draft.width = img.naturalWidth;
  draft.height = img.naturalHeight;
  image = img;
  refit();
  repaintBase();
  const note = payload?.note ? `${escapeHtml(payload.note)} ｜ ` : '';
  setStatus(`${note}${board.width} × ${board.height} px ｜ 左侧选工具标注，右上复制 / 下载 / 发送到桌面端`);
}

void (async () => {
  const captureId = new URLSearchParams(window.location.search).get('id');
  if (!captureId) {
    showError('缺少截图 id（请从工具栏重新发起截图）');
    return;
  }
  let res: { found?: boolean; payload?: CapturePayload } | undefined;
  try {
    res = (await chrome.runtime.sendMessage({ type: 'hawksnap:get-capture', id: captureId })) as
      | { found?: boolean; payload?: CapturePayload }
      | undefined;
  } catch (err) {
    showError(`无法从后台取得截图数据：${err instanceof Error ? err.message : String(err)}`);
    return;
  }
  const data = res?.payload;
  if (!res?.found || !data || !data.pngBase64) {
    showError(
      (data?.note ? `捕获失败：${data.note}` : '截图数据不存在（可能因浏览器重启丢失；session 数据不持久化）') +
        '\n请从工具栏重新发起截图。',
    );
    return;
  }
  payload = data;
  const img = new Image();
  try {
    await new Promise<void>((resolve, reject) => {
      img.onload = () => resolve();
      img.onerror = () => reject(new Error('PNG 解码失败'));
      img.src = `data:image/png;base64,${data.pngBase64}`;
    });
  } catch (err) {
    showError(err instanceof Error ? err.message : String(err));
    return;
  }
  showImage(img);
})();
