//! The live connection: a private server, the session on it, the event pump,
//! and the command loop.
//!
//! One `run` drives exactly one kumu session, like every other backend: its own
//! `opencode serve`, one opencode session on it, and a serial command loop. A
//! prompt is only an enqueue on this API, so a turn is followed on the event
//! stream until it settles — the pump hands the settle to whichever prompt is
//! waiting, which is what keeps the loop serial.

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt as _;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::convert::{self, Catalog, PermissionAsk, Settle, Stream};
use super::transport::{Server, debug_log};
use crate::agent::config::Config;
use crate::agent::connection::WeakPermissions;
use crate::agent::event::{Cmd, CommandInfo, Event, Prompt, emit};
use crate::agent::session::{
    CancelRequests, PermissionQuestion, ask_user, forget_stale_cancels,
    release_pending_permissions, settle,
};

/// How long a fresh server may take to have its model catalogue ready: the
/// first read can answer an empty list while it is still loading.
const CATALOGUE_WAIT: Duration = Duration::from_secs(10);

/// State the command loop and the event pump share.
struct Shared {
    agent: &'static str,
    server: Server,
    session_id: String,
    ev_tx: Sender<Event>,
    permissions: WeakPermissions,
    /// Distinguishes permission requests on this connection for the UI.
    next_request_id: AtomicU64,
    stream: Mutex<Stream>,
    catalog: Mutex<Catalog>,
    commands: Vec<CommandInfo>,
    /// The prompt waiting for its turn to settle, if one is.
    turn: Mutex<Option<oneshot::Sender<Settle>>>,
}

impl Shared {
    fn path(&self, tail: &str) -> String {
        format!("/api/session/{}{tail}", self.session_id)
    }

    fn emit_options(&self) {
        let event = self.catalog.lock().unwrap().options(self.agent);
        emit(&self.ev_tx, event);
    }
}

/// Drive one opencode session to completion. Called by the shared [`spawn`]
/// dispatcher.
///
/// [`spawn`]: crate::agent::spawn
pub(in crate::agent) async fn run(
    cfg: Config,
    cwd: PathBuf,
    ev_tx: Sender<Event>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    mut cancels: CancelRequests,
    permissions: WeakPermissions,
) -> Result<(), String> {
    let server = Server::start(&cfg.command, &cfg.env, &cwd).await?;
    // Subscribed before the session exists: the stream carries no history, so
    // anything it said before this would be lost.
    let mut events = server.events().await?;

    // The catalogue and the session do not depend on each other, and each is a
    // round trip to a server that has only just started.
    let open = async {
        match &cfg.session.resume_id {
            // The id is the one opencode gave the session, over this API or
            // over ACP before it — the two share their sessions.
            Some(id) => server
                .get(&format!("/api/session/{id}"))
                .await
                .map_err(|error| format!("恢复 agent 会话失败，未开启新会话：{error}")),
            None => Ok(server
                .post("/api/session", json!({}))
                .await?
                .unwrap_or_default()),
        }
    };
    let (loaded, session) = tokio::join!(load_catalog(&server), open);
    let (mut catalog, commands) = loaded?;
    let session = session?;
    let info = session.get("data").unwrap_or(&Value::Null);
    let session_id = info
        .get("id")
        .and_then(Value::as_str)
        .ok_or("opencode 没有返回会话 id")?
        .to_string();
    // A session that has run has its own model and agent; a fresh one keeps
    // the location's defaults the catalogue started from.
    if let Some((model, variant)) = info.get("model").and_then(convert::model_ref) {
        catalog.model = Some(model);
        catalog.variant = variant;
    }
    if let Some(agent) = info.get("agent").and_then(Value::as_str) {
        catalog.mode = Some(agent.to_string());
    }
    let stream = Stream::new(catalog.window());

    emit(
        &ev_tx,
        Event::SessionReady {
            resume_id: session_id.clone(),
            // A session lives in opencode's store, not in this process, so any
            // later server reopens it by id.
            resumable: true,
        },
    );
    emit(
        &ev_tx,
        Event::Commands {
            agent: cfg.agent,
            commands: commands.clone(),
        },
    );

    let shared = Arc::new(Shared {
        agent: cfg.agent,
        server,
        session_id,
        ev_tx: ev_tx.clone(),
        permissions,
        next_request_id: AtomicU64::new(1),
        stream: Mutex::new(stream),
        catalog: Mutex::new(catalog),
        commands,
        turn: Mutex::new(None),
    });
    shared.emit_options();

    let pump = {
        let shared = shared.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                on_event(&shared, &event);
            }
            // Nothing will settle a waiting turn now; dropping its sender is
            // what tells it so.
            shared.turn.lock().unwrap().take();
        })
    };
    let mut pump = pump.fuse();

    // Headless convenience, matching the other backends.
    if let Some(auto) = cfg.auto_prompt.clone() {
        emit(&ev_tx, Event::Log(format!("auto prompt: {auto}")));
        prompt(&shared, &auto.into(), &mut cancels).await;
    }

    loop {
        // The stream closing means the server is gone: parking on the command
        // channel alone would leave the UI prompting a dead agent.
        let command = futures::select! {
            command = cmd_rx.recv().fuse() => command,
            _ = pump => {
                emit(&ev_tx, Event::Error("opencode 服务已退出".to_string()));
                break;
            }
        };
        match command {
            Some(Cmd::Prompt(request)) => prompt(&shared, &request, &mut cancels).await,
            Some(Cmd::SetModel(value)) => switch_model(&shared, Some(&value), None).await,
            Some(Cmd::SetReasoning(value)) => switch_model(&shared, None, Some(&value)).await,
            Some(Cmd::SetMode(value)) => set_mode(&shared, &value).await,
            // opencode's permission rules are its own configuration; kumu offers
            // no sandbox axis for it, so a stored one is a leftover.
            Some(Cmd::SetSandbox(value)) => {
                emit(
                    &ev_tx,
                    Event::Log(format!("opencode 没有 sandbox 可设置：{value}")),
                );
            }
            None => break,
        }
    }

    release_pending_permissions(&shared.permissions);
    emit(&ev_tx, Event::Log("connection closing".into()));
    Ok(())
}

/// What a session here starts from: the location's models, modes and
/// commands, with its default model and the first mode in force.
///
/// The reads are independent, so they run together. The model list is read
/// until it is non-empty, for a while: a fresh server loads it lazily,
/// answering the first read with nothing (see the schema README).
pub(super) async fn load_catalog(server: &Server) -> Result<(Catalog, Vec<CommandInfo>), String> {
    let models = async {
        let deadline = tokio::time::Instant::now() + CATALOGUE_WAIT;
        loop {
            let models = convert::model_entries(&server.get("/api/model").await?);
            if !models.is_empty() || tokio::time::Instant::now() >= deadline {
                break Ok::<_, String>(models);
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    };
    let (models, agents, commands, default) = tokio::join!(
        models,
        server.get("/api/agent"),
        server.get("/api/command"),
        server.get("/api/model/default"),
    );
    let modes = convert::mode_catalog(&agents.unwrap_or_default());
    let catalog = Catalog {
        models: models?,
        model: default
            .unwrap_or_default()
            .get("data")
            .and_then(convert::model_ref)
            .map(|(model, _)| model),
        mode: modes.first().map(|mode| mode.value.clone()),
        modes,
        variant: None,
    };
    Ok((
        catalog,
        convert::command_catalog(&commands.unwrap_or_default()),
    ))
}

/// One event from the stream: translated, shown, and — for a permission or a
/// settle — routed to whoever has to act on it.
fn on_event(shared: &Arc<Shared>, event: &Value) {
    let translated = shared
        .stream
        .lock()
        .unwrap()
        .translate(event, &shared.session_id);
    for event in translated.events {
        emit(&shared.ev_tx, event);
    }
    if let Some(ask) = translated.permission {
        on_permission(shared, ask);
    }
    if let Some(outcome) = translated.settle {
        match shared.turn.lock().unwrap().take() {
            Some(waiter) => {
                let _ = waiter.send(outcome);
            }
            // A turn nobody is waiting on — one the agent ran on its own.
            None => debug_log(&format!("unclaimed settle: {outcome:?}")),
        }
    }
}

/// Ask the user, and reply with their answer. No answer — the UI gone, or the
/// question dropped — is a rejection: nothing runs without a yes.
fn on_permission(shared: &Arc<Shared>, ask: PermissionAsk) {
    let reply = {
        let shared = shared.clone();
        // To the session that asked, which may be a subagent's.
        let path = format!(
            "/api/session/{}/permission/{}/reply",
            ask.session_id, ask.id
        );
        move |decision: Option<String>| {
            let decision = decision.unwrap_or_else(|| "reject".to_string());
            tokio::spawn(async move {
                if let Err(error) = shared
                    .server
                    .post(&path, json!({"decision": decision}))
                    .await
                {
                    emit(&shared.ev_tx, Event::Log(format!("回复授权失败：{error}")));
                }
            });
        }
    };
    let Some(table) = shared.permissions.upgrade() else {
        reply(None);
        return;
    };
    ask_user(
        table,
        &shared.next_request_id,
        &shared.ev_tx,
        PermissionQuestion {
            tool_call_id: ask.tool_call_id,
        },
        reply,
    );
}

/// Send a prompt (or a command) and follow it to the end of its turn.
async fn prompt(shared: &Arc<Shared>, request: &Prompt, cancels: &mut CancelRequests) {
    forget_stale_cancels(cancels);
    // Armed before sending, so a turn that settles at once is not missed.
    let (tx, mut rx) = oneshot::channel();
    *shared.turn.lock().unwrap() = Some(tx);

    let sent = match convert::command_call(&request.text, &shared.commands) {
        Some((name, arguments)) => {
            let mut body = convert::prompt_body(arguments, &request.images);
            body["name"] = json!(name);
            shared.server.post(&shared.path("/command"), body).await
        }
        None => {
            let body = convert::prompt_body(&request.text, &request.images);
            shared.server.post(&shared.path("/prompt"), body).await
        }
    };
    if let Err(error) = sent {
        // Refused before it was queued: no turn opened, and nothing will settle.
        shared.turn.lock().unwrap().take();
        for event in settle("error", Some(error)) {
            emit(&shared.ev_tx, event);
        }
        return;
    }

    let mut asked = false;
    let outcome = loop {
        tokio::select! {
            outcome = &mut rx => break outcome,
            // Watched for here, not on the command channel: this loop is parked
            // on the very turn a cancel is about.
            _ = cancels.recv(), if !asked => {
                asked = true;
                emit(&shared.ev_tx, Event::Log("cancel requested".into()));
                if let Err(error) = shared.server.post(&shared.path("/interrupt"), json!({})).await {
                    emit(&shared.ev_tx, Event::Log(format!("interrupt 失败：{error}")));
                }
            }
        }
    };
    let (stop, failure) = match outcome {
        Ok(outcome) => outcome.outcome(),
        Err(_) => ("error", Some("opencode 事件流中断".to_string())),
    };
    for event in settle(stop, failure) {
        emit(&shared.ev_tx, event);
    }
}

/// Switch the session's model or its reasoning level — the two halves of one
/// `Model.Ref` — keeping the other half as it is. A level the model does not
/// offer is dropped, so the model's own default applies.
async fn switch_model(shared: &Arc<Shared>, model: Option<&str>, variant: Option<&str>) {
    let (model, reference) = {
        let catalog = shared.catalog.lock().unwrap();
        let Some(model) = model.or(catalog.model.as_deref()).map(str::to_string) else {
            return;
        };
        let variant = variant.or(catalog.variant.as_deref());
        if catalog.model.as_deref() == Some(model.as_str()) && catalog.variant.as_deref() == variant
        {
            return;
        }
        let reference = catalog.model_ref(&model, variant);
        (model, reference)
    };
    let Some(reference) = reference else {
        // A pick saved from a catalogue this location no longer offers (the ACP
        // backend listed more): nothing to switch to, and the session keeps the
        // model it has.
        emit(
            &shared.ev_tx,
            Event::Log(format!("opencode 没有提供这个模型：{model}")),
        );
        return;
    };
    match shared
        .server
        .post(&shared.path("/model"), json!({"model": reference}))
        .await
    {
        Ok(_) => {
            let window = {
                let mut catalog = shared.catalog.lock().unwrap();
                catalog.model = Some(model.clone());
                catalog.variant = reference
                    .get("variant")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                catalog.window()
            };
            shared.stream.lock().unwrap().set_window(window);
            emit(&shared.ev_tx, Event::Log(format!("model set: {model}")));
            shared.emit_options();
        }
        // Not an `Error`: that would end the turn a prompt queued behind this
        // pick is about to run. Re-sending the options puts the catalogue back on
        // the model the session is still running.
        Err(error) => {
            emit(&shared.ev_tx, Event::Log(format!("切换模型失败：{error}")));
            shared.emit_options();
        }
    }
}

async fn set_mode(shared: &Arc<Shared>, value: &str) {
    if shared.catalog.lock().unwrap().mode.as_deref() == Some(value) {
        return;
    }
    match shared
        .server
        .post(&shared.path("/agent"), json!({"agent": value}))
        .await
    {
        Ok(_) => {
            shared.catalog.lock().unwrap().mode = Some(value.to_string());
            emit(&shared.ev_tx, Event::Log(format!("mode set: {value}")));
            shared.emit_options();
        }
        // As for a model: logged, and the catalogue put back.
        Err(error) => {
            emit(&shared.ev_tx, Event::Log(format!("切换模式失败：{error}")));
            shared.emit_options();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::agent::test_support::{AGENT_TEST_LOCK, command_exists};
    use crate::agent::{Cmd, Config, Event, spawn};

    /// End-to-end: a real `opencode serve`, one prompt, the reply streamed and
    /// the turn settled — then the same session reopened by its id. Ignored by
    /// default, since it spends a model call; skips itself without opencode.
    #[test]
    #[ignore]
    fn session_prompts_opencode_end_to_end() {
        let _guard = AGENT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = Config::for_agent("opencode").expect("known agent");
        if !command_exists(&cfg.command) {
            println!(
                "skipping session_prompts_opencode_end_to_end: {} not found",
                cfg.command
            );
            return;
        }
        cfg.auto_prompt = None;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let (tx, rx) = std::sync::mpsc::channel();
        let conn = spawn(cfg, cwd.clone(), tx);
        conn.send(Cmd::Prompt(
            "Reply with exactly: ok. Do not use any tools.".into(),
        ));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        let (mut reply, mut resume_id, mut models) = (String::new(), None, 0);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining).expect("turn timed out") {
                Event::SessionReady {
                    resume_id: id,
                    resumable,
                } => {
                    assert!(resumable);
                    resume_id = Some(id);
                }
                Event::SessionOptions { models: list, .. } => models = list.len(),
                Event::Agent(chunk) => reply.push_str(&chunk),
                Event::Done(stop) => {
                    assert_eq!(stop, "completed");
                    break;
                }
                Event::Error(error) => panic!("turn failed: {error}"),
                _ => {}
            }
        }
        assert!(models > 0, "no models were offered");
        assert!(reply.to_lowercase().contains("ok"), "reply: {reply:?}");
        drop(conn);

        // Reopen the same session in a fresh server.
        let mut cfg =
            Config::for_agent("opencode")
                .unwrap()
                .with_session(crate::agent::SessionPicks {
                    resume_id: resume_id.clone(),
                    ..Default::default()
                });
        cfg.auto_prompt = None;
        let (tx, rx) = std::sync::mpsc::channel();
        let _conn = spawn(cfg, cwd, tx);
        loop {
            match rx
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("resume timed out")
            {
                Event::SessionReady { resume_id: id, .. } => {
                    assert_eq!(Some(id), resume_id);
                    break;
                }
                Event::Error(error) => panic!("resume failed: {error}"),
                _ => {}
            }
        }
    }
}
