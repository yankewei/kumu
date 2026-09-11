//! The live connection: handshake, session, prompt loop, and the two handlers
//! the protocol calls back on.
//!
//! One `run` drives exactly one session and is deliberately serial — commands
//! are read one at a time from the channel and each request is awaited to
//! completion before the next is read, so a connection has at most one prompt
//! in flight. Anything that must not block that loop (waiting on a user
//! decision) is moved into its own task.
//!
//! Notification handling only translates protocol shapes into [`Event`]s; all
//! turn state lives on the UI side.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, TextContent,
};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Client, ConnectionTo, Result, on_receive_notification,
    on_receive_request,
};

use super::config::Config;
use super::connection::WeakPermissions;
use super::convert::{
    mode_catalog, model_catalog, permission_detail, permission_kind, permission_outcome,
    status_name, tool_call_content, variant_name,
};
use super::event::{Cmd, Event, PermissionOptionInfo, emit};

pub(super) async fn run(
    cfg: Config,
    cwd: PathBuf,
    ev_tx: Sender<Event>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    permissions: WeakPermissions,
) -> Result<()> {
    let mut launch = AcpAgentConfig::new(&cfg.command);
    for a in &cfg.args {
        launch = launch.arg(a.clone());
    }
    for (k, v) in &cfg.env {
        launch = launch.env(k.clone(), v.clone());
    }
    let agent = AcpAgent::new(launch);

    let notif_tx = ev_tx.clone();
    let perm_tx = ev_tx.clone();
    let log_tx = ev_tx.clone();
    let auto_prompt = cfg.auto_prompt.clone();
    // Both the notification handler and the connection closure report this
    // agent's session options; `cfg` itself is moved into the latter, so take
    // the id out first.
    let agent_id = cfg.agent;
    // Distinguishes permission requests on this connection; the agent does not
    // give us one of its own.
    let next_perm_id = Arc::new(AtomicU64::new(1));
    // The request handler and the connection closure both need the table.
    let handler_permissions = permissions.clone();

    emit(
        &log_tx,
        Event::Log(format!("spawning: {} {}", cfg.command, cfg.args.join(" "))),
    );

    Client
        .builder()
        .name("kumu-demo")
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                match notification.update {
                    SessionUpdate::AgentMessageChunk(chunk) => {
                        if let ContentBlock::Text(t) = chunk.content {
                            emit(&notif_tx, Event::Agent(t.text));
                        }
                    }
                    SessionUpdate::AgentThoughtChunk(chunk) => {
                        if let ContentBlock::Text(t) = chunk.content {
                            emit(&notif_tx, Event::Thought(t.text));
                        }
                    }
                    SessionUpdate::ToolCall(call) => {
                        let content = tool_call_content(&call.content, &call.raw_output);
                        emit(
                            &notif_tx,
                            Event::ToolCall {
                                id: call.tool_call_id.0.to_string(),
                                title: call.title,
                                status: status_name(&call.status),
                                content,
                            },
                        );
                    }
                    SessionUpdate::ToolCallUpdate(update) => {
                        let content = update
                            .fields
                            .content
                            .as_ref()
                            .map(|c| tool_call_content(c, &update.fields.raw_output))
                            .unwrap_or_default();
                        emit(
                            &notif_tx,
                            Event::ToolCall {
                                id: update.tool_call_id.0.to_string(),
                                title: update.fields.title.unwrap_or_default(),
                                status: update
                                    .fields
                                    .status
                                    .as_ref()
                                    .map(status_name)
                                    .unwrap_or(""),
                                content,
                            },
                        );
                    }
                    SessionUpdate::SessionInfoUpdate(info) => {
                        if let Some(title) = info.title.take() {
                            let title = title.trim().to_string();
                            if !title.is_empty() {
                                emit(&notif_tx, Event::SessionTitle { title });
                            }
                        }
                    }
                    SessionUpdate::ConfigOptionUpdate(update) => {
                        // The notification carries the complete state, so both
                        // catalogs are rebuilt from it. This is what keeps the
                        // pickers honest when the agent changes its own model
                        // or mode (e.g. leaving a plan mode on its own).
                        let config_options = Some(update.config_options);
                        let (_, current_model, models) = model_catalog(&config_options);
                        let (_, current_mode, modes) = mode_catalog(&config_options);
                        emit(
                            &notif_tx,
                            Event::SessionOptions {
                                agent: agent_id,
                                models,
                                current_model,
                                modes,
                                current_mode,
                            },
                        );
                    }
                    other => {
                        let name = variant_name(&other).to_string();
                        emit(&notif_tx, Event::Log(format!("update: {name}")));
                    }
                }
                Ok(())
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                // With nothing to offer the user there is no way to ask, so the
                // only honest answer is the protocol's cancelled outcome.
                if request.options.is_empty() {
                    let _ = responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                    return Ok(());
                }

                // No UI left to ask: its handle was dropped, which is exactly
                // the signal that this connection is being torn down.
                let Some(table) = handler_permissions.upgrade() else {
                    let _ = responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                    return Ok(());
                };

                let request_id = format!("perm-{}", next_perm_id.fetch_add(1, Ordering::Relaxed));
                let tool_call_id = request.tool_call.tool_call_id.0.to_string();
                let title = request.tool_call.fields.title.clone().unwrap_or_default();
                let detail = permission_detail(&request.tool_call);
                let options = request
                    .options
                    .iter()
                    .map(|o| PermissionOptionInfo {
                        id: o.option_id.0.to_string(),
                        name: o.name.clone(),
                        kind: permission_kind(&o.kind),
                    })
                    .collect::<Vec<_>>();

                let (tx, rx) = tokio::sync::oneshot::channel();
                // The guard is released before spawning, so it never crosses an
                // await; the upgraded handle is dropped so teardown is not held
                // up by this registration.
                table.lock().unwrap().insert(request_id.clone(), tx);
                drop(table);
                emit(
                    &perm_tx,
                    Event::PermissionRequest {
                        tool_call_id,
                        request_id,
                        title,
                        detail,
                        options,
                    },
                );

                // Waiting for the user here would block the dispatch loop and
                // stall every other message on this connection, so the responder
                // moves into its own task instead.
                let log_tx = perm_tx.clone();
                tokio::spawn(async move {
                    let option_id = rx.await.ok().flatten();
                    emit(
                        &log_tx,
                        Event::Log(format!("permission answered: {option_id:?}")),
                    );
                    let _ = responder.respond(RequestPermissionResponse::new(permission_outcome(
                        option_id,
                    )));
                });
                Ok(())
            },
            on_receive_request!(),
        )
        .connect_with(
            agent,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                let init = connection
                    .send_request(InitializeRequest::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                    .block_task()
                    .await?;
                emit(
                    &log_tx,
                    Event::Log(format!(
                        "initialized: agent={:?}",
                        init.agent_info.as_ref().map(|i| &i.name)
                    )),
                );

                let new_session = connection
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let session_id = new_session.session_id;
                emit(&log_tx, Event::Log(format!("session ready: {session_id}")));

                // Read the model and mode config options: their ids let the UI
                // switch either later; the catalogs feed the composer's
                // pickers.
                let (model_option_id, current_model, models) =
                    model_catalog(&new_session.config_options);
                let (mode_option_id, current_mode, modes) =
                    mode_catalog(&new_session.config_options);
                emit(
                    &ev_tx,
                    Event::SessionOptions {
                        agent: agent_id,
                        models,
                        current_model,
                        modes,
                        current_mode,
                    },
                );

                // Headless convenience: send one prompt before the UI loop.
                if let Some(auto) = auto_prompt {
                    emit(&log_tx, Event::Log(format!("auto prompt: {auto}")));
                    let response = connection
                        .send_request(PromptRequest::new(
                            session_id.clone(),
                            vec![ContentBlock::Text(TextContent::new(auto))],
                        ))
                        .block_task()
                        .await?;
                    emit(&ev_tx, Event::Done(format!("{:?}", response.stop_reason)));
                }

                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        Cmd::Prompt(text) => {
                            emit(&log_tx, Event::Log("prompt sent".into()));
                            let response = connection
                                .send_request(PromptRequest::new(
                                    session_id.clone(),
                                    vec![ContentBlock::Text(TextContent::new(text))],
                                ))
                                .block_task()
                                .await?;
                            emit(&ev_tx, Event::Done(format!("{:?}", response.stop_reason)));
                        }
                        Cmd::SetModel(value) => match &model_option_id {
                            Some(option_id) => {
                                if let Err(err) = connection
                                    .send_request(SetSessionConfigOptionRequest::new(
                                        session_id.clone(),
                                        option_id.clone(),
                                        value.as_str(),
                                    ))
                                    .block_task()
                                    .await
                                {
                                    emit(&log_tx, Event::Log(format!("set model failed: {err}")));
                                } else {
                                    emit(&log_tx, Event::Log(format!("model set: {value}")));
                                }
                            }
                            None => {
                                emit(&log_tx, Event::Log("no model config option to set".into()))
                            }
                        },
                        Cmd::SetMode(value) => match &mode_option_id {
                            Some(option_id) => {
                                if let Err(err) = connection
                                    .send_request(SetSessionConfigOptionRequest::new(
                                        session_id.clone(),
                                        option_id.clone(),
                                        value.as_str(),
                                    ))
                                    .block_task()
                                    .await
                                {
                                    emit(&log_tx, Event::Log(format!("set mode failed: {err}")));
                                } else {
                                    emit(&log_tx, Event::Log(format!("mode set: {value}")));
                                }
                            }
                            None => {
                                emit(&log_tx, Event::Log("no mode config option to set".into()))
                            }
                        },
                    }
                }
                // Dropping the senders wakes any task still waiting on a user
                // decision; it answers `Cancelled` so the agent is never left
                // hanging on a connection that is going away. (If the UI already
                // dropped its handle the table is gone and this is unnecessary.)
                if let Some(table) = permissions.upgrade() {
                    table.lock().unwrap().clear();
                }
                emit(&log_tx, Event::Log("connection closing".into()));
                Ok(())
            },
        )
        .await
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::test_support::{AGENT_TEST_LOCK, command_exists};
    use crate::acp::{Cmd, Config, Event, PermissionKind, spawn};

    /// End-to-end: spawn a real agent (KUMU_AGENT, default omp), send one
    /// prompt, and expect streamed Agent chunks followed by Done — the same
    /// path the home window uses. Skipped when the binary is missing.
    #[test]
    fn session_prompts_agent_end_to_end() {
        let _guard = AGENT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let agent: &'static str = match std::env::var("KUMU_AGENT").as_deref() {
            Ok("opencode") => "opencode",
            _ => "omp",
        };
        let mut cfg = Config::for_agent(agent).expect("known agent");
        if !command_exists(&cfg.command) {
            println!(
                "skipping session_prompts_agent_end_to_end: {} not found",
                cfg.command
            );
            return;
        }
        cfg.auto_prompt = None;
        let (tx, rx) = std::sync::mpsc::channel();
        let conn = spawn(
            cfg,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            tx,
        );

        // Wait for the catalogs, then pick a model and a mode before prompting
        // — exercises session/set_config_option on the live path for both, in
        // the order they are queued.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining).expect("catalog timed out") {
                Event::SessionOptions { models, modes, .. } => {
                    if let Some(model) = models.first() {
                        conn.send(Cmd::SetModel(model.value.clone()));
                    }
                    if let Some(mode) = modes.get(1) {
                        conn.send(Cmd::SetMode(mode.value.clone()));
                    }
                    break;
                }
                Event::Error(err) => panic!("acp error: {err}"),
                _ => {}
            }
        }
        conn.send(Cmd::Prompt("Reply with exactly: ok".into()));

        let mut text = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining).expect("prompt timed out") {
                Event::Agent(chunk) => text.push_str(&chunk),
                // Nobody is there to click, so take the first allow option and
                // let the turn finish; this also exercises the answer path.
                Event::PermissionRequest {
                    request_id,
                    options,
                    ..
                } => {
                    let choice = options
                        .iter()
                        .find(|o| {
                            matches!(
                                o.kind,
                                PermissionKind::AllowOnce | PermissionKind::AllowAlways
                            )
                        })
                        .or_else(|| options.first())
                        .map(|o| o.id.clone());
                    conn.resolve_permission(&request_id, choice);
                }
                Event::Done(_) => {
                    println!("{agent} replied: {text:?}");
                    assert!(text.contains("ok"));
                    break;
                }
                Event::Error(err) => panic!("acp error: {err}"),
                _ => {}
            }
        }
    }
}
