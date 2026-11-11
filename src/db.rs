//! Local SQLite persistence for projects and the home window's last layout.
//!
//! Accessed synchronously on the UI (main) thread. Schema is created and
//! upgraded on open through `PRAGMA user_version`; tables and columns left over
//! from the removed in-app chat path are dropped there too, so an older
//! database keeps working with the current one. A database written by a newer
//! build is refused ([`OpenError::SchemaTooNew`]) rather than opened against
//! queries that no longer match it.

use std::collections::HashMap;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Result, params};
use serde_json::{Value, json};

/// A project: a directory with its own terminal panes.
#[derive(Clone, Debug)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub path: String,
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
const SCHEMA_VERSION: i64 = 4;

impl Store {
    pub fn open(path: &std::path::Path) -> Result<Self, OpenError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path)?;
        prepare(&conn)?;
        let store = Self { conn };
        // A newer build has already migrated this file. Its tables no longer
        // match this build's queries, so reopening it here must fail loudly
        // instead of panicking halfway through the first read.
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(OpenError::SchemaTooNew {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        store.init()?;
        Ok(store)
    }

    /// An empty store that lives only for this process. Used when the on-disk
    /// database cannot be opened, so the window still works for this run
    /// even though nothing is saved.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        prepare(&conn)?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    fn init(&self) -> Result<()> {
        // Keep the old layout and its session ownership together until the
        // rewritten layout, table removal, and schema version all commit.
        let transaction = self.conn.unchecked_transaction()?;
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
        if version < 4 {
            self.migrate_v4()?;
        }
        if version < SCHEMA_VERSION {
            self.conn
                .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        transaction.commit()
    }

    /// The project/session/settings schema, plus the cleanup of the removed
    /// in-app chat path. Existing databases have `user_version = 0` and already
    /// contain most of this, so every step here is idempotent.
    ///
    /// `sessions` is created even on a fresh database because the v3 step below
    /// inspects its columns; `migrate_v4` then drops it in the same transaction.
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

    /// Projects own panes directly. Older databases stored a session row per
    /// terminal; their layout is rewritten onto the owning project and the
    /// table is dropped. A project can later gain a worktree without growing
    /// another list in the sidebar.
    fn migrate_v4(&self) -> Result<()> {
        let has_sessions: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sessions')",
            [],
            |row| row.get(0),
        )?;
        if has_sessions {
            let mut session_project = HashMap::new();
            {
                let mut stmt = self.conn.prepare("SELECT id, project_id FROM sessions")?;
                let rows =
                    stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
                for row in rows {
                    let (session, project) = row?;
                    session_project.insert(session, project);
                }
            }
            if let Some(json) = self.workspace_state()?
                && let Some(next) = rekey_workspace_json(&json, &session_project)
            {
                self.save_workspace_state(&next)?;
            }
            self.conn.execute_batch(
                "DROP INDEX IF EXISTS sessions_last_used;
                 DROP TABLE IF EXISTS sessions;",
            )?;
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

    /// Rename a project without moving it.
    pub fn rename_project(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE projects SET name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        Ok(())
    }

    /// Delete a project. Callers stop its terminals first.
    pub fn delete_project(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM projects WHERE id = ?1", params![id])?;
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

/// Setting key marking that the first-run project was seeded. It stays
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
                .iter()
                .any(|p| p.id == doomed && p.name == "renamed")
        );

        store.delete_project(doomed).unwrap();
        let projects = store.projects();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, kept);
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
        let store = Store { conn };
        store.init().unwrap();

        assert_eq!(store.projects().len(), 1, "the project survives");
        assert_eq!(store.projects()[0].name, "kumu");
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
        let store = Store { conn };
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
            "CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL);
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
        let store = Store { conn };
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
}
