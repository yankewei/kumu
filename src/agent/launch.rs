//! What a pane runs, and how to bring the same conversation back.
//!
//! A pane is either an interactive shell or one agent's TUI. The agent's
//! conversation outlives the process, so a pane remembers the agent-side
//! session id and turns it into a resume command on the next launch. Only
//! syntax that has been checked against the CLI belongs here; an agent with no
//! known resume flag starts a fresh conversation instead of guessing.

use serde::{Deserialize, Serialize};

use super::executable;

/// Prefix that marks a notification-shaped OSC as an agent session update
/// rather than something to show the user. The payload is
/// `\x1b]777;notify;<agent>;kumu-session:<id>\x07`.
pub const SESSION_PREFIX: &str = "kumu-session:";

/// The launch choice saved with a pane.
///
/// `Shell` is the default and stays out of the saved JSON, so layouts written
/// before agent panes existed keep decoding and re-encoding unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PaneLaunch {
    #[default]
    Shell,
    Agent {
        agent: String,
        /// Agent-side conversation id. `None` starts a new one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
}

impl PaneLaunch {
    pub fn is_shell(&self) -> bool {
        matches!(self, Self::Shell)
    }

    /// The agent command this pane runs, or `None` for a shell pane — and for
    /// an agent that cannot be launched here (missing binary, unknown id).
    pub fn agent_command(&self) -> Option<AgentCommand> {
        let Self::Agent { agent, session } = self else {
            return None;
        };
        // An uninstalled agent must not take the pane down: the caller falls
        // back to a shell when there is nothing to run.
        if !super::installed(agent) {
            return None;
        }
        Some(AgentCommand {
            program: executable(agent)?,
            args: resume_args(agent, session.as_deref()),
        })
    }

    /// The same pane without a conversation to resume.
    pub fn fresh(self) -> Self {
        match self {
            Self::Agent { agent, .. } => Self::Agent {
                agent,
                session: None,
            },
            Self::Shell => Self::Shell,
        }
    }
}

pub struct AgentCommand {
    pub program: String,
    pub args: Vec<String>,
}

/// The arguments that start `agent`, resuming `session` when this CLI has a
/// checked resume syntax and the id is well formed.
fn resume_args(agent: &str, session: Option<&str>) -> Vec<String> {
    let Some(id) = session.filter(|id| well_formed(id)) else {
        return Vec::new();
    };
    match agent {
        // Verified against each CLI's `--help` on a machine that has it.
        // Agents missing here (pi, omp, grok) stay unimplemented rather than
        // receive a flag they may not accept.
        "codex" => vec!["resume".into(), id.into()],
        "opencode" => vec!["--session".into(), id.into()],
        "claude" => vec!["--resume".into(), id.into()],
        "cursor" => vec!["--resume".into(), id.into()],
        _ => Vec::new(),
    }
}

/// Session ids are agent-generated, but they end up inside a terminal escape
/// sequence, so only characters that cannot terminate or split it are kept.
pub fn clean_session(id: &str) -> Option<&str> {
    (!id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')))
    .then_some(id)
}

fn well_formed(id: &str) -> bool {
    clean_session(id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, session: Option<&str>) -> PaneLaunch {
        PaneLaunch::Agent {
            agent: name.into(),
            session: session.map(str::to_string),
        }
    }

    fn args(agent: &str, session: Option<&str>) -> Vec<String> {
        resume_args(agent, session)
    }

    #[test]
    fn a_new_conversation_passes_no_flags() {
        assert!(args("codex", None).is_empty());
        assert!(args("opencode", None).is_empty());
    }

    #[test]
    fn resume_syntax_is_per_cli() {
        assert_eq!(args("codex", Some("abc")), ["resume", "abc"]);
        assert_eq!(args("opencode", Some("abc")), ["--session", "abc"]);
        assert_eq!(args("claude", Some("abc")), ["--resume", "abc"]);
        assert_eq!(args("cursor", Some("abc")), ["--resume", "abc"]);
        assert!(args("grok", Some("abc")).is_empty());
        assert!(args("nope", Some("abc")).is_empty());
    }

    #[test]
    fn a_malformed_or_oversized_id_starts_fresh_instead_of_escaping() {
        assert!(args("codex", Some("a;b")).is_empty());
        assert!(args("codex", Some("a\x07b")).is_empty());
        assert!(args("codex", Some("")).is_empty());
        assert!(args("codex", Some(&"a".repeat(129))).is_empty());
        assert_eq!(args("codex", Some("a-b_c.d")), ["resume", "a-b_c.d"]);
    }

    #[test]
    fn shell_never_has_an_agent_command() {
        assert!(PaneLaunch::Shell.agent_command().is_none());
    }

    #[test]
    fn fresh_keeps_the_agent_and_drops_the_conversation() {
        assert_eq!(
            agent("codex", Some("abc")).fresh(),
            agent("codex", None),
            "the pane stays an agent pane"
        );
        assert_eq!(PaneLaunch::Shell.fresh(), PaneLaunch::Shell);
    }
}
