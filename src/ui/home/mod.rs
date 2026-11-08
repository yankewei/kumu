//! Home window: projects → sessions tree on the left, a splitable pane tree of
//! terminals on the right. Owns the settings window handle.
//!
//! A pane shows one session; splitting is layout only — sessions stay owned by
//! their project.

mod panes;
mod sidebar;
mod sprites;
mod terminal;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::SettingsWindow;
use crate::app::{Workspace, WorkspaceEvent};
use crate::ui::layers;
use crate::ui::tooltip::Tooltips;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;
use gpui_updater::Updater;
use panes::{PaneId, PaneNode};
use terminal::{TerminalEvent, TerminalView};

pub struct KumuApp {
    /// Projects, sessions and their live agents; the home view draws it and
    /// hands it every user action.
    workspace: Entity<Workspace>,
    /// Owned by the home window so closing Settings does not interrupt an update.
    updater: Entity<Updater>,
    settings_window: Option<WindowHandle<Root>>,
    /// Session highlighted in the sidebar — the focused pane's session.
    selected: Option<i64>,
    /// Projects collapsed in the sidebar tree.
    collapsed_projects: HashSet<i64>,
    /// Hover labels for the sidebar tree (see [`Tooltips`]).
    tooltips: Entity<Tooltips>,
    /// Terminal views keyed by pane id.
    panes: HashMap<PaneId, Entity<TerminalView>>,
    /// How the content area is split.
    layout: PaneNode,
    /// Pane that receives sidebar selection and keyboard focus.
    focused: PaneId,
    /// Focus this pane's terminal on the next paint (after a split / select).
    pending_focus: Option<PaneId>,
    next_pane: u64,
    next_group: u64,
}

impl KumuApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = cx.new(Workspace::new);
        let tooltips = cx.new(|cx| Tooltips::new(cx));
        // Only what the home view itself draws. A terminal frame is the pane's
        // own, subscribed inside [`TerminalView`].
        cx.subscribe(&workspace, |this, _, ev, cx| match ev {
            WorkspaceEvent::Changed => cx.notify(),
            WorkspaceEvent::SessionClosed { id, next } => {
                this.on_session_closed(*id, *next, cx);
            }
            _ => {}
        })
        .detach();

        let pane = PaneId(1);
        let terminal =
            cx.new(|cx| TerminalView::new(pane, workspace.clone(), tooltips.clone(), cx));
        cx.subscribe(&terminal, move |this, _, ev, cx| {
            this.on_terminal_event(pane, ev, cx);
        })
        .detach();

        let updater = crate::updates::init(cx);
        cx.observe(&updater, |_, _, cx| cx.notify()).detach();

        let mut app = Self {
            workspace,
            updater,
            settings_window: None,
            selected: None,
            collapsed_projects: HashSet::new(),
            tooltips,
            panes: HashMap::from([(pane, terminal)]),
            layout: PaneNode::leaf(pane),
            focused: pane,
            pending_focus: None,
            next_pane: 2,
            next_group: 1,
        };
        if let Some(first) = app.workspace.read(cx).sessions().first().map(|s| s.id) {
            app.select_session(first, cx);
            app.focus_terminal(window, cx);
        }
        app.sync_pane_chrome(cx);
        app
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
        self.focused = pane;
        self.selected = self.panes.get(&pane).and_then(|t| t.read(cx).session_id());
        self.sync_pane_chrome(cx);
        cx.notify();
    }

    fn focus_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.panes.get(&self.focused).cloned() {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
        }
    }

    fn select_session(&mut self, id: i64, cx: &mut Context<Self>) {
        // A session already on screen: jump to its pane instead of duplicating.
        if let Some((pane, _)) = self
            .panes
            .iter()
            .find(|(_, t)| t.read(cx).session_id() == Some(id))
        {
            let pane = *pane;
            self.focused = pane;
            self.selected = Some(id);
            self.sync_pane_chrome(cx);
            cx.notify();
            return;
        }
        self.selected = Some(id);
        if let Some(terminal) = self.panes.get(&self.focused).cloned() {
            terminal.update(cx, |terminal, cx| terminal.show(id, cx));
        }
        self.sync_pane_chrome(cx);
        cx.notify();
    }

    /// Split `pane` along `axis`, open a new shell in the same project, and
    /// focus the new pane.
    fn split_pane(&mut self, pane: PaneId, axis: Axis, cx: &mut Context<Self>) {
        if !self.panes.contains_key(&pane) {
            return;
        }
        let project_id = self
            .panes
            .get(&pane)
            .and_then(|t| t.read(cx).session_id())
            .and_then(|sid| self.workspace.read(cx).session(sid).map(|s| s.project_id));
        let Some(session_id) = self
            .workspace
            .update(cx, |workspace, cx| workspace.new_session(project_id, cx))
        else {
            return;
        };

        let new_id = PaneId(self.next_pane);
        self.next_pane += 1;
        let group_id = self.next_group;
        self.next_group += 1;
        if !self.layout.split(pane, axis, new_id, group_id) {
            self.workspace
                .update(cx, |workspace, cx| workspace.delete_session(session_id, cx));
            return;
        }

        let workspace = self.workspace.clone();
        let tooltips = self.tooltips.clone();
        let terminal = cx.new(|cx| TerminalView::new(new_id, workspace, tooltips, cx));
        terminal.update(cx, |terminal, cx| terminal.show(session_id, cx));
        cx.subscribe(&terminal, move |this, _, ev, cx| {
            this.on_terminal_event(new_id, ev, cx);
        })
        .detach();
        self.panes.insert(new_id, terminal);
        self.focused = new_id;
        self.selected = Some(session_id);
        self.pending_focus = Some(new_id);
        self.sync_pane_chrome(cx);
        cx.notify();
    }

    fn close_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let Some(next) = self.layout.neighbor(pane) else {
            return;
        };
        if !self.layout.remove(pane) {
            return;
        }
        self.panes.remove(&pane);
        self.focused = next;
        self.selected = self.panes.get(&next).and_then(|t| t.read(cx).session_id());
        self.pending_focus = Some(next);
        self.sync_pane_chrome(cx);
        cx.notify();
    }

    fn sync_pane_chrome(&mut self, cx: &mut Context<Self>) {
        let can_close = self.panes.len() > 1;
        let focused = self.focused;
        for (id, terminal) in &self.panes {
            let active = *id == focused;
            terminal.update(cx, |terminal, cx| {
                terminal.set_chrome(active, can_close, cx);
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
            cx.notify();
        }
    }

    /// Ask before deleting: the session and its whole history go away for good.
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
                .description(format!(
                    "「{title}」的全部消息将被永久删除，此操作无法撤销。"
                ))
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
        let next = self
            .workspace
            .update(cx, |workspace, cx| workspace.delete_session(session_id, cx));
        self.on_session_closed(session_id, next, cx);
    }

    /// After a session disappears — deleted by the user, or its shell exited —
    /// drop any split pane that was showing it so a sibling fills the space.
    /// The last remaining pane keeps the slot: show a neighbor session, or clear.
    fn on_session_closed(&mut self, session_id: i64, next: Option<i64>, cx: &mut Context<Self>) {
        let showing: Vec<PaneId> = self
            .panes
            .iter()
            .filter(|(_, t)| t.read(cx).session_id() == Some(session_id))
            .map(|(id, _)| *id)
            .collect();
        for pane in showing {
            if self.panes.len() > 1 {
                self.close_pane(pane, cx);
                continue;
            }
            let replacement = next.filter(|n| self.workspace.read(cx).session(*n).is_some());
            if let Some(terminal) = self.panes.get(&pane).cloned() {
                match replacement {
                    Some(id) if !self.pane_shows(id, cx) => {
                        terminal.update(cx, |terminal, cx| terminal.show(id, cx));
                    }
                    _ => {
                        terminal.update(cx, |terminal, cx| terminal.clear(cx));
                    }
                }
            }
        }
        if self.selected == Some(session_id) {
            self.selected = self
                .panes
                .get(&self.focused)
                .and_then(|t| t.read(cx).session_id())
                .or(next.filter(|n| self.workspace.read(cx).session(*n).is_some()));
        }
        self.sync_pane_chrome(cx);
        cx.notify();
    }

    fn pane_shows(&self, session_id: i64, cx: &App) -> bool {
        self.panes
            .values()
            .any(|t| t.read(cx).session_id() == Some(session_id))
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
        let workspace = self.workspace.clone();
        let updater = self.updater.clone();
        if let Ok(handle) = cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| SettingsWindow::new(workspace, updater, window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        }) {
            handle.update(cx, activate).ok();
            self.settings_window = Some(handle);
        }
    }

    fn render_panes(&self, cx: &Context<Self>) -> AnyElement {
        self.render_node(&self.layout, cx)
    }

    fn render_node(&self, node: &PaneNode, _cx: &Context<Self>) -> AnyElement {
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
                for child in children {
                    group = group.child(resizable_panel().child(self.render_node(child, _cx)));
                }
                group.into_any_element()
            }
        }
    }

    fn render_home(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let overlays = layers::window_layers(&self.tooltips, window, cx);
        let theme = cx.theme();
        let has_sessions = !self.workspace.read(cx).sessions().is_empty();
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
        self.render_home(window, cx)
    }
}
