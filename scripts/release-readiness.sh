#!/usr/bin/env bash
# scripts/release-readiness.sh —— 1.0 分发就绪门（M4 Task P4）。
#
# 纯静态检查：只读文件 + 跑纯 node（i18n 门禁，无依赖）+ 纯 python/ruby（YAML 语法）。
# **不构建、不联网、不跑 cargo/npm install**——重量级验证（cargo test / clippy /
# npm test+build / tauri build + check-size / bench-* / soak）是 macOS-only 的人工清单，
# 见 docs/superpowers/bench/2026-10-14-m4-release-gate.md。
#
# 用法：bash scripts/release-readiness.sh   （任意 cwd；自动 cd 到仓库根）
# 退出码：0 = 全绿；1 = 有失败项（逐项打印 FAIL 原因）。
set -u
cd "$(dirname "$0")/.." || exit 1

PASS=0
FAIL=0

ok()   { PASS=$((PASS + 1)); printf '  PASS  %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf '  FAIL  %s\n' "$1"; }
skip() { printf '  SKIP  %s\n' "$1"; }

echo "== HawkSnap 1.0 分发就绪门（release readiness gate）=="
echo "repo: $(pwd)"

# ---------- (a) LICENSE 存在 ----------
echo
echo "(a) LICENSE"
if [ -f LICENSE ]; then
  ok "LICENSE 存在（$(wc -c < LICENSE | tr -d ' ') bytes）"
else
  bad "LICENSE 缺失——AGPL-3.0-or-later 分发必须随仓库附带完整许可文本"
fi

# ---------- (b) 版本 1.0.0 五处一致 ----------
echo
echo "(b) 版本一致性（期望 1.0.0，5 处 + Cargo.lock 的 4 个 workspace crate）"
check_grep() { # file pattern label
  if grep -qE "$2" "$1" 2>/dev/null; then
    ok "$3 = 1.0.0"
  else
    bad "$3 不是 1.0.0（$1 未匹配 /$2/）"
  fi
}
check_grep Cargo.toml                    '^version = "1\.0\.0"'            "Cargo.toml [workspace.package]"
check_grep app/src-tauri/Cargo.toml      '^version = "1\.0\.0"'            "app/src-tauri/Cargo.toml [package]"
check_grep app/src-tauri/tauri.conf.json '"version": "1\.0\.0"'           "app/src-tauri/tauri.conf.json"
check_grep app/package.json              '"version": "1\.0\.0"'           "app/package.json"

# (c) app/package-lock.json 根版本（顶层 "version" 与 packages[""].version 两处都必须是 1.0.0）
if grep -qE '^  "version": "1\.0\.0",' app/package-lock.json \
  && grep -qE '^      "version": "1\.0\.0",' app/package-lock.json; then
  ok "app/package-lock.json 根版本（顶层 + packages[\"\"]）= 1.0.0"
else
  bad "app/package-lock.json 根版本不是 1.0.0（需同时改顶层与 packages[\"\"] 两处）"
fi

# Cargo.lock：hawksnap / hawksnap-core / hawksnap-ocr / hawksnap-platform-mac 四个 crate
for crate in hawksnap hawksnap-core hawksnap-ocr hawksnap-platform-mac; do
  if awk -v crate="$crate" '
      $1 == "name" && $3 == "\""crate"\"" { seen=1; next }
      seen && $1 == "version" { print ($3 == "\"1.0.0\"") ? "yes" : "no"; exit }
      seen && $1 == "name" { print "no"; exit }' Cargo.lock | grep -q yes; then
    ok "Cargo.lock: $crate = 1.0.0"
  else
    bad "Cargo.lock: $crate 不是 1.0.0（手改 name/version 相邻两行）"
  fi
done

# ---------- (d) i18n 门禁（纯 node，只读文件） ----------
echo
echo "(d) i18n key 覆盖率（scripts/check-i18n.mjs，纯 node 无依赖）"
if command -v node >/dev/null 2>&1; then
  if node scripts/check-i18n.mjs; then
    ok "check-i18n 通过（A⊆Z 且 A⊆E 且 Z==E）"
  else
    bad "check-i18n 失败（见上方缺失/孤儿 key 清单）"
  fi
else
  bad "node 不可用——无法跑 i18n 门禁（PENDING：在有 node 的机器执行 scripts/check-i18n.mjs）"
fi

# ---------- (e) workflow YAML 可解析 ----------
echo
echo "(e) workflow YAML 语法"
yaml_files=$(find .github/workflows -maxdepth 1 -name '*.yml' 2>/dev/null)
if [ -z "$yaml_files" ]; then
  bad "找不到 .github/workflows/*.yml"
else
  if python3 -c 'import yaml' >/dev/null 2>&1; then
    if python3 -c 'import yaml,sys; [yaml.safe_load(open(f, encoding="utf-8")) for f in sys.argv[1:]]' $yaml_files; then
      ok "python3+PyYAML 解析通过：$yaml_files"
    else
      bad "workflow YAML 解析失败（见上方报错）"
    fi
  elif command -v ruby >/dev/null 2>&1; then
    if ruby -ryaml -e 'ARGV.each { |f| YAML.load_file(f) }' $yaml_files >/dev/null; then
      ok "ruby+psych 解析通过：${yaml_files}（本机无 PyYAML，走 ruby 后备）"
    else
      bad "workflow YAML 解析失败（ruby psych 报错）"
    fi
  else
    bad "本机既无 python3+PyYAML 也无 ruby，无法验证 YAML（安装其一后重跑）"
  fi
fi

# ---------- (f) release job 里有源码归档命令（AGPL §6 对应源码） ----------
echo
echo "(f) 源码包（AGPL §6 corresponding source）"
if awk '/^  release:/{inrel=1; next} /^  [a-zA-Z-]+:/{inrel=0} inrel' .github/workflows/ci.yml \
    | grep -q 'git archive'; then
  ok "release job 内含 git archive 源码归档步骤"
else
  bad "release job 内未找到 git archive——二进制发布必须附对应源码（AGPL §6）"
fi

# ---------- (g) plugin/dist 已 gitignore ----------
echo
echo "(g) 插件构建产物不入库"
if git check-ignore -q -- plugin/dist/ 2>/dev/null || grep -qE '^plugin/dist/?$' .gitignore 2>/dev/null; then
  ok "plugin/dist/ 已 gitignore"
else
  bad "plugin/dist/ 未被 gitignore（构建产物不得入库）"
fi

# ---------- (h) 第三方声明提及 gifski（AGPL 触发源） ----------
echo
echo "(h) LICENSE-THIRD-PARTY.md"
if [ -f LICENSE-THIRD-PARTY.md ]; then
  if grep -qi 'gifski' LICENSE-THIRD-PARTY.md && grep -q 'AGPL' LICENSE-THIRD-PARTY.md; then
    ok "第三方声明含 gifski（AGPL-3.0-or-later 触发源）及其分发义务"
  else
    bad "LICENSE-THIRD-PARTY.md 未完整声明 gifski / AGPL（传染性许可必须逐字声明）"
  fi
else
  bad "LICENSE-THIRD-PARTY.md 缺失"
fi

# ---------- 汇总 ----------
echo
echo "== 结果：$PASS 项通过，$FAIL 项失败 =="
if [ "$FAIL" -gt 0 ]; then
  echo
  echo "就绪门未通过：修复上方 FAIL 项后重跑。"
  echo "注意：本脚本只覆盖静态项；发布前还须完成 macOS-only 人工清单"
  echo "（cargo test/clippy、npm test/build、tauri build + check-size、bench-*、soak），"
  echo "以及 GitHub 发布前置（repo public、签名 secrets、push tag v1.0.0）——"
  echo "见 docs/superpowers/bench/2026-10-14-m4-release-gate.md。"
  exit 1
fi
echo "静态就绪项全绿。发布前仍须完成 macOS 人工清单与 GitHub 发布前置（见上方路径）。"
exit 0
