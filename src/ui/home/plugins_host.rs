use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{AgentEntry, KumuApp, agent_display_name, shell_prompt, submits_after_paste};
use crate::agent::AgentState;
use crate::agent::launch::PaneLaunch;
use crate::app::layout::{PaneId, PaneNode, ProjectTab, ProjectTabs, SplitAxis};
use crate::db::TodoStatus;
use crate::plugins::{
    AgentPaneInfo, DispatchMode, PaneTarget, PluginHost, PluginProject, PluginSpec, SendOutcome,
};
use crate::term::KeyInput;

/// The plugin panel's opening width and the range its divider allows.
pub(super) const PLUGIN_WIDTH: f32 = 300.;
pub(super) const PLUGIN_MIN_WIDTH: f32 = 240.;
pub(super) const PLUGIN_MAX_WIDTH: f32 = 480.;

/// How long a newly dispatched TODO agent pane waits to draw before sending its prompt.
const DISPATCH_PROMPT_TIMEOUT: Duration = Duration::from_secs(12);
/// Brief settle window after the first drawn frame so bracketed paste does not race startup.
const DISPATCH_PROMPT_SETTLE: Duration = Duration::from_millis(400);

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
        let theme = cx.theme();
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
                return Some(theme.warning);
            }
            if todo.awaiting_review {
                has_review = true;
            }
        }
        has_review.then_some(theme.success)
    }

    /// The panes a plugin may hand text to, in layout order. The header's label
    /// gives way to the agent's name only when no program has set one yet.
    pub(super) fn plugin_panes(&self, project: i64, cx: &App) -> Vec<PaneTarget> {
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return Vec::new();
        };
        let Some(tab) = tabs.active() else {
            return Vec::new();
        };
        let focused = tab.focused;
        let workspace = self.workspace.read(cx);
        tab.layout
            .panes()
            .into_iter()
            .enumerate()
            .map(|(index, pane)| {
                let agent = match self.launches.get(&pane) {
                    Some(PaneLaunch::Agent { agent, .. }) => Some(agent.clone()),
                    _ => None,
                };
                let title = workspace
                    .terminal_handle(pane.0)
                    .map(|term| term.label())
                    .filter(|label| !label.is_empty())
                    .or_else(|| agent.as_deref().and_then(agent_display_name))
                    .unwrap_or_else(|| "终端".to_string());
                PaneTarget {
                    pane: pane.0,
                    ordinal: index + 1,
                    title,
                    agent,
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

    /// Open `spec`'s panel, or close it when it is already the open one. The
    /// panel is built on first open and dropped on close so its refresh loop
    /// stops with it.
    pub(super) fn toggle_plugin(
        &mut self,
        spec: PluginSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_plugin == Some(spec.id) {
            self.open_plugin = None;
            self.plugin_views.clear();
        } else {
            self.open_plugin = Some(spec.id);
            self.plugin_views
                .retain(|id, _| Some(*id) == self.open_plugin);
            if !self.plugin_views.contains_key(spec.id) {
                let project = self.plugin_project(cx);
                let host = self.plugin_host(cx);
                let view = spec.create(project, host, window, cx);
                self.plugin_views.insert(spec.id, view);
            }
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
        let theme = cx.theme();
        let active = self.open_plugin == Some(spec.id);
        let background = if active {
            theme.accent
        } else {
            theme.transparent
        };
        let foreground = if active {
            theme.link
        } else {
            theme.muted_foreground
        };
        let attention = (spec.id == "todo")
            .then(|| self.todo_attention_color(cx))
            .flatten();
        let button = Button::new(format!("plugin-toggle-{}", spec.id))
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(background)
                    .foreground(foreground)
                    .hover(theme.accent.opacity(0.5)),
            )
            .compact()
            .icon(Icon::empty().path(spec.icon))
            .tooltip(spec.title)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle_plugin(spec.clone(), window, cx);
            }));
        let Some(dot_color) = attention else {
            return button.into_any_element();
        };
        div()
            .relative()
            .child(button)
            .child(
                div()
                    .absolute()
                    .top(px(2.))
                    .right(px(2.))
                    .w(px(6.))
                    .h(px(6.))
                    .rounded_full()
                    .bg(dot_color),
            )
            .into_any_element()
    }

    /// The open plugin panel: host chrome (title and close) above the plugin's
    /// own content.
    pub(super) fn render_plugin_panel(&self, cx: &Context<Self>) -> AnyElement {
        let Some(id) = self.open_plugin else {
            return div().into_any_element();
        };
        let Some(spec) = crate::plugins::spec(id) else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.sidebar_border);
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(border)
                    .child(Icon::empty().path(spec.icon).small().text_color(muted))
                    .child(div().text_xs().font_medium().child(spec.title))
                    .child(div().flex_1())
                    .child(
                        Button::new(format!("plugin-close-{}", spec.id))
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .color(theme.transparent)
                                    .foreground(muted)
                                    .hover(theme.accent.opacity(0.5)),
                            )
                            .compact()
                            .icon(IconName::Close)
                            .tooltip("收起面板")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_plugin = None;
                                this.plugin_views.clear();
                                cx.notify();
                            })),
                    ),
            )
            .children(self.plugin_views.get(id).cloned().map(|view| {
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(view.cached(StyleRefinement::default().size_full().min_h_0().min_w_0()))
            }))
            .into_any_element()
    }
}
