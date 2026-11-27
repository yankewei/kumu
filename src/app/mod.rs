//! The application layer: projects, layouts, and live terminals.
//!
//! Business state lives in framework-free [`core::WorkspaceCore`] and
//! [`layout`]; this module provides the GPUI `Entity` adapter ([`Workspace`])
//! that bridges core events and terminal wake streams into GPUI views.

pub mod core;
pub mod layout;

use std::path::Path;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use gpui::{Context, EventEmitter, Global};

pub use self::core::{NotificationSettings, SpawnedTerminal, WorkspaceCore, WorkspaceEvent};
use crate::agent::launch::PaneLaunch;
use crate::db::{Project, TodoItem, TodoPatch, TodoStatus};
use crate::term::{KeyInput, MouseInput, MouseMods, Snapshot, Term, TermColors};
use crate::theme;

impl Global for NotificationSettings {}

/// GPUI entity adapter around [`WorkspaceCore`].
///
/// Shares the underlying `Arc<Mutex<WorkspaceCore>>` so future NSH UI threads
/// can observe and mutate the same single source of truth without going
/// through GPUI entity borrows.
pub struct Workspace {
    core: Arc<Mutex<WorkspaceCore>>,
    /// Main-thread cache of `core.projects()` so existing GPUI callers keep
    /// zero-copy `&[Project]` access without holding the mutex across renders.
    projects: Vec<Project>,
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        let core = WorkspaceCore::open_default();
        let projects = core.projects().to_vec();
        Self {
            core: Arc::new(Mutex::new(core)),
            projects,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WorkspaceCore> {
        self.core.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit_events(&mut self, events: Vec<WorkspaceEvent>, cx: &mut Context<Self>) {
        if events.iter().any(|e| matches!(e, WorkspaceEvent::Changed)) {
            let projects = self.lock().projects().to_vec();
            self.projects = projects;
        }
        for event in events {
            cx.emit(event);
        }
    }

    /// Take the failure recorded before any view subscribed.
    pub fn take_startup_error(&mut self) -> Option<String> {
        self.lock().take_startup_error()
    }

    /// Take the layout JSON loaded before the home view existed.
    pub fn take_saved_workspace(&mut self) -> Option<String> {
        self.lock().take_saved_workspace()
    }

    /// Replace the saved layout, reporting a write failure to the home window.
    pub fn save_workspace_state(&self, json: &str, cx: &mut Context<Self>) -> bool {
        let mut events = Vec::new();
        let ok = self.lock().save_workspace_state(json, &mut events);
        for event in events {
            cx.emit(event);
        }
        ok
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
        self.lock().board_project_id(project_id)
    }

    /// All project ids (the root repository plus its worktrees) that share
    /// `board_id`'s TODO board.
    pub fn board_member_project_ids(&self, board_id: i64) -> Vec<i64> {
        self.lock().board_member_project_ids(board_id)
    }

    /// All TODO items for the repository owning `project_id`.
    pub fn todos_for_project(&self, project_id: i64) -> Vec<TodoItem> {
        self.lock().todos_for_project(project_id)
    }

    /// Look up one TODO item by id.
    pub fn todo(&self, id: i64) -> Option<TodoItem> {
        self.lock().todo(id)
    }

    /// Create a TODO item on the board owning `project_id`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_todo(
        &self,
        project_id: i64,
        title: &str,
        description: &str,
        status: TodoStatus,
        pane_id: Option<u64>,
        agent_id: Option<&str>,
        agent_session_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Option<TodoItem> {
        let mut events = Vec::new();
        let item = self.lock().add_todo(
            project_id,
            title,
            description,
            status,
            pane_id,
            agent_id,
            agent_session_id,
            &mut events,
        );
        for event in events {
            cx.emit(event);
        }
        item
    }

    /// Update one TODO item and notify subscribers.
    pub fn update_todo(
        &self,
        id: i64,
        patch: &TodoPatch,
        cx: &mut Context<Self>,
    ) -> Option<TodoItem> {
        let mut events = Vec::new();
        let item = self.lock().update_todo(id, patch, &mut events);
        for event in events {
            cx.emit(event);
        }
        item
    }

    /// Delete one TODO item and notify subscribers.
    pub fn delete_todo(&self, id: i64, cx: &mut Context<Self>) -> bool {
        let mut events = Vec::new();
        let removed = self.lock().delete_todo(id, &mut events);
        for event in events {
            cx.emit(event);
        }
        removed
    }

    /// Clear all completed TODO items on the board owning `project_id`.
    pub fn clear_done_todos(&self, project_id: i64, cx: &mut Context<Self>) -> usize {
        let mut events = Vec::new();
        let removed = self.lock().clear_done_todos(project_id, &mut events);
        for event in events {
            cx.emit(event);
        }
        removed
    }

    /// Mark in-progress TODOs bound to `pane_id` as awaiting human confirmation.
    pub fn mark_pane_todos_review(&self, pane_id: u64, cx: &mut Context<Self>) {
        let mut events = Vec::new();
        self.lock().mark_pane_todos_review(pane_id, &mut events);
        for event in events {
            cx.emit(event);
        }
    }

    /// Clear `awaiting_review` on TODOs bound to `pane_id` when the agent runs again.
    pub fn clear_pane_todos_review(&self, pane_id: u64, cx: &mut Context<Self>) {
        let mut events = Vec::new();
        self.lock().clear_pane_todos_review(pane_id, &mut events);
        for event in events {
            cx.emit(event);
        }
    }

    /// Record the latest agent id and session id for TODOs linked to `pane_id`.
    pub fn update_pane_todos_session(
        &self,
        pane_id: u64,
        agent_id: &str,
        session_id: &str,
        cx: &mut Context<Self>,
    ) {
        let mut events = Vec::new();
        self.lock()
            .update_pane_todos_session(pane_id, agent_id, session_id, &mut events);
        for event in events {
            cx.emit(event);
        }
    }

    /// Detach `pane_id` from TODOs when that pane closes or exits back to a shell.
    pub fn unbind_pane_todos(&self, pane_id: u64, cx: &mut Context<Self>) {
        let mut events = Vec::new();
        self.lock().unbind_pane_todos(pane_id, &mut events);
        for event in events {
            cx.emit(event);
        }
    }

    /// The first project, creating the first-run one when the list is empty.
    pub fn ensure_project(&mut self, cx: &mut Context<Self>) -> Option<i64> {
        let mut events = Vec::new();
        let id = self.lock().ensure_project(&mut events);
        self.emit_events(events, cx);
        id
    }

    /// The pane's screen, after making sure its process is running in
    /// `project_id`.
    pub fn show_terminal(
        &mut self,
        pane: u64,
        project_id: i64,
        launch: &PaneLaunch,
        cx: &mut Context<Self>,
    ) -> Snapshot {
        let (snapshot, spawned) = self
            .lock()
            .show_terminal(pane, project_id, launch, || theme::term_colors(cx));
        if let Some(spawned) = spawned {
            self.watch_terminal(spawned, cx);
        }
        snapshot
    }

    /// Start a pane's process without drawing it, parked until its project is
    /// selected.
    pub fn prestart(
        &mut self,
        pane: u64,
        project_id: i64,
        launch: &PaneLaunch,
        cx: &mut Context<Self>,
    ) -> bool {
        let (ok, spawned) = self
            .lock()
            .prestart(pane, project_id, launch, || theme::term_colors(cx));
        if let Some(spawned) = spawned {
            self.watch_terminal(spawned, cx);
        }
        ok
    }

    /// Whether the process in `pane` was started as a shell rather than as an
    /// agent. `None` when the pane has no live process to inspect.
    pub fn pane_is_shell(&self, pane: u64) -> Option<bool> {
        self.lock().pane_is_shell(pane)
    }

    /// Last OSC 0/2 title observed for `pane` (visible or parked).
    pub fn pane_title(&self, pane: u64) -> Option<String> {
        self.lock().pane_title(pane)
    }

    /// Snapshot the current process table once so callers checking several
    /// panes do not spawn `/bin/ps` repeatedly on the UI thread.
    pub fn process_tree_snapshot() -> Option<String> {
        WorkspaceCore::process_tree_snapshot()
    }

    /// Whether `program` (an executable file name) still runs below the shell
    /// shown in `pane`, evaluated against a previously captured `ps` snapshot.
    pub fn pane_runs_program_in(&self, pane: u64, program: &str, ps: &str) -> Option<bool> {
        self.lock().pane_runs_program_in(pane, program, ps)
    }

    /// Every live shell pane (visible or parked) paired with its root child PID.
    pub fn live_shell_panes(&self) -> Vec<(u64, u32)> {
        self.lock().live_shell_panes()
    }

    /// Inspect a single `ps` snapshot to detect which known agent (if any) is
    /// running below each shell pane. Intended for background threads.
    pub fn detect_shell_agents(shells: &[(u64, u32)]) -> Option<Vec<(u64, Option<String>)>> {
        WorkspaceCore::detect_shell_agents(shells)
    }

    /// A removed terminal or the terminal being read no longer needs attention.
    /// `focused_pane` is absent while the home window is inactive.
    pub fn notification_is_read(&self, terminal: u64, focused_pane: Option<u64>) -> bool {
        self.lock().notification_is_read(terminal, focused_pane)
    }

    /// Bring the exact terminal that sent a notification back into a pane.
    /// A parked process is reattached without restarting its agent.
    pub fn reveal_terminal(&mut self, terminal: u64) -> Option<(u64, i64)> {
        self.lock().reveal_terminal(terminal)
    }

    /// Push the UI theme's colors into every live terminal.
    pub fn set_terminal_colors(&self, colors: TermColors) {
        self.lock().set_terminal_colors(colors);
    }

    pub fn terminal_snapshot(&self, pane: u64) -> Option<Snapshot> {
        self.lock().terminal_snapshot(pane)
    }

    /// What the terminal at `terminal` calls itself, for the notification
    /// title. Empty when the program set no title and no command is known.
    pub fn terminal_label(&self, terminal: u64) -> String {
        self.lock().terminal_label(terminal)
    }

    /// Pane currently showing or owning `terminal`, if it is still live.
    pub fn terminal_pane(&self, terminal: u64) -> Option<u64> {
        self.lock().terminal_pane(terminal)
    }

    pub fn terminal_wake_handled(&self, pane: u64) {
        self.lock().terminal_wake_handled(pane);
    }

    pub fn terminal_key(&self, pane: u64, input: KeyInput) {
        self.lock().terminal_key(pane, input);
    }

    pub fn terminal_write(&self, pane: u64, bytes: impl Into<Vec<u8>>) {
        self.lock().terminal_write(pane, bytes);
    }

    pub fn terminal_paste(&self, pane: u64, text: impl Into<Vec<u8>>) {
        self.lock().terminal_paste(pane, text);
    }

    /// A handle to the pane's terminal, for work that must not run on the UI
    /// thread (the selection round trips block).
    pub fn terminal_handle(&self, pane: u64) -> Option<Term> {
        self.lock().terminal_handle(pane)
    }

    pub fn terminal_mouse(&self, pane: u64, input: MouseInput) {
        self.lock().terminal_mouse(pane, input);
    }

    pub fn terminal_clear_selection(&self, pane: u64) {
        self.lock().terminal_clear_selection(pane);
    }

    pub fn terminal_search_set(&self, pane: u64, needle: &str) {
        self.lock().terminal_search_set(pane, needle);
    }

    pub fn terminal_search_step(&self, pane: u64, older: bool) {
        self.lock().terminal_search_step(pane, older);
    }

    pub fn terminal_search_clear(&self, pane: u64) {
        self.lock().terminal_search_clear(pane);
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
        self.lock()
            .terminal_scroll(pane, lines, x, y, cell_w, cell_h, mods);
    }

    pub fn terminal_resize(&mut self, pane: u64, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        self.lock()
            .terminal_resize(pane, cols, rows, cell_w, cell_h);
    }

    /// A pane's terminal, whether the project is showing or the process is
    /// parked behind another project.
    pub fn terminal_for_pane(&self, pane: u64) -> Option<Term> {
        self.lock().terminal_for_pane(pane)
    }

    /// Clear retained OSC 7501 status records for `pane`'s live terminal.
    pub fn reset_terminal_program_status(&self, pane: u64) {
        self.lock().reset_terminal_program_status(pane);
    }

    /// Stop the process shown in `pane`, and any process parked for that pane.
    pub fn drop_terminal(&mut self, pane: u64) {
        self.lock().drop_terminal(pane);
    }

    /// Park a pane while its project is hidden from the window.
    pub fn hide_terminal(&mut self, pane: u64) {
        self.lock().hide_terminal(pane);
    }

    fn watch_terminal(&self, spawned: SpawnedTerminal, cx: &mut Context<Self>) {
        let SpawnedTerminal { term_id, mut wake } = spawned;
        cx.spawn(async move |this, cx| {
            let drain = |this: &gpui::WeakEntity<Workspace>, cx: &mut gpui::AsyncApp| -> bool {
                this.update(cx, |workspace, cx| {
                    let mut events = Vec::new();
                    let keep = workspace.lock().drain_terminal(term_id, &mut events);
                    workspace.emit_events(events, cx);
                    keep
                })
                .unwrap_or(false)
            };

            while wake.next().await.is_some() {
                if !drain(&this, cx) {
                    return;
                }
            }
            // Final drain when the sender closes so an exit or final
            // notification can never be lost on channel shutdown.
            let _ = drain(&this, cx);
        })
        .detach();
    }

    /// Add the folder at `path` as a project, named after the folder itself —
    /// or, when it already is one, return that project instead of inserting a
    /// duplicate row.
    pub fn add_project(&mut self, path: &str, cx: &mut Context<Self>) -> Option<i64> {
        let mut events = Vec::new();
        let id = self.lock().add_project(path, &mut events);
        self.emit_events(events, cx);
        id
    }

    /// Add a worktree row below the project it was created from.
    pub fn add_worktree(
        &mut self,
        parent_id: i64,
        name: &str,
        path: &str,
        cx: &mut Context<Self>,
    ) -> Option<i64> {
        let mut events = Vec::new();
        let id = self.lock().add_worktree(parent_id, name, path, &mut events);
        self.emit_events(events, cx);
        id
    }

    /// The project that already points at `path`, if any.
    pub fn project_by_path(&self, path: &Path) -> Option<i64> {
        self.lock().project_by_path(path)
    }

    /// Attach a worktree that already exists on disk.
    pub fn open_worktree(
        &mut self,
        parent_id: i64,
        name: &str,
        path: &str,
        cx: &mut Context<Self>,
    ) -> Option<i64> {
        let mut events = Vec::new();
        let id = self
            .lock()
            .open_worktree(parent_id, name, path, &mut events);
        self.emit_events(events, cx);
        id
    }

    /// A project and every worktree below it.
    pub fn project_tree_ids(&self, project_id: i64) -> Vec<i64> {
        self.lock().project_tree_ids(project_id)
    }

    /// Rename a project without moving it.
    pub fn rename_project(&mut self, project_id: i64, name: String, cx: &mut Context<Self>) {
        let mut events = Vec::new();
        self.lock().rename_project(project_id, name, &mut events);
        self.emit_events(events, cx);
    }

    /// Delete a project and its worktrees.
    pub fn delete_project(&mut self, project_id: i64, cx: &mut Context<Self>) -> Result<(), ()> {
        let mut events = Vec::new();
        let result = self.lock().delete_project(project_id, &mut events);
        self.emit_events(events, cx);
        result
    }
}

#[cfg(test)]
mod tests {
    #[gpui::test]
    fn shell_exit_reports_both_visible_and_parked_panes(cx: &mut gpui::TestAppContext) {
        use super::core::LiveTerm;
        use super::{SpawnedTerminal, Workspace, WorkspaceCore, WorkspaceEvent};
        use crate::agent::launch::PaneLaunch;
        use crate::term::{Term, TermColors};
        use gpui::{AppContext, Entity};
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let workspace = cx.new(|cx| {
            let mut core = WorkspaceCore::in_memory();
            core.pane_term = HashMap::from([(1, 1), (2, 2), (3, 3)]);
            core.term_lru = vec![1, 2, 3];
            core.next_term = 4;

            let mut watchers = Vec::new();
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
                core.terms.insert(
                    id,
                    LiveTerm {
                        term,
                        project: 10,
                        pane: Some(id),
                        parked_for: None,
                        launch: PaneLaunch::Shell,
                        started: Instant::now(),
                        last_notification: None,
                        last_title: String::new(),
                    },
                );
                if id != 3 {
                    // Deliver the final wake on this thread: GPUI's test
                    // scheduler rejects wakes from native PTY threads.
                    let (tx, wake) = futures::channel::mpsc::unbounded();
                    tx.unbounded_send(()).unwrap();
                    drop(tx);
                    watchers.push(SpawnedTerminal { term_id: id, wake });
                }
            }
            core.hide_terminal(2);
            core.reclaim_terminals();
            assert_eq!(core.parked_terminal(10, 2), Some(2));
            let mut workspace = Workspace {
                core: Arc::new(Mutex::new(core)),
                projects: Vec::new(),
            };
            for spawned in watchers {
                workspace.watch_terminal(spawned, cx);
            }
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
            let core = workspace.read(cx).lock();
            assert_eq!(core.terms.len(), 1);
            assert_eq!(core.pane_term, HashMap::from([(3, 3)]));
            assert_eq!(core.term_lru, vec![3]);
        });
    }
}
