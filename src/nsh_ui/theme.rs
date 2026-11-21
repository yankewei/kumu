//! Maps Kumu's active theme (`themes/light.json` & `themes/dark.json`) to NSH
//! semantic theme tokens and enforces Kumu's two-step type scale (`AGENTS.md`).

use gpui_kit::Hsla;
use gpui_kit::component::{Theme, ThemeMode};
use nsh_core::Points;
use nsh_platform_gpui::{AdaptiveColor, Appearance};

/// Body text size (14px): section titles, main empty states, primary content.
pub const TEXT_SM: Points = Points(14.0);

/// Secondary/meta text size (12px): sidebar rows, labels, pills, captions.
pub const TEXT_XS: Points = Points(12.0);

fn convert_color(c: Hsla) -> nsh_platform_gpui::Hsla {
    nsh_platform_gpui::Hsla {
        h: c.h,
        s: c.s,
        l: c.l,
        a: c.a,
    }
}

/// Convert Kumu's light/dark mode to NSH's [`Appearance`].
pub fn appearance(mode: ThemeMode) -> Appearance {
    if mode.is_dark() {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

/// Convert Kumu's active GPUI Kit [`Theme`] into an [`nsh_platform_gpui::Theme`].
///
/// Called on initial mount and whenever the user switches theme mode or UI font
/// in Settings, keeping NSH views visually indistinguishable from Kumu's chrome.
pub fn nsh_theme(theme: &Theme) -> nsh_platform_gpui::Theme {
    let mut nsh = nsh_platform_gpui::Theme::default();
    let colors = &theme.colors;
    for (name, c) in [
        ("background", colors.background),
        ("foreground", colors.foreground),
        ("card", colors.group_box),
        ("card-foreground", colors.group_box_foreground),
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
    ] {
        nsh.set_color(name, AdaptiveColor::from(convert_color(c)));
    }
    nsh.set_number("radius", f32::from(theme.radius));
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
        let mut theme = Theme::default();
        theme.mode = ThemeMode::Dark;
        theme.radius = gpui_kit::px(6.0);
        theme.colors.primary = gpui_kit::rgb(0x2563eb).into();

        let mapped = nsh_theme(&theme);
        assert_eq!(appearance(theme.mode), Appearance::Dark);
        assert_eq!(Color::from(mapped.primary.dark), Color(0x2563ebff));
        assert_eq!(mapped.radius, Points(6.0));
    }
}
