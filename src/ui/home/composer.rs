//! Lower right: message input and toolbar.

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{MoveDown, MoveUp, Paste, Textarea};

use super::{CONTENT_MAX_REM, CONTENT_MIN_REM, KumuApp};
use crate::agent::AGENTS;
use crate::db::Session;
use crate::ui::widgets::image_thumbnail;

impl KumuApp {
    pub(super) fn render_composer(&self, session: &Session, cx: &Context<Self>) -> Div {
        let theme = cx.theme();
        let (radius, border) = (theme.radius, theme.border);
        // The session-scoped axes, in the order they read left to right:
        // reasoning first, because it is the model's axis rather than a
        // session-wide setting, then how much the agent may do before it asks,
        // then what it is doing.
        //
        // They are offered only for an agent kumu can drive: a probe-only one
        // has nothing that could apply a pick, and a control that silently does
        // nothing is worse than no control. The model chip is not one of them —
        // it also switches the session's agent, so it always shows.
        let mut axes: Vec<AnyElement> = Vec::new();
        if session_axes_editable(&session.agent) {
            axes.extend(self.reasoning_picker(session, cx));
            axes.extend(self.access_picker(session, cx));
            axes.extend(self.mode_picker(session, cx));
            axes.extend(self.sandbox_picker(session, cx));
        }
        let ring = theme.ring;
        // Composer: the lower half of the conversation area —
        // a multi-line textarea with the send button inside,
        // on the same centered column as the messages.
        h_flex()
            .flex_none()
            .px_5()
            .py_3()
            .bg(theme.muted.opacity(0.5))
            .child(
                v_flex()
                    .w_full()
                    .max_w(rems(CONTENT_MAX_REM))
                    .min_w(rems(CONTENT_MIN_REM))
                    .mx_auto()
                    .rounded(radius)
                    .border_1()
                    .border_color(border)
                    .bg(theme.background)
                    .overflow_hidden()
                    // Files dropped on the composer join the message: images
                    // as attachments, anything else as a reference.
                    .drag_over::<ExternalPaths>(move |style, _, _, _| style.border_color(ring))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                        this.take_paths(paths.paths(), window, cx);
                    }))
                    .children(self.render_attachments(cx))
                    .child(
                        div()
                            .p_2()
                            // A pasted image or Finder file is taken here; plain
                            // text goes on to the textarea as usual.
                            .capture_action(cx.listener(|this, _: &Paste, window, cx| {
                                if this.paste(window, cx) {
                                    cx.stop_propagation();
                                }
                            }))
                            // While the slash-command menu is open, Up/Down move
                            // its highlight. Caught on the way down to the
                            // textarea, which would otherwise take them as caret
                            // moves; with the menu closed they pass through.
                            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                                if this.move_menu_cursor(false, cx) {
                                    cx.stop_propagation();
                                }
                            }))
                            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                                if this.move_menu_cursor(true, cx) {
                                    cx.stop_propagation();
                                }
                            }))
                            .child(
                                Textarea::new(&self.input_state)
                                    .appearance(false)
                                    .bordered(false)
                                    .w_full(),
                            ),
                    )
                    .child(
                        // Toolbar: model picker on the left,
                        // send pinned to the far right.
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .bg(theme.muted.opacity(0.5))
                            .px_2()
                            .py_1p5()
                            .child(
                                Button::new("attach")
                                    .ghost()
                                    .small()
                                    .icon(Icon::empty().path("icons/paperclip.svg"))
                                    .tooltip("添加图片或文件")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.pick_attachments(window, cx);
                                    })),
                            )
                            .child(self.model_picker(session, cx))
                            .children(axes)
                            .child(div().flex_1())
                            // Beside send: how full the context is decides
                            // whether the next message still fits.
                            .children(self.usage_indicator(session, cx))
                            .child(if self.workspace.read(cx).has_turn(session.id) {
                                // A turn is in flight, so sending is refused (see
                                // `Workspace::send`) and the slot offers the one
                                // action that is available: stopping the turn
                                // that is running.
                                Button::new("stop")
                                    .primary()
                                    .small()
                                    .icon(Icon::empty().path("icons/stop.svg"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.cancel_turn(cx);
                                    }))
                            } else {
                                Button::new("send")
                                    .primary()
                                    .small()
                                    .icon(IconName::ArrowUp)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.send(window, cx);
                                    }))
                            }),
                    ),
            )
    }
}

impl KumuApp {
    /// The images waiting to be sent, as a row of thumbnails above the input,
    /// each with a button that takes it back off; nothing when there are none.
    fn render_attachments(&self, cx: &Context<Self>) -> Option<Div> {
        let attachments = self.attachments(self.selected?);
        if attachments.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let thumbnails = attachments.iter().enumerate().map(|(index, image)| {
            image_thumbnail(image.path.clone(), rems(3.5), theme.radius, theme.border)
                .relative()
                .child(
                    div().absolute().top_0p5().right_0p5().child(
                        Button::new(("remove-attachment", index))
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("移除")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_attachment(index, cx);
                            })),
                    ),
                )
        });
        Some(
            h_flex()
                .flex_wrap()
                .gap_2()
                .px_2()
                .pt_2()
                .children(thumbnails),
        )
    }
}

/// Whether the session-scoped axes belong on the toolbar for this agent.
///
/// A pick made for an agent kumu cannot drive yet would sit there applying
/// nothing, so those controls wait for the session loop that would act on them.
/// An unknown agent counts as one that cannot be driven.
fn session_axes_editable(agent: &str) -> bool {
    AGENTS
        .iter()
        .find(|known| known.id == agent)
        .is_some_and(|known| known.drives_sessions)
}

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use core::prelude::v1::test;

    use super::session_axes_editable;

    /// Every agent that can be driven shows the axes. Claude Code is included
    /// now that its session loop exists — the guard is what keeps an agent
    /// without one from offering picks that could never be applied.
    #[test]
    fn session_axes_need_an_agent_with_a_session_loop() {
        assert!(session_axes_editable("omp"));
        assert!(session_axes_editable("codex"));
        assert!(session_axes_editable("claude"));
        assert!(!session_axes_editable("nonesuch"), "unknown agent");
    }
}
