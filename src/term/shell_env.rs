//! The environment a pane's program starts with.
//!
//! Kumu launched from Finder inherits launchd's minimal environment: no PATH
//! entries from the user's shell, no API keys exported by `~/.zshrc`. A shell
//! pane still sees them because the shell reads its own startup files, but an
//! agent pane runs the agent binary directly and would start with launchd's
//! environment alone — which is why an agent started from the pane picker (or
//! `kumu pane split --agent …`) reports a missing key while the same agent
//! runs fine when typed into a shell pane.
//!
//! Capture the login shell's exported environment once, through a PTY and an
//! interactive shell so the same startup files a shell pane loads are loaded
//! here, then lay it under the PTY of every pane that starts a program
//! directly. A login-shell pane reads those files itself and is left alone.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// How long the login shell gets to print its environment.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the startup PATH probe gets before falling back to common directories.
const PATH_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// How long to wait before retrying after a failed or timed-out capture, so a
/// broken rc file does not stall every subsequent agent pane for 5 seconds.
const RETRY_BACKOFF: Duration = Duration::from_secs(60);

/// Markers bracket the `env` output so a chatty rc file (banners, version
/// managers, fetch tools) cannot be mistaken for a variable.
const BEGIN: &str = "__kumu_env_begin__";
const END: &str = "__kumu_env_end__";

/// Markers bracketing the login-shell `PATH` probe output.
const PATH_BEGIN: &[u8] = b"kumu-path[";
const PATH_END: &[u8] = b"]kumu-path";
const PATH_PROBE_SCRIPT: &str = "printf 'kumu-path['; /usr/bin/printenv PATH; printf ']kumu-path'";

struct Cache {
    env: Option<HashMap<String, String>>,
    last_failure: Option<Instant>,
}

static SHELL_ENV: Mutex<Cache> = Mutex::new(Cache {
    env: None,
    last_failure: None,
});

/// Set once the first capture attempt has finished, successfully or not.
/// Launch code uses it to tell "this agent is not installed" apart from "the
/// login shell's PATH has not been read yet"; treating the latter as missing
/// would silently start a shell in an agent pane during app startup.
static CAPTURE_SETTLED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
fn kill_process_group(child: &mut Box<dyn portable_pty::Child + Send + Sync>) {
    const SIGKILL: i32 = 9;
    if let Some(pid) = child.process_id()
        && let Ok(pgid) = i32::try_from(pid)
        && pgid > 0
    {
        unsafe {
            kill(-pgid, SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill_process_group(child: &mut Box<dyn portable_pty::Child + Send + Sync>) {
    let _ = child.kill();
}

/// Populate this process's `PATH` once when launched from Finder/Dock
/// (`parent_id() == 1` on Unix, i.e. launchd).
///
/// Must be called early in `main` before other threads start, because it calls
/// `std::env::set_var`. Subprocesses spawned directly by Kumu (such as `git`
/// running Git LFS smudge filters or hooks) then inherit the user's login
/// `PATH` and common Homebrew/local bin directories.
pub fn fill_process_path() {
    #[cfg(unix)]
    {
        if std::os::unix::process::parent_id() != 1 {
            return;
        }
        let (program, base_args) = crate::term::default_shell();
        let login_path = probe_login_path_from(&program, &base_args, &[], PATH_PROBE_TIMEOUT);
        let fallback_dirs = default_fallback_dirs(std::env::var_os("HOME").as_deref());
        let current_path = std::env::var_os("PATH");
        if let Some(merged) = merge_paths(
            login_path.as_deref(),
            &fallback_dirs,
            current_path.as_deref(),
        ) {
            // SAFETY: Called once at startup in `main` before spawning threads;
            // `probe_login_path_from` joins its reader thread before returning.
            unsafe {
                std::env::set_var("PATH", merged);
            }
        }
    }
}

#[cfg(unix)]
fn kill_std_process_group(child: &mut std::process::Child) {
    const SIGKILL: i32 = 9;
    if let Ok(pgid) = i32::try_from(child.id())
        && pgid > 0
    {
        unsafe {
            kill(-pgid, SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(unix)]
fn probe_login_path_from(
    program: &str,
    base_args: &[String],
    extra_env: &[(&str, String)],
    timeout: Duration,
) -> Option<OsString> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(program);
    cmd.args(base_args)
        .args(["-c", PATH_PROBE_SCRIPT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;

    let (tx, rx) = mpsc::channel();
    let reader_handle = std::thread::Builder::new()
        .name("kumu-path-probe".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        bytes.extend_from_slice(&chunk[..n]);
                        if has_marker(&bytes, PATH_END) || bytes.len() >= 256 * 1024 {
                            break;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = tx.send(bytes);
        })
        .ok();
    let Some(reader_handle) = reader_handle else {
        kill_std_process_group(&mut child);
        let _ = child.wait();
        return None;
    };

    let bytes = match rx.recv_timeout(timeout) {
        Ok(bytes) => {
            let wait_deadline = Instant::now() + Duration::from_millis(50);
            while Instant::now() < wait_deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if !matches!(child.try_wait(), Ok(Some(_))) {
                kill_std_process_group(&mut child);
                let _ = child.wait();
            }
            Some(bytes)
        }
        Err(_) => {
            kill_std_process_group(&mut child);
            let _ = child.wait();
            None
        }
    };
    // Always join the reader thread before returning so the caller remains
    // single-threaded when updating process `PATH`.
    let _ = reader_handle.join();

    bytes.as_deref().and_then(parse_path_probe)
}

fn has_marker(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle)
}

fn parse_path_probe(bytes: &[u8]) -> Option<OsString> {
    let begin_pos = bytes
        .windows(PATH_BEGIN.len())
        .position(|w| w == PATH_BEGIN)?;
    let after_begin = &bytes[begin_pos + PATH_BEGIN.len()..];
    let end_rel = after_begin
        .windows(PATH_END.len())
        .position(|w| w == PATH_END)?;
    let mut body = &after_begin[..end_rel];
    while let Some(rest) = body
        .strip_prefix(b"\r")
        .or_else(|| body.strip_prefix(b"\n"))
    {
        body = rest;
    }
    while let Some(rest) = body
        .strip_suffix(b"\r")
        .or_else(|| body.strip_suffix(b"\n"))
    {
        body = rest;
    }
    if body.is_empty() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(OsStr::from_bytes(body).to_os_string())
    }
    #[cfg(not(unix))]
    {
        Some(OsString::from(String::from_utf8_lossy(body).into_owned()))
    }
}

fn default_fallback_dirs(home: Option<&OsStr>) -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(3);
    if let Some(home) = home
        && !home.is_empty()
    {
        dirs.push(PathBuf::from(home).join(".local/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs
}

/// Merge `PATH` in priority order: login shell `PATH` → fallback directories →
/// current process `PATH`, deduplicating while preserving first-seen order.
fn merge_paths(
    login_path: Option<&OsStr>,
    fallback_dirs: &[PathBuf],
    current_path: Option<&OsStr>,
) -> Option<OsString> {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    let mut push = |dir: PathBuf| {
        if !dir.as_os_str().is_empty() && seen.insert(dir.clone()) {
            merged.push(dir);
        }
    };

    if let Some(path) = login_path {
        for dir in std::env::split_paths(path) {
            push(dir);
        }
    }
    for dir in fallback_dirs {
        push(dir.clone());
    }
    if let Some(path) = current_path {
        for dir in std::env::split_paths(path) {
            push(dir);
        }
    }

    if merged.is_empty() {
        return None;
    }
    std::env::join_paths(merged).ok()
}

/// Start the capture early so the first pane finds it ready. Panes themselves
/// wait for the result in [`apply`], off the UI thread.
pub fn prewarm() {
    let _ = std::thread::Builder::new()
        .name("kumu-env".into())
        .spawn(captured);
}

/// The captured login-shell `PATH`, if already available (or once captured).
/// Safe to call from background threads; never blocks the UI thread if called
/// via [`cached_path`].
pub fn captured_path() -> Option<OsString> {
    captured().get("PATH").map(OsString::from)
}

/// Non-blocking read of the already-captured `PATH`, for callers that must not
/// wait on shell capture.
pub fn cached_path() -> Option<OsString> {
    SHELL_ENV.try_lock().ok().and_then(|slot| {
        slot.env
            .as_ref()
            .and_then(|env| env.get("PATH"))
            .map(OsString::from)
    })
}

/// Whether the first environment capture has finished (successfully or not).
///
/// A caller checking whether an agent is installed must not treat `false` as
/// "not installed": the captured login-shell PATH is what makes nvm/fnm/mise
/// installs visible, and the capture runs alongside app startup.
pub fn settled() -> bool {
    CAPTURE_SETTLED.load(Ordering::Acquire)
}

/// Lay the captured shell environment under `cmd`. Variables that describe
/// the app rather than the shell keep Kumu's value; everything else — PATH,
/// API keys, locale — is whatever the user's shell exports.
pub fn apply(cmd: &mut CommandBuilder) {
    for (key, value) in captured() {
        if !keep_from_app(&key) {
            cmd.env(key, value);
        }
    }
}

fn captured() -> HashMap<String, String> {
    // Holding the lock across the capture keeps two first panes from running
    // the login shell twice; the timeout bounds how long a pane waits.
    let mut slot = SHELL_ENV.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(env) = slot.env.as_ref() {
        CAPTURE_SETTLED.store(true, Ordering::Release);
        return env.clone();
    }
    if slot
        .last_failure
        .is_some_and(|at| at.elapsed() < RETRY_BACKOFF)
    {
        CAPTURE_SETTLED.store(true, Ordering::Release);
        return HashMap::new();
    }
    let env = capture();
    if env.is_empty() {
        slot.last_failure = Some(Instant::now());
    } else {
        slot.last_failure = None;
        slot.env = Some(env.clone());
    }
    CAPTURE_SETTLED.store(true, Ordering::Release);
    env
}

fn capture() -> HashMap<String, String> {
    // Terminal tests run in the developer's environment: sourcing their real
    // login files would make every PTY test wait on (and depend on) rc
    // contents. `capture_from` is still covered directly by its test.
    if cfg!(test) {
        return HashMap::new();
    }
    let (program, args) = crate::term::default_shell();
    capture_from(&program, &args, &[])
}

/// Run `program` in a PTY, ask it for its environment, and parse the answer.
///
/// An interactive shell (`-i`) is required: zsh reads `~/.zshrc` only then,
/// and a shell pane is interactive because its stdin is the PTY slave. `-c`
/// keeps it from drawing a prompt or waiting for input.
fn capture_from(
    program: &str,
    base_args: &[String],
    extra_env: &[(&str, String)],
) -> HashMap<String, String> {
    let Ok(pty) = native_pty_system().openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 80 * 8,
        pixel_height: 24 * 16,
    }) else {
        return HashMap::new();
    };

    let mut args = base_args.to_vec();
    args.extend(["-i".to_string(), "-c".to_string(), script()]);
    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    // A shell started from inside another Kumu pane must not believe it is a
    // pane itself; the rc file may branch on these.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KUMU_") {
            cmd.env_remove(key);
        }
    }
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.env("TERM", "xterm-256color");

    let Ok(mut child) = pty.slave.spawn_command(cmd) else {
        return HashMap::new();
    };
    // The parent's copy of the slave would keep the master from seeing EOF
    // once the shell exits.
    drop(pty.slave);
    let Ok(mut reader) = pty.master.try_clone_reader() else {
        kill_process_group(&mut child);
        let _ = child.wait();
        return HashMap::new();
    };

    let (tx, rx) = mpsc::channel();
    let reader_handle = std::thread::Builder::new()
        .name("kumu-env-read".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        bytes.extend_from_slice(&chunk[..n]);
                        // Stop as soon as the closing marker arrives instead of
                        // waiting for EOF, so background helpers launched by
                        // `~/.zshrc` that inherit the PTY slave cannot trigger a
                        // timeout.
                        if has_end_marker(&bytes) || bytes.len() >= 2 * 1024 * 1024 {
                            break;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = tx.send(bytes);
        });
    if reader_handle.is_err() {
        kill_process_group(&mut child);
        let _ = child.wait();
        return HashMap::new();
    }

    let bytes = match rx.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(bytes) => bytes,
        Err(error) => {
            match error {
                RecvTimeoutError::Timeout => {
                    eprintln!("[term] shell environment capture timed out");
                }
                RecvTimeoutError::Disconnected => {
                    eprintln!("[term] shell environment capture reader stopped");
                }
            }
            kill_process_group(&mut child);
            drop(pty.master);
            let _ = child.wait();
            return HashMap::new();
        }
    };
    // Ensure the shell and any background helpers it spawned are cleaned up so
    // `wait()` cannot hang.
    drop(pty.master);
    let wait_deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < wait_deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !matches!(child.try_wait(), Ok(Some(_))) {
        kill_process_group(&mut child);
        let _ = child.wait();
    }

    let env = parse(&bytes);
    if env.is_empty() {
        eprintln!("[term] shell environment capture returned nothing");
    }
    env
}

fn has_end_marker(bytes: &[u8]) -> bool {
    let needle = END.as_bytes();
    bytes.windows(needle.len()).any(|w| w == needle)
}

fn script() -> String {
    format!(
        "command printf '%s\\n' {BEGIN}; /usr/bin/env -0 2>/dev/null || /usr/bin/env; command printf '\\n%s\\n' {END}"
    )
}

fn parse(bytes: &[u8]) -> HashMap<String, String> {
    let begin = BEGIN.as_bytes();
    let end = END.as_bytes();
    let Some(begin_pos) = bytes.windows(begin.len()).position(|w| w == begin) else {
        return HashMap::new();
    };
    let after_begin = &bytes[begin_pos + begin.len()..];
    let Some(end_rel) = after_begin.windows(end.len()).position(|w| w == end) else {
        return HashMap::new();
    };
    let body = &after_begin[..end_rel];
    // Strip the newline (CR/LF) right after BEGIN.
    let body = body
        .strip_prefix(b"\r\n")
        .or_else(|| body.strip_prefix(b"\n"))
        .unwrap_or(body);

    if body.contains(&0) {
        let mut env = HashMap::new();
        for entry in body.split(|&b| b == 0) {
            if entry.is_empty() {
                continue;
            }
            let text = String::from_utf8_lossy(entry);
            // PTY line discipline translates LF inside values into CRLF.
            let cleaned = text.replace("\r\n", "\n");
            if let Some((key, value)) = cleaned.split_once('=')
                && is_env_name(key)
            {
                env.insert(key.to_string(), value.to_string());
            }
        }
        return env;
    }

    let text = String::from_utf8_lossy(body);
    let mut env = HashMap::new();
    let mut last: Option<String> = None;
    // `lines` strips the CR a PTY puts before every LF.
    for line in text.lines() {
        match line.split_once('=') {
            Some((key, value)) if is_env_name(key) => {
                env.insert(key.to_string(), value.to_string());
                last = Some(key.to_string());
            }
            // A value with a newline continues on the next line; keep it
            // rather than dropping a variable the shell meant to export.
            _ => {
                if let Some(key) = &last
                    && let Some(value) = env.get_mut(key)
                {
                    value.push('\n');
                    value.push_str(line);
                }
            }
        }
    }
    env
}

/// Whether `key` can be a shell-exported variable name. `env` prints one
/// `KEY=VALUE` per line, but a value may wrap onto following lines; requiring
/// a shell-style name keeps a wrapped line that happens to contain `=` (a
/// bash function body, a script snippet) from becoming a variable of its own.
fn is_env_name(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Variables where Kumu's value must win: where the app lives and how macOS
/// started it. Anything else is the shell's to define.
fn keep_from_app(key: &str) -> bool {
    matches!(
        key,
        "HOME"
            | "TMPDIR"
            | "USER"
            | "LOGNAME"
            | "SHELL"
            | "PWD"
            | "OLDPWD"
            | "SHLVL"
            | "_"
            | "COMMAND_MODE"
    ) || key.starts_with("__CF")
        || key.starts_with("XPC_")
        || key.starts_with("KUMU_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_between_the_markers_and_keeps_multi_line_values() {
        let bytes = b"welcome banner\r\n__kumu_env_begin__\r\nPATH=/opt/homebrew/bin:/usr/bin\r\nMINIMAX_API_KEY=abc=def\r\nNOTE=first\r\n    local port=8080\r\nsecond\r\n__kumu_env_end__\r\ntrailing\r\n";
        let env = parse(bytes);
        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        assert_eq!(
            env.get("MINIMAX_API_KEY").map(String::as_str),
            Some("abc=def")
        );
        // A wrapped line that contains `=` belongs to the previous value, not
        // to a variable of its own.
        assert_eq!(
            env.get("NOTE").map(String::as_str),
            Some("first\n    local port=8080\nsecond")
        );
        assert!(!env.contains_key("    local port"));
        assert!(!env.contains_key("welcome banner"));
    }

    #[test]
    fn a_partial_or_missing_marker_yields_no_environment() {
        assert!(parse(b"PATH=/usr/bin\r\n").is_empty());
        assert!(parse(b"__kumu_env_begin__\r\nPATH=/usr/bin\r\n").is_empty());
    }

    #[test]
    fn a_capture_attempt_settles_launch_checks() {
        // Tests take the fast path in `capture`, but launch code only needs
        // to know that the attempt finished, successful or not.
        let _ = captured();
        assert!(settled());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn capture_loads_the_profile_and_the_interactive_rc() {
        let home = std::env::temp_dir().join(format!("kumu-shell-env-{}", std::process::id()));
        std::fs::create_dir(&home).expect("create shell fixture");
        std::fs::write(
            home.join(".zprofile"),
            "export KUMU_PROFILE_TEST=from_profile\n",
        )
        .expect("write profile");
        std::fs::write(home.join(".zshrc"), "export KUMU_RC_TEST=from_rc\n").expect("write rc");

        let env = capture_from(
            "/bin/zsh",
            &["-l".to_string()],
            &[
                ("HOME", home.display().to_string()),
                ("ZDOTDIR", home.display().to_string()),
            ],
        );
        std::fs::remove_dir_all(&home).expect("remove shell fixture");

        assert_eq!(
            env.get("KUMU_PROFILE_TEST").map(String::as_str),
            Some("from_profile")
        );
        assert_eq!(env.get("KUMU_RC_TEST").map(String::as_str), Some("from_rc"));
    }

    #[test]
    fn parse_path_probe_extracts_marked_value_and_strips_noise() {
        let output = b"login banner\nkumu-path[/opt/homebrew/bin:/usr/bin\n]kumu-path\ntrailing\n";
        assert_eq!(
            parse_path_probe(output),
            Some(OsString::from("/opt/homebrew/bin:/usr/bin"))
        );
        assert_eq!(parse_path_probe(b"no markers here"), None);
        assert_eq!(parse_path_probe(b"kumu-path[/usr/bin"), None);
        assert_eq!(parse_path_probe(b"kumu-path[\n]kumu-path"), None);
    }

    #[test]
    fn merge_paths_preserves_order_and_deduplicates() {
        let fallback = default_fallback_dirs(Some(OsStr::new("/Users/test")));
        let merged = merge_paths(
            Some(OsStr::new(
                "/custom/bin::/opt/homebrew/bin:/usr/bin:/custom/bin",
            )),
            &fallback,
            Some(OsStr::new("/usr/bin:/bin:/usr/local/bin:/sbin")),
        )
        .expect("merged PATH");
        let entries: Vec<PathBuf> = std::env::split_paths(&merged).collect();
        assert_eq!(
            entries,
            vec![
                PathBuf::from("/custom/bin"),
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/Users/test/.local/bin"),
                PathBuf::from("/usr/local/bin"),
                PathBuf::from("/bin"),
                PathBuf::from("/sbin"),
            ]
        );
    }

    #[test]
    fn merge_paths_falls_back_when_login_probe_fails() {
        let fallback = default_fallback_dirs(Some(OsStr::new("/Users/test")));
        let merged =
            merge_paths(None, &fallback, Some(OsStr::new("/usr/bin:/bin"))).expect("merged PATH");
        let entries: Vec<PathBuf> = std::env::split_paths(&merged).collect();
        assert_eq!(
            entries,
            vec![
                PathBuf::from("/Users/test/.local/bin"),
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin"),
            ]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn probe_login_path_reads_zprofile() {
        let home = std::env::temp_dir().join(format!("kumu-path-probe-{}", std::process::id()));
        std::fs::create_dir(&home).expect("create probe fixture");
        std::fs::write(
            home.join(".zprofile"),
            "echo noisy banner\nexport PATH=\"/kumu/test/bin:$PATH\"\n",
        )
        .expect("write zprofile");

        let probed = probe_login_path_from(
            "/bin/zsh",
            &["-l".to_string()],
            &[
                ("HOME", home.display().to_string()),
                ("ZDOTDIR", home.display().to_string()),
                ("PATH", "/usr/bin:/bin".to_string()),
            ],
            PATH_PROBE_TIMEOUT,
        );
        std::fs::remove_dir_all(&home).expect("remove probe fixture");

        let probed = probed.expect("probed PATH");
        let entries: Vec<PathBuf> = std::env::split_paths(&probed).collect();
        assert_eq!(entries.first(), Some(&PathBuf::from("/kumu/test/bin")));
    }
}
