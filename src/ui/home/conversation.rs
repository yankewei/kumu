//! Upper right: session title, message history, and live reply.

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use gpui_kit::component::text::TextView;

use super::{CONTENT_MAX_REM, CONTENT_MIN_REM, KumuApp};
use crate::db::Session;

impl KumuApp {
    pub(super) fn render_conversation(&self, session: &Session, cx: &Context<Self>) -> Div {
        let theme = cx.theme();
        let (radius, muted_fg, primary, success, danger) = (
            theme.radius,
            theme.muted_foreground,
            theme.primary,
            theme.success,
            theme.danger,
        );
        let title = session.title.clone();
        let messages = self.messages.clone();
        let turn = self.turns.get(&session.id);
        // One centered column shared by messages and composer: capped
        // width for readability, floored so it never gets too skinny.
        let mut rows = v_flex()
            .gap_3()
            .children(messages.into_iter().enumerate().map(move |(ix, m)| {
                if m.role == "user" {
                    h_flex().justify_end().child(
                        div()
                            .max_w(relative(0.78))
                            .px_3()
                            .py_2()
                            .rounded(radius)
                            .bg(primary.opacity(0.1))
                            .child(div().text_sm().child(m.content)),
                    )
                } else if m.role == "system" {
                    div().text_xs().text_color(muted_fg).child(m.content)
                } else {
                    // Agent replies arrive as markdown (ACP text
                    // blocks SHOULD render as markdown).
                    div().text_sm().child(TextView::markdown(
                        SharedString::from(format!("msg-{ix}")),
                        m.content,
                    ))
                }
            }));
        if let Some(turn) = turn {
            // Thinking is collapsed: a bare status row, never the
            // raw reasoning text.
            if turn.thinking {
                rows = rows.child(
                    h_flex()
                        .items_center()
                        .gap_1p5()
                        .child(
                            Icon::new(IconName::LoaderCircle)
                                .small()
                                .text_color(muted_fg),
                        )
                        .child(div().text_xs().text_color(muted_fg).child("思考中…")),
                );
            }
            // Tool calls: one compact status row each, in
            // first-seen order.
            for run in &turn.tools {
                let (icon, color) = match run.status {
                    "done" => (IconName::Check, success),
                    "failed" => (IconName::Close, danger),
                    _ => (IconName::LoaderCircle, muted_fg),
                };
                rows = rows.child(
                    h_flex()
                        .items_center()
                        .gap_1p5()
                        .child(Icon::new(icon).small().text_color(color))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted_fg)
                                .truncate()
                                .child(run.title.clone()),
                        ),
                );
            }
            // The answer streams incrementally; before the first
            // chunk lands, show a bare ellipsis.
            let body: AnyElement = if turn.answer_len == 0 {
                div().child("…").into_any_element()
            } else {
                div()
                    .text_sm()
                    .child(TextView::new(&turn.answer))
                    .into_any_element()
            };
            rows = rows.child(div().text_sm().text_color(muted_fg).child(body));
            // Working-time label at the bottom-left of the reply.
            let secs = turn.started_at.elapsed().as_secs();
            let label = if secs >= 60 {
                format!("{}m{:02}s", secs / 60, secs % 60)
            } else {
                format!("{secs}s")
            };
            rows = rows.child(
                h_flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::LoaderCircle)
                            .small()
                            .text_color(muted_fg),
                    )
                    .child(div().text_xs().text_color(muted_fg).child(label)),
            );
        }
        // Pin to the bottom of this frame's content while
        // following: re-armed every render, consumed at paint,
        // so async markdown growth stays glued to the tail.
        if self.follow_tail {
            self.conversation_scroll.scroll_to_bottom();
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(
                h_flex()
                    .items_center()
                    .px_4()
                    .py_2()
                    .child(div().text_sm().font_medium().child(title)),
            )
            .child(
                div()
                    .flex_1()
                    .id("conversation")
                    .track_scroll(&self.conversation_scroll)
                    .overflow_y_scroll()
                    .on_scroll_wheel(cx.listener(|this, _, _, _| {
                        // Any manual scroll that leaves the bottom
                        // releases the tail; landing back engages it.
                        let handle = &this.conversation_scroll;
                        this.follow_tail = handle.offset().y + handle.max_offset().y <= px(8.);
                    }))
                    .px_5()
                    .py_4()
                    .child(
                        div()
                            .max_w(rems(CONTENT_MAX_REM))
                            .min_w(rems(CONTENT_MIN_REM))
                            .mx_auto()
                            .child(rows),
                    ),
            )
    }
}
