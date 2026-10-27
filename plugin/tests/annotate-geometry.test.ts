/**
 * 标注几何纯函数测试：选区归一化 / 箭头头部 / 显示→画布坐标映射 / 越界钳制。
 * 编辑器（editor.ts）只做事件与绘制；几何行为在此锁定。
 */

import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import {
  arrowGeometry,
  clampPointToRect,
  isSelectionLargeEnough,
  normalizeRect,
  toCanvasPoint,
} from '../src/shared/annotate-geometry.js';

describe('normalizeRect（拖拽方向归一化）', () => {
  it('从右下往左上拖 → 非负宽高的矩形', () => {
    assert.deepEqual(normalizeRect({ x: 10, y: 10 }, { x: 4, y: 3 }), { x: 4, y: 3, width: 6, height: 7 });
  });
  it('从左上往右下拖 → 同一矩形', () => {
    assert.deepEqual(normalizeRect({ x: 4, y: 3 }, { x: 10, y: 10 }), { x: 4, y: 3, width: 6, height: 7 });
  });
  it('单击（起终点重合）→ 零尺寸', () => {
    assert.deepEqual(normalizeRect({ x: 5, y: 5 }, { x: 5, y: 5 }), { x: 5, y: 5, width: 0, height: 0 });
  });
});

describe('isSelectionLargeEnough（误触过滤）', () => {
  it('默认 minSize=4：4×4 有效、3×9 无效', () => {
    assert.equal(isSelectionLargeEnough({ x: 0, y: 0, width: 4, height: 4 }), true);
    assert.equal(isSelectionLargeEnough({ x: 0, y: 0, width: 3, height: 9 }), false);
  });
  it('可自定义阈值', () => {
    assert.equal(isSelectionLargeEnough({ x: 0, y: 0, width: 2, height: 2 }, 2), true);
  });
});

describe('arrowGeometry（箭头头部）', () => {
  it('水平向右：头部在 tip 后方对称张开', () => {
    const g = arrowGeometry({ x: 0, y: 0 }, { x: 10, y: 0 }, 5); // headLen = clamp(5×4)=20
    assert.deepEqual(g.tip, { x: 10, y: 0 });
    assert.equal(Math.abs(Math.hypot(g.tip.x - g.baseA.x, g.tip.y - g.baseA.y) - 20) < 1e-9, true);
    assert.ok(g.baseA.x < 10, '底角在 tip 后方');
    assert.equal(Math.abs(g.baseA.y + g.baseB.y) < 1e-9, true, '对称张开');
    assert.ok(g.baseA.y > 0 && g.baseB.y < 0);
  });
  it('笔画细：headLen 有下限（仍可见）', () => {
    const g = arrowGeometry({ x: 0, y: 0 }, { x: 100, y: 0 }, 2); // raw 8 → 钳到 10
    assert.equal(Math.hypot(g.tip.x - g.baseA.x, g.tip.y - g.baseA.y), 10);
  });
  it('笔画粗：headLen 有上限（不爆视野）', () => {
    const g = arrowGeometry({ x: 0, y: 0 }, { x: 100, y: 0 }, 100); // raw 400 → 钳到 28
    assert.equal(Math.hypot(g.tip.x - g.baseA.x, g.tip.y - g.baseA.y), 28);
  });
  it('竖直向下：头部沿反方向延伸且左右对称', () => {
    const g = arrowGeometry({ x: 0, y: 0 }, { x: 0, y: 10 }, 5);
    assert.ok(g.baseA.y < 10 && g.baseB.y < 10, '底角朝 from 一侧');
    assert.equal(Math.abs(Math.abs(g.baseA.x) - Math.abs(g.baseB.x)) < 1e-9, true);
    assert.ok(g.baseA.x < 0 && g.baseB.x > 0);
  });
});

describe('toCanvasPoint（显示坐标 → 真实像素）', () => {
  const display = { x: 100, y: 50, width: 800, height: 400 };
  it('2× 缩放映射', () => {
    assert.deepEqual(toCanvasPoint(150, 90, display, 1600), { x: 100, y: 80 });
  });
  it('display 原点偏移被扣除、四舍五入到整数像素', () => {
    assert.deepEqual(toCanvasPoint(101.4, 51.9, display, 1600), { x: 3, y: 4 });
  });
  it('display 宽为 0（未布局）→ 不放大', () => {
    assert.deepEqual(toCanvasPoint(7, 8, { x: 0, y: 0, width: 0, height: 0 }, 100), { x: 7, y: 8 });
  });
});

describe('clampPointToRect（越界钳制）', () => {
  it('负坐标钳到 0，超界钳到宽高', () => {
    assert.deepEqual(clampPointToRect({ x: -5, y: 20 }, 10, 10), { x: 0, y: 10 });
  });
  it('界内点不变', () => {
    assert.deepEqual(clampPointToRect({ x: 3, y: 7 }, 10, 10), { x: 3, y: 7 });
  });
});
