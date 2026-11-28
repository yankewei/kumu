---
name: kumu-artifacts
description: 把方案、设计稿、报告、架构图或可交互 HTML 原型发布成 Kumu artifact，在右侧 Artifacts 抽屉和全屏阅读页里查看。当用户说“做个方案”“给我一个设计稿”“生成报告/原型”“这个文档在终端里看不清”，或你准备输出大段 Markdown/HTML 时使用。
---

# Kumu Artifacts

Kumu 的每个项目都有一个本地 `.kumu/artifacts` 目录。将 Markdown、HTML 或纯文本发布进去后，Kumu 会自动记录发布窗格、Agent 与会话，并在右侧 **Artifacts** 抽屉中按会话分组展示。

**不要把整份 HTML 或大段长文档直接打印到终端；直接发布为 artifact。**

## 工具优先级

1. **优先调用 Kumu MCP 工具**（若当前会话已挂载）：`artifact_publish`、`artifact_list`、`artifact_open`、`artifact_remove`（参数以工具 schema 为准）。
2. **Pi / Oh My Pi**：直接调用原生扩展工具 `kumu_artifact_publish` 和 `kumu_artifact_list`。
3. **CLI 兜底**（未启用 MCP 时）：通过 `"$KUMU_APP_BINARY"`（或 PATH 上的 `kumu`）调用（完整参数见 `kumu artifact --help`）：

```sh
"$KUMU_APP_BINARY" artifact publish ./plan.md --title "登录页重构方案"
"$KUMU_APP_BINARY" artifact publish ./mockup.html --title "交互原型" --open
"$KUMU_APP_BINARY" artifact publish ./mockup.html --replace <ARTIFACT_ID>
"$KUMU_APP_BINARY" artifact list
```

## 什么时候发布

- **应该发布**：方案、设计稿、评审意见、调研报告、架构说明、HTML 原型/演示页/可视化图表、或超过一屏在终端阅读体验明显变差的长文档；用户明确说“发到 artifacts”“在 Kumu 里给我看”。
- **不要发布**：普通的一两段回答、简短代码改动说明、命令输出总结。

## 规则与约束

- **标题**：始终提供清晰可读的中文 `title`。
- **迭代更新**：修改同一份方案或原型时，使用 `replace`（CLI 为 `--replace <id>`）原地更新，不要反复产生重复条目。
- **焦点礼仪**：默认只刷新抽屉、不抢用户焦点；仅当用户明确说“打开看看”时才传 `open: true`（CLI 为 `--open`）。
- **目录与资源**：HTML 依赖本地 CSS/JS/图片时发布整个目录并指定入口（`entry` / `--entry`）；内嵌 WebView 直接渲染并隔离非持久存储，外链交给系统浏览器。
- **体积与隐私**：仅写入项目本地 `.kumu/artifacts`（`.kumu/.gitignore` 默认忽略），不上传；单文件上限 32 MB，目录上限 64 MB / 500 文件，拒绝指向目录外的 symlink。
- **失败处理**：若提示 `cannot reach kumu`，说明不在 Kumu 窗格内或 Kumu 未运行，文件仍会写入本地 `.kumu/artifacts`，等 Kumu 打开该项目即可查看。
