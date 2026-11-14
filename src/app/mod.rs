//! The application layer: projects and live terminals — independent of how
//! it is drawn.
//!
//! The UI hears about changes through [`WorkspaceEvent`] rather than a blanket
//! notify: a terminal frame only concerns that pane.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui_kit::{Context, EventEmitter, Global};

use crate::agent::launch::{AgentCommand, PaneLaunch, SESSION_PREFIX};
use crate::db::{Project, Store, db_path};
use crate::remote::{MAX_GRID, RemoteError};
use crate::term::{KeyInput, MouseInput, MouseMods, Snapshot, Term, TermColors, TermNotification};
use crate::theme;

/// Shared by the home and settings windows; changes apply immediately.
pub struct NotificationSettings {
    pub enabled: bool,
    /// Play the macOS alert sound when an attention notification is raised.
    pub sound: bool,
}

impl Global for NotificationSettings {}

/// A resumed conversation that dies almost immediately usually names a session
/// that no longer exists. One fresh-conversation retry is better than dropping
/// the pane to a shell on the first bad id; a second failure still becomes a
/// shell, so a broken agent cannot loop.
fn resume_failed(launch: &PaneLaunch, started: Instant) -> bool {
    matches!(
        launch,
        PaneLaunch::Agent {
            session: Some(_),
            ..
        }
    ) && started.elapsed() < Duration::from_secs(5)
}

/// How many terminal processes may stay running at once.
///
/// A terminal past the cap is stopped, least recently shown first — but only a
/// parked one: a terminal a pane is showing is never stopped, so the cap is
/// soft when every process is on screen. Showing a stopped pane again starts
/// a new shell.
const MAX_LIVE_TERMINALS: usize = 12;

/// What changed in the [`Workspace`], for the views that draw it.
pub enum WorkspaceEvent {
    /// Something the home view draws: the project list.
    Changed,
    /// A persistence write failed. The home window shows the message until the
    /// user dismisses it, instead of leaving the action looking like a no-op.
    Failed(String),
    /// A pane's terminal grid changed. Only that pane draws it.
    Frame(u64),
    /// An explicit OSC notification from a visible or parked terminal.
    Attention {
        terminal: u64,
        project: i64,
        notification: TermNotification,
    },
    /// A pane's shell exited cleanly. The home window closes that pane. The
    /// project stays; with no pane left, its tree is empty.
    PaneExited { pane: u64, project: i64 },
    /// A pane's agent exited. The pane stays: it restarts the same agent with
    /// a new conversation when `resume_failed`, otherwise it becomes a shell
    /// and the old conversation is not resumed again.
    AgentExited {
        pane: u64,
        project: i64,
        resume_failed: bool,
    },
    /// An agent reported the conversation it is running, for the pane that
    /// owns its terminal. The home window stores it with the pane.
    Session {
        pane: u64,
        agent: String,
        session: String,
    },
}

/// One native TUI process and where it is drawn.
struct LiveTerm {
    term: Term,
    /// Project this process was started in.
    project: i64,
    /// The pane showing it, or `None` while parked: its project was hidden,
    /// and the process keeps running so switching back reattaches the same
    /// shell instead of restarting it.
    pane: Option<u64>,
    /// Pane that owned this process before its project was hidden.
    parked_for: Option<u64>,
    /// What this process was started with. An agent exit turns the pane into
    /// a shell; a shell exit closes the pane.
    launch: PaneLaunch,
    /// When the process started, so an immediate failure can be told from a
    /// conversation the user actually ended.
    started: Instant,
    last_notification: Option<(Instant, TermNotification)>,
}

/// Projects and each live terminal.
///
/// One per app, owned by the home window. Nothing in here renders. A pane runs
/// the user's shell in the project's directory, or an agent TUI when the pane
/// was launched as one.
///
/// A project may occupy several panes; each pane has its own PTY. Switching
/// projects parks those processes rather than killing them, so a hidden shell
/// keeps its state until the cap stops it.
pub struct Workspace {
    store: Store,
    /// Whether `store` is the on-disk database. An in-memory fallback still
    /// gets its first-run project, because nothing else persists.
    persistent: bool,
    projects: Vec<Project>,
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
    /// Grid size the desktop last measured for each pane. A remote lease keeps
    /// this aside and restores it when the phone detaches.
    desktop_sizes: HashMap<u64, PaneSize>,
    /// Panes whose PTY size is currently owned by a phone.
    remote_sizes: HashMap<u64, RemoteLease>,
    /// The next attach attempt's id. Distinguishes a phone that took a pane
    /// over from one whose attach was already superseded.
    next_remote_attach: u64,
}

#[derive(Clone, Copy, Debug)]
struct PaneSize {
    cols: u16,
    rows: u16,
    cell_w: u32,
    cell_h: u32,
}

impl PaneSize {
    fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            cols: snapshot
                .cells
                .first()
                .map(|row| row.len().clamp(1, u16::MAX as usize) as u16)
                .unwrap_or(80),
            rows: snapshot.rows.max(1),
            cell_w: 8,
            cell_h: 16,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RemoteLease {
    /// Identifies this attach attempt. A phone that was superseded while its
    /// attach was still in flight must not take the pane over when it lands.
    attach: u64,
    /// The tap id from the terminal thread; `None` while the phone is still
    /// completing the attach.
    tap: Option<u64>,
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn new(_cx: &mut Context<Self>) -> Self {
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
            terms: HashMap::new(),
            pane_term: HashMap::new(),
            term_lru: Vec::new(),
            next_term: 1,
            startup_error,
            saved_workspace,
            desktop_sizes: HashMap::new(),
            remote_sizes: HashMap::new(),
            next_remote_attach: 1,
        };
        workspace.bootstrap();
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

    /// A first run gets a project for the current directory, so the window
    /// never opens onto nothing. The seeded flag keeps an intentionally
    /// emptied workspace empty on later launches.
    fn bootstrap(&mut self) {
        let seeded = self.persistent && crate::db::get_setting(crate::db::SETTING_SEEDED).is_some();
        if !seeded {
            let seeded_ok = if self.store.projects().is_empty() {
                insert_bootstrap_project(&self.store).is_some()
            } else {
                true
            };
            if seeded_ok && self.persistent {
                let _ = crate::db::set_setting(crate::db::SETTING_SEEDED, "1");
            }
        }
        self.refresh();
    }

    fn refresh(&mut self) {
        self.projects = self.store.projects();
    }

    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub fn project(&self, id: i64) -> Option<&Project> {
        self.projects.iter().find(|project| project.id == id)
    }

    /// The first project, creating the first-run one when the list is empty.
    pub fn ensure_project(&mut self, cx: &mut Context<Self>) -> Option<i64> {
        if let Some(id) = self.projects.first().map(|project| project.id) {
            return Some(id);
        }
        let Some(id) = insert_bootstrap_project(&self.store) else {
            cx.emit(WorkspaceEvent::Failed("无法创建默认项目".to_string()));
            return None;
        };
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
        Some(id)
    }

    /// The pane's screen, after making sure its process is running in
    /// `project_id`.
    ///
    /// `launch` says what to start when this pane has no process yet: a shell,
    /// or an agent TUI (with a conversation to resume or a fresh one). An
    /// exited shell closes the pane and an exited agent becomes a shell (see
    /// [`Self::watch_terminal`]). Hiding the project parks the process and
    /// reattaches it when that project is shown again. Starting a process past
    /// the cap stops the parked one shown least recently; a pane's visible
    /// terminal is never stopped.
    pub fn show_terminal(
        &mut self,
        pane: u64,
        project_id: i64,
        launch: &PaneLaunch,
        cx: &mut Context<Self>,
    ) -> Snapshot {
        // A clean exit is still owned by its pane until the watcher reports
        // it. Switching projects in that gap must not start a replacement.
        if let Some(term_id) = self.pane_term.get(&pane).copied()
            && let Some(live) = self.terms.get(&term_id)
            && live.project == project_id
            && live.term.snapshot().spawn_error.is_none()
        {
            let term = live.term.clone();
            self.touch_terminal(term_id);
            term.wake_handled();
            return term.snapshot();
        }
        self.park_terminal(pane);
        let Some(path) = self.project(project_id).map(|project| project.path.clone()) else {
            return Snapshot::blank("", "");
        };
        // This pane was shown before: reattach its process and screen. Try
        // this before reclaiming, because a reattach starts no new process and
        // must not cost some other terminal its process — least of all its own.
        if let Some(term_id) = self.parked_terminal(project_id, pane) {
            let live = self.terms.get_mut(&term_id).unwrap();
            live.pane = Some(pane);
            live.parked_for = None;
            self.pane_term.insert(pane, term_id);
            self.touch_terminal(term_id);
            let term = self.terms[&term_id].term.clone();
            term.wake_handled();
            return term.snapshot();
        }
        self.reclaim_terminals();
        let (program, args, launch) = self.command(launch);
        let term = self.spawn(pane, project_id, &path, program, args, launch, cx);
        term.snapshot()
    }

    /// Start a pane's process without drawing it, parked until its project is
    /// selected. Used at startup so every project's panes are already warm at
    /// the moment the user looks at them — agent panes first, shells as the
    /// live-terminal cap allows.
    ///
    /// Returns false when the cap is full or there is nothing to start; the
    /// pane then starts lazily on selection, exactly as before.
    pub fn prestart(
        &mut self,
        pane: u64,
        project_id: i64,
        launch: &PaneLaunch,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.pane_term.contains_key(&pane)
            || self
                .terms
                .values()
                .any(|live| live.parked_for == Some(pane) && live.project == project_id)
        {
            return true;
        }
        if self.terms.len() >= MAX_LIVE_TERMINALS {
            return false;
        }
        let Some(path) = self.project(project_id).map(|project| project.path.clone()) else {
            return false;
        };
        let (program, args, launch) = self.command(launch);
        let _ = self.spawn(pane, project_id, &path, program, args, launch, cx);
        let Some(term_id) = self.pane_term.remove(&pane) else {
            return false;
        };
        // `spawn` attached it; keeping it parked is the whole point.
        if let Some(live) = self.terms.get_mut(&term_id) {
            live.pane = None;
            live.parked_for = Some(pane);
        }
        self.touch_terminal(term_id);
        true
    }

    /// Whether the process in `pane` was started as a shell rather than as an
    /// agent. `None` when the pane has no live process to inspect.
    pub fn pane_is_shell(&self, pane: u64) -> Option<bool> {
        self.live_for_pane(pane).map(|live| live.launch.is_shell())
    }

    /// Whether `program` (an executable file name) still runs below the shell
    /// shown in `pane`. `None` when there is nothing to inspect — a pane
    /// started as an agent reports its own exit, and a pane with no process
    /// has no evidence either way.
    pub fn pane_runs_program(&self, pane: u64, program: &str) -> Option<bool> {
        let live = self.live_for_pane(pane)?;
        if !live.launch.is_shell() {
            return None;
        }
        descendant_runs(live.term.child_pid()?, program)
    }

    /// The live terminal a pane shows, or the one parked for it.
    fn live_for_pane(&self, pane: u64) -> Option<&LiveTerm> {
        if let Some(term_id) = self.pane_term.get(&pane) {
            return self.terms.get(term_id);
        }
        self.terms
            .values()
            .find(|live| live.parked_for == Some(pane))
    }

    /// The program and arguments for a pane's launch, falling back to the
    /// user's shell when the requested agent cannot run here.
    fn command(&self, launch: &PaneLaunch) -> (String, Vec<String>, PaneLaunch) {
        match launch.agent_command() {
            Some(AgentCommand { program, args, .. }) => (program, args, launch.clone()),
            None => {
                let (program, args) = crate::term::default_shell();
                (program, args, PaneLaunch::Shell)
            }
        }
    }

    /// Start `program` in `path` and remember it as `pane`'s process.
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &mut self,
        pane: u64,
        project_id: i64,
        path: &str,
        program: String,
        args: Vec<String>,
        launch: PaneLaunch,
        cx: &mut Context<Self>,
    ) -> Term {
        let colors = theme::term_colors(cx);
        let (term, wake) = Term::launch(program, args, PathBuf::from(path), colors);
        let term_id = self.next_term;
        self.next_term += 1;
        self.terms.insert(
            term_id,
            LiveTerm {
                term: term.clone(),
                project: project_id,
                pane: Some(pane),
                parked_for: None,
                launch,
                started: Instant::now(),
                last_notification: None,
            },
        );
        self.pane_term.insert(pane, term_id);
        self.touch_terminal(term_id);
        self.watch_terminal(term_id, project_id, wake, cx);
        term
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
    pub fn reveal_terminal(&mut self, terminal: u64) -> Option<(u64, i64)> {
        let live = self.terms.get(&terminal)?;
        let project = live.project;
        let pane = live.pane.or(live.parked_for)?;
        if live.pane.is_none() {
            self.park_terminal(pane);
            let live = self.terms.get_mut(&terminal)?;
            live.pane = Some(pane);
            live.parked_for = None;
            self.pane_term.insert(pane, terminal);
        }
        self.touch_terminal(terminal);
        Some((pane, project))
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

    fn live_term_for_pane(&self, pane: u64) -> Option<&LiveTerm> {
        self.shown_term(pane).or_else(|| {
            self.terms
                .values()
                .find(|live| live.pane.is_none() && live.parked_for == Some(pane))
        })
    }

    pub fn terminal_snapshot(&self, pane: u64) -> Option<Snapshot> {
        self.shown_term(pane).map(|live| live.term.snapshot())
    }

    /// What the terminal at `terminal` calls itself, for the notification
    /// title. Empty when the program set no title and no command is known.
    pub fn terminal_label(&self, terminal: u64) -> String {
        self.terms
            .get(&terminal)
            .map(|live| live.term.snapshot().label())
            .unwrap_or_default()
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

    /// Raw bytes from the device that owns the pane's remote lease.
    pub fn remote_write(&self, pane: u64, tap: u64, bytes: impl Into<Vec<u8>>) -> bool {
        if !self
            .remote_sizes
            .get(&pane)
            .is_some_and(|lease| lease.tap == Some(tap))
        {
            return false;
        }
        let Some(live) = self.live_term_for_pane(pane) else {
            return false;
        };
        live.term.write(bytes);
        true
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

    pub fn terminal_search_set(&self, pane: u64, needle: &str) {
        if let Some(live) = self.shown_term(pane) {
            live.term.search_set(needle);
        }
    }

    pub fn terminal_search_step(&self, pane: u64, older: bool) {
        if let Some(live) = self.shown_term(pane) {
            live.term.search_step(older);
        }
    }

    pub fn terminal_search_clear(&self, pane: u64) {
        if let Some(live) = self.shown_term(pane) {
            live.term.search_clear();
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

    pub fn terminal_resize(&mut self, pane: u64, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        self.desktop_sizes.insert(
            pane,
            PaneSize {
                cols,
                rows,
                cell_w,
                cell_h,
            },
        );
        // A phone owns the PTY size until it detaches. The desktop keeps its
        // measurement so the pane can snap back without waiting for a layout.
        if self.remote_sizes.contains_key(&pane) {
            return;
        }
        if let Some(live) = self.shown_term(pane) {
            live.term.resize(cols, rows, cell_w, cell_h);
        }
    }

    /// A pane's terminal for remote control, whether the project is showing
    /// or the process is parked behind another project.
    pub fn terminal_for_pane(&self, pane: u64) -> Option<Term> {
        self.live_term_for_pane(pane).map(|live| live.term.clone())
    }

    /// The terminal id a pane shows, for attention lookups.
    pub fn term_id_for_pane(&self, pane: u64) -> Option<u64> {
        self.pane_term.get(&pane).copied().or_else(|| {
            self.terms
                .iter()
                .find(|(_, live)| live.pane.is_none() && live.parked_for == Some(pane))
                .map(|(id, _)| *id)
        })
    }

    /// Reserve `pane` for a phone's control lease. The caller attaches the
    /// terminal on a background thread; `confirm_remote_attach` records the
    /// tap id, and `cancel_remote_attach` undoes the reservation.
    ///
    /// The returned id identifies this attempt: only it may confirm or cancel
    /// the lease, so a phone whose attach was superseded halfway through
    /// cannot take the pane from the phone that replaced it.
    pub fn begin_remote_attach(&mut self, pane: u64) -> Result<(Term, u64), RemoteError> {
        // A reconnect, a second phone, or a stale lease from a connection the
        // Mac has not noticed closing: the newest attach wins. The old tap is
        // released first so the desktop size is restored before the new grid
        // is applied. An attach that has not confirmed yet has no tap to
        // release; its own confirm will find itself superseded.
        if let Some(lease) = self.remote_sizes.get(&pane).copied()
            && let Some(tap) = lease.tap
        {
            self.release_remote_attach(pane, tap);
        }
        let Some(live) = self.live_term_for_pane(pane) else {
            return Err(RemoteError::new(
                "pane_gone",
                "the pane has no running terminal",
            ));
        };
        let term = live.term.clone();
        let snapshot = term.snapshot();
        if snapshot.exited {
            return Err(RemoteError::new("pane_gone", "the terminal has exited"));
        }
        let attach = self.next_remote_attach;
        self.next_remote_attach += 1;
        self.desktop_sizes
            .entry(pane)
            .or_insert_with(|| PaneSize::from_snapshot(&snapshot));
        self.remote_sizes
            .insert(pane, RemoteLease { attach, tap: None });
        Ok((term, attach))
    }

    /// Record the tap of an attach that finished. False when another attach
    /// took the pane over in the meantime.
    pub fn confirm_remote_attach(&mut self, pane: u64, attach: u64, tap: u64) -> bool {
        let Some(lease) = self.remote_sizes.get_mut(&pane) else {
            return false;
        };
        if lease.attach != attach {
            return false;
        }
        lease.tap = Some(tap);
        true
    }

    /// Drop a reservation whose attach failed or was superseded. True when
    /// this attempt still owned the pane; false when a newer attach replaced it
    /// while the terminal was being prepared.
    pub fn cancel_remote_attach(&mut self, pane: u64, attach: u64) -> bool {
        let Some(lease) = self.remote_sizes.get(&pane).copied() else {
            return false;
        };
        if lease.attach != attach {
            return false;
        }
        self.remote_sizes.remove(&pane);
        let Some(term) = self.terminal_for_pane(pane) else {
            return true;
        };
        if let Some(size) = self.desktop_sizes.get(&pane).copied() {
            term.resize(size.cols, size.rows, size.cell_w, size.cell_h);
        }
        true
    }

    /// Release a phone's size ownership. A stale tap from a previous
    /// connection must not release a newer lease.
    pub fn release_remote_attach(&mut self, pane: u64, tap: u64) {
        let Some(lease) = self.remote_sizes.get(&pane).copied() else {
            return;
        };
        if lease.tap != Some(tap) {
            return;
        }
        self.remote_sizes.remove(&pane);
        let Some(term) = self.terminal_for_pane(pane) else {
            return;
        };
        term.detach_remote(tap);
        if let Some(size) = self.desktop_sizes.get(&pane).copied() {
            term.resize(size.cols, size.rows, size.cell_w, size.cell_h);
        }
    }

    pub fn resize_remote(&mut self, pane: u64, tap: u64, cols: u16, rows: u16) {
        let Some(lease) = self.remote_sizes.get(&pane) else {
            return;
        };
        if lease.tap != Some(tap) {
            return;
        }
        // A phone's grid has the same bounds as an attach: an unclamped size
        // reaches libghostty as rows × cols cells.
        let cols = cols.clamp(1, MAX_GRID);
        let rows = rows.clamp(1, MAX_GRID);
        if let Some(term) = self.terminal_for_pane(pane) {
            term.resize(cols, rows, 8, 16);
        }
    }

    /// Stop the process shown in `pane`, and any process parked for that pane.
    pub fn drop_terminal(&mut self, pane: u64) {
        self.desktop_sizes.remove(&pane);
        self.remote_sizes.remove(&pane);
        if let Some(term_id) = self.pane_term.remove(&pane) {
            self.drop_term(term_id);
        }
        let parked: Vec<u64> = self
            .terms
            .iter()
            .filter(|(_, live)| live.pane.is_none() && live.parked_for == Some(pane))
            .map(|(id, _)| *id)
            .collect();
        for id in parked {
            self.drop_term(id);
        }
    }

    /// Park the process `pane` shows so switching back reattaches it. Clean
    /// exits keep their owner until the watcher closes the pane.
    fn park_terminal(&mut self, pane: u64) {
        let Some(term_id) = self.pane_term.remove(&pane) else {
            return;
        };
        let Some(live) = self.terms.get_mut(&term_id) else {
            return;
        };
        live.pane = None;
        live.parked_for = Some(pane);
        if live.term.snapshot().spawn_error.is_some() {
            self.drop_term(term_id);
        }
    }

    /// Park a pane while its project is hidden from the window.
    pub fn hide_terminal(&mut self, pane: u64) {
        self.park_terminal(pane);
    }

    /// The hidden terminal owned by `pane`.
    fn parked_terminal(&self, project_id: i64, pane: u64) -> Option<u64> {
        self.term_lru.iter().rev().copied().find(|id| {
            self.terms.get(id).is_some_and(|live| {
                live.pane.is_none()
                    && live.project == project_id
                    && live.parked_for == Some(pane)
                    && live.term.snapshot().spawn_error.is_none()
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

    fn drop_project_terminals(&mut self, project_id: i64) {
        let terms: Vec<u64> = self
            .terms
            .iter()
            .filter(|(_, live)| live.project == project_id)
            .map(|(id, _)| *id)
            .collect();
        for id in terms {
            self.drop_term(id);
        }
    }

    /// Stop parked processes until one more running process fits under the cap.
    ///
    /// Only parked terminals are candidates. Dropping a visible one would
    /// remove its pane's mapping and leave a pane that still believes it has a
    /// shell while nothing answers its input; the cap is soft when every
    /// running process is on screen. A terminal a phone is controlling is kept
    /// for the same reason, even while its project is hidden.
    fn reclaim_terminals(&mut self) {
        // The watcher owns exit cleanup and must still report the pane to the
        // UI. Reclaiming it here would consume that exit before it is handled.
        let leased: Vec<u64> = self
            .remote_sizes
            .keys()
            .filter_map(|pane| self.term_id_for_pane(*pane))
            .collect();
        let alive: Vec<u64> = self
            .term_lru
            .iter()
            .copied()
            .filter(|id| {
                self.terms
                    .get(id)
                    .is_some_and(|live| !live.term.snapshot().exited)
            })
            .collect();
        let parked: Vec<u64> = alive
            .iter()
            .copied()
            .filter(|id| {
                !leased.contains(id) && self.terms.get(id).is_some_and(|live| live.pane.is_none())
            })
            .collect();
        for id in terminal_evictions(&parked, alive.len(), MAX_LIVE_TERMINALS) {
            self.drop_term(id);
        }
    }

    fn watch_terminal(
        &self,
        term_id: u64,
        project_id: i64,
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
                        let owning_pane = pane.or(live.parked_for);
                        let agent_pane = !live.launch.is_shell();
                        let resume_failed = resume_failed(&live.launch, live.started);
                        let term = live.term.clone();
                        while let Some(notification) = term.take_notification() {
                            // A session update travels as a notification but
                            // is not one: record it with the pane and stay
                            // silent.
                            if let Some(session) = notification.body.strip_prefix(SESSION_PREFIX)
                                && let Some(pane) = owning_pane
                            {
                                cx.emit(WorkspaceEvent::Session {
                                    pane,
                                    agent: notification.title.clone(),
                                    session: session.to_string(),
                                });
                                continue;
                            }
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
                                    project: project_id,
                                    notification,
                                });
                            }
                        }
                        // A clean shell death closes this pane; an agent death
                        // turns it back into a shell. The project stays.
                        // Spawn failures keep the pane so the header can show
                        // why.
                        let shell_done = snapshot.exited && snapshot.spawn_error.is_none();
                        if shell_done {
                            workspace.drop_term(term_id);
                            if let Some(pane) = owning_pane {
                                if agent_pane {
                                    cx.emit(WorkspaceEvent::AgentExited {
                                        pane,
                                        project: project_id,
                                        resume_failed,
                                    });
                                } else {
                                    cx.emit(WorkspaceEvent::PaneExited {
                                        pane,
                                        project: project_id,
                                    });
                                }
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

    /// Add a worktree row below the project it was created from. The
    /// directory already exists; this only makes it visible as a project of
    /// its own.
    pub fn add_worktree(
        &mut self,
        parent_id: i64,
        name: &str,
        path: &str,
        cx: &mut Context<Self>,
    ) -> Option<i64> {
        if !self.projects.iter().any(|project| project.id == parent_id) {
            return None;
        }
        let id = match self.store.insert_worktree(parent_id, name, path) {
            Ok(id) => id,
            Err(error) => {
                Self::failed(cx, "添加 worktree 失败", error);
                return None;
            }
        };
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
        Some(id)
    }

    /// The project that already points at `path`, if any.
    pub fn project_by_path(&self, path: &Path) -> Option<i64> {
        project_for_path(&self.projects, path)
    }

    /// Attach a worktree that already exists on disk. Returns the project
    /// that owns the path when it was opened before, so picking it again
    /// selects the existing row instead of inserting a duplicate.
    pub fn open_worktree(
        &mut self,
        parent_id: i64,
        name: &str,
        path: &str,
        cx: &mut Context<Self>,
    ) -> Option<i64> {
        if let Some(existing) = project_for_path(&self.projects, Path::new(path)) {
            return Some(existing);
        }
        self.add_worktree(parent_id, name, path, cx)
    }

    /// A project and every worktree below it. Deleting the project removes
    /// all of these rows, so the caller must clean up each one.
    pub fn project_tree_ids(&self, project_id: i64) -> Vec<i64> {
        let mut ids = vec![project_id];
        let mut index = 0;
        while index < ids.len() {
            let parent = ids[index];
            for project in &self.projects {
                if project.parent_id == Some(parent) && !ids.contains(&project.id) {
                    ids.push(project.id);
                }
            }
            index += 1;
        }
        ids
    }

    /// Rename a project without moving it.
    pub fn rename_project(&mut self, project_id: i64, name: String, cx: &mut Context<Self>) {
        if let Err(error) = self.store.rename_project(project_id, &name) {
            Self::failed(cx, "重命名项目失败", error);
            return;
        }
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
    }

    /// Delete a project and its worktrees. Terminals are stopped only after
    /// the rows are gone, so a failed write leaves the workspace untouched.
    pub fn delete_project(&mut self, project_id: i64, cx: &mut Context<Self>) -> Result<(), ()> {
        let removed = self.project_tree_ids(project_id);
        if let Err(error) = self.store.delete_project(project_id) {
            Self::failed(cx, "删除项目失败", error);
            return Err(());
        }
        for id in removed {
            self.drop_project_terminals(id);
        }
        self.refresh();
        cx.emit(WorkspaceEvent::Changed);
        Ok(())
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

/// The project that already points at `path`, comparing canonical paths so a
/// worktree opened through a symlink or a relative path does not become a
/// duplicate row.
fn project_for_path(projects: &[Project], path: &Path) -> Option<i64> {
    let normalize = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let wanted = normalize(path);
    projects
        .iter()
        .find(|project| normalize(Path::new(&project.path)) == wanted)
        .map(|project| project.id)
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

/// Which parked terminals to stop so one more process fits under `cap`, least
/// recently shown first. `order` holds only parked terminals; `alive` is every
/// running process, including those a pane shows. Returning nothing is correct
/// when no parked terminal is left: the cap is soft rather than letting a
/// visible pane lose its process.
fn terminal_evictions(order: &[u64], alive: usize, cap: usize) -> Vec<u64> {
    let needed = (alive + 1).saturating_sub(cap);
    order.iter().copied().take(needed).collect()
}

/// Whether `program` (a file name) runs anywhere below `root` in `ps -axo
/// pid=,ppid=,command=` output.
///
/// A shell pane's launch is borrowed from the agent the user started inside it.
/// Nothing reports that agent leaving, so at quit this is the evidence that
/// the conversation is still there to resume. Matching is by executable name
/// because agents are launched by path, alias, or through a runtime.
fn tree_runs(ps: &str, root: u32, program: &str) -> bool {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut commands: HashMap<u32, &str> = HashMap::new();
    for line in ps.lines() {
        let Some((pid, rest)) = line.trim().split_once(' ') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        let Some((ppid, command)) = rest.trim_start().split_once(' ') else {
            continue;
        };
        let Ok(ppid) = ppid.parse::<u32>() else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
        commands.insert(pid, command.trim());
    }
    // Start below the root: the shell itself is never the agent, even if the
    // program name happens to match its own.
    let mut stack: Vec<u32> = children.get(&root).cloned().unwrap_or_default();
    let mut seen = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if commands.get(&pid).is_some_and(|command| {
            command.split_whitespace().any(|token| {
                Path::new(token)
                    .file_name()
                    .is_some_and(|name| name.to_str() == Some(program))
            })
        }) {
            return true;
        }
        stack.extend(children.get(&pid).into_iter().flatten().copied());
    }
    false
}

/// Ask `ps` whether `program` is still a descendant of `root`. `None` when the
/// question cannot be answered; callers keep the saved choice in that case.
fn descendant_runs(root: u32, program: &str) -> Option<bool> {
    // Absolute path: a Finder-launched app has a minimal PATH.
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| tree_runs(&String::from_utf8_lossy(&output.stdout), root, program))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::{
        ProjectPick, first_run_dir_from, project_name, resolve_project_pick, terminal_evictions,
    };
    use crate::agent::launch::PaneLaunch;
    use crate::db::Project;

    #[test]
    fn only_an_immediately_failed_resume_gets_a_fresh_retry() {
        let now = Instant::now();
        let resumed = PaneLaunch::Agent {
            agent: "codex".into(),
            session: Some("abc".into()),
        };
        assert!(super::resume_failed(&resumed, now));
        assert!(!super::resume_failed(
            &resumed,
            now - Duration::from_secs(10)
        ));
        let fresh = PaneLaunch::Agent {
            agent: "codex".into(),
            session: None,
        };
        assert!(!super::resume_failed(&fresh, now));
        assert!(!super::resume_failed(&PaneLaunch::Shell, now));
    }

    fn project(id: i64, path: &str) -> Project {
        Project {
            id,
            name: format!("project-{id}"),
            path: path.into(),
            parent_id: None,
        }
    }

    #[test]
    fn notification_navigation_restores_the_exact_terminal_and_acknowledges_only_its_pane() {
        use super::{LiveTerm, Workspace};
        use crate::term::{Term, TermColors};
        use std::collections::HashMap;

        let mut terms = HashMap::new();
        for (id, project, pane, parked_for) in [
            (1, 10, Some(1), None),
            (2, 10, Some(2), None),
            (3, 20, None, Some(3)),
        ] {
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
                    project,
                    pane,
                    parked_for,
                    launch: PaneLaunch::Shell,
                    started: Instant::now(),
                    last_notification: None,
                },
            );
        }
        let mut workspace = Workspace {
            store: crate::db::Store::in_memory().expect("test store"),
            persistent: false,
            projects: Vec::new(),
            terms,
            pane_term: HashMap::from([(1, 1), (2, 2)]),
            term_lru: vec![3, 1, 2],
            next_term: 4,
            startup_error: None,
            saved_workspace: None,
            desktop_sizes: HashMap::new(),
            remote_sizes: HashMap::new(),
            next_remote_attach: 1,
        };
        assert!(!workspace.notification_is_read(1, None), "inactive window");
        assert!(
            !workspace.notification_is_read(2, Some(1)),
            "same project, other pane"
        );
        assert!(workspace.notification_is_read(1, Some(1)));
        assert!(
            !workspace.notification_is_read(3, Some(1)),
            "parked terminal"
        );
        assert_eq!(workspace.reveal_terminal(2), Some((2, 10)));
        assert_eq!(
            workspace.pane_term[&1], 1,
            "an already visible terminal keeps its pane"
        );
        assert_eq!(workspace.reveal_terminal(3), Some((3, 20)));
        assert_eq!(workspace.pane_term[&1], 1);
        assert_eq!(workspace.pane_term[&2], 2);
        assert_eq!(workspace.pane_term[&3], 3);
        assert_eq!(workspace.terms[&3].pane, Some(3));
        assert_eq!(workspace.terms[&3].parked_for, None);
        assert!(workspace.notification_is_read(3, Some(3)));
        workspace.hide_terminal(3);
        assert_eq!(workspace.parked_terminal(20, 3), Some(3));
        workspace.drop_terminal(1);
        assert!(
            workspace.notification_is_read(1, None),
            "removed terminal clears stale attention"
        );
        assert_eq!(
            workspace.reveal_terminal(1),
            None,
            "stale notification cannot restart a process"
        );
    }

    #[gpui::test]
    fn shell_exit_reports_both_visible_and_parked_panes(cx: &mut gpui_kit::TestAppContext) {
        use super::{LiveTerm, Workspace, WorkspaceEvent};
        use crate::term::{Term, TermColors};
        use gpui_kit::{AppContext, Entity};
        use std::collections::HashMap;
        use std::time::{Duration, Instant};

        let workspace = cx.new(|cx| {
            let mut workspace = Workspace {
                store: crate::db::Store::in_memory().expect("test store"),
                persistent: false,
                projects: Vec::new(),
                terms: HashMap::new(),
                pane_term: HashMap::from([(1, 1), (2, 2), (3, 3)]),
                term_lru: vec![1, 2, 3],
                next_term: 4,
                startup_error: None,
                saved_workspace: None,
                desktop_sizes: HashMap::new(),
                remote_sizes: HashMap::new(),
                next_remote_attach: 1,
            };
            for id in 1..=3 {
                let (program, args) = if id == 3 {
                    ("/bin/cat", Vec::new())
                } else {
                    ("/bin/sh", vec!["-c".into(), "exit 0".into()])
                };
                let (term, _wake) = Term::launch(
                    program.into(),
                    args,
                    std::env::temp_dir(),
                    TermColors::default(),
                );
                if id != 3 {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while !term.snapshot().exited {
                        assert!(Instant::now() < deadline, "test shell must exit");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    assert!(term.snapshot().spawn_error.is_none());
                }
                workspace.terms.insert(
                    id,
                    LiveTerm {
                        term,
                        project: 10,
                        pane: Some(id),
                        parked_for: None,
                        launch: PaneLaunch::Shell,
                        started: Instant::now(),
                        last_notification: None,
                    },
                );
                if id != 3 {
                    // Deliver the final wake on this thread: GPUI's test
                    // scheduler rejects wakes from native PTY threads.
                    let (tx, wake) = futures::channel::mpsc::unbounded();
                    tx.unbounded_send(()).unwrap();
                    drop(tx);
                    workspace.watch_terminal(id, 10, wake, cx);
                }
            }
            workspace.hide_terminal(2);
            workspace.reclaim_terminals();
            assert_eq!(workspace.parked_terminal(10, 2), Some(2));
            assert!(
                workspace
                    .show_terminal(1, 10, &PaneLaunch::Shell, cx)
                    .exited
            );
            workspace
        });
        let events: Entity<Vec<(u64, i64)>> = cx.new(|cx| {
            cx.subscribe(&workspace, |events: &mut Vec<(u64, i64)>, _, event, _| {
                if let WorkspaceEvent::PaneExited { pane, project } = event {
                    events.push((*pane, *project));
                }
            })
            .detach();
            Vec::new()
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let mut exited = events.read(cx).clone();
            exited.sort_unstable();
            assert_eq!(exited, vec![(1, 10), (2, 10)]);
            let workspace = workspace.read(cx);
            assert_eq!(workspace.terms.len(), 1);
            assert_eq!(workspace.pane_term, HashMap::from([(3, 3)]));
            assert_eq!(workspace.term_lru, vec![3]);
        });
    }

    /// A phone that loses the pane to a newer attach while its own attach is
    /// still landing must not take the pane over, and a detach carrying the
    /// superseded tap must not release the lease that replaced it.
    #[gpui::test]
    fn a_superseded_remote_attach_cannot_claim_the_pane(cx: &mut gpui_kit::TestAppContext) {
        use super::{LiveTerm, Workspace};
        use crate::agent::launch::PaneLaunch;
        use crate::term::{Term, TermColors};
        use gpui_kit::AppContext;
        use std::collections::HashMap;
        use std::time::{Duration, Instant};

        fn wait_for_cols(term: &Term, cols: usize, what: &str) {
            let deadline = Instant::now() + Duration::from_secs(3);
            while term.snapshot().cells.first().map_or(0, |row| row.len()) != cols {
                assert!(
                    Instant::now() < deadline,
                    "{what}: the terminal never reached {cols} columns"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        let workspace = cx.new(|_| {
            let (term, _wake) = Term::launch(
                "/bin/cat".into(),
                Vec::new(),
                std::env::temp_dir(),
                TermColors::default(),
            );
            Workspace {
                store: crate::db::Store::in_memory().expect("test store"),
                persistent: false,
                projects: Vec::new(),
                terms: HashMap::from([(
                    1,
                    LiveTerm {
                        term,
                        project: 10,
                        pane: Some(1),
                        parked_for: None,
                        launch: PaneLaunch::Shell,
                        started: Instant::now(),
                        last_notification: None,
                    },
                )]),
                pane_term: HashMap::from([(1, 1)]),
                term_lru: vec![1],
                next_term: 2,
                startup_error: None,
                saved_workspace: None,
                desktop_sizes: HashMap::new(),
                remote_sizes: HashMap::new(),
                next_remote_attach: 1,
            }
        });
        let term = workspace
            .update(cx, |workspace, _| workspace.terminal_for_pane(1))
            .expect("pane 1's terminal");

        cx.update(|cx| {
            workspace.update(cx, |workspace, _| {
                workspace.terminal_resize(1, 100, 30, 8, 16)
            });
            wait_for_cols(&term, 100, "the desktop size");

            let (first_term, first) = workspace
                .update(cx, |workspace, _| workspace.begin_remote_attach(1))
                .expect("the first attach");
            let first_tap = first_term
                .attach_remote(first, 40, 20)
                .expect("the first tap")
                .tap;
            wait_for_cols(&term, 40, "the first phone's grid");

            let (second_term, second) = workspace
                .update(cx, |workspace, _| workspace.begin_remote_attach(1))
                .expect("the second attach");
            let second_tap = second_term
                .attach_remote(second, 50, 25)
                .expect("the second tap")
                .tap;
            wait_for_cols(&term, 50, "the second phone's grid");
            assert!(
                first_term.attach_remote(first, 40, 20).is_none(),
                "an attach older than the one already applied is refused"
            );
            wait_for_cols(&term, 50, "a stale attach must not resize the shell");

            assert!(
                !workspace.update(cx, |workspace, _| {
                    workspace.confirm_remote_attach(1, first, first_tap)
                }),
                "a superseded attach cannot confirm the lease"
            );
            assert!(workspace.update(cx, |workspace, _| {
                workspace.confirm_remote_attach(1, second, second_tap)
            }));
            workspace.update(cx, |workspace, _| {
                workspace.release_remote_attach(1, first_tap)
            });
            assert!(
                workspace.update(cx, |workspace, _| {
                    workspace.remote_write(1, second_tap, b"x".to_vec())
                }),
                "a stale tap must not release the current lease"
            );

            workspace.update(cx, |workspace, _| {
                workspace.release_remote_attach(1, second_tap)
            });
            assert!(
                !workspace.update(cx, |workspace, _| {
                    workspace.remote_write(1, second_tap, b"x".to_vec())
                }),
                "the released tap no longer owns the pane"
            );
            wait_for_cols(&term, 100, "the desktop size restored");
        });
    }

    /// A shell pane's adopted launch survives only while the agent is still a
    /// child of that shell.
    #[test]
    fn the_process_tree_finds_an_agent_below_its_shell() {
        let ps = "\
    1     0 /sbin/launchd
  100     1 /bin/zsh -l
  101   100 /Users/me/.opencode/bin/opencode --session abc
  102   101 /usr/bin/git status
  200     1 /Applications/Other.app/Contents/MacOS/opencode-helper
";
        assert!(super::tree_runs(ps, 100, "opencode"));
        assert!(super::tree_runs(ps, 1, "opencode"));
        assert!(!super::tree_runs(ps, 100, "claude"));
        assert!(
            !super::tree_runs(ps, 100, "zsh"),
            "the shell itself is not a descendant"
        );
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

    /// A worktree opened through a path that differs only in its written form
    /// still resolves to the row that owns it.
    #[test]
    fn a_worktree_path_matches_the_project_that_owns_it() {
        let dir = std::env::temp_dir().join(format!(
            "kumu-open-worktree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let worktree = dir.join("repo-feat");
        std::fs::create_dir_all(&worktree).unwrap();
        let projects = vec![project(1, "/repo"), project(2, &worktree.to_string_lossy())];
        assert_eq!(
            super::project_for_path(&projects, &worktree.join(".")),
            Some(2),
            "a path written differently still names the same directory"
        );
        assert_eq!(super::project_for_path(&projects, &dir.join("other")), None);

        std::fs::remove_dir_all(&dir).unwrap();
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
}
