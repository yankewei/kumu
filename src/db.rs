//! Local SQLite persistence for projects, sessions, and chat messages.
//!
//! Accessed synchronously on the UI (main) thread. Schema is created on open.

use rusqlite::{Connection, Result, params};

/// A project: a workspace the user groups sessions under.
#[derive(Clone, Debug)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub path: String,
}

/// A session (conversation) belonging to a project and bound to an agent.
#[derive(Clone, Debug)]
pub struct Session {
    pub id: i64,
    pub project_id: i64,
    pub agent: String,
    pub title: String,
    pub cwd: String,
    /// Selected model (config-option value); None = agent default.
    pub model: Option<String>,
}

/// A chat message stored in a session.
#[derive(Clone, Debug)]
pub struct Message {
    pub role: String, // "user" | "agent" | "system" | "thinking" | "tool"
    pub content: String,
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
                model TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;
        // Migration for dbs created before model was part of CREATE TABLE
        // (duplicate-column errors on fresh dbs are expected and ignored).
        self.conn
            .execute("ALTER TABLE sessions ADD COLUMN model TEXT", [])
            .ok();
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
            .prepare("SELECT id, project_id, agent, title, cwd, model FROM sessions ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| {
            Ok(Session {
                id: row.get(0)?,
                project_id: row.get(1)?,
                agent: row.get(2)?,
                title: row.get(3)?,
                cwd: row.get(4)?,
                model: row.get(5)?,
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

    pub fn update_session_agent_model(
        &self,
        id: i64,
        agent: &str,
        model: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET agent = ?1, model = ?2, updated_at = datetime('now') WHERE id = ?3",
            params![agent, model, id],
        )?;
        Ok(())
    }

    pub fn update_session_title(&self, id: i64, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET title = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![title, id],
        )?;
        Ok(())
    }

    pub fn messages(&self, session_id: i64) -> Vec<Message> {
        let mut stmt = self
            .conn
            .prepare("SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY id")
            .unwrap();
        stmt.query_map(params![session_id], |row| {
            Ok(Message {
                role: row.get(0)?,
                content: row.get(1)?,
            })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    pub fn insert_message(&self, session_id: i64, role: &str, content: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO messages (session_id, role, content) VALUES (?1, ?2, ?3)",
            params![session_id, role, content],
        )?;
        self.conn.execute(
            "UPDATE sessions SET updated_at = datetime('now') WHERE id = ?1",
            params![session_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
}

/// Setting key holding the UI font family chosen in settings.
pub const SETTING_FONT: &str = "font_family";

/// Settings use a short-lived connection instead of the UI-owned Store: they
/// are read once at startup (before any window exists) and written rarely
/// from the settings window. A busy timeout covers collisions with Store
/// writes.
const SETTING_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Read one app setting; None when unset.
pub fn get_setting(key: &str) -> Option<String> {
    let conn = Connection::open(&db_path()).ok()?;
    conn.busy_timeout(SETTING_BUSY_TIMEOUT).ok()?;
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .ok()
}

/// Persist one app setting.
pub fn set_setting(key: &str, value: &str) -> Result<()> {
    let conn = Connection::open(&db_path())?;
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
