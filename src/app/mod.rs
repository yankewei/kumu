//! The application layer: projects, sessions, live terminals, and agent
//! discovery — independent of how it is drawn.
//!
//!   - this module — [`Workspace`]: projects, sessions, and live terminals;
//!   - `discovery` — what each agent offers, as its probe reports it.
//!
//! The UI hears about changes through [`WorkspaceEvent`] rather than a blanket
//! notify: a terminal frame only concerns that pane.

pub mod discovery;

use std::collections::HashMap;
use std::path::PathBuf;

use futures::StreamExt;
use gpui_kit::{Context, EventEmitter};

use crate::agent;
use crate::db::{Project, Session, Store, db_path};
use crate::term::{KeyInput, Snapshot, Term, TermColors};
use crate::theme;
use discovery::Discovery;

/// How many native agent TUIs may stay running at once.
///
/// A session past the cap is stopped, least recently shown first. Selecting it
/// again starts a new process.
const MAX_LIVE_TERMINALS: usize = 12;

/// What changed in the [`Workspace`], for the views that draw it.
pub enum WorkspaceEvent {
    /// Something the home or settings view draws: the project or session list,
    /// a session's title, what an agent offers.
    Changed,
    /// A session's terminal grid changed. Only the terminal pane draws it.
    Frame(i64),
    /// A session was removed by the app (its shell exited). `next` is who to
    /// show instead, computed before the row was gone.
    SessionClosed { id: i64, next: Option<i64> },
}

/// Projects, sessions, and each session's live terminal.
///
/// One per app, owned by the home window and shared with the settings window.
/// Nothing in here renders. Agent TUIs are launched through [`agent::interactive`];
/// protocol probes go through [`agent::probe`].
pub struct Workspace {
    store: Store,
    projects: Vec<Project>,
    sessions: Vec<Session>,
    /// Native TUI processes keyed by session id. A hidden session keeps
    /// reading, so its agent does not block on a full pty, until the cap
    /// stops it.
    terms: HashMap<i64, Term>,
    /// Least recently shown first. Only sessions whose process is still running.
    term_lru: Vec<i64>,
    /// What each agent offers in general, as probed.
    discovery: Discovery,
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = Store::open(&db_path()).expect("failed to open sqlite db");
        let mut workspace = Self {
            store,
            projects: Vec::new(),
            sessions: Vec::new(),
            terms: HashMap::new(),
            term_lru: Vec::new(),
            discovery: Discovery::default(),
        };
        workspace.bootstrap();
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
    /// none is given, returning its id.
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
            .insert_session(project_id, crate::term::SHELL, &format!("终端 {n}"), &cwd)
            .ok()?;
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
        Some(id)
    }

    /// Apply the name the user chose. It is marked as theirs, which is what
    /// keeps a title generated later from replacing it.
    pub fn rename_session(&mut self, session_id: i64, title: String, cx: &mut Context<Self>) {
        self.store.rename_session(session_id, &title).ok();
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
    }

    /// Delete a session: drop its live terminal, then remove the row.
    ///
    /// Returns the neighbor to select next — computed before the row is gone —
    /// so a caller that was looking at this session can land somewhere else.
    pub fn delete_session(&mut self, session_id: i64, cx: &mut Context<Self>) -> Option<i64> {
        let next = neighbor_session(&self.sessions, session_id);
        self.forget_terminal(session_id);
        self.store.delete_session(session_id).ok();
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
        next
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
    use super::{ProjectPick, neighbor_session, resolve_project_pick, terminal_evictions};
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
}
