//! Left column host adapter: builds [`SidebarSnapshot`] for the Pure Rust NSH
//! sidebar (`sidebar_nsh.rs`) and handles [`SidebarAction`] callbacks.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{App, ClipboardItem, Context, Window};

use super::KumuApp;
use super::sidebar_nsh::{
    AgentRowSnapshot, ProjectRowKind, ProjectRowSnapshot, SidebarAction, SidebarSnapshot,
    UpdateNoticeSnapshot,
};
use crate::agent::AgentState;
use crate::app::layout::PaneId;
use crate::updates::UpdateStatus;

impl KumuApp {
    /// Periodically probe each root project's checked-out git branch off the UI
    /// thread so the sidebar's main worktree row follows branch switches.
    pub(super) fn start_project_branch_watcher(&self, cx: &mut Context<Self>) {
        self.refresh_project_branches(cx);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(3))
                    .await;
                let Ok(roots) = this.update(cx, |app, cx| app.root_project_paths(cx)) else {
                    break;
                };
                if roots.is_empty() {
                    continue;
                }
                let branches = cx
                    .background_executor()
                    .spawn(async move { probe_project_branches(roots) })
                    .await;
                if this
                    .update(cx, |app, cx| app.apply_project_branches(branches, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// Trigger an immediate background probe of root project branches (e.g.
    /// right after adding a project).
    pub(super) fn refresh_project_branches(&self, cx: &mut Context<Self>) {
        let roots = self.root_project_paths(cx);
        if roots.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let branches = cx
                .background_executor()
                .spawn(async move { probe_project_branches(roots) })
                .await;
            let _ = this.update(cx, |app, cx| app.apply_project_branches(branches, cx));
        })
        .detach();
    }

    fn root_project_paths(&self, cx: &Context<Self>) -> Vec<(i64, PathBuf)> {
        let workspace = self.workspace.read(cx);
        let projects = workspace.projects();
        projects
            .iter()
            .filter(|project| {
                project.parent_id.is_none_or(|parent| {
                    parent == project.id || !projects.iter().any(|candidate| candidate.id == parent)
                })
            })
            .map(|project| (project.id, PathBuf::from(&project.path)))
            .collect()
    }

    fn apply_project_branches(&mut self, branches: HashMap<i64, String>, cx: &mut Context<Self>) {
        if self.project_branches != branches {
            self.project_branches = branches;
            cx.notify();
        }
    }

    pub(super) fn build_sidebar_snapshot(&self, cx: &Context<Self>) -> SidebarSnapshot {
        let updater = self.updater.read(cx);
        let update_notice = match updater.status() {
            UpdateStatus::Available(version) => Some(UpdateNoticeSnapshot {
                icon: "icons/arrow-up.svg",
                label: format!("新版本 {version}"),
            }),
            UpdateStatus::Downloading { downloaded, total } => {
                let label = match total {
                    Some(total) if *total > 0 => format!(
                        "正在下载 {}%",
                        (downloaded.saturating_mul(100) / total).min(100)
                    ),
                    _ => "正在下载更新…".to_string(),
                };
                Some(UpdateNoticeSnapshot {
                    icon: "icons/loader-circle.svg",
                    label,
                })
            }
            UpdateStatus::Installing => Some(UpdateNoticeSnapshot {
                icon: "icons/loader-circle.svg",
                label: "正在安装更新…".to_string(),
            }),
            UpdateStatus::Staged(_) => Some(UpdateNoticeSnapshot {
                icon: "icons/check.svg",
                label: "重启以完成更新".to_string(),
            }),
            UpdateStatus::Errored(_) if updater.available().is_some() => {
                Some(UpdateNoticeSnapshot {
                    icon: "icons/triangle-alert.svg",
                    label: "更新失败，查看详情".to_string(),
                })
            }
            _ => None,
        };

        let workspace = self.workspace.read(cx);
        let projects = workspace.projects();

        // Order root repositories with their worktrees directly below them.
        let mut ordered_projects: Vec<(&crate::db::Project, bool)> =
            Vec::with_capacity(projects.len());
        let mut seen = HashSet::with_capacity(projects.len());
        let roots = projects.iter().filter(|project| {
            project.parent_id.is_none_or(|parent| {
                parent == project.id || !projects.iter().any(|candidate| candidate.id == parent)
            })
        });
        for project in roots {
            if seen.insert(project.id) {
                ordered_projects.push((project, false));
            }
            for worktree in projects
                .iter()
                .filter(|worktree| worktree.parent_id == Some(project.id))
            {
                if seen.insert(worktree.id) {
                    ordered_projects.push((worktree, true));
                }
            }
        }
        for project in projects {
            if seen.insert(project.id) {
                ordered_projects.push((project, false));
            }
        }

        // A status change the user has not looked at yet keeps its project
        // marked, so a change in a background project is not silent.
        let unseen_projects: HashSet<i64> = self
            .tabs_by_project
            .iter()
            .filter(|(_, tabs)| {
                tabs.panes().iter().any(|pane| {
                    self.agent_status
                        .get(pane)
                        .is_some_and(|entry| entry.unseen)
                })
            })
            .map(|(project, _)| *project)
            .collect();

        let is_unread = |project_id: i64| {
            self.unread_notifications
                .values()
                .any(|&pid| pid == project_id)
                || unseen_projects.contains(&project_id)
        };

        let mut project_rows: Vec<ProjectRowSnapshot> = Vec::with_capacity(projects.len() * 2);
        let mut root_seen = HashSet::with_capacity(projects.len());
        let roots = projects.iter().filter(|project| {
            project.parent_id.is_none_or(|parent| {
                parent == project.id || !projects.iter().any(|candidate| candidate.id == parent)
            })
        });
        for project in roots {
            if !root_seen.insert(project.id) {
                continue;
            }
            let children: Vec<&crate::db::Project> = projects
                .iter()
                .filter(|wt| wt.parent_id == Some(project.id) && root_seen.insert(wt.id))
                .collect();

            let summary = self
                .todo_summaries
                .get(&project.id)
                .copied()
                .unwrap_or_default();
            let in_board_mode = self.active_todo_board.is_some();

            if let Some(branch) = self.project_branches.get(&project.id) {
                let has_active_child = !in_board_mode
                    && (self.selected == Some(project.id)
                        || children.iter().any(|wt| self.selected == Some(wt.id)));
                project_rows.push(ProjectRowSnapshot {
                    id: project.id,
                    name: project.name.clone(),
                    path: project.path.clone(),
                    kind: ProjectRowKind::RepoHeader,
                    repo_name: None,
                    active: self.active_todo_board == Some(project.id),
                    has_active_child,
                    unread: false,
                    todo_count: summary.open_count,
                    todo_attention: summary.needs_attention,
                });
                project_rows.push(ProjectRowSnapshot {
                    id: project.id,
                    name: branch.clone(),
                    path: project.path.clone(),
                    kind: ProjectRowKind::MainWorktree,
                    repo_name: Some(project.name.clone()),
                    active: !in_board_mode && self.selected == Some(project.id),
                    has_active_child: false,
                    unread: is_unread(project.id),
                    todo_count: 0,
                    todo_attention: false,
                });
            } else {
                project_rows.push(ProjectRowSnapshot {
                    id: project.id,
                    name: project.name.clone(),
                    path: project.path.clone(),
                    kind: ProjectRowKind::PlainProject,
                    repo_name: None,
                    active: if in_board_mode {
                        self.active_todo_board == Some(project.id)
                    } else {
                        self.selected == Some(project.id)
                    },
                    has_active_child: false,
                    unread: is_unread(project.id),
                    todo_count: summary.open_count,
                    todo_attention: summary.needs_attention,
                });
            }

            for worktree in children {
                project_rows.push(ProjectRowSnapshot {
                    id: worktree.id,
                    name: worktree.name.clone(),
                    path: worktree.path.clone(),
                    kind: ProjectRowKind::LinkedWorktree,
                    repo_name: Some(project.name.clone()),
                    active: !in_board_mode && self.selected == Some(worktree.id),
                    has_active_child: false,
                    unread: is_unread(worktree.id),
                    todo_count: 0,
                    todo_attention: false,
                });
            }
        }
        for project in projects {
            if root_seen.insert(project.id) {
                let summary = self
                    .todo_summaries
                    .get(&project.id)
                    .copied()
                    .unwrap_or_default();
                let in_board_mode = self.active_todo_board.is_some();
                project_rows.push(ProjectRowSnapshot {
                    id: project.id,
                    name: project.name.clone(),
                    path: project.path.clone(),
                    kind: ProjectRowKind::PlainProject,
                    repo_name: None,
                    active: if in_board_mode {
                        self.active_todo_board == Some(project.id)
                    } else {
                        self.selected == Some(project.id)
                    },
                    has_active_child: false,
                    unread: is_unread(project.id),
                    todo_count: summary.open_count,
                    todo_attention: summary.needs_attention,
                });
            }
        }

        let mut agent_rows = Vec::new();
        for (project, _) in &ordered_projects {
            let Some(tabs) = self.tabs_by_project.get(&project.id) else {
                continue;
            };
            for (tab_index, tab) in tabs.tabs.iter().enumerate() {
                let tab_name = Self::tab_name(tabs, tab_index);
                for pane in tab.layout.panes() {
                    let Some(entry) = self.agent_status.get(&pane) else {
                        continue;
                    };
                    agent_rows.push(AgentRowSnapshot {
                        pane,
                        project_name: project.name.clone(),
                        project_path: project.path.clone(),
                        tab_name: tab_name.clone(),
                        agent: entry.agent.clone(),
                        title: entry.display_title(),
                        state: entry.state,
                        focused: self.selected == Some(project.id) && self.focused == pane,
                        unseen: entry.unseen,
                    });
                }
            }
        }
        agent_rows.sort_by_key(|row| row.state.map_or(4, AgentState::priority));

        SidebarSnapshot {
            projects: project_rows,
            agents: agent_rows,
            agents_collapsed: self.agents_collapsed,
            update_notice,
        }
    }

    pub(super) fn handle_sidebar_action(
        &mut self,
        action: SidebarAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            SidebarAction::AddProject => self.add_project(cx),
            SidebarAction::SelectProject(project_id) => {
                self.active_todo_board = None;
                self.active_todo_modal = None;
                self.select_project(project_id, cx);
                self.focus_terminal(window, cx);
                cx.notify();
            }
            SidebarAction::OpenProjectBoard(project_id) => {
                self.open_todo_board(project_id, None, window, cx);
            }
            SidebarAction::NewTerminal(project_id) => {
                self.active_todo_board = None;
                self.active_todo_modal = None;
                self.add_terminal(Some(project_id), cx);
                self.focus_terminal(window, cx);
                cx.notify();
            }
            SidebarAction::PromptCreateWorktree(project_id) => {
                self.prompt_create_worktree(project_id, window, cx);
            }
            SidebarAction::PromptOpenWorktree(project_id) => {
                self.prompt_open_worktree(project_id, window, cx);
            }
            SidebarAction::PromptRenameProject(project_id) => {
                self.prompt_rename_project(project_id, window, cx);
            }
            SidebarAction::ConfirmDeleteProject(project_id) => {
                self.confirm_delete_project(project_id, window, cx);
            }
            SidebarAction::ConfirmRemoveWorktree(project_id) => {
                self.confirm_remove_worktree(project_id, window, cx);
            }
            SidebarAction::RevealInFinder(path) => reveal_in_finder(path, cx),
            SidebarAction::CopyText(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            SidebarAction::ToggleAgentsCollapsed => {
                self.agents_collapsed = !self.agents_collapsed;
                cx.notify();
            }
            SidebarAction::RevealAgent(pane) => {
                self.reveal_agent(pane, window, cx);
            }
            SidebarAction::OpenSettings(show_updates) => {
                self.open_settings(show_updates, cx);
            }
        }
    }

    /// Switch to the row's project/tab and focus its pane without clearing its
    /// status: a live agent stays in the list for its entire lifetime.
    pub(super) fn reveal_agent(
        &mut self,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_todo_board = None;
        self.active_todo_modal = None;
        self.focus_pane_from_user(pane, cx);
        self.focus_terminal(window, cx);
        cx.notify();
    }
}

/// Probe the checked-out git branch for each root project path.
fn probe_project_branches(roots: Vec<(i64, PathBuf)>) -> HashMap<i64, String> {
    let mut branches = HashMap::with_capacity(roots.len());
    for (id, path) in roots {
        if let Some(branch) = crate::git::repo_branch(&path) {
            branches.insert(id, branch);
        }
    }
    branches
}

/// Show `path` in a Finder window without blocking the UI thread.
fn reveal_in_finder(path: String, cx: &mut App) {
    cx.background_executor()
        .spawn(async move {
            let _ = std::process::Command::new("/usr/bin/open")
                .arg("--")
                .arg(path)
                .status();
        })
        .detach();
}
