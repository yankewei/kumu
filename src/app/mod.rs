//! The application layer: what kumu does with agents, independent of how it is
//! drawn. The UI renders and reads this; the agent layer below it speaks the
//! protocols, and the store keeps what outlives a run.
//!
//!   - this module — [`Workspace`]: projects, sessions, and the entity that owns
//!     them;
//!   - `session`   — a session's flow: sending, connections, the events a live
//!     agent reports, and permissions;
//!   - `turn`      — a live turn as plain data, and how agent events fold into it;
//!   - `discovery` — what each agent offers, as its probe reports it.
//!
//! The UI hears about changes through [`WorkspaceEvent`] rather than a blanket
//! notify: a streamed chunk only concerns the conversation, and notifying the
//! whole home view per chunk is what made the window stutter.

pub mod discovery;
mod session;
pub mod turn;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use futures::StreamExt;
use gpui_kit::{Context, EventEmitter};

use crate::agent;
use crate::db::{Message, Project, Session, Store, db_path};
use crate::term::{KeyInput, Snapshot, Term, TermColors};
use crate::theme;
use discovery::Discovery;
use turn::{PendingPermission, Turn};

/// How many agent subprocesses may stay connected at once.
///
/// A session whose connection is reclaimed reconnects on its next prompt and
/// resumes its agent-side context from the stored `resume_id`, so the cap
/// trades a little latency for a bounded number of live processes. Only a
/// session that can actually be restored that way is reclaimed: one whose agent
/// cannot reopen it (see [`agent::Event::SessionReady`]'s `resumable`) is kept
/// alive instead of being stranded, so the cap is a target rather than a
/// guarantee when such a session is among the live ones.
const MAX_LIVE_CONNECTIONS: usize = 3;

/// How many native agent TUIs may stay running at once.
///
/// A session past the cap is stopped, least recently shown first. Selecting it
/// again starts a new process. There is no exception for a session that cannot
/// be resumed: the TUI is the agent's own, and starting it over is the whole
/// recovery.
const MAX_LIVE_TERMINALS: usize = 4;

/// What changed in the [`Workspace`], for the views that draw it.
pub enum WorkspaceEvent {
    /// Something the home view draws: the project or session list, a session's
    /// title, a turn opening or closing, a permission prompt, what an agent
    /// offers.
    Changed,
    /// A session's live turn changed — streamed text, reasoning, a tool update.
    /// Only the conversation draws it.
    TurnChanged(i64),
    /// A session's live turn ended; anything cached to draw it can go.
    TurnClosed { session_id: i64, turn_id: u64 },
    /// A session's saved history changed: a prompt, a finished turn, a notice.
    HistoryChanged(i64),
    /// A session's terminal grid changed. Only the terminal pane draws it.
    Frame(i64),
    /// A session was removed by the app (its shell exited). `next` is who to
    /// show instead, computed before the row was gone.
    SessionClosed { id: i64, next: Option<i64> },
}

/// Projects, sessions, and each session's live connection and turn.
///
/// One per app, owned by the home window and shared with the settings window.
/// Nothing in here renders, and nothing in here names a protocol: agents are
/// reached only through [`agent`]'s vocabulary.
pub struct Workspace {
    store: Store,
    projects: Vec<Project>,
    sessions: Vec<Session>,
    /// Live connections keyed by session id, reused across prompts.
    conns: HashMap<i64, agent::Connection>,
    /// Each live session's position in the reclamation order, least recently
    /// used first. Kept in step with `conns` by [`Self::touch_connection`] and
    /// [`Self::forget_connection`], and read by
    /// [`Self::reclaim_idle_connections`].
    conn_lru: Vec<i64>,
    /// Native TUI processes keyed by session id. A hidden session keeps
    /// reading, so its agent does not block on a full pty, until the cap
    /// stops it.
    terms: HashMap<i64, Term>,
    /// Least recently shown first. Only sessions whose process is still running.
    term_lru: Vec<i64>,
    /// Sessions already told that their history cannot be resumed. A reclaimed
    /// connection is opened again on the next prompt, and without this the
    /// notice would be written into the conversation every time.
    no_resume_notified: HashSet<i64>,
    /// Sessions whose agent cannot reopen its conversation from the stored
    /// `resume_id` (see [`agent::Event::SessionReady`]'s `resumable`). Their
    /// live connection is the only copy of the agent-side session, so
    /// [`Self::reclaim_idle_connections`] never closes it.
    unresumable: HashSet<i64>,
    /// Sessions already told that their connection was reclaimed. The notice
    /// is worth writing once; a session that is cycled through the cap again
    /// would otherwise pile up a copy per reclaim.
    reclaim_notified: HashSet<i64>,
    /// Each session's turn in flight: streaming reasoning, tool calls, and the
    /// reply being built. Dropped when the turn ends.
    turns: HashMap<i64, Turn>,
    /// What each agent offers in general, as probed.
    discovery: Discovery,
    /// What each session's agent last reported about its context, decoded once
    /// from the store at start and kept current by [`Self::record_usage`].
    usage: HashMap<i64, agent::Usage>,
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = Store::open(&db_path()).expect("failed to open sqlite db");
        let mut workspace = Self {
            store,
            projects: Vec::new(),
            sessions: Vec::new(),
            conns: HashMap::new(),
            conn_lru: Vec::new(),
            terms: HashMap::new(),
            term_lru: Vec::new(),
            no_resume_notified: HashSet::new(),
            unresumable: HashSet::new(),
            reclaim_notified: HashSet::new(),
            turns: HashMap::new(),
            discovery: Discovery::default(),
            usage: HashMap::new(),
        };
        workspace.bootstrap();
        workspace.usage = workspace
            .sessions
            .iter()
            .filter_map(|session| {
                let usage = serde_json::from_str(session.usage.as_deref()?).ok()?;
                Some((session.id, usage))
            })
            .collect();
        workspace.probe_missing(cx);
        workspace
    }

    /// A first run gets a project for the current directory and a shell in
    /// it, so the window never opens onto nothing.
    fn bootstrap(&mut self) {
        if self.store.projects().is_empty() {
            let cwd = std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("/"))
                .to_string_lossy()
                .into_owned();
            self.store.insert_project("kumu", &cwd).ok();
        }
        self.refresh();
        if self.sessions.is_empty()
            && let Some(project) = self.projects.first().cloned()
        {
            self.store
                .insert_session(project.id, crate::term::SHELL, "终端 1", &project.path)
                .ok();
            self.refresh();
        }
    }

    fn refresh(&mut self) {
        self.projects = self.store.projects();
        self.sessions = self.store.sessions();
    }

    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    pub fn session(&self, id: i64) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    /// The session's screen, after making sure its process is running.
    ///
    /// Agent sessions launch that agent's interactive TUI; shell sessions
    /// launch the user's `$SHELL`. An exited agent TUI starts again; an exited
    /// shell is deleted instead (see [`Self::watch_terminal`]). A fifth
    /// running process stops the one shown least recently.
    pub fn show_terminal(&mut self, session_id: i64, cx: &mut Context<Self>) -> Snapshot {
        if self
            .terms
            .get(&session_id)
            .is_some_and(|term| !term.snapshot().exited)
        {
            self.touch_terminal(session_id);
            self.terminal_wake_handled(session_id);
            return self.terms[&session_id].snapshot();
        }
        self.terms.remove(&session_id);
        self.term_lru.retain(|&id| id != session_id);
        self.reclaim_terminals();
        let Some(session) = self.session(session_id).cloned() else {
            return Snapshot::blank("", "");
        };
        let colors = theme::term_colors(cx);
        let launch = if crate::term::is_shell(&session.agent) {
            Some(crate::term::default_shell())
        } else {
            agent::interactive(&session.agent)
        };
        let Some((program, args)) = launch else {
            let mut snapshot = Snapshot::blank_with(&session.agent, &session.cwd, colors.bg);
            snapshot.exited = true;
            snapshot.spawn_error = Some("没有这个 agent".to_string());
            return snapshot;
        };
        let (term, wake) = Term::launch(program, args, PathBuf::from(&session.cwd), colors);
        let snapshot = term.snapshot();
        self.terms.insert(session_id, term);
        self.touch_terminal(session_id);
        self.watch_terminal(session_id, wake, cx);
        snapshot
    }

    /// Push the UI theme's colors into every live terminal.
    pub fn set_terminal_colors(&self, colors: TermColors) {
        for term in self.terms.values() {
            term.set_colors(colors);
        }
    }

    pub fn terminal_snapshot(&self, session_id: i64) -> Option<Snapshot> {
        self.terms.get(&session_id).map(Term::snapshot)
    }

    pub fn terminal_generation(&self, session_id: i64) -> Option<u64> {
        self.terms.get(&session_id).map(Term::generation)
    }

    pub fn terminal_wake_handled(&self, session_id: i64) {
        if let Some(term) = self.terms.get(&session_id) {
            term.wake_handled();
        }
    }

    pub fn terminal_key(&self, session_id: i64, input: KeyInput) {
        if let Some(term) = self.terms.get(&session_id) {
            term.key(input);
        }
    }

    pub fn terminal_write(&self, session_id: i64, bytes: impl Into<Vec<u8>>) {
        if let Some(term) = self.terms.get(&session_id) {
            term.write(bytes);
        }
    }

    pub fn terminal_resize(&self, session_id: i64, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        if let Some(term) = self.terms.get(&session_id) {
            term.resize(cols, rows, cell_w, cell_h);
        }
    }

    fn touch_terminal(&mut self, session_id: i64) {
        self.term_lru.retain(|&id| id != session_id);
        self.term_lru.push(session_id);
    }

    fn forget_terminal(&mut self, session_id: i64) {
        self.terms.remove(&session_id);
        self.term_lru.retain(|&id| id != session_id);
    }

    /// Drop exited processes, then stop the least recently shown ones until
    /// one more process fits under the cap.
    fn reclaim_terminals(&mut self) {
        let dead: Vec<i64> = self
            .terms
            .iter()
            .filter(|(_, term)| term.snapshot().exited)
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            self.forget_terminal(id);
        }
        let alive: Vec<i64> = self
            .term_lru
            .iter()
            .copied()
            .filter(|id| self.terms.contains_key(id))
            .collect();
        for id in terminal_evictions(&alive, MAX_LIVE_TERMINALS) {
            self.forget_terminal(id);
        }
    }

    fn watch_terminal(
        &self,
        session_id: i64,
        mut wake: futures::channel::mpsc::UnboundedReceiver<()>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while wake.next().await.is_some() {
                let watching = this
                    .update(cx, |workspace, cx| {
                        let Some(term) = workspace.terms.get(&session_id) else {
                            return false;
                        };
                        let snapshot = term.snapshot();
                        // `exit` (or any clean shell death) ends the session
                        // itself — leaving an "已退出" row around would just
                        // restart `$SHELL` on the next focus. Spawn failures
                        // keep the row so the header can show why.
                        let shell_done = snapshot.exited
                            && snapshot.spawn_error.is_none()
                            && workspace
                                .session(session_id)
                                .is_some_and(|s| crate::term::is_shell(&s.agent));
                        if shell_done {
                            let next = workspace.delete_session(session_id, cx);
                            cx.emit(WorkspaceEvent::SessionClosed {
                                id: session_id,
                                next,
                            });
                            return false;
                        }
                        cx.emit(WorkspaceEvent::Frame(session_id));
                        true
                    })
                    .unwrap_or(false);
                if !watching {
                    break;
                }
            }
        })
        .detach();
    }

    /// A session's saved history, read from the store.
    pub fn messages(&self, session_id: i64) -> Vec<Message> {
        self.store.messages(session_id)
    }

    pub fn turn(&self, session_id: i64) -> Option<&Turn> {
        self.turns.get(&session_id)
    }

    pub fn has_turn(&self, session_id: i64) -> bool {
        self.turns.contains_key(&session_id)
    }

    /// The oldest permission prompt the session's agent is waiting on.
    pub fn pending_permission(&self, session_id: i64) -> Option<&PendingPermission> {
        self.turn(session_id)?.pending_permission()
    }

    /// What each agent offers in general, and which probes are out.
    pub fn discovery(&self) -> &Discovery {
        &self.discovery
    }

    /// Add the folder at `path` as a project, named after the folder itself —
    /// or, when it already is one, return that project instead of inserting a
    /// duplicate row.
    pub fn add_project(&mut self, path: &str, cx: &mut Context<Self>) -> Option<i64> {
        let name = match resolve_project_pick(&self.projects, path) {
            ProjectPick::Existing(id) => return Some(id),
            ProjectPick::New { name } => name,
        };
        let id = self.store.insert_project(&name, path).ok()?;
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
        Some(id)
    }

    /// Start a shell session in `project_id`, or in the first project when
    /// none is given, returning its id. The home pane is a terminal, so a
    /// new session is a login shell until the user opens an agent TUI.
    pub fn new_session(&mut self, project_id: Option<i64>, cx: &mut Context<Self>) -> Option<i64> {
        self.new_shell_session(project_id, cx)
    }

    /// Start a pure shell session in `project_id` (or the first project).
    pub fn new_shell_session(
        &mut self,
        project_id: Option<i64>,
        cx: &mut Context<Self>,
    ) -> Option<i64> {
        self.insert_session(project_id, crate::term::SHELL, "终端", cx)
    }

    fn insert_session(
        &mut self,
        project_id: Option<i64>,
        agent: &str,
        title_prefix: &str,
        cx: &mut Context<Self>,
    ) -> Option<i64> {
        let project_id = match project_id.or_else(|| self.projects.first().map(|p| p.id)) {
            Some(id) => id,
            None => self
                .store
                .insert_project("kumu", "cwd-not-set")
                .unwrap_or(1),
        };
        let cwd = self
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .map(|p| p.path.clone())
            .unwrap_or_default();
        let n = self.sessions.len() + 1;
        let id = self
            .store
            .insert_session(project_id, agent, &format!("{title_prefix} {n}"), &cwd)
            .ok()?;
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
        Some(id)
    }

    /// Apply the name the user chose. It is marked as theirs, which is what
    /// keeps a title generated later from replacing it.
    pub fn rename_session(&mut self, session_id: i64, title: String, cx: &mut Context<Self>) {
        self.store.rename_session(session_id, &title).ok();
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.title = title;
            session.title_manual = true;
        }
        cx.emit(WorkspaceEvent::Changed);
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
    /// connection (which tears down the agent subprocess), forget its turn, then
    /// remove the row and its history.
    ///
    /// Returns the neighbor to select next — computed before the row is gone —
    /// so a caller that was looking at this session can land somewhere else.
    pub fn delete_session(&mut self, session_id: i64, cx: &mut Context<Self>) -> Option<i64> {
        let next = neighbor_session(&self.sessions, session_id);
        self.cancel_pending_permissions(session_id);
        self.forget_terminal(session_id);
        self.forget_connection(session_id);
        self.no_resume_notified.remove(&session_id);
        self.unresumable.remove(&session_id);
        self.reclaim_notified.remove(&session_id);
        self.usage.remove(&session_id);
        self.take_turn(session_id, cx);
        self.store.delete_session(session_id).ok();
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
        next
    }

    /// Record that a session's connection was used just now, so a later
    /// reclamation keeps the most recently used ones. The only writer is
    /// [`Self::ensure_connection`], which is the only place a connection is
    /// opened or reused.
    fn touch_connection(&mut self, session_id: i64) {
        self.conn_lru.retain(|&id| id != session_id);
        self.conn_lru.push(session_id);
    }

    /// Drop the session's live connection handle — which tears down its agent
    /// subprocess — and forget its place in the reclamation order.
    fn forget_connection(&mut self, session_id: i64) {
        self.conns.remove(&session_id);
        self.conn_lru.retain(|&id| id != session_id);
    }

    /// Close the least recently used idle connections once more than
    /// [`MAX_LIVE_CONNECTIONS`] are open, so agent subprocesses do not
    /// accumulate for the life of the app.
    ///
    /// A session with a turn in flight is never closed — its agent is running —
    /// and neither is `active`, the session the caller is acting on: the one
    /// being prompted, or the one whose turn just ended. A session whose agent
    /// cannot load it back is never closed either: dropping its connection
    /// would lose the conversation for good, not merely park it.
    ///
    /// A reclaimed session reconnects on its next prompt and resumes its
    /// agent-side context from the stored `resume_id`. "Idle" here means only
    /// that no turn is running; the agent process may still be holding things a
    /// turn left behind — a background task it started, a session-scoped
    /// approval — and those are lost with it, so the reclaim is announced.
    fn reclaim_idle_connections(&mut self, active: i64, cx: &mut Context<Self>) {
        let conns = &self.conns;
        self.conn_lru.retain(|id| conns.contains_key(id));
        let busy: HashSet<i64> = self.turns.keys().copied().collect();
        let evictions = idle_evictions(
            &self.conn_lru,
            &busy,
            &self.unresumable,
            active,
            MAX_LIVE_CONNECTIONS,
        );
        if evictions.is_empty() {
            return;
        }
        for session_id in evictions {
            self.cancel_pending_permissions(session_id);
            self.forget_connection(session_id);
            // What the agent was holding between turns goes with the process,
            // and the user is not looking at this session, so say so — once,
            // rather than writing the notice into the conversation on every
            // reclaim.
            if self.reclaim_notified.insert(session_id) {
                self.system_message(
                    session_id,
                    format!(
                        "连接已回收：同时连接的会话超过 {MAX_LIVE_CONNECTIONS} 个，已关闭这个会话的 \
                         agent 进程以释放资源。下次发消息会重新连接并恢复上下文；agent 进程中的后台任务\
                         和本会话内的授权不会恢复。"
                    ),
                    cx,
                );
            }
        }
        cx.emit(WorkspaceEvent::Changed);
    }

    /// Persist and show a system notice (unsupported agent, agent error…).
    fn system_message(&mut self, session_id: i64, text: String, cx: &mut Context<Self>) {
        self.store.insert_message(session_id, "system", &text).ok();
        cx.emit(WorkspaceEvent::HistoryChanged(session_id));
    }

    /// Probe the agents nothing is known about yet — never probed, or probed
    /// and failed — so the settings window has a catalogue before any session
    /// has connected.
    pub fn probe_missing(&mut self, cx: &mut Context<Self>) {
        let missing = self.discovery.missing();
        self.probe(&missing, cx);
    }

    /// Probe `agents` again, replacing what is cached as the answers arrive.
    /// An agent whose probe is already out is left to that one.
    ///
    /// This is the only way to refresh a catalog mid-run: an agent that was
    /// updated, restarted, or pointed at another account is otherwise cached
    /// for the life of the process.
    pub fn probe(&mut self, agents: &[&'static str], cx: &mut Context<Self>) {
        let launchable: Vec<(&'static str, agent::Config)> = agents
            .iter()
            .filter_map(|&id| Some((id, agent::Config::for_agent(id)?)))
            .collect();
        let ids: Vec<&'static str> = launchable.iter().map(|(id, _)| *id).collect();
        let started = self.discovery.start(&ids);
        if started.is_empty() {
            return;
        }
        let mut reports = discovery::probe(
            launchable
                .into_iter()
                .filter(|(id, _)| started.contains(id))
                .collect(),
        );
        cx.emit(WorkspaceEvent::Changed);
        cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(ev) = reports.next().await {
                let updated = this.update(cx, |workspace, cx| {
                    if workspace.discovery.record(ev) {
                        cx.emit(WorkspaceEvent::Changed);
                    }
                });
                if updated.is_err() {
                    return;
                }
            }
            // A probe that ended without a word is over all the same.
            let _ = this.update(cx, |workspace, cx| {
                workspace.discovery.finish(&started);
                cx.emit(WorkspaceEvent::Changed);
            });
        })
        .detach();
    }
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

/// Which session to select after `id` is deleted.
///
/// The nearest sibling in the same project (the next one, else the previous),
/// else the first session left anywhere, else none — so the right pane never
/// goes blank while sessions remain.
pub(crate) fn neighbor_session(sessions: &[Session], id: i64) -> Option<i64> {
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

/// Which idle connections to close to get back down to `cap`, least recently
/// used first.
///
/// `order` is the reclamation order, least recently used first. The session
/// being prompted (`active`), any session with a turn in flight (`busy`), and
/// any session whose agent cannot load it back (`unresumable`) are skipped:
/// their agent is not something to stop — and for the last, closing it would
/// lose the conversation rather than park it.
fn idle_evictions(
    order: &[i64],
    busy: &HashSet<i64>,
    unresumable: &HashSet<i64>,
    active: i64,
    cap: usize,
) -> Vec<i64> {
    let mut evictions = Vec::new();
    for &session_id in order {
        if order.len() - evictions.len() <= cap {
            break;
        }
        if session_id == active || busy.contains(&session_id) || unresumable.contains(&session_id) {
            continue;
        }
        evictions.push(session_id);
    }
    evictions
}

/// Which running terminals to stop so one more fits under `cap`, least
/// recently shown first. `order` does not include the session about to start.
fn terminal_evictions(order: &[i64], cap: usize) -> Vec<i64> {
    let mut evictions = Vec::new();
    for &session_id in order {
        if order.len() - evictions.len() < cap {
            break;
        }
        evictions.push(session_id);
    }
    evictions
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        ProjectPick, idle_evictions, neighbor_session, resolve_project_pick, terminal_evictions,
    };
    use crate::db::{Project, Session};

    fn project(id: i64, path: &str) -> Project {
        Project {
            id,
            name: format!("project-{id}"),
            path: path.into(),
        }
    }

    fn session(id: i64, project_id: i64) -> Session {
        Session {
            id,
            project_id,
            agent: "shell".into(),
            title: format!("终端 {id}"),
            cwd: "/tmp".into(),
            resume_id: None,
            model: None,
            mode: None,
            sandbox: None,
            reasoning: None,
            title_manual: false,
            has_conversation: false,
            usage: None,
        }
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

    #[test]
    fn a_fifth_terminal_stops_the_least_recently_shown() {
        assert!(terminal_evictions(&[1, 2, 3], 4).is_empty());
        assert_eq!(terminal_evictions(&[1, 2, 3, 4], 4), vec![1]);
        assert_eq!(terminal_evictions(&[1, 2, 3, 4, 5], 4), vec![1, 2]);
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

    /// Nothing is closed while the open connections are within the cap.
    #[test]
    fn nothing_is_evicted_under_the_cap() {
        assert!(idle_evictions(&[1, 2], &HashSet::new(), &HashSet::new(), 2, 3).is_empty());
        assert!(idle_evictions(&[1, 2, 3], &HashSet::new(), &HashSet::new(), 3, 3).is_empty());
    }

    /// Over the cap the least recently used connections go first.
    #[test]
    fn the_least_recently_used_idle_connections_are_evicted() {
        assert_eq!(
            idle_evictions(&[1, 2, 3, 4, 5], &HashSet::new(), &HashSet::new(), 5, 3),
            vec![1, 2]
        );
    }

    /// The session being prompted is kept even when it is the oldest.
    #[test]
    fn the_active_session_is_never_evicted() {
        assert_eq!(
            idle_evictions(&[1, 2, 3, 4], &HashSet::new(), &HashSet::new(), 1, 3),
            vec![2]
        );
    }

    /// A session with a turn in flight is skipped: its agent is running and
    /// must not be stopped to make room.
    #[test]
    fn a_busy_session_is_never_evicted() {
        let busy = HashSet::from([1_i64]);
        assert_eq!(
            idle_evictions(&[1, 2, 3, 4], &busy, &HashSet::new(), 4, 3),
            vec![2]
        );
    }

    /// A session whose agent cannot load it back is skipped too: closing its
    /// connection would strand the conversation rather than let it reconnect.
    #[test]
    fn a_session_that_cannot_be_resumed_is_never_evicted() {
        let unresumable = HashSet::from([1_i64]);
        assert_eq!(
            idle_evictions(&[1, 2, 3, 4], &HashSet::new(), &unresumable, 4, 3),
            vec![2]
        );
    }
}
