//! Window overlay layers that GPUI Kit's `Root` does not mount itself.
//!
//! `Root::render` (gpui-component `src/root.rs`) mounts exactly the
//! text-selection layer, the view, and the tooltip / native-menu overlays. The
//! tooltip overlay there is the library's own and is crate-private, so app
//! elements carry their hover labels through a second one of the same kind
//! ([`Tooltips`]) mounted here. Dialogs, sheets and notifications live in
//! sibling layers that every `Root`-hosting view has to add to its own tree —
//! the library's own tests mount the dialog layer the same way (see
//! `ModalScopeTestView` in `gpui-component/src/text/window_selection.rs`).
//!
//! Forgetting one fails silently: `window.open_alert_dialog` still pushes the
//! dialog into `Root`, but a layer that never renders looks exactly like a
//! dialog that never opened. So every window goes through [`window_layers`]
//! rather than calling `Root::render_*_layer` at the mount site.

use gpui_kit::component::Root;
use gpui_kit::*;

use super::tooltip::Tooltips;

/// The overlay layers for a `Root`-hosted window.
///
/// Sheets and notifications would be mounted here too, one call each; the app
/// opens neither yet, so dialogs and hover labels are what there is.
///
/// `use<>` opts out of edition 2024's implicit lifetime capture: the layer
/// borrows neither `window` nor `cx` once built, so callers can keep using
/// `cx` while holding it.
pub fn window_layers(
    tooltips: &Entity<Tooltips>,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement + use<> {
    div()
        .children(Root::render_dialog_layer(window, cx))
        .child(tooltips.clone())
}
