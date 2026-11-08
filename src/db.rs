//! Local SQLite persistence for projects and sessions.
//!
//! Accessed synchronously on the UI (main) thread. Schema is created on open;
//! tables and columns left over from the removed in-app chat path are dropped
//! there too, so an older database keeps working with the current one.

use rusqlite::{Connection, Result, params};

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
    pub resume_id: Option<String>,
    /// The user typed this title; generated names must not replace it.
    pub title_manual: bool,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    fn init(&self) -> Result<()> {
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

    pub fn sessions(&self) -> Vec<Session> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project_id, agent, title, cwd, resume_id, title_manual
                 FROM sessions ORDER BY id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok(Session {
                id: row.get(0)?,
                project_id: row.get(1)?,
                agent: row.get(2)?,
                title: row.get(3)?,
                cwd: row.get(4)?,
                resume_id: row.get(5)?,
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
            "INSERT INTO sessions (project_id, agent, title, cwd) VALUES (?1, ?2, ?3, ?4)",
            params![project_id, agent, title, cwd],
        )?;
        Ok(self.conn.last_insert_rowid())
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
    pub fn set_session_resume_id(&self, id: i64, resume_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET resume_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![resume_id, id],
        )?;
        Ok(())
    }

    /// Drop a resume binding (the agent process is gone or was never ours).
    pub fn clear_session_resume_id(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET resume_id = NULL, updated_at = datetime('now') WHERE id = ?1",
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
}

/// Setting key holding the UI font family chosen in settings.
pub const SETTING_FONT: &str = "font_family";

/// Setting key holding the UI font size chosen in settings, in px.
pub const SETTING_FONT_SIZE: &str = "font_size";

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
        let store = Store {
            conn: Connection::open_in_memory().unwrap(),
        };
        store.init().unwrap();
        store
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
        assert!(!store
            .update_session_title(session, "agent 起的名字")
            .unwrap());
        assert_eq!(store.sessions()[0].title, "重构 backend");
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
    }
}
