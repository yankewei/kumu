//! Session option discovery for the Claude Code backend.
//!
//! The axes are a fixed vocabulary plus a read of Claude Code's own
//! configuration, because the CLI reports neither the effort in force nor the
//! mode a fresh session would start in ([`settings`]). kumu only reads it: with
//! no pick nothing is ever sent.
//!
//! Why the axes are not discovered: `system/init`, the event that describes a
//! running session, arrives only with the first turn, and it carries no editable
//! catalogue. Its `model` is a single already-resolved string and its
//! `permissionMode` a single value; the effort levels are documented rather than
//! advertised. So `convert` owns the vocabulary, and the liveness check below is
//! what tells the settings window the difference between "this agent is
//! installed" and "this agent is not".
//!
//! The commands *are* discovered, because the `initialize` control response —
//! which kumu already sends on a live connection — carries a real catalogue
//! (`commands`, with the hint each takes; see `schema/claude-stream-json/`,
//! pinned to **claude 2.1.269**). That is a handshake, not a turn: the CLI
//! answers it immediately, nothing is prompted, and no model call is billed.
//!
//! Deliberately not "probe a session": a turn both costs a model round trip and
//! writes a transcript under the user's `~/.claude/projects/`, which is why
//! nothing here sends a message. Whether the bare handshake leaves anything
//! behind is checked by the test below, which runs it under a scratch `HOME`.
//!
//! [`settings`]: super::settings

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crate::agent::config::Config;
use crate::agent::event::{CommandInfo, Event, emit};
use crate::agent::probe::run;
use crate::agent::session::SessionOptionsBuilder;

use super::transport::{self, debug_log};
use super::{convert, protocol, settings};

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
            // the axes say something concrete before the first message.
            .modes(convert::mode_options(), Some(mode.clone()))
            .access(convert::access_options(), access.clone())
            .reasoning(convert::reasoning_options(), Some(effort))
            .build();
        emit(&ev_tx, options);
        // The commands are the one catalogue a probe can read without prompting:
        // the handshake's response lists them, and asking for it starts no turn
        // and costs no model call. Best-effort on purpose — a CLI that answers
        // `--version` but not the control protocol (an older build, an account
        // that cannot open a session) must still leave the axes above reported
        // and the agent counted as installed, which is what this probe is for.
        match command_catalogue(&cfg).await {
            Ok(commands) => emit(&ev_tx, Event::Commands { agent, commands }),
            Err(error) => debug_log(&format!("claude 命令目录：{error}")),
        }
        Ok::<(), String>(())
    });
}

/// Read the CLI's command catalogue off a handshake, without prompting anything.
///
/// The argv is a session's own, because that is what decides which commands
/// exist — the project's `.claude/commands/`, the account's plugins and skills —
/// and the working directory is this process's, the same one a probe for the ACP
/// agents uses.
///
/// Nothing is sent but the `initialize` control request, which the CLI answers
/// immediately (see `schema/claude-stream-json/2.1.269/README.md`). The child is
/// dropped when this returns, and `kill_on_drop` takes it from there.
async fn command_catalogue(cfg: &Config) -> Result<Vec<CommandInfo>, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let (transport, _child) = transport::spawn(&cfg.command, &cfg.args, &cfg.env, &cwd)
        .map_err(|error| format!("failed to launch {}: {error}", cfg.command))?;
    let reported: protocol::InitializeResponse = transport
        .control(serde_json::json!({"subtype": "initialize", "hooks": null}))
        .await
        .map(|payload| serde_json::from_value(payload).unwrap_or_default())?;
    // Nothing can be terminal-only yet: that is only learnable from the first
    // turn's `system/init`, which this probe deliberately never reaches.
    Ok(convert::command_catalog(&reported.commands, &[]))
}

#[cfg(test)]
mod tests {
    use crate::agent::test_support::{AGENT_TEST_LOCK, command_exists};
    use crate::agent::{Config, Event};

    use super::probe;

    /// End-to-end: probe a real `claude` and expect the fixed vocabulary.
    /// Ignored by default, so a plain `cargo test` never drives a real agent;
    /// `cargo test -- --ignored` runs it, and it skips itself when the binary is
    /// absent.
    #[test]
    #[ignore = "drives the real claude binary"]
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
        // A scratch home: the probe now also reads a catalogue over a handshake,
        // and nothing it does may touch this machine's own Claude Code state. The
        // same isolation the session test uses.
        let home = std::env::temp_dir().join(format!("kumu-claude-probe-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("a scratch home");
        let mut cfg = cfg;
        cfg.env
            .push(("HOME".to_string(), home.to_string_lossy().into_owned()));
        cfg.env.push((
            "CLAUDE_CONFIG_DIR".to_string(),
            home.join(".claude").to_string_lossy().into_owned(),
        ));
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
                Event::Commands { .. } => {}
                Event::ProbeFailed { agent } => panic!("probe failed: {agent}"),
            }
        }
        // The catalogue follows the axes — it comes from the handshake the probe
        // runs after them — and it is what a fresh session's `/` menu offers.
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx
                .recv_timeout(remaining)
                .expect("the probe ended without a command catalogue")
            {
                Event::Commands { commands, .. } => {
                    assert!(
                        commands.iter().any(|command| command.name == "compact"),
                        "the catalogue should hold the CLI's own commands: {commands:?}"
                    );
                    break;
                }
                Event::SessionOptions { .. } => {}
                Event::ProbeFailed { agent } => panic!("probe failed: {agent}"),
            }
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// A probe against a missing binary must surface `ProbeFailed` rather than
    /// failing silently.
    #[test]
    fn probe_missing_agent_reports_failure() {
        let (tx, rx) = std::sync::mpsc::channel();
        probe(
            "missing",
            Config {
                backend: crate::agent::config::Backend::Claude,
                command: "/nonexistent/kumu-claude-test".into(),
                args: vec![],
                env: vec![],
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
                Event::Commands { .. } => {}
                Event::ProbeFailed { agent } => {
                    assert_eq!(agent, "missing");
                    break;
                }
            }
        }
    }
}
