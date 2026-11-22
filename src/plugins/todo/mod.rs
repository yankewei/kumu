//! Right-hand TODO Kanban board plugin rendered in Pure Rust NSH
//! (`nsh-platform-gpui`), linked bidirectionally with Kumu's Agent panes.
//!
//! Designed around three UX principles:
//! 1. Never block the live terminals — task creation expands inline and detail
//!    editing opens inside the right-hand panel (`← 返回看板`).
//! 2. One-click high-frequency flows — clicking a linked in-progress card jumps
//!    directly to its Agent pane; Todo cards remember the last-used Agent for
//!    1-click split dispatch (`▶ 交给 Claude | ▾`).
//! 3. Calm visual hierarchy — borderless status pills and a 3px left accent bar
//!    only for cards needing human attention (`等待输入`, `待确认`, `执行失败`).

mod view;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription,
    Task, Window, div,
};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::{AgentPaneInfo, DispatchMode, Plugin, PluginHost, PluginProject};
use crate::agent::{AGENTS, AgentState};
use crate::app::WorkspaceEvent;
use crate::db::{SETTING_TODO_DEFAULT_AGENT, TodoItem, TodoPatch, TodoStatus};
use crate::nsh_ui::mount::MountRegistry;
use crate::nsh_ui::theme::{appearance, nsh_theme};
use crate::theme::KumuTheme;

/// Format the prompt sent to an Agent pane when dispatching a TODO card.
pub(super) fn format_task_prompt(todo: &TodoItem) -> String {
    let title = todo.title.trim();
    let desc = todo.description.trim();
    if desc.is_empty() {
        format!("请完成以下 TODO 任务（#{}）：{}", todo.id, title)
    } else {
        format!(
            "请完成以下 TODO 任务（#{}）：{}\n\n{}",
            todo.id, title, desc
        )
    }
}

/// Short display name for a known agent id (e.g. `"claude"` → `"Claude"`).
pub(super) fn short_agent_name(agent_id: &str) -> String {
    match agent_id {
        "claude" => "Claude".to_string(),
        "codex" => "Codex".to_string(),
        "opencode" => "OpenCode".to_string(),
        "cursor" => "Cursor".to_string(),
        "grok" => "Grok".to_string(),
        "agy" => "Agy".to_string(),
        "omp" => "OMP".to_string(),
        "pi" => "Pi".to_string(),
        other => AGENTS
            .iter()
            .find(|def| def.id == other)
            .map(|def| def.name.to_string())
            .unwrap_or_else(|| other.to_string()),
    }
}

/// Pick the initial default agent: saved preference if installed, else first installed agent.
fn resolve_default_agent() -> Option<String> {
    if let Some(saved) = crate::db::get_setting(SETTING_TODO_DEFAULT_AGENT)
        && crate::agent::installed(&saved)
    {
        return Some(saved);
    }
    AGENTS
        .iter()
        .find(|def| crate::agent::installed(def.id))
        .map(|def| def.id.to_string())
}

fn installed_agents_list() -> Vec<(String, String)> {
    AGENTS
        .iter()
        .filter(|def| crate::agent::installed(def.id))
        .map(|def| (def.id.to_string(), def.name.to_string()))
        .collect()
}

/// Immutable board snapshot pushed from the GPUI host into the Pure Rust NSH view.
#[derive(Clone, Debug, Default)]
pub(super) struct TodoBoardSnapshot {
    pub(super) has_project: bool,
    pub(super) board_id: Option<i64>,
    pub(super) shared_board_name: Option<String>,
    pub(super) todos: Vec<TodoItem>,
    pub(super) live_panes: Vec<AgentPaneInfo>,
    pub(super) installed_agents: Vec<(String, String)>,
    pub(super) default_agent: Option<String>,
}

/// Events pushed from the GPUI host into the NSH TODO board engine thread.
#[derive(Clone, Debug)]
pub(super) enum TodoHostEvent {
    Snapshot(TodoBoardSnapshot),
    Notice { message: String, is_error: bool },
    ClearNotice,
    OpenInProgressLane,
    ClearFollowup,
}

/// User actions emitted by the Pure Rust NSH TODO board back to the GPUI host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TodoAction {
    CreateTodo {
        title: String,
        description: String,
        dispatch_default: bool,
    },
    SetStatus {
        id: i64,
        status: TodoStatus,
    },
    UpdateTitle {
        id: i64,
        title: String,
    },
    UpdateDescription {
        id: i64,
        description: String,
    },
    DeleteItem(i64),
    ClearDone,
    UnbindAgent(i64),
    DispatchDefault(i64),
    DispatchNewAgent {
        id: i64,
        agent_id: String,
        new_tab: bool,
    },
    BindExistingPane {
        id: i64,
        pane: AgentPaneInfo,
        send_prompt: bool,
    },
    SendFollowup {
        id: i64,
        text: String,
    },
    RevealPane(u64),
}

pub(super) struct TodoMount {
    pub(super) initial: TodoBoardSnapshot,
    pub(super) events_rx: UnboundedReceiver<TodoHostEvent>,
    pub(super) actions_tx: UnboundedSender<TodoAction>,
}

static MOUNTS: MountRegistry<TodoMount> = MountRegistry::new();

pub(super) fn take_mount(id: u64) -> Option<TodoMount> {
    MOUNTS.take(id)
}

pub struct TodoPanel {
    mount_id: u64,
    project: Option<PluginProject>,
    host: PluginHost,
    app_view: Entity<AppView>,
    events_tx: UnboundedSender<TodoHostEvent>,
    default_agent: Option<String>,
    _subscriptions: Vec<Subscription>,
    _actions_task: Task<()>,
}

impl Drop for TodoPanel {
    fn drop(&mut self) {
        let _ = take_mount(self.mount_id);
    }
}

impl Plugin for TodoPanel {
    const ID: &'static str = "todo";
    const TITLE: &'static str = "TODO";
    const ICON: &'static str = "icons/list-todo.svg";

    fn new(
        project: Option<PluginProject>,
        host: PluginHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (events_tx, events_rx) = unbounded::<TodoHostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<TodoAction>();

        let default_agent = resolve_default_agent();
        // `TodoPanel::new` runs synchronously inside `KumuApp::toggle_plugin`
        // while `KumuApp` is mutably leased, so reading `host.agent_panes(...)`
        // here would re-enter `KumuApp` and panic. Build the initial snapshot
        // from `Workspace` alone and defer the live-pane refresh until the
        // outer `KumuApp` update finishes.
        let initial = Self::build_snapshot_inner(
            project.as_ref(),
            &host,
            default_agent.as_deref(),
            false,
            cx,
        );

        let mount_id = MOUNTS.register(TodoMount {
            initial,
            events_rx,
            actions_tx,
        });

        let kumu_theme = KumuTheme::global(cx);
        let initial_nsh_theme = nsh_theme(kumu_theme);
        let initial_appearance = appearance(kumu_theme.mode);

        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu TODO".to_owned(),
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.set_theme(Some(initial_nsh_theme), cx);
            view.set_appearance(initial_appearance, cx);
            view.load(nsh_platform_gpui::App::Rust(Root::new::<view::TodoRoot>(
                move || view::TodoRootProps {
                    mount_id: mount_id.into(),
                },
            )));
            view
        });

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.observe_global::<KumuTheme>(|panel: &mut Self, cx| {
            let kumu_theme = KumuTheme::global(cx);
            let (mapped, mode) = (nsh_theme(kumu_theme), appearance(kumu_theme.mode));
            panel.app_view.update(cx, |view, cx| {
                view.set_theme(Some(mapped), cx);
                view.set_appearance(mode, cx);
            });
        }));

        if let Some(workspace) = host.workspace() {
            subscriptions.push(cx.subscribe(&workspace, |panel, _, event, cx| match event {
                WorkspaceEvent::TodosChanged(board_id) => {
                    if panel.board_id(cx) == Some(*board_id) {
                        panel.schedule_snapshot(cx);
                    }
                }
                WorkspaceEvent::Changed
                | WorkspaceEvent::AgentStatus { .. }
                | WorkspaceEvent::AgentTitle { .. }
                | WorkspaceEvent::AgentExited { .. }
                | WorkspaceEvent::PaneExited { .. }
                | WorkspaceEvent::Session { .. } => {
                    panel.schedule_snapshot(cx);
                }
                _ => {}
            }));
        }

        let actions_task = cx.spawn_in(window, async move |this, cx| {
            while let Some(action) = actions_rx.next().await {
                let res = this.update_in(cx, |panel, window, cx| {
                    panel.handle_action(action, window, cx);
                });
                if res.is_err() {
                    break;
                }
            }
        });

        let panel = Self {
            mount_id,
            project,
            host,
            app_view,
            events_tx,
            default_agent,
            _subscriptions: subscriptions,
            _actions_task: actions_task,
        };
        panel.schedule_snapshot(cx);
        panel
    }

    fn set_project(&mut self, project: Option<PluginProject>, cx: &mut Context<Self>) {
        if self.project == project {
            return;
        }
        self.project = project;
        self.schedule_snapshot(cx);
    }
}

impl TodoPanel {
    fn board_id(&self, cx: &App) -> Option<i64> {
        let project = self.project.as_ref()?;
        let workspace = self.host.workspace()?;
        Some(workspace.read(cx).board_project_id(project.id))
    }

    fn build_snapshot(
        project: Option<&PluginProject>,
        host: &PluginHost,
        default_agent: Option<&str>,
        cx: &App,
    ) -> TodoBoardSnapshot {
        Self::build_snapshot_inner(project, host, default_agent, true, cx)
    }

    fn build_snapshot_inner(
        project: Option<&PluginProject>,
        host: &PluginHost,
        default_agent: Option<&str>,
        include_live_panes: bool,
        cx: &App,
    ) -> TodoBoardSnapshot {
        let Some(project) = project else {
            return TodoBoardSnapshot {
                installed_agents: installed_agents_list(),
                default_agent: Self::active_default_agent_from(default_agent),
                ..Default::default()
            };
        };
        let (board_id, shared_board_name, todos) = match host.workspace() {
            Some(ws_entity) => {
                let ws = ws_entity.read(cx);
                let bid = ws.board_project_id(project.id);
                let shared = (bid != project.id)
                    .then(|| ws.project(bid).map(|p| p.name.clone()))
                    .flatten();
                (Some(bid), shared, ws.todos_for_project(project.id))
            }
            None => (None, None, Vec::new()),
        };
        let live_panes = if include_live_panes {
            board_id
                .map(|bid| host.agent_panes(bid, cx))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        TodoBoardSnapshot {
            has_project: true,
            board_id,
            shared_board_name,
            todos,
            live_panes,
            installed_agents: installed_agents_list(),
            default_agent: Self::active_default_agent_from(default_agent),
        }
    }

    /// Defer snapshot pushes until the current GPUI update finishes so callers
    /// holding a mutable lease on `KumuApp` never re-enter `KumuApp::read(cx)`.
    fn schedule_snapshot(&self, cx: &mut Context<Self>) {
        let panel = cx.weak_entity();
        (**cx).defer(move |cx| {
            let _ = panel.read_with(cx, |panel, cx| panel.push_snapshot(cx));
        });
    }

    fn push_snapshot(&self, cx: &App) {
        let snap = Self::build_snapshot(
            self.project.as_ref(),
            &self.host,
            self.default_agent.as_deref(),
            cx,
        );
        let _ = self.events_tx.unbounded_send(TodoHostEvent::Snapshot(snap));
    }

    fn push_notice(&self, message: impl Into<String>, is_error: bool) {
        let _ = self.events_tx.unbounded_send(TodoHostEvent::Notice {
            message: message.into(),
            is_error,
        });
    }

    fn active_default_agent_from(saved: Option<&str>) -> Option<String> {
        saved
            .filter(|id| crate::agent::installed(id))
            .map(str::to_string)
            .or_else(resolve_default_agent)
    }

    fn active_default_agent(&self) -> Option<String> {
        Self::active_default_agent_from(self.default_agent.as_deref())
    }

    fn remember_default_agent(&mut self, agent_id: &str) {
        if self.default_agent.as_deref() == Some(agent_id) {
            return;
        }
        self.default_agent = Some(agent_id.to_string());
        let _ = crate::db::set_setting(SETTING_TODO_DEFAULT_AGENT, agent_id);
    }

    fn todo_by_id(&self, id: i64, cx: &App) -> Option<TodoItem> {
        self.host.workspace()?.read(cx).todo(id)
    }

    fn handle_action(&mut self, action: TodoAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            TodoAction::CreateTodo {
                title,
                description,
                dispatch_default,
            } => {
                if let Some(item) = self.create_todo(&title, &description, cx)
                    && dispatch_default
                {
                    self.dispatch_default_agent(&item, cx);
                }
                self.push_snapshot(cx);
            }
            TodoAction::SetStatus { id, status } => {
                self.set_status(id, status, cx);
                self.push_snapshot(cx);
            }
            TodoAction::UpdateTitle { id, title } => {
                if let Some(workspace) = self.host.workspace() {
                    workspace.update(cx, |ws, cx| {
                        ws.update_todo(
                            id,
                            &TodoPatch {
                                title: Some(title),
                                ..Default::default()
                            },
                            cx,
                        );
                    });
                }
            }
            TodoAction::UpdateDescription { id, description } => {
                if let Some(workspace) = self.host.workspace() {
                    workspace.update(cx, |ws, cx| {
                        ws.update_todo(
                            id,
                            &TodoPatch {
                                description: Some(description),
                                ..Default::default()
                            },
                            cx,
                        );
                    });
                }
            }
            TodoAction::DeleteItem(id) => {
                if let Some(workspace) = self.host.workspace() {
                    workspace.update(cx, |ws, cx| {
                        ws.delete_todo(id, cx);
                    });
                }
                self.push_snapshot(cx);
            }
            TodoAction::ClearDone => {
                if let (Some(project), Some(workspace)) =
                    (self.project.as_ref(), self.host.workspace())
                {
                    workspace.update(cx, |ws, cx| {
                        ws.clear_done_todos(project.id, cx);
                    });
                }
                self.push_snapshot(cx);
            }
            TodoAction::UnbindAgent(id) => {
                if let Some(workspace) = self.host.workspace() {
                    let patch = TodoPatch {
                        pane_id: Some(None),
                        awaiting_review: Some(false),
                        ..Default::default()
                    };
                    workspace.update(cx, |ws, cx| {
                        ws.update_todo(id, &patch, cx);
                    });
                }
                self.push_snapshot(cx);
            }
            TodoAction::DispatchDefault(id) => {
                if let Some(item) = self.todo_by_id(id, cx) {
                    self.dispatch_default_agent(&item, cx);
                }
                self.push_snapshot(cx);
            }
            TodoAction::DispatchNewAgent {
                id,
                agent_id,
                new_tab,
            } => {
                if let Some(item) = self.todo_by_id(id, cx) {
                    self.dispatch_new_agent(&item, &agent_id, new_tab, cx);
                }
                self.push_snapshot(cx);
            }
            TodoAction::BindExistingPane {
                id,
                pane,
                send_prompt,
            } => {
                if let Some(item) = self.todo_by_id(id, cx) {
                    self.bind_existing_pane(&item, &pane, send_prompt, cx);
                }
                self.push_snapshot(cx);
            }
            TodoAction::SendFollowup { id, text } => {
                self.send_followup(id, &text, cx);
                self.push_snapshot(cx);
            }
            TodoAction::RevealPane(pane) => {
                self.host.reveal_pane(pane, window, cx);
            }
        }
    }

    fn create_todo(
        &mut self,
        title: &str,
        description: &str,
        cx: &mut Context<Self>,
    ) -> Option<TodoItem> {
        let project = self.project.as_ref()?;
        let workspace = self.host.workspace()?;
        let _ = self.events_tx.unbounded_send(TodoHostEvent::ClearNotice);
        workspace.update(cx, |ws, cx| {
            ws.add_todo(
                project.id,
                title,
                description,
                TodoStatus::Todo,
                None,
                None,
                None,
                cx,
            )
        })
    }

    fn set_status(&mut self, id: i64, status: TodoStatus, cx: &mut Context<Self>) {
        let Some(workspace) = self.host.workspace() else {
            return;
        };
        let _ = self.events_tx.unbounded_send(TodoHostEvent::ClearNotice);
        let patch = TodoPatch {
            status: Some(status),
            awaiting_review: (status != TodoStatus::InProgress).then_some(false),
            ..Default::default()
        };
        workspace.update(cx, |ws, cx| {
            ws.update_todo(id, &patch, cx);
        });
    }

    fn dispatch_default_agent(&mut self, item: &TodoItem, cx: &mut Context<Self>) {
        let Some(agent_id) = self.active_default_agent() else {
            self.push_notice("未检测到已安装的编码 Agent", true);
            return;
        };
        self.dispatch_new_agent(item, &agent_id, false, cx);
    }

    fn dispatch_new_agent(
        &mut self,
        item: &TodoItem,
        agent_id: &str,
        new_tab: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project.as_ref() else {
            return;
        };
        let prompt = format_task_prompt(item);
        let mode = if new_tab {
            DispatchMode::NewTab {
                tab_name: item.title.clone(),
            }
        } else {
            DispatchMode::SplitRight
        };
        match self
            .host
            .dispatch_agent(project.id, agent_id, mode, prompt, cx)
        {
            Ok(pane_id) => {
                self.remember_default_agent(agent_id);
                if let Some(workspace) = self.host.workspace() {
                    let patch = TodoPatch {
                        status: Some(TodoStatus::InProgress),
                        pane_id: Some(Some(pane_id)),
                        agent_id: Some(Some(agent_id.to_string())),
                        awaiting_review: Some(false),
                        ..Default::default()
                    };
                    workspace.update(cx, |ws, cx| {
                        ws.update_todo(item.id, &patch, cx);
                    });
                }
                let _ = self
                    .events_tx
                    .unbounded_send(TodoHostEvent::OpenInProgressLane);
                let _ = self.events_tx.unbounded_send(TodoHostEvent::ClearNotice);
            }
            Err(error) => {
                self.push_notice(error, true);
            }
        }
    }

    fn bind_existing_pane(
        &mut self,
        item: &TodoItem,
        pane: &AgentPaneInfo,
        send_prompt: bool,
        cx: &mut Context<Self>,
    ) {
        if send_prompt {
            let prompt = format_task_prompt(item);
            if let Err(error) = self.host.send_to_any_pane(pane.pane, &prompt, cx) {
                self.push_notice(error, true);
                return;
            }
        }
        self.remember_default_agent(&pane.agent);
        if let Some(workspace) = self.host.workspace() {
            let awaiting = !send_prompt && pane.state == Some(AgentState::Completed);
            let patch = TodoPatch {
                status: Some(TodoStatus::InProgress),
                pane_id: Some(Some(pane.pane)),
                agent_id: Some(Some(pane.agent.clone())),
                awaiting_review: Some(awaiting),
                ..Default::default()
            };
            workspace.update(cx, |ws, cx| {
                ws.update_todo(item.id, &patch, cx);
            });
        }
        let _ = self
            .events_tx
            .unbounded_send(TodoHostEvent::OpenInProgressLane);
        let _ = self.events_tx.unbounded_send(TodoHostEvent::ClearNotice);
    }

    fn send_followup(&mut self, todo_id: i64, text: &str, cx: &mut Context<Self>) {
        let Some(pane_id) = self.todo_by_id(todo_id, cx).and_then(|item| item.pane_id) else {
            self.push_notice("该任务尚未关联运行中的 Agent 窗格", true);
            return;
        };
        match self.host.send_to_any_pane(pane_id, text, cx) {
            Ok(_) => {
                let _ = self.events_tx.unbounded_send(TodoHostEvent::ClearFollowup);
                if let Some(workspace) = self.host.workspace() {
                    workspace.update(cx, |ws, cx| {
                        ws.update_todo(
                            todo_id,
                            &TodoPatch {
                                status: Some(TodoStatus::InProgress),
                                awaiting_review: Some(false),
                                ..Default::default()
                            },
                            cx,
                        );
                    });
                }
                let _ = self.events_tx.unbounded_send(TodoHostEvent::ClearNotice);
            }
            Err(error) => {
                self.push_notice(error, true);
            }
        }
    }
}

impl Render for TodoPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.app_view.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_engine::EngineConfig;

    #[test]
    fn task_prompt_omits_blank_description_and_keeps_multiline_body() {
        let mut item = TodoItem {
            id: 12,
            project_id: 1,
            title: "Fix OSC parser".into(),
            description: "   ".into(),
            status: TodoStatus::Todo,
            position: 0,
            pane_id: None,
            agent_id: None,
            agent_session_id: None,
            awaiting_review: false,
            created_at: String::new(),
            updated_at: String::new(),
            completed_at: None,
        };
        assert_eq!(
            format_task_prompt(&item),
            "请完成以下 TODO 任务（#12）：Fix OSC parser"
        );
        item.description = "Handle kumu-status signals\nAdd tests".into();
        assert_eq!(
            format_task_prompt(&item),
            "请完成以下 TODO 任务（#12）：Fix OSC parser\n\nHandle kumu-status signals\nAdd tests"
        );
    }

    #[test]
    fn todo_nsh_board_dispatch_confirm_and_detail_navigation() {
        let (events_tx, events_rx) = unbounded::<TodoHostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<TodoAction>();

        let initial = TodoBoardSnapshot {
            has_project: true,
            board_id: Some(1),
            shared_board_name: None,
            todos: vec![
                TodoItem {
                    id: 1,
                    project_id: 1,
                    title: "重构 TODO 看板".into(),
                    description: "使用 Pure Rust NSH 实现".into(),
                    status: TodoStatus::InProgress,
                    position: 0,
                    pane_id: Some(8),
                    agent_id: Some("agy".into()),
                    agent_session_id: None,
                    awaiting_review: true,
                    created_at: String::new(),
                    updated_at: String::new(),
                    completed_at: None,
                },
                TodoItem {
                    id: 2,
                    project_id: 1,
                    title: "迁移 Git Log 面板".into(),
                    description: String::new(),
                    status: TodoStatus::Todo,
                    position: 0,
                    pane_id: None,
                    agent_id: None,
                    agent_session_id: None,
                    awaiting_review: false,
                    created_at: String::new(),
                    updated_at: String::new(),
                    completed_at: None,
                },
            ],
            live_panes: vec![AgentPaneInfo {
                pane: 8,
                project_id: 1,
                project_name: "kumu".into(),
                tab_name: "主终端".into(),
                ordinal: 1,
                agent: "agy".into(),
                title: "Agy".into(),
                state: Some(AgentState::Completed),
                focused: true,
            }],
            installed_agents: vec![("claude".into(), "Claude Code".into())],
            default_agent: Some("claude".into()),
        };

        let mount_id = MOUNTS.register(TodoMount {
            initial,
            events_rx,
            actions_tx,
        });

        let root = nsh_core::element::Root::new::<view::TodoRoot>(move || view::TodoRootProps {
            mount_id: mount_id.into(),
        });
        let mut app = nsh_testing::Headless::start(EngineConfig::new(root)).expect("start TODO");

        app.wait_for("重构 TODO 看板").expect("in-progress card");
        app.wait_for("Agy · 待确认").expect("review pill");
        app.wait_for("迁移 Git Log 面板").expect("pending card");

        // 1-click confirm on awaiting-review card marks it Done.
        app.press("确认").expect("press confirm CTA");
        assert_eq!(
            actions_rx.try_recv().ok(),
            Some(TodoAction::SetStatus {
                id: 1,
                status: TodoStatus::Done
            })
        );

        // 1-click default dispatch on pending card.
        app.press("交给 Claude").expect("press default dispatch");
        assert_eq!(
            actions_rx.try_recv().ok(),
            Some(TodoAction::DispatchDefault(2))
        );

        // Open inline detail view by clicking pending card title.
        app.press("迁移 Git Log 面板").expect("open detail");
        app.wait_for("返回看板").expect("detail header renders");
        app.press("返回看板").expect("return to board");
        app.wait_for("进行中").expect("board lanes render again");

        // Host notice appears and dismisses cleanly.
        events_tx
            .unbounded_send(TodoHostEvent::Notice {
                message: "测试提示".into(),
                is_error: false,
            })
            .expect("send notice");
        app.wait_for("测试提示").expect("notice renders");
    }

    #[gpui::test]
    fn opening_todo_panel_during_host_update_does_not_reenter_host(cx: &mut gpui::TestAppContext) {
        use std::path::PathBuf;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct FakeHost {
            panel: Option<Entity<TodoPanel>>,
            reads: Arc<AtomicUsize>,
        }
        impl Render for FakeHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().children(self.panel.clone())
            }
        }

        cx.update(|cx| {
            cx.set_global(KumuTheme::load_bundled());
        });
        let workspace = cx.new(crate::app::Workspace::new);
        let reads = Arc::new(AtomicUsize::new(0));
        let reads_for_host = Arc::clone(&reads);
        let window = cx.add_window(|_, _| FakeHost {
            panel: None,
            reads: reads_for_host,
        });

        // Simulate `KumuApp::toggle_plugin`: create `TodoPanel` while `FakeHost`
        // is mutably leased, with `PluginHost::agent_panes` reading `FakeHost`.
        window
            .update(cx, |host_view, window, cx| {
                let weak_host = cx.entity().downgrade();
                let plugin_host = PluginHost::with_panes(
                    |_, _, _, _| Ok(crate::plugins::SendOutcome::Pasted),
                    |_, _| Vec::new(),
                )
                .with_agent_tools(
                    workspace.clone(),
                    move |_, cx| {
                        if let Some(home) = weak_host.upgrade() {
                            home.read(cx).reads.fetch_add(1, Ordering::SeqCst);
                        }
                        Vec::new()
                    },
                    |_, _, _, _, _| Err("unused".into()),
                    |_, _, _| Err("unused".into()),
                    |_, _, _| {},
                );
                let project = Some(PluginProject {
                    id: 1,
                    path: PathBuf::from("/tmp/kumu"),
                });
                let panel = cx.new(|cx| TodoPanel::new(project, plugin_host, window, cx));
                host_view.panel = Some(panel);
            })
            .expect("toggle TODO panel inside host update without re-entrant panic");

        // Deferred snapshot runs after the outer host update finishes and can
        // safely read the host entity.
        assert!(
            reads.load(Ordering::SeqCst) >= 1,
            "deferred live-pane snapshot should run once host update completes"
        );
    }
}
