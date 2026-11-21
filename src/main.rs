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
//!   - `nsh_ui/`  Pure Rust NSH foundation (theme, components, assets, dialogs, settings);
//!   - `ui/`      GPUI window shell, GPU Ghostty VT `TerminalView`, split grid, and NSH hosts for sidebar & tab bar;
//!   - `theme.rs` themes/*.json loading into the ThemeRegistry (shared with NSH);
//!   - `app/`     framework-agnostic `WorkspaceCore` + `layout` tree and thin GPUI adapter;
//!   - `plugins/` Pure Rust NSH right-hand plugin panels (TODO board, Git Log & Diff Review);
//!   - `git.rs`   worktree creation and commit history;
//!   - `db.rs`    local SQLite persistence (projects and the workspace layout);
//!   - `term/`    PTY + VT for shells and the agent TUIs they launch;
//!   - `agent/`   registry and executable lookup;
//!   - `window_state.rs` the main window's saved geometry.

mod agent;
mod app;
mod assets;
mod control;
mod db;
mod dock_icon;
mod fonts;
mod git;
mod nsh_ui;
mod plugins;
mod term;
mod theme;
mod ui;
mod updates;
mod window_state;

use gpui_kit::component::*;
use gpui_kit::*;
use ui::KumuApp;

fn main() {
    if let Some(code) = agent::integration::run_cli().or_else(control::run_cli) {
        std::process::exit(code);
    }
    // Keep registered native extensions (OpenCode, Pi, OMP) and Claude hooks
    // aligned with this build before any agent pane starts.
    agent::integration::refresh_installed();
    // Capture the login shell's environment before any pane starts: an agent
    // pane runs its binary directly and would otherwise miss what `~/.zshrc`
    // exports (PATH, API keys) when Kumu was opened from Finder.
    term::shell_env::prewarm();
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

struct MainWindow(Option<AnyWindowHandle>);
impl Global for MainWindow {}

fn open_main_window(cx: &mut App) {
    // Keep the check and creation synchronous so repeated reopen events
    // cannot queue multiple main windows. Checking the tracked main window
    // handle lets the Dock reopen Kumu even while Settings is still open.
    if let Some(handle) = cx.try_global::<MainWindow>().and_then(|m| m.0)
        && cx.windows().contains(&handle)
    {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
        return;
    }

    let mut window_options = TitleBar::window_options();
    window_options.window_bounds = Some(
        window_state::load(cx)
            .unwrap_or_else(|| WindowBounds::centered(None, size(px(1360.), px(900.)), cx)),
    );
    window_options.window_min_size = Some(size(
        px(window_state::MIN_WIDTH),
        px(window_state::MIN_HEIGHT),
    ));

    match gpui_kit::open_window(window_options, cx, |window, cx| {
        cx.new(|cx| KumuApp::new(window, cx))
    }) {
        Ok((handle, _)) => {
            cx.set_global(MainWindow(Some(handle)));
            eprintln!("[ui] window opened");
        }
        Err(e) => eprintln!("[ui] window open FAILED: {e:?}"),
    }
}
