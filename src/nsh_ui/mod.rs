//! Native Shell (`longbridge/nsh`) Pure-Rust UI foundation for Kumu.
//!
//! - `theme`         — maps `themes/*.json` & two-step type scale (`14px`/`12px`) to NSH;
//! - `components`    — styled Kumu wrappers over unstyled `nsh-base` primitives;
//! - `bridge`        — thread-safe `WorkspaceCore` + Host action bridge for NSH views;
//! - `terminal_host` — `NativeComponent` embedding Kumu's GPUI `TerminalView`;
//! - `verify_window` — Phase 2 macOS real-window verification harness (`KUMU_NSH_VERIFY=1`).

pub mod bridge;
pub mod components;
pub mod terminal_host;
pub mod theme;
pub mod verify_window;
