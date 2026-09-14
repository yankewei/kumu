//! Session option discovery for the Pi backend.
//!
//! Pi answers a catalogue over the protocol, so unlike Claude Code's probe there
//! is something real to read: `get_available_models` returns every model this
//! machine is configured for, with the provider each belongs to. That is the
//! whole reason this opens a sessionless RPC process instead of running
//! `--version` — Pi's menu is per-user and dynamic, so a fixed vocabulary would
//! describe a machine kumu is not running on.
//!
//! What a probe reports is what a fresh session would start with: Pi resolves a
//! default model and thinking level before any turn, and both are visible in
//! `get_state` even when nothing is persisted.

use std::sync::mpsc::Sender;

use serde_json::json;

use crate::backend::config::Config;
use crate::backend::event::{Event, emit};
use crate::backend::probe::run;
use crate::backend::session::SessionOptionsBuilder;

use super::convert;
use super::transport;

/// The argv a probe runs with.
///
/// `--no-session` is the one that matters: without it, probing would create a
/// session file for every check, littering the user's Pi history with throwaway
/// sessions — the same hazard the Codex probe avoids by never calling
/// `thread/start`.
///
/// Skills and prompt templates are deliberately left enabled, unlike the
/// context files: they *are* part of what this probe is asked for, since Pi
/// reports them as invocable commands (`get_commands`) and a menu that omitted
/// them would offer less than a session does. Loading them costs a directory
/// read, not a model call. Context files stay off — nothing here reads them and
/// no turn will run for their sake.
///
/// `--approve` is not about this probe's own safety — it runs in a temporary
/// directory and never executes a tool. It is there because an untrusted
/// directory can make Pi ask before it starts, and in RPC mode that question has
/// nobody to answer it: the probe would sit in its handshake until the timeout.
///
/// Every flag was checked against `pi --help` (0.85.1). Pi rejects an unknown
/// flag outright rather than ignoring it, so this list cannot be padded with
/// plausible-sounding ones.
fn probe_args() -> Vec<String> {
    [
        "--mode",
        "rpc",
        "--approve",
        "--no-session",
        "--no-context-files",
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect()
}

/// Probe a local `pi` far enough to read its catalogue, then report it.
pub fn probe(agent: &'static str, cfg: Config, ev_tx: Sender<Event>) {
    run(agent, "pi", ev_tx, move |ev_tx| async move {
        // The receiver is held (not dropped) for the whole probe: when it goes
        // away the reader stops, and every later request would then time out
        // waiting for a reply nobody is reading.
        let (transport, _frames, _child) =
            transport::spawn(&cfg.command, &probe_args(), &cfg.env, &std::env::temp_dir())
                .map_err(|error| format!("{}: {error}", cfg.command))?;
        let state = transport.request(json!({"type": "get_state"})).await?;
        let models = transport
            .request(json!({"type": "get_available_models"}))
            .await?;
        let levels = transport
            .request(json!({"type": "get_available_thinking_levels"}))
            .await?;
        let commands = transport.request(json!({"type": "get_commands"})).await?;
        let data = state
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        // No mode, access or sandbox axis: Pi reports neither of the two, and
        // has no permission system to expose an access policy from.
        let options = SessionOptionsBuilder::new(agent)
            .models(
                convert::model_catalog(models.get("data").unwrap_or(&serde_json::Value::Null)),
                // A sessionless run still resolves a model and a thinking level,
                // so this is what a fresh session would start with rather than a
                // guess.
                convert::current_model(&data),
            )
            .reasoning(
                convert::reasoning_catalog(levels.get("data").unwrap_or(&serde_json::Value::Null)),
                convert::current_reasoning(&data),
            )
            .build();
        emit(&ev_tx, options);
        // The commands this machine's Pi offers, in Pi's own form: what a session
        // here would accept as `/name`.
        emit(
            &ev_tx,
            Event::Commands {
                agent,
                commands: convert::command_catalog(
                    commands.get("data").unwrap_or(&serde_json::Value::Null),
                ),
            },
        );
        // Nothing is kept alive: the child is dropped here, and that kills it.
        Ok::<(), String>(())
    });
}

#[cfg(test)]
mod tests {
    use super::probe;
    use crate::backend::test_support::{AGENT_TEST_LOCK, command_exists};
    use crate::backend::{Config, Event};

    /// End-to-end: probe a real `pi` and expect the catalogue it reports.
    /// Skipped when the binary is absent, so the suite still passes on machines
    /// without a local agent.
    #[test]
    fn probe_reports_pi_options() {
        let _guard = AGENT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = Config::for_agent("pi").expect("pi is a registered agent");
        if !command_exists(&cfg.command) {
            println!(
                "skipping probe_reports_pi_options: {} not found",
                cfg.command
            );
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        probe("pi", cfg, tx);
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
                    current_reasoning,
                } => {
                    println!(
                        "{agent}: {} models across {} providers (active: {current_model:?}), \
                         thinking {:?} (active: {current_reasoning:?})",
                        models.len(),
                        models
                            .iter()
                            .map(|model| model.provider.clone())
                            .collect::<std::collections::HashSet<_>>()
                            .len(),
                        reasoning
                            .iter()
                            .map(|level| level.name.as_str())
                            .collect::<Vec<_>>(),
                    );
                    assert!(!models.is_empty(), "pi reported no models");
                    assert!(
                        !reasoning.is_empty(),
                        "pi reports the thinking levels of the model in force"
                    );
                    assert!(
                        models
                            .iter()
                            .all(|model| model.value.contains('/') && !model.provider.is_empty()),
                        "a model value is the provider/model pair set_model takes"
                    );
                    assert!(
                        current_model
                            .as_deref()
                            .is_some_and(|model| model.contains('/')),
                        "the model in force is reported in the same form: {current_model:?}"
                    );
                    assert!(
                        modes.is_empty() && access.is_empty() && sandboxes.is_empty(),
                        "pi exposes none of these axes: {modes:?} {access:?} {sandboxes:?}"
                    );
                    assert!(current_mode.is_none() && current_access.is_none());
                    assert!(current_sandbox.is_none());
                    break;
                }
                Event::ProbeFailed { agent } => panic!("probe failed: {agent}"),
                _ => {}
            }
        }
        // The catalogue follows the axes, and it is what a fresh session's `/`
        // menu offers: the extension commands, prompt templates and skills this
        // machine has, since the probe no longer trims them.
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx
                .recv_timeout(remaining)
                .expect("the probe ended without a command catalogue")
            {
                Event::Commands { commands, .. } => {
                    println!(
                        "pi: {} commands ({})",
                        commands.len(),
                        commands
                            .iter()
                            .map(|command| command.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    assert!(!commands.is_empty(), "pi reported no commands");
                    break;
                }
                Event::ProbeFailed { agent } => panic!("probe failed: {agent}"),
                _ => {}
            }
        }
    }

    /// A probe against a missing binary must surface `ProbeFailed` rather than
    /// failing silently, which is what tells the settings window the difference
    /// between "installed" and "not installed".
    #[test]
    fn probe_missing_agent_reports_failure() {
        let (tx, rx) = std::sync::mpsc::channel();
        probe(
            "missing",
            Config {
                agent: "missing",
                backend: crate::backend::config::Backend::Pi,
                command: "/nonexistent/kumu-pi-test".into(),
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
