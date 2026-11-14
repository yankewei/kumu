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
//!   - `app/`     projects and live terminals;
//!   - `plugins/` right-hand plugin panels opened from the title bar;
//!   - `git.rs`   worktree creation and commit history;
//!   - `db.rs`    local SQLite persistence (projects and the workspace layout);
//!   - `term/`    PTY + VT for shells and the agent TUIs they launch;
//!   - `agent/`   registry and executable lookup;
//!   - `window_state.rs` the main window's saved geometry.

mod agent;
mod app;
mod assets;
mod db;
mod dock_icon;
mod fonts;
mod git;
mod plugins;
mod remote;
mod term;
mod theme;
mod ui;
mod updates;
mod window_state;

use gpui_kit::component::*;
use gpui_kit::*;
use ui::KumuApp;

fn main() {
    if let Some(code) = agent::integration::run_cli() {
        std::process::exit(code);
    }
    // A registered native extension (OpenCode, Pi, OMP) is a copy of this
    // build's files, so an app update has to refresh it before any agent pane
    // starts. Hook-based agents run this binary and need nothing here.
    agent::integration::refresh_native();
    let app = gpui_kit::application().with_assets(assets::KumuAssetSource);

    // Closing the last window on macOS leaves the app running. A Dock click
    // must create a window again instead of only activating the empty app.
    app.on_reopen(open_main_window);

    app.run(move |cx| {
        gpui_kit::init(cx);
        ui::init(cx);
        cx.set_app_identity("com.yankewei.kumu", "Kumu");
        fonts::install(cx);
        dock_icon::install();
        theme::load_kumu_themes(cx);
        cx.spawn(async move |cx| cx.update(open_main_window))
            .detach();
    });
}

fn open_main_window(cx: &mut App) {
    // Keep the check and creation synchronous so repeated reopen events
    // cannot queue multiple main windows.
    if !cx.windows().is_empty() {
        return;
    }

    let mut window_options = TitleBar::window_options();
    window_options.window_bounds = Some(
        window_state::load(cx)
            .unwrap_or_else(|| WindowBounds::centered(size(px(1360.), px(900.)), cx)),
    );
    window_options.window_min_size = Some(size(
        px(window_state::MIN_WIDTH),
        px(window_state::MIN_HEIGHT),
    ));

    match gpui_kit::open_window(window_options, cx, |window, cx| {
        cx.new(|cx| KumuApp::new(window, cx))
    }) {
        Ok(_) => eprintln!("[ui] window opened"),
        Err(e) => eprintln!("[ui] window open FAILED: {e:?}"),
    }
}
