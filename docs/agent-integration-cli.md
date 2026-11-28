# Agent Integration CLI：统一接入通知、MCP 与 Skills

> 状态：已实现

## 1. 问题与目标

当前 Kumu 给各 Coding Agent 注入能力分散在三个入口里：

1. **通知（Hooks / 原生扩展）**：仅在设置页 UI 里逐个开启/关闭；
2. **MCP**：随设置页通知开启时附带注册，或手动执行 `kumu mcp install [agent|all]`；且在非 Kumu 终端/IDE 中启动 Agent 时，Kumu MCP 仍会暴露全部 11 个工具，白白消耗上下文 token；
3. **Skills**：应用本体不管理，用户必须额外安装 Node 并手动执行 `npx skills add yankewei/kumu`，Kumu 升级后 Skill 容易与 CLI/MCP 版本漂移，且内容与 MCP 工具存在重复。

本方案的目标是把三项能力收口到 **一个极简 CLI 入口 + 一套共享 Rust 托管引擎**：

1. 提供 `kumu integration <install|uninstall|status>` 三个子命令，一条命令完成 **通知 + MCP + Skills** 的安装、卸载与诊断；
2. `kumu integration install` 不传参数时，**仅针对本机已检测到的 Agent** 安装，不在未安装的 Agent 目录下创建空配置；
3. 将 `skills/kumu-panes/SKILL.md` 与 `skills/kumu-artifacts/SKILL.md` 编译期内嵌进 Kumu 二进制，托管到 `~/.kumu/skills/` 并软链到各 Agent 的 skills 目录，启动时自动刷新已托管文件；
4. 让 MCP 在非 Kumu 环境中**彻底静默**（`tools/list` 返回空列表、`tools/call` 拒绝、`initialize.instructions` 省略），避免污染外部终端/IDE 上下文；
5. 精简 Skill 内容，明确 **MCP 优先、CLI 兜底** 以及 **非 Kumu 环境下禁止 `pane` 操作** 的守卫规则；
6. 设置页 UI 与 `kumu integration` CLI 共享同一套底层逻辑，行为完全一致。

非目标（本阶段不做）：

- 按会话启动参数动态注入（如 Claude `--plugin-dir`、Codex `-c` 等作为后续增量优化，本阶段先把统一的持久化接入 + 非 Kumu 彻底静默做扎实）；
- 为 `kumu integration` 引入大量细粒度 flag（保持 CLI 极致简单，保留现有 `kumu mcp` 子命令向后兼容）。

---

## 2. 总体架构

```
                  ┌──────────────────────────────────────────┐
                  │                Kumu Binary               │
                  │  ┌────────────────┐  ┌────────────────┐  │
                  │  │ Settings UI    │  │ CLI            │  │
                  │  │ (nsh_ui)       │  │ kumu           │  │
                  │  └───────┬────────┘  │  integration   │  │
                  │          │           └───────┬────────┘  │
                  │          └─────────┬─────────┘           │
                  │                    ▼                     │
                  │       src/agent/integration/             │
                  │   ├─ hooks.rs  (Hooks / 原生扩展软链)      │
                  │   ├─ mcp.rs    (~/.kumu/bin/kumu-mcp)    │
                  │   └─ skills.rs (内嵌 SKILL.md -> 软链)    │
                  └────────────────────┬─────────────────────┘
                                       │
           ┌───────────────────────────┼───────────────────────────┐
           ▼                           ▼                           ▼
   ~/.kumu/agent-integration/   ~/.kumu/bin/kumu-mcp       ~/.kumu/skills/
   (opencode / pi / omp 扩展)    (stdio MCP wrapper)        ├─ kumu-panes/SKILL.md
           │                           │                    └─ kumu-artifacts/SKILL.md
           ▼                           ▼                           │
     各 Agent Hooks / 扩展        各 Agent MCP 配置                  ▼
     (KUMU_TERMINAL=1 守卫)      (非 Kumu 环境彻底静默)        各 Agent skills/ 软链
```

---

## 3. CLI 规范

在 [src/control/cli.rs](file:///Users/yankewei/Documents/github/kumu-one/kumu-feat-artifact-rendering/src/control/cli.rs) 注册顶级子命令 `integration`，实现放在 `src/control/cli_integration.rs`。

### 3.1 命令列表

```sh
kumu integration install [AGENT]
kumu integration uninstall [AGENT]
kumu integration status [--json]
```

### 3.2 行为、输出与退出码

| 命令 | 不传 `AGENT` | 传 `AGENT` |
| --- | --- | --- |
| `kumu integration install` | 遍历 `AGENTS`，仅对 `crate::agent::installed(id) == true` 的 Agent 安装（Notify + MCP + Skills）。若本机未检测到任何支持的 Agent，输出提示并返回 `0`。 | 用 `agent::id_for(arg)` 解析（支持 `claude`、`claude-code`、`agy`、`antigravity` 等别名），显式为该 Agent 安装；若本机未检测到该可执行文件，仍写入配置并附带一行提示。 |
| `kumu integration uninstall` | 遍历 `AGENTS`，仅对当前存在任一项已接入或错误状态的 Agent 执行卸载（遇到损坏配置打印错误但不中断其余 Agent）。 | 解析 `AGENT` 并卸载该 Agent 的三项接入。 |
| `kumu integration status` | 列出所有支持的 Agent 的检测状态与三项接入明细；支持 `--json`。 | — |

- **逐项执行与错误聚合**：单个 Agent 的 `notify` / `mcp` / `skills` 三项**独立执行**，某一项失败不中止其余两项，汇总输出每一项结果：
  ```text
  claude: notify ok, mcp ok, skills ok
  codex: notify ok, mcp ok, skills ok (重新启动 Codex 后请在 /hooks 中审查并信任)
  cursor: notify ok, mcp ok, skills failed: 接入路径已有非 Kumu 文件，未覆盖：...
  ```
- **退出码**（与现有 CLI 一致）：
  - `0`：全部成功（或无参 `install`/`uninstall` 无目标）；
  - `1`：至少有一个 Agent 的某一项失败；
  - `2`：用法错误（未知子命令、未知选项、未知 Agent ID）。

### 3.3 `status` 输出格式

文本输出（`kumu integration status`）：

```text
AGENT      INSTALLED  NOTIFY      MCP           SKILLS
opencode   yes        registered  registered    registered
codex      yes        registered  registered    registered
claude     yes        registered  registered    registered
cursor     no         missing     missing       missing
pi         yes        registered  ext(partial)  registered
omp        no         missing     ext(partial)  missing
grok       no         missing     missing       missing
agy        yes        registered  registered    registered
```

JSON 输出（`kumu integration status --json`）：

```json
{
  "agents": [
    {
      "agent": "claude",
      "installed": true,
      "notify": "registered",
      "mcp": "registered",
      "skills": "registered"
    },
    {
      "agent": "pi",
      "installed": true,
      "notify": "registered",
      "mcp": "extension_partial",
      "skills": "registered"
    }
  ]
}
```

> 说明：
> - 列名使用 `NOTIFY`（而非 `HOOKS`），涵盖外部 Hooks（`claude`/`codex`/`cursor`/`grok`/`agy`）和原生扩展（`opencode`/`pi`/`omp`）；
> - `pi` / `omp` 目前仅在 `pi.js` 中通过 CLI 封装注册了 `kumu_artifact_publish` 和 `kumu_artifact_list`（无 `pane_*`，非标准 MCP），因此状态如实展示为 `ext(partial)` / `"extension_partial"`；
> - 某一项配置损坏时，该格输出 `error: <原因>`，**不拖垮整张 `status` 表**。

---

## 4. Skills 内嵌托管设计（`src/agent/integration/skills.rs`）

### 4.1 托管资产目录

编译期通过 `include_str!` 直接嵌入仓库中的 Skill 定义，保证单一事实来源：

```rust
const SKILLS: [(&str, &str); 2] = [
    ("kumu-panes", include_str!("../../../skills/kumu-panes/SKILL.md")),
    ("kumu-artifacts", include_str!("../../../skills/kumu-artifacts/SKILL.md")),
];
```

运行时原子写入（临时文件 + `sync_all` + `rename`，权限 `0o644`）到：

- `~/.kumu/skills/kumu-panes/SKILL.md`
- `~/.kumu/skills/kumu-artifacts/SKILL.md`

### 4.2 各 Agent 的软链路径与共享目录语义

所有路径解析抽成接受 `home: &Path` 的纯函数（便于并行单元测试，不改进程 `HOME` 环境变量）。

| Agent | 软链路径（指向 `~/.kumu/skills/<skill>`） | 说明 |
| --- | --- | --- |
| `codex` | `$HOME/.agents/skills/<skill>` | **共享发现目录**（Codex 官方用户级路径，不受 `CODEX_HOME` 影响；也会被 Cursor/OpenCode/Pi/Grok 扫描） |
| `agy` | `~/.gemini/config/skills/<skill>` | 与 `~/.gemini/config/hooks.json` 一致，App / IDE / CLI 三端共用 |
| `claude` | `${CLAUDE_CONFIG_DIR:-~/.claude}/skills/<skill>` | Claude Code 默认目录（也会被 Cursor/OpenCode/Grok 扫描） |
| `cursor` | `${CURSOR_CONFIG_DIR:-${XDG_CONFIG_HOME/cursor 或 ~/.cursor}}/skills/<skill>` | Cursor 独立目录 |
| `opencode` | `${XDG_CONFIG_HOME:-~/.config}/opencode/skills/<skill>` | OpenCode 独立目录 |
| `pi` | `${PI_CODING_AGENT_DIR:-~/.pi/agent}/skills/<skill>` | Pi 独立目录 |
| `omp` | `${PI_CODING_AGENT_DIR:-${PI_CONFIG_DIR:-~/.omp}/agent}/skills/<skill>` | Oh My Pi 目录 |
| `grok` | `~/.grok/skills/<skill>` | Grok Build 目录 |

**共享目录卸载策略**：
- `codex` 使用的 `$HOME/.agents/skills/<skill>` 与 `claude` 使用的 `~/.claude/skills/<skill>` 在发现层可能跨 Agent 可见（这是各 Agent 自身扫描约定所致，在文档中如实说明）；
- 当两个 Agent 映射到同一物理路径（例如 `pi` 和 `omp` 共用 `PI_CODING_AGENT_DIR`），或卸载 `codex` 时，仅当没有其他仍处于接入状态的 Agent 依赖同一路径时才移除该软链；执行无参 `kumu integration uninstall` 时全部清理。

### 4.3 `npx skills add` 旧副本自动迁移与安全守卫

复用 `is_under_managed(path, managed_skills_dir)` 保护用户文件，同时平滑迁移早期通过 `npx skills add yankewei/kumu` 安装的副本：

- **识别 Kumu 旧副本**：若目标路径已存在且不是 Kumu 托管软链（如真实目录或指向 `~/.agents/skills/` 之外的旧软链），检查 `<target>/SKILL.md`：
  - YAML frontmatter 中 `name:` 严格匹配当前 skill 名（`kumu-panes` 或 `kumu-artifacts`），且正文包含 Kumu 特征标记（`KUMU_APP_BINARY` 或 `kumu pane` / `kumu artifact`）；
  - 满足条件时判定为 Kumu 旧版副本，自动移除旧副本并替换为指向 `~/.kumu/skills/<skill>` 的托管软链；
  - **不满足条件**（例如用户自定义同名文件/目录）时，**拒绝覆盖并返回明确错误**。
- **卸载**：只删除指向 `~/.kumu/skills/` 下的软链，非 Kumu 文件原样保留。
- **状态判定（`skills_status(agent)`）**：
  - 所有内置 skill 软链均不存在 → `Status::Missing`；
  - 所有内置 skill 均已软链到正确目标，且 `~/.kumu/skills/<name>/SKILL.md` 内容与当前二进制一致 → `Status::Registered`；
  - 其余情况（部分缺失、内容过期、软链失效、存在待迁移旧副本）→ `Status::NeedsRepair`。
- **启动刷新（`refresh_skills()`）**：仅当 `~/.kumu/skills/` 已存在时，启动时静默将内嵌的最新 `SKILL.md` 原子刷新到托管目录，绝不主动给未安装 Skills 的用户创建软链。

---

## 5. MCP 非 Kumu 彻底静默 & Skill 内容对齐

### 5.1 MCP 非 Kumu 彻底静默（[src/control/mcp.rs](file:///Users/yankewei/Documents/github/kumu-one/kumu-feat-artifact-rendering/src/control/mcp.rs)）

判定函数（内部核心分发函数接收 `in_kumu: bool` 以便纯单元测试）：

```rust
fn in_kumu_terminal() -> bool {
    std::env::var("KUMU_TERMINAL").as_deref() == Ok("1")
        || std::env::var_os("KUMU_PANE").is_some_and(|v| !v.is_empty())
}
```

当 `in_kumu == false`（用户在系统终端、VS Code、Cursor IDE、Antigravity IDE 中启动 Agent）：
1. `initialize`：正常完成握手（避免客户端报错/重连），但**省略 `instructions` 字段**；
2. `tools/list`：返回 `{ "tools": [] }`，零工具暴露、零上下文 token 浪费；
3. `tools/call`：即便客户端缓存了旧 schema 强行调用，也直接返回 `isError: true` 与明确提示：`"Kumu MCP tools are only available inside a Kumu terminal pane."`，防止误操作 Kumu 窗口。

当 `in_kumu == true`：正常返回 `instructions` 和全部 11 个工具（4 个 `artifact_*` + 7 个 `pane_*`）。

### 5.2 Skill 内容收敛与非 Kumu 守卫

更新 [skills/kumu-panes/SKILL.md](file:///Users/yankewei/Documents/github/kumu-one/kumu-feat-artifact-rendering/skills/kumu-panes/SKILL.md) 与 [skills/kumu-artifacts/SKILL.md](file:///Users/yankewei/Documents/github/kumu-one/kumu-feat-artifact-rendering/skills/kumu-artifacts/SKILL.md)：

- **不展开 MCP 11 个工具的参数表**（schema 已自带描述），专注写触发条件、何时不该发 artifact、`replace` 迭代、焦点与隐私约定、多窗格协作套路与失败处理；
- **明确优先级**：优先调用 Kumu MCP 工具（`artifact_*` / `pane_*`）；Pi / Oh My Pi 单独一句说明原生工具名 `kumu_artifact_publish` / `kumu_artifact_list`；无 MCP 时再使用 `"$KUMU_APP_BINARY"` CLI 兜底（只保留入口确认和 3-4 条最常用命令，其余靠 `--help`）；
- **非 Kumu 硬性守卫**：`kumu-panes` 明确要求 **只有 `KUMU_TERMINAL=1` 或 `KUMU_PANE` 非空时才允许执行窗格操作**，否则提示用户需要在 Kumu 窗格内运行，避免在外部 IDE/终端误操作 `focused` 窗格。

---

## 6. 统一状态模型、启动刷新与设置页 UI 协同

### 6.1 `IntegrationDetail` 与 `status()` 重构（[src/agent/integration/mod.rs](file:///Users/yankewei/Documents/github/kumu-one/kumu-feat-artifact-rendering/src/agent/integration/mod.rs)）

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpState {
    Supported(Result<Status, String>),
    ExtensionPartial,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationDetail {
    pub notify: Result<Status, String>,
    pub mcp: McpState,
    pub skills: Result<Status, String>,
}
```

- **取消提前返回**：`status(agent)` 改为基于 `detailed_status(agent)` 汇总三项状态：
  - 若三项中任一项为 `Err`，返回该错误；
  - `notify`、`skills` 和（若 `Supported`）`mcp` 全部为 `Missing` → `Status::Missing`；
  - 全部为 `Registered`（`ExtensionPartial` 视为满足）→ `Status::Registered`；
  - 其余组合（例如老用户已装 Notify + MCP 但未装 Skills，或 Skills 存在但 Hooks 缺失）→ `Status::NeedsRepair`。

### 6.2 `refresh_installed()` 严格只修已存在组件

避免老用户升级 Kumu 后在启动时被静默加装 Skills：

- `mcp::refresh_mcp_wrapper()`：仅当 `~/.kumu/bin/kumu-mcp` 已存在时刷新；
- `skills::refresh_skills()`：仅当 `~/.kumu/skills` 已存在时刷新内嵌 `SKILL.md` 内容；
- `refresh_native()`：仅当 `opencode` / `pi` / `omp` 扩展软链已指向 Kumu 托管目录时刷新；
- `claude` / `agy` Hooks 自动升级：改用 `notify_status(agent)` 单独判定，仅当 `notify_status == Ok(Status::NeedsRepair)` 时才刷新 Hooks 文件，**不再调用全量 `install(agent)`**。

### 6.3 设置页 UI 协同（[src/nsh_ui/settings/mod.rs](file:///Users/yankewei/Documents/github/kumu-one/kumu-feat-artifact-rendering/src/nsh_ui/settings/mod.rs)）

- 继续调用 `integration::install(agent)` / `uninstall(agent)` / `status(agent)`；
- 将成功 toast 文案微调为准确反映全量接入（如“已开启接入（通知 / MCP / Skills）；请重新启动 Agent。” / “已关闭接入；请重新启动 Agent。”）。

---

## 7. 文件改动清单与落地顺序

| 顺序 | 文件 | 改动类型 | 说明 |
| --- | --- | --- | --- |
| 1 | `src/agent/integration/skills.rs` | 新增 | 路径参数化解析、内嵌 `SKILL.md` 写入、旧副本自动迁移、安全守卫、`refresh_skills` |
| 2 | `src/agent/integration/mod.rs` | 修改 | 整合 `skills` 模块；新增 `IntegrationDetail` / `detailed_status`；重构 `status()` 与 `refresh_installed()`；逐项执行的 `install_detailed` / `uninstall_detailed` |
| 3 | `src/control/mcp.rs` | 修改 | 非 Kumu 彻底静默（`initialize.instructions`、`tools/list`、`tools/call`）；`kumu mcp status` 附带 `kumu integration status` 提示 |
| 4 | `src/control/cli_integration.rs`、`src/control/cli.rs`、`src/control/mod.rs` | 新增 / 修改 | 注册并实现 `kumu integration <install\|uninstall\|status>` |
| 5 | `src/nsh_ui/settings/mod.rs` | 修改 | 同步开启/关闭接入 toast 文案 |
| 6 | `skills/kumu-panes/SKILL.md`、`skills/kumu-artifacts/SKILL.md` | 修改 | 精简策略型文案（MCP 优先、Pi/OMP 提示、CLI 兜底、非 Kumu 守卫） |
| 7 | `README.md`、`docs/agent-notifications.md` | 修改 | 统一安装指引为 `kumu integration install`，同步 Skills 目录表与设置语义 |

---

## 8. 边界与风险对策

| 风险 | 对策 |
| --- | --- |
| 老用户已用 `npx skills add yankewei/kumu` 装过同名目录 | 校验 `SKILL.md` frontmatter `name` 与 Kumu 特征后自动迁移为托管软链；非 Kumu 自有文件坚决拒绝覆盖 |
| 老用户升级后启动 Kumu 被静默安装 Skills | `refresh_installed()` 按组件判断，只刷新本来就已存在的组件，不静默新增 |
| `codex` 的 `~/.agents/skills` 被其他 Agent 共享读取 | 卸载时做引用检查，有其他接入 Agent 依赖时不误删；文档明确说明跨 Agent 可见 |
| 外部 IDE/终端启动 Agent 拉起 `kumu-mcp` | 非 Kumu 环境 `initialize.instructions` 省略、`tools/list` 返回 `[]`、`tools/call` 返回 `isError` |
| 某个 Agent 配置文件损坏 | `detailed_status` 每格独立 `Result`，`install`/`uninstall` 逐项执行并汇总，不拖垮其他 Agent 或其他组件 |
| 旧版 Kumu 启动覆盖新版 Skills 内容 | 已知可接受行为（多版本混用场景，新版再次启动会自动刷回） |

---

## 9. 验证计划

1. **单元测试**：
   - `skills.rs`：纯函数路径解析（`codex -> ~/.agents/skills`、`agy -> ~/.gemini/config/skills` 等）；安装/幂等/内容过期（`NeedsRepair`）/卸载；`npx skills` 旧副本自动迁移与用户自定义文件安全拒绝；
   - `integration/mod.rs`：聚合 `status()` 矩阵（三项各种组合，确保无提前返回 bug）；`refresh_installed()` 不静默安装缺失 Skills；
   - `control/mcp.rs`：Kumu 内返回 11 个工具，非 Kumu 环境 `initialize.instructions` 省略、`tools/list` 返回空、`tools/call` 返回 `isError`；
   - `control/cli_integration.rs`：参数解析、退出码、`install` 无参仅处理 `installed == true`（用 `*_BIN` 覆盖隔离本机环境）。
2. **开发后检查**：
   - 运行 `cargo test` 全部通过；
   - 运行 `kumu integration status` / `kumu integration status --json` 核对输出；
   - 按 `AGENTS.md` 审查新增测试，删除冗余或无意义的 case。
