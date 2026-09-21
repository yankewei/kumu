//! The permission prompt: a card shown above the composer when an agent asks
//! for authorization (`session/request_permission`).
//!
//! It deliberately sits next to the input rather than on the tool row it
//! authorizes, so the decision is always in the same place and never scrolls
//! out of view.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{CONTENT_MAX_REM, CONTENT_MIN_REM, KumuApp, PendingPermission};
use crate::agent::PermissionKind;
use crate::ui::widgets::pill;

impl KumuApp {
    /// The authorization card, on the same centered column as the conversation
    /// and the composer. The agent's own options are rendered in its order and
    /// passed straight back through, so `allow_always` / `reject_always` stay
    /// the agent's to remember.
    pub(super) fn render_permission_card(
        &self,
        session_id: i64,
        pending: &PendingPermission,
        cx: &Context<Self>,
    ) -> Div {
        let theme = cx.theme();
        let (radius, muted, muted_fg, warning, primary, link) = (
            theme.radius,
            theme.muted,
            theme.muted_foreground,
            theme.warning,
            theme.primary,
            theme.link,
        );

        // The agent decides how many options to offer, so let them wrap.
        let mut actions = h_flex().items_center().flex_wrap().gap_2();
        for option in &pending.options {
            let option_id = option.id.clone();
            let label = option.name.clone();
            let request_id = pending.request_id.clone();
            let button = Button::new(format!("perm-{}-{}", pending.request_id, option.id))
                .small()
                .label(option.name.clone())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.resolve_permission(
                        session_id,
                        &request_id,
                        Some(option_id.clone()),
                        label.clone(),
                        cx,
                    );
                }));
            actions = actions.child(match option.kind {
                PermissionKind::AllowOnce => button.primary(),
                PermissionKind::AllowAlways => button.secondary(),
                PermissionKind::RejectOnce | PermissionKind::RejectAlways => button.danger(),
                PermissionKind::Other => button.ghost(),
            });
        }
        // Always leave a way to decline without choosing one of the agent's
        // options; this is the protocol's `cancelled` outcome.
        let cancel_request_id = pending.request_id.clone();
        actions = actions.child(
            Button::new(format!("perm-cancel-{}", pending.request_id))
                .small()
                .ghost()
                .label("取消")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.resolve_permission(
                        session_id,
                        &cancel_request_id,
                        None,
                        "已取消".into(),
                        cx,
                    );
                })),
        );

        let mut card = v_flex()
            .gap_2()
            .p_2p5()
            .rounded(radius)
            .border_1()
            .border_color(warning)
            .bg(theme.background)
            .child(
                h_flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::LoaderCircle)
                            .small()
                            .text_color(warning),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_medium()
                            .child("此操作需要你的确认"),
                    )
                    .child(pill(
                        false,
                        "等待批准",
                        radius,
                        primary,
                        link,
                        muted,
                        muted_fg,
                    )),
            );

        if !pending.title.is_empty() {
            card = card.child(
                div()
                    .text_sm()
                    .text_color(muted_fg)
                    .child(pending.title.clone()),
            );
        }

        if !pending.detail.is_empty() {
            card = card.child(
                div()
                    .id(SharedString::from(format!(
                        "perm-detail-{}",
                        pending.request_id
                    )))
                    .max_h(rems(10.))
                    .overflow_y_scroll()
                    .rounded(radius)
                    .bg(muted.opacity(0.5))
                    .p_2()
                    .text_xs()
                    .text_color(muted_fg)
                    .child(pending.detail.clone()),
            );
        }

        // Shares the composer's backdrop so the two read as one input region,
        // separated from the conversation by the divider above.
        v_flex()
            .flex_none()
            .px_5()
            .pt_3()
            .bg(muted.opacity(0.5))
            .child(
                v_flex()
                    .w_full()
                    .max_w(rems(CONTENT_MAX_REM))
                    .min_w(rems(CONTENT_MIN_REM))
                    .mx_auto()
                    .child(card.child(actions)),
            )
    }
}

/// The one-line echo of a decision, kept under the tool row so the history
/// shows how the call was authorized.
pub(super) fn render_permission_decision(decision: &str, muted_fg: Hsla) -> Div {
    div()
        .ml_8()
        .px_1()
        .py_0p5()
        .text_xs()
        .text_color(muted_fg)
        .child(format!("权限 · {decision}"))
}
