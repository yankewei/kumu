//! How to launch the local agent subprocess: the static registry entry is
//! turned into a command line here, including the env a GUI-launched process
//! needs to find a model.
//!
//! This is also the one place that decides *which* backend an agent speaks;
//! everything downstream is shared, so a new agent is a registry entry plus a
//! builder here.

use std::path::{Path, PathBuf};

/// The protocol a subprocess speaks. [`spawn`] and [`probe`] dispatch on it.
///
/// [`spawn`]: crate::agent::spawn
/// [`probe`]: crate::agent::probe
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Agent Client Protocol v1 over stdio.
    Acp,
    /// OpenAI Codex `app-server` JSON-RPC over stdio.
    Codex,
    /// Claude Code's own headless protocol: newline-delimited JSON in and out
    /// of a single `claude -p` process.
    Claude,
    /// Pi's own RPC mode: newline-delimited JSON commands in, replies and
    /// events out, over a single `pi --mode rpc` process.
    Pi,
    /// opencode's own v2 API: HTTP and server-sent events against a private
    /// `opencode serve` process, which lives as long as its stdin.
    Opencode,
}

/// How to launch a local agent subprocess for probing (and the argv shape
/// interactive TUIs share their binary with).
pub struct Config {
    pub backend: Backend,
    pub command: String,
    pub args: Vec<String>,
    /// Extra env vars applied on top of the inherited environment.
    pub env: Vec<(String, String)>,
}

impl Config {
    /// Build the config for a known agent id from the static registry.
    pub fn for_agent(id: &'static str) -> Option<Self> {
        match id {
            "opencode" => Some(Self::opencode()),
            "omp" => Some(Self::omp()),
            "codex" => Some(Self::codex()),
            "claude" => Some(Self::claude()),
            "pi" => Some(Self::pi()),
            _ => None,
        }
    }

    /// `opencode serve`, opencode's v2 API, with the RC gateway key injected
    /// so the server can reach a model even when launched from a GUI app. The
    /// server's own arguments (`--stdio --port 0`) and its password are the
    /// transport's to choose, so `args` stays empty.
    fn opencode() -> Self {
        let mut env = Vec::new();
        if let Some(key) = opencode_api_key() {
            env.push(("OPENCODE_API_KEY".to_string(), key));
        }
        Self {
            backend: Backend::Opencode,
            command: opencode_bin(),
            args: Vec::new(),
            env,
        }
    }

    /// `omp acp` (Oh My Pi); uses its own stored model config.
    fn omp() -> Self {
        Self {
            backend: Backend::Acp,
            command: omp_bin(),
            args: vec!["acp".to_string()],
            env: Vec::new(),
        }
    }

    /// `codex app-server` (stdio transport, the default). No env is injected:
    /// Codex resolves its own credentials, so the user's `codex login` is what
    /// authenticates it.
    fn codex() -> Self {
        Self {
            backend: Backend::Codex,
            command: codex_bin(),
            args: vec!["app-server".to_string()],
            env: Vec::new(),
        }
    }

    /// `claude -p` speaking newline-delimited JSON in both directions.
    ///
    /// Used by the settings probe. `--verbose` is required by `claude` itself
    /// alongside `stream-json`. See `schema/claude-stream-json/2.1.269/README.md`.
    fn claude() -> Self {
        Self {
            backend: Backend::Claude,
            command: claude_bin(),
            args: vec![
                "-p".to_string(),
                "--input-format".to_string(),
                "stream-json".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
                "--include-partial-messages".to_string(),
                "--permission-prompt-tool".to_string(),
                "stdio".to_string(),
            ],
            env: Vec::new(),
        }
    }

    /// `pi --mode rpc`, Pi's own embedding protocol.
    ///
    /// `--approve` lets the probe read the project's files; Pi has no
    /// permission system of its own.
    fn pi() -> Self {
        Self {
            backend: Backend::Pi,
            command: pi_bin(),
            args: vec![
                "--mode".to_string(),
                "rpc".to_string(),
                "--approve".to_string(),
            ],
            env: Vec::new(),
        }
    }

    /// Whether this agent can actually be launched here. A resolved path is
    /// checked directly and a bare name is looked up on PATH — exactly what
    /// `Command::new` does when the probe spawns it — so a config that reports
    /// installed is one whose probe stands a chance of starting.
    pub fn installed(&self) -> bool {
        is_executable(&self.command)
    }
}

/// Whether the agent `id`'s executable is present on this machine.
///
/// The registry says what kumu supports, not what is installed: this asks the
/// same resolver [`Config::for_agent`] launches through (env override, install
/// locations, then PATH), so the settings list can tell the two apart without
/// spawning anything.
pub fn installed(id: &'static str) -> bool {
    Config::for_agent(id).is_some_and(|cfg| cfg.installed())
}

/// Whether `command` names something launchable: a file at a path, or a bare
/// name that resolves on this process's PATH.
fn is_executable(command: &str) -> bool {
    if command.contains(std::path::MAIN_SEPARATOR) {
        return is_executable_file(Path::new(command));
    }
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| is_executable_file(&dir.join(command)))
    })
}

/// A file the OS will actually run. A directory or a non-executable file that
/// happens to carry the agent's name must not count, or the row would claim
/// the agent is installed while every probe spawn-fails.
fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The interactive TUI for an agent: the program itself, with no protocol
/// subcommand. `omp acp`, `opencode serve`, `codex app-server`, `claude -p`,
/// and `pi --mode rpc` are the headless transports, not this.
pub fn interactive(agent: &str) -> Option<(String, Vec<String>)> {
    let command = match agent {
        "opencode" => opencode_bin(),
        "omp" => omp_bin(),
        "codex" => codex_bin(),
        "claude" => claude_bin(),
        "pi" => pi_bin(),
        _ => return None,
    };
    Some((command, Vec::new()))
}

fn omp_bin() -> String {
    std::env::var("OMP_BIN").unwrap_or_else(|_| {
        let mut candidates = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(&home).join(".local/bin/omp"));
        }
        candidates.push(PathBuf::from("/opt/homebrew/bin/omp"));
        candidates.push(PathBuf::from("/usr/local/bin/omp"));
        for candidate in candidates {
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
        "omp".to_string()
    })
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

/// Resolve the codex executable: env override, then the usual install
/// locations, then PATH.
///
/// The absolute candidates are not optional politeness: a GUI-launched process
/// does not inherit the shell's PATH, so a `brew install`ed or per-user codex
/// would otherwise be invisible.
fn codex_bin() -> String {
    std::env::var("CODEX_BIN").unwrap_or_else(|_| {
        let mut candidates = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            // The official installer and per-user installs.
            candidates.push(PathBuf::from(&home).join(".local/bin/codex"));
            candidates.push(PathBuf::from(&home).join(".codex/bin/codex"));
        }
        // Homebrew, Intel and Apple Silicon.
        candidates.push(PathBuf::from("/opt/homebrew/bin/codex"));
        candidates.push(PathBuf::from("/usr/local/bin/codex"));
        for candidate in candidates {
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
        "codex".to_string()
    })
}

/// Resolve the `claude` executable: env override, then the usual install
/// locations, then PATH.
///
/// The absolute candidates matter for the same reason as Codex's: a
/// GUI-launched process does not inherit the shell's PATH. The native installer
/// links `~/.local/bin/claude` into `~/.local/share/claude/versions/`, and both
/// Homebrew casks land in a `bin` directory above.
fn claude_bin() -> String {
    std::env::var("CLAUDE_BIN").unwrap_or_else(|_| {
        let mut candidates = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(&home).join(".local/bin/claude"));
        }
        candidates.push(PathBuf::from("/opt/homebrew/bin/claude"));
        candidates.push(PathBuf::from("/usr/local/bin/claude"));
        for candidate in candidates {
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
        "claude".to_string()
    })
}

/// Resolve the `pi` executable: env override, then the usual install
/// locations, then PATH.
///
/// The absolute candidates matter for the same reason as the other agents': a
/// GUI-launched process does not inherit the shell's PATH. Pi installs through
/// npm and into a per-user prefix — the npm global `bin` is where a default
/// `npm install -g` puts it — so neither would be found otherwise.
fn pi_bin() -> String {
    std::env::var("PI_BIN").unwrap_or_else(|_| {
        let mut candidates = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(&home).join(".local/bin/pi"));
            candidates.push(PathBuf::from(&home).join(".npm-global/bin/pi"));
        }
        candidates.push(PathBuf::from("/opt/homebrew/bin/pi"));
        candidates.push(PathBuf::from("/usr/local/bin/pi"));
        for candidate in candidates {
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
        "pi".to_string()
    })
}

/// Resolve an API key for the RC gateway: env first, then the key opencode
/// stored in its auth.json, so a GUI-launched process still works.
fn opencode_api_key() -> Option<String> {
    if let Ok(key) = std::env::var("OPENCODE_API_KEY")
        && !key.is_empty()
    {
        return Some(key);
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

#[cfg(test)]
mod tests {
    use super::{Config, installed, interactive, is_executable};
    use crate::agent::AGENTS;

    #[test]
    fn interactive_launch_is_the_tui() {
        for agent in AGENTS.iter().map(|a| a.id) {
            let (command, args) = interactive(agent).expect(agent);
            assert!(!command.is_empty(), "{agent}");
            assert!(args.is_empty(), "{agent} args were {args:?}");
            // Every registry entry must also be launchable as a probe, or the
            // settings row could never show a catalogue.
            assert!(Config::for_agent(agent).is_some(), "{agent}");
        }
        assert!(interactive("nope").is_none());
    }

    /// Detection must not take a path's word for it: a stale override or a
    /// binary the user uninstalled would otherwise be listed as installed and
    /// probe-fail on every refresh.
    #[test]
    fn a_missing_or_unrunnable_command_is_not_installed() {
        assert!(!is_executable("/nonexistent/kumu-agent"));
        assert!(!installed("nope"));
        #[cfg(unix)]
        {
            assert!(is_executable("/bin/sh"));
            assert!(!is_executable("/tmp"), "a directory is not a command");
        }
    }
}
