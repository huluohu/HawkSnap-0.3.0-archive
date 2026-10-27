/**
 * 测试文件的 node 内建模块类型声明（仓库只允许 esbuild 一个 devDependency，
 * 不引入 @types/node —— 这里给出最小 ambient 声明让 `tsc --noEmit` 与编辑器
 * 通过；运行时类型由 node:test 自身保证（esbuild 不做类型检查）。
 */

declare module 'node:test' {
  export function describe(name: string, fn: () => void): void;
  export function it(name: string, fn: () => void): void;
}

declare module 'node:assert/strict';
