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
    /// Opaque backend cursor used to reopen the agent's persisted conversation.
    pub resume_id: Option<String>,
    /// Selected model (config-option value); None = agent default.
    pub model: Option<String>,
    /// Selected session mode (config-option value); None = agent default.
    pub mode: Option<String>,
    /// Selected sandbox policy (value); None = whatever the agent's own
    /// configuration says, which is also what an agent without the concept
    /// keeps here.
    pub sandbox: Option<String>,
    /// Selected reasoning effort (value); None = the model's own default. Only
    /// agents whose models advertise efforts keep anything here.
    pub reasoning: Option<String>,
    /// Whether the title is the user's own, set by renaming the session. A name
    /// the user typed outranks one generated for them, so this is what stops a
    /// generated title (kumu's fallback or the agent's own) from replacing it.
    pub title_manual: bool,
    /// Whether the conversation has begun, which is what fixes the session's
    /// agent: a switched agent would be handed a transcript it never wrote, so
    /// switching is only offered while this is false.
    ///
    /// Counts every message except kumu's own notices (`system`). Excluding
    /// those is deliberate — a backend error is a `system` row, and a session
    /// whose only content is one of those has no conversation to preserve, so
    /// counting them would lock a session to whichever agent happened to fail
    /// first.
    pub has_conversation: bool,
}

/// A chat message stored in a session.
#[derive(Clone, Debug)]
pub struct Message {
    pub role: String, // "user" | "agent" | "system" | "thinking" | "tool"
    pub content: String,
    /// How long the turn this message closed ran, when it closed one.
    ///
    /// The total belongs to the turn, not to the message, and the schema has no
    /// turn of its own — but the message that ended one is exactly where the
    /// settled total is anchored, so it is recorded here rather than in a table
    /// nothing else would read.
    pub duration_ms: Option<i64>,
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
                model TEXT,
                mode TEXT,
                sandbox TEXT,
                reasoning TEXT,
                title_manual INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                duration_ms INTEGER,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            -- Every read of a session's history, and the `has_conversation`
            -- probe in `sessions()`, filters on `session_id`; without this each
            -- one is a full scan of a table that only grows.
            CREATE INDEX IF NOT EXISTS idx_messages_session_id
                ON messages(session_id);",
        )?;
        // Migrations for dbs created before model/mode/sandbox/reasoning/
        // title_manual/duration_ms were part of CREATE TABLE (duplicate-column
        // errors on fresh dbs are expected and ignored).
        self.conn
            .execute("ALTER TABLE sessions ADD COLUMN model TEXT", [])
            .ok();
        self.conn
            .execute("ALTER TABLE sessions ADD COLUMN mode TEXT", [])
            .ok();
        self.conn
            .execute("ALTER TABLE sessions ADD COLUMN sandbox TEXT", [])
            .ok();
        self.conn
            .execute("ALTER TABLE sessions ADD COLUMN reasoning TEXT", [])
            .ok();
        self.conn
            .execute(
                "ALTER TABLE sessions ADD COLUMN title_manual INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .ok();
        self.conn
            .execute("ALTER TABLE messages ADD COLUMN duration_ms INTEGER", [])
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
                "SELECT id, project_id, agent, title, cwd, model, mode, sandbox, reasoning,
                        title_manual, resume_id,
                        EXISTS(
                            SELECT 1 FROM messages
                            WHERE messages.session_id = sessions.id AND messages.role <> 'system'
                        )
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
                model: row.get(5)?,
                mode: row.get(6)?,
                sandbox: row.get(7)?,
                reasoning: row.get(8)?,
                title_manual: row.get(9)?,
                resume_id: row.get(10)?,
                has_conversation: row.get(11)?,
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

    /// Delete a session and all of its messages.
    ///
    /// The messages FK cascade does not fire here (SQLite's `foreign_keys`
    /// pragma is off), so the rows are removed explicitly, in one transaction.
    pub fn delete_session(&self, id: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM messages WHERE session_id = ?1", params![id])?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        tx.commit()
    }

    pub fn update_session_agent_model(
        &self,
        id: i64,
        agent: &str,
        model: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET resume_id = CASE WHEN agent = ?1 THEN resume_id ELSE NULL END,
             agent = ?1, model = ?2, updated_at = datetime('now') WHERE id = ?3",
            params![agent, model, id],
        )?;
        Ok(())
    }

    /// Keep the backend's restore cursor independently of model selections.
    pub fn update_session_resume_id(&self, id: i64, resume_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET resume_id = ?1 WHERE id = ?2",
            params![resume_id, id],
        )?;
        Ok(())
    }

    /// Remember the session's mode, or clear it (`None`) when it no longer
    /// applies — mode ids are agent-specific, so switching agents invalidates
    /// the stored one.
    pub fn update_session_mode(&self, id: i64, mode: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET mode = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![mode, id],
        )?;
        Ok(())
    }

    /// Remember the session's sandbox policy, or clear it (`None`) when it no
    /// longer applies — the values are Codex's own vocabulary, so switching to
    /// an agent without the concept invalidates the stored one.
    pub fn update_session_sandbox(&self, id: i64, sandbox: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET sandbox = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![sandbox, id],
        )?;
        Ok(())
    }

    /// Remember the session's reasoning effort, or clear it (`None`) when it no
    /// longer applies — the set is advertised per model, so switching the model
    /// (or the agent) invalidates a stored one.
    pub fn update_session_reasoning(&self, id: i64, reasoning: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET reasoning = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![reasoning, id],
        )?;
        Ok(())
    }

    /// Store a generated title — kumu's fallback from the first message, or the
    /// agent's own — and report whether it was applied.
    ///
    /// A session the user has renamed refuses it: [`Store::rename_session`] makes
    /// the title the user's, and a generated one must not overwrite a name they
    /// typed out. Callers mirror this verdict rather than deciding for
    /// themselves, so the rule has one home.
    pub fn update_session_title(&self, id: i64, title: &str) -> Result<bool> {
        let applied = self.conn.execute(
            "UPDATE sessions SET title = ?1, updated_at = datetime('now')
             WHERE id = ?2 AND title_manual = 0",
            params![title, id],
        )?;
        Ok(applied > 0)
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

    pub fn messages(&self, session_id: i64) -> Vec<Message> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT role, content, duration_ms FROM messages WHERE session_id = ?1 ORDER BY id",
            )
            .unwrap();
        stmt.query_map(params![session_id], |row| {
            Ok(Message {
                role: row.get(0)?,
                content: row.get(1)?,
                duration_ms: row.get(2)?,
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

    /// Record how long the turn this message closed ran.
    ///
    /// Apart from [`Store::insert_message`] because only the caller knows which
    /// message ended the turn, and the total belongs to that row rather than to
    /// whichever role the turn happened to finish on.
    pub fn set_message_duration(&self, id: i64, duration_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET duration_ms = ?1 WHERE id = ?2",
            params![duration_ms, id],
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

    #[test]
    fn old_sessions_gain_restore_cursors_without_losing_history() {
        let store = Store {
            conn: Connection::open_in_memory().unwrap(),
        };
        store
            .conn
            .execute_batch(
                "CREATE TABLE sessions (
                id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
                agent TEXT NOT NULL, title TEXT NOT NULL, cwd TEXT NOT NULL,
                created_at TEXT, updated_at TEXT
            );
            INSERT INTO sessions VALUES (1, 1, 'omp', '旧会话', '/tmp', NULL, NULL);",
            )
            .unwrap();
        store.init().unwrap();
        store.insert_message(1, "user", "旧消息").unwrap();
        store.init().unwrap();

        let session = &store.sessions()[0];
        assert_eq!(session.title, "旧会话");
        assert!(session.resume_id.is_none());
        assert!(session.has_conversation);
        assert_eq!(store.messages(1)[0].content, "旧消息");

        store
            .update_session_resume_id(1, "backend-session")
            .unwrap();
        store.init().unwrap();
        assert_eq!(
            store.sessions()[0].resume_id.as_deref(),
            Some("backend-session")
        );
    }

    #[test]
    fn model_changes_keep_restore_cursor_but_agent_changes_clear_it() {
        let store = store();
        let project = store.insert_project("test", "/tmp").unwrap();
        let id = store
            .insert_session(project, "omp", "test", "/tmp")
            .unwrap();
        store.update_session_resume_id(id, "omp-session").unwrap();
        store
            .update_session_agent_model(id, "omp", Some("another-model"))
            .unwrap();
        assert_eq!(
            store.sessions()[0].resume_id.as_deref(),
            Some("omp-session")
        );
        store.update_session_agent_model(id, "codex", None).unwrap();
        assert!(store.sessions()[0].resume_id.is_none());
    }

    /// Deleting a session drops its history with it, and touches nothing else:
    /// the cascade on `messages` never fires (the pragma is off), so this is
    /// the only thing keeping orphan rows out of the db.
    #[test]
    fn deleting_a_session_removes_only_its_messages() {
        let store = store();
        let project = store.insert_project("kumu", "/tmp").unwrap();
        let doomed = store
            .insert_session(project, "omp", "会话 1", "/tmp")
            .unwrap();
        let kept = store
            .insert_session(project, "omp", "会话 2", "/tmp")
            .unwrap();
        store.insert_message(doomed, "user", "删除我").unwrap();
        store.insert_message(kept, "user", "留下我").unwrap();

        store.delete_session(doomed).unwrap();

        let sessions = store.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, kept);
        assert!(store.messages(doomed).is_empty());
        assert_eq!(store.messages(kept).len(), 1);
        assert_eq!(store.messages(kept)[0].content, "留下我");
    }

    /// Deleting an unknown id is a no-op rather than an error, so a stale
    /// menu action can never break the db.
    #[test]
    fn deleting_an_unknown_session_is_harmless() {
        let store = store();
        store.delete_session(404).unwrap();
        assert!(store.sessions().is_empty());
    }

    /// A name the user typed outranks one generated for them: renaming marks the
    /// title as the user's, and every later generated title — kumu's fallback
    /// from the first message and the agent's own both come through here — is
    /// refused, leaving the session as the user named it.
    #[test]
    fn a_renamed_session_keeps_the_users_title() {
        let store = store();
        let project = store.insert_project("kumu", "/tmp").unwrap();
        let session = store
            .insert_session(project, "omp", "会话 1", "/tmp")
            .unwrap();

        assert!(
            store.update_session_title(session, "首条消息").unwrap(),
            "a session with no name of its own takes a generated title"
        );
        assert_eq!(store.sessions()[0].title, "首条消息");
        assert!(!store.sessions()[0].title_manual);

        store.rename_session(session, "重构 backend").unwrap();
        assert_eq!(store.sessions()[0].title, "重构 backend");
        assert!(store.sessions()[0].title_manual);

        assert!(
            !store
                .update_session_title(session, "agent 起的名字")
                .unwrap(),
            "a generated title is refused once the user has renamed the session"
        );
        assert_eq!(store.sessions()[0].title, "重构 backend");
    }

    /// The chosen mode is remembered across reads, and clearing it is how a
    /// session falls back to the agent's default — mode ids are agent-specific,
    /// so switching agents has to be able to drop a stale one.
    #[test]
    fn session_mode_round_trips_and_clears() {
        let store = store();
        let project = store.insert_project("kumu", "/tmp").unwrap();
        let session = store
            .insert_session(project, "omp", "会话 1", "/tmp")
            .unwrap();
        assert_eq!(
            store.sessions()[0].mode,
            None,
            "a new session stores no mode"
        );

        store.update_session_mode(session, Some("ask")).unwrap();
        assert_eq!(store.sessions()[0].mode.as_deref(), Some("ask"));

        store.update_session_mode(session, None).unwrap();
        assert_eq!(store.sessions()[0].mode, None);
    }

    /// The chosen sandbox policy is remembered across reads, and clearing it is
    /// how a session goes back to whatever the agent's own configuration says.
    #[test]
    fn session_sandbox_round_trips_and_clears() {
        let store = store();
        let project = store.insert_project("kumu", "/tmp").unwrap();
        let session = store
            .insert_session(project, "codex", "会话 1", "/tmp")
            .unwrap();
        assert_eq!(
            store.sessions()[0].sandbox,
            None,
            "a new session overrides nothing"
        );

        store
            .update_session_sandbox(session, Some("read-only"))
            .unwrap();
        assert_eq!(store.sessions()[0].sandbox.as_deref(), Some("read-only"));

        store.update_session_sandbox(session, None).unwrap();
        assert_eq!(store.sessions()[0].sandbox, None);
    }

    /// The chosen reasoning effort round-trips the same way, and clearing it is
    /// how a session falls back to the model's own default — the effort set is
    /// per model, so a model switch has to be able to drop a stale one.
    #[test]
    fn session_reasoning_round_trips_and_clears() {
        let store = store();
        let project = store.insert_project("kumu", "/tmp").unwrap();
        let session = store
            .insert_session(project, "codex", "会话 1", "/tmp")
            .unwrap();
        assert_eq!(
            store.sessions()[0].reasoning,
            None,
            "a new session overrides nothing"
        );

        store
            .update_session_reasoning(session, Some("xhigh"))
            .unwrap();
        assert_eq!(store.sessions()[0].reasoning.as_deref(), Some("xhigh"));

        store.update_session_reasoning(session, None).unwrap();
        assert_eq!(store.sessions()[0].reasoning, None);
    }

    /// `has_conversation` is what fixes a session's agent, so it must stay false
    /// for a session whose only rows are kumu's own notices — otherwise a
    /// backend error would lock a session to whichever agent failed first — and
    /// it must be per-session, since the query behind it is correlated.
    #[test]
    fn has_conversation_ignores_kumus_own_notices() {
        let store = store();
        let project = store.insert_project("kumu", "/tmp").unwrap();
        let session = store
            .insert_session(project, "claude", "会话 1", "/tmp")
            .unwrap();
        let other = store
            .insert_session(project, "omp", "会话 2", "/tmp")
            .unwrap();
        assert!(
            !store.sessions()[0].has_conversation,
            "a fresh session is still open to an agent switch"
        );

        store
            .insert_message(session, "system", "Claude Code 会话尚未实现")
            .unwrap();
        assert!(
            !store.sessions()[0].has_conversation,
            "a notice is not a conversation, so the agent stays changeable"
        );

        store.insert_message(other, "user", "你好").unwrap();
        let sessions = store.sessions();
        assert!(
            sessions[1].has_conversation,
            "the session that was prompted has a conversation"
        );
        assert!(
            !sessions[0].has_conversation,
            "and the one that was not still has none"
        );
    }

    /// Both `messages()` and the `has_conversation` probe filter on
    /// `session_id`, on a table that only grows, so the index must exist — and
    /// `init` runs on every open, so re-opening an old database must leave
    /// exactly one.
    #[test]
    fn messages_are_indexed_by_session() {
        let store = store();
        store.init().unwrap();
        let count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_messages_session_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "exactly one session_id index, however often init runs"
        );
    }

    /// A turn's total is recorded on the message that closed it and comes back
    /// with the history, which is what lets a settled row be rebuilt after a
    /// restart. Every other message reads `None` rather than a zero, so "no
    /// turn closed here" stays distinguishable from "a turn took no time".
    #[test]
    fn a_closing_message_round_trips_its_turn_duration() {
        let store = store();
        let project = store.insert_project("kumu", "/tmp").unwrap();
        let session = store
            .insert_session(project, "omp", "会话 1", "/tmp")
            .unwrap();
        store.insert_message(session, "user", "你好").unwrap();
        let closing = store.insert_message(session, "agent", "在").unwrap();
        store.set_message_duration(closing, 65_000).unwrap();

        let messages = store.messages(session);
        assert_eq!(
            messages[0].duration_ms, None,
            "the prompt carries no total of its own"
        );
        assert_eq!(messages[1].duration_ms, Some(65_000));
    }

    /// A database written before the column existed gains it on open: the
    /// upgrade path is the same `init` every open runs, so an in-place upgrade
    /// must not be the case that quietly loses the total.
    #[test]
    fn an_older_messages_table_gains_the_duration_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )
        .unwrap();
        let store = Store { conn };
        store.init().unwrap();

        let columns: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('messages') WHERE name = 'duration_ms'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(columns, 1, "the migration adds the column");
    }
}
