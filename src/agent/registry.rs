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

pub const AGENTS: [AgentDef; 7] = [
    AgentDef {
        id: "opencode",
        name: "Opencode",
        desc: "开源终端编码 Agent",
        logo: Some("opencode.png"),
    },
    AgentDef {
        id: "omp",
        name: "Oh My Pi",
        desc: "个人 AI 终端编码 Agent",
        logo: Some("omp.png"),
    },
    AgentDef {
        id: "codex",
        name: "Codex",
        desc: "OpenAI 的终端编码 Agent",
        logo: Some("codex.svg"),
    },
    AgentDef {
        id: "claude",
        name: "Claude Code",
        desc: "Anthropic 的终端编码 Agent",
        logo: Some("claude.png"),
    },
    AgentDef {
        id: "pi",
        name: "Pi",
        desc: "轻量终端编码 Agent",
        logo: Some("pi.png"),
    },
    AgentDef {
        id: "cursor",
        name: "Cursor CLI",
        desc: "Cursor 的终端编码 Agent",
        logo: Some("cursor.svg"),
    },
    AgentDef {
        id: "grok",
        name: "Grok Build",
        desc: "xAI 的终端编码 Agent",
        logo: Some("grok.svg"),
    },
];
