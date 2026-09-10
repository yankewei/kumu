//! The vocabulary crossing the ACP boundary: what the UI sends down ([`Cmd`])
//! and what comes back up ([`Event`]), plus the value types they carry.
//!
//! Everything the UI sees is defined here, so no protocol type leaks out of
//! this module tree.

use std::sync::mpsc::Sender;

/// Events flowing from the ACP thread to the UI.
pub enum Event {
    /// Log / connection chatter (grey text).
    Log(String),
    /// A chunk of agent reply text; append to the current agent row.
    Agent(String),
    /// A chunk of the agent's internal reasoning; transient (not persisted).
    Thought(String),
    /// A tool call started or changed. `title`/`status` are empty when the
    /// update didn't touch them; `status` is one of pending/running/done/failed.
    /// `content` is the flattened text output (empty when none).
    ToolCall {
        id: String,
        title: String,
        status: &'static str,
        content: String,
    },
    /// The agent named the session (session/info update with a title).
    SessionTitle { title: String },
    /// The prompt turn ended with this stop reason.
    Done(String),
    /// The agent is asking for authorization before running a tool call. The UI
    /// shows the options on the matching tool row and answers back via
    /// [`crate::acp::Connection::resolve_permission`].
    PermissionRequest {
        /// Tool call this request belongs to, so the UI can attach the prompt
        /// to the row it already rendered.
        tool_call_id: String,
        /// Stable id used by the UI to answer this request.
        request_id: String,
        /// The agent's description of what it wants to do.
        title: String,
        /// What the agent is proposing, taken from the tool call's content
        /// (text or file diff) and falling back to its `raw_input`; may be
        /// empty when the agent describes nothing.
        detail: String,
        /// The options the agent offers, passed through verbatim.
        options: Vec<PermissionOptionInfo>,
    },
    /// A fatal error occurred.
    Error(String),
    /// The models an agent exposes, from its session's model config option
    /// (sent by both probes and live connections).
    Models {
        agent: &'static str,
        models: Vec<ModelInfo>,
        /// The value the agent reports as active for this session.
        current: Option<String>,
    },
    /// A probe connection failed (agent missing / handshake error).
    ProbeFailed { agent: &'static str },
}

/// One selectable model as exposed by an agent.
#[derive(Clone, Debug)]
pub struct ModelInfo {
    /// Value to pass to `session/set_config_option`.
    pub value: String,
    /// Human-readable label.
    pub name: String,
    /// Provider inferred from the value prefix before `/`.
    pub provider: String,
}

/// What a permission option means, used to pick the UI treatment. The label is
/// always the agent's own `name`; this only decides the button styling.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PermissionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    /// A kind this build doesn't know about (`PermissionOptionKind` is
    /// `#[non_exhaustive]`); rendered neutrally rather than as a rejection.
    Other,
}

/// One permission option offered by the agent, passed through verbatim.
#[derive(Clone, Debug)]
pub struct PermissionOptionInfo {
    /// Value to echo back in the response.
    pub id: String,
    /// Label to display.
    pub name: String,
    /// Semantic classification for styling.
    pub kind: PermissionKind,
}

/// UI -> ACP commands on the channel returned by [`crate::acp::spawn`].
pub enum Cmd {
    Prompt(String),
    /// Select the session's model by its config-option value.
    SetModel(String),
}

/// Send an event to the UI, echoing to stderr when KUMU_DEBUG is set (handy
/// for headless verification).
pub(super) fn emit(tx: &Sender<Event>, ev: Event) {
    if std::env::var("KUMU_DEBUG").is_ok() {
        match &ev {
            Event::Agent(t) => eprintln!("[acp] agent: {t}"),
            Event::Thought(t) => eprintln!("[acp] thought: {t}"),
            Event::ToolCall {
                id,
                title,
                status,
                content,
            } => {
                eprintln!(
                    "[acp] tool {id}: {title} ({status}) content={} chars",
                    content.len()
                )
            }
            Event::SessionTitle { title } => eprintln!("[acp] title: {title}"),
            Event::Done(s) => eprintln!("[acp] done: {s}"),
            Event::PermissionRequest {
                tool_call_id,
                request_id,
                options,
                ..
            } => eprintln!(
                "[acp] permission {request_id} for tool {tool_call_id}: {} options",
                options.len()
            ),
            Event::Error(s) => eprintln!("[acp] error: {s}"),
            Event::Log(s) => eprintln!("[acp] log: {s}"),
            Event::Models {
                agent,
                models,
                current,
            } => eprintln!(
                "[acp] models: {agent} exposes {} models (active: {:?}) across {} providers",
                models.len(),
                current,
                models
                    .iter()
                    .map(|m| m.provider.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .len()
            ),
            Event::ProbeFailed { agent } => eprintln!("[acp] probe {agent} failed"),
        }
    }
    let _ = tx.send(ev);
}
