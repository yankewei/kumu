//! The agent layer: which local coding agents kumu can launch, and where to
//! find them.
//!
//! Agents run as their own interactive TUI inside a project pane. Kumu does
//! not speak any agent protocol itself. `integration` registers persistent
//! notification hooks and extensions for these TUIs.
//!
//! Layout:
//!   - this module  — the facade;
//!   - `config`     — executable resolution and install detection;
//!   - `launch`     — pane launch choice and resume arguments;
//!   - `registry`   — the static list of agents;
//!   - `integration` — persistent notification registration.

mod config;
pub mod integration;
pub mod launch;
mod registry;

pub use config::{executable, installed};
pub use registry::{AGENTS, id_for};
