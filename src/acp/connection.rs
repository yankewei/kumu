//! The UI-side handle to one live ACP connection, and the thread that runs it.
//!
//! Dropping the last [`Connection`] clone is the teardown signal: the command
//! channel closes, the session loop ends, and the agent subprocess is
//! reclaimed. This is also why the permission table lives here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};

use super::config::Config;
use super::event::{Cmd, Event};
use super::session;

/// The table of pending permission requests awaiting a user decision, keyed by
/// request id.
///
/// The senders are handed to the UI so it can answer without going through
/// [`crate::acp::Cmd`]: the ACP thread is parked on the prompt that is waiting
/// for this very answer, so routing the reply through the command channel would
/// deadlock.
///
/// Only [`Connection`] holds this table strongly; the ACP thread keeps a
/// [`WeakPermissions`]. That way dropping the last UI-side handle frees the
/// table, which drops every pending sender and makes the waiting tasks answer
/// `Cancelled` — the only thing that can unblock a prompt whose agent is sitting
/// on a permission request.
type PermissionTable = Mutex<HashMap<String, tokio::sync::oneshot::Sender<Option<String>>>>;
type SharedPermissions = Arc<PermissionTable>;
pub(super) type WeakPermissions = Weak<PermissionTable>;

/// Distinguishes connections so late events and teardown from a replaced one
/// cannot touch the session's current connection.
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

/// The UI-side handle to one live ACP connection: send commands, and answer
/// permission requests.
///
/// Dropping the last clone tears the connection down: the command channel
/// closes and any outstanding permission prompt is abandoned.
#[derive(Clone)]
pub struct Connection {
    /// Stable per-connection identity, so the UI can tell whether a bridge task
    /// still belongs to the session's current connection.
    id: u64,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
    permissions: SharedPermissions,
}

impl Connection {
    /// This connection's identity, compared against the one registered for a
    /// session to discard stale events and teardown.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Queue a command; non-blocking.
    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Answer a pending permission request. `option_id` of `None` responds with
    /// the protocol's `Cancelled` outcome. A no-op for unknown or already
    /// answered requests.
    pub fn resolve_permission(&self, request_id: &str, option_id: Option<String>) {
        if let Some(tx) = self.permissions.lock().unwrap().remove(request_id) {
            // The waiting task may already be gone; the reply is then moot.
            let _ = tx.send(option_id);
        }
    }

    /// A handle attached to no thread, for tests that only exercise identity.
    #[cfg(test)]
    pub(crate) fn detached_for_test(id: u64) -> Self {
        Self {
            id,
            cmd_tx: tokio::sync::mpsc::unbounded_channel().0,
            permissions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// Spawn the ACP thread and return the UI-side connection handle.
pub fn spawn(cfg: Config, cwd: PathBuf, ev_tx: Sender<Event>) -> Connection {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
    // Shared so the UI can answer permission requests without the command queue.
    // The thread only borrows it, so dropping the handle can still free it.
    let permissions: SharedPermissions = Arc::new(Mutex::new(HashMap::new()));
    let thread_permissions = Arc::downgrade(&permissions);
    std::thread::Builder::new()
        .name("kumu-acp".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            if let Err(err) = rt.block_on(session::run(
                cfg,
                cwd,
                ev_tx.clone(),
                cmd_rx,
                thread_permissions,
            )) {
                super::event::emit(&ev_tx, Event::Error(err.to_string()));
            }
        })
        .expect("failed to spawn acp thread");
    Connection {
        id: NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed),
        cmd_tx,
        permissions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The UI answers through this handle, so it must deliver to the waiting
    /// task and be a no-op for ids that are unknown or already answered.
    #[test]
    fn resolve_permission_delivers_once() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
        let permissions: SharedPermissions = Arc::new(Mutex::new(HashMap::new()));
        let conn = Connection {
            id: 1,
            cmd_tx,
            permissions: permissions.clone(),
        };

        // Unknown id: nothing to send to, and it must not panic.
        conn.resolve_permission("perm-missing", Some("allow-once".into()));

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        permissions.lock().unwrap().insert("perm-1".into(), tx);
        conn.resolve_permission("perm-1", Some("allow-once".into()));
        assert_eq!(
            rx.try_recv().expect("decision not delivered"),
            Some("allow-once".to_string())
        );

        // Answering the same request again is a no-op rather than a second send.
        conn.resolve_permission("perm-1", None);
        assert!(rx.try_recv().is_err());
    }

    /// Dropping the last UI handle must free the table, which drops the pending
    /// senders and so unblocks a task waiting on a user decision. This is what
    /// keeps a prompt from deadlocking the connection when the window closes.
    #[test]
    fn dropping_connection_releases_pending_decisions() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
        let permissions: SharedPermissions = Arc::new(Mutex::new(HashMap::new()));
        // The ACP thread only borrows the table.
        let thread_view = Arc::downgrade(&permissions);

        let conn = Connection {
            id: 1,
            cmd_tx,
            permissions: permissions.clone(),
        };
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        conn.permissions.lock().unwrap().insert("perm-1".into(), tx);

        // From here the handle is the only strong owner, as in the real app.
        drop(permissions);
        assert!(thread_view.upgrade().is_some());

        drop(conn);
        // The waiting task observes the sender being dropped...
        assert!(
            rx.try_recv().is_err(),
            "sender should be gone once the handle is dropped"
        );
        // ...and the thread stops holding the table at all.
        assert!(
            thread_view.upgrade().is_none(),
            "the table must be freed once the last handle is dropped"
        );
    }
}
