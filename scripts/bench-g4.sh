#!/usr/bin/env bash
# scripts/bench-g4.sh — G4 验收基准（plan D5）：冷启动→托盘 ≤800ms；主面板打开 ≤300ms。
#
# 姊妹脚本：G3 延迟基准 = scripts/bench-latency.sh（同一「应用内建仪器 + 读
# stderr」口径：HAWKSNAP_BENCH=N 自跑 N 轮、进程内算 P90，单命令出两个数）。
#
# 原理：以 HAWKSNAP_G4=1 启动 HawkSnap，应用在托盘就绪后自动打开主面板并自动退出
# （g4.rs），stderr 输出一行 `[g4] cold_start=..ms panel=..ms`。本脚本连跑 N 次
# （默认 5），取中位数 + 最近秩 P90，对目标线（≤800ms / ≤300ms）给出判定。
#
# 副作用（须知）：每轮面板会真实打开约 1 秒——面板即被测对象，窗口隐藏/无头化
# 无法度量「用户可见的面板就绪」，此可见闪烁不可避免。除此之外无键鼠注入、
# 不触碰剪贴板、不截图。
#
# 用法：bash scripts/bench-g4.sh [轮数]        # 默认 5
#       HAWKSNAP_BIN=/path/to/hawksnap bash scripts/bench-g4.sh
#
# 依赖：repo 内已构建的二进制（优先 target/release/hawksnap，其次 debug；可用
# HAWKSNAP_BIN 覆盖）。构建方式：cd app && npm run build && cd .. && cargo build --release。
# 注意：确保 HawkSnap 当前未在运行（单实例锁会让测量实例秒退、该轮计为失败）。
# 判定口径：中位数（n=5 时最近秩 P90 = 最大值，离群敏感，一并打印供参考）；
# 任一中位数超目标线 → 脚本 exit 1。
set -euo pipefail

RUNS="${1:-5}"
case "$RUNS" in ''|*[!0-9]*) echo "[g4] 轮数必须是正整数：$RUNS" >&2; exit 2;; esac
[ "$RUNS" -ge 1 ] || { echo "[g4] 轮数必须 ≥1" >&2; exit 2; }

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${HAWKSNAP_BIN:-}"
if [ -z "$BIN" ]; then
  if [ -x "$ROOT/target/release/hawksnap" ]; then
    BIN="$ROOT/target/release/hawksnap"
  elif [ -x "$ROOT/target/debug/hawksnap" ]; then
    BIN="$ROOT/target/debug/hawksnap"
    echo "[g4] 未找到 release 二进制，退回 debug（数值会偏慢，验收请用 release）" >&2
  fi
fi
[ -n "$BIN" ] && [ -x "$BIN" ] || {
  echo "[g4] 找不到可执行的 hawksnap 二进制。先构建：cd app && npm run build && cargo build --release" >&2
  exit 2
}

TMP="$(mktemp -d /tmp/hawksnap-g4.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

COLD_ALL=(); PANEL_ALL=(); FAILED=0
for i in $(seq 1 "$RUNS"); do
  HAWKSNAP_G4=1 "$BIN" >/dev/null 2>"$TMP/run-$i.err" &
  PID=$!
  # 兜底看门狗：正常应 ~1s 自退；60s 不退视为挂死，kill -9 后计失败。
  ( sleep 60; kill -9 "$PID" 2>/dev/null; exit 0 ) & WD=$!
  wait "$PID" || true
  kill "$WD" 2>/dev/null || true
  wait "$WD" 2>/dev/null || true

  if grep -q "另一实例正在运行" "$TMP/run-$i.err"; then
    echo "[g4] run $i: 单实例锁被占（HawkSnap 正在运行？），该轮计为失败" >&2
    FAILED=$((FAILED + 1)); continue
  fi
  LINE="$(grep -oE 'cold_start=[0-9]+ms panel=[0-9]+ms' "$TMP/run-$i.err" | tail -1 || true)"
  if [ -z "$LINE" ]; then
    echo "[g4] run $i: 未捕获汇总行（该轮计为失败），stderr 摘要：" >&2
    tail -3 "$TMP/run-$i.err" >&2 || true
    FAILED=$((FAILED + 1)); continue
  fi
  COLD="${LINE#cold_start=}"; COLD="${COLD%%ms*}"
  PANEL="${LINE#*panel=}";   PANEL="${PANEL%%ms*}"
  echo "[g4] run $i/$RUNS: cold_start=${COLD}ms panel=${PANEL}ms"
  COLD_ALL+=("$COLD"); PANEL_ALL+=("$PANEL")
done

# —— 统计（纯 bash：sort -n 排序后按下标取值）——
# 兼容 macOS 自带 bash 3.2：不用 mapfile/readarray（bash 4+）。
# 最近秩 P90：升序第 ceil(0.9*n) 个 = 下标 (9n+9)/10 - 1（与 bench_mode 同公式）。
sorted_asc() {
  local v; SORTED=()
  while IFS= read -r v; do SORTED+=("$v"); done < <(printf '%s\n' "$@" | sort -n)
}
median() {
  local n=$#; sorted_asc "$@"
  if (( n % 2 == 1 )); then echo "${SORTED[(n + 1) / 2 - 1]}";
  else echo $(( (SORTED[n / 2 - 1] + SORTED[n / 2]) / 2 )); fi
}
p90() {
  local n=$#; sorted_asc "$@"
  echo "${SORTED[(9 * n + 9) / 10 - 1]}"
}

if [ "${#COLD_ALL[@]}" -eq 0 ]; then
  echo "[g4] 无有效样本（全部 $FAILED 轮失败），无法判定" >&2
  exit 1
fi

COLD_MED=$(median "${COLD_ALL[@]}"); COLD_P90=$(p90 "${COLD_ALL[@]}")
PANEL_MED=$(median "${PANEL_ALL[@]}"); PANEL_P90=$(p90 "${PANEL_ALL[@]}")
COLD_OK=$([ "$COLD_MED" -le 800 ] && echo PASS || echo FAIL)
PANEL_OK=$([ "$PANEL_MED" -le 300 ] && echo PASS || echo FAIL)
[ "$FAILED" -gt 0 ] && echo "[g4] 失败轮：$FAILED/$RUNS（未计入统计）" >&2
echo "[g4] cold_start median=${COLD_MED}ms P90=${COLD_P90}ms target<=800ms $COLD_OK"
echo "[g4] panel median=${PANEL_MED}ms P90=${PANEL_P90}ms target<=300ms $PANEL_OK"
[ "$COLD_OK" = PASS ] && [ "$PANEL_OK" = PASS ] || exit 1
