//! Home window: projects on the left, the selected project's splitable pane
//! tree on the right. Owns the settings window handle.
//!
//! A project owns its panes. Splitting adds another cell to that project; a
//! cell can hold several terminals as tabs.

mod control_handler;
mod dialogs;
mod native_host;
mod plugins_host;
mod shell_nsh;
mod sidebar;
mod sidebar_nsh;
mod sprites;
mod tabs;
mod tabs_nsh;
mod terminal;

use std::collections::{HashMap, HashSet};

use crate::agent::launch::{PaneLaunch, session_launch};
use crate::agent::status::clean_osc_title;
use crate::agent::{AgentState, StatusSignal};
use crate::app::layout::{
    self, PaneDirection, PaneId, PaneNode, ProjectTab, ProjectTabs, SavedProjectTab,
    SavedProjectTabs, SavedWorkspace, SplitAxis, TabId, WORKSPACE_STATE_VERSION,
};
use crate::app::{NotificationSettings, Workspace, WorkspaceEvent};
use crate::nsh_ui::dialogs::ActiveDialog;
use crate::nsh_ui::toast::ToastItem;
use crate::theme::KumuTheme;
use crate::updates::Updater;
use gpui::*;
use native_host::NativeViewRegistry;
use plugins_host::{PLUGIN_MAX_WIDTH, PLUGIN_MIN_WIDTH, PLUGIN_WIDTH};
use shell_nsh::{
    CenterModeSnapshot, HomeShellAction, HomeShellSnapshot, LayoutNodeSnapshot, NshHomeShell,
    PluginPanelSnapshot,
};
use terminal::TerminalEvent;
pub(crate) use terminal::TerminalView;

/// Whether panes come back with the conversation they were running.
fn auto_resume_enabled() -> bool {
    crate::db::get_setting(crate::db::SETTING_AUTO_RESUME).as_deref() != Some("0")
}

/// Whether a pasted prompt should be submitted. Only a pane started as an
/// agent takes the Enter; a shell keeps the text on its command line, where
/// Enter would run it. `None` (no live process) is treated like a shell.
fn submits_after_paste(pane_is_shell: Option<bool>) -> bool {
    pane_is_shell == Some(false)
}

/// A shell without bracketed paste runs each line of a multi-line paste, so
/// its copy of the message arrives as one line of space-separated words.
fn shell_prompt(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The name a pane's agent shows in the UI, for a pane whose program has not
/// set a title yet.
fn agent_display_name(id: &str) -> Option<String> {
    crate::agent::AGENTS
        .iter()
        .find(|agent| agent.id == id)
        .map(|agent| agent.name.to_string())
}

/// The sidebar's opening width and the range the divider beside it lets the
/// user drag it through. The maximum still leaves room for the panes on the
/// app's minimum window width.
const SIDEBAR_WIDTH: f32 = 256.;
pub(super) const SIDEBAR_MIN_WIDTH: f32 = 180.;
pub(super) const SIDEBAR_MAX_WIDTH: f32 = 520.;

/// What the sidebar, pane header and tab strip know about a live agent.
#[derive(Clone, Debug)]
pub(super) struct AgentEntry {
    /// Active state, or `None` when the agent is alive and idle/ready.
    pub(super) state: Option<AgentState>,
    /// Authoritative session title reported via `kumu-title:<title>`.
    pub(super) session_title: Option<String>,
    /// First prompt reported via `kumu-title:prompt:<text>`.
    pub(super) prompt_title: Option<String>,
    /// Cleaned OSC 0/2 window title (`snapshot.title`).
    pub(super) osc_title: Option<String>,
    /// Agent id (e.g. `"claude"`, `"opencode"`).
    pub(super) agent: String,
}

impl AgentEntry {
    fn idle(agent: String) -> Self {
        Self {
            state: None,
            session_title: None,
            prompt_title: None,
            osc_title: None,
            agent,
        }
    }

    /// Title priority: reported session title > first prompt > OSC 0/2 title > agent name.
    pub(super) fn display_title(&self) -> String {
        self.session_title
            .as_deref()
            .or(self.prompt_title.as_deref())
            .or(self.osc_title.as_deref())
            .map(str::to_string)
            .or_else(|| agent_display_name(&self.agent))
            .unwrap_or_else(|| self.agent.clone())
    }
}

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
    /// Active agent state per pane. Idle and plain shell panes have no entry.
    agent_status: HashMap<PaneId, AgentEntry>,
    /// Whether the sidebar's agent section is collapsed to its summary line.
    agents_collapsed: bool,
    /// A persistence failure the workspace reported, shown above the panes
    /// until dismissed.
    store_error: Option<String>,
    /// Terminal views keyed by pane id.
    panes: HashMap<PaneId, Entity<TerminalView>>,
    /// Each project's independent tab list; the selected tab's tree is shown.
    tabs_by_project: HashMap<i64, ProjectTabs>,
    /// Saved split ratios per split group (normalized, summing to 1.0).
    group_ratios: HashMap<u64, Vec<f32>>,
    /// Focused pane in the selected project's selected tab.
    focused: PaneId,
    /// Focus this pane's terminal on the next paint (after a split / select).
    pending_focus: Option<PaneId>,
    /// A tab close waiting for confirmation; clearing a tab ends every pane.
    pending_close_tab: Option<(i64, TabId)>,
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
    /// Open right-hand plugin panel, by registry id.
    open_plugin: Option<&'static str>,
    /// Live plugin panels. A panel is created when it opens and dropped when
    /// it closes, so a closed panel's refresh loop stops.
    plugin_views: HashMap<&'static str, AnyView>,
    /// Width of the open plugin panel in px, chosen by dragging its divider.
    plugin_width: f32,
    /// Registry backing `"kumu.terminal"` and `"kumu.plugin"` native NSH slots.
    native_registry: NativeViewRegistry,
    /// Unified Pure Rust NSH home shell (sidebar + tabs + split pane grid + plugin slot + toast stack).
    shell_nsh: NshHomeShell,
    /// Full-window Pure Rust NSH modal dialog overlay (mounted only while open).
    active_dialog: Option<ActiveDialog>,
    /// Full-window Pure Rust NSH right-edge sheet overlay (e.g. Git Diff Review).
    pub(super) active_sheet: Option<AnyView>,
    /// Syncs KumuTheme updates into `shell_nsh` and `active_dialog`.
    _nsh_theme_sub: Subscription,
    /// The local control socket a program inside a pane reads its neighbours
    /// through. Lives exactly as long as this window.
    control: Option<crate::control::ControlHandle>,
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
            WorkspaceEvent::Frame(_) => {}
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
                if window.is_window_active() {
                    // Foreground window uses Pure Rust NSH toast stack inside the home shell.
                    this.shell_nsh.push_toast(
                        ToastItem::info(terminal.to_string(), Some(title), message).clickable(true),
                    );
                } else {
                    // A background window posts only to Notification Center: one
                    // event should not surface as both a banner and a toast. The
                    // sidebar's unread marker is the in-app fallback when macOS
                    // declines delivery or kumu runs unbundled.
                    cx.show_system_notification(SystemNotification {
                        tag: terminal.to_string().into(),
                        title: title.into(),
                        body: message.into(),
                        actions: Vec::new(),
                    });
                }
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
                // When the agent exits back to a shell it is no longer alive,
                // so it leaves the global agent list. A broken resume that
                // restarts fresh comes back idle with no stale title.
                if next.is_shell() {
                    this.agent_status.remove(&pane);
                    this.workspace
                        .update(cx, |workspace, cx| workspace.unbind_pane_todos(pane.0, cx));
                } else if let PaneLaunch::Agent { agent, .. } = &next {
                    this.agent_status
                        .insert(pane, AgentEntry::idle(agent.clone()));
                }
                // Only the visible tab's panes are on screen. Restarting a
                // hidden one would spend a process nobody can see; it starts
                // when its tab is selected.
                if this.pane_is_visible(pane)
                    && let Some(terminal) = this.panes.get(&pane).cloned()
                {
                    terminal.update(cx, |terminal, cx| {
                        terminal.hide(cx);
                        terminal.show(*project, &next, cx);
                    });
                }
                this.sync_pane_agent_status(pane, cx);
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
                let previous_session = match this.launches.get(&pane) {
                    Some(PaneLaunch::Agent { session, .. }) => session.clone(),
                    _ => None,
                };
                let shell_pane = this
                    .workspace
                    .read(cx)
                    .pane_is_shell(pane.0)
                    .unwrap_or(false);
                if let Some(launch) = session_launch(
                    this.launches.get(&pane),
                    agent,
                    session,
                    this.auto_resume,
                    shell_pane,
                ) && this.launches.get(&pane) != Some(&launch)
                {
                    this.launches.insert(pane, launch);
                    this.persist_workspace(cx);
                }
                let osc_title = this
                    .workspace
                    .read(cx)
                    .pane_title(pane.0)
                    .and_then(|title| clean_osc_title(&title, agent));
                let entry = this.agent_status.entry(pane).or_insert_with(|| {
                    let mut entry = AgentEntry::idle(agent.to_string());
                    entry.osc_title = osc_title;
                    entry
                });
                let agent_changed = entry.agent != agent;
                let session_changed = previous_session
                    .as_deref()
                    .is_some_and(|prev| prev != session);
                if agent_changed || session_changed {
                    entry.session_title = None;
                    entry.prompt_title = None;
                }
                entry.agent = agent.to_string();
                this.workspace.update(cx, |workspace, cx| {
                    workspace.update_pane_todos_session(pane.0, agent, session, cx);
                });
                cx.notify();
            }
            WorkspaceEvent::AgentStatus {
                pane,
                project,
                agent,
                signal,
            } => {
                this.note_agent_status(PaneId(*pane), *project, agent, *signal, cx);
            }
            WorkspaceEvent::AgentTitle {
                pane,
                project,
                agent,
                title,
                is_prompt,
            } => {
                this.note_agent_title(PaneId(*pane), *project, agent, title, *is_prompt, cx);
            }
            WorkspaceEvent::PaneTitle {
                pane,
                project,
                title,
            } => {
                this.note_pane_title(PaneId(*pane), *project, title, cx);
            }
            WorkspaceEvent::TodosChanged(_) => cx.notify(),
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
        let notif_weak = cx.entity().downgrade();
        let notif_window = window_handle;
        cx.on_system_notification_response(move |response, cx| {
            if let Ok(terminal) = response.tag.parse::<u64>() {
                let _ = notif_window.update(cx, |_, window, cx| {
                    window.activate_window();
                    let _ = notif_weak.update(cx, |this, cx| {
                        this.reveal_terminal_by_id(terminal, window, cx);
                    });
                });
            }
        });
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

        let initial_theme = KumuTheme::global(cx).clone();
        let native_registry = NativeViewRegistry::new();
        let shell_nsh = NshHomeShell::new(
            HomeShellSnapshot {
                sidebar_width,
                plugin_width: PLUGIN_WIDTH,
                ..Default::default()
            },
            &native_registry,
            &initial_theme,
            window,
            cx,
            |app, action, window, cx| app.handle_shell_action(action, window, cx),
        );
        let nsh_theme_sub = cx.observe_global::<KumuTheme>(|app: &mut Self, cx| {
            let theme = KumuTheme::global(cx).clone();
            app.shell_nsh.sync_theme(&theme, cx);
            if let Some(dialog) = &app.active_dialog {
                dialog.sync_theme(&theme, cx);
            }
        });

        let mut app = Self {
            workspace,
            updater,
            settings_window: None,
            selected: None,
            unread_notifications: HashMap::new(),
            agent_status: HashMap::new(),
            agents_collapsed: false,
            store_error: startup_error,
            panes: HashMap::new(),
            tabs_by_project: HashMap::new(),
            group_ratios: HashMap::new(),
            focused: PaneId(1),
            pending_focus: None,
            pending_close_tab: None,
            next_pane: 1,
            next_group: 1,
            launches: HashMap::new(),
            auto_resume: auto_resume_enabled(),
            last_saved_layout,
            sidebar_width,
            open_plugin: None,
            plugin_views: HashMap::new(),
            plugin_width: PLUGIN_WIDTH,
            native_registry,
            shell_nsh,
            active_dialog: None,
            active_sheet: None,
            _nsh_theme_sub: nsh_theme_sub,
            control: None,
        };
        if !app.restore_workspace(saved_workspace.as_deref(), cx) {
            app.start_default(cx);
        }
        app.start_control(cx);
        app.focus_terminal(window, cx);
        app.sync_pane_chrome(cx);
        let shell_snap = app.build_shell_snapshot(cx);
        app.shell_nsh.sync(shell_snap);
        app.persist_workspace(cx);
        app
    }

    /// Show the first project in a single tab — the first run, and the
    /// fallback whenever a saved layout cannot be used.
    fn start_default(&mut self, cx: &mut Context<Self>) {
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
    fn install_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) -> Entity<TerminalView> {
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
    /// shell (see [`WorkspaceEvent::AgentExited`]). A shell pane only borrowed
    /// the launch from the conversation inside it, and nothing reports that
    /// agent leaving, so the process tree decides at quit. When it cannot
    /// answer, the launch is kept rather than risk dropping a live
    /// conversation.
    fn release_finished_agent_panes(&mut self, cx: &mut Context<Self>) {
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
    fn persist_workspace(&mut self, cx: &mut Context<Self>) {
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

    fn on_terminal_event(&mut self, pane: PaneId, ev: &TerminalEvent, cx: &mut Context<Self>) {
        match ev {
            TerminalEvent::Activated => self.focus_pane(pane, cx),
            TerminalEvent::SplitRight => self.split_pane(pane, SplitAxis::Horizontal, cx),
            TerminalEvent::SplitDown => self.split_pane(pane, SplitAxis::Vertical, cx),
            TerminalEvent::NewTab => self.new_tab(cx),
            TerminalEvent::NextTab => self.switch_tab(1, cx),
            TerminalEvent::PrevTab => self.switch_tab(-1, cx),
            TerminalEvent::SelectTab(index) => self.select_tab(*index, cx),
            TerminalEvent::CloseTab => self.close_selected_tab(cx),
            TerminalEvent::Focus(direction) => self.focus_neighbor(pane, *direction, cx),
            TerminalEvent::ChromeChanged => cx.notify(),
        }
    }

    /// The agent id a pane was launched with, or `None` for a shell.
    fn pane_agent_id(&self, pane: PaneId) -> Option<String> {
        match self.launches.get(&pane) {
            Some(PaneLaunch::Agent { agent, .. }) => Some(agent.clone()),
            _ => None,
        }
    }

    /// Ensure an `AgentEntry` exists for `pane`, seeding its OSC 0/2 title if
    /// the terminal already set one.
    fn ensure_agent_entry(
        &mut self,
        pane: PaneId,
        reported_agent: &str,
        cx: &Context<Self>,
    ) -> &mut AgentEntry {
        let launched = self.pane_agent_id(pane);
        let agent = crate::agent::id_for(reported_agent)
            .map(str::to_string)
            .or(launched)
            .unwrap_or_else(|| reported_agent.to_string());
        let osc_title = self
            .workspace
            .read(cx)
            .pane_title(pane.0)
            .and_then(|title| clean_osc_title(&title, &agent));
        let entry = self.agent_status.entry(pane).or_insert_with(|| {
            let mut entry = AgentEntry::idle(agent.clone());
            entry.osc_title = osc_title;
            entry
        });
        if !agent.is_empty() && entry.agent != agent {
            entry.agent = agent;
            entry.session_title = None;
            entry.prompt_title = None;
        }
        entry
    }

    /// Record a status signal an agent reported on its silent OSC channel.
    fn note_agent_status(
        &mut self,
        pane: PaneId,
        project: i64,
        agent: &str,
        signal: StatusSignal,
        cx: &mut Context<Self>,
    ) {
        // A stale process must not write into a pane that now belongs to
        // another project.
        if self.pane_tab(pane).map(|(owner, _)| owner) != Some(project) {
            return;
        }
        let entry = self.ensure_agent_entry(pane, agent, cx);
        match signal {
            StatusSignal::Active(state) => entry.state = Some(state),
            StatusSignal::Resume => {
                if entry.state == Some(AgentState::Waiting) {
                    entry.state = Some(AgentState::Running);
                }
            }
            StatusSignal::Idle => entry.state = None,
        }
        match signal {
            StatusSignal::Active(AgentState::Completed) => {
                self.workspace.update(cx, |workspace, cx| {
                    workspace.mark_pane_todos_review(pane.0, cx)
                });
            }
            StatusSignal::Active(AgentState::Running) | StatusSignal::Resume => {
                self.workspace.update(cx, |workspace, cx| {
                    workspace.clear_pane_todos_review(pane.0, cx);
                });
            }
            _ => {}
        }
        self.sync_pane_agent_status(pane, cx);
        cx.notify();
    }

    /// Record a session or first-prompt title reported on `kumu-title:`.
    fn note_agent_title(
        &mut self,
        pane: PaneId,
        project: i64,
        agent: &str,
        title: &str,
        is_prompt: bool,
        cx: &mut Context<Self>,
    ) {
        if self.pane_tab(pane).map(|(owner, _)| owner) != Some(project) {
            return;
        }
        let entry = self.ensure_agent_entry(pane, agent, cx);
        if !is_prompt {
            entry.session_title = Some(title.to_string());
        } else if entry.prompt_title.is_none() {
            entry.prompt_title = Some(title.to_string());
        }
        cx.notify();
    }

    /// Keep a live agent's OSC 0/2 fallback title up to date when the terminal
    /// changes its window title.
    fn note_pane_title(&mut self, pane: PaneId, project: i64, title: &str, cx: &mut Context<Self>) {
        if self.pane_tab(pane).map(|(owner, _)| owner) != Some(project) {
            return;
        }
        let Some(entry) = self.agent_status.get_mut(&pane) else {
            return;
        };
        let next = clean_osc_title(title, &entry.agent);
        if next != entry.osc_title {
            entry.osc_title = next;
            cx.notify();
        }
    }

    /// Push the active state the pane's header should draw (`None` for idle
    /// agents and plain shells).
    pub(super) fn sync_pane_agent_status(&self, pane: PaneId, cx: &mut Context<Self>) {
        let state = self.agent_status.get(&pane).and_then(|entry| entry.state);
        if let Some(view) = self.panes.get(&pane) {
            view.update(cx, |view, cx| view.set_agent_status(state, cx));
        }
    }

    /// The project that owns `pane`, including panes in hidden tabs.
    fn pane_project(&self, pane: PaneId, cx: &Context<Self>) -> Option<i64> {
        self.pane_tab(pane).map(|(project, _)| project).or_else(|| {
            self.panes
                .get(&pane)
                .and_then(|view| view.read(cx).project_id())
        })
    }

    /// Move keyboard focus to the pane next to `pane` in `direction`.
    fn focus_neighbor(&mut self, pane: PaneId, direction: PaneDirection, cx: &mut Context<Self>) {
        let Some(next) = self.pane_tab(pane).and_then(|(project, tab_index)| {
            self.tabs_by_project
                .get(&project)
                .and_then(|tabs| tabs.tabs.get(tab_index))
                .and_then(|tab| tab.layout.neighbor_in(pane, direction))
        }) else {
            return;
        };
        self.focus_pane(next, cx);
    }

    fn focus_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
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

    fn focus_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .selected
            .and_then(|project| self.active_tab(project))
            .is_some_and(|tab| tab.layout.contains(self.focused))
        {
            return;
        }
        if let Some(terminal) = self.panes.get(&self.focused).cloned() {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
        }
    }

    fn select_project(&mut self, id: i64, cx: &mut Context<Self>) {
        self.pending_close_tab = None;
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
        self.sync_pane_chrome(cx);
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    fn ensure_project_layout(&mut self, project: i64, cx: &mut Context<Self>) {
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

    fn hide_project_panes(&mut self, project: i64, cx: &mut Context<Self>) {
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return;
        };
        for pane in tabs.panes() {
            if let Some(terminal) = self.panes.get(&pane).cloned() {
                terminal.update(cx, |terminal, cx| terminal.hide(cx));
            }
        }
    }

    /// Split `pane` along `axis`, open another shell in the same tab, and
    /// focus the new pane.
    fn split_pane(&mut self, pane: PaneId, axis: SplitAxis, cx: &mut Context<Self>) {
        self.split_pane_with(pane, axis, PaneLaunch::Shell, cx);
    }

    /// Split `pane` and run `launch` in the new pane.
    fn split_pane_with(
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
    fn close_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
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
        self.sync_pane_chrome(cx);
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    fn sync_pane_chrome(&mut self, cx: &mut Context<Self>) {
        let visible: HashSet<PaneId> = self
            .selected
            .and_then(|project_id| self.active_tab(project_id))
            .map(|tab| tab.layout.panes())
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

    fn build_layout_snapshot(&self, node: &PaneNode) -> LayoutNodeSnapshot {
        match node {
            PaneNode::Leaf(id) => LayoutNodeSnapshot::Leaf(*id),
            PaneNode::Group { id, axis, children } => LayoutNodeSnapshot::Group {
                id: *id,
                axis: *axis,
                ratios: self.group_ratios.get(id).cloned().unwrap_or_default(),
                children: children
                    .iter()
                    .map(|c| self.build_layout_snapshot(c))
                    .collect(),
            },
        }
    }

    fn build_shell_snapshot(&self, cx: &Context<Self>) -> HomeShellSnapshot {
        let has_projects = !self.workspace.read(cx).projects().is_empty();
        let active_tab = self.selected.and_then(|project| self.active_tab(project));
        let (center, pane_chrome) = match active_tab {
            Some(tab) if !tab.layout.panes().is_empty() => {
                let mut chrome = HashMap::new();
                for pane_id in tab.layout.panes() {
                    if let Some(view) = self.panes.get(&pane_id) {
                        chrome.insert(pane_id, view.read(cx).chrome_snapshot());
                    }
                }
                (
                    CenterModeSnapshot::Panes(self.build_layout_snapshot(&tab.layout)),
                    chrome,
                )
            }
            _ if has_projects => (CenterModeSnapshot::EmptyTerminals, HashMap::new()),
            _ => (CenterModeSnapshot::EmptyProjects, HashMap::new()),
        };

        let open_plugin =
            self.open_plugin
                .and_then(crate::plugins::spec)
                .map(|spec| PluginPanelSnapshot {
                    id: spec.id,
                    title: spec.title,
                    icon: spec.icon,
                });

        HomeShellSnapshot {
            store_error: self.store_error.clone(),
            sidebar_width: self.sidebar_width,
            plugin_width: self.plugin_width,
            sidebar: self.build_sidebar_snapshot(cx),
            tab_bar: self.build_tab_bar_snapshot(),
            center,
            pane_chrome,
            open_plugin,
        }
    }

    fn handle_shell_action(
        &mut self,
        action: HomeShellAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            HomeShellAction::Sidebar(a) => self.handle_sidebar_action(a, window, cx),
            HomeShellAction::TabBar(a) => self.handle_tab_bar_action(a, window, cx),
            HomeShellAction::DismissStoreError => {
                self.store_error = None;
                cx.notify();
            }
            HomeShellAction::EmptyAddTerminal => {
                self.add_terminal(None, cx);
                self.focus_terminal(window, cx);
                cx.notify();
            }
            HomeShellAction::EmptyAddProject => {
                self.add_project(cx);
            }
            HomeShellAction::ClosePlugin => {
                self.open_plugin = None;
                self.plugin_views.clear();
                self.native_registry.clear_plugins();
                cx.notify();
            }
            HomeShellAction::ResizeOuter {
                sidebar_width,
                plugin_width,
            } => {
                let width = sidebar_width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
                if (width - self.sidebar_width).abs() >= 1.0 {
                    crate::db::set_setting(crate::db::SETTING_SIDEBAR_WIDTH, &format!("{width}"))
                        .ok();
                    self.sidebar_width = width;
                }
                if let Some(pw) = plugin_width {
                    self.plugin_width = pw.clamp(PLUGIN_MIN_WIDTH, PLUGIN_MAX_WIDTH);
                }
            }
            HomeShellAction::ResizeGroup { group_id, ratios } => {
                self.group_ratios.insert(group_id, ratios);
                self.persist_workspace(cx);
            }
            HomeShellAction::FocusPane(pane) => {
                self.focus_pane(pane, cx);
                self.focus_terminal(window, cx);
                cx.notify();
            }
            HomeShellAction::SplitRight(pane) => {
                self.split_pane(pane, SplitAxis::Horizontal, cx);
            }
            HomeShellAction::SplitDown(pane) => {
                self.split_pane(pane, SplitAxis::Vertical, cx);
            }
            HomeShellAction::ClosePane(pane) => {
                self.close_pane(pane, cx);
            }
            HomeShellAction::AgentPane { pane, agent } => {
                self.split_pane_with(
                    pane,
                    SplitAxis::Horizontal,
                    PaneLaunch::Agent {
                        agent,
                        session: None,
                    },
                    cx,
                );
            }
            HomeShellAction::FindChange { pane, needle } => {
                if let Some(view) = self.panes.get(&pane).cloned() {
                    view.update(cx, |v, cx| v.set_find_needle(needle, cx));
                }
            }
            HomeShellAction::FindStep { pane, older } => {
                if let Some(view) = self.panes.get(&pane).cloned() {
                    view.update(cx, |v, cx| v.step_find(older, cx));
                }
            }
            HomeShellAction::FindClose(pane) => {
                if let Some(view) = self.panes.get(&pane).cloned() {
                    view.update(cx, |v, cx| v.close_find(window, cx));
                }
            }
            HomeShellAction::ToastClicked(id) => {
                if let Ok(terminal) = id.parse::<u64>() {
                    self.reveal_terminal_by_id(terminal, window, cx);
                }
            }
        }
    }

    fn render_home(&self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = KumuTheme::global(cx);
        let plugins = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .children(crate::plugins::specs().map(|spec| self.render_plugin_button(spec, cx)));
        let title_bar = crate::ui::titlebar::title_bar(
            "home-titlebar",
            theme,
            Some(plugins.into_any_element()),
        );

        div()
            .flex()
            .flex_col()
            .relative()
            .size_full()
            .font_family(theme.font_family.clone())
            .bg(theme.colors.background)
            .text_color(theme.colors.foreground)
            .child(title_bar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(self.shell_nsh.app_view.clone()),
            )
            .children(self.active_sheet.as_ref().map(|sheet| {
                div()
                    .absolute()
                    .inset_0()
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        if ev.keystroke.key == "escape" {
                            this.active_sheet = None;
                            this.focus_terminal(window, cx);
                            cx.notify();
                        }
                    }))
                    .child(sheet.clone())
            }))
            .children(self.active_dialog.as_ref().map(|dialog| {
                div()
                    .absolute()
                    .inset_0()
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        if ev.keystroke.key == "escape" {
                            this.active_dialog = None;
                            this.focus_terminal(window, cx);
                            cx.notify();
                        }
                    }))
                    .child(dialog.app_view.clone())
            }))
    }

    fn reveal_terminal_by_id(
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
            self.focus_pane(pane, cx);
            self.focus_terminal(window, cx);
        }
    }
}

impl Render for KumuApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.active_dialog.is_none()
            && self.active_sheet.is_none()
            && let Some(pane) = self.pending_focus.take()
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
        if !read.is_empty() {
            for &terminal in &read {
                self.unread_notifications.remove(&terminal);
                self.shell_nsh.dismiss_toast(terminal.to_string());
                cx.dismiss_system_notification(&terminal.to_string());
            }
        }
        let shell_snap = self.build_shell_snapshot(cx);
        self.shell_nsh.sync(shell_snap);
        self.render_home(window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{shell_prompt, submits_after_paste};

    /// Only a pane started as an agent may receive Enter after the paste; a
    /// shell would run the review notes as a command.
    #[test]
    fn only_an_agent_pane_takes_the_enter_after_a_paste() {
        assert!(submits_after_paste(Some(false)));
        assert!(!submits_after_paste(Some(true)));
        assert!(!submits_after_paste(None));
    }

    /// Without bracketed paste a shell would run every line, so its copy of
    /// the message is one line.
    #[test]
    fn a_shell_copy_of_a_message_stays_on_one_line() {
        assert_eq!(
            shell_prompt("请修改：\n\nsrc/a.rs:12\n这里先检查\n"),
            "请修改： src/a.rs:12 这里先检查"
        );
    }
}
