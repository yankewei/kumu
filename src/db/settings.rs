use std::sync::atomic::Ordering;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Result, params};

use super::schema::SCHEMA_VERSION;
use super::{SETTINGS_READONLY, Store};

impl Store {
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

/// Setting key holding the UI font family chosen in settings.
pub const SETTING_FONT: &str = "font_family";

/// Setting key holding the UI font size chosen in settings, in px.
pub const SETTING_FONT_SIZE: &str = "font_size";

/// Setting key holding the terminal font family chosen in settings.
pub const SETTING_TERM_FONT: &str = "terminal_font_family";

/// Setting key holding the terminal font size chosen in settings, in px.
pub const SETTING_TERM_FONT_SIZE: &str = "terminal_font_size";

/// Setting key holding the last light/dark choice: `"light"` or `"dark"`.
pub const SETTING_THEME_MODE: &str = "theme_mode";

/// Setting key holding the independent terminal color preset id (e.g. `"follow"`, `"dark"`, `"tokyo_night"`).
pub const SETTING_TERMINAL_THEME: &str = "terminal_theme";

/// Whether terminal attention notifications are enabled (`"0"` disables them).
pub const SETTING_NOTIFICATIONS: &str = "notifications";

/// Whether attention notifications play the macOS alert sound (`"0"` mutes).
pub const SETTING_NOTIFICATION_SOUND: &str = "notification_sound";

/// Whether agent panes resume their previous conversation at startup
/// (`"0"` starts a new one in each restored pane).
pub const SETTING_AUTO_RESUME: &str = "auto_resume";

/// Setting key marking that the first-run project was seeded. It stays
/// after the user deletes them, so an intentionally empty workspace is not
/// refilled on the next launch.
pub const SETTING_SEEDED: &str = "seeded";

/// Setting key marking that the user explicitly completed or skipped the
/// onboarding wizard (`"1"` suppresses future automatic opens).
pub const SETTING_ONBOARDING_DONE: &str = "onboarding_done";

/// Setting key holding the main window's last geometry as JSON.
pub const SETTING_WINDOW_BOUNDS: &str = "window_bounds";

/// Setting key holding the sidebar's width in px, chosen by dragging the
/// divider beside it.
pub const SETTING_SIDEBAR_WIDTH: &str = "sidebar_width";

/// Width of the right-hand plugin panel, in px.
pub const SETTING_PLUGIN_WIDTH: &str = "plugin_width";

/// Setting key holding whether the left sidebar is visible ("0" = collapsed).
pub const SETTING_SIDEBAR_VISIBLE: &str = "sidebar_visible";

/// Setting key prefix holding optional extra CLI flags per coding agent
/// (keyed as `agent_args:<agent_id>`).
pub const SETTING_AGENT_ARGS_PREFIX: &str = "agent_args:";

/// Build the setting key storing extra CLI arguments for `agent_id`.
pub fn agent_args_key(agent_id: &str) -> String {
    format!("{SETTING_AGENT_ARGS_PREFIX}{agent_id}")
}

/// Read persisted extra CLI arguments for `agent_id`, or empty when unset.
pub fn get_agent_args(agent_id: &str) -> String {
    get_setting(&agent_args_key(agent_id)).unwrap_or_default()
}

/// Persist extra CLI arguments for `agent_id`.
pub fn set_agent_args(agent_id: &str, args: &str) -> Result<(), rusqlite::Error> {
    set_setting(&agent_args_key(agent_id), args.trim())
}

/// Settings use a short-lived connection instead of the UI-owned Store: they
/// are read once at startup (before any window exists) and written rarely
/// from the settings window. A busy timeout covers collisions with Store
/// writes.
const SETTING_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Read one app setting; None when unset or when the on-disk database has not
/// been initialized yet. Opens read-only so startup reads before [`Store::open`]
/// never create an unmigrated 0-byte database file.
pub fn get_setting(key: &str) -> Option<String> {
    if SETTINGS_READONLY.load(Ordering::Acquire) {
        return None;
    }
    let path = db_path();
    if !path.is_absolute() {
        return None;
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.busy_timeout(SETTING_BUSY_TIMEOUT).ok()?;
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .ok()?;
    if version > SCHEMA_VERSION {
        return None;
    }
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .ok()
}

/// Persist one app setting. No-op when this process refused a newer-schema
/// database on startup so an older build never mutates a newer database.
pub fn set_setting(key: &str, value: &str) -> Result<()> {
    if SETTINGS_READONLY.load(Ordering::Acquire) {
        return Ok(());
    }
    let path = db_path();
    if !path.is_absolute() {
        return Ok(());
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(SETTING_BUSY_TIMEOUT)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        SETTINGS_READONLY.store(true, Ordering::Release);
        return Ok(());
    }
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Default database location: ~/.kumu/kumu.db. Returns an empty path if `HOME`
/// is unset or relative so callers never silently write into the current
/// working directory.
pub fn db_path() -> std::path::PathBuf {
    let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) else {
        return std::path::PathBuf::new();
    };
    let home = std::path::PathBuf::from(home);
    if !home.is_absolute() {
        return std::path::PathBuf::new();
    }
    home.join(".kumu").join("kumu.db")
}
