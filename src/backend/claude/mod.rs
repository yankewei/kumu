//! The Claude Code backend: Claude Code's own headless protocol over stdio.
//!
//! Neither JSON-RPC nor ACP. A single long-lived process is launched as
//! `claude -p --input-format stream-json --output-format stream-json
//! --verbose`, which reads newline-delimited user messages on stdin and writes
//! newline-delimited events on stdout. The same contract with the rest of kumu
//! as every other backend: it produces [`Event`]s, consumes [`Cmd`]s, and
//! answers approvals through the shared permission table, so nothing above
//! `backend` knows which agent is talking.
//!
//! Layout:
//!   - `probe`   — session-option discovery for the settings window;
//!   - `convert` — the fixed vocabularies the pickers offer.
//!
//! Session options are the whole of this backend so far: the session loop, the
//! event mapping and the permission path are not implemented yet, so `spawn`
//! reports that plainly instead of starting a subprocess it cannot drive.
//!
//! [`Event`]: crate::backend::Event
//! [`Cmd`]: crate::backend::Cmd

pub(super) mod convert;
pub(super) mod probe;
