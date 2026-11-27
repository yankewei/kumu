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

pub const AGENTS: [AgentDef; 8] = [
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
    AgentDef {
        id: "agy",
        name: "Antigravity CLI",
        desc: "Google 的终端编码 Agent",
        logo: Some("agy.png"),
    },
];

/// The registry id behind a name an agent reported about itself.
///
/// Hooks pass the lowercase id (`--kumu-notify codex`), in-process plugins use
/// the display name ("OpenCode"), and OSC 7501 uses stable app identifiers
/// (`claude-code`, `antigravity`, `cursor-agent`).
pub fn id_for(name: &str) -> Option<&'static str> {
    let trimmed = name.trim();
    if let Some(agent) = AGENTS.iter().find(|agent| {
        agent.id.eq_ignore_ascii_case(trimmed) || agent.name.eq_ignore_ascii_case(trimmed)
    }) {
        return Some(agent.id);
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "claude-code" | "claude_code" => Some("claude"),
        "antigravity" | "antigravity-cli" | "antigravity_cli" => Some("agy"),
        "cursor-agent" | "cursor_agent" | "cursor-cli" | "cursor_cli" => Some("cursor"),
        "oh-my-pi" | "oh_my_pi" => Some("omp"),
        "grok-build" | "grok_build" => Some("grok"),
        "pi-coding-agent" => Some("pi"),
        _ => None,
    }
}

/// Whether Kumu's native in-process extension for `id` should remain
/// authoritative over OSC 7501 when both are active on the same pane.
///
/// OpenCode, Pi, and Oh My Pi ship full lifecycle extensions in Kumu; external
/// hook-based CLIs (Codex, Claude Code, Antigravity CLI, Grok) prefer OSC 7501.
pub fn prefers_integration_status(id: &str) -> bool {
    matches!(id, "opencode" | "pi" | "omp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_for_resolves_osc7501_app_aliases_and_rejects_non_agents() {
        assert_eq!(id_for("codex"), Some("codex"));
        assert_eq!(id_for("antigravity"), Some("agy"));
        assert_eq!(id_for("antigravity-cli"), Some("agy"));
        assert_eq!(id_for("claude-code"), Some("claude"));
        assert_eq!(id_for("cursor-agent"), Some("cursor"));
        assert_eq!(id_for("oh-my-pi"), Some("omp"));
        assert_eq!(id_for("grok-build"), Some("grok"));
        assert_eq!(id_for("pi-coding-agent"), Some("pi"));
        assert_eq!(id_for("cargo"), None);
        assert_eq!(id_for("terraform"), None);

        assert!(prefers_integration_status("opencode"));
        assert!(prefers_integration_status("pi"));
        assert!(prefers_integration_status("omp"));
        assert!(!prefers_integration_status("codex"));
        assert!(!prefers_integration_status("claude"));
        assert!(!prefers_integration_status("agy"));
    }
}
