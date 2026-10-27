# Native Messaging 协议（com.hawksnap.app）

> 契约文档。**P2（浏览器插件，TypeScript）必须逐字对齐本文的 JSON 字段与语义。**
> 桌面端实现：`app/src-tauri/src/native_protocol.rs`（帧结构与校验，纯逻辑）+
> `app/src-tauri/src/native_messaging.rs`（监听与入库）+
> `app/src-tauri/src/bin/hawksnap-host.rs`（宿主进程）。
> 计划：`docs/superpowers/plans/2026-10-14-hawksnap-m4-plugin-site-1.0.md`（Task P1）。

## 0. 拓扑（谁拉起谁）

```
Chrome 扩展（MV3, P2）
   │  chrome.runtime.connectNative("com.hawksnap.app")
   ▼
Chrome 读 manifest → 拉起宿主进程 hawksnap-host
   │  stdin/stdout：JSON 帧（下面 §2）
   ▼
hawksnap-host（独立小进程；读 0600 宿主配置拿端口+令牌）
   │  TCP 127.0.0.1:<port>，每帧一行 JSON，带令牌
   ▼
HawkSnap.app（运行中的桌面端：落盘 save_dir + 历史入库 kind="browser"）
```

**关键约束**：Chrome 拉起的是**独立进程**，要的是一对新鲜的 stdin/stdout 管道。
已在运行的 Tauri app 的 stdin 不是管道（拉起 app 会变成第二实例，被单实例守卫
挡掉）→ 宿主必须是独立二进制，由它把帧转发给运行中的 app。

## 1. 宿主 manifest（浏览器侧注册）

Chrome / Edge 的 manifest 路径（macOS）：

- `~/Library/Application Support/Google/Chrome/NativeMessagingHosts/com.hawksnap.app.json`
- `~/Library/Application Support/Microsoft Edge/NativeMessagingHosts/com.hawksnap.app.json`

内容（由 `scripts/install-native-host.sh` 写入）：

```json
{
  "name": "com.hawksnap.app",
  "description": "HawkSnap native messaging host (browser capture → desktop app)",
  "path": "/绝对路径/hawksnap-host",
  "type": "stdio",
  "allowed_origins": ["chrome-extension://<扩展ID>/"]
}
```

- `path` 必须是**绝对路径**的宿主二进制（Chrome 不做 PATH 查找）。
- `allowed_origins` 必须列出插件的真实扩展 ID（未列入的来源一律拒绝连接）；
  ID 格式 = **32 个 a-p 字符**（`scripts/install-native-host.sh --ext-id` 会校验）。
- 宿主二进制构建：`cargo build --release --features native-host --bin hawksnap-host
  --manifest-path app/src-tauri/Cargo.toml`（feature 门控，不进默认构建）。

### 1.1 宿主随 .app 分发（打包）

宿主是 feature 门控的独立 bin → **默认构建不含它**，必须显式打包，否则 manifest
指向的路径不存在（插件永远连不上）：

```bash
scripts/package-native-host.sh            # 构建 + 暂存给 tauri-bundler
cd app && npm run tauri build -- --config src-tauri/tauri.release.conf.json --bundles app dmg
```

- `scripts/package-native-host.sh` 把宿主构建出来并暂存到
  `app/src-tauri/binaries/<target-triple>/hawksnap-host`；
- `app/src-tauri/tauri.release.conf.json` 的 `bundle.externalBin: ["hawksnap-host"]`
  让 bundler 把它复制进 .app → **`/Applications/HawkSnap.app/Contents/MacOS/hawksnap-host`**
  （**在生成 .app.tar.gz 之前**——事后往 .app 里拷会晚于归档）；
- `scripts/install-native-host.sh` 的探测顺序把该路径排在**第一位**（其次是仓库
  里的本地构建，供开发用）。

## 2. 帧格式（插件 ↔ 宿主）

### 2.1 JSON 契约（**P2 照此实现**）

**插件 → 宿主**（请求）：

```json
{"cmd":"capture","id":"<uuid>","path":"/tmp/hawksnap-plugin-<id>.png","width":1920,"height":1080}
```

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `cmd` | string | 是 | 固定 `"capture"`（另有 `"save"`，见 §4） |
| `id` | string | 是 | 请求 id（插件生成的 uuid）；回执原样带回 |
| `path` | string | 是 | 插件截图的临时 PNG **绝对路径**（约束见 §3） |
| `width` | number | 是 | 截图宽（像素）；桌面端以 PNG 实际像素为准（不一致只记日志） |
| `height` | number | 是 | 截图高（像素）；同上 |

**宿主 → 插件**（回执）：

```json
{"id":"<uuid>","ok":true,"savedPath":"/Users/you/Pictures/HawkSnap/browser 2026-10-27 153000.png"}
{"id":"<uuid>","ok":false,"error":"路径不在系统临时目录内"}
```

| 字段 | 类型 | 语义 |
|---|---|---|
| `id` | string | 与请求一致（帧无法解析时为 `""`） |
| `ok` | boolean | 成败 |
| `savedPath` | string | **仅 ok=true**：桌面端落盘后的绝对路径 |
| `error` | string | **仅 ok=false**：人类可读原因（中文） |

失败回执**不含** `savedPath`，成功回执**不含** `error`（Rust 侧 `skip_serializing_if`）。

### 2.2 传输层（插件不需要关心）

- **Chrome/Edge 真实链路**：Chrome 的 native messaging 传输是「4 字节小端长度 +
  载荷」，**由浏览器代劳**——扩展侧用 `port.postMessage(obj)` 发对象、用
  `port.onMessage` 收对象即可，**插件不接触长度前缀**。
- **宿主侧**：宿主进程兼容两种输入格式，**默认按 Chrome 长度前缀解析**，只有
  「首字节 `{` 且次字节是可打印 ASCII」才按裸 JSON 行解析（测试与开发工具用）；
  显式强制：`HAWKSNAP_NATIVE_FRAMING=lines|chrome`。
  - 误判兜底：行模式下若缓冲区里已出现**完整的 JSON 值**（即使前面混入了长度
    前缀字节），宿主会直接取出该值并**把帧格式纠正回 Chrome** → 不会挂死。
  - 静默兜底：宿主等待下一帧超过 **5 分钟**即回一条错误回执并退出（插件会收到
    `onDisconnect` → **重连**即可，Chrome 会重新拉起宿主）。
- 回执格式与输入一致（`lines` 写一行，`chrome` 写长度前缀）。

> **插件侧建议**：监听 `port.onDisconnect` 并自动重连（宿主可能因静默超时退出
> 或被浏览器重启）；每条消息都带 `id` 并按 `id` 配对回执。

## 3. 路径约束（桌面端与宿主**各校验一次**）

`capture` 帧的 `path` 必须同时满足：

1. **绝对路径**；
2. 扩展名 `.png`；
3. 文件名形如 `hawksnap-plugin-*.png`；
4. 位于**系统临时目录**内（`std::env::temp_dir()`，macOS 即 `$TMPDIR`）——
   判据是**规范化后的真实路径**（`../` 穿越与指向目录外的符号链接都会现形）；
5. 文件**已存在**（插件刚写完）。

任一不满足 → 回执 `{"ok":false,"error":"…"}`。

**额外的两道闸（解码之前）**

| 闸 | 上限 | 为什么 |
|---|---|---|
| 文件体积 | ≤ **64 MiB** | 防「超大文件被整个读进内存」 |
| 像素数（PNG IHDR 的真实宽高） | ≤ **40 MP** | 防「解码时的内存尖峰」；1920×10000 的整页长截图 ≈ 19.2 MP，在限内 |

另外：帧声明的宽高与图片**真实**宽高**悬殊不符**（任一轴 > 4 倍）→ 拒；仅轻微
差异（如 CSS 像素 vs 设备像素）→ 以真实像素为准并记一条日志。

> 为什么卡死在临时目录：桌面端只接受「插件自己写的临时文件」。固定前缀让
> 「任意文件被诱导导入」在协议层就不可表达（路径根 + 扩展名 + 前缀三重约束）。

### 3.1 扩展写不进临时目录 → 用 `save`（§4）

浏览器扩展**无法**直接写 `$TMPDIR`（扩展沙箱只能经 `chrome.downloads` 写下载
目录）。宿主是原生进程，能写临时目录 → 扩展把图片**数据**交给宿主落盘，拿回
路径再发 `capture` 帧。**推荐链路**：

```
扩展 → save（带 base64 PNG）→ 宿主落盘到 <tmp>/hawksnap-native-inbox/hawksnap-plugin-<id>.png
扩展 → capture（path = 上一步返回的 path）→ 桌面端落盘 save_dir + 入库
```

## 4. 扩展命令：`save`（宿主侧落盘，桌面端不感知）

**插件 → 宿主**：

```json
{"cmd":"save","id":"<uuid>","data":"<base64 PNG，不含 data: 前缀>"}
```

**宿主 → 插件**：

```json
{"id":"<uuid>","ok":true,"path":"/var/folders/…/T/hawksnap-native-inbox/hawksnap-plugin-<uuid>.png"}
{"id":"<uuid>","ok":false,"error":"base64 解码失败: …"}
```

- 落点：`<系统临时目录>/hawksnap-native-inbox/`（0700，仅本人可读写）；
- 文件名：`hawksnap-plugin-<消毒后的 id>.png`（id 只保留 `[A-Za-z0-9_-]`）；
- 上限：单帧 8 MiB（base64 后的载荷）——Chrome 侧浏览器→宿主单消息上限 4 MB，
  超长整页截图请改走 `capture` 之外的分片方案（本版未实现）；
- 回执字段是 `path`（不是 `savedPath`）——它是**临时文件**路径，不是桌面端
  的落盘路径。

> **`save` 是宿主侧命令**：桌面端只认 `capture`——收到 `cmd:"save"` 的帧会被
> 明确拒绝（`桌面端不支持命令: save`），不会被当成一次截图入库。

## 5. 桌面端行为（收到合法 `capture` 后）

1. 校验令牌（见 §6）与路径（§3）；
2. 把临时 PNG **按字节复制**到 `save_dir`（`settings.save_dir`，默认
   `~/Pictures/HawkSnap`），文件名走命名模板 `{app} {date} {time}`，`app` 段固定
   `browser`（如 `browser 2026-10-27 153000.png`）；不重编码（插件给的是 PNG）；
3. 生成 320 长边缩略图 → `<save_dir>/thumbs/<同名>.png`；
4. 历史入库一行：`kind="browser"`、`app="browser"`、宽高取 PNG 实际像素；
5. 向前端推事件 `hawk://browser-capture`，载荷 `{path, width, height}`。

失败（令牌不符 / 路径不合法 / 文件损坏 / 落盘失败）→ 回执 `ok=false` + `error`，
**不入库**。

## 6. 安全模型

| 面 | 约束 |
|---|---|
| 监听地址 | **仅 127.0.0.1**（无外网出口；插件与桌面端之间不过公网） |
| 端口 | 首选 38471，被占则向上回退扫描 16 个；**实际端口写进配置文件**，协议不硬编码端口 |
| 令牌 | 桌面端**每次启动重新生成**（`/dev/urandom` 128 bit）；配置文件权限 **0600**；常量时间比较 |
| 令牌轮换 | 桌面端重启会换令牌与端口 → **宿主每次转发前都重读配置文件**（不信缓存；桌面端没起时重试 5×300ms） |
| 配置文件 | `~/Library/Application Support/com.hawksnap.app/native-host.json`（macOS；Linux `$XDG_CONFIG_HOME/com.hawksnap.app/…`；Windows `%APPDATA%\com.hawksnap.app\…`），内容 `{port, token, pid, hostName}`；**写临时文件再 rename**（原子替换 → 读者不会读到半截 JSON）；配置目录新建时权限 **0700** |
| 来源限制 | 只有拿到令牌的进程能驱动桌面端；插件拿不到令牌（令牌只在宿主与桌面端之间流转） |
| 载荷上限 | 单帧 8 MiB；连接读写超时 15s（桌面端）/ 30s（宿主↔桌面端） |
| 写权限面 | 桌面端只会写 `save_dir` 与其 `thumbs/` 子目录；临时文件只读不删 |

## 7. 状态与排错

- 桌面端是否就绪：Tauri 命令 `native_messaging_status() -> bool`（主面板
  「浏览器插件」分区展示；`true` = 回环监听已起）。
- 宿主侧常见失败：
  - `无法连接 HawkSnap 桌面端 (127.0.0.1:<port>)` → 桌面端没在跑，或端口配置
    是旧的（重启桌面端即可刷新配置）。
  - `读取宿主配置失败` → 桌面端从未启动过，或配置文件权限/路径不对。
  - `未授权的请求（令牌无效）` → 桌面端的令牌与宿主带来的一致性被破坏（正常不会
    发生：宿主每次转发都重读配置）。若出现，说明有第三方在直连回环端口。
- 端到端手工验证（无需浏览器）：

  ```bash
  # 1) 起桌面端（或直接跑 app），确认配置文件已生成
  cat ~/Library/Application\ Support/com.hawksnap.app/native-host.json
  # 2) 用裸 JSON 行喂宿主（HAWKSNAP_NATIVE_FRAMING=lines）
  echo '{"cmd":"capture","id":"t1","path":"/tmp/hawksnap-plugin-t1.png","width":1,"height":1}' \
    | HAWKSNAP_NATIVE_FRAMING=lines ./target/release/hawksnap-host
  ```
