//! The vocabulary crossing the backend boundary: what the UI sends down
//! ([`Cmd`]) and what comes back up ([`Event`]), plus the value types they
//! carry.
//!
//! Everything the UI sees is defined here, so no protocol type leaks out of
//! this module tree — and nothing here names a protocol either.

use std::sync::mpsc::Sender;

/// Events flowing from the backend thread up to the app.
pub enum Event {
    /// Log / connection chatter (grey text).
    Log(String),
    /// A chunk of agent reply text; append to the current agent row.
    Agent(String),
    /// A chunk of the agent's internal reasoning; transient (not persisted).
    ///
    /// `message` is the agent's own label for the piece of reasoning the chunk
    /// belongs to, when it gives one: chunks that share a label are one piece,
    /// and a chunk labelled differently starts another. The UI groups its rows
    /// by it rather than by what came before, because what comes between two
    /// chunks of one piece is not always reasoning — an agent working a
    /// background task reports its progress while it goes on thinking, and
    /// those reports arrive on this same channel.
    ///
    /// An agent that labels nothing (Pi sends no id on a streamed message)
    /// leaves it `None`, and then consecutive chunks simply run together.
    Thought {
        text: String,
        message: Option<String>,
    },
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
    /// The backend opened a conversation and supplied its durable restore cursor.
    ///
    /// `resumable` says whether that cursor can actually reopen the conversation
    /// in a later connection. An ACP agent that does not advertise `load_session`
    /// still hands back an id for the session it just created, but will refuse
    /// to load it, so dropping that connection loses the conversation for good;
    /// the flag is `false` there, and `true` everywhere a reconnect can restore
    /// the session from `resume_id`.
    SessionReady { resume_id: String, resumable: bool },
    /// The prompt turn ended with this stop reason.
    Done(String),
    /// The agent is asking for authorization before running a tool call. The UI
    /// shows the options on the matching tool row and answers back via
    /// [`crate::agent::Connection::resolve_permission`].
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
    /// The options an agent exposes for a session — models, modes and sandbox
    /// policy — each with the value the agent reports as active. Sent by both
    /// probes and live connections.
    ///
    /// Agents differ in which axes they have: modes are an ACP config option,
    /// the sandbox is Codex's own setting, and reasoning is an ACP config
    /// option that Codex only has per model. An empty list is therefore the
    /// normal way to say "this agent has no such axis", and the matching picker
    /// renders nothing rather than an empty menu.
    SessionOptions {
        agent: &'static str,
        models: Vec<ModelInfo>,
        current_model: Option<String>,
        /// The work modes the session offers: what the agent is doing, as
        /// opposed to how much it may do on its own. ACP agents report their
        /// own (`build` / `plan`); Claude Code has no such flag, so its backend
        /// derives the same two from the one permission mode it does have.
        modes: Vec<ModeInfo>,
        current_mode: Option<String>,
        /// The access policies the session offers — how much the agent may do
        /// before it has to ask. Claude Code's permission modes fill this axis;
        /// nothing else kumu drives exposes one yet.
        access: Vec<AccessInfo>,
        current_access: Option<String>,
        sandboxes: Vec<SandboxInfo>,
        current_sandbox: Option<String>,
        /// The reasoning efforts the session offers. Session-level on purpose:
        /// ACP reports it as its own config option, and Codex derives it from
        /// the selected model, so the two shapes are reconciled inside each
        /// backend and only one reaches the UI.
        reasoning: Vec<ReasoningInfo>,
        /// The effort the agent reports it is running at; `None` when it has no
        /// such axis or has not reported one yet.
        current_reasoning: Option<String>,
    },
    /// The slash commands this agent says it will run, in the agent's own
    /// order. Its own event rather than a field of [`Event::SessionOptions`]:
    /// a command set has no value in force, and it arrives on its own schedule
    /// — at the handshake, and again whenever the agent changes it.
    ///
    /// An empty list is the honest report for an agent that has none, which is
    /// what keeps the menu off the composer there rather than offering commands
    /// that could not run. Codex's app-server has no slash commands of its own
    /// (they live in its TUI), so its backend reports the few built-ins it runs
    /// itself, then the session's skills.
    Commands {
        agent: &'static str,
        commands: Vec<CommandInfo>,
    },
    /// A probe connection failed (agent missing / handshake error).
    ProbeFailed { agent: &'static str },
    /// How full the session's context is, and what it has cost so far.
    Usage(Usage),
}

/// How full a session's context is, as the agent last reported it.
///
/// Every field is optional because the protocols disagree on what they report:
/// ACP's cost is optional, Codex reports no cost at all, and Pi forgets how full
/// the context is right after a compaction until the next reply measures it.
/// Each report replaces the last one, except that a report which does not know
/// the window leaves the last known one standing: a Claude Code process only
/// learns it at the end of its first turn, and the model rarely changes.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    /// Tokens in the context now: what the next request will send.
    pub used: Option<u64>,
    /// The model's context window, in tokens.
    pub window: Option<u64>,
    /// What the session has cost so far.
    pub cost: Option<Cost>,
}

impl Usage {
    /// A report as an agent gives it, with zero read as "not known": no agent
    /// has a context window of zero, and a context or cost of zero is what one
    /// reports before it has measured anything.
    pub(super) fn reported(used: Option<u64>, window: Option<u64>, cost: Option<Cost>) -> Self {
        Self {
            used: used.filter(|used| *used > 0),
            window: window.filter(|window| *window > 0),
            cost: cost.filter(|cost| cost.amount > 0.),
        }
    }
}

/// An amount of money in an ISO 4217 currency ("USD").
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Cost {
    pub amount: f64,
    pub currency: String,
}

impl Cost {
    pub fn usd(amount: f64) -> Self {
        Self {
            amount,
            currency: "USD".to_string(),
        }
    }
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

/// One slash command an agent says it will run, as reported by its own
/// catalogue.
///
/// Not an axis: a command has no value "in force", and the set may change
/// mid-session (ACP re-sends it whenever it likes). It is also not something
/// kumu resolves — the agent recognizes the `/name` prefix in a prompt, so
/// invoking one is just sending it.
///
/// The description an agent offers is deliberately not carried: the composer's
/// lists show the same source's long prose nowhere else (a picker row is the
/// name and nothing more), so it is dropped at this boundary rather than
/// carried up to be discarded at render time.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandInfo {
    /// The command's name, without a leading `/`.
    pub name: String,
    /// What the command expects after its name, when the agent says.
    pub hint: Option<String>,
}

/// One selectable reasoning effort, as exposed by an agent.
///
/// Deliberately not [`ModeInfo`]: modes and reasoning are different axes, and
/// they are carried separately so an agent may have either, both, or neither.
/// ACP's options are session-wide (`thought_level`); Codex advertises a set per
/// model, which its backend reduces to this same session-level list.
#[derive(Clone, Debug)]
pub struct ReasoningInfo {
    /// Value to send back when the user picks this effort.
    pub value: String,
    /// Human-readable label; falls back to the value for an agent that names
    /// its efforts nothing better (Codex describes them but does not label
    /// them).
    pub name: String,
}

/// One selectable session mode as exposed by an agent's mode config option.
/// The set is the agent's own (there is no fixed read-only / ask / code
/// vocabulary), and so is its order.
#[derive(Clone, Debug)]
pub struct ModeInfo {
    /// Value to pass to `session/set_config_option`.
    pub value: String,
    /// Human-readable label.
    pub name: String,
}

/// One selectable access policy: how much the agent may do before it has to
/// ask, as opposed to what it is doing ([`ModeInfo`]) or the boundary it runs
/// inside ([`SandboxInfo`]).
#[derive(Clone, Debug)]
pub struct AccessInfo {
    /// Value to send back when the user picks this policy.
    pub value: String,
    /// Human-readable label.
    pub name: String,
}

/// One selectable sandbox policy, for agents that expose one (Codex's
/// `read-only` / `workspace-write` / `danger-full-access`).
///
/// Deliberately not [`ModeInfo`]: these are different axes, and an agent may
/// have either, both, or neither.
#[derive(Clone, Debug)]
pub struct SandboxInfo {
    /// Value to send back when the user picks this policy.
    pub value: String,
    /// Human-readable label.
    pub name: String,
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

/// What the user sends as one prompt: the text, and the images attached to it.
///
/// Images travel as files rather than bytes. Codex's app-server takes a local
/// path and reads the file itself, while the other protocols want the bytes
/// inline — so each backend reads the file only if its wire needs it, and a
/// prompt stays cheap to move across the channel.
///
/// A file the user references by name is not an attachment: it is written into
/// the text as `@path`, and the agent reads it with its own tools from the
/// project directory it runs in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prompt {
    pub text: String,
    pub images: Vec<ImageAttachment>,
}

impl From<&str> for Prompt {
    fn from(text: &str) -> Self {
        text.to_string().into()
    }
}

impl From<String> for Prompt {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

/// An image attached to a prompt: a file on disk and its MIME type.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageAttachment {
    pub path: std::path::PathBuf,
    pub mime: String,
}

impl ImageAttachment {
    /// The file's bytes as base64, for the protocols that carry images inline;
    /// the error is the message the user is shown.
    pub(super) fn base64(&self) -> Result<String, String> {
        use base64::Engine as _;
        std::fs::read(&self.path)
            .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes))
            .map_err(|error| format!("读取图片 {} 失败：{error}", self.path.display()))
    }
}

/// UI -> backend commands on the channel returned by [`crate::agent::spawn`].
pub enum Cmd {
    Prompt(Prompt),
    /// Select the session's model by its config-option value.
    SetModel(String),
    /// Select the session's mode by its config-option value.
    SetMode(String),
    /// Select the session's sandbox policy by its value.
    SetSandbox(String),
    /// Select the session's reasoning effort by its value.
    SetReasoning(String),
}

/// Send an event to the UI, echoing to stderr when KUMU_DEBUG is set (handy
/// for headless verification). The prefix is the layer, not a protocol: every
/// backend reports through here.
pub(super) fn emit(tx: &Sender<Event>, ev: Event) {
    if std::env::var("KUMU_DEBUG").is_ok() {
        match &ev {
            Event::Agent(t) => eprintln!("[agent] agent: {t}"),
            Event::Thought { text, .. } => eprintln!("[agent] thought: {text}"),
            Event::ToolCall {
                id,
                title,
                status,
                content,
            } => {
                eprintln!(
                    "[agent] tool {id}: {title} ({status}) content={} chars",
                    content.len()
                )
            }
            Event::SessionTitle { title } => eprintln!("[agent] title: {title}"),
            Event::SessionReady {
                resume_id,
                resumable,
            } => eprintln!("[agent] session ready: {resume_id} (resumable: {resumable})"),
            Event::Done(s) => eprintln!("[agent] done: {s}"),
            Event::PermissionRequest {
                tool_call_id,
                request_id,
                options,
                ..
            } => eprintln!(
                "[agent] permission {request_id} for tool {tool_call_id}: {} options",
                options.len()
            ),
            Event::Error(s) => eprintln!("[agent] error: {s}"),
            Event::Log(s) => eprintln!("[agent] log: {s}"),
            Event::SessionOptions {
                agent,
                models,
                current_model,
                modes,
                current_mode,
                access,
                current_access,
                sandboxes,
                current_sandbox,
                reasoning,
                current_reasoning,
            } => eprintln!(
                "[agent] options: {agent} exposes {} models (active: {current_model:?}) across {} providers; \
                 modes: [{}] (active: {current_mode:?}); access: {} (active: {current_access:?}); sandboxes: [{}] (active: {current_sandbox:?}); \
                 reasoning: [{}] (active: {current_reasoning:?})",
                models.len(),
                models
                    .iter()
                    .map(|m| m.provider.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                modes
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                access
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                sandboxes
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                reasoning
                    .iter()
                    .map(|r| r.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            Event::Commands { agent, commands } => eprintln!(
                "[agent] commands: {agent} offers {} ({})",
                commands.len(),
                commands
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            Event::ProbeFailed { agent } => eprintln!("[agent] probe {agent} failed"),
            Event::Usage(usage) => eprintln!("[agent] usage: {usage:?}"),
        }
    }
    let _ = tx.send(ev);
}
