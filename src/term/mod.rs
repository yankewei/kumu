//! One process and the cells its TUI has drawn.
//!
//! Used for both an agent's interactive TUI and a plain login shell. `pty`
//! starts the process. `vt` turns the byte stream into a grid and answers
//! terminal queries. `session` owns both on one thread so a query reply is
//! written back before the next read.

mod pty;
mod session;
mod vt;

pub use session::Term;
pub use vt::{Cell, CursorShape, KeyInput, Rgb, Snapshot, TermColors};

/// Stored on [`crate::db::Session::agent`] for a pure shell session — not an
/// agent, and not listed in [`crate::agent::AGENTS`].
pub const SHELL: &str = "shell";

/// Whether this session id is a pure shell rather than an agent TUI.
pub fn is_shell(agent: &str) -> bool {
    agent == SHELL
}

/// The user's login shell: `$SHELL`, falling back to `/bin/sh`.
pub fn default_shell() -> (String, Vec<String>) {
    let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    (program, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_id_is_reserved() {
        assert!(is_shell(SHELL));
        assert!(!is_shell("omp"));
        assert!(!is_shell(""));
    }

    #[test]
    fn default_shell_has_a_program() {
        let (program, args) = default_shell();
        assert!(!program.is_empty());
        assert!(args.is_empty());
    }
}
