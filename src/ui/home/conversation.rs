//! Upper right: session title, message history, and live reply.

use gpui_kit::component::collapsible::Collapsible;
use gpui_kit::component::spinner::Spinner;
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
    /// How the call was authorized, when the agent asked. Absent on rows
    /// stored before permissions were surfaced.
    #[serde(default)]
    decision: Option<String>,
}

/// One tool call as the conversation renders it, normalized from either a
/// persisted history row or the live turn so both share one renderer.
struct ToolRow {
    /// Element-id stem, unique within the column. A run of calls also keys its
    /// summary row off the first call's stem.
    id: String,
    title: String,
    status: String,
    content: String,
    decision: Option<String>,
    open: bool,
    /// How this row flips `open`.
    toggle: RowToggle,
}

/// Where a row's expanded state lives, so its click handler can flip it.
///
/// History rows and tool runs are keyed in [`KumuApp::expanded_rows`]; a live
/// turn block owns an `open` flag of its own.
enum RowToggle {
    Key(String),
    Block(usize),
}

/// One item of the conversation column before folding: a tool call, which
/// joins the run of calls around it, or anything else already rendered.
enum ColumnItem {
    Tool(ToolRow),
    Element(AnyElement),
}

/// The click handler that flips a row's expanded state, wherever it lives.
fn row_toggle_click(
    cx: &Context<KumuApp>,
    session_id: i64,
    toggle: RowToggle,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    cx.listener(move |this, _, _, cx| {
        match &toggle {
            RowToggle::Key(key) => {
                // `insert` reports whether the key was new, so a second click
                // on the same row collapses it again.
                if !this.expanded_rows.insert(key.clone()) {
                    this.expanded_rows.remove(key);
                }
            }
            RowToggle::Block(index) => {
                if let Some(block) = this
                    .turns
                    .get_mut(&session_id)
                    .and_then(|turn| turn.blocks.get_mut(*index))
                {
                    match block {
                        TurnBlock::Thinking { open, .. } => *open = !*open,
                        TurnBlock::Tool(run) => run.open = !run.open,
                        // Only expandable blocks are ever wrapped in `Block`.
                        TurnBlock::Answer { .. } => {}
                    }
                }
            }
        }
        cx.notify();
    })
}

impl KumuApp {
    pub(super) fn render_conversation(&self, session: &Session, cx: &Context<Self>) -> Div {
        let theme = cx.theme();
        let (radius, muted, muted_fg, primary) = (
            theme.radius,
            theme.muted,
            theme.muted_foreground,
            theme.primary,
        );
        let session_id = session.id;
        let title = session.title.clone();

        // Two sources, one column: persisted history first, then the live turn.
        // Both become the same items, so a run of tool calls folds the same way
        // wherever it appears.
        let mut items: Vec<ColumnItem> = Vec::new();
        for (index, message) in self.messages.iter().enumerate() {
            items.push(match message.role.as_str() {
                "user" => ColumnItem::Element(
                    h_flex()
                        .justify_end()
                        .child(
                            div()
                                .max_w(relative(0.78))
                                .px_3()
                                .py_2()
                                .rounded(radius)
                                .bg(primary.opacity(0.1))
                                .child(div().text_sm().child(message.content.clone())),
                        )
                        .into_any_element(),
                ),
                "system" => ColumnItem::Element(
                    div()
                        .text_xs()
                        .text_color(muted_fg)
                        .child(message.content.clone())
                        .into_any_element(),
                ),
                "thinking" => {
                    let key = format!("think-{index}");
                    let open = self.expanded_rows.contains(&key);
                    let text = message.content.clone();
                    ColumnItem::Element(
                        Collapsible::new()
                            .open(open)
                            .motion_id(SharedString::from(format!("think-{index}")))
                            .child(
                                h_flex()
                                    .id(SharedString::from(format!("think-header-{index}")))
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
                                    .child(disclosure_caret(open, muted_fg))
                                    .on_click(row_toggle_click(
                                        cx,
                                        session_id,
                                        RowToggle::Key(key),
                                    )),
                            )
                            .content(collapsed_body(
                                SharedString::from(format!("think-body-{index}")),
                                text,
                                muted_fg,
                            ))
                            .into_any_element(),
                    )
                }
                "tool" => match serde_json::from_str::<ToolPayload>(&message.content) {
                    Ok(payload) => {
                        let id = format!("tool-{index}");
                        let open = self.expanded_rows.contains(&id);
                        ColumnItem::Tool(ToolRow {
                            id: id.clone(),
                            title: payload.title,
                            status: payload.status,
                            content: payload.content,
                            decision: payload.decision,
                            open,
                            toggle: RowToggle::Key(id),
                        })
                    }
                    // Not a payload this build writes: show it as it stands
                    // rather than dropping the row.
                    Err(_) => ColumnItem::Element(
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(message.content.clone())
                            .into_any_element(),
                    ),
                },
                // Agent replies arrive as markdown (ACP text
                // blocks SHOULD render as markdown).
                _ => ColumnItem::Element(
                    div()
                        .child(
                            TextView::markdown(
                                SharedString::from(format!("msg-{index}")),
                                message.content.clone(),
                            )
                            .text_sm()
                            .line_height(relative(1.75)),
                        )
                        .into_any_element(),
                ),
            });
        }

        if let Some(turn) = self.turns.get(&session_id) {
            for (index, block) in turn.blocks.iter().enumerate() {
                match block {
                    TurnBlock::Thinking { text, open } => {
                        let (open, text) = (*open, text.clone());
                        let active = turn.active_thinking == Some(index);
                        items.push(ColumnItem::Element(
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
                                        .child(if active {
                                            Spinner::new()
                                                .icon(Icon::new(IconName::LoaderCircle))
                                                .small()
                                                .color(muted_fg)
                                                .into_any_element()
                                        } else {
                                            Icon::new(IconName::CircleCheck)
                                                .small()
                                                .text_color(muted_fg)
                                                .into_any_element()
                                        })
                                        .child(
                                            div().text_xs().text_color(muted_fg).truncate().child(
                                                if active { "思考中…" } else { "已思考" },
                                            ),
                                        )
                                        .child(disclosure_caret(open, muted_fg))
                                        .on_click(row_toggle_click(
                                            cx,
                                            session_id,
                                            RowToggle::Block(index),
                                        )),
                                )
                                .content(collapsed_body(
                                    SharedString::from(format!("thinking-body-{index}")),
                                    text,
                                    muted_fg,
                                ))
                                .into_any_element(),
                        ));
                    }
                    TurnBlock::Tool(run) => items.push(ColumnItem::Tool(ToolRow {
                        id: format!("tool-live-{}", run.id),
                        title: run.title.clone(),
                        status: run.status.to_string(),
                        content: run.content.clone(),
                        // The prompt itself lives above the composer; the row
                        // only records how the call ended up authorized, and
                        // stays quiet while a prompt is still parked on it.
                        decision: run
                            .pending_permission
                            .is_none()
                            .then(|| run.decision.clone())
                            .flatten(),
                        open: run.open,
                        toggle: RowToggle::Block(index),
                    })),
                    TurnBlock::Answer { view, .. } => items.push(ColumnItem::Element(
                        div()
                            .child(TextView::new(view).text_sm().line_height(relative(1.75)))
                            .into_any_element(),
                    )),
                }
            }
            if turn.blocks.is_empty() {
                items.push(ColumnItem::Element(
                    div()
                        .text_sm()
                        .text_color(muted_fg)
                        .child("…")
                        .into_any_element(),
                ));
            }
            // Working-time label at the bottom-left of the reply.
            let secs = turn.started_at.elapsed().as_secs();
            let label = if secs >= 60 {
                format!("{}m{:02}s", secs / 60, secs % 60)
            } else {
                format!("{secs}s")
            };
            items.push(ColumnItem::Element(
                h_flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        Spinner::new()
                            .icon(Icon::new(IconName::LoaderCircle))
                            .small()
                            .color(muted_fg),
                    )
                    .child(div().text_xs().text_color(muted_fg).child(label))
                    .into_any_element(),
            ));
        }

        let rows = self.render_column(items, session_id, cx);
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

    /// Lay the column out, folding each run of consecutive tool calls into a
    /// single summary row. A run is flushed as soon as anything else separates
    /// it from the next call, and once more at the end.
    fn render_column(&self, items: Vec<ColumnItem>, session_id: i64, cx: &Context<Self>) -> Div {
        let mut column = v_flex().gap_3();
        let mut run: Vec<ToolRow> = Vec::new();
        for item in items {
            match item {
                ColumnItem::Tool(row) => run.push(row),
                ColumnItem::Element(element) => {
                    if !run.is_empty() {
                        let rows = std::mem::take(&mut run);
                        column = column.child(self.render_tool_run(rows, session_id, cx));
                    }
                    column = column.child(element);
                }
            }
        }
        if !run.is_empty() {
            column = column.child(self.render_tool_run(run, session_id, cx));
        }
        column
    }

    /// A run of consecutive tool calls. A lone call is left as its own row —
    /// there is nothing to summarize — while two or more hide behind one line
    /// that expands to the calls themselves.
    fn render_tool_run(
        &self,
        rows: Vec<ToolRow>,
        session_id: i64,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (radius, muted, muted_fg, success, danger) = (
            theme.radius,
            theme.muted,
            theme.muted_foreground,
            theme.success,
            theme.danger,
        );
        let Some((run_status, label)) = fold_summary(&rows) else {
            let row = rows
                .into_iter()
                .next()
                .expect("a run holds at least one call");
            return self.render_tool_row(row, session_id, cx);
        };
        // With the calls folded away this row is the only sign they are still
        // moving, so an unfinished run gets a spinner rather than a still icon.
        let status = match run_status {
            RunStatus::Failed => Icon::new(IconName::Close)
                .small()
                .text_color(danger)
                .into_any_element(),
            RunStatus::Running => Spinner::new()
                .icon(Icon::new(IconName::LoaderCircle))
                .small()
                .color(muted_fg)
                .into_any_element(),
            RunStatus::Done => Icon::new(IconName::Check)
                .small()
                .text_color(success)
                .into_any_element(),
        };

        let id = format!("group-{}", rows[0].id);
        let open = self.expanded_rows.contains(&id);
        Collapsible::new()
            .open(open)
            .motion_id(SharedString::from(id.clone()))
            .child(
                h_flex()
                    .id(SharedString::from(format!("{id}-header")))
                    .cursor_pointer()
                    .items_center()
                    .gap_1p5()
                    .px_1()
                    .py_0p5()
                    .rounded(radius)
                    .hover(move |s| s.bg(muted))
                    .child(status)
                    .child(div().text_xs().text_color(muted_fg).child(label))
                    .child(disclosure_caret(open, muted_fg))
                    .on_click(row_toggle_click(cx, session_id, RowToggle::Key(id))),
            )
            .content(
                v_flex().gap_1().pl_4().children(
                    rows.into_iter()
                        .map(|row| self.render_tool_row(row, session_id, cx)),
                ),
            )
            .into_any_element()
    }

    /// One tool call: its status, its title, and — for a call that produced
    /// output — the output behind its own expander.
    fn render_tool_row(&self, row: ToolRow, session_id: i64, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (radius, muted, muted_fg, success, danger) = (
            theme.radius,
            theme.muted,
            theme.muted_foreground,
            theme.success,
            theme.danger,
        );
        let (icon, color) = match row.status.as_str() {
            "done" => (IconName::Check, success),
            "failed" => (IconName::Close, danger),
            _ => (IconName::LoaderCircle, muted_fg),
        };
        let open = row.open;
        let header = h_flex()
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
                    .child(row.title),
            );
        let element = if row.content.is_empty() {
            header.into_any_element()
        } else {
            let id = row.id;
            let body_id = SharedString::from(format!("{id}-body"));
            Collapsible::new()
                .open(open)
                .motion_id(SharedString::from(id.clone()))
                .child(
                    header
                        .id(SharedString::from(format!("{id}-header")))
                        .cursor_pointer()
                        .rounded(radius)
                        .hover(move |s| s.bg(muted))
                        .child(disclosure_caret(open, muted_fg))
                        .on_click(row_toggle_click(cx, session_id, row.toggle)),
                )
                .content(collapsed_body(body_id, row.content, muted_fg))
                .into_any_element()
        };
        // A stored decision explains how the call was authorized, which the
        // status alone does not.
        let mut container = v_flex().child(element);
        if let Some(decision) = row.decision {
            container = container.child(self.render_permission_decision(&decision, cx));
        }
        container.into_any_element()
    }
}

/// The worst status in a folded run: a failure outranks work still in flight,
/// which outranks a run that came back clean.
#[derive(Debug, PartialEq)]
enum RunStatus {
    Failed,
    Running,
    Done,
}

/// Whether a run of tool calls folds into one summary row, and if so, its
/// worst status and its label ("N 个工具调用 · M 个失败"). `None` means the run
/// has nothing to summarize — a lone call is left as its own row instead (see
/// [`KumuApp::render_tool_run`]).
fn fold_summary(rows: &[ToolRow]) -> Option<(RunStatus, String)> {
    if rows.len() <= 1 {
        return None;
    }
    let failures = rows.iter().filter(|row| row.status == "failed").count();
    // Same reading of status as a single row: anything not settled is still
    // in flight.
    let running = rows
        .iter()
        .any(|row| !matches!(row.status.as_str(), "done" | "failed"));
    let status = if failures > 0 {
        RunStatus::Failed
    } else if running {
        RunStatus::Running
    } else {
        RunStatus::Done
    };
    let mut label = format!("{} 个工具调用", rows.len());
    if failures > 0 {
        label.push_str(&format!(" · {failures} 个失败"));
    }
    Some((status, label))
}

/// The caret a foldable row turns: pointing down while the row is closed, half
/// a turn once it is open.
fn disclosure_caret(open: bool, muted_fg: gpui::Hsla) -> impl IntoElement {
    Icon::new(IconName::ChevronDown)
        .xsmall()
        .flex_none()
        .text_color(muted_fg)
        .rotate(percentage(if open { 0.5 } else { 0. }))
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

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use core::prelude::v1::test;

    use super::{RowToggle, RunStatus, ToolRow, fold_summary};

    fn tool_row(status: &str) -> ToolRow {
        ToolRow {
            id: "t".into(),
            title: "工具".into(),
            status: status.into(),
            content: String::new(),
            decision: None,
            open: false,
            toggle: RowToggle::Key("t".into()),
        }
    }

    /// A lone call has nothing to summarize, so it is left to render as its
    /// own row rather than folding into a one-item group.
    #[test]
    fn a_lone_call_does_not_fold() {
        assert_eq!(fold_summary(&[tool_row("done")]), None);
        assert_eq!(fold_summary(&[]), None);
    }

    /// A settled run with nothing failed reads as done, and the label just
    /// counts the calls.
    #[test]
    fn a_clean_run_labels_only_the_count() {
        let (status, label) = fold_summary(&[tool_row("done"), tool_row("done")]).unwrap();
        assert_eq!(status, RunStatus::Done);
        assert_eq!(label, "2 个工具调用");
    }

    /// Anything not yet `done` or `failed` reads as still running, which is
    /// what keeps the summary's spinner going while the folded calls finish.
    #[test]
    fn an_unsettled_call_keeps_the_run_running() {
        let (status, label) = fold_summary(&[tool_row("done"), tool_row("running")]).unwrap();
        assert_eq!(status, RunStatus::Running);
        assert_eq!(
            label, "2 个工具调用",
            "no failures yet, so none are reported"
        );
    }

    /// A failure outranks work still in flight: the run reads as failed even
    /// with another call still unsettled, and the label says how many.
    #[test]
    fn a_failure_outranks_a_still_running_call() {
        let (status, label) =
            fold_summary(&[tool_row("failed"), tool_row("running"), tool_row("done")]).unwrap();
        assert_eq!(status, RunStatus::Failed);
        assert_eq!(label, "3 个工具调用 · 1 个失败");
    }
}
