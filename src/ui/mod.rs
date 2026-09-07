//! Frontend UI: one module per window, shared widgets in `widgets`.

mod home;
mod settings;
pub mod widgets;

pub use home::KumuApp;
pub use settings::SettingsWindow;
