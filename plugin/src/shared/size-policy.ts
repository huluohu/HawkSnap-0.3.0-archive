/**
 * 整页截图的尺寸策略（spec §3.5：10000px 高页面整页保真（含懒加载图））。
 *
 * 约束来源：
 *  - Chrome 渲染面单边 ~16384px 的安全上限（GPU 纹理 / captureScreenshot 对
 *    超大表面的失败或空白输出）；
 *  - 桌面端桥的 40 MP 像素闸（native_protocol.rs MAX_CAPTURE_PIXELS，
 *    1920×10000 ≈ 19.2 MP 在限内；超限帧被桌面端直接拒收）。
 *
 * 策略：优先原尺寸（保真）；超高/超宽/超像素时**整体降采样**（压 deviceScaleFactor，
 * 不裁内容）；降到 dsf 下限仍放不下才**裁掉放不下的部分**（最后手段，明确标记）。
 * 纯函数，可独立测试（scripts/test.mjs → node --test）。
 */

/** 渲染面单边安全上限（设备像素）。 */
export const MAX_SURFACE_EDGE_PX = 16_384;

/** 桌面端桥的像素闸（native_protocol::MAX_CAPTURE_PIXELS = 40 MP）。 */
export const MAX_BRIDGE_PIXELS = 40_000_000;

/** deviceScaleFactor 下限：再往下压文本已不可读，宁可裁剪。 */
export const MIN_DEVICE_SCALE_FACTOR = 0.1;

export type CapturePolicyInput = {
  /** 页面 CSS 宽（Page.getLayoutMetrics contentSize）。 */
  cssWidth: number;
  /** 页面 CSS 高。 */
  cssHeight: number;
  /** 当前 devicePixelRatio（含浏览器缩放）。 */
  devicePixelRatio: number;
};

export type CapturePolicy = {
  /** 传给 Emulation.setDeviceMetricsOverride 的 deviceScaleFactor。 */
  deviceScaleFactor: number;
  /** 传给 setDeviceMetricsOverride 的 CSS 尺寸（= 想要渲染的页面区域）。 */
  captureCssWidth: number;
  captureCssHeight: number;
  /** 产出的设备像素尺寸（captureScreenshot 的实际结果，供帧回执/编辑器使用）。 */
  outWidth: number;
  outHeight: number;
  /** 是否发生了整体降采样（保真损失，UI 可提示）。 */
  downscaled: boolean;
  /** 是否发生了裁剪（dsf 已到下限仍放不下，内容不完整）。 */
  cropped: boolean;
};

function clampInt(v: number, min: number): number {
  return Math.max(min, Math.round(v));
}

/**
 * 计算整页截图策略。
 *
 * 步骤：
 *  1. 原始设备像素 = css × dpr，若超出单边上限 → 等比压 scale（双边同压）；
 *  2. 压完像素数仍超 40 MP → 继续等比压到 40 MP 以内；
 *  3. effective dsf = dpr × scale；若低于下限 → 钉在下限，
 *     此时若仍超限 → 裁剪 CSS 尺寸到能放下的最大区域（cropped=true）。
 */
export function computeCapturePolicy(input: CapturePolicyInput): CapturePolicy {
  const { cssWidth, cssHeight } = input;
  const dpr = input.devicePixelRatio > 0 ? input.devicePixelRatio : 1;

  const rawWidth = cssWidth * dpr;
  const rawHeight = cssHeight * dpr;
  let scale = 1;

  // 1) 单边上限
  if (rawWidth > MAX_SURFACE_EDGE_PX) scale = Math.min(scale, MAX_SURFACE_EDGE_PX / rawWidth);
  if (rawHeight > MAX_SURFACE_EDGE_PX) scale = Math.min(scale, MAX_SURFACE_EDGE_PX / rawHeight);

  // 2) 桥端像素闸
  const pixels = rawWidth * scale * (rawHeight * scale);
  if (pixels > MAX_BRIDGE_PIXELS) {
    scale *= Math.sqrt(MAX_BRIDGE_PIXELS / pixels);
  }

  // 3) dsf 下限与裁剪
  let dsf = dpr * scale;
  let cropped = false;
  let captureCssWidth = cssWidth;
  let captureCssHeight = cssHeight;
  if (dsf < MIN_DEVICE_SCALE_FACTOR) {
    dsf = MIN_DEVICE_SCALE_FACTOR;
    captureCssWidth = Math.min(cssWidth, Math.floor(MAX_SURFACE_EDGE_PX / dsf));
    captureCssHeight = Math.min(cssHeight, Math.floor(MAX_SURFACE_EDGE_PX / dsf));
    const croppedPixels = captureCssWidth * dsf * (captureCssHeight * dsf);
    if (croppedPixels > MAX_BRIDGE_PIXELS) {
      // 裁剪后仍超 40 MP（极端极端场景）：再按像素闸裁高度。
      const over = Math.sqrt(MAX_BRIDGE_PIXELS / croppedPixels);
      captureCssHeight = Math.floor(captureCssHeight * over);
    }
    cropped = true;
  }

  return {
    deviceScaleFactor: dsf,
    captureCssWidth: clampInt(captureCssWidth, 1),
    captureCssHeight: clampInt(captureCssHeight, 1),
    outWidth: clampInt(captureCssWidth * dsf, 1),
    outHeight: clampInt(captureCssHeight * dsf, 1),
    downscaled: dsf < dpr - 1e-9,
    cropped,
  };
}

// —— 懒加载图（spec §3.5「含懒加载图」）：有界滚动 + 有界等待 ——

/** 每步滚动间隔（视口高的倍数）。 */
export const LAZY_SCROLL_VIEWPORTS = 0.9;

/** 每步之间等待图片开始/继续加载的最短间隔。 */
export const LAZY_STEP_DELAY_MS = 120;

/** 单页懒加载等待总预算：超出即放弃等待、直接截图（有界，不无限挂起）。 */
export const LAZY_TOTAL_BUDGET_MS = 8_000;

/** 单步内等待未完成图片的最长时间（单张图片卡死时不拖垮整体）。 */
export const LAZY_STEP_MAX_WAIT_MS = 1_500;

export type SettleDecision = 'continue' | 'wait' | 'give-up';

/**
 * 懒加载等待的有界决策（每步轮询调用）：
 *  - 预算耗尽 → give-up（截图照常进行，注明可能有图未加载）；
 *  - 还有未完成图片且单步等待未超时 → wait；
 *  - 全部就绪 → continue（进入下一步滚动 / 收尾）。
 */
export function imageSettleDecision(input: {
  /** 文档内图片总数。 */
  totalImages: number;
  /** 已完成（complete 且 naturalWidth>0，或本身就是 0 尺寸占位）的图片数。 */
  settledImages: number;
  /** 本次决策已连续等待的毫秒数。 */
  stepElapsedMs: number;
  /** 懒加载阶段已累计消耗的毫秒数。 */
  totalElapsedMs: number;
}): SettleDecision {
  if (input.totalElapsedMs >= LAZY_TOTAL_BUDGET_MS) return 'give-up';
  if (input.settledImages >= input.totalImages) return 'continue';
  if (input.stepElapsedMs >= LAZY_STEP_MAX_WAIT_MS) return 'continue';
  return 'wait';
}

/**
 * 懒加载滚动计划：从 0 滚到底再回顶，每步 LAZY_SCROLL_VIEWPORTS 个视口高。
 * 返回滚动位置列表（CSS px），供 Runtime.evaluate 逐步执行。
 * 纯函数：页面比一屏还矮 → 只滚 [0]（仍保留一次回顶）。
 */
export function planLazyScroll(pageHeightCss: number, viewportHeightCss: number): number[] {
  const vh = viewportHeightCss > 0 ? viewportHeightCss : 600;
  const step = Math.max(64, Math.floor(vh * LAZY_SCROLL_VIEWPORTS));
  const positions: number[] = [];
  for (let y = 0; y < pageHeightCss; y += step) {
    positions.push(y);
  }
  if (positions.length === 0) positions.push(0);
  return positions;
}
