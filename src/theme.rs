//! Theme loading: parses `themes/light.json` and `themes/dark.json` directly
//! into Kumu's own [`KumuTheme`] global (zero `gpui-kit` `ThemeRegistry`
//! dependency), drives both NSH (`src/nsh_ui/theme.rs`) and Ghostty VT terminal
//! colors, and persists light/dark mode plus UI font family & size.

use std::collections::HashMap;

use gpui::{App, Global, Hsla, SharedString, Window, hsla, rgba};
use serde::Deserialize;

use crate::term::{Rgb, TermColors};

#[derive(Default)]
pub(crate) struct ThemeRevision(pub u64);

impl Global for ThemeRevision {}

pub const THEME_LIGHT: &str = include_str!("../themes/light.json");
pub const THEME_DARK: &str = include_str!("../themes/dark.json");
pub const THEME_LIGHT_NAME: &str = "Kumu Light";
pub const THEME_DARK_NAME: &str = "Zed Dark";

/// The app's default UI font family: the platform system font.
pub const SYSTEM_FONT_FAMILY: &str = ".SystemUIFont";
/// Label for the system-font entry in the settings font picker.
pub const SYSTEM_FONT_LABEL: &str = "系统字体";

/// The UI font size (px) in force when the user has not chosen one.
pub const DEFAULT_FONT_SIZE: f32 = 16.;
/// The sizes the settings picker offers, in px.
pub const FONT_SIZES: [u32; 9] = [12, 13, 14, 15, 16, 17, 18, 19, 20];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeMode {
    #[default]
    Light,
    Dark,
}

impl ThemeMode {
    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }
}

/// Semantic color palette loaded from `themes/*.json`.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ThemeColors {
    pub background: Hsla,
    pub foreground: Hsla,
    pub border: Hsla,
    pub muted: Hsla,
    pub muted_foreground: Hsla,
    pub secondary: Hsla,
    pub secondary_foreground: Hsla,
    pub primary: Hsla,
    pub primary_foreground: Hsla,
    pub accent: Hsla,
    pub accent_foreground: Hsla,
    pub link: Hsla,
    pub ring: Hsla,
    pub selection: Hsla,
    pub sidebar: Hsla,
    pub sidebar_foreground: Hsla,
    pub sidebar_border: Hsla,
    pub sidebar_accent: Hsla,
    pub sidebar_accent_foreground: Hsla,
    pub sidebar_primary: Hsla,
    pub sidebar_primary_foreground: Hsla,
    pub input: Hsla,
    pub popover: Hsla,
    pub popover_foreground: Hsla,
    pub title_bar: Hsla,
    pub title_bar_border: Hsla,
    pub tab_bar: Hsla,
    pub tab_active: Hsla,
    pub tab_active_foreground: Hsla,
    pub tab_foreground: Hsla,
    pub caret: Hsla,
    pub danger: Hsla,
    pub warning: Hsla,
    pub success: Hsla,
    pub info: Hsla,
    pub red: Hsla,
    pub green: Hsla,
    pub yellow: Hsla,
    pub blue: Hsla,
    pub magenta: Hsla,
    pub cyan: Hsla,
    pub red_light: Hsla,
    pub green_light: Hsla,
    pub yellow_light: Hsla,
    pub blue_light: Hsla,
    pub magenta_light: Hsla,
    pub cyan_light: Hsla,
    pub transparent: Hsla,
}

#[derive(Clone, Debug)]
struct ThemePreset {
    mode: ThemeMode,
    colors: ThemeColors,
    radius: f32,
    font_family: SharedString,
    mono_font_family: SharedString,
    mono_font_size: f32,
}

/// Kumu's active application theme global.
#[derive(Clone, Debug)]
pub struct KumuTheme {
    pub mode: ThemeMode,
    pub colors: ThemeColors,
    pub radius: f32,
    pub font_family: SharedString,
    pub font_size: f32,
    pub mono_font_family: SharedString,
    pub mono_font_size: f32,
    light_preset: ThemePreset,
    dark_preset: ThemePreset,
}

impl Global for KumuTheme {}

impl KumuTheme {
    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    pub fn global_mut(cx: &mut App) -> &mut Self {
        cx.global_mut::<Self>()
    }

    /// Parse both bundled theme JSON files and build the initial light theme.
    pub fn load_bundled() -> Self {
        let light_preset = parse_theme_json(THEME_LIGHT, THEME_LIGHT_NAME, ThemeMode::Light);
        let dark_preset = parse_theme_json(THEME_DARK, THEME_DARK_NAME, ThemeMode::Dark);
        let active = light_preset.clone();
        Self {
            mode: active.mode,
            colors: active.colors,
            radius: active.radius,
            font_family: active.font_family,
            font_size: DEFAULT_FONT_SIZE,
            mono_font_family: active.mono_font_family,
            mono_font_size: active.mono_font_size,
            light_preset,
            dark_preset,
        }
    }

    fn apply_preset(&mut self, mode: ThemeMode) {
        let preset = match mode {
            ThemeMode::Light => &self.light_preset,
            ThemeMode::Dark => &self.dark_preset,
        };
        self.mode = mode;
        self.colors = preset.colors.clone();
        self.radius = preset.radius;
        self.font_family = preset.font_family.clone();
        self.mono_font_family = preset.mono_font_family.clone();
        self.mono_font_size = preset.mono_font_size;
    }
}

#[derive(Deserialize)]
struct ThemeFile {
    themes: Vec<ThemeEntry>,
}

#[derive(Deserialize)]
struct ThemeEntry {
    name: String,
    #[serde(rename = "font.family")]
    font_family: Option<String>,
    #[serde(rename = "mono_font.family")]
    mono_font_family: Option<String>,
    #[serde(rename = "mono_font.size")]
    mono_font_size: Option<f32>,
    radius: Option<f32>,
    colors: HashMap<String, String>,
}

fn parse_hex_color(raw: &str) -> Option<Hsla> {
    let hex = raw.trim().strip_prefix('#')?;
    let val = match hex.len() {
        6 => {
            let rgb = u32::from_str_radix(hex, 16).ok()?;
            (rgb << 8) | 0xff
        }
        8 => u32::from_str_radix(hex, 16).ok()?,
        _ => return None,
    };
    Some(rgba(val).into())
}

fn parse_theme_json(json: &str, name: &str, fallback_mode: ThemeMode) -> ThemePreset {
    let file: ThemeFile = serde_json::from_str(json).expect("bundled theme JSON must be valid");
    let entry = file
        .themes
        .into_iter()
        .find(|t| t.name == name)
        .expect("bundled theme name must exist");
    let map = &entry.colors;
    let pick = |keys: &[&str], fallback: u32| -> Hsla {
        for key in keys {
            if let Some(raw) = map.get(*key)
                && let Some(c) = parse_hex_color(raw)
            {
                return c;
            }
        }
        rgba(fallback).into()
    };

    let dark = fallback_mode.is_dark();
    let background = pick(&["background"], if dark { 0x22252bff } else { 0xf9f9f8ff });
    let foreground = pick(&["foreground"], if dark { 0xd7dadfff } else { 0x403f44ff });
    let border = pick(&["border"], if dark { 0x30343dff } else { 0xe4e4e1ff });
    let muted = pick(
        &["muted.background"],
        if dark { 0x2f333dff } else { 0xe9e9e6ff },
    );
    let muted_foreground = pick(
        &["muted.foreground"],
        if dark { 0x7c828cff } else { 0x8a8b90ff },
    );
    let secondary = pick(
        &["secondary.background"],
        if dark { 0x2f333dff } else { 0xe9e9e6ff },
    );
    let secondary_foreground = pick(
        &["secondary.foreground"],
        if dark { 0xd7dadfff } else { 0x403f44ff },
    );
    let primary = pick(
        &["primary.background"],
        if dark { 0x62adefff } else { 0x94a4f5ff },
    );
    let primary_foreground = pick(
        &["primary.foreground"],
        if dark { 0x0e1216ff } else { 0x1c1d21ff },
    );
    let accent = pick(
        &["accent.background"],
        if dark { 0x2c313bff } else { 0xe9e9e6ff },
    );
    let accent_foreground = pick(
        &["accent.foreground"],
        if dark { 0xd7dadfff } else { 0x3f3e43ff },
    );
    let link = pick(
        &["link", "link.foreground"],
        if dark { 0x62adefff } else { 0x5570e0ff },
    );
    let ring = pick(&["ring"], if dark { 0x62adefff } else { 0x94a4f5ff });
    let selection = pick(
        &["selection.background"],
        if dark { 0x62adef40 } else { 0x94a4f540 },
    );
    let sidebar = pick(
        &["sidebar.background"],
        if dark { 0x22252bff } else { 0xf0f0eeff },
    );
    let sidebar_foreground = pick(
        &["sidebar.foreground"],
        if dark { 0xd7dadfff } else { 0x3f3e43ff },
    );
    let sidebar_border = pick(
        &["sidebar.border"],
        if dark { 0x30343dff } else { 0xe3e3e0ff },
    );
    let sidebar_accent = pick(
        &["sidebar.accent.background"],
        if dark { 0x2c313bff } else { 0xe4e4e1ff },
    );
    let sidebar_accent_foreground = pick(
        &["sidebar.accent.foreground"],
        if dark { 0xd7dadfff } else { 0x232227ff },
    );
    let sidebar_primary = pick(
        &["sidebar.primary.background"],
        if dark { 0x62adefff } else { 0x94a4f5ff },
    );
    let sidebar_primary_foreground = pick(
        &["sidebar.primary.foreground"],
        if dark { 0x0e1216ff } else { 0x1c1d21ff },
    );
    let input = pick(
        &["input.border"],
        if dark { 0x3f4450ff } else { 0xcfcecaff },
    );
    let popover = pick(
        &["popover.background"],
        if dark { 0x262931ff } else { 0xfdfdfcff },
    );
    let popover_foreground = pick(
        &["popover.foreground"],
        if dark { 0xd7dadfff } else { 0x403f44ff },
    );
    let title_bar = pick(
        &["title_bar.background"],
        if dark { 0x22252bff } else { 0xf0f0eeff },
    );
    let title_bar_border = pick(
        &["title_bar.border"],
        if dark { 0x30343dff } else { 0xe3e3e0ff },
    );
    let tab_bar = pick(
        &["tab_bar.background"],
        if dark { 0x22252bff } else { 0xf0f0eeff },
    );
    let tab_active = pick(
        &["tab.active.background"],
        if dark { 0x2f333dff } else { 0xf9f9f8ff },
    );
    let tab_active_foreground = pick(
        &["tab.active.foreground"],
        if dark { 0xd7dadfff } else { 0x403f44ff },
    );
    let tab_foreground = pick(
        &["tab.foreground"],
        if dark { 0xd4d4d4ff } else { 0x8a8b90ff },
    );
    let caret = pick(&["caret"], if dark { 0xd7dadfff } else { 0x403f44ff });
    let danger = pick(
        &["danger.background", "base.red"],
        if dark { 0xe06c75ff } else { 0xd1686fff },
    );
    let warning = pick(
        &["warning.background", "base.yellow"],
        if dark { 0xe5c07bff } else { 0xd5ab60ff },
    );
    let success = pick(
        &["success.background", "base.green"],
        if dark { 0x98c379ff } else { 0x7fae6aff },
    );
    let info = pick(
        &["info.background", "base.blue"],
        if dark { 0x56b6c2ff } else { 0x94a4f5ff },
    );
    let red = pick(&["base.red"], if dark { 0xe06c75ff } else { 0xd1686fff });
    let green = pick(&["base.green"], if dark { 0x98c379ff } else { 0x7fae6aff });
    let yellow = pick(&["base.yellow"], if dark { 0xe5c07bff } else { 0xd5ab60ff });
    let blue = pick(&["base.blue"], if dark { 0x62adefff } else { 0x94a4f5ff });
    let magenta = pick(&["base.magenta"], 0xbc74d2ff);
    let cyan = pick(&["base.cyan"], 0x56b6c2ff);
    let red_light = pick(
        &["base.red.light", "base.red"],
        if dark { 0xef8a92ff } else { 0xd1686fff },
    );
    let green_light = pick(
        &["base.green.light", "base.green"],
        if dark { 0xb0dd97ff } else { 0x7fae6aff },
    );
    let yellow_light = pick(
        &["base.yellow.light", "base.yellow"],
        if dark { 0xf2cf9cff } else { 0xd5ab60ff },
    );
    let blue_light = pick(
        &["base.blue.light", "base.blue"],
        if dark { 0x7fc2ffff } else { 0xa5b4f8ff },
    );
    let magenta_light = pick(&["base.magenta.light", "base.magenta"], 0xd09ce0ff);
    let cyan_light = pick(&["base.cyan.light", "base.cyan"], 0x7fd0daff);

    ThemePreset {
        mode: fallback_mode,
        colors: ThemeColors {
            background,
            foreground,
            border,
            muted,
            muted_foreground,
            secondary,
            secondary_foreground,
            primary,
            primary_foreground,
            accent,
            accent_foreground,
            link,
            ring,
            selection,
            sidebar,
            sidebar_foreground,
            sidebar_border,
            sidebar_accent,
            sidebar_accent_foreground,
            sidebar_primary,
            sidebar_primary_foreground,
            input,
            popover,
            popover_foreground,
            title_bar,
            title_bar_border,
            tab_bar,
            tab_active,
            tab_active_foreground,
            tab_foreground,
            caret,
            danger,
            warning,
            success,
            info,
            red,
            green,
            yellow,
            blue,
            magenta,
            cyan,
            red_light,
            green_light,
            yellow_light,
            blue_light,
            magenta_light,
            cyan_light,
            transparent: hsla(0., 0., 0., 0.),
        },
        radius: entry.radius.unwrap_or(6.0),
        font_family: SharedString::from(
            entry
                .font_family
                .unwrap_or_else(|| SYSTEM_FONT_FAMILY.to_owned()),
        ),
        mono_font_family: SharedString::from(
            entry
                .mono_font_family
                .unwrap_or_else(|| crate::fonts::TERM_FONT.to_owned()),
        ),
        mono_font_size: entry.mono_font_size.unwrap_or(13.0),
    }
}

/// Load `themes/*.json` into [`KumuTheme`] and apply the saved light/dark choice
/// and user font settings.
pub fn load_kumu_themes(cx: &mut App) {
    cx.set_global(KumuTheme::load_bundled());
    // Transitional: keep gpui-kit ThemeRegistry populated for the remaining
    // GPUI window shell / terminal chrome until Stage 3 removes gpui-kit.
    init_legacy_gpui_kit_themes(cx);

    let mode =
        theme_mode_from_setting(crate::db::get_setting(crate::db::SETTING_THEME_MODE).as_deref());
    apply_mode(mode, None, cx);
}

/// The light/dark choice saved by Settings, defaulting to light.
fn theme_mode_from_setting(value: Option<&str>) -> ThemeMode {
    match value {
        Some("dark") => ThemeMode::Dark,
        _ => ThemeMode::Light,
    }
}

/// Switch light/dark, remember the choice, then restore persisted font choices.
pub fn change_mode(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    crate::db::set_setting(
        crate::db::SETTING_THEME_MODE,
        if mode.is_dark() { "dark" } else { "light" },
    )
    .ok();
    apply_mode(mode, window, cx);
}

fn apply_mode(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    if !cx.has_global::<KumuTheme>() {
        cx.set_global(KumuTheme::load_bundled());
    }
    KumuTheme::global_mut(cx).apply_preset(mode);
    sync_legacy_gpui_kit_mode(mode, window, cx);
    cx.default_global::<ThemeRevision>().0 += 1;
    apply_saved_font(cx);
}

/// Terminal typeface: theme mono family (JetBrains Mono by default), sized
/// with the UI font scale so the settings font-size control grows the grid
/// with the rest of the app.
pub fn term_font(cx: &App) -> (SharedString, f32) {
    let theme = KumuTheme::global(cx);
    let family = theme.mono_font_family.clone();
    let family = if family.is_empty() {
        SharedString::from(crate::fonts::TERM_FONT)
    } else {
        family
    };
    let size = theme.mono_font_size * (theme.font_size / DEFAULT_FONT_SIZE);
    (family, size.max(10.))
}

/// Terminal colors taken from the active [`KumuTheme`].
pub fn term_colors(cx: &App) -> TermColors {
    term_colors_from(KumuTheme::global(cx))
}

pub fn term_colors_from(theme: &KumuTheme) -> TermColors {
    let c = &theme.colors;
    let rgb = |color: Hsla| {
        let rgba = color.to_rgb();
        Rgb(
            (rgba.r * 255.).round() as u8,
            (rgba.g * 255.).round() as u8,
            (rgba.b * 255.).round() as u8,
        )
    };
    let fg = rgb(c.foreground);
    let bg = rgb(c.background);
    // ANSI black: ink on light themes, the surface itself on dark ones.
    let ansi_black = if theme.mode.is_dark() { bg } else { fg };
    TermColors {
        fg,
        bg,
        cursor: rgb(c.caret),
        selection: rgb(c.accent),
        palette: [
            ansi_black,
            rgb(c.red),
            rgb(c.green),
            rgb(c.yellow),
            rgb(c.blue),
            rgb(c.magenta),
            rgb(c.cyan),
            fg,
            rgb(c.muted_foreground),
            rgb(c.red_light),
            rgb(c.green_light),
            rgb(c.yellow_light),
            rgb(c.blue_light),
            rgb(c.magenta_light),
            rgb(c.cyan_light),
            fg,
        ],
    }
}

fn clamp_font_size(size: f32) -> f32 {
    if size.is_finite() {
        size.clamp(10., 32.)
    } else {
        DEFAULT_FONT_SIZE
    }
}

/// Set the app-wide UI font family and redraw every window.
pub fn set_font_family(cx: &mut App, family: &str) {
    KumuTheme::global_mut(cx).font_family = SharedString::from(family.to_owned());
    sync_legacy_gpui_kit_font(cx);
    cx.refresh_windows();
}

/// Set the app-wide UI font size, in px.
pub fn set_font_size(cx: &mut App, size: f32) {
    KumuTheme::global_mut(cx).font_size = clamp_font_size(size);
    sync_legacy_gpui_kit_font(cx);
    cx.refresh_windows();
}

/// Re-apply the persisted font choices on top of the current theme.
pub fn apply_saved_font(cx: &mut App) {
    let family = crate::db::get_setting(crate::db::SETTING_FONT);
    let size = crate::db::get_setting(crate::db::SETTING_FONT_SIZE)
        .and_then(|size| size.parse::<f32>().ok())
        .map(clamp_font_size)
        .unwrap_or(DEFAULT_FONT_SIZE);
    {
        let theme = KumuTheme::global_mut(cx);
        if let Some(family) = family {
            theme.font_family = SharedString::from(family);
        }
        theme.font_size = size;
    }
    sync_legacy_gpui_kit_font(cx);
    cx.refresh_windows();
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

// ---------------------------------------------------------------------------
// Transitional gpui-kit theme mirror (removed in Stage 3 when gpui-kit is dropped)
// ---------------------------------------------------------------------------

fn init_legacy_gpui_kit_themes(cx: &mut App) {
    if !cx.has_global::<gpui_kit::component::ThemeRegistry>() {
        return;
    }
    let (light, dark) = {
        let registry = gpui_kit::component::ThemeRegistry::global_mut(cx);
        let _ = registry.load_themes_from_str(THEME_LIGHT);
        let _ = registry.load_themes_from_str(THEME_DARK);
        (
            registry.themes().get(THEME_LIGHT_NAME).cloned(),
            registry.themes().get(THEME_DARK_NAME).cloned(),
        )
    };
    if cx.has_global::<gpui_kit::component::Theme>() {
        let theme = gpui_kit::component::Theme::global_mut(cx);
        if let Some(t) = light {
            theme.light_theme = t;
        }
        if let Some(t) = dark {
            theme.dark_theme = t;
        }
    }
}

fn sync_legacy_gpui_kit_mode(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    if !cx.has_global::<gpui_kit::component::Theme>() {
        return;
    }
    let legacy = if mode.is_dark() {
        gpui_kit::component::ThemeMode::Dark
    } else {
        gpui_kit::component::ThemeMode::Light
    };
    gpui_kit::component::Theme::change(legacy, window, cx);
}

fn sync_legacy_gpui_kit_font(cx: &mut App) {
    if !cx.has_global::<gpui_kit::component::Theme>() {
        return;
    }
    let (family, size) = {
        let t = KumuTheme::global(cx);
        (t.font_family.clone(), t.font_size)
    };
    {
        let legacy = gpui_kit::component::Theme::global_mut(cx);
        legacy.font_family = family;
        legacy.font_size = gpui::px(size);
    }
    gpui_kit::component::Theme::sync_base(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_themes_parse_light_and_dark_palettes() {
        let mut theme = KumuTheme::load_bundled();
        assert!(!theme.mode.is_dark());
        let light_term = term_colors_from(&theme);
        assert_ne!(light_term.bg, light_term.fg);

        theme.apply_preset(ThemeMode::Dark);
        assert!(theme.mode.is_dark());
        let dark_term = term_colors_from(&theme);
        assert_ne!(dark_term.bg, light_term.bg);
        assert_eq!(theme.mono_font_family.as_ref(), "JetBrains Mono");
    }
}
