//! Hook notification CLI entrypoint (`--kumu-notify`) and OSC 777 sequence builders.

use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::non_empty_env_path;
use crate::agent::launch::{SESSION_PREFIX, clean_session};
use crate::agent::status::{
    AgentState, STATUS_PREFIX, StatusSignal, TITLE_PREFIX, TITLE_PROMPT_PREFIX,
    clean_reported_title,
};
use crate::term::SESSION_TOKEN_ENV;

const MAX_HOOK_PAYLOAD_BYTES: u64 = 1024 * 1024;

#[cfg(target_os = "macos")]
const O_NOCTTY: i32 = 0x20000;
#[cfg(target_os = "macos")]
const O_NONBLOCK: i32 = 0x0004;
#[cfg(target_os = "linux")]
const O_NOCTTY: i32 = 0x0100;
#[cfg(target_os = "linux")]
const O_NONBLOCK: i32 = 0x0800;

/// Called by persistent hooks; the CLI returns before GPUI is initialized.
pub fn run_cli() -> Option<i32> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--kumu-notify") {
        return None;
    }
    let agent = args.next().unwrap_or_default();
    let kind = args.next().unwrap_or_default();
    if std::env::var("KUMU_TERMINAL").as_deref() != Ok("1") {
        return Some(0);
    }
    let token = std::env::var(SESSION_TOKEN_ENV)
        .ok()
        .filter(|t| !t.is_empty());
    // Hooks run with the event's JSON on stdin, which is where Claude, Codex,
    // Cursor and Antigravity put the conversation/session id. The session
    // update is another OSC on the same channel, framed as a notification so
    // the pane can tell it apart.
    let payload = hook_payload();
    let mut sequence = String::new();
    if let Some(session) = payload.as_ref().and_then(session_id)
        && let Some(update) = session_sequence(&agent, &session, token.as_deref())
    {
        sequence.push_str(&update);
    }
    if kind == "start"
        && let Some(prompt) = payload.as_ref().and_then(hook_prompt)
        && let Some(update) = title_sequence(&agent, &prompt, true)
    {
        sequence.push_str(&update);
    }
    // The sidebar tracks the active states plus resume/idle transitions; the
    // human notification below fires only for user-facing events.
    if let Some(signal) = notification_status(&agent, &kind, payload.as_ref()) {
        sequence.push_str(&status_sequence(&agent, signal));
    }
    if let Some(message) = notification_message(&agent, &kind, payload.as_ref()) {
        sequence.push_str(&format!("\x1b]777;notify;{agent};{message}\x07"));
    }
    if sequence.is_empty() {
        return Some(0);
    }
    match agent.as_str() {
        // Claude runs command hooks in their own session without a controlling
        // terminal. Its documented channel is `terminalSequence` on stdout.
        "claude" => {
            let output = json!({ "terminalSequence": sequence });
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(output.to_string().as_bytes());
            let _ = stdout.write_all(b"\n");
        }
        // Codex, Grok, Cursor and Antigravity detach hooks, so write to the
        // explicit PTY path Kumu inherited into the process rather than `/dev/tty`.
        "codex" | "grok" | "cursor" | "agy" => {
            if let Some(mut tty) = open_notification_tty() {
                let _ = tty.write_all(sequence.as_bytes());
            }
        }
        _ => {}
    }
    Some(0)
}

pub(super) fn notification_message(
    agent: &str,
    kind: &str,
    payload: Option<&Value>,
) -> Option<&'static str> {
    if matches!(kind, "session" | "start" | "resume" | "idle" | "exited") {
        return None;
    }
    if agent == "agy" && kind == "complete" {
        match agy_termination(payload) {
            Some("USER_CANCELED") => return None,
            Some("ERROR") => return Some("任务执行失败"),
            _ => {}
        }
    }
    Some(match kind {
        "complete" => "本轮任务已结束",
        "permission" => "需要你确认操作权限",
        _ => "需要你输入、确认或选择",
    })
}

fn agy_termination(payload: Option<&Value>) -> Option<&str> {
    payload
        .and_then(|value| value.get("terminationReason"))
        .and_then(Value::as_str)
}

/// The status signal a hook kind reports to the sidebar. `session` is only a
/// conversation id; a cancelled Antigravity turn returns the pane to idle.
pub(super) fn notification_status(
    agent: &str,
    kind: &str,
    payload: Option<&Value>,
) -> Option<StatusSignal> {
    match kind {
        "start" => Some(StatusSignal::Active(AgentState::Running)),
        "resume" => Some(StatusSignal::Resume),
        "idle" => Some(StatusSignal::Idle),
        "exited" => Some(StatusSignal::Exited),
        "permission" | "input" | "ask" => Some(StatusSignal::Active(AgentState::Waiting)),
        "failed" => Some(StatusSignal::Active(AgentState::Failed)),
        "complete" if agent == "agy" => match agy_termination(payload) {
            Some("USER_CANCELED") => Some(StatusSignal::Idle),
            Some("ERROR") => Some(StatusSignal::Active(AgentState::Failed)),
            _ => Some(StatusSignal::Active(AgentState::Completed)),
        },
        "complete" => Some(StatusSignal::Active(AgentState::Completed)),
        _ => None,
    }
}

/// The silent status update; the same notification-shaped OSC every hook
/// already writes, with a body the pane records instead of showing.
pub(super) fn status_sequence(agent: &str, signal: StatusSignal) -> String {
    format!(
        "\x1b]777;notify;{agent};{STATUS_PREFIX}{}\x07",
        signal.as_str()
    )
}

/// The submitted prompt; Antigravity names it `lastUserInput`.
fn hook_prompt(payload: &Value) -> Option<String> {
    ["prompt", "lastUserInput"]
        .iter()
        .find_map(|key| payload.get(*key).and_then(Value::as_str))
        .map(str::to_string)
}

pub(super) fn title_sequence(agent: &str, raw: &str, is_prompt: bool) -> Option<String> {
    let title = clean_reported_title(raw, is_prompt)?;
    let prefix = if is_prompt {
        format!("{TITLE_PREFIX}{TITLE_PROMPT_PREFIX}")
    } else {
        TITLE_PREFIX.to_string()
    };
    Some(format!("\x1b]777;notify;{agent};{prefix}{title}\x07"))
}

/// The hook event as JSON, or `None` when stdin is a terminal or stays silent.
fn hook_payload() -> Option<Value> {
    use std::sync::mpsc;
    use std::time::Duration;

    if io::stdin().is_terminal() {
        return None;
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut payload = String::new();
        let _ = io::stdin()
            .take(MAX_HOOK_PAYLOAD_BYTES)
            .read_to_string(&mut payload);
        let _ = tx.send(payload);
    });
    let payload = rx.recv_timeout(Duration::from_millis(500)).ok()?;
    serde_json::from_str(&payload).ok()
}

/// The OSC that reports a conversation id, framed as a notification so it
/// travels the channel every hook already writes to.
pub(super) fn session_sequence(agent: &str, session: &str, token: Option<&str>) -> Option<String> {
    let session = clean_session(session)?;
    let body = match token.filter(|t| {
        !t.is_empty()
            && t.len() <= 128
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    }) {
        Some(token) => format!("{SESSION_PREFIX}{token}:{session}"),
        None => format!("{SESSION_PREFIX}{session}"),
    };
    Some(format!("\x1b]777;notify;{agent};{body}\x07"))
}

/// The conversation id in a hook payload. Every shape Kumu registers uses
/// `session_id`; the other spellings cover CLIs that name it differently.
pub(super) fn session_id(payload: &Value) -> Option<String> {
    const KEYS: [&str; 6] = [
        "session_id",
        "sessionId",
        "conversation_id",
        "conversationId",
        "thread_id",
        "threadId",
    ];
    KEYS.iter()
        .find_map(|key| payload.get(*key).and_then(Value::as_str))
        .map(str::to_string)
}

fn open_notification_tty() -> Option<std::fs::File> {
    let path = non_empty_env_path("KUMU_TTY").unwrap_or_else(|| PathBuf::from("/dev/tty"));
    let text = path.to_str()?;
    let valid_dev =
        text == "/dev/tty" || text.starts_with("/dev/ttys") || text.starts_with("/dev/pts/");
    if !valid_dev {
        return None;
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(O_NOCTTY | O_NONBLOCK)
        .open(&path)
        .ok()?;
    file.is_terminal().then_some(file)
}
