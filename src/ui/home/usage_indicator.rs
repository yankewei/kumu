//! The composer's context indicator: how full the session's context is, and
//! what the session has cost, as its agent last reported.
//!
//! A ring and a percentage when the window is known, the token count alone when
//! it is not, and nothing at all before the agent has reported anything. The
//! ring turns to the warning colour at 70% and to the danger colour at 90% —
//! the point where it is time to compact or start a new session. The details
//! (tokens against the window, the cost) are in its hover label.

use gpui_kit::component::progress::ProgressCircle;
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::KumuApp;
use crate::agent::{Cost, Usage};
use crate::db::Session;
use crate::ui::tooltip::HoverLabel as _;

/// How close to full the context is, which picks the ring's colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Normal,
    Warning,
    Danger,
}

fn level(fraction: f64) -> Level {
    if fraction >= 0.9 {
        Level::Danger
    } else if fraction >= 0.7 {
        Level::Warning
    } else {
        Level::Normal
    }
}

/// A token count at a glance: `850`, `12.3k`, `1.2M`.
fn format_tokens(tokens: u64) -> String {
    let compact = |value: f64, unit: &str| {
        let text = format!("{value:.1}");
        format!("{}{unit}", text.trim_end_matches(".0"))
    };
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => compact(tokens as f64 / 1_000., "k"),
        _ => compact(tokens as f64 / 1_000_000., "M"),
    }
}

/// Dollars get a `$`; any other currency its code. Below a cent the value is
/// still shown, since a short session's cost is exactly that small.
fn format_cost(cost: &Cost) -> String {
    let amount = if cost.amount < 0.01 {
        format!("{:.4}", cost.amount)
    } else {
        format!("{:.2}", cost.amount)
    };
    if cost.currency.eq_ignore_ascii_case("USD") {
        format!("${amount}")
    } else {
        format!("{amount} {}", cost.currency)
    }
}

/// What the indicator shows: the ring's fill (0–1) when the window is known,
/// the short label beside it, and the hover label. `None` when the report has
/// nothing to show.
fn describe(usage: &Usage) -> Option<(Option<f64>, String, String)> {
    let cost = usage.cost.as_ref();
    let fraction = match (usage.used, usage.window) {
        (Some(used), Some(window)) => Some((used as f64 / window as f64).min(1.)),
        _ => None,
    };
    let short = match (fraction, usage.used, cost) {
        (Some(fraction), _, _) => format!("{:.0}%", fraction * 100.),
        (None, Some(used), _) => format_tokens(used),
        (None, None, Some(cost)) => format_cost(cost),
        (None, None, None) => return None,
    };
    let context = match (usage.used, usage.window) {
        (Some(used), Some(window)) => format!(
            "上下文 {} / {}（{:.0}%）",
            format_tokens(used),
            format_tokens(window),
            fraction.unwrap_or_default() * 100.
        ),
        (Some(used), None) => format!("上下文 {} tokens", format_tokens(used)),
        (None, Some(window)) => format!("上下文用量未知 / {}", format_tokens(window)),
        (None, None) => "上下文用量未知".to_string(),
    };
    let detail = match cost {
        Some(cost) => format!("{context} · 费用 {}", format_cost(cost)),
        None => context,
    };
    Some((fraction, short, detail))
}

impl KumuApp {
    /// The indicator for the session, or nothing before its agent has reported.
    pub(super) fn usage_indicator(
        &self,
        session: &Session,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let usage = self.workspace.read(cx).usage(session.id)?;
        let (fraction, short, detail) = describe(usage)?;
        let theme = cx.theme();
        let color = match fraction.map(level) {
            Some(Level::Danger) => theme.danger,
            Some(Level::Warning) => theme.warning,
            _ => theme.muted_foreground,
        };
        Some(
            h_flex()
                .id("usage-indicator")
                .items_center()
                .gap_1()
                .px_1()
                .text_xs()
                .text_color(color)
                .when_some(fraction, |row, fraction| {
                    row.child(
                        ProgressCircle::new("usage-ring")
                            .xsmall()
                            .color(color)
                            .value(fraction as f32 * 100.),
                    )
                })
                .child(short)
                .hover_label(&self.tooltips, detail)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use core::prelude::v1::test;

    use super::{Level, describe, format_cost, format_tokens, level};
    use crate::agent::{Cost, Usage};

    #[test]
    fn token_counts_read_at_a_glance() {
        assert_eq!(format_tokens(850), "850");
        assert_eq!(format_tokens(12_345), "12.3k");
        assert_eq!(format_tokens(200_000), "200k");
        assert_eq!(format_tokens(1_000_000), "1M");
    }

    #[test]
    fn costs_keep_their_currency_and_small_amounts() {
        assert_eq!(format_cost(&Cost::usd(0.126)), "$0.13");
        assert_eq!(format_cost(&Cost::usd(0.0042)), "$0.0042");
        let euros = Cost {
            amount: 1.5,
            currency: "EUR".into(),
        };
        assert_eq!(format_cost(&euros), "1.50 EUR");
    }

    #[test]
    fn the_colour_changes_at_seventy_and_ninety_percent() {
        assert_eq!(level(0.69), Level::Normal);
        assert_eq!(level(0.7), Level::Warning);
        assert_eq!(level(0.9), Level::Danger);
    }

    /// With a window the ring and a percentage; without one the token count;
    /// with only a cost, the cost; with nothing, no indicator.
    #[test]
    fn the_indicator_shows_what_the_report_knows() {
        let full = Usage {
            used: Some(60_000),
            window: Some(200_000),
            cost: Some(Cost::usd(0.12)),
        };
        let (fraction, short, detail) = describe(&full).unwrap();
        assert_eq!(fraction, Some(0.3));
        assert_eq!(short, "30%");
        assert_eq!(detail, "上下文 60k / 200k（30%） · 费用 $0.12");

        let no_window = Usage {
            used: Some(60_000),
            ..Default::default()
        };
        let (fraction, short, detail) = describe(&no_window).unwrap();
        assert_eq!((fraction, short.as_str()), (None, "60k"));
        assert_eq!(detail, "上下文 60k tokens");

        let cost_only = Usage {
            cost: Some(Cost::usd(0.5)),
            ..Default::default()
        };
        assert_eq!(describe(&cost_only).unwrap().1, "$0.50");

        assert!(describe(&Usage::default()).is_none());
    }
}
