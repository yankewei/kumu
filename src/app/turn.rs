//! A live turn as data: what the agent has streamed so far, in arrival order,
//! and how each [`agent::Event`] that belongs to a turn folds into it.
//!
//! Nothing here renders. The conversation keeps its own caches (parsed
//! markdown, which rows are expanded) keyed by [`Turn::id`] and a block's index,
//! which is stable because blocks are only ever appended: a tool update changes
//! the block it first created, and nothing is removed until the turn ends.

use std::time::{Duration, Instant};

use crate::agent;

/// Distinguishes turns so a render cache keyed on one cannot be picked up by
/// the next, even when both are the same session's and start at block 0.
static NEXT_TURN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A permission request waiting for the user, shown above the composer while
/// its tool call is still parked on the agent's side.
#[derive(Clone)]
pub struct PendingPermission {
    pub request_id: String,
    pub title: String,
    pub detail: String,
    pub options: Vec<agent::PermissionOptionInfo>,
}

/// One tool call observed during the live turn, in first-seen order.
pub struct ToolRun {
    pub id: String,
    pub title: String,
    /// pending / running / done / failed
    pub status: &'static str,
    /// Flattened text output from the agent.
    pub content: String,
    /// Authorization the agent is still waiting on for this call.
    pub pending_permission: Option<PendingPermission>,
    /// The choice the user made (the agent's own option label), kept on the row
    /// so the conversation shows how the call was authorized.
    pub decision: Option<String>,
}

/// Blocks stay in arrival order; tool updates mutate their original block.
pub enum TurnBlock {
    Thinking {
        text: String,
        /// The agent's own label for the message this reasoning belongs to.
        ///
        /// The row is held open by it rather than by what came last: an agent
        /// with a background task running reports that task's progress between
        /// two chunks of one message, and grouping by adjacency broke one
        /// sentence into a row per chunk. `None` is an agent that labels
        /// nothing, and then adjacency is all there is to go on.
        message: Option<String>,
    },
    Tool(ToolRun),
    /// Reply text; only ever appended to, which is what lets the conversation
    /// feed its markdown view the new tail instead of re-parsing the whole.
    Answer(String),
}

pub struct Turn {
    id: u64,
    started_at: Instant,
    pub blocks: Vec<TurnBlock>,
}

impl Turn {
    pub fn new() -> Self {
        Self {
            id: NEXT_TURN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            started_at: Instant::now(),
            blocks: Vec::new(),
        }
    }

    /// This turn's identity, for caches that outlive a single frame.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// How long the turn has been running. Read as it closes, so the total can
    /// be recorded against the message that ends it.
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Append reply text, continuing the reply block when it is the last one.
    pub fn push_answer(&mut self, chunk: String) {
        if let Some(TurnBlock::Answer(text)) = self.blocks.last_mut() {
            text.push_str(&chunk);
        } else {
            self.blocks.push(TurnBlock::Answer(chunk));
        }
    }

    /// Append a reasoning chunk to the row it belongs to, opening one when the
    /// chunk starts a piece of reasoning of its own.
    ///
    /// Which row that is comes from the agent's own label for the message, not
    /// from what arrived last: a background task reports its progress between
    /// two chunks of one message, and those reports used to cut the message's
    /// reasoning into a row per chunk.
    pub fn push_thinking(&mut self, text: String, message: Option<String>) {
        if let Some(TurnBlock::Thinking {
            text: current,
            message: held,
        }) = self.blocks.last_mut()
            && continues_reasoning(held.as_deref(), message.as_deref())
        {
            current.push_str(&text);
        } else {
            self.blocks.push(TurnBlock::Thinking { text, message });
        }
    }

    /// Start or update a tool call. Empty fields are ones the update did not
    /// touch, so they leave the call as it was.
    pub fn apply_tool(&mut self, id: String, title: String, status: &'static str, content: String) {
        if let Some(run) = self.tool_mut(&id) {
            if !status.is_empty() {
                run.status = status;
            }
            if !title.is_empty() {
                run.title = title;
            }
            if !content.is_empty() {
                run.content = content;
            }
        } else {
            self.blocks.push(TurnBlock::Tool(ToolRun {
                id,
                title,
                status: if status.is_empty() { "running" } else { status },
                content,
                pending_permission: None,
                decision: None,
            }));
        }
    }

    /// Park a permission request on the call it belongs to, returning the
    /// request it replaces — one prompt per row, so the caller settles the
    /// earlier one as cancelled.
    ///
    /// The request can arrive before the tool call it refers to; the row is then
    /// seeded from what the request itself carries so the prompt is never dropped.
    pub fn request_permission(
        &mut self,
        tool_call_id: String,
        pending: PendingPermission,
    ) -> Option<String> {
        if self.tool_mut(&tool_call_id).is_none() {
            self.blocks.push(TurnBlock::Tool(ToolRun {
                id: tool_call_id.clone(),
                title: pending.title.clone(),
                status: "pending",
                content: String::new(),
                pending_permission: None,
                decision: None,
            }));
        }
        self.tool_mut(&tool_call_id)?
            .pending_permission
            .replace(pending)
            .map(|previous| previous.request_id)
    }

    /// Record the user's decision on the row its request was parked on.
    pub fn resolve_permission(&mut self, request_id: &str, label: String) {
        for block in &mut self.blocks {
            let TurnBlock::Tool(run) = block else {
                continue;
            };
            if run
                .pending_permission
                .as_ref()
                .is_some_and(|p| p.request_id == request_id)
            {
                run.pending_permission = None;
                run.decision = Some(label);
                return;
            }
        }
    }

    /// Take every request still waiting on the user, for the caller to answer
    /// as cancelled.
    pub fn take_pending_permissions(&mut self) -> Vec<String> {
        self.blocks
            .iter_mut()
            .filter_map(|block| match block {
                TurnBlock::Tool(run) => run.pending_permission.take().map(|p| p.request_id),
                _ => None,
            })
            .collect()
    }

    /// The oldest outstanding permission prompt, if any.
    ///
    /// Only one is shown at a time: the agent is blocked on the earliest request
    /// anyway, and a single card keeps the input area from growing unbounded.
    pub fn pending_permission(&self) -> Option<&PendingPermission> {
        self.blocks.iter().find_map(|block| match block {
            TurnBlock::Tool(run) => run.pending_permission.as_ref(),
            _ => None,
        })
    }

    /// Whether the block at `index` is reasoning still being written: only the
    /// last block can be, and anything pushed after it ends it.
    pub fn is_thinking(&self, index: usize) -> bool {
        index + 1 == self.blocks.len() && matches!(self.blocks[index], TurnBlock::Thinking { .. })
    }

    /// The turn as the rows it is saved as — `(role, content)` in block order,
    /// with blank blocks left out.
    pub fn into_records(self) -> Vec<(&'static str, String)> {
        self.blocks
            .into_iter()
            .map(|block| match block {
                TurnBlock::Thinking { text, .. } => ("thinking", text),
                TurnBlock::Answer(text) => ("agent", text),
                TurnBlock::Tool(run) => (
                    "tool",
                    serde_json::json!({
                        "title": run.title,
                        "status": run.status,
                        "content": run.content,
                        "decision": run.decision,
                    })
                    .to_string(),
                ),
            })
            .filter(|(_, text)| !text.trim().is_empty())
            .collect()
    }

    fn tool_mut(&mut self, id: &str) -> Option<&mut ToolRun> {
        self.blocks.iter_mut().find_map(|block| match block {
            TurnBlock::Tool(run) if run.id == id => Some(run),
            _ => None,
        })
    }
}

/// Whether a reasoning chunk belongs to the piece of reasoning a row holds.
///
/// With both labelled the label decides, so one message's chunks stay one row
/// whatever lands between them. With either unlabelled nothing contradicts the
/// row: an agent that labels nothing (Pi sends no id on a streamed message) is
/// one the UI only has adjacency for.
fn continues_reasoning(held: Option<&str>, chunk: Option<&str>) -> bool {
    match (held, chunk) {
        (Some(held), Some(chunk)) => held == chunk,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{PendingPermission, Turn, TurnBlock, continues_reasoning};

    /// The text of the turn's `index`-th block, which has to be a thinking one.
    fn thinking_text(turn: &Turn, index: usize) -> String {
        match &turn.blocks[index] {
            TurnBlock::Thinking { text, .. } => text.clone(),
            _ => panic!("block {index} is not a thinking block"),
        }
    }

    fn pending(request_id: &str) -> PendingPermission {
        PendingPermission {
            request_id: request_id.into(),
            title: "运行命令".into(),
            detail: String::new(),
            options: Vec::new(),
        }
    }

    /// Whether a chunk continues the row before it, over every combination of
    /// labels: equal labels stay together, and a side that labelled nothing
    /// cannot contradict the row, so it joins it.
    #[test]
    fn a_chunk_continues_a_row_unless_the_labels_disagree() {
        assert!(continues_reasoning(Some("m1"), Some("m1")));
        assert!(continues_reasoning(None, None));
        assert!(continues_reasoning(None, Some("m1")));
        assert!(continues_reasoning(Some("m1"), None));
        assert!(!continues_reasoning(Some("m1"), Some("m2")));
    }

    /// One message's reasoning is one row however the stream is interrupted,
    /// and another message's opens a row of its own.
    #[test]
    fn reasoning_is_grouped_by_the_agents_message_label() {
        let mut turn = Turn::new();
        turn.push_thinking("Still running. ".into(), Some("m1".into()));
        turn.push_thinking("Let me continue".into(), Some("m1".into()));
        turn.push_thinking("second".into(), Some("m2".into()));

        assert_eq!(turn.blocks.len(), 2);
        assert_eq!(thinking_text(&turn, 0), "Still running. Let me continue");
        assert_eq!(thinking_text(&turn, 1), "second");
    }

    /// A row that is no longer the last one is not reopened: something arriving
    /// between two chunks of one message keeps the order the agent sent.
    #[test]
    fn a_row_is_never_reopened_across_what_came_after_it() {
        let mut turn = Turn::new();
        turn.push_thinking("before".into(), Some("m1".into()));
        turn.apply_tool("t1".into(), "读取".into(), "running", String::new());
        turn.push_thinking("after".into(), Some("m1".into()));

        assert_eq!(turn.blocks.len(), 3);
        assert_eq!(thinking_text(&turn, 2), "after");
    }

    /// A tool update changes the block the call first created, wherever it now
    /// sits, and leaves fields the update did not carry alone.
    #[test]
    fn a_tool_update_changes_its_original_block() {
        let mut turn = Turn::new();
        turn.apply_tool("t1".into(), "读取".into(), "", String::new());
        turn.push_answer("中间".into());
        turn.apply_tool("t1".into(), String::new(), "done", "输出".into());

        assert_eq!(turn.blocks.len(), 2);
        let TurnBlock::Tool(run) = &turn.blocks[0] else {
            panic!("the call keeps its slot");
        };
        assert_eq!((run.title.as_str(), run.status), ("读取", "done"));
        assert_eq!(run.content, "输出");
    }

    /// A request that arrives before its tool call seeds the row, the call's
    /// own events then update that same row, and a second request on it hands
    /// back the first for cancelling.
    #[test]
    fn a_permission_request_may_arrive_before_its_tool_call() {
        let mut turn = Turn::new();
        assert_eq!(turn.request_permission("t1".into(), pending("p1")), None);
        turn.apply_tool("t1".into(), String::new(), "running", String::new());

        assert_eq!(turn.blocks.len(), 1, "the call joins the seeded row");
        assert_eq!(
            turn.pending_permission().map(|p| p.request_id.as_str()),
            Some("p1")
        );
        assert_eq!(
            turn.request_permission("t1".into(), pending("p2")),
            Some("p1".into())
        );

        turn.resolve_permission("p2", "允许".into());
        assert!(turn.pending_permission().is_none());
        let TurnBlock::Tool(run) = &turn.blocks[0] else {
            unreachable!()
        };
        assert_eq!(run.decision.as_deref(), Some("允许"));
    }

    /// A saved turn keeps block order, joins streamed reply text, and drops
    /// blocks with nothing in them.
    #[test]
    fn a_turn_saves_as_its_blocks_in_order() {
        let mut turn = Turn::new();
        turn.push_thinking("  ".into(), None);
        turn.apply_tool("t1".into(), "读取".into(), "done", String::new());
        turn.push_answer("你".into());
        turn.push_answer("好".into());

        let records = turn.into_records();
        let roles: Vec<_> = records.iter().map(|(role, _)| *role).collect();
        assert_eq!(roles, ["tool", "agent"]);
        assert_eq!(records[1].1, "你好");
    }
}
