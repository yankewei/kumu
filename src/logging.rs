//! File logging for launches that have no terminal attached.
//!
//! `cargo run` reports `eprintln!` diagnostics in the terminal, but a Finder
//! launched `.app` inherits launchd's stderr, which nobody can read. When
//! stderr is not a terminal, point it at `~/Library/Logs/Kumu/kumu.log` so
//! users can attach the log to a bug report. A terminal keeps its own stderr
//! for local development; `KUMU_LOG_TO_FILE=1` forces the file anyway.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// Keep the log small enough to mail; rotate one previous generation.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

#[cfg(unix)]
unsafe extern "C" {
    fn dup2(oldfd: i32, newfd: i32) -> i32;
}

pub fn init() {
    match std::env::var_os("KUMU_LOG_TO_FILE") {
        Some(value) if value == "0" => return,
        Some(_) => {}
        None if std::io::stderr().is_terminal() => return,
        None => {}
    }
    let Some(path) = log_path() else {
        return;
    };
    if install(&path) {
        eprintln!(
            "[kumu] {} started (pid {}, log {})",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            path.display()
        );
    }
}

fn log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return None;
    }
    Some(home.join("Library/Logs/Kumu/kumu.log"))
}

fn install(path: &Path) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    rotate(path);
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
        else {
            return false;
        };
        // The duplicate owns fd 2; the original handle can close here.
        unsafe { dup2(file.as_raw_fd(), 2) != -1 }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

fn rotate(path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() <= MAX_LOG_BYTES {
        return;
    }
    let previous = path.with_extension("log.1");
    let _ = std::fs::remove_file(&previous);
    let _ = std::fs::rename(path, previous);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_lives_under_the_users_library_logs() {
        let path = log_path().expect("HOME is set on macOS");
        assert!(path.ends_with("Library/Logs/Kumu/kumu.log"));
    }
}
