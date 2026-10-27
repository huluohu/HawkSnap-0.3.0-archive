/**
 * 标注几何（纯函数）：选区归一化、箭头箭头头部、显示坐标 → 画布真实像素映射。
 * 编辑器（editor.ts）只做绘制与事件，几何全部在这里，可独立测试。
 */

export type Point = { x: number; y: number };

export type Rect = { x: number; y: number; width: number; height: number };

/** 拖拽起点/终点 → 归一化矩形（支持往任意方向拖；宽高取非负）。 */
export function normalizeRect(start: Point, end: Point): Rect {
  const x = Math.min(start.x, end.x);
  const y = Math.min(start.y, end.y);
  return {
    x,
    y,
    width: Math.abs(end.x - start.x),
    height: Math.abs(end.y - start.y),
  };
}

/** 选区是否有效（过小的拖拽视为误触：宽高都要 ≥ minSize）。 */
export function isSelectionLargeEnough(rect: Rect, minSize = 4): boolean {
  return rect.width >= minSize && rect.height >= minSize;
}

/**
 * 箭头几何：给线段 (from → tip) 与箭头头部边长，返回三个顶点
 * （tip + 两个底角）。底角相对线段方向对称张开 HEAD_SPREAD_RAD。
 * 长度按笔画宽度缩放并夹在 [minHeadLen, maxHeadLen]，随缩放仍可见。
 */
export const HEAD_SPREAD_RAD = 0.45;

export function arrowGeometry(
  from: Point,
  tip: Point,
  strokeWidth: number,
  minHeadLen = 10,
  maxHeadLen = 28,
): { tip: Point; baseA: Point; baseB: Point } {
  const angle = Math.atan2(tip.y - from.y, tip.x - from.x);
  const raw = strokeWidth * 4;
  const headLen = Math.min(maxHeadLen, Math.max(minHeadLen, raw));
  const baseA: Point = {
    x: tip.x - headLen * Math.cos(angle - HEAD_SPREAD_RAD),
    y: tip.y - headLen * Math.sin(angle - HEAD_SPREAD_RAD),
  };
  const baseB: Point = {
    x: tip.x - headLen * Math.cos(angle + HEAD_SPREAD_RAD),
    y: tip.y - headLen * Math.sin(angle + HEAD_SPREAD_RAD),
  };
  return { tip, baseA, baseB };
}

/**
 * 显示（CSS）坐标 → 画布真实像素坐标。编辑器把大图缩放到可视区显示，
 * 事件坐标必须按显示矩形映射回真实分辨率（四舍五入到整数像素）。
 */
export function toCanvasPoint(
  displayX: number,
  displayY: number,
  display: Rect,
  canvasWidth: number,
): Point {
  const scale = display.width > 0 ? canvasWidth / display.width : 1;
  return {
    x: Math.round((displayX - display.x) * scale),
    y: Math.round((displayY - display.y) * scale),
  };
}

/** 显示比例：真实宽 → 显示宽（真实宽为 0 时退化为 1，防除零）。 */
export function fitScale(canvasWidth: number, displayWidth: number): number {
  if (canvasWidth <= 0 || displayWidth <= 0) return 1;
  return displayWidth / canvasWidth;
}

/**
 * 把任意点集夹进画布范围（画笔/荧光笔越界点截到边界，避免描边溢出画布）。
 */
export function clampPointToRect(p: Point, width: number, height: number): Point {
  return {
    x: Math.min(width, Math.max(0, p.x)),
    y: Math.min(height, Math.max(0, p.y)),
  };
}

/** 文本标注的默认字号（真实像素；编辑器可覆盖）。 */
export const DEFAULT_TEXT_SIZE = 24;
