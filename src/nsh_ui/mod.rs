//! Native Shell (`longbridge/nsh`) Pure-Rust UI foundation for Kumu.
//!
//! - `assets`     — bridges embedded SVG/PNG assets (`KumuAssetSource`) into NSH;
//! - `theme`      — maps `themes/*.json` & two-step type scale (`14px`/`12px`) to NSH;
//! - `components` — styled Kumu wrappers over unstyled `nsh-base` primitives;
//! - `mount`           — generic one-shot mount registry for NSH root payloads;
//! - `dialogs`         — full-window Pure Rust NSH modal dialogs (`ActiveDialog`);
//! - `agent_picker`    — anchored new-agent popover over a pane header (`ActiveAgentPicker`);
//! - `command_palette` — full-window Pure Rust NSH `⌘K` quick switcher & command palette;
//! - `toast`           — Pure Rust NSH foreground toast stack (`nsh_base::toast_stack`);
//! - `settings`        — Pure Rust NSH Settings window (`SettingsWindow`).

pub mod agent_picker;
pub mod assets;
pub mod command_palette;
pub mod components;
pub mod dialogs;
pub mod mount;
pub mod onboarding;
pub mod settings;
pub mod theme;
pub mod toast;
