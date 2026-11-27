//! Local SQLite persistence for projects and the home window's last layout.
//!
//! Accessed synchronously on the UI (main) thread. Schema is created and
//! upgraded on open through `PRAGMA user_version`; tables and columns left over
//! from the removed in-app chat path are dropped there too, so an older
//! database keeps working with the current one. A database written by a newer
//! build is refused ([`OpenError::SchemaTooNew`]) rather than opened against
//! queries that no longer match it.

mod projects;
mod schema;
mod settings;
mod todos;

use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::{Connection, Result};
use serde::{Deserialize, Serialize};

use schema::prepare;

pub use settings::{
    SETTING_AUTO_RESUME, SETTING_FONT, SETTING_FONT_SIZE, SETTING_NOTIFICATION_SOUND,
    SETTING_NOTIFICATIONS, SETTING_PLUGIN_WIDTH, SETTING_SEEDED, SETTING_SIDEBAR_VISIBLE,
    SETTING_SIDEBAR_WIDTH, SETTING_TERM_FONT, SETTING_TERM_FONT_SIZE, SETTING_TERMINAL_THEME,
    SETTING_THEME_MODE, SETTING_TODO_DEFAULT_AGENT, SETTING_WINDOW_BOUNDS, db_path, get_agent_args,
    get_setting, set_agent_args, set_setting,
};

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
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::{Value, json};

    use super::schema::{SCHEMA_VERSION, rekey_workspace_json};
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
