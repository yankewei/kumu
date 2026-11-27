use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Result, params};

use super::{Store, TodoAgentLink, TodoItem, TodoPatch, TodoStatus};

fn clean_opt(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

impl Store {
    fn link_from_row(row: &rusqlite::Row<'_>) -> Result<TodoAgentLink> {
        let pane_raw: Option<i64> = row.get(4)?;
        let awaiting_raw: i64 = row.get(7)?;
        Ok(TodoAgentLink {
            id: row.get(0)?,
            todo_id: row.get(1)?,
            agent_id: row.get::<_, Option<String>>(2)?.filter(|s| !s.is_empty()),
            agent_session_id: row.get::<_, Option<String>>(3)?.filter(|s| !s.is_empty()),
            pane_id: pane_raw.and_then(|value| u64::try_from(value).ok()),
            project_id: row.get(5)?,
            label: row.get(6)?,
            awaiting_review: awaiting_raw != 0,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
        })
    }

    fn todo_from_row(row: &rusqlite::Row<'_>) -> Result<TodoItem> {
        let status_raw: String = row.get(4)?;
        let awaiting_raw: i64 = row.get(6)?;
        Ok(TodoItem {
            id: row.get(0)?,
            project_id: row.get(1)?,
            title: row.get(2)?,
            description: row.get(3)?,
            status: TodoStatus::parse(&status_raw).unwrap_or(TodoStatus::Todo),
            position: row.get(5)?,
            pane_id: None,
            agent_id: None,
            agent_session_id: None,
            awaiting_review: awaiting_raw != 0,
            links: Vec::new(),
            created_at: row.get(7)?,
            updated_at: row.get(8)?,
            completed_at: row.get(9)?,
        })
    }

    fn attach_links(item: &mut TodoItem, links: Vec<TodoAgentLink>) {
        if let Some(primary) = links.iter().find(|l| l.pane_id.is_some()).or(links.first()) {
            item.pane_id = primary.pane_id;
            item.agent_id = primary.agent_id.clone();
            item.agent_session_id = primary.agent_session_id.clone();
        }
        if !links.is_empty() {
            item.awaiting_review =
                item.status == TodoStatus::InProgress && links.iter().any(|l| l.awaiting_review);
        } else {
            item.awaiting_review = item.status == TodoStatus::InProgress && item.awaiting_review;
        }
        item.links = links;
    }

    fn links_for_todo_conn(conn: &Connection, todo_id: i64) -> Result<Vec<TodoAgentLink>> {
        let mut stmt = conn.prepare(
            "SELECT id, todo_id, agent_id, agent_session_id, pane_id,
                    project_id, label, awaiting_review, created_at, updated_at
             FROM todo_agent_links
             WHERE todo_id = ?1
             ORDER BY (pane_id IS NOT NULL) DESC, awaiting_review DESC, updated_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![todo_id], Self::link_from_row)?;
        rows.collect()
    }

    fn links_for_board(&self, board_id: i64) -> Result<HashMap<i64, Vec<TodoAgentLink>>> {
        let mut stmt = self.conn.prepare(
            "SELECT l.id, l.todo_id, l.agent_id, l.agent_session_id, l.pane_id,
                    l.project_id, l.label, l.awaiting_review, l.created_at, l.updated_at
             FROM todo_agent_links l
             INNER JOIN todos t ON t.id = l.todo_id
             WHERE t.project_id = ?1
             ORDER BY (l.pane_id IS NOT NULL) DESC, l.awaiting_review DESC, l.updated_at DESC, l.id DESC",
        )?;
        let mut map: HashMap<i64, Vec<TodoAgentLink>> = HashMap::new();
        let rows = stmt.query_map(params![board_id], Self::link_from_row)?;
        for link in rows {
            let link = link?;
            map.entry(link.todo_id).or_default().push(link);
        }
        Ok(map)
    }

    /// All TODO items belonging to one root repository board.
    pub fn todos(&self, project_id: i64) -> Result<Vec<TodoItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, title, description, status, position,
                    awaiting_review, created_at, updated_at, completed_at
             FROM todos
             WHERE project_id = ?1
             ORDER BY position ASC, id DESC",
        )?;
        let mut items: Vec<TodoItem> = stmt
            .query_map(params![project_id], Self::todo_from_row)?
            .collect::<Result<_>>()?;
        let mut links_map = self.links_for_board(project_id)?;
        for item in &mut items {
            let links = links_map.remove(&item.id).unwrap_or_default();
            Self::attach_links(item, links);
        }
        Ok(items)
    }

    /// Look up one TODO item by id.
    pub fn todo(&self, id: i64) -> Result<Option<TodoItem>> {
        let Some(mut item) = self
            .conn
            .query_row(
                "SELECT id, project_id, title, description, status, position,
                        awaiting_review, created_at, updated_at, completed_at
                 FROM todos
                 WHERE id = ?1",
                params![id],
                Self::todo_from_row,
            )
            .optional()?
        else {
            return Ok(None);
        };
        let links = Self::links_for_todo_conn(&self.conn, id)?;
        Self::attach_links(&mut item, links);
        Ok(Some(item))
    }

    /// Look up one `TodoAgentLink` by id.
    pub fn todo_link(&self, link_id: i64) -> Result<Option<TodoAgentLink>> {
        self.conn
            .query_row(
                "SELECT id, todo_id, agent_id, agent_session_id, pane_id,
                        project_id, label, awaiting_review, created_at, updated_at
                 FROM todo_agent_links
                 WHERE id = ?1",
                params![link_id],
                Self::link_from_row,
            )
            .optional()
    }

    /// All TODO items currently linked to live `pane_id`.
    pub fn todos_for_pane(&self, pane_id: u64) -> Result<Vec<TodoItem>> {
        let Ok(pane_sql) = i64::try_from(pane_id) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT t.id, t.project_id, t.title, t.description, t.status, t.position,
                    t.awaiting_review, t.created_at, t.updated_at, t.completed_at
             FROM todos t
             INNER JOIN todo_agent_links l ON l.todo_id = t.id
             WHERE l.pane_id = ?1
             ORDER BY (t.status = 'done') ASC, t.position ASC, t.id DESC",
        )?;
        let mut items: Vec<TodoItem> = stmt
            .query_map(params![pane_sql], Self::todo_from_row)?
            .collect::<Result<_>>()?;
        if items.is_empty() {
            return Ok(items);
        }
        let mut link_stmt = self.conn.prepare(
            "SELECT l.id, l.todo_id, l.agent_id, l.agent_session_id, l.pane_id,
                    l.project_id, l.label, l.awaiting_review, l.created_at, l.updated_at
             FROM todo_agent_links l
             WHERE l.todo_id IN (
                 SELECT DISTINCT todo_id FROM todo_agent_links WHERE pane_id = ?1
             )
             ORDER BY (l.pane_id IS NOT NULL) DESC, l.awaiting_review DESC, l.updated_at DESC, l.id DESC",
        )?;
        let mut links_map: HashMap<i64, Vec<TodoAgentLink>> = HashMap::new();
        for link in link_stmt.query_map(params![pane_sql], Self::link_from_row)? {
            let link = link?;
            links_map.entry(link.todo_id).or_default().push(link);
        }
        for item in &mut items {
            let links = links_map.remove(&item.id).unwrap_or_default();
            Self::attach_links(item, links);
        }
        Ok(items)
    }

    /// Lightweight batched lookup of non-done `(todo_id, title)` badges for a
    /// set of visible panes, avoiding N+1 queries during frame snapshots.
    pub fn open_todo_summaries_for_panes(
        &self,
        pane_ids: &[u64],
    ) -> Result<HashMap<u64, Vec<(i64, String)>>> {
        let mut out: HashMap<u64, Vec<(i64, String)>> = HashMap::new();
        let pane_sqls: Vec<i64> = pane_ids
            .iter()
            .filter_map(|p| i64::try_from(*p).ok())
            .collect();
        if pane_sqls.is_empty() {
            return Ok(out);
        }
        let placeholders = vec!["?"; pane_sqls.len()].join(", ");
        let sql = format!(
            "SELECT DISTINCT l.pane_id, t.id, t.title, t.position
             FROM todo_agent_links l
             INNER JOIN todos t ON t.id = l.todo_id
             WHERE l.pane_id IN ({placeholders}) AND t.status != 'done'
             ORDER BY t.position ASC, t.id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(pane_sqls.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (pane_raw, todo_id, title) = row?;
            if let Ok(pane_id) = u64::try_from(pane_raw) {
                out.entry(pane_id).or_default().push((todo_id, title));
            }
        }
        Ok(out)
    }

    /// Create a new TODO item at the end of its status lane, optionally creating
    /// an initial `todo_agent_links` row in the same transaction.
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
        self.insert_todo_with_link(
            project_id,
            title,
            description,
            status,
            pane_id,
            agent_id,
            agent_session_id,
            Some(project_id),
            "",
        )
    }

    /// Create a TODO item and (when any link field is present) its initial
    /// `todo_agent_links` entry atomically inside one SQLite transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_todo_with_link(
        &self,
        project_id: i64,
        title: &str,
        description: &str,
        status: TodoStatus,
        pane_id: Option<u64>,
        agent_id: Option<&str>,
        agent_session_id: Option<&str>,
        link_project_id: Option<i64>,
        link_label: &str,
    ) -> Result<TodoItem> {
        let tx = self.conn.unchecked_transaction()?;
        let next_pos: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM todos WHERE project_id = ?1 AND status = ?2",
            params![project_id, status.as_str()],
            |row| row.get(0),
        )?;
        let completed_at_sql = (status == TodoStatus::Done).then_some("now");
        tx.execute(
            "INSERT INTO todos (
                project_id, title, description, status, position,
                pane_id, agent_id, agent_session_id, awaiting_review, completed_at
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                NULL, NULL, NULL, 0,
                CASE WHEN ?6 IS NOT NULL THEN datetime('now') ELSE NULL END
             )",
            params![
                project_id,
                title,
                description,
                status.as_str(),
                next_pos,
                completed_at_sql,
            ],
        )?;
        let id = tx.last_insert_rowid();
        let agent_clean = clean_opt(agent_id);
        let session_clean = clean_opt(agent_session_id);
        if pane_id.is_some() || agent_clean.is_some() || session_clean.is_some() {
            let pane_sql = pane_id.and_then(|p| i64::try_from(p).ok());
            tx.execute(
                "INSERT INTO todo_agent_links (
                    todo_id, agent_id, agent_session_id, pane_id, project_id, label, awaiting_review
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![
                    id,
                    agent_clean,
                    session_clean,
                    pane_sql,
                    link_project_id,
                    link_label.trim(),
                ],
            )?;
        }
        tx.commit()?;
        self.todo(id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
    }

    /// Apply a partial update to one TODO item, returning the updated row.
    /// Legacy binding columns on `todos` are never written here.
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
        let legacy_awaiting_review = if status != TodoStatus::InProgress {
            false
        } else if let Some(flag) = patch.awaiting_review {
            if existing.links.is_empty() {
                flag
            } else {
                // Once links exist, per-link `todo_agent_links.awaiting_review`
                // is authoritative; never keep stale legacy flags alive.
                false
            }
        } else if !existing.links.is_empty() {
            false
        } else {
            existing.awaiting_review
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
        let set_completed = status == TodoStatus::Done && existing.status != TodoStatus::Done;
        let clear_completed = status != TodoStatus::Done && existing.completed_at.is_some();

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE todos
             SET title = ?1,
                 description = ?2,
                 status = ?3,
                 position = ?4,
                 awaiting_review = ?5,
                 updated_at = datetime('now'),
                 completed_at = CASE
                     WHEN ?6 = 1 THEN datetime('now')
                     WHEN ?7 = 1 THEN NULL
                     ELSE completed_at
                 END
             WHERE id = ?8",
            params![
                title,
                description,
                status.as_str(),
                position,
                i64::from(legacy_awaiting_review),
                i64::from(set_completed),
                i64::from(clear_completed),
                id,
            ],
        )?;
        if status != TodoStatus::InProgress {
            tx.execute(
                "UPDATE todo_agent_links SET awaiting_review = 0, updated_at = datetime('now')
                 WHERE todo_id = ?1 AND awaiting_review = 1",
                params![id],
            )?;
        } else if let Some(flag) = patch.awaiting_review {
            if let Some(pane_sql) = patch
                .review_pane_id
                .and_then(|pane| i64::try_from(pane).ok())
            {
                let updated = tx.execute(
                    "UPDATE todo_agent_links SET awaiting_review = ?1, updated_at = datetime('now')
                     WHERE todo_id = ?2 AND pane_id = ?3",
                    params![i64::from(flag), id, pane_sql],
                )?;
                if updated == 0 && existing.links.is_empty() {
                    tx.execute(
                        "UPDATE todos SET awaiting_review = ?1 WHERE id = ?2",
                        params![i64::from(flag), id],
                    )?;
                }
            } else {
                tx.execute(
                    "UPDATE todo_agent_links SET awaiting_review = ?1, updated_at = datetime('now')
                     WHERE todo_id = ?2 AND pane_id IS NOT NULL",
                    params![i64::from(flag), id],
                )?;
            }
        }
        tx.commit()?;
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

    /// Append or idempotently update an agent/session link on `todo_id`.
    ///
    /// Deduplication priority:
    /// 1. Existing link on `(todo_id, pane_id)` when `pane_id` is `Some`.
    /// 2. Existing historical link on `(todo_id, agent_session_id)` when session is non-empty.
    /// 3. Insert a new `todo_agent_links` row.
    ///
    /// Returns `(board_project_id, link)` when the todo exists.
    #[allow(clippy::too_many_arguments)]
    pub fn add_todo_link(
        &self,
        todo_id: i64,
        agent_id: Option<&str>,
        agent_session_id: Option<&str>,
        pane_id: Option<u64>,
        project_id: Option<i64>,
        label: &str,
    ) -> Result<Option<(i64, TodoAgentLink)>> {
        let Some(board_id): Option<i64> = self
            .conn
            .query_row(
                "SELECT project_id FROM todos WHERE id = ?1",
                params![todo_id],
                |row| row.get(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let agent_clean = clean_opt(agent_id);
        let session_clean = clean_opt(agent_session_id);
        let pane_sql = pane_id.and_then(|p| i64::try_from(p).ok());
        let label_clean = label.trim();

        let tx = self.conn.unchecked_transaction()?;

        // 1. Match existing link on (todo_id, pane_id).
        let existing_by_pane: Option<i64> = if let Some(p) = pane_sql {
            tx.query_row(
                "SELECT id FROM todo_agent_links WHERE todo_id = ?1 AND pane_id = ?2 LIMIT 1",
                params![todo_id, p],
                |row| row.get(0),
            )
            .optional()?
        } else {
            None
        };

        // 2. Match existing link on (todo_id, agent_session_id).
        let existing_by_session: Option<i64> = if let Some(ses) = session_clean {
            tx.query_row(
                "SELECT id FROM todo_agent_links WHERE todo_id = ?1 AND agent_session_id = ?2 LIMIT 1",
                params![todo_id, ses],
                |row| row.get(0),
            )
            .optional()?
        } else {
            None
        };

        let link_id = match (existing_by_pane, existing_by_session) {
            (Some(pane_link_id), Some(ses_link_id)) if pane_link_id != ses_link_id => {
                // Merge into the session link while preserving review state & metadata
                // from the outgoing pane link so the unique (todo_id, agent_session_id) index holds.
                let old_meta: Option<(Option<i64>, String, i64)> = tx
                    .query_row(
                        "SELECT project_id, label, awaiting_review
                         FROM todo_agent_links WHERE id = ?1",
                        params![pane_link_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                let (old_proj, old_label, old_review) =
                    old_meta.unwrap_or((None, String::new(), 0));
                let merged_project = project_id.or(old_proj);
                let merged_label = if !label_clean.is_empty() {
                    label_clean
                } else {
                    old_label.trim()
                };
                tx.execute(
                    "DELETE FROM todo_agent_links WHERE id = ?1",
                    params![pane_link_id],
                )?;
                tx.execute(
                    "UPDATE todo_agent_links
                     SET agent_id = COALESCE(?1, agent_id),
                         pane_id = ?2,
                         project_id = COALESCE(?3, project_id),
                         label = CASE WHEN ?4 != '' THEN ?4 ELSE label END,
                         awaiting_review = MAX(awaiting_review, ?5),
                         updated_at = datetime('now')
                     WHERE id = ?6",
                    params![
                        agent_clean,
                        pane_sql,
                        merged_project,
                        merged_label,
                        old_review,
                        ses_link_id,
                    ],
                )?;
                ses_link_id
            }
            (Some(target_id), _) | (None, Some(target_id)) => {
                tx.execute(
                    "UPDATE todo_agent_links
                     SET agent_id = COALESCE(?1, agent_id),
                         agent_session_id = COALESCE(?2, agent_session_id),
                         pane_id = COALESCE(?3, pane_id),
                         project_id = COALESCE(?4, project_id),
                         label = CASE WHEN ?5 != '' THEN ?5 ELSE label END,
                         updated_at = datetime('now')
                     WHERE id = ?6",
                    params![
                        agent_clean,
                        session_clean,
                        pane_sql,
                        project_id,
                        label_clean,
                        target_id,
                    ],
                )?;
                target_id
            }
            (None, None) => {
                tx.execute(
                    "INSERT INTO todo_agent_links (
                        todo_id, agent_id, agent_session_id, pane_id, project_id, label, awaiting_review
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                    params![
                        todo_id,
                        agent_clean,
                        session_clean,
                        pane_sql,
                        project_id,
                        label_clean,
                    ],
                )?;
                tx.last_insert_rowid()
            }
        };

        tx.execute(
            "UPDATE todos SET updated_at = datetime('now') WHERE id = ?1",
            params![todo_id],
        )?;
        tx.commit()?;
        let link = self
            .todo_link(link_id)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        Ok(Some((board_id, link)))
    }

    /// Rebind an existing historical link to a freshly resumed live `pane_id`.
    pub fn rebind_todo_link_pane(
        &self,
        link_id: i64,
        pane_id: u64,
        project_id: Option<i64>,
    ) -> Result<Option<i64>> {
        let Some((todo_id, board_id)): Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT l.todo_id, t.project_id
                 FROM todo_agent_links l
                 INNER JOIN todos t ON t.id = l.todo_id
                 WHERE l.id = ?1",
                params![link_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let pane_sql = i64::try_from(pane_id).ok();
        self.conn.execute(
            "UPDATE todo_agent_links
             SET pane_id = ?1,
                 project_id = COALESCE(?2, project_id),
                 awaiting_review = 0,
                 updated_at = datetime('now')
             WHERE id = ?3",
            params![pane_sql, project_id, link_id],
        )?;
        self.conn.execute(
            "UPDATE todos SET updated_at = datetime('now') WHERE id = ?1",
            params![todo_id],
        )?;
        Ok(Some(board_id))
    }

    /// Detach live `pane_id` from one link while preserving `agent_id` & `agent_session_id`.
    pub fn detach_todo_link_pane(&self, link_id: i64) -> Result<Option<i64>> {
        let Some((todo_id, board_id)): Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT l.todo_id, t.project_id
                 FROM todo_agent_links l
                 INNER JOIN todos t ON t.id = l.todo_id
                 WHERE l.id = ?1",
                params![link_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE todo_agent_links
             SET pane_id = NULL, awaiting_review = 0, updated_at = datetime('now')
             WHERE id = ?1",
            params![link_id],
        )?;
        tx.execute(
            "UPDATE todos SET awaiting_review = 0 WHERE id = ?1 AND awaiting_review = 1",
            params![todo_id],
        )?;
        tx.commit()?;
        Ok(Some(board_id))
    }

    /// Detach the caller's `pane_id` from a specific `todo_id` (used by CLI `--unbind`).
    pub fn detach_todo_pane_link(&self, todo_id: i64, pane_id: u64) -> Result<Option<i64>> {
        let Ok(pane_sql) = i64::try_from(pane_id) else {
            return Ok(None);
        };
        let Some(board_id): Option<i64> = self
            .conn
            .query_row(
                "SELECT project_id FROM todos WHERE id = ?1",
                params![todo_id],
                |row| row.get(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE todo_agent_links
             SET pane_id = NULL, awaiting_review = 0, updated_at = datetime('now')
             WHERE todo_id = ?1 AND pane_id = ?2",
            params![todo_id, pane_sql],
        )?;
        if changed > 0 {
            tx.execute(
                "UPDATE todos SET awaiting_review = 0 WHERE id = ?1 AND awaiting_review = 1",
                params![todo_id],
            )?;
        }
        tx.commit()?;
        Ok((changed > 0).then_some(board_id))
    }

    /// Permanently delete one `todo_agent_links` row. Also clears legacy
    /// `todos.awaiting_review` so deleting the last migrated link never
    /// resurrects a stale awaiting-review badge.
    pub fn remove_todo_link(&self, link_id: i64) -> Result<Option<i64>> {
        let Some((todo_id, board_id)): Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT l.todo_id, t.project_id
                 FROM todo_agent_links l
                 INNER JOIN todos t ON t.id = l.todo_id
                 WHERE l.id = ?1",
                params![link_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let tx = self.conn.unchecked_transaction()?;
        let removed = tx.execute(
            "DELETE FROM todo_agent_links WHERE id = ?1",
            params![link_id],
        )?;
        if removed > 0 {
            tx.execute(
                "UPDATE todos SET awaiting_review = 0 WHERE id = ?1 AND awaiting_review = 1",
                params![todo_id],
            )?;
        }
        tx.commit()?;
        Ok((removed > 0).then_some(board_id))
    }

    /// Clear `awaiting_review` on one specific `(todo_id, pane_id)` link without
    /// touching other TODOs bound to the same pane.
    pub fn clear_todo_pane_review(&self, todo_id: i64, pane_id: u64) -> Result<Option<i64>> {
        let Ok(pane_sql) = i64::try_from(pane_id) else {
            return Ok(None);
        };
        let Some(board_id): Option<i64> = self
            .conn
            .query_row(
                "SELECT project_id FROM todos WHERE id = ?1",
                params![todo_id],
                |row| row.get(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let changed = self.conn.execute(
            "UPDATE todo_agent_links
             SET awaiting_review = 0, updated_at = datetime('now')
             WHERE todo_id = ?1 AND pane_id = ?2 AND awaiting_review = 1",
            params![todo_id, pane_sql],
        )?;
        Ok((changed > 0).then_some(board_id))
    }

    /// Clear `awaiting_review` on one specific `link_id` (UI "已确认" action).
    pub fn clear_todo_link_review(&self, link_id: i64) -> Result<Option<i64>> {
        let Some((todo_id, board_id)): Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT l.todo_id, t.project_id
                 FROM todo_agent_links l
                 INNER JOIN todos t ON t.id = l.todo_id
                 WHERE l.id = ?1",
                params![link_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE todo_agent_links
             SET awaiting_review = 0, updated_at = datetime('now')
             WHERE id = ?1 AND awaiting_review = 1",
            params![link_id],
        )?;
        tx.execute(
            "UPDATE todos SET awaiting_review = 0 WHERE id = ?1 AND awaiting_review = 1",
            params![todo_id],
        )?;
        tx.commit()?;
        Ok((changed > 0).then_some(board_id))
    }

    /// Mark in-progress links bound to `pane_id` as awaiting human review.
    pub fn mark_pane_todos_review(&self, pane_id: u64) -> Result<Vec<i64>> {
        let Ok(pane_sql) = i64::try_from(pane_id) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT t.project_id
             FROM todo_agent_links l
             INNER JOIN todos t ON t.id = l.todo_id
             WHERE l.pane_id = ?1 AND t.status = 'in_progress' AND l.awaiting_review = 0",
        )?;
        let boards: Vec<i64> = stmt
            .query_map(params![pane_sql], |row| row.get(0))?
            .collect::<Result<_>>()?;
        if boards.is_empty() {
            return Ok(boards);
        }
        self.conn.execute(
            "UPDATE todo_agent_links
             SET awaiting_review = 1, updated_at = datetime('now')
             WHERE pane_id = ?1
               AND awaiting_review = 0
               AND todo_id IN (SELECT id FROM todos WHERE status = 'in_progress')",
            params![pane_sql],
        )?;
        Ok(boards)
    }

    /// Clear `awaiting_review` only on links bound to `pane_id` (other links on the
    /// same TODO keep their own review state).
    pub fn clear_pane_todos_review(&self, pane_id: u64) -> Result<Vec<i64>> {
        let Ok(pane_sql) = i64::try_from(pane_id) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT t.project_id
             FROM todo_agent_links l
             INNER JOIN todos t ON t.id = l.todo_id
             WHERE l.pane_id = ?1 AND l.awaiting_review = 1",
        )?;
        let boards: Vec<i64> = stmt
            .query_map(params![pane_sql], |row| row.get(0))?
            .collect::<Result<_>>()?;
        if boards.is_empty() {
            return Ok(boards);
        }
        self.conn.execute(
            "UPDATE todo_agent_links
             SET awaiting_review = 0, updated_at = datetime('now')
             WHERE pane_id = ?1 AND awaiting_review = 1",
            params![pane_sql],
        )?;
        Ok(boards)
    }

    /// Keep `agent_id` and `agent_session_id` fresh on all links bound to `pane_id`.
    /// Merges cleanly if `(todo_id, session_id)` already exists as a historical link.
    pub fn sync_pane_link_session(
        &self,
        pane_id: u64,
        agent_id: &str,
        session_id: &str,
    ) -> Result<Vec<i64>> {
        let Ok(pane_sql) = i64::try_from(pane_id) else {
            return Ok(Vec::new());
        };
        let agent_clean = agent_id.trim();
        let session_clean = session_id.trim();
        if agent_clean.is_empty() && session_clean.is_empty() {
            return Ok(Vec::new());
        }

        let mut stmt = self.conn.prepare(
            "SELECT l.id, l.todo_id, t.project_id
             FROM todo_agent_links l
             INNER JOIN todos t ON t.id = l.todo_id
             WHERE l.pane_id = ?1
               AND (COALESCE(l.agent_id, '') != ?2 OR COALESCE(l.agent_session_id, '') != ?3)",
        )?;
        let targets: Vec<(i64, i64, i64)> = stmt
            .query_map(params![pane_sql, agent_clean, session_clean], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<_>>()?;
        if targets.is_empty() {
            return Ok(Vec::new());
        }

        let tx = self.conn.unchecked_transaction()?;
        let mut boards = Vec::new();
        for (link_id, todo_id, board_id) in targets {
            let duplicate: Option<i64> = if !session_clean.is_empty() {
                tx.query_row(
                    "SELECT id FROM todo_agent_links
                     WHERE todo_id = ?1 AND agent_session_id = ?2 AND id != ?3
                     LIMIT 1",
                    params![todo_id, session_clean, link_id],
                    |row| row.get(0),
                )
                .optional()?
            } else {
                None
            };
            if let Some(existing_id) = duplicate {
                let old_meta: Option<(Option<i64>, String, i64)> = tx
                    .query_row(
                        "SELECT project_id, label, awaiting_review
                         FROM todo_agent_links WHERE id = ?1",
                        params![link_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                let (old_proj, old_label, old_review) =
                    old_meta.unwrap_or((None, String::new(), 0));
                tx.execute(
                    "DELETE FROM todo_agent_links WHERE id = ?1",
                    params![link_id],
                )?;
                tx.execute(
                    "UPDATE todo_agent_links
                     SET agent_id = CASE WHEN ?1 != '' THEN ?1 ELSE agent_id END,
                         pane_id = ?2,
                         project_id = COALESCE(?3, project_id),
                         label = CASE WHEN ?4 != '' THEN ?4 ELSE label END,
                         awaiting_review = MAX(awaiting_review, ?5),
                         updated_at = datetime('now')
                     WHERE id = ?6",
                    params![
                        agent_clean,
                        pane_sql,
                        old_proj,
                        old_label.trim(),
                        old_review,
                        existing_id,
                    ],
                )?;
            } else {
                tx.execute(
                    "UPDATE todo_agent_links
                     SET agent_id = CASE WHEN ?1 != '' THEN ?1 ELSE agent_id END,
                         agent_session_id = CASE WHEN ?2 != '' THEN ?2 ELSE agent_session_id END,
                         updated_at = datetime('now')
                     WHERE id = ?3",
                    params![agent_clean, session_clean, link_id],
                )?;
            }
            if !boards.contains(&board_id) {
                boards.push(board_id);
            }
        }
        tx.commit()?;
        Ok(boards)
    }

    /// Keep the snapshot `label` fresh when a bound agent updates its conversation title.
    pub fn sync_pane_link_label(&self, pane_id: u64, label: &str) -> Result<Vec<i64>> {
        let Ok(pane_sql) = i64::try_from(pane_id) else {
            return Ok(Vec::new());
        };
        let label_clean = label.trim();
        if label_clean.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT t.project_id
             FROM todo_agent_links l
             INNER JOIN todos t ON t.id = l.todo_id
             WHERE l.pane_id = ?1 AND l.label != ?2",
        )?;
        let boards: Vec<i64> = stmt
            .query_map(params![pane_sql, label_clean], |row| row.get(0))?
            .collect::<Result<_>>()?;
        if boards.is_empty() {
            return Ok(boards);
        }
        self.conn.execute(
            "UPDATE todo_agent_links
             SET label = ?2, updated_at = datetime('now')
             WHERE pane_id = ?1 AND label != ?2",
            params![pane_sql, label_clean],
        )?;
        Ok(boards)
    }

    /// Detach live `pane_id` from all links when that pane closes or exits back to
    /// a shell, preserving `agent_id` & `agent_session_id` for future resume.
    pub fn detach_pane_links(&self, pane_id: u64) -> Result<Vec<i64>> {
        let Ok(pane_sql) = i64::try_from(pane_id) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT t.project_id
             FROM todo_agent_links l
             INNER JOIN todos t ON t.id = l.todo_id
             WHERE l.pane_id = ?1",
        )?;
        let boards: Vec<i64> = stmt
            .query_map(params![pane_sql], |row| row.get(0))?
            .collect::<Result<_>>()?;
        if boards.is_empty() {
            return Ok(boards);
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE todos
             SET awaiting_review = 0
             WHERE awaiting_review = 1
               AND id IN (SELECT todo_id FROM todo_agent_links WHERE pane_id = ?1)",
            params![pane_sql],
        )?;
        tx.execute(
            "UPDATE todo_agent_links
             SET pane_id = NULL, awaiting_review = 0, updated_at = datetime('now')
             WHERE pane_id = ?1",
            params![pane_sql],
        )?;
        tx.commit()?;
        Ok(boards)
    }

    /// Reconcile persisted `pane_id`s against the live panes restored at startup,
    /// detaching any stale `pane_id` that no longer exists in the workspace so
    /// historical sessions remain immediately resumable.
    pub fn reconcile_live_panes(&self, live_pane_ids: &[u64]) -> Result<Vec<i64>> {
        let live_sqls: Vec<i64> = live_pane_ids
            .iter()
            .filter_map(|p| i64::try_from(*p).ok())
            .collect();
        let (where_clause, params_vec) = if live_sqls.is_empty() {
            ("l.pane_id IS NOT NULL".to_string(), Vec::new())
        } else {
            let placeholders = vec!["?"; live_sqls.len()].join(", ");
            (
                format!("l.pane_id IS NOT NULL AND l.pane_id NOT IN ({placeholders})"),
                live_sqls,
            )
        };
        let query = format!(
            "SELECT DISTINCT t.project_id
             FROM todo_agent_links l
             INNER JOIN todos t ON t.id = l.todo_id
             WHERE {where_clause}"
        );
        let mut stmt = self.conn.prepare(&query)?;
        let boards: Vec<i64> = stmt
            .query_map(rusqlite::params_from_iter(params_vec.iter()), |row| {
                row.get(0)
            })?
            .collect::<Result<_>>()?;
        if boards.is_empty() {
            return Ok(boards);
        }
        let update_where = if params_vec.is_empty() {
            "pane_id IS NOT NULL".to_string()
        } else {
            let placeholders = vec!["?"; params_vec.len()].join(", ");
            format!("pane_id IS NOT NULL AND pane_id NOT IN ({placeholders})")
        };
        let update_sql = format!(
            "UPDATE todo_agent_links
             SET pane_id = NULL, awaiting_review = 0, updated_at = datetime('now')
             WHERE {update_where}"
        );
        self.conn
            .execute(&update_sql, rusqlite::params_from_iter(params_vec.iter()))?;
        Ok(boards)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stage 0 verification: migrating a real v7 database containing a shell-only
    /// binding (`pane_id` set, `agent_id = NULL`) and a historical session binding
    /// must succeed without violating NOT NULL and preserve all bindings.
    #[test]
    fn migrate_v7_to_v8_preserves_shell_and_session_bindings() {
        let dir = std::env::temp_dir().join(format!(
            "kumu-v7-migrate-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_file = dir.join("kumu.db");

        // Build a v7 database manually.
        {
            let conn = Connection::open(&db_file).unwrap();
            conn.execute_batch(
                "CREATE TABLE projects (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    path TEXT NOT NULL,
                    parent_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE workspace_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    layout_json TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE todos (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                    title TEXT NOT NULL,
                    description TEXT NOT NULL DEFAULT '',
                    status TEXT NOT NULL DEFAULT 'todo',
                    position INTEGER NOT NULL DEFAULT 0,
                    pane_id INTEGER,
                    agent_id TEXT,
                    agent_session_id TEXT,
                    awaiting_review INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                    completed_at TEXT
                );
                PRAGMA user_version = 7;
                INSERT INTO projects (id, name, path) VALUES (1, 'kumu', '/tmp/kumu');
                -- Legacy shell-only bind: pane_id present, agent_id NULL
                INSERT INTO todos (id, project_id, title, status, pane_id, agent_id, agent_session_id, awaiting_review)
                VALUES (10, 1, 'Shell bind', 'in_progress', 42, NULL, NULL, 1);
                -- Legacy closed session: pane_id NULL, agent + session present
                INSERT INTO todos (id, project_id, title, status, pane_id, agent_id, agent_session_id, awaiting_review)
                VALUES (11, 1, 'History session', 'todo', NULL, 'claude', 'ses_abc123', 0);",
            )
            .unwrap();
        }

        let store = Store::open(&db_file).unwrap();
        let todos = store.todos(1).unwrap();
        assert_eq!(todos.len(), 2);

        let shell_todo = todos.iter().find(|t| t.id == 10).unwrap();
        assert_eq!(shell_todo.links.len(), 1);
        assert_eq!(shell_todo.links[0].pane_id, Some(42));
        assert_eq!(shell_todo.links[0].agent_id, None);
        assert!(shell_todo.links[0].awaiting_review);
        assert!(shell_todo.awaiting_review);

        let hist_todo = todos.iter().find(|t| t.id == 11).unwrap();
        assert_eq!(hist_todo.links.len(), 1);
        assert_eq!(hist_todo.links[0].pane_id, None);
        assert_eq!(hist_todo.links[0].agent_id.as_deref(), Some("claude"));
        assert_eq!(
            hist_todo.links[0].agent_session_id.as_deref(),
            Some("ses_abc123")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_links_review_isolation_and_idempotent_rebind() {
        let store = Store::in_memory().unwrap();
        let project_id = store.insert_project("kumu", "/tmp/kumu").unwrap();
        let todo = store
            .insert_todo(
                project_id,
                "Multi agent task",
                "",
                TodoStatus::InProgress,
                None,
                None,
                None,
            )
            .unwrap();

        let (_, link_a) = store
            .add_todo_link(
                todo.id,
                Some("claude"),
                Some("ses_a"),
                Some(101),
                Some(project_id),
                "Impl",
            )
            .unwrap()
            .unwrap();
        let (_, _link_b) = store
            .add_todo_link(
                todo.id,
                Some("codex"),
                Some("ses_b"),
                Some(102),
                Some(project_id),
                "Review",
            )
            .unwrap()
            .unwrap();

        // Mark BOTH pane 101 and pane 102 awaiting review, then clear pane 102:
        // pane 102's review flag clears while pane 101 stays awaiting_review!
        store.mark_pane_todos_review(101).unwrap();
        store.mark_pane_todos_review(102).unwrap();
        store.clear_pane_todos_review(102).unwrap();
        let loaded = store.todo(todo.id).unwrap().unwrap();
        assert!(loaded.awaiting_review);
        assert_eq!(loaded.links.len(), 2);
        let link_101 = loaded
            .links
            .iter()
            .find(|l| l.pane_id == Some(101))
            .unwrap();
        let link_102 = loaded
            .links
            .iter()
            .find(|l| l.pane_id == Some(102))
            .unwrap();
        assert!(link_101.awaiting_review);
        assert!(!link_102.awaiting_review);

        // Detaching pane 101 preserves history session, and re-adding same session idempotently reuses the link.
        store.detach_pane_links(101).unwrap();
        let after_detach = store.todo(todo.id).unwrap().unwrap();
        let detached_a = after_detach
            .links
            .iter()
            .find(|l| l.id == link_a.id)
            .unwrap();
        assert_eq!(detached_a.pane_id, None);
        assert_eq!(detached_a.agent_session_id.as_deref(), Some("ses_a"));

        let (_, readded) = store
            .add_todo_link(
                todo.id,
                Some("claude"),
                Some("ses_a"),
                Some(103),
                Some(project_id),
                "Impl resumed",
            )
            .unwrap()
            .unwrap();
        assert_eq!(readded.id, link_a.id);
        assert_eq!(readded.pane_id, Some(103));
        assert_eq!(store.todo(todo.id).unwrap().unwrap().links.len(), 2);
    }

    #[test]
    fn update_todo_review_scopes_to_caller_pane_and_clear_todo_pane_review_is_isolated() {
        let store = Store::in_memory().unwrap();
        let project_id = store.insert_project("kumu", "/tmp/kumu").unwrap();
        let todo_a = store
            .insert_todo(
                project_id,
                "Task A",
                "",
                TodoStatus::InProgress,
                None,
                None,
                None,
            )
            .unwrap();
        let todo_b = store
            .insert_todo(
                project_id,
                "Task B",
                "",
                TodoStatus::InProgress,
                None,
                None,
                None,
            )
            .unwrap();

        store
            .add_todo_link(
                todo_a.id,
                Some("claude"),
                Some("ses_a"),
                Some(101),
                Some(project_id),
                "A",
            )
            .unwrap();
        store
            .add_todo_link(
                todo_a.id,
                Some("codex"),
                Some("ses_b"),
                Some(102),
                Some(project_id),
                "B",
            )
            .unwrap();
        store
            .add_todo_link(
                todo_b.id,
                Some("claude"),
                Some("ses_a"),
                Some(101),
                Some(project_id),
                "A",
            )
            .unwrap();

        // Caller pane 101 runs `kumu todo review <todo_a>`: only pane 101's link becomes awaiting_review.
        let updated = store
            .update_todo(
                todo_a.id,
                &TodoPatch {
                    status: Some(TodoStatus::InProgress),
                    awaiting_review: Some(true),
                    review_pane_id: Some(101),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        let a_101 = updated
            .links
            .iter()
            .find(|l| l.pane_id == Some(101))
            .unwrap();
        let a_102 = updated
            .links
            .iter()
            .find(|l| l.pane_id == Some(102))
            .unwrap();
        assert!(a_101.awaiting_review);
        assert!(!a_102.awaiting_review);

        // Mark todo_b on pane 101 awaiting review too, then clear ONLY (todo_a, 101) (SendFollowup path).
        store.mark_pane_todos_review(101).unwrap();
        store.clear_todo_pane_review(todo_a.id, 101).unwrap();
        let after_a = store.todo(todo_a.id).unwrap().unwrap();
        let after_b = store.todo(todo_b.id).unwrap().unwrap();
        assert!(!after_a.awaiting_review);
        assert!(after_b.awaiting_review);
    }

    #[test]
    fn link_merge_preserves_state_and_reconcile_detaches_stale_panes() {
        let store = Store::in_memory().unwrap();
        let project_id = store.insert_project("kumu", "/tmp/kumu").unwrap();
        let todo = store
            .insert_todo(
                project_id,
                "Merge & reconcile",
                "",
                TodoStatus::InProgress,
                None,
                None,
                None,
            )
            .unwrap();

        // Create historical session link + separate live pane link awaiting review, then sync session to trigger merge.
        store
            .add_todo_link(todo.id, Some("claude"), Some("ses_hist"), None, None, "")
            .unwrap();
        store
            .add_todo_link(
                todo.id,
                Some("claude"),
                None,
                Some(201),
                Some(project_id),
                "Active work",
            )
            .unwrap();
        store.mark_pane_todos_review(201).unwrap();
        store
            .sync_pane_link_session(201, "claude", "ses_hist")
            .unwrap();

        let merged = store.todo(todo.id).unwrap().unwrap();
        assert_eq!(merged.links.len(), 1);
        assert_eq!(merged.links[0].pane_id, Some(201));
        assert_eq!(merged.links[0].project_id, Some(project_id));
        assert_eq!(merged.links[0].label, "Active work");
        assert!(merged.links[0].awaiting_review);

        // Batched pane summaries returns open todos for pane 201.
        let summaries = store.open_todo_summaries_for_panes(&[201, 999]).unwrap();
        assert_eq!(
            summaries.get(&201),
            Some(&vec![(todo.id, "Merge & reconcile".to_string())])
        );
        assert!(!summaries.contains_key(&999));

        // Reconciling with pane 201 absent detaches it while preserving session_id for resume.
        let boards = store.reconcile_live_panes(&[300]).unwrap();
        assert_eq!(boards, vec![project_id]);
        let reconciled = store.todo(todo.id).unwrap().unwrap();
        assert_eq!(reconciled.links[0].pane_id, None);
        assert_eq!(
            reconciled.links[0].agent_session_id.as_deref(),
            Some("ses_hist")
        );
        assert!(!reconciled.links[0].awaiting_review);
    }

    #[test]
    fn removing_migrated_link_clears_legacy_awaiting_review() {
        let dir = std::env::temp_dir().join(format!(
            "kumu-v7-legacy-clear-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_file = dir.join("kumu.db");
        {
            let conn = Connection::open(&db_file).unwrap();
            conn.execute_batch(
                "CREATE TABLE projects (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    path TEXT NOT NULL,
                    parent_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE workspace_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    layout_json TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE todos (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                    title TEXT NOT NULL,
                    description TEXT NOT NULL DEFAULT '',
                    status TEXT NOT NULL DEFAULT 'todo',
                    position INTEGER NOT NULL DEFAULT 0,
                    pane_id INTEGER,
                    agent_id TEXT,
                    agent_session_id TEXT,
                    awaiting_review INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                    completed_at TEXT
                );
                PRAGMA user_version = 7;
                INSERT INTO projects (id, name, path) VALUES (1, 'kumu', '/tmp/kumu');
                INSERT INTO todos (id, project_id, title, status, pane_id, agent_id, awaiting_review)
                VALUES (10, 1, 'Migrated review', 'in_progress', 42, 'claude', 1);",
            )
            .unwrap();
        }
        let store = Store::open(&db_file).unwrap();
        let before = store.todo(10).unwrap().unwrap();
        assert!(before.awaiting_review);
        let link_id = before.links[0].id;
        store.remove_todo_link(link_id).unwrap();
        let after = store.todo(10).unwrap().unwrap();
        assert!(after.links.is_empty());
        assert!(!after.awaiting_review);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
