//! Framework-free application core: projects, TODO boards, and live/parked
//! terminal sessions — completely independent of GPUI or NSH.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;

use crate::agent::launch::{AgentCommand, PaneLaunch, SESSION_PREFIX, authenticated_session};
use crate::agent::status::{
    STATUS_PREFIX, StatusSignal, TITLE_PREFIX, TITLE_PROMPT_PREFIX, clean_reported_title,
};
use crate::db::{Project, Store, TodoItem, TodoPatch, TodoStatus, db_path};
use crate::term::{
    KeyInput, MouseInput, MouseMods, Snapshot, Term, TermColors, TermNotification,
    TermProgramStatus,
};

/// Shared by the home and settings windows; changes apply immediately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotificationSettings {
    pub enabled: bool,
    /// Play the macOS alert sound when an attention notification is raised.
    pub sound: bool,
}

/// A resumed conversation that dies almost immediately usually names a session
/// that no longer exists. One fresh-conversation retry is better than dropping
/// the pane to a shell on the first bad id; a second failure still becomes a
/// shell, so a broken agent cannot loop.
pub(crate) fn resume_failed(launch: &PaneLaunch, started: Instant) -> bool {
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
pub const MAX_LIVE_TERMINALS: usize = 12;

/// What changed in the workspace, for any UI layer (GPUI or NSH) that draws it.
#[derive(Clone, Debug)]
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
    /// An agent reported a status transition on its silent OSC channel.
    AgentStatus {
        pane: u64,
        project: i64,
        agent: String,
        signal: StatusSignal,
    },
    /// A terminal reported an aggregated OSC 7501 program status update.
    ProgramStatus {
        terminal: u64,
        pane: u64,
        project: i64,
        status: TermProgramStatus,
    },
    /// An agent reported a session or first-prompt title on its silent OSC channel.
    AgentTitle {
        pane: u64,
        project: i64,
        agent: String,
        title: String,
        is_prompt: bool,
    },
    /// A terminal's OSC 0/2 window title changed (visible or parked).
    PaneTitle {
        pane: u64,
        project: i64,
        title: String,
    },
    /// A terminal requested writing text to the system clipboard (OSC 52 / 1337 / 5522).
    ClipboardWrite(String),
    /// A repository's TODO board changed. Payload is the root board project id.
    TodosChanged(i64),
}

/// Handle returned when [`WorkspaceCore`] spawns a new terminal process so the
/// host runtime (GPUI task or NSH/background thread) can watch its wake stream
/// and drive [`WorkspaceCore::drain_terminal`].
pub struct SpawnedTerminal {
    pub term_id: u64,
    pub wake: UnboundedReceiver<()>,
}

/// One native TUI process and where it is drawn.
pub(crate) struct LiveTerm {
    pub(crate) term: Term,
    /// Project this process was started in.
    pub(crate) project: i64,
    /// The pane showing it, or `None` while parked: its project was hidden,
    /// and the process keeps running so switching back reattaches the same
    /// shell instead of restarting it.
    pub(crate) pane: Option<u64>,
    /// Pane that owned this process before its project was hidden.
    pub(crate) parked_for: Option<u64>,
    /// What this process was started with. An agent exit turns the pane into
    /// a shell; a shell exit closes the pane.
    pub(crate) launch: PaneLaunch,
    /// When the process started, so an immediate failure can be told from a
    /// conversation the user actually ended.
    pub(crate) started: Instant,
    pub(crate) last_notification: Option<(Instant, TermNotification)>,
    /// Last observed OSC 0/2 title, so `PaneTitle` only fires on change.
    pub(crate) last_title: String,
}

/// Pure-Rust application state: projects, TODOs, and live/parked terminals.
///
/// Holds no UI framework types; UI layers (GPUI today, NSH next) interact with
/// it via methods and consume emitted [`WorkspaceEvent`]s.
pub struct WorkspaceCore {
    pub(crate) store: Store,
    /// Whether `store` is the on-disk database. An in-memory fallback still
    /// gets its first-run project, because nothing else persists.
    pub(crate) persistent: bool,
    pub(crate) projects: Vec<Project>,
    /// Native TUI processes keyed by a stable terminal id.
    pub(crate) terms: HashMap<u64, LiveTerm>,
    /// Which terminal each pane shows right now.
    pub(crate) pane_term: HashMap<u64, u64>,
    /// Least recently shown first. Only terminals whose process is still running.
    pub(crate) term_lru: Vec<u64>,
    pub(crate) next_term: u64,
    /// A database failure from before any view could subscribe. The home window
    /// reads it once at construction and shows it like a later [`WorkspaceEvent::Failed`].
    pub(crate) startup_error: Option<String>,
    /// The home window's last layout JSON, handed to the view once at startup.
    pub(crate) saved_workspace: Option<String>,
}

impl WorkspaceCore {
    pub fn open_default() -> Self {
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
        let mut core = Self {
            store,
            persistent,
            projects: Vec::new(),
            terms: HashMap::new(),
            pane_term: HashMap::new(),
            term_lru: Vec::new(),
            next_term: 1,
            startup_error,
            saved_workspace,
        };
        core.bootstrap();
        core
    }

    #[cfg(test)]
    pub(crate) fn in_memory() -> Self {
        Self {
            store: Store::in_memory().expect("test store"),
            persistent: false,
            projects: Vec::new(),
            terms: HashMap::new(),
            pane_term: HashMap::new(),
            term_lru: Vec::new(),
            next_term: 1,
            startup_error: None,
            saved_workspace: None,
        }
    }

    fn emit(&mut self, events: &mut Vec<WorkspaceEvent>, event: WorkspaceEvent) {
        events.push(event);
    }

    fn emit_failed(
        &mut self,
        events: &mut Vec<WorkspaceEvent>,
        action: &str,
        error: impl std::fmt::Display,
    ) {
        self.emit(events, WorkspaceEvent::Failed(format!("{action}：{error}")));
    }

    /// Take the failure recorded before any view subscribed.
    pub fn take_startup_error(&mut self) -> Option<String> {
        self.startup_error.take()
    }

    /// Take the layout JSON loaded before the home view existed.
    pub fn take_saved_workspace(&mut self) -> Option<String> {
        self.saved_workspace.take()
    }

    /// Replace the saved layout, reporting a write failure via `events`.
    pub fn save_workspace_state(&mut self, json: &str, events: &mut Vec<WorkspaceEvent>) -> bool {
        match self.store.save_workspace_state(json) {
            Ok(()) => true,
            Err(error) => {
                self.emit_failed(events, "保存工作区布局失败", error);
                false
            }
        }
    }

    /// A first run gets a project for the current directory, so the window
    /// never opens onto nothing. The seeded flag keeps an intentionally
    /// emptied workspace empty on later launches.
    fn bootstrap(&mut self) {
        let seeded = self.persistent
            && match self.store.setting(crate::db::SETTING_SEEDED) {
                Ok(Some(_)) => true,
                Ok(None) => false,
                // Do not re-seed on a transient DB error.
                Err(_) => true,
            };
        if !seeded {
            let seeded_ok = match self.store.projects() {
                Ok(projects) if projects.is_empty() => {
                    insert_bootstrap_project(&self.store).is_some()
                }
                Ok(_) => true,
                Err(_) => false,
            };
            if seeded_ok && self.persistent {
                let _ = self.store.set_setting(crate::db::SETTING_SEEDED, "1");
            }
        }
        self.refresh();
    }

    fn refresh(&mut self) {
        match self.store.projects() {
            Ok(projects) => self.projects = projects,
            Err(error) => eprintln!("[app] 读取项目列表失败：{error}"),
        }
    }

    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub fn project(&self, id: i64) -> Option<&Project> {
        self.projects.iter().find(|project| project.id == id)
    }

    /// Root repository id that owns the TODO board for `project_id`.
    /// Worktrees share their parent repository's board.
    pub fn board_project_id(&self, project_id: i64) -> i64 {
        board_root_id(&self.projects, project_id)
    }

    /// All project ids (the root repository plus its worktrees) that share
    /// `board_id`'s TODO board.
    pub fn board_member_project_ids(&self, board_id: i64) -> Vec<i64> {
        self.projects
            .iter()
            .filter(|project| board_root_id(&self.projects, project.id) == board_id)
            .map(|project| project.id)
            .collect()
    }

    /// All TODO items for the repository owning `project_id`.
    pub fn todos_for_project(&self, project_id: i64) -> Vec<TodoItem> {
        let board_id = self.board_project_id(project_id);
        match self.store.todos(board_id) {
            Ok(items) => items,
            Err(error) => {
                eprintln!("[app] 读取 TODO 列表失败：{error}");
                Vec::new()
            }
        }
    }

    /// Look up one TODO item by id.
    pub fn todo(&self, id: i64) -> Option<TodoItem> {
        match self.store.todo(id) {
            Ok(item) => item,
            Err(error) => {
                eprintln!("[app] 读取 TODO #{id} 失败：{error}");
                None
            }
        }
    }

    /// Create a TODO item on the board owning `project_id`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_todo(
        &mut self,
        project_id: i64,
        title: &str,
        description: &str,
        status: TodoStatus,
        pane_id: Option<u64>,
        agent_id: Option<&str>,
        agent_session_id: Option<&str>,
        events: &mut Vec<WorkspaceEvent>,
    ) -> Option<TodoItem> {
        let board_id = self.board_project_id(project_id);
        match self.store.insert_todo(
            board_id,
            title,
            description,
            status,
            pane_id,
            agent_id,
            agent_session_id,
        ) {
            Ok(item) => {
                self.emit(events, WorkspaceEvent::TodosChanged(board_id));
                Some(item)
            }
            Err(error) => {
                self.emit_failed(events, "创建 TODO 失败", error);
                None
            }
        }
    }

    /// Update one TODO item and notify subscribers.
    pub fn update_todo(
        &mut self,
        id: i64,
        patch: &TodoPatch,
        events: &mut Vec<WorkspaceEvent>,
    ) -> Option<TodoItem> {
        match self.store.update_todo(id, patch) {
            Ok(Some(item)) => {
                self.emit(events, WorkspaceEvent::TodosChanged(item.project_id));
                Some(item)
            }
            Ok(None) => None,
            Err(error) => {
                self.emit_failed(events, "更新 TODO 失败", error);
                None
            }
        }
    }

    /// Delete one TODO item and notify subscribers.
    pub fn delete_todo(&mut self, id: i64, events: &mut Vec<WorkspaceEvent>) -> bool {
        let board_id = self.todo(id).map(|item| item.project_id);
        match self.store.delete_todo(id) {
            Ok(true) => {
                if let Some(board_id) = board_id {
                    self.emit(events, WorkspaceEvent::TodosChanged(board_id));
                }
                true
            }
            Ok(false) => false,
            Err(error) => {
                self.emit_failed(events, "删除 TODO 失败", error);
                false
            }
        }
    }

    /// Clear all completed TODO items on the board owning `project_id`.
    pub fn clear_done_todos(&mut self, project_id: i64, events: &mut Vec<WorkspaceEvent>) -> usize {
        let board_id = self.board_project_id(project_id);
        match self.store.clear_done_todos(board_id) {
            Ok(removed) => {
                if removed > 0 {
                    self.emit(events, WorkspaceEvent::TodosChanged(board_id));
                }
                removed
            }
            Err(error) => {
                self.emit_failed(events, "清理已完成 TODO 失败", error);
                0
            }
        }
    }

    /// Mark in-progress TODOs bound to `pane_id` as awaiting human confirmation.
    pub fn mark_pane_todos_review(&mut self, pane_id: u64, events: &mut Vec<WorkspaceEvent>) {
        match self.store.mark_pane_todos_review(pane_id) {
            Ok(boards) => {
                for board_id in boards {
                    self.emit(events, WorkspaceEvent::TodosChanged(board_id));
                }
            }
            Err(error) => eprintln!("[app] 更新 TODO 待确认状态失败：{error}"),
        }
    }

    /// Clear `awaiting_review` on TODOs bound to `pane_id` when the agent runs again.
    pub fn clear_pane_todos_review(&mut self, pane_id: u64, events: &mut Vec<WorkspaceEvent>) {
        match self.store.clear_pane_todos_review(pane_id) {
            Ok(boards) => {
                for board_id in boards {
                    self.emit(events, WorkspaceEvent::TodosChanged(board_id));
                }
            }
            Err(error) => eprintln!("[app] 清除 TODO 待确认状态失败：{error}"),
        }
    }

    /// Record the latest agent id and session id for TODOs linked to `pane_id`.
    pub fn update_pane_todos_session(
        &mut self,
        pane_id: u64,
        agent_id: &str,
        session_id: &str,
        events: &mut Vec<WorkspaceEvent>,
    ) {
        match self
            .store
            .update_pane_todos_session(pane_id, agent_id, session_id)
        {
            Ok(boards) => {
                for board_id in boards {
                    self.emit(events, WorkspaceEvent::TodosChanged(board_id));
                }
            }
            Err(error) => eprintln!("[app] 更新 TODO 会话记录失败：{error}"),
        }
    }

    /// Detach `pane_id` from TODOs when that pane closes or exits back to a shell.
    pub fn unbind_pane_todos(&mut self, pane_id: u64, events: &mut Vec<WorkspaceEvent>) {
        match self.store.unbind_pane_todos(pane_id) {
            Ok(boards) => {
                for board_id in boards {
                    self.emit(events, WorkspaceEvent::TodosChanged(board_id));
                }
            }
            Err(error) => eprintln!("[app] 解绑 TODO 窗格失败：{error}"),
        }
    }

    /// The first project, creating the first-run one when the list is empty.
    pub fn ensure_project(&mut self, events: &mut Vec<WorkspaceEvent>) -> Option<i64> {
        if let Some(id) = self.projects.first().map(|project| project.id) {
            return Some(id);
        }
        let Some(id) = insert_bootstrap_project(&self.store) else {
            self.emit(
                events,
                WorkspaceEvent::Failed("无法创建默认项目".to_string()),
            );
            return None;
        };
        self.refresh();
        self.emit(events, WorkspaceEvent::Changed);
        Some(id)
    }

    /// Ensure `pane` has a running terminal in `project_id`, returning its
    /// current screen snapshot and an optional [`SpawnedTerminal`] when a new
    /// PTY process was created and needs a watcher task.
    pub fn show_terminal(
        &mut self,
        pane: u64,
        project_id: i64,
        launch: &PaneLaunch,
        colors: impl FnOnce() -> TermColors,
    ) -> (Snapshot, Option<SpawnedTerminal>) {
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
            return (term.snapshot(), None);
        }
        self.park_terminal(pane);
        let Some(path) = self.project(project_id).map(|project| project.path.clone()) else {
            return (Snapshot::blank("", ""), None);
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
            return (term.snapshot(), None);
        }
        self.reclaim_terminals();
        let (program, args, launch) = self.command(launch);
        let (term, spawned) = self.spawn(pane, project_id, &path, program, args, launch, colors());
        (term.snapshot(), Some(spawned))
    }

    /// Start a pane's process without drawing it, parked until its project is
    /// selected. Returns `(started_or_already_live, optional_spawned_watcher)`.
    pub fn prestart(
        &mut self,
        pane: u64,
        project_id: i64,
        launch: &PaneLaunch,
        colors: impl FnOnce() -> TermColors,
    ) -> (bool, Option<SpawnedTerminal>) {
        if self.pane_term.contains_key(&pane)
            || self
                .terms
                .values()
                .any(|live| live.parked_for == Some(pane) && live.project == project_id)
        {
            return (true, None);
        }
        let alive = self
            .terms
            .values()
            .filter(|live| !live.term.snapshot().exited)
            .count();
        if alive >= MAX_LIVE_TERMINALS {
            return (false, None);
        }
        let Some(path) = self.project(project_id).map(|project| project.path.clone()) else {
            return (false, None);
        };
        let (program, args, launch) = self.command(launch);
        let (_term, spawned) = self.spawn(pane, project_id, &path, program, args, launch, colors());
        let Some(term_id) = self.pane_term.remove(&pane) else {
            return (false, Some(spawned));
        };
        // `spawn` attached it; keeping it parked is the whole point.
        if let Some(live) = self.terms.get_mut(&term_id) {
            live.pane = None;
            live.parked_for = Some(pane);
            live.term.wake_handled();
        }
        self.touch_terminal(term_id);
        (true, Some(spawned))
    }

    /// Process one wake from terminal `term_id`: drain OSC notifications,
    /// titles, exits, and frame updates into `events`. Returns `true` if the
    /// watcher should keep listening, or `false` once the terminal is gone.
    pub fn drain_terminal(&mut self, term_id: u64, events: &mut Vec<WorkspaceEvent>) -> bool {
        let Some(live) = self.terms.get_mut(&term_id) else {
            return false;
        };
        let project_id = live.project;
        let term = live.term.clone();
        // Clear the wake gate *before* reading the snapshot and draining
        // notifications so any event published concurrently schedules a fresh
        // wake — even for hidden tabs or parked terminals that have no view.
        term.wake_handled();
        let snapshot = term.snapshot();
        let pane = live.pane;
        let owning_pane = pane.or(live.parked_for);
        let agent_pane = !live.launch.is_shell();
        let resume_failed = resume_failed(&live.launch, live.started);
        let mut pending_events = Vec::new();
        while let Some(notification) = term.take_notification() {
            // A session update travels as a notification but is not one:
            // authenticate it against the pane's random token so untrusted
            // terminal output cannot spoof it.
            if let Some(rest) = notification.body.strip_prefix(SESSION_PREFIX) {
                if let Some(pane) = owning_pane
                    && let Some(session) = authenticated_session(rest, term.session_token())
                {
                    pending_events.push(WorkspaceEvent::Session {
                        pane,
                        agent: notification.title.clone(),
                        session: session.to_string(),
                    });
                }
                continue;
            }
            // The status channel is silent too: the sidebar and pane chrome
            // read it, no banner is shown.
            if let Some(signal) = notification
                .body
                .strip_prefix(STATUS_PREFIX)
                .and_then(StatusSignal::parse)
            {
                if let Some(pane) = owning_pane {
                    pending_events.push(WorkspaceEvent::AgentStatus {
                        pane,
                        project: project_id,
                        agent: notification.title.clone(),
                        signal,
                    });
                }
                continue;
            }
            if let Some(rest) = notification.body.strip_prefix(TITLE_PREFIX) {
                let (is_prompt, raw) = match rest.strip_prefix(TITLE_PROMPT_PREFIX) {
                    Some(prompt) => (true, prompt),
                    None => (false, rest),
                };
                if let Some(pane) = owning_pane
                    && let Some(title) = clean_reported_title(raw, is_prompt)
                {
                    pending_events.push(WorkspaceEvent::AgentTitle {
                        pane,
                        project: project_id,
                        agent: notification.title.clone(),
                        title,
                        is_prompt,
                    });
                }
                continue;
            }
            let now = Instant::now();
            if !live
                .last_notification
                .as_ref()
                .is_some_and(|(last, previous)| {
                    now.duration_since(*last) < Duration::from_secs(2) && previous == &notification
                })
            {
                live.last_notification = Some((now, notification.clone()));
                pending_events.push(WorkspaceEvent::Attention {
                    terminal: term_id,
                    project: project_id,
                    notification,
                });
            }
        }
        while let Some(status) = term.take_program_status() {
            if let Some(pane) = owning_pane {
                pending_events.push(WorkspaceEvent::ProgramStatus {
                    terminal: term_id,
                    pane,
                    project: project_id,
                    status,
                });
            }
        }
        if let Some(text) = term.take_clipboard_write() {
            pending_events.push(WorkspaceEvent::ClipboardWrite(text));
        }
        if snapshot.title != live.last_title {
            live.last_title = snapshot.title.clone();
            if let Some(pane) = owning_pane {
                pending_events.push(WorkspaceEvent::PaneTitle {
                    pane,
                    project: project_id,
                    title: snapshot.title.clone(),
                });
            }
        }
        let mut keep_watching = true;
        // A parked terminal that failed to spawn has no header to display the
        // error; drop it so it does not leak a slot.
        if pane.is_none() && snapshot.spawn_error.is_some() {
            self.drop_term(term_id);
            keep_watching = false;
        } else {
            // A clean shell death closes this pane; an agent death turns it
            // back into a shell. The project stays. Spawn failures keep the
            // visible pane so the header can show why.
            let shell_done = snapshot.exited && snapshot.spawn_error.is_none();
            if shell_done {
                self.drop_term(term_id);
                if let Some(pane) = owning_pane {
                    if agent_pane {
                        pending_events.push(WorkspaceEvent::AgentExited {
                            pane,
                            project: project_id,
                            resume_failed,
                        });
                    } else {
                        pending_events.push(WorkspaceEvent::PaneExited {
                            pane,
                            project: project_id,
                        });
                    }
                }
                keep_watching = false;
            } else if let Some(pane) = pane {
                pending_events.push(WorkspaceEvent::Frame(pane));
            }
        }
        for event in pending_events {
            self.emit(events, event);
        }
        keep_watching
    }

    /// Whether the process in `pane` was started as a shell rather than as an
    /// agent. `None` when the pane has no live process to inspect.
    pub fn pane_is_shell(&self, pane: u64) -> Option<bool> {
        self.live_for_pane(pane).map(|live| live.launch.is_shell())
    }

    /// Last OSC 0/2 title observed for `pane` (visible or parked).
    pub fn pane_title(&self, pane: u64) -> Option<String> {
        self.live_for_pane(pane).map(|live| live.last_title.clone())
    }

    /// Snapshot the current process table once so callers checking several
    /// panes do not spawn `/bin/ps` repeatedly on the UI thread.
    pub fn process_tree_snapshot() -> Option<String> {
        let output = std::process::Command::new("/bin/ps")
            .args(["-axo", "pid=,ppid=,command="])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Whether `program` (an executable file name) still runs below the shell
    /// shown in `pane`, evaluated against a previously captured `ps` snapshot.
    pub fn pane_runs_program_in(&self, pane: u64, program: &str, ps: &str) -> Option<bool> {
        let live = self.live_for_pane(pane)?;
        if !live.launch.is_shell() {
            return None;
        }
        Some(tree_runs(ps, live.term.child_pid()?, program))
    }

    /// Every live shell pane (visible or parked) paired with its root child PID.
    pub fn live_shell_panes(&self) -> Vec<(u64, u32)> {
        self.terms
            .values()
            .filter(|live| live.launch.is_shell())
            .filter_map(|live| {
                let pane = live.pane.or(live.parked_for)?;
                let pid = live.term.child_pid()?;
                Some((pane, pid))
            })
            .collect()
    }

    /// Inspect a single `ps` snapshot to detect which known agent (if any) is
    /// running below each shell pane. Intended for background threads.
    pub fn detect_shell_agents(shells: &[(u64, u32)]) -> Option<Vec<(u64, Option<String>)>> {
        if shells.is_empty() {
            return Some(Vec::new());
        }
        let ps = Self::process_tree_snapshot()?;
        let tree = ProcessTree::parse(&ps);
        let programs = agent_program_matchers();
        Some(
            shells
                .iter()
                .map(|&(pane, pid)| (pane, tree.detect_agent(pid, &programs)))
                .collect(),
        )
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
        colors: TermColors,
    ) -> (Term, SpawnedTerminal) {
        let (term, wake) =
            Term::launch_in_pane(Some(pane), program, args, PathBuf::from(path), colors);
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
                last_title: String::new(),
            },
        );
        self.pane_term.insert(pane, term_id);
        self.touch_terminal(term_id);
        (term, SpawnedTerminal { term_id, wake })
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
            live.term.wake_handled();
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
            .map(|live| live.term.label())
            .unwrap_or_default()
    }

    /// Pane currently showing or owning `terminal`, if it is still live.
    pub fn terminal_pane(&self, terminal: u64) -> Option<u64> {
        self.terms
            .get(&terminal)
            .and_then(|live| live.pane.or(live.parked_for))
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
        if let Some(live) = self.shown_term(pane) {
            live.term.resize(cols, rows, cell_w, cell_h);
        }
    }

    /// A pane's terminal, whether the project is showing or the process is
    /// parked behind another project.
    pub fn terminal_for_pane(&self, pane: u64) -> Option<Term> {
        self.live_term_for_pane(pane).map(|live| live.term.clone())
    }

    /// Clear retained OSC 7501 status records for `pane`'s live terminal.
    pub fn reset_terminal_program_status(&self, pane: u64) {
        if let Some(live) = self.live_term_for_pane(pane) {
            live.term.reset_program_status();
        }
    }

    /// Stop the process shown in `pane`, and any process parked for that pane.
    pub fn drop_terminal(&mut self, pane: u64) {
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
        live.term.wake_handled();
        if live.term.snapshot().spawn_error.is_some() {
            self.drop_term(term_id);
        }
    }

    /// Park a pane while its project is hidden from the window.
    pub fn hide_terminal(&mut self, pane: u64) {
        self.park_terminal(pane);
    }

    /// The hidden terminal owned by `pane`.
    pub(crate) fn parked_terminal(&self, project_id: i64, pane: u64) -> Option<u64> {
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
    /// running process is on screen.
    pub(crate) fn reclaim_terminals(&mut self) {
        // The watcher owns exit cleanup and must still report the pane to the
        // UI. Reclaiming it here would consume that exit before it is handled.
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
            .filter(|id| self.terms.get(id).is_some_and(|live| live.pane.is_none()))
            .collect();
        for id in terminal_evictions(&parked, alive.len(), MAX_LIVE_TERMINALS) {
            self.drop_term(id);
        }
    }

    /// Add the folder at `path` as a project, named after the folder itself —
    /// or, when it already is one, return that project instead of inserting a
    /// duplicate row.
    pub fn add_project(&mut self, path: &str, events: &mut Vec<WorkspaceEvent>) -> Option<i64> {
        let name = match resolve_project_pick(&self.projects, path) {
            ProjectPick::Existing(id) => return Some(id),
            ProjectPick::New { name } => name,
        };
        let id = match self.store.insert_project(&name, path) {
            Ok(id) => id,
            Err(error) => {
                self.emit_failed(events, "添加项目失败", error);
                return None;
            }
        };
        self.refresh();
        self.emit(events, WorkspaceEvent::Changed);
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
        events: &mut Vec<WorkspaceEvent>,
    ) -> Option<i64> {
        if !self.projects.iter().any(|project| project.id == parent_id) {
            return None;
        }
        let id = match self.store.insert_worktree(parent_id, name, path) {
            Ok(id) => id,
            Err(error) => {
                self.emit_failed(events, "添加 worktree 失败", error);
                return None;
            }
        };
        self.refresh();
        self.emit(events, WorkspaceEvent::Changed);
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
        events: &mut Vec<WorkspaceEvent>,
    ) -> Option<i64> {
        if let Some(existing) = project_for_path(&self.projects, Path::new(path)) {
            return Some(existing);
        }
        self.add_worktree(parent_id, name, path, events)
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
    pub fn rename_project(
        &mut self,
        project_id: i64,
        name: String,
        events: &mut Vec<WorkspaceEvent>,
    ) {
        if let Err(error) = self.store.rename_project(project_id, &name) {
            self.emit_failed(events, "重命名项目失败", error);
            return;
        }
        self.refresh();
        self.emit(events, WorkspaceEvent::Changed);
    }

    /// Delete a project and its worktrees. Terminals are stopped only after
    /// the rows are gone, so a failed write leaves the workspace untouched.
    pub fn delete_project(
        &mut self,
        project_id: i64,
        events: &mut Vec<WorkspaceEvent>,
    ) -> Result<(), ()> {
        let removed = self.project_tree_ids(project_id);
        if let Err(error) = self.store.delete_project(project_id) {
            self.emit_failed(events, "删除项目失败", error);
            return Err(());
        }
        for id in removed {
            self.drop_project_terminals(id);
        }
        self.refresh();
        self.emit(events, WorkspaceEvent::Changed);
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
    if let Some(id) = project_for_path(projects, Path::new(path)) {
        return ProjectPick::Existing(id);
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

/// Whether `command` (from `ps -o command=`) executes `program` directly or
/// through a scripting runtime (`node`, `bun`, `deno`, `python`, `sh`, `env`).
/// Matching only the command's executable / script entrypoint avoids false
/// positives like `vim codex` or `grep claude notes.txt`.
fn command_runs_program(command: &str, program: &str) -> bool {
    let mut tokens = command.split_whitespace();
    let Some(first) = tokens.next() else {
        return false;
    };
    let matches_token = |token: &str| {
        let path = Path::new(token);
        path.file_name().and_then(|n| n.to_str()) == Some(program)
            || path.file_stem().and_then(|n| n.to_str()) == Some(program)
    };
    if Path::new(first).file_name().and_then(|n| n.to_str()) == Some(program) {
        return true;
    }
    let runtime = Path::new(first)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let is_runtime = matches!(
        runtime,
        "node" | "bun" | "deno" | "python" | "python3" | "sh" | "bash" | "zsh" | "env"
    );
    if !is_runtime {
        return false;
    }
    // Skip runtime flags (`--experimental-...`, `-e`, `FOO=bar` for `env`) to
    // inspect the script/program token.
    for token in tokens {
        if token.starts_with('-') || (runtime == "env" && token.contains('=')) {
            continue;
        }
        return matches_token(token);
    }
    false
}

/// Parsed `ps -axo pid=,ppid=,command=` table for reuse across multiple panes.
struct ProcessTree<'a> {
    children: HashMap<u32, Vec<u32>>,
    commands: HashMap<u32, &'a str>,
}

impl<'a> ProcessTree<'a> {
    fn parse(ps: &'a str) -> Self {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut commands: HashMap<u32, &'a str> = HashMap::new();
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
        Self { children, commands }
    }

    /// Return the first descendant command matching `predicate` below `root`.
    fn find_descendant<T>(
        &self,
        root: u32,
        mut predicate: impl FnMut(&str) -> Option<T>,
    ) -> Option<T> {
        // Start below the root: the shell itself is never the agent, even if the
        // program name happens to match its own.
        let mut stack: Vec<u32> = self.children.get(&root).cloned().unwrap_or_default();
        let mut seen = HashSet::new();
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if let Some(command) = self.commands.get(&pid)
                && let Some(found) = predicate(command)
            {
                return Some(found);
            }
            stack.extend(self.children.get(&pid).into_iter().flatten().copied());
        }
        None
    }

    fn runs_program(&self, root: u32, program: &str) -> bool {
        self.find_descendant(root, |command| {
            command_runs_program(command, program).then_some(())
        })
        .is_some()
    }

    fn detect_agent(&self, root: u32, matchers: &[(&'static str, Vec<String>)]) -> Option<String> {
        self.find_descendant(root, |command| {
            matchers.iter().find_map(|(id, programs)| {
                programs
                    .iter()
                    .any(|program| command_runs_program(command, program))
                    .then(|| (*id).to_string())
            })
        })
    }
}

/// Executable names that identify each registered agent in a `ps` listing.
/// Generic names like `agent` (Cursor's `~/.local/bin/agent` symlink) are
/// skipped so unrelated binaries are never misclassified.
fn agent_program_matchers() -> Vec<(&'static str, Vec<String>)> {
    crate::agent::AGENTS
        .iter()
        .map(|def| {
            let mut names = Vec::new();
            if let Some(exe) = crate::agent::executable(def.id)
                && let Some(name) = Path::new(&exe).file_name().and_then(|n| n.to_str())
                && name != "agent"
            {
                names.push(name.to_string());
            }
            if def.id == "cursor" && !names.iter().any(|n| n == "cursor-agent") {
                names.push("cursor-agent".to_string());
            }
            (def.id, names)
        })
        .filter(|(_, names)| !names.is_empty())
        .collect()
}

fn tree_runs(ps: &str, root: u32, program: &str) -> bool {
    ProcessTree::parse(ps).runs_program(root, program)
}

/// Resolve a project or worktree id to the root repository id that owns its
/// TODO board, guarding against missing parents or corrupted cycles.
fn board_root_id(projects: &[Project], project_id: i64) -> i64 {
    let mut current = project_id;
    for _ in 0..8 {
        let Some(parent) = projects
            .iter()
            .find(|project| project.id == current)
            .and_then(|project| project.parent_id)
        else {
            break;
        };
        if parent == current || !projects.iter().any(|project| project.id == parent) {
            break;
        }
        current = parent;
    }
    current
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::{
        LiveTerm, ProjectPick, WorkspaceCore, first_run_dir_from, project_name,
        resolve_project_pick, terminal_evictions,
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
                    last_title: String::new(),
                },
            );
        }
        let mut core = WorkspaceCore::in_memory();
        core.terms = terms;
        core.pane_term = HashMap::from([(1, 1), (2, 2)]);
        core.term_lru = vec![3, 1, 2];
        core.next_term = 4;

        assert!(!core.notification_is_read(1, None), "inactive window");
        assert!(
            !core.notification_is_read(2, Some(1)),
            "same project, other pane"
        );
        assert!(core.notification_is_read(1, Some(1)));
        assert!(!core.notification_is_read(3, Some(1)), "parked terminal");
        assert_eq!(core.reveal_terminal(2), Some((2, 10)));
        assert_eq!(
            core.pane_term[&1], 1,
            "an already visible terminal keeps its pane"
        );
        assert_eq!(core.reveal_terminal(3), Some((3, 20)));
        assert_eq!(core.pane_term[&1], 1);
        assert_eq!(core.pane_term[&2], 2);
        assert_eq!(core.pane_term[&3], 3);
        assert_eq!(core.terms[&3].pane, Some(3));
        assert_eq!(core.terms[&3].parked_for, None);
        assert!(core.notification_is_read(3, Some(3)));
        core.hide_terminal(3);
        assert_eq!(core.parked_terminal(20, 3), Some(3));
        core.drop_terminal(1);
        assert!(
            core.notification_is_read(1, None),
            "removed terminal clears stale attention"
        );
        assert_eq!(
            core.reveal_terminal(1),
            None,
            "stale notification cannot restart a process"
        );
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

    #[test]
    fn worktrees_share_their_root_repository_board_id() {
        let projects = vec![
            Project {
                id: 1,
                name: "kumu".into(),
                path: "/repo/kumu".into(),
                parent_id: None,
            },
            Project {
                id: 2,
                name: "feat-todo".into(),
                path: "/repo/kumu-feat-todo".into(),
                parent_id: Some(1),
            },
            Project {
                id: 3,
                name: "orphan".into(),
                path: "/repo/orphan".into(),
                parent_id: Some(99),
            },
        ];
        assert_eq!(super::board_root_id(&projects, 1), 1);
        assert_eq!(super::board_root_id(&projects, 2), 1);
        assert_eq!(super::board_root_id(&projects, 3), 3);
    }

    #[test]
    fn process_tree_detects_agent_below_shell_and_ignores_unrelated_commands() {
        let ps = "\
  100   1 /bin/zsh -l
  101 100 node /Users/me/.npm-global/bin/pi
  200   1 /bin/zsh -l
  201 200 vim pi
  300   1 /bin/zsh -l
  301 300 /opt/homebrew/bin/codex
";
        let tree = super::ProcessTree::parse(ps);
        let matchers = vec![
            ("pi", vec!["pi".to_string()]),
            ("codex", vec!["codex".to_string()]),
        ];
        assert_eq!(tree.detect_agent(100, &matchers).as_deref(), Some("pi"));
        assert_eq!(tree.detect_agent(200, &matchers), None);
        assert_eq!(tree.detect_agent(300, &matchers).as_deref(), Some("codex"));
    }
}
