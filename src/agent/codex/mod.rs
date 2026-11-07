//! The Codex adapter: `app-server` JSON-RPC over stdio, used for settings probes.
//!
//! Layout:
//!   - `protocol`  — the hand-written slice of the app-server schema we read;
//!   - `transport` — newline-delimited JSON over the child's stdio;
//!   - `convert`   — protocol -> app translation;
//!   - `probe`     — model and skill discovery (`model/list`, `skills/list`).

pub(super) mod probe;

mod convert;
mod protocol;
mod transport;

/// The client name app-server records for compliance logging; the docs ask an
/// integration to identify itself here.
const CLIENT_NAME: &str = "kumu";

/// The `initialize` params identifying kumu to app-server.
pub(super) fn client_info() -> serde_json::Value {
    serde_json::json!({
        "clientInfo": {
            "name": CLIENT_NAME,
            "title": "kumu",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}
