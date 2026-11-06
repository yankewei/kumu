//! How to launch the local agent subprocess: the static registry entry is
//! turned into a command line here, including the env a GUI-launched process
//! needs to find a model.
//!
//! This is also the one place that decides *which* backend an agent speaks;
//! everything downstream is shared, so a new agent is a registry entry plus a
//! builder here.

use std::path::PathBuf;

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

/// The session's saved selections and backend restore cursor.
///
/// They live here as well as on the command channel because a backend may need
/// them at launch. Claude Code takes its model, effort and permission mode on
/// the command line; ACP and Codex apply theirs after the handshake and ignore
/// these. Sandbox policies are absent on purpose: no backend takes one at
/// launch — Codex's is sent as a command, and Claude's lives in its own settings.
#[derive(Clone, Default, Debug)]
pub struct SessionPicks {
    pub model: Option<String>,
    pub mode: Option<String>,
    pub reasoning: Option<String>,
    /// Backend-owned restore cursor; its format never reaches UI logic.
    pub resume_id: Option<String>,
}

/// How to launch the local agent subprocess.
pub struct Config {
    /// Static registry id this config was built for (used in events).
    pub agent: &'static str,
    pub backend: Backend,
    pub command: String,
    pub args: Vec<String>,
    /// Extra env vars applied on top of the inherited environment.
    pub env: Vec<(String, String)>,
    /// If set, send this prompt automatically once the session is ready.
    pub auto_prompt: Option<String>,
    /// The session's stored selections and restore cursor, empty until
    /// [`Config::with_session`] is called.
    pub session: SessionPicks,
}

impl Config {
    /// Fold a session's stored picks into this launch config.
    ///
    /// Only Claude Code needs them on the command line; ACP and Codex take their
    /// axes over the command channel after the handshake, so for them this only
    /// records what the session was opened with.
    ///
    /// Nothing is sent for an axis the user has not chosen: an unset value means
    /// the agent's own configuration decides, which is never kumu's to override.
    pub fn with_session(mut self, session: SessionPicks) -> Self {
        if matches!(self.backend, Backend::Claude) {
            self.args.extend(super::claude::session_args(&session));
            if let Some(id) = &session.resume_id {
                self.args.extend(["--resume".to_string(), id.clone()]);
            }
        }
        self.session = session;
        self
    }

    /// Build the config for a known agent id from the static registry.
    pub fn for_agent(id: &'static str) -> Option<Self> {
        match id {
            "opencode" => Some(Self::opencode(id)),
            "omp" => Some(Self::omp(id)),
            "codex" => Some(Self::codex(id)),
            "claude" => Some(Self::claude(id)),
            "pi" => Some(Self::pi(id)),
            _ => None,
        }
    }

    /// `opencode serve`, opencode's v2 API, with the RC gateway key injected
    /// so the server can reach a model even when launched from a GUI app. The
    /// server's own arguments (`--stdio --port 0`) and its password are the
    /// transport's to choose, so `args` stays empty.
    fn opencode(agent: &'static str) -> Self {
        let mut env = Vec::new();
        if let Some(key) = opencode_api_key() {
            env.push(("OPENCODE_API_KEY".to_string(), key));
        }
        Self {
            agent,
            backend: Backend::Opencode,
            command: opencode_bin(),
            args: Vec::new(),
            env,
            auto_prompt: auto_prompt_env(),
            session: SessionPicks::default(),
        }
    }

    /// `omp acp` (Oh My Pi); uses its own stored model config.
    fn omp(agent: &'static str) -> Self {
        Self {
            agent,
            backend: Backend::Acp,
            command: omp_bin(),
            args: vec!["acp".to_string()],
            env: Vec::new(),
            auto_prompt: auto_prompt_env(),
            session: SessionPicks::default(),
        }
    }

    /// `codex app-server` (stdio transport, the default). No env is injected:
    /// Codex resolves its own credentials, so the user's `codex login` is what
    /// authenticates it.
    fn codex(agent: &'static str) -> Self {
        Self {
            agent,
            backend: Backend::Codex,
            command: codex_bin(),
            args: vec!["app-server".to_string()],
            env: Vec::new(),
            auto_prompt: auto_prompt_env(),
            session: SessionPicks::default(),
        }
    }

    /// `claude -p` speaking newline-delimited JSON in both directions.
    ///
    /// One process per session, like the other backends: the flag combination
    /// below keeps a single conversation alive across prompts rather than
    /// answering one and exiting. `--verbose` is required by `claude` itself
    /// alongside `stream-json`.
    ///
    /// `--include-partial-messages` is what makes a reply stream token by token
    /// instead of arriving block by block, and `--permission-prompt-tool stdio`
    /// is what makes the CLI ask *kumu* about a tool call — without it the very
    /// same request comes back as a denial. Both were checked against claude
    /// 2.1.269; see `schema/claude-stream-json/2.1.269/README.md`.
    ///
    /// Two flags are deliberately not passed. `--bare` is out because it stops
    /// Claude Code reading OAuth credentials or the system keychain, which
    /// would break a Pro/Max subscription login; and no `--permission-mode` or
    /// `--model` is forced here — those arrive only through
    /// [`Config::with_session`], so an untouched session inherits the user's own
    /// Claude Code configuration instead of having kumu's defaults imposed on
    /// it.
    fn claude(agent: &'static str) -> Self {
        Self {
            agent,
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
            auto_prompt: auto_prompt_env(),
            session: SessionPicks::default(),
        }
    }

    /// `pi --mode rpc`, Pi's own embedding protocol.
    ///
    /// One process per session, like the other backends: RPC mode exists so an
    /// external client can drive Pi through a whole conversation rather than a
    /// single print-mode turn, and the process holds it across prompts.
    ///
    /// `--approve` stands in for an access posture Pi does not have. Pi has no
    /// permission system — nothing asks, and nothing could be answered if it
    /// did — so `--approve` is what lets it read the project's own files; there
    /// is no supervised mode to degrade to, which is why kumu offers no access
    /// axis for this agent at all.
    ///
    /// No env is injected: Pi resolves its own credentials, so the user's own
    /// `pi auth` is what authenticates it. Its startup update check is not
    /// suppressed either — 0.85.1 advertises `--offline` for that rather than
    /// the `PI_SKIP_VERSION_CHECK` variable older builds honoured, and the check
    /// does not sit in front of the RPC loop either way.
    fn pi(agent: &'static str) -> Self {
        Self {
            agent,
            backend: Backend::Pi,
            command: pi_bin(),
            args: vec![
                "--mode".to_string(),
                "rpc".to_string(),
                "--approve".to_string(),
            ],
            env: Vec::new(),
            auto_prompt: auto_prompt_env(),
            session: SessionPicks::default(),
        }
    }
}

fn auto_prompt_env() -> Option<String> {
    std::env::var("KUMU_AUTO_PROMPT")
        .ok()
        .filter(|s| !s.is_empty())
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
    use super::interactive;

    #[test]
    fn interactive_launch_is_the_tui() {
        for agent in ["opencode", "omp", "codex", "claude", "pi"] {
            let (command, args) = interactive(agent).expect(agent);
            assert!(!command.is_empty(), "{agent}");
            assert!(args.is_empty(), "{agent} args were {args:?}");
        }
        assert!(interactive("nope").is_none());
    }
}
