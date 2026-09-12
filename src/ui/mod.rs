//! Frontend UI: one module per window, shared widgets in `widgets`.

mod home;
mod layers;
mod settings;
pub mod widgets;

use std::collections::HashMap;

use gpui_kit::Global;

use crate::backend;

pub use home::KumuApp;
pub use settings::SettingsWindow;

/// An agent's session options: what it lets the user pick, each with the value
/// the agent reports as active (used as the default label before the user picks
/// one).
///
/// Which lists are empty depends on the agent — ACP agents have modes and a
/// reasoning level, Codex has a sandbox policy — so an empty list means "no such
/// axis here", and the matching picker renders nothing.
#[derive(Clone, Default)]
pub struct SessionOptions {
    pub models: Vec<backend::ModelInfo>,
    pub current_model: Option<String>,
    pub modes: Vec<backend::ModeInfo>,
    pub current_mode: Option<String>,
    pub sandboxes: Vec<backend::SandboxInfo>,
    pub current_sandbox: Option<String>,
    pub reasoning: Vec<backend::ReasoningInfo>,
    pub current_reasoning: Option<String>,
}

/// App-wide cache of each agent's session options, discovered by probing and by
/// live sessions. Survives window reopens.
#[derive(Default)]
pub struct AgentSessionOptions(pub HashMap<&'static str, SessionOptions>);

impl Global for AgentSessionOptions {}
