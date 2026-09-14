//! Session option discovery: a throwaway connection that handshakes, creates
//! one session, reads its model and mode config options and returns.
//!
//! Nothing is kept alive — the connection dropping is what kills the agent
//! subprocess, so a probe never leaves a process behind.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use agent_client_protocol::schema::v1::{InitializeRequest, NewSessionRequest};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Client, ConnectionTo};

use super::convert::{mode_catalog, model_catalog, reasoning_catalog};
use crate::backend::config::Config;
use crate::backend::event::{Event, emit};
use crate::backend::probe::run;
use crate::backend::session::SessionOptionsBuilder;

/// Probe an agent over ACP just to discover its session options: initialize,
/// create one throwaway session, read the model and mode config options, then
/// return — the connection teardown kills the agent subprocess.
pub fn probe(agent: &'static str, cfg: Config, ev_tx: Sender<Event>) {
    run(agent, "acp", ev_tx, move |ev_tx| async move {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut launch = AcpAgentConfig::new(&cfg.command);
        for a in &cfg.args {
            launch = launch.arg(a.clone());
        }
        for (k, v) in &cfg.env {
            launch = launch.env(k.clone(), v.clone());
        }
        Client
            .builder()
            .name("kumu-probe")
            .connect_with(
                AcpAgent::new(launch),
                move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                    connection
                        .send_request(InitializeRequest::new(
                            agent_client_protocol::schema::ProtocolVersion::V1,
                        ))
                        .block_task()
                        .await?;
                    let new_session = connection
                        .send_request(NewSessionRequest::new(cwd))
                        .block_task()
                        .await?;
                    let (_, current_model, models) = model_catalog(&new_session.config_options);
                    let (_, current_mode, modes) = mode_catalog(&new_session.config_options);
                    let (_, current_reasoning, reasoning) =
                        reasoning_catalog(&new_session.config_options);
                    // ACP has no access or sandbox axis, so neither is set.
                    let options = SessionOptionsBuilder::new(agent)
                        .models(models, current_model)
                        .modes(modes, current_mode)
                        .reasoning(reasoning, current_reasoning)
                        .build();
                    emit(&ev_tx, options);
                    Ok(())
                },
            )
            .await
    });
}

#[cfg(test)]
mod tests {
    use super::probe;
    use crate::backend::test_support::{AGENT_TEST_LOCK, command_exists};
    use crate::backend::{Config, Event};

    /// End-to-end: probe a real local agent (KUMU_AGENT, default omp) and
    /// expect a Models event. Skipped when the agent binary is missing so the
    /// suite still passes on machines without local agents.
    #[test]
    fn probe_discovers_models() {
        let _guard = AGENT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let agent: &'static str = match std::env::var("KUMU_AGENT").as_deref() {
            Ok("opencode") => "opencode",
            _ => "omp",
        };
        let cfg = Config::for_agent(agent).expect("known agent");
        if !command_exists(&cfg.command) {
            println!("skipping probe_discovers_models: {} not found", cfg.command);
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        probe(agent, cfg, tx);
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
                    reasoning,
                    current_reasoning,
                    ..
                } => {
                    println!(
                        "{agent}: {} models, active: {current_model:?}; modes: {:?}, active: \
                         {current_mode:?}; reasoning: {:?}, active: {current_reasoning:?}",
                        models.len(),
                        modes.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
                        reasoning
                            .iter()
                            .map(|effort| effort.name.as_str())
                            .collect::<Vec<_>>(),
                    );
                    assert!(!models.is_empty());
                    assert!(
                        !reasoning.is_empty(),
                        "{agent} should expose its thought_level option"
                    );
                    break;
                }
                Event::ProbeFailed { agent } => panic!("probe failed: {agent}"),
                _ => {}
            }
        }
    }

    /// A probe against a missing binary must surface `ProbeFailed` instead of
    /// failing silently (spawn/handshake errors were previously swallowed).
    #[test]
    fn probe_missing_agent_reports_failure() {
        let (tx, rx) = std::sync::mpsc::channel();
        probe(
            "missing",
            Config {
                agent: "missing",
                backend: crate::backend::config::Backend::Acp,
                command: "/nonexistent/kumu-agent-test".into(),
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
