//! Hover labels for app elements: one tooltip overlay per window.
//!
//! GPUI Kit puts tooltips on its own controls (`.tooltip()` on `Button`,
//! `Switch`, the sidebar menu), but those ask the overlay `Root` mounts, and
//! that overlay is crate-private: the tooltip surface is public while the trait
//! that requests it (`ManagedTooltipExt`) is not, so an app element cannot
//! borrow that wiring. The app therefore registers an overlay of its own as a
//! [`RootPlugin`](gpui_kit::base::RootPlugin); every window's `Root` creates one
//! and mounts it above the app's content, and elements ask it the way the
//! library's own helper does.
//!
//! Everything a tooltip decides for itself stays with the library: the surface,
//! the placement (above the trigger, flipped below it when there is no room
//! there, then clamped into the window), the delay before it appears on hover,
//! and the grace period before it goes. What is left here is the one thing the
//! overlay cannot know and the element must tell it — the trigger's bounds,
//! which exist only after layout, so they are read back in prepaint and handed
//! over on hover.
//!
//! The library's tooltips also fade in; that renderer is crate-private too, so
//! these appear without it.
//!
//! A hover label normally goes when the pointer leaves its trigger, but a
//! trigger can also leave the tree first: closing a pane or switching project
//! removes the button the tooltip belongs to, and a removed element never gets
//! the hover-out that would hide the label. The label then floats over whatever
//! took its place. Two guards close that hole: a click on a label's element
//! hides its label at once (that is the close button's own click), and while a
//! label is up it is re-checked against the pointer and the window's active
//! state, so a label that lost its trigger — or a window that lost focus —
//! takes itself down.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::base::ElementExt;
use gpui_kit::base::{Placement, TooltipOverlay, TooltipRequest};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::*;
use gpui_kit::*;

/// How often a shown label checks that its trigger is still under the pointer.
/// Short enough that a stray label reads as a glitch rather than a stuck popup,
/// long enough to cost one wake-up per quarter second while a tooltip is up.
const REVALIDATE_INTERVAL: Duration = Duration::from_millis(250);

/// A label goes after this long even when the trigger still seems to be under
/// the pointer. A trigger that left the tree in that spot looks exactly the
/// same as one that stayed, so the timeout is what keeps that case from
/// leaving a label up forever.
const MAX_LIFETIME: Duration = Duration::from_secs(10);

/// A window's tooltips.
pub struct Tooltips {
    overlay: Entity<TooltipOverlay>,
    /// Identifies the current label, so a watch from an older one stops.
    epoch: usize,
    watch: Option<Task<()>>,
}

impl Tooltips {
    pub fn new(cx: &mut App) -> Self {
        Self {
            overlay: cx.new(|_| TooltipOverlay::new()),
            epoch: 0,
            watch: None,
        }
    }

    /// Show `text` above the trigger at `trigger`, or let it go once the pointer
    /// has left one (`hovered`).
    ///
    /// The overlay keeps its own schedule: a second trigger asking while one is
    /// already up swaps the label at once, and leaving waits out a grace period
    /// first, so moving along a run of triggers does not blink.
    fn hover(
        &mut self,
        hovered: bool,
        trigger: Bounds<Pixels>,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !hovered {
            self.clear_watch();
            self.overlay
                .update(cx, |overlay, cx| overlay.request_hide(window, cx));
            return;
        }
        let text = text.into();
        self.overlay.update(cx, |overlay, cx| {
            overlay.request_show(
                TooltipRequest::new(trigger, move |window, cx| {
                    // A label is meta text, which the app sets a step below
                    // body copy. The surface is the library's own tooltip.
                    Tooltip::new(text.clone()).text_xs().build(window, cx)
                })
                // The overlay would prefer this side on its own; asking for
                // it keeps "above the trigger" a property of the label
                // rather than of a default.
                .with_placement(Placement::Top),
                window,
                cx,
            );
        });
        self.watch(trigger, window, cx);
    }

    /// Keep track of the label that is up, and take it down as soon as its
    /// trigger is no longer somewhere the pointer can reach.
    fn watch(&mut self, trigger: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.epoch += 1;
        let epoch = self.epoch;
        self.watch = Some(cx.spawn_in(window, async move |this, cx| {
            let mut shown_for = Duration::ZERO;
            loop {
                cx.background_executor().timer(REVALIDATE_INTERVAL).await;
                shown_for += REVALIDATE_INTERVAL;
                let expired = shown_for >= MAX_LIFETIME;
                let keep_watching = this.update_in(cx, |this, window, cx| {
                    if this.epoch != epoch {
                        return false;
                    }
                    if !expired
                        && window.is_window_active()
                        && trigger.contains(&window.mouse_position())
                    {
                        return true;
                    }
                    // The trigger left the tree (so no hover-out will come),
                    // the pointer moved off it, or the window went inactive.
                    this.clear_watch();
                    this.overlay.update(cx, |overlay, cx| overlay.hide(cx));
                    false
                });
                if !matches!(keep_watching, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// Stop watching the current label without touching the overlay; the
    /// caller decides whether it goes now or after the overlay's grace period.
    fn clear_watch(&mut self) {
        self.epoch += 1;
        self.watch = None;
    }

    /// Take the label down now: a click reached its trigger.
    fn hide(&mut self, cx: &mut Context<Self>) {
        self.clear_watch();
        self.overlay.update(cx, |overlay, cx| overlay.hide(cx));
    }
}

impl Render for Tooltips {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.overlay.clone()
    }
}

/// [`Root`](gpui_kit::base::Root) mounts one tooltip overlay per window.
impl gpui_kit::base::RootPlugin for Tooltips {}

/// The window's tooltips, when this window's `Root` has mounted them.
fn tooltips(window: &Window, cx: &mut App) -> Option<Entity<Tooltips>> {
    window
        .root::<gpui_kit::base::Root>()
        .flatten()
        .and_then(|root| root.read(cx).plugin::<Tooltips>())
}

/// A hover label on a stateful element: `div().id("x").hover_label("…")`.
pub trait HoverLabel: StatefulInteractiveElement + ElementExt + Sized {
    /// Show `text` above this element while the pointer is on it.
    fn hover_label(self, text: impl Into<SharedString> + 'static) -> Self {
        let trigger = Rc::new(Cell::new(Bounds::default()));
        let capture = trigger.clone();
        let text = text.into();
        self.on_prepaint(move |bounds, _, _| capture.set(bounds))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                // A click can remove the element the label belongs to (the
                // pane's close button, a project row switching views). Hide
                // before that, so the label cannot outlive its trigger.
                if let Some(tooltips) = tooltips(window, cx) {
                    tooltips.update(cx, |tooltips, cx| tooltips.hide(cx));
                }
            })
            .on_hover(move |hovered, window, cx| {
                // Tooltips belong to the window the user is working in; a
                // stationary pointer over a background window would otherwise
                // pop one up while the user is busy elsewhere.
                if *hovered && !window.is_window_active() {
                    return;
                }
                let Some(tooltips) = tooltips(window, cx) else {
                    return;
                };
                tooltips.update(cx, |tooltips, cx| {
                    tooltips.hover(*hovered, trigger.get(), text.clone(), window, cx)
                })
            })
    }
}

impl<E: StatefulInteractiveElement + ElementExt> HoverLabel for E {}
