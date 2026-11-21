use gpui_kit::{Context, Window};

use super::KumuApp;
use super::tabs_nsh::{TabAttention, TabBarAction, TabBarSnapshot, TabItemSnapshot};
use crate::agent::AgentState;
use crate::app::layout::{PaneId, PaneNode, ProjectTab, ProjectTabs, TabId};
use crate::nsh_ui::dialogs::{DialogOutcome, DialogSpec};

impl KumuApp {
    /// The project and tab that own `pane`.
    pub(super) fn pane_tab(&self, pane: PaneId) -> Option<(i64, usize)> {
        self.tabs_by_project.iter().find_map(|(project, tabs)| {
            tabs.tabs
                .iter()
                .position(|tab| tab.layout.contains(pane))
                .map(|index| (*project, index))
        })
    }

    /// Whether `pane` belongs to the project and tab currently on screen.
    pub(super) fn pane_is_visible(&self, pane: PaneId) -> bool {
        let Some((project, tab_index)) = self.pane_tab(pane) else {
            return false;
        };
        self.selected == Some(project)
            && self
                .tabs_by_project
                .get(&project)
                .is_some_and(|tabs| tabs.selected == tab_index)
    }

    /// The visible tab of `project`.
    pub(super) fn active_tab(&self, project: i64) -> Option<&ProjectTab> {
        self.tabs_by_project.get(&project)?.active()
    }

    pub(super) fn active_tab_mut(&mut self, project: i64) -> Option<&mut ProjectTab> {
        self.tabs_by_project.get_mut(&project)?.active_mut()
    }

    /// The visible tab name, with a numbered fallback until the user renames it.
    pub(super) fn tab_name(tabs: &ProjectTabs, index: usize) -> String {
        let name = tabs
            .tabs
            .get(index)
            .map(|tab| tab.name.trim())
            .unwrap_or("");
        if name.is_empty() {
            format!("工作区 {}", index + 1)
        } else {
            name.to_string()
        }
    }

    /// Materialize and attach every pane of one tab. The other tabs of the
    /// project are hidden but keep their processes.
    pub(super) fn show_tab_panes(&mut self, project: i64, index: usize, cx: &mut Context<Self>) {
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return;
        };
        let hidden: Vec<PaneId> = tabs
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .flat_map(|(_, tab)| tab.layout.panes())
            .collect();
        let visible = tabs
            .tabs
            .get(index)
            .map(|tab| tab.layout.panes())
            .unwrap_or_default();
        for pane in hidden {
            if let Some(terminal) = self.panes.get(&pane).cloned() {
                terminal.update(cx, |terminal, cx| terminal.hide(cx));
            }
        }
        for pane in visible {
            let terminal = if let Some(terminal) = self.panes.get(&pane).cloned() {
                terminal
            } else {
                self.install_pane(pane, cx)
            };
            let launch = self.launches.get(&pane).cloned().unwrap_or_default();
            terminal.update(cx, |terminal, cx| terminal.show(project, &launch, cx));
            self.sync_pane_agent_status(pane, cx);
        }
    }

    /// Show the tab the project had selected.
    pub(super) fn show_selected_tab(&mut self, project: i64, cx: &mut Context<Self>) {
        let Some(index) = self.tabs_by_project.get(&project).map(|tabs| tabs.selected) else {
            return;
        };
        self.show_tab_panes(project, index, cx);
    }

    /// Open a new tab with one shell, after the visible one, and focus it.
    pub(super) fn new_tab(&mut self, cx: &mut Context<Self>) {
        let project_id = self.selected.or_else(|| {
            self.workspace
                .update(cx, |workspace, cx| workspace.ensure_project(cx))
        });
        let Some(project_id) = project_id else {
            return;
        };
        if !self.tabs_by_project.contains_key(&project_id) {
            self.ensure_project_layout(project_id, cx);
            self.select_project(project_id, cx);
            return;
        }
        let pane = PaneId(self.next_pane);
        let Some(tabs) = self.tabs_by_project.get_mut(&project_id) else {
            return;
        };
        let index = (tabs.selected + 1).min(tabs.tabs.len());
        tabs.tabs.insert(
            index,
            ProjectTab::new(String::new(), PaneNode::leaf(pane), pane),
        );
        tabs.selected = index;
        self.next_pane += 1;
        self.launches.remove(&pane);
        if self.selected != Some(project_id) {
            if let Some(previous) = self.selected {
                self.hide_project_panes(previous, cx);
            }
            self.selected = Some(project_id);
        }
        self.focused = pane;
        self.pending_focus = Some(pane);
        self.pending_close_tab = None;
        self.show_selected_tab(project_id, cx);
        self.sync_pane_chrome(cx);
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    /// Move to the next (+1) or previous (-1) tab, wrapping.
    pub(super) fn switch_tab(&mut self, offset: isize, cx: &mut Context<Self>) {
        let Some(project) = self.selected else {
            return;
        };
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return;
        };
        let count = tabs.tabs.len();
        if count <= 1 {
            return;
        }
        let next = (tabs.selected as isize + offset).rem_euclid(count as isize) as usize;
        self.select_tab(next, cx);
    }

    /// Show tab `index` (0-based) of the selected project.
    pub(super) fn select_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(project) = self.selected else {
            return;
        };
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return;
        };
        if index >= tabs.tabs.len() || tabs.selected == index {
            return;
        }
        self.pending_close_tab = None;
        if let Some(tabs) = self.tabs_by_project.get_mut(&project) {
            tabs.selected = index;
        }
        if let Some(focused) = self.active_tab(project).map(|tab| tab.focused) {
            self.focused = focused;
            self.pending_focus = Some(focused);
        }
        self.show_selected_tab(project, cx);
        self.sync_pane_chrome(cx);
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    pub(super) fn tab_index_by_id(&self, project: i64, tab_id: TabId) -> Option<usize> {
        self.tabs_by_project
            .get(&project)?
            .tabs
            .iter()
            .position(|tab| tab.id == tab_id)
    }

    /// Close request from `⌘W` or the tab's `×`. A tab with several panes asks
    /// first, because closing it ends every terminal in it.
    pub(super) fn request_close_tab(
        &mut self,
        project: i64,
        tab_id: TabId,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(project, tab_id) else {
            return;
        };
        let Some(panes) = self
            .tabs_by_project
            .get(&project)
            .and_then(|tabs| tabs.tabs.get(index))
            .map(|tab| tab.layout.panes().len())
        else {
            return;
        };
        if panes > 1 {
            self.pending_close_tab = Some((project, tab_id));
            cx.notify();
            return;
        }
        self.finish_close_tab(project, index, cx);
    }

    pub(super) fn finish_close_tab_by_id(
        &mut self,
        project: i64,
        tab_id: TabId,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(project, tab_id) else {
            self.pending_close_tab = None;
            cx.notify();
            return;
        };
        self.finish_close_tab(project, index, cx);
    }

    pub(super) fn close_selected_tab(&mut self, cx: &mut Context<Self>) {
        if let Some((project, tab_id)) = self.pending_close_tab {
            self.finish_close_tab_by_id(project, tab_id, cx);
            return;
        }
        let Some(project) = self.selected else {
            return;
        };
        let Some(tab_id) = self
            .tabs_by_project
            .get(&project)
            .and_then(ProjectTabs::active)
            .map(|tab| tab.id)
        else {
            return;
        };
        self.request_close_tab(project, tab_id, cx);
    }

    /// Rename a tab via Pure Rust NSH modal dialog.
    pub(super) fn prompt_rename_tab(
        &mut self,
        project: i64,
        tab_id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(project, tab_id) else {
            return;
        };
        let Some(current) = self
            .tabs_by_project
            .get(&project)
            .map(|tabs| Self::tab_name(tabs, index))
        else {
            return;
        };
        self.open_nsh_dialog(
            DialogSpec::TextInput {
                title: "重命名页签".into(),
                description: None,
                placeholder: "页签名称".into(),
                initial_value: current,
                ok_label: "保存".into(),
                cancel_label: "取消".into(),
                require_non_empty: true,
            },
            window,
            cx,
            move |app, outcome, window, cx| {
                if let DialogOutcome::SubmittedText(name) = outcome
                    && !name.is_empty()
                {
                    if let Some(tabs) = app.tabs_by_project.get_mut(&project)
                        && let Some(tab) = tabs.tabs.iter_mut().find(|tab| tab.id == tab_id)
                    {
                        tab.name = name;
                    }
                    app.persist_workspace(cx);
                }
                app.focus_terminal(window, cx);
                cx.notify();
            },
        );
    }

    /// Remove a tab and end every terminal in it.
    pub(super) fn finish_close_tab(&mut self, project: i64, index: usize, cx: &mut Context<Self>) {
        self.pending_close_tab = None;
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return;
        };
        let Some(tab) = tabs.tabs.get(index) else {
            return;
        };
        let panes = tab.layout.panes();
        let was_selected = tabs.selected == index;
        let leaving_project = tabs.tabs.len() == 1;
        let new_tab = if leaving_project {
            None
        } else if was_selected {
            Some(index.min(tabs.tabs.len() - 2))
        } else if index < tabs.selected {
            Some(tabs.selected - 1)
        } else {
            Some(tabs.selected)
        };
        if leaving_project {
            self.tabs_by_project.remove(&project);
        } else if let Some(tabs) = self.tabs_by_project.get_mut(&project) {
            tabs.tabs.remove(index);
            if let Some(new_tab) = new_tab {
                tabs.selected = new_tab;
            }
        }
        for pane in panes {
            self.workspace.update(cx, |workspace, cx| {
                workspace.drop_terminal(pane.0);
                workspace.unbind_pane_todos(pane.0, cx);
            });
            self.panes.remove(&pane);
            self.native_registry.unregister_pane(pane.0);
            self.launches.remove(&pane);
            self.agent_status.remove(&pane);
        }
        if self.selected == Some(project) {
            if leaving_project {
                self.pending_focus = None;
            } else {
                if let Some(focused) = self.active_tab(project).map(|tab| tab.focused) {
                    self.focused = focused;
                    self.pending_focus = Some(focused);
                }
                self.show_selected_tab(project, cx);
            }
        }
        self.sync_pane_chrome(cx);
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    /// Build the current snapshot for the Pure Rust NSH tab strip.
    pub(super) fn build_tab_bar_snapshot(&self) -> TabBarSnapshot {
        let Some(project) = self.selected else {
            return TabBarSnapshot::Hidden;
        };
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return TabBarSnapshot::Hidden;
        };

        if let Some((pending_project, tab_id)) = self
            .pending_close_tab
            .filter(|(pending_project, _)| *pending_project == project)
            && let Some(index) = tabs.tabs.iter().position(|tab| tab.id == tab_id)
            && let Some(tab) = tabs.tabs.get(index)
        {
            return TabBarSnapshot::ConfirmClose {
                project_id: pending_project,
                tab_id,
                name: Self::tab_name(tabs, index),
                pane_count: tab.layout.panes().len(),
            };
        }

        let items = (0..tabs.tabs.len())
            .map(|index| {
                let tab = &tabs.tabs[index];
                let mut waiting = 0usize;
                let mut failed = 0usize;
                for pane in tab.layout.panes() {
                    match self.agent_status.get(&pane).and_then(|entry| entry.state) {
                        Some(AgentState::Waiting) => waiting += 1,
                        Some(AgentState::Failed) => failed += 1,
                        _ => {}
                    }
                }
                let attention = if waiting > 0 {
                    Some(TabAttention::Waiting)
                } else if failed > 0 {
                    Some(TabAttention::Failed)
                } else {
                    None
                };
                TabItemSnapshot {
                    id: tab.id,
                    name: Self::tab_name(tabs, index),
                    active: index == tabs.selected,
                    attention,
                }
            })
            .collect();

        TabBarSnapshot::Tabs {
            project_id: project,
            selected_tab_id: tabs.active().map(|tab| tab.id),
            tabs: items,
        }
    }

    pub(super) fn handle_tab_bar_action(
        &mut self,
        action: TabBarAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            TabBarAction::Select { project_id, tab_id } => {
                if let Some(idx) = self.tab_index_by_id(project_id, tab_id) {
                    self.select_tab(idx, cx);
                }
                self.focus_terminal(window, cx);
            }
            TabBarAction::RequestClose { project_id, tab_id } => {
                self.request_close_tab(project_id, tab_id, cx);
            }
            TabBarAction::CancelClose => {
                self.pending_close_tab = None;
                self.focus_terminal(window, cx);
                cx.notify();
            }
            TabBarAction::ConfirmClose { project_id, tab_id } => {
                self.finish_close_tab_by_id(project_id, tab_id, cx);
            }
            TabBarAction::Add => {
                self.new_tab(cx);
            }
            TabBarAction::PromptRename { project_id, tab_id } => {
                self.prompt_rename_tab(project_id, tab_id, window, cx);
            }
        }
    }
}
