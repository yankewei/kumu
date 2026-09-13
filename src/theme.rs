//! Theme loading: themes/*.json into the ThemeRegistry ("Kumu Light" / "Zed
//! Dark" as the light/dark defaults), then apply the light theme. App code
//! only reads semantic tokens from `cx.theme()`. Also owns the user's UI font
//! choices (family and size), which sit on top of the loaded theme and must be
//! re-applied whenever the theme is re-applied from its JSON.

use gpui_kit::component::*;
use gpui_kit::*;

pub const THEME_LIGHT: &str = include_str!("../themes/light.json");
pub const THEME_DARK: &str = include_str!("../themes/dark.json");
pub const THEME_LIGHT_NAME: &str = "Kumu Light";
pub const THEME_DARK_NAME: &str = "Zed Dark";

/// The app's default UI font family: the platform system font.
pub const SYSTEM_FONT_FAMILY: &str = ".SystemUIFont";
/// Label for the system-font entry in the settings font picker.
pub const SYSTEM_FONT_LABEL: &str = "系统字体";

/// The UI font size (px) in force when the user has not chosen one:
/// gpui-component's own base, which every window's rem size is derived from.
pub const DEFAULT_FONT_SIZE: f32 = 16.;
/// The sizes the settings picker offers, in px.
pub const FONT_SIZES: [u32; 9] = [12, 13, 14, 15, 16, 17, 18, 19, 20];

/// Load themes/*.json into the registry and make "Kumu Light" / "Zed Dark" the
/// light/dark defaults, then apply the light theme and the user's font.
pub fn load_kumu_themes(cx: &mut App) {
    let (light, dark) = {
        let registry = ThemeRegistry::global_mut(cx);
        let _ = registry.load_themes_from_str(THEME_LIGHT);
        let _ = registry.load_themes_from_str(THEME_DARK);
        (
            registry.themes().get(THEME_LIGHT_NAME).cloned(),
            registry.themes().get(THEME_DARK_NAME).cloned(),
        )
    };
    {
        let theme = Theme::global_mut(cx);
        if let Some(t) = light {
            theme.light_theme = t;
        }
        if let Some(t) = dark {
            theme.dark_theme = t;
        }
    }
    change_mode(ThemeMode::Light, None, cx);
}

/// Switch light/dark, then restore the persisted font choices: `Theme::change`
/// re-applies what the theme JSON defines, which would drop the family.
pub fn change_mode(mode: impl Into<ThemeMode>, window: Option<&mut Window>, cx: &mut App) {
    Theme::change(mode, window, cx);
    apply_saved_font(cx);
    install_text_view_style(cx);
}

/// Set the app-wide UI font family: update the theme global, push it down to
/// the Base layer, and redraw every window so the roots pick it up.
pub fn set_font_family(cx: &mut App, family: &str) {
    Theme::global_mut(cx).font_family = family.into();
    Theme::sync_base(cx);
    install_text_view_style(cx);
    cx.refresh_windows();
}

/// Set the app-wide UI font size, in px.
///
/// Nothing here touches a window's rem size directly: every window renders
/// inside gpui-component's `Root`, which pushes `Theme::font_size` into
/// `Window::set_rem_size` on each frame. The app's layout and type scale are
/// all in rems, so this one value scales the whole UI — text, icons, spacing —
/// in proportion, the way Zed's `ui_font_size` scales its UI.
pub fn set_font_size(cx: &mut App, size: f32) {
    Theme::global_mut(cx).font_size = px(size);
    Theme::sync_base(cx);
    install_text_view_style(cx);
    cx.refresh_windows();
}

/// Re-apply the persisted font choices on top of the current theme.
pub fn apply_saved_font(cx: &mut App) {
    if let Some(family) = crate::db::get_setting(crate::db::SETTING_FONT) {
        set_font_family(cx, &family);
    }
    let size = crate::db::get_setting(crate::db::SETTING_FONT_SIZE)
        .and_then(|size| size.parse().ok())
        .unwrap_or(DEFAULT_FONT_SIZE);
    set_font_size(cx, size);
}

/// Markdown look for chat replies: the component theme's colors plus a wider
/// paragraph gap. Installed after every theme/base sync because gpui-component
/// resets the global defaults whenever it re-applies the theme.
fn install_text_view_style(cx: &mut App) {
    let style =
        base::TextViewStyle::from_theme(&base::Theme::global(cx)).with_paragraph_gap(rems(1.25));
    base::TextViewDefaults::new().with_style(style).install(cx);
}

/// The font picker's items: the system default first, then every installed,
/// user-visible family sorted. Hidden CoreText families (".SF NS …") are
/// skipped — gpui resolves ".SystemUIFont" to the real system font itself.
pub fn system_fonts(cx: &App) -> Vec<(SharedString, SharedString)> {
    let mut families: Vec<SharedString> = cx
        .text_system()
        .all_font_names()
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .map(SharedString::from)
        .collect();
    families.sort();
    families.dedup();

    let mut items = vec![(
        SharedString::from(SYSTEM_FONT_LABEL),
        SharedString::from(SYSTEM_FONT_FAMILY),
    )];
    items.extend(families.into_iter().map(|family| {
        let label = family.clone();
        (label, family)
    }));
    items
}

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use core::prelude::v1::test;

    use super::{DEFAULT_FONT_SIZE, FONT_SIZES};

    /// The settings picker preselects by matching the size in force against an
    /// offered one, so the default has to be on the list or it would open with
    /// nothing selected.
    #[test]
    fn the_default_font_size_is_offered_by_the_picker() {
        assert!(FONT_SIZES.contains(&(DEFAULT_FONT_SIZE as u32)));
    }
}
