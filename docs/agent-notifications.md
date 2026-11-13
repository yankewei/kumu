# Agent 通知

Kumu 不改写用户的 Agent 启动命令，不添加 `-c`、`--settings` 或 `--extension`，不替换 PATH、ZDOTDIR 或 shell 启动文件。

在 **设置 → 服务商 → 编码 Agent** 对已安装的 Agent 点击 **开启通知**，把通知处理器注册到 Agent 自己的持久配置或自动发现目录，然后重新启动 Agent。之后使用普通命令、绝对路径、别名或自定义脚本启动，均不要求额外的通知参数。已开启时同一位置显示 **关闭通知**。

未安装的 Agent 只显示“未安装”，没有任何通知操作：注册文件只有对应 Agent 的进程才会加载，为一个不存在的 CLI 预写配置既不会生效，也会让状态变成没有覆盖的声明。安装 CLI 后，这一行才会出现 **开启通知**。

开启和重新开启保留用户已有 hooks、注释与配置文件链接；关闭仅删除带 Kumu 标记的处理器或指向 Kumu 管理目录的链接。配置格式错误或接入路径已有其他文件时会报错，不覆盖原配置。此工作区的开发与测试没有替用户安装到真实的 Agent 配置，需从设置中执行开启。

| Agent | 默认注册位置 | 事件来源 |
| --- | --- | --- |
| Codex | `~/.codex/hooks.json` | `Stop`、`PermissionRequest`、匹配 `request_user_input` 的 `PreToolUse` |
| Claude Code | `~/.claude/settings.json` 的 `hooks` | `Stop`、`PermissionRequest`、提问／计划确认的 `PreToolUse`、MCP elicitation 的 `Notification` |
| OpenCode | `~/.config/opencode/plugins/kumu-notifications/` | 自动发现的插件，支持 V2 TUI 事件和 V1 `server()` hooks |
| Pi | `~/.pi/agent/extensions/kumu-notifications/` | `agent_settled`、`ui_prompt_start`、提问工具事件 |
| Oh My Pi | `~/.omp/agent/extensions/kumu-notifications/` | `agent_end`、`tool_approval_requested`、提问工具事件、输入／确认／选择对话框 |
| Cursor CLI | `~/.cursor/hooks.json` | `stop`（只覆盖任务结束） |
| Grok Build | `~/.grok/hooks/kumu-notifications.json` | `Stop`、`Notification` |

Cursor 的钩子文件是扁平结构：事件直接对应处理器数组，处理器自身就是命令（不像 Claude／Codex／Grok 那样再嵌一层 `hooks`），因此 Kumu 用单独的读改写逻辑处理它，并保留 `version` 字段与其他事件。Cursor 没有“等待权限”事件：`beforeShellExecution` 是权限闸门，返回格式错误会阻断命令，因此不用它发送通知，Cursor 只提供任务结束提醒。

Grok 的钩子格式与 Claude 相同，可以复用同一套 CST 逻辑；Kumu 写自己的 `~/.grok/hooks/kumu-notifications.json`，不修改用户已有的 hook 文件。

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
- **Codex／Grok Build／Cursor**：hook 打开 `KUMU_TTY` 写入 OSC 777；不向标准输出写内容，避免被各自的 hook 输出协议误读。Cursor 的 `hooks.json` 与 IDE 共用，`KUMU_TERMINAL` 守卫让它在 IDE 中立即返回。
- **Pi／Oh My Pi／OpenCode**：扩展通过同一 `KUMU_TTY` 写入；拿不到时回退 `/dev/tty`。

后代进程会继承这些环境变量。不向 hook 的标准输出写入协议内容，也不替用户回答、选择或批准操作。

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

Rust 检查配置合并、注释与用户 hooks 保留、重复安装、移除（包括旧版本事件）、文件权限与符号链接保护，以及自定义启动命令的参数和终端身份。Python 在没有控制终端的 session 中检查 Claude 的 `terminalSequence`、Codex 的 `KUMU_TTY` 传输和标准输出隔离。Node 检查 Pi／OMP 的完成边界、同一扩展的宿主识别、OpenCode 事件适配与非 Kumu 环境中的停用。这些测试不等同于七种 Agent 的真实模型任务和 macOS 横幅验收。

接口依据：[OpenAI hooks](https://learn.chatgpt.com/docs/hooks)、[Claude Code hooks](https://code.claude.com/docs/en/hooks)、[OpenCode 插件自动发现](https://opencode.ai/docs/plugins/)、[OpenCode V1 插件兼容接口](https://opencode.ai/docs/build/plugins/server/#support-v1)、[Pi 发布包](https://www.npmjs.com/package/@mariozechner/pi-coding-agent)、[OMP 扩展加载](https://github.com/can1357/oh-my-pi/blob/main/docs/extension-loading.md)、[Cursor hooks](https://cursor.com/docs/agent/hooks)、[Grok Build hooks](https://docs.x.ai/build/features/hooks)。
