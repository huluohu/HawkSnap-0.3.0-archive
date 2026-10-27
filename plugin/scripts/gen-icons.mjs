/**
 * 图标生成（零依赖：纯 Node zlib + 手写 PNG 容器）。
 *
 * 产物：icon16/32/48/128.png —— 墨色圆角方块（#0C1624）+ 琥珀隼翼折形（#FFB020）
 * + 蓝色眼点（#4dabf7），与产品视觉一致（鹰隼蓝 / 琥珀 / 墨色）。
 * 4× 超采样抗锯齿；写进目标目录（构建时 dist/icons/，`npm run icons` 时 plugin/icons/）。
 *
 * Usage: node scripts/gen-icons.mjs [targetDir]
 */

import { deflateSync } from 'node:zlib';
import { mkdirSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

// —— PNG 容器 ——

const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c;
  }
  return table;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const out = Buffer.alloc(8 + data.length + 4);
  out.writeUInt32BE(data.length, 0);
  out.write(type, 4, 'ascii');
  data.copy(out, 8);
  out.writeUInt32BE(crc32(out.subarray(4, 8 + data.length)), 8 + data.length);
  return out;
}

/** RGBA → PNG（filter 0 扫描线，zlib deflate）。 */
function encodePng(width, height, rgba) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // color type RGBA
  const stride = width * 4;
  const raw = Buffer.alloc((stride + 1) * height);
  for (let y = 0; y < height; y++) {
    raw[y * (stride + 1)] = 0; // filter: none
    rgba.copy(raw, y * (stride + 1) + 1, y * stride, (y + 1) * stride);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

// —— 图案 ——

const BG = [12, 22, 36]; // #0C1624 墨
const WING = [255, 176, 32]; // #FFB020 琥珀
const EYE = [77, 171, 247]; // #4dabf7 蓝

/** 单位坐标（0..1）下的圆角方块掩码。 */
function inRoundedSquare(x, y, radius) {
  const r = radius;
  const cx = Math.min(Math.max(x, r), 1 - r);
  const cy = Math.min(Math.max(y, r), 1 - r);
  const dx = x - cx;
  const dy = y - cy;
  return dx * dx + dy * dy <= r * r;
}

/** 单位坐标下的隼翼折形（凸多边形，cross product 同侧判定）。 */
const WING_POLY = [
  [0.14, 0.70],
  [0.40, 0.26],
  [0.52, 0.50],
  [0.86, 0.30],
  [0.60, 0.72],
];

function inPolygon(poly, x, y) {
  let sign = 0;
  for (let i = 0; i < poly.length; i++) {
    const [x1, y1] = poly[i];
    const [x2, y2] = poly[(i + 1) % poly.length];
    const cross = (x2 - x1) * (y - y1) - (y2 - y1) * (x - x1);
    if (cross !== 0) {
      const s = cross > 0 ? 1 : -1;
      if (sign === 0) sign = s;
      else if (s !== sign) return false;
    }
  }
  return true;
}

function inEye(x, y) {
  const dx = x - 0.30;
  const dy = y - 0.38;
  return dx * dx + dy * dy <= 0.0036; // r = 0.06
}

/** 渲染 size×size RGBA。 */
function render(size) {
  const rgba = Buffer.alloc(size * size * 4);
  const SS = 4; // 每轴超采样
  const radius = 0.22;
  for (let py = 0; py < size; py++) {
    for (let px = 0; px < size; px++) {
      let r = 0;
      let g = 0;
      let b = 0;
      let a = 0;
      for (let sy = 0; sy < SS; sy++) {
        for (let sx = 0; sx < SS; sx++) {
          const u = (px + (sx + 0.5) / SS) / size;
          const v = (py + (sy + 0.5) / SS) / size;
          if (!inRoundedSquare(u, v, radius)) continue;
          a += 255;
          if (inEye(u, v)) {
            r += EYE[0];
            g += EYE[1];
            b += EYE[2];
          } else if (inPolygon(WING_POLY, u, v)) {
            r += WING[0];
            g += WING[1];
            b += WING[2];
          } else {
            r += BG[0];
            g += BG[1];
            b += BG[2];
          }
        }
      }
      const samples = SS * SS;
      const idx = (py * size + px) * 4;
      rgba[idx] = Math.round(r / samples);
      rgba[idx + 1] = Math.round(g / samples);
      rgba[idx + 2] = Math.round(b / samples);
      rgba[idx + 3] = Math.round(a / samples);
    }
  }
  return rgba;
}

const SIZES = [16, 32, 48, 128];

export function generateIcons(targetDir) {
  mkdirSync(targetDir, { recursive: true });
  const written = [];
  for (const size of SIZES) {
    const file = path.join(targetDir, `icon${size}.png`);
    writeFileSync(file, encodePng(size, size, render(size)));
    written.push(path.basename(file));
  }
  return written;
}

const SELF = fileURLToPath(import.meta.url);
if (process.argv[1] && path.resolve(process.argv[1]) === SELF) {
  const target = process.argv[2] ?? path.join(path.dirname(path.dirname(SELF)), 'icons');
  const files = generateIcons(target);
  console.log(`[hawksnap] 图标已生成 → ${target}: ${files.join(', ')}`);
}
