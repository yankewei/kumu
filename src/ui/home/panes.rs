//! Pane focus, splitting, closing, chrome sync, project selection, and terminal reveal.

use std::collections::HashSet;

use crate::agent::launch::PaneLaunch;
use crate::app::layout::{PaneDirection, PaneId, PaneNode, ProjectTab, ProjectTabs, SplitAxis};
use crate::nsh_ui::agent_picker::{
    ActiveAgentPicker, AgentPickerAnchor, AgentPickerItem, AgentPickerOutcome,
};
use crate::theme::KumuTheme;
use gpui::*;

use super::{AgentEntry, KumuApp};

impl KumuApp {
    /// The project that owns `pane`, including panes in hidden tabs.
    pub(super) fn pane_project(&self, pane: PaneId, cx: &Context<Self>) -> Option<i64> {
        self.pane_tab(pane).map(|(project, _)| project).or_else(|| {
            self.panes
                .get(&pane)
                .and_then(|view| view.read(cx).project_id())
        })
    }

    /// Move keyboard focus to the pane next to `pane` in `direction`.
    pub(super) fn focus_neighbor(
        &mut self,
        pane: PaneId,
        direction: PaneDirection,
        cx: &mut Context<Self>,
    ) {
        let Some(next) = self.pane_tab(pane).and_then(|(project, tab_index)| {
            self.tabs_by_project
                .get(&project)
                .and_then(|tabs| tabs.tabs.get(tab_index))
                .and_then(|tab| tab.layout.neighbor_in(pane, direction))
        }) else {
            return;
        };
        self.focus_pane_from_user(next, cx);
    }

    /// Focus a pane from a user gesture. Looking at the pane acknowledges a
    /// status change that arrived while the user was elsewhere.
    pub(super) fn focus_pane_from_user(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        self.acknowledge_pane(pane, cx);
        self.focus_pane(pane, cx);
    }

    /// Move focus without treating it as the user looking at the pane. Control
    /// and auto-close paths use this so they cannot clear an unseen marker.
    pub(super) fn focus_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let Some((project, tab_index)) = self.pane_tab(pane) else {
            return;
        };
        let project_changed = self.selected != Some(project);
        if project_changed {
            if let Some(previous) = self.selected {
                self.hide_project_panes(previous, cx);
            }
            self.selected = Some(project);
        }
        let previous_tab = self.tabs_by_project.get(&project).map(|tabs| tabs.selected);
        let needs_view = !self.panes.contains_key(&pane);
        if previous_tab != Some(tab_index)
            && let Some(tabs) = self.tabs_by_project.get_mut(&project)
        {
            tabs.selected = tab_index;
        }
        if let Some(tab) = self.active_tab_mut(project) {
            tab.focused = pane;
        }
        if project_changed || previous_tab != Some(tab_index) || needs_view {
            self.show_selected_tab(project, cx);
        }
        if self.zoomed_pane != Some(pane) {
            self.zoomed_pane = None;
        }
        self.focused = pane;
        self.pending_focus = Some(pane);
        self.pending_close_tab = None;
        self.sync_pane_chrome(cx);
        // Focusing a pane can switch the selected project without going
        // through `select_project` (a notification for another project's
        // shell, say), so open panels are told here too.
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    pub(super) fn focus_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(modal) = &self.active_todo_modal {
            modal.app_view.read(cx).focus_handle(cx).focus(window, cx);
            return;
        }
        if self.active_todo_board.is_some() {
            self.shell_nsh
                .app_view
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
            return;
        }
        if self
            .selected
            .and_then(|project| self.active_tab(project))
            .is_some_and(|tab| tab.layout.contains(self.focused))
            && let Some(terminal) = self.panes.get(&self.focused).cloned()
        {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
            return;
        }
        // Empty workspace / empty project: keep focus on the home window root
        // so global shortcuts (`⌘K`, `⌘B`, `⌘,`, `⌘T`) still fire.
        self.window_focus.focus(window, cx);
    }

    pub(super) fn select_project(&mut self, id: i64, cx: &mut Context<Self>) {
        self.pending_close_tab = None;
        self.zoomed_pane = None;
        if self.selected != Some(id)
            && let Some(previous) = self.selected
        {
            self.hide_project_panes(previous, cx);
        }
        self.selected = Some(id);
        self.focused = self
            .active_tab(id)
            .map(|tab| tab.focused)
            .unwrap_or(PaneId(1));
        self.pending_focus = self.active_tab(id).map(|_| self.focused);
        self.show_selected_tab(id, cx);
        self.acknowledge_visible_tab(id, cx);
        self.sync_pane_chrome(cx);
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    pub(super) fn ensure_project_layout(&mut self, project: i64, cx: &mut Context<Self>) {
        if self.tabs_by_project.contains_key(&project) {
            return;
        }
        let pane = PaneId(self.next_pane);
        self.next_pane += 1;
        self.tabs_by_project.insert(
            project,
            ProjectTabs {
                tabs: vec![ProjectTab::new(String::new(), PaneNode::leaf(pane), pane)],
                selected: 0,
            },
        );
        self.install_pane(pane, cx);
    }

    pub(super) fn hide_project_panes(&mut self, project: i64, cx: &mut Context<Self>) {
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return;
        };
        for pane in tabs.panes() {
            if let Some(terminal) = self.panes.get(&pane).cloned() {
                terminal.update(cx, |terminal, cx| terminal.hide(cx));
            }
        }
    }

    /// Show the anchored agent picker for `pane`. The list is resolved here,
    /// on the UI thread, so the NSH engine never reads the filesystem.
    pub(super) fn open_agent_picker(
        &mut self,
        pane: PaneId,
        anchor: AgentPickerAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_agent_picker.is_some() {
            return;
        }
        let items: Vec<AgentPickerItem> = crate::agent::AGENTS
            .iter()
            .filter(|agent| crate::agent::installed(agent.id))
            .map(|agent| AgentPickerItem {
                id: agent.id.to_owned(),
                name: agent.name.to_owned(),
                logo: agent.logo,
            })
            .collect();
        let theme = KumuTheme::global(cx).clone();
        self.active_agent_picker = Some(ActiveAgentPicker::open(
            anchor,
            items,
            &theme,
            window,
            cx,
            move |this, outcome, window, cx| {
                this.active_agent_picker = None;
                match outcome {
                    AgentPickerOutcome::Selected(agent) => this.split_pane_with(
                        pane,
                        SplitAxis::Horizontal,
                        PaneLaunch::Agent {
                            agent,
                            session: None,
                        },
                        cx,
                    ),
                    AgentPickerOutcome::Dismissed => this.focus_terminal(window, cx),
                }
                cx.notify();
            },
        ));
        cx.notify();
    }

    /// Split `pane` along `axis`, open another shell in the same tab, and
    /// focus the new pane.
    pub(super) fn split_pane(&mut self, pane: PaneId, axis: SplitAxis, cx: &mut Context<Self>) {
        self.split_pane_with(pane, axis, PaneLaunch::Shell, cx);
    }

    /// Split `pane` and run `launch` in the new pane.
    pub(super) fn split_pane_with(
        &mut self,
        pane: PaneId,
        axis: SplitAxis,
        launch: PaneLaunch,
        cx: &mut Context<Self>,
    ) {
        let Some((project_id, tab_index)) = self.pane_tab(pane) else {
            return;
        };
        // Splitting from a hidden tab (or a pane with no view yet) is
        // meaningful for the control socket: bring it on screen first.
        let hidden = self.selected != Some(project_id)
            || self
                .tabs_by_project
                .get(&project_id)
                .is_some_and(|tabs| tabs.selected != tab_index)
            || !self.panes.contains_key(&pane);
        if hidden {
            self.focus_pane(pane, cx);
        }
        let new_id = PaneId(self.next_pane);
        let group_id = self.next_group;
        let Some(tab) = self
            .tabs_by_project
            .get_mut(&project_id)
            .and_then(|tabs| tabs.tabs.get_mut(tab_index))
        else {
            return;
        };
        if !tab.layout.split(pane, axis, new_id, group_id) {
            return;
        }
        let made_group = tab.layout.contains_group(group_id);
        tab.focused = new_id;
        self.next_pane += 1;
        self.next_group += 1;

        if made_group {
            self.group_ratios.remove(&group_id);
        }
        if launch.is_shell() {
            self.launches.remove(&new_id);
        } else {
            if let PaneLaunch::Agent { agent, .. } = &launch {
                self.agent_status
                    .insert(new_id, AgentEntry::idle(agent.clone()));
            }
            self.launches.insert(new_id, launch.clone());
        }
        let terminal = self.install_pane(new_id, cx);
        terminal.update(cx, |terminal, cx| terminal.show(project_id, &launch, cx));
        self.zoomed_pane = None;
        self.focused = new_id;
        self.selected = Some(project_id);
        self.pending_focus = Some(new_id);
        self.sync_pane_chrome(cx);
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    /// Remove a pane after a close action or shell exit. Closing the last pane
    /// of a tab removes the tab; the project keeps its other tabs.
    pub(super) fn close_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let Some((project_id, tab_index)) = self.pane_tab(pane) else {
            return;
        };
        let Some(tab) = self
            .tabs_by_project
            .get(&project_id)
            .and_then(|tabs| tabs.tabs.get(tab_index))
        else {
            return;
        };
        if tab.layout.panes().len() <= 1 {
            self.finish_close_tab(project_id, tab_index, cx);
            return;
        }
        let next = tab.layout.neighbor(pane);
        let focused_here = tab.focused == pane;
        let shown = self.selected == Some(project_id)
            && self
                .tabs_by_project
                .get(&project_id)
                .is_some_and(|tabs| tabs.selected == tab_index);
        if let Some(tab) = self
            .tabs_by_project
            .get_mut(&project_id)
            .and_then(|tabs| tabs.tabs.get_mut(tab_index))
        {
            if !tab.layout.remove(pane) {
                return;
            }
            if focused_here && let Some(next) = next {
                tab.focused = next;
            }
        }
        if shown
            && focused_here
            && let Some(focused) = self.active_tab(project_id).map(|tab| tab.focused)
        {
            self.focused = focused;
            self.pending_focus = Some(focused);
        }
        self.workspace.update(cx, |workspace, cx| {
            workspace.drop_terminal(pane.0);
            workspace.unbind_pane_todos(pane.0, cx);
        });
        self.panes.remove(&pane);
        self.native_registry.unregister_pane(pane.0);
        self.launches.remove(&pane);
        self.agent_status.remove(&pane);
        if self.zoomed_pane == Some(pane)
            || self
                .selected
                .and_then(|project| self.active_tab(project))
                .is_none_or(|tab| tab.layout.panes().len() <= 1)
        {
            self.zoomed_pane = None;
        }
        self.sync_pane_chrome(cx);
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    pub(super) fn sync_pane_chrome(&mut self, cx: &mut Context<Self>) {
        let visible: HashSet<PaneId> = self
            .selected
            .and_then(|project_id| self.active_tab(project_id))
            .map(|tab| tab.layout.panes())
            .unwrap_or_default()
            .into_iter()
            .collect();
        let can_zoom_tab = visible.len() > 1;
        let zoomed_pane = self.active_zoomed_pane();
        let focused = self.focused;
        for (id, terminal) in &self.panes {
            let shown = visible.contains(id);
            let active = shown && *id == focused;
            let can_zoom = shown && can_zoom_tab;
            let zoomed = zoomed_pane == Some(*id);
            terminal.update(cx, |terminal, cx| {
                terminal.set_chrome(active, shown, can_zoom, zoomed, cx);
            });
        }
    }

    /// Open another shell in `project_id`, or in the selected project, or in
    /// the first one. A project with no pane gets its first; one that already
    /// has panes gains a split beside the focused pane.
    pub(super) fn add_terminal(&mut self, project_id: Option<i64>, cx: &mut Context<Self>) {
        let project_id = project_id.or(self.selected).or_else(|| {
            self.workspace
                .update(cx, |workspace, cx| workspace.ensure_project(cx))
        });
        let Some(project_id) = project_id else {
            return;
        };
        let had_panes = self
            .active_tab(project_id)
            .is_some_and(|tab| !tab.layout.panes().is_empty());
        if had_panes {
            self.select_project(project_id, cx);
            self.split_pane(self.focused, SplitAxis::Horizontal, cx);
        } else {
            self.ensure_project_layout(project_id, cx);
            self.select_project(project_id, cx);
        }
    }

    pub(super) fn reveal_terminal_by_id(
        &mut self,
        terminal: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self
            .workspace
            .update(cx, |workspace, _| workspace.reveal_terminal(terminal));
        if let Some((pane, project)) = target {
            let pane = PaneId(pane);
            let snapshot = self.workspace.read(cx).terminal_snapshot(pane.0);
            if let Some(view) = self.panes.get(&pane)
                && let Some(snapshot) = snapshot
            {
                view.update(cx, |view, cx| view.attach(project, snapshot, cx));
            }
            self.focus_pane_from_user(pane, cx);
            self.focus_terminal(window, cx);
        }
    }
}
