/**
 * 尺寸策略纯逻辑测试（spec §3.5：10000px 高页面整页保真）。
 * 对象：computeCapturePolicy（降采样/裁剪决策）、imageSettleDecision（懒加载
 * 等待边界）、planLazyScroll（滚动计划）——全部与 chrome.* 无关。
 */

import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import {
  computeCapturePolicy,
  imageSettleDecision,
  LAZY_STEP_MAX_WAIT_MS,
  LAZY_TOTAL_BUDGET_MS,
  MAX_BRIDGE_PIXELS,
  MAX_SURFACE_EDGE_PX,
  MIN_DEVICE_SCALE_FACTOR,
  planLazyScroll,
} from '../src/shared/size-policy.js';

describe('computeCapturePolicy（整页尺寸策略）', () => {
  it('普通页面：原尺寸保真，不降采样不裁剪', () => {
    const p = computeCapturePolicy({ cssWidth: 1920, cssHeight: 1080, devicePixelRatio: 1 });
    assert.equal(p.deviceScaleFactor, 1);
    assert.equal(p.outWidth, 1920);
    assert.equal(p.outHeight, 1080);
    assert.equal(p.downscaled, false);
    assert.equal(p.cropped, false);
  });

  it('spec 边界：10000px 高 @dpr1 整页保真（不降采样）', () => {
    const p = computeCapturePolicy({ cssWidth: 1920, cssHeight: 10000, devicePixelRatio: 1 });
    assert.equal(p.cropped, false);
    assert.equal(p.downscaled, false);
    assert.equal(p.outWidth, 1920);
    assert.equal(p.outHeight, 10000);
  });

  it('>10000px：超出渲染面/桥端像素闸时整体降采样（不裁内容）', () => {
    // 1920×10000 css @dpr2 = 3840×20000 设备像素 → 超单边上限，且压完超 40MP。
    const p = computeCapturePolicy({ cssWidth: 1920, cssHeight: 10000, devicePixelRatio: 2 });
    assert.equal(p.cropped, false, '内容必须完整');
    assert.equal(p.downscaled, true);
    assert.equal(p.deviceScaleFactor < 2, true);
    // 单边不超渲染面上限、像素不超桥端闸（rounding 余量 0.25% ≈ 1px/轴）。
    assert.ok(p.outWidth <= MAX_SURFACE_EDGE_PX);
    assert.ok(p.outHeight <= MAX_SURFACE_EDGE_PX);
    assert.ok(p.outWidth * p.outHeight <= MAX_BRIDGE_PIXELS * 1.0025);
  });

  it('极端长页：dsf 压到下限仍放不下 → 裁剪（cropped=true，明确标记）', () => {
    const p = computeCapturePolicy({ cssWidth: 1920, cssHeight: 1_000_000, devicePixelRatio: 2 });
    assert.equal(p.cropped, true);
    assert.equal(p.deviceScaleFactor, MIN_DEVICE_SCALE_FACTOR);
    assert.equal(p.outHeight, MAX_SURFACE_EDGE_PX);
    assert.ok(p.outWidth * p.outHeight <= MAX_BRIDGE_PIXELS * 1.0025);
  });

  it('dpr 异常值（0）按 1 处理', () => {
    const p = computeCapturePolicy({ cssWidth: 1000, cssHeight: 800, devicePixelRatio: 0 });
    assert.equal(p.deviceScaleFactor, 1);
    assert.equal(p.outWidth, 1000);
  });

  it('超宽页面：宽度方向同样受单边上限约束', () => {
    const p = computeCapturePolicy({ cssWidth: 20000, cssHeight: 1000, devicePixelRatio: 1 });
    assert.equal(p.downscaled, true);
    assert.equal(p.outWidth, MAX_SURFACE_EDGE_PX);
    assert.equal(p.cropped, false);
  });
});

describe('imageSettleDecision（懒加载等待的有界决策）', () => {
  const base = { totalImages: 10, settledImages: 4, stepElapsedMs: 0, totalElapsedMs: 0 };

  it('全部就绪 → continue', () => {
    assert.equal(imageSettleDecision({ ...base, settledImages: 10 }), 'continue');
  });

  it('有未完成图且未超任何界 → wait', () => {
    assert.equal(imageSettleDecision({ ...base, stepElapsedMs: 100, totalElapsedMs: 500 }), 'wait');
  });

  it('单步等待超过上限 → continue（单张卡死不拖垮整体）', () => {
    assert.equal(imageSettleDecision({ ...base, stepElapsedMs: LAZY_STEP_MAX_WAIT_MS }), 'continue');
  });

  it('总预算耗尽 → give-up（优先级最高，即使还有未完成图）', () => {
    assert.equal(imageSettleDecision({ ...base, totalElapsedMs: LAZY_TOTAL_BUDGET_MS }), 'give-up');
    assert.equal(imageSettleDecision({ ...base, settledImages: 10, totalElapsedMs: LAZY_TOTAL_BUDGET_MS + 1 }), 'give-up');
  });
});

describe('planLazyScroll（懒加载滚动计划）', () => {
  it('10000px 高页面：按 0.9 视口高步进覆盖全页', () => {
    const positions = planLazyScroll(10000, 600);
    assert.equal(positions[0], 0);
    assert.ok(positions[positions.length - 1] >= 10000 - 540, '最后一步应到达页面底部附近');
    assert.ok(positions.every((y, i) => i === 0 || y > positions[i - 1]), '严格递增');
    const expected = Math.ceil(10000 / Math.floor(600 * 0.9));
    assert.equal(positions.length, expected);
  });

  it('矮页面：只返回 [0]（不产生负向/越界滚动）', () => {
    assert.deepEqual(planLazyScroll(300, 600), [0]);
  });

  it('视口高度异常（0）按默认 600 处理', () => {
    const positions = planLazyScroll(10000, 0);
    assert.equal(positions.length, Math.ceil(10000 / Math.max(64, Math.floor(600 * 0.9))));
  });
});
