//! The Pi backend: Pi's own RPC protocol over stdio.
//!
//! Neither JSON-RPC nor ACP. A single long-lived process is launched as
//! `pi --mode rpc --approve`, which reads newline-delimited commands on stdin
//! and writes newline-delimited replies and events on stdout, and holds the
//! whole conversation. The same contract with the rest of kumu as every other
//! backend: it produces [`Event`]s and consumes [`Cmd`]s, so nothing above
//! `backend` knows which agent is talking.
//!
//! Pi has no permission system at all, so unlike ACP, Codex and Claude there is
//! no approval to route: the shared permission table stays empty, and the access
//! axis is reported as absent rather than degraded.
//!
//! Layout:
//!   - `transport` — the wire: newline-delimited JSON over the child's stdio,
//!     plus the command/reply round trip;
//!   - `convert`   — protocol -> app translation, where protocol shapes stop;
//!   - `session`   — the handshake, the frame pump, and the command loop;
//!   - `probe`     — session-option discovery for the settings window.
//!
//! There is deliberately no `protocol` module here, unlike the Claude Code
//! backend: Pi's frames are shallow enough to read as they arrive, so there is
//! no wire slice worth pinning as types. What *was* pinned is the behaviour
//! checked against the real CLI, kept under `schema/pi-rpc/`.
//!
//! [`Event`]: crate::agent::Event
//! [`Cmd`]: crate::agent::Cmd

pub(super) mod convert;
pub(super) mod probe;
pub(super) mod session;

mod transport;
