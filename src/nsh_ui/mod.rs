//! Native Shell (`longbridge/nsh`) Pure-Rust UI foundation for Kumu.
//!
//! - `assets`     — bridges embedded SVG/PNG assets (`KumuAssetSource`) into NSH;
//! - `theme`      — maps `themes/*.json` & two-step type scale (`14px`/`12px`) to NSH;
//! - `components` — styled Kumu wrappers over unstyled `nsh-base` primitives;
//! - `dialogs`    — full-window Pure Rust NSH modal dialogs (`ActiveDialog`);
//! - `toast`      — Pure Rust NSH foreground toast stack (`nsh_base::toast_stack`);
//! - `settings`   — Pure Rust NSH Settings window (`SettingsWindow`).

pub mod assets;
pub mod components;
pub mod dialogs;
pub mod settings;
pub mod theme;
pub mod toast;
