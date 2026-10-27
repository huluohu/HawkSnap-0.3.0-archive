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
#      `app/src-tauri/binaries/hawksnap-host-<target-triple>`
#      ——配合 tauri.release.conf.json 的
#      `bundle.externalBin: ["binaries/hawksnap-host"]`，
#      bundler 会把它复制进 .app（`Contents/MacOS/hawksnap-host`），
#      **在生成 .app.tar.gz 之前**（这是「宿主随 .app 分发」的唯一可靠路径：
#      打包后再往 .app 里拷已经晚于 tarball 生成）；
#   3. 可选：把宿主注入一个**已构建**的 .app（`--inject <path>`）——兜底路径
#      （若 bundler 因故没带上外部二进制，至少还有一份能用的）；注入后
#      **必须重签名**（拷贝进已签名的 .app 会破坏签名 → macOS 拒绝启动它）。
#
# —— externalBin 的路径形状（评审修复轮 P1）——
# tauri v2 把 externalBin 的每一项当作**文件名的一部分**（不是目录！）：
# tauri-utils `resources::external_binaries` 拼的是
#   `format!("{item}-{target_triple}{ext}")`
# 再由 bundler 相对**tauri 目录**（CLI 的 CWD = app/src-tauri）去找。
# 所以 `externalBin: ["binaries/hawksnap-host"]` 对应的磁盘文件是
#   `app/src-tauri/binaries/hawksnap-host-aarch64-apple-darwin`
# （macOS 的三元组 = `{arch}-{apple-darwin}`，与 `rustc -vV` 的 host 三元组一致）。
# bundler 落进 bundle 时会**去掉 `-<triple>` 后缀** → 最终仍在
# `HawkSnap.app/Contents/MacOS/hawksnap-host`（安装脚本的探测路径不变）。
# ⚠️ 旧写法 `externalBin: ["hawksnap-host"]` + 目录式暂存
# (`binaries/<triple>/hawksnap-host`) 在 tauri v2 下会找
# `app/src-tauri/hawksnap-host-aarch64-apple-darwin` → 不存在 → 打包直接失败。
#
# 用法：
#   scripts/package-native-host.sh                 # release 构建 + 暂存
#   scripts/package-native-host.sh --debug         # debug 构建 + 暂存
#   scripts/package-native-host.sh --inject /path/to/HawkSnap.app
#
# 环境变量：
#   HAWKSNAP_CODESIGN_IDENTITY  --inject 时的签名身份（如 "Developer ID Application: …"）；
#                                 未设置 → adhoc 签名（可启动，但 TCC 授权需重新授予）。
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
    -h|--help) sed -n '2,48p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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
# tauri v2 的形状：`binaries/hawksnap-host-<target-triple>`（见文件头注）。
STAGE_DIR="$REPO_DIR/app/src-tauri/binaries"
mkdir -p "$STAGE_DIR"
STAGED="$STAGE_DIR/hawksnap-host-$TARGET_TRIPLE"
cp "$BIN_SRC" "$STAGED"
chmod +x "$STAGED"
echo "==> 已暂存: $STAGED"

# —— 3) 可选：注入已构建的 .app ——
if [ -n "$INJECT_APP" ]; then
  [ -d "$INJECT_APP" ] || die "--inject 指向的目录不存在: $INJECT_APP"
  DEST_DIR="$INJECT_APP/Contents/MacOS"
  [ -d "$DEST_DIR" ] || die "不是 macOS .app bundle（缺 Contents/MacOS）: $INJECT_APP"
  cp "$BIN_SRC" "$DEST_DIR/hawksnap-host"
  chmod +x "$DEST_DIR/hawksnap-host"
  echo "==> 已注入: $DEST_DIR/hawksnap-host"

  # —— 3.1) 重签名（评审修复轮 P8）——
  # 往**已签名**的 .app 里拷贝文件会破坏 bundle 签名（seal 校验失败）→
  # macOS 直接拒绝启动该 app（`The signature is invalid`）→ 注入的宿主
  # 永远不会被 Chrome 拉起。故注入后必须重签。
  # 签名身份：优先 HAWKSNAP_CODESIGN_IDENTITY（与正式发布同一证书）；
  # 未给 → adhoc 签名（`-`）——能启动，但 TCC 授权（屏幕录制/辅助功能）
  # 会因签名锚点变化而失效，需用户重新授权（ loudly 提醒）。
  SIGN_IDENTITY="${HAWKSNAP_CODESIGN_IDENTITY:-}"
  if [ -z "$SIGN_IDENTITY" ]; then
    SIGN_IDENTITY="-"
    echo "⚠️  未设置 HAWKSNAP_CODESIGN_IDENTITY → 使用 adhoc 签名（'-'）。" >&2
    echo "    adhoc 签名可启动，但屏幕录制/辅助功能授权需用户重新授予。" >&2
  fi
  if command -v codesign >/dev/null 2>&1; then
    codesign --force --sign "$SIGN_IDENTITY" "$DEST_DIR/hawksnap-host" \
      || echo "⚠️  宿主二进制签名失败（继续）" >&2
    codesign --force --deep --sign "$SIGN_IDENTITY" "$INJECT_APP" \
      || echo "⚠️  .app 重签名失败——macOS 可能拒绝启动它（请手动 codesign）" >&2
    echo "==> 已重签名: $INJECT_APP（identity=$SIGN_IDENTITY）"
  else
    echo "⚠️  找不到 codesign —— 注入已破坏 .app 签名，macOS 会拒绝启动它！" >&2
    echo "    请手动执行: codesign --force --deep --sign <identity> \"$INJECT_APP\"" >&2
  fi
fi

cat <<EOF

完成。宿主二进制：
  构建产物: $BIN_SRC
  暂存文件: $STAGED（tauri-bundler 的 externalBin 输入）
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
