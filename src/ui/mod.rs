//! Frontend UI: Home window, native macOS title bar, and Pure Rust NSH Settings window.

pub(crate) mod home;
pub(crate) mod titlebar;

pub use crate::nsh_ui::settings::SettingsWindow;
pub use home::KumuApp;

pub fn init(cx: &mut gpui::App) {
    if !cx.has_global::<crate::app::NotificationSettings>() {
        cx.set_global(crate::app::NotificationSettings {
            enabled: crate::db::get_setting(crate::db::SETTING_NOTIFICATIONS).as_deref()
                != Some("0"),
            sound: crate::db::get_setting(crate::db::SETTING_NOTIFICATION_SOUND).as_deref()
                != Some("0"),
        });
    }
    if !cx.has_global::<crate::app::OnboardingRequested>() {
        cx.set_global(crate::app::OnboardingRequested::default());
    }
    // Let the focused terminal receive completion keys through its normal encoder
    // instead of any outer focus-navigation binding.
    cx.bind_keys([
        gpui::KeyBinding::new("tab", gpui::NoAction, Some("Terminal")),
        gpui::KeyBinding::new("shift-tab", gpui::NoAction, Some("Terminal")),
    ]);
}

#[cfg(test)]
mod tests {
    use gpui::{KeyBinding, KeyContext, Keystroke, TestAppContext};

    gpui::actions!(test_nav, [FocusNext, FocusPrev]);

    #[gpui::test]
    fn terminal_completion_keys_bypass_root_focus_navigation(cx: &mut TestAppContext) {
        cx.update(|cx| {
            // Simulate an outer focus-navigation binding on `Root` so the test
            // verifies `super::init` actually masks it inside `Terminal`.
            cx.bind_keys([
                KeyBinding::new("tab", FocusNext, Some("Root")),
                KeyBinding::new("shift-tab", FocusPrev, Some("Root")),
            ]);
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

                let (outer_bindings, _) =
                    keymap.bindings_for_input(&input, std::slice::from_ref(&root));
                assert_eq!(
                    outer_bindings.len(),
                    1,
                    "{key} still navigates outside terminals"
                );
            }
        });
    }
}
