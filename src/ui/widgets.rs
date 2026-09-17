//! Shared UI building blocks (theme-token helpers reused across windows).

use std::time::Duration;

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::backend;

/// What a chip says when nothing is in force on its axis: the agent's own
/// configuration decides, and it says which agent, so the label answers "what
/// will happen" rather than reading like the name of a config file.
pub fn inherited_label(agent_name: &str) -> String {
    format!("跟随 {agent_name} 配置")
}

/// The widest a chip's label gets before it truncates.
///
/// One cap for every axis rather than one per picker: the model chip is the only
/// one whose values run long, but at 12px this holds the longest of them, so a
/// second, smaller cap would just be an unexplained number.
const CHIP_LABEL_MAX_W: f32 = 220.;

/// The widest and tallest a picker's menu card gets before its list scrolls.
const PICKER_MENU_MAX: f32 = 320.;

/// The theme tokens a composer picker draws itself with.
///
/// The chip and the menu it opens are one surface to the eye, so they read their
/// tokens from one bundle instead of each picker destructuring the theme again:
/// adding an axis is then a chip, a menu, and nothing else.
///
/// `accent` is the sidebar accent — what every picker tints its selected row
/// with, deliberately not `theme.accent`.
#[derive(Clone, Copy)]
pub struct PickerChrome {
    pub radius: Pixels,
    pub muted: Hsla,
    pub muted_fg: Hsla,
    pub border: Hsla,
    pub accent: Hsla,
    pub accent_fg: Hsla,
    pub background: Hsla,
}

impl PickerChrome {
    pub fn new(theme: &Theme) -> Self {
        Self {
            radius: theme.radius,
            muted: theme.muted,
            muted_fg: theme.muted_foreground,
            border: theme.border,
            accent: theme.sidebar_accent,
            accent_fg: theme.sidebar_accent_foreground,
            background: theme.background,
        }
    }
}

/// A picker's chip: the toolbar affordance that opens its menu.
///
/// `leading` is the glyph a chip leads with, for the axes that name a thing —
/// the model chip shows the agent's logo, reasoning a brain. `open` tints it, so
/// the chip keeps saying which menu it is holding open.
pub fn picker_chip(
    id: impl Into<ElementId>,
    chrome: &PickerChrome,
    label: String,
    leading: Option<AnyElement>,
    open: bool,
) -> ChipTrigger {
    ChipTrigger {
        element: h_flex()
            .id(id)
            .h(px(24.))
            .px_1p5()
            .gap_1p5()
            .rounded(chrome.radius)
            .cursor_pointer()
            .text_color(chrome.muted_fg)
            .when(open, |el| el.bg(chrome.muted.opacity(0.6)))
            .when(!open, |el| {
                el.hover(move |s| s.bg(chrome.muted.opacity(0.6)))
            })
            .children(leading)
            .child(
                div()
                    .max_w(px(CHIP_LABEL_MAX_W))
                    .text_xs()
                    .truncate()
                    .child(label),
            ),
    }
}

/// The column a picker's rows live in.
pub fn picker_list() -> Div {
    v_flex().gap_0p5().p_1p5()
}

/// The card a picker's list opens in: content-driven with a cap, not a fixed
/// width, so a list of bare labels reserves no room for a description it has
/// none of. (Waku caps its menu cards at the same 320.)
pub fn picker_menu(id: impl Into<ElementId>, chrome: &PickerChrome, list: Div) -> Div {
    picker_card(chrome).child(div().id(id).overflow_y_scroll().child(list))
}

/// A picker menu whose rows are driven from the keyboard: the rows are the
/// scroll container's own children, so `scroll.scroll_to_item(ix)` brings row
/// `ix` into view.
pub fn tracked_picker_menu(
    id: impl Into<ElementId>,
    chrome: &PickerChrome,
    scroll: &ScrollHandle,
    rows: impl IntoIterator<Item = impl IntoElement>,
) -> Div {
    picker_card(chrome).child(
        picker_list()
            .id(id)
            .overflow_y_scroll()
            .track_scroll(scroll)
            .children(rows),
    )
}

fn picker_card(chrome: &PickerChrome) -> Div {
    v_flex()
        .max_w(px(PICKER_MENU_MAX))
        .max_h(px(PICKER_MENU_MAX))
        .rounded(chrome.radius)
        .border_1()
        .border_color(chrome.border)
        .bg(chrome.background)
        .shadow_lg()
        .overflow_hidden()
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
    chrome: &PickerChrome,
    name: &str,
    subtitle: Option<String>,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let subtitle_key = subtitle.as_deref().unwrap_or_default();
    let has_subtitle = subtitle.is_some();
    let fg = if selected {
        chrome.accent_fg
    } else {
        chrome.muted_fg
    };
    let label = div()
        .min_w_0()
        .text_xs()
        .font_medium()
        .truncate()
        .text_color(fg)
        .child(name.to_string());
    div()
        .id(SharedString::from(format!(
            "select-row-{name}-{subtitle_key}"
        )))
        .px_2p5()
        .py_1p5()
        .gap_2()
        .rounded(chrome.radius)
        .flex()
        .items_center()
        .min_w_0()
        .cursor_pointer()
        .when(selected, |el| el.bg(chrome.accent))
        .when(!selected, |el| el.hover(move |s| s.bg(chrome.muted)))
        // Only a label that shares its row takes the slack; a lone label keeps
        // its natural width instead of padding the row out to nothing.
        .child(if has_subtitle { label.flex_1() } else { label })
        .children(subtitle.map(|subtitle| {
            div()
                .min_w_0()
                .text_xs()
                .text_color(fg)
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

/// One entry of an agent-reported catalogue: the value that goes back over the
/// protocol, and the agent's own name for it.
///
/// Every pickable axis has this shape, which is what lets the label rules below
/// have one home instead of one per picker.
pub trait CatalogueChoice {
    fn value(&self) -> &str;
    fn name(&self) -> &str;
}

impl CatalogueChoice for backend::ModeInfo {
    fn value(&self) -> &str {
        &self.value
    }

    fn name(&self) -> &str {
        &self.name
    }
}

impl CatalogueChoice for backend::AccessInfo {
    fn value(&self) -> &str {
        &self.value
    }

    fn name(&self) -> &str {
        &self.name
    }
}

impl CatalogueChoice for backend::SandboxInfo {
    fn value(&self) -> &str {
        &self.value
    }

    fn name(&self) -> &str {
        &self.name
    }
}

impl CatalogueChoice for backend::ReasoningInfo {
    fn value(&self) -> &str {
        &self.value
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// The value in force on an axis: the user's stored pick while the catalogue
/// still offers it, else what the agent reports.
///
/// The membership test is what keeps one stored value from leaking across axes
/// that share it — Claude Code stores its work mode and its access policy as one
/// value — and what lets a pick whose model has since changed drop out.
pub fn in_force<'a, T: CatalogueChoice>(
    items: &'a [T],
    stored: Option<&'a str>,
    reported: Option<&'a str>,
) -> Option<&'a str> {
    stored
        .filter(|value| items.iter().any(|item| item.value() == *value))
        .or(reported)
}

/// The agent's own name for a value, the value itself when the catalogue no
/// longer lists it (a value the user can still read beats a blank chip), and the
/// inherited wording when there is nothing in force.
pub fn choice_label<T: CatalogueChoice>(items: &[T], value: Option<&str>, agent: &str) -> String {
    match value {
        Some(value) => items
            .iter()
            .find(|item| item.value() == value)
            .map(|item| item.name().to_string())
            .unwrap_or_else(|| value.to_string()),
        None => inherited_label(agent),
    }
}

/// The label an axis chip shows, or no chip at all (`None`) when the agent
/// reports no such axis — which is what keeps the chip off the toolbar.
pub fn axis_label<T: CatalogueChoice>(
    items: &[T],
    value: Option<&str>,
    agent: &str,
) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    Some(choice_label(items, value, agent))
}

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use core::prelude::v1::test;

    use super::{axis_label, choice_label, in_force, inherited_label};
    use crate::backend::{ModeInfo, ReasoningInfo};

    fn mode(value: &str, name: &str) -> ModeInfo {
        ModeInfo {
            value: value.into(),
            name: name.into(),
        }
    }

    /// A stored pick only stands while the catalogue still offers it; once it
    /// does not — a pick belonging to the axis that shares this one's value, or
    /// one made for a model since changed — the agent's report decides.
    #[test]
    fn a_stored_pick_only_stands_while_it_is_offered() {
        let items = vec![mode("ask", "Ask"), mode("build", "Build")];

        assert_eq!(in_force(&items, Some("build"), Some("ask")), Some("build"));
        assert_eq!(
            in_force(&items, Some("gone"), Some("ask")),
            Some("ask"),
            "an unlisted pick yields to the report"
        );
        assert_eq!(in_force(&items, None, Some("ask")), Some("ask"));
        assert_eq!(in_force(&items, None, None), None);
    }

    /// A listed value resolves to the agent's own wording, an unlisted one shows
    /// as itself, and nothing in force says whose configuration is deciding.
    #[test]
    fn a_choice_resolves_to_the_agents_own_wording() {
        let items = vec![mode("ask", "Ask")];

        assert_eq!(choice_label(&items, Some("ask"), "Claude Code"), "Ask");
        assert_eq!(
            choice_label(&items, Some("gone"), "Claude Code"),
            "gone",
            "an unlisted value is still worth showing"
        );
        assert_eq!(
            choice_label(&items, None, "Claude Code"),
            inherited_label("Claude Code")
        );
    }

    /// No catalogue entries means the axis does not exist on this agent, so
    /// there is no chip to label — the rule is per axis, not per entry type.
    #[test]
    fn an_axis_with_no_entries_has_no_label() {
        assert_eq!(
            axis_label(&Vec::<ModeInfo>::new(), None, "Claude Code"),
            None
        );
        assert_eq!(
            axis_label(&[mode("ask", "Ask")], Some("ask"), "Claude Code").as_deref(),
            Some("Ask")
        );

        let efforts = vec![ReasoningInfo {
            value: "low".into(),
            name: "Low".into(),
        }];
        assert_eq!(
            axis_label(&efforts, Some("low"), "Codex").as_deref(),
            Some("Low")
        );
    }
}
