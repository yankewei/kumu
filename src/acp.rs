//! ACP integration: talks v1 ACP over stdio to a local agent process (`opencode acp`).
//!
//! Runs on a dedicated tokio runtime thread. UI -> here via an unbounded tokio
//! channel (send is non-blocking, safe from the gpui thread); here -> UI via a
//! std mpsc channel that the gpui side polls.
//!
//! Permission requests are the one thing that travels back on a different path:
//! they are answered through [`Connection::resolve_permission`], not the command
//! channel. See [`SharedPermissions`] for why.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};

use agent_client_protocol::schema::v1::SessionUpdate;
use agent_client_protocol::schema::v1::{
    ContentBlock, Diff, InitializeRequest, NewSessionRequest, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigId, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption, SessionConfigSelectOptions,
    SessionNotification, SetSessionConfigOptionRequest, TextContent, ToolCallContent,
    ToolCallStatus, ToolCallUpdate,
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
    /// `content` is the flattened text output (empty when none).
    ToolCall {
        id: String,
        title: String,
        status: &'static str,
        content: String,
    },
    /// The agent named the session (session/info update with a title).
    SessionTitle { title: String },
    /// The prompt turn ended with this stop reason.
    Done(String),
    /// The agent is asking for authorization before running a tool call. The UI
    /// shows the options on the matching tool row and answers back via
    /// [`Connection::resolve_permission`].
    PermissionRequest {
        /// Tool call this request belongs to, so the UI can attach the prompt
        /// to the row it already rendered.
        tool_call_id: String,
        /// Stable id used by the UI to answer this request.
        request_id: String,
        /// The agent's description of what it wants to do.
        title: String,
        /// What the agent is proposing, taken from the tool call's content
        /// (text or file diff) and falling back to its `raw_input`; may be
        /// empty when the agent describes nothing.
        detail: String,
        /// The options the agent offers, passed through verbatim.
        options: Vec<PermissionOptionInfo>,
    },
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

/// What a permission option means, used to pick the UI treatment. The label is
/// always the agent's own `name`; this only decides the button styling.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PermissionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    /// A kind this build doesn't know about (`PermissionOptionKind` is
    /// `#[non_exhaustive]`); rendered neutrally rather than as a rejection.
    Other,
}

/// One permission option offered by the agent, passed through verbatim.
#[derive(Clone, Debug)]
pub struct PermissionOptionInfo {
    /// Value to echo back in the response.
    pub id: String,
    /// Label to display.
    pub name: String,
    /// Semantic classification for styling.
    pub kind: PermissionKind,
}

/// UI -> ACP commands on the channel returned by [`spawn`].
pub enum Cmd {
    Prompt(String),
    /// Select the session's model by its config-option value.
    SetModel(String),
}

/// The table of pending permission requests awaiting a user decision, keyed by
/// request id.
///
/// The senders are handed to the UI so it can answer without going through
/// [`Cmd`]: the ACP thread is parked on the prompt that is waiting for this very
/// answer, so routing the reply through the command channel would deadlock.
///
/// Only [`Connection`] holds this table strongly; the ACP thread keeps a
/// [`WeakPermissions`]. That way dropping the last UI-side handle frees the
/// table, which drops every pending sender and makes the waiting tasks answer
/// `Cancelled` — the only thing that can unblock a prompt whose agent is sitting
/// on a permission request.
type PermissionTable = Mutex<HashMap<String, tokio::sync::oneshot::Sender<Option<String>>>>;
type SharedPermissions = Arc<PermissionTable>;
type WeakPermissions = Weak<PermissionTable>;

/// Distinguishes connections so late events and teardown from a replaced one
/// cannot touch the session's current connection.
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

/// The UI-side handle to one live ACP connection: send commands, and answer
/// permission requests.
///
/// Dropping the last clone tears the connection down: the command channel
/// closes and any outstanding permission prompt is abandoned.
#[derive(Clone)]
pub struct Connection {
    /// Stable per-connection identity, so the UI can tell whether a bridge task
    /// still belongs to the session's current connection.
    id: u64,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
    permissions: SharedPermissions,
}

impl Connection {
    /// This connection's identity, compared against the one registered for a
    /// session to discard stale events and teardown.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Queue a command; non-blocking.
    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Answer a pending permission request. `option_id` of `None` responds with
    /// the protocol's `Cancelled` outcome. A no-op for unknown or already
    /// answered requests.
    pub fn resolve_permission(&self, request_id: &str, option_id: Option<String>) {
        if let Some(tx) = self.permissions.lock().unwrap().remove(request_id) {
            // The waiting task may already be gone; the reply is then moot.
            let _ = tx.send(option_id);
        }
    }

    /// A handle attached to no thread, for tests that only exercise identity.
    #[cfg(test)]
    pub(crate) fn detached_for_test(id: u64) -> Self {
        Self {
            id,
            cmd_tx: tokio::sync::mpsc::unbounded_channel().0,
            permissions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
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
            Event::ToolCall {
                id,
                title,
                status,
                content,
            } => {
                eprintln!(
                    "[acp] tool {id}: {title} ({status}) content={} chars",
                    content.len()
                )
            }
            Event::SessionTitle { title } => eprintln!("[acp] title: {title}"),
            Event::Done(s) => eprintln!("[acp] done: {s}"),
            Event::PermissionRequest {
                tool_call_id,
                request_id,
                options,
                ..
            } => eprintln!(
                "[acp] permission {request_id} for tool {tool_call_id}: {} options",
                options.len()
            ),
            Event::Error(s) => eprintln!("[acp] error: {s}"),
            Event::Log(s) => eprintln!("[acp] log: {s}"),
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

/// Spawn the ACP thread and return the UI-side connection handle.
pub fn spawn(cfg: Config, cwd: PathBuf, ev_tx: Sender<Event>) -> Connection {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
    // Shared so the UI can answer permission requests without the command queue.
    // The thread only borrows it, so dropping the handle can still free it.
    let permissions: SharedPermissions = Arc::new(Mutex::new(HashMap::new()));
    let thread_permissions = Arc::downgrade(&permissions);
    std::thread::Builder::new()
        .name("kumu-acp".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            if let Err(err) = rt.block_on(run(cfg, cwd, ev_tx.clone(), cmd_rx, thread_permissions))
            {
                emit(&ev_tx, Event::Error(err.to_string()));
            }
        })
        .expect("failed to spawn acp thread");
    Connection {
        id: NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed),
        cmd_tx,
        permissions,
    }
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

/// Classify a permission option for the UI. Unknown kinds (the enum is
/// `#[non_exhaustive]`) map to [`PermissionKind::Other`] so they render
/// neutrally instead of being mislabelled as a rejection.
fn permission_kind(kind: &PermissionOptionKind) -> PermissionKind {
    match kind {
        PermissionOptionKind::AllowOnce => PermissionKind::AllowOnce,
        PermissionOptionKind::AllowAlways => PermissionKind::AllowAlways,
        PermissionOptionKind::RejectOnce => PermissionKind::RejectOnce,
        PermissionOptionKind::RejectAlways => PermissionKind::RejectAlways,
        _ => PermissionKind::Other,
    }
}

/// Turn the user's choice into the protocol outcome. `None` means the turn
/// ended (or was abandoned) before the user decided, which the spec requires
/// clients to answer with `Cancelled`.
fn permission_outcome(option_id: Option<String>) -> RequestPermissionOutcome {
    match option_id {
        Some(id) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
        None => RequestPermissionOutcome::Cancelled,
    }
}

/// The detail shown in a permission prompt, so the user can inspect what they
/// are approving.
///
/// Agents describe the pending operation either as content blocks (text, file
/// diffs) or as `raw_input`; neither is guaranteed, and either may be the only
/// one present. Content wins when available because it is what the agent chose
/// to show about the change itself.
fn permission_detail(tool_call: &ToolCallUpdate) -> String {
    let mut parts = Vec::new();
    if let Some(content) = &tool_call.fields.content {
        for block in content {
            match block {
                ToolCallContent::Content(c) => {
                    if let ContentBlock::Text(t) = &c.content {
                        parts.push(t.text.clone());
                    }
                }
                ToolCallContent::Diff(diff) => parts.push(render_diff(diff)),
                // A terminal id is not something a prompt can show inline.
                _ => {}
            }
        }
    }
    if !parts.is_empty() {
        return parts.join("\n");
    }
    tool_call
        .fields
        .raw_input
        .as_ref()
        .and_then(|v| serde_json::to_string_pretty(v).ok())
        .unwrap_or_default()
}

/// Render a file modification as +/- lines for review before approval.
fn render_diff(diff: &Diff) -> String {
    let mut out = format!("{}\n", diff.path.display());
    if let Some(old) = &diff.old_text {
        for line in old.lines() {
            out.push_str(&format!("- {line}\n"));
        }
    }
    for line in diff.new_text.lines() {
        out.push_str(&format!("+ {line}\n"));
    }
    out.trim_end().to_string()
}

/// Flatten a tool call's content blocks into one text string, falling back to
/// the pretty-printed raw output when there are no text blocks.
fn tool_call_content(
    content: &[ToolCallContent],
    raw_output: &Option<serde_json::Value>,
) -> String {
    let mut parts = Vec::new();
    for c in content {
        if let ToolCallContent::Content(c) = c {
            if let ContentBlock::Text(t) = &c.content {
                parts.push(t.text.clone());
            }
        }
    }
    if !parts.is_empty() {
        return parts.join("\n");
    }
    raw_output
        .as_ref()
        .and_then(|v| serde_json::to_string_pretty(v).ok())
        .unwrap_or_default()
}

async fn run(
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
    use agent_client_protocol::schema::v1::ToolCallUpdateFields;

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

        // Wait for the model catalog, then pick its first model before
        // prompting — exercises session/set_config_option on the live path.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining).expect("catalog timed out") {
                Event::Models { models, .. } => {
                    if let Some(model) = models.first() {
                        conn.send(Cmd::SetModel(model.value.clone()));
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

    /// Every known option kind maps to itself; the enum is `#[non_exhaustive]`
    /// so anything else must fall back to the neutral variant rather than being
    /// presented to the user as a rejection.
    #[test]
    fn permission_kinds_map_to_ui_kinds() {
        assert_eq!(
            permission_kind(&PermissionOptionKind::AllowOnce),
            PermissionKind::AllowOnce
        );
        assert_eq!(
            permission_kind(&PermissionOptionKind::AllowAlways),
            PermissionKind::AllowAlways
        );
        assert_eq!(
            permission_kind(&PermissionOptionKind::RejectOnce),
            PermissionKind::RejectOnce
        );
        assert_eq!(
            permission_kind(&PermissionOptionKind::RejectAlways),
            PermissionKind::RejectAlways
        );
    }

    /// A pick becomes a `Selected` outcome; no pick must become `Cancelled`,
    /// which the spec requires once the turn is over.
    #[test]
    fn permission_outcome_reflects_the_choice() {
        match permission_outcome(Some("allow-once".into())) {
            RequestPermissionOutcome::Selected(selected) => {
                assert_eq!(selected.option_id.0.as_ref(), "allow-once");
            }
            other => panic!("expected Selected, got {other:?}"),
        }
        assert_eq!(
            permission_outcome(None),
            RequestPermissionOutcome::Cancelled
        );
    }

    /// The UI answers through this handle, so it must deliver to the waiting
    /// task and be a no-op for ids that are unknown or already answered.
    #[test]
    fn resolve_permission_delivers_once() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
        let permissions: SharedPermissions = Arc::new(Mutex::new(HashMap::new()));
        let conn = Connection {
            id: 1,
            cmd_tx,
            permissions: permissions.clone(),
        };

        // Unknown id: nothing to send to, and it must not panic.
        conn.resolve_permission("perm-missing", Some("allow-once".into()));

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        permissions.lock().unwrap().insert("perm-1".into(), tx);
        conn.resolve_permission("perm-1", Some("allow-once".into()));
        assert_eq!(
            rx.try_recv().expect("decision not delivered"),
            Some("allow-once".to_string())
        );

        // Answering the same request again is a no-op rather than a second send.
        conn.resolve_permission("perm-1", None);
        assert!(rx.try_recv().is_err());
    }

    /// Dropping the last UI handle must free the table, which drops the pending
    /// senders and so unblocks a task waiting on a user decision. This is what
    /// keeps a prompt from deadlocking the connection when the window closes.
    #[test]
    fn dropping_connection_releases_pending_decisions() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
        let permissions: SharedPermissions = Arc::new(Mutex::new(HashMap::new()));
        // The ACP thread only borrows the table.
        let thread_view = Arc::downgrade(&permissions);

        let conn = Connection {
            id: 1,
            cmd_tx,
            permissions: permissions.clone(),
        };
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        conn.permissions.lock().unwrap().insert("perm-1".into(), tx);

        // From here the handle is the only strong owner, as in the real app.
        drop(permissions);
        assert!(thread_view.upgrade().is_some());

        drop(conn);
        // The waiting task observes the sender being dropped...
        assert!(
            rx.try_recv().is_err(),
            "sender should be gone once the handle is dropped"
        );
        // ...and the thread stops holding the table at all.
        assert!(
            thread_view.upgrade().is_none(),
            "the table must be freed once the last handle is dropped"
        );
    }

    /// A diff carried in the tool call's content, with no `raw_input`, must
    /// still describe what the user is approving.
    #[test]
    fn permission_detail_renders_content_diff() {
        let tool_call = ToolCallUpdate::new(
            "call-1",
            ToolCallUpdateFields::new().content(vec![ToolCallContent::Diff(
                Diff::new("/tmp/notes.txt", "new line\n").old_text("old line\n"),
            )]),
        );
        let detail = permission_detail(&tool_call);
        assert!(detail.contains("/tmp/notes.txt"), "missing path: {detail}");
        assert!(detail.contains("- old line"), "missing removal: {detail}");
        assert!(detail.contains("+ new line"), "missing addition: {detail}");
    }

    /// Falls back to the raw input when the agent sends no content, and is
    /// empty when neither is present.
    #[test]
    fn permission_detail_falls_back_to_raw_input() {
        let with_input = ToolCallUpdate::new(
            "call-1",
            ToolCallUpdateFields::new().raw_input(serde_json::json!({ "path": "/tmp/a" })),
        );
        assert!(permission_detail(&with_input).contains("/tmp/a"));

        let empty = ToolCallUpdate::new("call-1", ToolCallUpdateFields::new());
        assert_eq!(permission_detail(&empty), "");
    }

    /// Text content is shown even when `raw_input` is also present, since the
    /// agent chose it as the human-readable description.
    #[test]
    fn permission_detail_prefers_content_text() {
        let tool_call = ToolCallUpdate::new(
            "call-1",
            ToolCallUpdateFields::new()
                .content(vec![ToolCallContent::from(ContentBlock::Text(
                    TextContent::new("Overwrite the config"),
                ))])
                .raw_input(serde_json::json!({ "path": "/tmp/a" })),
        );
        let detail = permission_detail(&tool_call);
        assert!(detail.contains("Overwrite the config"), "got: {detail}");
        assert!(!detail.contains("/tmp/a"), "got: {detail}");
    }
}
