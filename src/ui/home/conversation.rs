//! Upper right: session title, message history, and live reply.

use std::rc::Rc;

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
#[derive(Clone)]
enum RowToggle {
    Key(String),
    Block(usize),
}

/// Where one item of the transcript lives.
#[derive(Clone, Copy)]
enum Source {
    /// A persisted history row, by index into [`KumuApp::messages`].
    History(usize),
    /// A block of the live turn, by index into its block list.
    Live(usize),
}

/// One row of the conversation column, after folding.
///
/// A row names where its content lives rather than holding it: the column is
/// rebuilt from the transcript on every render, and a row outside the viewport
/// is never built into an element at all.
enum Row {
    /// A single message or live block.
    Item(Source),
    /// A run of consecutive tool calls folded into one row.
    Tools(Vec<Source>),
    /// The live turn before it has produced a block.
    Pending,
    /// The live turn's working-time label, which closes the column.
    Working,
}

/// Close an open run of tool calls into its own row.
fn flush_run(run: &mut Vec<Source>, rows: &mut Vec<Row>) {
    if !run.is_empty() {
        rows.push(Row::Tools(std::mem::take(run)));
    }
}

/// The click handler that flips a row's expanded state, wherever it lives.
///
/// Rows are built from the list's own layout pass rather than the view's
/// render, so there is no render context to bind a listener to: the handler
/// reaches the app through a weak handle instead.
fn row_toggle_click(
    weak: &WeakEntity<KumuApp>,
    session_id: i64,
    toggle: RowToggle,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let weak = weak.clone();
    move |_, _, cx| {
        weak.update(cx, |this, cx| this.toggle_row(session_id, toggle.clone(), cx))
            .ok();
    }
}

impl KumuApp {
    pub(super) fn render_conversation(&self, session: &Session, cx: &Context<Self>) -> Div {
        let session_id = session.id;
        let title = session.title.clone();

        // The column is virtual: only the rows in view are built into elements
        // and laid out, so a long transcript costs what a short one does. The
        // rows themselves are folded again on every render, which is cheap
        // because a row carries indices rather than content.
        let rows = self.conversation_rows(session_id);
        if self.conversation_list.item_count() != rows.len() {
            // `reset` forgets where the transcript was scrolled to, and with
            // nothing remembered the list falls back to the top. Carry the
            // position over, so calls arriving below the fold do not move a
            // reader who has scrolled up; a reader at the bottom is held there
            // by the list's own tail mode as the reply streams in.
            let anchor = self.conversation_list.logical_scroll_top();
            let following = self.conversation_list.is_following_tail();
            self.conversation_list.reset(rows.len());
            if !following {
                self.conversation_list.scroll_to(anchor);
            }
        }
        let rows = Rc::new(rows);
        let last_row = rows.len().saturating_sub(1);
        let weak = cx.weak_entity();
        let list = list(self.conversation_list.clone(), move |index, _, cx| {
            let Some(row) = rows.get(index) else {
                return div().into_any_element();
            };
            weak.read_with(cx, |app, cx| {
                // The list lays rows out at its full width and insets them only
                // vertically, so each row carries the column's own gutter
                // outside the shared content width.
                div()
                    .w_full()
                    .px_5()
                    .when(index < last_row, |this| this.pb_3())
                    .child(
                        div()
                            .w_full()
                            .max_w(rems(CONTENT_MAX_REM))
                            .min_w(rems(CONTENT_MIN_REM))
                            .mx_auto()
                            .child(app.render_row(row, session_id, cx, &weak)),
                    )
                    .into_any_element()
            })
            .unwrap_or_else(|_| div().into_any_element())
        })
        .size_full()
        .min_h_0()
        .py_4();

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
            .child(div().flex_1().min_h_0().min_w_0().child(list))
    }

    /// Fold the transcript into rows: persisted history first, then the live
    /// turn, with each run of consecutive tool calls becoming one row. A run
    /// may straddle the two, so the calls a turn makes continue the run the
    /// history ended on and fold the same way wherever they appear.
    fn conversation_rows(&self, session_id: i64) -> Vec<Row> {
        let mut rows: Vec<Row> = Vec::new();
        let mut run: Vec<Source> = Vec::new();

        for (index, message) in self.messages.iter().enumerate() {
            if message.role == "tool" {
                run.push(Source::History(index));
            } else {
                flush_run(&mut run, &mut rows);
                rows.push(Row::Item(Source::History(index)));
            }
        }

        if let Some(turn) = self.turns.get(&session_id) {
            for (index, block) in turn.blocks.iter().enumerate() {
                if matches!(block, TurnBlock::Tool(_)) {
                    run.push(Source::Live(index));
                } else {
                    flush_run(&mut run, &mut rows);
                    rows.push(Row::Item(Source::Live(index)));
                }
            }
            flush_run(&mut run, &mut rows);
            if turn.blocks.is_empty() {
                rows.push(Row::Pending);
            }
            rows.push(Row::Working);
        } else {
            flush_run(&mut run, &mut rows);
        }

        rows
    }

    /// Build one row into an element. Called from the list's layout pass, once
    /// per row that is in view.
    fn render_row(
        &self,
        row: &Row,
        session_id: i64,
        cx: &App,
        weak: &WeakEntity<Self>,
    ) -> AnyElement {
        match row {
            Row::Item(Source::History(index)) => {
                self.render_history_row(*index, session_id, cx, weak)
            }
            Row::Item(Source::Live(index)) => self.render_live_block(*index, session_id, cx, weak),
            Row::Tools(sources) => self.render_tool_run(sources, session_id, cx, weak),
            Row::Pending => div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("…")
                .into_any_element(),
            Row::Working => self.render_working(session_id, cx),
        }
    }

    /// One persisted history row, by role.
    fn render_history_row(
        &self,
        index: usize,
        session_id: i64,
        cx: &App,
        weak: &WeakEntity<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (radius, muted, muted_fg, primary) = (
            theme.radius,
            theme.muted,
            theme.muted_foreground,
            theme.primary,
        );
        let message = &self.messages[index];
        match message.role.as_str() {
            "user" => h_flex()
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
            "system" => div()
                .text_xs()
                .text_color(muted_fg)
                .child(message.content.clone())
                .into_any_element(),
            "thinking" => {
                let key = format!("think-{index}");
                let open = self.expanded_rows.contains(&key);
                let text = message.content.clone();
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
                                weak,
                                session_id,
                                RowToggle::Key(key),
                            )),
                    )
                    .content(collapsed_body(
                        SharedString::from(format!("think-body-{index}")),
                        text,
                        muted_fg,
                    ))
                    .into_any_element()
            }
            // Agent replies arrive as markdown (ACP text blocks SHOULD render
            // as markdown).
            _ => div()
                .child(
                    TextView::markdown(
                        SharedString::from(format!("msg-{index}")),
                        message.content.clone(),
                    )
                    .text_sm()
                    .line_height(relative(1.75)),
                )
                .into_any_element(),
        }
    }

    /// One block of the live turn.
    fn render_live_block(
        &self,
        index: usize,
        session_id: i64,
        cx: &App,
        weak: &WeakEntity<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (radius, muted, muted_fg) = (theme.radius, theme.muted, theme.muted_foreground);
        let Some(turn) = self.turns.get(&session_id) else {
            return div().into_any_element();
        };
        let Some(block) = turn.blocks.get(index) else {
            return div().into_any_element();
        };
        match block {
            TurnBlock::Thinking { text, open } => {
                let (open, text) = (*open, text.clone());
                let active = turn.active_thinking == Some(index);
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
                                weak,
                                session_id,
                                RowToggle::Block(index),
                            )),
                    )
                    .content(collapsed_body(
                        SharedString::from(format!("thinking-body-{index}")),
                        text,
                        muted_fg,
                    ))
                    .into_any_element()
            }
            TurnBlock::Answer { view, .. } => div()
                .child(TextView::new(view).text_sm().line_height(relative(1.75)))
                .into_any_element(),
            // A tool call never reaches a row on its own: `conversation_rows`
            // folds it into the run of calls around it.
            TurnBlock::Tool(_) => div().into_any_element(),
        }
    }

    /// The live turn's working-time label. It counts between chunks too, which
    /// is why it lives on the ticker's notify rather than on the event stream.
    fn render_working(&self, session_id: i64, cx: &App) -> AnyElement {
        let muted_fg = cx.theme().muted_foreground;
        let Some(turn) = self.turns.get(&session_id) else {
            return div().into_any_element();
        };
        let secs = turn.started_at.elapsed().as_secs();
        let label = if secs >= 60 {
            format!("{}m{:02}s", secs / 60, secs % 60)
        } else {
            format!("{secs}s")
        };
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
            .into_any_element()
    }

    /// One tool call as a row, from whichever source it lives in. `None` when a
    /// history row holds text this build did not write.
    ///
    /// This is the only place a stored payload is deserialized, and it runs for
    /// the calls of the runs in view rather than for the whole column.
    fn tool_row(&self, source: Source, session_id: i64) -> Option<ToolRow> {
        match source {
            Source::History(index) => {
                let payload =
                    serde_json::from_str::<ToolPayload>(&self.messages[index].content).ok()?;
                let id = format!("tool-{index}");
                let open = self.expanded_rows.contains(&id);
                Some(ToolRow {
                    id: id.clone(),
                    title: payload.title,
                    status: payload.status,
                    content: payload.content,
                    decision: payload.decision,
                    open,
                    toggle: RowToggle::Key(id),
                })
            }
            Source::Live(index) => {
                let TurnBlock::Tool(run) = self.turns.get(&session_id)?.blocks.get(index)? else {
                    return None;
                };
                Some(ToolRow {
                    id: format!("tool-live-{}", run.id),
                    title: run.title.clone(),
                    status: run.status.to_string(),
                    content: run.content.clone(),
                    // The prompt itself lives above the composer; the row only
                    // records how the call ended up authorized, and stays quiet
                    // while a prompt is still parked on it.
                    decision: run
                        .pending_permission
                        .is_none()
                        .then(|| run.decision.clone())
                        .flatten(),
                    open: run.open,
                    toggle: RowToggle::Block(index),
                })
            }
        }
    }

    /// A run of consecutive tool calls. A lone call is left as its own row —
    /// there is nothing to summarize — while two or more hide behind one line
    /// that expands to the calls themselves.
    fn render_tool_run(
        &self,
        sources: &[Source],
        session_id: i64,
        cx: &App,
        weak: &WeakEntity<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (radius, muted, muted_fg, success, danger) = (
            theme.radius,
            theme.muted,
            theme.muted_foreground,
            theme.success,
            theme.danger,
        );
        let Some(first) = sources.first().copied() else {
            return div().into_any_element();
        };
        if sources.len() == 1 {
            return self.render_tool_source(first, session_id, cx, weak);
        }

        let parsed: Vec<ToolRow> = sources
            .iter()
            .filter_map(|source| self.tool_row(*source, session_id))
            .collect();
        // A run this build cannot read still folds — the calls stay out of the
        // way — it just has no status to report.
        let (run_status, label) = fold_summary(&parsed)
            .unwrap_or_else(|| (RunStatus::Done, format!("{} 个工具调用", sources.len())));
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

        // A run is keyed off its first call, so expanding it survives the calls
        // after that one changing.
        let id = match first {
            Source::History(index) => format!("group-tool-{index}"),
            Source::Live(index) => format!("group-live-{index}"),
        };
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
                    .on_click(row_toggle_click(weak, session_id, RowToggle::Key(id))),
            )
            .content(v_flex().gap_1().pl_4().children(
                sources
                    .iter()
                    .map(|source| self.render_tool_source(*source, session_id, cx, weak)),
            ))
            .into_any_element()
    }

    /// One call of a run, however its payload reads.
    fn render_tool_source(
        &self,
        source: Source,
        session_id: i64,
        cx: &App,
        weak: &WeakEntity<Self>,
    ) -> AnyElement {
        if let Some(row) = self.tool_row(source, session_id) {
            return self.render_tool_row(row, session_id, cx, weak);
        }
        // A live call always carries a payload this build wrote, so only a
        // history row lands here: show it as it stands rather than dropping
        // the row.
        let Source::History(index) = source else {
            return div().into_any_element();
        };
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(self.messages[index].content.clone())
            .into_any_element()
    }

    /// One tool call: its status, its title, and — for a call that produced
    /// output — the output behind its own expander.
    fn render_tool_row(
        &self,
        row: ToolRow,
        session_id: i64,
        cx: &App,
        weak: &WeakEntity<Self>,
    ) -> AnyElement {
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
                        .on_click(row_toggle_click(weak, session_id, row.toggle)),
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

    /// Flip a row's expanded state, wherever it lives.
    fn toggle_row(&mut self, session_id: i64, toggle: RowToggle, cx: &mut Context<Self>) {
        match &toggle {
            RowToggle::Key(key) => {
                // `insert` reports whether the key was new, so a second click
                // on the same row collapses it again.
                if !self.expanded_rows.insert(key.clone()) {
                    self.expanded_rows.remove(key);
                }
            }
            RowToggle::Block(index) => {
                if let Some(block) = self
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
