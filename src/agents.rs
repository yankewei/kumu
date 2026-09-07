//! Static registry of agents kumu can drive. ACP wiring is the next step.

/// One drivable agent: id used in storage, name shown in the UI.
pub struct AgentDef {
    pub id: &'static str,
    pub name: &'static str,
    pub desc: &'static str,
}

pub const AGENTS: [AgentDef; 2] = [
    AgentDef {
        id: "opencode",
        name: "Opencode",
        desc: "SST Opencode · ACP v1",
    },
    AgentDef {
        id: "omp",
        name: "Oh My Pi",
        desc: "omp · ACP v1",
    },
];
