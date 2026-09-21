//! Home window: projects → sessions tree on the left, the selected session's
//! conversation on the right. Owns the settings window handle.

mod access_picker;
mod command_menu;
mod composer;
mod conversation;
mod markdown_mermaid;
mod mermaid_viewer;
mod mode_picker;
mod model_picker;
mod permission;
mod reasoning_picker;
mod sandbox_picker;
mod sidebar;

// Shared column width keeps the conversation and composer aligned.
const CONTENT_MAX_REM: f32 = 44.;
const CONTENT_MIN_REM: f32 = 24.;

use std::collections::HashSet;
use std::path::PathBuf;

use self::conversation::ConversationView;
use super::SettingsWindow;
use crate::agent::AGENTS;
use crate::app::turn::PendingPermission;
use crate::app::{Workspace, WorkspaceEvent};
use crate::db::Session;
use crate::ui::layers;
use crate::ui::tooltip::Tooltips;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputEvent, InputState, TextareaState};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

/// Which session to select after `id` is deleted.
///
/// The nearest sibling in the same project (the next one, else the previous),
/// else the first session left anywhere, else none — so the conversation pane
/// never goes blank while sessions remain.
fn neighbor_session(sessions: &[Session], id: i64) -> Option<i64> {
    let project = sessions.iter().find(|s| s.id == id)?.project_id;
    let mut siblings: Vec<i64> = sessions
        .iter()
        .filter(|s| s.project_id == project)
        .map(|s| s.id)
        .collect();
    siblings.sort_unstable();
    siblings
        .iter()
        .copied()
        .find(|&sid| sid > id)
        .or_else(|| siblings.iter().rev().copied().find(|&sid| sid < id))
        .or_else(|| sessions.iter().map(|s| s.id).find(|&sid| sid != id))
}

pub struct KumuApp {
    /// Projects, sessions and their live agents; the home view draws it and
    /// hands it every user action.
    workspace: Entity<Workspace>,
    settings_window: Option<WindowHandle<Root>>,
    selected: Option<i64>,
    /// Projects collapsed in the sidebar tree.
    collapsed_projects: HashSet<i64>,
    /// The selected session's history and every session's live turn. Its own
    /// entity so streaming and animation repaint that column alone.
    conversation: Entity<ConversationView>,
    input_state: Entity<TextareaState>,
    /// Which picker's menu is open, if any. One at a time — see
    /// [`KumuApp::set_picker_open`].
    open_picker: Option<PickerAxis>,
    /// Model picker state: which agent's rail tab is active (None = the
    /// session's current agent), and the search field that no other picker has
    /// (the other lists are short enough to read whole).
    model_picker_tab: Option<&'static str>,
    model_search: Entity<InputState>,
    /// The slash-command menu's highlighted row, and the scroll that keeps it in
    /// view. Back to the first match whenever the input changes.
    command_cursor: usize,
    command_scroll: ScrollHandle,
    /// Hover labels for the sidebar tree (see [`Tooltips`]).
    tooltips: Entity<Tooltips>,
}

/// Which composer picker's menu is open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PickerAxis {
    Model,
    Reasoning,
    Access,
    Mode,
    Sandbox,
}

impl KumuApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = cx.new(Workspace::new);
        // Only what the home view itself draws: a streamed chunk is the
        // conversation's alone, and repainting the whole window per chunk is
        // what used to make it stutter.
        cx.subscribe(&workspace, |_, _, ev, cx| {
            if matches!(ev, WorkspaceEvent::Changed) {
                cx.notify();
            }
        })
        .detach();
        let mut app = Self {
            conversation: cx.new(|cx| ConversationView::new(workspace.clone(), cx)),
            workspace,
            settings_window: None,
            selected: None,
            collapsed_projects: HashSet::new(),
            input_state: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(1, 6)
                    .placeholder("输入消息…")
                    // Chat composer semantics: plain Enter sends, Shift+Enter
                    // inserts a newline (the textarea handles the newline).
                    .submit_on_enter(true)
            }),
            open_picker: None,
            model_picker_tab: None,
            model_search: cx.new(|cx| InputState::new(window, cx).placeholder("搜索模型…")),
            command_cursor: 0,
            command_scroll: ScrollHandle::new(),
            tooltips: cx.new(|cx| Tooltips::new(cx)),
        };
        // Enter in the composer sends the prompt (Shift+Enter newline is
        // handled by the textarea itself via submit_on_enter).
        cx.subscribe_in(&app.input_state, window, |this, _, ev, window, cx| {
            match ev {
                // Shift+Enter also emits PressEnter; only plain Enter sends.
                InputEvent::PressEnter { shift: false, .. } => this.send(window, cx),
                // A new prefix is a new list, so the highlight starts over.
                InputEvent::Change => {
                    this.command_cursor = 0;
                    this.command_scroll.scroll_to_item(0);
                }
                _ => {}
            }
        })
        .detach();
        if let Some(first) = app.workspace.read(cx).sessions().first().map(|s| s.id) {
            app.select_session(first, cx);
        }
        app
    }

    fn select_session(&mut self, id: i64, cx: &mut Context<Self>) {
        self.selected = Some(id);
        let messages = self.workspace.read(cx).messages(id);
        self.conversation.update(cx, |conversation, cx| {
            conversation.show(Some(id), &messages, cx)
        });
    }

    /// Start a session — in `project_id`, or the first project — and show it.
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
        // Pick the fallback while the doomed row is still in the list.
        let next = neighbor_session(self.workspace.read(cx).sessions(), session_id);
        self.workspace
            .update(cx, |workspace, cx| workspace.delete_session(session_id, cx));

        if self.selected == Some(session_id) {
            match next.filter(|next| self.workspace.read(cx).session(*next).is_some()) {
                Some(next) => self.select_session(next, cx),
                None => {
                    self.selected = None;
                    self.conversation
                        .update(cx, |conversation, cx| conversation.show(None, &[], cx));
                }
            }
        }
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session(cx) else {
            return;
        };
        // Enter completes while the input is still a bare command, to the row the
        // menu highlights, so the effect is visible before the key is pressed.
        // Sending takes a second Enter. The menu is closed while a turn runs.
        if !self.workspace.read(cx).has_turn(session.id)
            && let Some(name) = self.chosen_command(&session, cx)
        {
            self.complete_command(&name, window, cx);
            return;
        }
        // The raw value rather than a trimmed one reaches the check above: a
        // completed command ends with a space, and that space is what closes
        // the menu. The workspace trims what it sends.
        let text = self.input_state.read(cx).value().to_string();
        let sent = self
            .workspace
            .update(cx, |workspace, cx| workspace.send(session.id, &text, cx));
        if !sent {
            return;
        }
        self.conversation
            .update(cx, |conversation, cx| conversation.scroll_to_end(cx));
        self.input_state
            .update(cx, |state, cx| state.set_value("", window, cx));
    }

    /// Stop the selected session's turn in flight, if there is one.
    fn cancel_turn(&mut self, cx: &mut Context<Self>) {
        if let Some(session_id) = self.selected {
            self.workspace.read(cx).cancel_turn(session_id);
        }
    }

    /// Whether any tool call in this session is still waiting on the user.
    fn has_pending_permission(&self, session_id: i64, cx: &App) -> bool {
        self.workspace
            .read(cx)
            .pending_permission(session_id)
            .is_some()
    }

    /// The oldest outstanding permission prompt for a session, if any.
    fn pending_permission_for(&self, session_id: i64, cx: &App) -> Option<PendingPermission> {
        self.workspace
            .read(cx)
            .pending_permission(session_id)
            .cloned()
    }

    /// Answer a permission prompt with the option the user chose; `None`
    /// declines it.
    fn resolve_permission(
        &mut self,
        session_id: i64,
        request_id: &str,
        option_id: Option<String>,
        label: String,
        cx: &mut Context<Self>,
    ) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.resolve_permission(session_id, request_id, option_id, label, cx)
        });
    }

    /// The session on screen, as the workspace has it now.
    fn selected_session(&self, cx: &App) -> Option<Session> {
        self.workspace.read(cx).session(self.selected?).cloned()
    }

    fn agent_name(&self, agent_id: &str) -> String {
        AGENTS
            .iter()
            .find(|a| a.id == agent_id)
            .map(|a| a.name.to_string())
            .unwrap_or_else(|| agent_id.to_string())
    }

    /// Focus the settings window if it's open, else open a new one (resizable,
    /// opens at its minimum size).
    fn open_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = &self.settings_window {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
        }
        let mut options = TitleBar::window_options();
        options.window_bounds = Some(WindowBounds::centered(size(px(680.), px(640.)), cx));
        options.window_min_size = Some(size(px(680.), px(640.)));
        if let Ok(handle) = cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| SettingsWindow::new(window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        }) {
            handle
                .update(cx, |_, window, _| window.activate_window())
                .ok();
            self.settings_window = Some(handle);
        }
    }

    /// Record a picker opening or closing.
    ///
    /// Only one is open at a time: the chips sit in a row and their menus
    /// overlap. A close only clears the flag when it is *that* picker closing —
    /// opening a second chip dismisses the first, and the two reports can arrive
    /// in either order, so clearing unconditionally would drop the menu the user
    /// just opened.
    fn set_picker_open(&mut self, axis: PickerAxis, open: bool) {
        self.open_picker = if open {
            Some(axis)
        } else if self.open_picker == Some(axis) {
            None
        } else {
            self.open_picker
        };
    }

    fn render_home(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let overlays = layers::window_layers(&self.tooltips, window, cx);
        let theme = cx.theme();
        let right =
            match self.selected_session(cx) {
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
                    // Cached: this frame reuses the conversation's last paint
                    // unless the conversation itself was notified.
                    .child(
                        self.conversation.clone().cached(
                            StyleRefinement::default()
                                .flex_1()
                                .min_h_0()
                                .min_w_0()
                                .w_full(),
                        ),
                    )
                    .child(div().h_px().w_full().bg(theme.border))
                    // A pending authorization sits directly above the input, so
                    // the decision is always in reach.
                    .children(
                        self.pending_permission_for(session.id, cx)
                            .map(|pending| self.render_permission_card(session.id, &pending, cx)),
                    )
                    // The command menu goes closest to the input: it is about
                    // what is being typed, and it is gone the moment the message
                    // is sent, while the card above it answers a decision the
                    // agent is still blocked on.
                    .children(self.command_menu(&session, cx))
                    .child(self.render_composer(&session, cx)),
                None => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .text_color(theme.muted_foreground)
                    .child(div().text_sm().child("还没有会话"))
                    .child(Button::new("empty-new").ghost().label("新建会话").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.new_session(None, cx);
                            cx.notify();
                        }),
                    )),
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

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use core::prelude::v1::test;

    use super::neighbor_session;
    use crate::db::Session;

    fn session(id: i64, project_id: i64) -> Session {
        Session {
            id,
            project_id,
            agent: "omp".into(),
            title: format!("会话 {id}"),
            cwd: "/tmp".into(),
            resume_id: None,
            model: None,
            mode: None,
            sandbox: None,
            reasoning: None,
            title_manual: false,
            has_conversation: false,
        }
    }

    /// Deleting the session the user is looking at must land them next door,
    /// not on a blank pane.
    #[test]
    fn deletion_falls_back_to_the_nearest_sibling() {
        let sessions = vec![session(1, 10), session(2, 10), session(3, 10)];

        assert_eq!(neighbor_session(&sessions, 2), Some(3), "the next one");
        assert_eq!(
            neighbor_session(&sessions, 3),
            Some(2),
            "no next: the previous one"
        );
        assert_eq!(
            neighbor_session(&sessions, 1),
            Some(2),
            "the first one skips forward too"
        );
    }

    /// With no sibling left in the project, the other project's sessions still
    /// beat the empty state; only a truly empty list returns `None`.
    #[test]
    fn deletion_falls_back_across_projects_then_to_nothing() {
        let sessions = vec![session(1, 10), session(2, 20)];

        assert_eq!(neighbor_session(&sessions, 1), Some(2), "another project");
        assert_eq!(neighbor_session(&sessions, 2), Some(1), "and back");
        assert_eq!(neighbor_session(&[session(1, 10)], 1), None, "the last one");
        assert_eq!(neighbor_session(&[], 1), None, "unknown id");
    }
}
