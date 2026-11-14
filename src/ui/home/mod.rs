//! Home window: projects on the left, the selected project's splitable pane
//! tree on the right. Owns the settings window handle.
//!
//! A project owns its panes. Splitting adds another shell to that project.

mod panes;
mod sidebar;
mod sprites;
mod terminal;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::SettingsWindow;
use crate::agent::launch::{PaneLaunch, session_launch};
use crate::app::{NotificationSettings, Workspace, WorkspaceEvent};
use crate::git::WorktreeEntry;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::{Notification, NotificationDelivery};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;
use gpui_updater::Updater;
use panes::{
    PaneDirection, PaneId, PaneNode, SavedProjectLayout, SavedWorkspace, WORKSPACE_STATE_VERSION,
};
use terminal::{TerminalEvent, TerminalView};

/// Whether panes come back with the conversation they were running.
fn auto_resume_enabled() -> bool {
    crate::db::get_setting(crate::db::SETTING_AUTO_RESUME).as_deref() != Some("0")
}

/// Arbitrary units fed to `ResizableState::insert_panel` when restoring split
/// ratios. Only the relative values matter: the group rescales them to the
/// container's real size on its first paint.
const RATIO_SCALE: f32 = 10_000.;

/// The sidebar's opening width and the range the divider beside it lets the
/// user drag it through. The maximum still leaves room for the panes on the
/// app's minimum window width.
const SIDEBAR_WIDTH: f32 = 256.;
const SIDEBAR_MIN_WIDTH: f32 = 180.;
const SIDEBAR_MAX_WIDTH: f32 = 520.;

pub struct KumuApp {
    /// Projects and their live shells; the home view draws it and hands it
    /// every user action.
    workspace: Entity<Workspace>,
    /// Owned by the home window so closing Settings does not interrupt an update.
    updater: Entity<Updater>,
    settings_window: Option<AnyWindowHandle>,
    /// Project currently shown in the content area.
    selected: Option<i64>,
    /// Terminals that requested attention while unattended, mapped to their
    /// project for the sidebar marker.
    unread_notifications: HashMap<u64, i64>,
    /// A persistence failure the workspace reported, shown above the panes
    /// until dismissed.
    store_error: Option<String>,
    /// Terminal views keyed by pane id.
    panes: HashMap<PaneId, Entity<TerminalView>>,
    /// Each project owns an independent pane tree.
    layouts: HashMap<i64, PaneNode>,
    /// Last focused pane in each project's tree.
    focused_by_project: HashMap<i64, PaneId>,
    /// Resizable state per split group, so drag sizes can be saved and restored.
    group_states: HashMap<u64, Entity<ResizableState>>,
    /// Focused pane in the selected project.
    focused: PaneId,
    /// Focus this pane's terminal on the next paint (after a split / select).
    pending_focus: Option<PaneId>,
    next_pane: u64,
    next_group: u64,
    /// What each pane runs. Absent panes are shells, so a workspace restored
    /// from before agent panes existed needs no entries.
    launches: HashMap<PaneId, PaneLaunch>,
    /// Whether panes come back with their old conversations. Read once at
    /// startup; the settings window's switch applies on the next launch.
    auto_resume: bool,
    /// The JSON last written for this layout, so repeated focus/select calls do
    /// not rewrite identical state.
    last_saved_layout: Option<String>,
    /// Sidebar width in px, restored from the last drag of the divider.
    sidebar_width: f32,
}

impl KumuApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = cx.new(Workspace::new);
        let startup_error = workspace.update(cx, |workspace, _| workspace.take_startup_error());
        let saved_workspace = workspace.update(cx, |workspace, _| workspace.take_saved_workspace());
        let last_saved_layout = saved_workspace.clone();
        // Only what the home view itself draws. A terminal frame is the pane's
        // own, subscribed inside [`TerminalView`].
        cx.subscribe_in(&workspace, window, |this, _, ev, window, cx| match ev {
            WorkspaceEvent::Changed => cx.notify(),
            WorkspaceEvent::Failed(message) => {
                this.store_error = Some(message.clone());
                cx.notify();
            }
            WorkspaceEvent::Attention {
                terminal,
                project,
                notification,
            } => {
                if !cx.global::<NotificationSettings>().enabled
                    || this.workspace.read(cx).notification_is_read(
                        *terminal,
                        window.is_window_active().then_some(this.focused.0),
                    )
                {
                    return;
                }
                let workspace = this.workspace.read(cx);
                let Some(project_info) = workspace.project(*project) else {
                    return;
                };
                if cx.global::<NotificationSettings>().sound {
                    window.play_system_bell();
                }
                let title = match workspace.terminal_label(*terminal) {
                    label if label.is_empty() => project_info.name.clone(),
                    label => format!("{} · {}", project_info.name, label),
                };
                let message = match (notification.title.is_empty(), notification.body.is_empty()) {
                    (true, true) => "终端需要你的关注".to_string(),
                    (false, true) => notification.title.clone(),
                    (true, false) => notification.body.clone(),
                    (false, false) => format!("{}\n{}", notification.title, notification.body),
                };
                this.unread_notifications.insert(*terminal, *project);
                let terminal = *terminal;
                let weak = cx.entity().downgrade();
                let note = Notification::info(message)
                    .title(title)
                    .id1::<NotificationSettings>(terminal.to_string())
                    .delivery(if window.is_window_active() {
                        NotificationDelivery::InApp
                    } else {
                        NotificationDelivery::System
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            let target = this
                                .workspace
                                .update(cx, |workspace, _| workspace.reveal_terminal(terminal));
                            if let Some((pane, project)) = target {
                                let pane = PaneId(pane);
                                let snapshot = this.workspace.read(cx).terminal_snapshot(pane.0);
                                if let Some(view) = this.panes.get(&pane)
                                    && let Some(snapshot) = snapshot
                                {
                                    view.update(cx, |view, cx| view.attach(project, snapshot, cx));
                                }
                                this.focus_pane(pane, cx);
                                this.focus_terminal(window, cx);
                            }
                        });
                    });
                // A background window posts only to Notification Center: one
                // event should not surface as both a banner and a toast. The
                // sidebar's unread marker is the in-app fallback when macOS
                // declines delivery or kumu runs unbundled.
                window.push_notification(note, cx);
                cx.notify();
            }
            WorkspaceEvent::PaneExited { pane, project } => {
                let pane = PaneId(*pane);
                if this
                    .panes
                    .get(&pane)
                    .and_then(|view| view.read(cx).project_id())
                    == Some(*project)
                {
                    this.close_pane(pane, cx);
                }
            }
            WorkspaceEvent::AgentExited {
                pane,
                project,
                resume_failed,
            } => {
                // The agent is gone but the pane stays. A resume that failed
                // immediately gets one fresh conversation; anything else
                // becomes a shell, and its conversation is not resumed again.
                let pane = PaneId(*pane);
                // A prestarted pane has no view yet; its event is still the
                // truth. Only a view that now shows another project is stale.
                if let Some(view) = this.panes.get(&pane)
                    && view.read(cx).project_id() != Some(*project)
                {
                    return;
                }
                let next = match (resume_failed, this.launches.get(&pane).cloned()) {
                    (true, Some(PaneLaunch::Agent { agent, .. })) => PaneLaunch::Agent {
                        agent,
                        session: None,
                    },
                    _ => PaneLaunch::Shell,
                };
                if next.is_shell() {
                    this.launches.remove(&pane);
                } else {
                    this.launches.insert(pane, next.clone());
                }
                // Only the selected project's panes are on screen. Restarting
                // a hidden one would spend a process nobody can see; it starts
                // when the project is selected.
                if this.selected == Some(*project)
                    && let Some(terminal) = this.panes.get(&pane).cloned()
                {
                    terminal.update(cx, |terminal, cx| {
                        terminal.hide(cx);
                        terminal.show(*project, &next, cx);
                    });
                }
                this.persist_workspace(cx);
                cx.notify();
            }
            WorkspaceEvent::Session {
                pane,
                agent,
                session,
            } => {
                let Some(agent) = crate::agent::id_for(agent) else {
                    return;
                };
                let pane = PaneId(*pane);
                // A stale process from before the pane was repurposed must not
                // overwrite the spec of the agent that runs in it now. Only a
                // shell pane may switch agents: that is the user starting a
                // different one in the same shell.
                let shell_pane = this
                    .workspace
                    .read(cx)
                    .pane_is_shell(pane.0)
                    .unwrap_or(false);
                let Some(launch) = session_launch(
                    this.launches.get(&pane),
                    agent,
                    session,
                    this.auto_resume,
                    shell_pane,
                ) else {
                    return;
                };
                if this.launches.get(&pane) == Some(&launch) {
                    return;
                }
                this.launches.insert(pane, launch);
                this.persist_workspace(cx);
            }
            _ => {}
        })
        .detach();

        cx.observe_global::<NotificationSettings>(|_, cx| cx.notify())
            .detach();
        cx.observe_window_activation(window, |_, _, cx| cx.notify())
            .detach();

        let updater = crate::updates::init(cx);
        cx.observe(&updater, |_, _, cx| cx.notify()).detach();

        // A width saved by an older launch, falling back to the default when
        // the setting is missing, unreadable, or outside the draggable range.
        let sidebar_width = crate::db::get_setting(crate::db::SETTING_SIDEBAR_WIDTH)
            .and_then(|value| value.trim().parse::<f32>().ok())
            .filter(|width| width.is_finite())
            .map(|width| width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH))
            .unwrap_or(SIDEBAR_WIDTH);

        // Save the window's geometry when it closes. The app-quit path does not
        // hand the callback a window, so it reads the same bounds through the
        // handle; either path can fire first and they write the same value.
        // Closing the window also ends the panes' processes, so this is the
        // last moment the process tree can say which adopted agents were real.
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            crate::window_state::save(window);
            let _ = weak.update(cx, |app, cx| {
                app.release_finished_agent_panes(cx);
                app.persist_workspace(cx);
            });
            true
        });
        let window_handle = window.window_handle();
        cx.on_app_quit(move |app, cx| {
            // The last word on the layout: a quit is also the moment a pane's
            // launch or a split ratio is most likely to differ from what was
            // last written, and a failed earlier write gets one more chance
            // before the process is gone.
            app.release_finished_agent_panes(cx);
            app.persist_workspace(cx);
            if let Ok(bounds) = window_handle.update(cx, |_, window, _| window.window_bounds()) {
                crate::window_state::save_bounds(bounds);
            }
            async {}
        })
        .detach();

        let mut app = Self {
            workspace,
            updater,
            settings_window: None,
            selected: None,
            unread_notifications: HashMap::new(),
            store_error: startup_error,
            panes: HashMap::new(),
            layouts: HashMap::new(),
            focused_by_project: HashMap::new(),
            group_states: HashMap::new(),
            focused: PaneId(1),
            pending_focus: None,
            next_pane: 1,
            next_group: 1,
            launches: HashMap::new(),
            auto_resume: auto_resume_enabled(),
            last_saved_layout,
            sidebar_width,
        };
        if !app.restore_workspace(saved_workspace.as_deref(), cx) {
            app.start_default(cx);
        }
        app.focus_terminal(window, cx);
        app.sync_pane_chrome(cx);
        app.persist_workspace(cx);
        app
    }

    /// Show the first project in a single pane — the first run, and the
    /// fallback whenever a saved layout cannot be used.
    fn start_default(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.workspace.read(cx).projects().first().map(|p| p.id) else {
            return;
        };
        self.ensure_project_layout(project, cx);
        self.selected = Some(project);
        self.focused = self
            .focused_by_project
            .get(&project)
            .copied()
            .unwrap_or(PaneId(1));
        self.show_project_panes(project, cx);
    }

    /// Recreate the saved pane tree and terminals, or return false so the
    /// caller falls back to [`Self::start_default`]. A layout is accepted only
    /// when its JSON and version are known and every pane still has a project:
    /// a stale row, a corrupt document, or an absurd pane count must never
    /// prevent the window from opening.
    fn restore_workspace(&mut self, saved: Option<&str>, cx: &mut Context<Self>) -> bool {
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

        let panes::RestoredWorkspace {
            layouts,
            focused_by_project,
            selected,
            group_sizes,
            launches,
            next_pane,
            next_group,
        } = restored;
        self.layouts = layouts;
        self.launches = launches;
        if !self.auto_resume {
            // The panes still come back as agent panes; only the conversations
            // stay behind.
            for launch in self.launches.values_mut() {
                *launch = launch.clone().fresh();
            }
        }
        self.focused_by_project = focused_by_project;
        self.selected = selected;
        self.next_pane = next_pane;
        self.next_group = next_group;
        self.group_states.clear();
        for (group, ratios) in group_sizes {
            let state = Self::new_group_state(&ratios, cx);
            self.group_states.insert(group, state);
        }
        if let Some(project) = self.selected {
            self.show_project_panes(project, cx);
        }
        self.focused = self
            .selected
            .and_then(|project| self.focused_by_project.get(&project).copied())
            .unwrap_or(PaneId(1));
        self.queue_pane_restores(cx);
        true
    }

    /// Materialize and attach every pane in the selected project. Other
    /// projects keep their saved trees and create their views when selected.
    fn show_project_panes(&mut self, project: i64, cx: &mut Context<Self>) {
        let Some(layout) = self.layouts.get(&project) else {
            return;
        };
        let pane_ids = layout.panes();
        for pane in pane_ids {
            let terminal = if let Some(terminal) = self.panes.get(&pane).cloned() {
                terminal
            } else {
                self.install_pane(pane, cx)
            };
            let launch = self.launches.get(&pane).cloned().unwrap_or_default();
            terminal.update(cx, |terminal, cx| terminal.show(project, &launch, cx));
        }
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
            if Some(project) == self.selected {
                continue;
            }
            let Some(layout) = self.layouts.get(&project) else {
                continue;
            };
            for pane in layout.panes() {
                let launch = self.launches.get(&pane).cloned().unwrap_or_default();
                if launch.is_shell() {
                    shells.push((pane.0, project, launch));
                } else {
                    agents.push((pane.0, project, launch));
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
    fn install_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) -> Entity<TerminalView> {
        let workspace = self.workspace.clone();
        let terminal = cx.new(|cx| TerminalView::new(pane, workspace, cx));
        cx.subscribe(&terminal, move |this, _, ev, cx| {
            this.on_terminal_event(pane, ev, cx);
        })
        .detach();
        self.panes.insert(pane, terminal.clone());
        terminal
    }

    /// A resizable state for one split group, optionally seeded with ratios.
    /// Seeding before the first paint is what makes `adopt_sizes` — which is
    /// private upstream — unnecessary: the group rescales these values to the
    /// container on its first prepaint.
    fn new_group_state(ratios: &[f32], cx: &mut Context<Self>) -> Entity<ResizableState> {
        let state = cx.new(|_| ResizableState::default());
        if !ratios.is_empty() {
            state.update(cx, |state, cx| {
                for ratio in ratios {
                    state.insert_panel(Some(px(ratio * RATIO_SCALE)), None, cx);
                }
            });
        }
        state
    }

    /// Forget launches borrowed from an agent that is no longer running.
    ///
    /// A pane started as an agent reports its own exit and turns back into a
    /// shell (see [`WorkspaceEvent::AgentExited`]). A shell pane only borrowed
    /// the launch from the conversation inside it, and nothing reports that
    /// agent leaving, so the process tree decides at quit. When it cannot
    /// answer, the launch is kept rather than risk dropping a live
    /// conversation.
    fn release_finished_agent_panes(&mut self, cx: &mut Context<Self>) {
        let workspace = self.workspace.read(cx);
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
                (workspace.pane_runs_program(pane.0, &program) == Some(false)).then_some(*pane)
            })
            .collect();
        for pane in finished {
            self.launches.remove(&pane);
        }
    }

    /// Write each project's pane tree and the focused pane for the next
    /// launch. Identical state is not rewritten, so selecting or clicking
    /// around does not turn into a stream of writes.
    fn persist_workspace(&mut self, cx: &mut Context<Self>) {
        // Group ids are unique across project layouts and stale groups can be
        // dropped once their owning pane tree no longer contains them.
        let live_groups: HashSet<u64> = self
            .layouts
            .values()
            .flat_map(PaneNode::group_ids)
            .collect();
        self.group_states
            .retain(|group, _| live_groups.contains(group));

        let ratios: HashMap<u64, Vec<f32>> = self
            .group_states
            .iter()
            .filter_map(|(group, state)| {
                let sizes = state.read(cx).sizes();
                let total: f32 = sizes.iter().map(|size| f32::from(*size)).sum();
                if sizes.is_empty() || !total.is_finite() || total <= 0. {
                    return None;
                }
                let ratios: Vec<f32> = sizes.iter().map(|size| f32::from(*size) / total).collect();
                ratios
                    .iter()
                    .all(|ratio| ratio.is_finite() && *ratio > 0.)
                    .then_some((*group, ratios))
            })
            .collect();
        self.launches.retain(|pane, _| {
            self.layouts
                .values()
                .any(|layout| layout.panes().contains(pane))
        });
        let mut projects: Vec<SavedProjectLayout> = self
            .layouts
            .iter()
            .filter_map(|(project, layout)| {
                let focused = self
                    .focused_by_project
                    .get(project)
                    .copied()
                    .filter(|pane| layout.panes().contains(pane))
                    .or_else(|| layout.panes().first().copied())?;
                Some(SavedProjectLayout {
                    project: *project,
                    layout: layout.to_saved(&ratios),
                    focused: focused.0,
                })
            })
            .collect();
        projects.sort_by_key(|project| project.project);
        let state = SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: panes::SavedNode::ProjectLayouts {
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

    fn on_terminal_event(&mut self, pane: PaneId, ev: &TerminalEvent, cx: &mut Context<Self>) {
        match ev {
            TerminalEvent::Activated => self.focus_pane(pane, cx),
            TerminalEvent::SplitRight => self.split_pane(pane, Axis::Horizontal, cx),
            TerminalEvent::SplitDown => self.split_pane(pane, Axis::Vertical, cx),
            TerminalEvent::AgentPane(agent) => self.split_pane_with(
                pane,
                Axis::Horizontal,
                PaneLaunch::Agent {
                    agent: agent.clone(),
                    session: None,
                },
                cx,
            ),
            TerminalEvent::Focus(direction) => self.focus_neighbor(pane, *direction, cx),
            TerminalEvent::Close => self.close_pane(pane, cx),
        }
    }

    /// Move keyboard focus to the pane next to `pane` in `direction`.
    fn focus_neighbor(&mut self, pane: PaneId, direction: PaneDirection, cx: &mut Context<Self>) {
        let Some(next) = self
            .layouts
            .values()
            .find_map(|layout| layout.neighbor_in(pane, direction))
        else {
            return;
        };
        self.pending_focus = Some(next);
        self.focus_pane(next, cx);
    }

    fn focus_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if !self.panes.contains_key(&pane) {
            return;
        }
        let project = self
            .panes
            .get(&pane)
            .and_then(|terminal| terminal.read(cx).project_id());
        if let Some(project) = project {
            if self.selected != Some(project) {
                if let Some(previous) = self.selected {
                    self.hide_project_panes(previous, cx);
                }
                self.selected = Some(project);
                self.show_project_panes(project, cx);
            }
            self.focused_by_project.insert(project, pane);
        }
        self.focused = pane;
        self.sync_pane_chrome(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    fn focus_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .selected
            .and_then(|project| self.layouts.get(&project))
            .is_some_and(|layout| layout.panes().contains(&self.focused))
        {
            return;
        }
        if let Some(terminal) = self.panes.get(&self.focused).cloned() {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
        }
    }

    fn select_project(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.selected != Some(id)
            && let Some(previous) = self.selected
        {
            self.hide_project_panes(previous, cx);
        }
        self.selected = Some(id);
        self.focused = self
            .focused_by_project
            .get(&id)
            .copied()
            .or_else(|| self.layouts.get(&id)?.panes().first().copied())
            .unwrap_or(PaneId(1));
        if self.layouts.contains_key(&id) {
            self.focused_by_project.insert(id, self.focused);
        }
        self.pending_focus = self.layouts.contains_key(&id).then_some(self.focused);
        self.show_project_panes(id, cx);
        self.sync_pane_chrome(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    fn ensure_project_layout(&mut self, project: i64, cx: &mut Context<Self>) {
        if self.layouts.contains_key(&project) {
            return;
        }
        let pane = PaneId(self.next_pane);
        self.next_pane += 1;
        self.layouts.insert(project, PaneNode::leaf(pane));
        self.focused_by_project.insert(project, pane);
        self.install_pane(pane, cx);
    }

    fn hide_project_panes(&mut self, project: i64, cx: &mut Context<Self>) {
        let Some(layout) = self.layouts.get(&project) else {
            return;
        };
        for pane in layout.panes() {
            if let Some(terminal) = self.panes.get(&pane).cloned() {
                terminal.update(cx, |terminal, cx| terminal.hide(cx));
            }
        }
    }

    /// Split `pane` along `axis`, open another shell in the same project, and
    /// focus the new pane.
    fn split_pane(&mut self, pane: PaneId, axis: Axis, cx: &mut Context<Self>) {
        self.split_pane_with(pane, axis, PaneLaunch::Shell, cx);
    }

    /// Split `pane` and run `launch` in the new pane.
    fn split_pane_with(
        &mut self,
        pane: PaneId,
        axis: Axis,
        launch: PaneLaunch,
        cx: &mut Context<Self>,
    ) {
        if !self.panes.contains_key(&pane) {
            return;
        }
        let Some(project_id) = self
            .panes
            .get(&pane)
            .and_then(|t| t.read(cx).project_id())
            .filter(|&id| self.workspace.read(cx).project(id).is_some())
        else {
            return;
        };

        let new_id = PaneId(self.next_pane);
        self.next_pane += 1;
        let group_id = self.next_group;
        self.next_group += 1;
        let Some(layout) = self.layouts.get_mut(&project_id) else {
            return;
        };
        if !layout.split(pane, axis, new_id, group_id) {
            return;
        }
        if layout.contains_group(group_id) {
            let state = Self::new_group_state(&[], cx);
            self.group_states.insert(group_id, state);
        }

        if launch.is_shell() {
            self.launches.remove(&new_id);
        } else {
            self.launches.insert(new_id, launch.clone());
        }
        let terminal = self.install_pane(new_id, cx);
        terminal.update(cx, |terminal, cx| terminal.show(project_id, &launch, cx));
        self.focused = new_id;
        self.selected = Some(project_id);
        self.focused_by_project.insert(project_id, new_id);
        self.pending_focus = Some(new_id);
        self.sync_pane_chrome(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    /// Remove a pane after a close action or shell exit. The last pane leaves
    /// an empty project; a background exit keeps the selected project intact.
    fn close_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let project_id = self.panes.get(&pane).and_then(|t| t.read(cx).project_id());
        let Some(project_id) = project_id else {
            return;
        };
        let Some(layout) = self.layouts.get_mut(&project_id) else {
            return;
        };
        if !layout.panes().contains(&pane) {
            return;
        }
        if let Some(next) = layout.neighbor(pane) {
            if !layout.remove(pane) {
                return;
            }
            if self.focused_by_project.get(&project_id) == Some(&pane) {
                self.focused_by_project.insert(project_id, next);
                if self.selected == Some(project_id) {
                    self.focused = next;
                    self.pending_focus = Some(next);
                }
            }
        } else {
            self.layouts.remove(&project_id);
            self.focused_by_project.remove(&project_id);
            if self.selected == Some(project_id) {
                self.pending_focus = None;
            }
        }
        self.workspace
            .update(cx, |workspace, _| workspace.drop_terminal(pane.0));
        self.panes.remove(&pane);
        self.launches.remove(&pane);
        self.sync_pane_chrome(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    fn sync_pane_chrome(&mut self, cx: &mut Context<Self>) {
        let visible: HashSet<PaneId> = self
            .selected
            .and_then(|project_id| self.layouts.get(&project_id))
            .map(PaneNode::panes)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let focused = self.focused;
        for (id, terminal) in &self.panes {
            let active = visible.contains(id) && *id == focused;
            terminal.update(cx, |terminal, cx| {
                terminal.set_chrome(active, visible.contains(id), cx);
            });
        }
    }

    /// Open another shell in `project_id`, or in the selected project, or in
    /// the first one. A project with no pane gets its first; one that already
    /// has panes gains a split beside the focused pane.
    fn add_terminal(&mut self, project_id: Option<i64>, cx: &mut Context<Self>) {
        let project_id = project_id.or(self.selected).or_else(|| {
            self.workspace
                .update(cx, |workspace, cx| workspace.ensure_project(cx))
        });
        let Some(project_id) = project_id else {
            return;
        };
        let had_panes = self
            .layouts
            .get(&project_id)
            .is_some_and(|layout| !layout.panes().is_empty());
        if had_panes {
            self.select_project(project_id, cx);
            self.split_pane(self.focused, Axis::Horizontal, cx);
        } else {
            self.ensure_project_layout(project_id, cx);
            self.select_project(project_id, cx);
        }
    }

    /// Add a project from a native folder picker; the picked folder's own
    /// name becomes the project name, matching how Zed/waku open a project
    /// (no separate naming step). Picking a folder that's already a project
    /// selects it instead of inserting a duplicate row.
    fn add_project(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择项目目录".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |app, cx| app.finish_add_project(path, cx));
        })
        .detach();
    }

    /// Show the picked project — whether it was just added or was one already.
    fn finish_add_project(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let path = path.to_string_lossy().into_owned();
        let id = self
            .workspace
            .update(cx, |workspace, cx| workspace.add_project(&path, cx));
        if let Some(id) = id {
            self.ensure_project_layout(id, cx);
            self.select_project(id, cx);
        }
    }

    /// Ask for a branch name and create a worktree next to the repository.
    fn prompt_create_worktree(&self, project_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.workspace.read(cx).project(project_id).cloned() else {
            return;
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("分支名，例如 feat/login"));
        let weak = cx.weak_entity();
        let dialog_input = input.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            dialog
                .title("新建 worktree")
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "将在「{}」的同级目录创建 worktree，并检出同名分支。",
                            project.path
                        )),
                )
                .child(Input::new(&dialog_input).small())
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("创建")
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .on_ok({
                    let weak = weak.clone();
                    let input = dialog_input.clone();
                    move |_, _, cx| {
                        let value = input.read(cx).value().to_string();
                        let branch = value.trim().to_string();
                        if branch.is_empty() {
                            return false;
                        }
                        let _ = weak.update(cx, move |app, cx| {
                            app.create_worktree(project_id, branch, cx)
                        });
                        true
                    }
                })
                .centered()
        });
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            cx.notify();
        });
    }

    /// Create the worktree off the UI thread, then add it as a project under
    /// its repository. Failures stay in the banner above the panes.
    fn create_worktree(&mut self, project_id: i64, branch: String, cx: &mut Context<Self>) {
        let Some(path) = self
            .workspace
            .read(cx)
            .project(project_id)
            .map(|project| PathBuf::from(&project.path))
        else {
            return;
        };
        let work = cx
            .background_executor()
            .spawn(async move { crate::git::create_worktree(&path, &branch) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |app, cx| match result {
                Ok(worktree) => {
                    let path = worktree.path.to_string_lossy().into_owned();
                    let id = app.workspace.update(cx, |workspace, cx| {
                        workspace.add_worktree(project_id, &worktree.name, &path, cx)
                    });
                    if let Some(id) = id {
                        app.ensure_project_layout(id, cx);
                        app.select_project(id, cx);
                    }
                }
                Err(message) => {
                    app.store_error = Some(format!("创建 worktree 失败：{message}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// List the repository's existing worktrees and let the user attach one
    /// to this project. Listing runs off the UI thread because worktree
    /// metadata can live on a slow volume.
    fn prompt_open_worktree(&self, project_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.workspace.read(cx).project(project_id).cloned() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let path = PathBuf::from(&project.path);
            let entries = cx
                .background_executor()
                .spawn(async move { crate::git::list_worktrees(&path) })
                .await;
            let _ = this.update_in(cx, |app, window, cx| match entries {
                Ok(entries) => app.show_worktree_picker(project_id, entries, window, cx),
                Err(message) => {
                    app.store_error = Some(format!("读取 worktree 失败：{message}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Offer the worktrees git already knows about. A worktree Kumu shows
    /// already is labelled and selected instead of being added twice.
    fn show_worktree_picker(
        &mut self,
        project_id: i64,
        entries: Vec<WorktreeEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.workspace.read(cx).project(project_id).cloned() else {
            return;
        };
        // The main checkout is the project itself, and an entry whose
        // directory is gone cannot be opened. Git keeps listing a worktree
        // until it is pruned.
        let items: Vec<(String, PathBuf, Option<i64>)> = entries
            .into_iter()
            .filter(|entry| !entry.main && entry.path.exists())
            .map(|entry| {
                let open = self.workspace.read(cx).project_by_path(&entry.path);
                (entry.name, entry.path, open)
            })
            .collect();
        if items.is_empty() {
            window.open_dialog(cx, move |dialog, _, cx| {
                dialog
                    .title("打开已有 worktree")
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "「{}」没有其他 worktree。可在项目右键菜单中新建一个。",
                                project.name
                            )),
                    )
                    .button_props(
                        DialogButtonProps::default()
                            .ok_text("关闭")
                            .show_cancel(false),
                    )
                    .centered()
            });
            return;
        }

        let weak = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let theme = cx.theme();
            let (muted, hover, radius) = (theme.muted_foreground, theme.accent, theme.radius);
            let rows = items.iter().enumerate().map(|(index, (name, path, open))| {
                let shown = path.to_string_lossy().into_owned();
                let row_name = name.clone();
                let row_path = path.clone();
                let open = *open;
                let weak = weak.clone();
                v_flex()
                    .id(("open-worktree", index))
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .gap_0p5()
                    .rounded(radius)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover.opacity(0.5)))
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |app, cx| match open {
                            Some(id) => {
                                app.ensure_project_layout(id, cx);
                                app.select_project(id, cx);
                            }
                            None => app.attach_worktree(
                                project_id,
                                row_name.clone(),
                                row_path.clone(),
                                cx,
                            ),
                        });
                        window.close_dialog(cx);
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_between()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .font_medium()
                                    .child(name.clone()),
                            )
                            .when_some(open, |row, _| {
                                row.child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(muted)
                                        .child("已打开"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(muted)
                            .child(shown),
                    )
            });
            dialog
                .title("打开已有 worktree")
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "选择要加入「{}」的 worktree，它会作为子项目出现在侧栏。",
                            project.name
                        )),
                )
                .child(
                    div()
                        .id("open-worktree-list")
                        .w_full()
                        .max_h(px(320.))
                        .overflow_y_scroll()
                        .child(v_flex().w_full().children(rows)),
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("关闭")
                        .show_cancel(false),
                )
                .centered()
        });
    }

    /// Attach an existing worktree below its repository and show it.
    fn attach_worktree(
        &mut self,
        parent_id: i64,
        name: String,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let path = path.to_string_lossy().into_owned();
        let id = self.workspace.update(cx, |workspace, cx| {
            workspace.open_worktree(parent_id, &name, &path, cx)
        });
        let Some(id) = id else {
            return;
        };
        self.ensure_project_layout(id, cx);
        self.select_project(id, cx);
    }

    /// Rename a project: the dialog starts on the current name.
    fn prompt_rename_project(&self, project_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .workspace
            .read(cx)
            .projects()
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.name.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name));
        let weak = cx.weak_entity();
        let dialog_input = input.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("重命名项目")
                .child(Input::new(&dialog_input).small())
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("保存")
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .on_ok({
                    let weak = weak.clone();
                    let input = dialog_input.clone();
                    move |_, _, cx| {
                        let name = input.read(cx).value().trim().to_string();
                        if !name.is_empty() {
                            let _ = weak.update(cx, move |app, cx| {
                                app.workspace.update(cx, |workspace, cx| {
                                    workspace.rename_project(project_id, name, cx)
                                })
                            });
                        }
                        true
                    }
                })
                .centered()
        });
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
    }

    /// Ask before deleting a project and closing its terminals.
    fn confirm_delete_project(&self, project_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.workspace.read(cx).project(project_id).cloned() else {
            return;
        };
        let name = project.name;
        // A worktree row is only a view of a directory; removing it must not
        // pretend the checkout on disk goes away.
        let (title, description, ok_text) = if project.parent_id.is_some() {
            (
                "移除 worktree",
                format!("「{name}」将从侧边栏移除，磁盘上的 worktree 不会被删除。"),
                "移除",
            )
        } else {
            (
                "删除项目",
                format!("「{name}」及其 worktree、终端将被关闭，此操作无法撤销。"),
                "删除",
            )
        };
        // The dialog outlives this call, so it reaches the view weakly.
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let weak = weak.clone();
            alert
                .title(title)
                .description(description.clone())
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(ok_text)
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    let _ = weak.update(cx, move |app, cx| app.delete_project(project_id, cx));
                    true
                })
                .centered()
        });
    }

    /// Delete a project and its worktrees, then show another project when
    /// one remains. Worktree directories stay on disk.
    fn delete_project(&mut self, project_id: i64, cx: &mut Context<Self>) {
        let removed = self.workspace.read(cx).project_tree_ids(project_id);
        if self
            .workspace
            .update(cx, |workspace, cx| workspace.delete_project(project_id, cx))
            .is_err()
        {
            // The failure is already on its way to the banner; leave the
            // project and its panes exactly where they are.
            return;
        }
        let selected_removed = self.selected.is_some_and(|id| removed.contains(&id));
        for id in removed {
            if let Some(layout) = self.layouts.remove(&id) {
                for pane in layout.panes() {
                    self.panes.remove(&pane);
                }
            }
            self.focused_by_project.remove(&id);
        }
        if selected_removed {
            let next = self.workspace.read(cx).projects().first().map(|p| p.id);
            self.selected = next;
            if let Some(next) = next {
                self.select_project(next, cx);
                return;
            }
            self.focused = PaneId(1);
        }
        self.sync_pane_chrome(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    /// Focus the settings window if it's open, else open a new one (resizable,
    /// opens at its minimum size).
    fn open_settings(&mut self, show_updates: bool, cx: &mut Context<Self>) {
        let activate = |view: AnyView, window: &mut Window, cx: &mut App| {
            if show_updates
                && let Ok(root) = view.downcast::<Root>()
                && let Ok(settings) = root.read(cx).view().clone().downcast::<SettingsWindow>()
            {
                settings.update(cx, |settings, cx| settings.show_updates(window, cx));
            }
            window.activate_window();
        };
        if let Some(handle) = &self.settings_window
            && handle.update(cx, activate).is_ok()
        {
            return;
        }
        let mut options = TitleBar::window_options();
        options.window_bounds = Some(WindowBounds::centered(size(px(680.), px(640.)), cx));
        options.window_min_size = Some(size(px(680.), px(640.)));
        let updater = self.updater.clone();
        if let Ok((handle, _)) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| SettingsWindow::new(updater, window, cx))
        }) {
            handle.update(cx, activate).ok();
            self.settings_window = Some(handle);
        }
    }

    fn render_panes(&self, cx: &Context<Self>) -> AnyElement {
        self.selected
            .and_then(|project_id| self.layouts.get(&project_id))
            .map(|layout| self.render_node(layout, cx))
            .unwrap_or_else(|| div().into_any_element())
    }

    fn render_node(&self, node: &PaneNode, cx: &Context<Self>) -> AnyElement {
        match node {
            PaneNode::Leaf(id) => {
                let Some(terminal) = self.panes.get(id) else {
                    return div().into_any_element();
                };
                terminal
                    .clone()
                    .cached(StyleRefinement::default().size_full().min_h_0().min_w_0())
                    .into_any_element()
            }
            PaneNode::Group { id, axis, children } => {
                let mut group = match axis {
                    Axis::Horizontal => h_resizable(format!("pane-split-{id}")),
                    Axis::Vertical => v_resizable(format!("pane-split-{id}")),
                };
                if let Some(state) = self.group_states.get(id) {
                    group = group.with_state(state);
                }
                for child in children {
                    group = group.child(resizable_panel().child(self.render_node(child, cx)));
                }
                // A drag ends here: write the new fractions before another
                // change can replace them.
                group = group.on_resize({
                    let weak = cx.entity().downgrade();
                    move |_, _, cx| {
                        let _ = weak.update(cx, |app, cx| app.persist_workspace(cx));
                    }
                });
                group.into_any_element()
            }
        }
    }

    fn render_home(&self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let warning = theme.warning;
        let workspace = self.workspace.read(cx);
        let has_projects = !workspace.projects().is_empty();
        let has_panes = self
            .selected
            .and_then(|project| self.layouts.get(&project))
            .is_some_and(|layout| !layout.panes().is_empty());
        let right =
            if has_panes {
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .min_h_0()
                    .child(self.render_panes(cx))
            } else if has_projects {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .text_color(theme.muted_foreground)
                    .child(div().text_sm().child("还没有终端"))
                    .child(Button::new("empty-new").ghost().label("新建终端").on_click(
                        cx.listener(|this, _, window, cx| {
                            this.add_terminal(None, cx);
                            this.focus_terminal(window, cx);
                            cx.notify();
                        }),
                    ))
            } else {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .text_color(theme.muted_foreground)
                    .child(div().text_sm().child("还没有项目"))
                    .child(
                        Button::new("empty-open")
                            .ghost()
                            .label("打开项目")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.add_project(cx);
                            })),
                    )
            };

        // Full-width title bar strip: brand on the left (traffic lights float
        // to its left); the settings entry lives in the sidebar's bottom-left.
        let title_bar = TitleBar::new().child(div().text_sm().font_bold().child("kumu"));

        v_flex()
            .size_full()
            .font_family(theme.font_family.clone())
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(title_bar)
            .children(self.store_error.clone().map(|message| {
                h_flex()
                    .flex_none()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .items_center()
                    .bg(warning.opacity(0.12))
                    .text_color(warning)
                    .child(div().flex_1().min_w_0().text_xs().child(message))
                    .child(
                        div()
                            .id("dismiss-store-error")
                            .flex_none()
                            .rounded(theme.radius)
                            .p_0p5()
                            .cursor_pointer()
                            .hover(move |style| style.bg(warning.opacity(0.25)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store_error = None;
                                cx.notify();
                            }))
                            .child(Icon::new(IconName::Close).small().text_color(warning)),
                    )
            }))
            .child(
                // The sidebar and panes share one resizable group, so the
                // divider between them is a real drag handle rather than a
                // painted line. The sidebar keeps its width when the window
                // resizes; only dragging its handle changes it.
                div().flex_1().min_h_0().min_w_0().child(
                    h_resizable("home-split")
                        .child(
                            resizable_panel()
                                .size(px(self.sidebar_width))
                                .size_range(px(SIDEBAR_MIN_WIDTH)..px(SIDEBAR_MAX_WIDTH))
                                .fixed()
                                .child(self.render_sidebar(cx)),
                        )
                        .child(resizable_panel().child(right))
                        // Persist the width once the drag ends, the same way
                        // the pane splits save their ratios.
                        .on_resize({
                            let weak = cx.entity().downgrade();
                            move |state, _, cx| {
                                let Some(width) = state.read(cx).sizes().first().copied() else {
                                    return;
                                };
                                let width =
                                    f32::from(width).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
                                crate::db::set_setting(
                                    crate::db::SETTING_SIDEBAR_WIDTH,
                                    &format!("{width}"),
                                )
                                .ok();
                                let _ = weak.update(cx, |app, _| app.sidebar_width = width);
                            }
                        }),
                ),
            )
    }
}

impl Render for KumuApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(pane) = self.pending_focus.take()
            && let Some(terminal) = self.panes.get(&pane).cloned()
        {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
        }
        // Reading the focused terminal acknowledges its attention request.
        // Also retract notifications when a project is deleted or the setting
        // is disabled, including banners still in Notification Center.
        let enabled = cx.global::<NotificationSettings>().enabled;
        let workspace = self.workspace.read(cx);
        let focused = window.is_window_active().then_some(self.focused.0);
        let read: Vec<u64> = self
            .unread_notifications
            .iter()
            .filter(|(terminal, project)| {
                !enabled
                    || workspace.project(**project).is_none()
                    || workspace.notification_is_read(**terminal, focused)
            })
            .map(|(terminal, _)| *terminal)
            .collect();
        for terminal in read {
            self.unread_notifications.remove(&terminal);
            window.remove_notification1::<NotificationSettings>(terminal.to_string(), cx);
        }
        self.render_home(window, cx)
    }
}
