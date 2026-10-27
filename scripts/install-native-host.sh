#!/usr/bin/env bash
# scripts/install-native-host.sh —— 注册 Native Messaging 宿主（M4 P1，macOS）。
#
# Chrome/Edge 的 Native Messaging 模型：**浏览器拉起宿主进程**——浏览器读用户
# 目录下的 manifest（`NativeMessagingHosts/<host-name>.json`），按其中的 `path`
# 拉起宿主二进制，再用 stdin/stdout 与它说 JSON。本脚本只做一件事：把这份
# manifest 写到位。
#
# 写两份（Chrome + Edge）——两者的 manifest 目录不同，其余字段一致。
#
# 用法：
#   scripts/install-native-host.sh --ext-id <扩展ID> [--host-path <宿主二进制>] [--dry-run]
#   scripts/install-native-host.sh --uninstall
#
# 扩展 ID：Chrome 扩展页面的 ID（32 位 a-p 字符）。不填则写入占位符
# `<EXT_ID>`——**必须**由用户替换成真实 ID，否则浏览器会以「未知扩展」拒绝
# 连接本宿主（Chrome 会报 "Specified native messaging host not found" 或
# 拒绝未列入 allowed_origins 的来源）。
#
# 宿主二进制：默认按下列顺序探测（找不到就报错并给出构建命令）：
#   1) --host-path 显式指定
#   2) /Applications/HawkSnap.app/Contents/MacOS/hawksnap-host（**随 .app 分发**
#      的那份——由 scripts/package-native-host.sh 注入）
#   3) <repo>/target/release/hawksnap-host（本地开发构建）
#   4) <repo>/target/debug/hawksnap-host
#
# 构建宿主（feature 门控，不进默认构建）：
#   cargo build --release --features native-host --bin hawksnap-host \
#     --manifest-path app/src-tauri/Cargo.toml
#
# 随 .app 分发（发布流程必须跑）：
#   scripts/package-native-host.sh            # 构建宿主 + 注入 HawkSnap.app
#
# 平台：**仅 macOS**（1.0 的浏览器插件只随 macOS 版发布；Linux/Windows 的
# manifest 目录与安装流程不同，见 README 的平台支持表）。

set -euo pipefail

HOST_NAME="com.hawksnap.app"
EXT_ID=""
HOST_PATH=""
DRY_RUN=0
UNINSTALL=0

die() {
  echo "错误: $*" >&2
  exit 1
}

usage() {
  sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --ext-id) EXT_ID="${2:-}"; shift 2 ;;
    --host-path) HOST_PATH="${2:-}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "未知参数: $1（-h 看用法）" ;;
  esac
done

[ "$(uname -s)" = "Darwin" ] || die "本脚本仅支持 macOS（当前: $(uname -s)）"

HOME_DIR="${HOME:?HOME 未设置}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# —— 探测宿主二进制 ——
if [ -z "$HOST_PATH" ]; then
  for candidate in \
    "/Applications/HawkSnap.app/Contents/MacOS/hawksnap-host" \
    "$REPO_DIR/target/release/hawksnap-host" \
    "$REPO_DIR/target/debug/hawksnap-host"; do
    if [ -x "$candidate" ]; then
      HOST_PATH="$candidate"
      break
    fi
  done
fi
if [ -z "$HOST_PATH" ]; then
  die "找不到宿主二进制 hawksnap-host。请先构建：
  cargo build --release --features native-host --bin hawksnap-host --manifest-path app/src-tauri/Cargo.toml
或用 --host-path 指定路径。"
fi
[ -x "$HOST_PATH" ] || die "宿主二进制不可执行: $HOST_PATH"
HOST_PATH="$(cd "$(dirname "$HOST_PATH")" && pwd)/$(basename "$HOST_PATH")"

# —— manifest 目录（Chrome + Edge）——
CHROME_DIR="$HOME_DIR/Library/Application Support/Google/Chrome/NativeMessagingHosts"
EDGE_DIR="$HOME_DIR/Library/Application Support/Microsoft Edge/NativeMessagingHosts"
MANIFEST="$HOST_NAME.json"

if [ "$UNINSTALL" = "1" ]; then
  for dir in "$CHROME_DIR" "$EDGE_DIR"; do
    if [ -f "$dir/$MANIFEST" ]; then
      rm -f "$dir/$MANIFEST"
      echo "已移除: $dir/$MANIFEST"
    fi
  done
  echo "已卸载 Native Messaging 宿主注册。"
  exit 0
fi

ALLOWED_ORIGIN="chrome-extension://${EXT_ID:-<EXT_ID>}/"
if [ -z "$EXT_ID" ]; then
  echo "提示: 未提供 --ext-id，allowed_origins 写入占位符 ${ALLOWED_ORIGIN}" >&2
  echo "      请把 <EXT_ID> 替换成插件的真实扩展 ID 后重新运行本脚本。" >&2
elif ! printf '%s' "$EXT_ID" | grep -Eq '^[a-p]{32}$'; then
  # Chrome 的扩展 ID 是 32 个 a-p 字符（a..p 的 base-16 变体）。写错 → 浏览器
  # 会拒绝连接该宿主，且错因极难排查 → 这里当场拒。
  die "--ext-id 格式非法：应为 32 个 a-p 字符（如 aapbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb），收到: $EXT_ID"
fi

manifest_body() {
  cat <<JSON
{
  "name": "$HOST_NAME",
  "description": "HawkSnap native messaging host (browser capture → desktop app)",
  "path": "$HOST_PATH",
  "type": "stdio",
  "allowed_origins": [
    "$ALLOWED_ORIGIN"
  ]
}
JSON
}

write_manifest() {
  local dir="$1"
  local target="$dir/$MANIFEST"
  # **dry-run 绝不写盘**（旧版是先写再判断 dry-run → 「预览」其实改了系统）。
  if [ "$DRY_RUN" = "1" ]; then
    echo "--- (dry-run) 将写入 $target ---"
    manifest_body
    echo "--- (dry-run) 未写入任何文件 ---"
    return
  fi
  mkdir -p "$dir"
  manifest_body > "$target"
  echo "已写入: $target"
}

write_manifest "$CHROME_DIR"
write_manifest "$EDGE_DIR"

if [ "$DRY_RUN" != "1" ]; then
  cat <<EOF

完成。检查清单：
  1. 扩展 ID 已替换：$( [ -n "$EXT_ID" ] && echo "是（$EXT_ID）" || echo "否 —— 仍需手动替换 <EXT_ID>" )
  2. 宿主二进制路径：$HOST_PATH
  3. HawkSnap.app 正在运行（桌面端监听 127.0.0.1，端口与令牌写在
     ~/Library/Application Support/com.hawksnap.app/native-host.json，权限 0600）
  4. Chrome/Edge 完全退出再重启（manifest 变更需要重启浏览器才生效）
EOF
fi
