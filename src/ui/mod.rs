//! Frontend UI: one module per window, shared widgets in `widgets`.

mod home;
mod layers;
mod settings;
pub mod widgets;

use std::collections::HashMap;

use gpui_kit::Global;

use crate::acp;

pub use home::KumuApp;
pub use settings::SettingsWindow;

/// An agent's session options: the selectable models and modes, each with the
/// value the agent reports as active (used as the default label before the
/// user picks one).
#[derive(Clone, Default)]
pub struct SessionOptions {
    pub models: Vec<acp::ModelInfo>,
    pub current_model: Option<String>,
    pub modes: Vec<acp::ModeInfo>,
    pub current_mode: Option<String>,
}

/// App-wide cache of each agent's session options, discovered over ACP
/// (settings probes; live sessions report too). Survives window reopens.
#[derive(Default)]
pub struct AgentSessionOptions(pub HashMap<&'static str, SessionOptions>);

impl Global for AgentSessionOptions {}
