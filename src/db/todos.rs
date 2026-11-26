use rusqlite::{OptionalExtension, Result, params};

use super::{Store, TodoItem, TodoPatch, TodoStatus};

impl Store {
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
