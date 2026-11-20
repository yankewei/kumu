---
name: kumu-panes
description: 在 Kumu 里读取和操作其它窗格（pane）。当用户说“看看右边那个窗格”、“左边那个终端在报什么错”、“读一下 pane 3”、“在右侧开一个 codex 窗格让它 review diff”、“让另一个 agent 去做这个”，或需要参考/驱动同一窗口里别的终端时使用。
---

# Kumu 窗格

Kumu 的每个窗格是一个独立终端。你只能看到自己这一格，但可以问 Kumu 要别的窗格：读它们的内容，开新的 Agent 窗格，或者往已有的 Agent 窗格发一句话。

- `kumu panes` 列出所有窗格（id、项目、标题、cwd、尺寸、是否聚焦）。
- `kumu pane read <目标>` 返回一个窗格的纯文本：屏幕内容，加上可选的滚动历史。
- `kumu pane split right|down --agent <id>` 在你旁边开一个 Agent 窗格。
- `kumu pane send <目标> <文字>` 往一个 **Agent 窗格** 输入文字并回车。

## 先确认入口

在 Kumu 的窗格里，环境变量 `KUMU_APP_BINARY` 指向 Kumu 本体，`KUMU_PANE` 是你自己的窗格 id：

```sh
"$KUMU_APP_BINARY" panes
```

不在 Kumu 里（`KUMU_APP_BINARY` 为空）时用 PATH 上的 `kumu`。两者都没有就告诉用户：需要在 Kumu 窗格里运行。

## 目标怎么写

| 目标 | 含义 |
| --- | --- |
| `right` `left` `up` `down` | 相对你自己的窗格；没有 `KUMU_PANE` 时相对当前聚焦的窗格 |
| `focused` | 当前聚焦的窗格（默认） |
| `current` / `self` | 你自己 |
| `3` | 指定 id，以 `panes` 列出的为准 |

## 开一个 Agent 窗格

```sh
# 在你右侧开一个 codex 窗格，并让它立刻开始 review 所有改动
"$KUMU_APP_BINARY" pane split right --agent codex --prompt "review 当前仓库的所有改动（git diff），列出问题和建议"

# 向下（纵向）开一个
"$KUMU_APP_BINARY" pane split down --agent codex --prompt "跑一遍测试并总结失败原因"
```

- 方向只有 `right`（横向，右边）和 `down`（纵向，下面）：Kumu 只会在目标之后追加窗格。
- `--agent` 用注册 id（`codex`、`claude`、`opencode`、`pi`、`omp`、`cursor`、`grok`、`agy`）或显示名；没安装会报 `agentNotInstalled`。
- `--prompt` 会等新 Agent 画出自己的输入框（最多约 12 秒）再粘贴文字并回车，所以新窗格会直接开工；也可以省略 `--prompt`，只用它占位。
- 省略 `--prompt` 时方向后面剩下的词也会当作 prompt，例如 `pane split right --agent codex review the diff`。
- 新窗格开在你旁边，但你自己的键盘焦点不变。

## 往 Agent 窗格说话

```sh
"$KUMU_APP_BINARY" pane send right "继续，把测试也补上"
"$KUMU_APP_BINARY" pane send 3 "先别改 src/app.rs"        # 指定 id
"$KUMU_APP_BINARY" pane send right "草稿，先别发" --no-enter
```

- 默认粘贴文字后回车；`--no-enter` 只输入不提交。
- 多行文字可以用引号包起来，会作为一次粘贴送入。
- 文本以 `-` 开头，或者整段正好是 `--json`、`-h` 这类词时，先加 `--`：`pane send right -- "-x"`。
- 只能发给 Agent 窗格（`panes` 里带 `agent` 的）；shell 窗格会报 `notAgent`，因为在那里输入等于执行命令。
- 如果目标 Agent 还在启动中，先用 `pane read` 确认它已经画出输入框，或者一开始就用 `pane split --prompt`。

## 读内容

```sh
# 所有窗格
"$KUMU_APP_BINARY" panes

# 右边窗格最近 200 行滚动历史 + 屏幕，只要最后 60 行
"$KUMU_APP_BINARY" pane read right --history 200 --tail 60

# 指定窗格的完整输出，机器可读
"$KUMU_APP_BINARY" pane read 3 --json
```

输出第一行是窗格信息（id、项目、标题、agent、cwd、尺寸、focused/shown），`---` 之后是纯文本，没有颜色和控制符。需要“刚才发生了什么”时带 `--history N`（最多 2000 行）；只看结论时用 `--tail` 截断。全屏 TUI（多数 Agent 界面跑在备用屏幕上）没有滚动历史，`--history` 只会得到当前屏幕；要更早的内容，让那个 Agent 自己打印一段摘要。

`pane split --prompt ""` 会被当成用法错误；不带 prompt 就直接省略这个参数，只开窗格。

## 做法

1. 不确定是哪个窗格时，先跑 `panes`，按项目、标题、cwd 挑出目标。
2. 用户要“另开一个 agent 做某件事”时，用 `pane split … --prompt "完整任务"`，把任务写全（那个 agent 听不到你和用户的对话），然后告诉用户开在哪个窗格。
3. 引用其它窗格的内容时说明来自哪个窗格（id 和标题），别让用户以为是你自己终端里的输出。

## 出错时

- `unknownTarget` / `unknownPane` / `noPane`：目标写错了，或者那个方向没有窗格；用 `panes` 重新确认。
- `unknownDirection`：只能 `right` 或 `down`。
- `unknownAgent` / `agentNotInstalled`：Agent 名字写错或这台 Mac 没装。
- `notAgent`：目标不是 Agent 窗格（可能是普通 shell）。
- `paneGone`：窗格没有运行中的进程（从未启动或已退出）。
- `promptTimeout`：新窗格开了，但 Agent 12 秒内没画出输入框；窗格还在，可以过一会儿用 `pane send` 补发。
- `unavailable` / `cannot reach kumu`：Kumu 没开，或者命令不在 Kumu 窗格里。直接告诉用户，不要反复重试。
