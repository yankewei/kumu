//! A loopback HTTP server for one artifact directory.
//!
//! The platform web view loads `http://127.0.0.1:<port>/<entry>`, so relative
//! CSS/JS/images and root-relative `/assets/...` links resolve the way the
//! artifact author wrote them. The server is bound to loopback, serves only
//! files inside the artifact directory, and stops when the reading sheet drops.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::ArtifactError;

const READ_LIMIT: usize = 8 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONCURRENT_REQUESTS: usize = 32;

pub struct ArtifactServer {
    origin: String,
    port: u16,
    entry_path: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ArtifactServer {
    /// Serve `root` on an ephemeral loopback port, with `entry` as the page the
    /// reading sheet loads.
    pub fn start(root: &Path, entry: &Path) -> Result<Self, ArtifactError> {
        let root = fs::canonicalize(root)?;
        let entry = fs::canonicalize(entry)?;
        if !entry.starts_with(&root) {
            return Err(ArtifactError(format!(
                "{} is outside the artifact directory",
                entry.display()
            )));
        }
        let entry_path = entry
            .strip_prefix(&root)
            .map_err(|error| ArtifactError(error.to_string()))?;
        let entry_path = encode_path(entry_path);

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let origin = format!("http://127.0.0.1:{port}");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let active = Arc::new(AtomicUsize::new(0));
        let serve_root = root;
        let thread = thread::Builder::new()
            .name("kumu-artifact-server".into())
            .spawn(move || {
                for incoming in listener.incoming() {
                    if stop_thread.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut stream) = incoming else {
                        break;
                    };
                    if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONCURRENT_REQUESTS {
                        active.fetch_sub(1, Ordering::AcqRel);
                        respond(&mut stream, 503, "text/plain", b"too many requests", false);
                        continue;
                    }
                    let root = serve_root.clone();
                    let active_req = Arc::clone(&active);
                    let spawned = thread::Builder::new()
                        .name("kumu-artifact-request".into())
                        .spawn(move || {
                            serve(stream, &root);
                            active_req.fetch_sub(1, Ordering::AcqRel);
                        });
                    if spawned.is_err() {
                        active.fetch_sub(1, Ordering::AcqRel);
                    }
                }
            })?;

        Ok(Self {
            origin,
            port,
            entry_path,
            stop,
            thread: Some(thread),
        })
    }

    /// `http://127.0.0.1:<port>/<entry>`.
    pub fn entry_url(&self) -> String {
        format!("{}/{}", self.origin, self.entry_path)
    }

    /// The loopback origin every artifact page and asset shares.
    pub fn origin(&self) -> &str {
        &self.origin
    }
}

impl Drop for ArtifactServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake the blocking `accept()` immediately without polling.
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], self.port));
        let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(mut stream: TcpStream, root: &Path) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    let mut request = Vec::with_capacity(1024);
    let mut buffer = [0u8; 1024];
    while request.len() < READ_LIMIT {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let Ok(text) = String::from_utf8(request) else {
        respond(&mut stream, 400, "text/plain", b"bad request", false);
        return;
    };
    let Some(line) = text.lines().next() else {
        respond(&mut stream, 400, "text/plain", b"bad request", false);
        return;
    };
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        respond(&mut stream, 400, "text/plain", b"bad request", false);
        return;
    };
    if !matches!(method, "GET" | "HEAD") {
        respond(&mut stream, 405, "text/plain", b"method not allowed", false);
        return;
    }

    let Some(relative) = decode_request_path(target) else {
        respond(&mut stream, 404, "text/plain", b"not found", false);
        return;
    };
    let mut path = root.join(relative);
    if path.is_dir() {
        path.push("index.html");
    }
    let Ok(path) = fs::canonicalize(&path) else {
        respond(&mut stream, 404, "text/plain", b"not found", false);
        return;
    };
    if !path.starts_with(root) || !path.is_file() {
        respond(&mut stream, 403, "text/plain", b"forbidden", false);
        return;
    }
    let Ok(bytes) = fs::read(&path) else {
        respond(&mut stream, 404, "text/plain", b"not found", false);
        return;
    };
    respond(&mut stream, 200, mime_type(&path), &bytes, method == "HEAD");
}

fn respond(stream: &mut TcpStream, status: u16, mime: &str, body: &[u8], head: bool) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {mime}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    if !head {
        let _ = stream.write_all(body);
    }
    let _ = stream.flush();
}

fn decode_request_path(target: &str) -> Option<PathBuf> {
    let target = target.split(['?', '#']).next().unwrap_or(target);
    let target = target.strip_prefix('/').unwrap_or(target);
    let bytes = target.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(*bytes.get(index + 1)?)?;
            let low = hex(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded).ok()?;
    let mut path = PathBuf::new();
    for component in Path::new(&decoded).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(path)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn encode_path(path: &Path) -> String {
    let mut out = String::new();
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(&mut out, "%{byte:02X}");
        }
    }
    out
}

fn mime_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "xml" => "application/xml",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    fn get(url: &str, path: &str) -> (u16, String) {
        let address = url
            .trim_start_matches("http://")
            .split('/')
            .next()
            .expect("authority");
        let mut stream = TcpStream::connect(address).expect("connect");
        write!(stream, "GET {path} HTTP/1.1\r\nHost: {address}\r\n\r\n").expect("request");
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).expect("status");
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("header");
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).expect("body");
        (
            status
                .split_whitespace()
                .nth(1)
                .and_then(|code| code.parse().ok())
                .unwrap_or(0),
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    #[test]
    fn paths_round_trip_and_spaces_are_encoded() {
        let encoded = encode_path(Path::new("assets/a b.css"));
        assert_eq!(encoded, "assets/a%20b.css");
        assert_eq!(
            decode_request_path("/assets/a%20b.css").expect("decode"),
            PathBuf::from("assets/a b.css")
        );
    }

    #[test]
    fn serves_the_entry_and_relative_assets_inside_the_artifact() {
        let root = std::env::temp_dir().join(format!(
            "kumu-artifact-server-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(root.join("index.html"), "<h1>Hello</h1>").unwrap();
        fs::write(root.join("assets/app.css"), "body{}").unwrap();
        let entry = root.join("index.html");
        let server = ArtifactServer::start(&root, &entry).expect("start server");
        let origin = server.origin().to_string();
        let (status, body) = get(&origin, "/index.html");
        assert_eq!(status, 200);
        assert!(body.contains("Hello"));
        let (status, body) = get(&origin, "/assets/app.css");
        assert_eq!(status, 200);
        assert_eq!(body, "body{}");
        // `/` resolves to index.html.
        let (status, _) = get(&origin, "/");
        assert_eq!(status, 200);
        drop(server);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_paths_outside_the_artifact() {
        let root = std::env::temp_dir().join(format!(
            "kumu-artifact-server-deny-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("index.html"), "safe").unwrap();
        fs::write(root.with_extension("secret"), "secret").unwrap();
        let server = ArtifactServer::start(&root, &root.join("index.html")).expect("start");
        let origin = server.origin().to_string();
        let (status, body) = get(&origin, "/../secret");
        assert_ne!(status, 200, "{body}");
        assert!(!body.contains("secret"));
        let (status, _) = get(&origin, "/%2e%2e/secret");
        assert_ne!(status, 200);
        drop(server);
        let _ = fs::remove_file(root.with_extension("secret"));
        let _ = fs::remove_dir_all(&root);
    }
}
