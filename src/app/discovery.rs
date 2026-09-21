//! What each agent offers — its session options and slash commands — as
//! discovered by probing it, and the bridge that carries agent events onto the
//! GPUI thread.

use std::collections::HashMap;

use futures::channel::mpsc::UnboundedReceiver;
use gpui_kit::Global;

use crate::agent;

/// An agent's session options: what it lets the user pick, each with the value
/// the agent reports as active (used as the default label before the user picks
/// one).
///
/// Which lists are empty depends on the agent — ACP agents have modes and a
/// reasoning level, Codex has a sandbox policy — so an empty list means "no such
/// axis here", and the matching picker renders nothing.
#[derive(Clone, Default)]
pub struct SessionOptions {
    pub models: Vec<agent::ModelInfo>,
    pub current_model: Option<String>,
    pub modes: Vec<agent::ModeInfo>,
    pub current_mode: Option<String>,
    pub access: Vec<agent::AccessInfo>,
    pub current_access: Option<String>,
    pub sandboxes: Vec<agent::SandboxInfo>,
    pub current_sandbox: Option<String>,
    pub reasoning: Vec<agent::ReasoningInfo>,
    pub current_reasoning: Option<String>,
}

impl SessionOptions {
    /// The options an [`agent::Event::SessionOptions`] reports, with the agent
    /// that reported them; `None` for any other event.
    pub fn from_event(ev: &agent::Event) -> Option<(&'static str, Self)> {
        let agent::Event::SessionOptions {
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
        } = ev
        else {
            return None;
        };
        Some((
            *agent,
            Self {
                models: models.clone(),
                current_model: current_model.clone(),
                modes: modes.clone(),
                current_mode: current_mode.clone(),
                access: access.clone(),
                current_access: current_access.clone(),
                sandboxes: sandboxes.clone(),
                current_sandbox: current_sandbox.clone(),
                reasoning: reasoning.clone(),
                current_reasoning: current_reasoning.clone(),
            },
        ))
    }

    /// Whether the report names any axis at all. The axes are independent — an
    /// agent may offer any subset of them — so an entirely empty report is not
    /// worth caching.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
            && self.modes.is_empty()
            && self.sandboxes.is_empty()
            && self.reasoning.is_empty()
    }
}

/// App-wide cache of each agent's session options, discovered by probing and by
/// live sessions. Survives window reopens.
#[derive(Default)]
pub struct AgentSessionOptions(pub HashMap<&'static str, SessionOptions>);

impl Global for AgentSessionOptions {}

/// App-wide cache of each agent's slash commands, as its probe discovered them:
/// what a session offers until its own connection reports the commands its
/// working directory adds.
///
/// Deliberately not a field of [`SessionOptions`]: those are axes with a value in
/// force, re-reported whole by several different events (a handshake, a config
/// option update, a Claude axis change), any of which would clobber a command
/// list cached beside them. A command list arrives on its own and is replaced
/// only by another one.
#[derive(Default)]
pub struct AgentCommands(pub HashMap<&'static str, Vec<agent::CommandInfo>>);

impl Global for AgentCommands {}

/// Carry agent events from the backend thread's std channel onto one the GPUI
/// thread can await: a plain forwarder thread in between, which ends when
/// either side hangs up.
pub fn bridge(events: std::sync::mpsc::Receiver<agent::Event>) -> UnboundedReceiver<agent::Event> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    std::thread::spawn(move || {
        for ev in events {
            if tx.unbounded_send(ev).is_err() {
                break;
            }
        }
    });
    rx
}

/// Probe `agents`, returning the stream their reports arrive on.
pub fn probe(agents: &[&'static str]) -> UnboundedReceiver<agent::Event> {
    let (ev_tx, ev_rx) = std::sync::mpsc::channel::<agent::Event>();
    for &id in agents {
        if let Some(cfg) = agent::Config::for_agent(id) {
            agent::probe(id, cfg, ev_tx.clone());
        }
    }
    bridge(ev_rx)
}
