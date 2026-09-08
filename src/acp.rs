//! ACP integration: talks v1 ACP over stdio to a local agent process (`opencode acp`).
//!
//! Runs on a dedicated tokio runtime thread. UI -> here via an unbounded tokio
//! channel (send is non-blocking, safe from the gpui thread); here -> UI via a
//! std mpsc channel that the gpui side polls.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use agent_client_protocol::schema::v1::SessionUpdate;
use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigId, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption, SessionConfigSelectOptions,
    SessionNotification, SetSessionConfigOptionRequest, TextContent, ToolCallStatus,
};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Client, ConnectionTo, Result, on_receive_notification,
    on_receive_request,
};

/// Events flowing from the ACP thread to the UI.
pub enum Event {
    /// Log / connection chatter (grey text).
    Log(String),
    /// A chunk of agent reply text; append to the current agent row.
    Agent(String),
    /// A chunk of the agent's internal reasoning; transient (not persisted).
    Thought(String),
    /// A tool call started or changed. `title`/`status` are empty when the
    /// update didn't touch them; `status` is one of pending/running/done/failed.
    ToolCall {
        id: String,
        title: String,
        status: &'static str,
    },
    /// The agent named the session (session/info update with a title).
    SessionTitle { title: String },
    /// The prompt turn ended with this stop reason.
    Done(String),
    /// A permission request was auto-approved (which option).
    Permission(String),
    /// A fatal error occurred.
    Error(String),
    /// The models an agent exposes, from its session's model config option
    /// (sent by both probes and live connections).
    Models {
        agent: &'static str,
        models: Vec<ModelInfo>,
        /// The value the agent reports as active for this session.
        current: Option<String>,
    },
    /// A probe connection failed (agent missing / handshake error).
    ProbeFailed { agent: &'static str },
}

/// One selectable model as exposed by an agent.
#[derive(Clone, Debug)]
pub struct ModelInfo {
    /// Value to pass to `session/set_config_option`.
    pub value: String,
    /// Human-readable label.
    pub name: String,
    /// Provider inferred from the value prefix before `/`.
    pub provider: String,
}

/// UI -> ACP commands on the channel returned by [`spawn`].
pub enum Cmd {
    Prompt(String),
    /// Select the session's model by its config-option value.
    SetModel(String),
}

/// How to launch the local agent subprocess.
pub struct Config {
    /// Static registry id this config was built for (used in events).
    pub agent: &'static str,
    pub command: String,
    pub args: Vec<String>,
    /// Extra env vars applied on top of the inherited environment.
    pub env: Vec<(String, String)>,
    /// If set, send this prompt automatically once the session is ready.
    pub auto_prompt: Option<String>,
}

impl Config {
    /// Build the config for a known agent id from the static registry.
    pub fn for_agent(id: &'static str) -> Option<Self> {
        match id {
            "opencode" => Some(Self::opencode(id)),
            "omp" => Some(Self::omp(id)),
            _ => None,
        }
    }

    /// `opencode acp`, with the RC gateway key injected so the subprocess can
    /// reach a model even when launched from a GUI app.
    fn opencode(agent: &'static str) -> Self {
        let mut env = Vec::new();
        if let Some(key) = opencode_api_key() {
            env.push(("OPENCODE_API_KEY".to_string(), key));
        }
        Self {
            agent,
            command: opencode_bin(),
            args: vec!["acp".to_string()],
            env,
            auto_prompt: auto_prompt_env(),
        }
    }

    /// `omp acp` (Oh My Pi); uses its own stored model config.
    fn omp(agent: &'static str) -> Self {
        Self {
            agent,
            command: std::env::var("OMP_BIN").unwrap_or_else(|_| "omp".to_string()),
            args: vec!["acp".to_string()],
            env: Vec::new(),
            auto_prompt: auto_prompt_env(),
        }
    }
}

fn auto_prompt_env() -> Option<String> {
    std::env::var("KUMU_AUTO_PROMPT")
        .ok()
        .filter(|s| !s.is_empty())
}

fn opencode_bin() -> String {
    std::env::var("OPENCODE_BIN").unwrap_or_else(|_| {
        if let Some(home) = std::env::var_os("HOME") {
            let candidate = PathBuf::from(&home).join(".opencode/bin/opencode");
            if candidate.exists() {
                return candidate.to_string_lossy().into_owned();
            }
        }
        "opencode".to_string()
    })
}

/// Resolve an API key for the RC gateway: env first, then the key opencode
/// stored in its auth.json, so a GUI-launched process still works.
fn opencode_api_key() -> Option<String> {
    if let Ok(k) = std::env::var("OPENCODE_API_KEY") {
        if !k.is_empty() {
            return Some(k);
        }
    }
    let home = std::env::var_os("HOME")?;
    let auth_path = PathBuf::from(&home).join(".local/share/opencode/auth.json");
    let text = std::fs::read_to_string(auth_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("RC-AI-Gateway")
        .and_then(|o| o.get("key"))
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
}

/// Send an event to the UI, echoing to stderr when KUMU_DEBUG is set (handy
/// for headless verification).
fn emit(tx: &Sender<Event>, ev: Event) {
    if std::env::var("KUMU_DEBUG").is_ok() {
        match &ev {
            Event::Agent(t) => eprintln!("[acp] agent: {t}"),
            Event::Thought(t) => eprintln!("[acp] thought: {t}"),
            Event::ToolCall { id, title, status } => {
                eprintln!("[acp] tool {id}: {title} ({status})")
            }
            Event::SessionTitle { title } => eprintln!("[acp] title: {title}"),
            Event::Done(s) => eprintln!("[acp] done: {s}"),
            Event::Error(s) => eprintln!("[acp] error: {s}"),
            Event::Log(s) => eprintln!("[acp] log: {s}"),
            Event::Permission(s) => eprintln!("[acp] perm: {s}"),
            Event::Models {
                agent,
                models,
                current,
            } => eprintln!(
                "[acp] models: {agent} exposes {} models (active: {:?}) across {} providers",
                models.len(),
                current,
                models
                    .iter()
                    .map(|m| m.provider.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .len()
            ),
            Event::ProbeFailed { agent } => eprintln!("[acp] probe {agent} failed"),
        }
    }
    let _ = tx.send(ev);
}

/// Spawn the ACP thread and return the UI-side command sender.
pub fn spawn(
    cfg: Config,
    cwd: PathBuf,
    ev_tx: Sender<Event>,
) -> tokio::sync::mpsc::UnboundedSender<Cmd> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
    std::thread::Builder::new()
        .name("kumu-acp".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            if let Err(err) = rt.block_on(run(cfg, cwd, ev_tx.clone(), cmd_rx)) {
                emit(&ev_tx, Event::Error(err.to_string()));
            }
        })
        .expect("failed to spawn acp thread");
    cmd_tx
}

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

/// Extract the session's model config option: its id (for
/// `session/set_config_option`), the value the agent reports as active, and
/// the selectable models, grouped flat.
fn model_catalog(
    config_options: &Option<Vec<SessionConfigOption>>,
) -> (Option<SessionConfigId>, Option<String>, Vec<ModelInfo>) {
    let mut option_id = None;
    let mut current = None;
    let mut models: Vec<ModelInfo> = Vec::new();
    if let Some(options) = config_options {
        for option in options {
            let is_model = matches!(&option.category, Some(SessionConfigOptionCategory::Model))
                || option.id.0.as_ref() == "model";
            if !is_model {
                continue;
            }
            if let SessionConfigKind::Select(select) = &option.kind {
                option_id = Some(option.id.clone());
                current = Some(select.current_value.0.as_ref().to_string());
                let mut add = |value: &SessionConfigSelectOption| {
                    let raw = value.value.0.as_ref();
                    let provider = raw.split('/').next().unwrap_or(raw).to_string();
                    models.push(ModelInfo {
                        value: raw.to_string(),
                        name: strip_provider_prefix(&value.name, &provider),
                        provider,
                    });
                };
                match &select.options {
                    SessionConfigSelectOptions::Ungrouped(values) => {
                        for v in values {
                            add(v);
                        }
                    }
                    SessionConfigSelectOptions::Grouped(groups) => {
                        for group in groups {
                            for v in &group.options {
                                add(v);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    models.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.name.cmp(&b.name)));
    (option_id, current, models)
}

/// Drop the provider prefix from a display name ("OpenCode Go/GLM-5.3" →
/// "GLM-5.3"): agents like opencode concatenate provider + model in the name,
/// which duplicates the provider column in the picker. The prefix only counts
/// as a provider when it normalizes to the same key as the value's provider
/// segment; otherwise the name is kept untouched.
fn strip_provider_prefix(name: &str, provider: &str) -> String {
    let key = |s: &str| {
        s.to_lowercase()
            .chars()
            .filter(|c: &char| c.is_alphanumeric())
            .collect::<String>()
    };
    match name.split_once('/') {
        Some((prefix, rest)) if !rest.is_empty() && key(prefix) == key(provider) => {
            rest.to_string()
        }
        _ => name.to_string(),
    }
}

/// Stable lowercase names for tool call statuses (used by the UI rows).
fn status_name(status: &ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "running",
        ToolCallStatus::Completed => "done",
        ToolCallStatus::Failed => "failed",
        _ => "pending",
    }
}

async fn run(
    cfg: Config,
    cwd: PathBuf,
    ev_tx: Sender<Event>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
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
                        emit(
                            &notif_tx,
                            Event::ToolCall {
                                id: call.tool_call_id.0.to_string(),
                                title: call.title,
                                status: status_name(&call.status),
                            },
                        );
                    }
                    SessionUpdate::ToolCallUpdate(update) => {
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
                // Demo policy: auto-approve the first "allow" option.
                let chosen = request
                    .options
                    .iter()
                    .find(|o| {
                        matches!(
                            o.kind,
                            PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                        )
                    })
                    .or_else(|| request.options.first())
                    .cloned();
                match chosen {
                    Some(option) => {
                        emit(
                            &perm_tx,
                            Event::Permission(format!(
                                "auto-approved: {} ({})",
                                option.option_id, option.name
                            )),
                        );
                        responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                option.option_id,
                            )),
                        ))
                    }
                    None => {
                        emit(&perm_tx, Event::Permission("no options, cancelling".into()));
                        responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ))
                    }
                }
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

                // Read the model config option: its id lets the UI switch the
                // session's model later; the catalog feeds the model picker.
                let (model_option_id, current, models) = model_catalog(&new_session.config_options);
                emit(
                    &ev_tx,
                    Event::Models {
                        agent: cfg.agent,
                        models,
                        current,
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
                    }
                }
                emit(&log_tx, Event::Log("connection closing".into()));
                Ok(())
            },
        )
        .await
}

/// Name an update variant for logging.
fn variant_name(u: &SessionUpdate) -> &'static str {
    match u {
        SessionUpdate::UserMessageChunk(_) => "user_message_chunk",
        SessionUpdate::AgentMessageChunk(_) => "agent_message_chunk",
        SessionUpdate::AgentThoughtChunk(_) => "agent_thought_chunk",
        SessionUpdate::ToolCall(_) => "tool_call",
        SessionUpdate::ToolCallUpdate(_) => "tool_call_update",
        SessionUpdate::Plan(_) => "plan",
        SessionUpdate::AvailableCommandsUpdate(_) => "available_commands_update",
        SessionUpdate::CurrentModeUpdate(_) => "current_mode_update",
        SessionUpdate::ConfigOptionUpdate(_) => "config_option_update",
        SessionUpdate::SessionInfoUpdate(_) => "session_info_update",
        SessionUpdate::UsageUpdate(_) => "usage_update",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `command` resolves to an existing executable (a path, or on
    /// PATH).
    fn command_exists(command: &str) -> bool {
        if command.contains('/') {
            return std::path::Path::new(command).is_file();
        }
        std::env::var_os("PATH")
            .map(|paths| {
                std::env::split_paths(&paths)
                    .map(|dir| dir.join(command))
                    .any(|candidate| candidate.is_file())
            })
            .unwrap_or(false)
    }

    /// The real-agent tests share one local agent binary; running its
    /// subprocesses concurrently is slow enough to trip the timeouts, so
    /// serialize them.
    static AGENT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
                Event::Models { agent, models, current } => {
                    println!("{agent}: {} models, active: {current:?}", models.len());
                    assert!(!models.is_empty());
                    break;
                }
                Event::ProbeFailed { agent } => panic!("probe failed: {agent}"),
                _ => {}
            }
        }
    }

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
        let cmd_tx = spawn(
            cfg,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            tx,
        );

        // Wait for the model catalog, then pick its first model before
        // prompting — exercises session/set_config_option on the live path.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining).expect("catalog timed out") {
                Event::Models { models, .. } => {
                    if let Some(model) = models.first() {
                        cmd_tx.send(Cmd::SetModel(model.value.clone())).unwrap();
                    }
                    break;
                }
                Event::Error(err) => panic!("acp error: {err}"),
                _ => {}
            }
        }
        cmd_tx
            .send(Cmd::Prompt("Reply with exactly: ok".into()))
            .unwrap();

        let mut text = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining).expect("prompt timed out") {
                Event::Agent(chunk) => text.push_str(&chunk),
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
