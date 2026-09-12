//! Session option discovery for the Claude Code backend.
//!
//! Unlike the ACP and Codex probes there is no handshake and no catalog to
//! read, so this is deliberately the smallest thing that can still answer the
//! question the settings window asks.
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

use super::convert;

/// Probe a local `claude` just far enough to know it runs, then report the
/// fixed session vocabulary.
pub fn probe(agent: &'static str, cfg: Config, ev_tx: Sender<Event>) {
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
        emit(
            &ev_tx,
            Event::SessionOptions {
                agent,
                models: convert::model_options(),
                // No session exists, so nothing is in force: the pickers show
                // their inherited label rather than claiming one of these is
                // active.
                current_model: None,
                modes: convert::mode_options(),
                current_mode: None,
                // Claude Code has no sandbox policy axis.
                sandboxes: Vec::new(),
                current_sandbox: None,
                reasoning: convert::reasoning_options(),
                current_reasoning: None,
            },
        );
        Ok::<(), String>(())
    });
}

#[cfg(test)]
mod tests {
    use super::probe;
    use crate::backend::test_support::{AGENT_TEST_LOCK, command_exists};
    use crate::backend::{Config, Event};

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
                    sandboxes,
                    current_sandbox,
                    reasoning,
                    current_reasoning,
                } => {
                    println!(
                        "{agent}: {} models, {} modes, reasoning {:?}",
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
                        "claude exposes no sandbox policy axis"
                    );
                    // A probe starts no session, so nothing is in force.
                    assert!(current_model.is_none());
                    assert!(current_mode.is_none());
                    assert!(current_sandbox.is_none());
                    assert!(current_reasoning.is_none());
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
