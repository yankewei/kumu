//! Local SQLite persistence for projects, sessions, and the home window's last
//! layout.
//!
//! Accessed synchronously on the UI (main) thread. Schema is created and
//! upgraded on open through `PRAGMA user_version`; tables and columns left over
//! from the removed in-app chat path are dropped there too, so an older
//! database keeps working with the current one.

use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Result, params};

/// A project: a workspace the user groups sessions under.
#[derive(Clone, Debug)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub path: String,
}

/// A session belonging to a project and bound to an agent (or a shell).
#[derive(Clone, Debug)]
pub struct Session {
    pub id: i64,
    pub project_id: i64,
    pub agent: String,
    pub title: String,
    pub cwd: String,
    /// Agent-side session id, when we have latched onto one (`agent:id`).
    /// Currently used to look the title up; it is not a resume cursor yet.
    pub agent_session_id: Option<String>,
    /// The user typed this title; generated names must not replace it.
    pub title_manual: bool,
}

pub struct Store {
    conn: Connection,
}

/// How long a write waits for another connection's write to finish.
const STORE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Schema version stored in `PRAGMA user_version`. Bump it with a new
/// `migrate_vN` step; opening an older database applies the steps it is
/// missing exactly once.
const SCHEMA_VERSION: i64 = 3;

impl Store {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path)?;
        prepare(&conn)?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    /// An empty store that lives only for this process. Used when the on-disk
    /// database cannot be opened, so the window still works for the session
    /// even though nothing is saved.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        prepare(&conn)?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    fn init(&self) -> Result<()> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 1 {
            self.migrate_v1()?;
        }
        if version < 2 {
            self.migrate_v2()?;
        }
        if version < 3 {
            self.migrate_v3()?;
        }
        if version < SCHEMA_VERSION {
            self.conn
                .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(())
    }

    /// The project/session/settings schema, plus the cleanup of the removed
    /// in-app chat path. Existing databases have `user_version = 0` and already
    /// contain most of this, so every step here is idempotent.
    fn migrate_v1(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS projects (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                path TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                agent TEXT NOT NULL,
                title TEXT NOT NULL,
                cwd TEXT NOT NULL,
                resume_id TEXT,
                title_manual INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            -- The removed chat path's history. Its index goes with the table.
            DROP TABLE IF EXISTS messages;",
        )?;
        // Columns from that chat path. `DROP COLUMN` errors when the column is
        // already missing, which is the expected case on fresh and current dbs.
        for column in ["model", "mode", "sandbox", "reasoning", "usage"] {
            self.conn
                .execute(&format!("ALTER TABLE sessions DROP COLUMN {column}"), [])
                .ok();
        }
        // Databases created before `title_manual` and `resume_id` joined the
        // schema gain them here.
        self.conn
            .execute(
                "ALTER TABLE sessions ADD COLUMN title_manual INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .ok();
        let has_resume_id: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'resume_id')",
            [],
            |row| row.get(0),
        )?;
        if !has_resume_id {
            self.conn
                .execute("ALTER TABLE sessions ADD COLUMN resume_id TEXT", [])?;
        }
        Ok(())
    }

    /// The home window's last layout: pane tree, pane → session mapping,
    /// focused pane, and collapsed projects, as one JSON document.
    fn migrate_v2(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS workspace_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                layout_json TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )?;
        Ok(())
    }

    /// Session recency, used to order the sidebar, plus the rename of the old
    /// `resume_id` column to `agent_session_id`. Existing rows start their
    /// recency clock at their creation time, so the first upgrade does not
    /// shuffle them into an arbitrary order.
    fn migrate_v3(&self) -> Result<()> {
        // The field only ever held the agent-side session id used for title
        // lookup; `agent_session_id` says that instead of promising a resume.
        let has_old: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'resume_id')",
            [],
            |row| row.get(0),
        )?;
        let has_new: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'agent_session_id')",
            [],
            |row| row.get(0),
        )?;
        if has_old && !has_new {
            self.conn.execute(
                "ALTER TABLE sessions RENAME COLUMN resume_id TO agent_session_id",
                [],
            )?;
        } else if !has_new {
            self.conn
                .execute("ALTER TABLE sessions ADD COLUMN agent_session_id TEXT", [])?;
        }
        self.conn
            .execute("ALTER TABLE sessions ADD COLUMN last_used_at TEXT", [])
            .ok();
        self.conn.execute(
            "UPDATE sessions
             SET last_used_at = COALESCE(last_used_at, created_at, datetime('now'))",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS sessions_last_used
             ON sessions(project_id, last_used_at DESC)",
            [],
        )?;
        Ok(())
    }

    pub fn projects(&self) -> Vec<Project> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, path FROM projects ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| {
            Ok(Project {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
            })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    pub fn insert_project(&self, name: &str, path: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO projects (name, path) VALUES (?1, ?2)",
            params![name, path],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Rename a project without moving it or touching its sessions.
    pub fn rename_project(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE projects SET name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        Ok(())
    }

    /// Delete a project. Its sessions go with it through the schema's
    /// `ON DELETE CASCADE`; callers must stop their terminals first.
    pub fn delete_project(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM projects WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Sessions in sidebar order: most recently used first within a project,
    /// newest first as the tie-break. Title updates do not touch this clock.
    pub fn sessions(&self) -> Vec<Session> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project_id, agent, title, cwd, agent_session_id, title_manual
                 FROM sessions
                 ORDER BY project_id, last_used_at DESC, id DESC",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok(Session {
                id: row.get(0)?,
                project_id: row.get(1)?,
                agent: row.get(2)?,
                title: row.get(3)?,
                cwd: row.get(4)?,
                agent_session_id: row.get(5)?,
                title_manual: row.get::<_, i64>(6)? != 0,
            })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    pub fn insert_session(
        &self,
        project_id: i64,
        agent: &str,
        title: &str,
        cwd: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sessions (project_id, agent, title, cwd, last_used_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))",
            params![project_id, agent, title, cwd],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Mark a session as used now, without touching its title bookkeeping.
    ///
    /// This is the sidebar's recency clock; it deliberately runs on selection
    /// rather than on every write, so generated titles cannot reorder the tree.
    pub fn touch_session(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_used_at = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Delete a session.
    pub fn delete_session(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Store a generated title — typically one read from an agent's own session
    /// store — and report whether it was applied.
    ///
    /// A session the user has renamed refuses it: [`Store::rename_session`]
    /// makes the title the user's, and a generated one must not overwrite a
    /// name they typed out.
    pub fn update_session_title(&self, id: i64, title: &str) -> Result<bool> {
        let applied = self.conn.execute(
            "UPDATE sessions SET title = ?1, updated_at = datetime('now')
             WHERE id = ?2 AND title_manual = 0",
            params![title, id],
        )?;
        Ok(applied > 0)
    }

    /// Remember which agent-side session this row is bound to (`agent:id`).
    pub fn set_session_agent_session_id(&self, id: i64, agent_session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET agent_session_id = ?1, updated_at = datetime('now')
             WHERE id = ?2",
            params![agent_session_id, id],
        )?;
        Ok(())
    }

    /// Drop the agent-session binding (the agent process is gone or was never
    /// ours).
    pub fn clear_session_agent_session_id(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET agent_session_id = NULL, updated_at = datetime('now')
             WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Rename a session to a name the user chose. From here on the name is the
    /// session's own: generated titles no longer replace it.
    pub fn rename_session(&self, id: i64, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET title = ?1, title_manual = 1, updated_at = datetime('now')
             WHERE id = ?2",
            params![title, id],
        )?;
        Ok(())
    }

    /// The layout JSON the home window saved last, when there is one.
    pub fn workspace_state(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT layout_json FROM workspace_state WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()
    }

    /// Replace the saved layout. One row; the home window owns the JSON shape.
    pub fn save_workspace_state(&self, json: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO workspace_state (id, layout_json, updated_at)
             VALUES (1, ?1, datetime('now'))
             ON CONFLICT(id) DO UPDATE SET
                 layout_json = excluded.layout_json,
                 updated_at = datetime('now')",
            params![json],
        )?;
        Ok(())
    }
}

/// Connection settings shared by the store and its in-memory fallback: WAL lets
/// the settings window's short-lived connections share the file with the
/// UI-owned store, and the busy timeout covers the writes that still collide.
fn prepare(conn: &Connection) -> Result<()> {
    conn.busy_timeout(STORE_BUSY_TIMEOUT)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

/// Setting key holding the UI font family chosen in settings.
pub const SETTING_FONT: &str = "font_family";

/// Setting key holding the UI font size chosen in settings, in px.
pub const SETTING_FONT_SIZE: &str = "font_size";

/// Setting key holding the last light/dark choice: `"light"` or `"dark"`.
pub const SETTING_THEME_MODE: &str = "theme_mode";

/// Whether terminal attention notifications are enabled (`"0"` disables them).
pub const SETTING_NOTIFICATIONS: &str = "notifications";

/// Whether attention notifications play the macOS alert sound (`"0"` mutes).
pub const SETTING_NOTIFICATION_SOUND: &str = "notification_sound";

/// Setting key marking that the first-run project/session were seeded. It stays
/// after the user deletes them, so an intentionally empty workspace is not
/// refilled on the next launch.
pub const SETTING_SEEDED: &str = "seeded";

/// Setting key holding the main window's last geometry as JSON.
pub const SETTING_WINDOW_BOUNDS: &str = "window_bounds";

/// Settings use a short-lived connection instead of the UI-owned Store: they
/// are read once at startup (before any window exists) and written rarely
/// from the settings window. A busy timeout covers collisions with Store
/// writes.
const SETTING_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Read one app setting; None when unset.
pub fn get_setting(key: &str) -> Option<String> {
    let conn = Connection::open(db_path()).ok()?;
    conn.busy_timeout(SETTING_BUSY_TIMEOUT).ok()?;
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .ok()
}

/// Persist one app setting.
pub fn set_setting(key: &str, value: &str) -> Result<()> {
    let conn = Connection::open(db_path())?;
    conn.busy_timeout(SETTING_BUSY_TIMEOUT)?;
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Default database location: ~/.kumu/kumu.db.
pub fn db_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join(".kumu").join("kumu.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::in_memory().unwrap()
    }

    /// The on-disk store must be the one connection the whole process writes
    /// through, shared with Settings' short-lived connections: WAL and the busy
    /// timeout are what keep those writes from failing under each other.
    #[test]
    fn an_on_disk_store_uses_wal_and_enforces_foreign_keys() {
        let dir = std::env::temp_dir().join(format!(
            "kumu-db-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("kumu.db")).unwrap();

        let mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let foreign_keys: i64 = store
            .conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);

        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Deleting one session leaves its siblings alone.
    #[test]
    fn deleting_a_session_keeps_the_others() {
        let store = store();
        let project = store.insert_project("kumu", "/tmp").unwrap();
        let doomed = store
            .insert_session(project, "omp", "会话 1", "/tmp")
            .unwrap();
        let kept = store
            .insert_session(project, "omp", "会话 2", "/tmp")
            .unwrap();

        store.delete_session(doomed).unwrap();

        let sessions = store.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, kept);
    }

    /// Deleting an unknown id is a no-op rather than an error, so a stale
    /// menu action can never break the db.
    #[test]
    fn deleting_an_unknown_session_is_harmless() {
        let store = store();
        store.delete_session(404).unwrap();
        assert!(store.sessions().is_empty());
    }

    #[test]
    fn renaming_a_session_updates_its_title() {
        let store = store();
        let project = store.insert_project("kumu", "/tmp").unwrap();
        let session = store
            .insert_session(project, "omp", "会话 1", "/tmp")
            .unwrap();

        store.rename_session(session, "重构 backend").unwrap();
        assert_eq!(store.sessions()[0].title, "重构 backend");
        assert!(store.sessions()[0].title_manual);
    }

    /// A generated title fills an unnamed session, but once the user renames
    /// it every later generated title is refused — the name they typed stands.
    #[test]
    fn a_manual_title_outranks_generated_ones() {
        let store = store();
        let project = store.insert_project("kumu", "/tmp").unwrap();
        let session = store
            .insert_session(project, "omp", "会话 1", "/tmp")
            .unwrap();

        assert!(store.update_session_title(session, "首条消息").unwrap());
        assert_eq!(store.sessions()[0].title, "首条消息");

        store.rename_session(session, "重构 backend").unwrap();
        assert!(
            !store
                .update_session_title(session, "agent 起的名字")
                .unwrap()
        );
        assert_eq!(store.sessions()[0].title, "重构 backend");
    }

    /// Renaming a project keeps its sessions; deleting it takes them with it
    /// through the schema's cascade, which `foreign_keys=ON` makes real.
    #[test]
    fn project_rename_and_delete_keep_the_right_sessions() {
        let store = store();
        let kept_project = store.insert_project("keep", "/keep").unwrap();
        let doomed_project = store.insert_project("doomed", "/doomed").unwrap();
        let kept = store
            .insert_session(kept_project, "shell", "终端 1", "/keep")
            .unwrap();
        store
            .insert_session(doomed_project, "shell", "终端 2", "/doomed")
            .unwrap();

        store.rename_project(doomed_project, "renamed").unwrap();
        assert!(
            store
                .projects()
                .iter()
                .any(|p| p.id == doomed_project && p.name == "renamed")
        );

        store.delete_project(doomed_project).unwrap();
        assert_eq!(store.projects().len(), 1);
        let sessions = store.sessions();
        assert_eq!(sessions.len(), 1, "the other project's session survives");
        assert_eq!(sessions[0].id, kept);
    }

    /// The sidebar's order comes from `last_used_at`, and generated-title
    /// bookkeeping must not move a session.
    #[test]
    fn sessions_order_by_recent_use_not_title_updates() {
        let store = store();
        let project = store.insert_project("kumu", "/tmp").unwrap();
        let (a, b, c) = (
            store.insert_session(project, "shell", "a", "/tmp").unwrap(),
            store.insert_session(project, "shell", "b", "/tmp").unwrap(),
            store.insert_session(project, "shell", "c", "/tmp").unwrap(),
        );
        for (id, at) in [
            (a, "2024-01-01 00:00:00"),
            (b, "2024-01-02 00:00:00"),
            (c, "2024-01-03 00:00:00"),
        ] {
            store
                .conn
                .execute(
                    "UPDATE sessions SET last_used_at = ?1 WHERE id = ?2",
                    rusqlite::params![at, id],
                )
                .unwrap();
        }
        let ids = || {
            store
                .sessions()
                .into_iter()
                .map(|session| session.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(), vec![c, b, a]);

        store.touch_session(a).unwrap();
        assert_eq!(ids(), vec![a, c, b], "using a session moves it up");

        store.update_session_title(a, "agent title").unwrap();
        assert_eq!(
            ids(),
            vec![a, c, b],
            "a generated title must not reorder the sidebar"
        );
    }

    /// The layout is one replaceable row; reading it back is what lets the
    /// home window restore its panes on the next launch.
    #[test]
    fn a_workspace_state_round_trips_and_replaces_the_previous_one() {
        let store = store();
        assert_eq!(store.workspace_state().unwrap(), None);

        store.save_workspace_state(r#"{"version":1}"#).unwrap();
        assert_eq!(
            store.workspace_state().unwrap().as_deref(),
            Some(r#"{"version":1}"#)
        );

        store
            .save_workspace_state(r#"{"version":1,"focused":7}"#)
            .unwrap();
        assert_eq!(
            store.workspace_state().unwrap().as_deref(),
            Some(r#"{"version":1,"focused":7}"#),
            "saving replaces rather than appends"
        );
        let rows: i64 = store
            .conn
            .query_row("SELECT count(*) FROM workspace_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    /// A database from the chat era keeps its rows but loses that era's table
    /// and columns the next time it is opened.
    #[test]
    fn an_older_database_drops_the_chat_schema_and_keeps_its_sessions() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                path TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                agent TEXT NOT NULL,
                title TEXT NOT NULL,
                cwd TEXT NOT NULL,
                resume_id TEXT,
                model TEXT,
                mode TEXT,
                sandbox TEXT,
                reasoning TEXT,
                usage TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                duration_ms INTEGER,
                attachments TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            INSERT INTO projects (name, path) VALUES ('kumu', '/tmp');
            INSERT INTO sessions (project_id, agent, title, cwd, model)
                VALUES (1, 'shell', '终端 1', '/tmp', 'old-model');
            INSERT INTO messages (session_id, role, content) VALUES (1, 'user', 'hi');",
        )
        .unwrap();
        let store = Store { conn };
        store.init().unwrap();

        let sessions = store.sessions();
        assert_eq!(sessions.len(), 1, "the session row survives");
        assert_eq!(sessions[0].title, "终端 1");
        let chat_table: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'messages'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(chat_table, 0, "the chat table is gone");
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION, "the upgrade is recorded once");
        let workspace_table: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'workspace_state'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(workspace_table, 1, "the workspace table is added");
        let last_used: Option<String> = store
            .conn
            .query_row(
                "SELECT last_used_at FROM sessions WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            last_used.is_some(),
            "the recency clock starts at the existing row's creation time"
        );
        let renamed: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('sessions')
                 WHERE name = 'agent_session_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(renamed, 1, "resume_id is renamed to agent_session_id");
        for column in ["model", "mode", "sandbox", "reasoning", "usage"] {
            let left: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM pragma_table_info('sessions') WHERE name = ?1",
                    [column],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(left, 0, "{column} is gone");
        }

        // Opening it again must be a no-op rather than re-running the upgrade.
        store.init().unwrap();
    }
}
