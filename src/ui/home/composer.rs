//! Lower right: message input and toolbar.

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Textarea;

use super::{CONTENT_MAX_REM, CONTENT_MIN_REM, KumuApp};
use crate::db::Session;

impl KumuApp {
    pub(super) fn render_composer(&self, session: &Session, cx: &Context<Self>) -> Div {
        let theme = cx.theme();
        let (radius, border) = (theme.radius, theme.border);
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
                            // Reasoning sits beside the model because it is
                            // that model's axis, not a session-wide setting.
                            .children(self.reasoning_picker(session, cx))
                            .children(self.mode_picker(session, cx))
                            .children(self.sandbox_picker(session, cx))
                            .child(div().flex_1())
                            .child(
                                Button::new("send")
                                    .primary()
                                    .small()
                                    .icon(IconName::ArrowUp)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.send(window, cx);
                                    })),
                            ),
                    ),
            )
    }
}
