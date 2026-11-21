//! Local SQLite persistence for projects and the home window's last layout.
//!
//! Accessed synchronously on the UI (main) thread. Schema is created and
//! upgraded on open through `PRAGMA user_version`; tables and columns left over
//! from the removed in-app chat path are dropped there too, so an older
//! database keeps working with the current one. A database written by a newer
//! build is refused ([`OpenError::SchemaTooNew`]) rather than opened against
//! queries that no longer match it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Result, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Set when [`Store::open`] refuses a newer-schema database so standalone
/// `set_setting` calls never write into a newer build's database.
static SETTINGS_READONLY: AtomicBool = AtomicBool::new(false);

/// A project: a directory with its own terminal panes. A worktree is a
/// project whose `parent_id` points at the repository it was created from.
#[derive(Clone, Debug)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub parent_id: Option<i64>,
}

/// The three columns of a project's TODO board.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Todo,
    InProgress,
    Done,
}

impl TodoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "todo" | "待办" => Some(Self::Todo),
            "in_progress" | "in-progress" | "inprogress" | "进行中" => Some(Self::InProgress),
            "done" | "completed" | "已完成" => Some(Self::Done),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Todo => "待办",
            Self::InProgress => "进行中",
            Self::Done => "已完成",
        }
    }
}

/// One task on a repository's TODO board.
///
/// `project_id` always points at the root repository so worktrees share the
/// same board. `pane_id` links the task to a live Agent pane; `agent_id` stays
/// as history even after that pane closes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    pub id: i64,
    pub project_id: i64,
    pub title: String,
    pub description: String,
    pub status: TodoStatus,
    pub position: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    pub awaiting_review: bool,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

/// Partial update for [`Store::update_todo`]. `None` leaves a field untouched;
/// double-Option fields distinguish "leave untouched" from "clear to NULL".
#[derive(Clone, Debug, Default)]
pub struct TodoPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<TodoStatus>,
    pub pane_id: Option<Option<u64>>,
    pub agent_id: Option<Option<String>>,
    pub agent_session_id: Option<Option<String>>,
    pub awaiting_review: Option<bool>,
}

pub struct Store {
    conn: Connection,
}

/// Why a database file could not be opened for this build.
#[derive(Debug)]
pub enum OpenError {
    Db(rusqlite::Error),
    /// The file was last written by a newer build whose schema this one does
    /// not know. Reading it as if it were current would fail on missing tables
    /// and columns, so the app keeps running from memory instead.
    SchemaTooNew {
        found: i64,
        supported: i64,
    },
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(error) => write!(f, "{error}"),
            Self::SchemaTooNew { found, supported } => write!(
                f,
                "数据库版本为 {found}，高于当前 Kumu 支持的版本 {supported}，请升级 Kumu"
            ),
        }
    }
}

impl std::error::Error for OpenError {}

impl From<rusqlite::Error> for OpenError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(error)
    }
}

/// How long a write waits for another connection's write to finish.
const STORE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Schema version stored in `PRAGMA user_version`. Bump it with a new
/// `migrate_vN` step; opening an older database applies the steps it is
/// missing exactly once.
const SCHEMA_VERSION: i64 = 6;

impl Store {
    pub fn open(path: &std::path::Path) -> Result<Self, OpenError> {
        if path.as_os_str().is_empty() {
            return Err(OpenError::Db(rusqlite::Error::InvalidPath(
                path.to_path_buf(),
            )));
        }
        if let Some(dir) = path.parent() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
                let _ = std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir);
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
            #[cfg(not(unix))]
            {
                std::fs::create_dir_all(dir).ok();
            }
        }
        let conn = Connection::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        prepare(&conn)?;
        let mut store = Self { conn };
        if let Err(err) = store.init() {
            if matches!(err, OpenError::SchemaTooNew { .. }) {
                SETTINGS_READONLY.store(true, Ordering::Release);
            }
            return Err(err);
        }
        Ok(store)
    }

    /// An empty store that lives only for this process. Used when the on-disk
    /// database cannot be opened, so the window still works for this run
    /// even though nothing is saved.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        prepare(&conn)?;
        let mut store = Self { conn };
        store.init().map_err(|err| match err {
            OpenError::Db(e) => e,
            OpenError::SchemaTooNew { .. } => rusqlite::Error::InvalidQuery,
        })?;
        Ok(store)
    }

    fn init(&mut self) -> Result<(), OpenError> {
        // Check `user_version` and migrate inside a single IMMEDIATE transaction
        // so concurrent launches serialize cleanly via `busy_timeout` instead
        // of hitting SQLITE_BUSY on a deferred write-lock upgrade.
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(OpenError::SchemaTooNew {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version < 1 {
            Self::migrate_v1_conn(&transaction)?;
        }
        if version < 2 {
            Self::migrate_v2_conn(&transaction)?;
        }
        if version < 3 {
            Self::migrate_v3_conn(&transaction)?;
        }
        if version < 4 {
            Self::migrate_v4_conn(&transaction)?;
        }
        if version < 5 {
            Self::migrate_v5_conn(&transaction)?;
        }
        if version < 6 {
            Self::migrate_v6_conn(&transaction)?;
        }
        if version < SCHEMA_VERSION {
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn migrate_v1_conn(conn: &Connection) -> Result<()> {
        conn.execute_batch(
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
            conn.execute(&format!("ALTER TABLE sessions DROP COLUMN {column}"), [])
                .ok();
        }
        // Databases created before `title_manual` and `resume_id` joined the
        // schema gain them here.
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN title_manual INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .ok();
        let has_resume_id: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'resume_id')",
            [],
            |row| row.get(0),
        )?;
        if !has_resume_id {
            conn.execute("ALTER TABLE sessions ADD COLUMN resume_id TEXT", [])?;
        }
        Ok(())
    }

    /// The home window's last layout: pane tree, pane → session mapping,
    /// focused pane, and collapsed projects, as one JSON document.
    fn migrate_v2_conn(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS workspace_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                layout_json TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )?;
        Ok(())
    }

    /// Session recency, used to order the sidebar, plus the rename of the old
    /// `resume_id` column to `agent_session_id`.
    fn migrate_v3_conn(conn: &Connection) -> Result<()> {
        let has_old: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'resume_id')",
            [],
            |row| row.get(0),
        )?;
        let has_new: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'agent_session_id')",
            [],
            |row| row.get(0),
        )?;
        if has_old && !has_new {
            conn.execute(
                "ALTER TABLE sessions RENAME COLUMN resume_id TO agent_session_id",
                [],
            )?;
        } else if !has_new {
            conn.execute("ALTER TABLE sessions ADD COLUMN agent_session_id TEXT", [])?;
        }
        conn.execute("ALTER TABLE sessions ADD COLUMN last_used_at TEXT", [])
            .ok();
        conn.execute(
            "UPDATE sessions
             SET last_used_at = COALESCE(last_used_at, created_at, datetime('now'))",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS sessions_last_used
             ON sessions(project_id, last_used_at DESC)",
            [],
        )?;
        Ok(())
    }

    /// Projects own panes directly. Older databases stored a session row per
    /// terminal; their layout is rewritten onto the owning project and the
    /// table is dropped.
    fn migrate_v4_conn(conn: &Connection) -> Result<()> {
        let has_sessions: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sessions')",
            [],
            |row| row.get(0),
        )?;
        if has_sessions {
            let mut session_project = HashMap::new();
            {
                let mut stmt = conn.prepare("SELECT id, project_id FROM sessions")?;
                let rows =
                    stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
                for row in rows {
                    let (session, project) = row?;
                    session_project.insert(session, project);
                }
            }
            let existing: Option<String> = conn
                .query_row(
                    "SELECT layout_json FROM workspace_state WHERE id = 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(json) = existing
                && let Some(next) = rekey_workspace_json(&json, &session_project)
            {
                conn.execute(
                    "INSERT INTO workspace_state (id, layout_json, updated_at)
                     VALUES (1, ?1, datetime('now'))
                     ON CONFLICT(id) DO UPDATE SET
                        layout_json = excluded.layout_json,
                        updated_at = datetime('now')",
                    params![next],
                )?;
            }
            conn.execute_batch(
                "DROP INDEX IF EXISTS sessions_last_used;
                 DROP TABLE IF EXISTS sessions;",
            )?;
        }
        Ok(())
    }

    /// A worktree is a project that points at the repository it came from.
    fn migrate_v5_conn(conn: &Connection) -> Result<()> {
        let has_parent: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('projects') WHERE name = 'parent_id')",
            [],
            |row| row.get(0),
        )?;
        if !has_parent {
            conn.execute_batch(
                "ALTER TABLE projects
                     ADD COLUMN parent_id INTEGER
                     REFERENCES projects(id) ON DELETE CASCADE;
                 CREATE INDEX IF NOT EXISTS projects_parent
                     ON projects(parent_id);",
            )?;
        }
        Ok(())
    }

    /// Per-repository TODO board items, optionally linked to a live agent pane.
    fn migrate_v6_conn(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS todos (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                title TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'todo'
                    CHECK (status IN ('todo', 'in_progress', 'done')),
                position INTEGER NOT NULL DEFAULT 0,
                pane_id INTEGER,
                agent_id TEXT,
                agent_session_id TEXT,
                awaiting_review INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                completed_at TEXT
            );
            CREATE INDEX IF NOT EXISTS todos_project_status_pos
                ON todos(project_id, status, position, id);
            CREATE INDEX IF NOT EXISTS todos_pane
                ON todos(pane_id) WHERE pane_id IS NOT NULL;",
        )?;
        Ok(())
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, path, parent_id FROM projects ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok(Project {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
                parent_id: row.get(3)?,
            })
        })?;
        rows.collect()
    }

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

    pub fn insert_project(&self, name: &str, path: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO projects (name, path) VALUES (?1, ?2)",
            params![name, path],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Add a worktree row under the repository it belongs to.
    pub fn insert_worktree(&self, parent_id: i64, name: &str, path: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO projects (name, path, parent_id) VALUES (?1, ?2, ?3)",
            params![name, path, parent_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Rename a project without moving it.
    pub fn rename_project(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE projects SET name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        Ok(())
    }

    /// Delete a project and its worktree rows. Callers stop their terminals
    /// first; the worktree directories stay on disk.
    pub fn delete_project(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "WITH RECURSIVE tree(id) AS (
                 SELECT id FROM projects WHERE id = ?1
                 UNION
                 SELECT projects.id FROM projects JOIN tree ON projects.parent_id = tree.id
             )
             DELETE FROM projects WHERE id IN (SELECT id FROM tree)",
            params![id],
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

    fn todo_from_row(row: &rusqlite::Row<'_>) -> Result<TodoItem> {
        let status_raw: String = row.get(4)?;
        let pane_raw: Option<i64> = row.get(6)?;
        let awaiting_raw: i64 = row.get(9)?;
        Ok(TodoItem {
            id: row.get(0)?,
            project_id: row.get(1)?,
            title: row.get(2)?,
            description: row.get(3)?,
            status: TodoStatus::parse(&status_raw).unwrap_or(TodoStatus::Todo),
            position: row.get(5)?,
            pane_id: pane_raw.and_then(|value| u64::try_from(value).ok()),
            agent_id: row.get(7)?,
            agent_session_id: row.get(8)?,
            awaiting_review: awaiting_raw != 0,
            created_at: row.get(10)?,
            updated_at: row.get(11)?,
            completed_at: row.get(12)?,
        })
    }

    /// All TODO items belonging to one root repository board.
    pub fn todos(&self, project_id: i64) -> Result<Vec<TodoItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, title, description, status, position,
                    pane_id, agent_id, agent_session_id, awaiting_review,
                    created_at, updated_at, completed_at
             FROM todos
             WHERE project_id = ?1
             ORDER BY position ASC, id DESC",
        )?;
        let rows = stmt.query_map(params![project_id], Self::todo_from_row)?;
        rows.collect()
    }

    /// Look up one TODO item by id.
    pub fn todo(&self, id: i64) -> Result<Option<TodoItem>> {
        self.conn
            .query_row(
                "SELECT id, project_id, title, description, status, position,
                        pane_id, agent_id, agent_session_id, awaiting_review,
                        created_at, updated_at, completed_at
                 FROM todos
                 WHERE id = ?1",
                params![id],
                Self::todo_from_row,
            )
            .optional()
    }

    /// Create a new TODO item at the end of its status lane.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_todo(
        &self,
        project_id: i64,
        title: &str,
        description: &str,
        status: TodoStatus,
        pane_id: Option<u64>,
        agent_id: Option<&str>,
        agent_session_id: Option<&str>,
    ) -> Result<TodoItem> {
        let next_pos: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM todos WHERE project_id = ?1 AND status = ?2",
            params![project_id, status.as_str()],
            |row| row.get(0),
        )?;
        let pane_sql = pane_id.and_then(|id| i64::try_from(id).ok());
        let completed_at_sql = (status == TodoStatus::Done).then_some("now");
        self.conn.execute(
            "INSERT INTO todos (
                project_id, title, description, status, position,
                pane_id, agent_id, agent_session_id, awaiting_review, completed_at
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, 0,
                CASE WHEN ?9 IS NOT NULL THEN datetime('now') ELSE NULL END
             )",
            params![
                project_id,
                title,
                description,
                status.as_str(),
                next_pos,
                pane_sql,
                agent_id,
                agent_session_id,
                completed_at_sql,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.todo(id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
    }

    /// Apply a partial update to one TODO item, returning the updated row.
    pub fn update_todo(&self, id: i64, patch: &TodoPatch) -> Result<Option<TodoItem>> {
        let Some(existing) = self.todo(id)? else {
            return Ok(None);
        };
        let title = patch.title.as_deref().unwrap_or(&existing.title);
        let description = patch
            .description
            .as_deref()
            .unwrap_or(&existing.description);
        let status = patch.status.unwrap_or(existing.status);
        let pane_id = patch.pane_id.unwrap_or(existing.pane_id);
        let agent_id = match &patch.agent_id {
            Some(next) => next.as_deref(),
            None => existing.agent_id.as_deref(),
        };
        let agent_session_id = match &patch.agent_session_id {
            Some(next) => next.as_deref(),
            None => existing.agent_session_id.as_deref(),
        };
        // Moving out of `in_progress` clears `awaiting_review` unless explicitly
        // overridden.
        let awaiting_review = match patch.awaiting_review {
            Some(value) => value && status == TodoStatus::InProgress,
            None => existing.awaiting_review && status == TodoStatus::InProgress,
        };
        let position = if status != existing.status {
            self.conn.query_row(
                "SELECT COALESCE(MAX(position), -1) + 1 FROM todos WHERE project_id = ?1 AND status = ?2",
                params![existing.project_id, status.as_str()],
                |row| row.get(0),
            )?
        } else {
            existing.position
        };
        let pane_sql = pane_id.and_then(|id| i64::try_from(id).ok());
        let set_completed = status == TodoStatus::Done && existing.status != TodoStatus::Done;
        let clear_completed = status != TodoStatus::Done && existing.completed_at.is_some();
        self.conn.execute(
            "UPDATE todos
             SET title = ?1,
                 description = ?2,
                 status = ?3,
                 position = ?4,
                 pane_id = ?5,
                 agent_id = ?6,
                 agent_session_id = ?7,
                 awaiting_review = ?8,
                 updated_at = datetime('now'),
                 completed_at = CASE
                     WHEN ?9 = 1 THEN datetime('now')
                     WHEN ?10 = 1 THEN NULL
                     ELSE completed_at
                 END
             WHERE id = ?11",
            params![
                title,
                description,
                status.as_str(),
                position,
                pane_sql,
                agent_id,
                agent_session_id,
                i64::from(awaiting_review),
                i64::from(set_completed),
                i64::from(clear_completed),
                id,
            ],
        )?;
        self.todo(id)
    }

    /// Delete one TODO item. Returns whether a row was removed.
    pub fn delete_todo(&self, id: i64) -> Result<bool> {
        let removed = self
            .conn
            .execute("DELETE FROM todos WHERE id = ?1", params![id])?;
        Ok(removed > 0)
    }

    /// Remove all `done` TODO items for one root repository board.
    pub fn clear_done_todos(&self, project_id: i64) -> Result<usize> {
        self.conn.execute(
            "DELETE FROM todos WHERE project_id = ?1 AND status = 'done'",
            params![project_id],
        )
    }

    /// Mark in-progress TODOs bound to `pane_id` as awaiting human review.
    /// Returns the distinct root project ids whose boards changed.
    pub fn mark_pane_todos_review(&self, pane_id: u64) -> Result<Vec<i64>> {
        let Ok(pane_sql) = i64::try_from(pane_id) else {
            return Ok(Vec::new());
        };
        let projects = self
            .projects_with_pane_todos(pane_sql, "status = 'in_progress' AND awaiting_review = 0")?;
        if projects.is_empty() {
            return Ok(projects);
        }
        self.conn.execute(
            "UPDATE todos
             SET awaiting_review = 1, updated_at = datetime('now')
             WHERE pane_id = ?1 AND status = 'in_progress' AND awaiting_review = 0",
            params![pane_sql],
        )?;
        Ok(projects)
    }

    /// Clear `awaiting_review` on TODOs bound to `pane_id` when the agent resumes running.
    pub fn clear_pane_todos_review(&self, pane_id: u64) -> Result<Vec<i64>> {
        let Ok(pane_sql) = i64::try_from(pane_id) else {
            return Ok(Vec::new());
        };
        let projects = self.projects_with_pane_todos(pane_sql, "awaiting_review = 1")?;
        if projects.is_empty() {
            return Ok(projects);
        }
        self.conn.execute(
            "UPDATE todos
             SET awaiting_review = 0, updated_at = datetime('now')
             WHERE pane_id = ?1 AND awaiting_review = 1",
            params![pane_sql],
        )?;
        Ok(projects)
    }

    /// Keep `agent_id` and `agent_session_id` fresh when a bound pane reports its session.
    pub fn update_pane_todos_session(
        &self,
        pane_id: u64,
        agent_id: &str,
        session_id: &str,
    ) -> Result<Vec<i64>> {
        let Ok(pane_sql) = i64::try_from(pane_id) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT project_id FROM todos
             WHERE pane_id = ?1
               AND (COALESCE(agent_id, '') != ?2 OR COALESCE(agent_session_id, '') != ?3)",
        )?;
        let changed: Vec<i64> = stmt
            .query_map(params![pane_sql, agent_id, session_id], |row| row.get(0))?
            .collect::<Result<_>>()?;
        if changed.is_empty() {
            return Ok(changed);
        }
        self.conn.execute(
            "UPDATE todos
             SET agent_id = ?2, agent_session_id = ?3, updated_at = datetime('now')
             WHERE pane_id = ?1",
            params![pane_sql, agent_id, session_id],
        )?;
        Ok(changed)
    }

    /// Detach `pane_id` from TODOs when that pane closes, keeping `agent_id` as history.
    pub fn unbind_pane_todos(&self, pane_id: u64) -> Result<Vec<i64>> {
        let Ok(pane_sql) = i64::try_from(pane_id) else {
            return Ok(Vec::new());
        };
        let projects = self.projects_with_pane_todos(pane_sql, "1 = 1")?;
        if projects.is_empty() {
            return Ok(projects);
        }
        self.conn.execute(
            "UPDATE todos
             SET pane_id = NULL, updated_at = datetime('now')
             WHERE pane_id = ?1",
            params![pane_sql],
        )?;
        Ok(projects)
    }

    fn projects_with_pane_todos(&self, pane_sql: i64, predicate: &str) -> Result<Vec<i64>> {
        let sql =
            format!("SELECT DISTINCT project_id FROM todos WHERE pane_id = ?1 AND {predicate}");
        let mut stmt = self.conn.prepare(&sql)?;
        stmt.query_map(params![pane_sql], |row| row.get(0))?
            .collect()
    }
}

/// Rewrite a version 1 or 2 layout so each project owns the panes that used to
/// belong to its sessions. Version 3 is left untouched.
fn rekey_workspace_json(json: &str, session_project: &HashMap<i64, i64>) -> Option<String> {
    let mut value: Value = serde_json::from_str(json).ok()?;
    let version = value.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version == 0 || version >= 3 {
        return None;
    }
    let layout = value.get("layout").cloned().unwrap_or(Value::Null);
    let focused_hint = value.get("focused").and_then(Value::as_u64);
    let grouped = if let Some(body) = layout.get("SessionLayouts") {
        projects_from_session_layouts(body, session_project, focused_hint)
    } else {
        projects_from_legacy_tree(
            &layout,
            value.get("panes").and_then(Value::as_array),
            session_project,
            focused_hint,
        )
    };
    let (projects, selected) = grouped.unwrap_or_else(|| (Vec::new(), None));
    let focused = selected
        .as_ref()
        .and_then(|project| {
            projects
                .iter()
                .find(|row| row.get("project") == Some(project))
        })
        .and_then(|row| row.get("focused"))
        .and_then(Value::as_u64)
        .unwrap_or(1);
    value["version"] = json!(3);
    value["layout"] = json!({
        "ProjectLayouts": {
            "selected": selected,
            "projects": projects,
        }
    });
    value["focused"] = json!(focused);
    value.as_object_mut()?.remove("panes");
    value.as_object_mut()?.remove("collapsed_projects");
    serde_json::to_string(&value).ok()
}

fn projects_from_session_layouts(
    body: &Value,
    session_project: &HashMap<i64, i64>,
    focused_hint: Option<u64>,
) -> Option<(Vec<Value>, Option<Value>)> {
    let selected_session = body.get("selected").and_then(Value::as_i64);
    let sessions = body.get("sessions")?.as_array()?;
    let mut order: Vec<i64> = Vec::new();
    let mut by_project: HashMap<i64, Vec<&Value>> = HashMap::new();
    for session in sessions {
        let Some(id) = session.get("session").and_then(Value::as_i64) else {
            continue;
        };
        let Some(project) = session_project.get(&id).copied() else {
            continue;
        };
        if !by_project.contains_key(&project) {
            order.push(project);
        }
        by_project.entry(project).or_default().push(session);
    }
    let selected_project = selected_session.and_then(|id| session_project.get(&id).copied());
    Some(finish_project_rows(
        order,
        by_project,
        selected_project,
        focused_hint,
    ))
}

fn projects_from_legacy_tree(
    layout: &Value,
    panes: Option<&Vec<Value>>,
    session_project: &HashMap<i64, i64>,
    focused_hint: Option<u64>,
) -> Option<(Vec<Value>, Option<Value>)> {
    let panes = panes?;
    let mut order: Vec<i64> = Vec::new();
    let mut panes_by_project: HashMap<i64, Vec<u64>> = HashMap::new();
    for pane in panes {
        let Some(pane_id) = pane.get("pane").and_then(Value::as_u64) else {
            continue;
        };
        let Some(session) = pane.get("session").and_then(Value::as_i64) else {
            continue;
        };
        let Some(project) = session_project.get(&session).copied() else {
            continue;
        };
        if !panes_by_project.contains_key(&project) {
            order.push(project);
        }
        panes_by_project.entry(project).or_default().push(pane_id);
    }
    let mut by_project: HashMap<i64, Vec<Value>> = HashMap::new();
    for (project, ids) in &panes_by_project {
        let valid: std::collections::HashSet<u64> = ids.iter().copied().collect();
        if let Some(filtered) = filter_layout(layout, &valid) {
            by_project.insert(
                *project,
                vec![json!({"layout": filtered, "focused": focused_hint.unwrap_or(ids[0])})],
            );
        }
    }
    let selected_project = focused_hint.and_then(|pane| {
        panes.iter().find_map(|row| {
            (row.get("pane").and_then(Value::as_u64) == Some(pane))
                .then(|| row.get("session").and_then(Value::as_i64))
                .flatten()
                .and_then(|session| session_project.get(&session).copied())
        })
    });
    Some(finish_project_rows(
        order,
        by_project
            .iter()
            .map(|(k, v)| (*k, v.iter().collect()))
            .collect(),
        selected_project,
        focused_hint,
    ))
}

fn finish_project_rows(
    order: Vec<i64>,
    by_project: HashMap<i64, Vec<&Value>>,
    selected_project: Option<i64>,
    focused_hint: Option<u64>,
) -> (Vec<Value>, Option<Value>) {
    let mut projects = Vec::new();
    for project in order {
        let Some(rows) = by_project.get(&project) else {
            continue;
        };
        let layouts: Vec<Value> = rows
            .iter()
            .filter_map(|row| row.get("layout").cloned())
            .collect();
        if layouts.is_empty() {
            continue;
        }
        let layout = merge_layouts(layouts);
        let focused = focused_hint
            .filter(|pane| contains_pane(&layout, *pane))
            .or_else(|| {
                rows.iter()
                    .filter_map(|row| row.get("focused").and_then(Value::as_u64))
                    .find(|pane| contains_pane(&layout, *pane))
            })
            .or_else(|| first_pane(&layout))
            .unwrap_or(1);
        projects.push(json!({
            "project": project,
            "layout": layout,
            "focused": focused,
        }));
    }
    let selected = selected_project
        .filter(|project| {
            projects
                .iter()
                .any(|row| row.get("project").and_then(Value::as_i64) == Some(*project))
        })
        .or_else(|| {
            projects
                .first()
                .and_then(|row| row.get("project"))
                .and_then(Value::as_i64)
        });
    (projects, selected.map(|id| json!(id)))
}

fn merge_layouts(layouts: Vec<Value>) -> Value {
    if layouts.len() == 1 {
        return layouts.into_iter().next().unwrap();
    }
    json!({
        "Group": {
            "horizontal": true,
            "sizes": [],
            "children": layouts,
        }
    })
}

fn filter_layout(node: &Value, valid: &std::collections::HashSet<u64>) -> Option<Value> {
    if let Some(pane) = node
        .get("Leaf")
        .and_then(|leaf| leaf.get("pane"))
        .and_then(Value::as_u64)
    {
        return valid.contains(&pane).then(|| node.clone());
    }
    let children = node.get("Group")?.get("children")?.as_array()?;
    let kept: Vec<Value> = children
        .iter()
        .filter_map(|child| filter_layout(child, valid))
        .collect();
    match kept.len() {
        0 => None,
        1 => kept.into_iter().next(),
        _ => {
            let mut group = node.get("Group")?.clone();
            group["children"] = json!(kept);
            if kept.len() != children.len() {
                group["sizes"] = json!([]);
            }
            Some(json!({ "Group": group }))
        }
    }
}

fn contains_pane(node: &Value, pane: u64) -> bool {
    if node
        .get("Leaf")
        .and_then(|leaf| leaf.get("pane"))
        .and_then(Value::as_u64)
        == Some(pane)
    {
        return true;
    }
    node.get("Group")
        .and_then(|group| group.get("children"))
        .and_then(Value::as_array)
        .is_some_and(|children| children.iter().any(|child| contains_pane(child, pane)))
}

fn first_pane(node: &Value) -> Option<u64> {
    if let Some(pane) = node
        .get("Leaf")
        .and_then(|leaf| leaf.get("pane"))
        .and_then(Value::as_u64)
    {
        return Some(pane);
    }
    node.get("Group")
        .and_then(|group| group.get("children"))
        .and_then(Value::as_array)
        .and_then(|children| children.iter().find_map(first_pane))
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

/// Whether agent panes resume their previous conversation at startup
/// (`"0"` starts a new one in each restored pane).
pub const SETTING_AUTO_RESUME: &str = "auto_resume";

/// Setting key marking that the first-run project was seeded. It stays
/// after the user deletes them, so an intentionally empty workspace is not
/// refilled on the next launch.
pub const SETTING_SEEDED: &str = "seeded";

/// Setting key holding the main window's last geometry as JSON.
pub const SETTING_WINDOW_BOUNDS: &str = "window_bounds";

/// Setting key holding the sidebar's width in px, chosen by dragging the
/// divider beside it.
pub const SETTING_SIDEBAR_WIDTH: &str = "sidebar_width";

/// Setting key holding the last agent id chosen for one-click TODO dispatch.
pub const SETTING_TODO_DEFAULT_AGENT: &str = "todo_default_agent";

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

    /// A file written by a newer build has already been migrated past this
    /// build's queries. Opening it must fail so the app can fall back to
    /// memory, not panic on the first read of a table that is gone.
    #[test]
    fn a_newer_schema_is_refused() {
        let dir = std::env::temp_dir().join(format!(
            "kumu-db-newer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kumu.db");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(conn);

        let Err(error) = Store::open(&path) else {
            panic!("a newer schema must not open");
        };
        assert!(matches!(
            error,
            OpenError::SchemaTooNew { found, supported }
                if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION
        ));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Renaming a project keeps it; deleting it removes only that row.
    #[test]
    fn project_rename_and_delete() {
        let store = store();
        let kept = store.insert_project("keep", "/keep").unwrap();
        let doomed = store.insert_project("doomed", "/doomed").unwrap();

        store.rename_project(doomed, "renamed").unwrap();
        assert!(
            store
                .projects()
                .unwrap()
                .iter()
                .any(|p| p.id == doomed && p.name == "renamed")
        );

        store.delete_project(doomed).unwrap();
        let projects = store.projects().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, kept);
    }

    /// A worktree row hangs off its repository and is removed with it; the
    /// directory on disk is not the database's business.
    #[test]
    fn worktrees_are_children_of_their_project() {
        let store = store();
        let repo = store.insert_project("repo", "/repo").unwrap();
        let worktree = store
            .insert_worktree(repo, "feat/login", "/repo-feat-login")
            .unwrap();

        let projects = store.projects().unwrap();
        assert_eq!(
            projects
                .iter()
                .find(|project| project.id == worktree)
                .unwrap()
                .parent_id,
            Some(repo)
        );

        store.delete_project(repo).unwrap();
        assert!(
            store
                .projects()
                .unwrap()
                .iter()
                .all(|project| project.id != worktree)
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

    /// A database from the chat era keeps its project and drops that era's
    /// tables; the old session rows go with the removed session list.
    #[test]
    fn an_older_database_drops_sessions_and_keeps_its_project() {
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
        let mut store = Store { conn };
        store.init().unwrap();

        assert_eq!(store.projects().unwrap().len(), 1, "the project survives");
        assert_eq!(store.projects().unwrap()[0].name, "kumu");
        let sessions_table: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'sessions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sessions_table, 0, "sessions are no longer a table");
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

        // Opening it again must be a no-op rather than re-running the upgrade.
        store.init().unwrap();
    }

    /// Two sessions under one project become one pane tree. A session in
    /// another project stays on that project.
    #[test]
    fn opening_an_old_layout_gives_each_project_its_panes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                path TEXT NOT NULL
            );
            CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                agent TEXT NOT NULL,
                title TEXT NOT NULL,
                cwd TEXT NOT NULL
            );
            CREATE TABLE workspace_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                layout_json TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            INSERT INTO projects (name, path) VALUES ('kumu', '/kumu'), ('other', '/other');
            INSERT INTO sessions (project_id, agent, title, cwd) VALUES
                (1, 'shell', '终端 1', '/kumu'),
                (1, 'shell', '终端 2', '/kumu'),
                (2, 'shell', '终端 3', '/other');
            PRAGMA user_version = 3;",
        )
        .unwrap();
        let layout = r#"{
            "version": 2,
            "layout": {
                "SessionLayouts": {
                    "selected": 2,
                    "sessions": [
                        {"session": 1, "layout": {"Leaf": {"pane": 1}}, "focused": 1},
                        {"session": 2, "layout": {"Leaf": {"pane": 2}}, "focused": 2},
                        {"session": 3, "layout": {"Leaf": {"pane": 3}}, "focused": 3}
                    ]
                }
            },
            "focused": 2
        }"#;
        conn.execute(
            "INSERT INTO workspace_state (id, layout_json) VALUES (1, ?1)",
            [layout],
        )
        .unwrap();
        let mut store = Store { conn };
        store.init().unwrap();

        let saved: serde_json::Value =
            serde_json::from_str(&store.workspace_state().unwrap().unwrap()).unwrap();
        assert_eq!(saved["version"], 3);
        let projects = saved["layout"]["ProjectLayouts"]["projects"]
            .as_array()
            .unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(saved["layout"]["ProjectLayouts"]["selected"], 1);
        assert_eq!(saved["focused"], 2);
        assert_eq!(projects[0]["focused"], 2);
        let merged = &projects[0]["layout"]["Group"]["children"];
        assert_eq!(merged[0]["Leaf"]["pane"], 1);
        assert_eq!(merged[1]["Leaf"]["pane"], 2);
        assert_eq!(projects[1]["layout"]["Leaf"]["pane"], 3);
    }

    #[test]
    fn legacy_layout_migration_preserves_surviving_split_sizes() {
        let layout = json!({
            "version": 1,
            "focused": 2,
            "layout": {"Group": {
                "horizontal": true,
                "sizes": [0.7, 0.3],
                "children": [
                    {"Group": {
                        "horizontal": false,
                        "sizes": [0.4, 0.6],
                        "children": [{"Leaf": {"pane": 1}}, {"Leaf": {"pane": 2}}]
                    }},
                    {"Leaf": {"pane": 3}}
                ]
            }},
            "panes": [
                {"pane": 1, "session": 10},
                {"pane": 2, "session": 10},
                {"pane": 3, "session": 20}
            ]
        });
        let migrated =
            rekey_workspace_json(&layout.to_string(), &HashMap::from([(10, 1), (20, 2)])).unwrap();
        let migrated: Value = serde_json::from_str(&migrated).unwrap();
        let projects = &migrated["layout"]["ProjectLayouts"]["projects"];
        assert_eq!(projects[0]["layout"]["Group"]["sizes"], json!([0.4, 0.6]));
        assert_eq!(projects[0]["focused"], 2);
        assert_eq!(projects[1]["layout"]["Leaf"]["pane"], 3);
    }

    #[test]
    fn a_failed_upgrade_rolls_back_layout_and_session_removal() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (
                 id INTEGER PRIMARY KEY, name TEXT NOT NULL, path TEXT NOT NULL
             );
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL);
             INSERT INTO sessions VALUES (1, 10);
             CREATE TABLE workspace_state (
                 id INTEGER PRIMARY KEY, layout_json TEXT NOT NULL, updated_at TEXT
             );
             PRAGMA user_version = 3;",
        )
        .unwrap();
        let layout = json!({
            "version": 2,
            "layout": {"SessionLayouts": {
                "selected": 1,
                "sessions": [{"session": 1, "layout": {"Leaf": {"pane": 1}}, "focused": 1}]
            }},
            "focused": 1
        })
        .to_string();
        conn.execute(
            "INSERT INTO workspace_state VALUES (1, ?1, NULL)",
            [&layout],
        )
        .unwrap();
        // Fail after the layout write, at the table-removal step.
        conn.execute_batch(
            "CREATE TABLE session_guard (session_id INTEGER REFERENCES sessions(id));
             INSERT INTO session_guard VALUES (1);
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        let mut store = Store { conn };
        assert!(store.init().is_err());
        assert_eq!(store.workspace_state().unwrap(), Some(layout));
        assert_eq!(
            store
                .conn
                .query_row("SELECT count(*) FROM sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            3
        );
        store.conn.execute("DROP TABLE session_guard", []).unwrap();
        store.init().unwrap();
        assert_eq!(
            store
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn todos_crud_review_transitions_and_project_cascade_work() {
        let store = store();
        let root = store.insert_project("kumu", "/tmp/kumu").unwrap();

        let item = store
            .insert_todo(
                root,
                "Implement TODO board",
                "Wire up SQLite and panel UI",
                TodoStatus::Todo,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(item.status, TodoStatus::Todo);
        assert!(!item.awaiting_review);
        assert_eq!(store.todos(root).unwrap().len(), 1);

        // Bind to pane 7 and move to in_progress.
        let active = store
            .update_todo(
                item.id,
                &TodoPatch {
                    status: Some(TodoStatus::InProgress),
                    pane_id: Some(Some(7)),
                    agent_id: Some(Some("claude".into())),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(active.status, TodoStatus::InProgress);
        assert_eq!(active.pane_id, Some(7));
        assert_eq!(active.agent_id.as_deref(), Some("claude"));

        // Agent completes -> awaiting review; agent resumes -> cleared.
        assert_eq!(store.mark_pane_todos_review(7).unwrap(), vec![root]);
        assert!(store.todo(item.id).unwrap().unwrap().awaiting_review);
        assert_eq!(store.clear_pane_todos_review(7).unwrap(), vec![root]);
        assert!(!store.todo(item.id).unwrap().unwrap().awaiting_review);

        // Unbinding a closed pane clears pane_id while preserving agent_id.
        assert_eq!(store.unbind_pane_todos(7).unwrap(), vec![root]);
        let detached = store.todo(item.id).unwrap().unwrap();
        assert_eq!(detached.pane_id, None);
        assert_eq!(detached.agent_id.as_deref(), Some("claude"));

        // Completing sets completed_at and clears awaiting_review.
        let done = store
            .update_todo(
                item.id,
                &TodoPatch {
                    status: Some(TodoStatus::Done),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(done.status, TodoStatus::Done);
        assert!(done.completed_at.is_some());
        assert!(!done.awaiting_review);

        // Deleting the root project cascades to its todos.
        store.delete_project(root).unwrap();
        assert!(store.todos(root).unwrap().is_empty());
    }
}
