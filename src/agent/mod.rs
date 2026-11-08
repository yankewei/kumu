//! The agent layer: which local coding agents kumu can launch, and where to
//! find them.
//!
//! Sessions run each agent's own interactive TUI in a PTY; kumu does not speak
//! any agent protocol itself. `disk_title` reads the names agents write to
//! their own on-disk stores so the sidebar can show them.
//!
//! Layout:
//!   - this module  — the facade;
//!   - `config`     — executable resolution and install detection;
//!   - `registry`   — the static list of agents;
//!   - `disk_title` — titles from the agents' session stores.

mod config;
mod disk_title;
mod registry;

pub use config::{installed, interactive};
pub use disk_title::{
    DiskTitle, discover as discover_disk_title, lookup as lookup_disk_title,
    resume_belongs_to_terminal,
};
pub use registry::AGENTS;
