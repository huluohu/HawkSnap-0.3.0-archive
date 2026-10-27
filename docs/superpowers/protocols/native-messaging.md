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
- `allowed_origins` 必须列出插件的真实扩展 ID（未列入的来源一律拒绝连接）。
- 宿主二进制构建：`cargo build --release --features native-host --bin hawksnap-host
  --manifest-path app/src-tauri/Cargo.toml`（feature 门控，不进默认构建）。

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
- **宿主侧**：宿主进程同时兼容两种输入格式并在**首帧自动识别**（之后整条流沿用）：
  - `lines`：裸 JSON 行（`{"cmd":...}\n`）——测试与开发工具用；
  - `chrome`：4 字节小端长度 + JSON 载荷（浏览器链路）。
- 回执格式与输入一致（`lines` 写一行，`chrome` 写长度前缀）。
- 强制指定：`HAWKSNAP_NATIVE_FRAMING=lines|chrome`（宿主进程环境变量，调试用）。

## 3. 路径约束（桌面端与宿主**各校验一次**）

`capture` 帧的 `path` 必须同时满足：

1. **绝对路径**；
2. 扩展名 `.png`；
3. 文件名形如 `hawksnap-plugin-*.png`；
4. 位于**系统临时目录**内（`std::env::temp_dir()`，macOS 即 `$TMPDIR`）——
   判据是**规范化后的真实路径**（`../` 穿越与指向目录外的符号链接都会现形）；
5. 文件**已存在**（插件刚写完）。

任一不满足 → 回执 `{"ok":false,"error":"…"}`。

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
| 令牌 | 桌面端**每次启动重新生成**（`/dev/urandom` 128 bit）；配置文件权限 **0600** |
| 配置文件 | `~/Library/Application Support/com.hawksnap.app/native-host.json`（macOS；Linux `$XDG_CONFIG_HOME/com.hawksnap.app/…`；Windows `%APPDATA%\com.hawksnap.app\…`），内容 `{port, token, pid, hostName}` |
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
  - `未授权的请求（令牌无效）` → 桌面端重启过（令牌轮换）而宿主拿的是旧令牌
    → 宿主每次启动都重读配置，正常不会发生；出现即说明有第三方在直连回环端口。
- 端到端手工验证（无需浏览器）：

  ```bash
  # 1) 起桌面端（或直接跑 app），确认配置文件已生成
  cat ~/Library/Application\ Support/com.hawksnap.app/native-host.json
  # 2) 用裸 JSON 行喂宿主（HAWKSNAP_NATIVE_FRAMING=lines）
  echo '{"cmd":"capture","id":"t1","path":"/tmp/hawksnap-plugin-t1.png","width":1,"height":1}' \
    | HAWKSNAP_NATIVE_FRAMING=lines ./target/release/hawksnap-host
  ```
