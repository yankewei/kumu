//! Kumu's styled Pure-Rust NSH component wrappers over `nsh-base`.
//!
//! Enforces the design constraints in `AGENTS.md`:
//! - Strict two-step type scale (`text_sm_size` = 14px, `text_xs_size` = 12px, scaled by UI font size);
//! - Semantic theme tokens only (`nsh_base::theme()`);
//! - Primary button reserved for decision commits; internal nav uses Ghost/Default/Outline.

use nsh_base::{ButtonElement, SwitchElement, button, input, switch, theme};
use nsh_core::element::{Input, IntoProp, Text};
use nsh_core::prelude::*;

use super::theme::{TOKEN_ACCENT_PILL_BG, TOKEN_LINK, kumu_color, text_sm_size, text_xs_size};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ButtonVariant {
    Primary,
    Default,
    Outline,
    Ghost,
    Danger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PillTone {
    Neutral,
    Accent,
    Danger,
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

/// Styled Kumu button built over `nsh_base::button()`.
pub fn kumu_button(
    label: impl IntoReactive<SharedString>,
    variant: ButtonVariant,
) -> ButtonElement {
    let base = button()
        .flex_row()
        .items_center()
        .justify_center()
        .px_2p5()
        .py_1()
        .rounded(move || theme().radius)
        .active(|s| s.opacity(0.75))
        .disabled_style(|s| s.opacity(0.45));

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

/// Styled Kumu single-line input over `nsh_base::input()`.
pub fn kumu_input(placeholder: &'static str) -> Input {
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

/// Styled Kumu toggle switch over `nsh_base::switch()`.
#[allow(dead_code)]
pub fn kumu_switch<M>(checked: impl IntoProp<bool, M>, label: &'static str) -> SwitchElement {
    kumu_switch_bare(checked).gap_2().child(body_text(label))
}

/// Borderless status pill matching `AGENTS.md` / `src/ui/widgets.rs`.
pub fn pill(label: impl IntoReactive<SharedString>, tone: PillTone) -> View {
    type ColorFn = fn() -> AdaptiveColor;
    let (bg, fg): (ColorFn, ColorFn) = match tone {
        PillTone::Neutral => (|| theme().muted, || theme().muted_foreground),
        PillTone::Accent => (
            || kumu_color(TOKEN_ACCENT_PILL_BG, |t| t.accent),
            || kumu_color(TOKEN_LINK, |t| t.accent_foreground),
        ),
        PillTone::Danger => (|| theme().destructive, || theme().primary_foreground),
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

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_engine::EngineConfig;

    #[component]
    fn DesignSystemFixture() -> impl IntoElement {
        let count = signal(0u32);
        let enabled = signal(false);
        let draft = signal(String::new());

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
                    .child(
                        kumu_switch(move || enabled.get(), "Auto Resume")
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
        app.press("Auto Resume").expect("toggle switch");
        app.wait_for("ON").expect("switch updates pill");
    }
}
