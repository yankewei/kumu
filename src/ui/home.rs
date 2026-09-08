//! Home window: projects → sessions tree on the left, the selected session's
//! conversation on the right. Owns the settings window handle.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

use super::{AgentModels, ModelCatalog, SettingsWindow};
use crate::acp;
use crate::agents::AGENTS;
use crate::db::{Message, Project, Session, Store, db_path};
use crate::ui::widgets::agent_logo;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

pub struct KumuApp {
    store: Store,
    settings_window: Option<WindowHandle<Root>>,
    projects: Vec<Project>,
    sessions: Vec<Session>,
    selected: Option<i64>,
    /// Projects collapsed in the sidebar tree.
    collapsed_projects: HashSet<i64>,
    messages: Vec<Message>,
    input_state: Entity<TextareaState>,
    /// Live ACP connections keyed by session id (reused across prompts).
    acp_txs: HashMap<i64, UnboundedSender<acp::Cmd>>,
    /// In-flight agent replies keyed by session id.
    streaming: HashMap<i64, String>,
    /// Model picker state: controlled open flag, which agent's rail tab is
    /// active (None = the session's current agent), and the search field.
    model_picker_open: bool,
    model_picker_tab: Option<&'static str>,
    model_search: Entity<InputState>,
}

impl KumuApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = Store::open(&db_path()).expect("failed to open sqlite db");
        let mut app = Self {
            store,
            settings_window: None,
            projects: Vec::new(),
            sessions: Vec::new(),
            selected: None,
            collapsed_projects: HashSet::new(),
            messages: Vec::new(),
            input_state: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(1, 6)
                    .placeholder("输入消息…")
            }),
            acp_txs: HashMap::new(),
            streaming: HashMap::new(),
            model_picker_open: false,
            model_picker_tab: None,
            model_search: cx.new(|cx| InputState::new(window, cx).placeholder("搜索模型…")),
        };
        app.bootstrap();
        app.probe_models(cx);
        app
    }

    /// Probe agents whose model catalog isn't cached yet, so the composer's
    /// model picker has data even before settings was opened.
    fn probe_models(&mut self, cx: &mut Context<Self>) {
        let mut missing = Vec::new();
        if cx.has_global::<AgentModels>() {
            for a in AGENTS.iter() {
                if !cx.global::<AgentModels>().0.contains_key(a.id) {
                    missing.push(a.id);
                }
            }
        } else {
            cx.set_global(AgentModels::default());
            missing.extend(AGENTS.iter().map(|a| a.id));
        }
        if missing.is_empty() {
            return;
        }

        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<acp::Event>();
        for agent in missing {
            if let Some(cfg) = acp::Config::for_agent(agent) {
                acp::probe(agent, cfg, ev_tx.clone());
            }
        }
        drop(ev_tx);

        let (bridge_tx, mut bridge_rx) = futures::channel::mpsc::unbounded::<acp::Event>();
        std::thread::spawn(move || {
            for ev in ev_rx {
                if bridge_tx.unbounded_send(ev).is_err() {
                    break;
                }
            }
        });
        cx.spawn(async move |this, cx| {
            while let Some(ev) = bridge_rx.next().await {
                let _ = this.update(cx, |app, cx| {
                    app.on_acp_event(-1, ev, cx);
                });
            }
        })
        .detach();
    }

    fn bootstrap(&mut self) {
        if self.store.projects().is_empty() {
            let cwd = std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("/"))
                .to_string_lossy()
                .into_owned();
            self.store.insert_project("kumu", &cwd).ok();
        }
        self.refresh();
        if self.sessions.is_empty() {
            if let Some(p) = self.projects.first().cloned() {
                let cwd = p.path.clone();
                self.store.insert_session(p.id, "omp", "会话 1", &cwd).ok();
                self.refresh();
            }
        }
        if self.selected.is_none() {
            if let Some(s) = self.sessions.first().cloned() {
                self.select_session(s.id);
            }
        }
    }

    fn refresh(&mut self) {
        self.projects = self.store.projects();
        self.sessions = self.store.sessions();
    }

    fn select_session(&mut self, id: i64) {
        self.selected = Some(id);
        self.messages = self.store.messages(id);
    }

    fn new_session(&mut self) {
        let project_id = self.projects.first().map(|p| p.id).unwrap_or_else(|| {
            self.store
                .insert_project("kumu", "cwd-not-set")
                .unwrap_or(1)
        });
        self.new_session_in(project_id);
    }

    fn new_session_in(&mut self, project_id: i64) {
        let cwd = self
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .map(|p| p.path.clone())
            .unwrap_or_default();
        let n = self.sessions.len() + 1;
        if let Ok(id) = self
            .store
            .insert_session(project_id, "omp", &format!("会话 {n}"), &cwd)
        {
            self.refresh();
            self.select_session(id);
        }
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected else {
            return;
        };
        let text = self.input_state.read(cx).value().trim().to_string();
        if text.is_empty() || self.streaming.contains_key(&session_id) {
            return;
        }
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        self.store.insert_message(session_id, "user", &text).ok();
        self.messages = self.store.messages(session_id);
        self.input_state
            .update(cx, |state, cx| state.set_value("", window, cx));

        self.ensure_connection(&session, cx);
        if let Some(tx) = self.acp_txs.get(&session_id) {
            self.streaming.insert(session_id, String::new());
            let _ = tx.send(acp::Cmd::Prompt(text));
        } else {
            self.streaming.remove(&session_id);
        }
        cx.notify();
    }

    /// Get or open the ACP connection for a session's agent, and start the
    /// task that folds ACP events back into this view.
    fn ensure_connection(&mut self, session: &Session, cx: &mut Context<Self>) {
        if self.acp_txs.contains_key(&session.id) {
            return;
        }
        let agent_id = AGENTS.iter().find(|a| a.id == session.agent).map(|a| a.id);
        let Some(agent_id) = agent_id else {
            self.system_message(session.id, format!("未支持的 agent：{}", session.agent));
            return;
        };
        let Some(cfg) = acp::Config::for_agent(agent_id) else {
            return;
        };
        let cwd = if session.cwd.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
        } else {
            PathBuf::from(&session.cwd)
        };

        // ACP thread -> std channel -> forwarder thread -> async channel ->
        // UI task (same bridge pattern as the settings window's probes).
        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<acp::Event>();
        let cmd_tx = acp::spawn(cfg, cwd, ev_tx);
        // Re-apply the session's stored model on a fresh connection; queued
        // until the ACP thread finishes initialize + session/new.
        if let Some(model) = session.model.clone() {
            let _ = cmd_tx.send(acp::Cmd::SetModel(model));
        }
        self.acp_txs.insert(session.id, cmd_tx);

        let (bridge_tx, mut bridge_rx) = futures::channel::mpsc::unbounded::<acp::Event>();
        std::thread::spawn(move || {
            for ev in ev_rx {
                if bridge_tx.unbounded_send(ev).is_err() {
                    break;
                }
            }
        });
        let session_id = session.id;
        cx.spawn(async move |this, cx| {
            while let Some(ev) = bridge_rx.next().await {
                let _ = this.update(cx, |app, cx| {
                    app.on_acp_event(session_id, ev, cx);
                });
            }
        })
        .detach();
    }

    fn on_acp_event(&mut self, session_id: i64, ev: acp::Event, cx: &mut Context<Self>) {
        match ev {
            acp::Event::Agent(chunk) => {
                self.streaming
                    .entry(session_id)
                    .or_default()
                    .push_str(&chunk);
            }
            acp::Event::Done(_) => {
                if let Some(text) = self.streaming.remove(&session_id) {
                    if !text.trim().is_empty() {
                        self.store.insert_message(session_id, "agent", &text).ok();
                    }
                    if self.selected == Some(session_id) {
                        self.messages = self.store.messages(session_id);
                    }
                }
            }
            acp::Event::Error(err) => {
                self.streaming.remove(&session_id);
                self.system_message(session_id, err);
            }
            acp::Event::Models {
                agent,
                models,
                current,
            } => {
                if !models.is_empty() {
                    if !cx.has_global::<AgentModels>() {
                        cx.set_global(AgentModels::default());
                    }
                    cx.global_mut::<AgentModels>()
                        .0
                        .insert(agent, ModelCatalog { models, current });
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Switch a session's agent and/or model: persists the choice, reopens the
    /// ACP connection when the agent changes, and applies the model over the
    /// session's model config option.
    fn pick_model(&mut self, agent_id: &'static str, model_value: String, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected else {
            return;
        };
        let Some(session) = self.selected_session() else {
            return;
        };
        let agent_changed = session.agent != agent_id;
        self.store
            .update_session_agent_model(session_id, agent_id, Some(&model_value))
            .ok();
        self.refresh();
        if agent_changed {
            // Dropping the sender closes the old agent subprocess; the fresh
            // connection below re-applies the stored model on session/new.
            // The in-flight reply (if any) is abandoned with it.
            self.acp_txs.remove(&session_id);
            self.streaming.remove(&session_id);
        }
        if let Some(session) = self.selected_session().cloned() {
            // A connection that already existed needs the model pushed
            // explicitly; a fresh one from ensure_connection re-applies the
            // just-stored model itself.
            let existed = self.acp_txs.contains_key(&session_id);
            self.ensure_connection(&session, cx);
            if existed {
                if let Some(tx) = self.acp_txs.get(&session_id) {
                    let _ = tx.send(acp::Cmd::SetModel(model_value));
                }
            }
        }
        cx.notify();
    }

    /// Label for the composer's model picker chip: the selected model's name
    /// (the chip's icon already identifies the agent). Fresh sessions fall
    /// back to the model the agent reports as active, then the agent name.
    fn picker_label(&self, session: &Session, cx: &App) -> String {
        let catalog = cx
            .try_global::<AgentModels>()
            .and_then(|g| g.0.get(session.agent.as_str()));
        let model_value = session
            .model
            .as_deref()
            .or(catalog.and_then(|c| c.current.as_deref()));
        match model_value {
            Some(value) => catalog
                .and_then(|c| c.models.iter().find(|m| m.value == value))
                .map(|m| m.name.clone())
                .unwrap_or_else(|| value.to_string()),
            None => self.agent_name(&session.agent),
        }
    }

    /// The composer's model picker, Waku-style: a chip trigger opening a
    /// controlled panel with an agent rail on the left and a searchable model
    /// list on the right. Picking a model switches the session's agent with it.
    fn model_picker(&self, session: &Session, cx: &Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let label = self.picker_label(session, cx);
        let chip_logo = AGENTS
            .iter()
            .find(|a| a.id == session.agent)
            .and_then(|a| a.logo);
        let (radius, muted, muted_fg, border, accent, accent_fg, background) = {
            let theme = cx.theme();
            (
                theme.radius,
                theme.muted,
                theme.muted_foreground,
                theme.border,
                theme.sidebar_accent,
                theme.sidebar_accent_foreground,
                theme.background,
            )
        };
        let search = self.model_search.clone();
        let open = self.model_picker_open;

        let chip = ChipTrigger {
            element: h_flex()
                .id("model-picker-chip")
                .h(px(24.))
                .px_1p5()
                .gap_1p5()
                .rounded(radius)
                .cursor_pointer()
                .text_color(muted_fg)
                .when(open, |el| el.bg(muted.opacity(0.6)))
                .when(!open, |el| el.hover(move |s| s.bg(muted.opacity(0.6))))
                .child(agent_logo(chip_logo, px(13.), muted_fg))
                .child(div().max_w(px(220.)).text_xs().truncate().child(label)),
        };

        Popover::new("model-picker")
            .appearance(false)
            .anchor(Anchor::BottomLeft)
            .open(open)
            .on_open_change({
                let weak = weak.clone();
                let search = search.clone();
                move |open, window, cx| {
                    let open = *open;
                    let _ = weak.update(cx, |app, cx| {
                        app.model_picker_open = open;
                        if open {
                            // Waku resets onto the session's agent and clears
                            // the filter each time the panel opens.
                            app.model_picker_tab = None;
                            search.update(cx, |state, cx| state.set_value("", window, cx));
                        }
                        cx.notify();
                    });
                }
            })
            .trigger(chip)
            .content(move |_state, _window, cx| {
                let cache = if cx.has_global::<AgentModels>() {
                    cx.global::<AgentModels>().0.clone()
                } else {
                    HashMap::new()
                };
                let (cur_agent, cur_model) = weak
                    .upgrade()
                    .and_then(|app| app.read(cx).selected_session().cloned())
                    .map(|s| (s.agent, s.model))
                    .unwrap_or_default();
                let tab = weak.upgrade().and_then(|app| app.read(cx).model_picker_tab);
                let query = search.read(cx).value().trim().to_lowercase();
                let searching = !query.is_empty();
                // The rail highlights the active tab; with no tab picked it
                // follows the session's current agent.
                let active_agent = tab.unwrap_or_else(|| cur_agent.as_str());

                // ---- Left rail: one logo tab per agent (Waku-style). ----
                let mut rail = v_flex()
                    .w_12()
                    .h_full()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .py_2()
                    .bg(muted.opacity(0.4))
                    .border_r_1()
                    .border_color(border);
                for a in AGENTS.iter() {
                    let selected = active_agent == a.id;
                    let tab_weak = weak.clone();
                    rail = rail.child(
                        div()
                            .id(format!("model-tab-{}", a.id))
                            .size(px(34.))
                            .rounded(radius)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .when(selected, |el| el.bg(accent))
                            .when(!selected, |el| el.hover(move |s| s.bg(muted)))
                            .child(agent_logo(
                                a.logo,
                                px(18.),
                                if selected { accent_fg } else { muted_fg },
                            ))
                            .on_click(move |_, _, cx| {
                                let _ = tab_weak.update(cx, |app, cx| {
                                    app.model_picker_tab = Some(a.id);
                                    cx.notify();
                                });
                            }),
                    );
                }

                // ---- Right: search field + model rows. ----
                let mut list = v_flex().gap_0p5().p_1p5();
                if searching {
                    let matched: Vec<(&'static str, &acp::ModelInfo)> = AGENTS
                        .iter()
                        .flat_map(|a| {
                            cache
                                .get(a.id)
                                .map(|c| c.models.as_slice())
                                .into_iter()
                                .flatten()
                                .filter(|m| {
                                    m.name.to_lowercase().contains(&query)
                                        || m.value.to_lowercase().contains(&query)
                                })
                                .map(|m| (a.id, m))
                        })
                        .collect();
                    if matched.is_empty() {
                        list = list.child(empty_list_state("没有匹配的模型", muted_fg));
                    }
                    for (agent_id, model) in matched {
                        let agent_name = AGENTS
                            .iter()
                            .find(|a| a.id == agent_id)
                            .map(|a| a.name)
                            .unwrap_or("?");
                        list = list.child(model_row(
                            &model.name,
                            format!("{agent_name} · {}", model.provider),
                            agent_id == cur_agent
                                && cur_model.as_deref() == Some(model.value.as_str()),
                            radius,
                            muted,
                            muted_fg,
                            accent,
                            accent_fg,
                            {
                                let weak = weak.clone();
                                let value = model.value.clone();
                                move |_, _, cx| {
                                    let _ = weak.update(cx, |app, cx| {
                                        app.pick_model(agent_id, value.clone(), cx);
                                        app.model_picker_open = false;
                                        cx.notify();
                                    });
                                }
                            },
                        ));
                    }
                } else {
                    let list_agent =
                        tab.or_else(|| AGENTS.iter().find(|a| a.id == cur_agent).map(|a| a.id));
                    match list_agent.and_then(|id| cache.get(id)) {
                        Some(catalog) if !catalog.models.is_empty() => {
                            let agent_id = list_agent.unwrap();
                            for model in catalog.models.iter() {
                                list = list.child(model_row(
                                    &model.name,
                                    model.provider.clone(),
                                    agent_id == cur_agent
                                        && cur_model.as_deref() == Some(model.value.as_str()),
                                    radius,
                                    muted,
                                    muted_fg,
                                    accent,
                                    accent_fg,
                                    {
                                        let weak = weak.clone();
                                        let value = model.value.clone();
                                        move |_, _, cx| {
                                            let _ = weak.update(cx, |app, cx| {
                                                app.pick_model(agent_id, value.clone(), cx);
                                                app.model_picker_open = false;
                                                cx.notify();
                                            });
                                        }
                                    },
                                ));
                            }
                        }
                        Some(_) => {
                            list = list.child(empty_list_state("无可用模型", muted_fg));
                        }
                        None => {
                            list = list.child(empty_list_state("检测模型…", muted_fg));
                        }
                    }
                }

                h_flex()
                    .w(px(460.))
                    .h(px(340.))
                    .rounded(radius)
                    .border_1()
                    .border_color(border)
                    .bg(background)
                    .shadow_lg()
                    .overflow_hidden()
                    .child(rail)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(div().p_2().border_b_1().border_color(border).child(
                                Input::new(&search).small().prefix(
                                    Icon::new(IconName::Search).small().text_color(muted_fg),
                                ),
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .id("model-picker-list")
                                    .overflow_y_scroll()
                                    .child(list),
                            ),
                    )
            })
    }

    /// Persist and show a system notice (unsupported agent, ACP error…).
    fn system_message(&mut self, session_id: i64, text: String) {
        self.store.insert_message(session_id, "system", &text).ok();
        if self.selected == Some(session_id) {
            self.messages = self.store.messages(session_id);
        }
    }

    fn selected_session(&self) -> Option<&Session> {
        let id = self.selected?;
        self.sessions.iter().find(|s| s.id == id)
    }

    fn agent_name(&self, agent_id: &str) -> String {
        AGENTS
            .iter()
            .find(|a| a.id == agent_id)
            .map(|a| a.name.to_string())
            .unwrap_or_else(|| agent_id.to_string())
    }

    /// Focus the settings window if it's open, else open a new one (resizable,
    /// opens at its minimum size).
    fn open_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = &self.settings_window {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
        }
        let mut options = TitleBar::window_options();
        options.window_bounds = Some(WindowBounds::centered(size(px(680.), px(640.)), cx));
        options.window_min_size = Some(size(px(680.), px(640.)));
        if let Ok(handle) = cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| SettingsWindow::new(window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        }) {
            handle
                .update(cx, |_, window, _| window.activate_window())
                .ok();
            self.settings_window = Some(handle);
        }
    }

    fn render_home(&self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let font_family = theme.font_family.clone();
        // Owned copies so row closures are 'static (no theme borrow).
        let (foreground, muted_fg, border, radius) = (
            theme.foreground,
            theme.muted_foreground,
            theme.border,
            theme.radius,
        );
        let (sidebar, sidebar_fg, sidebar_border) = (
            theme.sidebar,
            theme.sidebar_foreground,
            theme.sidebar_border,
        );
        let (sidebar_accent, sidebar_accent_fg) =
            (theme.sidebar_accent, theme.sidebar_accent_foreground);
        let primary = theme.primary;
        let selected = self.selected;

        // ---- Left: sidebar surface with the project/session tree. ----
        let mut tree: Vec<Stateful<Div>> = Vec::new();
        for project in &self.projects {
            let project_name = project.name.clone();
            let project_id = project.id;
            let collapsed = self.collapsed_projects.contains(&project_id);
            let group = format!("project-row-{project_id}");
            tree.push(
                h_flex()
                    .id(format!("project-head-{project_id}"))
                    .group(group.clone())
                    .mt_2()
                    .px_3()
                    .py_1p5()
                    .gap_1()
                    .cursor_pointer()
                    .hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed_projects.remove(&project_id) {
                            this.collapsed_projects.insert(project_id);
                        }
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if collapsed {
                            IconName::Folder
                        } else {
                            IconName::FolderOpen
                        })
                        .small()
                        .text_color(muted_fg),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_medium()
                            .text_color(sidebar_fg)
                            .child(project_name),
                    )
                    .child(
                        div()
                            .id(format!("project-new-session-{project_id}"))
                            .rounded(radius)
                            .p_0p5()
                            .cursor_pointer()
                            .text_color(muted_fg)
                            .opacity(0.)
                            .group_hover(group, |style| style.opacity(1.))
                            .hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.new_session_in(project_id);
                                cx.stop_propagation();
                                cx.notify();
                            }))
                            .child(Icon::new(IconName::Plus).small()),
                    ),
            );
            let project_sessions = self
                .sessions
                .iter()
                .filter(|s| s.project_id == project.id)
                .cloned()
                .collect::<Vec<_>>();
            if !collapsed {
                for s in project_sessions {
                    let active = selected == Some(s.id);
                    let (bg, fg, tag) = if active {
                        (sidebar_accent, sidebar_accent_fg, sidebar_accent_fg)
                    } else {
                        (theme.transparent, sidebar_fg, muted_fg)
                    };
                    let sid = s.id;
                    let title = s.title.clone();
                    let agent = AGENTS
                        .iter()
                        .find(|a| a.id == s.agent)
                        .and_then(|a| a.logo);
                    tree.push(
                        div()
                            .id(format!("session-{sid}"))
                            .px_3()
                            .py_1p5()
                            .cursor_pointer()
                            .bg(bg)
                            .text_color(fg)
                            .when(!active, |this| {
                                this.hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_session(sid);
                                cx.notify();
                            }))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().flex_1().text_xs().child(title))
                                    .child(agent_logo(agent, px(14.), tag)),
                            ),
                    );
                }
            }
        }

        let left = v_flex()
            .w_64()
            .h_full()
            .flex_shrink_0()
            .bg(sidebar)
            .text_color(sidebar_fg)
            .child(
                div()
                    .flex_1()
                    .id("session-tree")
                    .overflow_y_scroll()
                    .children(tree),
            )
            .child(
                div()
                    .px_3()
                    .py_1p5()
                    .flex()
                    .border_t_1()
                    .border_color(sidebar_border)
                    .child(
                        div()
                            .id("open-settings")
                            .rounded(radius)
                            .p_1()
                            .cursor_pointer()
                            .text_color(muted_fg)
                            .hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                            .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)))
                            .child(Icon::new(IconName::Settings).small()),
                    ),
            );

        // ---- Right: selected session's header + conversation. ----
        let right =
            match self.selected_session() {
                Some(session) => {
                    let agent = self.agent_name(&session.agent);
                    let title = session.title.clone();
                    let messages = self.messages.clone();
                    let agent_for_rows = agent.clone();
                    let streaming = self
                        .streaming
                        .get(&session.id)
                        .cloned()
                        .map(|text| (agent.clone(), text));
                    // One centered column shared by messages and composer: capped
                    // width for readability, floored so it never gets too skinny.
                    let (col_max, col_min) = (rems(44.), rems(24.));
                    let mut rows = v_flex()
                        .gap_3()
                        .children(messages.into_iter().map(move |m| {
                            if m.role == "user" {
                                h_flex().justify_end().child(
                                    div()
                                        .max_w(relative(0.78))
                                        .px_3()
                                        .py_2()
                                        .rounded(radius)
                                        .bg(primary.opacity(0.1))
                                        .child(div().text_sm().child(m.content)),
                                )
                            } else if m.role == "system" {
                                div().text_xs().text_color(muted_fg).child(m.content)
                            } else {
                                v_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_medium()
                                            .text_color(muted_fg)
                                            .child(agent_for_rows.clone()),
                                    )
                                    .child(div().text_sm().child(m.content))
                            }
                        }));
                    if let Some((agent_name, text)) = streaming {
                        rows = rows.child(
                            v_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_medium()
                                        .text_color(muted_fg)
                                        .child(agent_name),
                                )
                                .child(div().text_sm().text_color(muted_fg).child(
                                    if text.is_empty() {
                                        "…".to_string()
                                    } else {
                                        text
                                    },
                                )),
                        );
                    }
                    v_flex()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .child(
                            h_flex()
                                .items_center()
                                .px_4()
                                .py_2()
                                .child(div().text_sm().font_medium().child(title)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .id("conversation")
                                .overflow_y_scroll()
                                .px_5()
                                .py_4()
                                .child(div().max_w(col_max).min_w(col_min).mx_auto().child(rows)),
                        )
                        .child(div().h_px().w_full().bg(border))
                        .child(
                            // Composer: the lower half of the conversation area —
                            // a multi-line textarea with the send button inside,
                            // on the same centered column as the messages.
                            h_flex()
                                .flex_none()
                                .px_5()
                                .py_3()
                                .bg(theme.muted.opacity(0.5))
                                .child(
                                    v_flex()
                                        .w_full()
                                        .max_w(col_max)
                                        .min_w(col_min)
                                        .mx_auto()
                                        .rounded(radius)
                                        .border_1()
                                        .border_color(border)
                                        .bg(theme.background)
                                        .overflow_hidden()
                                        .child(
                                            div().p_2().child(
                                                Textarea::new(&self.input_state)
                                                    .appearance(false)
                                                    .bordered(false)
                                                    .w_full(),
                                            ),
                                        )
                                        .child(
                                            // Toolbar: model picker on the left,
                                            // send pinned to the far right.
                                            h_flex()
                                                .items_center()
                                                .gap_1p5()
                                                .bg(theme.muted.opacity(0.5))
                                                .px_2()
                                                .py_1p5()
                                                .child(self.model_picker(session, cx))
                                                .child(div().flex_1())
                                                .child(
                                                    Button::new("send")
                                                        .primary()
                                                        .small()
                                                        .icon(IconName::ArrowUp)
                                                        .on_click(cx.listener(
                                                            |this, _, window, cx| {
                                                                this.send(window, cx);
                                                            },
                                                        )),
                                                ),
                                        ),
                                ),
                        )
                }
                None => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .text_color(muted_fg)
                    .child(div().text_sm().child("还没有会话"))
                    .child(Button::new("empty-new").ghost().label("新建会话").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.new_session();
                            cx.notify();
                        }),
                    )),
            };

        // Full-width title bar strip: brand on the left (traffic lights float
        // to its left); the settings entry lives in the sidebar's bottom-left.
        let title_bar = TitleBar::new().child(div().text_sm().font_bold().child("kumu"));

        v_flex()
            .size_full()
            .font_family(font_family)
            .bg(theme.background)
            .text_color(foreground)
            .child(title_bar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(left)
                    .child(div().w_px().h_full().bg(border))
                    .child(right),
            )
    }
}

/// A Waku-style chip trigger for the picker's popover: a plain interactive
/// div wrapped so it can carry the popover's `selected` (open) state.
struct ChipTrigger {
    element: Stateful<Div>,
}

impl Selectable for ChipTrigger {
    fn selected(self, _selected: bool) -> Self {
        // The open-state tint is applied at build time from the controlled
        // flag; nothing to do here.
        self
    }

    fn is_selected(&self) -> bool {
        false
    }
}

impl IntoElement for ChipTrigger {
    type Element = AnyElement;

    fn into_element(self) -> Self::Element {
        self.element.into_any_element()
    }
}

/// One selectable model row in the picker's list.
fn model_row(
    name: &str,
    subtitle: String,
    selected: bool,
    radius: Pixels,
    muted: Hsla,
    muted_fg: Hsla,
    accent: Hsla,
    accent_fg: Hsla,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(SharedString::from(format!("model-row-{name}-{subtitle}")))
        .h(px(52.))
        .px_2p5()
        .rounded(radius)
        .flex()
        .items_center()
        .min_w_0()
        .cursor_pointer()
        .when(selected, |el| el.bg(accent))
        .when(!selected, |el| el.hover(move |s| s.bg(muted)))
        .child(
            v_flex().min_w_0().child(
                div()
                    .text_sm()
                    .font_medium()
                    .truncate()
                    .text_color(if selected { accent_fg } else { muted_fg })
                    .child(name.to_string()),
            ),
        )
        .child(
            div()
                .text_xs()
                .text_color(if selected { accent_fg } else { muted_fg })
                .truncate()
                .child(subtitle),
        )
        .on_click(on_click)
}

/// Centered placeholder text for an empty model list.
fn empty_list_state(label: &'static str, muted_fg: Hsla) -> Div {
    div()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(muted_fg)
        .child(label)
}

impl Render for KumuApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_home(window, cx)
    }
}
