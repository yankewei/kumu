//! Agent status and title signals carried over the terminal's OSC 777 channel.
//!
//! They travel as notification-shaped OSCs so every hook and native extension
//! reuses the same transport:
//! - `\x1b]777;notify;<agent>;kumu-status:<running|waiting|completed|failed|resume|idle>\x07`
//! - `\x1b]777;notify;<agent>;kumu-title:<session title>\x07`
//! - `\x1b]777;notify;<agent>;kumu-title:prompt:<first prompt>\x07`

/// Marks a notification-shaped OSC as an agent status update rather than a
/// user-facing alert.
pub const STATUS_PREFIX: &str = "kumu-status:";

/// Marks a notification-shaped OSC as an agent session or task title update.
pub const TITLE_PREFIX: &str = "kumu-title:";

/// Sub-prefix under [`TITLE_PREFIX`] for a first-prompt fallback title (does
/// not overwrite an existing session or prompt title for the same conversation).
pub const TITLE_PROMPT_PREFIX: &str = "prompt:";

/// Maximum length kept for a reported or OSC task title.
const MAX_TITLE_CHARS: usize = 160;

/// The four active states an agent reports. An alive agent without one of these
/// is idle (shown in the sidebar list, hidden in the pane header and tab bar).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Running,
    Waiting,
    Completed,
    Failed,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "waiting" => Some(Self::Waiting),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    /// The order agents are listed in: waiting first, completed last (idle
    /// sits below completed at priority 4).
    pub fn priority(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Failed => 1,
            Self::Running => 2,
            Self::Completed => 3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "运行中",
            Self::Waiting => "等待输入",
            Self::Completed => "已完成",
            Self::Failed => "执行失败",
        }
    }
}

/// Wire-level status signal sent on `kumu-status:`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusSignal {
    Active(AgentState),
    /// Only moves a `Waiting` agent back to `Running` (e.g. after a permission
    /// approval or question answer), without disturbing other states.
    Resume,
    /// Returns the agent to the idle state (e.g. after user cancellation) while
    /// keeping the live agent in the sidebar list.
    Idle,
}

impl StatusSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active(state) => state.as_str(),
            Self::Resume => "resume",
            Self::Idle => "idle",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "resume" => Some(Self::Resume),
            "idle" => Some(Self::Idle),
            other => AgentState::parse(other).map(Self::Active),
        }
    }
}

/// Normalize a title reported via `kumu-title:` or a hook prompt payload.
///
/// Takes the first non-empty line, strips control characters and OSC field
/// delimiters (`;`), collapses whitespace, and optionally skips slash commands.
pub fn clean_reported_title(raw: &str, skip_slash: bool) -> Option<String> {
    let line = raw
        .lines()
        .map(|line| {
            let stripped: String = line
                .chars()
                .filter(|ch| !ch.is_control() && *ch != ';')
                .collect();
            stripped.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .find(|line| !line.is_empty())?;
    if skip_slash && line.starts_with('/') {
        return None;
    }
    Some(line.chars().take(MAX_TITLE_CHARS).collect())
}

/// Extract a usable session title from a terminal's OSC 0/2 window title,
/// discarding generic program names, paths, and leading spinner glyphs.
pub fn clean_osc_title(raw: &str, agent: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|ch| !ch.is_control()).collect();
    let trimmed = strip_leading_status_glyphs(cleaned.trim());
    let joined = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.is_empty() || joined == "." || joined.starts_with('/') || joined.starts_with('~') {
        return None;
    }
    if is_generic_title(&joined, agent) {
        return None;
    }
    Some(joined.chars().take(MAX_TITLE_CHARS).collect())
}

fn strip_leading_status_glyphs(mut text: &str) -> &str {
    loop {
        let mut chars = text.chars();
        let Some(first) = chars.next() else {
            return text;
        };
        let is_glyph = matches!(
            first,
            '\u{2800}'
                ..='\u{28FF}'
                    | '✳'
                    | '✽'
                    | '✻'
                    | '✶'
                    | '✢'
                    | '·'
                    | '•'
                    | '●'
                    | '○'
                    | '◆'
                    | '◇'
                    | '▶'
                    | '▸'
                    | '›'
                    | '»'
                    | '✓'
                    | '✔'
                    | '✗'
                    | '✕'
                    | '⏳'
                    | '⚡'
        );
        if is_glyph {
            text = chars.as_str().trim_start();
        } else {
            return text;
        }
    }
}

fn is_generic_title(title: &str, agent: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    if !agent.is_empty() && lower == agent.to_ascii_lowercase() {
        return true;
    }
    if super::AGENTS
        .iter()
        .any(|def| def.id.eq_ignore_ascii_case(title) || def.name.eq_ignore_ascii_case(title))
    {
        return true;
    }
    matches!(
        lower.as_str(),
        "zsh"
            | "bash"
            | "fish"
            | "sh"
            | "nu"
            | "node"
            | "bun"
            | "deno"
            | "python"
            | "python3"
            | "antigravity"
            | "terminal"
            | "终端"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_round_trip_and_keep_the_design_order() {
        for signal in [
            StatusSignal::Active(AgentState::Running),
            StatusSignal::Active(AgentState::Waiting),
            StatusSignal::Active(AgentState::Completed),
            StatusSignal::Active(AgentState::Failed),
            StatusSignal::Resume,
            StatusSignal::Idle,
        ] {
            assert_eq!(StatusSignal::parse(signal.as_str()), Some(signal));
        }
        assert_eq!(StatusSignal::parse("unknown"), None);
        let mut states = [
            AgentState::Completed,
            AgentState::Failed,
            AgentState::Running,
            AgentState::Waiting,
        ];
        states.sort_by_key(|state| state.priority());
        assert_eq!(
            states,
            [
                AgentState::Waiting,
                AgentState::Failed,
                AgentState::Running,
                AgentState::Completed,
            ]
        );
    }

    #[test]
    fn title_cleaners_strip_noise_and_ignore_generic_values() {
        assert_eq!(
            clean_reported_title("  \n  Fix sidebar   status;now  \nsecond line", true).as_deref(),
            Some("Fix sidebar statusnow")
        );
        assert_eq!(clean_reported_title("/model", true), None);
        assert_eq!(
            clean_osc_title("✳  Implement AgentStatus OSC", "claude").as_deref(),
            Some("Implement AgentStatus OSC")
        );
        assert_eq!(clean_osc_title("⠋ Claude Code", "claude"), None);
        assert_eq!(clean_osc_title("~/github/kumu", "codex"), None);
        assert_eq!(clean_osc_title("zsh", "claude"), None);
    }
}
