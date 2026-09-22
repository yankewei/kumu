//! Exercise real stdio adapters against a local fixture, including process and
//! database reopening. These tests never invoke a model service.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use super::{Cmd, Config, Connection, Event, SessionPicks, spawn};
use crate::db::Store;

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    directory: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "kumu-resume-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&directory).unwrap();
        Self { directory }
    }

    fn config(&self, agent: &'static str, resume_id: Option<String>) -> Config {
        let mut cfg = Config::for_agent(agent).unwrap();
        let protocol = if agent == "omp" { "acp" } else { agent };
        cfg.command = "python3".into();
        cfg.args = vec![
            format!(
                "{}/src/agent/test_support/resume_agent.py",
                env!("CARGO_MANIFEST_DIR")
            ),
            protocol.into(),
            self.directory.to_string_lossy().into_owned(),
        ];
        cfg.env.clear();
        cfg.auto_prompt = None;
        cfg.with_session(SessionPicks {
            resume_id,
            ..Default::default()
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn finish_connection(conn: Connection, rx: Receiver<Event>) {
    drop(conn);
    loop {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("backend did not close"),
        }
    }
}

fn prompt(conn: &Connection, rx: &Receiver<Event>, text: &str, store: &Store, id: i64) -> String {
    conn.send(Cmd::Prompt(text.into()));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut answer = String::new();
    loop {
        match rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            Event::SessionReady { resume_id, .. } => {
                store.update_session_resume_id(id, &resume_id).unwrap()
            }
            Event::Agent(text) => answer.push_str(&text),
            Event::Thought { .. } | Event::ToolCall { .. } => {
                panic!("history replay leaked into live turn")
            }
            Event::Done(_) => return answer,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
}

fn restores_conversation(agent: &'static str) {
    let fixture = Fixture::new();
    let db_path = fixture.directory.join("kumu.db");
    let store = Store::open(&db_path).unwrap();
    let project = store
        .insert_project("test", fixture.directory.to_str().unwrap())
        .unwrap();
    let id = store
        .insert_session(project, agent, "test", fixture.directory.to_str().unwrap())
        .unwrap();
    let (tx, rx) = channel();
    let conn = spawn(fixture.config(agent, None), fixture.directory.clone(), tx);
    assert_eq!(
        prompt(&conn, &rx, "remember this marker", &store, id),
        "remember this marker"
    );
    let cursor = store.sessions()[0]
        .resume_id
        .clone()
        .expect("backend supplied a cursor");
    finish_connection(conn, rx);
    drop(store);

    let store = Store::open(&db_path).unwrap();
    assert_eq!(
        store.sessions()[0].resume_id.as_deref(),
        Some(cursor.as_str())
    );
    let (tx, rx) = channel();
    let conn = spawn(
        fixture.config(agent, Some(cursor)),
        fixture.directory.clone(),
        tx,
    );
    assert_eq!(
        prompt(&conn, &rx, "recall the previous prompt", &store, id),
        "remember this marker"
    );
    finish_connection(conn, rx);
}

#[test]
fn codex_restores_conversation_after_reopening() {
    restores_conversation("codex");
}

#[test]
fn claude_restores_conversation_after_reopening() {
    restores_conversation("claude");
}

#[test]
fn pi_restores_conversation_after_reopening() {
    restores_conversation("pi");
}

#[test]
fn acp_restores_conversation_without_replaying_history() {
    restores_conversation("omp");
}

#[test]
fn failed_restore_never_creates_a_fresh_conversation_or_sends_the_prompt() {
    for agent in ["codex", "claude", "pi", "omp"] {
        let fixture = Fixture::new();
        let (tx, rx) = channel();
        let conn = spawn(
            fixture.config(agent, Some("missing".into())),
            fixture.directory.clone(),
            tx,
        );
        conn.send(Cmd::Prompt("do not execute".into()));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Event::Error(_) => break,
                Event::Agent(_) | Event::SessionReady { .. } => {
                    panic!("{agent} silently opened a new session")
                }
                _ => {}
            }
        }
        finish_connection(conn, rx);
        let log =
            std::fs::read_to_string(fixture.directory.join("requests.jsonl")).unwrap_or_default();
        for line in log.lines() {
            let frame: serde_json::Value = serde_json::from_str(line).unwrap();
            let method = frame
                .get("method")
                .or_else(|| frame.get("type"))
                .and_then(|v| v.as_str())
                .unwrap();
            assert!(
                !matches!(
                    method,
                    "thread/start"
                        | "session/new"
                        | "turn/start"
                        | "session/prompt"
                        | "prompt"
                        | "user"
                ),
                "{agent} issued {method}"
            );
        }
    }
}

#[test]
fn acp_checks_restore_capability_before_loading() {
    let fixture = Fixture::new();
    let mut cfg = fixture.config("omp", Some("conversation-1".into()));
    cfg.env.push(("KUMU_FAKE_NO_LOAD".into(), "1".into()));
    let (tx, rx) = channel();
    let conn = spawn(cfg, fixture.directory.clone(), tx);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Event::Error(error) = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            assert!(error.contains("不支持恢复会话"));
            break;
        }
    }
    finish_connection(conn, rx);
    let log = std::fs::read_to_string(fixture.directory.join("requests.jsonl")).unwrap();
    assert!(!log.contains("session/load") && !log.contains("session/new"));
}

/// A fresh session says whether its agent will load it back. That flag is what
/// keeps the app from reclaiming a connection it could not reopen, so an agent
/// without `load_session` must report `false` rather than hand back a cursor it
/// would refuse to use.
#[test]
fn a_fresh_session_reports_whether_it_can_be_resumed() {
    for (no_load, expected) in [(false, true), (true, false)] {
        let fixture = Fixture::new();
        let mut cfg = fixture.config("omp", None);
        cfg.env.push((
            "KUMU_FAKE_NO_LOAD".into(),
            if no_load { "1" } else { "0" }.into(),
        ));
        let (tx, rx) = channel();
        let conn = spawn(cfg, fixture.directory.clone(), tx);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Event::SessionReady { resumable, .. } => {
                    assert_eq!(resumable, expected, "KUMU_FAKE_NO_LOAD={no_load}");
                    break;
                }
                Event::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
        finish_connection(conn, rx);
    }
}
