use std::time::{Duration, Instant};

use gpui::*;

use super::{AgentEntry, KumuApp, agent_display_name, shell_prompt, submits_after_paste};
use crate::agent::AgentState;
use crate::agent::launch::PaneLaunch;
use crate::app::layout::{PaneId, PaneNode, ProjectTab, ProjectTabs, SplitAxis};
use crate::db::TodoStatus;
use crate::plugins::{
    AgentPaneInfo, DispatchMode, PaneTarget, PluginHost, PluginProject, PluginSpec, SendOutcome,
};
use crate::term::KeyInput;
use crate::theme::KumuTheme;

/// The plugin panel's opening width and the range its divider allows.
pub(super) const PLUGIN_WIDTH: f32 = 300.;
pub(super) const PLUGIN_MIN_WIDTH: f32 = 240.;
pub(super) const PLUGIN_MAX_WIDTH: f32 = 480.;

/// How long a newly dispatched TODO agent pane waits to draw before sending its prompt.
const DISPATCH_PROMPT_TIMEOUT: Duration = Duration::from_secs(12);
/// Brief settle window after the first drawn frame so bracketed paste does not race startup.
const DISPATCH_PROMPT_SETTLE: Duration = Duration::from_millis(400);
/// Safety upper bound before clearing deferred terminal resizes or a closed
/// plugin view if the NSH settle callback never arrives.
const PANEL_SETTLE_FALLBACK: Duration = Duration::from_millis(360);

impl KumuApp {
    /// The project a plugin panel shows, if one is selected.
    pub(super) fn plugin_project(&self, cx: &App) -> Option<PluginProject> {
        self.selected
            .and_then(|id| self.workspace.read(cx).project(id))
            .map(PluginProject::from)
    }

    /// How plugins hand text to the user's terminal. A panel names the pane it
    /// writes to, and the window resolves it only when the send happens, so a
    /// closed pane is an error rather than text aimed at a stale id.
    pub(super) fn plugin_host(&self, cx: &Context<Self>) -> PluginHost {
        let home = cx.entity().downgrade();
        let listing = home.clone();
        let agent_listing = home.clone();
        let dispatcher = home.clone();
        let any_sender = home.clone();
        let revealer = home.clone();
        let sheet_opener = home.clone();
        let sheet_closer = home.clone();
        PluginHost::with_panes(
            move |project, pane, text, cx| {
                let Some(home) = home.upgrade() else {
                    return Err("窗口已关闭".to_string());
                };
                home.update(cx, |app, cx| app.send_text_to_pane(project, pane, text, cx))
            },
            move |project, cx| {
                listing
                    .upgrade()
                    .map(|home| home.read(cx).plugin_panes(project, cx))
                    .unwrap_or_default()
            },
        )
        .with_agent_tools(
            self.workspace.clone(),
            move |board_id, cx| {
                agent_listing
                    .upgrade()
                    .map(|home| home.read(cx).plugin_agent_panes(board_id, cx))
                    .unwrap_or_default()
            },
            move |project_id, agent_id, mode, prompt, cx| {
                let Some(home) = dispatcher.upgrade() else {
                    return Err("窗口已关闭".to_string());
                };
                home.update(cx, |app, cx| {
                    app.dispatch_agent_for_todo(project_id, agent_id, mode, prompt, cx)
                })
            },
            move |pane, text, cx| {
                let Some(home) = any_sender.upgrade() else {
                    return Err("窗口已关闭".to_string());
                };
                home.update(cx, |app, cx| app.send_text_to_any_pane(pane, text, cx))
            },
            move |pane, window, cx| {
                if let Some(home) = revealer.upgrade() {
                    home.update(cx, |app, cx| app.reveal_agent(PaneId(pane), window, cx));
                }
            },
        )
        .with_sheet_tools(
            move |view, _window, cx| {
                if let Some(home) = sheet_opener.upgrade() {
                    home.update(cx, |app, cx| {
                        app.active_sheet = Some(view);
                        cx.notify();
                    });
                }
            },
            move |window, cx| {
                if let Some(home) = sheet_closer.upgrade() {
                    home.update(cx, |app, cx| {
                        app.active_sheet = None;
                        app.focus_terminal(window, cx);
                        cx.notify();
                    });
                }
            },
        )
    }

    /// Live agent panes belonging to `board_id` and its worktrees, across all tabs.
    pub(super) fn plugin_agent_panes(&self, board_id: i64, cx: &App) -> Vec<AgentPaneInfo> {
        let workspace = self.workspace.read(cx);
        let member_projects = workspace.board_member_project_ids(board_id);
        let mut out = Vec::new();
        for project_id in member_projects {
            let Some(project) = workspace.project(project_id) else {
                continue;
            };
            let Some(tabs) = self.tabs_by_project.get(&project_id) else {
                continue;
            };
            for (tab_index, tab) in tabs.tabs.iter().enumerate() {
                let tab_name = Self::tab_name(tabs, tab_index);
                for (ordinal_index, pane) in tab.layout.panes().into_iter().enumerate() {
                    let agent = self
                        .agent_status
                        .get(&pane)
                        .map(|entry| entry.agent.clone())
                        .or_else(|| match self.launches.get(&pane) {
                            Some(PaneLaunch::Agent { agent, .. }) => Some(agent.clone()),
                            _ => None,
                        });
                    let Some(agent) = agent else {
                        continue;
                    };
                    let state = self.agent_status.get(&pane).and_then(|entry| entry.state);
                    let title = self
                        .agent_status
                        .get(&pane)
                        .map(AgentEntry::display_title)
                        .or_else(|| {
                            workspace
                                .terminal_handle(pane.0)
                                .map(|term| term.label())
                                .filter(|label| !label.is_empty())
                        })
                        .or_else(|| agent_display_name(&agent))
                        .unwrap_or_else(|| agent.clone());
                    out.push(AgentPaneInfo {
                        pane: pane.0,
                        project_id,
                        project_name: project.name.clone(),
                        tab_name: tab_name.clone(),
                        ordinal: ordinal_index + 1,
                        agent,
                        title,
                        state,
                        focused: self.selected == Some(project_id) && self.focused == pane,
                    });
                }
            }
        }
        out.sort_by_key(|row| row.state.map_or(4, AgentState::priority));
        out
    }

    /// Launch a fresh Agent pane for a TODO card (either split beside the
    /// active pane or inside a new tab) and submit `prompt` once the TUI draws.
    pub(super) fn dispatch_agent_for_todo(
        &mut self,
        project_id: i64,
        agent_id: &str,
        mode: DispatchMode,
        prompt: String,
        cx: &mut Context<Self>,
    ) -> Result<u64, String> {
        let Some(agent) = crate::agent::id_for(agent_id) else {
            return Err(format!("未知的 Agent：{agent_id}"));
        };
        if !crate::agent::installed(agent) {
            return Err(format!("{agent} 尚未在本机安装"));
        }
        if self.workspace.read(cx).project(project_id).is_none() {
            return Err("项目不存在".to_string());
        }
        let launch = PaneLaunch::Agent {
            agent: agent.to_string(),
            session: None,
        };
        let added = match mode {
            DispatchMode::SplitRight => {
                if !self.tabs_by_project.contains_key(&project_id) {
                    self.ensure_project_layout(project_id, cx);
                }
                if self.selected != Some(project_id) {
                    self.select_project(project_id, cx);
                }
                let anchor = self
                    .active_tab(project_id)
                    .map(|tab| tab.focused)
                    .unwrap_or(self.focused);
                let before = self.focused;
                self.split_pane_with(anchor, SplitAxis::Horizontal, launch, cx);
                if self.focused == before {
                    return Err("无法在当前页签分屏启动 Agent".to_string());
                }
                self.focused
            }
            DispatchMode::NewTab { tab_name } => {
                self.open_agent_tab(project_id, tab_name, launch, cx)
            }
            DispatchMode::NewWorktree { branch, tab_name } => {
                let (root_id, root_path) = {
                    let ws = self.workspace.read(cx);
                    let board_id = ws.board_project_id(project_id);
                    let path = ws
                        .project(board_id)
                        .or_else(|| ws.project(project_id))
                        .map(|p| std::path::PathBuf::from(&p.path))
                        .ok_or_else(|| "无法定位根仓库目录".to_string())?;
                    (board_id, path)
                };
                let worktree = crate::git::create_worktree(&root_path, &branch)
                    .map_err(|err| format!("创建 worktree 失败：{err}"))?;
                let wt_path = worktree.path.to_string_lossy().into_owned();
                let wt_project_id = self
                    .workspace
                    .update(cx, |ws, cx| {
                        ws.add_worktree(root_id, &worktree.name, &wt_path, cx)
                    })
                    .ok_or_else(|| "将 worktree 添加到工作区失败".to_string())?;
                self.open_agent_tab(wt_project_id, tab_name, launch, cx)
            }
        };

        if !prompt.trim().is_empty() {
            let prompt_text = prompt;
            cx.spawn(async move |this, cx| {
                let deadline = Instant::now() + DISPATCH_PROMPT_TIMEOUT;
                loop {
                    let ready = this.update(cx, |app, cx| app.control_prompt_state(added, cx));
                    match ready {
                        Ok(Ok(true)) => break,
                        Ok(Ok(false)) => {}
                        Ok(Err(_)) | Err(_) => return,
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(150))
                        .await;
                }
                cx.background_executor().timer(DISPATCH_PROMPT_SETTLE).await;
                let _ = this.update(cx, |app, cx| {
                    let _ = app.send_text_to_any_pane(added.0, &prompt_text, cx);
                });
            })
            .detach();
        }
        Ok(added.0)
    }

    /// Open a new tab in `project_id` running `launch` directly.
    fn open_agent_tab(
        &mut self,
        project_id: i64,
        tab_name: String,
        launch: PaneLaunch,
        cx: &mut Context<Self>,
    ) -> PaneId {
        let pane = PaneId(self.next_pane);
        self.next_pane += 1;
        let name: String = tab_name.trim().chars().take(24).collect();
        let tabs = self
            .tabs_by_project
            .entry(project_id)
            .or_insert_with(|| ProjectTabs {
                tabs: Vec::new(),
                selected: 0,
            });
        let index = if tabs.tabs.is_empty() {
            0
        } else {
            (tabs.selected + 1).min(tabs.tabs.len())
        };
        tabs.tabs
            .insert(index, ProjectTab::new(name, PaneNode::leaf(pane), pane));
        tabs.selected = index;
        if let PaneLaunch::Agent { agent, .. } = &launch {
            self.agent_status
                .insert(pane, AgentEntry::idle(agent.clone()));
            self.launches.insert(pane, launch);
        }
        if self.selected != Some(project_id) {
            if let Some(previous) = self.selected {
                self.hide_project_panes(previous, cx);
            }
            self.selected = Some(project_id);
        }
        self.focused = pane;
        self.pending_focus = Some(pane);
        self.pending_close_tab = None;
        self.show_selected_tab(project_id, cx);
        self.sync_pane_chrome(cx);
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
        pane
    }

    /// Paste `text` into any live pane (across tabs or worktrees).
    pub(super) fn send_text_to_any_pane(
        &self,
        pane: u64,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Result<SendOutcome, String> {
        let workspace = self.workspace.read(cx);
        let Some(term) = workspace.terminal_for_pane(pane) else {
            return Err("目标窗格没有运行中的终端".to_string());
        };
        let snapshot = term.snapshot();
        if snapshot.exited || snapshot.spawn_error.is_some() {
            return Err("目标窗格已退出".to_string());
        }
        let shell = workspace.pane_is_shell(pane);
        if shell == Some(true) {
            term.paste(shell_prompt(text));
        } else {
            term.paste(text);
        }
        if !submits_after_paste(shell) {
            return Ok(SendOutcome::Pasted);
        }
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            term.key(KeyInput {
                key: "enter".into(),
                key_char: Some("\r".into()),
                shift: false,
                control: false,
                alt: false,
            });
        })
        .detach();
        Ok(SendOutcome::Submitted)
    }

    /// Semantic attention color for the title-bar TODO icon, if the selected
    /// project's board has a Waiting agent (warning) or awaiting-review task (success).
    fn todo_attention_color(&self, cx: &Context<Self>) -> Option<Hsla> {
        let project = self.selected?;
        let todos = self.workspace.read(cx).todos_for_project(project);
        let colors = &KumuTheme::global(cx).colors;
        let mut has_review = false;
        for todo in todos {
            if todo.status != TodoStatus::InProgress {
                continue;
            }
            if let Some(pane) = todo.pane_id
                && self
                    .agent_status
                    .get(&PaneId(pane))
                    .and_then(|entry| entry.state)
                    == Some(AgentState::Waiting)
            {
                return Some(colors.warning);
            }
            if todo.awaiting_review {
                has_review = true;
            }
        }
        has_review.then_some(colors.success)
    }

    /// The panes a plugin may hand text to, in layout order. For agent panes,
    /// uses `AgentEntry::display_title()` (session title > first prompt > OSC
    /// title > agent name) alongside the agent id and live status.
    pub(super) fn plugin_panes(&self, project: i64, cx: &App) -> Vec<PaneTarget> {
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return Vec::new();
        };
        let Some(tab) = tabs.active() else {
            return Vec::new();
        };
        let tab_name = Self::tab_name(tabs, tabs.selected);
        let focused = tab.focused;
        let workspace = self.workspace.read(cx);
        tab.layout
            .panes()
            .into_iter()
            .enumerate()
            .map(|(index, pane)| {
                let agent_entry = self.agent_status.get(&pane);
                let agent = agent_entry.map(|entry| entry.agent.clone()).or_else(|| {
                    match self.launches.get(&pane) {
                        Some(PaneLaunch::Agent { agent, .. }) => Some(agent.clone()),
                        _ => None,
                    }
                });
                let state = agent_entry.and_then(|entry| entry.state);
                let title = agent_entry
                    .map(AgentEntry::display_title)
                    .filter(|label| !label.is_empty())
                    .or_else(|| {
                        workspace
                            .terminal_handle(pane.0)
                            .map(|term| term.label())
                            .filter(|label| !label.is_empty())
                    })
                    .or_else(|| agent.as_deref().and_then(agent_display_name))
                    .unwrap_or_else(|| "终端".to_string());
                PaneTarget {
                    pane: pane.0,
                    ordinal: index + 1,
                    tab_name: tab_name.clone(),
                    title,
                    agent,
                    state,
                    focused: pane == focused,
                }
            })
            .collect()
    }

    /// Paste `text` into one pane of `project`, or into its focused pane when
    /// `pane` is `None`. An agent pane also receives Enter, because the user is
    /// handing the agent an instruction; a shell keeps the text on its command
    /// line, where Enter would run it. Without bracketed paste a shell would run
    /// each pasted line, so a shell gets the message folded onto one line.
    pub(super) fn send_text_to_pane(
        &self,
        project: i64,
        pane: Option<u64>,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Result<SendOutcome, String> {
        if self.selected != Some(project) {
            return Err("请先切回该项目再发送".to_string());
        }
        let pane = match pane {
            Some(pane) => {
                let known = self
                    .tabs_by_project
                    .get(&project)
                    .and_then(ProjectTabs::active)
                    .is_some_and(|tab| tab.layout.panes().iter().any(|id| id.0 == pane));
                if !known {
                    return Err("目标窗格已关闭，请重新选择".to_string());
                }
                pane
            }
            None => {
                self.active_tab(project)
                    .map(|tab| tab.focused)
                    .unwrap_or(self.focused)
                    .0
            }
        };
        let Some(term) = self.workspace.read(cx).terminal_handle(pane) else {
            return Err("当前窗格没有运行中的终端".to_string());
        };
        let shell = self.workspace.read(cx).pane_is_shell(pane);
        if shell == Some(true) {
            term.paste(shell_prompt(text));
        } else {
            term.paste(text);
        }
        if !submits_after_paste(shell) {
            return Ok(SendOutcome::Pasted);
        }
        // A large bracketed paste can keep the TUI busy long enough to swallow
        // an Enter that arrives in the same read, so submit after it settles.
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            term.key(KeyInput {
                key: "enter".into(),
                key_char: Some("\r".into()),
                shift: false,
                control: false,
                alt: false,
            });
        })
        .detach();
        Ok(SendOutcome::Submitted)
    }

    /// Pace live PTY resizes on all panes while a left/right workspace panel
    /// slides open or shut, then flush the exact final column count on settle.
    pub(super) fn begin_panel_resize_defer(&mut self, cx: &mut Context<Self>) {
        self.panel_anim_gen = self.panel_anim_gen.wrapping_add(1);
        let generation = self.panel_anim_gen;
        for terminal in self.panes.values() {
            terminal.update(cx, |term, cx| term.set_panel_animating(true, cx));
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PANEL_SETTLE_FALLBACK).await;
            let _ = this.update(cx, |app, cx| {
                if app.panel_anim_gen == generation {
                    app.finish_panel_resize_defer(cx);
                    if app.open_plugin.is_none() {
                        app.plugin_views.clear();
                        app.native_registry.clear_plugins();
                    }
                }
            });
        })
        .detach();
    }

    /// End animation pacing and flush any remaining 1–2 column delta immediately.
    pub(super) fn finish_panel_resize_defer(&mut self, cx: &mut Context<Self>) {
        for terminal in self.panes.values() {
            terminal.update(cx, |term, cx| term.set_panel_animating(false, cx));
        }
    }

    /// Close the active right-hand plugin panel smoothly: keep its native view
    /// alive until `HomeShellAction::PluginCloseFinished` arrives after the
    /// slide-out animation completes.
    pub(super) fn close_plugin(&mut self, cx: &mut Context<Self>) {
        if self.open_plugin.is_none() {
            return;
        }
        self.begin_panel_resize_defer(cx);
        self.open_plugin = None;
        cx.notify();
    }

    /// Open `spec`'s panel, or close it when it is already the open one. The
    /// panel is built on first open and dropped once its close slide finishes
    /// so its refresh loop stops with it.
    pub(super) fn toggle_plugin(
        &mut self,
        spec: PluginSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_plugin == Some(spec.id) {
            self.close_plugin(cx);
            return;
        }
        let was_closed = self.open_plugin.is_none();
        if was_closed {
            self.begin_panel_resize_defer(cx);
        }
        self.open_plugin = Some(spec.id);
        self.plugin_views
            .retain(|id, _| Some(*id) == self.open_plugin);
        if let Some(existing) = self.plugin_views.get(spec.id).cloned() {
            self.native_registry.set_plugin(spec.id, existing);
        } else {
            let project = self.plugin_project(cx);
            let host = self.plugin_host(cx);
            let view = spec.create(project, host, window, cx);
            self.native_registry.set_plugin(spec.id, view.clone());
            self.plugin_views.insert(spec.id, view);
        }
        self.sync_plugin_project(cx);
        cx.notify();
    }

    /// Tell the open panel which project is selected. The panel ignores a
    /// project it is already showing.
    ///
    /// Deferred on `App` so a plugin callback into the host (dispatching an
    /// agent or revealing a pane) never re-enters `panel.update(...)` while
    /// the plugin entity is already leased on the call stack.
    pub(super) fn sync_plugin_project(&mut self, cx: &mut Context<Self>) {
        if self.open_plugin.is_none() {
            return;
        }
        let home = cx.entity().downgrade();
        (**cx).defer(move |cx| {
            let Some((spec, view, project)) = home
                .read_with(cx, |app, cx| {
                    let id = app.open_plugin?;
                    let view = app.plugin_views.get(id).cloned()?;
                    let spec = crate::plugins::spec(id)?;
                    Some((spec, view, app.plugin_project(cx)))
                })
                .ok()
                .flatten()
            else {
                return;
            };
            spec.set_project(&view, project, cx);
        });
    }

    /// One title-bar entry per registered plugin; the open panel's entry stays
    /// highlighted.
    pub(super) fn render_plugin_button(&self, spec: PluginSpec, cx: &Context<Self>) -> AnyElement {
        let theme = KumuTheme::global(cx);
        let colors = &theme.colors;
        let active = self.open_plugin == Some(spec.id);
        let hovered = self.hovered_plugin_tooltip == Some(spec.id);
        let background = if active {
            colors.accent
        } else {
            colors.transparent
        };
        let hover_bg = colors.accent.opacity(0.5);
        let foreground = if active {
            colors.link
        } else {
            colors.muted_foreground
        };
        let attention = (spec.id == "todo")
            .then(|| self.todo_attention_color(cx))
            .flatten();
        let icon = spec.icon;
        let plugin_id = spec.id;
        let plugin_title = spec.title;
        let button = div()
            .id(SharedString::from(format!("plugin-toggle-{}", spec.id)))
            .flex()
            .items_center()
            .justify_center()
            .w(px(24.))
            .h(px(24.))
            .rounded(px(6.))
            .cursor_pointer()
            .bg(background)
            .hover(move |style| style.bg(hover_bg))
            .on_hover(cx.listener(move |this, is_hovered: &bool, _, cx| {
                let next = if *is_hovered {
                    Some(plugin_id)
                } else if this.hovered_plugin_tooltip == Some(plugin_id) {
                    None
                } else {
                    this.hovered_plugin_tooltip
                };
                if this.hovered_plugin_tooltip != next {
                    this.hovered_plugin_tooltip = next;
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.hovered_plugin_tooltip = None;
                this.toggle_plugin(spec.clone(), window, cx);
            }))
            .child(svg().path(icon).size(px(14.)).text_color(foreground));

        let mut wrapper = div().relative().child(button);
        if let Some(dot_color) = attention {
            wrapper = wrapper.child(
                div()
                    .absolute()
                    .top(px(2.))
                    .right(px(2.))
                    .w(px(6.))
                    .h(px(6.))
                    .rounded_full()
                    .bg(dot_color),
            );
        }
        if hovered {
            wrapper = wrapper.child(
                deferred(
                    div()
                        .absolute()
                        .top(px(28.))
                        .right_0()
                        .px_2()
                        .py_1()
                        .rounded(px(theme.radius))
                        .bg(colors.popover)
                        .border_1()
                        .border_color(colors.border)
                        .shadow_md()
                        .whitespace_nowrap()
                        .text_xs()
                        .text_color(colors.popover_foreground)
                        .child(plugin_title),
                )
                .with_priority(1),
            );
        }
        wrapper.into_any_element()
    }
}
