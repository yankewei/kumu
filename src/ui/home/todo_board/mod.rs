//! Project-level Jira-style TODO Kanban board and full-window Create/Detail modals.
//!
//! Owns:
//! - Snapshot types (`TodoBoardSnapshot`, `TodoCardSnapshot`, `TodoLinkSnapshot`)
//! - Sidebar summary cache (`TodoBoardSummary`) refreshed on `TodosChanged`
//! - Pure Rust NSH board view (`board.rs`) and full-window modal overlay (`modal.rs`)
//! - `KumuApp` host handlers for navigation, 1:N Agent/Session dispatch, and historical resume.

pub(in crate::ui::home) mod board;
pub(in crate::ui::home) mod modal;

use std::collections::HashMap;

use gpui::{Context, Focusable, Window};

use super::KumuApp;
use crate::agent::launch::{PaneLaunch, session_is_valid, supports_resume};
use crate::agent::{AGENTS, AgentState};
use crate::app::layout::PaneId;
use crate::db::{SETTING_TODO_DEFAULT_AGENT, TodoItem, TodoPatch, TodoStatus};
use crate::nsh_ui::toast::ToastItem;
use crate::plugins::{AgentPaneInfo, DispatchMode};

pub(in crate::ui::home) use modal::{ActiveTodoModal, TodoModalKind};

/// Cached per-root-board counters for the sidebar (`build_sidebar_snapshot`
/// never queries SQLite on every frame).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::ui::home) struct TodoBoardSummary {
    pub(in crate::ui::home) open_count: usize,
    pub(in crate::ui::home) needs_attention: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::ui::home) enum TodoBoardFilter {
    #[default]
    All,
    Linked,
    Attention,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui::home) enum TodoDispatchTarget {
    SplitRight,
    NewTab,
    NewWorktree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui::home) struct TodoLinkSnapshot {
    pub(in crate::ui::home) id: i64,
    pub(in crate::ui::home) agent_id: Option<String>,
    pub(in crate::ui::home) agent_name: String,
    pub(in crate::ui::home) logo: Option<&'static str>,
    pub(in crate::ui::home) session_id: Option<String>,
    pub(in crate::ui::home) pane_id: Option<u64>,
    pub(in crate::ui::home) project_id: Option<i64>,
    pub(in crate::ui::home) worktree_name: Option<String>,
    pub(in crate::ui::home) label: String,
    pub(in crate::ui::home) state: Option<AgentState>,
    pub(in crate::ui::home) awaiting_review: bool,
    pub(in crate::ui::home) can_resume: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui::home) struct TodoCardSnapshot {
    pub(in crate::ui::home) id: i64,
    pub(in crate::ui::home) project_id: i64,
    pub(in crate::ui::home) title: String,
    pub(in crate::ui::home) description: String,
    pub(in crate::ui::home) status: TodoStatus,
    pub(in crate::ui::home) awaiting_review: bool,
    pub(in crate::ui::home) needs_attention: bool,
    pub(in crate::ui::home) links: Vec<TodoLinkSnapshot>,
    pub(in crate::ui::home) created_at: String,
    pub(in crate::ui::home) updated_at: String,
    pub(in crate::ui::home) completed_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::ui::home) struct TodoBoardSnapshot {
    pub(in crate::ui::home) board_id: i64,
    pub(in crate::ui::home) board_name: String,
    pub(in crate::ui::home) launch_project_id: i64,
    pub(in crate::ui::home) todos: Vec<TodoCardSnapshot>,
    pub(in crate::ui::home) live_panes: Vec<AgentPaneInfo>,
    pub(in crate::ui::home) installed_agents: Vec<(String, String)>,
    pub(in crate::ui::home) default_agent: Option<String>,
    pub(in crate::ui::home) search_focus_nonce: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui::home) enum TodoBoardAction {
    OpenCreateModal(TodoStatus),
    OpenDetailModal(i64),
    CloseModal,
    BackToTerminal,
    CreateTodo {
        title: String,
        description: String,
        status: TodoStatus,
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
    DeleteTodo(i64),
    ClearDone,
    DispatchNewAgent {
        id: i64,
        agent_id: String,
        target: TodoDispatchTarget,
    },
    BindExistingPane {
        id: i64,
        pane: AgentPaneInfo,
        send_prompt: bool,
    },
    ResumeLink(i64),
    DetachLinkPane(i64),
    RemoveLink(i64),
    ClearReview {
        todo_id: i64,
        link_id: Option<i64>,
    },
    SendFollowup {
        todo_id: i64,
        pane_id: u64,
        text: String,
    },
    RevealPane(u64),
}

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

pub(super) fn agent_logo_path(agent_id: &str) -> Option<&'static str> {
    AGENTS
        .iter()
        .find(|def| {
            def.id.eq_ignore_ascii_case(agent_id) || def.name.eq_ignore_ascii_case(agent_id)
        })
        .and_then(|def| def.logo)
}

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

pub(in crate::ui::home) fn format_task_prompt(todo: &TodoItem) -> String {
    let title = todo.title.trim();
    let desc = todo.description.trim();
    let mut prompt = format!("请完成以下 TODO 任务（#{}）：{}", todo.id, title);
    if !desc.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(desc);
    }
    prompt
}

fn slugify_branch(todo: &TodoItem) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in todo.title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
        if slug.len() >= 24 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("todo-{}", todo.id)
    } else {
        format!("todo-{}-{slug}", todo.id)
    }
}

impl KumuApp {
    /// Reconcile persisted `todo_agent_links.pane_id` values against the live
    /// panes actually present in `tabs_by_project` after startup/restore.
    pub(super) fn reconcile_todo_panes(&mut self, cx: &mut Context<Self>) {
        let live_panes: Vec<u64> = self
            .tabs_by_project
            .values()
            .flat_map(|tabs| tabs.panes().into_iter().map(|p| p.0))
            .collect();
        self.workspace
            .update(cx, |ws, cx| ws.reconcile_todo_panes(&live_panes, cx));
    }

    /// Refresh cached installed-agent and default-agent lists off the hot
    /// render path (called on startup and when opening the TODO board).
    pub(super) fn refresh_todo_agent_cache(&mut self) {
        self.installed_agents_cache = installed_agents_list();
        self.todo_default_agent_cache = resolve_default_agent();
    }

    /// Open the Jira-style TODO Kanban board for the root repository owning `project_id`.
    pub(super) fn open_todo_board(
        &mut self,
        project_id: i64,
        open_todo_id: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let board_id = self.workspace.read(cx).board_project_id(project_id);
        self.active_todo_board = Some(board_id);
        self.pending_focus = None;
        self.refresh_todo_agent_cache();
        self.refresh_todo_summary(board_id, cx);
        if let Some(todo_id) = open_todo_id {
            self.open_todo_modal(TodoModalKind::Detail(todo_id), window, cx);
        } else {
            self.active_todo_modal = None;
            self.todo_search_focus_nonce = self.todo_search_focus_nonce.wrapping_add(1);
            self.shell_nsh
                .app_view
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        }
        cx.notify();
    }

    /// Close the TODO Kanban board and return to the last selected terminal.
    pub(super) fn close_todo_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active_todo_board = None;
        self.active_todo_modal = None;
        if let Some(project) = self.selected {
            if !self.tabs_by_project.contains_key(&project) {
                self.ensure_project_layout(project, cx);
            }
            self.show_selected_tab(project, cx);
            self.focus_terminal(window, cx);
        } else if let Some(first) = self.workspace.read(cx).projects().first().map(|p| p.id) {
            self.select_project(first, cx);
            self.focus_terminal(window, cx);
        }
        cx.notify();
    }

    /// Toggle between the current project's TODO board and its terminal (`⌘⇧T`).
    pub(super) fn toggle_todo_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_todo_board.is_some() {
            self.close_todo_board(window, cx);
        } else if let Some(project) = self
            .selected
            .or_else(|| self.workspace.read(cx).projects().first().map(|p| p.id))
        {
            self.open_todo_board(project, None, window, cx);
        }
    }

    /// Recompute the cached sidebar summary for one root board.
    pub(super) fn refresh_todo_summary(&mut self, board_id: i64, cx: &Context<Self>) {
        let todos = self.workspace.read(cx).todos_for_project(board_id);
        let open_count = todos
            .iter()
            .filter(|t| t.status != TodoStatus::Done)
            .count();
        let needs_attention = todos.iter().any(|t| {
            t.status != TodoStatus::Done
                && (t.awaiting_review
                    || t.links.iter().any(|l| {
                        l.pane_id.is_some_and(|p| {
                            matches!(
                                self.agent_status
                                    .get(&crate::app::layout::PaneId(p))
                                    .and_then(|e| e.state),
                                Some(AgentState::Waiting | AgentState::Failed)
                            )
                        })
                    }))
        });
        self.todo_summaries.insert(
            board_id,
            TodoBoardSummary {
                open_count,
                needs_attention,
            },
        );
    }

    /// Recompute cached summaries for all root repositories (used at startup / project list change).
    pub(super) fn refresh_all_todo_summaries(&mut self, cx: &Context<Self>) {
        let roots: Vec<i64> = {
            let ws = self.workspace.read(cx);
            let projects = ws.projects();
            projects
                .iter()
                .filter(|p| {
                    p.parent_id.is_none_or(|parent| {
                        parent == p.id || !projects.iter().any(|c| c.id == parent)
                    })
                })
                .map(|p| p.id)
                .collect()
        };
        self.todo_summaries.retain(|id, _| roots.contains(id));
        for root in roots {
            self.refresh_todo_summary(root, cx);
        }
    }

    /// Build the live snapshot for the currently open TODO Kanban board.
    pub(super) fn build_todo_board_snapshot(
        &self,
        board_id: i64,
        cx: &Context<Self>,
    ) -> TodoBoardSnapshot {
        let ws = self.workspace.read(cx);
        let board_name = ws
            .project(board_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| format!("项目 #{board_id}"));
        let member_ids = ws.board_member_project_ids(board_id);
        let launch_project_id = self
            .selected
            .filter(|id| member_ids.contains(id))
            .unwrap_or(board_id);

        let project_names: HashMap<i64, String> = member_ids
            .iter()
            .filter_map(|id| {
                let p = ws.project(*id)?;
                let label = if *id == board_id {
                    self.project_branches
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| p.name.clone())
                } else {
                    p.name.clone()
                };
                Some((*id, label))
            })
            .collect();

        let live_panes = self.plugin_agent_panes(board_id, cx);
        let pane_map: HashMap<u64, &AgentPaneInfo> =
            live_panes.iter().map(|p| (p.pane, p)).collect();

        let todos: Vec<TodoCardSnapshot> = ws
            .todos_for_project(board_id)
            .into_iter()
            .map(|item| {
                let links: Vec<TodoLinkSnapshot> = item
                    .links
                    .iter()
                    .map(|l| {
                        let live_pane_id = l.pane_id.filter(|p| self.control_owns(PaneId(*p)));
                        let live = live_pane_id.and_then(|p| pane_map.get(&p).copied());
                        let state = live.and_then(|p| p.state);
                        let agent_name = l
                            .agent_id
                            .as_deref()
                            .map(short_agent_name)
                            .unwrap_or_else(|| "Shell 终端".to_string());
                        let logo = l.agent_id.as_deref().and_then(agent_logo_path);
                        let can_resume = live_pane_id.is_none()
                            && l.agent_id.as_deref().is_some_and(supports_resume)
                            && l.agent_session_id.as_deref().is_some_and(session_is_valid);
                        let worktree_name = l
                            .project_id
                            .and_then(|pid| project_names.get(&pid).cloned())
                            .or_else(|| live.map(|p| p.project_name.clone()));
                        let label = if !l.label.is_empty() {
                            l.label.clone()
                        } else if let Some(p) = live {
                            p.title.clone()
                        } else {
                            String::new()
                        };
                        TodoLinkSnapshot {
                            id: l.id,
                            agent_id: l.agent_id.clone(),
                            agent_name,
                            logo,
                            session_id: l.agent_session_id.clone(),
                            pane_id: live_pane_id,
                            project_id: l.project_id,
                            worktree_name,
                            label,
                            state,
                            awaiting_review: live_pane_id.is_some() && l.awaiting_review,
                            can_resume,
                        }
                    })
                    .collect();

                let needs_attention = item.status != TodoStatus::Done
                    && (item.awaiting_review
                        || links.iter().any(|l| {
                            l.pane_id.is_some()
                                && (l.awaiting_review
                                    || matches!(
                                        l.state,
                                        Some(AgentState::Waiting | AgentState::Failed)
                                    ))
                        }));

                TodoCardSnapshot {
                    id: item.id,
                    project_id: item.project_id,
                    title: item.title,
                    description: item.description,
                    status: item.status,
                    awaiting_review: item.awaiting_review,
                    needs_attention,
                    links,
                    created_at: item.created_at,
                    updated_at: item.updated_at,
                    completed_at: item.completed_at,
                }
            })
            .collect();

        TodoBoardSnapshot {
            board_id,
            board_name,
            launch_project_id,
            todos,
            live_panes,
            installed_agents: self.installed_agents_cache.clone(),
            default_agent: self.todo_default_agent_cache.clone(),
            search_focus_nonce: self.todo_search_focus_nonce,
        }
    }

    pub(super) fn open_todo_modal(
        &mut self,
        kind: TodoModalKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(board_id) = self.active_todo_board else {
            return;
        };
        let snap = self.build_todo_board_snapshot(board_id, cx);
        let modal = ActiveTodoModal::open(
            kind,
            snap,
            |app: &mut KumuApp, action, window, cx| {
                app.handle_todo_board_action(action, window, cx);
            },
            window,
            cx,
        );
        modal.app_view.read(cx).focus_handle(cx).focus(window, cx);
        self.active_todo_modal = Some(modal);
        cx.notify();
    }

    pub(super) fn sync_active_todo_modal(&self, cx: &Context<Self>) {
        if let (Some(board_id), Some(modal)) =
            (self.active_todo_board, self.active_todo_modal.as_ref())
        {
            let snap = self.build_todo_board_snapshot(board_id, cx);
            modal.sync_board(snap);
        }
    }

    pub(super) fn handle_todo_board_action(
        &mut self,
        action: TodoBoardAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(board_id) = self.active_todo_board else {
            return;
        };
        match action {
            TodoBoardAction::OpenCreateModal(status) => {
                self.open_todo_modal(TodoModalKind::Create(status), window, cx);
            }
            TodoBoardAction::OpenDetailModal(todo_id) => {
                self.open_todo_modal(TodoModalKind::Detail(todo_id), window, cx);
            }
            TodoBoardAction::CloseModal => {
                self.active_todo_modal = None;
                self.focus_terminal(window, cx);
                cx.notify();
            }
            TodoBoardAction::BackToTerminal => {
                self.close_todo_board(window, cx);
            }
            TodoBoardAction::CreateTodo {
                title,
                description,
                status,
                dispatch_default,
            } => {
                let title = title.trim();
                if title.is_empty() {
                    return;
                }
                let created = self.workspace.update(cx, |ws, cx| {
                    ws.add_todo(
                        board_id,
                        title,
                        description.trim(),
                        status,
                        None,
                        None,
                        None,
                        cx,
                    )
                });
                self.active_todo_modal = None;
                if let Some(item) = created
                    && dispatch_default
                {
                    if let Some(agent_id) = self
                        .todo_default_agent_cache
                        .clone()
                        .or_else(resolve_default_agent)
                    {
                        self.dispatch_todo_to_new_agent(
                            board_id,
                            item.id,
                            &agent_id,
                            TodoDispatchTarget::NewTab,
                            window,
                            cx,
                        );
                    } else {
                        self.toast_overlay.push(ToastItem::error(
                            "todo-dispatch",
                            "未检测到已安装的编码 Agent，已仅创建任务",
                        ));
                        self.focus_terminal(window, cx);
                    }
                } else {
                    self.focus_terminal(window, cx);
                }
                cx.notify();
            }
            TodoBoardAction::SetStatus { id, status } => {
                self.workspace.update(cx, |ws, cx| {
                    ws.update_todo(
                        id,
                        &TodoPatch {
                            status: Some(status),
                            ..Default::default()
                        },
                        cx,
                    );
                });
            }
            TodoBoardAction::UpdateTitle { id, title } => {
                let trimmed = title.trim().to_string();
                if !trimmed.is_empty() {
                    self.workspace.update(cx, |ws, cx| {
                        ws.update_todo(
                            id,
                            &TodoPatch {
                                title: Some(trimmed),
                                ..Default::default()
                            },
                            cx,
                        );
                    });
                }
            }
            TodoBoardAction::UpdateDescription { id, description } => {
                self.workspace.update(cx, |ws, cx| {
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
            TodoBoardAction::DeleteTodo(id) => {
                self.workspace.update(cx, |ws, cx| {
                    ws.delete_todo(id, cx);
                });
                if matches!(
                    self.active_todo_modal.as_ref().map(|m| m.kind),
                    Some(TodoModalKind::Detail(open_id)) if open_id == id
                ) {
                    self.active_todo_modal = None;
                    self.focus_terminal(window, cx);
                }
                cx.notify();
            }
            TodoBoardAction::ClearDone => {
                self.workspace.update(cx, |ws, cx| {
                    ws.clear_done_todos(board_id, cx);
                });
            }
            TodoBoardAction::DispatchNewAgent {
                id,
                agent_id,
                target,
            } => {
                let _ = crate::db::set_setting(SETTING_TODO_DEFAULT_AGENT, &agent_id);
                self.todo_default_agent_cache = Some(agent_id.clone());
                self.dispatch_todo_to_new_agent(board_id, id, &agent_id, target, window, cx);
            }
            TodoBoardAction::BindExistingPane {
                id,
                pane,
                send_prompt,
            } => {
                let session_id = match self.launches.get(&crate::app::layout::PaneId(pane.pane)) {
                    Some(PaneLaunch::Agent { session, .. }) => session.clone(),
                    _ => None,
                };
                self.workspace.update(cx, |ws, cx| {
                    ws.add_todo_link(
                        id,
                        Some(&pane.agent),
                        session_id.as_deref(),
                        Some(pane.pane),
                        Some(pane.project_id),
                        &pane.title,
                        cx,
                    );
                    ws.update_todo(
                        id,
                        &TodoPatch {
                            status: Some(TodoStatus::InProgress),
                            ..Default::default()
                        },
                        cx,
                    );
                });
                if send_prompt && let Some(item) = self.workspace.read(cx).todo(id) {
                    let prompt = format_task_prompt(&item);
                    let _ = self.send_text_to_any_pane(pane.pane, &prompt, cx);
                }
            }
            TodoBoardAction::ResumeLink(link_id) => {
                self.resume_todo_link(link_id, window, cx);
            }
            TodoBoardAction::DetachLinkPane(link_id) => {
                self.workspace.update(cx, |ws, cx| {
                    ws.detach_todo_link_pane(link_id, cx);
                });
            }
            TodoBoardAction::RemoveLink(link_id) => {
                self.workspace.update(cx, |ws, cx| {
                    ws.remove_todo_link(link_id, cx);
                });
            }
            TodoBoardAction::ClearReview { todo_id, link_id } => {
                self.workspace.update(cx, |ws, cx| {
                    if let Some(link_id) = link_id {
                        ws.clear_todo_link_review(link_id, cx);
                    } else {
                        ws.update_todo(
                            todo_id,
                            &TodoPatch {
                                awaiting_review: Some(false),
                                ..Default::default()
                            },
                            cx,
                        );
                    }
                });
            }
            TodoBoardAction::SendFollowup {
                todo_id,
                pane_id,
                text,
            } => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    let _ = self.send_text_to_any_pane(pane_id, trimmed, cx);
                    self.workspace.update(cx, |ws, cx| {
                        ws.clear_todo_pane_review(todo_id, pane_id, cx);
                        ws.update_todo(
                            todo_id,
                            &TodoPatch {
                                status: Some(TodoStatus::InProgress),
                                ..Default::default()
                            },
                            cx,
                        );
                    });
                }
            }
            TodoBoardAction::RevealPane(pane_id) => {
                self.active_todo_board = None;
                self.active_todo_modal = None;
                self.reveal_agent(crate::app::layout::PaneId(pane_id), window, cx);
            }
        }
    }

    fn dispatch_todo_to_new_agent(
        &mut self,
        board_id: i64,
        todo_id: i64,
        agent_id: &str,
        target: TodoDispatchTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.workspace.read(cx).todo(todo_id) else {
            return;
        };
        let member_ids = self.workspace.read(cx).board_member_project_ids(board_id);
        let launch_project_id = self
            .selected
            .filter(|id| member_ids.contains(id))
            .unwrap_or(board_id);
        let tab_name: String = item.title.chars().take(24).collect();
        let mode = match target {
            TodoDispatchTarget::SplitRight => DispatchMode::SplitRight,
            TodoDispatchTarget::NewTab => DispatchMode::NewTab { tab_name },
            TodoDispatchTarget::NewWorktree => DispatchMode::NewWorktree {
                branch: slugify_branch(&item),
                tab_name,
            },
        };
        let prompt = format_task_prompt(&item);
        match self.dispatch_agent_for_todo(launch_project_id, agent_id, mode, prompt, cx) {
            Ok(pane_id) => {
                let owner_project = self
                    .pane_project(PaneId(pane_id), cx)
                    .unwrap_or(launch_project_id);
                self.workspace.update(cx, |ws, cx| {
                    ws.add_todo_link(
                        todo_id,
                        Some(agent_id),
                        None,
                        Some(pane_id),
                        Some(owner_project),
                        &item.title,
                        cx,
                    );
                    ws.update_todo(
                        todo_id,
                        &TodoPatch {
                            status: Some(TodoStatus::InProgress),
                            ..Default::default()
                        },
                        cx,
                    );
                });
                self.active_todo_board = None;
                self.active_todo_modal = None;
                self.focus_terminal(window, cx);
                cx.notify();
            }
            Err(message) => {
                self.toast_overlay
                    .push(ToastItem::error("todo-dispatch", message));
                cx.notify();
            }
        }
    }

    /// Resume a historical Agent session stored on `link_id` in its original project/worktree.
    pub(super) fn resume_todo_link(
        &mut self,
        link_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(link) = self.workspace.read(cx).todo_link(link_id) else {
            return;
        };
        let Some(agent_id) = link.agent_id.as_deref() else {
            return;
        };
        let Some(session_id) = link.agent_session_id.as_deref() else {
            return;
        };
        if !supports_resume(agent_id) || !session_is_valid(session_id) {
            return;
        }
        if !crate::agent::installed(agent_id) {
            return;
        }
        let target_project = {
            let ws = self.workspace.read(cx);
            let board_id = ws
                .todo(link.todo_id)
                .map(|t| t.project_id)
                .unwrap_or_else(|| self.active_todo_board.unwrap_or(0));
            link.project_id
                .filter(|pid| ws.project(*pid).is_some())
                .unwrap_or(board_id)
        };
        if self.workspace.read(cx).project(target_project).is_none() {
            return;
        }
        let tab_name = if !link.label.is_empty() {
            link.label.chars().take(24).collect()
        } else {
            short_agent_name(agent_id)
        };
        let launch = PaneLaunch::Agent {
            agent: agent_id.to_string(),
            session: Some(session_id.to_string()),
        };
        let pane = self.open_agent_tab(target_project, tab_name, launch, cx);
        self.workspace.update(cx, |ws, cx| {
            ws.rebind_todo_link_pane(link_id, pane.0, Some(target_project), cx);
            ws.update_todo(
                link.todo_id,
                &TodoPatch {
                    status: Some(TodoStatus::InProgress),
                    ..Default::default()
                },
                cx,
            );
        });
        self.active_todo_board = None;
        self.active_todo_modal = None;
        self.focus_terminal(window, cx);
        cx.notify();
    }
}
