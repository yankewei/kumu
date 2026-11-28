---
name: kumu-panes
description: 在 Kumu 里读取和操作其它窗格（pane）。当用户说“看看右边那个窗格”、“左边那个终端在报什么错”、“读一下 pane 3”、“在右侧开一个 codex 窗格让它 review diff”、“让另一个 agent 去做这个”，或需要参考/驱动同一窗口里别的终端时使用。
---

# Kumu 窗格

Kumu 的每个窗格是一个独立终端。你可以读取相邻窗格输出、在旁边拆分新的 Agent 窗格、向 Agent 窗格发消息，并等待其完成。

## 非 Kumu 环境守卫（必守）

**只有当 `KUMU_TERMINAL=1` 或 `KUMU_PANE` 非空时才执行窗格操作。** 如果你运行在 Kumu 之外（例如系统终端、VS Code、Cursor/Antigravity IDE），禁止调用任何 `pane` 命令，直接告知用户需要在 Kumu 窗格内运行，避免误操作 Kumu 当前聚焦的无关窗格。

## 工具优先级

1. **优先调用 Kumu MCP 工具**（若当前会话已挂载）：`pane_list`、`pane_read`、`pane_scroll`、`pane_split`、`pane_send`、`pane_wait`、`pane_close`。参数以工具 schema 为准，无需手动拼接 shell 命令。
2. **CLI 兜底**（未启用 MCP 时，例如 Pi / Oh My Pi 或脚本环境）：通过 `"$KUMU_APP_BINARY"` 调用控制命令（完整参数见 `"$KUMU_APP_BINARY" pane --help`）：

```sh
"$KUMU_APP_BINARY" panes
"$KUMU_APP_BINARY" pane read right --history 200 --tail 60
"$KUMU_APP_BINARY" pane split right --agent codex --prompt "review 当前改动"
"$KUMU_APP_BINARY" pane wait right --timeout 300
"$KUMU_APP_BINARY" pane send right "把测试也补上"
"$KUMU_APP_BINARY" pane close right
```

## 协作规则与边界

- **目标定位**：支持相对位置（`right` / `left` / `up` / `down`）、`focused`、`current` / `self`，或 `pane_list` 中的数字 id。
- **拆分新 Agent**：方向仅支持 `right` / `down`，新建时**不会抢走当前键盘焦点**。派发任务时直接带 `prompt`，Kumu 会等新 Agent 画出输入框后再提交；若目标 Agent 未安装会报 `agentNotInstalled`。
- **发消息边界**：`pane_send` **仅允许发给 Agent 窗格**，普通 shell 窗格会报 `notAgent`（防止误执行 shell 命令）。
- **等待完成**：派发任务或追加消息后，用 `pane_wait` 阻塞等待目标进入 `waiting` / `completed` / `failed` 或退出，避免反复轮询 `pane_read`；向已有 Agent 追加消息后传 `next: true`（CLI 为 `--next`）等待下一轮终态。
- **全屏 TUI 翻页**：OpenCode / Claude Code 等全屏 TUI 超出当前屏时，先用 `pane_scroll` 翻页再 `pane_read`。
- **关闭窗格**：默认拒绝关闭调用方自身（`refuseSelf`），确需关闭自身时显式传 `allowSelf` / `--self`。
