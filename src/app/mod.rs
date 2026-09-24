//! The application layer: what kumu does with agents, independent of how it is
//! drawn. The UI renders and reads this; the agent layer below it speaks the
//! protocols, and the store keeps what outlives a run.
//!
//!   - this module — [`Workspace`]: projects, sessions, and the entity that owns
//!     them;
//!   - `session`   — a session's flow: sending, connections, the events a live
//!     agent reports, permissions and picks;
//!   - `turn`      — a live turn as plain data, and how agent events fold into it;
//!   - `discovery` — what each agent offers, as its probe reports it;
//!   - `attachments` — images attached to prompts, kept beside the database;
//!   - `files`     — the names a project's files go by, for `@` references.
//!
//! The UI hears about changes through [`WorkspaceEvent`] rather than a blanket
//! notify: a streamed chunk only concerns the conversation, and notifying the
//! whole home view per chunk is what made the window stutter.

pub mod attachments;
pub mod discovery;
mod files;
mod session;
pub mod turn;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::{AppContext, Context, EventEmitter};

use crate::agent;
use crate::db::{Message, Project, Session, Store, db_path};
use discovery::{Discovery, SessionOptions};
use turn::{PendingPermission, Turn};

pub use session::Axis;

/// The agent a new session starts with, until the user picks another.
const DEFAULT_AGENT: &str = "omp";

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

/// What changed in the [`Workspace`], for the views that draw it.
pub enum WorkspaceEvent {
    /// Something the home view draws: the project or session list, a session's
    /// title or picks, a turn opening or closing, a permission prompt, what an
    /// agent offers.
    Changed,
    /// A session's live turn changed — streamed text, reasoning, a tool update.
    /// Only the conversation draws it.
    TurnChanged(i64),
    /// A session's live turn ended; anything cached to draw it can go.
    TurnClosed { session_id: i64, turn_id: u64 },
    /// A session's saved history changed: a prompt, a finished turn, a notice.
    HistoryChanged(i64),
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
    /// The commands each live session reported, with the agent that reported
    /// them. What a session can run depends on its working directory (a
    /// project's own commands and skills), so one session's list must not stand
    /// in for another's; the agent's probed list only fills in until a session
    /// reports.
    live_commands: HashMap<i64, (&'static str, Vec<agent::CommandInfo>)>,
    /// The options each live session reported, with the agent that reported
    /// them: the models it offers, and the values in force on *its* process.
    /// Two sessions of one agent may run different models — and a Codex model
    /// has its own efforts — so this is per session, over the agent's catalog.
    live_options: HashMap<i64, (&'static str, SessionOptions)>,
    /// What each agent offers in general, as probed.
    discovery: Discovery,
    /// Each project directory's files, as last listed, for completing `@`
    /// references; keyed by the directory a session runs in.
    project_files: HashMap<String, Arc<files::FileIndex>>,
    /// What each session's agent last reported about its context, decoded once
    /// from the store at start and kept current by [`Self::record_usage`].
    usage: HashMap<i64, agent::Usage>,
    /// Directories being listed right now, so a burst of keystrokes does not
    /// start a listing per key.
    listing_files: HashSet<String>,
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
            no_resume_notified: HashSet::new(),
            unresumable: HashSet::new(),
            reclaim_notified: HashSet::new(),
            turns: HashMap::new(),
            live_commands: HashMap::new(),
            live_options: HashMap::new(),
            discovery: Discovery::default(),
            project_files: HashMap::new(),
            usage: HashMap::new(),
            listing_files: HashSet::new(),
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

    /// A first run gets a project for the current directory and a session in
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
                .insert_session(project.id, DEFAULT_AGENT, "会话 1", &project.path)
                .ok();
            self.refresh();
        }
    }

    fn refresh(&mut self) {
        self.projects = self.store.projects();
        self.sessions = self.store.sessions();
    }

    /// The entries under `dir` that a reference typed as `query` could mean,
    /// best first; empty until the directory has been listed (see
    /// [`Self::refresh_project_files`]).
    pub fn file_matches(&self, dir: &str, query: &str, limit: usize) -> Vec<String> {
        self.project_files
            .get(dir)
            .map(|entries| {
                files::rank(entries, query, limit)
                    .into_iter()
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// List `dir`'s files again, off the main thread.
    ///
    /// Called when a reference starts being typed rather than on every key: a
    /// file created since the last listing shows up the next time an `@` is
    /// typed, and the keys in between search the list already in hand.
    pub fn refresh_project_files(&mut self, dir: &str, cx: &mut Context<Self>) {
        if !self.listing_files.insert(dir.to_string()) {
            return;
        }
        let dir = dir.to_string();
        let root = PathBuf::from(&dir);
        let task = cx.background_spawn(async move { files::list(&root) });
        cx.spawn(async move |this, cx| {
            let entries = task.await;
            let _ = this.update(cx, |workspace, cx| {
                workspace.listing_files.remove(&dir);
                workspace.project_files.insert(dir, Arc::new(entries));
                cx.emit(WorkspaceEvent::Changed);
            });
        })
        .detach();
    }

    /// How full the session's context is and what it has cost, as its agent
    /// last reported; `None` before any report.
    pub fn usage(&self, session_id: i64) -> Option<&agent::Usage> {
        self.usage.get(&session_id)
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

    /// The options the session's pickers offer: what its own connection
    /// reported, else — before it has one, or after its agent changed — what
    /// the agent's probe reported.
    pub fn session_options(&self, session: &Session) -> Option<&SessionOptions> {
        self.live_options
            .get(&session.id)
            .filter(|(agent, _)| *agent == session.agent)
            .map(|(_, options)| options)
            .or_else(|| self.discovery.catalog(&session.agent))
    }

    /// The commands the session can run, from the same two sources as
    /// [`Self::session_options`].
    ///
    /// Empty for an agent that has reported none (yet), which is what keeps the
    /// menu out of that session's composer rather than offering commands that
    /// could not run.
    pub fn session_commands(&self, session: &Session) -> &[agent::CommandInfo] {
        self.live_commands
            .get(&session.id)
            .filter(|(agent, _)| *agent == session.agent)
            .map(|(_, commands)| commands.as_slice())
            .or_else(|| self.discovery.commands(&session.agent))
            .unwrap_or_default()
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

    /// Start a session in `project_id`, or in the first project when none is
    /// given, returning its id.
    pub fn new_session(&mut self, project_id: Option<i64>, cx: &mut Context<Self>) -> Option<i64> {
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
            .insert_session(project_id, DEFAULT_AGENT, &format!("会话 {n}"), &cwd)
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
    pub fn delete_session(&mut self, session_id: i64, cx: &mut Context<Self>) {
        self.cancel_pending_permissions(session_id);
        self.forget_connection(session_id);
        self.live_commands.remove(&session_id);
        self.live_options.remove(&session_id);
        self.no_resume_notified.remove(&session_id);
        self.unresumable.remove(&session_id);
        self.reclaim_notified.remove(&session_id);
        self.usage.remove(&session_id);
        self.take_turn(session_id, cx);
        self.store.delete_session(session_id).ok();
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
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
    ///
    /// The last-reported commands and options are left in place: they still
    /// describe what the agent offers, and a reconnect refreshes them.
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
    /// and failed — so the pickers have something to offer before any session
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{ProjectPick, idle_evictions, resolve_project_pick};
    use crate::db::Project;

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
