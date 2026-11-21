//! Right-hand TODO Kanban board plugin, linked bidirectionally with Kumu's
//! Agent panes.
//!
//! Designed around three UX principles:
//! 1. Never block the live terminals — task creation expands inline and detail
//!    editing opens inside the right-hand panel (`← 返回看板`).
//! 2. One-click high-frequency flows — clicking a linked in-progress card jumps
//!    directly to its Agent pane; Todo cards remember the last-used Agent for
//!    1-click split dispatch (`▶ 交给 Claude | ▾`).
//! 3. Calm visual hierarchy — borderless status pills and a 3px left accent bar
//!    only for cards needing human attention (`等待输入`, `待确认`, `执行失败`).

mod card;
mod detail;

use std::collections::HashMap;

use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use self::detail::DetailEditor;
use super::{AgentPaneInfo, DispatchMode, Plugin, PluginHost, PluginProject};
use crate::agent::{AGENTS, AgentState};
use crate::app::WorkspaceEvent;
use crate::db::{SETTING_TODO_DEFAULT_AGENT, TodoItem, TodoPatch, TodoStatus};

/// Drag payload for moving a card across lanes.
#[derive(Clone, Debug)]
pub(super) struct DraggedTodo {
    pub(super) id: i64,
    pub(super) title: String,
}

/// Compact floating chip painted while dragging a TODO card.
struct DragPreview {
    title: String,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .max_w(px(220.))
            .px_2p5()
            .py_1p5()
            .rounded(theme.radius)
            .bg(theme.background)
            .border_1()
            .border_color(theme.link)
            .shadow_sm()
            .text_xs()
            .font_medium()
            .text_color(theme.foreground)
            .truncate()
            .child(self.title.clone())
    }
}

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

pub struct TodoPanel {
    project: Option<PluginProject>,
    host: PluginHost,
    quick_input: Entity<InputState>,
    composer_open: bool,
    composer_title: Entity<InputState>,
    composer_desc: Entity<TextareaState>,
    in_progress_open: bool,
    todo_open: bool,
    done_open: bool,
    detail: Option<DetailEditor>,
    default_agent: Option<String>,
    /// Transient inline notice (`(message, is_error)`).
    notice: Option<(String, bool)>,
    _subscriptions: Vec<Subscription>,
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
        let quick_input = cx.new(|cx| InputState::new(window, cx).placeholder("添加待办…"));
        let composer_title = cx.new(|cx| InputState::new(window, cx).placeholder("任务标题"));
        let composer_desc = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("详细说明 / Agent Prompt（可选）…")
                .auto_grow(2, 5)
        });

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &quick_input,
            window,
            |panel, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { secondary, .. } = event {
                    panel.submit_quick_add(*secondary, window, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &composer_title,
            window,
            |panel, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { secondary, .. } = event {
                    panel.submit_composer(*secondary, window, cx);
                }
            },
        ));
        if let Some(workspace) = host.workspace() {
            subscriptions.push(cx.subscribe(&workspace, |panel, _, event, cx| match event {
                WorkspaceEvent::TodosChanged(board_id) => {
                    if panel.board_id(cx) == Some(*board_id) {
                        panel.sync_detail_alive(cx);
                        cx.notify();
                    }
                }
                WorkspaceEvent::Changed
                | WorkspaceEvent::AgentStatus { .. }
                | WorkspaceEvent::AgentTitle { .. }
                | WorkspaceEvent::AgentExited { .. }
                | WorkspaceEvent::PaneExited { .. }
                | WorkspaceEvent::Session { .. } => {
                    panel.sync_detail_alive(cx);
                    cx.notify();
                }
                _ => {}
            }));
        }

        Self {
            project,
            host,
            quick_input,
            composer_open: false,
            composer_title,
            composer_desc,
            in_progress_open: true,
            todo_open: true,
            done_open: false,
            detail: None,
            default_agent: resolve_default_agent(),
            notice: None,
            _subscriptions: subscriptions,
        }
    }

    fn set_project(&mut self, project: Option<PluginProject>, cx: &mut Context<Self>) {
        if self.project == project {
            return;
        }
        let old_board = self.board_id(cx);
        self.project = project;
        let new_board = self.board_id(cx);
        if old_board != new_board {
            self.detail = None;
            self.notice = None;
        }
        cx.notify();
    }
}

impl TodoPanel {
    /// Root repository board id for the currently selected project/worktree.
    pub(super) fn board_id(&self, cx: &App) -> Option<i64> {
        let project = self.project.as_ref()?;
        let workspace = self.host.workspace()?;
        Some(workspace.read(cx).board_project_id(project.id))
    }

    /// Name of the root repository board, and whether the current selection is
    /// a worktree sharing its parent repository's board.
    fn board_scope(&self, cx: &App) -> Option<(String, bool)> {
        let project = self.project.as_ref()?;
        let workspace = self.host.workspace()?;
        let ws = workspace.read(cx);
        let board_id = ws.board_project_id(project.id);
        let board_name = ws
            .project(board_id)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        Some((board_name, board_id != project.id))
    }

    fn todos(&self, cx: &App) -> Vec<TodoItem> {
        let Some(project) = self.project.as_ref() else {
            return Vec::new();
        };
        let Some(workspace) = self.host.workspace() else {
            return Vec::new();
        };
        workspace.read(cx).todos_for_project(project.id)
    }

    pub(super) fn live_agent_panes(&self, cx: &App) -> Vec<AgentPaneInfo> {
        let Some(board_id) = self.board_id(cx) else {
            return Vec::new();
        };
        self.host.agent_panes(board_id, cx)
    }

    fn remember_default_agent(&mut self, agent_id: &str) {
        if self.default_agent.as_deref() == Some(agent_id) {
            return;
        }
        self.default_agent = Some(agent_id.to_string());
        let _ = crate::db::set_setting(SETTING_TODO_DEFAULT_AGENT, agent_id);
    }

    fn active_default_agent(&self) -> Option<String> {
        self.default_agent
            .as_deref()
            .filter(|id| crate::agent::installed(id))
            .map(str::to_string)
            .or_else(resolve_default_agent)
    }

    pub(super) fn set_notice(&mut self, message: impl Into<String>, is_error: bool) {
        self.notice = Some((message.into(), is_error));
    }

    fn submit_quick_add(
        &mut self,
        dispatch_immediately: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = self.quick_input.read(cx).value().trim().to_string();
        if title.is_empty() {
            return;
        }
        let Some(item) = self.create_todo(&title, "", cx) else {
            return;
        };
        self.quick_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        if dispatch_immediately {
            self.dispatch_default_agent(&item, cx);
        }
        cx.notify();
    }

    fn toggle_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer_open = !self.composer_open;
        if self.composer_open {
            let draft = self.quick_input.read(cx).value().to_string();
            if !draft.trim().is_empty() {
                self.composer_title
                    .update(cx, |state, cx| state.set_value(draft, window, cx));
                self.quick_input
                    .update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.composer_title
                .update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    fn submit_composer(
        &mut self,
        dispatch_immediately: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = self.composer_title.read(cx).value().trim().to_string();
        if title.is_empty() {
            return;
        }
        let desc = self.composer_desc.read(cx).value().trim().to_string();
        let Some(item) = self.create_todo(&title, &desc, cx) else {
            return;
        };
        self.composer_title
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.composer_desc
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.composer_open = false;
        if dispatch_immediately {
            self.dispatch_default_agent(&item, cx);
        }
        cx.notify();
    }

    fn create_todo(
        &mut self,
        title: &str,
        description: &str,
        cx: &mut Context<Self>,
    ) -> Option<TodoItem> {
        let project = self.project.as_ref()?;
        let workspace = self.host.workspace()?;
        self.notice = None;
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

    pub(super) fn set_status(&mut self, id: i64, status: TodoStatus, cx: &mut Context<Self>) {
        let Some(workspace) = self.host.workspace() else {
            return;
        };
        self.notice = None;
        let patch = TodoPatch {
            status: Some(status),
            awaiting_review: (status != TodoStatus::InProgress).then_some(false),
            ..Default::default()
        };
        workspace.update(cx, |ws, cx| {
            ws.update_todo(id, &patch, cx);
        });
        cx.notify();
    }

    pub(super) fn delete_item(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(workspace) = self.host.workspace() else {
            return;
        };
        workspace.update(cx, |ws, cx| {
            ws.delete_todo(id, cx);
        });
        if self.detail.as_ref().is_some_and(|d| d.todo_id == id) {
            self.detail = None;
        }
        cx.notify();
    }

    fn clear_done(&mut self, cx: &mut Context<Self>) {
        let (Some(project), Some(workspace)) = (self.project.as_ref(), self.host.workspace())
        else {
            return;
        };
        workspace.update(cx, |ws, cx| {
            ws.clear_done_todos(project.id, cx);
        });
        cx.notify();
    }

    pub(super) fn unbind_agent(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(workspace) = self.host.workspace() else {
            return;
        };
        let patch = TodoPatch {
            pane_id: Some(None),
            awaiting_review: Some(false),
            ..Default::default()
        };
        workspace.update(cx, |ws, cx| {
            ws.update_todo(id, &patch, cx);
        });
        cx.notify();
    }

    pub(super) fn dispatch_default_agent(&mut self, item: &TodoItem, cx: &mut Context<Self>) {
        let Some(agent_id) = self.active_default_agent() else {
            self.set_notice("未检测到已安装的编码 Agent", true);
            cx.notify();
            return;
        };
        self.dispatch_new_agent(item, &agent_id, false, cx);
    }

    pub(super) fn dispatch_new_agent(
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
                self.in_progress_open = true;
                self.notice = None;
            }
            Err(error) => {
                self.set_notice(error, true);
            }
        }
        cx.notify();
    }

    pub(super) fn bind_existing_pane(
        &mut self,
        item: &TodoItem,
        pane: &AgentPaneInfo,
        send_prompt: bool,
        cx: &mut Context<Self>,
    ) {
        if send_prompt {
            let prompt = format_task_prompt(item);
            if let Err(error) = self.host.send_to_any_pane(pane.pane, &prompt, cx) {
                self.set_notice(error, true);
                cx.notify();
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
        self.in_progress_open = true;
        self.notice = None;
        cx.notify();
    }

    pub(super) fn reveal_pane(&self, pane: u64, window: &mut Window, cx: &mut App) {
        self.host.reveal_pane(pane, window, cx);
    }

    pub(super) fn open_detail(
        &mut self,
        item: &TodoItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notice = None;
        self.detail = Some(DetailEditor::new(item, window, cx));
        cx.notify();
    }

    pub(super) fn close_detail(&mut self, cx: &mut Context<Self>) {
        self.detail = None;
        self.notice = None;
        cx.notify();
    }

    fn sync_detail_alive(&mut self, cx: &App) {
        let Some(todo_id) = self.detail.as_ref().map(|d| d.todo_id) else {
            return;
        };
        let exists = self
            .host
            .workspace()
            .and_then(|ws| ws.read(cx).todo(todo_id))
            .is_some();
        if !exists {
            self.detail = None;
        }
    }

    /// Build the shared dispatch/bind menu items for a TODO card or detail view.
    pub(super) fn populate_dispatch_menu(
        menu: PopupMenu,
        window: &mut Window,
        menu_cx: &mut Context<PopupMenu>,
        panel: WeakEntity<Self>,
        item: TodoItem,
        live_panes: Vec<AgentPaneInfo>,
        default_agent: Option<String>,
    ) -> PopupMenu {
        let installed: Vec<_> = AGENTS
            .iter()
            .filter(|def| crate::agent::installed(def.id))
            .collect();
        let mut menu = menu;
        if installed.is_empty() {
            menu = menu.item(PopupMenuItem::new("未检测到已安装的 Agent").disabled(true));
        } else {
            for def in &installed {
                let agent_id = def.id.to_string();
                let is_default = default_agent.as_deref() == Some(def.id);
                let label = if is_default {
                    format!("分屏启动 {}（默认）", def.name)
                } else {
                    format!("分屏启动 {}", def.name)
                };
                let panel = panel.clone();
                let item = item.clone();
                menu = menu.item(PopupMenuItem::new(label).icon(IconName::Bot).on_click(
                    move |_, _, cx| {
                        let _ = panel.update(cx, |this, cx| {
                            this.dispatch_new_agent(&item, &agent_id, false, cx);
                        });
                    },
                ));
            }
            let new_tab_panel = panel.clone();
            let new_tab_item = item.clone();
            menu = menu.submenu(
                "在新页签中启动",
                window,
                menu_cx,
                move |sub, _, _| {
                    let mut sub = sub;
                    for def in AGENTS.iter().filter(|def| crate::agent::installed(def.id)) {
                        let agent_id = def.id.to_string();
                        let panel = new_tab_panel.clone();
                        let item = new_tab_item.clone();
                        sub = sub.item(PopupMenuItem::new(def.name).icon(IconName::Bot).on_click(
                            move |_, _, cx| {
                                let _ = panel.update(cx, |this, cx| {
                                    this.dispatch_new_agent(&item, &agent_id, true, cx);
                                });
                            },
                        ));
                    }
                    sub
                },
            );
        }

        if !live_panes.is_empty() {
            menu = menu.separator();
            for pane in &live_panes {
                let status = pane.state.map_or("空闲", AgentState::label);
                let label = format!(
                    "发送到 窗格 {} · {}（{}）",
                    pane.ordinal,
                    short_agent_name(&pane.agent),
                    status
                );
                let panel = panel.clone();
                let item = item.clone();
                let pane = pane.clone();
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .icon(IconName::SquareTerminal)
                        .on_click(move |_, _, cx| {
                            let _ = panel.update(cx, |this, cx| {
                                this.bind_existing_pane(&item, &pane, true, cx);
                            });
                        }),
                );
            }
            let link_panel = panel.clone();
            let link_item = item.clone();
            let link_panes = live_panes;
            menu = menu.separator().submenu(
                "仅关联已有窗格（不发送）",
                window,
                menu_cx,
                move |sub, _, _| {
                    let mut sub = sub;
                    for pane in &link_panes {
                        let status = pane.state.map_or("空闲", AgentState::label);
                        let label = format!(
                            "窗格 {} · {}（{}）",
                            pane.ordinal,
                            short_agent_name(&pane.agent),
                            status
                        );
                        let panel = link_panel.clone();
                        let item = link_item.clone();
                        let pane = pane.clone();
                        sub = sub.item(
                            PopupMenuItem::new(label)
                                .icon(IconName::SquareTerminal)
                                .on_click(move |_, _, cx| {
                                    let _ = panel.update(cx, |this, cx| {
                                        this.bind_existing_pane(&item, &pane, false, cx);
                                    });
                                }),
                        );
                    }
                    sub
                },
            );
        }
        menu
    }

    fn render_quick_add(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, fg, bg) = (
            theme.sidebar_border,
            theme.muted_foreground,
            theme.foreground,
            theme.background,
        );
        let default_label = self
            .active_default_agent()
            .map(|id| short_agent_name(&id))
            .unwrap_or_else(|| "Agent".to_string());

        let mut wrap = v_flex()
            .flex_none()
            .px_2p5()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(border);

        if let Some((board_name, shared)) = self.board_scope(cx)
            && shared
            && !board_name.is_empty()
        {
            wrap = wrap.child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("共享自 {board_name}")),
            );
        }

        if self.composer_open {
            wrap = wrap.child(
                v_flex()
                    .p_2()
                    .gap_1p5()
                    .rounded(theme.radius)
                    .bg(bg)
                    .border_1()
                    .border_color(theme.border)
                    .child(Input::new(&self.composer_title).small())
                    .child(
                        div()
                            .rounded(theme.radius)
                            .bg(theme.sidebar.opacity(0.65))
                            .px_1p5()
                            .py_1()
                            .child(
                                Textarea::new(&self.composer_desc)
                                    .appearance(false)
                                    .bordered(false),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(
                                Button::new("todo-composer-add")
                                    .small()
                                    .ghost()
                                    .label("仅添加")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_composer(false, window, cx);
                                    })),
                            )
                            .child(
                                Button::new("todo-composer-run")
                                    .small()
                                    .primary()
                                    .icon(IconName::Play)
                                    .label(format!("添加并交给 {default_label}"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_composer(true, window, cx);
                                    })),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("todo-composer-cancel")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Close)
                                    .tooltip("收起")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.composer_open = false;
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        } else {
            wrap = wrap.child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.quick_input).small()),
                    )
                    .child(
                        Button::new("todo-expand-composer")
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .color(bg)
                                    .foreground(fg)
                                    .hover(theme.accent.opacity(0.6)),
                            )
                            .compact()
                            .border_1()
                            .border_color(theme.border)
                            .icon(IconName::Plus)
                            .tooltip("展开多行说明（输入框支持 ↵ 添加 / ⌘↵ 添加并派发）")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_composer(window, cx);
                            })),
                    ),
            );
        }

        if let Some((message, is_error)) = &self.notice {
            let color = if *is_error {
                theme.danger
            } else {
                theme.muted_foreground
            };
            wrap = wrap.child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1p5()
                    .rounded(theme.radius)
                    .bg(color.opacity(0.12))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(color)
                            .child(message.clone()),
                    )
                    .child(
                        div()
                            .id("todo-dismiss-notice")
                            .cursor_pointer()
                            .text_color(color)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.notice = None;
                                cx.notify();
                            }))
                            .child(Icon::new(IconName::Close).with_size(px(11.))),
                    ),
            );
        }

        wrap.into_any_element()
    }

    fn render_lane_header(
        &self,
        status: TodoStatus,
        count: usize,
        expanded: bool,
        on_toggle: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, hover) = (theme.muted_foreground, theme.sidebar_accent.opacity(0.5));
        let id = match status {
            TodoStatus::InProgress => "todo-lane-hdr-in-progress",
            TodoStatus::Todo => "todo-lane-hdr-todo",
            TodoStatus::Done => "todo-lane-hdr-done",
        };
        let mut header = h_flex()
            .id(id)
            .w_full()
            .h(px(26.))
            .px_1()
            .gap_1p5()
            .rounded(theme.radius)
            .items_center()
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| on_toggle(this, cx)))
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(muted)
                    .child(status.label()),
            )
            .child(div().flex_1());

        if status == TodoStatus::Done && count > 0 {
            let clear_panel = cx.entity().downgrade();
            header = header.child(
                div()
                    .id("todo-clear-done")
                    .px_1p5()
                    .py_0p5()
                    .rounded(theme.radius)
                    .text_xs()
                    .text_color(muted)
                    .hover(move |style| style.bg(hover))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        let _ = clear_panel.update(cx, |this, cx| this.clear_done(cx));
                    })
                    .child("清空"),
            );
        }

        header
            .child(
                div()
                    .text_xs()
                    .font_medium()
                    .text_color(muted)
                    .child(count.to_string()),
            )
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .with_size(px(12.))
                .text_color(muted),
            )
            .into_any_element()
    }
}

impl Render for TodoPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;

        if self.project.is_none() {
            return v_flex()
                .size_full()
                .px_3()
                .py_4()
                .text_xs()
                .text_color(muted)
                .child("没有选中的项目")
                .into_any_element();
        }

        let live_panes = self.live_agent_panes(cx);
        let pane_map: HashMap<u64, AgentPaneInfo> = live_panes
            .iter()
            .cloned()
            .map(|info| (info.pane, info))
            .collect();

        if let Some(todo_id) = self.detail.as_ref().map(|d| d.todo_id)
            && let Some(item) = self
                .host
                .workspace()
                .and_then(|ws| ws.read(cx).todo(todo_id))
        {
            return self
                .render_detail(&item, &live_panes, &pane_map, window, cx)
                .into_any_element();
        }

        let todos = self.todos(cx);
        let mut in_progress: Vec<TodoItem> = Vec::new();
        let mut todo_items: Vec<TodoItem> = Vec::new();
        let mut done_items: Vec<TodoItem> = Vec::new();

        for item in todos {
            match item.status {
                TodoStatus::InProgress => in_progress.push(item),
                TodoStatus::Todo => todo_items.push(item),
                TodoStatus::Done => done_items.push(item),
            }
        }

        // Surface items needing human input/confirmation at the top of `进行中`.
        in_progress.sort_by_key(|item| {
            let state = item
                .pane_id
                .and_then(|pane| pane_map.get(&pane))
                .and_then(|info| info.state);
            let rank = if state == Some(AgentState::Waiting) {
                0
            } else if item.awaiting_review || state == Some(AgentState::Completed) {
                1
            } else if state == Some(AgentState::Failed) {
                2
            } else if state == Some(AgentState::Running) {
                3
            } else {
                4
            };
            (rank, item.position, std::cmp::Reverse(item.id))
        });

        let default_agent = self.active_default_agent();
        let drop_bg = theme.link.opacity(0.08);
        let drop_border = theme.link.opacity(0.45);
        let lane_divider = theme.sidebar_border;

        let make_lane = |id: &'static str,
                         target: TodoStatus,
                         header: AnyElement,
                         cards: Vec<AnyElement>,
                         expanded: bool,
                         empty_hint: &'static str,
                         cx: &Context<Self>| {
            let mut lane = v_flex()
                .id(id)
                .w_full()
                .gap_1p5()
                .p_0p5()
                .rounded(theme.radius)
                .drag_over::<DraggedTodo>(move |style, _, _, _| {
                    style.bg(drop_bg).border_1().border_color(drop_border)
                })
                .on_drop(cx.listener(move |this, dragged: &DraggedTodo, _, cx| {
                    this.set_status(dragged.id, target, cx);
                }))
                .child(header);
            if expanded {
                if cards.is_empty() {
                    lane = lane.child(
                        div()
                            .px_1p5()
                            .py_1()
                            .text_xs()
                            .text_color(muted)
                            .child(empty_hint),
                    );
                } else {
                    lane = lane.children(cards);
                }
            }
            lane
        };

        let in_progress_cards: Vec<AnyElement> = in_progress
            .iter()
            .map(|item| {
                self.render_in_progress_card(
                    item,
                    item.pane_id.and_then(|p| pane_map.get(&p)),
                    &live_panes,
                    default_agent.as_deref(),
                    cx,
                )
            })
            .collect();
        let todo_cards: Vec<AnyElement> = todo_items
            .iter()
            .map(|item| self.render_todo_card(item, &live_panes, default_agent.as_deref(), cx))
            .collect();
        let done_rows: Vec<AnyElement> = done_items
            .iter()
            .map(|item| self.render_done_row(item, cx))
            .collect();

        let in_progress_header = self.render_lane_header(
            TodoStatus::InProgress,
            in_progress.len(),
            self.in_progress_open,
            |this, cx| {
                this.in_progress_open = !this.in_progress_open;
                cx.notify();
            },
            cx,
        );
        let todo_header = self.render_lane_header(
            TodoStatus::Todo,
            todo_items.len(),
            self.todo_open,
            |this, cx| {
                this.todo_open = !this.todo_open;
                cx.notify();
            },
            cx,
        );
        let done_header = self.render_lane_header(
            TodoStatus::Done,
            done_items.len(),
            self.done_open,
            |this, cx| {
                this.done_open = !this.done_open;
                cx.notify();
            },
            cx,
        );

        let body = v_flex()
            .id("todo-lanes-scroll")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .px_2()
            .py_2()
            .gap_2()
            .child(make_lane(
                "todo-lane-in-progress",
                TodoStatus::InProgress,
                in_progress_header,
                in_progress_cards,
                self.in_progress_open,
                "暂无进行中的任务",
                cx,
            ))
            .child(div().w_full().h(px(1.)).flex_none().bg(lane_divider))
            .child(make_lane(
                "todo-lane-todo",
                TodoStatus::Todo,
                todo_header,
                todo_cards,
                self.todo_open,
                "在上方输入框添加待办",
                cx,
            ))
            .child(div().w_full().h(px(1.)).flex_none().bg(lane_divider))
            .child(make_lane(
                "todo-lane-done",
                TodoStatus::Done,
                done_header,
                done_rows,
                self.done_open,
                "还没有已完成的任务",
                cx,
            ));

        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .text_color(theme.sidebar_foreground)
            .child(self.render_quick_add(cx))
            .child(body)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{TodoItem, TodoStatus, format_task_prompt};

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
}
