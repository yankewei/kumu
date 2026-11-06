//! The Claude Code backend: Claude Code's own headless protocol over stdio.
//!
//! Neither JSON-RPC nor ACP. A single long-lived process is launched as
//! `claude -p --input-format stream-json --output-format stream-json --verbose`,
//! which reads newline-delimited user messages on stdin and writes
//! newline-delimited events on stdout. The same contract with the rest of kumu
//! as every other backend: it produces [`Event`]s, consumes [`Cmd`]s, and
//! answers approvals through the shared permission table, so nothing above
//! `backend` knows which agent is talking.
//!
//! Layout:
//!   - `protocol`  — the hand-written slice of the wire kumu reads, pinned to a
//!     captured session under `schema/claude-stream-json/`;
//!   - `transport` — the wire: newline-delimited JSON over the child's stdio,
//!     plus the control-request round trip;
//!   - `convert`   — protocol -> app translation, where protocol types stop;
//!   - `session`   — handshake, the message pump, and the command loop;
//!   - `probe`     — session-option discovery for the settings window;
//!   - `settings`  — a read of Claude Code's own configuration files, which is
//!     where the axes the wire does not carry come from.
//!
//! [`Event`]: crate::agent::Event
//! [`Cmd`]: crate::agent::Cmd

pub(super) mod convert;
pub(super) mod probe;
pub(super) mod session;
pub(super) mod settings;

mod protocol;
mod transport;

use crate::agent::config::SessionPicks;

/// The argv a session's picks become.
///
/// Claude Code takes every axis on the command line, so this is where a pick
/// turns into a flag — and where an axis the user never chose emits nothing,
/// since an unset value means the agent's own configuration stays in charge.
///
/// There is no sandbox here on purpose: Claude's sandbox is an OS-level boundary
/// configured through its own settings, and the protocol carries no per-session
/// form for it (checked against 2.1.269 — every plausible control subtype is
/// rejected). `mode` is therefore the session's permission mode as stored, which
/// is the single value behind both axes: the work mode (`plan` / `default`) and
/// the access policy (the approval ladder).
pub(super) fn session_args(picks: &SessionPicks) -> Vec<String> {
    let mut args = Vec::new();
    for (flag, value) in [
        ("--model", &picks.model),
        ("--permission-mode", &picks.mode),
        ("--effort", &picks.reasoning),
    ] {
        if let Some(value) = value {
            args.push(flag.to_string());
            args.push(value.clone());
        }
    }
    args
}
