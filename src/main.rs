//! kumu — local multi-agent desktop shell.
//!
//! Architecture & design rules:
//!   - all business UI (sidebar, tabs, splitable workspace shell, pane headers,
//!     find bar, dialogs, settings, toast stack, tooltips, plugin panels & diff
//!     sheet) is built with Pure Rust NSH (`nsh-core` + `nsh-base`) hosted on
//!     `nsh-platform-gpui`;
//!   - bare `gpui` (`gpui-fast`) is reserved exclusively for the macOS window &
//!     transparent titlebar shell, native `SystemNotification`, and the
//!     GPU-accelerated Ghostty VT `TerminalView` mounted as an NSH `NativeView`;
//!   - colors/surfaces come from `KumuTheme` (`themes/*.json`) by semantic role
//!     with zero `gpui-kit` dependency;
//!   - typography stays on the two-step scale (`text_sm` 14px / `text_xs` 12px);
//!   - primary is reserved for the decision commit, internal nav = ghost/default.
//!
//! Module layout:
//!   - `nsh_ui/`  Pure Rust NSH foundation (theme, components, assets, dialogs, settings, toast);
//!   - `ui/`      GPUI window shell, macOS titlebar, GPU Ghostty VT `TerminalView`, and unified `NshHomeShell`;
//!   - `theme.rs` standalone `themes/*.json` loader driving both NSH and Ghostty `TermColors`;
//!   - `app/`     framework-agnostic `WorkspaceCore` + `layout` tree and thin GPUI adapter;
//!   - `plugins/` Pure Rust NSH right-hand plugin panels (Git Log & Diff Review sheet);
//!   - `git.rs`   worktree creation and commit history;
//!   - `db.rs`    local SQLite persistence (projects and the workspace layout);
//!   - `term/`    PTY + VT for shells and the agent TUIs they launch;
//!   - `agent/`   registry and executable lookup;
//!   - `logging.rs` stderr redirection to `~/Library/Logs/Kumu` for `.app` runs;
//!   - `window_state.rs` the main window's saved geometry.

mod agent;
mod app;
mod artifacts;
mod assets;
mod control;
mod db;
mod dock_icon;
mod fonts;
mod git;
mod logging;
mod nsh_ui;
mod plugins;
mod term;
mod theme;
mod ui;
mod updates;
mod window_state;

use gpui::*;
use ui::KumuApp;

fn main() {
    if let Some(code) = agent::integration::run_cli().or_else(control::run_cli) {
        std::process::exit(code);
    }
    // Enrich this process's PATH once when launched from Finder/Dock (launchd)
    // before any threads start, so child processes like `git` (and its LFS
    // filters/hooks) find Homebrew and user-local binaries.
    term::shell_env::fill_process_path();
    // A Finder-launched app has no readable stderr. Send diagnostics to
    // ~/Library/Logs/Kumu/kumu.log so users can attach them to a report.
    logging::init();
    // Keep registered native extensions (OpenCode, Pi, OMP) and Claude hooks
    // aligned with this build before any agent pane starts.
    agent::integration::refresh_installed();
    // Capture the login shell's environment before any pane starts: an agent
    // pane runs its binary directly and would otherwise miss what `~/.zshrc`
    // exports (PATH, API keys) when Kumu was opened from Finder.
    term::shell_env::prewarm();
    let app = gpui_platform::application().with_assets(assets::KumuAssetSource);

    // Closing the last window on macOS leaves the app running. A Dock click
    // must create a window again instead of only activating the empty app.
    app.on_reopen(open_main_window);

    app.run(move |cx| {
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

    let mut window_options = ui::titlebar::window_options();
    window_options.window_bounds = Some(
        window_state::load(cx)
            .unwrap_or_else(|| WindowBounds::centered(None, size(px(1360.), px(900.)), cx)),
    );
    window_options.window_min_size = Some(size(
        px(window_state::MIN_WIDTH),
        px(window_state::MIN_HEIGHT),
    ));

    match cx.open_window(window_options, |window, cx| {
        cx.new(|cx| KumuApp::new(window, cx))
    }) {
        Ok(handle) => {
            cx.set_global(MainWindow(Some(handle.into())));
            eprintln!("[ui] window opened");
        }
        Err(e) => eprintln!("[ui] window open FAILED: {e:?}"),
    }
}
