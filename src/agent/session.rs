//! The contract every backend's session loop implements.
//!
//! Four protocols arrive at the same handful of shapes, and each one used to
//! carry its own copy: the twelve-field options event, the order a settled turn
//! is reported in, the permission round trip, and what teardown releases. Those
//! live here so a convention is stated once instead of being cross-referenced
//! between protocol modules by comment.
//!
//! What deliberately does *not* live here is everything the protocols really do
//! differently: the transport, the handshake, and how a turn is waited on (ACP
//! awaits its prompt, Codex and Claude park on a `turn_done` oneshot, Pi follows
//! an event stream). Those stay in the module that owns them.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use super::connection::{PermissionTable, WeakPermissions};
use super::event::{
    AccessInfo, Event, ModeInfo, ModelInfo, PermissionOptionInfo, ReasoningInfo, SandboxInfo, emit,
};

/// Assembles [`Event::SessionOptions`], whose twelve fields are the same for
/// every agent; only the source of each list differs.
///
/// Every axis starts empty, which is the established way to say "this agent has
/// no such axis": the matching picker renders nothing rather than an empty menu.
/// So a backend fills in only the axes it actually has, and saying nothing about
/// an axis is how it reports not having one.
pub(super) struct SessionOptionsBuilder {
    agent: &'static str,
    models: Vec<ModelInfo>,
    current_model: Option<String>,
    modes: Vec<ModeInfo>,
    current_mode: Option<String>,
    access: Vec<AccessInfo>,
    current_access: Option<String>,
    sandboxes: Vec<SandboxInfo>,
    current_sandbox: Option<String>,
    reasoning: Vec<ReasoningInfo>,
    current_reasoning: Option<String>,
}

impl SessionOptionsBuilder {
    pub(super) fn new(agent: &'static str) -> Self {
        Self {
            agent,
            models: Vec::new(),
            current_model: None,
            modes: Vec::new(),
            current_mode: None,
            access: Vec::new(),
            current_access: None,
            sandboxes: Vec::new(),
            current_sandbox: None,
            reasoning: Vec::new(),
            current_reasoning: None,
        }
    }

    pub(super) fn models(mut self, list: Vec<ModelInfo>, current: Option<String>) -> Self {
        self.models = list;
        self.current_model = current;
        self
    }

    pub(super) fn modes(mut self, list: Vec<ModeInfo>, current: Option<String>) -> Self {
        self.modes = list;
        self.current_mode = current;
        self
    }

    pub(super) fn access(mut self, list: Vec<AccessInfo>, current: Option<String>) -> Self {
        self.access = list;
        self.current_access = current;
        self
    }

    pub(super) fn sandboxes(mut self, list: Vec<SandboxInfo>, current: Option<String>) -> Self {
        self.sandboxes = list;
        self.current_sandbox = current;
        self
    }

    pub(super) fn reasoning(mut self, list: Vec<ReasoningInfo>, current: Option<String>) -> Self {
        self.reasoning = list;
        self.current_reasoning = current;
        self
    }

    pub(super) fn build(self) -> Event {
        Event::SessionOptions {
            agent: self.agent,
            models: self.models,
            current_model: self.current_model,
            modes: self.modes,
            current_mode: self.current_mode,
            access: self.access,
            current_access: self.current_access,
            sandboxes: self.sandboxes,
            current_sandbox: self.current_sandbox,
            reasoning: self.reasoning,
            current_reasoning: self.current_reasoning,
        }
    }
}

/// The stop reason that marks a turn the user cancelled.
///
/// Every protocol has its own word for the same thing — ACP's
/// `StopReason::Cancelled`, Codex's `interrupted`, Pi's `aborted` — and the UI
/// must not have to know which agent it is talking to, so each backend reports
/// this spelling instead of its own.
pub(super) const CANCELLED: &str = "cancelled";

/// The channel the UI asks for the turn in flight to be cancelled on.
///
/// Out of band, for the same reason permission answers are (see
/// `super::connection`): the command loop is parked on the very turn being
/// cancelled, so a command would not be read until that turn had already ended
/// — which is the one moment a cancel is useless.
pub(super) type CancelRequests = tokio::sync::mpsc::UnboundedReceiver<()>;

/// Drop cancel requests that arrived after their turn had ended.
///
/// A cancel is only meaningful while a turn is open, and the UI cannot click
/// pause once it is not; this only covers the race between the click and the
/// turn ending, so the request cannot settle the *next* turn by mistake.
pub(super) fn forget_stale_cancels(cancels: &mut CancelRequests) {
    while cancels.try_recv().is_ok() {}
}

/// What a settled turn reports, in the one order that is correct for every
/// backend: `Done` first, then the failure — if there was one — second.
///
/// The UI persists a turn's blocks when it ends, and an error ends it too, so
/// reporting a failure ahead of the settle would throw away everything that did
/// stream before the run failed.
pub(super) fn settle(stop: impl Into<String>, failure: Option<String>) -> Vec<Event> {
    let mut events = vec![Event::Done(stop.into())];
    if let Some(detail) = failure {
        events.push(Event::Error(detail));
    }
    events
}

/// The question the UI is asked before a tool call runs.
pub(super) struct PermissionQuestion {
    pub tool_call_id: String,
    pub title: String,
    pub detail: String,
    pub options: Vec<PermissionOptionInfo>,
}

/// Hand one approval question to the UI, and let `answer` reply to the agent
/// with whatever the user chose — `None` when nobody answered.
///
/// Each protocol answers in its own shape, so only the wiring is shared and the
/// response is the caller's closure. The UI answers by taking the sender out of
/// the same table, so the upgraded handle is released before the task below is
/// spawned: it never crosses an await, and it does not hold up teardown.
pub(super) fn ask_user<F>(
    table: Arc<PermissionTable>,
    next_id: &AtomicU64,
    ev_tx: &Sender<Event>,
    question: PermissionQuestion,
    answer: F,
) where
    F: FnOnce(Option<String>) + Send + 'static,
{
    // The agent does not give us an id of our own to correlate on.
    let request_id = format!("perm-{}", next_id.fetch_add(1, Ordering::Relaxed));
    let (tx, rx) = tokio::sync::oneshot::channel();
    table.lock().unwrap().insert(request_id.clone(), tx);
    drop(table);
    emit(
        ev_tx,
        Event::PermissionRequest {
            tool_call_id: question.tool_call_id,
            request_id,
            title: question.title,
            detail: question.detail,
            options: question.options,
        },
    );
    // Waiting for the user here would stall the dispatch loop and every other
    // message on this connection, so the responder moves into its own task.
    tokio::spawn(async move {
        let option_id = rx.await.ok().flatten();
        answer(option_id);
    });
}

/// Wake anything still waiting on a decision, so the agent is never left hanging
/// on a connection that is going away.
///
/// Dropping the pending senders is what does it: the waiting task wakes with no
/// answer and replies with the protocol's own "nobody replied" outcome. When the
/// UI already dropped its handle the table is gone, and this is unnecessary.
pub(super) fn release_pending_permissions(permissions: &WeakPermissions) {
    if let Some(table) = permissions.upgrade() {
        table.lock().unwrap().clear();
    }
}
