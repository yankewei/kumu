//! Home window: projects → sessions tree on the left, the selected session's
//! conversation on the right. Owns the settings window handle.

mod composer;
mod conversation;
mod model_picker;
mod sidebar;

// Shared column width keeps the conversation and composer aligned.
const CONTENT_MAX_REM: f32 = 44.;
const CONTENT_MIN_REM: f32 = 24.;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

use super::{AgentModels, ModelCatalog, SettingsWindow};
use crate::acp;
use crate::agents::AGENTS;
use crate::db::{Message, Project, Session, Store, db_path};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::text::TextViewState;
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

/// One tool call observed during the live turn, in first-seen order.
struct ToolRun {
    id: String,
    title: String,
    /// pending / running / done / failed
    status: &'static str,
}

/// Live view of one session's in-flight turn. The answer is an
/// incrementally-parsed markdown view; thinking is collapsed into a status
/// row and tool calls into status rows. All of it is transient: only the
/// answer text is persisted on Done.
struct TurnState {
    started_at: std::time::Instant,
    /// Whether thinking chunks have arrived (rendered as a collapsed status).
    thinking: bool,
    answer: Entity<TextViewState>,
    answer_len: usize,
    /// Mirror of the answer source; persisted on Done.
    answer_text: String,
    tools: Vec<ToolRun>,
}

impl TurnState {
    fn new(cx: &mut App) -> Self {
        Self {
            started_at: std::time::Instant::now(),
            thinking: false,
            answer: cx.new(|cx| TextViewState::markdown("", cx)),
            answer_len: 0,
            answer_text: String::new(),
            tools: Vec::new(),
        }
    }
}

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
    /// Live turn state keyed by session id: streaming thinking, tool calls,
    /// and the answer being built. Dropped when the turn ends.
    turns: HashMap<i64, TurnState>,
    /// Scroll position of the conversation list; pinned to the bottom every
    /// frame while [`Self::follow_tail`] is on.
    conversation_scroll: ScrollHandle,
    /// Whether the conversation auto-scrolls as new content streams in.
    /// Released when the user scrolls up, re-engaged at the bottom.
    follow_tail: bool,
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
                    // Chat composer semantics: plain Enter sends, Shift+Enter
                    // inserts a newline (the textarea handles the newline).
                    .submit_on_enter(true)
            }),
            acp_txs: HashMap::new(),
            turns: HashMap::new(),
            conversation_scroll: ScrollHandle::new(),
            follow_tail: true,
            model_picker_open: false,
            model_picker_tab: None,
            model_search: cx.new(|cx| InputState::new(window, cx).placeholder("搜索模型…")),
        };
        // Enter in the composer sends the prompt (Shift+Enter newline is
        // handled by the textarea itself via submit_on_enter).
        cx.subscribe_in(&app.input_state, window, |this, _, ev, window, cx| {
            // Shift+Enter also emits PressEnter; only plain Enter sends.
            if let InputEvent::PressEnter { shift: false, .. } = ev {
                this.send(window, cx);
            }
        })
        .detach();
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
        self.follow_tail = true;
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
        if text.is_empty() || self.turns.contains_key(&session_id) {
            return;
        }
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        self.follow_tail = true;
        // Until the agent reports a real title (session/info), fall back to
        // the first line of the first user message.
        if session.title.starts_with("会话 ") {
            let line = text.lines().next().unwrap_or(&text).trim();
            if !line.is_empty() {
                let mut title: String = line.chars().take(24).collect();
                if line.chars().count() > 24 {
                    title.push('…');
                }
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                    s.title = title.clone();
                }
                self.store.update_session_title(session_id, &title).ok();
            }
        }
        self.store.insert_message(session_id, "user", &text).ok();
        self.messages = self.store.messages(session_id);
        self.input_state
            .update(cx, |state, cx| state.set_value("", window, cx));

        self.ensure_connection(&session, cx);
        if let Some(tx) = self.acp_txs.get(&session_id) {
            let _ = tx.send(acp::Cmd::Prompt(text));
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
                // Feed the chunk straight into the session's markdown view:
                // `push_str` appends incrementally instead of re-parsing the
                // whole document on every chunk.
                let turn = self.turn_entry(session_id, cx);
                turn.answer
                    .update(cx, |state, cx| state.push_str(&chunk, cx));
                turn.answer_text.push_str(&chunk);
                turn.answer_len += chunk.chars().count();
            }
            acp::Event::Thought(chunk) => {
                let _ = chunk;
                self.turn_entry(session_id, cx).thinking = true;
            }
            acp::Event::ToolCall { id, title, status } => {
                let turn = self.turn_entry(session_id, cx);
                if let Some(run) = turn.tools.iter_mut().find(|t| t.id == id) {
                    if !status.is_empty() {
                        run.status = status;
                    }
                    if !title.is_empty() {
                        run.title = title;
                    }
                } else {
                    turn.tools.push(ToolRun {
                        id,
                        title,
                        status: if status.is_empty() { "running" } else { status },
                    });
                }
            }
            acp::Event::SessionTitle { title } => {
                if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                    session.title = title.clone();
                }
                self.store.update_session_title(session_id, &title).ok();
                cx.notify();
            }
            acp::Event::Done(_) => {
                if let Some(turn) = self.turns.remove(&session_id) {
                    // Only the answer is persisted; thinking and tool rows
                    // were transient views of the live turn.
                    let text = turn.answer_text;
                    if !text.trim().is_empty() {
                        self.store.insert_message(session_id, "agent", &text).ok();
                    }
                    if self.selected == Some(session_id) {
                        self.messages = self.store.messages(session_id);
                    }
                }
            }
            acp::Event::Error(err) => {
                self.turns.remove(&session_id);
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

    /// Get (or lazily create) the live turn state for a session. Creating one
    /// also starts a 500ms notify ticker so the working-time label keeps
    /// counting even when no ACP chunks arrive.
    fn turn_entry(&mut self, session_id: i64, cx: &mut Context<Self>) -> &mut TurnState {
        if !self.turns.contains_key(&session_id) {
            self.turns.insert(session_id, TurnState::new(cx));
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(500))
                        .await;
                    let alive = this
                        .update(cx, |app, cx| {
                            if app.turns.contains_key(&session_id) {
                                cx.notify();
                                true
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false);
                    if !alive {
                        break;
                    }
                }
            })
            .detach();
        }
        self.turns.get_mut(&session_id).unwrap()
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
        let right =
            match self.selected_session() {
                Some(session) => v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .child(self.render_conversation(session, cx))
                    .child(div().h_px().w_full().bg(theme.border))
                    .child(self.render_composer(session, cx)),
                None => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .text_color(theme.muted_foreground)
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
            .font_family(theme.font_family.clone())
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(title_bar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(div().w_px().h_full().bg(theme.border))
                    .child(right),
            )
    }
}

impl Render for KumuApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_home(window, cx)
    }
}
