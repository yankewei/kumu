//! Upper right: session title, message history, and live reply.

use gpui_kit::component::collapsible::Collapsible;
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use gpui_kit::component::text::TextView;

use super::{CONTENT_MAX_REM, CONTENT_MIN_REM, KumuApp, TurnBlock};
use crate::db::Session;

/// A persisted tool call, deserialized from the message content.
#[derive(serde::Deserialize)]
struct ToolPayload {
    title: String,
    status: String,
    content: String,
}

impl KumuApp {
    pub(super) fn render_conversation(&self, session: &Session, cx: &Context<Self>) -> Div {
        let theme = cx.theme();
        let (radius, muted, muted_fg, primary, success, danger) = (
            theme.radius,
            theme.muted,
            theme.muted_foreground,
            theme.primary,
            theme.success,
            theme.danger,
        );
        let session_id = session.id;
        let title = session.title.clone();
        let messages = self.messages.clone();
        let expanded = self.expanded_rows.clone();
        let turn = self.turns.get(&session.id);
        // One centered column shared by messages and composer: capped
        // width for readability, floored so it never gets too skinny.
        let mut rows = v_flex()
            .gap_3()
            .children(messages.into_iter().enumerate().map(move |(ix, m)| {
                match m.role.as_str() {
                    "user" => h_flex()
                        .justify_end()
                        .child(
                            div()
                                .max_w(relative(0.78))
                                .px_3()
                                .py_2()
                                .rounded(radius)
                                .bg(primary.opacity(0.1))
                                .child(div().text_sm().child(m.content)),
                        )
                        .into_any_element(),
                    "system" => div()
                        .text_xs()
                        .text_color(muted_fg)
                        .child(m.content)
                        .into_any_element(),
                    "thinking" => {
                        let key = format!("think-{ix}");
                        let motion_id = SharedString::from(format!("think-{ix}"));
                        let header_id = SharedString::from(format!("think-header-{ix}"));
                        let body_id = SharedString::from(format!("think-body-{ix}"));
                        let open = expanded.contains(&key);
                        let text = m.content;
                        Collapsible::new()
                            .open(open)
                            .motion_id(motion_id)
                            .child(
                                h_flex()
                                    .id(header_id)
                                    .cursor_pointer()
                                    .items_center()
                                    .gap_1p5()
                                    .px_1()
                                    .py_0p5()
                                    .rounded(radius)
                                    .hover(move |s| s.bg(muted))
                                    .child(
                                        Icon::new(IconName::CircleCheck)
                                            .small()
                                            .text_color(muted_fg),
                                    )
                                    .child(div().text_xs().text_color(muted_fg).child("已思考"))
                                    .child(
                                        Icon::new(IconName::ChevronDown)
                                            .xsmall()
                                            .flex_none()
                                            .text_color(muted_fg)
                                            .rotate(percentage(if open { 0.5 } else { 0. })),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this.expanded_rows.contains(&key) {
                                            this.expanded_rows.remove(&key);
                                        } else {
                                            this.expanded_rows.insert(key.clone());
                                        }
                                        cx.notify();
                                    })),
                            )
                            .content(collapsed_body(body_id, text, muted_fg))
                            .into_any_element()
                    }
                    "tool" => match serde_json::from_str::<ToolPayload>(&m.content) {
                        Ok(payload) => {
                            let (icon, color) = match payload.status.as_str() {
                                "done" => (IconName::Check, success),
                                "failed" => (IconName::Close, danger),
                                _ => (IconName::LoaderCircle, muted_fg),
                            };
                            let key = format!("tool-{ix}");
                            let motion_id = SharedString::from(format!("tool-{ix}"));
                            let header_id = SharedString::from(format!("tool-header-{ix}"));
                            let body_id = SharedString::from(format!("tool-body-{ix}"));
                            let open = expanded.contains(&key);
                            let title_text = payload.title;
                            let text = payload.content;
                            if text.is_empty() {
                                return h_flex()
                                    .items_center()
                                    .gap_1p5()
                                    .px_1()
                                    .py_0p5()
                                    .child(Icon::new(icon).small().text_color(color))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .truncate()
                                            .child(title_text),
                                    )
                                    .into_any_element();
                            }
                            Collapsible::new()
                                .open(open)
                                .motion_id(motion_id)
                                .child(
                                    h_flex()
                                        .id(header_id)
                                        .cursor_pointer()
                                        .items_center()
                                        .gap_1p5()
                                        .px_1()
                                        .py_0p5()
                                        .rounded(radius)
                                        .hover(move |s| s.bg(muted))
                                        .child(Icon::new(icon).small().text_color(color))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(muted_fg)
                                                .truncate()
                                                .child(title_text),
                                        )
                                        .child(
                                            Icon::new(IconName::ChevronDown)
                                                .xsmall()
                                                .flex_none()
                                                .text_color(muted_fg)
                                                .rotate(percentage(if open { 0.5 } else { 0. })),
                                        )
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if this.expanded_rows.contains(&key) {
                                                this.expanded_rows.remove(&key);
                                            } else {
                                                this.expanded_rows.insert(key.clone());
                                            }
                                            cx.notify();
                                        })),
                                )
                                .content(collapsed_body(body_id, text, muted_fg))
                                .into_any_element()
                        }
                        Err(_) => div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(m.content)
                            .into_any_element(),
                    },
                    // Agent replies arrive as markdown (ACP text
                    // blocks SHOULD render as markdown).
                    _ => div()
                        .child(
                            TextView::markdown(SharedString::from(format!("msg-{ix}")), m.content)
                                .text_sm()
                                .line_height(relative(1.75)),
                        )
                        .into_any_element(),
                }
            }));
        if let Some(turn) = turn {
            for (index, block) in turn.blocks.iter().enumerate() {
                match block {
                    TurnBlock::Thinking { text, open } => {
                        let open = *open;
                        let text = text.clone();
                        let active = turn.active_thinking == Some(index);
                        rows = rows.child(
                            Collapsible::new()
                                .open(open)
                                .motion_id(SharedString::from(format!("thinking-{index}")))
                                .child(
                                    h_flex()
                                        .id(SharedString::from(format!("thinking-header-{index}")))
                                        .cursor_pointer()
                                        .items_center()
                                        .gap_1p5()
                                        .px_1()
                                        .py_0p5()
                                        .rounded(radius)
                                        .hover(move |s| s.bg(muted))
                                        .child(
                                            Icon::new(if active {
                                                IconName::LoaderCircle
                                            } else {
                                                IconName::CircleCheck
                                            })
                                            .small()
                                            .text_color(muted_fg),
                                        )
                                        .child(
                                            div().text_xs().text_color(muted_fg).truncate().child(
                                                if active { "思考中…" } else { "已思考" },
                                            ),
                                        )
                                        .child(
                                            Icon::new(IconName::ChevronDown)
                                                .xsmall()
                                                .flex_none()
                                                .text_color(muted_fg)
                                                .rotate(percentage(if open { 0.5 } else { 0. })),
                                        )
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(TurnBlock::Thinking { open, .. }) = this
                                                .turns
                                                .get_mut(&session_id)
                                                .and_then(|turn| turn.blocks.get_mut(index))
                                            {
                                                *open = !*open;
                                                cx.notify();
                                            }
                                        })),
                                )
                                .content(collapsed_body(
                                    SharedString::from(format!("thinking-body-{index}")),
                                    text,
                                    muted_fg,
                                )),
                        );
                    }
                    TurnBlock::Tool(run) => {
                        let (icon, color) = match run.status {
                            "done" => (IconName::Check, success),
                            "failed" => (IconName::Close, danger),
                            _ => (IconName::LoaderCircle, muted_fg),
                        };
                        let header = |truncate_title: bool| {
                            h_flex()
                                .items_center()
                                .gap_1p5()
                                .child(Icon::new(icon).small().text_color(color))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted_fg)
                                        .when(truncate_title, |this| this.truncate())
                                        .child(run.title.clone()),
                                )
                        };
                        if run.content.is_empty() {
                            rows = rows.child(header(true).px_1().py_0p5());
                            continue;
                        }
                        let open = run.open;
                        let motion_id = SharedString::from(format!("tool-{}", run.id));
                        let body_id = SharedString::from(format!("tool-body-{}", run.id));
                        let header_id = SharedString::from(format!("tool-header-{}", run.id));
                        let text = run.content.clone();
                        rows = rows.child(
                            Collapsible::new()
                                .open(open)
                                .motion_id(motion_id)
                                .child(
                                    header(true)
                                        .id(header_id)
                                        .cursor_pointer()
                                        .px_1()
                                        .py_0p5()
                                        .rounded(radius)
                                        .hover(move |s| s.bg(muted))
                                        .child(
                                            Icon::new(IconName::ChevronDown)
                                                .xsmall()
                                                .flex_none()
                                                .text_color(muted_fg)
                                                .rotate(percentage(if open { 0.5 } else { 0. })),
                                        )
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(TurnBlock::Tool(run)) = this
                                                .turns
                                                .get_mut(&session_id)
                                                .and_then(|turn| turn.blocks.get_mut(index))
                                            {
                                                run.open = !run.open;
                                                cx.notify();
                                            }
                                        })),
                                )
                                .content(collapsed_body(body_id, text, muted_fg)),
                        );
                    }
                    TurnBlock::Answer { view, .. } => {
                        rows = rows.child(
                            div().child(TextView::new(view).text_sm().line_height(relative(1.75))),
                        );
                    }
                }
            }
            if turn.blocks.is_empty() {
                rows = rows.child(div().text_sm().text_color(muted_fg).child("…"));
            }
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
                            .w_full()
                            .max_w(rems(CONTENT_MAX_REM))
                            .min_w(rems(CONTENT_MIN_REM))
                            .mx_auto()
                            .child(rows),
                    ),
            )
    }
}

/// Collapsible row body: the captured text, capped in height with its own
/// scroll so long reasoning/output doesn't flood the conversation.
fn collapsed_body(
    id: impl Into<SharedString>,
    text: String,
    muted_fg: gpui::Hsla,
) -> Stateful<Div> {
    div()
        .id(id.into())
        .max_h(rems(16.))
        .overflow_y_scroll()
        .mt_1()
        .mb_0p5()
        .pl_8()
        .text_xs()
        .text_color(muted_fg)
        .child(text)
}
