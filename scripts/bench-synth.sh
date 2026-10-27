#!/usr/bin/env bash
# scripts/bench-synth.sh [N] — G3 延迟基准的**免人工按键**入口（spec G3）
#
# 用法：bash scripts/bench-synth.sh [轮数]   # 默认 20
#
# —— 0.3.0 收尾变更（终审 review wave I5）——
# 旧实现：要求用户**先手工运行应用**，再用 swift 注入合成按键（F1 → 拖拽 → 双击）
# 共 N 轮，最后解析 perf CSV 算 P90。该流程在当前门禁下**已不成立**：
#   * perf CSV 现在**只在基准模式 / G4 模式写**（perf.rs 的落盘总闸，生产零写入）；
#     而这两种模式都会**自动驱动并自己退出应用**（基准模式跑完 N 轮即 exit，
#     G4 模式开面板后 exit）→ 合成注入无处安放、CSV 也不会被普通实例写出。
#   * 旧脚本因此在「应用已运行」的前提下解析一个**根本不存在**的 CSV → 必失败。
# 现在：G3 改由应用内建的**基准模式**测（HAWKSNAP_BENCH=N）——它自己驱动真实的
# 「捕获 → 覆盖层 → 确认 → 剪贴板」链路、**进程内**计时、结束时把两条 P90 打到
# stderr，**不需要任何键鼠注入**（注入本身就是噪声源）。
#
# 因此本脚本退化为 bench-latency.sh 的同款入口（单命令 → 两个数）：
#   bash scripts/bench-synth.sh 20     ≡   bash scripts/bench-latency.sh 20
# 保留本文件名的理由：它是「G3 免人工按键基准」在文档与历史报告里的既有入口。
# （旧的合成注入脚本 scripts/bench_synth.swift 仍在仓库里备查，但当前门禁下不再
#  有可运行的流程——它需要向一个**普通运行**的应用注入按键，而普通运行不写 CSV。）
set -euo pipefail
N="${1:-20}"
DIR="$(cd "$(dirname "$0")" && pwd)"

exec "$DIR/bench-latency.sh" "$N"
