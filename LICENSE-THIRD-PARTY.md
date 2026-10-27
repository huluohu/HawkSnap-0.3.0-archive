# 第三方组件许可声明（HawkSnap 1.0.0）

本文件列出 HawkSnap 分发时**需要特别声明**的第三方组件。完整依赖清单与精确版本见仓库根的
`Cargo.lock`（各组件的完整许可文本随其上游仓库发布）。

## 需要特别声明的组件（copyleft / 传染性许可）

| 组件 | 版本 | 许可 | 用途 | 分发义务 |
|---|---|---|---|---|
| [gifski](https://github.com/AnimatedPNG/gifski) | 1.34（`default-features = false`） | **AGPL-3.0-or-later** | M3 GIF 导出的编码器（`add_frame_rgba` 内存直喂；只用库形态，不含其 CLI） | 随二进制分发时须提供**完整对应源码**（含本仓库在该版本下的构建脚本与依赖锁定信息），并保留本声明 |

> **口径裁定（见 M3 任务 N2 的 Ruling）**：HawkSnap 免费且以**公开仓库 + release 源码包**的形式
> 分发 → 满足 AGPL-3.0-or-later 的「提供对应源码」义务。**若未来引入闭源分发形态**
> （例如闭源商业版 / 预装固件），必须改用 MIT 系编码器（例如 `gif` crate）并重做 GIF 编码层。

## 其余组件

其余依赖均为 MIT / Apache-2.0 / BSD / ISC / Unicode-3.0 等宽松许可，不要求源码公开。逐项清单以
`Cargo.lock` 与各 crate 的上游仓库为准；本项目未对任何组件的许可文本做修改。

## 本项目自身

HawkSnap 整体作品以 **GNU AGPL-3.0-or-later** 授权——完整许可文本见仓库根的
[`LICENSE`](LICENSE)（0.3.0 起随仓库提供；此前版本未附带该文件）。

分发二进制时必须同时提供**对应源码**（AGPL-3.0-or-later §6）：本仓库在该版本下的完整源码
（含 `Cargo.lock` = 构建脚本与依赖锁定信息），以及 Release 页面上的源码归档
`HawkSnap-<tag>-source.tar.gz`（CI 由 `git archive` 从该 tag 生成）。HawkSnap 以
**免费、纯本地**的形式发布。
