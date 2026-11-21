//! Thread-safe bridge between NSH Pure-Rust views (engine thread) and Kumu's
//! shared [`WorkspaceCore`] + GPUI host window (main thread).

use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::app::{WorkspaceCore, WorkspaceEvent};
use crate::db::Project;

/// Actions an NSH view requests from the GPUI host window.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum HostAction {
    ToggleThemeMode,
    FocusPane(u64),
}

/// Shared handle passed into NSH views via [`nsh_platform_gpui::AppOptions::context`]
/// or process-scoped state for clean engine-thread ↔ host-thread coordination.
#[derive(Clone)]
pub struct NshBridge {
    core: Arc<Mutex<WorkspaceCore>>,
    actions: UnboundedSender<HostAction>,
}

impl NshBridge {
    pub fn new(core: Arc<Mutex<WorkspaceCore>>) -> (Self, UnboundedReceiver<HostAction>) {
        let (actions, rx) = futures::channel::mpsc::unbounded();
        (Self { core, actions }, rx)
    }

    /// Snapshot of projects currently registered in `WorkspaceCore`.
    pub fn projects(&self) -> Vec<Project> {
        self.core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .projects()
            .to_vec()
    }

    /// Subscribe to pure-Rust [`WorkspaceEvent`] updates from `WorkspaceCore`.
    #[allow(dead_code)]
    pub fn subscribe(&self) -> UnboundedReceiver<WorkspaceEvent> {
        self.core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subscribe()
    }

    /// Send an action to the GPUI host window.
    pub fn send(&self, action: HostAction) {
        let _ = self.actions.unbounded_send(action);
    }
}
