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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, LoadSessionRequest, NewSessionRequest,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, TextContent,
};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Client, ConnectionTo, Result, on_receive_notification,
    on_receive_request,
};

use super::convert::{
    command_catalog, mode_catalog, model_catalog, permission_detail, permission_kind,
    permission_outcome, reasoning_catalog, status_name, stop_reason, tool_call_content,
    variant_name,
};
use crate::agent::config::Config;
use crate::agent::connection::WeakPermissions;
use crate::agent::event::{Cmd, Event, PermissionOptionInfo, emit};
use crate::agent::session::{
    CancelRequests, PermissionQuestion, SessionOptionsBuilder, ask_user, forget_stale_cancels,
    release_pending_permissions,
};

/// Translate one session update into the event it produces, or none for an
/// update kind nobody renders — every kind maps to at most one `Event`.
///
/// Stateless, unlike the Claude and Codex backends' own message handlers:
/// this protocol's notifications carry no history to consult (no
/// delta/complete-text dedup, no output to accumulate), so there is nothing
/// here to seed beyond the agent's own id.
fn notification_event(agent_id: &'static str, update: SessionUpdate) -> Option<Event> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
            ContentBlock::Text(t) => Some(Event::Agent(t.text)),
            _ => None,
        },
        SessionUpdate::AgentThoughtChunk(chunk) => {
            // The label reasoning rows are grouped by; see `Event::Thought`.
            let message = chunk.message_id.map(|id| id.0.to_string());
            match chunk.content {
                ContentBlock::Text(t) => Some(Event::Thought {
                    text: t.text,
                    message,
                }),
                _ => None,
            }
        }
        SessionUpdate::ToolCall(call) => {
            let content = tool_call_content(&call.content, &call.raw_output);
            Some(Event::ToolCall {
                id: call.tool_call_id.0.to_string(),
                title: call.title,
                status: status_name(&call.status),
                content,
            })
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let content = update
                .fields
                .content
                .as_ref()
                .map(|c| tool_call_content(c, &update.fields.raw_output))
                .unwrap_or_default();
            Some(Event::ToolCall {
                id: update.tool_call_id.0.to_string(),
                title: update.fields.title.unwrap_or_default(),
                status: update.fields.status.as_ref().map(status_name).unwrap_or(""),
                content,
            })
        }
        SessionUpdate::SessionInfoUpdate(info) => info
            .title
            .take()
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty())
            .map(|title| Event::SessionTitle { title }),
        // The agent's command catalogue, replaced whole whenever it says so:
        // these are usually the commands its own TUI offers, and invoking one is
        // a prompt that starts with the command text.
        SessionUpdate::AvailableCommandsUpdate(update) => Some(Event::Commands {
            agent: agent_id,
            commands: command_catalog(&update.available_commands),
        }),
        SessionUpdate::ConfigOptionUpdate(update) => {
            // The notification carries the complete state, so every catalog is
            // rebuilt from it. This is what keeps the pickers honest when the
            // agent changes its own model, mode or reasoning level.
            let config_options = Some(update.config_options);
            let (_, current_model, models) = model_catalog(&config_options);
            let (_, current_mode, modes) = mode_catalog(&config_options);
            let (_, current_reasoning, reasoning) = reasoning_catalog(&config_options);
            // ACP has no access or sandbox axis, so neither is set.
            Some(
                SessionOptionsBuilder::new(agent_id)
                    .models(models, current_model)
                    .modes(modes, current_mode)
                    .reasoning(reasoning, current_reasoning)
                    .build(),
            )
        }
        other => Some(Event::Log(format!("update: {}", variant_name(&other)))),
    }
}

/// Drive one ACP session to completion. Called by the shared [`spawn`]
/// dispatcher, which is one level up, so the visibility follows the caller.
///
/// [`spawn`]: crate::agent::spawn
pub(in crate::agent) async fn run(
    cfg: Config,
    cwd: PathBuf,
    ev_tx: Sender<Event>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    mut cancels: CancelRequests,
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
    let loading = Arc::new(AtomicBool::new(cfg.session.resume_id.is_some()));
    let notification_loading = loading.clone();

    emit(
        &log_tx,
        Event::Log(format!("spawning: {} {}", cfg.command, cfg.args.join(" "))),
    );

    Client
        .builder()
        .name("kumu-demo")
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                // Loading replays the transcript before replying. SQLite already
                // supplies those rows; only session metadata should reach the UI.
                if notification_loading.load(Ordering::Acquire)
                    && matches!(notification.update,
                        SessionUpdate::UserMessageChunk(_)
                        | SessionUpdate::AgentMessageChunk(_)
                        | SessionUpdate::AgentThoughtChunk(_)
                        | SessionUpdate::ToolCall(_)
                        | SessionUpdate::ToolCallUpdate(_))
                {
                    return Ok(());
                }
                if let Some(event) = notification_event(agent_id, notification.update) {
                    emit(&notif_tx, event);
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

                let log_tx = perm_tx.clone();
                ask_user(
                    table,
                    &next_perm_id,
                    &perm_tx,
                    PermissionQuestion {
                        tool_call_id: request.tool_call.tool_call_id.0.to_string(),
                        title: request.tool_call.fields.title.clone().unwrap_or_default(),
                        detail: permission_detail(&request.tool_call),
                        options: request
                            .options
                            .iter()
                            .map(|o| PermissionOptionInfo {
                                id: o.option_id.0.to_string(),
                                name: o.name.clone(),
                                kind: permission_kind(&o.kind),
                            })
                            .collect(),
                    },
                    move |option_id| {
                        emit(
                            &log_tx,
                            Event::Log(format!("permission answered: {option_id:?}")),
                        );
                        let _ = responder.respond(RequestPermissionResponse::new(
                            permission_outcome(option_id),
                        ));
                    },
                );
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

                let (session_id, config_options) = if let Some(id) = &cfg.session.resume_id {
                    if !init.agent_capabilities.load_session {
                        return Err(agent_client_protocol::Error::new(
                            -32603,
                            "当前 agent 不支持恢复会话，未开启新会话。请新建会话，或使用支持恢复的 agent 版本。",
                        ));
                    }
                    let loaded = connection
                        .send_request(LoadSessionRequest::new(id.clone(), cwd))
                        .block_task()
                        .await
                        .map_err(|error| agent_client_protocol::Error::new(
                            -32603, format!("恢复 agent 会话失败，未开启新会话：{error}"),
                        ))?;
                    (id.clone().into(), loaded.config_options)
                } else {
                    let created = connection
                        .send_request(NewSessionRequest::new(cwd))
                        .block_task()
                        .await?;
                    (created.session_id, created.config_options)
                };
                loading.store(false, Ordering::Release);
                emit(&log_tx, Event::SessionReady { resume_id: session_id.0.to_string() });
                emit(&log_tx, Event::Log(format!("session ready: {session_id}")));

                // Read the model, mode and reasoning config options: their ids
                // let the UI switch any of them later; the catalogs feed the
                // composer's pickers.
                let (model_option_id, current_model, models) =
                    model_catalog(&config_options);
                let (mode_option_id, current_mode, modes) =
                    mode_catalog(&config_options);
                let (reasoning_option_id, current_reasoning, reasoning) =
                    reasoning_catalog(&config_options);
                let options = SessionOptionsBuilder::new(agent_id)
                    .models(models, current_model)
                    .modes(modes, current_mode)
                    .reasoning(reasoning, current_reasoning)
                    .build();
                emit(&ev_tx, options);

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
                    emit(&ev_tx, Event::Done(stop_reason(&response.stop_reason)));
                }

                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        Cmd::Prompt(text) => {
                            forget_stale_cancels(&mut cancels);
                            emit(&log_tx, Event::Log("prompt sent".into()));
                            let prompt = connection
                                .send_request(PromptRequest::new(
                                    session_id.clone(),
                                    vec![ContentBlock::Text(TextContent::new(text))],
                                ))
                                .block_task();
                            // The cancel has to reach the agent while this request
                            // is outstanding, so it is watched for here rather
                            // than on the command channel: this loop is parked on
                            // the very request it would cancel.
                            tokio::pin!(prompt);
                            let mut asked = false;
                            let response = loop {
                                tokio::select! {
                                    response = &mut prompt => break response?,
                                    _ = cancels.recv(), if !asked => {
                                        asked = true;
                                        emit(&log_tx, Event::Log("cancel requested".into()));
                                        // A notification, not the request's
                                        // answer: the prompt is still outstanding
                                        // and its own response carries the stop
                                        // reason this backend reports below.
                                        let _ = connection.send_notification(
                                            CancelNotification::new(session_id.clone()),
                                        );
                                        // The protocol requires every pending
                                        // permission request to be answered
                                        // `Cancelled` when a turn is cancelled, or
                                        // it parks behind a prompt nobody can
                                        // answer. Dropping the senders is what
                                        // makes the waiting tasks do exactly that.
                                        release_pending_permissions(&permissions);
                                    }
                                }
                            };
                            emit(&ev_tx, Event::Done(stop_reason(&response.stop_reason)));
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
                        Cmd::SetSandbox(value) => {
                            // ACP has no sandbox axis, so the UI offers no chip
                            // for one and this should not arrive; saying so beats
                            // dropping it silently.
                            emit(&log_tx, Event::Log(format!("no sandbox to set: {value}")));
                        }
                        Cmd::SetReasoning(value) => match &reasoning_option_id {
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
                                    emit(
                                        &log_tx,
                                        Event::Log(format!("set reasoning failed: {err}")),
                                    );
                                } else {
                                    emit(&log_tx, Event::Log(format!("reasoning set: {value}")));
                                }
                            }
                            None => emit(
                                &log_tx,
                                Event::Log("no reasoning config option to set".into()),
                            ),
                        },
                    }
                }
                // The dropped senders wake any task still waiting on a user
                // decision; it answers `Cancelled` so the agent is never left
                // hanging on a connection that is going away.
                release_pending_permissions(&permissions);
                emit(&log_tx, Event::Log("connection closing".into()));
                Ok(())
            },
        )
        .await
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::agent::test_support::{AGENT_TEST_LOCK, command_exists};
    use crate::agent::{Cmd, Config, Event, PermissionKind, spawn};

    use super::notification_event;
    use agent_client_protocol::schema::v1::{
        AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, ContentBlock,
        ContentChunk, Plan, SessionInfoUpdate, SessionUpdate, TextContent, ToolCall,
        ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, UnstructuredCommandInput,
    };

    fn text(t: &str) -> ContentBlock {
        ContentBlock::Text(TextContent::new(t))
    }

    /// A reply chunk with text content becomes agent text; kumu renders none of
    /// the other content kinds (image, audio, resource) inline, so those must
    /// not turn into an `Event`.
    #[test]
    fn agent_message_text_streams_as_agent_text() {
        let event = notification_event(
            "omp",
            SessionUpdate::AgentMessageChunk(ContentChunk::new(text("hi"))),
        );
        assert!(matches!(event, Some(Event::Agent(t)) if t == "hi"));
    }

    /// The reasoning chunk is the same shape as the reply, but becomes a
    /// `Thought` rather than `Agent` — the two must not be confused — and it
    /// carries the message it belongs to, which is what groups reasoning rows.
    #[test]
    fn agent_thought_text_streams_as_thought_under_its_message() {
        let event = notification_event(
            "omp",
            SessionUpdate::AgentThoughtChunk(
                ContentChunk::new(text("considering...")).message_id("m1"),
            ),
        );
        assert!(
            matches!(
                event,
                Some(Event::Thought { text, message: Some(message) })
                    if text == "considering..." && message == "m1"
            ),
            "the chunk's own message id has to survive the crossing"
        );
    }

    /// An agent that stamps no message id on its reasoning still streams it:
    /// the label is what the UI groups rows by, not something it renders.
    #[test]
    fn a_thought_chunk_without_a_message_id_still_streams() {
        let event = notification_event(
            "omp",
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(text("considering..."))),
        );
        assert!(matches!(
            event,
            Some(Event::Thought { text, message: None }) if text == "considering..."
        ));
    }

    /// A new tool call starts running under its own id and title, in the
    /// agent's own words.
    #[test]
    fn a_new_tool_call_reports_its_id_and_title() {
        let event = notification_event(
            "omp",
            SessionUpdate::ToolCall(ToolCall::new("call-1", "Write file")),
        );
        match event {
            Some(Event::ToolCall {
                id, title, status, ..
            }) => {
                assert_eq!(id, "call-1");
                assert_eq!(title, "Write file");
                assert_eq!(status, "pending", "a fresh call has no status yet");
            }
            _ => panic!("expected a ToolCall"),
        }
    }

    /// A later update carries only what changed — an empty title, say, when
    /// only the status moved — but always reports the same id, so the UI can
    /// find the row it belongs to.
    #[test]
    fn a_tool_calls_update_reports_only_what_changed() {
        let event = notification_event(
            "omp",
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "call-1",
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            )),
        );
        match event {
            Some(Event::ToolCall {
                id, title, status, ..
            }) => {
                assert_eq!(id, "call-1");
                assert_eq!(title, "", "the title did not change on this update");
                assert_eq!(status, "done");
            }
            _ => panic!("expected a ToolCall"),
        }
    }

    /// A title the agent sets becomes the session's; a title it explicitly
    /// clears (`null`, as opposed to one it never mentions) is not shown as
    /// one — there is nothing to rename to.
    #[test]
    fn session_info_update_reports_a_title_but_not_a_clearing() {
        let renamed = notification_event(
            "omp",
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title("重构计划")),
        );
        assert!(matches!(renamed, Some(Event::SessionTitle { title }) if title == "重构计划"));

        let cleared = notification_event(
            "omp",
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(None::<String>)),
        );
        assert!(cleared.is_none());
    }

    /// The agent's command catalogue becomes a `Commands` event in the agent's
    /// own order, carrying the input hint it described. A nameless entry is not
    /// an offer, and a command with no hint is simply a command.
    #[test]
    fn available_commands_become_the_command_catalogue() {
        let event = notification_event(
            "omp",
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                AvailableCommand::new("web", "Search the web").input(
                    AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("query")),
                ),
                AvailableCommand::new("test", "Run tests"),
                AvailableCommand::new("", "nameless"),
            ])),
        );
        match event {
            Some(Event::Commands { agent, commands }) => {
                assert_eq!(agent, "omp");
                assert_eq!(
                    commands
                        .iter()
                        .map(|command| command.name.as_str())
                        .collect::<Vec<_>>(),
                    ["web", "test"],
                    "the agent's order stands, and a nameless entry is dropped"
                );
                assert_eq!(commands[0].hint.as_deref(), Some("query"));
                assert_eq!(commands[1].hint, None, "no input spec is no hint");
            }
            _ => panic!("expected a Commands event"),
        }
    }

    /// An update kind nobody renders (the agent's plan, say) is not dropped
    /// silently — it reaches the log under its own name, which is the canary
    /// for the protocol growing an update kind this build should show.
    #[test]
    fn an_unrendered_update_is_logged_by_name() {
        let event = notification_event("omp", SessionUpdate::Plan(Plan::new(Vec::new())));
        assert!(matches!(event, Some(Event::Log(msg)) if msg.contains("plan")));
    }

    /// End-to-end: spawn a real agent (KUMU_AGENT, default omp), send one
    /// prompt, and expect streamed Agent chunks followed by Done — the same
    /// path the home window uses. Ignored by default, so a plain `cargo test`
    /// never drives a real agent; `cargo test -- --ignored` runs it, and it
    /// skips itself when the binary is absent.
    #[test]
    #[ignore = "drives a real agent binary (KUMU_AGENT)"]
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
                Event::SessionOptions {
                    models,
                    modes,
                    reasoning,
                    ..
                } => {
                    if let Some(model) = models.first() {
                        conn.send(Cmd::SetModel(model.value.clone()));
                    }
                    if let Some(mode) = modes.get(1) {
                        conn.send(Cmd::SetMode(mode.value.clone()));
                    }
                    // The second effort differs from the reported one, so this
                    // exercises setting a thought_level option over the wire.
                    if let Some(effort) = reasoning.get(1) {
                        conn.send(Cmd::SetReasoning(effort.value.clone()));
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
