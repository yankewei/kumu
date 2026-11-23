use rusqlite::{OptionalExtension, Result, params};

use super::{Project, Store};

impl Store {
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
}
