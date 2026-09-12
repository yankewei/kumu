//! The agent backend layer: how kumu drives local coding agents.
//!
//! Every backend converges on one vocabulary ([`Event`] out, [`Cmd`] in),
//! so no protocol type reaches the UI and no UI branch depends on which
//! agent is talking. Which backend an agent uses is decided in
//! [`Config::for_agent`] and dispatched in [`spawn`] and [`probe`].
//!
//! Backends:
//!   - `acp`    — Agent Client Protocol v1 over stdio (`opencode acp`, `omp acp`);
//!   - `codex`  — OpenAI Codex `app-server` JSON-RPC over stdio;
//!   - `claude` — Claude Code's headless stream-json protocol over stdio.
//!
//! Runs on a dedicated tokio runtime thread. UI -> here via an unbounded tokio
//! channel (send is non-blocking, safe from the gpui thread); here -> UI via a
//! std mpsc channel that the gpui side polls.
//!
//! Permission requests are the one thing that travels back on a different path:
//! they are answered through [`Connection::resolve_permission`], not the command
//! channel. Every backend faces the same hazard — its transport is parked
//! waiting for the answer — so the table and the strong/weak split that makes
//! teardown safe are shared, and documented alongside it in `connection`.
//!
//! Layout, split by responsibility:
//!   - this module  — the facade: everything the UI is allowed to name;
//!   - `event`      — the event/command vocabulary and the value types;
//!   - `config`     — which backend, and how to launch its subprocess;
//!   - `connection` — the UI-side handle, the permission table, the thread;
//!   - `probe`      — discovery, dispatched to the backend that owns the agent;
//!   - `acp/`       — the ACP backend (session, probe, protocol translation);
//!   - `codex/`     — the Codex app-server backend;
//!   - `claude/`    — the Claude Code backend.

mod acp;
mod claude;
mod codex;
mod config;
mod connection;
mod event;
mod probe;

#[cfg(test)]
mod test_support;

pub use config::Config;
pub use connection::{Connection, spawn};
pub use event::{
    Cmd, Event, ModeInfo, ModelInfo, PermissionKind, PermissionOptionInfo, ReasoningInfo,
    SandboxInfo,
};
pub use probe::probe;
