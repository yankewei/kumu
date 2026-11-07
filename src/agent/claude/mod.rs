//! The Claude Code adapter: headless stream-json over stdio, used for settings
//! probes.
//!
//! Layout:
//!   - `protocol`  — the hand-written slice of the wire kumu reads;
//!   - `transport` — newline-delimited JSON over the child's stdio;
//!   - `convert`   — protocol -> app translation;
//!   - `probe`     — session-option discovery for the settings window;
//!   - `settings`  — a read of Claude Code's own configuration files.

pub(super) mod convert;
pub(super) mod probe;
pub(super) mod settings;

mod protocol;
mod transport;
