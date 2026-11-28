//! Local control channel: what a program inside a pane may ask the app.
//!
//! Everything a pane shows lives in this process, so a program in one pane (an
//! agent, a script) reaches that state over a Unix socket: one JSON request
//! line in, one JSON reply line out. The socket is owner-only under `~/.kumu`.
//! It lists the panes, reads one pane's text, splits a pane into a new agent,
//! and types into an agent pane — a shell pane never takes input, because text
//! there would run as a command.
//!
//! The module holds both ends: the CLI a pane's agent runs and the server the
//! home window starts.

mod cli;
mod cli_artifact;
mod cli_pane;
mod protocol;
mod server;

use std::path::PathBuf;
use std::time::Duration;

pub use cli::run_cli;
pub use protocol::{
    ArtifactNotifyResult, CloseResult, Command, ControlError, PaneContent, PaneSummary, PaneTarget,
    ScrollDirection, ScrollResult, SendResult, SplitResult, WaitResult,
};
#[cfg(test)]
use protocol::{Request, RequestLine};
pub use server::{ControlHandle, start};

/// How long the server waits for the UI to answer one request. A split that
/// waits for the agent to draw its prompt is the slow one.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the accept loop looks at its stop flag.
const POLL: Duration = Duration::from_millis(25);
/// Upper bound on one JSON request line accepted by the control socket.
const MAX_REQUEST_BYTES: u64 = 256 * 1024;
/// Upper bound on one JSON reply line read by the CLI client.
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum concurrent control-socket worker threads.
const MAX_CONCURRENT_CLIENTS: usize = 32;
/// The most scrollback rows one read may ask for. Reading history pages the
/// viewport, so the bound is work, not storage.
pub const MAX_HISTORY: usize = 2_000;
/// Maximum lines / wheel ticks accepted in a single `pane scroll` request.
pub const MAX_SCROLL_LINES: u16 = 500;
/// The pid of the Kumu a pane's process belongs to. Kumu exports it to every
/// pane, so a client can tell its own app apart from another instance that
/// happens to serve the socket.
pub const PID_ENV: &str = "KUMU_APP_PID";

/// Where the socket lives. `KUMU_CONTROL_SOCKET` lets a test or a second
/// instance pick its own; the default sits beside the database.
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("KUMU_CONTROL_SOCKET").filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    let db = crate::db::db_path();
    if db.as_os_str().is_empty() {
        return PathBuf::new();
    }
    db.with_file_name("control.sock")
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    use serde_json::Value;

    use super::cli::{call_at_instance, last_lines};
    use super::cli_pane::{ScrollCliArgs, ScrollParse, parse_pane_scroll};
    use super::server::start_at;
    use super::*;

    /// The test's socket is served by this process, so that is the instance a
    /// client should expect.
    fn call_here(path: &Path, request: &Request) -> Result<Value, String> {
        call_at_instance(path, request, Some(std::process::id()))
    }

    #[test]
    fn a_request_line_carries_the_instance_only_when_known() {
        let request = Request::Panes;
        let plain = serde_json::to_string(&RequestLine {
            pid: None,
            request: request.clone(),
        })
        .expect("request line");
        assert_eq!(plain, r#"{"op":"panes"}"#);

        let known = serde_json::to_string(&RequestLine {
            pid: Some(7),
            request,
        })
        .expect("request line");
        let decoded: RequestLine = serde_json::from_str(&known).expect("decode");
        assert_eq!(decoded.pid, Some(7));
        assert!(matches!(decoded.request, Request::Panes));
    }

    #[test]
    fn artifact_requests_carry_an_optional_pane_and_require_a_project_path() {
        let published = serde_json::to_string(&Request::ArtifactPublished {
            from: None,
            project_path: "/repo".into(),
            artifact: "20261128-153012-a1b2".into(),
        })
        .expect("published request");
        assert_eq!(
            published,
            r#"{"op":"artifactPublished","projectPath":"/repo","artifact":"20261128-153012-a1b2"}"#
        );

        let opened = serde_json::to_string(&Request::ArtifactOpen {
            from: Some(7),
            project_path: "/repo".into(),
            artifact: "20261128-153012-a1b2".into(),
        })
        .expect("open request");
        assert_eq!(
            opened,
            r#"{"op":"artifactOpen","from":7,"projectPath":"/repo","artifact":"20261128-153012-a1b2"}"#
        );
        let decoded: Request = serde_json::from_str(&opened).expect("decode");
        assert!(matches!(
            decoded,
            Request::ArtifactOpen { from: Some(7), .. }
        ));
    }

    #[test]
    fn a_read_request_round_trips_as_one_json_line() {
        let request = Request::Read {
            pane: PaneTarget::Name("right".into()),
            from: Some(7),
            history: 200,
        };
        let text = serde_json::to_string(&request).expect("request");
        assert_eq!(
            text,
            r#"{"op":"read","pane":"right","from":7,"history":200}"#
        );
        let decoded: Request = serde_json::from_str(&text).expect("decode");
        assert!(matches!(
            decoded,
            Request::Read {
                pane: PaneTarget::Name(name),
                from: Some(7),
                history: 200,
            } if name == "right"
        ));
    }

    #[test]
    fn a_pane_id_target_survives_the_same_line() {
        let request: Request = serde_json::from_str(r#"{"op":"read","pane":3}"#).expect("decode");
        assert!(matches!(
            request,
            Request::Read {
                pane: PaneTarget::Id(3),
                from: None,
                history: 0,
            }
        ));
    }

    #[test]
    fn the_tail_keeps_the_newest_lines() {
        assert_eq!(last_lines("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(last_lines("a\nb", 9), "a\nb");
        assert_eq!(last_lines("a\nb", 0), "");
    }

    fn summary(id: u64) -> PaneSummary {
        PaneSummary {
            id,
            project: 1,
            project_name: "kumu".into(),
            title: "zsh".into(),
            command: "zsh".into(),
            cwd: "/tmp".into(),
            agent: None,
            focused: id == 3,
            selected: true,
            project_path: "/tmp".into(),
            agent_session_id: None,
            tab_name: "工作区 1".into(),
            cols: 120,
            rows: 38,
            exited: false,
        }
    }

    /// The window side of the channel is a plain receiver here: the point is
    /// that a client really gets a reply over the socket, and that the request
    /// reached the app intact.
    #[test]
    fn a_client_reads_a_list_and_a_pane_over_the_socket() {
        use futures::StreamExt;

        let dir = std::env::temp_dir().join(format!(
            "kumu-control-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("socket dir");
        let path = dir.join("control.sock");
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let handle = start_at(path.clone(), tx).expect("bind the socket");

        let answering = std::thread::spawn(move || {
            while let Some(command) = futures::executor::block_on(rx.next()) {
                match command {
                    Command::Panes { reply } => {
                        reply.send(Ok(vec![summary(3)])).expect("list reply");
                    }
                    Command::Read {
                        target,
                        from,
                        history,
                        reply,
                    } => {
                        assert!(
                            matches!(&target, PaneTarget::Name(name) if name == "right"),
                            "{target:?}"
                        );
                        assert_eq!(from, Some(3));
                        assert_eq!(history, 200, "history is passed through");
                        reply
                            .send(Ok(PaneContent {
                                pane: summary(4),
                                text: "hello".into(),
                            }))
                            .expect("read reply");
                    }
                    Command::Split {
                        direction,
                        agent,
                        from,
                        prompt,
                        cancelled: _,
                        reply,
                    } => {
                        assert_eq!(direction, "right");
                        assert_eq!(agent, "codex");
                        assert_eq!(from, Some(3));
                        assert_eq!(prompt.as_deref(), Some("review the diff"));
                        reply
                            .send(Ok(SplitResult { pane: summary(5) }))
                            .expect("split reply");
                    }
                    Command::Send {
                        target,
                        from,
                        text,
                        enter,
                        reply,
                    } => {
                        assert!(matches!(&target, PaneTarget::Id(5)), "{target:?}");
                        assert_eq!(from, Some(3));
                        assert_eq!(text, "review the diff");
                        assert!(enter, "Enter follows the text by default");
                        reply
                            .send(Ok(SendResult {
                                pane: summary(5),
                                submitted: enter,
                            }))
                            .expect("send reply");
                    }
                    Command::Close {
                        target,
                        allow_self,
                        reply,
                        ..
                    } => {
                        assert!(matches!(target, PaneTarget::Id(5)));
                        assert!(!allow_self);
                        reply
                            .send(Ok(CloseResult {
                                pane: 5,
                                closed: true,
                            }))
                            .expect("close reply");
                    }
                    Command::Wait {
                        target,
                        timeout_secs,
                        next,
                        reply,
                        ..
                    } => {
                        assert!(matches!(target, PaneTarget::Id(5)));
                        assert_eq!(timeout_secs, 30);
                        assert!(next);
                        reply
                            .send(Ok(WaitResult {
                                pane: PaneSummary {
                                    id: 5,
                                    project: 1,
                                    project_name: "kumu".into(),
                                    title: "codex".into(),
                                    command: "codex".into(),
                                    cwd: "/tmp/kumu".into(),
                                    agent: Some("codex".into()),
                                    focused: false,
                                    selected: true,
                                    project_path: "/tmp/kumu".into(),
                                    agent_session_id: Some("session-1".into()),
                                    tab_name: "工作区 1".into(),
                                    cols: 80,
                                    rows: 24,
                                    exited: false,
                                },
                                state: "completed".into(),
                                elapsed_ms: 420,
                            }))
                            .expect("wait reply");
                    }
                    Command::Scroll {
                        target,
                        from,
                        direction,
                        lines,
                        col,
                        row,
                        reply,
                    } => {
                        assert!(matches!(target, PaneTarget::Name(ref s) if s == "right"));
                        assert_eq!(from, Some(3));
                        assert_eq!(direction, ScrollDirection::Up);
                        assert_eq!(lines, Some(15));
                        assert_eq!(col, Some(20));
                        assert_eq!(row, Some(10));
                        reply
                            .send(Ok(ScrollResult {
                                pane: summary(4),
                                direction,
                                lines: 15,
                                moved: Some(-15),
                            }))
                            .expect("scroll reply");
                    }
                    Command::ArtifactPublished { .. } | Command::ArtifactOpen { .. } => {}
                }
            }
        });

        let list = call_here(&path, &Request::Panes).expect("list");
        assert_eq!(list["panes"][0]["id"], 3);
        assert_eq!(list["panes"][0]["projectName"], "kumu");

        let read = call_here(
            &path,
            &Request::Read {
                pane: PaneTarget::Name("right".into()),
                from: Some(3),
                history: 200,
            },
        )
        .expect("read");
        assert_eq!(read["text"], "hello");
        assert_eq!(read["pane"]["id"], 4);

        let scrolled = call_here(
            &path,
            &Request::Scroll {
                pane: PaneTarget::Name("right".into()),
                from: Some(3),
                direction: ScrollDirection::Up,
                lines: Some(15),
                col: Some(20),
                row: Some(10),
            },
        )
        .expect("scroll");
        assert_eq!(scrolled["pane"]["id"], 4);
        assert_eq!(scrolled["direction"], "up");
        assert_eq!(scrolled["lines"], 15);
        assert_eq!(scrolled["moved"], -15);

        let bad_scroll = call_here(
            &path,
            &Request::Scroll {
                pane: PaneTarget::Name("right".into()),
                from: Some(3),
                direction: ScrollDirection::Down,
                lines: Some(MAX_SCROLL_LINES + 1),
                col: None,
                row: None,
            },
        )
        .expect_err("out-of-range scroll lines rejected");
        assert!(bad_scroll.contains("between 1 and"), "{bad_scroll}");

        let split = call_here(
            &path,
            &Request::Split {
                direction: "right".into(),
                agent: "codex".into(),
                from: Some(3),
                prompt: Some("review the diff".into()),
            },
        )
        .expect("split");
        assert_eq!(split["pane"]["id"], 5);

        let send = call_here(
            &path,
            &Request::Send {
                pane: PaneTarget::Id(5),
                from: Some(3),
                text: "review the diff".into(),
                enter: true,
            },
        )
        .expect("send");
        assert_eq!(send["submitted"], true);

        let waited = call_here(
            &path,
            &Request::Wait {
                pane: PaneTarget::Id(5),
                from: Some(3),
                timeout_secs: 30,
                next: true,
            },
        )
        .expect("wait");
        assert_eq!(waited["state"], "completed");
        assert_eq!(waited["elapsedMs"], 420);

        let closed = call_here(
            &path,
            &Request::Close {
                pane: PaneTarget::Id(5),
                from: Some(3),
                allow_self: false,
            },
        )
        .expect("close");
        assert_eq!(closed["pane"], 5);
        assert_eq!(closed["closed"], true);

        // A pane of another Kumu must not read this one's panes: the reply
        // carries the serving pid, so the client can refuse the answer.
        let foreign = call_at_instance(&path, &Request::Panes, Some(std::process::id() + 1))
            .expect_err("another instance is refused");
        assert!(foreign.contains("another Kumu"), "{foreign}");

        drop(handle);
        answering
            .join()
            .expect("the answering side ends with the socket");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// accept() inherits the listener's nonblocking flag on macOS, so a client
    /// whose request lands a moment after the connection must still be read as
    /// one request instead of an empty line.
    #[test]
    fn a_late_request_is_not_read_as_an_empty_one() {
        use futures::StreamExt;

        let dir = std::env::temp_dir().join(format!(
            "kumu-late-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("socket dir");
        let path = dir.join("control.sock");
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let handle = start_at(path.clone(), tx).expect("bind the socket");
        let answering = std::thread::spawn(move || {
            while let Some(command) = futures::executor::block_on(rx.next()) {
                if let Command::Panes { reply } = command {
                    let _ = reply.send(Ok(vec![summary(3)]));
                }
            }
        });

        let mut stream = UnixStream::connect(&path).expect("connect");
        std::thread::sleep(Duration::from_millis(300));
        stream
            .write_all(b"{\"op\":\"panes\"}\n")
            .expect("write request");
        let mut reply = String::new();
        BufReader::new(stream)
            .read_line(&mut reply)
            .expect("read reply");
        assert!(reply.contains("\"ok\":true"), "{reply}");

        drop(handle);
        answering
            .join()
            .expect("the answering side ends with the socket");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_pane_scroll_handles_direction_target_and_bounds() {
        let s = |xs: &[&str]| xs.iter().map(|v| (*v).to_string()).collect::<Vec<_>>();

        let ScrollParse::Parsed(parsed) = parse_pane_scroll(&s(&["up"])) else {
            panic!("expected parsed up with default target");
        };
        assert_eq!(
            parsed,
            ScrollCliArgs {
                direction: ScrollDirection::Up,
                target: None,
                lines: None,
                col: None,
                row: None,
                json: false,
            }
        );

        // `up` as direction and `up` as target must not be ambiguous.
        let ScrollParse::Parsed(parsed) = parse_pane_scroll(&s(&[
            "down", "up", "--lines", "12", "--col", "8", "--row", "4", "--json",
        ])) else {
            panic!("expected parsed down targeting pane above");
        };
        assert_eq!(
            parsed,
            ScrollCliArgs {
                direction: ScrollDirection::Down,
                target: Some("up".into()),
                lines: Some(12),
                col: Some(8),
                row: Some(4),
                json: true,
            }
        );

        assert!(matches!(
            parse_pane_scroll(&s(&["right"])),
            ScrollParse::Error(_)
        ));
        assert!(matches!(
            parse_pane_scroll(&s(&["up", "23", "--lines", "0"])),
            ScrollParse::Error(_)
        ));
        assert!(matches!(
            parse_pane_scroll(&s(&["up", "23", "--lines", "501"])),
            ScrollParse::Error(_)
        ));
    }
}
