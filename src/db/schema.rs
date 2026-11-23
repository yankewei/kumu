use std::collections::HashMap;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Result, TransactionBehavior, params};
use serde_json::{Value, json};

use super::{OpenError, Store};

/// How long a write waits for another connection's write to finish.
const STORE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Schema version stored in `PRAGMA user_version`. Bump it with a new
/// `migrate_vN` step; opening an older database applies the steps it is
/// missing exactly once.
pub(super) const SCHEMA_VERSION: i64 = 6;

impl Store {
    pub(super) fn init(&mut self) -> Result<(), OpenError> {
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
}

/// Rewrite a version 1 or 2 layout so each project owns the panes that used to
/// belong to its sessions. Version 3 is left untouched.
pub(super) fn rekey_workspace_json(
    json: &str,
    session_project: &HashMap<i64, i64>,
) -> Option<String> {
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
pub(super) fn prepare(conn: &Connection) -> Result<()> {
    conn.busy_timeout(STORE_BUSY_TIMEOUT)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}
