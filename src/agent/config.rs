//! Where each local coding agent's executable lives, and whether it can run.
//!
//! kumu does not speak any agent protocol itself: it starts the agent's TUI in
//! a project pane (see [`super::launch`]). This module only resolves the
//! executable — a GUI-launched process does not inherit the shell's PATH — and
//! checks that it is actually runnable, so the settings list can tell supported
//! from installed.
//!
//! Each agent has a `*_BIN` environment override; otherwise the usual install
//! locations are tried before falling back to a bare name on PATH.

use std::path::{Path, PathBuf};

/// Whether the agent `id`'s executable is present and runnable on this machine.
///
/// The registry says what kumu supports, not what is installed. This asks the
/// executable resolver, so a listed agent and a launchable one stay the same.
pub fn installed(id: &str) -> bool {
    executable(id).is_some_and(|command| is_executable(&command))
}

/// The resolved executable for a registry agent id, or `None` when the id is
/// not one kumu knows.
pub fn executable(agent: &str) -> Option<String> {
    Some(match agent {
        "opencode" => opencode_bin(),
        "omp" => omp_bin(),
        "codex" => codex_bin(),
        "claude" => claude_bin(),
        "pi" => pi_bin(),
        "cursor" => cursor_bin(),
        "grok" => grok_bin(),
        "agy" => agy_bin(),
        _ => return None,
    })
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn non_empty_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Whether `command` names something launchable: a file at a path, or a bare
/// name that resolves on this process's PATH or the captured login-shell PATH.
fn is_executable(command: &str) -> bool {
    if command.is_empty() {
        return false;
    }
    if command.contains(std::path::MAIN_SEPARATOR) {
        return is_executable_file(Path::new(command));
    }
    let on_path = |path: &std::ffi::OsStr| {
        std::env::split_paths(path).any(|dir| is_executable_file(&dir.join(command)))
    };
    if std::env::var_os("PATH").is_some_and(|p| on_path(&p)) {
        return true;
    }
    // Finder-launched apps have launchd's minimal PATH; consult the captured
    // login-shell PATH so version-manager installs (nvm/fnm/mise/bun/cargo)
    // are recognized.
    crate::term::shell_env::cached_path().is_some_and(|p| on_path(&p))
}

/// Like [`installed`], but awaits login-shell PATH capture when needed. Call
/// from background threads (e.g. Settings open probe), never the UI thread.
pub fn installed_with_shell_env(id: &str) -> bool {
    let Some(command) = executable(id) else {
        return false;
    };
    if is_executable(&command) {
        return true;
    }
    if command.contains(std::path::MAIN_SEPARATOR) {
        return false;
    }
    crate::term::shell_env::captured_path().is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| is_executable_file(&dir.join(&command)))
    })
}

/// A file the OS will actually run. A directory or a non-executable file that
/// happens to carry the agent's name must not count, or the row would claim
/// the agent is installed while launching it always fails.
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

fn first_executable(candidates: &[PathBuf]) -> Option<String> {
    candidates
        .iter()
        .find(|candidate| is_executable_file(candidate))
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

fn omp_bin() -> String {
    non_empty_env("OMP_BIN").unwrap_or_else(|| {
        let mut candidates = Vec::new();
        if let Some(home) = non_empty_home() {
            candidates.push(home.join(".local/bin/omp"));
        }
        candidates.push(PathBuf::from("/opt/homebrew/bin/omp"));
        candidates.push(PathBuf::from("/usr/local/bin/omp"));
        first_executable(&candidates).unwrap_or_else(|| "omp".to_string())
    })
}

fn opencode_bin() -> String {
    non_empty_env("OPENCODE_BIN").unwrap_or_else(|| {
        let mut candidates = Vec::new();
        if let Some(home) = non_empty_home() {
            candidates.push(home.join(".opencode/bin/opencode"));
            candidates.push(home.join(".local/bin/opencode"));
        }
        candidates.push(PathBuf::from("/opt/homebrew/bin/opencode"));
        candidates.push(PathBuf::from("/usr/local/bin/opencode"));
        first_executable(&candidates).unwrap_or_else(|| "opencode".to_string())
    })
}

/// Resolve the codex executable: env override, then the usual install
/// locations, then PATH.
fn codex_bin() -> String {
    non_empty_env("CODEX_BIN").unwrap_or_else(|| {
        let mut candidates = Vec::new();
        if let Some(home) = non_empty_home() {
            candidates.push(home.join(".local/bin/codex"));
            candidates.push(home.join(".codex/bin/codex"));
        }
        candidates.push(PathBuf::from("/opt/homebrew/bin/codex"));
        candidates.push(PathBuf::from("/usr/local/bin/codex"));
        first_executable(&candidates).unwrap_or_else(|| "codex".to_string())
    })
}

/// Resolve the `claude` executable: env override, then the usual install
/// locations, then PATH.
fn claude_bin() -> String {
    non_empty_env("CLAUDE_BIN").unwrap_or_else(|| {
        let mut candidates = Vec::new();
        if let Some(home) = non_empty_home() {
            candidates.push(home.join(".local/bin/claude"));
        }
        candidates.push(PathBuf::from("/opt/homebrew/bin/claude"));
        candidates.push(PathBuf::from("/usr/local/bin/claude"));
        first_executable(&candidates).unwrap_or_else(|| "claude".to_string())
    })
}

/// Resolve the `pi` executable: env override, then the usual install
/// locations, then PATH.
fn pi_bin() -> String {
    non_empty_env("PI_BIN").unwrap_or_else(|| {
        let mut candidates = Vec::new();
        if let Some(home) = non_empty_home() {
            candidates.push(home.join(".local/bin/pi"));
            candidates.push(home.join(".npm-global/bin/pi"));
        }
        candidates.push(PathBuf::from("/opt/homebrew/bin/pi"));
        candidates.push(PathBuf::from("/usr/local/bin/pi"));
        first_executable(&candidates).unwrap_or_else(|| "pi".to_string())
    })
}

/// Resolve the Cursor CLI executable: env override, then its install
/// locations, then PATH.
///
/// `~/.local/bin/agent` is shared with Grok Build on some setups, so it is
/// accepted only when its canonical target path identifies Cursor.
fn cursor_bin() -> String {
    non_empty_env("CURSOR_AGENT_BIN").unwrap_or_else(|| {
        let mut candidates = Vec::new();
        if let Some(home) = non_empty_home() {
            candidates.push(home.join(".local/bin/cursor-agent"));
            let agent = home.join(".local/bin/agent");
            if is_executable_file(&agent)
                && std::fs::canonicalize(&agent).is_ok_and(|target| {
                    target
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains("cursor")
                })
            {
                candidates.push(agent);
            }
        }
        candidates.push(PathBuf::from("/opt/homebrew/bin/cursor-agent"));
        candidates.push(PathBuf::from("/usr/local/bin/cursor-agent"));
        first_executable(&candidates).unwrap_or_else(|| "cursor-agent".to_string())
    })
}

/// Resolve the Grok Build executable: env override, then its install
/// locations, then PATH.
fn grok_bin() -> String {
    non_empty_env("GROK_BIN").unwrap_or_else(|| {
        let mut candidates = Vec::new();
        if let Some(home) = non_empty_home() {
            candidates.push(home.join(".grok/bin/grok"));
            candidates.push(home.join(".local/bin/grok"));
        }
        candidates.push(PathBuf::from("/opt/homebrew/bin/grok"));
        candidates.push(PathBuf::from("/usr/local/bin/grok"));
        first_executable(&candidates).unwrap_or_else(|| "grok".to_string())
    })
}

/// Resolve the Antigravity CLI executable: env override, then its install
/// locations, then PATH.
///
/// The official installer puts `agy` in `~/.local/bin/agy`, and Homebrew
/// casks land in `/opt/homebrew/bin` or `/usr/local/bin`.
fn agy_bin() -> String {
    std::env::var("AGY_BIN").unwrap_or_else(|_| {
        let mut candidates = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(&home).join(".local/bin/agy"));
        }
        candidates.push(PathBuf::from("/opt/homebrew/bin/agy"));
        candidates.push(PathBuf::from("/usr/local/bin/agy"));
        for candidate in candidates {
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
        "agy".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::{installed, is_executable};
    use crate::agent::AGENTS;

    #[test]
    fn every_agent_has_an_executable() {
        for agent in AGENTS.iter().map(|a| a.id) {
            assert!(super::executable(agent).is_some(), "{agent}");
        }
        assert!(super::executable("nope").is_none());
    }

    /// Detection must not take a path's word for it: a stale override or a
    /// binary the user uninstalled must not be listed as installed.
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
