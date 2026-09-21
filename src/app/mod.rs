//! The application layer: what kumu does with agents, independent of how it is
//! drawn. The UI renders and reads this; the agent layer below it speaks the
//! protocols, and the store keeps what outlives a run.
//!
//!   - this module — [`Workspace`]: projects, sessions, and the entity that owns
//!     them;
//!   - `session`   — a session's flow: sending, connections, the events a live
//!     agent reports, permissions and picks;
//!   - `turn`      — a live turn as plain data, and how agent events fold into it;
//!   - `discovery` — what each agent offers, as its probe reports it.
//!
//! The UI hears about changes through [`WorkspaceEvent`] rather than a blanket
//! notify: a streamed chunk only concerns the conversation, and notifying the
//! whole home view per chunk is what made the window stutter.

pub mod discovery;
mod session;
pub mod turn;

use std::collections::HashMap;
use std::path::PathBuf;

use gpui_kit::{Context, EventEmitter};

use crate::agent;
use crate::db::{Message, Project, Session, Store, db_path};
use turn::{PendingPermission, Turn};

pub use session::Axis;

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
/// One per app, owned by the home window. Nothing in here renders, and nothing
/// in here names a protocol: agents are reached only through [`agent`]'s
/// vocabulary.
pub struct Workspace {
    store: Store,
    projects: Vec<Project>,
    sessions: Vec<Session>,
    /// Live connections keyed by session id, reused across prompts.
    conns: HashMap<i64, agent::Connection>,
    /// Each session's turn in flight: streaming reasoning, tool calls, and the
    /// reply being built. Dropped when the turn ends.
    turns: HashMap<i64, Turn>,
    /// The commands each live session reported, with the agent that reported
    /// them. What a session can run depends on its working directory (a
    /// project's own commands and skills), so one session's list must not stand
    /// in for another's; [`discovery::AgentCommands`] only fills in until a
    /// session reports.
    live_commands: HashMap<i64, (&'static str, Vec<agent::CommandInfo>)>,
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
            turns: HashMap::new(),
            live_commands: HashMap::new(),
        };
        workspace.bootstrap();
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
                .insert_session(project.id, "omp", "会话 1", &project.path)
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

    /// The commands the session's own connection reported, with the agent that
    /// reported them.
    pub fn live_commands(&self, session_id: i64) -> Option<(&'static str, &[agent::CommandInfo])> {
        self.live_commands
            .get(&session_id)
            .map(|(agent, commands)| (*agent, commands.as_slice()))
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
            .insert_session(project_id, "omp", &format!("会话 {n}"), &cwd)
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
        self.conns.remove(&session_id);
        self.live_commands.remove(&session_id);
        self.take_turn(session_id, cx);
        self.store.delete_session(session_id).ok();
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
    }

    /// Persist and show a system notice (unsupported agent, agent error…).
    fn system_message(&mut self, session_id: i64, text: String, cx: &mut Context<Self>) {
        self.store.insert_message(session_id, "system", &text).ok();
        cx.emit(WorkspaceEvent::HistoryChanged(session_id));
    }

    /// Probe agents whose session options aren't cached yet, so the composer's
    /// pickers have data before any session has connected.
    fn probe_missing(&mut self, cx: &mut Context<Self>) {
        if !cx.has_global::<discovery::AgentSessionOptions>() {
            cx.set_global(discovery::AgentSessionOptions::default());
        }
        let cached = &cx.global::<discovery::AgentSessionOptions>().0;
        let missing: Vec<&'static str> = agent::AGENTS
            .iter()
            .map(|a| a.id)
            .filter(|id| !cached.contains_key(id))
            .collect();
        if missing.is_empty() {
            return;
        }
        let mut reports = discovery::probe(&missing);
        cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(ev) = reports.next().await {
                if this
                    .update(cx, |workspace, cx| workspace.on_probe_event(ev, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// A probe speaks for the agent in general, so what it reports is what
    /// every session of that agent starts from.
    fn on_probe_event(&mut self, ev: agent::Event, cx: &mut Context<Self>) {
        match ev {
            agent::Event::Commands { agent, commands } => {
                if !cx.has_global::<discovery::AgentCommands>() {
                    cx.set_global(discovery::AgentCommands::default());
                }
                cx.global_mut::<discovery::AgentCommands>()
                    .0
                    .insert(agent, commands);
                cx.emit(WorkspaceEvent::Changed);
            }
            ev => self.cache_options(&ev, cx),
        }
    }

    /// Cache the options an agent reported, when it reported any.
    fn cache_options(&mut self, ev: &agent::Event, cx: &mut Context<Self>) {
        let Some((agent, options)) = discovery::SessionOptions::from_event(ev) else {
            return;
        };
        if options.is_empty() {
            return;
        }
        if !cx.has_global::<discovery::AgentSessionOptions>() {
            cx.set_global(discovery::AgentSessionOptions::default());
        }
        cx.global_mut::<discovery::AgentSessionOptions>()
            .0
            .insert(agent, options);
        cx.emit(WorkspaceEvent::Changed);
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

#[cfg(test)]
mod tests {
    use super::{ProjectPick, resolve_project_pick};
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
}
