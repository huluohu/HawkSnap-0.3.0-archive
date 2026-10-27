# HawkSnap M4: 浏览器插件 / 官网 / 1.0 收官 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付 spec §11 M4 全量：Chrome/Edge 浏览器插件（MV3，整页/选区截图 + 标注 + 复制下载，经 Native Messaging 打通桌面端）、官网（产品页 + 下载 + 文档）、跨平台编译重构（CI 三平台真绿）、1.0 发布门（分发就绪检查）。

**Architecture:** 插件 = TypeScript + MV3（service worker + offscreen document 做 canvas 合成；chrome.debugger 抓整页或自绘 DOM 序列化——选 chrome.debugger 是 spec §3.5 明示路径）；桌面端桥接 = Native Messaging（宿主名 `com.hawksnap.app`，JSON 帧：{cmd:"capture", ...} ↔ {path, width, height}），桌面端新增 `native_messaging.rs`（tauri 插件或 std::process 管道——裁定：**用 std::process 启停 chrome host manifest 注册的宿主二进制**，无新 crate）；官网 = 静态站（无框架：手写 HTML/CSS/JS + 暗色主题 + 下载链接 + 文档），放在 `site/`，CI 里用 GitHub Pages 部署（workflow）。

**Tech Stack:** 既有栈 + TypeScript 插件（vite 构建 MV3）+ 静态站；Rust 侧 Native Messaging 走 std::process（JSON over stdin/stdout）。

**Spec:** `docs/superpowers/specs/2026-10-13-hawksnap-design.md`（§3.5 浏览器插件、§11 M4、§5 指标、F12 已于 M2 完成）；验收：插件整页截图 ≥10000px 高保真（§3.5「10000px 高页面整页保真（含懒加载图）」）、桌面端可接收并入库。

## Global Constraints（继承 + M4 新增）

- 继承 M1–M3 全部约束（不抢用户输入、不在用户机器上跑测试——本计划的所有验证走远端 Linux 农场 `ssh muniu@192.168.9.102`；macOS-only 部分只读码评审 + 留给用户的清单）。
- 隐私：插件只截用户显式触发（工具栏点击/快捷键）的页面；不后台采集、不上报。
- Native Messaging 通道只允许本机已注册的宿主（manifest 里固定 `allowed_origins: ["chrome-extension://<ID>/"]`）；桌面端收到非本机来源的帧 → 拒绝。
- 插件不得引入 npm 运行时依赖（零依赖或仅 devDeps），保持体积小。
- 分发就绪门（1.0）：LICENSE + 版本 5 处一致 + CI 全绿（若三平台编译重构完成）+ 源码包 + 签名 secrets + 安装包体积门。
- 每任务全绿（远端）+ conventional commit。

---

### Task P0: 跨平台编译重构（CI 三平台真绿）

**Files:**
- Modify: `crates/hawksnap-platform-mac/Cargo.toml`（objc2*/screen-capture-kit/av-foundation/block2/dispatch2/chrono 全部移入 `[target.'cfg(target_os = "macos")'.dependencies]`；core-graphics/core-foundation 同理）
- Modify: `crates/hawksnap-platform-mac/src/lib.rs`（`pub mod ax/permissions/pin/record/windowcap;` 全部 `#[cfg(target_os="macos")]` 门控 + 非 macOS stub 模块（返回 Err("该平台暂不支持") 的同名函数）
- Modify: `crates/hawksnap-platform-mac/src/*.rs`（逐文件的 cfg 拆分：macOS 实现 + `#[cfg(not(target_os="macos"))]` stub）
- Modify: `app/src-tauri/Cargo.toml`（macOS-only 依赖门控：arboard?（跨平台 ✓ 保留）、objc2 系（macOS only）、chrono（跨平台 ✓））
- Modify: `app/src-tauri/src/*.rs`（引用 macOS-only API 的模块加门控 + stub）
- Modify: `.github/workflows/ci.yml`（rust-test 腿去掉 continue-on-error）

**Interfaces:**
- Produces: 非 macOS 平台可编译（`cargo check` 在 Linux/Windows 通过），macOS 功能在其它平台返回清晰 Err。跨平台 API 面 = Linux/Windows 的截图功能留待 M4+（spec §6.3 Linux X11 全功能是更大工程——本任务只保证**可编译 + 清晰降级**，README 明示）。

- [ ] Step 1: 远端 Linux 上 `cargo check --workspace` 逐错误清零（对方代理在远端做，本机只编辑）。
- [ ] Step 2: CI 三平台腿恢复为硬门（去掉 continue-on-error）。
- [ ] Step 3: README 明示「0.3.x/1.0：Windows/Linux 可编译但截图功能待平台适配；macOS 功能完整」。
- [ ] Step 4: Commit: `refactor: gate macos-only crates behind cfg so all platforms compile`

### Task P1: Native Messaging 桥（桌面端）

**Files:**
- Create: `app/src-tauri/src/native_messaging.rs`
- Modify: `app/src-tauri/src/lib.rs`（注册 + 启动监听）
- Modify: `app/src-tauri/src/commands.rs`（接收的截图 → 落盘 save_dir + history 入库 kind="browser"）

**Interfaces:**
- Produces: 命令 `native_messaging_status() -> bool`（是否已注册宿主）；宿主名 `com.hawksnap.app`；帧协议 JSON 行：
  - 插件→桌面：`{"cmd":"capture","id":"...","path":"/tmp/xxx.png","width":N,"height":N}`
  - 桌面→插件：`{"id":"...","ok":true,"savedPath":"..."}` / `{"id":"...","ok":false,"error":"..."}`
- 实现：macOS 上由 app 自己启动宿主进程？Native Messaging 的标准模型是**浏览器启动宿主**（浏览器读 manifest 找到宿主二进制并拉起，宿主与浏览器通过 stdio 通信）。因此桌面端实现 = 读 stdin 的 JSON 行 + 回写 stdout。宿主二进制路径需注册到 Chrome（`~/Library/Application Support/Google/Chrome/NativeMessagingHosts/com.hawksnap.app.json`）——本任务提供安装脚本 `scripts/install-native-host.sh`（写 manifest，路径指向已安装的 HawkSnap.app 可执行文件）。
- 安全：只接受来自浏览器的 stdio 管道（无网络）；收到的 path 必须在 TMPDIR 或由插件声明的临时前缀下（防止任意文件写入）。

- [ ] Step 1: 失败测试（协议解析纯函数：`parse_frame(line) -> Option<Frame>`，恶意 path 拒绝）；实现；命令注册。
- [ ] Step 2: 安装脚本 + README 说明。
- [ ] Step 3: 远端可测部分（协议纯函数）；macOS 部分读码评审 + 用户清单。
- [ ] Step 4: Commit: `feat(native): native messaging bridge for browser plugin`

### Task P2: 浏览器插件（MV3）

**Files:**
- Create: `plugin/`（manifest.json、service-worker.ts、offscreen.ts、content-script.ts、options/、icons/、build 用 vite 或 esbuild）
- Modify: `plugin/src/`（截图逻辑、标注 UI、下载）

**Interfaces:**
- Produces: MV3 扩展：工具栏点击 → 选区/整页截图（chrome.debugger Page.captureScreenshot captureBeyondViewport=true，限高 10000px+）→ 打开编辑器页（offscreen document + canvas）→ 复制到剪贴板（navigator.clipboard）/下载 PNG → 可选「发送到 HawkSnap 桌面端」（经 Native Messaging，帧协议同 P1）。
- 零运行时依赖；构建产物 `plugin/dist/`；manifest 权限最小集（debugger、downloads、clipboardWrite、storage、offscreen）。

- [ ] Step 1: 纯逻辑测试（可远端跑：node 环境下的 vitest——截图尺寸裁剪策略（>10000px 缩放）、懒加载图处理、帧编码）
- [ ] Step 2: 扩展实现 + 构建脚本（npm run build → dist）
- [ ] Step 3: 远端 `npm test` + 构建验证（Chrome 打包用 web-ext lint? 无依赖则跳过，注明）
- [ ] Step 4: Commit: `feat(plugin): chrome/edge extension for page and area capture`

### Task P3: 官网（静态站）

**Files:**
- Create: `site/index.html`、`site/assets/style.css`、`site/assets/main.js`、`site/docs/index.html`、`site/README.md`
- Modify: `.github/workflows/ci.yml`（pages 部署 job：构建 → gh-pages）

**Interfaces:**
- Produces: 暗色主题单页站（与产品视觉一致：鹰隼蓝 #22405F→#0C1624 / 琥珀 #FFB020 / 墨色 #0D1522）：Hero + 功能网格 + 下载按钮（macOS dmg + GitHub Releases）+ 文档页（安装/权限说明/快捷键/FAQ）+ 隐私与许可声明（AGPL + 第三方声明）。
- 无框架、无构建（纯静态，CI 直接部署）——保持轻量。

- [ ] Step 1: 静态检查（HTML 有效性、链接可达、无外部资源依赖——除字体用系统栈）
- [ ] Step 2: pages workflow
- [ ] Step 3: Commit: `docs(site): product site and docs with github pages deploy`

### Task P4: 1.0 发布门

**Files:**
- Create: `scripts/release-readiness.sh`（检查：LICENSE 存在、版本 5 处一致（cargo/package/lock×2/conf）、check-size 通过、i18n 绿、workflow 合法、源码包可生成）
- Modify: 版本 → 1.0.0；README；`docs/superpowers/bench/2026-10-14-m4-release-gate.md`

- [ ] Step 1: readiness 脚本 + 远端跑（macOS-only 项标注跳过）
- [ ] Step 2: 版本 1.0.0 + 归档 + tag v1.0.0（前置：仓库公开 + secrets，由用户完成）
- [ ] Step 3: Commit: `chore: release 1.0.0 with readiness gate`

---

## 自审记录

1. **Spec 覆盖**：§3.5 插件→P2（10000px 保真 = captureBeyondViewport + 限高策略）；Native Messaging→P1；官网→P3；跨平台→P0；1.0 门→P4。**缺口**：spec §6.3 Linux/Windows 全功能（X11 录屏/吸附）不在 1.0 范围——P0 只保证可编译+清晰降级，README 明示；spec §10 的浏览器插件 E2E 冒烟（Tauri WebDriver）不在 M4（M4 插件用 chrome.debugger + 手工验收）。
2. **无占位符**：插件的 chrome API 面是标准 MV3；站点纯静态；Native Messaging 协议明确定义。
3. **类型一致**：Frame 协议字段在 P1（Rust）与 P2（TS）必须一致——P1 先定协议并把它写进 `docs/superpowers/protocols/native-messaging.md`，P2 照抄。
