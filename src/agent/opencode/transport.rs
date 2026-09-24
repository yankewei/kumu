//! The opencode v2 wire: a private `opencode serve` per session, spoken to over
//! HTTP on a local port, with its events on a server-sent event stream.
//!
//! The server is launched the way opencode's own CLI launches one for
//! `--standalone`: `serve --stdio --port 0` with a random password in
//! `OPENCODE_PASSWORD`. It prints `{"url": …}` as its first stdout line and
//! then serves until its stdin closes — so holding the stdin handle is what
//! keeps it alive, and dropping [`Server`] is what ends it, the same lifetime
//! every other backend's subprocess has. Every request carries HTTP Basic auth
//! (user `opencode`); without it the server answers `401` to everything.
//!
//! See `schema/opencode-v2/2.0.16/README.md` for how this was checked.

use std::io::Read as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, header};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

/// How long the server may take to report its URL. Its first start on a
/// machine migrates storage and loads plugins, which is slow but bounded.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an ordinary request may take. Every call kumu makes answers as soon
/// as it is accepted — a prompt is an enqueue — so one that has not answered by
/// now is not going to.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A running `opencode serve`, and the client that talks to it.
pub(super) struct Server {
    base: String,
    auth: String,
    client: Client<HttpConnector, Full<Bytes>>,
    /// Held, never written: the server serves until this closes.
    _stdin: ChildStdin,
    /// Killed on drop, in case closing stdin was not enough.
    _child: Child,
}

impl Server {
    /// Launch a server in `cwd` and wait for it to say where it listens.
    ///
    /// `cwd` matters: it is the location a new session is created in, and the
    /// location whose configuration (models, agents, commands) the catalogue
    /// reads are answered for.
    pub(super) async fn start(
        command: &str,
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<Self, String> {
        let password = random_password().map_err(|error| format!("生成密码失败：{error}"))?;
        let mut child = Command::new(command)
            .args(["serve", "--stdio", "--port", "0"])
            .envs(env.iter().map(|(k, v)| (k.clone(), v.clone())))
            .env("OPENCODE_PASSWORD", &password)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // opencode logs nothing to stderr in stdio mode unless asked to
            // (`OPENCODE_PRINT_LOGS`), and what it does print is worth seeing.
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("{command}: {error}"))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");

        // The URL is the first line that parses as one; anything printed
        // before it (a notice, a migration) is passed over. stdout is then
        // drained for the server's lifetime, so a later write never meets a
        // closed pipe.
        let mut lines = BufReader::new(stdout).lines();
        let find_url = async {
            let mut skipped = Vec::new();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => match server_url(&line) {
                        Ok(url) => return Ok(url),
                        Err(_) => skipped.push(line),
                    },
                    Ok(None) if skipped.is_empty() => {
                        return Err("opencode 服务启动后立即退出".to_string());
                    }
                    Ok(None) => {
                        return Err(format!("opencode 服务没有报告地址：{}", skipped.join("\n")));
                    }
                    Err(error) => return Err(format!("读取 opencode 服务地址失败：{error}")),
                }
            }
        };
        let base = tokio::time::timeout(START_TIMEOUT, find_url)
            .await
            .map_err(|_| "opencode 服务启动超时".to_string())??;
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                debug_log(&format!("stdout: {line}"));
            }
        });
        debug_log(&format!("server at {base}"));

        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"))
        );
        Ok(Self {
            base,
            auth,
            client: Client::builder(TokioExecutor::new()).build_http(),
            _stdin: stdin,
            _child: child,
        })
    }

    pub(super) async fn get(&self, path: &str) -> Result<Value, String> {
        self.request(Method::GET, path, None)
            .await
            .map(Option::unwrap_or_default)
    }

    /// POST a JSON body; `None` for the `204` most actions answer with.
    pub(super) async fn post(&self, path: &str, body: Value) -> Result<Option<Value>, String> {
        self.request(Method::POST, path, Some(body)).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Option<Value>, String> {
        let label = format!("{method} {path}");
        let bytes = body.map(|b| b.to_string()).unwrap_or_default();
        let request = Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base))
            .header(header::AUTHORIZATION, &self.auth)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(bytes)))
            .map_err(|error| format!("{label}: {error}"))?;
        let exchange = async {
            let response = self
                .client
                .request(request)
                .await
                .map_err(|error| format!("{label}: {error}"))?;
            let status = response.status();
            let body = response
                .into_body()
                .collect()
                .await
                .map_err(|error| format!("{label}: {error}"))?
                .to_bytes();
            Ok::<_, String>((status, body))
        };
        let (status, body) = tokio::time::timeout(REQUEST_TIMEOUT, exchange)
            .await
            .map_err(|_| format!("{label}: 响应超时"))??;
        debug_log(&format!("{label} -> {status}"));
        if !status.is_success() {
            return Err(format!(
                "{label}: {}",
                error_message(status.as_u16(), &body)
            ));
        }
        if body.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| format!("{label}: 响应无法解析：{error}"))
    }

    /// Subscribe to the server's event stream. Each event's JSON arrives on the
    /// returned channel; the channel closes when the stream does.
    ///
    /// Opened before anything is asked of a session, so nothing it reports can
    /// be missed: the stream carries no history.
    pub(super) async fn events(&self) -> Result<mpsc::UnboundedReceiver<Value>, String> {
        let request = Request::builder()
            .uri(format!("{}/api/event", self.base))
            .header(header::AUTHORIZATION, &self.auth)
            .header(header::ACCEPT, "text/event-stream")
            .body(Full::new(Bytes::new()))
            .map_err(|error| format!("GET /api/event: {error}"))?;
        let response = tokio::time::timeout(REQUEST_TIMEOUT, self.client.request(request))
            .await
            .map_err(|_| "GET /api/event: 响应超时".to_string())?
            .map_err(|error| format!("GET /api/event: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("GET /api/event: HTTP {}", response.status()));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let mut body = response.into_body();
        tokio::spawn(async move {
            let mut parser = SseParser::default();
            while let Some(Ok(frame)) = body.frame().await {
                let Ok(chunk) = frame.into_data() else {
                    continue;
                };
                for data in parser.push(&chunk) {
                    match serde_json::from_str::<Value>(&data) {
                        Ok(event) => {
                            if tx.send(event).is_err() {
                                return;
                            }
                        }
                        Err(error) => debug_log(&format!("unparsable event ({error})")),
                    }
                }
            }
            debug_log("event stream closed");
        });
        Ok(rx)
    }
}

/// A password the way opencode's CLI makes one: 32 random bytes, base64url.
fn random_password() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// The URL from the server's first stdout line, `{"url": "http://…"}`.
fn server_url(line: &str) -> Result<String, String> {
    serde_json::from_str::<Value>(line.trim())
        .ok()
        .and_then(|value| value.get("url")?.as_str().map(str::to_string))
        .map(|url| url.trim_end_matches('/').to_string())
        .ok_or_else(|| format!("opencode 服务没有报告地址：{}", line.trim()))
}

/// What a failed request says: the `message` the server's error body carries,
/// else the status with whatever text there was.
fn error_message(status: u16, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value.get("message")?.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("HTTP {status} {}", text.trim()))
}

/// Splits a server-sent event stream into the `data` of each event.
///
/// Only what this server sends is handled: events are separated by a blank
/// line, and payload lines start with `data:`. Heartbeats are comment lines
/// (`: heartbeat`) and carry no data, so they produce nothing. Chunks may split
/// anywhere — mid-line, mid-event, mid-character — which is why raw bytes are
/// buffered and only a complete event is decoded.
#[derive(Default)]
struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        // A payload never holds a raw `\r` (JSON escapes it), so dropping them
        // turns `\r\n` into `\n` even when a chunk splits the pair.
        self.buffer.extend(chunk.iter().filter(|&&b| b != b'\r'));
        let mut out = Vec::new();
        while let Some(end) = self.buffer.windows(2).position(|pair| pair == b"\n\n") {
            let event: Vec<u8> = self.buffer.drain(..end + 2).collect();
            let event = String::from_utf8_lossy(&event);
            let data: Vec<&str> = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|data| data.strip_prefix(' ').unwrap_or(data))
                .collect();
            if !data.is_empty() {
                out.push(data.join("\n"));
            }
        }
        out
    }
}

/// Echo the wire traffic to stderr when KUMU_DEBUG is set, for headless
/// verification (mirrors the shared `emit` convention).
pub(super) fn debug_log(message: &str) {
    if std::env::var("KUMU_DEBUG").is_ok() {
        eprintln!("[opencode] {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::{SseParser, error_message, server_url};

    /// Events split across chunks come out whole, and heartbeats come out as
    /// nothing.
    #[test]
    fn sse_events_survive_arbitrary_chunking() {
        let mut parser = SseParser::default();
        assert!(parser.push(b": heartbeat\n\ndata: {\"type\":").is_empty());
        assert_eq!(parser.push(b"\"a\"}\n\ndata: {}\n"), ["{\"type\":\"a\"}"]);
        assert_eq!(parser.push(b"\n"), ["{}"]);
        assert!(parser.push(b": heartbeat\r\n\r\n").is_empty());
        // A character split between chunks comes out whole.
        let event = "data: \"你好\"\n\n".as_bytes();
        assert!(parser.push(&event[..8]).is_empty());
        assert_eq!(parser.push(&event[8..]), ["\"你好\""]);
    }

    #[test]
    fn the_first_line_names_the_server() {
        assert_eq!(
            server_url("{\"url\":\"http://127.0.0.1:61354\"}\n").unwrap(),
            "http://127.0.0.1:61354"
        );
        assert!(server_url("server listening").is_err());
    }

    #[test]
    fn an_error_body_says_why() {
        assert_eq!(
            error_message(
                404,
                br#"{"_tag":"SessionNotFound","message":"Session not found"}"#
            ),
            "Session not found"
        );
        assert_eq!(error_message(401, b""), "HTTP 401 ");
    }
}
