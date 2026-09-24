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
    /// Whether kumu can drive a session with this agent yet. A backend may
    /// discover an agent's session options without having a session loop to
    /// apply them, so the composer keeps the session-scoped controls off an
    /// agent that cannot be driven instead of offering picks that could never
    /// take effect.
    pub drives_sessions: bool,
}

pub const AGENTS: [AgentDef; 5] = [
    AgentDef {
        id: "opencode",
        name: "Opencode",
        desc: "opencode serve · v2 API",
        logo: Some("opencode.png"),
        drives_sessions: true,
    },
    AgentDef {
        id: "omp",
        name: "Oh My Pi",
        desc: "omp · ACP v1",
        logo: Some("omp.png"),
        drives_sessions: true,
    },
    AgentDef {
        id: "codex",
        name: "Codex",
        desc: "codex app-server",
        logo: Some("codex.svg"),
        drives_sessions: true,
    },
    AgentDef {
        id: "claude",
        name: "Claude Code",
        desc: "claude -p · stream-json",
        logo: Some("claude.png"),
        drives_sessions: true,
    },
    AgentDef {
        id: "pi",
        name: "Pi",
        desc: "pi --mode rpc",
        logo: Some("pi.png"),
        drives_sessions: true,
    },
];
