//! The application layer: projects and live terminals — independent of how
//! it is drawn.
//!
//! The UI hears about changes through [`WorkspaceEvent`] rather than a blanket
//! notify: a terminal frame only concerns that pane.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui_kit::{Context, EventEmitter, Global};

use crate::db::{Project, Store, db_path};
use crate::term::{KeyInput, MouseInput, MouseMods, Snapshot, Term, TermColors, TermNotification};
use crate::theme;

/// Shared by the home and settings windows; changes apply immediately.
pub struct NotificationSettings {
    pub enabled: bool,
    /// Play the macOS alert sound when an attention notification is raised.
    pub sound: bool,
}

impl Global for NotificationSettings {}

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
    last_notification: Option<(Instant, TermNotification)>,
}

/// Projects and each live terminal.
///
/// One per app, owned by the home window. Nothing in here renders. Every pane
/// runs the user's shell in the project's directory.
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

    /// The pane's screen, after making sure a shell is running in `project_id`.
    ///
    /// An exited shell closes the pane (see [`Self::watch_terminal`]). Hiding
    /// the project parks the process and reattaches it when that project is
    /// shown again. Starting a process past the cap stops the parked one shown
    /// least recently; a pane's visible terminal is never stopped.
    pub fn show_terminal(
        &mut self,
        pane: u64,
        project_id: i64,
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
        let colors = theme::term_colors(cx);
        let (program, args) = crate::term::default_shell();
        let (term, wake) = Term::launch(program, args, PathBuf::from(&path), colors);
        let snapshot = term.snapshot();
        let term_id = self.next_term;
        self.next_term += 1;
        self.terms.insert(
            term_id,
            LiveTerm {
                term,
                project: project_id,
                pane: Some(pane),
                parked_for: None,
                last_notification: None,
            },
        );
        self.pane_term.insert(pane, term_id);
        self.touch_terminal(term_id);
        self.watch_terminal(term_id, project_id, wake, cx);
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

    pub fn terminal_resize(&self, pane: u64, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        if let Some(live) = self.shown_term(pane) {
            live.term.resize(cols, rows, cell_w, cell_h);
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
    /// running process is on screen.
    fn reclaim_terminals(&mut self) {
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
                                    project: project_id,
                                    notification,
                                });
                            }
                        }
                        // `exit` (or any clean shell death) closes this pane.
                        // The project stays. Spawn failures keep the pane so
                        // the header can show why.
                        let shell_done = snapshot.exited && snapshot.spawn_error.is_none();
                        if shell_done {
                            workspace.drop_term(term_id);
                            if let Some(pane) = owning_pane {
                                cx.emit(WorkspaceEvent::PaneExited {
                                    pane,
                                    project: project_id,
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        ProjectPick, first_run_dir_from, project_name, resolve_project_pick, terminal_evictions,
    };
    use crate::db::Project;

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
            assert!(workspace.show_terminal(1, 10, cx).exited);
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
