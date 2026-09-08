//! Frontend UI: one module per window, shared widgets in `widgets`.

mod home;
mod settings;
pub mod widgets;

use std::collections::HashMap;

use gpui_kit::Global;

use crate::acp;

pub use home::KumuApp;
pub use settings::SettingsWindow;

/// An agent's model catalog: the selectable models plus the value the agent
/// reports as active on session/new (used as the default label).
#[derive(Clone, Default)]
pub struct ModelCatalog {
    pub models: Vec<acp::ModelInfo>,
    pub current: Option<String>,
}

/// App-wide cache of each agent's model catalog, discovered over ACP
/// (settings probes; live sessions report too). Survives window reopens.
#[derive(Default)]
pub struct AgentModels(pub HashMap<&'static str, ModelCatalog>);

impl Global for AgentModels {}
