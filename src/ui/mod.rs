//! Frontend UI: one module per window, shared widgets in `widgets`.

mod home;
mod layers;
mod settings;
mod tooltip;
pub mod widgets;

pub use home::KumuApp;
pub use settings::SettingsWindow;

pub fn init(cx: &mut gpui_kit::App) {
    if !cx.has_global::<crate::app::NotificationSettings>() {
        cx.set_global(crate::app::NotificationSettings {
            enabled: crate::db::get_setting(crate::db::SETTING_NOTIFICATIONS).as_deref()
                != Some("0"),
            sound: crate::db::get_setting(crate::db::SETTING_NOTIFICATION_SOUND).as_deref()
                != Some("0"),
        });
    }
    // Root's focus-navigation actions run before on_key_down. Let the
    // focused terminal receive completion keys through its normal encoder.
    cx.bind_keys([
        gpui_kit::KeyBinding::new("tab", gpui_kit::NoAction, Some("Terminal")),
        gpui_kit::KeyBinding::new("shift-tab", gpui_kit::NoAction, Some("Terminal")),
    ]);
}

#[cfg(test)]
mod tests {
    use gpui_kit::{KeyContext, Keystroke, TestAppContext};

    #[gpui::test]
    fn terminal_completion_keys_bypass_root_focus_navigation(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            super::init(cx);
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            let root = KeyContext::parse("Root").expect("root context");
            let terminal = KeyContext::parse("Terminal").expect("terminal context");
            for key in ["tab", "shift-tab"] {
                let input = [Keystroke::parse(key).expect("completion key")];
                let (bindings, pending) =
                    keymap.bindings_for_input(&input, &[root.clone(), terminal.clone()]);
                assert!(bindings.is_empty(), "{key} must reach on_key_down");
                assert!(!pending, "{key} must not wait for another keystroke");

                let (bindings, _) = keymap.bindings_for_input(&input, std::slice::from_ref(&root));
                assert_eq!(bindings.len(), 1, "{key} still navigates outside terminals");
            }
        });
    }
}
