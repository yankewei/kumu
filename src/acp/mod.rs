//! ACP integration: talks v1 ACP over stdio to a local agent process (`opencode acp`).
//!
//! Runs on a dedicated tokio runtime thread. UI -> here via an unbounded tokio
//! channel (send is non-blocking, safe from the gpui thread); here -> UI via a
//! std mpsc channel that the gpui side polls.
//!
//! Permission requests are the one thing that travels back on a different path:
//! they are answered through [`Connection::resolve_permission`], not the command
//! channel. The reasoning, and the strong/weak split that makes teardown safe,
//! is documented alongside the table in `connection`.
//!
//! Layout, split by responsibility:
//!   - this module  — the facade: everything the UI is allowed to name;
//!   - `event`      — the event/command vocabulary and the value types;
//!   - `config`     — how to launch an agent subprocess;
//!   - `connection` — the UI-side handle, the permission table, the thread;
//!   - `session`    — the live session loop and its two handlers;
//!   - `probe`      — the throwaway connection that discovers models;
//!   - `convert`    — protocol -> app translation, where protocol types stop.

mod config;
mod connection;
mod convert;
mod event;
mod probe;
mod session;

#[cfg(test)]
mod test_support;

pub use config::Config;
pub use connection::{Connection, spawn};
pub use event::{Cmd, Event, ModelInfo, PermissionKind, PermissionOptionInfo};
pub use probe::probe;
