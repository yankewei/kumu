//! Hover labels for app elements: one tooltip overlay per window.
//!
//! GPUI Kit puts tooltips on its own controls (`.tooltip()` on `Button`,
//! `Switch`, the sidebar menu), but those ask the overlay `Root` mounts, and
//! that overlay is crate-private: the tooltip surface is public while the trait
//! that requests it (`ManagedTooltipExt`) is not, so an app element cannot
//! borrow that wiring. The app therefore mounts an overlay of its own, rendered
//! by each window, and asks it the way the library's own helper does.
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

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::base::ElementExt;
use gpui_kit::base::{Placement, TooltipOverlay, TooltipRequest};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::*;
use gpui_kit::*;

/// A window's tooltips.
pub struct Tooltips {
    overlay: Entity<TooltipOverlay>,
}

impl Tooltips {
    pub fn new(cx: &mut App) -> Self {
        Self {
            overlay: cx.new(|_| TooltipOverlay::new()),
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
        let text = text.into();
        self.overlay.update(cx, |overlay, cx| {
            if hovered {
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
            } else {
                overlay.request_hide(window, cx);
            }
        });
    }
}

impl Render for Tooltips {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.overlay.clone()
    }
}

/// A hover label on a stateful element: `div().id("x").hover_label(&tooltips, "…")`.
pub trait HoverLabel: StatefulInteractiveElement + ElementExt + Sized {
    /// Show `text` above this element while the pointer is on it.
    fn hover_label(
        self,
        tooltips: &Entity<Tooltips>,
        text: impl Into<SharedString> + 'static,
    ) -> Self {
        let trigger = Rc::new(Cell::new(Bounds::default()));
        let capture = trigger.clone();
        let text = text.into();
        let tooltips = tooltips.clone();
        self.on_prepaint(move |bounds, _, _| capture.set(bounds))
            .on_hover(move |hovered, window, cx| {
                tooltips.update(cx, |tooltips, cx| {
                    tooltips.hover(*hovered, trigger.get(), text.clone(), window, cx)
                });
            })
    }
}

impl<E: StatefulInteractiveElement + ElementExt> HoverLabel for E {}
