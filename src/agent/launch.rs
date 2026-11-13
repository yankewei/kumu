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

/// The launch a pane should remember after `agent` reports it is running
/// `session` there.
///
/// A pane already running this agent takes the newer id. A shell pane is
/// adopted when `adopt_shell` *and* this CLI can resume the id: the picker is
/// not the only way to start an agent, and a session reported from a pane's
/// own PTY is the same evidence as one picked there. Adopting an agent with no
/// resume syntax would only replace the user's shell with a fresh agent.
///
/// `shell_pane` says the live process in the pane is a shell. An agent pane
/// ignores a different agent's report — that is a stale process. A shell pane
/// accepts it, because the report proves the pane now hosts that conversation
/// (the user switched agents).
pub fn session_launch(
    current: Option<&PaneLaunch>,
    agent: &str,
    session: &str,
    adopt_shell: bool,
    shell_pane: bool,
) -> Option<PaneLaunch> {
    match current {
        Some(PaneLaunch::Agent { agent: running, .. }) if running == agent => {}
        Some(PaneLaunch::Agent { .. }) if shell_pane => {}
        Some(PaneLaunch::Agent { .. }) => return None,
        Some(PaneLaunch::Shell) | None => {
            if !adopt_shell || resume_args(agent, Some(session)).is_empty() {
                return None;
            }
        }
    }
    Some(PaneLaunch::Agent {
        agent: agent.to_string(),
        session: Some(session.to_string()),
    })
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
    fn a_reported_session_updates_a_pane_and_adopts_resumable_shells() {
        let picked = agent("codex", Some("old"));
        assert_eq!(
            session_launch(Some(&picked), "codex", "new", true, false),
            Some(agent("codex", Some("new")))
        );
        // An agent started by hand in a shell pane becomes resumable.
        assert_eq!(
            session_launch(None, "opencode", "ses-1", true, true),
            Some(agent("opencode", Some("ses-1")))
        );
        assert_eq!(
            session_launch(Some(&PaneLaunch::Shell), "opencode", "ses-1", true, true),
            Some(agent("opencode", Some("ses-1")))
        );
        assert_eq!(
            session_launch(None, "opencode", "ses-1", false, true),
            None,
            "auto resume off leaves the shell alone"
        );
        assert_eq!(
            session_launch(None, "grok", "ses-1", true, true),
            None,
            "an agent with no resume syntax keeps its shell"
        );
        assert_eq!(
            session_launch(
                Some(&agent("grok", Some("old"))),
                "grok",
                "new",
                true,
                false
            ),
            Some(agent("grok", Some("new"))),
            "a picked pane still records an id its CLI cannot resume"
        );
    }

    #[test]
    fn only_a_shell_pane_may_switch_agents() {
        let picked = agent("opencode", Some("old"));
        assert_eq!(
            session_launch(Some(&picked), "claude", "new", true, false),
            None,
            "a stale process must not repurpose an agent pane"
        );
        assert_eq!(
            session_launch(Some(&picked), "claude", "new", true, true),
            Some(agent("claude", Some("new"))),
            "the same shell now runs another agent"
        );
        assert_eq!(
            session_launch(Some(&picked), "claude", "new", false, true),
            Some(agent("claude", Some("new"))),
            "switching agents does not need the resume setting"
        );
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
