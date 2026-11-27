---
name: kumu-todos
description: 在 Kumu 里读取和更新当前项目的 TODO 看板。当用户让你查看待办、认领某个 TODO、拆分子任务、更新进展、把任务标记为待确认或完成时使用。
---

# Kumu TODO 看板

Kumu 的项目级 TODO 看板（在应用内点击左侧项目名或按 `⌘⇧T` 进入）按根仓库隔离（Git Worktree 自动共享父仓库的看板），并支持单条 TODO 同时关联多个 Agent 与历史 Session（1:N）。在 Kumu 的窗格里，你可以直接通过 `kumu todo` 读写当前仓库的任务，无需指定项目 id——Kumu 会自动按 `$KUMU_PANE` 定位你所在的仓库看板，并将 `--bind` / `--unbind` / `review` 精确作用域限定到当前窗格的关联记录，不会影响同一任务下其他协作 Agent 的状态。

- `kumu todo list` 列出当前仓库的任务。
- `kumu todo show <id>` 查看单条任务的完整描述和关联 Agent 信息。
- `kumu todo add <title>` 新建任务（可加 `--desc`、`--status`、`--bind`）。
- `kumu todo update <id>` 更新标题、描述、状态或绑定关系。
- `kumu todo review <id>` 将任务标记为「待确认」（保留在「进行中」，等待用户确认后归档）。
- `kumu todo done <id>` 将任务直接标记为「已完成」。
- `kumu todo remove <id>` 删除任务。

## 先确认入口

在 Kumu 的窗格里，环境变量 `KUMU_APP_BINARY` 指向 Kumu 本体，`KUMU_PANE` 是你自己的窗格 id：

```sh
"$KUMU_APP_BINARY" todo list
```

不在 Kumu 里（`KUMU_APP_BINARY` 为空）时用 PATH 上的 `kumu`。

## 常用操作

```sh
# 查看所有待办和进行中的任务
"$KUMU_APP_BINARY" todo list --json

# 只看绑定到我自己的任务
"$KUMU_APP_BINARY" todo list --mine

# 新建任务并绑定到当前 Agent 窗格、进入「进行中」
"$KUMU_APP_BINARY" todo add "重构插件宿主接口" --desc "支持跨页签查询 Agent 状态" --status in_progress --bind

# 认领一条待办任务（置为进行中并绑定当前窗格）
"$KUMU_APP_BINARY" todo update 12 --status in_progress --bind

# 干完活后请用户验收（推荐：进入待确认状态，由用户确认后再归档）
"$KUMU_APP_BINARY" todo review 12

# 用户明确要求直接关闭时
"$KUMU_APP_BINARY" todo done 12
```

## 约定

1. 认领待办任务时同时传 `--status in_progress --bind`，这样看板上会实时显示你的运行状态，用户也能一键跳转到你的窗格。
2. 完成开发与验证后，默认使用 `kumu todo review <id>` 提醒用户确认，而不是直接 `done`，除非用户明确让你关闭任务。
3. 所有子命令都支持 `--json`，便于脚本化读取。
