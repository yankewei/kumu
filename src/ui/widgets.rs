//! Shared UI building blocks (theme-token helpers reused across windows).

use std::time::Duration;

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

/// An image as a fixed-size square thumbnail, cropped to fill it. The size is
/// fixed so a row measures the same before and after the image loads.
pub fn image_thumbnail(path: std::path::PathBuf, size: Rems, radius: Pixels, border: Hsla) -> Div {
    div()
        .size(size)
        .flex_none()
        .rounded(radius)
        .border_1()
        .border_color(border)
        .overflow_hidden()
        .child(img(path).size_full().object_fit(ObjectFit::Cover))
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

/// The rotating loader's period, and the cadence it re-renders at.
const LOADER_PERIOD: Duration = Duration::from_millis(900);
/// 15 fps — the discrete-step cadence these chunky pulses read the same at.
const LOADER_FPS: f32 = 15.;

/// The working loader: a rotating `LoaderCircle` at [`LOADER_FPS`].
///
/// Not the component library's `Spinner`, which wraps a repeating
/// `with_animation`. A repeating animation requests a frame every display frame
/// for as long as it is mounted, so one working row would hold the whole window
/// redrawing at the panel's refresh rate. `Animation::with_max_fps` is the
/// framework's own low-frequency repaint: it schedules the next render
/// `1 / LOADER_FPS` later rather than on every frame. `with_animation` is also
/// what carries `reduce_motion` — under it the loader holds its first frame and
/// nothing is scheduled — and a row that leaves the element tree stops the
/// chain by itself, so there is no task to cancel.
pub fn loader(color: Hsla) -> AnyElement {
    Icon::new(IconName::LoaderCircle)
        .small()
        .text_color(color)
        .with_animation(
            "loader",
            Animation::new(LOADER_PERIOD)
                .repeat()
                .with_max_fps(LOADER_FPS),
            |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
        )
        .into_any_element()
}
