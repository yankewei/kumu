//! Lower right: message input and toolbar.

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Textarea;

use super::{CONTENT_MAX_REM, CONTENT_MIN_REM, KumuApp};
use crate::agents::AGENTS;
use crate::db::Session;

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
                    .child(
                        div().p_2().child(
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
                            .child(self.model_picker(session, cx))
                            .children(axes)
                            .child(div().flex_1())
                            .child(if self.conversation.read(cx).has_turn(session.id) {
                                // A turn is in flight, so sending is refused (see
                                // `KumuApp::send`) and the slot offers the one
                                // action that is available: stopping the turn
                                // that is running.
                                Button::new("stop")
                                    .primary()
                                    .small()
                                    .icon(Icon::empty().path("icons/stop.svg"))
                                    .on_click(cx.listener(|this, _, _, _| {
                                        this.cancel_turn();
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
