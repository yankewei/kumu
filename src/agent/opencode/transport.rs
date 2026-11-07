//! The opencode v2 wire: a private `opencode serve` per probe, spoken to over
//! HTTP on a local port.
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

/// How long the server may take to report its URL. Its first start on a
/// machine migrates storage and loads plugins, which is slow but bounded.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a catalogue read may take. The server answers as soon as it has
/// the data, so a read that has not answered by now is not going to.
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

/// Echo the wire traffic to stderr when KUMU_DEBUG is set, for headless
/// verification (mirrors the shared `emit` convention).
pub(super) fn debug_log(message: &str) {
    if std::env::var("KUMU_DEBUG").is_ok() {
        eprintln!("[opencode] {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::{error_message, server_url};

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
