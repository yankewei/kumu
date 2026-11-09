//! The application layer: projects, sessions, and live terminals —
//! independent of how it is drawn.
//!
//! The UI hears about changes through [`WorkspaceEvent`] rather than a blanket
//! notify: a terminal frame only concerns that pane.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui_kit::{Context, EventEmitter, Global};

use crate::agent;
use crate::db::{Project, Session, Store, db_path};
use crate::term::{KeyInput, MouseInput, MouseMods, Snapshot, Term, TermColors, TermNotification};
use crate::theme;

/// Shared by the home and settings windows; changes apply immediately.
pub struct NotificationSettings {
    pub enabled: bool,
    /// Play the macOS alert sound when an attention notification is raised.
    pub sound: bool,
}

impl Global for NotificationSettings {}

/// How often to re-read agent session stores for sidebar titles.
const TITLE_SYNC_EVERY: Duration = Duration::from_secs(2);

/// How many native agent TUIs may stay running at once.
///
/// A session past the cap is stopped, least recently shown first — but only a
/// parked one: a terminal a pane is showing is never stopped, so the cap is
/// soft when every process is on screen. Selecting a stopped session again
/// starts a new process.
const MAX_LIVE_TERMINALS: usize = 12;

/// What changed in the [`Workspace`], for the views that draw it.
pub enum WorkspaceEvent {
    /// Something the home or settings view draws: the project or session list,
    /// or a session's title.
    Changed,
    /// A persistence write failed. The home window shows the message until the
    /// user dismisses it, instead of leaving the action looking like a no-op.
    Failed(String),
    /// A pane's terminal grid changed. Only that pane draws it.
    Frame(u64),
    /// An explicit OSC notification from a visible or parked terminal.
    Attention {
        terminal: u64,
        session: i64,
        notification: TermNotification,
    },
    /// A pane's shell exited cleanly. The home window closes that pane; the
    /// session row is removed only when no pane still belongs to it.
    PaneExited { pane: u64, session: i64 },
}

/// One native TUI process and where it is drawn.
struct LiveTerm {
    term: Term,
    /// Sidebar session this process belongs to.
    session: i64,
    /// The pane showing it, or `None` while parked: its pane switched to
    /// another session, and the process keeps running (and reading) so
    /// switching back reattaches the same TUI instead of restarting it.
    pane: Option<u64>,
    last_notification: Option<(Instant, TermNotification)>,
}

/// Projects, sessions, and each live terminal.
///
/// One per app, owned by the home window. Nothing in here renders. Agent TUIs
/// are launched through [`agent::interactive`].
///
/// A sidebar session may occupy several panes; each pane has its own PTY. A
/// pane switching sessions parks its process rather than killing it, so a
/// hidden agent keeps its state until the cap stops it.
pub struct Workspace {
    store: Store,
    /// Whether `store` is the on-disk database. An in-memory fallback still
    /// gets its first-run project and shell, because nothing else persists.
    persistent: bool,
    projects: Vec<Project>,
    sessions: Vec<Session>,
    /// Native TUI processes keyed by a stable terminal id.
    terms: HashMap<u64, LiveTerm>,
    /// Which terminal each pane shows right now.
    pane_term: HashMap<u64, u64>,
    /// Least recently shown first. Only terminals whose process is still running.
    term_lru: Vec<u64>,
    next_term: u64,
    /// A database failure from before any view could subscribe. The home window
    /// reads it once at construction and shows it like a later [`WorkspaceEvent::Failed`].
    startup_error: Option<String>,
    /// The home window's last layout JSON, handed to the view once at startup.
    saved_workspace: Option<String>,
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // A database that cannot be opened must not take the window down with
        // it: keep working from memory for this run, and say that nothing will
        // be saved rather than dying before there is anywhere to say it.
        let (store, mut startup_error, persistent) = match Store::open(&db_path()) {
            Ok(store) => (store, None, true),
            Err(error) => (
                Store::in_memory().expect("in-memory sqlite db"),
                Some(format!("无法打开本地数据库：{error}。本次更改不会保存。")),
                false,
            ),
        };
        let saved_workspace = match store.workspace_state() {
            Ok(state) => state,
            Err(error) => {
                startup_error.get_or_insert_with(|| format!("读取工作区布局失败：{error}。"));
                None
            }
        };
        let mut workspace = Self {
            store,
            persistent,
            projects: Vec::new(),
            sessions: Vec::new(),
            terms: HashMap::new(),
            pane_term: HashMap::new(),
            term_lru: Vec::new(),
            next_term: 1,
            startup_error,
            saved_workspace,
        };
        workspace.bootstrap();
        workspace.watch_agent_titles(cx);
        workspace
    }

    /// Report a persistence failure to whichever view is showing this
    /// workspace. The action itself is left as it was.
    fn failed(cx: &mut Context<Self>, action: &str, error: impl std::fmt::Display) {
        cx.emit(WorkspaceEvent::Failed(format!("{action}：{error}")));
    }

    /// Take the failure recorded before any view subscribed.
    pub fn take_startup_error(&mut self) -> Option<String> {
        self.startup_error.take()
    }

    /// Take the layout JSON loaded before the home view existed.
    pub fn take_saved_workspace(&mut self) -> Option<String> {
        self.saved_workspace.take()
    }

    /// Replace the saved layout, reporting a write failure to the home window.
    pub fn save_workspace_state(&self, json: &str, cx: &mut Context<Self>) -> bool {
        match self.store.save_workspace_state(json) {
            Ok(()) => true,
            Err(error) => {
                Self::failed(cx, "保存工作区布局失败", error);
                false
            }
        }
    }

    /// A first run gets a project for the current directory and a shell in
    /// it, so the window never opens onto nothing. The seeded flag keeps an
    /// intentionally emptied workspace empty on later launches.
    fn bootstrap(&mut self) {
        let seeded = self.persistent && crate::db::get_setting(crate::db::SETTING_SEEDED).is_some();
        if !seeded {
            let mut seeded_ok = true;
            if self.store.projects().is_empty() {
                seeded_ok = insert_bootstrap_project(&self.store).is_some();
            }
            self.refresh();
            if seeded_ok
                && self.sessions.is_empty()
                && let Some(project) = self.projects.first().cloned()
            {
                seeded_ok = self
                    .store
                    .insert_session(project.id, crate::term::SHELL, "终端 1", &project.path)
                    .is_ok();
            }
            if seeded_ok && self.persistent {
                let _ = crate::db::set_setting(crate::db::SETTING_SEEDED, "1");
            }
        }
        self.refresh();
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

    /// Mark a session as used for the sidebar's recency order. The in-memory
    /// order is deliberately left alone, so rows do not jump under the pointer
    /// while the user is working; the next launch shows the new order.
    pub fn touch_session(&self, session_id: i64) {
        self.store.touch_session(session_id).ok();
    }

    /// The pane's screen, after making sure its process is running for
    /// `session_id`.
    ///
    /// Agent sessions launch that agent's interactive TUI; shell sessions
    /// launch the user's `$SHELL`. An exited agent TUI starts again; an exited
    /// shell closes the pane (see [`Self::watch_terminal`]). Switching the pane
    /// to another session parks the old process and reattaches it when that
    /// session is shown again. Starting a process past the cap stops the parked
    /// one shown least recently; a pane's visible terminal is never stopped.
    pub fn show_terminal(
        &mut self,
        pane: u64,
        session_id: i64,
        cx: &mut Context<Self>,
    ) -> Snapshot {
        if let Some(term_id) = self.pane_term.get(&pane).copied()
            && let Some(live) = self.terms.get(&term_id)
            && live.session == session_id
            && !live.term.snapshot().exited
        {
            let term = live.term.clone();
            self.touch_terminal(term_id);
            term.wake_handled();
            return term.snapshot();
        }
        self.park_terminal(pane);
        let Some(session) = self.session(session_id).cloned() else {
            return Snapshot::blank("", "");
        };
        // This session was shown before: reattach its process and screen. Try
        // this before reclaiming, because a reattach starts no new process and
        // must not cost some other terminal its process — least of all its own.
        if let Some(term_id) = self.parked_terminal(session_id) {
            self.terms.get_mut(&term_id).unwrap().pane = Some(pane);
            self.pane_term.insert(pane, term_id);
            self.touch_terminal(term_id);
            let term = self.terms[&term_id].term.clone();
            term.wake_handled();
            return term.snapshot();
        }
        self.reclaim_terminals();
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
        let term_id = self.next_term;
        self.next_term += 1;
        self.terms.insert(
            term_id,
            LiveTerm {
                term,
                session: session_id,
                pane: Some(pane),
                last_notification: None,
            },
        );
        self.pane_term.insert(pane, term_id);
        self.touch_terminal(term_id);
        self.watch_terminal(term_id, session_id, wake, cx);
        snapshot
    }

    /// A removed terminal or the terminal being read no longer needs attention.
    /// `focused_pane` is absent while the home window is inactive.
    pub fn notification_is_read(&self, terminal: u64, focused_pane: Option<u64>) -> bool {
        match self.terms.get(&terminal) {
            None => true,
            Some(live) => live.pane.is_some() && live.pane == focused_pane,
        }
    }

    /// Bring the exact terminal that sent a notification back into a pane.
    /// A parked process is reattached without restarting its agent.
    pub fn reveal_terminal(&mut self, terminal: u64, fallback_pane: u64) -> Option<(u64, i64)> {
        let live = self.terms.get(&terminal)?;
        let session = live.session;
        let pane = live.pane.unwrap_or(fallback_pane);
        if live.pane.is_none() {
            self.park_terminal(pane);
            self.terms.get_mut(&terminal)?.pane = Some(pane);
            self.pane_term.insert(pane, terminal);
        }
        self.touch_terminal(terminal);
        Some((pane, session))
    }

    /// Push the UI theme's colors into every live terminal.
    pub fn set_terminal_colors(&self, colors: TermColors) {
        for live in self.terms.values() {
            live.term.set_colors(colors);
        }
    }

    fn shown_term(&self, pane: u64) -> Option<&LiveTerm> {
        self.terms.get(self.pane_term.get(&pane)?)
    }

    pub fn terminal_snapshot(&self, pane: u64) -> Option<Snapshot> {
        self.shown_term(pane).map(|live| live.term.snapshot())
    }

    pub fn terminal_generation(&self, pane: u64) -> Option<u64> {
        self.shown_term(pane).map(|live| live.term.generation())
    }

    pub fn terminal_wake_handled(&self, pane: u64) {
        if let Some(live) = self.shown_term(pane) {
            live.term.wake_handled();
        }
    }

    pub fn terminal_key(&self, pane: u64, input: KeyInput) {
        if let Some(live) = self.shown_term(pane) {
            live.term.key(input);
        }
    }

    pub fn terminal_write(&self, pane: u64, bytes: impl Into<Vec<u8>>) {
        if let Some(live) = self.shown_term(pane) {
            live.term.write(bytes);
        }
    }

    pub fn terminal_paste(&self, pane: u64, text: impl Into<Vec<u8>>) {
        if let Some(live) = self.shown_term(pane) {
            live.term.paste(text);
        }
    }

    /// A handle to the pane's terminal, for work that must not run on the UI
    /// thread (the selection round trips block).
    pub fn terminal_handle(&self, pane: u64) -> Option<Term> {
        self.shown_term(pane).map(|live| live.term.clone())
    }

    pub fn terminal_mouse(&self, pane: u64, input: MouseInput) {
        if let Some(live) = self.shown_term(pane) {
            live.term.mouse(input);
        }
    }

    pub fn terminal_clear_selection(&self, pane: u64) {
        if let Some(live) = self.shown_term(pane) {
            live.term.clear_selection();
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
        if let Some(live) = self.shown_term(pane) {
            live.term.scroll(lines, x, y, cell_w, cell_h, mods);
        }
    }

    pub fn terminal_resize(&self, pane: u64, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        if let Some(live) = self.shown_term(pane) {
            live.term.resize(cols, rows, cell_w, cell_h);
        }
    }

    /// Stop the process shown in `pane`, and any parked ones for the session it
    /// was showing — closing the last pane of a session ends the terminal.
    pub fn drop_terminal(&mut self, pane: u64) {
        let Some(term_id) = self.pane_term.remove(&pane) else {
            return;
        };
        let session = self.terms.get(&term_id).map(|live| live.session);
        self.drop_term(term_id);
        let Some(session) = session else {
            return;
        };
        // Another pane still shows the session; its parked siblings belong to
        // this pane's shell and go with it.
        let parked: Vec<u64> = self
            .terms
            .iter()
            .filter(|(_, live)| live.session == session && live.pane.is_none())
            .map(|(id, _)| *id)
            .collect();
        for id in parked {
            self.drop_term(id);
        }
    }

    /// Park the process `pane` shows so switching back reattaches it. Exited
    /// processes, duplicate terminals for a session another pane still shows,
    /// and older parked ones are stopped instead.
    fn park_terminal(&mut self, pane: u64) {
        let Some(term_id) = self.pane_term.remove(&pane) else {
            return;
        };
        let Some(live) = self.terms.get_mut(&term_id) else {
            return;
        };
        live.pane = None;
        let session = live.session;
        let exited = live.term.snapshot().exited;
        let shown_elsewhere = self
            .terms
            .values()
            .any(|other| other.session == session && other.pane.is_some());
        if exited || shown_elsewhere {
            self.drop_term(term_id);
            return;
        }
        // One hidden terminal per session is enough to switch back to.
        if let Some(old) = self.parked_terminal(session).filter(|&id| id != term_id) {
            self.drop_term(old);
        }
    }

    /// The session's hidden terminal, most recently shown first.
    fn parked_terminal(&self, session_id: i64) -> Option<u64> {
        self.term_lru.iter().rev().copied().find(|id| {
            self.terms.get(id).is_some_and(|live| {
                live.pane.is_none() && live.session == session_id && !live.term.snapshot().exited
            })
        })
    }

    /// Stop one terminal process and forget where it was shown.
    fn drop_term(&mut self, term_id: u64) {
        if let Some(live) = self.terms.remove(&term_id)
            && let Some(pane) = live.pane
            && self.pane_term.get(&pane) == Some(&term_id)
        {
            self.pane_term.remove(&pane);
        }
        self.term_lru.retain(|&id| id != term_id);
    }

    fn touch_terminal(&mut self, term_id: u64) {
        self.term_lru.retain(|&id| id != term_id);
        self.term_lru.push(term_id);
    }

    fn drop_session_terminals(&mut self, session_id: i64) {
        let terms: Vec<u64> = self
            .terms
            .iter()
            .filter(|(_, live)| live.session == session_id)
            .map(|(id, _)| *id)
            .collect();
        for id in terms {
            self.drop_term(id);
        }
    }

    /// Drop exited processes, then stop parked ones until one more process fits
    /// under the cap.
    ///
    /// Only parked terminals are candidates. Dropping a visible one would
    /// remove its pane's mapping and leave a pane that still believes it has a
    /// session while nothing answers its input; the cap is soft when every
    /// running process is on screen.
    fn reclaim_terminals(&mut self) {
        let dead: Vec<u64> = self
            .terms
            .iter()
            .filter(|(_, live)| live.term.snapshot().exited)
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            self.drop_term(id);
        }
        let alive: Vec<u64> = self
            .term_lru
            .iter()
            .copied()
            .filter(|id| self.terms.contains_key(id))
            .collect();
        let parked: Vec<u64> = alive
            .iter()
            .copied()
            .filter(|id| self.terms.get(id).is_some_and(|live| live.pane.is_none()))
            .collect();
        for id in terminal_evictions(&parked, alive.len(), MAX_LIVE_TERMINALS) {
            self.drop_term(id);
        }
    }

    fn watch_terminal(
        &self,
        term_id: u64,
        session_id: i64,
        mut wake: futures::channel::mpsc::UnboundedReceiver<()>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while wake.next().await.is_some() {
                let watching = this
                    .update(cx, |workspace, cx| {
                        let Some(live) = workspace.terms.get_mut(&term_id) else {
                            return false;
                        };
                        let snapshot = live.term.snapshot();
                        let pane = live.pane;
                        let term = live.term.clone();
                        while let Some(notification) = term.take_notification() {
                            let now = Instant::now();
                            if !live
                                .last_notification
                                .as_ref()
                                .is_some_and(|(last, previous)| {
                                    now.duration_since(*last) < Duration::from_secs(2)
                                        && previous == &notification
                                })
                            {
                                live.last_notification = Some((now, notification.clone()));
                                cx.emit(WorkspaceEvent::Attention {
                                    terminal: term_id,
                                    session: session_id,
                                    notification,
                                });
                            }
                        }
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
                            workspace.drop_term(term_id);
                            if let Some(pane) = pane {
                                cx.emit(WorkspaceEvent::PaneExited {
                                    pane,
                                    session: session_id,
                                });
                            }
                            return false;
                        }
                        match pane {
                            Some(pane) => cx.emit(WorkspaceEvent::Frame(pane)),
                            // Parked: keep reading so it does not block on a
                            // full pty, but no pane draws it.
                            None => term.wake_handled(),
                        }
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

    /// Add the folder at `path` as a project, named after the folder itself —
    /// or, when it already is one, return that project instead of inserting a
    /// duplicate row.
    pub fn add_project(&mut self, path: &str, cx: &mut Context<Self>) -> Option<i64> {
        let name = match resolve_project_pick(&self.projects, path) {
            ProjectPick::Existing(id) => return Some(id),
            ProjectPick::New { name } => name,
        };
        let id = match self.store.insert_project(&name, path) {
            Ok(id) => id,
            Err(error) => {
                Self::failed(cx, "添加项目失败", error);
                return None;
            }
        };
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
        Some(id)
    }

    /// Rename a project without moving it or its sessions.
    pub fn rename_project(&mut self, project_id: i64, name: String, cx: &mut Context<Self>) {
        if let Err(error) = self.store.rename_project(project_id, &name) {
            Self::failed(cx, "重命名项目失败", error);
            return;
        }
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
    }

    /// Delete a project and every session in it. Terminals are stopped only
    /// after the row is gone, so a failed write leaves the workspace untouched.
    ///
    /// Returns the first session left in another project, for the panes that
    /// were showing the deleted project to land on.
    pub fn delete_project(
        &mut self,
        project_id: i64,
        cx: &mut Context<Self>,
    ) -> Result<Option<i64>, ()> {
        let sessions: Vec<i64> = self
            .sessions
            .iter()
            .filter(|session| session.project_id == project_id)
            .map(|session| session.id)
            .collect();
        let next = self
            .sessions
            .iter()
            .find(|session| session.project_id != project_id)
            .map(|session| session.id);
        if let Err(error) = self.store.delete_project(project_id) {
            Self::failed(cx, "删除项目失败", error);
            return Err(());
        }
        for session_id in sessions {
            self.drop_session_terminals(session_id);
        }
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
        Ok(next)
    }

    /// Start a shell session in `project_id`, or in the first project when
    /// none is given, returning its id.
    pub fn new_session(&mut self, project_id: Option<i64>, cx: &mut Context<Self>) -> Option<i64> {
        let project_id = match project_id.or_else(|| self.projects.first().map(|p| p.id)) {
            Some(id) => id,
            None => {
                // The row was just inserted, so pick up its path before reading
                // the cwd below.
                let Some(id) = insert_bootstrap_project(&self.store) else {
                    cx.emit(WorkspaceEvent::Failed("无法创建默认项目".to_string()));
                    return None;
                };
                self.refresh();
                id
            }
        };
        let cwd = self
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .map(|p| p.path.clone())
            .unwrap_or_default();
        let n = self.sessions.len() + 1;
        let id = match self.store.insert_session(
            project_id,
            crate::term::SHELL,
            &format!("终端 {n}"),
            &cwd,
        ) {
            Ok(id) => id,
            Err(error) => {
                Self::failed(cx, "新建会话失败", error);
                return None;
            }
        };
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
        Some(id)
    }

    /// Apply the name the user chose. It is marked as theirs, which is what
    /// keeps a title generated later from replacing it.
    pub fn rename_session(&mut self, session_id: i64, title: String, cx: &mut Context<Self>) {
        if let Err(error) = self.store.rename_session(session_id, &title) {
            Self::failed(cx, "重命名会话失败", error);
            return;
        }
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
        // One query per sidebar session. Several terminals may share a session;
        // prefer one a pane still shows, then the lowest terminal id, so title
        // claims stay deterministic.
        let mut live: Vec<(i64, bool, u64, Option<u32>)> = self
            .terms
            .iter()
            .filter(|(_, live)| !live.term.snapshot().exited)
            .map(|(id, live)| {
                (
                    live.session,
                    live.pane.is_none(),
                    *id,
                    live.term.process_id(),
                )
            })
            .collect();
        live.sort_by_key(|(session_id, parked, term_id, _)| (*session_id, *parked, *term_id));
        let mut seen = HashSet::new();
        live.into_iter()
            .filter_map(|(session_id, _, _, pid)| {
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
                    agent_session_id: session.agent_session_id.clone(),
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
            .filter_map(|s| s.agent_session_id.clone())
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
                let _ = self.store.clear_session_agent_session_id(session_id);
                if let Some(resume) = session.agent_session_id.as_deref() {
                    claimed.remove(resume);
                }
                if let Some(row) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                    row.agent_session_id = None;
                }
                changed = true;
                continue;
            }
            let next = if let Some(resume) = session.agent_session_id.as_deref() {
                bound_title.map(|title| (resume.to_string(), title))
            } else {
                candidates.retain(|c| !claimed.contains(&c.agent_session_id));
                candidates
                    .into_iter()
                    .max_by_key(|c| c.score)
                    .map(|c| (c.agent_session_id, c.title))
            };
            let Some((agent_session_id, title)) = next else {
                continue;
            };
            if session.agent_session_id.as_deref() != Some(agent_session_id.as_str()) {
                if self
                    .store
                    .set_session_agent_session_id(session_id, &agent_session_id)
                    .is_ok()
                {
                    claimed.insert(agent_session_id.clone());
                    if let Some(row) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                        row.agent_session_id = Some(agent_session_id);
                    }
                    changed = true;
                }
            } else {
                claimed.insert(agent_session_id);
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

    /// Delete a session: remove the row, then drop every pane terminal bound to
    /// it.
    ///
    /// Returns the neighbor to select next — computed before the row is gone —
    /// so a caller that was looking at this session can land somewhere else.
    /// The row goes first: if the write fails, nothing is torn down and the
    /// session can be retried.
    pub fn delete_session(
        &mut self,
        session_id: i64,
        cx: &mut Context<Self>,
    ) -> Result<Option<i64>, ()> {
        let next = neighbor_session(&self.sessions, session_id);
        if let Err(error) = self.store.delete_session(session_id) {
            Self::failed(cx, "删除会话失败", error);
            return Err(());
        }
        self.drop_session_terminals(session_id);
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
        Ok(next)
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

/// The directory a first-run project opens in.
///
/// `current_dir()` alone is not enough on macOS: a Finder-launched `.app`
/// starts with `/` as its working directory, and opening the first project
/// there would put its shell at the filesystem root. Fall back to the home
/// directory instead.
fn first_run_dir() -> PathBuf {
    first_run_dir_from(
        std::env::current_dir().ok(),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn first_run_dir_from(cwd: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    cwd.filter(|dir| dir.as_path() != Path::new("/"))
        .or(home)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The project name a first-run directory gets: its own folder name.
fn project_name(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "kumu".to_string())
}

/// Insert the first-run project, or return `None` when the row cannot be
/// written.
fn insert_bootstrap_project(store: &Store) -> Option<i64> {
    let dir = first_run_dir();
    let path = dir.to_string_lossy().into_owned();
    store.insert_project(&project_name(&dir), &path).ok()
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
    agent_session_id: Option<String>,
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
        agent_session_id,
    } = query;
    let mut scan = TitleScan {
        session_id,
        binding_alive: None,
        bound_title: None,
        candidates: Vec::new(),
    };
    match agent_session_id.as_deref() {
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
/// The nearest sibling in the same project in sidebar order (the next one, else
/// the previous), else the first session left anywhere, else none — so the
/// right pane never goes blank while sessions remain.
pub(crate) fn neighbor_session(sessions: &[Session], id: i64) -> Option<i64> {
    let project = sessions.iter().find(|s| s.id == id)?.project_id;
    let siblings: Vec<i64> = sessions
        .iter()
        .filter(|s| s.project_id == project)
        .map(|s| s.id)
        .collect();
    let position = siblings.iter().position(|&sid| sid == id);
    position
        .and_then(|i| siblings.get(i + 1).copied())
        .or_else(|| {
            position
                .and_then(|i| i.checked_sub(1))
                .and_then(|i| siblings.get(i).copied())
        })
        .or_else(|| sessions.iter().map(|s| s.id).find(|&sid| sid != id))
}

/// Which parked terminals to stop so one more process fits under `cap`, least
/// recently shown first. `order` holds only parked terminals; `alive` is every
/// running process, including those a pane shows. Returning nothing is correct
/// when no parked terminal is left: the cap is soft rather than letting a
/// visible pane lose its process.
fn terminal_evictions(order: &[u64], alive: usize, cap: usize) -> Vec<u64> {
    let needed = (alive + 1).saturating_sub(cap);
    order.iter().copied().take(needed).collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        ProjectPick, first_run_dir_from, neighbor_session, project_name, resolve_project_pick,
        terminal_evictions,
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
            agent_session_id: None,
            title_manual: false,
        }
    }

    #[test]
    fn notification_navigation_restores_the_exact_terminal_and_acknowledges_only_its_pane() {
        use super::{LiveTerm, Workspace};
        use crate::term::{Term, TermColors};
        use std::collections::HashMap;

        let mut terms = HashMap::new();
        for (id, session, pane) in [(1, 10, Some(1)), (2, 10, Some(2)), (3, 20, None)] {
            let (term, _wake) = Term::launch(
                "/bin/cat".into(),
                Vec::new(),
                std::env::temp_dir(),
                TermColors::default(),
            );
            terms.insert(
                id,
                LiveTerm {
                    term,
                    session,
                    pane,
                    last_notification: None,
                },
            );
        }
        let mut workspace = Workspace {
            store: crate::db::Store::in_memory().expect("test store"),
            persistent: false,
            projects: Vec::new(),
            sessions: Vec::new(),
            terms,
            pane_term: HashMap::from([(1, 1), (2, 2)]),
            term_lru: vec![3, 1, 2],
            next_term: 4,
            startup_error: None,
            saved_workspace: None,
        };
        assert!(!workspace.notification_is_read(1, None), "inactive window");
        assert!(
            !workspace.notification_is_read(2, Some(1)),
            "same session, other pane"
        );
        assert!(workspace.notification_is_read(1, Some(1)));
        assert!(
            !workspace.notification_is_read(3, Some(1)),
            "parked terminal"
        );
        assert_eq!(workspace.reveal_terminal(2, 1), Some((2, 10)));
        assert_eq!(
            workspace.pane_term[&1], 1,
            "an already visible terminal keeps its pane"
        );
        assert_eq!(workspace.reveal_terminal(3, 1), Some((1, 20)));
        assert_eq!(workspace.pane_term[&1], 3);
        assert_eq!(workspace.pane_term[&2], 2);
        assert_eq!(workspace.terms[&3].pane, Some(1));
        assert!(workspace.notification_is_read(3, Some(1)));
        assert!(
            workspace.notification_is_read(1, None),
            "removed terminal clears stale attention"
        );
        assert_eq!(
            workspace.reveal_terminal(1, 1),
            None,
            "stale notification cannot restart a process"
        );
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

    /// The sidebar orders sessions by recency, so the neighbor after a delete
    /// has to follow that order rather than numeric ids.
    #[test]
    fn deletion_follows_sidebar_order_not_id_order() {
        let sessions = vec![session(3, 10), session(1, 10), session(2, 10)];
        assert_eq!(neighbor_session(&sessions, 1), Some(2), "the next row");
        assert_eq!(neighbor_session(&sessions, 2), Some(1), "the previous row");
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
    fn a_fifth_terminal_stops_the_least_recently_shown_parked_one() {
        assert!(terminal_evictions(&[1, 2, 3], 3, 4).is_empty());
        assert_eq!(terminal_evictions(&[1, 2, 3, 4], 4, 4), vec![1]);
        assert_eq!(terminal_evictions(&[1, 2, 3, 4, 5], 5, 4), vec![1, 2]);
    }

    /// With every process on screen there is no parked candidate; the cap has
    /// to give way, because stopping a visible pane's terminal would leave the
    /// pane alive but dead to input.
    #[test]
    fn a_visible_pane_is_never_evicted() {
        assert!(terminal_evictions(&[], 12, 12).is_empty());
        assert!(terminal_evictions(&[], 40, 12).is_empty());
        // One parked terminal is stopped even when many visible ones put the
        // total far over the cap.
        assert_eq!(terminal_evictions(&[7], 40, 12), vec![7]);
    }

    /// A Finder-launched `.app` starts in `/`; the first project must not open
    /// its shell at the filesystem root when a home directory exists.
    #[test]
    fn a_first_run_never_opens_at_the_filesystem_root() {
        let home = PathBuf::from("/Users/kumu");
        assert_eq!(
            first_run_dir_from(Some(PathBuf::from("/")), Some(home.clone())),
            home
        );
        assert_eq!(first_run_dir_from(None, Some(home.clone())), home);
        assert_eq!(
            first_run_dir_from(Some(PathBuf::from("/repo/kumu")), Some(home)),
            PathBuf::from("/repo/kumu"),
            "a real working directory still wins"
        );
        assert_eq!(
            first_run_dir_from(Some(PathBuf::from("/")), None),
            PathBuf::from("/")
        );
    }

    #[test]
    fn a_first_run_project_is_named_after_its_folder() {
        assert_eq!(project_name(Path::new("/repo/kumu")), "kumu");
        assert_eq!(project_name(Path::new("/")), "kumu", "no folder name");
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
