//! Model discovery: a throwaway connection that handshakes, creates one
//! session, reads its model config option and returns.
//!
//! Nothing is kept alive — the connection dropping is what kills the agent
//! subprocess, so a probe never leaves a process behind.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use agent_client_protocol::schema::v1::{InitializeRequest, NewSessionRequest};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Client, ConnectionTo};

use super::config::Config;
use super::convert::model_catalog;
use super::event::{Event, emit};

/// Probe an agent over ACP just to discover its models: initialize, create
/// one throwaway session, read the model config option, then return — the
/// connection teardown kills the agent subprocess. Fails after `TIMEOUT`.
pub fn probe(agent: &'static str, cfg: Config, ev_tx: Sender<Event>) {
    std::thread::Builder::new()
        .name(format!("kumu-probe-{agent}"))
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            let result = rt.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(15), async {
                    let cwd =
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
                    let mut launch = AcpAgentConfig::new(&cfg.command);
                    for a in &cfg.args {
                        launch = launch.arg(a.clone());
                    }
                    for (k, v) in &cfg.env {
                        launch = launch.env(k.clone(), v.clone());
                    }
                    let ev_tx = ev_tx.clone();
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
                                let (_, current, models) =
                                    model_catalog(&new_session.config_options);
                                emit(
                                    &ev_tx,
                                    Event::Models {
                                        agent,
                                        models,
                                        current,
                                    },
                                );
                                Ok(())
                            },
                        )
                        .await
                })
                .await
            });
            let fail = |msg: String| {
                if std::env::var("KUMU_DEBUG").is_ok() {
                    eprintln!("[acp] probe {agent} failed: {msg}");
                }
                emit(&ev_tx, Event::ProbeFailed { agent });
            };
            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => fail(err.to_string()),
                Err(err) => fail(format!("timed out: {err}")),
            }
        })
        .expect("failed to spawn probe thread");
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{AGENT_TEST_LOCK, command_exists};
    use super::probe;
    use crate::acp::{Config, Event};

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
                Event::Models {
                    agent,
                    models,
                    current,
                } => {
                    println!("{agent}: {} models, active: {current:?}", models.len());
                    assert!(!models.is_empty());
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
                command: "/nonexistent/kumu-agent-test".into(),
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
                Event::Models { .. } => panic!("unexpected probe success"),
                Event::ProbeFailed { agent } => {
                    assert_eq!(agent, "missing");
                    break;
                }
                _ => {}
            }
        }
    }
}
