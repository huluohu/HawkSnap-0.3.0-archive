#!/usr/bin/env node
// scripts/make-latest.mjs — 组装 Tauri updater 静态源 latest.json（plan D6，G7 更新通道）。
//
// 用法：node scripts/make-latest.mjs --tag v0.1.0 --repo OWNER/REPO --dist <dir> --out <dir>
//       [--platforms darwin-aarch64,linux-x86_64,windows-x86_64]
//
// 在 --dist（publish job 里为 release artifact 合并下载目录）中为每个
// updater 平台键各找一份更新载荷 + 同名 .sig：
//   darwin-aarch64  <- *.app.tar.gz     （tauri build --bundles app 的 updater 归档）
//   linux-x86_64    <- *.AppImage       （v2 updater 直接分发 AppImage 本体）
//   windows-x86_64  <- *-setup.exe      （v2 updater 直接分发 NSIS 安装器本体）
// 语义：任一平台缺失载荷/签名、或多于一份 → exit 1（宁可失败也不出脏 feed；
// .sig 缺失即「未签名产物不得发布」——与 workflow 的 secrets 硬失败同一防线）。
//
// --platforms：本次发布**参与**的平台键（逗号分隔）。未列出的平台不要求载荷、
// 也不出现在 feed 里——0.3.0 起发布策略为 macOS-only（Linux/Windows 编译验证
// 顺延 M4），feed 里写上没有产物的平台键 = 指向 404 的下载链接 = 脏 feed。
// 对**列出的**平台仍是「恰好 1 份 + 签名齐全」的硬判据。省略该参数时行为不变
// （三个平台全要），保持既有调用方兼容。
//
// signature 字段 = .sig 全文（单行 base64(minisign 文本)，tauri signer 产物），
// 仅去首尾空白；url 指向该 tag 的 Release 下载地址。
// version 取自 --tag（去掉 v 前缀），并与 app/src-tauri/tauri.conf.json 的 version 交叉校验，
// 不一致即失败——防止 tag 与应用版本漂移出脏 feed。
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const ALL_PLATFORM_KEYS = ["darwin-aarch64", "linux-x86_64", "windows-x86_64"];

const PAYLOADS = [
  { key: "darwin-aarch64", ends: ".app.tar.gz" },
  { key: "linux-x86_64", ends: ".AppImage" },
  { key: "windows-x86_64", ends: "-setup.exe" },
];

function walk(dir, out = []) {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) walk(p, out);
    else out.push(p);
  }
  return out;
}

const argv = process.argv.slice(2);
const arg = (name) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : undefined;
};
const tag = arg("--tag");
const repo = arg("--repo");
const dist = arg("--dist");
const outDir = arg("--out");
for (const [flag, v] of [["--tag", tag], ["--repo", repo], ["--dist", dist], ["--out", outDir]]) {
  if (!v) {
    console.error(`make-latest: 缺少参数 ${flag}（用法：--tag v0.1.0 --repo OWNER/REPO --dist <dir> --out <dir>）`);
    process.exit(2);
  }
}
if (!/^v\d/.test(tag)) {
  console.error(`make-latest: tag 应形如 v0.1.0，实得：${tag}`);
  process.exit(2);
}

// --platforms：本次发布参与的平台键（省略 = 全部三平台，向后兼容）。
const platformArg = arg("--platforms");
const selectedKeys = platformArg
  ? platformArg.split(",").map((s) => s.trim()).filter(Boolean)
  : ALL_PLATFORM_KEYS;
for (const k of selectedKeys) {
  if (!ALL_PLATFORM_KEYS.includes(k)) {
    console.error(`make-latest: 未知平台键 ${k}（可选：${ALL_PLATFORM_KEYS.join(", ")}）`);
    process.exit(2);
  }
}
if (selectedKeys.length === 0) {
  console.error("make-latest: --platforms 至少要给一个平台键");
  process.exit(2);
}

const files = walk(dist);
const platforms = {};
for (const { key, ends } of PAYLOADS) {
  if (!selectedKeys.includes(key)) continue; // 未参与本次发布的平台 → 不进 feed
  const hits = files.filter((f) => f.endsWith(ends));
  if (hits.length !== 1) {
    console.error(
      `make-latest: ${key} 期望恰好 1 份 *${ends}，实得 ${hits.length}：\n  ${hits.map((f) => relative(dist, f)).join("\n  ") || "(无)"}`,
    );
    process.exit(1);
  }
  const payload = hits[0];
  const sigPath = `${payload}.sig`;
  let signature;
  try {
    signature = readFileSync(sigPath, "utf8").trim();
  } catch {
    console.error(`make-latest: 缺签名文件 ${relative(dist, sigPath)}——未签名产物不得发布`);
    process.exit(1);
  }
  if (!signature) {
    console.error(`make-latest: 空签名 ${relative(dist, sigPath)}`);
    process.exit(1);
  }
  platforms[key] = {
    signature,
    url: `https://github.com/${repo}/releases/download/${tag}/${payload.split("/").pop()}`,
  };
  console.log(`make-latest: ${key} <- ${relative(dist, payload)}`);
}

const version = tag.slice(1);
const tauriConf = JSON.parse(
  readFileSync(fileURLToPath(new URL("../app/src-tauri/tauri.conf.json", import.meta.url)), "utf8"),
);
if (tauriConf.version !== version) {
  console.error(
    `make-latest: 版本不一致——tag ${tag} 推出 version ${version}，但 tauri.conf.json 为 ${tauriConf.version}`,
  );
  process.exit(1);
}

const latest = {
  version,
  notes: `HawkSnap ${tag} — see README for install steps and known limitations.`,
  pub_date: new Date().toISOString(),
  platforms,
};
writeFileSync(join(outDir, "latest.json"), `${JSON.stringify(latest, null, 2)}\n`);
console.log(
  `make-latest: wrote ${join(outDir, "latest.json")} (version ${latest.version}, ${Object.keys(platforms).length} platforms)`,
);
