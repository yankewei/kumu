//! The agent layer: which local coding agents kumu can drive, and how.
//!
//! Every backend converges on one vocabulary ([`Event`] out, [`Cmd`] in),
//! so no protocol type reaches the UI and no UI branch depends on which
//! agent is talking. Which backend an agent uses is decided in
//! [`Config::for_agent`] and dispatched in [`spawn`] and [`probe`].
//!
//! Backends:
//!   - `acp`      — Agent Client Protocol v1 over stdio (`omp acp`);
//!   - `opencode` — opencode's own v2 API: a private `opencode serve` per
//!     session, over HTTP and server-sent events on a local port;
//!   - `codex`  — OpenAI Codex `app-server` JSON-RPC over stdio;
//!   - `claude` — Claude Code's headless stream-json protocol over stdio;
//!   - `pi`     — Pi's own RPC mode over stdio.
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
//!   - `session`    — the contract the four session loops share;
//!   - `registry`   — the static list of agents, what the UI shows for each;
//!   - `probe`      — discovery, dispatched to the backend that owns the agent;
//!   - `acp/`       — the ACP backend (session, probe, protocol translation);
//!   - `codex/`     — the Codex app-server backend;
//!   - `claude/`    — the Claude Code backend;
//!   - `pi/`        — the Pi backend;
//!   - `opencode/`  — the opencode v2 backend.

mod acp;
mod claude;
mod codex;
mod config;
mod connection;
mod event;
mod opencode;
mod pi;
mod probe;
mod registry;
mod session;

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod resume_tests;

pub use config::{Config, SessionPicks, interactive};
pub use connection::{Connection, spawn};
pub use event::{
    Cmd, Event, ImageAttachment, ModeInfo, ModelInfo, Prompt, ReasoningInfo, SandboxInfo, Usage,
};
pub use probe::probe;
pub use registry::AGENTS;
