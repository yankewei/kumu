//! Where each local coding agent's executable lives, and whether it can run.
//!
//! kumu does not speak any agent protocol itself. The user starts an agent's
//! TUI inside a project pane. This module only resolves the executable — a
//! GUI-launched process does not inherit the shell's PATH — and checks that it
//! is actually runnable, so the settings list can tell supported from installed.
//!
//! Each agent has a `*_BIN` environment override; otherwise the usual install
//! locations are tried before falling back to a bare name on PATH.

use std::path::{Path, PathBuf};

/// Whether the agent `id`'s executable is present and runnable on this machine.
///
/// The registry says what kumu supports, not what is installed. This asks the
/// executable resolver, so a listed agent and a launchable one stay the same.
pub fn installed(id: &'static str) -> bool {
    executable(id).is_some_and(|command| is_executable(&command))
}

/// The resolved executable for a registry agent id, or `None` when the id is
/// not one kumu knows.
fn executable(agent: &str) -> Option<String> {
    Some(match agent {
        "opencode" => opencode_bin(),
        "omp" => omp_bin(),
        "codex" => codex_bin(),
        "claude" => claude_bin(),
        "pi" => pi_bin(),
        _ => return None,
    })
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
/// The absolute candidates matter: a GUI-launched process does not inherit the
/// shell's PATH, so a `brew install`ed or per-user codex would otherwise be
/// invisible.
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
/// The native installer links `~/.local/bin/claude` into
/// `~/.local/share/claude/versions/`, and both Homebrew casks land in a `bin`
/// directory above.
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
/// Pi installs through npm into a per-user prefix — the npm global `bin` is
/// where a default `npm install -g` puts it — so neither would be found
/// otherwise from a GUI launch.
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
