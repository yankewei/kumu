//! Theme loading: themes/*.json into the ThemeRegistry ("Kumu Light" / "Zed
//! Dark" as the light/dark defaults), then apply the light theme. App code
//! only reads semantic tokens from `cx.theme()`.

use gpui_kit::component::*;
use gpui_kit::*;

pub const THEME_LIGHT: &str = include_str!("../themes/zed-light.json");
pub const THEME_DARK: &str = include_str!("../themes/zed-dark.json");
pub const THEME_LIGHT_NAME: &str = "Kumu Light";
pub const THEME_DARK_NAME: &str = "Zed Dark";

/// Load themes/*.json into the registry and make "Kumu Light" / "Zed Dark" the
/// light/dark defaults, then apply the light theme.
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
    Theme::change(ThemeMode::Light, None, cx);
}
