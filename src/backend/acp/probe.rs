//! Session option discovery: a throwaway connection that handshakes, creates
//! one session, reads its model and mode config options, waits briefly for the
//! command catalogue it advertises, and returns.
//!
//! Nothing is kept alive — the connection dropping is what kills the agent
//! subprocess, so a probe never leaves a process behind.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    InitializeRequest, NewSessionRequest, SessionNotification, SessionUpdate,
};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Client, ConnectionTo, on_receive_notification,
};

use super::convert::{command_catalog, mode_catalog, model_catalog, reasoning_catalog};
use crate::backend::config::Config;
use crate::backend::event::{Event, emit};
use crate::backend::probe::run;
use crate::backend::session::SessionOptionsBuilder;

/// How long to wait for the command catalogue after creating the session.
///
/// It arrives as a notification rather than in the `session/new` response, so
/// there is nothing to read synchronously; an agent that offers no commands
/// sends nothing at all, which is why this is a wait with a deadline and not a
/// failure.
const COMMAND_WAIT: Duration = Duration::from_secs(2);

/// Probe an agent over ACP just to discover its session options: initialize,
/// create one throwaway session, read the model and mode config options, then
/// return — the connection teardown kills the agent subprocess.
pub fn probe(agent: &'static str, cfg: Config, ev_tx: Sender<Event>) {
    run(agent, "acp", ev_tx, move |ev_tx| async move {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let (commands_tx, mut commands_rx) = tokio::sync::mpsc::unbounded_channel();
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
            // The command catalogue is a session notification, not part of the
            // `session/new` response, so it is collected as it arrives and
            // awaited after the session exists. The channel is unbounded, which
            // is what makes an update that lands during `session/new` still count.
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    if let SessionUpdate::AvailableCommandsUpdate(update) = notification.update {
                        let _ = commands_tx.send(command_catalog(&update.available_commands));
                    }
                    Ok(())
                },
                on_receive_notification!(),
            )
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
                    // The agent reports its commands after creating the session.
                    // Waiting for a moment is what separates "this agent offers
                    // none" from "not yet" — and an agent that offers none simply
                    // sends nothing, so the wait ending empty is not a failure.
                    if let Ok(Some(commands)) =
                        tokio::time::timeout(COMMAND_WAIT, commands_rx.recv()).await
                    {
                        emit(&ev_tx, Event::Commands { agent, commands });
                    }
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
    /// expect a Models event. Ignored by default, so a plain `cargo test` never
    /// drives a real agent; `cargo test -- --ignored` runs it, and it skips
    /// itself when the binary is absent.
    #[test]
    #[ignore = "drives a real agent binary (KUMU_AGENT)"]
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
        // The commands follow the axes, from the notification the agent sends
        // after creating the session. This is also the check that the two ACP
        // agents kumu drives advertise a catalogue at all.
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx
                .recv_timeout(remaining)
                .expect("the probe ended without a command catalogue")
            {
                Event::Commands { commands, .. } => {
                    println!(
                        "{agent}: {} commands ({})",
                        commands.len(),
                        commands
                            .iter()
                            .map(|command| command.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    assert!(!commands.is_empty(), "{agent} advertised no commands");
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
