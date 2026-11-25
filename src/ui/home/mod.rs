//! Home window: projects on the left, the selected project's splitable pane
//! tree on the right. Owns the settings window handle.
//!
//! A project owns its panes. Splitting adds another cell to that project; a
//! cell can hold several terminals as tabs.

mod agents;
mod control_handler;
mod dialogs;
mod native_host;
mod panes;
mod plugins_host;
mod shell_nsh;
mod shortcuts;
mod sidebar;
mod sidebar_nsh;
mod sprites;
mod tabs;
mod tabs_nsh;
mod terminal;
mod workspace;

use std::collections::HashMap;

use crate::agent::launch::{PaneLaunch, session_launch};
use crate::agent::status::clean_osc_title;
use crate::app::layout::{PaneId, PaneNode, ProjectTabs, SplitAxis, TabId};
use crate::app::{NotificationSettings, Workspace, WorkspaceEvent};
use crate::nsh_ui::agent_picker::ActiveAgentPicker;
use crate::nsh_ui::command_palette::ActiveCommandPalette;
use crate::nsh_ui::dialogs::ActiveDialog;
use crate::nsh_ui::toast::{ToastItem, ToastOverlay};
use crate::theme::KumuTheme;
use crate::updates::Updater;
use agents::{AgentEntry, agent_display_name};
use gpui::*;
use native_host::NativeViewRegistry;
use plugins_host::{PLUGIN_MAX_WIDTH, PLUGIN_MIN_WIDTH, PLUGIN_WIDTH};
use shell_nsh::{
    CenterModeSnapshot, HomeShellAction, HomeShellSnapshot, LayoutNodeSnapshot, NshHomeShell,
    PluginPanelSnapshot,
};
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

/// The sidebar's opening width and the range the divider beside it lets the
/// user drag it through. The maximum still leaves room for the panes on the
/// app's minimum window width.
const SIDEBAR_WIDTH: f32 = 256.;
pub(super) const SIDEBAR_MIN_WIDTH: f32 = 180.;
pub(super) const SIDEBAR_MAX_WIDTH: f32 = 520.;

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
    /// Whether the home window currently has keyboard focus. A status change
    /// while it does not still becomes an unseen marker until the pane is
    /// clicked.
    window_active: bool,
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
    /// Whether the left sidebar is expanded (`⌘B`).
    sidebar_visible: bool,
    /// Title-bar sidebar toggle button currently hovered.
    hovered_sidebar_tooltip: bool,
    /// Pane temporarily maximized over its split siblings (`⌘⇧Enter`).
    zoomed_pane: Option<PaneId>,
    /// Root focus handle so window-level shortcuts work even on empty states.
    window_focus: FocusHandle,
    /// Open right-hand plugin panel, by registry id.
    open_plugin: Option<&'static str>,
    /// Live plugin panels. A panel is created when it opens and dropped when
    /// it closes, so a closed panel's refresh loop stops.
    plugin_views: HashMap<&'static str, AnyView>,
    /// Width of the open plugin panel in px, chosen by dragging its divider.
    plugin_width: f32,
    /// Monotonic token tracking in-flight left/right drawer slide animations
    /// so deferred terminal PTY resizes always settle on the latest transition.
    panel_anim_gen: u64,
    /// Title-bar plugin button currently hovered (for pure-GPUI hover tooltip).
    pub(super) hovered_plugin_tooltip: Option<&'static str>,
    /// Registry backing `"kumu.terminal"` and `"kumu.plugin"` native NSH slots.
    native_registry: NativeViewRegistry,
    /// Unified Pure Rust NSH home shell (sidebar + tabs + split pane grid + plugin slot).
    shell_nsh: NshHomeShell,
    /// GPUI-level foreground toast overlay above `shell_nsh` (so native terminal
    /// hitboxes never block hover or clicks on notification cards).
    toast_overlay: ToastOverlay,
    /// Full-window Pure Rust NSH `⌘K` command palette overlay (mounted only while open).
    active_palette: Option<ActiveCommandPalette>,
    /// Full-window Pure Rust NSH modal dialog overlay (mounted only while open).
    active_dialog: Option<ActiveDialog>,
    /// Anchored new-agent popover, mounted only while open (GPUI overlay, so
    /// presses over the terminal reach it).
    active_agent_picker: Option<ActiveAgentPicker>,
    /// Full-window Pure Rust NSH right-edge sheet overlay (e.g. Git Diff Review).
    pub(super) active_sheet: Option<crate::plugins::ActiveSheet>,
    /// Syncs KumuTheme updates into `shell_nsh` and the NSH overlays.
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
                    // Foreground window uses the GPUI-level NSH toast overlay above the home shell.
                    this.toast_overlay.push(
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
                        if next.is_shell() {
                            terminal.guard_post_exit_ctrl_c();
                        }
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
                if agent_changed {
                    // A different agent owns this pane now; the old state and
                    // marker must not make the new agent's first report look
                    // like a repeat.
                    entry.state = None;
                    entry.unseen = false;
                }
                entry.agent = agent.to_string();
                this.sync_pane_agent_status(pane, cx);
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
        cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            cx.notify();
        })
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
        let sidebar_visible =
            crate::db::get_setting(crate::db::SETTING_SIDEBAR_VISIBLE).as_deref() != Some("0");
        let plugin_width = crate::db::get_setting(crate::db::SETTING_PLUGIN_WIDTH)
            .and_then(|value| value.trim().parse::<f32>().ok())
            .filter(|width| width.is_finite())
            .map(|width| width.clamp(PLUGIN_MIN_WIDTH, PLUGIN_MAX_WIDTH))
            .unwrap_or(PLUGIN_WIDTH);

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
                sidebar_visible,
                sidebar_width,
                plugin_width,
                ..Default::default()
            },
            &native_registry,
            &initial_theme,
            window,
            cx,
            |app, action, window, cx| app.handle_shell_action(action, window, cx),
        );
        let toast_overlay = ToastOverlay::new(
            nsh_base::Anchor::TopRight,
            &initial_theme,
            window,
            cx,
            |app, id, window, cx| {
                if let Ok(terminal) = id.parse::<u64>() {
                    app.reveal_terminal_by_id(terminal, window, cx);
                } else {
                    app.focus_terminal(window, cx);
                }
            },
        );
        let nsh_theme_sub = cx.observe_global::<KumuTheme>(|app: &mut Self, cx| {
            let theme = KumuTheme::global(cx).clone();
            app.shell_nsh.sync_theme(&theme, cx);
            app.toast_overlay.sync_theme(&theme, cx);
            if let Some(palette) = &app.active_palette {
                palette.sync_theme(&theme, cx);
            }
            if let Some(dialog) = &app.active_dialog {
                dialog.sync_theme(&theme, cx);
            }
            if let Some(picker) = &app.active_agent_picker {
                picker.sync_theme(&theme, cx);
            }
        });
        let window_focus = cx.focus_handle();

        let mut app = Self {
            workspace,
            updater,
            settings_window: None,
            selected: None,
            unread_notifications: HashMap::new(),
            window_active: window.is_window_active(),
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
            sidebar_visible,
            hovered_sidebar_tooltip: false,
            zoomed_pane: None,
            window_focus,
            open_plugin: None,
            plugin_views: HashMap::new(),
            plugin_width,
            panel_anim_gen: 0,
            hovered_plugin_tooltip: None,
            native_registry,
            shell_nsh,
            toast_overlay,
            active_palette: None,
            active_dialog: None,
            active_agent_picker: None,
            active_sheet: None,
            _nsh_theme_sub: nsh_theme_sub,
            control: None,
        };
        if !app.restore_workspace(saved_workspace.as_deref(), cx) {
            app.start_default(cx);
        }
        app.start_control(cx);
        app.start_shell_agent_watcher(cx);
        app.focus_terminal(window, cx);
        app.sync_pane_chrome(cx);
        let shell_snap = app.build_shell_snapshot(cx);
        app.shell_nsh.sync(shell_snap);
        app.persist_workspace(cx);
        app
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
        let zoomed = self.active_zoomed_pane();
        let (center, pane_chrome) = match active_tab {
            Some(tab) if !tab.layout.panes().is_empty() => {
                let mut chrome = HashMap::new();
                for pane_id in tab.layout.panes() {
                    if let Some(view) = self.panes.get(&pane_id) {
                        chrome.insert(pane_id, view.read(cx).chrome_snapshot());
                    }
                }
                let layout = match zoomed {
                    Some(pane_id) => LayoutNodeSnapshot::Leaf(pane_id),
                    None => self.build_layout_snapshot(&tab.layout),
                };
                (CenterModeSnapshot::Panes(layout), chrome)
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
            sidebar_visible: self.sidebar_visible,
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
                self.close_plugin(cx);
            }
            HomeShellAction::PanelAnimationSettled => {
                self.finish_panel_resize_defer(cx);
            }
            HomeShellAction::PluginCloseFinished => {
                if self.open_plugin.is_none() {
                    self.plugin_views.clear();
                    self.native_registry.clear_plugins();
                }
            }
            HomeShellAction::ResizeSidebar(sidebar_width) => {
                let width = sidebar_width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
                if (width - self.sidebar_width).abs() >= 1.0 {
                    crate::db::set_setting(crate::db::SETTING_SIDEBAR_WIDTH, &format!("{width}"))
                        .ok();
                    self.sidebar_width = width;
                }
            }
            HomeShellAction::ResizePlugin(plugin_width) => {
                let width = plugin_width.clamp(PLUGIN_MIN_WIDTH, PLUGIN_MAX_WIDTH);
                if (width - self.plugin_width).abs() >= 1.0 {
                    crate::db::set_setting(crate::db::SETTING_PLUGIN_WIDTH, &format!("{width}"))
                        .ok();
                    self.plugin_width = width;
                }
            }
            HomeShellAction::ResizeGroup { group_id, ratios } => {
                self.group_ratios.insert(group_id, ratios);
                self.persist_workspace(cx);
            }
            HomeShellAction::FocusPane(pane) => {
                self.focus_pane_from_user(pane, cx);
                self.focus_terminal(window, cx);
                cx.notify();
            }
            HomeShellAction::ToggleZoomPane(pane) => {
                self.toggle_zoom_pane(pane, window, cx);
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
            HomeShellAction::OpenAgentPicker { pane, anchor } => {
                self.open_agent_picker(pane, anchor, window, cx);
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
        let title_bar = crate::ui::titlebar::title_bar_with_controls(
            "home-titlebar",
            theme,
            Some(self.render_sidebar_toggle_button(cx)),
            Some(plugins.into_any_element()),
        );

        div()
            .track_focus(&self.window_focus)
            .capture_key_down(cx.listener(Self::on_window_key_down))
            .on_key_down(cx.listener(Self::on_window_key_down_bubble))
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
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(self.shell_nsh.app_view.clone())
                    .children(self.toast_overlay.overlay_size().map(|(w, h)| {
                        div()
                            .absolute()
                            .top_0()
                            .right_0()
                            .w(px(w))
                            .h(px(h))
                            .child(self.toast_overlay.app_view.clone())
                    })),
            )
            .children(self.active_sheet.as_ref().map(|sheet| {
                div()
                    .absolute()
                    .inset_0()
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _window, cx| {
                        if ev.keystroke.key == "escape"
                            && let Some(sheet) = this.active_sheet.clone()
                        {
                            sheet.dismiss(cx);
                        }
                    }))
                    .child(sheet.view.clone())
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
            .children(self.active_agent_picker.as_ref().map(|picker| {
                div()
                    .absolute()
                    .inset_0()
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        if ev.keystroke.key == "escape" {
                            this.active_agent_picker = None;
                            this.focus_terminal(window, cx);
                            cx.notify();
                        }
                    }))
                    .child(picker.app_view.clone())
            }))
            .children(
                self.active_palette
                    .as_ref()
                    .map(|palette| div().absolute().inset_0().child(palette.app_view.clone())),
            )
    }
}

impl Render for KumuApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.active_palette.is_none()
            && self.active_dialog.is_none()
            && self.active_agent_picker.is_none()
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
                self.toast_overlay.dismiss(terminal.to_string());
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
