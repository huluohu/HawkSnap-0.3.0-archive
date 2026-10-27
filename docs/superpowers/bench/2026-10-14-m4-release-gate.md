# M4 / 1.0 发布门（release readiness gate）

- 日期：2026-10-14；目标版本 **v1.0.0**（HEAD：`chore: release 1.0.0 with readiness gate`）；平台分层：静态门跨平台可跑，重量项 macOS-only
- 结构：**三层**——① 静态就绪门（本仓库脚本，可重复执行）→ ② macOS 人工验证清单（构建 / 测试 / 性能 / 手工功能）→ ③ GitHub 发布前置与 tag 流程
- 分工：②③ 只能由维护者在本机 / GitHub 上执行（不在代理环境跑测试的继承约束）；本档案记录静态门实测输出与清单的判定线

## 第①层：静态就绪门（scripts/release-readiness.sh，已实测）

覆盖：LICENSE 存在、版本 1.0.0 五处一致（workspace Cargo.toml / app Cargo.toml / tauri.conf.json / package.json / package-lock.json 顶层+根）、
Cargo.lock 四个 workspace crate 同步、i18n 门禁（纯 node）、workflow YAML 可解析、release job 含 AGPL §6 源码归档、
plugin/dist 已 gitignore、LICENSE-THIRD-PARTY.md 声明 gifski（AGPL 触发源）。

2026-10-14 本机实测（15 项全绿，exit 0）：

```
== HawkSnap 1.0 分发就绪门（release readiness gate）==
(a) LICENSE
  PASS  LICENSE 存在（34523 bytes）
(b) 版本一致性（期望 1.0.0，5 处 + Cargo.lock 的 4 个 workspace crate）
  PASS  Cargo.toml [workspace.package] = 1.0.0
  PASS  app/src-tauri/Cargo.toml [package] = 1.0.0
  PASS  app/src-tauri/tauri.conf.json = 1.0.0
  PASS  app/package.json = 1.0.0
  PASS  app/package-lock.json 根版本（顶层 + packages[""]）= 1.0.0
  PASS  Cargo.lock: hawksnap = 1.0.0
  PASS  Cargo.lock: hawksnap-core = 1.0.0
  PASS  Cargo.lock: hawksnap-ocr = 1.0.0
  PASS  Cargo.lock: hawksnap-platform-mac = 1.0.0
(d) i18n key 覆盖率
  PASS  check-i18n 通过（A⊆Z、A⊆E 且 Z==E）   [字面量 128 key；zh=131 en=131]
(e) workflow YAML 语法
  PASS  ruby+psych 解析通过：.github/workflows/ci.yml（本机无 PyYAML，走 ruby 后备）
(f) 源码包（AGPL §6 corresponding source）
  PASS  release job 内含 git archive 源码归档步骤
(g) 插件构建产物不入库
  PASS  plugin/dist/ 已 gitignore
(h) LICENSE-THIRD-PARTY.md
  PASS  第三方声明含 gifski（AGPL-3.0-or-later 触发源）及其分发义务
```

## 第②层：macOS 人工清单（发布 tag 前逐项执行，全绿才推 tag）

| # | 命令 / 操作 | 判定线 | 依据 |
|---|---|---|---|
| 1 | `cargo test --workspace` | 0 失败（权限/窗口门控的 ignored 除外） | CI macOS 腿同口径 |
| 2 | `cargo clippy --workspace -- -D warnings` | 0 告警（存量 `neg_cmp_op_on_partial_ord` 若未清零，按 D2 裁定记录豁免理由） | CI 尚未接入，人工把关 |
| 3 | `cd app && npm ci && npm test && npm run build` | 0 失败、tsc 0 错误 | CI frontend 腿同口径 |
| 4 | `node scripts/check-i18n.mjs` | OK（静态门已绿，此处复核） | G6 |
| 5 | `scripts/package-native-host.sh && cd app && npm run tauri build -- --config src-tauri/tauri.release.conf.json --bundles app dmg` | 构建成功、dmg 内含 `Contents/MacOS/hawksnap-host` | M4 P1（externalBin 顺序敏感） |
| 6 | `bash scripts/check-size.sh` | dmg ≤ **15MB** | G1 |
| 7 | `bash scripts/bench-synth.sh`（G3 延迟） | h2o P90 ≤150ms / c2c P90 ≤100ms | G3 |
| 8 | `bash scripts/bench-g4.sh`（G4 冷启动/面板） | cold_start P90 ≤800ms / panel P90 ≤300ms | G4 |
| 9 | `bash scripts/sample-mem.sh`（G2 空载内存） | ≤80MB | G2 |
| 10 | **F9 摄像头画中画**手工验证：录屏中开启 PiP，检查成片时间线连续（无冻结 / 跳变），相机缺席时录制不受影响 | 时间线连续 | M3 F9 |
| 11 | **F10 长截图**手工滚动验证：多页面滚动拼接，周期内容应被拒绝而非产出重影 | 拼接正确 / 拒绝有提示 | M3 F10 |
| 12 | **keycast 焦点检查**：录制中开启按键显示，确认 CGEventTap 只读旁路**不吞按键**、修饰键单按不显示、F6 停止照常触发 | 行为符合 M3 规格 | M3 keycast |
| 13 | 浏览器插件冒烟：`cd plugin && npm install && npm test`（仓库未提交 plugin/package-lock.json，故用 install；lockfile 首跑生成后建议提交并改回 `npm ci`）（远端 Linux 农场或本机 node）→ Chrome `chrome://extensions` 加载 `plugin/dist` → 整页 / 可见区 / 区域截图 → 标注 → 复制 / 下载 → 注册宿主后「发送到 HawkSnap」入历史 | 全链路可用 | M4 P2 |
| 14 | G5 soak（72h 连续录屏，无崩溃、内存增长 <10%）——**长测，可与发布并行，结果回填本档案** | 见 G5 定义 | 已知限制保留项 |

## 第③层：GitHub 发布前置与 tag 流程

1. **仓库公开**（AGPL-3.0-or-later 分发口径的前提——免费公开分发满足 §6 对应源码义务）；
2. **Settings → Pages → Source 选 GitHub Actions**（官网 `pages` job 的 deploy-pages 前置，一次性）；
3. **Secrets 配齐**（缺一签名步骤即硬失败，有意为之）：
   - `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`（Ed25519 updater 密钥；pubkey 已随 0.1.0 填入 tauri.conf.json，须配对）；
   - `APPLE_CERTIFICATE` / `APPLE_CERTIFICATE_PASSWORD`（HawkSnapCodesign 证书，TCC 授权锚定）；
4. push tag：`git tag v1.0.0 && git push origin v1.0.0`（注意：tag 应指向 `chore: release 1.0.0 with readiness gate` 提交；本地提交后由维护者 push）；
5. CI release job 产出签名 dmg / .app.tar.gz + .sig + `HawkSnap-v1.0.0-source.tar.gz`（AGPL §6）→ publish job 组装 latest.json（darwin-aarch64）→ **草稿 Release**；
6. 人工复核草稿（产物齐全、已知限制写入 Release notes：macOS-only 分发、G5 长测并行中、Rust 侧文案未 i18n、Windows/Linux 可编译待适配）→ 发布；
7. 发布后验证：`releases/latest/download/latest.json` 可拉取且签名有效；dmg 下载安装后能收到更新检查；官网 Pages 已更新。

## PENDING（环境受限，本任务未执行）

- **远端 Linux 农场（muniu@192.168.9.102）当日不可达**（ssh 超时）——P2 插件的 `npm test` 远端执行、P0 的 Linux `cargo test` 复核未能重跑；以 CI 三平台腿（macOS/Linux 硬门）结果为准，人工清单第 13 项含插件测试。
- 第②层全部条目（macOS-only 构建 / 性能 / 手工功能）与第③层 GitHub 操作：**待维护者执行**，结果回填本档案。
