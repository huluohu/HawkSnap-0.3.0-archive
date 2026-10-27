#!/usr/bin/env bash
# scripts/check-size.sh — 安装包体积断言（G1: ≤15MB）
#
# 断言的必须是**真正发布出去的那个 dmg**：评审修复轮 P2 之前，本脚本量的是
# size job 用**不带** tauri.release.conf.json 构建出来的 dmg —— 那个 dmg 里
# **没有** Native Messaging 宿主（externalBin 只在 release 配置里开），而发布
# 的 dmg 有 → 体积门对发布件不成立。
# 现在 size job 与 release job 用同一套 externalBin 配置构建（见 .github/
# workflows/ci.yml），本脚本额外**断言宿主确实在 .app 里**，防止配置漂移 /
# 暂存步骤被跳过时又量到一个「偏小的假包」。
set -euo pipefail

# Cargo workspace 共享根 target/：用 cargo metadata 权威解析目标目录；
# brief 中的 app/src-tauri/target 仅作独立构建时的兜底路径。
# || true：pipefail 下无匹配时 ls 非零会导致赋值失败、脚本静默退出，必须吞掉。
TARGET_DIR=$(cargo metadata --format-version 1 --no-deps | python3 -c "import sys,json;print(json.load(sys.stdin)['target_directory'])")
DMG=$(ls "$TARGET_DIR"/release/bundle/dmg/*.dmg 2>/dev/null | head -1 || true)
if [ -z "$DMG" ]; then
  DMG=$(ls app/src-tauri/target/release/bundle/dmg/*.dmg 2>/dev/null | head -1 || true)
fi
[ -z "$DMG" ] && { echo "未找到 dmg：请先在 app/ 目录运行 npm run tauri build"; exit 1; }

# —— 断言：被量的这个 dmg 的 .app 里必须带宿主 ——
# dmg 与 .app 出自同一次构建（dmg 由该 .app 打成），故检查 .app 即可
# （不必挂载 dmg —— 挂载在 CI 里既慢又需要交互式许可）。
APP_DIR="$(cd "$(dirname "$DMG")/.." && pwd)/macos/HawkSnap.app"
HOST_BIN="$APP_DIR/Contents/MacOS/hawksnap-host"
if [ ! -x "$HOST_BIN" ]; then
  echo "体积门失败：构建产物里没有 Native Messaging 宿主（$HOST_BIN 不存在）。" >&2
  echo "  说明本次构建没带 externalBin —— 量到的 dmg 比发布件小，15MB 断言不成立。" >&2
  echo "  修法：先跑 scripts/package-native-host.sh，再用带 externalBin 的配置构建" >&2
  echo "        （--config src-tauri/tauri.host.conf.json）。" >&2
  exit 1
fi

SIZE_MB=$(du -m "$DMG" | cut -f1)
echo "dmg = ${SIZE_MB}MB ($DMG)"
echo "宿主 = $HOST_BIN（$(du -m "$HOST_BIN" | cut -f1)MB，已计入上面的 dmg）"
[ "$SIZE_MB" -le 15 ] || { echo "超过 15MB 预算"; exit 1; }
