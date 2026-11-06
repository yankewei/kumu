//! A session's flow: sending a prompt, the connection that carries it, the
//! events the agent reports back, and permission bookkeeping.

use std::collections::HashMap;
use std::path::PathBuf;

use futures::StreamExt;
use gpui_kit::Context;

use super::discovery;
use super::turn::{PendingPermission, Turn};
use super::{Workspace, WorkspaceEvent};
use crate::agent;
use crate::db::Session;

/// Whether `conn_id` is still the connection registered for this session.
///
/// A session's connection is replaced when the agent is switched, so a bridge
/// task must verify identity before acting: late events and teardown from the
/// old connection must not touch the session that a newer one now owns.
fn owns_connection(conns: &HashMap<i64, agent::Connection>, session_id: i64, conn_id: u64) -> bool {
    conns
        .get(&session_id)
        .is_some_and(|conn| conn.id() == conn_id)
}

/// A new usage report over the last one: the report wins, except where it does
/// not know the window and the last one did (see [`agent::Usage`]).
fn merge_usage(previous: Option<&agent::Usage>, mut usage: agent::Usage) -> agent::Usage {
    if usage.window.is_none() {
        usage.window = previous.and_then(|p| p.window);
    }
    usage
}

/// The title a session falls back to from its first message, until the agent
/// names it: the first line, cut at 24 characters.
fn fallback_title(text: &str) -> Option<String> {
    let line = text.lines().next().unwrap_or(text).trim();
    if line.is_empty() {
        return None;
    }
    let mut title: String = line.chars().take(24).collect();
    if line.chars().count() > 24 {
        title.push('…');
    }
    Some(title)
}

impl Workspace {
    /// Send a prompt to the session's agent, returning whether it was taken.
    ///
    /// Refused when there is neither text nor an image, or the session already
    /// has a turn in flight: one turn per session at a time, while other
    /// sessions run on their own.
    pub fn send(
        &mut self,
        session_id: i64,
        text: &str,
        images: Vec<agent::ImageAttachment>,
        cx: &mut Context<Self>,
    ) -> bool {
        let text = text.trim();
        if (text.is_empty() && images.is_empty()) || self.has_turn(session_id) {
            return false;
        }
        let Some(session) = self.session(session_id).cloned() else {
            return false;
        };
        // Until the agent reports a real title (session/info), fall back to
        // the first line of the first user message.
        if session.title.starts_with("会话 ") {
            let title = fallback_title(text).unwrap_or_else(|| "图片".to_string());
            self.apply_generated_title(session_id, &title);
        }
        let paths: Vec<String> = images
            .iter()
            .map(|image| image.path.to_string_lossy().into_owned())
            .collect();
        self.store
            .insert_user_message(session_id, text, &paths)
            .ok();
        // The first message is what fixes the session's agent, so the list is
        // re-read here rather than letting the flag lag a turn behind.
        self.refresh();
        cx.emit(WorkspaceEvent::HistoryChanged(session_id));
        cx.emit(WorkspaceEvent::Changed);

        self.ensure_connection(&session, cx);
        // Opening or reusing this prompt's connection is the only way the pool
        // grows, so it is the moment to close the least recently used idle ones
        // back down to the cap.
        if self.conns.contains_key(&session_id) {
            self.reclaim_idle_connections(session_id, cx);
        }
        // No connection means nothing will ever answer this prompt (an agent
        // this build no longer supports, say), so leave it looking delivered
        // rather than in flight; `ensure_connection` has written the notice
        // that explains why.
        let Some(conn) = self.conns.get(&session_id).cloned() else {
            return true;
        };
        // Open the turn here rather than waiting for the first agent event:
        // spawning the subprocess and streaming the first token can take a
        // while, and until then the conversation would show no sign that the
        // prompt was accepted.
        self.update_turn(session_id, cx, |_| ());
        conn.send(agent::Cmd::Prompt(agent::Prompt {
            text: text.to_string(),
            images,
        }));
        true
    }

    /// Keep the agent's latest usage report for the session, on the session row
    /// as well, so a restart still shows it.
    fn record_usage(&mut self, session_id: i64, usage: agent::Usage, cx: &mut Context<Self>) {
        let previous = self.usage.get(&session_id);
        let usage = merge_usage(previous, usage);
        if previous == Some(&usage) {
            return;
        }
        if let Ok(json) = serde_json::to_string(&usage) {
            self.store.update_session_usage(session_id, &json).ok();
        }
        self.usage.insert(session_id, usage);
        cx.emit(WorkspaceEvent::Changed);
    }

    /// Mutate the session's live turn, opening it first if there is none.
    fn update_turn<R>(
        &mut self,
        session_id: i64,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Turn) -> R,
    ) -> R {
        let opened = !self.turns.contains_key(&session_id);
        let result = f(self.turns.entry(session_id).or_insert_with(Turn::new));
        cx.emit(WorkspaceEvent::TurnChanged(session_id));
        // A turn opening changes what the home view draws; a chunk streaming
        // into one already open changes only the conversation.
        if opened {
            cx.emit(WorkspaceEvent::Changed);
        }
        result
    }

    /// Close the session's live turn, handing it back.
    ///
    /// A turn ending is when its connection can become idle — its agent is no
    /// longer running — so this is also where the pool is trimmed back to the
    /// cap. The session itself is kept: its turn just ended, and the user is
    /// watching it.
    pub(super) fn take_turn(&mut self, session_id: i64, cx: &mut Context<Self>) -> Option<Turn> {
        let turn = self.turns.remove(&session_id)?;
        cx.emit(WorkspaceEvent::TurnClosed {
            session_id,
            turn_id: turn.id(),
        });
        cx.emit(WorkspaceEvent::Changed);
        self.reclaim_idle_connections(session_id, cx);
        Some(turn)
    }

    /// Get or open the connection for a session's agent, and start the task
    /// that folds its events back into the workspace.
    fn ensure_connection(&mut self, session: &Session, cx: &mut Context<Self>) {
        if self.conns.contains_key(&session.id) {
            // Reusing it is a use of it, so it stays the most recently used.
            self.touch_connection(session.id);
            return;
        }
        // A pure shell has no agent subprocess to drive.
        if crate::term::is_shell(&session.agent) {
            return;
        }
        let agent_id = agent::AGENTS
            .iter()
            .find(|a| a.id == session.agent)
            .map(|a| a.id);
        let Some(agent_id) = agent_id else {
            self.system_message(session.id, format!("未支持的 agent：{}", session.agent), cx);
            return;
        };
        let Some(cfg) = agent::Config::for_agent(agent_id) else {
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
        // handshake. Whether either applies is the backend's business — this
        // only says what the session is set to.
        let picks = agent::SessionPicks {
            model: session.model.clone(),
            mode: session.mode.clone(),
            reasoning: session.reasoning.clone(),
            resume_id: session.resume_id.clone(),
        };
        let cfg = cfg.with_session(picks);
        // Said once per session per run: a reclaimed connection is opened again
        // on the next prompt, and repeating a notice that is written into the
        // conversation would pile up a copy per reconnect.
        if session.has_conversation
            && session.resume_id.is_none()
            && self.no_resume_notified.insert(session.id)
        {
            self.system_message(
                session.id,
                "这段历史没有保存 agent 会话标识，无法恢复原来的上下文；本次将开启新的 agent 会话。".into(),
                cx,
            );
        }

        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<agent::Event>();
        let conn = agent::spawn(cfg, cwd, ev_tx);
        // Remember which connection this bridge task belongs to, so its events
        // and teardown can be ignored once a newer one takes over the session.
        let conn_id = conn.id();
        // Re-apply the session's stored picks on a fresh connection; queued
        // until the agent thread has finished its handshake. A backend that
        // already launched with them (Claude Code) sees the same value twice
        // and skips the repeat.
        if let Some(model) = session.model.clone() {
            conn.send(agent::Cmd::SetModel(model));
        }
        if let Some(mode) = session.mode.clone() {
            conn.send(agent::Cmd::SetMode(mode));
        }
        if let Some(sandbox) = session.sandbox.clone() {
            conn.send(agent::Cmd::SetSandbox(sandbox));
        }
        if let Some(reasoning) = session.reasoning.clone() {
            conn.send(agent::Cmd::SetReasoning(reasoning));
        }
        self.conns.insert(session.id, conn);
        self.touch_connection(session.id);

        let mut events = discovery::bridge(ev_rx);
        let session_id = session.id;
        // The task holds the workspace weakly, so it never keeps a connection
        // alive past the window that owns it.
        cx.spawn(async move |this, cx| {
            while let Some(ev) = events.next().await {
                // A replaced connection (agent switched) must not feed the
                // session that a newer one now owns.
                let current = this
                    .update(cx, |workspace, cx| {
                        if !owns_connection(&workspace.conns, session_id, conn_id) {
                            return false;
                        }
                        workspace.on_agent_event(session_id, ev, cx);
                        true
                    })
                    .unwrap_or(false);
                if !current {
                    break;
                }
            }
            let _ = this.update(cx, |workspace, cx| {
                workspace.on_connection_closed(session_id, conn_id, cx)
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
        // Drop the in-flight turn with the connection: nothing can deliver its
        // answer now, so the working indicator must not keep counting. The
        // normal paths have already dropped it (a turn ends with `Done`, and a
        // failure reports `Error` first); this covers a connection that died
        // without either, which would otherwise leave the turn — and the send
        // path, which refuses to send while one exists — stuck for good.
        // The handle goes first: closing the turn trims the pool back to the
        // cap, and a dead connection still counted there would push out a
        // live idle one for nothing.
        self.forget_connection(session_id);
        self.take_turn(session_id, cx);
        cx.emit(WorkspaceEvent::Changed);
    }

    /// Answer every outstanding permission prompt for a session with the
    /// protocol's `Cancelled` outcome and drop it from the turn. Used when the
    /// turn ends or the connection goes away, so the agent is never left
    /// waiting on a prompt that no longer exists.
    pub(super) fn cancel_pending_permissions(&mut self, session_id: i64) {
        let Some(turn) = self.turns.get_mut(&session_id) else {
            return;
        };
        let request_ids = turn.take_pending_permissions();
        if let Some(conn) = self.conns.get(&session_id) {
            for request_id in &request_ids {
                conn.resolve_permission(request_id, None);
            }
        }
    }

    /// Fold one event from a session's live connection into the workspace.
    fn on_agent_event(&mut self, session_id: i64, ev: agent::Event, cx: &mut Context<Self>) {
        match ev {
            agent::Event::SessionReady {
                resume_id,
                resumable,
            } => {
                // An agent that will not load its own id back cannot be
                // restored from it, so the connection must be kept rather than
                // reclaimed; remember that, and do not persist a cursor that
                // only a connect-time error would come of. The session still
                // gets the ordinary "no saved context" notice on a later
                // reconnect, which is a fresh conversation rather than a
                // failure loop.
                if !resumable {
                    self.unresumable.insert(session_id);
                } else {
                    self.unresumable.remove(&session_id);
                    match self.store.update_session_resume_id(session_id, &resume_id) {
                        Ok(()) => {
                            if let Some(session) =
                                self.sessions.iter_mut().find(|s| s.id == session_id)
                            {
                                session.resume_id = Some(resume_id);
                            }
                        }
                        Err(error) => self.system_message(
                            session_id,
                            format!("保存 agent 会话标识失败，重启后将无法恢复上下文：{error}"),
                            cx,
                        ),
                    }
                }
            }
            agent::Event::Agent(chunk) => {
                self.update_turn(session_id, cx, |turn| turn.push_answer(chunk));
            }
            agent::Event::Thought { text, message } => {
                self.update_turn(session_id, cx, |turn| turn.push_thinking(text, message));
            }
            agent::Event::ToolCall {
                id,
                title,
                status,
                content,
            } => {
                self.update_turn(session_id, cx, |turn| {
                    turn.apply_tool(id, title, status, content)
                });
            }
            agent::Event::PermissionRequest {
                tool_call_id,
                request_id,
            } => {
                let pending = PendingPermission { request_id };
                let previous = self.update_turn(session_id, cx, |turn| {
                    turn.request_permission(tool_call_id, pending)
                });
                // One prompt per row: settle any earlier one as cancelled.
                if let (Some(previous), Some(conn)) = (previous, self.conns.get(&session_id)) {
                    conn.resolve_permission(&previous, None);
                }
                // The sidebar's marker.
                cx.emit(WorkspaceEvent::Changed);
            }
            agent::Event::SessionTitle { title } => {
                self.apply_generated_title(session_id, &title);
                cx.emit(WorkspaceEvent::Changed);
            }
            agent::Event::Done(_) => {
                // The turn is over, so nothing can still be authorized.
                self.cancel_pending_permissions(session_id);
                if let Some(turn) = self.take_turn(session_id, cx) {
                    let elapsed = turn.elapsed();
                    // The message the turn finished on, which is where its total
                    // is recorded — and so where the settled label is rebuilt
                    // from when the history is read back.
                    let mut closed = None;
                    for (role, text) in turn.into_records() {
                        closed = self.store.insert_message(session_id, role, &text).ok();
                    }
                    if let Some(id) = closed {
                        self.store
                            .set_message_duration(id, elapsed.as_millis() as i64)
                            .ok();
                    }
                }
                cx.emit(WorkspaceEvent::HistoryChanged(session_id));
            }
            agent::Event::Error(err) => {
                self.cancel_pending_permissions(session_id);
                self.take_turn(session_id, cx);
                self.system_message(session_id, err, cx);
            }
            agent::Event::Usage(usage) => self.record_usage(session_id, usage, cx),
            // A live session's own catalogues are not drawn while the chat pane
            // is off; the agent's probe report is what the settings window shows.
            agent::Event::Commands { .. }
            | agent::Event::SessionOptions { .. }
            | agent::Event::Log(_)
            | agent::Event::ProbeFailed { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{fallback_title, merge_usage, owns_connection};
    use crate::agent;

    /// A session's connection is replaced when the agent is switched. Events
    /// and teardown from the replaced connection must not be attributed to the
    /// session, or they would clobber the connection that succeeded it.
    #[test]
    fn only_the_registered_connection_owns_a_session() {
        let mut conns: HashMap<i64, agent::Connection> = HashMap::new();
        conns.insert(7, agent::Connection::detached_for_test(1));

        assert!(owns_connection(&conns, 7, 1));
        assert!(!owns_connection(&conns, 7, 2), "another connection");
        assert!(!owns_connection(&conns, 8, 1), "another session");

        // Replacement under the same session id: the old connection is stale.
        conns.insert(7, agent::Connection::detached_for_test(2));
        assert!(
            !owns_connection(&conns, 7, 1),
            "the replaced connection must not match"
        );
        assert!(owns_connection(&conns, 7, 2));

        // Once gone, nothing owns the session.
        conns.remove(&7);
        assert!(!owns_connection(&conns, 7, 2));
    }

    /// A report without a window keeps the one already known; everything else
    /// is the report's, including a context it no longer knows.
    #[test]
    fn a_usage_report_keeps_the_known_window() {
        let known = agent::Usage {
            used: Some(10),
            window: Some(200_000),
            cost: None,
        };
        let merged = merge_usage(
            Some(&known),
            agent::Usage {
                used: None,
                window: None,
                cost: None,
            },
        );
        assert_eq!(merged.window, Some(200_000));
        assert_eq!((merged.used, merged.cost), (None, None));
        let replaced = merge_usage(
            Some(&known),
            agent::Usage {
                window: Some(1_000_000),
                ..Default::default()
            },
        );
        assert_eq!(replaced.window, Some(1_000_000));
    }

    /// The fallback title is the first line, cut at 24 characters with an
    /// ellipsis only when something was cut.
    #[test]
    fn the_fallback_title_is_the_first_line_cut_short() {
        assert_eq!(
            fallback_title("修复登录\n细节…").as_deref(),
            Some("修复登录")
        );
        let long = "一".repeat(30);
        assert_eq!(fallback_title(&long), Some(format!("{}…", "一".repeat(24))));
    }
}
