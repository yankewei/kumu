//! Home window: projects → sessions tree on the left, the selected session's
//! terminal on the right. Owns the settings window handle.

mod sidebar;
mod sprites;
mod terminal;

use std::collections::HashSet;
use std::path::PathBuf;

use super::SettingsWindow;
use crate::app::{Workspace, WorkspaceEvent};
use crate::db::Session;
use crate::ui::layers;
use crate::ui::tooltip::Tooltips;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

pub struct KumuApp {
    /// Projects, sessions and their live agents; the home view draws it and
    /// hands it every user action.
    workspace: Entity<Workspace>,
    settings_window: Option<WindowHandle<Root>>,
    selected: Option<i64>,
    /// Projects collapsed in the sidebar tree.
    collapsed_projects: HashSet<i64>,
    /// Hover labels for the sidebar tree (see [`Tooltips`]).
    tooltips: Entity<Tooltips>,
    /// Native-agent TUI for the selected session.
    terminal: Entity<terminal::TerminalView>,
}

impl KumuApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = cx.new(Workspace::new);
        let terminal_workspace = workspace.clone();
        // Only what the home view itself draws. A terminal frame is the pane's
        // own, subscribed inside [`TerminalView`].
        cx.subscribe(&workspace, |this, _, ev, cx| {
            match ev {
                WorkspaceEvent::Changed => cx.notify(),
                WorkspaceEvent::SessionClosed { id, next } => {
                    this.on_session_closed(*id, *next, cx);
                }
                _ => {}
            }
        })
        .detach();
        let mut app = Self {
            workspace,
            settings_window: None,
            selected: None,
            collapsed_projects: HashSet::new(),
            tooltips: cx.new(|cx| Tooltips::new(cx)),
            terminal: cx.new(|cx| terminal::TerminalView::new(terminal_workspace, window, cx)),
        };
        if let Some(first) = app.workspace.read(cx).sessions().first().map(|s| s.id) {
            app.select_session(first, cx);
            app.focus_terminal(window, cx);
        }
        app
    }

    fn select_session(&mut self, id: i64, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.terminal
            .update(cx, |terminal, cx| terminal.show(id, cx));
    }

    fn focus_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal
            .update(cx, |terminal, cx| terminal.focus(window, cx));
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
    /// keep the right pane on a neighbor when that session was selected.
    fn on_session_closed(&mut self, session_id: i64, next: Option<i64>, cx: &mut Context<Self>) {
        if self.selected == Some(session_id) {
            match next.filter(|next| self.workspace.read(cx).session(*next).is_some()) {
                Some(next) => self.select_session(next, cx),
                None => {
                    self.selected = None;
                    self.terminal.update(cx, |terminal, cx| terminal.clear(cx));
                }
            }
        }
        cx.notify();
    }

    /// The session on screen, as the workspace has it now.
    fn selected_session(&self, cx: &App) -> Option<Session> {
        self.workspace.read(cx).session(self.selected?).cloned()
    }

    /// Focus the settings window if it's open, else open a new one (resizable,
    /// opens at its minimum size).
    fn open_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = &self.settings_window
            && handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
        {
            return;
        }
        let mut options = TitleBar::window_options();
        options.window_bounds = Some(WindowBounds::centered(size(px(680.), px(640.)), cx));
        options.window_min_size = Some(size(px(680.), px(640.)));
        let workspace = self.workspace.clone();
        if let Ok(handle) = cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| SettingsWindow::new(workspace, window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        }) {
            handle
                .update(cx, |_, window, _| window.activate_window())
                .ok();
            self.settings_window = Some(handle);
        }
    }

    fn render_home(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let overlays = layers::window_layers(&self.tooltips, window, cx);
        let theme = cx.theme();
        let right = match self.selected_session(cx) {
            Some(session) => v_flex()
                .flex_1()
                .h_full()
                .min_w_0()
                .child(
                    h_flex()
                        .items_center()
                        .px_4()
                        .py_2()
                        .child(div().text_sm().font_medium().child(session.title.clone())),
                )
                .child(
                    self.terminal.clone().cached(
                        StyleRefinement::default()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .w_full(),
                    ),
                ),
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(theme.muted_foreground)
                .child(div().text_sm().child("还没有会话"))
                .child(
                    Button::new("empty-new")
                        .ghost()
                        .label("新建终端")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.new_session(None, cx);
                            this.focus_terminal(window, cx);
                            cx.notify();
                        })),
                ),
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
        self.render_home(window, cx)
    }
}
