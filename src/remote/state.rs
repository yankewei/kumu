//! The workspace facts the phone can see. The UI owns this snapshot; the
//! network layer only reads it, so a slow or disconnected phone never blocks
//! a terminal.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteState {
    pub computer: ComputerInfo,
    #[serde(default)]
    pub projects: Vec<ProjectInfo>,
    #[serde(default)]
    pub panes: Vec<PaneInfo>,
    #[serde(default)]
    pub agents: Vec<AgentInfo>,
    #[serde(default)]
    pub selected_project: Option<i64>,
    #[serde(default)]
    pub focused_pane: Option<u64>,
    #[serde(default)]
    pub recent_pane: Option<u64>,
    #[serde(default)]
    pub version: u64,
}

/// An agent the desktop can start in a new pane. `installed` is what makes
/// the phone hide a CLI that is not on the Mac.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    pub installed: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerInfo {
    pub name: String,
    pub id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    pub id: i64,
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub worktree: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaneStatus {
    Working,
    Waiting,
    #[default]
    Idle,
    Exited,
    Unknown,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneInfo {
    pub id: u64,
    pub project: i64,
    /// `shell` or `agent`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub title: String,
    pub cwd: String,
    pub status: PaneStatus,
    pub unread: bool,
    #[serde(default)]
    pub last_line: String,
    #[serde(default)]
    pub last_activity: u64,
    pub cols: u16,
    pub rows: u16,
}

impl RemoteState {
    pub fn bump(&mut self) {
        self.version = self.version.wrapping_add(1);
    }
}
