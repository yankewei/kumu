//! Maps Kumu's active [`KumuTheme`] (`themes/light.json` & `themes/dark.json`)
//! to NSH semantic theme tokens and enforces Kumu's two-step type scale (`AGENTS.md`).

use gpui::Hsla;
use nsh_base::theme as active_nsh_theme;
use nsh_core::Points;
use nsh_platform_gpui::{AdaptiveColor, Appearance};

use crate::theme::{DEFAULT_FONT_SIZE, KumuTheme, ThemeMode};

/// Base body text size (14px at 16px root): section titles, main empty states, primary content.
pub const TEXT_SM: Points = Points(14.0);

/// Base secondary/meta text size (12px at 16px root): sidebar rows, labels, pills, captions.
pub const TEXT_XS: Points = Points(12.0);

/// Custom token names layered on top of shadcn's standard NSH theme tokens.
pub const TOKEN_CARD_SURFACE: &str = "kumu-card";
pub const TOKEN_ICON_TILE: &str = "kumu-tile";
pub const TOKEN_LINK: &str = "kumu-link";
pub const TOKEN_WARNING: &str = "kumu-warning";
pub const TOKEN_SUCCESS: &str = "kumu-success";
pub const TOKEN_ACCENT_PILL_BG: &str = "kumu-accent-pill-bg";
pub const TOKEN_UI_SCALE: &str = "kumu-ui-scale";

fn convert_color(c: Hsla) -> nsh_platform_gpui::Hsla {
    nsh_platform_gpui::Hsla {
        h: c.h,
        s: c.s,
        l: c.l,
        a: c.a,
    }
}

/// Reactive body text size (`14px` scaled by the user's UI font-size setting).
pub fn text_sm_size() -> Points {
    let scale = active_nsh_theme().number(TOKEN_UI_SCALE).unwrap_or(1.0);
    Points(TEXT_SM.0 * scale)
}

/// Reactive secondary/meta text size (`12px` scaled by the user's UI font-size setting).
pub fn text_xs_size() -> Points {
    let scale = active_nsh_theme().number(TOKEN_UI_SCALE).unwrap_or(1.0);
    Points(TEXT_XS.0 * scale)
}

/// Reactive lookup of a Kumu custom color token with a fallback semantic token.
pub fn kumu_color(name: &str, fallback: fn(&nsh_base::Theme) -> AdaptiveColor) -> AdaptiveColor {
    let t = active_nsh_theme();
    t.color(name).unwrap_or_else(|| fallback(&t))
}

/// Convert Kumu's light/dark mode to NSH's [`Appearance`].
pub fn appearance(mode: ThemeMode) -> Appearance {
    if mode.is_dark() {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

/// Convert Kumu's active [`KumuTheme`] into an [`nsh_platform_gpui::Theme`].
///
/// Called on initial mount and whenever the user switches theme mode or UI font
/// in Settings, keeping all NSH views visually aligned with `themes/*.json`.
pub fn nsh_theme(theme: &KumuTheme) -> nsh_platform_gpui::Theme {
    let mut nsh = nsh_platform_gpui::Theme::default();
    let colors = &theme.colors;
    let dark = theme.mode.is_dark();
    let card_surface = if dark { colors.muted } else { colors.popover };
    let icon_tile = if dark {
        colors.background
    } else {
        colors.muted
    };

    for (name, c) in [
        ("background", colors.background),
        ("foreground", colors.foreground),
        ("card", card_surface),
        ("card-foreground", colors.foreground),
        ("popover", colors.popover),
        ("popover-foreground", colors.popover_foreground),
        ("primary", colors.primary),
        ("primary-foreground", colors.primary_foreground),
        ("secondary", colors.secondary),
        ("secondary-foreground", colors.secondary_foreground),
        ("muted", colors.muted),
        ("muted-foreground", colors.muted_foreground),
        ("accent", colors.accent),
        ("accent-foreground", colors.accent_foreground),
        ("destructive", colors.danger),
        ("border", colors.border),
        ("input", colors.input),
        ("ring", colors.ring),
        ("sidebar", colors.sidebar),
        ("sidebar-foreground", colors.sidebar_foreground),
        ("sidebar-primary", colors.sidebar_primary),
        (
            "sidebar-primary-foreground",
            colors.sidebar_primary_foreground,
        ),
        ("sidebar-accent", colors.sidebar_accent),
        (
            "sidebar-accent-foreground",
            colors.sidebar_accent_foreground,
        ),
        ("sidebar-border", colors.sidebar_border),
        ("sidebar-ring", colors.ring),
        (TOKEN_CARD_SURFACE, card_surface),
        (TOKEN_ICON_TILE, icon_tile),
        (TOKEN_LINK, colors.link),
        (TOKEN_WARNING, colors.warning),
        (TOKEN_SUCCESS, colors.success),
        (TOKEN_ACCENT_PILL_BG, colors.primary.opacity(0.14)),
    ] {
        nsh.set_color(name, AdaptiveColor::from(convert_color(c)));
    }
    nsh.set_number("radius", theme.radius);
    let scale = (theme.font_size / DEFAULT_FONT_SIZE).clamp(0.625, 2.0);
    nsh.set_number(TOKEN_UI_SCALE, scale);
    nsh.set_font(nsh_core::element::FONT_SANS, theme.font_family.to_string());
    nsh.set_font(
        nsh_core::element::FONT_MONO,
        theme.mono_font_family.to_string(),
    );
    nsh
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_platform_gpui::Color;

    #[test]
    fn theme_mapping_preserves_colors_radius_and_appearance() {
        let mut theme = KumuTheme::load_bundled();
        theme.mode = ThemeMode::Dark;
        theme.radius = 6.0;
        theme.font_size = 18.0;
        theme.colors.primary = gpui::rgb(0x2563eb).into();

        let mapped = nsh_theme(&theme);
        assert_eq!(appearance(theme.mode), Appearance::Dark);
        assert_eq!(Color::from(mapped.primary.dark), Color(0x2563ebff));
        assert_eq!(mapped.radius, Points(6.0));
        assert!((mapped.number(TOKEN_UI_SCALE).unwrap() - 18.0 / 16.0).abs() < 1e-4);
    }
}
