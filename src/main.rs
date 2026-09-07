//! kumu — local multi-agent desktop shell.
//!
//! Follows GPUI Kit's design guides:
//!   - layout uses rem helpers (w_64/p_4/gap_3/text_sm…), px only for physical
//!     boundaries (hairlines via h_px/w_px, window chrome);
//!   - colors/surfaces read from cx.theme() by semantic role (sidebar uses
//!     sidebar_* tokens; no raw hex/hsla in app code);
//!   - density/spacing/typography stay on the semantic scale;
//!   - primary reserved for the decision commit, internal nav = ghost/default.
//!
//! Module layout:
//!   - `ui/`      GPUI views: `home` (Home window), `settings` (Settings
//!     window), `widgets` (theme-token helpers shared across windows);
//!   - `theme.rs` themes/*.json loading into the ThemeRegistry;
//!   - `agents.rs` static registry of agents kumu can drive;
//!   - `db.rs`    local SQLite persistence (projects/sessions/messages);
//!   - `acp.rs`   ACP client layer on a dedicated thread (settings probes
//!     agents over it to discover their models; full sessions still TODO).

#[allow(dead_code)] // run() not wired into the UI yet
mod acp;
mod agents;
mod assets;
mod db;
mod theme;
mod ui;

use gpui_kit::component::*;
use gpui_kit::*;
use ui::KumuApp;

fn main() {
    let app = gpui_kit::application().with_assets(assets::KumuAssetSource);

    app.run(move |cx| {
        gpui_kit::init(cx);
        theme::load_kumu_themes(cx);
        let mut window_options = TitleBar::window_options();
        window_options.window_bounds = Some(WindowBounds::centered(size(px(1360.), px(900.)), cx));
        window_options.window_min_size = Some(size(px(960.), px(640.)));

        cx.spawn(async move |cx| {
            match cx.open_window(window_options, |window, cx| {
                let view = cx.new(|cx| KumuApp::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            }) {
                Ok(_) => eprintln!("[ui] window opened"),
                Err(e) => eprintln!("[ui] window open FAILED: {e:?}"),
            }
        })
        .detach();
    });
}
