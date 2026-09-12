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
/// [`spawn`]: crate::backend::spawn
/// [`probe`]: crate::backend::probe
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Agent Client Protocol v1 over stdio.
    Acp,
    /// OpenAI Codex `app-server` JSON-RPC over stdio.
    Codex,
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
}

impl Config {
    /// Build the config for a known agent id from the static registry.
    pub fn for_agent(id: &'static str) -> Option<Self> {
        match id {
            "opencode" => Some(Self::opencode(id)),
            "omp" => Some(Self::omp(id)),
            "codex" => Some(Self::codex(id)),
            _ => None,
        }
    }

    /// `opencode acp`, with the RC gateway key injected so the subprocess can
    /// reach a model even when launched from a GUI app.
    fn opencode(agent: &'static str) -> Self {
        let mut env = Vec::new();
        if let Some(key) = opencode_api_key() {
            env.push(("OPENCODE_API_KEY".to_string(), key));
        }
        Self {
            agent,
            backend: Backend::Acp,
            command: opencode_bin(),
            args: vec!["acp".to_string()],
            env,
            auto_prompt: auto_prompt_env(),
        }
    }

    /// `omp acp` (Oh My Pi); uses its own stored model config.
    fn omp(agent: &'static str) -> Self {
        Self {
            agent,
            backend: Backend::Acp,
            command: std::env::var("OMP_BIN").unwrap_or_else(|_| "omp".to_string()),
            args: vec!["acp".to_string()],
            env: Vec::new(),
            auto_prompt: auto_prompt_env(),
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
        }
    }
}

fn auto_prompt_env() -> Option<String> {
    std::env::var("KUMU_AUTO_PROMPT")
        .ok()
        .filter(|s| !s.is_empty())
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

/// Resolve an API key for the RC gateway: env first, then the key opencode
/// stored in its auth.json, so a GUI-launched process still works.
fn opencode_api_key() -> Option<String> {
    if let Ok(k) = std::env::var("OPENCODE_API_KEY") {
        if !k.is_empty() {
            return Some(k);
        }
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
