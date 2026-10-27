# M3 验收数据（0.3.0）

- 版本：0.3.0（区域录制 / GIF 导出 / 摄像头画中画 / 按键显示 / 长截图）
- 采集环境：**远端 Linux 农场** `muniu@192.168.9.102`（Ubuntu 24.04 / 20 核 / 30GB）——
  本机 Mac 在用户在场期间**未跑任何 build/test/eval**（用户禁令），全部重活走 SSH。
- 归档口径：**可远程测的实测**，**macOS-only 的留控制器/用户跑**（命令已写全，标 ⏳）。

## 0. 总览

| Gate | 目标 | 实测 | 结果 |
|---|---|---|---|
| F8 GIF 编码 | 150 帧 ≤10s | **4.34s**（Linux 侧编码器，150 帧 800×800 高熵源）；Mac 侧同路径历史实测 1.39s（800×600 渐变源） | ✅（采集侧 ⏳ macOS） |
| F9 画中画 pts 连续 | stts 逐帧时长无离散倍峰 | 平台专属（AVCapture + SCStream），Linux 不可编译 | ⏳ **控制器跑**（命令见 §2） |
| F10 长截图拼接 | 严格成功率 ≥90% | **19/21 = 90.5%**（严格）/ **21/21 = 100%**（F10 字面）；输出高 **20145–23912px** | ✅（人工层 ⏳ macOS） |
| G1 安装包体积 | ≤15MB | 0.2.0 构建实测 app **10.93 MiB** / dmg **5.77 MiB**；**0.3.0 需 macOS 重建** | ⏳ **控制器跑** |
| G2 空载内存 | ≤80MB | macOS-only（进程 RSS 采样） | ⏳ **控制器跑** |
| G3 截图延迟 | c2c ≤100ms / h2o ≤150ms | macOS-only（合成注入基准） | ⏳ **控制器跑** |
| G4 冷启动 / 面板 | ≤800ms / ≤300ms | macOS-only（HAWKSNAP_G4=1 自测仪器） | ⏳ **控制器跑** |
| F7 录屏 CPU | ≤8% | 区域录制 800×600 实测 **4.30–4.93%**（N1，release）；GIF 通道 ON 的复采样 ⏳ | ✅ / ⏳ 复采样 |

## 1. 可远程实测的部分（Ubuntu 192.168.9.102，`/tmp/hawksnap-build`）

### 1.1 代码级验证

```
$ cargo test -p hawksnap-core --release      → 49 passed / 0 failed
$ npx vitest run                              → Test Files 19 passed · Tests 152 passed
$ npx tsc --noEmit                            → exit 0（无输出）
$ node scripts/check-i18n.mjs                 → OK：A⊆Z、A⊆E 且 Z==E（zh=122 en=122）
```

### 1.2 F10 —— 长截图拼接 eval（自动层，21 个标准场景 + 1 个专项局限场景）

`scripts/scrollshot-eval/run-eval.sh`（合成像素 → 复刻产品侧采集节奏 → 走**生产同一条**
`hawksnap_core::stitch`）：

```
[scrollshot-eval] 成功率 19/21 = 90.5%（严格判据：无重影 + 输出≥20000px + 无内容缺口；门槛 ≥90%）
[scrollshot-eval] F10 字面判据（无重影 21/21，输出≥20000px 21/21）：21/21 = 100.0%
[scrollshot-eval] 输出高度：最小 20145px · 最大 23912px
[scrollshot-eval] SUMMARY passed=19 total=21 rate=0.905 f10=21 min_h=20145 max_h=23912
```

逐场景（标准场景 21 个，对齐率 / 输出高）：

| 场景 | 输出高 | 对齐率 | 结论 |
|---|---|---|---|
| 01-text / 02-text-dense / 03-images / 04-tables / 05-code | 22894–23836px | 1.000 | OK |
| **06-chat** | 20954px | 1.000 | **内容缺口 519px**（漂移上限 214）→ 判失败 |
| 07-fixed-header / 08-sticky-side / 09-dark / 10-doc | 23417–23912px | 1.000 | OK |
| **11-gallery** | 20145px | 1.000 | **eval 迭代未收敛**（页高 20596px 与实测步长推出的 20406px 分离）+ 内容缺口 451px → 判失败 |
| 12-blog … 20-mixed | 20147–23912px | 1.000 | OK |
| **21-flash-frame**（C1 回归场景） | 23223px | **1.000** | OK（跳帧后重基准正确） |
| 22-blank-band（专项局限场景，不计入成功率） | 11644px | 1.000 | OK（内容截断 4246px —— **预期**：空白带不可跨越） |

**两个失败场景的性质（都不是「错拼」）**：对齐率均为 1.000（无重影、无错位），失败来自
「内容缺口」——**周期内容**（聊天气泡等宽同色、图库瓦片同构）在多个位移上二维相关接近
→ 匹配器判**歧义** → 拒绝拼接（宁可不拼，绝不产出重影图）。

**11-gallery 的口径变更（0.3.0 收尾）**：旧实现遇到「步长 ↔ 页高」不动点**未收敛**时，
会**按最终步长重新渲染页面**再比对 → 比对基准被换掉，缺口的来源被掩盖。现改为
**检测未收敛 → 该场景判失败**（结论如实写在 eval 输出里：`⚠ eval 未收敛`），失败率口径
不变（19/21 = 90.5%），但**结论不再有「换基准后测过了」的假象**。

### 1.3 F10 —— C1 反向证明（eval + 单测仍能抓住该回归）

> **方法学提醒（本轮踩到的坑）**：`rsync -a` **保留 mtime** → 远端「注缺陷→跑→恢复源码」
> 之后若再用 rsync 覆盖同一文件，源文件的 mtime 可能**早于**已编译产物 → **cargo 认为无需
> 重建** → eval 跑的是**上一次的旧二进制**（本轮就撞到过：一次复跑把 19/21 变成 18/21，
> 原因是跑的是反向证明阶段编译的**带缺陷**二进制）。**正确做法**：rsync 之后
> `find crates app/src-tauri/src -name '*.rs' -exec touch {} \;` 强制重建（或 rsync 用
> `--checksum`）。本文件的所有数字都在**强制重建后**复跑确认过。

**方法**：在远端副本里把 C1 缺陷**注回源码**（跳帧后不再对「上一张被接受的帧」重新估计，
改为沿用「被跳过的帧」作基准），复跑单测与 eval，然后恢复源码复跑。

```
注入前：cargo test -p hawksnap-core --release → 49 passed / 0 failed
        eval → rate=0.905 f10=21 min_h=20145 max_h=23912，退出码 0 ✅

注入缺陷后：
  cargo test -p hawksnap-core --release → 46 passed / 3 FAILED
    （chain_rebases_onto_last_accepted_frame_after_skip、
      stitch_skips_unmatched_frames_and_keeps_chain、
      stitch_with_unusable_middle_frame_keeps_offsets_correct）
  eval → 21-flash-frame 对齐率 0.691（内容错位）+ 缺口 362px
        成功率 18/21 = 85.7%（跌破 90% 门槛）→ 退出码 1 ❌

恢复源码后：49 passed · eval 回到 rate=0.905 f10=21 ✅
```

**结论**：0.3.0 收尾期间对 eval 的「未收敛 → 判失败」改动**没有削弱**回归捕获能力。

### 1.4 F8 —— GIF 编码计时（**只测编码器**）

gifski 是纯 Rust → 编码路径可在 Linux 复现；**采集路径（SCStream 抽帧 + 降采样）是
macOS 专有，无法远程复现**。远端用**独立一次性 crate**（`/tmp/gifski-bench`，不入库）
复刻 `app/src-tauri/src/gif.rs` 的 `encode_gif` 全部要素（`Settings{width:None,
height:None, quality:90, fast:false, repeat:Infinite}` + writer/feeder 双线程 + PTS 归零）：

```
[f8] 150 帧 800x800 · quality=90 · 编码耗时 4.339s · 产物 119.53MB
```

对照（0.2.0 → 0.3.0 dev 的 **Mac 侧**历史实测，N2 任务报告）：

| 口径 | 帧数 / 尺寸 | 源熵 | 编码耗时 | 产物 | 预算 |
|---|---|---|---|---|---|
| Linux 农场（本次实测） | 150 × 800×800 | 高熵噪声（最坏情形） | **4.34s** | 119.5MB | ≤10s ✅（2.3× 余量） |
| Mac（N2 报告） | 150 × 800×600 | 渐变（低熵） | **1.39s** | 13.0MB | ≤10s ✅（7.2× 余量） |
| Mac 真实端到端（N2 报告） | 16 帧（10s 录制） | 真实屏幕 | **0.11s** | 256KB | ≤10s ✅ |

### 1.5 画布分配量回归守卫（0.3.0 收尾新增，反向证明）

`canvas_allocation_tracks_logical_size` 改为「900 行步长（= 一个视口）+ 分配量 ≤1.05×
逻辑量 + 余量 ≤ 一块」。把 `grow_rows` 注回缺陷版本（多块循环里**过度请求**）：

```
注入缺陷后 → canvas_allocation_tracks_logical_size FAILED
            （余量 196608B > 一块 65536B）
恢复修复后 → 1 passed ✅
```

## 2. macOS-only（⏳ 控制器 / 用户在本机跑）

### F9 画中画 pts 连续性

```bash
cd app/src-tauri
# ① 端到端集成测试（动窗驱动 30fps 交付 + 相机/注入源合成 + 两段真实录制 + stts 逐帧时长）
cargo test -p hawksnap-platform-mac --test real_pip_recording -- --ignored --nocapture
# ② 只想看机读协议行时，直接跑探针
cargo run -p hawksnap-platform-mac --bin pip-f9-probe
# ③ 需要抽帧证据时（探针会提示调用）
swift scripts/pip_frames.swift <mp4> <out-dir>
swift scripts/pip_region_motion.swift <mp4> <out-dir>
```

前置：**屏幕录制权限**（未授权时 SCStream 返回空）；相机**已授权**时走真实相机道，
未授权时探针走**注入模式**（合成帧走同一条「槽 → 合成 → writer → MP4」链路）——
两种口径都要在报告里注明用的是哪一种。

### G1 安装包体积

```bash
cd app && npm run tauri build
cd ../.. && ./scripts/check-size.sh      # dmg ≤15MB 断言
```

0.2.0 构建实测：**HawkSnap.app 10.93 MiB / DMG 5.77 MiB**（M2 归档另记 dmg 5.56MB —— MiB 与
MB 的计量口径差异）。0.3.0 新增 gifski（+0.19MB 量级，N2 报告实测 10.7MB）→ **必须 macOS
重建后重新取数**。

### G2 空载内存 / G3 延迟 / G4 冷启动

```bash
./scripts/sample-mem.sh        # G2：空载 RSS 5 次采样，≤80MB
./scripts/bench-synth.sh 20     # G3：合成注入 F1→拖拽→双击 20 轮，c2c ≤100ms / h2o ≤150ms
./scripts/bench-g4.sh          # G4：冷启动 ≤800ms / 主面板 ≤300ms（需先构建 release 二进制）
```

### F10 人工层（真实浏览器滚动）

```bash
node scripts/scrollshot-eval/generate-pages.mjs   # 生成 20 个本地长页面
open scripts/scrollshot-eval/pages/index.html     # 或用任意浏览器打开单个 page
```

流程：托盘「长截图」→ 拖选区域 → **真实滚动页面** → 提示条按 ⌘S（或点「完成」）→
出图（≥20000px）→ 落 `~/Pictures/HawkSnap` 并进历史。重点看：**周期内容页面
（06-chat / 11-gallery）是否明确报错而不是产出重影图**、**提示条是否定位在选区之外**。

### M3 其余人工项

- **区域录制（F6）**：F6 → 拖选 → 确认 → 只录该区域（悬浮条显示「区域 800×600」）；
  悬停直选窗口时录的是该窗口**区域**（含遮挡内容，非窗口级 filter —— M4 议题）。
- **画中画**：录制中开启相机 → 相机授权 → 圆形叠加出现、可拖动 / 调大小 / 隐藏；
  **未授权 → PiP 静默缺席、录制不受影响**（必须实测这一条）。
- **按键显示**：需**辅助功能**权限；确认 F6 停止热键在开启 keycast 后**照常触发**（tap 只读）。
- **长截图失败路径**：滚一个无特征页面 → 「完成」→ 提示条**不消失** + 行内红字 + 「放弃」按钮；
  点「放弃」→ 条关闭、⌘S 注销；托盘「停止长截图」再点 → **静默**（0.3.0 起不再弹横幅）。
- **停滞告警**：滚到长期匹配不上的页面 → ≥3s 后才出现「匹配已停滞」（0.3.0 起为墙钟窗口，
  不再是「10 帧」——动效页不应误报）。

## 3. 归档附注

- 本文件只记录**可追溯到一次具体运行**的数据；任何未在本轮实测的项一律标 ⏳ 并附命令，
  不做推测性结论。
- 0.3.0 的 eval 门槛余量只有 **0.5pp**（19/21 vs ≥18.9/21）——后续若再改拼接算法，
  任何「让某个场景变得更难」的改动都可能击穿门槛，须连同 eval 一起复核。
- ⏳ **N3 遗留：画中画 `startRunning` 静默失败无健康信号**（N6 清单⑤，0.3.0 未收口）：
  开启 PiP 后若 `startRunning` 静默失败（相机腿未点亮 / 授权未授予），悬浮条**不出现
  任何提示**——用户看不到「PiP 没开成」，只会以为功能不存在。补法：enable 后数秒
  `frames == 0` → 悬浮条给出提示（阈值需真机相机行为观察后确定，故本轮无法定值，
  仍列为待办）。相关同批遗留见 `.superpowers/sdd/2026-10-14-hawksnap-m3-region-gif-pip-scrollshot/task-3-report.md`
  的「N6 移交清单」。
