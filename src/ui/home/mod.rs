//! Home window: projects → sessions tree on the left, the selected session's
//! conversation on the right. Owns the settings window handle.

mod composer;
mod conversation;
mod model_picker;
mod permission;
mod sidebar;

// Shared column width keeps the conversation and composer aligned.
const CONTENT_MAX_REM: f32 = 44.;
const CONTENT_MIN_REM: f32 = 24.;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use futures::StreamExt;

use super::{AgentModels, ModelCatalog, SettingsWindow};
use crate::acp;
use crate::agents::AGENTS;
use crate::db::{Message, Project, Session, Store, db_path};
use crate::ui::layers;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::text::TextViewState;
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

/// A permission request waiting for the user, shown above the composer while
/// its tool call is still parked on the agent's side.
struct PendingPermission {
    request_id: String,
    title: String,
    detail: String,
    options: Vec<acp::PermissionOptionInfo>,
}

/// Whether `conn_id` is still the connection registered for this session.
///
/// A session's connection is replaced when the agent is switched, so a bridge
/// task must verify identity before acting: late events and teardown from the
/// old connection must not touch the session that a newer one now owns.
fn owns_connection(conns: &HashMap<i64, acp::Connection>, session_id: i64, conn_id: u64) -> bool {
    conns
        .get(&session_id)
        .is_some_and(|conn| conn.id() == conn_id)
}

/// Which session to select after `id` is deleted.
///
/// The nearest sibling in the same project (the next one, else the previous),
/// else the first session left anywhere, else none — so the conversation pane
/// never goes blank while sessions remain.
fn neighbor_session(sessions: &[Session], id: i64) -> Option<i64> {
    let project = sessions.iter().find(|s| s.id == id)?.project_id;
    let mut siblings: Vec<i64> = sessions
        .iter()
        .filter(|s| s.project_id == project)
        .map(|s| s.id)
        .collect();
    siblings.sort_unstable();
    siblings
        .iter()
        .copied()
        .find(|&sid| sid > id)
        .or_else(|| siblings.iter().rev().copied().find(|&sid| sid < id))
        .or_else(|| sessions.iter().map(|s| s.id).find(|&sid| sid != id))
}

/// One tool call observed during the live turn, in first-seen order.
struct ToolRun {
    id: String,
    title: String,
    /// pending / running / done / failed
    status: &'static str,
    /// Flattened text output from the agent, expandable in the UI.
    content: String,
    /// Whether the content is expanded.
    open: bool,
    /// Authorization the agent is still waiting on for this call.
    pending_permission: Option<PendingPermission>,
    /// The choice the user made (the agent's own option label), kept on the row
    /// so the conversation shows how the call was authorized.
    decision: Option<String>,
}

/// Blocks stay in arrival order; tool updates mutate their original block.
enum TurnBlock {
    Thinking {
        text: String,
        open: bool,
    },
    Tool(ToolRun),
    Answer {
        text: String,
        view: Entity<TextViewState>,
    },
}

struct TurnState {
    started_at: std::time::Instant,
    blocks: Vec<TurnBlock>,
    active_thinking: Option<usize>,
}

impl TurnState {
    fn new() -> Self {
        Self {
            started_at: std::time::Instant::now(),
            blocks: Vec::new(),
            active_thinking: None,
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
    /// Message-history rows (thinking/tool) expanded by their list index.
    expanded_rows: HashSet<String>,
    messages: Vec<Message>,
    input_state: Entity<TextareaState>,
    /// Live ACP connections keyed by session id (reused across prompts).
    conns: HashMap<i64, acp::Connection>,
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
            expanded_rows: HashSet::new(),
            messages: Vec::new(),
            input_state: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(1, 6)
                    .placeholder("输入消息…")
                    // Chat composer semantics: plain Enter sends, Shift+Enter
                    // inserts a newline (the textarea handles the newline).
                    .submit_on_enter(true)
            }),
            conns: HashMap::new(),
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

    /// Ask before deleting: the session and its whole history go away for good.
    fn confirm_delete_session(&self, session_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        // The dialog outlives this call, so it reaches the view weakly.
        let weak = cx.weak_entity();
        let title = self
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| s.title.clone())
            .unwrap_or_default();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let weak = weak.clone();
            alert
                .title("删除会话")
                .description(format!(
                    "「{title}」的全部消息将被永久删除，此操作无法撤销。"
                ))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("删除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    let _ = weak.update(cx, |app, cx| app.delete_session(session_id, cx));
                    true
                })
        });
    }

    /// Delete a session: settle anything it was waiting on, drop its live
    /// connection (which tears down the agent subprocess), forget its turn
    /// state, then remove the row and its history.
    fn delete_session(&mut self, session_id: i64, cx: &mut Context<Self>) {
        // Pick the fallback while the doomed row is still in the list.
        let next = neighbor_session(&self.sessions, session_id);
        self.cancel_pending_permissions(session_id);
        self.conns.remove(&session_id);
        self.turns.remove(&session_id);
        self.store.delete_session(session_id).ok();
        self.refresh();

        if self.selected == Some(session_id) {
            match next.filter(|next| self.sessions.iter().any(|s| s.id == *next)) {
                Some(next) => self.select_session(next),
                None => {
                    self.selected = None;
                    self.messages.clear();
                }
            }
        }
        cx.notify();
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
        if let Some(conn) = self.conns.get(&session_id) {
            conn.send(acp::Cmd::Prompt(text));
        }
        cx.notify();
    }

    /// Get or open the ACP connection for a session's agent, and start the
    /// task that folds ACP events back into this view.
    fn ensure_connection(&mut self, session: &Session, cx: &mut Context<Self>) {
        if self.conns.contains_key(&session.id) {
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
        let conn = acp::spawn(cfg, cwd, ev_tx);
        // Remember which connection this bridge task belongs to, so its events
        // and teardown can be ignored once a newer one takes over the session.
        let conn_id = conn.id();
        // Re-apply the session's stored model on a fresh connection; queued
        // until the ACP thread finishes initialize + session/new.
        if let Some(model) = session.model.clone() {
            conn.send(acp::Cmd::SetModel(model));
        }
        self.conns.insert(session.id, conn);

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
                // A replaced connection (agent switched) must not feed the
                // session that a newer one now owns.
                let current = this
                    .update(cx, |app, cx| {
                        if !owns_connection(&app.conns, session_id, conn_id) {
                            return false;
                        }
                        app.on_acp_event(session_id, ev, cx);
                        true
                    })
                    .unwrap_or(false);
                if !current {
                    break;
                }
            }
            let _ = this.update(cx, |app, cx| {
                app.on_connection_closed(session_id, conn_id, cx)
            });
        })
        .detach();
    }

    /// The agent exited or the connection dropped. Forget the handle and clear
    /// any prompt still on screen: its answer can never be delivered now.
    fn on_connection_closed(&mut self, session_id: i64, conn_id: u64, cx: &mut Context<Self>) {
        // Only the session's current connection may tear it down; a stale one
        // must not remove the connection that replaced it.
        if !owns_connection(&self.conns, session_id, conn_id) {
            return;
        }
        // Settle the prompts before dropping the handle, so the agent gets an
        // answer rather than a silent disconnect.
        self.cancel_pending_permissions(session_id);
        self.conns.remove(&session_id);
        cx.notify();
    }

    /// Answer every outstanding permission prompt for a session with the
    /// protocol's `Cancelled` outcome and drop it from the rows. Used when the
    /// turn ends or the connection goes away, so the agent is never left
    /// waiting on a prompt that no longer exists in the UI.
    fn cancel_pending_permissions(&mut self, session_id: i64) {
        let request_ids: Vec<String> = self
            .turns
            .get(&session_id)
            .map(|turn| {
                turn.blocks
                    .iter()
                    .filter_map(|block| match block {
                        TurnBlock::Tool(run) => run
                            .pending_permission
                            .as_ref()
                            .map(|pending| pending.request_id.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        if let Some(conn) = self.conns.get(&session_id) {
            for request_id in &request_ids {
                conn.resolve_permission(request_id, None);
            }
        }

        if let Some(turn) = self.turns.get_mut(&session_id) {
            for block in &mut turn.blocks {
                if let TurnBlock::Tool(run) = block {
                    run.pending_permission = None;
                }
            }
        }
    }

    /// Whether any tool call in this session is still waiting on the user.
    fn has_pending_permission(&self, session_id: i64) -> bool {
        self.pending_permission_for(session_id).is_some()
    }

    /// The oldest outstanding permission prompt for a session, if any.
    ///
    /// Only one is shown at a time: the agent is blocked on the earliest request
    /// anyway, and a single card keeps the input area from growing unbounded.
    fn pending_permission_for(&self, session_id: i64) -> Option<&PendingPermission> {
        self.turns
            .get(&session_id)?
            .blocks
            .iter()
            .find_map(|block| match block {
                TurnBlock::Tool(run) => run.pending_permission.as_ref(),
                _ => None,
            })
    }

    /// Record the user's decision for a pending permission and hand it to the
    /// agent. `option_id` of `None` cancels the request.
    fn resolve_permission(
        &mut self,
        session_id: i64,
        request_id: &str,
        option_id: Option<String>,
        label: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(conn) = self.conns.get(&session_id) {
            conn.resolve_permission(request_id, option_id);
        }
        if let Some(turn) = self.turns.get_mut(&session_id) {
            for block in &mut turn.blocks {
                let TurnBlock::Tool(run) = block else {
                    continue;
                };
                let matches_request = run
                    .pending_permission
                    .as_ref()
                    .is_some_and(|p| p.request_id == request_id);
                if matches_request {
                    run.pending_permission = None;
                    run.decision = Some(label);
                    break;
                }
            }
        }
        cx.notify();
    }

    fn on_acp_event(&mut self, session_id: i64, ev: acp::Event, cx: &mut Context<Self>) {
        match ev {
            acp::Event::Agent(chunk) => {
                // Feed the chunk straight into the session's markdown view:
                // `push_str` appends incrementally instead of re-parsing the
                // whole document on every chunk.
                let turn = self.turn_entry(session_id, cx);
                turn.active_thinking = None;
                if let Some(TurnBlock::Answer { text, view }) = turn.blocks.last_mut() {
                    view.update(cx, |state, cx| state.push_str(&chunk, cx));
                    text.push_str(&chunk);
                } else {
                    let view = cx.new(|cx| TextViewState::markdown(&chunk, cx));
                    turn.blocks.push(TurnBlock::Answer { text: chunk, view });
                }
            }
            acp::Event::Thought(chunk) => {
                let turn = self.turn_entry(session_id, cx);
                if let Some(TurnBlock::Thinking { text, .. }) = turn
                    .active_thinking
                    .and_then(|index| turn.blocks.get_mut(index))
                {
                    text.push_str(&chunk);
                } else {
                    turn.active_thinking = Some(turn.blocks.len());
                    turn.blocks.push(TurnBlock::Thinking {
                        text: chunk,
                        open: false,
                    });
                }
            }
            acp::Event::ToolCall {
                id,
                title,
                status,
                content,
            } => {
                let turn = self.turn_entry(session_id, cx);
                turn.active_thinking = None;
                if let Some(run) = turn.blocks.iter_mut().find_map(|block| match block {
                    TurnBlock::Tool(run) if run.id == id => Some(run),
                    _ => None,
                }) {
                    if !status.is_empty() {
                        run.status = status;
                    }
                    if !title.is_empty() {
                        run.title = title;
                    }
                    if !content.is_empty() {
                        run.content = content;
                    }
                } else {
                    turn.blocks.push(TurnBlock::Tool(ToolRun {
                        id,
                        title,
                        status: if status.is_empty() { "running" } else { status },
                        content,
                        open: false,
                        pending_permission: None,
                        decision: None,
                    }));
                }
            }
            acp::Event::PermissionRequest {
                tool_call_id,
                request_id,
                title,
                detail,
                options,
            } => {
                let conn = self.conns.get(&session_id).cloned();
                let turn = self.turn_entry(session_id, cx);
                turn.active_thinking = None;
                let index = match turn.blocks.iter().position(
                    |block| matches!(block, TurnBlock::Tool(run) if run.id == tool_call_id),
                ) {
                    Some(index) => index,
                    None => {
                        // The request can arrive before the tool call it refers
                        // to; seed the row from what the request itself carries
                        // so the prompt is never dropped.
                        turn.blocks.push(TurnBlock::Tool(ToolRun {
                            id: tool_call_id,
                            title: title.clone(),
                            status: "pending",
                            content: String::new(),
                            open: false,
                            pending_permission: None,
                            decision: None,
                        }));
                        turn.blocks.len() - 1
                    }
                };
                if let Some(TurnBlock::Tool(run)) = turn.blocks.get_mut(index) {
                    // One prompt per row: settle any earlier one as cancelled.
                    if let (Some(previous), Some(conn)) = (run.pending_permission.take(), conn) {
                        conn.resolve_permission(&previous.request_id, None);
                    }
                    run.pending_permission = Some(PendingPermission {
                        request_id,
                        title,
                        detail,
                        options,
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
                // The turn is over, so nothing can still be authorized.
                self.cancel_pending_permissions(session_id);
                if let Some(turn) = self.turns.remove(&session_id) {
                    for block in turn.blocks {
                        let (role, text) = match block {
                            TurnBlock::Thinking { text, .. } => ("thinking", text),
                            TurnBlock::Answer { text, .. } => ("agent", text),
                            TurnBlock::Tool(run) => (
                                "tool",
                                serde_json::json!({
                                    "title": run.title,
                                    "status": run.status,
                                    "content": run.content,
                                    "decision": run.decision,
                                })
                                .to_string(),
                            ),
                        };
                        if !text.trim().is_empty() {
                            self.store.insert_message(session_id, role, &text).ok();
                        }
                    }
                    if self.selected == Some(session_id) {
                        self.messages = self.store.messages(session_id);
                    }
                }
            }
            acp::Event::Error(err) => {
                self.cancel_pending_permissions(session_id);
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
            self.turns.insert(session_id, TurnState::new());
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

    fn render_home(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let overlays = layers::window_layers(window, cx);
        let theme = cx.theme();
        let right =
            match self.selected_session() {
                Some(session) => v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .child(self.render_conversation(session, cx))
                    .child(div().h_px().w_full().bg(theme.border))
                    // A pending authorization sits directly above the input, so
                    // the decision is always in reach.
                    .children(
                        self.pending_permission_for(session.id)
                            .map(|pending| self.render_permission_card(session.id, pending, cx)),
                    )
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
            .child(overlays)
    }
}

impl Render for KumuApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_home(window, cx)
    }
}

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use core::prelude::v1::test;

    use super::{neighbor_session, owns_connection};
    use crate::acp;
    use crate::db::Session;
    use std::collections::HashMap;

    fn session(id: i64, project_id: i64) -> Session {
        Session {
            id,
            project_id,
            agent: "omp".into(),
            title: format!("会话 {id}"),
            cwd: "/tmp".into(),
            model: None,
        }
    }

    /// A session's connection is replaced when the agent is switched. Events
    /// and teardown from the replaced connection must not be attributed to the
    /// session, or they would clobber the connection that succeeded it.
    #[test]
    fn only_the_registered_connection_owns_a_session() {
        let mut conns: HashMap<i64, acp::Connection> = HashMap::new();
        conns.insert(7, acp::Connection::detached_for_test(1));

        assert!(owns_connection(&conns, 7, 1));
        assert!(!owns_connection(&conns, 7, 2), "another connection");
        assert!(!owns_connection(&conns, 8, 1), "another session");

        // Replacement under the same session id: the old connection is stale.
        conns.insert(7, acp::Connection::detached_for_test(2));
        assert!(
            !owns_connection(&conns, 7, 1),
            "the replaced connection must not match"
        );
        assert!(owns_connection(&conns, 7, 2));

        // Once gone, nothing owns the session.
        conns.remove(&7);
        assert!(!owns_connection(&conns, 7, 2));
    }

    /// Deleting the session the user is looking at must land them next door,
    /// not on a blank pane.
    #[test]
    fn deletion_falls_back_to_the_nearest_sibling() {
        let sessions = vec![session(1, 10), session(2, 10), session(3, 10)];

        assert_eq!(neighbor_session(&sessions, 2), Some(3), "the next one");
        assert_eq!(
            neighbor_session(&sessions, 3),
            Some(2),
            "no next: the previous one"
        );
        assert_eq!(
            neighbor_session(&sessions, 1),
            Some(2),
            "the first one skips forward too"
        );
    }

    /// With no sibling left in the project, the other project's sessions still
    /// beat the empty state; only a truly empty list returns `None`.
    #[test]
    fn deletion_falls_back_across_projects_then_to_nothing() {
        let sessions = vec![session(1, 10), session(2, 20)];

        assert_eq!(neighbor_session(&sessions, 1), Some(2), "another project");
        assert_eq!(neighbor_session(&sessions, 2), Some(1), "and back");
        assert_eq!(neighbor_session(&[session(1, 10)], 1), None, "the last one");
        assert_eq!(neighbor_session(&[], 1), None, "unknown id");
    }
}
