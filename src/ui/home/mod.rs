//! Home window: projects → sessions tree on the left, the selected session's
//! splitable pane tree on the right. Owns the settings window handle.
//!
//! A session belongs to a project and owns its panes. Splitting adds another
//! pane (and its own shell) to that session; the sidebar lists sessions.

mod panes;
mod sidebar;
mod sprites;
mod terminal;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::SettingsWindow;
use crate::app::{NotificationSettings, Workspace, WorkspaceEvent};
use crate::ui::layers;
use crate::ui::tooltip::Tooltips;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::{Notification, NotificationDelivery};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;
use gpui_updater::Updater;
use panes::{
    PaneId, PaneNode, SavedPane, SavedSessionLayout, SavedWorkspace, WORKSPACE_STATE_VERSION,
};
use terminal::{TerminalEvent, TerminalView};

/// Arbitrary units fed to `ResizableState::insert_panel` when restoring split
/// ratios. Only the relative values matter: the group rescales them to the
/// container's real size on its first paint.
const RATIO_SCALE: f32 = 10_000.;

pub struct KumuApp {
    /// Projects, sessions and their live agents; the home view draws it and
    /// hands it every user action.
    workspace: Entity<Workspace>,
    /// Owned by the home window so closing Settings does not interrupt an update.
    updater: Entity<Updater>,
    settings_window: Option<WindowHandle<Root>>,
    /// Session currently shown in the content area.
    selected: Option<i64>,
    /// Terminals that requested attention while unattended, mapped to their
    /// session for the sidebar marker.
    unread_notifications: HashMap<u64, i64>,
    /// A persistence failure the workspace reported, shown above the panes
    /// until dismissed.
    store_error: Option<String>,
    /// Projects collapsed in the sidebar tree.
    collapsed_projects: HashSet<i64>,
    /// Hover labels for the sidebar tree (see [`Tooltips`]).
    tooltips: Entity<Tooltips>,
    /// Terminal views keyed by pane id.
    panes: HashMap<PaneId, Entity<TerminalView>>,
    /// Each sidebar session owns an independent pane tree.
    layouts: HashMap<i64, PaneNode>,
    /// Last focused pane in each session's tree.
    focused_by_session: HashMap<i64, PaneId>,
    /// Resizable state per split group, so drag sizes can be saved and restored.
    group_states: HashMap<u64, Entity<ResizableState>>,
    /// Focused pane in the selected session.
    focused: PaneId,
    /// Focus this pane's terminal on the next paint (after a split / select).
    pending_focus: Option<PaneId>,
    next_pane: u64,
    next_group: u64,
    /// The JSON last written for this layout, so repeated focus/select calls do
    /// not rewrite identical state.
    last_saved_layout: Option<String>,
}

impl KumuApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = cx.new(Workspace::new);
        let startup_error = workspace.update(cx, |workspace, _| workspace.take_startup_error());
        let saved_workspace = workspace.update(cx, |workspace, _| workspace.take_saved_workspace());
        let last_saved_layout = saved_workspace.clone();
        let tooltips = cx.new(|cx| Tooltips::new(cx));
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
                session,
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
                let Some(session_info) = workspace.session(*session) else {
                    return;
                };
                if cx.global::<NotificationSettings>().sound {
                    window.play_system_bell();
                }
                let project = workspace
                    .projects()
                    .iter()
                    .find(|project| project.id == session_info.project_id);
                let title = match project {
                    Some(project) => format!("{} · {}", project.name, session_info.title),
                    None => session_info.title.clone(),
                };
                let message = match (notification.title.is_empty(), notification.body.is_empty()) {
                    (true, true) => "终端需要你的关注".to_string(),
                    (false, true) => notification.title.clone(),
                    (true, false) => notification.body.clone(),
                    (false, false) => format!("{}\n{}", notification.title, notification.body),
                };
                this.unread_notifications.insert(*terminal, *session);
                let terminal = *terminal;
                let weak = cx.entity().downgrade();
                let note = Notification::info(message)
                    .title(title)
                    .id1::<NotificationSettings>(terminal.to_string())
                    .delivery(NotificationDelivery::InApp)
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            let target = this.workspace.update(cx, |workspace, _| {
                                workspace.reveal_terminal(terminal, this.focused.0)
                            });
                            if let Some((pane, session)) = target {
                                let pane = PaneId(pane);
                                let snapshot = this.workspace.read(cx).terminal_snapshot(pane.0);
                                if let Some(view) = this.panes.get(&pane)
                                    && let Some(snapshot) = snapshot
                                {
                                    view.update(cx, |view, cx| view.attach(session, snapshot, cx));
                                }
                                this.focus_pane(pane, cx);
                                this.focus_terminal(window, cx);
                            }
                        });
                    });
                // Keep a toast available when OS delivery is denied or when
                // developing with an unbundled `cargo run` binary.
                window.push_notification(
                    if window.is_window_active() {
                        note
                    } else {
                        note.in_app_and_system()
                    },
                    cx,
                );
                cx.notify();
            }
            WorkspaceEvent::PaneExited { pane, session } => {
                this.on_pane_exited(PaneId(*pane), *session, cx);
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

        // Save the window's geometry when it closes. The app-quit path does not
        // hand the callback a window, so it reads the same bounds through the
        // handle; either path can fire first and they write the same value.
        window.on_window_should_close(cx, |window, _cx| {
            crate::window_state::save(window);
            true
        });
        let window_handle = window.window_handle();
        cx.on_app_quit(move |_, cx| {
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
            collapsed_projects: HashSet::new(),
            tooltips,
            panes: HashMap::new(),
            layouts: HashMap::new(),
            focused_by_session: HashMap::new(),
            group_states: HashMap::new(),
            focused: PaneId(1),
            pending_focus: None,
            next_pane: 2,
            next_group: 1,
            last_saved_layout,
        };
        if !app.restore_workspace(saved_workspace.as_deref(), cx) {
            app.start_default(cx);
        }
        app.focus_terminal(window, cx);
        app.sync_pane_chrome(cx);
        app.persist_workspace(cx);
        app
    }

    /// Show the first session in a single pane — the first run, and the
    /// fallback whenever a saved layout cannot be used.
    fn start_default(&mut self, cx: &mut Context<Self>) {
        let pane = PaneId(1);
        let Some(session) = self.workspace.read(cx).sessions().first().map(|s| s.id) else {
            return;
        };
        self.layouts.insert(session, PaneNode::leaf(pane));
        self.focused_by_session.insert(session, pane);
        self.selected = Some(session);
        self.focused = pane;
        let terminal = self.install_pane(pane, cx);
        terminal.update(cx, |terminal, cx| terminal.show(session, cx));
        self.next_pane = 2;
        self.next_group = 1;
    }

    /// Recreate the saved pane tree and terminals, or return false so the
    /// caller falls back to [`Self::start_default`]. A layout is accepted only
    /// when its JSON and version are known and every pane still has a session:
    /// a stale row, a corrupt document, or an absurd pane count must never
    /// prevent the window from opening.
    fn restore_workspace(&mut self, saved: Option<&str>, cx: &mut Context<Self>) -> bool {
        let Some(saved) = saved else {
            return false;
        };
        let workspace = self.workspace.clone();
        let Some(restored) = SavedWorkspace::restore(
            saved,
            |session| workspace.read(cx).session(session).is_some(),
            |project| {
                workspace
                    .read(cx)
                    .projects()
                    .iter()
                    .any(|p| p.id == project)
            },
        ) else {
            return false;
        };

        let panes::RestoredWorkspace {
            layouts,
            focused_by_session,
            selected,
            collapsed_projects,
            group_sizes,
            next_pane,
            next_group,
            ..
        } = restored;
        self.layouts = layouts;
        self.focused_by_session = focused_by_session;
        self.selected = selected;
        self.next_pane = next_pane;
        self.next_group = next_group;
        self.collapsed_projects = collapsed_projects.into_iter().collect();
        self.group_states.clear();
        for (group, ratios) in group_sizes {
            let state = Self::new_group_state(&ratios, cx);
            self.group_states.insert(group, state);
        }
        if let Some(session) = self.selected {
            self.show_session_panes(session, cx);
        }
        self.focused = self
            .selected
            .and_then(|session| self.focused_by_session.get(&session).copied())
            .unwrap_or(PaneId(1));
        true
    }

    /// Materialize and attach every pane in the selected session. Other
    /// sessions keep their saved trees and create their views when selected.
    fn show_session_panes(&mut self, session: i64, cx: &mut Context<Self>) {
        let Some(layout) = self.layouts.get(&session) else {
            return;
        };
        let pane_ids = layout.panes();
        for pane in pane_ids {
            let terminal = if let Some(terminal) = self.panes.get(&pane).cloned() {
                terminal
            } else {
                self.install_pane(pane, cx)
            };
            terminal.update(cx, |terminal, cx| terminal.show(session, cx));
        }
    }

    /// Create and subscribe one pane's terminal view.
    fn install_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) -> Entity<TerminalView> {
        let workspace = self.workspace.clone();
        let tooltips = self.tooltips.clone();
        let terminal = cx.new(|cx| TerminalView::new(pane, workspace, tooltips, cx));
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

    /// Write the pane tree, its sessions, the focused pane and the collapsed
    /// projects for the next launch. Identical state is not rewritten, so
    /// selecting or clicking around does not turn into a stream of writes.
    fn persist_workspace(&mut self, cx: &mut Context<Self>) {
        // Group ids are unique across session layouts and stale groups can be
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
        let mut sessions: Vec<SavedSessionLayout> = self
            .layouts
            .iter()
            .filter_map(|(session, layout)| {
                let focused = self
                    .focused_by_session
                    .get(session)
                    .copied()
                    .filter(|pane| layout.panes().contains(pane))
                    .or_else(|| layout.panes().first().copied())?;
                Some(SavedSessionLayout {
                    session: *session,
                    layout: layout.to_saved(&ratios),
                    focused: focused.0,
                })
            })
            .collect();
        sessions.sort_by_key(|session| session.session);
        let mut collapsed: Vec<i64> = self.collapsed_projects.iter().copied().collect();
        collapsed.sort_unstable();
        let mut saved_panes: Vec<SavedPane> = self
            .layouts
            .iter()
            .flat_map(|(session, layout)| {
                layout.panes().into_iter().map(|pane| SavedPane {
                    pane: pane.0,
                    session: *session,
                })
            })
            .collect();
        saved_panes.sort_by_key(|pane| pane.pane);
        let state = SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: panes::SavedNode::SessionLayouts {
                selected: self.selected,
                sessions,
            },
            panes: saved_panes,
            focused: self.focused.0,
            collapsed_projects: collapsed,
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
            TerminalEvent::Close => self.close_pane(pane, cx),
        }
    }

    fn focus_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if !self.panes.contains_key(&pane) {
            return;
        }
        let session = self
            .panes
            .get(&pane)
            .and_then(|terminal| terminal.read(cx).session_id());
        if let Some(session) = session {
            if self.selected != Some(session) {
                if let Some(previous) = self.selected {
                    self.hide_session_panes(previous, cx);
                }
                self.selected = Some(session);
                self.show_session_panes(session, cx);
            }
            self.focused_by_session.insert(session, pane);
        }
        self.focused = pane;
        if let Some(session) = self.selected {
            self.workspace.read(cx).touch_session(session);
        }
        self.sync_pane_chrome(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    fn focus_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.panes.get(&self.focused).cloned() {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
        }
    }

    fn select_session(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.selected != Some(id) {
            if let Some(previous) = self.selected {
                self.hide_session_panes(previous, cx);
            }
        }
        self.ensure_session_layout(id, cx);
        self.selected = Some(id);
        self.focused = self
            .focused_by_session
            .get(&id)
            .copied()
            .or_else(|| self.layouts.get(&id)?.panes().first().copied())
            .unwrap_or(PaneId(1));
        self.focused_by_session.insert(id, self.focused);
        self.show_session_panes(id, cx);
        self.workspace.read(cx).touch_session(id);
        self.sync_pane_chrome(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    fn ensure_session_layout(&mut self, session: i64, cx: &mut Context<Self>) {
        if self.layouts.contains_key(&session) {
            return;
        }
        let pane = PaneId(self.next_pane);
        self.next_pane += 1;
        self.layouts.insert(session, PaneNode::leaf(pane));
        self.focused_by_session.insert(session, pane);
        self.install_pane(pane, cx);
    }

    fn hide_session_panes(&mut self, session: i64, cx: &mut Context<Self>) {
        let Some(layout) = self.layouts.get(&session) else {
            return;
        };
        for pane in layout.panes() {
            if let Some(terminal) = self.panes.get(&pane).cloned() {
                terminal.update(cx, |terminal, cx| terminal.hide(cx));
            }
        }
    }

    /// Split `pane` along `axis`, open another shell under the same session,
    /// and focus the new pane. No new sidebar row.
    fn split_pane(&mut self, pane: PaneId, axis: Axis, cx: &mut Context<Self>) {
        if !self.panes.contains_key(&pane) {
            return;
        }
        let Some(session_id) = self
            .panes
            .get(&pane)
            .and_then(|t| t.read(cx).session_id())
            .filter(|&sid| self.workspace.read(cx).session(sid).is_some())
        else {
            return;
        };

        let new_id = PaneId(self.next_pane);
        self.next_pane += 1;
        let group_id = self.next_group;
        self.next_group += 1;
        let Some(layout) = self.layouts.get_mut(&session_id) else {
            return;
        };
        if !layout.split(pane, axis, new_id, group_id) {
            return;
        }
        if layout.contains_group(group_id) {
            let state = Self::new_group_state(&[], cx);
            self.group_states.insert(group_id, state);
        }

        let terminal = self.install_pane(new_id, cx);
        terminal.update(cx, |terminal, cx| terminal.show(session_id, cx));
        self.focused = new_id;
        self.selected = Some(session_id);
        self.focused_by_session.insert(session_id, new_id);
        self.pending_focus = Some(new_id);
        self.sync_pane_chrome(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    fn close_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let session_id = self.panes.get(&pane).and_then(|t| t.read(cx).session_id());
        let Some(session_id) = session_id else {
            return;
        };
        let Some(layout) = self.layouts.get_mut(&session_id) else {
            return;
        };
        let Some(next) = layout.neighbor(pane) else {
            return;
        };
        if !layout.remove(pane) {
            return;
        }
        self.workspace.update(cx, |workspace, _| {
            workspace.drop_terminal(pane.0);
        });
        self.panes.remove(&pane);
        self.focused = next;
        self.focused_by_session.insert(session_id, next);
        self.selected = Some(session_id);
        self.pending_focus = Some(next);
        // Last pane for this session: drop the sidebar row so Close does not
        // leave an orphan "终端".
        if !self.pane_shows(session_id, cx) && self.workspace.read(cx).session(session_id).is_some()
        {
            self.workspace.update(cx, |workspace, cx| {
                let _ = workspace.delete_session(session_id, cx);
            });
            if self.selected == Some(session_id) {
                self.selected = self
                    .panes
                    .get(&self.focused)
                    .and_then(|t| t.read(cx).session_id());
            }
        }
        self.sync_pane_chrome(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    /// A pane's shell exited: close that pane (or clear the last one), then
    /// drop the session row when no pane still belongs to it.
    fn on_pane_exited(&mut self, pane: PaneId, session_id: i64, cx: &mut Context<Self>) {
        let Some(panes) = self.layouts.get(&session_id).map(PaneNode::panes) else {
            return;
        };
        if panes.len() > 1 {
            let Some(layout) = self.layouts.get_mut(&session_id) else {
                return;
            };
            let Some(next) = layout.neighbor(pane) else {
                return;
            };
            if !layout.remove(pane) {
                return;
            }
            self.panes.remove(&pane);
            if self.focused_by_session.get(&session_id) == Some(&pane) {
                self.focused_by_session.insert(session_id, next);
                if self.selected == Some(session_id) {
                    self.focused = next;
                    self.pending_focus = Some(next);
                }
            }
        } else {
            if let Some(terminal) = self.panes.get(&pane).cloned() {
                terminal.update(cx, |terminal, cx| terminal.clear(cx));
            }
            if self.workspace.read(cx).session(session_id).is_some() {
                let next = self
                    .workspace
                    .update(cx, |workspace, cx| workspace.delete_session(session_id, cx))
                    .ok()
                    .flatten();
                self.on_session_closed(session_id, next, cx);
                return;
            }
        }
        self.sync_pane_chrome(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    fn sync_pane_chrome(&mut self, cx: &mut Context<Self>) {
        let visible: HashSet<PaneId> = self
            .selected
            .and_then(|session| self.layouts.get(&session))
            .map(PaneNode::panes)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let can_close = visible.len() > 1;
        let focused = self.focused;
        for (id, terminal) in &self.panes {
            let active = visible.contains(id) && *id == focused;
            terminal.update(cx, |terminal, cx| {
                terminal.set_chrome(active, visible.contains(id) && can_close, cx);
            });
        }
    }

    /// Start a shell session — in `project_id`, or the first project — and show it.
    fn new_session(&mut self, project_id: Option<i64>, cx: &mut Context<Self>) {
        let id = self
            .workspace
            .update(cx, |workspace, cx| workspace.new_session(project_id, cx));
        if let Some(id) = id {
            self.select_session(id, cx);
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

    /// Show the picked project, expanded — whether it was just added or was
    /// one already.
    fn finish_add_project(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let path = path.to_string_lossy().into_owned();
        let id = self
            .workspace
            .update(cx, |workspace, cx| workspace.add_project(&path, cx));
        if let Some(id) = id {
            self.collapsed_projects.remove(&id);
            self.persist_workspace(cx);
            cx.notify();
        }
    }

    /// Rename a project: the dialog starts on the current name, like the
    /// session dialog.
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
        });
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
    }

    /// Ask before deleting a project and every session under it.
    fn confirm_delete_project(&self, project_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let (name, count) = {
            let workspace = self.workspace.read(cx);
            let name = workspace
                .projects()
                .iter()
                .find(|project| project.id == project_id)
                .map(|project| project.name.clone())
                .unwrap_or_default();
            let count = workspace
                .sessions()
                .iter()
                .filter(|session| session.project_id == project_id)
                .count();
            (name, count)
        };
        let description = if count == 0 {
            format!("「{name}」将被删除，此操作无法撤销。")
        } else {
            format!("「{name}」下的 {count} 个会话及其终端将被关闭，此操作无法撤销。")
        };
        // The dialog outlives this call, so it reaches the view weakly.
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let weak = weak.clone();
            alert
                .title("删除项目")
                .description(description.clone())
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("删除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    let _ = weak.update(cx, move |app, cx| app.delete_project(project_id, cx));
                    true
                })
        });
    }

    /// Delete a project and close every pane that was showing one of its
    /// sessions, landing them on a session that survived.
    fn delete_project(&mut self, project_id: i64, cx: &mut Context<Self>) {
        let sessions: Vec<i64> = self
            .workspace
            .read(cx)
            .sessions()
            .iter()
            .filter(|session| session.project_id == project_id)
            .map(|session| session.id)
            .collect();
        let Ok(next) = self
            .workspace
            .update(cx, |workspace, cx| workspace.delete_project(project_id, cx))
        else {
            // The failure is already on its way to the banner; leave the
            // project and its panes exactly where they are.
            return;
        };
        self.collapsed_projects.remove(&project_id);
        for session_id in sessions {
            self.on_session_closed(session_id, next, cx);
        }
        self.persist_workspace(cx);
        cx.notify();
    }

    /// Ask before deleting: the session and the terminals bound to it go away
    /// for good.
    fn confirm_delete_session(&self, session_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        // The dialog outlives this call, so it reaches the view weakly.
        let weak = cx.weak_entity();
        let title = self
            .workspace
            .read(cx)
            .session(session_id)
            .map(|s| s.title.clone())
            .unwrap_or_default();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let weak = weak.clone();
            alert
                .title("删除会话")
                .description(format!("「{title}」及其终端将被永久删除，此操作无法撤销。"))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("删除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    let _ = weak.update(cx, |app, cx| app.delete_session(session_id, cx));
                    true
                })
        });
    }

    /// Rename a session: the dialog's field starts on the current name, so the
    /// name can be replaced in one go.
    fn prompt_rename_session(&self, session_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let title = self
            .workspace
            .read(cx)
            .session(session_id)
            .map(|s| s.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(title));
        // The dialog outlives this call, so it reaches the view weakly.
        let weak = cx.weak_entity();
        let dialog_input = input.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("重命名会话")
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
                                    workspace.rename_session(session_id, name, cx)
                                })
                            });
                        }
                        true
                    }
                })
        });
        // Focusing after opening leaves the input the last handle the window
        // focused; the dialog only traps Tab, so nothing steals it back.
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
    }

    /// Delete a session, then land on its neighbor if it was the one on screen.
    fn delete_session(&mut self, session_id: i64, cx: &mut Context<Self>) {
        let Ok(next) = self
            .workspace
            .update(cx, |workspace, cx| workspace.delete_session(session_id, cx))
        else {
            // The failure is already on its way to the banner; leave the
            // session and its panes exactly where they are.
            return;
        };
        self.on_session_closed(session_id, next, cx);
    }

    /// After a session disappears, discard its pane tree and show a surviving
    /// session if the deleted one was active.
    fn on_session_closed(&mut self, session_id: i64, next: Option<i64>, cx: &mut Context<Self>) {
        if let Some(layout) = self.layouts.remove(&session_id) {
            for pane in layout.panes() {
                self.panes.remove(&pane);
            }
        }
        self.focused_by_session.remove(&session_id);
        if self.selected == Some(session_id) {
            let replacement = next
                .filter(|id| self.workspace.read(cx).session(*id).is_some())
                .or_else(|| self.workspace.read(cx).sessions().first().map(|s| s.id));
            self.selected = replacement;
            if let Some(replacement) = replacement {
                self.select_session(replacement, cx);
            } else {
                self.focused = PaneId(1);
            }
        }
        self.sync_pane_chrome(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    fn pane_shows(&self, session_id: i64, cx: &App) -> bool {
        self.layouts.get(&session_id).is_some_and(|layout| {
            layout.panes().iter().any(|pane| {
                self.panes
                    .get(pane)
                    .is_some_and(|terminal| terminal.read(cx).session_id() == Some(session_id))
            })
        })
    }

    /// Focus the settings window if it's open, else open a new one (resizable,
    /// opens at its minimum size).
    fn open_settings(&mut self, show_updates: bool, cx: &mut Context<Self>) {
        let activate = |root: &mut Root, window: &mut Window, cx: &mut Context<Root>| {
            if show_updates && let Ok(settings) = root.view().clone().downcast::<SettingsWindow>() {
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
        if let Ok(handle) = cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| SettingsWindow::new(updater, window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        }) {
            handle.update(cx, activate).ok();
            self.settings_window = Some(handle);
        }
    }

    fn render_panes(&self, cx: &Context<Self>) -> AnyElement {
        self.selected
            .and_then(|session| self.layouts.get(&session))
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

    fn render_home(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let overlays = layers::window_layers(&self.tooltips, window, cx);
        let theme = cx.theme();
        let warning = theme.warning;
        // Keep the pane tree if a pane still points at a live session — do not
        // blank the whole split just because the exited session was removed.
        let pane_sessions: Vec<Option<i64>> = self
            .panes
            .values()
            .map(|t| t.read(cx).session_id())
            .collect();
        let workspace = self.workspace.read(cx);
        let has_sessions = !workspace.sessions().is_empty()
            || pane_sessions
                .iter()
                .any(|id| id.is_some_and(|id| workspace.session(id).is_some()));
        let right =
            if has_sessions {
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .min_h_0()
                    .child(self.render_panes(cx))
            } else {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .text_color(theme.muted_foreground)
                    .child(div().text_sm().child("还没有会话"))
                    .child(Button::new("empty-new").ghost().label("新建终端").on_click(
                        cx.listener(|this, _, window, cx| {
                            this.new_session(None, cx);
                            this.focus_terminal(window, cx);
                            cx.notify();
                        }),
                    ))
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
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(div().w_px().h_full().bg(theme.border))
                    .child(right),
            )
            .child(overlays)
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
        // Also retract notifications when a session is deleted or the setting
        // is disabled, including banners still in Notification Center.
        let enabled = cx.global::<NotificationSettings>().enabled;
        let workspace = self.workspace.read(cx);
        let focused = window.is_window_active().then_some(self.focused.0);
        let read: Vec<u64> = self
            .unread_notifications
            .iter()
            .filter(|(terminal, session)| {
                !enabled
                    || workspace.session(**session).is_none()
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
