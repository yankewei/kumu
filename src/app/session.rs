//! A session's flow: sending a prompt, the connection that carries it, the
//! events the agent reports back, permission decisions, and the picks that
//! configure the session.

use std::collections::HashMap;
use std::path::PathBuf;

use futures::StreamExt;
use gpui_kit::Context;

use super::discovery::{self, SessionOptions};
use super::turn::{PendingPermission, Turn};
use super::{Workspace, WorkspaceEvent};
use crate::agent;
use crate::db::Session;

/// A session setting other than its model, each stored on the session and
/// applied over its connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// The work mode. Claude Code's access policy is the same stored value — one
    /// permission mode, two chips — so the access chip picks this axis too.
    Mode,
    Sandbox,
    Reasoning,
}

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
    /// Refused when the text is empty or the session already has a turn in
    /// flight: one turn per session at a time, while other sessions run on
    /// their own.
    pub fn send(&mut self, session_id: i64, text: &str, cx: &mut Context<Self>) -> bool {
        let text = text.trim();
        if text.is_empty() || self.has_turn(session_id) {
            return false;
        }
        let Some(session) = self.session(session_id).cloned() else {
            return false;
        };
        // Until the agent reports a real title (session/info), fall back to
        // the first line of the first user message.
        if session.title.starts_with("会话 ")
            && let Some(title) = fallback_title(text)
        {
            self.apply_generated_title(session_id, &title);
        }
        self.store.insert_message(session_id, "user", text).ok();
        // The first message is what fixes the session's agent, so the list is
        // re-read here rather than letting the flag lag a turn behind the
        // picker that reads it.
        self.refresh();
        cx.emit(WorkspaceEvent::HistoryChanged(session_id));
        cx.emit(WorkspaceEvent::Changed);

        self.ensure_connection(&session, cx);
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
        conn.send(agent::Cmd::Prompt(text.to_string()));
        true
    }

    /// Stop the session's turn in flight, if there is one.
    ///
    /// The cancel travels out of band rather than as an [`agent::Cmd`]: the
    /// backend's command loop is parked on the very turn being cancelled, so a
    /// command would not be read until that turn had already ended. The turn
    /// stays open until the backend reports it settled, because the agent is
    /// still running until it says otherwise.
    pub fn cancel_turn(&self, session_id: i64) {
        if let Some(conn) = self.conns.get(&session_id) {
            conn.cancel();
        }
    }

    /// Record the user's decision for a pending permission and hand it to the
    /// agent. `option_id` of `None` cancels the request.
    pub fn resolve_permission(
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
            turn.resolve_permission(request_id, label);
            cx.emit(WorkspaceEvent::TurnChanged(session_id));
        }
        cx.emit(WorkspaceEvent::Changed);
    }

    /// Switch a session's agent and/or model: persist the choice, drop the old
    /// agent's connection when the agent changes, then apply the model over the
    /// session's live connection. A session with no live connection just stores
    /// the pick, which is re-applied when one is opened.
    pub fn pick_model(
        &mut self,
        session_id: i64,
        agent_id: &'static str,
        model_value: String,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let agent_changed = session.agent != agent_id;
        // A session keeps its agent once it has a conversation: the transcript
        // was written by that agent, and a different one would be handed
        // history it never produced (the transcript is persisted, but the
        // agent's own context is not, so the switch is a silent amnesia even
        // for the same agent's later process). The picker does not offer the
        // switch in this state — this is the backstop that keeps the rule true
        // for anything else that reaches here.
        if agent_changed && session.has_conversation {
            return;
        }
        self.store
            .update_session_agent_model(session_id, agent_id, Some(&model_value))
            .ok();
        if agent_changed {
            // Mode ids, sandbox values and effort values are the agent's own
            // vocabulary, so the stored ones stop meaning anything: drop them,
            // and the pickers fall back to the new agent's defaults until the
            // user chooses again. Written before the refresh below so the
            // re-applied values come from the fresh row.
            //
            // A model change *within* an agent is deliberately not cleared here:
            // whether a pick still applies is the backend's call — Codex drops
            // one the new model does not advertise and restores it when a later
            // model does, and an ACP agent's level does not depend on the model
            // at all.
            self.store.update_session_mode(session_id, None).ok();
            self.store.update_session_sandbox(session_id, None).ok();
            self.store.update_session_reasoning(session_id, None).ok();
            // Dropping the handle closes the old agent subprocess; the fresh
            // connection re-applies the stored model on session/new. Any prompt
            // still waiting on the old connection is settled first, and the
            // in-flight reply (if any) is abandoned with it.
            self.cancel_pending_permissions(session_id);
            self.conns.remove(&session_id);
            self.live_commands.remove(&session_id);
            self.live_options.remove(&session_id);
            self.take_turn(session_id, cx);
        }
        self.refresh();
        // Opening a connection here just to deliver the value would start an
        // agent subprocess the user never prompted — and for an agent that
        // cannot run a session, that is an error written into a conversation
        // that has not begun.
        if let Some(conn) = self.conns.get(&session_id) {
            conn.send(agent::Cmd::SetModel(model_value));
        }
        cx.emit(WorkspaceEvent::Changed);
    }

    /// Set one of the session's other axes: persist it, then apply it over the
    /// session's live connection. A session with no live connection just stores
    /// the pick, which is re-applied when one is opened.
    pub fn pick(&mut self, session_id: i64, axis: Axis, value: String, cx: &mut Context<Self>) {
        let (stored, cmd) = match axis {
            Axis::Mode => (
                self.store.update_session_mode(session_id, Some(&value)),
                agent::Cmd::SetMode(value),
            ),
            Axis::Sandbox => (
                self.store.update_session_sandbox(session_id, Some(&value)),
                agent::Cmd::SetSandbox(value),
            ),
            Axis::Reasoning => (
                self.store
                    .update_session_reasoning(session_id, Some(&value)),
                agent::Cmd::SetReasoning(value),
            ),
        };
        stored.ok();
        self.refresh();
        if let Some(conn) = self.conns.get(&session_id) {
            conn.send(cmd);
        }
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
        // A turn opening changes the composer (send ↔ stop) and the command
        // menu; a chunk streaming into one already open changes only the
        // conversation.
        if opened {
            cx.emit(WorkspaceEvent::Changed);
        }
        result
    }

    /// Close the session's live turn, handing it back.
    pub(super) fn take_turn(&mut self, session_id: i64, cx: &mut Context<Self>) -> Option<Turn> {
        let turn = self.turns.remove(&session_id)?;
        cx.emit(WorkspaceEvent::TurnClosed {
            session_id,
            turn_id: turn.id(),
        });
        cx.emit(WorkspaceEvent::Changed);
        Some(turn)
    }

    /// Get or open the connection for a session's agent, and start the task
    /// that folds its events back into the workspace.
    fn ensure_connection(&mut self, session: &Session, cx: &mut Context<Self>) {
        if self.conns.contains_key(&session.id) {
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
        if session.has_conversation && session.resume_id.is_none() {
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
        // without either, which would otherwise leave the turn — and the
        // composer, which refuses to send while one exists — stuck for good.
        self.take_turn(session_id, cx);
        self.conns.remove(&session_id);
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
            agent::Event::SessionReady { resume_id } => {
                match self.store.update_session_resume_id(session_id, &resume_id) {
                    Ok(()) => {
                        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id)
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
                title,
                detail,
                options,
            } => {
                let pending = PendingPermission {
                    request_id,
                    title,
                    detail,
                    options,
                };
                let previous = self.update_turn(session_id, cx, |turn| {
                    turn.request_permission(tool_call_id, pending)
                });
                // One prompt per row: settle any earlier one as cancelled.
                if let (Some(previous), Some(conn)) = (previous, self.conns.get(&session_id)) {
                    conn.resolve_permission(&previous, None);
                }
                // The card above the composer and the sidebar's marker.
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
            // A live session speaks for itself alone.
            agent::Event::Commands { agent, commands } => {
                self.live_commands.insert(session_id, (agent, commands));
                cx.emit(WorkspaceEvent::Changed);
            }
            // What this session's process offers and runs with; the agent's
            // catalog stays what its probe said.
            ev @ agent::Event::SessionOptions { .. } => {
                if let Some((agent, options)) = SessionOptions::from_event(&ev)
                    && !options.is_empty()
                {
                    self.live_options.insert(session_id, (agent, options));
                    cx.emit(WorkspaceEvent::Changed);
                }
            }
            agent::Event::Log(_) | agent::Event::ProbeFailed { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{fallback_title, owns_connection};
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
