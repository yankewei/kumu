//! The application layer: projects, sessions, live terminals, and agent
//! discovery — independent of how it is drawn.
//!
//!   - this module — [`Workspace`]: projects, sessions, and live terminals;
//!   - `discovery` — what each agent offers, as its probe reports it.
//!
//! The UI hears about changes through [`WorkspaceEvent`] rather than a blanket
//! notify: a terminal frame only concerns that pane.

pub mod discovery;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use gpui_kit::{Context, EventEmitter};

use crate::agent;
use crate::db::{Project, Session, Store, db_path};
use crate::term::{KeyInput, MouseInput, MouseMods, Snapshot, Term, TermColors};
use crate::theme;
use discovery::Discovery;

/// How often to re-read agent session stores for sidebar titles.
const TITLE_SYNC_EVERY: Duration = Duration::from_secs(2);

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
    /// A pane's terminal grid changed. Only that pane draws it.
    Frame(u64),
    /// A pane's shell exited cleanly. The home window closes that pane; the
    /// session row is removed only when no pane still belongs to it.
    PaneExited { pane: u64, session: i64 },
}

/// Projects, sessions, and each pane's live terminal.
///
/// One per app, owned by the home window and shared with the settings window.
/// Nothing in here renders. Agent TUIs are launched through [`agent::interactive`];
/// protocol probes go through [`agent::probe`].
///
/// A sidebar session may occupy several panes; each pane has its own PTY,
/// keyed by pane id.
pub struct Workspace {
    store: Store,
    projects: Vec<Project>,
    sessions: Vec<Session>,
    /// Native TUI processes keyed by pane id.
    terms: HashMap<u64, Term>,
    /// Which sidebar session each live pane belongs to.
    term_session: HashMap<u64, i64>,
    /// Least recently shown first. Only panes whose process is still running.
    term_lru: Vec<u64>,
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
            term_session: HashMap::new(),
            term_lru: Vec::new(),
            discovery: Discovery::default(),
        };
        workspace.bootstrap();
        workspace.probe_missing(cx);
        workspace.watch_agent_titles(cx);
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

    /// The pane's screen, after making sure its process is running for
    /// `session_id`.
    ///
    /// Agent sessions launch that agent's interactive TUI; shell sessions
    /// launch the user's `$SHELL`. An exited agent TUI starts again; an exited
    /// shell closes the pane (see [`Self::watch_terminal`]). A thirteenth
    /// running process stops the one shown least recently.
    pub fn show_terminal(
        &mut self,
        pane: u64,
        session_id: i64,
        cx: &mut Context<Self>,
    ) -> Snapshot {
        if self.term_session.get(&pane) == Some(&session_id)
            && self
                .terms
                .get(&pane)
                .is_some_and(|term| !term.snapshot().exited)
        {
            self.touch_terminal(pane);
            self.terminal_wake_handled(pane);
            return self.terms[&pane].snapshot();
        }
        self.drop_terminal(pane);
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
        self.terms.insert(pane, term);
        self.term_session.insert(pane, session_id);
        self.touch_terminal(pane);
        self.watch_terminal(pane, session_id, wake, cx);
        snapshot
    }

    /// Push the UI theme's colors into every live terminal.
    pub fn set_terminal_colors(&self, colors: TermColors) {
        for term in self.terms.values() {
            term.set_colors(colors);
        }
    }

    pub fn terminal_snapshot(&self, pane: u64) -> Option<Snapshot> {
        self.terms.get(&pane).map(Term::snapshot)
    }

    pub fn terminal_generation(&self, pane: u64) -> Option<u64> {
        self.terms.get(&pane).map(Term::generation)
    }

    pub fn terminal_wake_handled(&self, pane: u64) {
        if let Some(term) = self.terms.get(&pane) {
            term.wake_handled();
        }
    }

    pub fn terminal_key(&self, pane: u64, input: KeyInput) {
        if let Some(term) = self.terms.get(&pane) {
            term.key(input);
        }
    }

    pub fn terminal_write(&self, pane: u64, bytes: impl Into<Vec<u8>>) {
        if let Some(term) = self.terms.get(&pane) {
            term.write(bytes);
        }
    }

    pub fn terminal_paste(&self, pane: u64, text: impl Into<Vec<u8>>) {
        if let Some(term) = self.terms.get(&pane) {
            term.paste(text);
        }
    }

    /// A handle to the pane's terminal, for work that must not run on the UI
    /// thread (the selection round trips block).
    pub fn terminal_handle(&self, pane: u64) -> Option<Term> {
        self.terms.get(&pane).cloned()
    }

    pub fn terminal_mouse(&self, pane: u64, input: MouseInput) {
        if let Some(term) = self.terms.get(&pane) {
            term.mouse(input);
        }
    }

    pub fn terminal_clear_selection(&self, pane: u64) {
        if let Some(term) = self.terms.get(&pane) {
            term.clear_selection();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn terminal_scroll(
        &self,
        pane: u64,
        lines: isize,
        x: f32,
        y: f32,
        cell_w: f32,
        cell_h: f32,
        mods: MouseMods,
    ) {
        if let Some(term) = self.terms.get(&pane) {
            term.scroll(lines, x, y, cell_w, cell_h, mods);
        }
    }

    pub fn terminal_resize(&self, pane: u64, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        if let Some(term) = self.terms.get(&pane) {
            term.resize(cols, rows, cell_w, cell_h);
        }
    }

    /// Stop the process in `pane`, if any.
    pub fn drop_terminal(&mut self, pane: u64) {
        self.terms.remove(&pane);
        self.term_session.remove(&pane);
        self.term_lru.retain(|&id| id != pane);
    }

    fn touch_terminal(&mut self, pane: u64) {
        self.term_lru.retain(|&id| id != pane);
        self.term_lru.push(pane);
    }

    fn drop_session_terminals(&mut self, session_id: i64) {
        let panes: Vec<u64> = self
            .term_session
            .iter()
            .filter(|(_, sid)| **sid == session_id)
            .map(|(pane, _)| *pane)
            .collect();
        for pane in panes {
            self.drop_terminal(pane);
        }
    }

    /// Drop exited processes, then stop the least recently shown ones until
    /// one more process fits under the cap.
    fn reclaim_terminals(&mut self) {
        let dead: Vec<u64> = self
            .terms
            .iter()
            .filter(|(_, term)| term.snapshot().exited)
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            self.drop_terminal(id);
        }
        let alive: Vec<u64> = self
            .term_lru
            .iter()
            .copied()
            .filter(|id| self.terms.contains_key(id))
            .collect();
        for id in terminal_evictions(&alive, MAX_LIVE_TERMINALS) {
            self.drop_terminal(id);
        }
    }

    fn watch_terminal(
        &self,
        pane: u64,
        session_id: i64,
        mut wake: futures::channel::mpsc::UnboundedReceiver<()>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while wake.next().await.is_some() {
                let watching = this
                    .update(cx, |workspace, cx| {
                        let Some(term) = workspace.terms.get(&pane) else {
                            return false;
                        };
                        let snapshot = term.snapshot();
                        // `exit` (or any clean shell death) closes this pane.
                        // The session row stays while another pane still shows
                        // it; spawn failures keep the pane so the header can
                        // show why.
                        let shell_done = snapshot.exited
                            && snapshot.spawn_error.is_none()
                            && workspace
                                .session(session_id)
                                .is_some_and(|s| crate::term::is_shell(&s.agent));
                        if shell_done {
                            workspace.drop_terminal(pane);
                            cx.emit(WorkspaceEvent::PaneExited {
                                pane,
                                session: session_id,
                            });
                            return false;
                        }
                        cx.emit(WorkspaceEvent::Frame(pane));
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

    /// Poll each agent's on-disk session store for titles, so a shell that
    /// launched an agent picks up the name that agent wrote. The shell's
    /// process tree says which agent TUI is running under it; a title is only
    /// accepted when the session was written after that process started.
    /// `agent::disk_title` says why.
    ///
    /// The scan is file and process work — one `ps` snapshot per live terminal
    /// and a walk of the agents' stores — so it runs on the background executor
    /// and only its findings are applied on the UI thread. With no live
    /// terminal there is nothing to look up, and the round trip is skipped.
    fn watch_agent_titles(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TITLE_SYNC_EVERY).await;
                let queries = match this.update(cx, |workspace, _| workspace.title_queries()) {
                    Ok(queries) => queries,
                    Err(_) => break,
                };
                if queries.is_empty() {
                    continue;
                }
                let scans = cx
                    .background_executor()
                    .spawn(async move { queries.into_iter().map(scan_title).collect::<Vec<_>>() })
                    .await;
                let changed =
                    match this.update(cx, |workspace, _| workspace.apply_title_scan(scans)) {
                        Ok(changed) => changed,
                        Err(_) => break,
                    };
                if changed {
                    let _ = this.update(cx, |_workspace, cx| {
                        cx.emit(WorkspaceEvent::Changed);
                    });
                }
            }
        })
        .detach();
    }

    /// What a title poll needs to look at: live terminals whose titles the user
    /// has not set, with what the scan needs to ask the agents' stores. Gathered
    /// here because the PTY handles live in this struct.
    fn title_queries(&self) -> Vec<TitleQuery> {
        // One query per sidebar session. Several panes may share a session;
        // pick a stable pane (lowest id) so title claims stay deterministic.
        let mut live: Vec<(i64, u64, Option<u32>)> = self
            .terms
            .iter()
            .filter(|(_, term)| !term.snapshot().exited)
            .filter_map(|(pane, term)| {
                let session_id = *self.term_session.get(pane)?;
                Some((session_id, *pane, term.process_id()))
            })
            .collect();
        live.sort_by_key(|(session_id, pane, _)| (*session_id, *pane));
        let mut seen = HashSet::new();
        live.into_iter()
            .filter_map(|(session_id, _, pid)| {
                if !seen.insert(session_id) {
                    return None;
                }
                let session = self.sessions.iter().find(|s| s.id == session_id)?;
                if session.title_manual {
                    return None;
                }
                Some(TitleQuery {
                    session_id,
                    cwd: session.cwd.clone(),
                    pid,
                    agent_id: session.agent.clone(),
                    shell: crate::term::is_shell(&session.agent),
                    resume_id: session.resume_id.clone(),
                })
            })
            .collect()
    }

    /// Fold a batch of scans into the sessions, returning whether anything the
    /// sidebar shows changed.
    fn apply_title_scan(&mut self, scans: Vec<TitleScan>) -> bool {
        let mut claimed: HashSet<String> = self
            .sessions
            .iter()
            .filter_map(|s| s.resume_id.clone())
            .collect();
        let mut changed = false;
        for scan in scans {
            let TitleScan {
                session_id,
                binding_alive,
                bound_title,
                mut candidates,
            } = scan;
            let Some(session) = self.sessions.iter().find(|s| s.id == session_id).cloned() else {
                continue;
            };
            // A binding whose agent has exited is dropped so a later agent
            // session under the same shell can be claimed. The title stays: it
            // is still what this terminal is showing, and rewriting it to
            // "终端 N" would also lose every agent-assigned name on restart,
            // when the process is gone but the session row is not.
            if binding_alive == Some(false) {
                let _ = self.store.clear_session_resume_id(session_id);
                if let Some(resume) = session.resume_id.as_deref() {
                    claimed.remove(resume);
                }
                if let Some(row) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                    row.resume_id = None;
                }
                changed = true;
                continue;
            }
            let next = if let Some(resume) = session.resume_id.as_deref() {
                bound_title.map(|title| (resume.to_string(), title))
            } else {
                candidates.retain(|c| !claimed.contains(&c.resume_id));
                candidates
                    .into_iter()
                    .max_by_key(|c| c.score)
                    .map(|c| (c.resume_id, c.title))
            };
            let Some((resume_id, title)) = next else {
                continue;
            };
            if session.resume_id.as_deref() != Some(resume_id.as_str()) {
                if self
                    .store
                    .set_session_resume_id(session_id, &resume_id)
                    .is_ok()
                {
                    claimed.insert(resume_id.clone());
                    if let Some(row) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                        row.resume_id = Some(resume_id);
                    }
                    changed = true;
                }
            } else {
                claimed.insert(resume_id);
            }
            if session.title != title
                && self
                    .store
                    .update_session_title(session_id, &title)
                    .unwrap_or(false)
            {
                if let Some(row) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                    row.title = title;
                }
                changed = true;
            }
        }
        changed
    }

    /// Delete a session: drop every pane terminal bound to it, then remove the row.
    ///
    /// Returns the neighbor to select next — computed before the row is gone —
    /// so a caller that was looking at this session can land somewhere else.
    pub fn delete_session(&mut self, session_id: i64, cx: &mut Context<Self>) -> Option<i64> {
        let next = neighbor_session(&self.sessions, session_id);
        self.drop_session_terminals(session_id);
        self.store.delete_session(session_id).ok();
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
        next
    }

    /// Probe the agents nothing is known about yet — never probed, or probed
    /// and failed — so the settings window has a catalogue before any session
    /// has connected. Not-installed agents are dropped in [`Self::probe`].
    pub fn probe_missing(&mut self, cx: &mut Context<Self>) {
        let missing = self.discovery.missing();
        self.probe(&missing, cx);
    }

    /// Probe `agents` again, replacing what is cached as the answers arrive.
    /// An agent whose probe is already out is left to that one, and an agent
    /// that is not installed here is skipped: there is nothing to ask.
    pub fn probe(&mut self, agents: &[&'static str], cx: &mut Context<Self>) {
        let launchable: Vec<(&'static str, agent::Config)> = agents
            .iter()
            .filter_map(|&id| Some((id, agent::Config::for_agent(id)?)))
            .filter(|(_, cfg)| cfg.installed())
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

/// One live session as a title poll sees it: everything the off-thread scan
/// needs, copied off the [`Workspace`] so the scan can run on the background
/// executor.
struct TitleQuery {
    session_id: i64,
    cwd: String,
    pid: Option<u32>,
    agent_id: String,
    shell: bool,
    resume_id: Option<String>,
}

/// What a scan found for one query: the verdict on a stored binding, or the
/// candidates an unbound session could claim.
struct TitleScan {
    session_id: i64,
    /// `Some(false)` when the stored binding no longer belongs to the terminal.
    binding_alive: Option<bool>,
    /// The title a live binding resolves to.
    bound_title: Option<String>,
    candidates: Vec<agent::DiskTitle>,
}

/// Read one session's agent stores. Runs on the background executor: it shells
/// out to `ps` and walks the agents' session files, which is far too slow for
/// the UI thread's two-second poll.
fn scan_title(query: TitleQuery) -> TitleScan {
    let TitleQuery {
        session_id,
        cwd,
        pid,
        agent_id,
        shell,
        resume_id,
    } = query;
    let mut scan = TitleScan {
        session_id,
        binding_alive: None,
        bound_title: None,
        candidates: Vec::new(),
    };
    match resume_id.as_deref() {
        // A shell's binding has to be re-proved every poll: the agent it names
        // may have exited, and a cwd-only match must not let this terminal keep
        // someone else's name.
        Some(resume) if shell => {
            let alive = agent::resume_belongs_to_terminal(resume, pid);
            scan.binding_alive = Some(alive);
            if alive {
                scan.bound_title = agent::lookup_disk_title(resume);
            }
        }
        // A non-shell session is the agent itself; its own store answers, and
        // there is no process to re-check.
        Some(resume) => scan.bound_title = agent::lookup_disk_title(resume),
        None => {
            let prefer = (!shell).then_some(agent_id.as_str());
            scan.candidates = agent::discover_disk_title(&cwd, pid, prefer, shell);
        }
    }
    scan
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
fn terminal_evictions(order: &[u64], cap: usize) -> Vec<u64> {
    let mut evictions = Vec::new();
    for &pane in order {
        if order.len() - evictions.len() < cap {
            break;
        }
        evictions.push(pane);
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
            resume_id: None,
            title_manual: false,
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
