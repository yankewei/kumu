# Agent 通知

Kumu 不改写用户的 Agent 启动命令，不添加 `-c`、`--settings` 或 `--extension`，不替换 PATH、ZDOTDIR 或 shell 启动文件。

在 **设置 → 服务商 → 编码 Agent** 对已安装的 Agent 点击 **开启通知**，把通知处理器注册到 Agent 自己的持久配置或自动发现目录，然后重新启动 Agent。之后使用普通命令、绝对路径、别名或自定义脚本启动，均不要求额外的通知参数。已开启时同一位置显示 **关闭通知**。

未安装的 Agent 只显示“未安装”，没有任何通知操作：注册文件只有对应 Agent 的进程才会加载，为一个不存在的 CLI 预写配置既不会生效，也会让状态变成没有覆盖的声明。安装 CLI 后，这一行才会出现 **开启通知**。

开启和重新开启保留用户已有 hooks、注释与配置文件链接；关闭仅删除带 Kumu 标记的处理器或指向 Kumu 管理目录的链接。配置格式错误或接入路径已有其他文件时会报错，不覆盖原配置。此工作区的开发与测试没有替用户安装到真实的 Agent 配置，需从设置中执行开启。

| Agent | 默认注册位置 | 事件来源 |
| --- | --- | --- |
| Codex | `~/.codex/hooks.json` | `UserPromptSubmit`、`PostToolUse`、`Stop`、`PermissionRequest`、匹配 `request_user_input` 的 `PreToolUse` |
| Claude Code | `~/.claude/settings.json` 的 `hooks` | `UserPromptSubmit`、`PostToolUse`、`PostToolUseFailure`、`Stop`、`PermissionRequest`、提问／计划确认的 `PreToolUse`、MCP elicitation 的 `Notification` |
| OpenCode | `~/.config/opencode/plugins/kumu-notifications/` | 自动发现的插件，支持 V2 TUI 事件和 V1 `server()` hooks |
| Pi | `~/.pi/agent/extensions/kumu-notifications/` | `agent_settled`、`ui_prompt_start`、提问工具事件 |
| Oh My Pi | `~/.omp/agent/extensions/kumu-notifications/` | `agent_end`、`tool_approval_requested`、提问工具事件、输入／确认／选择对话框 |
| Cursor CLI | `~/.cursor/hooks.json` | `stop`（只覆盖任务结束） |
| Grok Build | `~/.grok/hooks/kumu-notifications.json` | `Stop`、`Notification` |
| Antigravity CLI | `~/.gemini/config/hooks.json` 的 `kumu-notifications` | `SessionStart`（静默上报会话 id）、`PreInvocation`（每次调用模型前上报运行中）、`Stop`（任务结束或失败，取消不提醒） |

Cursor 的钩子文件是扁平结构：事件直接对应处理器数组，处理器自身就是命令（不像 Claude／Codex／Grok 那样再嵌一层 `hooks`），因此 Kumu 用单独的读改写逻辑处理它，并保留 `version` 字段与其他事件。Cursor 没有“等待权限”事件：`beforeShellExecution` 是权限闸门，返回格式错误会阻断命令，因此不用它发送通知，Cursor 只提供任务结束提醒。

Grok 的钩子格式与 Claude 相同，可以复用同一套 CST 逻辑；Kumu 写自己的 `~/.grok/hooks/kumu-notifications.json`，不修改用户已有的 hook 文件。

Antigravity 的全局 `hooks.json` 以顶层命名键组织钩子，没有外层 `hooks` 字段，且 `SessionStart`／`PreInvocation`／`Stop` 为扁平处理器数组。Kumu 只管理自己的 `kumu-notifications` 键，保留同文件内的其他命名钩子与注释；由于同一全局文件会被 Antigravity IDE 与桌面端读取，命令在结束时固定向标准输出打印 `{}`，保证在非 Kumu 环境中也是合法的空响应。Antigravity 的 `PreToolUse` 是要求返回 `decision` 的权限闸门，因此不用于通知；Antigravity 没有提交提示词事件，用 `PreInvocation` 标记运行中并以 `lastUserInput` 作为首个提示词标题。已启用的 Antigravity hooks 与 Claude 一样在 Kumu 启动时自动升级。

Pi 与 Oh My Pi 共用 `~/.kumu/agent-integration/pi/` 下的一份扩展；安装时只创建各自的自动发现链接。扩展通过 OMP 独有的 `ctx.agent` 区分宿主（Pi 没有这个字段），因此两者即使通过 `PI_CODING_AGENT_DIR` 指向同一个 agent 目录，也不需要重复安装，更不会互相占用链接。共用同一目录时，移除其中一个链接也会让另一个失效，重新为任一 Agent 接入即可恢复。

OpenCode 的插件对象兼容接口要求 V1 1.18.29 及以上；V2 使用同一包的 TUI 部分。已运行的 OpenCode 后台服务如尚未发现插件，需要在合适的时候重新加载；Kumu 不自动重启用户的服务。

默认位置会根据 Kumu 启动环境中的 `CODEX_HOME`、`CLAUDE_CONFIG_DIR`、`PI_CODING_AGENT_DIR`、`PI_CONFIG_DIR`、`CURSOR_CONFIG_DIR`、`XDG_CONFIG_HOME` 解析（Grok 固定读 `~/.grok`）。若用户在启动脚本内切换配置目录、使用独立的 OMP profile、关闭自动发现，或主动禁用 hooks／扩展，这套注册可能不被加载，需要在对应配置中另行接入。Kumu 不会绕过这些选择。

## Codex 的信任和事件覆盖

Codex 要求用户在 `/hooks` 中审查并信任新增或修改的 hooks。设置中的“已开启”只表示配置存在，不能代替 Codex 的运行时信任状态。Kumu 不自动写入信任记录或绕过信任检查。

Kumu 只写 `~/.codex/hooks.json`。如果用户同时在 `~/.codex/config.toml` 里写了内联 `[hooks]`，Codex 会合并两处配置并在启动时提示；用户仍可在 `/hooks` 中查看合并结果。

Codex 提问提醒依赖其版本对 `request_user_input` 工具触发 `PreToolUse`。官方文档说明有些特殊工具路径可以跳过工具 hooks，因此该项仍需按实际 Codex 版本验证；不能仅凭注册成功声称所有提问都已覆盖。原生 OSC 9／777 通知仍可作为补充接收。

## 通知如何回到终端

Claude Code 和 Codex 都会把 command hook 放到没有控制终端的新 session 中执行，`/dev/tty` 在 hook 里不可用。Kumu 给每个 PTY 子进程注入：

- `KUMU_TERMINAL=1`：只有 Kumu 内的进程才发送通知；
- `KUMU_APP_BINARY`：当前 Kumu 可执行文件，用于 hook 回调；
- `KUMU_TTY`：该 PTY 的 slave 设备路径。

传输方式按 Agent 的官方能力区分：

- **Claude Code**：命令向标准输出返回 `{"terminalSequence":"..."}`，由 Claude 自己发到终端（OSC 777）。这是 Claude 文档推荐的无控制终端方案，也适用于 tmux/screen；不支持该字段的旧版本会静默忽略，请更新 Claude Code。
- **Codex／Grok Build／Cursor／Antigravity CLI**：hook 打开 `KUMU_TTY` 写入 OSC 777；不向标准输出写通知内容，避免被各自的 hook 输出协议误读（Antigravity 的 shell 包装命令末尾单独打印 `{}` 满足其 JSON 响应约定）。Cursor 的 `hooks.json` 与 Antigravity 的 `~/.gemini/config/hooks.json` 均与 IDE 共用，`KUMU_TERMINAL` 守卫让它们在 IDE 中保持静默。
- **Pi／Oh My Pi／OpenCode**：扩展通过同一 `KUMU_TTY` 写入；拿不到时回退 `/dev/tty`。

后代进程会继承这些环境变量。不向 hook 的标准输出写入协议内容，也不替用户回答、选择或批准操作。

### 会话 id 也走这条通道

Agent 窗格需要记住“这是哪个对话”，重启后才知道带哪条 `--resume`／`resume`／`--session` 参数。会话 id 因此复用同一个 OSC 通道：

- command hook 从 stdin 的事件 JSON 里读 `session_id`（Claude、Codex；Cursor／Grok 的 `conversation_id`／`thread_id` 也认），在正常通知之前先发一条 `OSC 777;notify;<agent>;kumu-session:<id>`；
- OpenCode 插件在根会话开始时发同样的序列；
- Kumu 看到 body 以 `kumu-session:` 开头就把它记到窗格上，**不弹提醒**。

Agent 不一定从「新建 Agent 窗格」启动：手动在普通 shell 窗格输入的支持恢复的 Agent（OpenCode／Codex／Claude／Cursor／Antigravity CLI）只要发出会话更新，Kumu 就把这个窗格记为对应 Agent，下次启动直接带恢复参数启动（关闭“恢复 Agent 会话”时保持 shell 窗格不变）。会话 id 来自该窗格自己的 PTY，因此不同分栏里的 Agent 不会记错。没有恢复语法的 Agent（Pi、OMP、Grok）不会接管 shell 窗格，否则只会把用户的 shell 换成一个全新的 Agent。

### Agent 状态与标题也走这条通道

侧边栏全局的「Agent 状态」、窗格标题栏的状态胶囊和页签的注意提示使用同一条 OSC 通道上的静默消息：

- 状态：`OSC 777;notify;<agent>;kumu-status:<running|waiting|completed|failed|resume|idle>`；
- 标题：`OSC 777;notify;<agent>;kumu-title:<session-title>`（权威会话标题）或 `kumu-title:prompt:<first-prompt>`（首轮提示词）。

它们都不弹提醒，只更新该 PTY 所属窗格的状态与标题：

- 存活即入列：只要 Agent 窗格存在或普通 shell 中启动的 Agent 上报了会话／状态，就会持续留在侧边栏全局列表中，点击只跨项目／跨页签跳转聚焦，**不因查看清除**；只有进程退出回 shell 或关闭窗格时移出。
- 标题优先级：Agent 协议上报（`kumu-title:`） > 终端 OSC 0/2 标题（清洗前缀符号并过滤通用程序名／路径） > 回退到 Agent 名称；不再通过拦截终端按键猜测输入标题。
- 状态流转：新建或重启后为空闲（窗格标题栏不显示胶囊）；提交任务（`UserPromptSubmit`／`agent_start`／`session.execution.started`／Antigravity `PreInvocation`）进入 `running`；权限或提问进入 `waiting`，确认后继续执行（`PostToolUse`／`replied`／对话框结束）发送 `resume` 从 `waiting` 回到 `running`；本轮结束为 `completed`，失败为 `failed`；用户取消中断（OpenCode `interrupted`、Pi `aborted`、OMP `ompInterrupted`、Antigravity `USER_CANCELED`）发送 `idle` 回到空闲。

状态只用于界面，不替代任务结果判断；`Stop` 仍表示本轮结束，不表示改动已验证。

这个接管有一个已知边界：Kumu 收不到“Agent 主动退出、shell 还在”的事件，只在退出 App 或关闭窗口时用进程树判断被接管的 Agent 是否仍在运行。已经退出的 Agent 会被释放，窗格恢复为 shell；仍在运行的会话会被恢复。但 App 运行期间窗格会一直带着这个 launch：如果随后在同一个窗格里启动另一个可恢复的 Agent，会替换掉原来的记录；如果启动的是 Pi／OMP／Grok，原记录会保留。需要撤销时，关闭该窗格再新开，或重启后退出被恢复的 Agent。

原生扩展（OpenCode、Pi、OMP）是复制到 `~/.kumu/agent-integration/` 的文件。对已经注册到 Kumu 的扩展，Kumu 启动时会把这批文件刷新到当前版本并修正旧版本留下的链接；未注册的路径和属于其他工具的条目不会被触碰。这样可以避免升级 App 后已注册的扩展仍停留在旧插件——旧版 OpenCode 插件不会上报会话 id，会让自动恢复静默失效。hook 类 Agent 的注册命令每次都调用当前 Kumu 可执行文件，不需要刷新。

id 只允许字母、数字和 `-_.`，超长或含其他字符时整条丢弃，避免终止或拆分转义序列。读取 stdin 有 300ms 上限，手动运行同一个二进制时不会挂住 hook 的超时。

全局扩展在其他终端中不注册事件处理器；全局 hooks 的命令在非 Kumu 环境中立即返回。脱离原终端、清空环境或把 OpenCode 服务放到另一台机器时不能保证送达。

- 正在查看的终端不弹提醒；其他终端在前台显示应用内提示和侧栏未读标记。
- Kumu 在后台时只发送 macOS 系统通知，不再重复弹应用内提示，需从 `.app` 启动并允许通知。通知被拒或使用 `cargo run` 时，后台提醒只保留侧栏未读标记。
- 点击提醒回到发出事件的终端，包括不同分栏和隐藏的进程；已退出的终端保留最后输出。
- 内容相同的短时间重复提醒会合并；输入请求与任务结束提醒互不抑制。
- **设置 → 通用 → Agent 通知** 可关闭显示或提示音；持久注册可在编码 Agent 列表中用 **关闭通知** 单独撤销。

`Stop` 表示本轮 Agent 工作已结束，不表示 Kumu 验证了任务成果。Pi 使用 `agent_settled`，会跳过取消、自动重试／压缩、排队中的下一条输入、无交互界面的运行及子 Agent 完成事件；OMP 通过 `agent_end` 的 `isTerminal`／`yielded` 过滤中间态。Claude 的 `PermissionRequest` 负责权限提醒，`Notification` 只保留 MCP elicitation，避免与“任务结束”“空闲等待”重复。OpenCode 子会话不单独提醒，失败使用失败提示。

## 验证

```sh
cargo test
cargo build
python3 scripts/test-agent-integration.py
node --test src/agent/integration/adapters.test.mjs
cargo clippy --all-targets -- -D warnings
```

Rust 检查配置合并、注释与用户 hooks 保留、重复安装、移除（包括旧版本事件）、文件权限与符号链接保护，以及自定义启动命令的参数和终端身份。Python 在没有控制终端的 session 中检查 Claude 的 `terminalSequence`、Codex／Grok／Cursor／Antigravity CLI 的 `KUMU_TTY` 传输和标准输出隔离。Node 检查 Pi／OMP 的完成边界、同一扩展的宿主识别、OpenCode 事件适配与非 Kumu 环境中的停用。这些测试不等同于八种 Agent 的真实模型任务和 macOS 横幅验收。

接口依据：[OpenAI hooks](https://learn.chatgpt.com/docs/hooks)、[Claude Code hooks](https://code.claude.com/docs/en/hooks)、[OpenCode 插件自动发现](https://opencode.ai/docs/plugins/)、[OpenCode V1 插件兼容接口](https://opencode.ai/docs/build/plugins/server/#support-v1)、[Pi 发布包](https://www.npmjs.com/package/@mariozechner/pi-coding-agent)、[OMP 扩展加载](https://github.com/can1357/oh-my-pi/blob/main/docs/extension-loading.md)、[Cursor hooks](https://cursor.com/docs/agent/hooks)、[Grok Build hooks](https://docs.x.ai/build/features/hooks)、[Antigravity hooks](https://antigravity.google/docs/hooks)。
