//! Model and skill discovery for the Codex backend.
//!
//! Only `model/list` and `skills/list` are asked for. A probe must **not** call
//! `thread/start`: that materialises a rollout on disk, so probing would litter
//! the user's Codex history with throwaway threads.

use std::sync::mpsc::Sender;

use serde_json::json;

use crate::agent::config::Config;
use crate::agent::event::{Event, emit};
use crate::agent::probe::run;
use crate::agent::session::SessionOptionsBuilder;

use super::protocol::{ModelListResult, SkillsListResult};
use super::transport::debug_log;
use super::{client_info, convert};

/// Probe a Codex agent just far enough to learn its models and skills:
/// initialize, ask `model/list` and `skills/list`, report, and return — the
/// dropped child kills the process.
pub fn probe(agent: &'static str, cfg: Config, ev_tx: Sender<Event>) {
    run(agent, "codex", ev_tx, move |ev_tx| async move {
        // The receiver is held (not dropped) for the whole probe: when it goes
        // away the reader stops and every later request would fail.
        let (transport, _messages, _child) =
            super::transport::spawn(&cfg.command, &cfg.args, &cfg.env)
                .map_err(|err| err.to_string())?;
        transport.request("initialize", client_info()).await?;
        transport.notify("initialized", json!({}));
        let listed = transport.request("model/list", json!({})).await?;
        let listed: ModelListResult =
            serde_json::from_value(listed).map_err(|err| err.to_string())?;
        let (current_model, models) = convert::model_catalog(&listed);
        let efforts = convert::model_reasoning(&listed);
        // A probe has no thread to echo an effort, so the effective one is the
        // selected model's own default.
        let reasoning = convert::reasoning_options(&efforts, current_model.as_deref());
        let current_reasoning = convert::default_effort(&efforts, current_model.as_deref());
        // Codex exposes no mode or access axis. The sandbox policies are a fixed
        // vocabulary, so they can be offered before any session exists; which
        // one is *in force* is only learnable from a live thread's own report,
        // so nothing is claimed here.
        let options = SessionOptionsBuilder::new(agent)
            .models(models, current_model)
            .sandboxes(convert::sandbox_options(), None)
            .reasoning(reasoning, current_reasoning)
            .build();
        emit(&ev_tx, options);
        // The skills are the session's commands. Best effort, like Claude's
        // catalogue: a Codex that cannot list them is still installed and its
        // models still reported. The working directory is this process's, the
        // same one the other probes use, so its repo-scoped skills belong to no
        // session and are left out; a session lists its own project's.
        let cwd = std::env::current_dir().unwrap_or_default();
        match transport
            .request("skills/list", json!({ "cwds": [cwd] }))
            .await
            .and_then(|listed| {
                serde_json::from_value::<SkillsListResult>(listed).map_err(|err| err.to_string())
            }) {
            Ok(listed) => {
                let skills: Vec<_> = convert::skill_catalog(listed)
                    .into_iter()
                    .filter(|skill| skill.scope != "repo")
                    .collect();
                let commands = convert::command_catalog(&skills);
                emit(&ev_tx, Event::Commands { agent, commands });
            }
            Err(err) => debug_log(&format!("codex skills/list：{err}")),
        }
        Ok::<(), String>(())
    });
}

#[cfg(test)]
mod tests {
    use super::probe;
    use crate::agent::test_support::{AGENT_TEST_LOCK, command_exists};
    use crate::agent::{Config, Event};

    /// End-to-end: probe a real Codex install (`codex app-server`) and expect
    /// its model catalog and its commands. Ignored by default, so a plain
    /// `cargo test` never drives a real agent; `cargo test -- --ignored` runs
    /// it, and it skips itself when the binary is absent.
    #[test]
    #[ignore = "drives the real codex binary"]
    fn probe_discovers_codex_models() {
        let _guard = AGENT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = Config::for_agent("codex").expect("codex is a registered agent");
        if !command_exists(&cfg.command) {
            println!(
                "skipping probe_discovers_codex_models: {} not found",
                cfg.command
            );
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        probe("codex", cfg, tx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let (mut options_seen, mut commands_seen) = (false, false);
        while !(options_seen && commands_seen) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining).expect("probe timed out") {
                Event::SessionOptions {
                    agent,
                    models,
                    modes,
                    reasoning,
                    current_reasoning,
                    ..
                } => {
                    println!(
                        "{agent}: {} models, {} modes, reasoning {:?} (active: {current_reasoning:?})",
                        models.len(),
                        modes.len(),
                        reasoning
                            .iter()
                            .map(|effort| effort.name.as_str())
                            .collect::<Vec<_>>(),
                    );
                    assert!(!models.is_empty(), "codex reported no models");
                    assert!(
                        modes.is_empty(),
                        "codex exposes no mode selector this backend maps"
                    );
                    assert!(
                        !reasoning.is_empty(),
                        "codex's default model should advertise reasoning efforts"
                    );
                    assert!(
                        reasoning
                            .iter()
                            .any(|effort| Some(effort.value.as_str())
                                == current_reasoning.as_deref()),
                        "the reported effort should be one of the offered ones: {current_reasoning:?}"
                    );
                    options_seen = true;
                }
                Event::Commands { agent, commands } => {
                    println!(
                        "{agent}: {} commands: {:?}",
                        commands.len(),
                        commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>()
                    );
                    // Codex ships system skills of its own, so an install always
                    // has some.
                    assert!(!commands.is_empty(), "codex reported no skills");
                    commands_seen = true;
                }
                Event::ProbeFailed { agent } => panic!("probe failed: {agent}"),
            }
        }
    }
}
