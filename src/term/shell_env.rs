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

use std::collections::HashMap;
use std::io::Read;
use std::sync::Mutex;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// How long the login shell gets to print its environment. A startup file
/// that waits on a network mount or a prompt must not hold up the panes of
/// every project, so a late answer is dropped and the app environment is used
/// alone — the same result as before the capture existed.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// Markers bracket the `env` output so a chatty rc file (banners, version
/// managers, fetch tools) cannot be mistaken for a variable.
const BEGIN: &str = "__kumu_env_begin__";
const END: &str = "__kumu_env_end__";

/// A successful capture, kept for the session. A failed or timed-out capture
/// stays `None` so the next pane tries again instead of leaving every later
/// pane with the app environment alone.
static SHELL_ENV: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Start the capture early so the first pane finds it ready. Panes themselves
/// wait for the result in [`apply`], off the UI thread.
pub fn prewarm() {
    std::thread::Builder::new()
        .name("kumu-env".into())
        .spawn(captured)
        .expect("spawn environment thread");
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
    let mut slot = SHELL_ENV.lock().expect("shell env lock");
    if let Some(env) = slot.as_ref() {
        return env.clone();
    }
    let env = capture();
    if !env.is_empty() {
        *slot = Some(env.clone());
    }
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
    let Some(pty) = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 80 * 8,
            pixel_height: 24 * 16,
        })
        .ok()
    else {
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
        let _ = child.kill();
        let _ = child.wait();
        return HashMap::new();
    };

    let (tx, rx) = mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("kumu-env-read".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            // EOF or EIO ends the read; either way the bytes already received
            // are the answer.
            let _ = reader.read_to_end(&mut bytes);
            let _ = tx.send(bytes);
        });
    if reader.is_err() {
        let _ = child.kill();
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
            let _ = child.kill();
            drop(pty.master);
            let _ = child.wait();
            return HashMap::new();
        }
    };
    drop(pty.master);
    let _ = child.wait();

    let env = parse(&bytes);
    if env.is_empty() {
        eprintln!("[term] shell environment capture returned nothing");
    }
    env
}

fn script() -> String {
    format!("printf '%s\\n' {BEGIN}; env; printf '%s\\n' {END}")
}

fn parse(bytes: &[u8]) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(bytes);
    let mut env = HashMap::new();
    let mut started = false;
    let mut finished = false;
    let mut last: Option<String> = None;
    // `lines` strips the CR a PTY puts before every LF.
    for line in text.lines() {
        if !started {
            started = line.trim() == BEGIN;
            continue;
        }
        if line.trim() == END {
            finished = true;
            break;
        }
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
    // Without the closing marker the output ended mid-stream (the shell died
    // or was killed). Applying the values received so far risks a truncated
    // PATH or key, which is worse than using the app environment alone.
    if finished { env } else { HashMap::new() }
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
}
