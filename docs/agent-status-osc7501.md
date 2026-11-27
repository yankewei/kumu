# Agent 状态检测重构方案（Issue #51）

关联 Issue：[yankewei/kumu#51](https://github.com/yankewei/kumu/issues/51)

## 1. 背景与现状问题

Kumu 当前的 Agent 运行状态（`running` / `waiting` / `completed` / `failed` / `idle`）完全依赖各 Agent 的外部命令行 Hooks（Claude Code、Codex、Cursor、Grok、Antigravity）或进程内扩展（OpenCode、Pi、Oh My Pi），通过 `OSC 777;notify;<agent>;kumu-status:<state>` 上报。

对于进程内扩展（OpenCode / Pi / OMP），事件流完整、状态准确；但对于外部命令行 Hooks 类 CLI，纯 Hook 方案存在结构性缺陷：

1. **边沿触发容易卡死**：用户按 `Esc` 中断、拒绝权限或取消提问时，多个 CLI 不会触发 `Stop` 或 `PostToolUse`，导致侧边栏永久卡在 `running` 或 `waiting`。
2. **事件能力高度不对称**：
   - Cursor 仅注册 `stop`，缺少 `running` 与 `waiting`；
   - Grok 缺少 `running` 与 `resume`；
   - Antigravity 的 `PreToolUse` 是权限闸门不能用来发通知，缺少等待权限事件；
   - Codex 的信任目录 / 更新提示等启动弹窗不经过工具链，且每次修改 `hooks.json` 需用户在 `/hooks` 中重新信任。
3. **配置门槛高**：未在设置中手动“开启通知”的窗格没有任何实时状态。

## 2. 生态调研结论

### 2.1 竞品路线对比

- **Herdr**：放弃用命令行 Hooks 追踪 Claude / Codex / Antigravity / Cursor / Grok 的状态，Hooks 收缩为 `session-only`（仅在会话启动时上报 `session_id` 供重启恢复），运行状态完全交给“终端屏幕规则清单 + OSC 0/2/9;4 检测”；仅对 OpenCode / Pi / OMP 等完整生命周期插件保留状态权威。核心原则是**单一状态权威（Single Source of Truth）**，避免双源打架。
- **Orca (`stablyai/orca`)**：采用“Hook 优先 + 终端标题兜底”的混合路线。由于 Hook 优先级高于终端实时状态，当 Hook 漏掉权限清除事件时，即使终端侧已恢复 `working`，侧边栏仍会死锁在 `waiting`。

### 2.2 Kumu 支持的 8 个 Agent 当前主推方案（2026-11 调研与实测）

2026 年 10 月 Ghostty 作者 Mitchell Hashimoto 发布 **OSC 7501（Program Status Protocol，rev 0.4）**，允许 CLI 程序直接通过终端转义序列上报精确的运行态，迅速成为终端 Coding Agent 的新一代标准。

| Agent | 本机实测版本 | OSC 7501 支持度 | 官方当前主推方案 |
| --- | --- | --- | --- |
| **Codex** | `0.169.0` | **已原生支持**（二进制实测含 `\x1b]7501;state=`、`:app=codex`、`state=clear`；不发 `?` 探测） | 终端状态走 **OSC 7501**；Hooks 做 `session_id`/拦截；GUI 集成推 `app-server` |
| **Antigravity CLI (`agy`)** | `1.5.18` | **已原生支持**（PTY 实测发 `\x1b]7501;?\x07` 探测，并上报 `app=antigravity` 的 `blocked` / `auth` / `permission` 等状态） | 终端状态走 **OSC 7501**；Hooks 做 `session_id`/护栏 |
| **Claude Code** | 未安装 | **已原生支持**（v2.1.295+，`app=claude-code`，启动发 `OSC 7501;?` 探测） | 终端状态走 **OSC 7501**；Hooks (`terminalSequence`) 做 `session_id`/拦截 |
| **OpenCode** | `v2.4.2` | **已原生支持**（OpenTUI 含 7501 探测，且 JS bundle 通过 `setProgramStatus` 按 session id 上报 7501） | 主推 **V1/V2 原生插件 API**（Kumu 已有）；未安装插件时可用 7501 |
| **Pi** | `1.6.0` | **已原生支持**（实际包内含 `program-status-reporter.js` 与 `PI_PROGRAM_STATUS`） | 主推 **原生扩展 API**（Kumu 已有完整扩展）；未安装扩展时可用 7501 |
| **Oh My Pi (`omp`)** | 未安装 | **已原生支持** | **原生扩展 API** + **OSC 7501**（Kumu 已有扩展） |
| **Grok Build** | 未安装 | **已原生支持**（v1.0.52+） | **OSC 7501** + 基础 Hooks |
| **Cursor CLI** | `2026.11.22` | **暂未支持**（实际安装目录未检出 `]7501;`） | 主推 **Hooks (`hooks.json`)**（偏安全护栏，缺运行中/等待输入事件） |

**结论**：8 个 Agent 中已有 **7 个**原生支持 OSC 7501，仅 Cursor CLI（以及旧版未升级的 CLI）暂缺原生状态通道。对于拥有完整进程内生命周期扩展的 OpenCode / Pi / Oh My Pi，已安装扩展时保持扩展优先，未安装扩展时自动由 OSC 7501 提供开箱即用的实时状态。

## 3. 总体演进路线（分三阶段）

1. **阶段 1（本次优先落地）：终端原生支持 OSC 7501**
   - 直接复用锁定版本 `libghostty-vt`（commit `108cacb`）已内置的 `GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS` 回调，自动处理探测回复与协议解析；
   - 在 Rust 侧实现符合规范 rev 0.4 的有界记录表（`clear` 子树清除、`app` 祖先继承、优先级聚合、Unicode 不可见字符清洗）；
   - 接入 `Term` → `WorkspaceCore` → `KumuApp` 状态机，按 Agent 能力划分单一状态权威，并自动联动桌面/应用内提醒与 TODO 待审阅。
2. **阶段 2：收敛 Hooks 与原生扩展的权威边界**
   - 确认 OSC 7501 在各 Agent 上稳定运行后，评估将 Claude / Codex / Antigravity / Grok 的 Hooks 收缩为 `session_id` + 首轮 `prompt` 标题上报，减少对用户配置的侵入。
3. **阶段 3：轻量屏幕 + OSC 0/2 标题检测兜底**
   - 仅在既无 OSC 7501 也无原生扩展上报的 Agent 窗格上运行，覆盖 Cursor CLI 及未升级到 7501 的旧版 CLI。

---

## 4. 阶段 1 详细设计：Kumu 原生支持 OSC 7501

### 4.1 协议规范摘要（rev 0.4）

- **能力探测（Program → Terminal）**：
  ```text
  ESC ] 7501 ; ? ST
  ```
- **能力响应（Terminal → Program）**：
  ```text
  ESC ] 7501 ; ?:states=idle,working,done,blocked,error:kinds=permission,question,auth ST
  ```
  其中 `ST` 接受 `BEL` (`\x07`) 或 `ESC \` (`\x1b\\`)；响应**沿用请求的终止符**（由 `libghostty-vt` 原生保证，例如 `agy 1.5.18` 用 `BEL` 探测则回 `BEL`，OpenTUI 用 `ESC \` 则回 `ESC \`），并在流式解析中天然排在紧随其后的 `CSI c`（DA1）响应之前。
- **状态上报（Program → Terminal）**：
  ```text
  ESC ] 7501 ; state=<state>[:kind=<kind>][:app=<app>][:id=<path>][:title=<base64>][:progress=<0-100>][:msg=<base64>] ST
  ```
  - `state`（必填）：`idle` | `working` | `done` | `blocked` | `error` | `clear`
  - `kind`（可选，仅 `blocked` 有效）：`permission` | `question` | `auth`
  - `app`（可选）：稳定程序标识（如 `claude-code`、`codex`、`antigravity`、`opencode`、`grok`、`omp`）
  - `id`（可选）：分层任务路径（如 `agent/sub1`，空串表示根记录），用于并发子任务状态跟踪
  - `title`（可选）：UTF-8 Base64 编码的记录短标签
  - `progress`（可选，仅 `working` / `blocked` 有效）：`0..=100`（`libghostty-vt` C API 以 `-1` 表示无进度，Rust 侧映射为 `None`）
  - `msg`（可选）：UTF-8 Base64 编码的一行说明文本

### 4.2 `libghostty-vt` 原生回调与记录表（`src/term/vt/program_status.rs`）

仓库锁定的 `vendor/ghostty`（commit `108cacb`）已在 `include/ghostty/vt/terminal.h` 提供完整 C API，且 `crates/ghostty-sys/build.rs` 的 bindgen allowlist 已自动生成绑定：

- `GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS = 47`
- `GhosttyTerminalProgramStatus`（`state`、`kind`、`progress`、`id`、`app`、`title`、`message`）
- `GhosttyTerminalProgramStatusFn`

注册 `on_program_status` 回调后，`libghostty-vt` 自动完成能力探测应答（同终止符、严格顺序）、4KB 负载限制、Base64 校验解码、C0 控制字符过滤、`id` 语法校验以及全重置（RIS `ESC c`）时触发空 `id` 的 `CLEAR` 回调。

Rust 侧仅需在 `src/term/vt/program_status.rs` 维护轻量状态表 `ProgramStatusTable`：

1. **有界记录存储**：按 `id` 存储记录（空串为根记录），上限 128 项（规范要求 64..=256）；新增 `id` 超限时淘汰最久未更新的条目。每次上报完整替换该 `id` 对应记录（缺省字段不继承旧值）。
2. **`clear` 子树清除、Prompt Start 与进程退出重置**：
   - `state=clear` 时若 `id` 为空则清空整张表；否则删除该 `id` 及其所有子记录（即 `key == id` 或 `key` 以 `"{id}/"` 开头）；
   - 同时注册 `GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT`，当主 shell prompt 开始（`OSC 133;A` / `GHOSTTY_SEMANTIC_PROMPT_PROMPT_START`）时按规范移除 `working` / `blocked` / `idle` 记录（保留 `done` / `error`）；
   - 当根记录（`id=""`）转为 `idle` / `working` 时，自动清理同 `app` 组下陈旧的 `working` / `blocked` 子记录，避免用户按 `Esc` 中断而 CLI 漏发子记录 `clear` 时侧边栏永久卡死；
   - 提供 `Vt::clear_program_status()` / `Term::reset_program_status()`，在 shell 窗格 Agent 退出（含 `SIGKILL` 未发 `clear`）、切换 Agent，以及未被本地进程 watcher 跟踪的 shell Agent 收到 `clear` 移除条目时主动清空残留记录表。
3. **`app` 祖先继承与按 `app` 隔离聚合**：若某记录的 `app` 为空，沿 `/` 分隔的父路径逐级向上查找最近非空 `app`（直至根记录 `""`）。聚合按**解析后的 `app` 分组**执行（组内按 `blocked` > `error` > `working` > `done` > `idle` 选出最高优先级，同优先级取最近更新者），并在记录更换 `app` 或被 LRU 淘汰时同步刷新受影响的旧 `app` 组。注：嵌套子工具（如 `cargo` / `terraform`）不遮蔽主 Agent 的前提是主 Agent 自身上报了 `app`；完全省略 `app` 的记录会归入公共 `""` 组。
4. **外部文本清洗**：Ghostty 仅保证去除 C0 控制字符；在离开终端前，对 `app` / `title` / `message` 额外清洗 Unicode 双向覆盖/隔离符（bidi override/isolate）、零宽空白/非连接符（保留 `U+200D` 以维持复合 emoji）、软连字符与 Unicode Tags，避免通知或界面被伪造排版。

### 4.3 事件管道：`Term` → `WorkspaceCore` → `KumuApp`

1. **`src/term/vt/types.rs` & `src/term/vt/mod.rs`**：
   - 定义 `ProgramState`、`ProgramBlockKind` 与 `TermProgramStatus`（含 `title`）。
   - 在 `Vt::with_colors` 中注册 `GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS`，暴露 `Vt::take_program_status`。
2. **`src/term/session.rs`**：
   - `Live` 新增 `program_statuses: Mutex<VecDeque<TermProgramStatus>>`。
   - `Running::publish` 与同步输出 hold 路径中同步 drain `program_statuses` 并触发 `force_wake`，确保隐藏页签与 parked 窗格实时更新。
   - `Term` 暴露 `take_program_status(&self) -> Option<TermProgramStatus>`。
3. **`src/agent/registry.rs` & `src/agent/status.rs`**：
   - 扩展 `id_for` 别名识别：`claude-code` → `claude`、`antigravity` / `antigravity-cli` → `agy`、`cursor-agent` / `cursor-cli` → `cursor`、`oh-my-pi` → `omp`、`grok-build` → `grok`、`pi-coding-agent` → `pi`。
   - 新增 `prefers_integration_status(id)`（`opencode` / `pi` / `omp` 返回 `true`）与 `StatusSource`（`Osc7501` / `Integration`）。
4. **`src/app/core.rs`**：
   - 新增 `WorkspaceEvent::ProgramStatus { terminal, pane, project, status }` 与 `terminal_pane(terminal)` 查询方法，保持 `WorkspaceCore` 不依赖 UI 侧进程树状态。
   - `drain_terminal` 消费 `term.take_program_status()` 并原样发出 `WorkspaceEvent::ProgramStatus`。
5. **`src/ui/home/agents.rs` & `src/ui/home/mod.rs`**（原 `src/plugins/todo/mod.rs` 已退役并迁移至 `src/ui/home/todo_board/` 与 `todo_agent_links`）：
   - **非 Agent 普通命令过滤与防劫持（在 UI 层统一判定）**：
     - 若 `status.app` 非空：必须经 `id_for(&status.app)` 命中已知 Agent，否则直接忽略（防止 `cargo`/`terraform`/`brew` 等子命令误写 Agent 状态；如用户使用自定义 `app` 名称例如 Pi `piConfigName`，需在 `src/agent/registry.rs` 登记别名或继续使用 Kumu 原生 Pi 扩展）；且已由 `launches` 启动为指定 Agent 的窗格（或 shell 窗格中处于活跃态的 Agent）不允许被嵌套子程序的其他 `app` 切换 Agent。
     - 若 `status.app` 为空：仅当窗格以 Agent 启动（`launches` 为 `PaneLaunch::Agent`）或已在 `agent_status` 中（由进程树 watcher 或 `Session` 识别）时接收。
   - **按 Agent 划分单一状态权威**：
     - **原生扩展优先组（OpenCode / Pi / Oh My Pi）**：`Integration > OSC 7501`。一旦该窗格收到原生扩展状态（`integration_active = true`），忽略同窗格后续 OSC 7501 状态与提醒；若用户未安装扩展（`integration_active = false`），则由 OSC 7501 直接驱动状态与提醒。
     - **OSC 7501 优先组（Codex / Claude Code / Antigravity CLI / Grok 等）**：`OSC 7501 > Hooks`。仅当收到**非 `Clear` 且带明确 `app`** 的 OSC 7501 状态时置位 `osc7501_active = true`（`Clear` 或匿名空 `app` 上报不建立权威，防止误伤 Cursor CLI 等纯 Hook Agent）；权威生效后忽略同窗格后续来自 `OSC 777` 的 `kumu-status:` 以及匹配 Kumu 标准 Hook 文案的重复文本通知（保留 `kumu-session:`、`kumu-title:` 及非 Hook 自定义通知）。
     - 当 Agent 退出回 shell（移除 `AgentEntry`）或同一 shell 窗格切换为其他 Agent 时，权威标记与终端内 `ProgramStatusTable` 同步重置。
   - **状态映射与提醒联动**：
     - `Working` → `Running`、`Blocked` → `Waiting`、`Done` → `Completed`、`Error` → `Failed`、`Idle | Clear` → `Idle`；复用现有 `unseen` 与 TODO `awaiting_review` 联动（通过 `todo_agent_links` 按窗格更新）。
     - 使用 `(state, kind, blocked prompt)` 跃迁键判断是否触发提醒（`Blocked` 的 `kind` 或提示内容变化会触发新提醒，同一提示的进度跳动不刷屏）；复用 `KumuApp::raise_attention` 发送提醒（通知标题为 `项目 · 终端标签` 以防跨窗格伪装，正文前缀为 Agent 显示名与可选记录 `title`，正文优先取清洗后的 `msg`，为空时回退中文默认文案；复用 2 秒去重）。

### 4.4 涉及文件清单

- 新增：`src/term/vt/program_status.rs`
- 修改：
  - `src/term/vt/types.rs`
  - `src/term/vt/mod.rs`
  - `src/term/mod.rs`
  - `src/term/session.rs`
  - `src/agent/registry.rs`
  - `src/agent/status.rs`
  - `src/agent/mod.rs`
  - `src/app/core.rs`
  - `src/app/mod.rs`
  - `src/ui/home/mod.rs`
  - `src/ui/home/agents.rs`
  - `src/plugins/todo/mod.rs`（历史记录，已在 PR #53 迁移至 `src/ui/home/todo_board/`）

## 5. 验证计划

1. **单元测试**：
   - `vt` & `program_status`：`OSC 7501;?` 原生探测回复（回显 `BEL` / `ST` 终止符、与 `CSI c` 同包时严格排在 DA1 之前）、RIS (`ESC c`) 清除、记录表 `clear` 子树、`app` 祖先继承、多 `id` 优先级聚合、容量 LRU 淘汰、Unicode 不可见/bidi 字符清洗。
   - `registry` & `agents`：OSC 7501 `app` 别名映射、非 Agent 命令（如 `cargo`/`terraform`）过滤、按 Agent 能力的权威裁决（OpenCode/Pi 扩展优先 vs Codex/Claude/agy 7501 优先）及重复 Hook 通知抑制。
2. **静态检查与回归**：
   - `cargo test`
   - `cargo clippy --all-targets -- -D warnings`
3. **真机验收**：
   - 在 Kumu 中使用本机 `agy 1.5.18`（含认证/信任目录 `blocked` 弹窗）和 `codex 0.169.0`，验证不依赖 Hook 时的 `running` / `waiting` / `completed` / `idle` 实时切换、按 `Esc` 中断恢复、后台窗格未读标记与系统通知。
