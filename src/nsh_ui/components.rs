//! Kumu's styled Pure-Rust NSH component wrappers over `nsh-base`.
//!
//! Enforces the design constraints in `AGENTS.md`:
//! - Strict two-step type scale (`text_sm_size` = 14px, `text_xs_size` = 12px, scaled by UI font size);
//! - Semantic theme tokens only (`nsh_base::theme()`);
//! - Primary button reserved for decision commits; internal nav uses Ghost/Default/Outline.

use nsh_base::{
    ButtonElement, SwitchElement, TooltipElement, button, input, switch, theme, tooltip,
};
use nsh_core::element::{Input, IntoProp, Text};
use nsh_core::prelude::*;

use super::assets::{asset_src, icon_svg};
use super::theme::{TOKEN_ACCENT_PILL_BG, TOKEN_LINK, kumu_color, text_sm_size, text_xs_size};

const CONTROL_COLOR_TRANSITION_MS: f32 = 120.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonVariant {
    Primary,
    Default,
    Outline,
    Ghost,
    Danger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlTone {
    Default,
    Sidebar,
    Pane,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PillTone {
    Neutral,
    Accent,
}

/// 14px body text (`text_sm`).
pub fn body_text(content: impl IntoReactive<SharedString>) -> Text {
    text(content)
        .text_size(text_sm_size)
        .text_color(move || theme().foreground)
}

/// 14px medium section/card title.
pub fn section_title(content: impl IntoReactive<SharedString>) -> Text {
    body_text(content).font_weight(FontWeight::MEDIUM)
}

/// 12px secondary/meta text (`text_xs`).
pub fn meta_text(content: impl IntoReactive<SharedString>) -> Text {
    text(content)
        .text_size(text_xs_size)
        .text_color(move || theme().muted_foreground)
}

/// 12px primary-color medium label (`text_xs` + `font_medium`), used for row titles.
pub fn row_title(content: impl IntoReactive<SharedString>) -> Text {
    text(content)
        .text_size(text_xs_size)
        .font_weight(FontWeight::MEDIUM)
        .text_color(move || theme().foreground)
}

fn button_frame() -> ButtonElement {
    button()
        .rounded(move || theme().radius)
        .active(|s| s.opacity(0.75))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(move || theme().ring))
        .disabled_style(|s| s.opacity(0.45))
        .when(!reduce_motion(), |s| {
            s.transition_colors().duration(CONTROL_COLOR_TRANSITION_MS)
        })
}

/// Styled Kumu button built over `nsh_base::button()`.
pub fn kumu_button(
    label: impl IntoReactive<SharedString>,
    variant: ButtonVariant,
) -> ButtonElement {
    let base = button_frame()
        .flex_row()
        .items_center()
        .justify_center()
        .px_2p5()
        .py_1();

    match variant {
        ButtonVariant::Primary => base.bg(move || theme().primary).child(
            text(label)
                .text_size(text_xs_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || theme().primary_foreground),
        ),
        ButtonVariant::Default => base
            .bg(move || theme().secondary)
            .border_1()
            .border_color(move || theme().border)
            .hover(|s| s.bg(move || theme().accent))
            .child(
                text(label)
                    .text_size(text_xs_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(move || theme().secondary_foreground),
            ),
        ButtonVariant::Outline => base
            .bg(move || theme().background)
            .border_1()
            .border_color(move || theme().border)
            .hover(|s| s.bg(move || theme().accent))
            .child(
                text(label)
                    .text_size(text_xs_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(move || theme().foreground),
            ),
        ButtonVariant::Ghost => base.hover(|s| s.bg(move || theme().accent)).child(
            text(label)
                .text_size(text_xs_size)
                .text_color(move || theme().foreground),
        ),
        ButtonVariant::Danger => base.bg(move || theme().destructive).child(
            text(label)
                .text_size(text_xs_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || theme().primary_foreground),
        ),
    }
}

/// Styled icon-only command button. Its label is both the accessible name and
/// the tooltip, so icon controls stay discoverable to pointer and keyboard users.
pub fn kumu_icon_button(
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    tone: ControlTone,
    on_press: impl Fn() + 'static,
) -> View {
    let label = label.into();
    let button = button_frame()
        .accessibility_label(label.clone())
        .flex_shrink_0()
        .size(px(24.0))
        .items_center()
        .justify_center()
        .on_press(on_press);

    let button = match tone {
        ControlTone::Default => button.hover(|s| s.bg(move || theme().accent.opacity(0.5))),
        ControlTone::Sidebar => button.hover(|s| s.bg(move || theme().sidebar_accent.opacity(0.5))),
        ControlTone::Pane => button.hover(|s| s.bg(move || theme().accent.opacity(0.5))),
    };

    view()
        .tooltip(kumu_tooltip(label))
        .child(button.child(icon))
}

/// Styled row command used for navigation and selectable rows in the sidebar
/// and small pickers. Callers provide the row's content and layout.
pub fn kumu_list_button<M>(
    label: impl Into<SharedString>,
    selected: impl IntoProp<bool, M>,
    tone: ControlTone,
) -> ButtonElement {
    let button = button_frame()
        .accessibility_label(label.into())
        .selected(selected)
        .flex_row()
        .w_full()
        .min_w_0()
        .items_center();

    match tone {
        ControlTone::Default => button
            .selected_style(|s| s.bg(move || theme().accent))
            .hover(|s| s.bg(move || theme().accent.opacity(0.5))),
        ControlTone::Sidebar => button
            .selected_style(|s| s.bg(move || theme().sidebar_accent))
            .hover(|s| s.bg(move || theme().sidebar_accent.opacity(0.5))),
        ControlTone::Pane => button
            .selected_style(|s| s.bg(AdaptiveColor::TRANSPARENT))
            .hover(|s| s.bg(move || theme().accent.opacity(0.5))),
    }
}

/// A tab button that exposes its selected state to accessibility APIs.
pub fn kumu_tab_button(label: impl Into<SharedString>, selected: bool) -> ButtonElement {
    button_frame()
        .accessibility_label(label.into())
        .selected(selected)
        .flex_row()
        .flex_shrink_0()
        .items_center()
        .gap_1()
        .h(px(26.0))
        .px_2()
        .border_1()
        .border_color(move || {
            if selected {
                theme().border
            } else {
                AdaptiveColor::TRANSPARENT
            }
        })
        .hover(|s| {
            s.bg(move || {
                if selected {
                    theme().background
                } else {
                    theme().accent.opacity(0.5)
                }
            })
        })
        .selected_style(|s| s.bg(move || theme().background))
}

/// Styled Kumu single-line input over `nsh_base::input()`.
pub fn kumu_input(placeholder: impl IntoReactive<String>) -> Input {
    input()
        .placeholder(placeholder)
        .placeholder_color(move || theme().muted_foreground.light)
        .px_2p5()
        .py_1p5()
        .border_1()
        .border_color(move || theme().input)
        .rounded(move || theme().radius)
        .bg(move || theme().background)
        .text_size(text_xs_size)
        .text_color(move || theme().foreground)
}

/// Styled Kumu toggle switch without inline label (for right-aligned settings rows).
pub fn kumu_switch_bare<M>(checked: impl IntoProp<bool, M>) -> SwitchElement {
    switch()
        .checked(checked)
        .flex_row()
        .items_center()
        .track_class("w-8 h-4 rounded-full bg-input checked:bg-primary p-0.5")
        .thumb_class("w-3 h-3 rounded-full bg-background checked:translate-x-4")
}

/// Borderless status pill matching `AGENTS.md`.
pub fn pill(label: impl IntoReactive<SharedString>, tone: PillTone) -> View {
    type ColorFn = fn() -> AdaptiveColor;
    let (bg, fg): (ColorFn, ColorFn) = match tone {
        PillTone::Neutral => (|| theme().muted, || theme().muted_foreground),
        PillTone::Accent => (
            || kumu_color(TOKEN_ACCENT_PILL_BG, |t| t.accent),
            || kumu_color(TOKEN_LINK, |t| t.accent_foreground),
        ),
    };
    view()
        .px_2()
        .py_0p5()
        .rounded(move || theme().radius)
        .bg(bg)
        .child(
            text(label)
                .text_size(text_xs_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(fg),
        )
}

/// Card container for grouped rows (Settings / detail sections).
pub fn section_card() -> View {
    view()
        .flex_col()
        .w_full()
        .px_5()
        .py_4()
        .rounded(px(8.0))
        .bg(move || theme().card)
}

/// Each agent's accent, used for its glyph and the tile behind it so the
/// monochrome brand marks still read as distinct, colored choices.
fn agent_accent(id: &str) -> AdaptiveColor {
    match id {
        "opencode" => AdaptiveColor::rgb(0x475569, 0x94a3b8),
        "omp" => AdaptiveColor::rgb(0xd97706, 0xfbbf24),
        "codex" => AdaptiveColor::rgb(0x0d8a6a, 0x34d399),
        "claude" => AdaptiveColor::rgb(0xc26144, 0xf0936e),
        "pi" => AdaptiveColor::rgb(0x7c3aed, 0xa78bfa),
        "cursor" => AdaptiveColor::rgb(0x4f46e5, 0x818cf8),
        "grok" => AdaptiveColor::rgb(0x0f172a, 0xcbd5e1),
        "agy" => AdaptiveColor::rgb(0x6366f1, 0x93c5fd),
        _ => AdaptiveColor::rgb(0x64748b, 0x94a3b8),
    }
}

/// Shared 20×20 tinted agent logo badge used in pickers and onboarding.
pub fn kumu_agent_logo(id: &str, logo: Option<&'static str>) -> AnyElement {
    let accent = agent_accent(id);
    let glyph: AnyElement = match logo {
        Some(path) if path.ends_with(".svg") => icon_svg(path, 14.0)
            .text_color(move || accent)
            .into_element(),
        Some(path) => image(asset_src(path))
            .w(px(14.0))
            .h(px(14.0))
            .flex_shrink_0()
            .into_element(),
        None => icon_svg("icons/bot.svg", 14.0)
            .text_color(move || accent)
            .into_element(),
    };
    view()
        .flex_shrink_0()
        .w(px(20.0))
        .h(px(20.0))
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .bg(move || accent.opacity(0.16))
        .border_1()
        .border_color(move || accent.opacity(0.35))
        .child(glyph)
        .into_element()
}

/// Styled Kumu hover tooltip built over `nsh_base::tooltip()`.
pub fn kumu_tooltip(label: impl IntoReactive<SharedString>) -> TooltipElement {
    tooltip()
        .px_2()
        .py_1()
        .rounded(move || theme().radius)
        .bg(move || theme().popover)
        .border_1()
        .border_color(move || theme().border)
        .shadow_md()
        .child(
            text(label)
                .text_size(text_xs_size)
                .text_color(move || theme().popover_foreground),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_engine::EngineConfig;

    #[component]
    fn DesignSystemFixture() -> impl IntoElement {
        let count = signal(0u32);
        let enabled = signal(false);
        let draft = signal(String::new());
        let selected = signal(false);

        view()
            .flex_1()
            .gap_3()
            .p_4()
            .child(section_title("Kumu NSH Design System"))
            .child(
                section_card()
                    .child(meta_text(move || format!("Clicks: {}", count.get())))
                    .child(
                        kumu_button("Commit", ButtonVariant::Primary)
                            .on_press(move || count.update(|n| *n += 1)),
                    )
                    .child(kumu_icon_button(
                        "增加计数",
                        text("+"),
                        ControlTone::Default,
                        move || count.update(|n| *n += 1),
                    ))
                    .child(
                        kumu_list_button("选择项目", move || selected.get(), ControlTone::Sidebar)
                            .px_2()
                            .py_1()
                            .child(body_text("项目一"))
                            .child(meta_text(
                                move || {
                                    if selected.get() { "已选" } else { "未选" }
                                },
                            ))
                            .on_press(move || selected.update(|value| *value = !*value)),
                    )
                    .child(
                        kumu_switch_bare(move || enabled.get())
                            .gap_2()
                            .child(body_text("Auto Resume"))
                            .on_change(move |next: &bool| enabled.set(*next)),
                    )
                    .child(pill(
                        move || if enabled.get() { "ON" } else { "OFF" },
                        PillTone::Accent,
                    ))
                    .child(
                        kumu_input("Type here")
                            .value(move || draft.get())
                            .on_change(move |val: &str| draft.set(val.to_owned())),
                    ),
            )
    }

    #[test]
    fn kumu_nsh_components_work_headlessly() {
        let mut app =
            nsh_testing::Headless::start(EngineConfig::new(nsh_testing::windowed_root::<
                DesignSystemFixture,
            >()))
            .expect("start headless NSH fixture");
        app.wait_for("Clicks: 0").expect("initial render");
        app.wait_for("OFF").expect("initial pill");
        app.press("Commit").expect("press button");
        app.wait_for("Clicks: 1").expect("button updates signal");
        app.press("增加计数").expect("press icon button");
        app.wait_for("Clicks: 2")
            .expect("icon button updates signal");
        app.press("选择项目").expect("press selectable row");
        app.wait_for("已选").expect("selected row updates");
        app.press("Auto Resume").expect("toggle switch");
        app.wait_for("ON").expect("switch updates pill");
    }
}
