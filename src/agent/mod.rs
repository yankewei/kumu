//! The agent layer: which local coding agents kumu can drive, and how.
//!
//! Home sessions launch each agent's interactive TUI ([`interactive`]).
//! Settings probes the same agents over their headless transports to discover
//! model catalogues ([`probe`]). Protocol adapters converge on one
//! [`Event`] vocabulary so nothing above this layer branches on which agent
//! answered.
//!
//! Adapters:
//!   - `acp`      — Agent Client Protocol v1 over stdio (`omp acp`);
//!   - `opencode` — opencode's own v2 API (`opencode serve` on a local port);
//!   - `codex`  — OpenAI Codex `app-server` JSON-RPC over stdio;
//!   - `claude` — Claude Code's headless stream-json protocol over stdio;
//!   - `pi`     — Pi's own RPC mode over stdio.
//!
//! Layout:
//!   - this module  — the facade;
//!   - `event`      — the event vocabulary probes report;
//!   - `config`     — which protocol, and how to launch its subprocess;
//!   - `session`    — shared options-event builder for probes;
//!   - `registry`   — the static list of agents;
//!   - `probe`      — discovery, dispatched per protocol;
//!   - `acp/` / `codex/` / `claude/` / `pi/` / `opencode/` — adapters.

mod acp;
mod claude;
mod codex;
mod config;
mod event;
mod opencode;
mod pi;
mod probe;
mod registry;
mod session;

#[cfg(test)]
mod test_support;

pub use config::{Config, interactive};
pub use event::{Event, ModeInfo, ModelInfo, ReasoningInfo, SandboxInfo};
pub use probe::probe;
pub use registry::AGENTS;
