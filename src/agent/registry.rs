//! Static registry of agents kumu can drive. Which protocol each one speaks is
//! decided in `config`, keyed by the `id` here.

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

pub const AGENTS: [AgentDef; 5] = [
    AgentDef {
        id: "opencode",
        name: "Opencode",
        desc: "opencode serve · v2 API",
        logo: Some("opencode.png"),
    },
    AgentDef {
        id: "omp",
        name: "Oh My Pi",
        desc: "omp · ACP v1",
        logo: Some("omp.png"),
    },
    AgentDef {
        id: "codex",
        name: "Codex",
        desc: "codex app-server",
        logo: Some("codex.svg"),
    },
    AgentDef {
        id: "claude",
        name: "Claude Code",
        desc: "claude -p · stream-json",
        logo: Some("claude.png"),
    },
    AgentDef {
        id: "pi",
        name: "Pi",
        desc: "pi --mode rpc",
        logo: Some("pi.png"),
    },
];
