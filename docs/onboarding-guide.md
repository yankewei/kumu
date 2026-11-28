# Kumu 桌面端新手引导（Onboarding Wizard）方案

> 状态：已实现

## 1. 目标与原则

Kumu 具备多窗格终端、编码 Agent 接入（通知 / MCP / Skills）、后台提醒与 Artifacts 可视化阅读等能力。本方案在 **Kumu 桌面 App 内** 提供一个克制、可交互的 **4 步分步引导向导**，每一步都带用户完成真实操作并看到即时反馈：

1. **接入编码 Agent**：自动检测本机已安装的 CLI，一键配置通知 + MCP + Skills；
2. **开启系统通知提醒**：配置提醒开关并发送真实测试通知，触发 macOS 首次权限弹窗；
3. **打开项目与第一个 Agent 窗格**：选择项目目录，一键在当前项目打开一个真实 Agent 窗格；
4. **体验 Artifacts 阅读页**：零磁盘副作用，直接用内置示例 Markdown 打开 `ArtifactReadingSheet` 预览页。

设计原则（对齐 `nsh-design-guides`）：
- **行动优先，不做 PPT**：每一步提供真实动作与明确副作用说明，状态实时反馈；
- **不打扰老用户、随时可重开**：仅全新初始化工作区自动弹出；`Esc` / 点击遮罩 / 右上角关闭仅本次关闭，点击「跳过，不再显示」、「完成引导」或第 4 步「预览示例 Artifact」写入 `onboarding_done = "1"`；随时可通过 `⌘K` 或设置页重开；
- **零运行时负担**：作为一次性向导 overlay（`ActiveOnboarding`），仅打开时挂载，关闭后零渲染与输入开销；不与 `active_dialog` / `active_sheet` 叠层；
- **纯状态机 + NSH 双向通道**：参考 `SettingsWindow` 的 `HostEvent` + `Action` 双向通道模式，流程状态抽成纯 Rust `OnboardingState`（便于直接单元测试），NSH 视图只负责渲染和事件转发，全量使用 `nsh_ui::components` 与 `active_theme()` 语义 token。

---

## 2. 触发时机与跨窗口入口

### 2.1 首次启动可靠判定
- `WorkspaceCore::bootstrap()` 返回 `seeded_now: bool`（本次 bootstrap 前 `SETTING_SEEDED` 不存在时为 `true`）；
- `Workspace` 持久化保存 `first_run: bool`，暴露 `first_run(&self) -> bool`；
- 纯函数判定：
  ```rust
  pub fn should_auto_open_onboarding(first_run: bool, onboarding_done: Option<&str>) -> bool {
      first_run && onboarding_done != Some("1")
  }
  ```
- `KumuApp::new` 读取该判定，仅当为 `true` 时在主窗口就绪后自动弹出 `ActiveOnboarding`。

### 2.2 退出与持久化语义
- 按 `Esc`、点击遮罩空白处或右上角关闭按钮：仅本次关闭，不写 `onboarding_done`；
- 点击左下角 **「跳过，不再显示」**、第 4 步 **「完成引导」** 或 **「预览示例 Artifact」**：写入 `SETTING_ONBOARDING_DONE = "1"` 后关闭（非预览场景归还终端焦点）。

### 2.3 重开入口
- **`⌘K` 命令面板**：新增 `PaletteTarget::OpenOnboarding`（在 `build_palette_items` 构造条目、`execute_palette_target` 中调用 `self.open_onboarding(window, cx)`）；
- **设置页跨窗口唤起**：新增 GPUI 全局信号 `OnboardingRequested { seq: u64 }`（定义于 `src/app/mod.rs` 并在 `src/ui/mod.rs` 初始化）。设置页点击「重新打开新手引导」时发送 `SettingsAction::OpenOnboarding` → `SettingsWindow` 更新 `OnboardingRequested` 全局信号 → 主窗口 `KumuApp` 通过 `cx.observe_global::<OnboardingRequested>` 自动打开 `active_onboarding` 并激活主窗口。

---

## 3. 4 步交互流详细设计

居中卡片宽度 `580px`、圆角 `12px` 带阴影（`shadow_lg`），中部内容区使用 `scroll()` 独立滚动；顶部 4 步进度条（区分已完成 `✓` / 当前步 / 待完成）+ 右上角关闭按钮；底部固定操作栏（左：「跳过，不再显示」；右：「上一步」+ 主按钮「下一步 / 完成引导」，且每屏保证唯一 Primary CTA）。

### 第 1 步：接入本机编码 Agent（通知 + MCP + Skills）
- **后台探测**：打开引导时通过 `background_executor` 调用 `agent::installed_with_shell_env` 与 `integration::detailed_status`，实时推送 `HostEvent::AgentsProbed`。
- **列表与状态**：
  - 优先展示本机已安装的 Agent（带官方 Logo、名称、Notify / MCP / Skills 状态徽章）；未安装的 Agent 汇总提示不干扰主操作；
  - 按钮下方注明副作用：「将向对应 Agent 的配置文件写入通知钩子、MCP 与 Skills 软链」。
- **核心动作**：
  - 主按钮 **「一键接入全部已安装 Agent」**：在 `background_executor` 中逐个调用 `integration::install_detailed()`，每完成一个推送 `HostEvent::AgentInstallFinished { agent, outcome, detail }`，行内精准展示通知 / MCP / Skills 各自的成功或失败详情；执行期间置 `busy` 禁止重复点击；
  - 每行也支持单独点击「接入 / 修复」；
  - 接入 `codex` 后行内提示：`重启 Codex 后请在 /hooks 中审查并信任`；
  - 本机无已安装 Agent 时显示友好空态，允许直接「下一步」。

### 第 2 步：开启后台通知提醒
- **控件与双向同步**：复用 `kumu_switch_bare` 渲染「启用后台通知」和「提示音」开关；切换时先持久化 `db::set_setting`，成功后再更新全局 `NotificationSettings`（失败时行内提示且不污染内存态）；同时在 `SettingsWindow` 补充 `cx.observe_global::<NotificationSettings>()`，确保设置页与引导同时打开时实时同步。
- **测试通知与权限**：
  - **「发送测试通知」**：调用 `cx.show_system_notification(...)`，首次从 `.app` 运行时触发 macOS 系统通知权限弹窗；行内注明“仅在打包后的 `Kumu.app` 中触发系统通知横幅（开发模式 `cargo run` 不会弹窗）”；
  - **「打开系统通知设置」**：调用 `open x-apple.systempreferences:com.apple.Notifications-Settings.extension`。

### 第 3 步：选择项目并打开第一个 Agent 窗格
- **异步闭环**：展示当前项目名称与路径；点击 **「选择项目目录」** 调用 `add_project`，在 `finish_add_project` / `select_project` 中推送 `HostEvent::ProjectChanged { name, path }` 实时更新卡片显示。
- **打开 Agent 窗格**：列出本机已安装的 Agent 单选（带官方 Logo）；点击 **「在当前项目打开 Agent 窗格」** 时先 `ensure_project_layout` 确保目标项目存在布局，再拆分/启动该 Agent 窗格，并将成功或失败结果以带语义色的 `FeedbackMessage` 回显到卡片内（创建后按钮切换为「再打开一个 Agent 窗格」避免误触重复拆屏）。未安装任何 Agent 时提示可直接进入下一步。

### 第 4 步：体验 Artifacts 可视化阅读（零磁盘副作用）
- **设计取舍**：不向用户项目写入 `.kumu/artifacts`（避免修改 `.kumu/.gitignore`、避免在抽屉出现「未关联」分组、也不要求必须先选中项目）。
- **核心动作**：
  - **「预览示例 Artifact」**：写入 `onboarding_done = "1"` 并关闭引导（不与 `active_sheet` 叠层，不抢终端焦点），直接用内置 Markdown 文本《Kumu 快速上手指南》（含 `⌘K` / `⌘D` / `⌘,` 快捷键与推荐提示词）打开 `ArtifactReadingSheet` 预览页；
  - 底部主按钮：**「完成引导」**（写入 `onboarding_done = "1"` 并关闭弹窗，焦点归还活动终端）。

---

## 4. Overlay 生命周期与 Modal 集成

1. **Modal Guard**（[src/ui/home/shortcuts.rs](file:///Users/yankewei/Documents/github/kumu-one/kumu-bugfix-optimize-onboarding/src/ui/home/shortcuts.rs)）：`has_modal` / 快捷键拦截条件加入 `self.active_onboarding.is_some()`，防止 `⌘K`、`⌘D`、`⌘,` 等穿透到底层工作区；`pane_is_attended` 同步排除 `active_onboarding`，避免遮挡期间误清底层 Agent 的 `unseen` 标记；
2. **Focus 归还**（[src/ui/home/mod.rs](file:///Users/yankewei/Documents/github/kumu-one/kumu-bugfix-optimize-onboarding/src/ui/home/mod.rs)）：`pending_focus` 条件排除 `self.active_onboarding.is_some()`，关闭引导时调用 `focus_terminal` 归还终端焦点；
3. **主题同步**（[src/ui/home/mod.rs](file:///Users/yankewei/Documents/github/kumu-one/kumu-bugfix-optimize-onboarding/src/ui/home/mod.rs)）：主题变更分支同步调用 `onboarding.sync_theme(theme, cx)`；同时订阅全局 `NotificationSettings` 变更，保持与设置页实时同步；
4. **Z-Order 与互斥**：打开 `active_onboarding` 时关闭 `active_palette` / `active_picker` / `active_dialog`；第 4 步打开 `ArtifactReadingSheet` 前先关闭 `active_onboarding`；
5. **键盘唯一入口**：`Esc` 统一在宿主侧 `src/ui/home/shortcuts.rs` 捕获阶段处理一次（不重复在 NSH 根节点监听），确保未聚焦 overlay 内部控件时也能可靠关闭；`Tab` / `Shift+Tab` 走 NSH 默认焦点顺序，`Enter` / `Space` 激活当前焦点控件，不全局劫持 `Enter`。

---

## 5. 文件改动清单

| 文件 | 改动类型 | 说明 |
| --- | --- | --- |
| `src/db/settings.rs`、`src/db/mod.rs` | 修改 | 新增 `SETTING_ONBOARDING_DONE` |
| `src/app/core.rs`、`src/app/mod.rs` | 修改 | `bootstrap()` 返回 `seeded_now`；`Workspace::first_run()`；定义全局 `OnboardingRequested` |
| `src/ui/mod.rs` | 修改 | 初始化 `OnboardingRequested` 全局状态 |
| `src/plugins/artifacts/sheet.rs` | 修改 | 新增 `ArtifactReadingSheet::from_inline_markdown(...)` 支持零磁盘副作用示例预览 |
| `src/nsh_ui/onboarding.rs`、`src/nsh_ui/mod.rs` | 新增 / 修改 | 纯状态机 `OnboardingState` + Pure Rust NSH 4 步引导视图 + `ActiveOnboarding` 双向桥接 |
| `src/ui/home/mod.rs`、`src/ui/home/dialogs.rs` | 修改 | 挂载 `active_onboarding`、首次启动判定、`observe_global::<OnboardingRequested>`、`pending_focus`、主题同步、动作处理 |
| `src/ui/home/shortcuts.rs`、`src/nsh_ui/command_palette.rs` | 修改 | Modal guard、`PaletteTarget::OpenOnboarding` 条目与执行分支 |
| `src/nsh_ui/settings/mod.rs`、`src/nsh_ui/settings/pages.rs` | 修改 | 订阅 `NotificationSettings` 全局同步；通用设置页增加「重新打开新手引导」按钮与 `SettingsAction::OpenOnboarding` |

---

## 6. 验证计划

1. **单元测试**：
   - `should_auto_open_onboarding`（新用户/老用户/已完成矩阵）；
   - `OnboardingState` 纯状态机（步骤前进后退边界、无已安装 Agent 空态、逐个安装部分失败明细聚合）；
2. **编译与质量检查**：
   - `cargo test` & `cargo clippy --all-targets -- -D warnings` 全部通过；
   - 按 `AGENTS.md` 审查新增测试，去除冗余 case。
