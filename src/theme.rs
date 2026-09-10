//! Theme loading: themes/*.json into the ThemeRegistry ("Kumu Light" / "Zed
//! Dark" as the light/dark defaults), then apply the light theme. App code
//! only reads semantic tokens from `cx.theme()`. Also owns the user's UI
//! font choice, which sits on top of the loaded theme and must be re-applied
//! whenever the theme is re-applied from its JSON.

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

/// Switch light/dark, then restore the persisted UI font: `Theme::change`
/// resets `font_family` from the theme JSON, which would drop the choice.
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

/// Re-apply the persisted font on top of the current theme.
pub fn apply_saved_font(cx: &mut App) {
    if let Some(family) = crate::db::get_setting(crate::db::SETTING_FONT) {
        set_font_family(cx, &family);
    }
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
