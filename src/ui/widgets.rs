//! Shared UI building blocks (theme-token helpers reused across windows).

use gpui_kit::prelude::*;
use gpui_kit::*;

/// A small status pill (accent = link-tinted, neutral = muted), Zed-badge style.
pub fn pill(accent: bool, label: impl Into<SharedString>, radius: Pixels, primary: Hsla, link: Hsla, muted: Hsla, muted_fg: Hsla) -> Div {
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
