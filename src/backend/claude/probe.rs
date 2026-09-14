//! Session option discovery for the Claude Code backend.
//!
//! Unlike the ACP and Codex probes there is no handshake and no catalog to
//! read, so this is deliberately the smallest thing that can still answer the
//! question the settings window asks — plus, since the CLI reports neither the
//! effort in force nor the mode a fresh session would start in, a read of Claude
//! Code's own configuration for both ([`settings`]). kumu only reads it: with no
//! pick nothing is ever sent.
//!
//! Why there is nothing to discover: Claude Code's `system/init` event, which
//! is the only place it describes a session, carries no list of anything. Its
//! `model` is a single already-resolved string and its `permissionMode` is a
//! single value; there is no model-list request that a catalog could come from
//! (the way Codex's `model/list` supplies one), and `--effort` levels are
//! documented rather than advertised. Checked against **claude 2.1.251**.
//! What is left is a fixed vocabulary, which `convert` owns, plus a liveness
//! check — and the liveness check is worth doing, because it is what tells the
//! settings window the difference between "this agent is installed" and "this
//! agent is not".
//!
//! Deliberately not "probe a session": `system/init` only arrives once a real
//! user message has been sent, and a message both costs a model round trip and
//! writes a transcript under the user's `~/.claude/projects/`. A probe that
//! did that would litter the user's Claude history — the same hazard the Codex
//! probe avoids by never calling `thread/start` — so it is not done here.

use std::sync::mpsc::Sender;

use crate::backend::config::Config;
use crate::backend::event::{Event, emit};
use crate::backend::probe::run;
use crate::backend::session::SessionOptionsBuilder;

use super::{convert, settings};

/// Probe a local `claude` just far enough to know it runs, then report the
/// fixed session vocabulary.
pub fn probe(agent: &'static str, cfg: Config, ev_tx: Sender<Event>) {
    // Read before the child runs: this is the same configuration the CLI will
    // read, and the same environment it will inherit.
    let mode = settings::permission_mode();
    let access = convert::current_access(Some(&mode));
    // No session is started, so no model is known: this machine's configuration
    // answers when it names a level, and the model table's own default
    // otherwise — the same answer a session gets before it reports a model.
    let effort = settings::effort_in_force(None)
        .unwrap_or_else(|| convert::default_effort(None).to_string());
    run(agent, "claude", ev_tx, move |ev_tx| async move {
        // `--version` starts no session and reads no configuration, so this is
        // free and leaves nothing behind.
        let output = tokio::process::Command::new(&cfg.command)
            .arg("--version")
            .envs(cfg.env.iter().map(|(k, v)| (k.clone(), v.clone())))
            .output()
            .await
            .map_err(|err| format!("{}: {err}", cfg.command))?;
        if !output.status.success() {
            return Err(format!(
                "{} --version: exited with {}",
                cfg.command, output.status
            ));
        }
        // No sandbox axis: Claude Code does have an OS-level boundary (Seatbelt
        // on macOS, bubblewrap on Linux/WSL2), but it is a switch plus
        // filesystem/network rule lists in Claude Code's own settings with no
        // per-session form — every plausible control subtype is rejected (see
        // `schema/claude-stream-json/2.1.269/README.md`). What carries "how much
        // may it do on its own" is the access policy.
        let options = SessionOptionsBuilder::new(agent)
            // No session exists, so no model is in force yet.
            .models(convert::model_options(), None)
            // A session starts in the mode this machine configures and at the
            // effort it pins or saves: read from Claude Code's own settings so
            // the chips say something concrete before the first message.
            .modes(convert::mode_options(), Some(mode.clone()))
            .access(convert::access_options(), access.clone())
            .reasoning(convert::reasoning_options(), Some(effort))
            .build();
        emit(&ev_tx, options);
        Ok::<(), String>(())
    });
}

#[cfg(test)]
mod tests {
    use crate::backend::test_support::{AGENT_TEST_LOCK, command_exists};
    use crate::backend::{Config, Event};

    use super::probe;

    /// End-to-end: probe a real `claude` and expect the fixed vocabulary.
    /// Skipped when the binary is absent, so the suite still passes on machines
    /// without a local agent.
    #[test]
    fn probe_reports_claude_options() {
        let _guard = AGENT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = Config::for_agent("claude").expect("claude is a registered agent");
        if !command_exists(&cfg.command) {
            println!(
                "skipping probe_reports_claude_options: {} not found",
                cfg.command
            );
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        probe("claude", cfg, tx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining).expect("probe timed out") {
                Event::SessionOptions {
                    agent,
                    models,
                    current_model,
                    modes,
                    current_mode,
                    access,
                    current_access,
                    sandboxes,
                    current_sandbox,
                    reasoning,
                    // The effort this machine saves or pins, if any — shown in
                    // the log below rather than asserted, since it comes from
                    // the environment or a settings file.
                    current_reasoning,
                } => {
                    println!(
                        "{agent}: {} models, {} modes, reasoning {:?}; a fresh session starts at \
                         mode {current_mode:?} (access {current_access:?}, effort \
                         {current_reasoning:?}) — read from this machine's Claude Code settings",
                        models.len(),
                        modes.len(),
                        reasoning
                            .iter()
                            .map(|effort| effort.name.as_str())
                            .collect::<Vec<_>>(),
                    );
                    assert!(!models.is_empty(), "claude should offer its aliases");
                    assert!(!modes.is_empty(), "claude should offer permission modes");
                    assert!(!reasoning.is_empty(), "claude should offer effort levels");
                    assert!(
                        sandboxes.is_empty(),
                        "kumu offers no sandbox axis for claude: {sandboxes:?}"
                    );
                    // No session exists, so no model is in force — but the mode
                    // and the effort are what this machine configures a fresh
                    // session to start with, read from Claude Code's own files.
                    assert!(current_model.is_none());
                    assert!(
                        current_mode.is_some(),
                        "a probe reports the mode a session would start in"
                    );
                    assert_eq!(
                        current_access.is_none(),
                        current_mode.as_deref() == Some("plan"),
                        "plan mode is the one case that leaves no policy in force"
                    );
                    assert!(!access.is_empty(), "claude should offer access modes");
                    assert!(current_sandbox.is_none());
                    break;
                }
                Event::ProbeFailed { agent } => panic!("probe failed: {agent}"),
                _ => {}
            }
        }
    }

    /// A probe against a missing binary must surface `ProbeFailed` rather than
    /// failing silently.
    #[test]
    fn probe_missing_agent_reports_failure() {
        let (tx, rx) = std::sync::mpsc::channel();
        probe(
            "missing",
            Config {
                agent: "missing",
                backend: crate::backend::config::Backend::Claude,
                command: "/nonexistent/kumu-claude-test".into(),
                args: vec![],
                env: vec![],
                auto_prompt: None,
                session: Default::default(),
            },
            tx,
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx
                .recv_timeout(remaining)
                .expect("probe ended without ProbeFailed")
            {
                Event::SessionOptions { .. } => panic!("unexpected probe success"),
                Event::ProbeFailed { agent } => {
                    assert_eq!(agent, "missing");
                    break;
                }
                _ => {}
            }
        }
    }
}
