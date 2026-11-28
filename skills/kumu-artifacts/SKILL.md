---
name: kumu-artifacts
description: 把方案、设计稿、报告、架构图或可交互 HTML 原型发布成 Kumu artifact，在右侧 Artifacts 抽屉和全屏阅读页里查看。当用户说“做个方案”“给我一个设计稿”“生成报告/原型”“这个文档在终端里看不清”，或你准备输出大段 Markdown/HTML 时使用。
---

# Kumu Artifacts

Kumu 的每个项目都有一个 `.kumu/artifacts` 目录。把 Markdown、HTML 或纯文本发布进去，Kumu 会记录是哪个窗格、哪个 Agent、哪个会话发布的，并在右侧 **Artifacts** 抽屉里按会话分组展示。任何窗格都能打开项目里的全部 artifacts。

不要用 `cat` 把整份 HTML 或长文档打到终端；直接发布。

## 先确认入口

在 Kumu 窗格里，`$KUMU_APP_BINARY` 指向 Kumu 本体：

```sh
"$KUMU_APP_BINARY" artifact publish ./plan.md --title "登录页重构方案"
```

不在 Kumu 里（`KUMU_APP_BINARY` 为空）时用 PATH 上的 `kumu`；两者都没有就告诉用户需要在 Kumu 窗格内运行，或先安装 Kumu。

## 什么时候发布

- 用户要一份方案、设计稿、评审意见、调研报告、架构说明、排期；
- 你生成了一个 HTML 原型、演示页、可视化图表；
- 内容超过一屏，终端里阅读体验明显变差；
- 用户说“发到 artifacts”“在 Kumu 里给我看”。

普通的一两段回答、代码修改说明、命令输出总结，不需要发布。

## 常用命令

```sh
# Markdown 方案
"$KUMU_APP_BINARY" artifact publish ./docs/plan.md --title "登录页重构方案"

# HTML 原型（单文件或目录）
"$KUMU_APP_BINARY" artifact publish ./mockup.html --title "交互原型"
"$KUMU_APP_BINARY" artifact publish ./site --entry index.html --title "营销页原型"

# 从 stdin 发布（适合临时报告）
cat <<'EOF' | "$KUMU_APP_BINARY" artifact publish - --title "排查报告"
# 结论
...
EOF

# 迭代同一份 artifact，不要产生新条目
"$KUMU_APP_BINARY" artifact publish ./mockup.html --replace <ARTIFACT_ID>

# 发布后立即打开阅读页（仅在用户明确想看时）
"$KUMU_APP_BINARY" artifact publish ./plan.md --title "方案" --open

# 查看与清理
"$KUMU_APP_BINARY" artifact list
"$KUMU_APP_BINARY" artifact path <ARTIFACT_ID>
"$KUMU_APP_BINARY" artifact remove <ARTIFACT_ID> --force
```

## 规则

- **标题**：始终传 `--title`，用用户能看懂的中文；缺省标题只会取首个 Markdown 标题或 HTML `<title>`。
- **类型**：`.md` / `.html` / `.txt` 会自动识别；stdin 会嗅探 `<html`，其余按 Markdown。需要时用 `--kind`。
- **迭代**：改同一份方案或原型时用 `--replace <id>`；`publish` 的 `--json` 输出里有 `id`。
- **目录**：HTML 依赖本地 CSS/JS/图片时发布整个目录，并用 `--entry` 指定入口；目录默认拒绝 symlink，有总大小与文件数上限。
- **打开**：默认只刷新抽屉，不抢用户焦点；只有用户说“打开看看”时才加 `--open`。
- **会话**：在 Kumu 窗格里发布会自动记录 pane、Agent 和会话 id，不需要手动传 `--session`。
- **隐私**：artifact 只写入项目本地的 `.kumu/artifacts`，不会上传；`.kumu/.gitignore` 默认忽略快照。

## HTML 说明

HTML artifact 会在阅读页内嵌 WebView 中直接渲染，支持本地 CSS/JS/图片、CDN 资源和根路径 `/assets/...`；每个页面使用非持久数据存储，关闭即丢弃。若 WebKit 不可用，阅读页会提供「在浏览器打开」兜底。外链点击会交给系统浏览器，不会把阅读页导航走。

## 失败时

- `cannot reach kumu`：Kumu 没运行或当前不在 Kumu 窗格内；文件仍会写入 `.kumu/artifacts`，等 Kumu 打开该项目就能看到。
- `artifact ... does not exist`：检查 `kumu artifact list` 里的 id。
- 发布被拒绝：检查单文件是否超过 32 MB、目录是否超过 64 MB / 500 个文件，或是否包含指向目录外的 symlink。
