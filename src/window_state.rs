//! The main window's geometry, saved across launches.
//!
//! Only the home window is persisted: Settings opens at a fixed size. The
//! restored rectangle is validated against the displays present now, so a
//! window saved on a monitor that is no longer attached opens centered.

use gpui_kit::*;

use crate::db;

/// Keep a restored window at least as large as the app's minimum; a corrupt or
/// stale document must not make the window unusable.
pub const MIN_WIDTH: f32 = 960.;
pub const MIN_HEIGHT: f32 = 640.;
const MAX_SIDE: f32 = 10_000.;

/// At least this much of the window must be visible on a display. A few pixels
/// of overlap is not enough to find or drag the window back.
const MIN_VISIBLE: f32 = 96.;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum WindowMode {
    #[default]
    Windowed,
    Maximized,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct SavedWindow {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    #[serde(default)]
    mode: WindowMode,
}

impl SavedWindow {
    fn from_bounds(bounds: WindowBounds) -> Self {
        let (rect, mode) = match bounds {
            WindowBounds::Windowed(rect) => (rect, WindowMode::Windowed),
            WindowBounds::Maximized(rect) => (rect, WindowMode::Maximized),
            WindowBounds::Fullscreen(rect) => (rect, WindowMode::Fullscreen),
        };
        Self {
            x: f32::from(rect.origin.x),
            y: f32::from(rect.origin.y),
            width: f32::from(rect.size.width),
            height: f32::from(rect.size.height),
            mode,
        }
    }

    /// The saved rectangle, clamped to the app's minimum size and accepted only
    /// when a meaningful part of it lands on one of `displays`.
    fn valid_bounds(&self, displays: &[Bounds<Pixels>]) -> Option<Bounds<Pixels>> {
        if !self.x.is_finite()
            || !self.y.is_finite()
            || !self.width.is_finite()
            || !self.height.is_finite()
        {
            return None;
        }
        let bounds = Bounds::new(
            point(px(self.x), px(self.y)),
            size(
                px(self.width.clamp(MIN_WIDTH, MAX_SIDE)),
                px(self.height.clamp(MIN_HEIGHT, MAX_SIDE)),
            ),
        );
        let visible = displays.iter().any(|display| {
            let overlap_width = (f32::from(bounds.right()).min(f32::from(display.right()))
                - f32::from(bounds.origin.x).max(f32::from(display.origin.x)))
            .max(0.);
            let overlap_height = (f32::from(bounds.bottom()).min(f32::from(display.bottom()))
                - f32::from(bounds.origin.y).max(f32::from(display.origin.y)))
            .max(0.);
            overlap_width >= MIN_VISIBLE && overlap_height >= MIN_VISIBLE
        });
        visible.then_some(bounds)
    }
}

/// The saved main-window geometry, when it is still usable on this machine.
pub fn load(cx: &App) -> Option<WindowBounds> {
    let saved: SavedWindow =
        serde_json::from_str(&db::get_setting(db::SETTING_WINDOW_BOUNDS)?).ok()?;
    let displays: Vec<Bounds<Pixels>> = cx
        .displays()
        .iter()
        .map(|display| display.visible_bounds())
        .collect();
    let bounds = saved.valid_bounds(&displays)?;
    Some(match saved.mode {
        WindowMode::Windowed => WindowBounds::Windowed(bounds),
        WindowMode::Maximized => WindowBounds::Maximized(bounds),
        WindowMode::Fullscreen => WindowBounds::Fullscreen(bounds),
    })
}

/// Persist the current main-window geometry.
pub fn save(window: &Window) {
    save_bounds(window.window_bounds());
}

/// Persist geometry read from a window handle, for the app-quit path where no
/// `Window` is passed to the callback.
pub fn save_bounds(bounds: WindowBounds) {
    let saved = SavedWindow::from_bounds(bounds);
    if let Ok(json) = serde_json::to_string(&saved) {
        db::set_setting(db::SETTING_WINDOW_BOUNDS, &json).ok();
    }
}

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use super::*;
    use core::prelude::v1::test;

    fn display(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(0.)), size(px(width), px(height)))
    }

    fn saved(x: f32, y: f32, width: f32, height: f32) -> SavedWindow {
        SavedWindow {
            x,
            y,
            width,
            height,
            mode: WindowMode::Windowed,
        }
    }

    /// A saved window on a display that still exists comes back unchanged.
    #[test]
    fn a_visible_window_is_restored() {
        let bounds = saved(100., 80., 1200., 800.)
            .valid_bounds(&[display(1440., 900.)])
            .expect("visible");
        assert_eq!(f32::from(bounds.origin.x), 100.);
        assert_eq!(f32::from(bounds.size.width), 1200.);
    }

    /// A window saved on a disconnected monitor must not open off-screen.
    #[test]
    fn an_offscreen_window_is_rejected() {
        assert!(
            saved(5000., 5000., 1200., 800.)
                .valid_bounds(&[display(1440., 900.)])
                .is_none()
        );
        assert!(saved(0., 0., 1200., 800.).valid_bounds(&[]).is_none());
    }

    /// A tiny or corrupt rectangle is clamped to the app's minimum instead of
    /// producing a window that cannot be used.
    #[test]
    fn a_restored_window_is_clamped_to_the_minimum_size() {
        let bounds = saved(10., 10., 100., 100.)
            .valid_bounds(&[display(1440., 900.)])
            .expect("clamped onto the display");
        assert_eq!(f32::from(bounds.size.width), MIN_WIDTH);
        assert_eq!(f32::from(bounds.size.height), MIN_HEIGHT);
        assert!(
            saved(f32::NAN, 0., 1200., 800.)
                .valid_bounds(&[display(1440., 900.)])
                .is_none()
        );
    }

    /// The mode survives the trip through the settings JSON.
    #[test]
    fn the_window_mode_round_trips() {
        let saved = SavedWindow {
            mode: WindowMode::Maximized,
            ..saved(10., 10., 1200., 800.)
        };
        let json = serde_json::to_string(&saved).unwrap();
        assert_eq!(serde_json::from_str::<SavedWindow>(&json).unwrap(), saved);
        assert!(json.contains("\"maximized\""));
    }
}
