//! Native Shell (`longbridge/nsh`) Pure-Rust UI foundation for Kumu.
//!
//! - `assets`        — bridges embedded SVG/PNG assets (`KumuAssetSource`) into NSH;
//! - `theme`         — maps `themes/*.json` & two-step type scale (`14px`/`12px`) to NSH;
//! - `components`    — styled Kumu wrappers over unstyled `nsh-base` primitives;
//! - `bridge`        — thread-safe `WorkspaceCore` + Host action bridge for NSH views;
//! - `dialogs`       — full-window Pure Rust NSH modal dialogs (`ActiveDialog`);
//! - `terminal_host` — `NativeComponent` embedding Kumu's GPUI `TerminalView`;
//! - `settings`      — Pure Rust NSH Settings window (`SettingsWindow`);
//! - `verify_window` — Phase 2 macOS real-window verification harness (`KUMU_NSH_VERIFY=1`).

pub mod assets;
pub mod bridge;
pub mod components;
pub mod dialogs;
pub mod settings;
pub mod terminal_host;
pub mod theme;
pub mod verify_window;
