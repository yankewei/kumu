//! Home window: projects → sessions tree on the left, the selected session's
//! conversation on the right. Owns the settings window handle.

mod access_picker;
mod command_menu;
mod composer;
mod conversation;
mod mode_picker;
mod model_picker;
mod permission;
mod reasoning_picker;
mod sandbox_picker;
mod sidebar;

// Shared column width keeps the conversation and composer aligned.
const CONTENT_MAX_REM: f32 = 44.;
const CONTENT_MIN_REM: f32 = 24.;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use futures::StreamExt;

use self::command_menu::matching_commands;
use self::conversation::{ConversationView, ToolRun, TurnBlock};
use super::{AgentCommands, AgentSessionOptions, SessionOptions, SettingsWindow};
use crate::agents::AGENTS;
use crate::backend;
use crate::db::{Project, Session, Store, db_path};
use crate::ui::layers;
use crate::ui::tooltip::Tooltips;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputEvent, InputState, TextareaState};
use gpui_kit::component::text::TextViewState;
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

/// A permission request waiting for the user, shown above the composer while
/// its tool call is still parked on the agent's side.
#[derive(Clone)]
struct PendingPermission {
    request_id: String,
    title: String,
    detail: String,
    options: Vec<backend::PermissionOptionInfo>,
}

/// Whether `conn_id` is still the connection registered for this session.
///
/// A session's connection is replaced when the agent is switched, so a bridge
/// task must verify identity before acting: late events and teardown from the
/// old connection must not touch the session that a newer one now owns.
fn owns_connection(
    conns: &HashMap<i64, backend::Connection>,
    session_id: i64,
    conn_id: u64,
) -> bool {
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

/// What picking a folder for a new project resolves to.
#[derive(Debug, PartialEq)]
enum ProjectPick {
    /// The folder is already a project; select it instead of inserting a
    /// duplicate row.
    Existing(i64),
    /// A folder not seen before; its own name becomes the project's.
    New { name: String },
}

/// Resolve a picked folder against the known projects — see [`ProjectPick`].
fn resolve_project_pick(projects: &[Project], path: &str) -> ProjectPick {
    if let Some(existing) = projects.iter().find(|p| p.path == path) {
        return ProjectPick::Existing(existing.id);
    }
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    ProjectPick::New { name }
}

pub struct KumuApp {
    store: Store,
    settings_window: Option<WindowHandle<Root>>,
    projects: Vec<Project>,
    sessions: Vec<Session>,
    selected: Option<i64>,
    /// Projects collapsed in the sidebar tree.
    collapsed_projects: HashSet<i64>,
    /// The selected session's history and every session's live turn. Its own
    /// entity so streaming and animation repaint that column alone.
    conversation: Entity<ConversationView>,
    input_state: Entity<TextareaState>,
    /// Live ACP connections keyed by session id (reused across prompts).
    conns: HashMap<i64, backend::Connection>,
    /// Which picker's menu is open, if any. One at a time — see
    /// [`KumuApp::set_picker_open`].
    open_picker: Option<PickerAxis>,
    /// Model picker state: which agent's rail tab is active (None = the
    /// session's current agent), and the search field that no other picker has
    /// (the other lists are short enough to read whole).
    model_picker_tab: Option<&'static str>,
    model_search: Entity<InputState>,
    /// Hover labels for the sidebar tree (see [`Tooltips`]).
    tooltips: Entity<Tooltips>,
}

/// Which composer picker's menu is open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PickerAxis {
    Model,
    Reasoning,
    Access,
    Mode,
    Sandbox,
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
            conversation: cx.new(ConversationView::new),
            input_state: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(1, 6)
                    .placeholder("输入消息…")
                    // Chat composer semantics: plain Enter sends, Shift+Enter
                    // inserts a newline (the textarea handles the newline).
                    .submit_on_enter(true)
            }),
            conns: HashMap::new(),
            open_picker: None,
            model_picker_tab: None,
            model_search: cx.new(|cx| InputState::new(window, cx).placeholder("搜索模型…")),
            tooltips: cx.new(|cx| Tooltips::new(cx)),
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
        app.bootstrap(cx);
        app.probe_models(cx);
        app
    }

    /// Probe agents whose session options aren't cached yet, so the composer's
    /// model and mode pickers have data even before settings was opened.
    fn probe_models(&mut self, cx: &mut Context<Self>) {
        let mut missing = Vec::new();
        if cx.has_global::<AgentSessionOptions>() {
            for a in AGENTS.iter() {
                if !cx.global::<AgentSessionOptions>().0.contains_key(a.id) {
                    missing.push(a.id);
                }
            }
        } else {
            cx.set_global(AgentSessionOptions::default());
            missing.extend(AGENTS.iter().map(|a| a.id));
        }
        if missing.is_empty() {
            return;
        }

        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<backend::Event>();
        for agent in missing {
            if let Some(cfg) = backend::Config::for_agent(agent) {
                backend::probe(agent, cfg, ev_tx.clone());
            }
        }
        drop(ev_tx);

        let (bridge_tx, mut bridge_rx) = futures::channel::mpsc::unbounded::<backend::Event>();
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

    fn bootstrap(&mut self, cx: &mut Context<Self>) {
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
                self.select_session(s.id, cx);
            }
        }
    }

    fn refresh(&mut self) {
        self.projects = self.store.projects();
        self.sessions = self.store.sessions();
    }

    fn select_session(&mut self, id: i64, cx: &mut Context<Self>) {
        self.selected = Some(id);
        let messages = self.store.messages(id);
        self.conversation.update(cx, |conversation, cx| {
            conversation.show(Some(id), &messages, cx)
        });
    }

    /// Re-read a session's persisted messages into the conversation, which
    /// ignores them unless that session is on screen.
    fn reload_messages(&mut self, session_id: i64, cx: &mut Context<Self>) {
        if self.selected != Some(session_id) {
            return;
        }
        let messages = self.store.messages(session_id);
        self.conversation.update(cx, |conversation, cx| {
            conversation.reload_history(session_id, &messages, cx)
        });
    }

    fn new_session(&mut self, cx: &mut Context<Self>) {
        let project_id = self.projects.first().map(|p| p.id).unwrap_or_else(|| {
            self.store
                .insert_project("kumu", "cwd-not-set")
                .unwrap_or(1)
        });
        self.new_session_in(project_id, cx);
    }

    /// Add a project from a native folder picker; the picked folder's own
    /// name becomes the project name, matching how Zed/waku open a project
    /// (no separate naming step). Picking a folder that's already a project
    /// selects it instead of inserting a duplicate row.
    fn add_project(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择项目目录".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |app, cx| app.finish_add_project(path, cx));
        })
        .detach();
    }

    fn finish_add_project(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let path = path.to_string_lossy().into_owned();
        match resolve_project_pick(&self.projects, &path) {
            ProjectPick::Existing(id) => {
                self.collapsed_projects.remove(&id);
                cx.notify();
            }
            ProjectPick::New { name } => {
                if self.store.insert_project(&name, &path).is_ok() {
                    self.refresh();
                    cx.notify();
                }
            }
        }
    }

    fn new_session_in(&mut self, project_id: i64, cx: &mut Context<Self>) {
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
            self.select_session(id, cx);
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

    /// Rename a session: the dialog's field starts on the current name, so the
    /// name can be replaced in one go.
    fn prompt_rename_session(&self, session_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let title = self
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| s.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(title));
        // The dialog outlives this call, so it reaches the view weakly.
        let weak = cx.weak_entity();
        let dialog_input = input.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("重命名会话")
                .child(Input::new(&dialog_input).small())
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("保存")
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .on_ok({
                    let weak = weak.clone();
                    let input = dialog_input.clone();
                    move |_, _, cx| {
                        let name = input.read(cx).value().trim().to_string();
                        if !name.is_empty() {
                            let _ = weak.update(cx, move |app, cx| {
                                app.rename_session(session_id, name, cx)
                            });
                        }
                        true
                    }
                })
        });
        // Focusing after opening leaves the input the last handle the window
        // focused; the dialog only traps Tab, so nothing steals it back.
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
    }

    /// Apply the name the user chose. It is marked as theirs, which is what
    /// keeps a title generated later from replacing it.
    fn rename_session(&mut self, session_id: i64, title: String, cx: &mut Context<Self>) {
        self.store.rename_session(session_id, &title).ok();
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.title = title;
            session.title_manual = true;
        }
        cx.notify();
    }

    /// Take a title generated for the session — kumu's fallback from the first
    /// message, or the agent's own.
    ///
    /// The store decides whether it may replace the current name, and a name the
    /// user chose wins; the in-memory list follows that verdict rather than
    /// deciding for itself, so the two cannot drift apart.
    fn apply_generated_title(&mut self, session_id: i64, title: &str) {
        if !self
            .store
            .update_session_title(session_id, title)
            .unwrap_or(false)
        {
            return;
        }
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.title = title.to_string();
        }
    }

    /// Delete a session: settle anything it was waiting on, drop its live
    /// connection (which tears down the agent subprocess), forget its turn
    /// state, then remove the row and its history.
    fn delete_session(&mut self, session_id: i64, cx: &mut Context<Self>) {
        // Pick the fallback while the doomed row is still in the list.
        let next = neighbor_session(&self.sessions, session_id);
        self.cancel_pending_permissions(session_id, cx);
        self.conns.remove(&session_id);
        self.conversation.update(cx, |conversation, cx| {
            conversation.take_turn(session_id, cx)
        });
        self.store.delete_session(session_id).ok();
        self.refresh();

        if self.selected == Some(session_id) {
            match next.filter(|next| self.sessions.iter().any(|s| s.id == *next)) {
                Some(next) => self.select_session(next, cx),
                None => {
                    self.selected = None;
                    self.conversation
                        .update(cx, |conversation, cx| conversation.show(None, &[], cx));
                }
            }
        }
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected else {
            return;
        };
        // The raw value rather than the trimmed one: a completed command ends
        // with a space, and that space is what closes the menu — trimming it away
        // would have the next Enter complete the same command again.
        let typed = self.input_state.read(cx).value();
        let text = typed.trim().to_string();
        if text.is_empty() || self.conversation.read(cx).has_turn(session_id) {
            return;
        }
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        // Enter completes while the input is still a bare command, and the menu's
        // first row is what it completes to, so the effect is visible before the
        // key is pressed. Sending takes a second Enter.
        if let Some(command) =
            matching_commands(&self.session_commands(&session, cx), &typed).first()
        {
            let name = command.name.clone();
            self.complete_command(&name, window, cx);
            return;
        }
        self.conversation
            .update(cx, |conversation, cx| conversation.scroll_to_end(cx));
        // Until the agent reports a real title (session/info), fall back to
        // the first line of the first user message.
        if session.title.starts_with("会话 ") {
            let line = text.lines().next().unwrap_or(&text).trim();
            if !line.is_empty() {
                let mut title: String = line.chars().take(24).collect();
                if line.chars().count() > 24 {
                    title.push('…');
                }
                self.apply_generated_title(session_id, &title);
            }
        }
        self.store.insert_message(session_id, "user", &text).ok();
        // The first message is what fixes the session's agent, so the list is
        // re-read here rather than letting the flag lag a turn behind the
        // picker that reads it.
        self.refresh();
        self.reload_messages(session_id, cx);
        self.input_state
            .update(cx, |state, cx| state.set_value("", window, cx));

        self.ensure_connection(&session, cx);
        // No connection means nothing will ever answer this prompt (an agent
        // this build no longer supports, say), so leave it looking delivered
        // rather than in flight. Still repaint: `ensure_connection` may have
        // just written the notice that explains why.
        let Some(conn) = self.conns.get(&session_id).cloned() else {
            cx.notify();
            return;
        };
        // Open the turn here rather than waiting for the first agent event:
        // spawning the subprocess and streaming the first token can take a
        // while, and until then the conversation would show no sign that the
        // prompt was accepted.
        self.conversation.update(cx, |conversation, cx| {
            conversation.update_turn(session_id, cx, |_, _| ())
        });
        conn.send(backend::Cmd::Prompt(text));
        cx.notify();
    }

    /// Stop the turn in flight, if there is one.
    ///
    /// The cancel travels out of band rather than as a [`backend::Cmd`]: the
    /// backend's command loop is parked on the very turn being cancelled, so a
    /// command would not be read until that turn had already ended. The turn
    /// stays open here until the backend reports it settled, because the agent is
    /// still running until it says otherwise.
    fn cancel_turn(&mut self) {
        let Some(session_id) = self.selected else {
            return;
        };
        if let Some(conn) = self.conns.get(&session_id) {
            conn.cancel();
        }
    }

    /// Get or open the ACP connection for a session's agent, and start the
    /// task that folds ACP events back into this view.
    fn ensure_connection(&mut self, session: &Session, cx: &mut Context<Self>) {
        if self.conns.contains_key(&session.id) {
            return;
        }
        let agent_id = AGENTS.iter().find(|a| a.id == session.agent).map(|a| a.id);
        let Some(agent_id) = agent_id else {
            self.system_message(session.id, format!("未支持的 agent：{}", session.agent), cx);
            return;
        };
        let Some(cfg) = backend::Config::for_agent(agent_id) else {
            return;
        };
        let cwd = if session.cwd.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
        } else {
            PathBuf::from(&session.cwd)
        };

        // The session's stored picks travel both ways: on the command line for
        // the backend that takes an axis at launch (Claude Code), and over the
        // command channel below for the ones that apply theirs after their
        // handshake. Whether either applies is the backend's business — the UI
        // only says what the session is set to.
        let picks = backend::SessionPicks {
            model: session.model.clone(),
            mode: session.mode.clone(),
            reasoning: session.reasoning.clone(),
        };
        let cfg = cfg.with_session(picks);

        // ACP thread -> std channel -> forwarder thread -> async channel ->
        // UI task (same bridge pattern as the settings window's probes).
        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<backend::Event>();
        let conn = backend::spawn(cfg, cwd, ev_tx);
        // Remember which connection this bridge task belongs to, so its events
        // and teardown can be ignored once a newer one takes over the session.
        let conn_id = conn.id();
        // Re-apply the session's stored picks on a fresh connection; queued
        // until the agent thread has finished its handshake. A backend that
        // already launched with them (Claude Code) sees the same value twice
        // and skips the repeat.
        if let Some(model) = session.model.clone() {
            conn.send(backend::Cmd::SetModel(model));
        }
        if let Some(mode) = session.mode.clone() {
            conn.send(backend::Cmd::SetMode(mode));
        }
        if let Some(sandbox) = session.sandbox.clone() {
            conn.send(backend::Cmd::SetSandbox(sandbox));
        }
        if let Some(reasoning) = session.reasoning.clone() {
            conn.send(backend::Cmd::SetReasoning(reasoning));
        }
        self.conns.insert(session.id, conn);

        let (bridge_tx, mut bridge_rx) = futures::channel::mpsc::unbounded::<backend::Event>();
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
        self.cancel_pending_permissions(session_id, cx);
        // Drop the in-flight turn with the connection: nothing can deliver its
        // answer now, so the working indicator must not keep counting. The
        // normal paths have already dropped it (a turn ends with `Done`, and a
        // failure reports `Error` first); this covers a connection that died
        // without either, which would otherwise leave the turn — and the
        // composer, which refuses to send while one exists — stuck for good.
        self.conversation.update(cx, |conversation, cx| {
            conversation.take_turn(session_id, cx)
        });
        self.conns.remove(&session_id);
        cx.notify();
    }

    /// Answer every outstanding permission prompt for a session with the
    /// protocol's `Cancelled` outcome and drop it from the rows. Used when the
    /// turn ends or the connection goes away, so the agent is never left
    /// waiting on a prompt that no longer exists in the UI.
    fn cancel_pending_permissions(&mut self, session_id: i64, cx: &mut Context<Self>) {
        let request_ids: Vec<String> = self
            .conversation
            .update(cx, |conversation, cx| {
                conversation.with_turn(session_id, cx, |turn| {
                    turn.blocks
                        .iter_mut()
                        .filter_map(|block| match block {
                            TurnBlock::Tool(run) => run
                                .pending_permission
                                .take()
                                .map(|pending| pending.request_id),
                            _ => None,
                        })
                        .collect()
                })
            })
            .unwrap_or_default();

        if let Some(conn) = self.conns.get(&session_id) {
            for request_id in &request_ids {
                conn.resolve_permission(request_id, None);
            }
        }
    }

    /// Whether any tool call in this session is still waiting on the user.
    fn has_pending_permission(&self, session_id: i64, cx: &App) -> bool {
        self.pending_permission_for(session_id, cx).is_some()
    }

    /// The oldest outstanding permission prompt for a session, if any.
    ///
    /// Only one is shown at a time: the agent is blocked on the earliest request
    /// anyway, and a single card keeps the input area from growing unbounded.
    fn pending_permission_for(&self, session_id: i64, cx: &App) -> Option<PendingPermission> {
        self.conversation
            .read(cx)
            .turn(session_id)?
            .blocks
            .iter()
            .find_map(|block| match block {
                TurnBlock::Tool(run) => run.pending_permission.clone(),
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
        self.conversation.update(cx, |conversation, cx| {
            conversation.with_turn(session_id, cx, |turn| {
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
            })
        });
        cx.notify();
    }

    fn on_acp_event(&mut self, session_id: i64, ev: backend::Event, cx: &mut Context<Self>) {
        // A turn opening or closing changes the composer (send ↔ stop) and the
        // command menu, which this view draws; a chunk streaming into a turn
        // that is already open only changes the conversation, which repaints
        // itself.
        let had_turn = self.conversation.read(cx).has_turn(session_id);
        match ev {
            backend::Event::Agent(chunk) => {
                // Feed the chunk straight into the session's markdown view:
                // `push_str` appends incrementally instead of re-parsing the
                // whole document on every chunk.
                self.conversation.update(cx, |conversation, cx| {
                    conversation.update_turn(session_id, cx, |turn, cx| {
                        turn.active_thinking = None;
                        if let Some(TurnBlock::Answer { text, view }) = turn.blocks.last_mut() {
                            view.update(cx, |state, cx| state.push_str(&chunk, cx));
                            text.push_str(&chunk);
                        } else {
                            let view = cx.new(|cx| TextViewState::markdown(&chunk, cx));
                            // The appended text parses in the background; repaint
                            // the conversation when it lands.
                            cx.observe(&view, |_, _, cx| cx.notify()).detach();
                            turn.blocks.push(TurnBlock::Answer { text: chunk, view });
                        }
                    })
                });
            }
            backend::Event::Thought { text, message } => {
                self.conversation.update(cx, |conversation, cx| {
                    conversation.update_turn(session_id, cx, |turn, _| {
                        // The row that took the chunk is the row still being
                        // written, so the spinner follows it there.
                        let row = turn.push_thinking(text, message);
                        turn.active_thinking = Some(row);
                    })
                });
            }
            backend::Event::ToolCall {
                id,
                title,
                status,
                content,
            } => {
                self.conversation.update(cx, |conversation, cx| {
                    conversation.update_turn(session_id, cx, |turn, _| {
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
                            // Opening a row ends the reasoning above it. An update
                            // to a row already there does not: an agent reporting
                            // a background task's progress would otherwise close
                            // the reasoning it interrupts, once per report.
                            turn.active_thinking = None;
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
                    })
                });
            }
            backend::Event::PermissionRequest {
                tool_call_id,
                request_id,
                title,
                detail,
                options,
            } => {
                let previous = self.conversation.update(cx, |conversation, cx| {
                    conversation.update_turn(session_id, cx, |turn, _| {
                        let index = match turn.blocks.iter().position(
                            |block| matches!(block, TurnBlock::Tool(run) if run.id == tool_call_id),
                        ) {
                            Some(index) => index,
                            None => {
                                // The request can arrive before the tool call it
                                // refers to; seed the row from what the request
                                // itself carries so the prompt is never dropped.
                                // Seeding a row ends the reasoning above it, the
                                // same as any other call opening one.
                                turn.active_thinking = None;
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
                        let TurnBlock::Tool(run) = &mut turn.blocks[index] else {
                            return None;
                        };
                        run.pending_permission
                            .replace(PendingPermission {
                                request_id,
                                title,
                                detail,
                                options,
                            })
                            .map(|previous| previous.request_id)
                    })
                });
                // One prompt per row: settle any earlier one as cancelled.
                if let (Some(previous), Some(conn)) = (previous, self.conns.get(&session_id)) {
                    conn.resolve_permission(&previous, None);
                }
                // The card above the composer and the sidebar's marker.
                cx.notify();
            }
            backend::Event::SessionTitle { title } => {
                self.apply_generated_title(session_id, &title);
                cx.notify();
            }
            backend::Event::Done(_) => {
                // The turn is over, so nothing can still be authorized.
                self.cancel_pending_permissions(session_id, cx);
                let turn = self.conversation.update(cx, |conversation, cx| {
                    conversation.take_turn(session_id, cx)
                });
                if let Some(turn) = turn {
                    let elapsed = turn.elapsed();
                    // The message the turn finished on, which is where its total
                    // is recorded — and so where the settled label is rebuilt
                    // from when the history is read back.
                    let mut closed = None;
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
                            closed = self.store.insert_message(session_id, role, &text).ok();
                        }
                    }
                    if let Some(id) = closed {
                        self.store
                            .set_message_duration(id, elapsed.as_millis() as i64)
                            .ok();
                    }
                }
                self.reload_messages(session_id, cx);
                cx.notify();
            }
            backend::Event::Error(err) => {
                self.cancel_pending_permissions(session_id, cx);
                self.conversation.update(cx, |conversation, cx| {
                    conversation.take_turn(session_id, cx)
                });
                self.system_message(session_id, err, cx);
                cx.notify();
            }
            backend::Event::Commands { agent, commands } => {
                if !cx.has_global::<AgentCommands>() {
                    cx.set_global(AgentCommands::default());
                }
                cx.global_mut::<AgentCommands>().0.insert(agent, commands);
                cx.notify();
            }
            // The axes are independent — an agent may offer any subset of them —
            // so an entirely empty report is not worth caching.
            backend::Event::SessionOptions {
                agent,
                models,
                current_model,
                modes,
                current_mode,
                access,
                current_access,
                sandboxes,
                current_sandbox,
                reasoning,
                current_reasoning,
            } if !models.is_empty()
                || !modes.is_empty()
                || !sandboxes.is_empty()
                || !reasoning.is_empty() =>
            {
                if !cx.has_global::<AgentSessionOptions>() {
                    cx.set_global(AgentSessionOptions::default());
                }
                cx.global_mut::<AgentSessionOptions>().0.insert(
                    agent,
                    SessionOptions {
                        models,
                        current_model,
                        modes,
                        current_mode,
                        access,
                        current_access,
                        sandboxes,
                        current_sandbox,
                        reasoning,
                        current_reasoning,
                    },
                );
                cx.notify();
            }
            _ => {}
        }
        if had_turn != self.conversation.read(cx).has_turn(session_id) {
            cx.notify();
        }
    }

    /// Persist and show a system notice (unsupported agent, ACP error…).
    fn system_message(&mut self, session_id: i64, text: String, cx: &mut Context<Self>) {
        self.store.insert_message(session_id, "system", &text).ok();
        self.reload_messages(session_id, cx);
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

    /// Record a picker opening or closing.
    ///
    /// Only one is open at a time: the chips sit in a row and their menus
    /// overlap. A close only clears the flag when it is *that* picker closing —
    /// opening a second chip dismisses the first, and the two reports can arrive
    /// in either order, so clearing unconditionally would drop the menu the user
    /// just opened.
    fn set_picker_open(&mut self, axis: PickerAxis, open: bool) {
        self.open_picker = if open {
            Some(axis)
        } else if self.open_picker == Some(axis) {
            None
        } else {
            self.open_picker
        };
    }

    fn render_home(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let overlays = layers::window_layers(&self.tooltips, window, cx);
        let theme = cx.theme();
        let right =
            match self.selected_session() {
                Some(session) => v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .child(
                        h_flex()
                            .items_center()
                            .px_4()
                            .py_2()
                            .child(div().text_sm().font_medium().child(session.title.clone())),
                    )
                    // Cached: this frame reuses the conversation's last paint
                    // unless the conversation itself was notified.
                    .child(
                        self.conversation.clone().cached(
                            StyleRefinement::default()
                                .flex_1()
                                .min_h_0()
                                .min_w_0()
                                .w_full(),
                        ),
                    )
                    .child(div().h_px().w_full().bg(theme.border))
                    // A pending authorization sits directly above the input, so
                    // the decision is always in reach.
                    .children(
                        self.pending_permission_for(session.id, cx)
                            .map(|pending| self.render_permission_card(session.id, &pending, cx)),
                    )
                    // The command menu goes closest to the input: it is about
                    // what is being typed, and it is gone the moment the message
                    // is sent, while the card above it answers a decision the
                    // agent is still blocked on.
                    .children(self.command_menu(session, cx))
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
                            this.new_session(cx);
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

    use super::{ProjectPick, neighbor_session, owns_connection, resolve_project_pick};
    use crate::backend;
    use crate::db::{Project, Session};
    use std::collections::HashMap;

    fn session(id: i64, project_id: i64) -> Session {
        Session {
            id,
            project_id,
            agent: "omp".into(),
            title: format!("会话 {id}"),
            cwd: "/tmp".into(),
            model: None,
            mode: None,
            sandbox: None,
            reasoning: None,
            title_manual: false,
            has_conversation: false,
        }
    }

    fn project(id: i64, path: &str) -> Project {
        Project {
            id,
            name: format!("project-{id}"),
            path: path.into(),
        }
    }

    /// Picking a folder that is already a project selects it rather than
    /// inserting a duplicate row.
    #[test]
    fn resolve_project_pick_selects_an_existing_folder() {
        let projects = vec![project(1, "/repo/a"), project(2, "/repo/b")];
        assert_eq!(
            resolve_project_pick(&projects, "/repo/b"),
            ProjectPick::Existing(2)
        );
    }

    /// A folder not seen before is a new project named after itself.
    #[test]
    fn resolve_project_pick_names_a_new_folder_after_itself() {
        let projects = vec![project(1, "/repo/a")];
        assert_eq!(
            resolve_project_pick(&projects, "/repo/kumu"),
            ProjectPick::New {
                name: "kumu".to_string()
            }
        );
    }

    /// A path with no file name of its own (the filesystem root) still
    /// resolves to something rather than an empty label.
    #[test]
    fn resolve_project_pick_falls_back_to_the_whole_path() {
        assert_eq!(
            resolve_project_pick(&[], "/"),
            ProjectPick::New {
                name: "/".to_string()
            }
        );
    }

    /// A session's connection is replaced when the agent is switched. Events
    /// and teardown from the replaced connection must not be attributed to the
    /// session, or they would clobber the connection that succeeded it.
    #[test]
    fn only_the_registered_connection_owns_a_session() {
        let mut conns: HashMap<i64, backend::Connection> = HashMap::new();
        conns.insert(7, backend::Connection::detached_for_test(1));

        assert!(owns_connection(&conns, 7, 1));
        assert!(!owns_connection(&conns, 7, 2), "another connection");
        assert!(!owns_connection(&conns, 8, 1), "another session");

        // Replacement under the same session id: the old connection is stale.
        conns.insert(7, backend::Connection::detached_for_test(2));
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
