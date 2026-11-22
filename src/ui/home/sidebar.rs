//! Left column host adapter: builds [`SidebarSnapshot`] for the Pure Rust NSH
//! sidebar (`sidebar_nsh.rs`) and handles [`SidebarAction`] callbacks.

use std::collections::HashSet;

use gpui::{App, ClipboardItem, Context, Window};

use super::KumuApp;
use super::sidebar_nsh::{
    AgentRowSnapshot, ProjectRowSnapshot, SidebarAction, SidebarSnapshot, UpdateNoticeSnapshot,
};
use crate::agent::AgentState;
use crate::app::layout::PaneId;
use crate::updates::UpdateStatus;

impl KumuApp {
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

        let project_rows: Vec<ProjectRowSnapshot> = ordered_projects
            .iter()
            .map(|(project, worktree)| ProjectRowSnapshot {
                id: project.id,
                name: project.name.clone(),
                path: project.path.clone(),
                worktree: *worktree,
                active: self.selected == Some(project.id),
                unread: self
                    .unread_notifications
                    .values()
                    .any(|&pid| pid == project.id),
            })
            .collect();

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
                self.select_project(project_id, cx);
                self.focus_terminal(window, cx);
                cx.notify();
            }
            SidebarAction::NewTerminal(project_id) => {
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
        self.focus_pane(pane, cx);
        self.focus_terminal(window, cx);
        cx.notify();
    }
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
