/**
 * 测试脚本（node:test 内建 runner，零额外依赖）。
 *
 * 用 esbuild 把 tests/*.test.ts 打成 CJS 到临时目录，再 `node --test` 执行。
 * 被测对象全部是纯逻辑模块（shared/*）：截图尺寸策略、协议帧构造/解析、
 * 标注几何 —— 不依赖 chrome.*（协议/策略的 chrome 环境假设见各文件注释）。
 */

import { build } from 'esbuild';
import { spawnSync } from 'node:child_process';
import { readdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const ROOT = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const testsDir = path.join(ROOT, 'tests');
const tests = readdirSync(testsDir).filter((f) => f.endsWith('.test.ts'));
if (tests.length === 0) {
  console.error('[hawksnap] tests/ 下没有 .test.ts');
  process.exit(1);
}

const outDir = mkdtempSync(path.join(tmpdir(), 'hawksnap-plugin-test-'));
try {
  await build({
    absWorkingDir: ROOT,
    entryPoints: tests.map((f) => path.join('tests', f)),
    outdir: outDir,
    bundle: true,
    format: 'cjs',
    platform: 'node',
    target: 'node18',
    sourcemap: false,
    logLevel: 'warning',
  });

  const outFiles = tests.map((f) => path.join(outDir, f.replace(/\.ts$/, '.js')));
  const r = spawnSync(process.execPath, ['--test', ...outFiles], { stdio: 'inherit' });
  process.exitCode = r.status ?? 1;
} finally {
  rmSync(outDir, { recursive: true, force: true });
}
