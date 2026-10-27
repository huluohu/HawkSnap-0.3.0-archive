# HawkSnap 浏览器插件（MV3）

Chrome/Edge 扩展：整页 / 可见区 / 区域截图 → 标注（矩形 / 箭头 / 画笔 / 荧光笔 / 文字）→ 复制 / 下载 PNG，可选发送到 HawkSnap 桌面端历史（Native Messaging）。

TypeScript + esbuild，**零运行时依赖**（唯一 devDependency：esbuild）。

## 构建 / 测试 / 加载

```bash
cd plugin
npm install     # 仓库未提交 package-lock.json（仅 esbuild 一个 devDep），故用 install；
                # 待 lockfile 首次生成并提交后改回 npm ci
npm run build   # 产物 → plugin/dist（已 gitignore）
npm test        # 纯逻辑单测（node:test，远端 Linux 农场跑）
```

加载（Chrome / Edge 相同）：

1. 打开 `chrome://extensions`（Edge：`edge://extensions`）；
2. 右上角开启「开发者模式」；
3. 点「加载已解压的扩展程序」，选择 **`plugin/dist`** 目录；
4. 工具栏出现隼形图标 → 点击选择：整页截图 / 可见区域 / 区域截取。

## manifest 权限（最小集，逐项理由）

| 权限 | 为什么需要 |
|---|---|
| `activeTab` | 工具栏点击 / 快捷键触发时临时获得当前标签的访问权（`captureVisibleTab`、注入选区浮层）。不触发时无任何页面访问权——对应「只截用户显式触发的页面」的隐私约束。 |
| `scripting` | 向页面注入 `content-script.js`（区域选择的拖拽浮层）。仅在用户选择「区域截取」时注入。 |
| `debugger` | 整页截图：`Page.captureScreenshot { captureBeyondViewport: true }`（spec §3.5 的 10000px 高整页保真路径）。使用期间页面顶部会出现浏览器调试横幅，截图结束即 `detach` 消失。 |
| `downloads` | 把标注后的 PNG 保存到本地下载目录；也是「发送到桌面端」失败时的降级出口（超 4MB / 宿主缺失 / 断连）。 |
| `clipboardWrite` | 编辑器「复制到剪贴板」按钮（`navigator.clipboard.write`）——浏览器要求写剪贴板需显式权限。 |
| `storage` | `storage.session`（仅内存级、随浏览器关闭清空）：service worker 把截图交给编辑器页的短时交接，不落盘、不上报。 |
| `offscreen` | MV3 service worker 无 DOM，需 offscreen document 用 canvas 对区域截图做裁剪。 |
| `nativeMessaging` | 「发送到 HawkSnap 桌面端」（可选功能）：`chrome.runtime.connectNative("com.hawksnap.app")` 必需此权限，缺失时浏览器直接拒绝连接。宿主只允许本插件 ID（宿主 manifest `allowed_origins`）。 |

没有 `host_permissions`：不预授权任何站点；页面访问一律走 activeTab 的用户意图授予。
没有 `notifications` / `tabs` 读权限 / 后台脚本：错误用图标角标就地提示。

## Native Messaging（可选：发送到桌面端）

- 宿主名 `com.hawksnap.app`；帧协议逐字对齐 `docs/superpowers/protocols/native-messaging.md`：
  1. 插件 → 宿主：`{"cmd":"save","id":...,"data":"<base64 PNG>"}` → 回执 `{"id":...,"ok":true,"path":"<临时文件>"}`；
  2. 插件 → 宿主：`{"cmd":"capture","id":...,"path":"<上一步 path>","width":N,"height":N}` → 回执 `{"id":...,"ok":true,"savedPath":"<桌面端落盘>"}`；
  3. `width/height` 填真实像素（从 PNG IHDR 读取）或 0；填错超 4 倍会被桌面端拒收。
- 注册：桌面端仓库 `scripts/install-native-host.sh --ext-id <扩展ID>`（扩展 ID 见插件设置页）。
- 失败语义（自动降级为本地下载并提示原因）：
  - 载荷超浏览器 → 宿主单消息 4 MB（整页长图常见，分片本版未实现）；
  - 宿主未注册 / 未安装（`connectNative` 即断连且无消息）；
  - 回执超时（30s，对齐宿主↔桌面端超时）；
  - 桌面端拒收（中文原因透传）。
- 宿主 5 分钟静默退出由协议规定，插件每次发送都新建连接，天然规避陈旧连接。

## 整页尺寸策略（spec §3.5）

- Chrome 渲染面单边安全上限 16384px、桌面端桥 40 MP 像素闸；
- 优先原尺寸保真；超限**整体降采样**（`Emulation.setDeviceMetricsOverride` 压 deviceScaleFactor，不裁内容）；
- 降到 dsf 下限 0.1 仍放不下才**裁剪**，并在编辑器状态栏明确标注；
- 懒加载图：截图前有界滚动（0.9 视口/步）+ 有界等待（单步 1.5s、总预算 8s），超时放弃等待直接截图。

## 目录

```
plugin/
├── manifest.json          # MV3；权限见上表
├── src/
│   ├── service-worker.ts  # 捕获编排（debugger 整页 / captureVisibleTab / 区域）
│   ├── content-script.ts  # 区域选择浮层
│   ├── offscreen.ts       # canvas 裁剪（SW 无 DOM）
│   ├── popup.ts / editor.ts / options.ts
│   ├── html/              # 对应页面 HTML（构建时拷入 dist）
│   └── shared/            # 纯逻辑：协议帧、尺寸策略、标注几何、native 客户端
├── tests/                 # node:test（esbuild 转 CJS 后跑）
└── scripts/               # build.mjs / test.mjs / gen-icons.mjs（零依赖 PNG 生成）
```
