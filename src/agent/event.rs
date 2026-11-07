//! The vocabulary probes report upward ([`Event`]), plus the value types they
//! carry.
//!
//! Everything Settings sees is defined here, so no protocol type leaks out of
//! this module tree — and nothing here names a protocol either.

use std::sync::mpsc::Sender;

/// Events flowing from a probe thread up to the app.
///
/// `SessionOptions` carries several catalogues at once; boxing it would only
/// complicate every probe match for events that cross the channel a handful of
/// times per settings open.
#[allow(clippy::large_enum_variant)]
pub enum Event {
    /// The options an agent exposes for a session — models, modes and sandbox
    /// policy — each with the value the agent reports as active.
    ///
    /// Agents differ in which axes they have: modes are an ACP config option,
    /// the sandbox is Codex's own setting, and reasoning is an ACP config
    /// option that Codex only has per model. An empty list is therefore the
    /// normal way to say "this agent has no such axis".
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
    /// what keeps the menu off the chat input there rather than offering commands
    /// that could not run. Codex's app-server has no slash commands of its own
    /// (they live in its TUI), so its backend reports the few built-ins it runs
    /// itself, then the session's skills.
    Commands {
        agent: &'static str,
        commands: Vec<CommandInfo>,
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

/// One slash command an agent says it will run, as reported by its own
/// catalogue.
///
/// Not an axis: a command has no value "in force", and the set may change
/// mid-session (ACP re-sends it whenever it likes). It is also not something
/// kumu resolves — the agent recognizes the `/name` prefix in a prompt, so
/// invoking one is just sending it.
///
/// The description an agent offers is deliberately not carried: the lists show
/// the same source's long prose nowhere else (a catalogue row is the
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

/// Send an event to the UI, echoing to stderr when KUMU_DEBUG is set (handy
/// for headless verification). The prefix is the layer, not a protocol: every
/// backend reports through here.
pub(super) fn emit(tx: &Sender<Event>, ev: Event) {
    if std::env::var("KUMU_DEBUG").is_ok() {
        match &ev {
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
                    .map(|m| format!("{}({})", m.name, m.value))
                    .collect::<Vec<_>>()
                    .join(", "),
                access
                    .iter()
                    .map(|a| format!("{}({})", a.name, a.value))
                    .collect::<Vec<_>>()
                    .join(", "),
                sandboxes
                    .iter()
                    .map(|s| format!("{}({})", s.name, s.value))
                    .collect::<Vec<_>>()
                    .join(", "),
                reasoning
                    .iter()
                    .map(|r| format!("{}({})", r.name, r.value))
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
        }
    }
    let _ = tx.send(ev);
}
