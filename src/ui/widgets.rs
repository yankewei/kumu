//! Shared UI building blocks (theme-token helpers reused across windows).

use gpui_kit::component::theme::Colorize as _;
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

/// What a status pill asserts, which decides its tint.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PillTone {
    /// Link-tinted: the healthy state kumu itself owns.
    Accent,
    /// Muted: absence, or a state that could not be read.
    Neutral,
    /// Amber-tinted: something the user should put right.
    Warning,
}

/// A small status pill, Zed-badge style.
pub fn pill(tone: PillTone, label: impl Into<SharedString>, theme: &Theme) -> Div {
    let (bg, fg) = match tone {
        PillTone::Accent => (theme.primary.opacity(0.14), theme.link),
        PillTone::Neutral => (theme.muted, theme.muted_foreground),
        // Amber itself is too light for badge text on a light card; pull it
        // toward the foreground, which darkens it on light and lifts it on dark.
        PillTone::Warning => (
            theme.warning.opacity(0.16),
            theme.warning.mix_oklab(theme.foreground, 0.45),
        ),
    };
    div()
        .px_2()
        .py_0p5()
        .rounded(theme.radius)
        .bg(bg)
        .text_xs()
        .text_color(fg)
        .child(label.into())
}

/// An agent's logo at `size`: svg logos are alpha masks tinted with `tint`;
/// other formats (png) keep their own colors. Falls back to a Bot icon.
pub fn agent_logo(logo: Option<&'static str>, size: Pixels, tint: Hsla) -> AnyElement {
    match logo {
        Some(l) if l.ends_with(".svg") => Icon::empty()
            .path(l)
            .with_size(size)
            .text_color(tint)
            .into_any_element(),
        Some(l) => img(l).size(size).into_any_element(),
        None => Icon::new(IconName::Bot)
            .with_size(size)
            .text_color(tint)
            .into_any_element(),
    }
}
