//! Shared UI building blocks (theme-token helpers reused across windows).

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

/// A small status pill (accent = link-tinted, neutral = muted), Zed-badge style.
pub fn pill(
    accent: bool,
    label: impl Into<SharedString>,
    radius: Pixels,
    primary: Hsla,
    link: Hsla,
    muted: Hsla,
    muted_fg: Hsla,
) -> Div {
    let (bg, fg) = if accent {
        (primary.opacity(0.14), link)
    } else {
        (muted, muted_fg)
    };
    div()
        .px_2()
        .py_0p5()
        .rounded(radius)
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
