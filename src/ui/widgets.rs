//! Shared UI building blocks (theme-token helpers reused across windows).

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

/// What a chip says when nothing is in force on its axis: the agent's own
/// configuration decides, and it says which agent, so the label answers "what
/// will happen" rather than reading like the name of a config file.
pub fn inherited_label(agent_name: &str) -> String {
    format!("跟随 {agent_name} 配置")
}

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

/// A trigger for a picker popover: a plain interactive div wrapped so it can
/// carry the popover's `selected` (open) state.
pub struct ChipTrigger {
    pub element: Stateful<Div>,
}

impl Selectable for ChipTrigger {
    fn selected(self, _selected: bool) -> Self {
        // The open-state tint is applied at build time from the controlled
        // flag; nothing to do here.
        self
    }

    fn is_selected(&self) -> bool {
        false
    }
}

impl IntoElement for ChipTrigger {
    type Element = AnyElement;

    fn into_element(self) -> Self::Element {
        self.element.into_any_element()
    }
}

/// One selectable row in a picker's list: label on the left, an optional muted
/// subtitle on the right, accent-tinted while it is the current choice.
///
/// The subtitle is optional because a list may have nothing more to say than
/// the names themselves; such a row sizes to its label rather than stretching,
/// which is what lets a popover of bare names stay narrow.
pub fn select_row(
    name: &str,
    subtitle: Option<String>,
    selected: bool,
    radius: Pixels,
    muted: Hsla,
    muted_fg: Hsla,
    accent: Hsla,
    accent_fg: Hsla,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let subtitle_key = subtitle.as_deref().unwrap_or_default();
    let has_subtitle = subtitle.is_some();
    let label = div()
        .min_w_0()
        .text_xs()
        .font_medium()
        .truncate()
        .text_color(if selected { accent_fg } else { muted_fg })
        .child(name.to_string());
    div()
        .id(SharedString::from(format!(
            "select-row-{name}-{subtitle_key}"
        )))
        .px_2p5()
        .py_1p5()
        .gap_2()
        .rounded(radius)
        .flex()
        .items_center()
        .min_w_0()
        .cursor_pointer()
        .when(selected, |el| el.bg(accent))
        .when(!selected, |el| el.hover(move |s| s.bg(muted)))
        // Only a label that shares its row takes the slack; a lone label keeps
        // its natural width instead of padding the row out to nothing.
        .child(if has_subtitle { label.flex_1() } else { label })
        .children(subtitle.map(|subtitle| {
            div()
                .min_w_0()
                .text_xs()
                .text_color(if selected { accent_fg } else { muted_fg })
                .truncate()
                .child(subtitle)
        }))
        .on_click(on_click)
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
