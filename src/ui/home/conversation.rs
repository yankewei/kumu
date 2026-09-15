//! Upper right: the selected session's message history and live reply.
//!
//! The conversation is its own entity, embedded as a cached view, so the things
//! that change it many times a second — streamed chunks, the working spinner,
//! the elapsed-time ticker — repaint this column alone, and a keystroke in the
//! composer or a hover in the sidebar leaves it untouched. Rows go through a
//! virtual list, so a frame only builds the rows on screen however long the
//! session has grown.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

use gpui_kit::component::collapsible::Collapsible;
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::permission::render_permission_decision;
use super::{CONTENT_MAX_REM, CONTENT_MIN_REM, PendingPermission};
use crate::db::Message;

/// One tool call observed during the live turn, in first-seen order.
pub(super) struct ToolRun {
    pub(super) id: String,
    pub(super) title: String,
    /// pending / running / done / failed
    pub(super) status: &'static str,
    /// Flattened text output from the agent, expandable in the UI.
    pub(super) content: String,
    /// Whether the content is expanded.
    pub(super) open: bool,
    /// Authorization the agent is still waiting on for this call.
    pub(super) pending_permission: Option<PendingPermission>,
    /// The choice the user made (the agent's own option label), kept on the row
    /// so the conversation shows how the call was authorized.
    pub(super) decision: Option<String>,
}

/// Blocks stay in arrival order; tool updates mutate their original block.
pub(super) enum TurnBlock {
    Thinking {
        text: String,
        open: bool,
    },
    Tool(ToolRun),
    Answer {
        text: String,
        view: Entity<TextViewState>,
    },
}

pub(super) struct TurnState {
    started_at: std::time::Instant,
    pub(super) blocks: Vec<TurnBlock>,
    pub(super) active_thinking: Option<usize>,
}

impl TurnState {
    fn new() -> Self {
        Self {
            started_at: std::time::Instant::now(),
            blocks: Vec::new(),
            active_thinking: None,
        }
    }
}

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

/// A persisted message, decoded once when the history is loaded rather than on
/// every frame that shows it.
enum HistoryEntry {
    User(SharedString),
    /// A system notice, or a tool row this build cannot decode, shown as it
    /// stands rather than dropped.
    Note(SharedString),
    Thinking(SharedString),
    Tool {
        title: SharedString,
        status: SharedString,
        content: SharedString,
        decision: Option<SharedString>,
    },
    /// Agent replies arrive as markdown (ACP text blocks SHOULD render as
    /// markdown). The parsed view is built the first time the row is drawn and
    /// kept, so scrolling a reply back into view does not parse it again.
    Agent {
        text: SharedString,
        view: Option<Entity<TextViewState>>,
    },
}

impl HistoryEntry {
    fn decode(message: &Message) -> Self {
        let content = SharedString::from(message.content.clone());
        match message.role.as_str() {
            "user" => Self::User(content),
            "system" => Self::Note(content),
            "thinking" => Self::Thinking(content),
            "tool" => match serde_json::from_str::<ToolPayload>(&message.content) {
                Ok(payload) => Self::Tool {
                    title: payload.title.into(),
                    status: payload.status.into(),
                    content: payload.content.into(),
                    decision: payload.decision.map(Into::into),
                },
                Err(_) => Self::Note(content),
            },
            _ => Self::Agent {
                text: content,
                view: None,
            },
        }
    }

    fn is_tool(&self) -> bool {
        matches!(self, Self::Tool { .. })
    }
}

/// One row of the virtual list. History rows point into the decoded history,
/// live rows into the session's turn blocks; a run of consecutive tool calls is
/// one row, folded behind a summary when it holds more than one call.
#[derive(Clone, Debug, PartialEq)]
enum Row {
    History(usize),
    HistoryTools(Range<usize>),
    Live(usize),
    LiveTools(Range<usize>),
    /// The turn has started but nothing has streamed in yet.
    Placeholder,
    /// The working-time label under a reply in flight.
    Working,
}

impl Row {
    /// Which list slot the row occupies. A tool run keeps its slot as calls
    /// join it, so growing a run remeasures the row instead of replacing it.
    fn slot(&self) -> (u8, usize) {
        match self {
            Row::History(index) => (0, *index),
            Row::HistoryTools(range) => (1, range.start),
            Row::Live(index) => (2, *index),
            Row::LiveTools(range) => (3, range.start),
            Row::Placeholder => (4, 0),
            Row::Working => (5, 0),
        }
    }

    /// Whether the row's content can change while its slot stays the same.
    fn is_live(&self) -> bool {
        !matches!(self, Row::History(_) | Row::HistoryTools(_))
    }
}

/// Fold items into rows: each maximal run of tool calls becomes one run row,
/// anything else a row of its own.
fn fold_rows(
    is_tool: impl IntoIterator<Item = bool>,
    single: fn(usize) -> Row,
    run: fn(Range<usize>) -> Row,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut run_start = None;
    let mut len = 0;
    for (index, tool) in is_tool.into_iter().enumerate() {
        len = index + 1;
        match (tool, run_start) {
            (true, None) => run_start = Some(index),
            (true, Some(_)) => {}
            (false, start) => {
                if let Some(start) = start {
                    rows.push(run(start..index));
                    run_start = None;
                }
                rows.push(single(index));
            }
        }
    }
    if let Some(start) = run_start {
        rows.push(run(start..len));
    }
    rows
}

/// How an existing list must change to show new rows in place of old ones.
#[derive(Debug, PartialEq)]
struct RowChange {
    /// Rows that kept their slot but may have changed, remeasured in place.
    remeasure: Option<Range<usize>>,
    /// The `(old range, new count)` splice after them.
    splice: Option<(Range<usize>, usize)>,
}

fn diff_rows(old: &[Row], new: &[Row]) -> RowChange {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(a, b)| a.slot() == b.slot())
        .count();
    let remeasure = old
        .iter()
        .zip(new)
        .take(prefix)
        .position(|(a, b)| a != b || b.is_live())
        .map(|start| start..prefix);
    let splice =
        (prefix < old.len() || prefix < new.len()).then(|| (prefix..old.len(), new.len() - prefix));
    RowChange { remeasure, splice }
}

/// One tool call as the conversation renders it, normalized from either a
/// persisted history row or the live turn so both share one renderer.
struct ToolRow {
    /// Element-id stem, unique within the column. A run of calls also keys its
    /// summary row off the first call's stem.
    id: String,
    title: SharedString,
    status: SharedString,
    content: SharedString,
    decision: Option<SharedString>,
    open: bool,
    /// How this row flips `open`.
    toggle: RowToggle,
}

/// Where a row's expanded state lives, so its click handler can flip it.
///
/// History rows and tool runs are keyed in [`ConversationView::expanded_rows`];
/// a live turn block owns an `open` flag of its own.
enum RowToggle {
    Key(String),
    Block(usize),
}

pub(super) struct ConversationView {
    /// The session on screen, if any.
    session_id: Option<i64>,
    history: Vec<HistoryEntry>,
    /// The history folded into rows; unchanged by the live turn.
    history_rows: Vec<Row>,
    /// Every row the list currently knows about, history then live.
    rows: Vec<Row>,
    /// Live turn state keyed by session id: streaming thinking, tool calls,
    /// and the answer being built. Dropped when the turn ends.
    turns: HashMap<i64, TurnState>,
    /// Expanded rows, keyed by the row's element id: a history row's list
    /// index, or a tool run's first call. Live turn blocks carry an `open` flag
    /// of their own instead.
    expanded_rows: HashSet<String>,
    scroller: Entity<MessageScrollerState>,
}

impl ConversationView {
    pub(super) fn new(cx: &mut Context<Self>) -> Self {
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        // The scroller notifies its own state (scrolling away from or back to
        // the tail); this view is what draws the jump button that depends on it.
        cx.observe(&scroller, |_, _, cx| cx.notify()).detach();
        Self {
            session_id: None,
            history: Vec::new(),
            history_rows: Vec::new(),
            rows: Vec::new(),
            turns: HashMap::new(),
            expanded_rows: HashSet::new(),
            scroller,
        }
    }

    /// Put a session on screen (or none), pinned to its latest message.
    pub(super) fn show(
        &mut self,
        session_id: Option<i64>,
        messages: &[Message],
        cx: &mut Context<Self>,
    ) {
        self.session_id = session_id;
        self.history = messages.iter().map(HistoryEntry::decode).collect();
        self.history_rows = self.fold_history();
        self.rows = self.current_rows();
        let count = self.rows.len();
        self.scroller
            .update(cx, |scroller, cx| scroller.reset(count, cx));
        cx.notify();
    }

    /// The session's persisted messages changed (a turn was saved, a notice
    /// written). Ignored unless that session is the one on screen.
    pub(super) fn reload_history(
        &mut self,
        session_id: i64,
        messages: &[Message],
        cx: &mut Context<Self>,
    ) {
        if self.session_id != Some(session_id) {
            return;
        }
        let mut previous = std::mem::take(&mut self.history).into_iter();
        self.history = messages
            .iter()
            .map(|message| {
                let mut entry = HistoryEntry::decode(message);
                // Keep a reply's parsed view when the reply is the same text.
                if let (
                    HistoryEntry::Agent { text, view },
                    Some(HistoryEntry::Agent {
                        text: old_text,
                        view: old_view,
                    }),
                ) = (&mut entry, previous.next())
                    && *text == old_text
                {
                    *view = old_view;
                }
                entry
            })
            .collect();
        self.history_rows = self.fold_history();
        self.sync_rows(cx);
    }

    /// Resume following the newest content, as sending a prompt does.
    pub(super) fn scroll_to_end(&mut self, cx: &mut Context<Self>) {
        self.scroller
            .update(cx, |scroller, cx| scroller.scroll_to_end(cx));
    }

    pub(super) fn has_turn(&self, session_id: i64) -> bool {
        self.turns.contains_key(&session_id)
    }

    pub(super) fn turn(&self, session_id: i64) -> Option<&TurnState> {
        self.turns.get(&session_id)
    }

    /// Mutate the session's live turn, opening it first if there is none, then
    /// bring the list in line with what changed.
    pub(super) fn update_turn<R>(
        &mut self,
        session_id: i64,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut TurnState, &mut Context<Self>) -> R,
    ) -> R {
        let turn = match self.turns.entry(session_id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                Self::start_ticker(session_id, cx);
                entry.insert(TurnState::new())
            }
        };
        let result = f(turn, cx);
        self.sync_turn(session_id, cx);
        result
    }

    /// Like [`Self::update_turn`], but only when a turn is already open.
    pub(super) fn with_turn<R>(
        &mut self,
        session_id: i64,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut TurnState) -> R,
    ) -> Option<R> {
        let result = f(self.turns.get_mut(&session_id)?);
        self.sync_turn(session_id, cx);
        Some(result)
    }

    /// Close the session's live turn, handing its blocks back.
    pub(super) fn take_turn(
        &mut self,
        session_id: i64,
        cx: &mut Context<Self>,
    ) -> Option<TurnState> {
        let turn = self.turns.remove(&session_id);
        if turn.is_some() {
            self.sync_turn(session_id, cx);
        }
        turn
    }

    /// Keep the working-time label counting even when no chunks arrive. The
    /// ticker stops by itself once the turn is gone.
    fn start_ticker(session_id: i64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let alive = this
                    .update(cx, |view, cx| {
                        let alive = view.turns.contains_key(&session_id);
                        if alive && view.session_id == Some(session_id) {
                            cx.notify();
                        }
                        alive
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    fn sync_turn(&mut self, session_id: i64, cx: &mut Context<Self>) {
        if self.session_id == Some(session_id) {
            self.sync_rows(cx);
        }
    }

    fn fold_history(&self) -> Vec<Row> {
        fold_rows(
            self.history.iter().map(HistoryEntry::is_tool),
            Row::History,
            Row::HistoryTools,
        )
    }

    fn current_rows(&self) -> Vec<Row> {
        let mut rows = self.history_rows.clone();
        if let Some(turn) = self.session_id.and_then(|id| self.turns.get(&id)) {
            rows.extend(fold_rows(
                turn.blocks
                    .iter()
                    .map(|block| matches!(block, TurnBlock::Tool(_))),
                Row::Live,
                Row::LiveTools,
            ));
            if turn.blocks.is_empty() {
                rows.push(Row::Placeholder);
            }
            rows.push(Row::Working);
        }
        rows
    }

    /// Tell the virtual list which rows changed: rows that kept their slot are
    /// remeasured where they stand (holding the reader's scroll position), and
    /// only the tail that differs is spliced.
    fn sync_rows(&mut self, cx: &mut Context<Self>) {
        let rows = self.current_rows();
        let RowChange { remeasure, splice } = diff_rows(&self.rows, &rows);
        self.rows = rows;
        self.scroller.update(cx, |scroller, cx| {
            if let Some(range) = remeasure {
                scroller.remeasure_items(range, cx);
            }
            if let Some((range, count)) = splice {
                scroller.splice(range, count, cx);
            }
        });
        cx.notify();
    }

    /// The click handler that flips a row's expanded state, wherever it lives.
    fn row_toggle_click(
        cx: &Context<Self>,
        session_id: i64,
        toggle: RowToggle,
    ) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
        cx.listener(move |this, _, _, cx| {
            match &toggle {
                RowToggle::Key(key) => {
                    // `insert` reports whether the key was new, so a second
                    // click on the same row collapses it again.
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

    /// Render list row `index`, on the same centered column as the composer.
    fn render_row(&mut self, index: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (Some(row), Some(session_id)) = (self.rows.get(index).cloned(), self.session_id) else {
            return div().into_any_element();
        };
        let body = match row {
            Row::History(index) => self.render_history_entry(index, session_id, cx),
            Row::HistoryTools(range) => {
                let rows = range
                    .filter_map(|index| self.history_tool_row(index))
                    .collect();
                self.render_tool_run(rows, session_id, cx)
            }
            Row::Live(index) => self.render_live_block(index, session_id, cx),
            Row::LiveTools(range) => {
                let rows = range
                    .filter_map(|index| self.live_tool_row(session_id, index))
                    .collect();
                self.render_tool_run(rows, session_id, cx)
            }
            Row::Placeholder => div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("…")
                .into_any_element(),
            Row::Working => self.render_working(session_id, cx),
        };
        div()
            .w_full()
            .max_w(rems(CONTENT_MAX_REM))
            .min_w(rems(CONTENT_MIN_REM))
            .mx_auto()
            .child(body)
            .into_any_element()
    }

    fn render_history_entry(
        &mut self,
        index: usize,
        session_id: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (radius, muted_fg, primary) = (theme.radius, theme.muted_foreground, theme.primary);
        let Some(entry) = self.history.get_mut(index) else {
            return div().into_any_element();
        };
        match entry {
            HistoryEntry::User(text) => h_flex()
                .justify_end()
                .child(
                    div()
                        .max_w(relative(0.78))
                        .px_3()
                        .py_2()
                        .rounded(radius)
                        .bg(primary.opacity(0.1))
                        .child(div().text_sm().child(text.clone())),
                )
                .into_any_element(),
            HistoryEntry::Note(text) => div()
                .text_xs()
                .text_color(muted_fg)
                .child(text.clone())
                .into_any_element(),
            HistoryEntry::Thinking(text) => {
                let key = format!("think-{index}");
                let open = self.expanded_rows.contains(&key);
                let text = text.clone();
                self.render_thinking(
                    format!("think-{index}"),
                    open,
                    false,
                    text,
                    RowToggle::Key(key),
                    session_id,
                    cx,
                )
            }
            HistoryEntry::Agent { text, view } => {
                let view = match view {
                    Some(view) => view.clone(),
                    None => {
                        let source = text.clone();
                        let created = cx.new(|cx| TextViewState::markdown(&source, cx));
                        // A long reply parses in the background; repaint when
                        // it lands.
                        cx.observe(&created, |_, _, cx| cx.notify()).detach();
                        *view = Some(created.clone());
                        created
                    }
                };
                div()
                    .child(TextView::new(&view).text_sm().line_height(relative(1.75)))
                    .into_any_element()
            }
            // Tool entries are always drawn through their run row.
            HistoryEntry::Tool { .. } => div().into_any_element(),
        }
    }

    fn history_tool_row(&self, index: usize) -> Option<ToolRow> {
        let HistoryEntry::Tool {
            title,
            status,
            content,
            decision,
        } = self.history.get(index)?
        else {
            return None;
        };
        let id = format!("tool-{index}");
        Some(ToolRow {
            open: self.expanded_rows.contains(&id),
            toggle: RowToggle::Key(id.clone()),
            id,
            title: title.clone(),
            status: status.clone(),
            content: content.clone(),
            decision: decision.clone(),
        })
    }

    fn live_tool_row(&self, session_id: i64, index: usize) -> Option<ToolRow> {
        let TurnBlock::Tool(run) = self.turns.get(&session_id)?.blocks.get(index)? else {
            return None;
        };
        Some(ToolRow {
            id: format!("tool-live-{}", run.id),
            title: run.title.clone().into(),
            status: run.status.into(),
            content: run.content.clone().into(),
            // The prompt itself lives above the composer; the row only records
            // how the call ended up authorized, and stays quiet while a prompt
            // is still parked on it.
            decision: run
                .pending_permission
                .is_none()
                .then(|| run.decision.clone().map(Into::into))
                .flatten(),
            open: run.open,
            toggle: RowToggle::Block(index),
        })
    }

    fn render_live_block(
        &self,
        index: usize,
        session_id: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(turn) = self.turns.get(&session_id) else {
            return div().into_any_element();
        };
        match turn.blocks.get(index) {
            Some(TurnBlock::Thinking { text, open }) => self.render_thinking(
                format!("thinking-{index}"),
                *open,
                turn.active_thinking == Some(index),
                text.clone().into(),
                RowToggle::Block(index),
                session_id,
                cx,
            ),
            Some(TurnBlock::Answer { view, .. }) => div()
                .child(TextView::new(view).text_sm().line_height(relative(1.75)))
                .into_any_element(),
            // Tool blocks are always drawn through their run row.
            Some(TurnBlock::Tool(_)) | None => div().into_any_element(),
        }
    }

    /// A reasoning block: a header that folds the captured text away, with a
    /// spinner while the agent is still thinking.
    #[allow(clippy::too_many_arguments)]
    fn render_thinking(
        &self,
        id: String,
        open: bool,
        active: bool,
        text: SharedString,
        toggle: RowToggle,
        session_id: i64,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (radius, muted, muted_fg) = (theme.radius, theme.muted, theme.muted_foreground);
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
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .truncate()
                            .child(if active { "思考中…" } else { "已思考" }),
                    )
                    .child(disclosure_caret(open, muted_fg))
                    .on_click(Self::row_toggle_click(cx, session_id, toggle)),
            )
            .content(collapsed_body(format!("{id}-body"), text, muted_fg))
            .into_any_element()
    }

    /// Working-time label at the bottom-left of the reply.
    fn render_working(&self, session_id: i64, cx: &Context<Self>) -> AnyElement {
        let Some(turn) = self.turns.get(&session_id) else {
            return div().into_any_element();
        };
        let muted_fg = cx.theme().muted_foreground;
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
            return match rows.into_iter().next() {
                Some(row) => self.render_tool_row(row, session_id, cx),
                None => div().into_any_element(),
            };
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
                    .on_click(Self::row_toggle_click(cx, session_id, RowToggle::Key(id))),
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
        let (icon, color) = match row.status.as_ref() {
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
                        .on_click(Self::row_toggle_click(cx, session_id, row.toggle)),
                )
                .content(collapsed_body(format!("{id}-body"), row.content, muted_fg))
                .into_any_element()
        };
        // A stored decision explains how the call was authorized, which the
        // status alone does not.
        v_flex()
            .child(element)
            .children(
                row.decision
                    .map(|decision| render_permission_decision(&decision, muted_fg)),
            )
            .into_any_element()
    }
}

impl Render for ConversationView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        MessageScroller::new(
            "conversation",
            self.scroller.clone(),
            cx.processor(|this, index, window, cx| this.render_row(index, window, cx)),
        )
        .with_jump_button_label("回到最新")
        .with_list_style(StyleRefinement::default().py_4())
        // Same rhythm as the column it replaces: side padding around the
        // centered column and a 0.75rem gap between rows.
        .with_row_style(StyleRefinement::default().px_5().pb_3())
        .size_full()
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
/// [`ConversationView::render_tool_run`]).
fn fold_summary(rows: &[ToolRow]) -> Option<(RunStatus, String)> {
    if rows.len() <= 1 {
        return None;
    }
    let failures = rows
        .iter()
        .filter(|row| row.status.as_ref() == "failed")
        .count();
    // Same reading of status as a single row: anything not settled is still
    // in flight.
    let running = rows
        .iter()
        .any(|row| !matches!(row.status.as_ref(), "done" | "failed"));
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
    text: SharedString,
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

    use super::{
        Row, RowChange, RowToggle, RunStatus, ToolRow, diff_rows, fold_rows, fold_summary,
    };

    fn tool_row(status: &str) -> ToolRow {
        ToolRow {
            id: "t".into(),
            title: "工具".into(),
            status: status.to_string().into(),
            content: "".into(),
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

    /// Consecutive tool calls share one row, broken by anything between them,
    /// including a run that is still open at the end.
    #[test]
    fn tool_calls_fold_into_runs() {
        let rows = fold_rows(
            [false, true, true, false, true],
            Row::History,
            Row::HistoryTools,
        );
        assert_eq!(
            rows,
            vec![
                Row::History(0),
                Row::HistoryTools(1..3),
                Row::History(3),
                Row::HistoryTools(4..5),
            ]
        );
        assert_eq!(fold_rows([], Row::Live, Row::LiveTools), vec![]);
    }

    /// Streaming into the last block keeps every slot, so the list remeasures
    /// the live rows in place and splices nothing — the reader's scroll
    /// position is anchored rather than reset.
    #[test]
    fn a_growing_turn_remeasures_without_splicing() {
        let old = vec![Row::History(0), Row::Live(0), Row::Working];
        let RowChange { remeasure, splice } = diff_rows(&old, &old);
        assert_eq!(remeasure, Some(1..3), "history rows are left alone");
        assert_eq!(splice, None);
    }

    /// A call joining a run keeps the run's slot: remeasured, not replaced.
    #[test]
    fn a_growing_run_keeps_its_slot() {
        let old = vec![Row::History(0), Row::LiveTools(0..1), Row::Working];
        let new = vec![Row::History(0), Row::LiveTools(0..2), Row::Working];
        let RowChange { remeasure, splice } = diff_rows(&old, &new);
        assert_eq!(remeasure, Some(1..3));
        assert_eq!(splice, None);
    }

    /// A new block changes the tail's shape: only the rows from the first
    /// differing slot on are spliced.
    #[test]
    fn a_new_block_splices_only_the_tail() {
        let old = vec![Row::History(0), Row::Placeholder, Row::Working];
        let new = vec![Row::History(0), Row::Live(0), Row::Working];
        let RowChange { remeasure, splice } = diff_rows(&old, &new);
        assert_eq!(remeasure, None);
        assert_eq!(splice, Some((1..3, 2)));
    }

    /// A saved turn turns live rows into history rows appended after the old
    /// history, which stays measured.
    #[test]
    fn a_saved_turn_replaces_the_live_tail() {
        let old = vec![Row::History(0), Row::Live(0), Row::Working];
        let new = vec![Row::History(0), Row::History(1)];
        let RowChange { remeasure, splice } = diff_rows(&old, &new);
        assert_eq!(remeasure, None);
        assert_eq!(splice, Some((1..3, 1)));
    }
}
