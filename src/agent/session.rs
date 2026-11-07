//! Shared shapes every protocol probe fills in the same way.
//!
//! The live session loops are gone; what remains is the options event builder
//! probes use so Settings can show model catalogues without knowing which
//! agent produced them.

use super::event::{AccessInfo, Event, ModeInfo, ModelInfo, ReasoningInfo, SandboxInfo};

/// The stop reason that marks a turn the user cancelled.
///
/// Still used by protocol translators that map agent-specific abort words onto
/// one spelling for Settings probes and leftover convert helpers.
pub(super) const CANCELLED: &str = "cancelled";

/// Assembles [`Event::SessionOptions`], whose fields are the same for every
/// agent; only the source of each list differs.
///
/// Every axis starts empty, which is the established way to say "this agent has
/// no such axis". A probe fills in only the axes it actually has.
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
