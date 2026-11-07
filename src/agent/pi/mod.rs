//! The Pi adapter: `pi --mode rpc` over stdio, used for settings probes.
//!
//! Layout:
//!   - `transport` — newline-delimited JSON over the child's stdio;
//!   - `convert`   — protocol -> app translation;
//!   - `probe`     — session-option discovery for the settings window.

pub(super) mod convert;
pub(super) mod probe;

mod transport;
