#!/usr/bin/env bash
# scripts/package-native-host.sh —— 构建并打包 Native Messaging 宿主（M4 P1）。
#
# 背景（打包缺口）：宿主二进制 `hawksnap-host` 是 **feature 门控**的（不进默认
# 构建——M3 N4 教训：第二个默认 bin 会让 `tauri build` 找错主程序）。但浏览器
# 需要一个**磁盘上真实存在**的可执行文件：Chrome 按 manifest 的 `path` 拉起它。
# 若不打包，manifest 指向的路径不存在 → 插件永远连不上宿主。
#
# 本脚本做三件事：
#   1. 用 `native-host` feature 构建宿主（release/debug）；
#   2. **暂存**到 tauri-bundler 的 sidecar 目录
#      `app/src-tauri/binaries/<target-triple>/hawksnap-host`
#      ——配合 tauri.release.conf.json 的 `bundle.externalBin`，
#      bundler 会把它复制进 .app（`Contents/MacOS/hawksnap-host`），
#      **在生成 .app.tar.gz 之前**（这是「宿主随 .app 分发」的唯一可靠路径：
#      打包后再往 .app 里拷已经晚于 tarball 生成）；
#   3. 可选：把宿主注入一个**已构建**的 .app（`--inject <path>`）——兜底路径
#      （若 bundler 因故没带上外部二进制，至少还有一份能用的）。
#
# 用法：
#   scripts/package-native-host.sh                 # release 构建 + 暂存
#   scripts/package-native-host.sh --debug         # debug 构建 + 暂存
#   scripts/package-native-host.sh --inject /path/to/HawkSnap.app
#
# 发布流程（macOS，CI release job 同款）：
#   scripts/package-native-host.sh
#   cd app && npm run tauri build -- --config src-tauri/tauri.release.conf.json \
#       --bundles app dmg
#
# 之后 Chrome 的 manifest 里写：
#   "path": "/Applications/HawkSnap.app/Contents/MacOS/hawksnap-host"
# （scripts/install-native-host.sh 会自动探测这个路径）

set -euo pipefail

PROFILE="release"
INJECT_APP=""
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST_PATH="$REPO_DIR/app/src-tauri/Cargo.toml"

die() {
  echo "错误: $*" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --debug) PROFILE="debug"; shift ;;
    --release) PROFILE="release"; shift ;;
    --inject) INJECT_APP="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "未知参数: $1" ;;
  esac
done

command -v cargo >/dev/null 2>&1 || die "找不到 cargo（宿主是 Rust 二进制）"

# —— 目标三元组（暂存目录名要用）——
TARGET_TRIPLE="$(rustc -vV | awk '/^host:/ {print $2}')"
[ -n "$TARGET_TRIPLE" ] || die "无法从 rustc -vV 解析 target triple"

# —— 1) 构建宿主 ——
echo "==> 构建宿主（profile=$PROFILE, feature=native-host）"
cargo build \
  --manifest-path "$MANIFEST_PATH" \
  $( [ "$PROFILE" = "release" ] && echo "--release" ) \
  --features native-host \
  --bin hawksnap-host

BIN_SRC="$REPO_DIR/target/$PROFILE/hawksnap-host"
[ -x "$BIN_SRC" ] || die "构建产物不存在: $BIN_SRC"

# —— 2) 暂存给 tauri-bundler（externalBin）——
STAGE_DIR="$REPO_DIR/app/src-tauri/binaries/$TARGET_TRIPLE"
mkdir -p "$STAGE_DIR"
cp "$BIN_SRC" "$STAGE_DIR/hawksnap-host"
chmod +x "$STAGE_DIR/hawksnap-host"
echo "==> 已暂存: $STAGE_DIR/hawksnap-host"

# —— 3) 可选：注入已构建的 .app ——
if [ -n "$INJECT_APP" ]; then
  [ -d "$INJECT_APP" ] || die "--inject 指向的目录不存在: $INJECT_APP"
  DEST_DIR="$INJECT_APP/Contents/MacOS"
  [ -d "$DEST_DIR" ] || die "不是 macOS .app bundle（缺 Contents/MacOS）: $INJECT_APP"
  cp "$BIN_SRC" "$DEST_DIR/hawksnap-host"
  chmod +x "$DEST_DIR/hawksnap-host"
  echo "==> 已注入: $DEST_DIR/hawksnap-host"
fi

cat <<EOF

完成。宿主二进制：
  构建产物: $BIN_SRC
  暂存目录: $STAGE_DIR/hawksnap-host（tauri-bundler 的 externalBin 输入）
EOF
if [ -z "$INJECT_APP" ]; then
  cat <<EOF
下一步（发布构建）:
  cd app && npm run tauri build -- --config src-tauri/tauri.release.conf.json --bundles app dmg
EOF
fi
cat <<EOF
安装（用户侧）:
  scripts/install-native-host.sh --ext-id <扩展ID>
  （manifest 指向 /Applications/HawkSnap.app/Contents/MacOS/hawksnap-host）
EOF
