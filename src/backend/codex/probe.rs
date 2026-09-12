//! Model discovery for the Codex backend.
//!
//! Only `model/list` is asked for. A probe must **not** call `thread/start`:
//! that materialises a rollout on disk, so probing would litter the user's
//! Codex history with throwaway threads.

use std::sync::mpsc::Sender;

use serde_json::json;

use crate::backend::config::Config;
use crate::backend::event::{Event, emit};
use crate::backend::probe::run;

use super::protocol::ModelListResult;
use super::{client_info, convert};

/// Probe a Codex agent just far enough to learn its models: initialize, ask
/// `model/list`, report, and return — the dropped child kills the process.
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
        emit(
            &ev_tx,
            Event::SessionOptions {
                agent,
                models,
                current_model,
                // Codex exposes no mode selector this backend maps.
                modes: Vec::new(),
                current_mode: None,
                // The policies are a fixed vocabulary, so they can be offered
                // before any session exists. Which one is *in force* is only
                // learnable from a live thread's own report, so nothing is
                // claimed here.
                sandboxes: convert::sandbox_options(),
                current_sandbox: None,
                reasoning,
                current_reasoning,
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

    /// End-to-end: probe a real Codex install (`codex app-server`) and expect
    /// its model catalog. Skipped when the binary is absent, so the suite still
    /// passes on machines without a local agent.
    #[test]
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
        loop {
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
                    break;
                }
                Event::ProbeFailed { agent } => panic!("probe failed: {agent}"),
                _ => {}
            }
        }
    }
}
