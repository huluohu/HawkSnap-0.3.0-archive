# HawkSnap 官网（site/）

纯静态单页 + 文档页：**无框架、无构建、无外部资源**——CI 直接把本目录作为 GitHub Pages artifact 部署，不需要任何打包步骤。

## 结构

```
site/
├── index.html          # 产品页：Hero + 功能网格 + 平台支持 + 隐私与许可
├── docs/index.html     # 文档页：安装 / 快捷键 / 功能入口速查 / 浏览器插件 / FAQ
├── assets/
│   ├── style.css       # 手写样式（暗色默认 + 浅色切换；鹰隼蓝 #22405F→#0C1624 / 琥珀 #FFB020 / 墨色 #0D1522）
│   ├── main.js         # 零依赖交互：主题切换（localStorage）/ 平滑滚动 / 复制按钮 / 页脚年份
│   ├── icon.svg        # 应用图标（自 design/logo/final/icon.svg 拷入）
│   └── lockup.svg      # 横排组合标（自 design/logo/final/lockup.svg 拷入；深色文字，页脚浅色底上使用）
└── README.md
```

## 部署（GitHub Pages）

由 `.github/workflows/ci.yml` 的 **`pages`** job 负责：

- **触发**：`workflow_dispatch`（手动）或 push 到 `main` 且**本次提交触达 `site/`**（job 内用
  `git diff --name-only <before> <sha>` 判定，未触达则跳过——CI 其余 job 不受影响）；
- **动作**：`actions/configure-pages` → `actions/upload-pages-artifact`（artifact = `site/` 目录）→
  `actions/deploy-pages`；
- **权限**：job 级 `permissions: pages: write, id-token: write`（deploy-pages 要求 OIDC）；
- **前置（一次性，仓库管理员）**：仓库 *Settings → Pages → Build and deployment → Source* 选
  **GitHub Actions**。没改过的话首次部署前需要设置一次，否则 `deploy-pages` 会报错。

## 本地预览

无服务器、无构建：直接用浏览器打开文件即可（所有资源均为相对路径，`file://` 下工作正常）。

```bash
open site/index.html        # macOS；或双击文件
open site/docs/index.html
```

注意：`file://` 下若浏览器限制 Clipboard API，代码块复制按钮会自动降级为 `execCommand` 路径（main.js 已处理）。

## 约束（改站点时保持）

- **零外部资源**：不引任何 CDN / Google Fonts / 外链图片；字体只用系统字体栈；
- 下载链接一律指向 <https://github.com/huluohu/HawkSnap/releases/latest>（不硬编码版本号）；
- 平台支持、隐私与许可文案与仓库 README / LICENSE 口径一致（如实标注「未适配、未实测」）；
- 主题：暗色默认，`prefers-color-scheme` + 手动切换，`localStorage["hawksnap-theme"]` 持久化。
