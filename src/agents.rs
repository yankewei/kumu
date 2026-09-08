//! Static registry of agents kumu can drive. ACP wiring is the next step.

/// One drivable agent: id used in storage, name shown in the UI.
pub struct AgentDef {
    pub id: &'static str,
    pub name: &'static str,
    pub desc: &'static str,
    /// Logo asset served from the app's `assets/` folder. Monochrome svgs are
    /// tinted with the theme; other formats (e.g. png) keep their own colors.
    /// The generic Bot icon is used when `None`.
    pub logo: Option<&'static str>,
}

pub const AGENTS: [AgentDef; 2] = [
    AgentDef {
        id: "opencode",
        name: "Opencode",
        desc: "ACP v1",
        logo: Some("opencode.png"),
    },
    AgentDef {
        id: "omp",
        name: "Oh My Pi",
        desc: "omp · ACP v1",
        logo: Some("omp.png"),
    },
];
