#!/usr/bin/env bash
# scripts/bench-latency.sh [轮数] — G3 截图延迟基准（**单命令口径**，spec G3）
#
# 用法：
#   bash scripts/bench-latency.sh            # 默认 20 轮
#   bash scripts/bench-latency.sh 30         # 自定义轮数
#   HAWKSNAP_BIN=/path/to/hawksnap bash scripts/bench-latency.sh
#
# 原理（为什么不再需要人工按 20 次 F1）：
#   应用侧有**基准模式**（HAWKSNAP_BENCH=<轮数>，app/src-tauri/src/bench_mode.rs）：
#   启动后自动跑 N 轮真实的「捕获 → 覆盖层出现 → 确认 → 进剪贴板」链路（覆盖层
#   不可见、无键鼠注入、不抢焦点），**计时全部在进程内完成**，结束时向 stderr
#   打印两行 P90：
#       [bench] h2o P90 = NNms (n=..)   # 热键 → 覆盖层出现（目标 ≤150ms）
#       [bench] c2c P90 = NNms (n=..)   # 确认 → 进剪贴板   （目标 ≤100ms）
#   本脚本负责：定位二进制 → 以 HAWKSNAP_BENCH=N 启动 → 收集 stderr → 抽这两行
#   → 打印 + 判定目标线。**用户只需跑这一条命令就能拿到两个数。**
#
# 前置（须知）：
#   * HawkSnap **不得正在运行**——单实例锁会让测量实例秒退（脚本会提前检查）。
#   * 需要已构建的二进制（优先 target/release/hawksnap；可用 HAWKSNAP_BIN 覆盖）。
#     构建：cd app && npm run build && cd .. && cargo build --release。
#   * 副作用：每轮覆盖剪贴板（被测路径就是「复制到剪贴板」）；屏幕录制权限须已授予。
#
# 口径说明：
#   * 第 1 轮无条件排除（webview 冷启动 = 预热成本，非稳态延迟），第 1 轮之外的
#     失败轮（覆盖层/剪贴板超时）同样排除——进程内 bench 模式自己的口径。
#   * CSV 回退口径（历史兼容）：若 stderr 里没有那两行（例如手动跑的应用没开
#     基准模式），脚本回退到解析 perf CSV（$TMPDIR/hawksnap-perf/*.csv），
#     按「hotkey / overlay_shown / confirm / clipboard_done」四列配对算 P90。
#     该回退要求 CSV 落盘闸打开（仅基准模式 / G4 模式写 CSV）。
set -euo pipefail

ROUNDS="${1:-20}"
case "$ROUNDS" in ''|*[!0-9]*) echo "[g3] 轮数必须是正整数：$ROUNDS" >&2; exit 2;; esac
[ "$ROUNDS" -ge 1 ] || { echo "[g3] 轮数必须 ≥1" >&2; exit 2; }

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# —— ① 定位二进制 ——
BIN="${HAWKSNAP_BIN:-}"
if [ -z "$BIN" ]; then
  if [ -x "$ROOT/target/release/hawksnap" ]; then
    BIN="$ROOT/target/release/hawksnap"
  elif [ -x "$ROOT/target/debug/hawksnap" ]; then
    BIN="$ROOT/target/debug/hawksnap"
    echo "[g3] 未找到 release 二进制，退回 debug（数值会偏慢，验收请用 release）" >&2
  fi
fi
[ -n "$BIN" ] && [ -x "$BIN" ] || {
  echo "[g3] 找不到可执行的 hawksnap 二进制。先构建：cd app && npm run build && cd .. && cargo build --release" >&2
  exit 2
}

# —— ② 单实例锁预检 ——
# 已有实例在跑 → 测量实例会因单实例锁秒退（bench 模式跑不到，stderr 里没有 P90）。
if pgrep -f "target/release/hawksnap" >/dev/null 2>&1 || pgrep -f "MacOS/hawksnap" >/dev/null 2>&1; then
  echo "[g3] 检测到 HawkSnap 正在运行——单实例锁会让测量实例秒退。请先退出应用再跑本脚本。" >&2
  exit 2
fi

TMP="$(mktemp -d /tmp/hawksnap-bench.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# —— ③ 以基准模式启动应用 ——
HAWKSNAP_BENCH="$ROUNDS" "$BIN" >/dev/null 2>"$TMP/bench.err" &
PID=$!

# 兜底看门狗：正常跑完 N 轮即 app.exit(0)；超时（挂死）视为失败并 kill。
# 上限：启动 1.5s + 每轮 6s 预算 + 30s 余量，钳在 600s 内。
TIMEOUT=$(( 30 + ROUNDS * 6 ))
[ "$TIMEOUT" -gt 600 ] && TIMEOUT=600
( sleep "$TIMEOUT"; kill -9 "$PID" 2>/dev/null; exit 0 ) & WD=$!
wait "$PID" || true
kill "$WD" 2>/dev/null || true
wait "$WD" 2>/dev/null || true

# —— ④ 抽进程内 P90（首选口径）——
H2O_LINE="$(grep -oE '\[bench\] h2o P90 = [0-9]+ms' "$TMP/bench.err" | tail -1 || true)"
C2C_LINE="$(grep -oE '\[bench\] c2c P90 = [0-9]+ms' "$TMP/bench.err" | tail -1 || true)"

if [ -n "$H2O_LINE" ] && [ -n "$C2C_LINE" ]; then
  # 行形如 "[bench] h2o P90 = 123ms (n=19)" → 取 "= <数字>ms" 里的数字（别抓 n=）。
  H2O="$(printf '%s' "$H2O_LINE" | sed -E 's/.*= ([0-9]+)ms.*/\1/')"
  C2C="$(printf '%s' "$C2C_LINE" | sed -E 's/.*= ([0-9]+)ms.*/\1/')"
  echo "[g3] 热键→覆盖层 P90 = ${H2O}ms (目标 ≤150)"
  echo "[g3] 确认→剪贴板 P90 = ${C2C}ms (目标 ≤100)"
  # 目标线判定（任一超线 → exit 1）：G3 的门槛本体。
  [ "$H2O" -le 150 ] && [ "$C2C" -le 100 ] && exit 0
  echo "[g3] 未达目标线（h2o ≤150ms / c2c ≤100ms）" >&2
  exit 1
fi

# —— ⑤ 回退：解析 perf CSV（历史兼容口径）——
# 走到这里说明应用没吐出进程内 P90（多半是手动启动的普通实例，或基准模式没跑完）。
echo "[g3] 未捕获进程内 P90（stderr 无 [bench] P90 行），回退到 perf CSV 口径" >&2
python3 - <<'EOF'
import csv, glob, sys, tempfile
# 与 Rust 侧 perf.rs 同目录：std::env::temp_dir()/hawksnap-perf（macOS 即 $TMPDIR，
# 与 tempfile.gettempdir() 一致，勿写死 /tmp）
perf_dir = tempfile.gettempdir() + "/hawksnap-perf"
rows = [list(csv.reader(open(f))) for f in glob.glob(perf_dir + "/*.csv")]
flat = [r for run in rows for r in run]
by = {}
for stage, ts in flat: by.setdefault(stage, []).append(int(ts))
# 长度校验（评审 Finding 2b）：Esc 取消的轮次缺 confirm/clipboard_done，游离/迟到事件会
# 多出行——按行 zip 配对会静默错位，宁可报错也不输出被污染的 P90。
n = len(by.get("hotkey", []))
if not (n > 0 and n == len(by.get("overlay_shown", [])) == len(by.get("clipboard_done", []))
        and len(by.get("confirm", [])) == n):
    sys.exit(f"数据不齐（CSV 口径）：清空 {perf_dir} 后重跑，bench 期间不要 Esc 取消")
def p90(xs): return sorted(xs)[int(len(xs)*0.9)]
h2o = [b-a for a,b in zip(by["hotkey"], by["overlay_shown"])]
# 确认→剪贴板从 confirm（finish_capture 首行，即 invoke 时刻）起算（评审 Finding 1）：
# 旧口径 overlay_shown→clipboard_done 混入人工框选时间，无法作为 G3 ≤100ms 的证据。
c2c = [b-a for a,b in zip(by["confirm"], by["clipboard_done"])]
print(f"热键→覆盖层 P90 = {p90(h2o)}ms (目标 ≤150)")
print(f"确认→剪贴板 P90 = {p90(c2c)}ms (目标 ≤100)")
EOF
