# 项目级 TODO 看板重构方案

关联 Issue：[#50](https://github.com/yankewei/kumu/issues/50)

## 1. 背景与目标

当前 TODO 作为右侧 `300px` 插件抽屉（`src/plugins/todo/`）存在两类限制：
1. **空间与交互受限**：只能纵向折叠泳道，无法承载宽屏看板与完整的任务编辑、多会话管理体验。
2. **绑定关系单一**：当前 `todos` 表仅存单个 `pane_id` / `agent_id` / `agent_session_id`，无法满足一个任务由多个 Agent 协作（例如实现、Review、补测试各自独立会话）的真实场景。

重构目标：
- 将 TODO 升级为**项目级一级模块**：点击左侧项目名在主内容区展示 **Jira 风格三列看板**，点击分支 / Worktree 展示终端；退役原右侧 TODO 插件。
- 支持 **1 个任务 ↔ 多个 Agent & Session** 持久化绑定、历史 Session 一键恢复（按 Agent 能力控制），以及在 Agent 终端内右键菜单与 Pane Header 双向关联。

---

## 2. 产品与交互设计

### 2.1 导航与信息架构
- **侧边栏结构保持极简**：
  - **点击项目名（`RepoHeader`）** → 主内容区切换为该项目的 TODO 看板，`RepoHeader` 显示选中态、未完成任务数徽标，以及原标题栏 TODO 按钮迁移过来的「待确认 / 等待输入」关注提示点。
  - **点击分支 / Worktree（`MainWorktree` / `LinkedWorktree`）** → 主内容区切换为该目录的多页签分屏终端。
  - 切换视图时，后台终端全程保持运行、不销毁。
- **全场景可达路径与返回路径**（覆盖无终端/无页签项目与非 Git 目录）：
  - **进入看板**：
    1. Git 仓库：直接点击侧边栏 `RepoHeader`。
    2. 非 Git 普通目录（`PlainProject`）：行右键菜单提供「打开 TODO 看板」；`EmptyTerminals` 空状态页提供「打开 TODO 看板」按钮；有终端时 Tab Bar 右侧提供「看板」入口。
    3. 任意项目下按全局快捷键 `⌘⇧T` 均可直接切入当前项目看板。
  - **返回终端**：看板顶部工具栏右侧提供显式 **「返回终端」** 按钮（切回 `self.selected` 记住的上次终端；若该项目尚无终端则自动打开主工作区终端），也可点击侧边栏任意分支/项目或再按 `⌘⇧T` 返回。

### 2.2 主内容区：Jira 风格三列看板
- **顶部工具栏**：项目名 + 搜索框（标题/编号）+ 快捷筛选（`全部` / `已关联 Agent` / `待确认`）+ `＋ 新建任务` + `返回终端`。
- **三列横向泳道**：`待办` | `进行中` | `已完成`
  - 列头展示状态指示、列标题、任务计数与 `+` 快捷新建按钮；列底提供 `+ 新建任务`。
  - 支持拖拽卡片跨列流转状态。
- **极简任务卡片**：
  - 上方：任务标题。
  - 底部一行：左下角编号（`#ID`），右下角为**折叠堆叠圆形 Agent 头像（Avatar Stack）**：
    - 每个绑定的 Agent Session 占一个圆形图标（相同的 Agent 也逐个展示；`agent_id` 为空的 shell 绑定显示通用终端图标）。
    - 负左间距重叠，**左侧头像覆盖右侧头像**（`z_index` 从左到右递减），带卡片背景色描边镂空。
    - 最多直接展示 **3 个**，超出在末尾叠放圆形 `+N`（例如 5 个显示 3 个头像 + `+2`）。
    - 运行中 / 等待输入带右下角小状态角标，历史已关闭会话降低透明度。
    - 只要任意关联会话处于 `Waiting` / `Failed` / `awaiting_review`，卡片左侧亮 3px 关注提示条。

### 2.3 新建与详情弹窗（全窗口 Overlay）
- **新建任务弹窗（Create Modal）**：
  - 点击顶部或列内 `＋ 新建任务` 弹出全窗口居中弹窗（半透明遮罩覆盖侧边栏与主内容区，点遮罩或按 `Esc` 关闭）。
  - 支持标题、描述、初始列选择，以及 **「仅创建」 / 「创建并启动默认 Agent」**。
- **查看 & 编辑双栏弹窗（Issue Detail Modal）**：
  - 点击任意卡片弹出全窗口居中双栏弹窗（点遮罩或按 `Esc` 关闭）：
    - **左栏**：可编辑标题与描述 + **「关联的 Agent & Session (N)」列表**。每条绑定独立显示 Agent 图标/状态/标题/Session ID/Worktree，支持 `↗ 跳转终端`、`追问`、`已确认`（清除待确认）、`断开`、`↺ 恢复 Session`（仅对支持 resume 且有合法 session ID 的 Agent 启用，其余展示禁用态与原因提示，可通过上方「启动 Agent」或右侧「快速派发」开启新会话）、`移除`；顶部支持随时追加启动新 Agent 或关联已有运行中终端。
    - **右栏**：三态切换器、快速启动协作 Agent（新 Tab / 新 Worktree / 向右分屏）、结构化元信息表（所属项目、会话数、创建/更新时间）及删除任务。

### 2.4 终端右键菜单与 Pane Header 联动
- **Agent 终端右键菜单（净新增）**：
  - 针对 Agent TUI 普遍开启 `mouse_tracking` 的现状，交互策略明确为：**右键始终弹出 Kumu 原生上下文菜单，`Shift + 右键` 才透传给 TUI**，确保在 Claude Code / Codex / OpenCode 等 TUI 内右键关联 TODO 100% 可达。
  - 菜单内容：基础项（复制、粘贴、向右分屏、向下分屏）+ 分隔线 + 当前终端已关联的 TODO 快捷项（「在看板中查看 `#ID`」「移至已完成」「解除与当前终端关联」）+ 「关联到 TODO 卡片」子列表（点击**追加**绑定，不覆盖其他 Agent）+ 「新建卡片并关联当前终端」。
- **Pane Header 胶囊（多 TODO 支持）**：
  - 作为显式常驻入口：已关联 TODO 的窗格在顶栏展示首个关联卡片的紧凑胶囊（点击直达看板并弹出该卡片详情）；若同一窗格关联了多个 TODO，末尾显示 `+N` 徽标，点击弹出菜单选择对应卡片。

---

## 3. 技术架构与实现方案

### 3.1 数据层与领域模型（无 UI 依赖）

#### 唯一事实来源原则
- **`todo_agent_links` 是 TODO ↔ Agent/Session 绑定关系的唯一事实来源**。
- `todos` 表原有的 `pane_id` / `agent_id` / `agent_session_id` 三列**仅保留作 v7 → v8 迁移的只读历史快照，所有写路径一律不再写入旧三列**，彻底杜绝双写冲突。
- 从 `TodoPatch`（`src/db/mod.rs`）移除 `pane_id` / `agent_id` / `agent_session_id`，所有绑定/解绑/同步统一走 `todo_agent_links` 专用 API。

#### SQLite Schema v7 → v8（`src/db/schema.rs`）
将 `SCHEMA_VERSION` 升至 `8`，新增 `migrate_v8_conn`：

```sql
CREATE TABLE IF NOT EXISTS todo_agent_links (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    todo_id INTEGER NOT NULL REFERENCES todos(id) ON DELETE CASCADE,
    agent_id TEXT,
    agent_session_id TEXT,
    pane_id INTEGER,
    project_id INTEGER REFERENCES projects(id) ON DELETE SET NULL,
    label TEXT NOT NULL DEFAULT '',
    awaiting_review INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS todo_agent_links_todo
    ON todo_agent_links(todo_id, id);
CREATE INDEX IF NOT EXISTS todo_agent_links_pane
    ON todo_agent_links(pane_id) WHERE pane_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS todo_agent_links_todo_session
    ON todo_agent_links(todo_id, agent_session_id)
    WHERE agent_session_id IS NOT NULL AND agent_session_id != '';
```

- **`agent_id` 允许为 `NULL`**：兼容旧数据中 `kumu todo add --bind` 在纯 shell pane 上产生的 `pane_id IS NOT NULL AND agent_id IS NULL` 行，v7 → v8 迁移无损复制旧行（含 `todos.awaiting_review`），不会触发 `NOT NULL` 失败。

#### 结构体与 Store API（`src/db/mod.rs`、`src/db/todos.rs`）
- 新增 `TodoAgentLink`：
  ```rust
  #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct TodoAgentLink {
      pub id: i64,
      pub todo_id: i64,
      #[serde(skip_serializing_if = "Option::is_none")]
      pub agent_id: Option<String>,
      #[serde(skip_serializing_if = "Option::is_none")]
      pub agent_session_id: Option<String>,
      #[serde(skip_serializing_if = "Option::is_none")]
      pub pane_id: Option<u64>,
      #[serde(skip_serializing_if = "Option::is_none")]
      pub project_id: Option<i64>,
      pub label: String,
      pub awaiting_review: bool,
      pub created_at: String,
      pub updated_at: String,
  }
  ```
- `TodoItem` 新增 `#[serde(default)] pub links: Vec<TodoAgentLink>`。
  - 读取 `todos(project_id)` / `todo(id)` 时批量加载 `links`，按 `pane_id IS NOT NULL DESC, awaiting_review DESC, updated_at DESC, id DESC` 排序。
  - `TodoItem` 的兼容字段（`pane_id` / `agent_id` / `agent_session_id`）从首条 live link（若无则首条 link）派生；`awaiting_review` 由 `links.iter().any(|l| l.awaiting_review)`（或无 link 时回退 `todos.awaiting_review`）派生，保证 CLI 与 JSON 输出兼容且避免多会话状态互相覆盖。
- **幂等去重规则**（`add_todo_link`）：
  1. 优先按 `(todo_id, pane_id)` 匹配已有记录并更新。
  2. 若未命中且 `agent_session_id` 非空，再按 `(todo_id, agent_session_id)` 匹配已有历史记录并重新写回 `pane_id`、`project_id`、`updated_at`，防止「断开后再重新关联同一会话」产生重复 link。
  3. 均未命中则插入新行。
- **全部 4 个旧 Store 方法迁至 `todo_agent_links`**：
  - `mark_pane_todos_review(pane_id)`：仅将 `todo_agent_links.pane_id = ?1` 且所属任务为 `in_progress` 的 link 置 `awaiting_review = 1`（不影响同任务下的其他 link）。
  - `clear_pane_todos_review(pane_id)`：仅清除该 `pane_id` 对应 link 的 `awaiting_review = 0`（A 会话 Waiting 不会被 B 会话 Running 误清）。
  - `update_pane_todos_session` → `sync_pane_link_session(pane_id, agent_id, session_id)`：同步更新该 `pane_id` 关联 link 的 `agent_id` 与 `agent_session_id`（若触发 `(todo_id, agent_session_id)` 唯一冲突则合并到已有历史行）。
  - `unbind_pane_todos` → `detach_pane_links(pane_id)`：将该 `pane_id` 对应 link 的 `pane_id` 置 `NULL`、`awaiting_review` 置 `0`，**保留 `agent_id` 与 `agent_session_id`**。
- 新增其余方法：
  - `sync_pane_link_label(pane_id, label)`
  - `rebind_todo_link_pane(link_id, pane_id, project_id)`
  - `detach_todo_link_pane(link_id)`
  - `remove_todo_link(link_id)`
  - `todos_for_pane(pane_id)`
  - `insert_todo_with_link(...)`：建任务并绑定放在同一 SQLite 事务内，避免半成功状态。

#### Workspace 与 Control CLI（`src/app/core.rs`、`src/control/`、`src/ui/home/control_handler.rs`）
- **统一事件刷新**：所有 `todos` / `todo_agent_links` 写操作统一发出 `WorkspaceEvent::TodosChanged(board_id)`，不再只发 `Changed`，保证侧边栏计数缓存、看板视图与 Pane Header 胶囊同步刷新。
- **CLI 全量适配**：
  - `TodoList { mine: true }`：按 `todo_agent_links.pane_id == caller` 过滤。
  - `TodoAdd { bind: true }`：调用事务版 `insert_todo_with_link`。
  - `TodoUpdate { bind: true }`：为调用者 pane 追加 link；`TodoUpdate { unbind: true }`：仅移除/断开调用者 pane 对应的 link，不影响该任务上的其他 Agent link。

---

### 3.2 主窗口状态机、导航与缓存（`src/ui/home/`）

- `KumuApp` 新增状态：
  - `active_todo_board: Option<i64>`：当前查看的根项目 `board_id`；`None` 时展示 `self.selected` 的终端页签。
  - `active_todo_modal: Option<ActiveTodoModal>`：全窗口挂载的新建/详情弹窗实体（与 `active_dialog` / `active_sheet` 同级）。
  - `todo_summaries: HashMap<i64, TodoBoardSummary>`：按根项目缓存 `open_count` 与 `needs_attention`（由 `WorkspaceEvent::TodosChanged` 及 `agent_status` 变化驱动增量刷新，避免 `build_sidebar_snapshot` 每帧查 SQLite）。
- **Sidebar 选中态隔离**（`sidebar.rs`、`sidebar_nsh.rs`）：
  - `ProjectRowSnapshot` 新增 `board_active: bool`、`todo_count: usize`、`todo_attention: bool`。
  - 当 `active_todo_board.is_some()` 时：`RepoHeader.active = (active_todo_board == Some(project.id))`，而 `MainWorktree` / `LinkedWorktree` / `PlainProject` 的 `active` 统一为 `false`（`self.selected` 保持不变以记住「返回终端」的目标）。
- **启动 Agent 的默认目标项目规则**：
  - `TodoBoardSnapshot` 携带 `launch_project_id: i64`：若 `self.selected` 属于当前 `board_id`（例如用户刚从 `feat/todo` worktree 切过来），则「新 Tab / 向右分屏」默认落在该 worktree；否则落在根仓库 `board_id`；「新 Worktree」始终基于根仓库创建。
- **快捷键守卫**（`src/ui/home/shortcuts.rs`）：
  - 顶层 overlay 守卫新增 `self.active_todo_modal.is_some()`：拦截所有工作区快捷键（`⌘T / ⌘W / ⌘D / ⌘F / ⌘1..9` 等），`Esc` 关闭弹窗。
  - 当 `self.active_todo_board.is_some()`（看板激活且无弹窗）时：拦截仅作用于终端的快捷键（`⌘D` / `⌘⇧D` / `⌘⇧Enter` / `⌘⇧W` 等），`⌘F` 聚焦看板搜索框而非隐藏终端的 find bar；`⌘⇧T` 切回终端。

---

### 3.3 Pure Rust NSH 看板与全窗口弹窗（`src/ui/home/todo_board/`）

严格遵循 Pure Rust NSH 规范与 `text_sm` (14px) / `text_xs` (12px) 两档字阶：
- `src/ui/home/todo_board/mod.rs`：快照类型（`TodoBoardSnapshot`、`TodoCardSnapshot`、`TodoLinkSnapshot`）、动作枚举（`TodoBoardAction`）、`KumuApp` 宿主回调与缓存维护。
- `src/ui/home/todo_board/board.rs`：顶部工具栏（含「返回终端」）+ 三列横向看板 + 极简卡片与左压右折叠圆形头像（`z_index` 递减、最多 3 个 + 圆形 `+N`）+ 跨列拖拽。
  - **跨列拖拽机制**：卡片记录 `pointer_down` 起点，位移超过 `6px` 阈值后才进入拖拽态（彻底隔离卡片点击与拖拽冲突）；拖拽期间直接按三列各自指针事件做列命中判定并高亮目标列边框，在列外或窗口外释放自动取消拖拽，仅在目标列内释放提交 `SetStatus`。
- `src/ui/home/todo_board/modal.rs`（含 `create_modal` & `detail_modal`）：
  - 采用与 `src/nsh_ui/dialogs.rs` 相同的 **full-window Pure Rust NSH overlay** 挂载在 `KumuApp` 顶层，完整遮罩侧边栏与主内容区，关闭后零渲染与输入开销。
- `src/ui/home/shell_nsh/mod.rs`：`CenterModeSnapshot` 新增 `TodoBoard(TodoBoardSnapshot)`，看板模式时隐藏终端 Tab Bar。

---

### 3.4 Session 同步、历史恢复与终端右键关联

- **生命周期同步（修正 `AgentExited` 与 stale-process 防护）**：
  - `WorkspaceEvent::Session { pane, agent, session }` → `workspace.sync_pane_link_session(pane.0, agent, session)`。
  - Agent 标题更新（`agents.rs`）→ 先校验 `pane_tab(pane).map(|(owner, _)| owner) == Some(project)` 防旧进程串写，再调用 `workspace.sync_pane_link_label(pane.0, title)`。
  - **`AgentExited`**：**仅在 `next.is_shell()` 时**调用 `workspace.detach_pane_links(pane.0)`；`resume_failed` 同窗格重启新 Agent 时保持绑定，等待新会话上报后同步。
  - **`PaneExited` 与 `close_pane`**：无条件调用 `workspace.detach_pane_links(pane.0)`。
- **历史 Session 恢复按能力控制**：
  - 在 `src/agent/launch.rs` 暴露 `pub fn supports_resume(agent: &str) -> bool`（对应 `codex` / `opencode` / `claude` / `cursor` / `agy`）。
  - `TodoLinkSnapshot` 携带 `can_resume: bool`（`supports_resume(agent)` 且 `agent_session_id` 合法）。
  - `KumuApp::resume_todo_link(link_id, window, cx)`：校验 `can_resume`，用 `PaneLaunch::Agent { agent, session: Some(session_id) }` 在 `link.project_id`（若已删除则回退根仓库）拉起新 Tab，调用 `rebind_todo_link_pane` 并聚焦终端。
- **终端右键原生菜单坐标与回调链路**：
  - `TerminalView`（`src/ui/home/terminal/mod.rs`）：`MouseButton::Right` 在 `!ev.modifiers.shift` 时发出 `TerminalEvent::ContextMenu { position: ev.position }`（仅 `Shift + 右键` 才走 `press_mouse` 透传给 TUI）。
  - `KumuApp` 将 GPUI 窗口坐标 `ev.position` 扣除 titlebar 高度转换为 NSH `page_position`，通过新增 `ShellHostEvent::OpenTerminalContextMenu { pane, position, items }` 交给 `NshHomeShell` 调用 `nsh_core::element::desktop::popup_menu_at`。
  - 菜单选中项通过新增 `HomeShellAction::TerminalContextAction { pane, action }` 回传 `KumuApp` 执行（复制、粘贴、分屏、打开对应卡片弹窗、移至已完成、解除关联、追加关联已有卡片、新建卡片并关联）。
- **Pane Header 胶囊**：
  - `PaneChromeSnapshot` 新增 `linked_todos: Vec<(i64, String)>`，`pane_tree.rs` 展示首条胶囊与可选 `+N` 下拉。

---

### 3.5 退役旧右侧 TODO Plugin
- 从 `src/plugins/mod.rs` 的 `specs()` 移除 `todo::TodoPanel`，删除 `src/plugins/todo/` 目录。
- 将 `src/ui/home/plugins_host.rs` 中专供 TODO 的派发逻辑迁入 `src/ui/home/todo_board/`，清理 `render_plugin_button` 中针对 `spec.id == "todo"` 的特判，并将关注点提示迁至侧边栏 `RepoHeader`。

---

## 4. 实施阶段与前置验证

### 阶段 0：前置验证（进入阶段 1 前先落测试/验证）
1. **迁移验证**：构造真实 v7 测试库（包含 `pane_id` 非空而 `agent_id = NULL` 的旧任务，以及含 `agent_session_id` 的历史任务），验证 v8 迁移不因 `NOT NULL` 失败且无损迁移 `awaiting_review` 与历史会话。
2. **右键菜单与坐标验证**：在开启 `mouse_tracking` 的 Agent TUI 中验证右键必弹 Kumu 原生菜单、`Shift + 右键` 透传给 TUI，且菜单弹出位置与鼠标指针精准对齐。
3. **弹窗与快捷键隔离验证**：打开全窗口新建/详情弹窗后，验证 `⌘T / ⌘W / ⌘D / ⌘F` 不会操作背后隐藏终端，点击遮罩不会误触侧边栏，`Esc` 正确关闭弹窗。

### 阶段 1–5：分步落地
1. **阶段 1（数据层）**：SQLite v8 迁移、`todo_agent_links` 唯一事实来源、`TodoAgentLink` CRUD、4 个旧方法迁移、`Workspace` 统一发 `TodosChanged`、CLI 全量适配与单测。
2. **阶段 2（导航、缓存与壳层）**：`active_todo_board`、`todo_summaries` 缓存、Sidebar `board_active` 与 `RepoHeader` 关注点、无终端/非 Git 目录入口、`⌘⇧T` 与快捷键守卫、`CenterModeSnapshot::TodoBoard`。
3. **阶段 3（看板与全窗口弹窗 UI）**：三列看板、阈值防冲突跨列拖拽、极简卡片（左压右折叠圆形头像最多 3 个 + `+N`）、全窗口新建弹窗与双栏详情弹窗。
4. **阶段 4（Session 闭环与终端联动）**：`AgentExited` 条件解绑、Session & 标题同步、`supports_resume` 历史恢复、终端右键原生菜单（含坐标转换与回传）与 Pane Header 多 TODO 胶囊。
5. **阶段 5（清理与验证）**：退役 `src/plugins/todo/`，运行 `cargo fmt --check`、`cargo clippy --all-targets`、`cargo test` 验证通过。
