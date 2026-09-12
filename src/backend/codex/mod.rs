//! The Codex backend: OpenAI Codex's `app-server` JSON-RPC over stdio.
//!
//! A separate protocol from ACP, but the same contract with the rest of kumu:
//! it produces [`Event`]s, consumes [`Cmd`]s, and answers approvals through the
//! shared permission table, so nothing above `backend` knows which agent is
//! talking.
//!
//! Layout:
//!   - `protocol`  — the hand-written slice of the app-server schema we read;
//!   - `transport` — the wire: newline-delimited JSON over the child's stdio;
//!   - `convert`   — protocol -> app translation, where protocol types stop;
//!   - `session`   — handshake, the single thread, and the command loop;
//!   - `probe`     — model discovery (`model/list`) for the settings window.
//!
//! [`Event`]: crate::backend::Event
//! [`Cmd`]: crate::backend::Cmd

pub(super) mod probe;
pub(super) mod session;

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
