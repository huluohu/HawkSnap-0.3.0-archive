/**
 * 构建脚本（唯一 devDependency：esbuild）。
 *
 * 入口 → dist/（gitignored）：
 *   src/service-worker.ts   → dist/service-worker.js        （manifest background）
 *   src/content-script.ts   → dist/content-script.js        （executeScript 注入）
 *   src/offscreen.ts        → dist/offscreen.js             （offscreen.html 引用）
 *   src/popup.ts            → dist/popup/popup.js           （popup/popup.html 引用）
 *   src/editor.ts           → dist/editor/editor.js         （editor/editor.html 引用）
 *   src/options.ts          → dist/options/options.js       （options/options.html 引用）
 * 静态资源：manifest.json + src/html/*.html → 对应 dist 路径；图标由 gen-icons 生成。
 *
 * format=iife（无 import/export 残留）：content script 与页面 <script> 必须是
 * 经典脚本；SW 同样按经典脚本加载。代码零运行时依赖（ledger Ruling 3）。
 */

import { build } from 'esbuild';
import { cpSync, mkdirSync, rmSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { generateIcons } from './gen-icons.mjs';

const ROOT = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const DIST = path.join(ROOT, 'dist');

rmSync(DIST, { recursive: true, force: true });
mkdirSync(DIST, { recursive: true });

await build({
  absWorkingDir: ROOT,
  entryPoints: [
    { in: 'src/service-worker.ts', out: 'service-worker' },
    { in: 'src/content-script.ts', out: 'content-script' },
    { in: 'src/offscreen.ts', out: 'offscreen' },
    { in: 'src/popup.ts', out: 'popup/popup' },
    { in: 'src/editor.ts', out: 'editor/editor' },
    { in: 'src/options.ts', out: 'options/options' },
  ],
  outdir: DIST,
  bundle: true,
  format: 'iife',
  platform: 'browser',
  target: ['chrome116'],
  sourcemap: false,
  minify: false, // 扩展体积小（零依赖），保留可读性便于排查
  logLevel: 'info',
});

// 静态资源：manifest 在 dist 根；HTML 各就各位（与 manifest 引用路径一致）。
cpSync(path.join(ROOT, 'manifest.json'), path.join(DIST, 'manifest.json'));
const htmlSrc = path.join(ROOT, 'src', 'html');
mkdirSync(path.join(DIST, 'popup'), { recursive: true });
mkdirSync(path.join(DIST, 'editor'), { recursive: true });
mkdirSync(path.join(DIST, 'options'), { recursive: true });
cpSync(path.join(htmlSrc, 'offscreen.html'), path.join(DIST, 'offscreen.html'));
cpSync(path.join(htmlSrc, 'popup.html'), path.join(DIST, 'popup', 'popup.html'));
cpSync(path.join(htmlSrc, 'editor.html'), path.join(DIST, 'editor', 'editor.html'));
cpSync(path.join(htmlSrc, 'options.html'), path.join(DIST, 'options', 'options.html'));

// 图标：构建时生成（仓库不存二进制，manifest 的 icons 路径在 dist 下成立）。
generateIcons(path.join(DIST, 'icons'));

console.log(`[hawksnap] 构建完成 → ${DIST}`);
