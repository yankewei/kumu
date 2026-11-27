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

const THEME_LIGHT: &str = include_str!("../themes/light.json");
const THEME_DARK: &str = include_str!("../themes/dark.json");
const THEME_LIGHT_NAME: &str = "Kumu Light";
const THEME_DARK_NAME: &str = "Zed Dark";

/// The app's default UI font family: the platform system font.
pub const SYSTEM_FONT_FAMILY: &str = ".SystemUIFont";
/// Label for the system-font entry in the settings font picker.
pub const SYSTEM_FONT_LABEL: &str = "系统字体";

/// The UI font size (px) in force when the user has not chosen one.
pub const DEFAULT_FONT_SIZE: f32 = 16.;
/// The sizes the settings picker offers for the UI, in px.
pub const FONT_SIZES: [u32; 9] = [12, 13, 14, 15, 16, 17, 18, 19, 20];

/// The terminal font size (px) in force when the user has not chosen one.
pub const DEFAULT_TERM_FONT_SIZE: f32 = 13.;
/// The sizes the settings picker offers for the terminal grid, in px.
pub const TERM_FONT_SIZES: [u32; 12] = [10, 11, 12, 13, 14, 15, 16, 17, 18, 20, 22, 24];

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

/// Independent terminal color preset, decoupled from the app shell's light/dark mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TerminalTheme {
    #[default]
    FollowApp,
    Light,
    Dark,
    PureWhite,
    PureBlack,
    TokyoNight,
    CatppuccinMocha,
    GruvboxDark,
    SolarizedDark,
    SolarizedLight,
}

/// Global holding the active [`TerminalTheme`] separately from [`KumuTheme`] so
/// changing terminal colors does not invalidate NSH shell theme observers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActiveTerminalTheme(pub TerminalTheme);

impl Global for ActiveTerminalTheme {}

struct TerminalPresetMeta {
    theme: TerminalTheme,
    id: &'static str,
    label: &'static str,
}

const TERMINAL_PRESETS: [TerminalPresetMeta; 10] = [
    TerminalPresetMeta {
        theme: TerminalTheme::FollowApp,
        id: "follow",
        label: "跟随应用外观",
    },
    TerminalPresetMeta {
        theme: TerminalTheme::Light,
        id: "light",
        label: "浅色",
    },
    TerminalPresetMeta {
        theme: TerminalTheme::Dark,
        id: "dark",
        label: "深色",
    },
    TerminalPresetMeta {
        theme: TerminalTheme::PureWhite,
        id: "pure_white",
        label: "纯白",
    },
    TerminalPresetMeta {
        theme: TerminalTheme::PureBlack,
        id: "pure_black",
        label: "纯黑",
    },
    TerminalPresetMeta {
        theme: TerminalTheme::TokyoNight,
        id: "tokyo_night",
        label: "Tokyo Night",
    },
    TerminalPresetMeta {
        theme: TerminalTheme::CatppuccinMocha,
        id: "catppuccin_mocha",
        label: "Catppuccin Mocha",
    },
    TerminalPresetMeta {
        theme: TerminalTheme::GruvboxDark,
        id: "gruvbox_dark",
        label: "Gruvbox Dark",
    },
    TerminalPresetMeta {
        theme: TerminalTheme::SolarizedDark,
        id: "solarized_dark",
        label: "Solarized Dark",
    },
    TerminalPresetMeta {
        theme: TerminalTheme::SolarizedLight,
        id: "solarized_light",
        label: "Solarized Light",
    },
];

impl TerminalTheme {
    pub const ALL: [Self; 10] = [
        Self::FollowApp,
        Self::Light,
        Self::Dark,
        Self::PureWhite,
        Self::PureBlack,
        Self::TokyoNight,
        Self::CatppuccinMocha,
        Self::GruvboxDark,
        Self::SolarizedDark,
        Self::SolarizedLight,
    ];

    fn meta(self) -> &'static TerminalPresetMeta {
        TERMINAL_PRESETS
            .iter()
            .find(|entry| entry.theme == self)
            .expect("every TerminalTheme variant must have metadata")
    }

    pub fn id(self) -> &'static str {
        self.meta().id
    }

    pub fn label(self) -> &'static str {
        self.meta().label
    }

    pub fn from_id(value: &str) -> Option<Self> {
        let needle = value.trim();
        TERMINAL_PRESETS
            .iter()
            .find(|entry| entry.id == needle)
            .map(|entry| entry.theme)
    }

    pub fn from_setting(value: Option<&str>) -> Self {
        value.and_then(Self::from_id).unwrap_or_default()
    }

    /// Resolve this preset into full terminal colors for the given app theme
    /// and shell mode (used for both live rendering and Settings swatches).
    pub fn resolve_colors(self, theme: &KumuTheme, app_mode: ThemeMode) -> TermColors {
        match self {
            Self::FollowApp => match app_mode {
                ThemeMode::Light => term_colors_from_preset(&theme.light_preset),
                ThemeMode::Dark => term_colors_from_preset(&theme.dark_preset),
            },
            Self::Light => term_colors_from_preset(&theme.light_preset),
            Self::Dark => term_colors_from_preset(&theme.dark_preset),
            Self::PureWhite => TermColors {
                fg: hex_rgb(0x1f2328),
                bg: hex_rgb(0xffffff),
                cursor: hex_rgb(0x1f2328),
                selection: hex_rgb(0xc5d6fa),
                palette: [
                    hex_rgb(0x24292f),
                    hex_rgb(0xcf222e),
                    hex_rgb(0x116329),
                    hex_rgb(0x9a6700),
                    hex_rgb(0x0969da),
                    hex_rgb(0x8250df),
                    hex_rgb(0x1b7c83),
                    hex_rgb(0x6e7781),
                    hex_rgb(0x57606a),
                    hex_rgb(0xa40e26),
                    hex_rgb(0x1a7f37),
                    hex_rgb(0xbf8700),
                    hex_rgb(0x218bff),
                    hex_rgb(0xa475f9),
                    hex_rgb(0x3192aa),
                    hex_rgb(0x424a53),
                ],
            },
            Self::PureBlack => TermColors {
                fg: hex_rgb(0xe6e8ec),
                bg: hex_rgb(0x0c0d10),
                cursor: hex_rgb(0xe6e8ec),
                selection: hex_rgb(0x2f3b52),
                palette: [
                    hex_rgb(0x181a20),
                    hex_rgb(0xf26d78),
                    hex_rgb(0x8bd47e),
                    hex_rgb(0xf2c97d),
                    hex_rgb(0x6cb6ff),
                    hex_rgb(0xd2a8ff),
                    hex_rgb(0x56d4dd),
                    hex_rgb(0xd0d7de),
                    hex_rgb(0x6e7681),
                    hex_rgb(0xff949d),
                    hex_rgb(0xa8e69c),
                    hex_rgb(0xf8dc9e),
                    hex_rgb(0x96ccff),
                    hex_rgb(0xe2c5ff),
                    hex_rgb(0x7ee7ef),
                    hex_rgb(0xf5f7fa),
                ],
            },
            Self::TokyoNight => TermColors {
                fg: hex_rgb(0xc0caf5),
                bg: hex_rgb(0x1a1b26),
                cursor: hex_rgb(0xc0caf5),
                selection: hex_rgb(0x33467c),
                palette: [
                    hex_rgb(0x15161e),
                    hex_rgb(0xf7768e),
                    hex_rgb(0x9ece6a),
                    hex_rgb(0xe0af68),
                    hex_rgb(0x7aa2f7),
                    hex_rgb(0xbb9af7),
                    hex_rgb(0x7dcfff),
                    hex_rgb(0xa9b1d6),
                    hex_rgb(0x414868),
                    hex_rgb(0xf7768e),
                    hex_rgb(0x9ece6a),
                    hex_rgb(0xe0af68),
                    hex_rgb(0x7aa2f7),
                    hex_rgb(0xbb9af7),
                    hex_rgb(0x7dcfff),
                    hex_rgb(0xc0caf5),
                ],
            },
            Self::CatppuccinMocha => TermColors {
                fg: hex_rgb(0xcdd6f4),
                bg: hex_rgb(0x1e1e2e),
                cursor: hex_rgb(0xf5e0dc),
                selection: hex_rgb(0x454c66),
                palette: [
                    hex_rgb(0x45475a),
                    hex_rgb(0xf38ba8),
                    hex_rgb(0xa6e3a1),
                    hex_rgb(0xf9e2af),
                    hex_rgb(0x89b4fa),
                    hex_rgb(0xf5c2e7),
                    hex_rgb(0x94e2d5),
                    hex_rgb(0xbac2de),
                    hex_rgb(0x585b70),
                    hex_rgb(0xf38ba8),
                    hex_rgb(0xa6e3a1),
                    hex_rgb(0xf9e2af),
                    hex_rgb(0x89b4fa),
                    hex_rgb(0xf5c2e7),
                    hex_rgb(0x94e2d5),
                    hex_rgb(0xa6adc8),
                ],
            },
            Self::GruvboxDark => TermColors {
                fg: hex_rgb(0xebdbb2),
                bg: hex_rgb(0x282828),
                cursor: hex_rgb(0xebdbb2),
                selection: hex_rgb(0x504945),
                palette: [
                    hex_rgb(0x282828),
                    hex_rgb(0xcc241d),
                    hex_rgb(0x98971a),
                    hex_rgb(0xd79921),
                    hex_rgb(0x458588),
                    hex_rgb(0xb16286),
                    hex_rgb(0x689d6a),
                    hex_rgb(0xa89984),
                    hex_rgb(0x928374),
                    hex_rgb(0xfb4934),
                    hex_rgb(0xb8bb26),
                    hex_rgb(0xfabd2f),
                    hex_rgb(0x83a598),
                    hex_rgb(0xd3869b),
                    hex_rgb(0x8ec07c),
                    hex_rgb(0xebdbb2),
                ],
            },
            Self::SolarizedDark => TermColors {
                fg: hex_rgb(0x839496),
                bg: hex_rgb(0x002b36),
                cursor: hex_rgb(0x93a1a1),
                selection: hex_rgb(0x1a5263),
                palette: [
                    hex_rgb(0x073642),
                    hex_rgb(0xdc322f),
                    hex_rgb(0x859900),
                    hex_rgb(0xb58900),
                    hex_rgb(0x268bd2),
                    hex_rgb(0xd33682),
                    hex_rgb(0x2aa198),
                    hex_rgb(0xeee8d5),
                    hex_rgb(0x586e75),
                    hex_rgb(0xcb4b16),
                    hex_rgb(0x586e75),
                    hex_rgb(0x657b83),
                    hex_rgb(0x839496),
                    hex_rgb(0x6c71c4),
                    hex_rgb(0x93a1a1),
                    hex_rgb(0xfdf6e3),
                ],
            },
            Self::SolarizedLight => TermColors {
                fg: hex_rgb(0x586e75),
                bg: hex_rgb(0xfdf6e3),
                cursor: hex_rgb(0x586e75),
                selection: hex_rgb(0xc8bfa8),
                palette: [
                    hex_rgb(0x073642),
                    hex_rgb(0xdc322f),
                    hex_rgb(0x859900),
                    hex_rgb(0xb58900),
                    hex_rgb(0x268bd2),
                    hex_rgb(0xd33682),
                    hex_rgb(0x2aa198),
                    hex_rgb(0x93a1a1),
                    hex_rgb(0x002b36),
                    hex_rgb(0xcb4b16),
                    hex_rgb(0x586e75),
                    hex_rgb(0x657b83),
                    hex_rgb(0x839496),
                    hex_rgb(0x6c71c4),
                    hex_rgb(0x586e75),
                    hex_rgb(0x586e75),
                ],
            },
        }
    }

    /// `(bg, fg)` swatch colors derived directly from the active bundled theme.
    pub fn preview_colors(self, theme: &KumuTheme, app_mode: ThemeMode) -> (Rgb, Rgb) {
        let colors = self.resolve_colors(theme, app_mode);
        (colors.bg, colors.fg)
    }
}

/// Semantic color palette loaded from `themes/*.json`.
#[derive(Clone, Debug)]
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
    pub caret: Hsla,
    pub danger: Hsla,
    pub warning: Hsla,
    pub success: Hsla,
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
            caret,
            danger,
            warning,
            success,
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

/// Load `themes/*.json` into [`KumuTheme`] and apply the saved light/dark choice,
/// independent terminal color preset, and user font settings.
pub fn load_kumu_themes(cx: &mut App) {
    cx.set_global(KumuTheme::load_bundled());
    let saved_terminal = TerminalTheme::from_setting(
        crate::db::get_setting(crate::db::SETTING_TERMINAL_THEME).as_deref(),
    );
    cx.set_global(ActiveTerminalTheme(saved_terminal));
    let mode =
        theme_mode_from_setting(crate::db::get_setting(crate::db::SETTING_THEME_MODE).as_deref());
    apply_mode(mode, None, cx);
}

/// Read the currently active [`TerminalTheme`].
pub fn terminal_theme(cx: &App) -> TerminalTheme {
    cx.try_global::<ActiveTerminalTheme>()
        .map(|g| g.0)
        .unwrap_or_default()
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

/// Switch the terminal color preset without touching [`KumuTheme`], persist it,
/// and notify live terminals. No-op when already active.
pub fn change_terminal_theme(
    terminal_theme: TerminalTheme,
    cx: &mut App,
) -> Result<(), rusqlite::Error> {
    if self::terminal_theme(cx) == terminal_theme {
        return Ok(());
    }
    cx.set_global(ActiveTerminalTheme(terminal_theme));
    cx.default_global::<ThemeRevision>().0 += 1;
    cx.refresh_windows();
    crate::db::set_setting(crate::db::SETTING_TERMINAL_THEME, terminal_theme.id())
}

fn apply_mode(mode: ThemeMode, _window: Option<&mut Window>, cx: &mut App) {
    if !cx.has_global::<KumuTheme>() {
        cx.set_global(KumuTheme::load_bundled());
    }
    KumuTheme::global_mut(cx).apply_preset(mode);
    cx.default_global::<ThemeRevision>().0 += 1;
    apply_saved_font(cx);
}

/// Terminal typeface: user-chosen mono family (JetBrains Mono by default) and
/// independent terminal font size.
pub fn term_font(cx: &App) -> (SharedString, f32) {
    let theme = KumuTheme::global(cx);
    let family = theme.mono_font_family.clone();
    let family = if family.is_empty() {
        SharedString::from(crate::fonts::TERM_FONT)
    } else {
        family
    };
    (family, clamp_term_font_size(theme.mono_font_size))
}

/// Terminal colors taken from the active [`KumuTheme`] and [`ActiveTerminalTheme`].
pub fn term_colors(cx: &App) -> TermColors {
    term_colors_from(KumuTheme::global(cx), terminal_theme(cx))
}

fn rgb_from_hsla(color: Hsla) -> Rgb {
    let rgba = color.to_rgb();
    Rgb(
        (rgba.r * 255.).round() as u8,
        (rgba.g * 255.).round() as u8,
        (rgba.b * 255.).round() as u8,
    )
}

fn hsla_from_rgb(rgb: Rgb, alpha: f32) -> Hsla {
    gpui::Rgba {
        r: rgb.0 as f32 / 255.,
        g: rgb.1 as f32 / 255.,
        b: rgb.2 as f32 / 255.,
        a: alpha,
    }
    .into()
}

fn blend_rgb(base: Rgb, overlay: Rgb, alpha: f32) -> Rgb {
    let mix = |b: u8, o: u8| {
        (b as f32 * (1.0 - alpha) + o as f32 * alpha)
            .round()
            .clamp(0., 255.) as u8
    };
    Rgb(
        mix(base.0, overlay.0),
        mix(base.1, overlay.1),
        mix(base.2, overlay.2),
    )
}

fn relative_luminance(rgb: Rgb) -> f32 {
    let ch = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(rgb.0) + 0.7152 * ch(rgb.1) + 0.0722 * ch(rgb.2)
}

fn term_colors_from_preset(preset: &ThemePreset) -> TermColors {
    let c = &preset.colors;
    let fg = rgb_from_hsla(c.foreground);
    let bg = rgb_from_hsla(c.background);
    // ANSI black: ink on light themes, the surface itself on dark ones.
    let ansi_black = if preset.mode.is_dark() { bg } else { fg };
    // Blend the theme's selection / link tint onto the terminal background so
    // non-inverted selection backgrounds stay clearly visible on both surfaces.
    let selection = if preset.mode.is_dark() {
        blend_rgb(bg, rgb_from_hsla(c.selection), 0.34)
    } else {
        blend_rgb(bg, rgb_from_hsla(c.link), 0.30)
    };
    TermColors {
        fg,
        bg,
        cursor: rgb_from_hsla(c.caret),
        selection,
        palette: [
            ansi_black,
            rgb_from_hsla(c.red),
            rgb_from_hsla(c.green),
            rgb_from_hsla(c.yellow),
            rgb_from_hsla(c.blue),
            rgb_from_hsla(c.magenta),
            rgb_from_hsla(c.cyan),
            fg,
            rgb_from_hsla(c.muted_foreground),
            rgb_from_hsla(c.red_light),
            rgb_from_hsla(c.green_light),
            rgb_from_hsla(c.yellow_light),
            rgb_from_hsla(c.blue_light),
            rgb_from_hsla(c.magenta_light),
            rgb_from_hsla(c.cyan_light),
            fg,
        ],
    }
}

const fn hex_rgb(hex: u32) -> Rgb {
    Rgb(
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

pub fn term_colors_from(theme: &KumuTheme, terminal_theme: TerminalTheme) -> TermColors {
    terminal_theme.resolve_colors(theme, theme.mode)
}

/// Find-bar highlight overlays `(hit, active)` derived from the terminal's own
/// palette so search matches stay legible even when app and terminal modes differ.
pub fn term_search_colors(colors: &TermColors) -> (Hsla, Hsla) {
    let light_bg = relative_luminance(colors.bg) > 0.5;
    let hit_tint = colors.palette[4];
    let active_tint = if light_bg {
        // Darken yellow slightly on bright surfaces so the active match pops.
        blend_rgb(colors.palette[3], hex_rgb(0xd97706), 0.5)
    } else {
        colors.palette[3]
    };
    let hit = hsla_from_rgb(hit_tint, if light_bg { 0.30 } else { 0.34 });
    let active = hsla_from_rgb(active_tint, if light_bg { 0.55 } else { 0.50 });
    (hit, active)
}

fn clamp_font_size(size: f32) -> f32 {
    if size.is_finite() {
        size.clamp(10., 32.)
    } else {
        DEFAULT_FONT_SIZE
    }
}

fn clamp_term_font_size(size: f32) -> f32 {
    if size.is_finite() {
        size.clamp(10., 32.)
    } else {
        DEFAULT_TERM_FONT_SIZE
    }
}

/// Set the app-wide UI font family and redraw every window.
pub fn set_font_family(cx: &mut App, family: &str) {
    KumuTheme::global_mut(cx).font_family = SharedString::from(family.to_owned());
    cx.refresh_windows();
}

/// Set the app-wide UI font size, in px.
pub fn set_font_size(cx: &mut App, size: f32) {
    KumuTheme::global_mut(cx).font_size = clamp_font_size(size);
    cx.refresh_windows();
}

/// Set the terminal font family and redraw every window.
pub fn set_terminal_font_family(cx: &mut App, family: &str) {
    let family = if family.trim().is_empty() {
        crate::fonts::TERM_FONT
    } else {
        family
    };
    KumuTheme::global_mut(cx).mono_font_family = SharedString::from(family.to_owned());
    cx.refresh_windows();
}

/// Set the terminal font size, in px.
pub fn set_terminal_font_size(cx: &mut App, size: f32) {
    KumuTheme::global_mut(cx).mono_font_size = clamp_term_font_size(size);
    cx.refresh_windows();
}

/// Re-apply the persisted font choices on top of the current theme.
pub fn apply_saved_font(cx: &mut App) {
    let family = crate::db::get_setting(crate::db::SETTING_FONT);
    let size = crate::db::get_setting(crate::db::SETTING_FONT_SIZE)
        .and_then(|size| size.parse::<f32>().ok())
        .map(clamp_font_size)
        .unwrap_or(DEFAULT_FONT_SIZE);
    let term_family = crate::db::get_setting(crate::db::SETTING_TERM_FONT)
        .filter(|value| !value.trim().is_empty());
    let term_size = crate::db::get_setting(crate::db::SETTING_TERM_FONT_SIZE)
        .and_then(|size| size.parse::<f32>().ok())
        .map(clamp_term_font_size);
    {
        let theme = KumuTheme::global_mut(cx);
        if let Some(family) = family {
            theme.font_family = SharedString::from(family);
        }
        theme.font_size = size;
        if let Some(term_family) = term_family {
            theme.mono_font_family = SharedString::from(term_family);
        }
        if let Some(term_size) = term_size {
            theme.mono_font_size = term_size;
        }
    }
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

/// Terminal font picker items: the bundled JetBrains Mono default first, then
/// every installed user-visible family (excluding the icon-only Nerd fallback).
pub fn terminal_fonts(cx: &App) -> Vec<(SharedString, SharedString)> {
    let mut families: Vec<SharedString> = cx
        .text_system()
        .all_font_names()
        .into_iter()
        .filter(|name| {
            !name.starts_with('.')
                && name != crate::fonts::TERM_FONT
                && name != crate::fonts::SYMBOLS_FONT
        })
        .map(SharedString::from)
        .collect();
    families.sort();
    families.dedup();

    let mut items = vec![(
        SharedString::from(format!("{}（内置）", crate::fonts::TERM_FONT)),
        SharedString::from(crate::fonts::TERM_FONT),
    )];
    items.extend(families.into_iter().map(|family| {
        let label = family.clone();
        (label, family)
    }));
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contrast(a: Rgb, b: Rgb) -> f32 {
        let (x, y) = (relative_luminance(a), relative_luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn follow_app_tracks_mode_while_explicit_presets_stay_independent() {
        let mut theme = KumuTheme::load_bundled();
        assert!(!theme.mode.is_dark());
        let follow_light = term_colors_from(&theme, TerminalTheme::FollowApp);
        let explicit_dark_in_light_app = term_colors_from(&theme, TerminalTheme::Dark);
        assert_ne!(follow_light.bg, explicit_dark_in_light_app.bg);

        theme.apply_preset(ThemeMode::Dark);
        assert!(theme.mode.is_dark());
        let follow_dark = term_colors_from(&theme, TerminalTheme::FollowApp);
        assert_ne!(follow_dark.bg, follow_light.bg);
        assert_eq!(follow_dark.bg, explicit_dark_in_light_app.bg);
        assert_eq!(theme.mono_font_family.as_ref(), "JetBrains Mono");

        // Explicit presets produce identical terminal colors regardless of shell mode.
        for preset in TerminalTheme::ALL {
            assert_eq!(TerminalTheme::from_setting(Some(preset.id())), preset);
            if preset != TerminalTheme::FollowApp {
                assert_eq!(
                    preset.resolve_colors(&theme, ThemeMode::Light),
                    preset.resolve_colors(&theme, ThemeMode::Dark)
                );
            }
        }
        assert_eq!(TerminalTheme::from_setting(None), TerminalTheme::FollowApp);
        assert_eq!(
            TerminalTheme::from_setting(Some("unknown")),
            TerminalTheme::FollowApp
        );
    }

    #[test]
    fn all_terminal_presets_meet_readability_and_highlight_contrast() {
        let theme = KumuTheme::load_bundled();
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            for preset in TerminalTheme::ALL {
                let colors = preset.resolve_colors(&theme, mode);
                assert!(
                    contrast(colors.fg, colors.bg) >= 4.5,
                    "{preset:?} ({mode:?}) fg/bg contrast too low"
                );
                assert!(
                    contrast(colors.selection, colors.bg) >= 1.35,
                    "{preset:?} ({mode:?}) selection/bg contrast too low"
                );
                let (hit, active) = term_search_colors(&colors);
                let hit_blended = blend_rgb(colors.bg, rgb_from_hsla(hit), hit.a);
                let active_blended = blend_rgb(colors.bg, rgb_from_hsla(active), active.a);
                assert!(
                    contrast(hit_blended, colors.bg) >= 1.2,
                    "{preset:?} ({mode:?}) search_hit contrast too low"
                );
                assert!(
                    contrast(active_blended, colors.bg) >= 1.4,
                    "{preset:?} ({mode:?}) search_active contrast too low"
                );
                assert_eq!(preset.preview_colors(&theme, mode), (colors.bg, colors.fg));
            }
        }

        // Solarized Light ANSI white / bright white must remain distinct from its cream bg.
        let solarized_light =
            TerminalTheme::SolarizedLight.resolve_colors(&theme, ThemeMode::Light);
        assert_ne!(solarized_light.palette[7], solarized_light.bg);
        assert_ne!(solarized_light.palette[15], solarized_light.bg);
    }
}
