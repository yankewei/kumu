//! Native macOS transparent title bar chrome implemented directly on `gpui`
//! (zero `gpui-kit` dependency).

use gpui::{
    AnyElement, ClickEvent, FontWeight, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, StatefulInteractiveElement, Styled, TitlebarOptions,
    WindowOptions, div, point, px,
};

use crate::theme::KumuTheme;

pub(crate) const TITLE_BAR_HEIGHT: f32 = 34.0;
const MACOS_TRAFFIC_LIGHT_INSET: f32 = 80.0;

/// Base [`WindowOptions`] for windows that render [`title_bar`].
pub fn window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(point(px(9.0), px(9.0))),
        }),
        app_owns_titlebar_drag: true,
        ..Default::default()
    }
}

/// Full-width 34px title bar with macOS traffic-light clearance, drag-to-move,
/// double-click-to-zoom, left brand label, optional left controls, and optional
/// right-hand controls.
pub fn title_bar(
    id: &'static str,
    theme: &KumuTheme,
    right: Option<AnyElement>,
) -> impl IntoElement {
    title_bar_with_controls(id, theme, None, right)
}

/// Variant of [`title_bar`] that also places compact controls beside the left
/// brand label (e.g. sidebar collapse/expand toggle).
pub fn title_bar_with_controls(
    id: &'static str,
    theme: &KumuTheme,
    left: Option<AnyElement>,
    right: Option<AnyElement>,
) -> impl IntoElement {
    let mut left_group = div().flex().flex_row().items_center().gap_2().child(
        div()
            .text_sm()
            .font_weight(FontWeight::BOLD)
            .text_color(theme.colors.foreground)
            .child("kumu"),
    );
    if let Some(left) = left {
        left_group = left_group.child(left);
    }

    let mut bar = div()
        .id(id)
        .flex()
        .flex_row()
        .flex_none()
        .w_full()
        .h(px(TITLE_BAR_HEIGHT))
        .pl(px(MACOS_TRAFFIC_LIGHT_INSET))
        .pr_2()
        .items_center()
        .justify_between()
        .bg(theme.colors.title_bar)
        .border_b_1()
        .border_color(theme.colors.title_bar_border)
        .on_mouse_down(MouseButton::Left, |ev: &MouseDownEvent, window, _| {
            if ev.click_count == 1 {
                window.start_window_move();
            }
        })
        .on_click(|ev: &ClickEvent, window, _| {
            if ev.click_count() == 2 {
                window.zoom_window();
            }
        })
        .child(left_group);

    if let Some(right) = right {
        bar = bar.child(right);
    }
    bar
}
