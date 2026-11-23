//! Workspace startup, restoration, pane prestarting, and persistence.

use std::collections::{HashMap, HashSet};

use crate::agent::launch::PaneLaunch;
use crate::app::Workspace;
use crate::app::layout::{
    self, PaneId, PaneNode, ProjectTabs, SavedProjectTab, SavedProjectTabs, SavedWorkspace,
    WORKSPACE_STATE_VERSION,
};
use gpui::*;

use super::{AgentEntry, KumuApp, TerminalView};

impl KumuApp {
    /// Show the first project in a single tab — the first run, and the
    /// fallback whenever a saved layout cannot be used.
    pub(super) fn start_default(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.workspace.read(cx).projects().first().map(|p| p.id) else {
            return;
        };
        self.ensure_project_layout(project, cx);
        self.selected = Some(project);
        self.focused = self
            .active_tab(project)
            .map(|tab| tab.focused)
            .unwrap_or(PaneId(1));
        self.show_selected_tab(project, cx);
    }

    /// Recreate the saved pane tree and terminals, or return false so the
    /// caller falls back to [`Self::start_default`]. A layout is accepted only
    /// when its JSON and version are known and every pane still has a project:
    /// a stale row, a corrupt document, or an absurd pane count must never
    /// prevent the window from opening.
    pub(super) fn restore_workspace(
        &mut self,
        saved: Option<&str>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(saved) = saved else {
            return false;
        };
        let workspace = self.workspace.clone();
        let Some(restored) = SavedWorkspace::restore(saved, |project| {
            workspace
                .read(cx)
                .projects()
                .iter()
                .any(|p| p.id == project)
        }) else {
            return false;
        };

        let layout::RestoredWorkspace {
            tabs,
            selected,
            group_sizes,
            launches,
            next_pane,
            next_group,
        } = restored;
        self.tabs_by_project = tabs;
        self.launches = launches;
        self.agent_status.clear();
        for (pane, launch) in &self.launches {
            if let PaneLaunch::Agent { agent, .. } = launch {
                self.agent_status
                    .insert(*pane, AgentEntry::idle(agent.clone()));
            }
        }
        if !self.auto_resume {
            // The panes still come back as agent panes; only the conversations
            // stay behind.
            for launch in self.launches.values_mut() {
                *launch = launch.clone().fresh();
            }
        }
        self.selected = selected;
        self.next_pane = next_pane;
        self.next_group = next_group;
        self.group_ratios.clear();
        for (group, ratios) in group_sizes {
            self.group_ratios.insert(group, ratios);
        }
        self.focused = self
            .selected
            .and_then(|project| self.active_tab(project))
            .map(|tab| tab.focused)
            .unwrap_or(PaneId(1));
        if let Some(project) = self.selected {
            self.show_selected_tab(project, cx);
        }
        self.queue_pane_restores(cx);
        true
    }

    /// Start every pane in the other projects' trees at startup, one spawn at
    /// a time, so switching to a project finds its panes already running.
    ///
    /// The live-terminal cap still applies: when it is reached the queue
    /// stops, and the remaining panes start lazily when their project is
    /// selected.
    fn queue_pane_restores(&mut self, cx: &mut Context<Self>) {
        let order: Vec<i64> = self
            .workspace
            .read(cx)
            .projects()
            .iter()
            .map(|project| project.id)
            .collect();
        // Agents first: their conversations already exist and a warm pane is
        // what the user came back for. A shell only needs to avoid a cold
        // start, so it must not take the last slot from an agent when the live
        // cap is reached.
        let mut agents: Vec<(u64, i64, PaneLaunch)> = Vec::new();
        let mut shells: Vec<(u64, i64, PaneLaunch)> = Vec::new();
        for project in order {
            let Some(tabs) = self.tabs_by_project.get(&project) else {
                continue;
            };
            for (index, tab) in tabs.tabs.iter().enumerate() {
                if Some(project) == self.selected && index == tabs.selected {
                    continue;
                }
                for pane in tab.layout.panes() {
                    let launch = self.launches.get(&pane).cloned().unwrap_or_default();
                    if launch.is_shell() {
                        shells.push((pane.0, project, launch));
                    } else {
                        agents.push((pane.0, project, launch));
                    }
                }
            }
        }
        if agents.is_empty() && shells.is_empty() {
            return;
        }
        let queue: Vec<(u64, i64, PaneLaunch)> = agents.into_iter().chain(shells).collect();
        cx.spawn(async move |this, cx| {
            for (pane, project, launch) in queue {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
                let started = this
                    .update(cx, |home, cx| {
                        home.prestart_pane(pane, project, &launch, cx)
                    })
                    .unwrap_or(false);
                if !started {
                    break;
                }
            }
        })
        .detach();
    }

    /// Start one queued pane unless it changed or disappeared while waiting.
    /// A skipped pane keeps the queue going; only a full cap stops it.
    fn prestart_pane(
        &mut self,
        pane: u64,
        project: i64,
        launch: &PaneLaunch,
        cx: &mut Context<Self>,
    ) -> bool {
        // Absent from `launches` means a shell, so compare against the
        // default instead of requiring an entry a shell never has.
        let current = self
            .launches
            .get(&PaneId(pane))
            .cloned()
            .unwrap_or_default();
        if &current != launch {
            return true;
        }
        self.workspace.update(cx, |workspace, cx| {
            workspace.prestart(pane, project, launch, cx)
        })
    }

    /// Create and subscribe one pane's terminal view.
    pub(super) fn install_pane(
        &mut self,
        pane: PaneId,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        let workspace = self.workspace.clone();
        let terminal = cx.new(|cx| TerminalView::new(pane, workspace, cx));
        cx.subscribe(&terminal, move |this, _, ev, cx| {
            this.on_terminal_event(pane, ev, cx);
        })
        .detach();
        self.native_registry.register_pane(pane.0, terminal.clone());
        self.panes.insert(pane, terminal.clone());
        terminal
    }

    /// Forget launches borrowed from an agent that is no longer running.
    ///
    /// A pane started as an agent reports its own exit and turns back into a
    /// shell (see [`crate::app::WorkspaceEvent::AgentExited`]). A shell pane
    /// only borrowed the launch from the conversation inside it, and nothing
    /// reports that agent leaving, so the process tree decides at quit. When
    /// it cannot answer, the launch is kept rather than risk dropping a live
    /// conversation.
    pub(super) fn release_finished_agent_panes(&mut self, cx: &mut Context<Self>) {
        let workspace = self.workspace.read(cx);
        let needs_check = self.launches.iter().any(|(pane, launch)| {
            !launch.is_shell() && workspace.pane_is_shell(pane.0) == Some(true)
        });
        if !needs_check {
            return;
        }
        let Some(ps) = Workspace::process_tree_snapshot() else {
            return;
        };
        let finished: Vec<PaneId> = self
            .launches
            .iter()
            .filter_map(|(pane, launch)| {
                let PaneLaunch::Agent { agent, .. } = launch else {
                    return None;
                };
                let program = crate::agent::executable(agent)?;
                let program = std::path::Path::new(&program)
                    .file_name()?
                    .to_string_lossy()
                    .into_owned();
                (workspace.pane_runs_program_in(pane.0, &program, &ps) == Some(false))
                    .then_some(*pane)
            })
            .collect();
        for pane in finished {
            self.launches.remove(&pane);
        }
    }

    /// Write each project's tabs and the focused panes for the next launch.
    /// Identical state is not rewritten, so selecting or clicking around does
    /// not turn into a stream of writes.
    pub(super) fn persist_workspace(&mut self, cx: &mut Context<Self>) {
        // Group ids are unique across every tab and stale groups can be
        // dropped once their owning tree no longer contains them.
        let live_groups: HashSet<u64> = self
            .tabs_by_project
            .values()
            .flat_map(|tabs| &tabs.tabs)
            .flat_map(|tab| PaneNode::group_ids(&tab.layout))
            .collect();
        self.group_ratios
            .retain(|group, _| live_groups.contains(group));

        let ratios: HashMap<u64, Vec<f32>> = self
            .group_ratios
            .iter()
            .filter_map(|(group, ratios)| {
                let total: f32 = ratios.iter().sum();
                if ratios.is_empty() || !total.is_finite() || total <= 0. {
                    return None;
                }
                let normalized: Vec<f32> = ratios.iter().map(|r| r / total).collect();
                normalized
                    .iter()
                    .all(|ratio| ratio.is_finite() && *ratio > 0.)
                    .then_some((*group, normalized))
            })
            .collect();
        let live_panes: HashSet<PaneId> = self
            .tabs_by_project
            .values()
            .flat_map(ProjectTabs::panes)
            .collect();
        self.launches.retain(|pane, _| live_panes.contains(pane));
        let mut projects: Vec<SavedProjectTabs> = self
            .tabs_by_project
            .iter()
            .filter_map(|(project, project_tabs)| {
                let saved_tabs: Vec<SavedProjectTab> = project_tabs
                    .tabs
                    .iter()
                    .filter_map(|tab| {
                        let panes = tab.layout.panes();
                        let focused = panes
                            .contains(&tab.focused)
                            .then_some(tab.focused)
                            .or_else(|| panes.first().copied())?;
                        Some(SavedProjectTab {
                            name: tab.name.clone(),
                            layout: tab.layout.to_saved(&ratios),
                            focused: focused.0,
                        })
                    })
                    .collect();
                if saved_tabs.is_empty() {
                    return None;
                }
                Some(SavedProjectTabs {
                    project: *project,
                    selected_tab: project_tabs.selected.min(saved_tabs.len() - 1),
                    tabs: saved_tabs,
                })
            })
            .collect();
        projects.sort_by_key(|project| project.project);
        let state = SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: layout::SavedNode::ProjectTabs {
                selected: self.selected,
                projects,
                launches: self
                    .launches
                    .iter()
                    .filter(|(_, launch)| !launch.is_shell())
                    .map(|(pane, launch)| (pane.0, launch.clone()))
                    .collect(),
            },
            focused: self.focused.0,
        };
        let Ok(json) = serde_json::to_string(&state) else {
            return;
        };
        if self.last_saved_layout.as_deref() == Some(json.as_str()) {
            return;
        }
        if self.workspace.update(cx, |workspace, cx| {
            workspace.save_workspace_state(&json, cx)
        }) {
            self.last_saved_layout = Some(json);
        }
    }
}
