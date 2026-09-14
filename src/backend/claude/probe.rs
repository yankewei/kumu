//! Session option discovery for the Claude Code backend.
//!
//! Unlike the ACP and Codex probes there is no handshake and no catalog to
//! read, so this is deliberately the smallest thing that can still answer the
//! question the settings window asks — plus, since the CLI reports no effort in
//! force and starts no session here, a read of Claude Code's own configuration
//! for what a fresh session would start with (`permissions.defaultMode` and
//! `CLAUDE_CODE_EFFORT_LEVEL`). kumu only reads it: with no pick nothing is ever
//! sent.
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

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use serde_json::Value;

use crate::backend::config::Config;
use crate::backend::event::{Event, emit};
use crate::backend::probe::run;
use crate::backend::session::SessionOptionsBuilder;

use super::convert;

/// Claude Code's own starting defaults, read from its configuration files.
///
/// A probe starts no session, so there is nothing "in force" to report — but two
/// of the composer's chips can still say something concrete instead of "your
/// configuration decides", because the configuration is right there: the
/// permission mode a fresh session starts in, and the effort this machine pins.
/// kumu only *reads* it: with no pick nothing is ever sent, so displaying the
/// agent's own choice is not the same as overriding it.
///
/// Managed settings win over the user's, and only those two are consulted: this
/// result is cached per agent, so it must not depend on a session's working
/// directory — project and local settings are that session's business, and its
/// own report corrects them once it connects.
fn configured_mode_and_effort() -> (String, Option<String>) {
    let paths = settings_paths();
    (permission_mode_from_paths(&paths), effort_from(&paths))
}

/// The files a fresh session's settings come from, most authoritative first.
fn settings_paths() -> Vec<PathBuf> {
    vec![managed_settings_path(), config_dir().join("settings.json")]
}

/// Where this build keeps managed settings. The directories are Claude Code's
/// own, quoted from its 2.1.269 binary.
fn managed_settings_path() -> PathBuf {
    let dir = if cfg!(target_os = "macos") {
        "/Library/Application Support/ClaudeCode"
    } else if cfg!(target_os = "windows") {
        r"C:\Program Files\ClaudeCode"
    } else {
        "/etc/claude-code"
    };
    PathBuf::from(dir).join("managed-settings.json")
}

/// Claude Code's configuration directory, resolved the way the CLI itself does.
fn config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
        .unwrap_or_default()
}

fn read_settings(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The mode `-p` starts in when nothing configures one: Manual, which Claude
/// Code reports as `default` and kumu labels 监督.
const DEFAULT_PERMISSION_MODE: &str = "default";

/// The `permissions.defaultMode` a fresh session would start in, from the first
/// file that names one.
fn permission_mode_from_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .find_map(|path| read_settings(path).and_then(|settings| default_mode(&settings)))
        .unwrap_or_else(|| DEFAULT_PERMISSION_MODE.to_string())
}

fn default_mode(settings: &Value) -> Option<String> {
    let value = settings.get("permissions")?.get("defaultMode")?.as_str()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The effort this machine pins, if any.
///
/// Claude Code reports no effort in force — not on the handshake, not on
/// `system/init` — but `CLAUDE_CODE_EFFORT_LEVEL` overrides whatever a session
/// asks for (the CLI says so itself), and a settings `env` block can set it, so
/// it is the one thing that can be known before a turn. A GUI launch inherits no
/// shell environment, which is why the settings block is consulted too.
fn effort_from(paths: &[PathBuf]) -> Option<String> {
    let from_environment = std::env::var("CLAUDE_CODE_EFFORT_LEVEL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    from_environment.or_else(|| {
        paths.iter().find_map(|path| {
            let settings = read_settings(path)?;
            let value = settings
                .get("env")?
                .get("CLAUDE_CODE_EFFORT_LEVEL")?
                .as_str()?;
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_string())
        })
    })
}

/// Probe a local `claude` just far enough to know it runs, then report the
/// fixed session vocabulary.
pub fn probe(agent: &'static str, cfg: Config, ev_tx: Sender<Event>) {
    // Read before the child runs: this is the same configuration the CLI will
    // read, and the same environment it will inherit.
    let (mode, access, effort) = {
        let (mode, effort) = configured_mode_and_effort();
        let access = convert::current_access(Some(&mode));
        (mode, access, effort)
    };
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
            // effort it pins: read from Claude Code's own settings so the chips
            // say something concrete before the first message.
            .modes(convert::mode_options(), Some(mode.clone()))
            .access(convert::access_options(), access.clone())
            .reasoning(convert::reasoning_options(), effort.clone())
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

    use super::*;

    fn settings_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kumu-claude-probe-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The mode comes from the first file that names one — managed settings
    /// before the user's — and a file that names none changes nothing.
    #[test]
    fn the_starting_mode_comes_from_the_most_authoritative_file() {
        let dir = settings_dir("mode");
        let managed = dir.join("managed.json");
        let user = dir.join("user.json");
        std::fs::write(
            &managed,
            r#"{"permissions": {"defaultMode": "acceptEdits"}}"#,
        )
        .unwrap();
        std::fs::write(&user, r#"{"permissions": {"defaultMode": "plan"}}"#).unwrap();

        assert_eq!(
            permission_mode_from_paths(&[managed.clone(), user.clone()]),
            "acceptEdits",
            "managed settings win"
        );
        assert_eq!(
            permission_mode_from_paths(&[dir.join("absent.json"), user]),
            "plan",
            "a missing file is skipped"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// A machine that configures nothing starts in Manual, which Claude Code
    /// reports as `default`.
    #[test]
    fn an_unconfigured_machine_starts_in_the_documented_mode() {
        let dir = settings_dir("unset");
        assert_eq!(
            permission_mode_from_paths(&[dir.join("absent.json")]),
            DEFAULT_PERMISSION_MODE
        );
        assert_eq!(permission_mode_from_paths(&[]), DEFAULT_PERMISSION_MODE);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Only a non-empty string counts, and the effort can come from a settings
    /// `env` block when the app inherited no shell environment.
    #[test]
    fn settings_are_read_defensively() {
        assert_eq!(
            default_mode(&serde_json::json!({"permissions": {"defaultMode": "  "}})),
            None
        );
        assert_eq!(default_mode(&serde_json::json!({"permissions": {}})), None);
        assert_eq!(default_mode(&serde_json::json!({})), None);

        let dir = settings_dir("env");
        let user = dir.join("user.json");
        std::fs::write(
            &user,
            r#"{"env": {"CLAUDE_CODE_EFFORT_LEVEL": "high", "OTHER": "x"}}"#,
        )
        .unwrap();
        let from_settings = effort_from(&[user]);
        // The process environment wins when it sets one, which is what the CLI
        // itself honours.
        assert_eq!(
            from_settings.as_deref(),
            std::env::var("CLAUDE_CODE_EFFORT_LEVEL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .as_deref()
                .or(Some("high"))
        );
        std::fs::remove_dir_all(dir).ok();
    }

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
                    // The effort this machine pins, if any — shown in the log
                    // below rather than asserted, since it comes from the
                    // environment or a settings `env` block.
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
