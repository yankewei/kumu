//! Pure-Rust pane header & find-bar snapshots derived from a terminal's live
//! [`Snapshot`], rendered by the unified Pure Rust NSH home shell (`shell_nsh.rs`).

use crate::agent::AgentState;
use crate::app::layout::PaneId;
use crate::term::Snapshot;

/// Immutable state of a pane's floating `⌘F` find bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FindBarSnapshot {
    pub(crate) needle: String,
    pub(crate) count_label: String,
    pub(crate) has_matches: bool,
    /// Monotonically increases each time `⌘F` explicitly requests input focus.
    pub(crate) focus_nonce: u64,
}

/// Immutable state of one terminal pane's header + optional find bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaneChromeSnapshot {
    pub(crate) id: PaneId,
    pub(crate) name: String,
    pub(crate) cwd: String,
    pub(crate) note: Option<String>,
    pub(crate) agent_status: Option<AgentState>,
    pub(crate) active: bool,
    pub(crate) can_close: bool,
    pub(crate) find: Option<FindBarSnapshot>,
}

/// The pane header's label: what the program calls itself, or a placeholder
/// before anything has run.
pub(crate) fn header_name(snapshot: &Snapshot) -> String {
    match snapshot.label() {
        label if label.is_empty() => "终端".into(),
        label => label,
    }
}

pub(crate) fn header_note(snapshot: &Snapshot) -> Option<String> {
    if let Some(error) = &snapshot.spawn_error {
        return Some(format!("启动失败：{error}"));
    }
    if snapshot.exited && !snapshot.command.is_empty() {
        return Some("已退出".into());
    }
    if snapshot.command.is_empty() {
        return Some("还没有终端".into());
    }
    None
}

pub(crate) fn short_cwd(cwd: &str) -> String {
    static HOME: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| {
        std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
            .map(std::path::PathBuf::from)
    });
    if let Some(home) = home.as_ref()
        && let Ok(stripped) = std::path::Path::new(cwd).strip_prefix(home)
    {
        let rest = stripped.to_string_lossy();
        if rest.is_empty() {
            return "~".into();
        }
        return format!("~/{rest}");
    }
    cwd.to_string()
}

/// Format the match-count badge shown inside the floating find bar.
pub(crate) fn find_bar_snapshot(
    snapshot: &Snapshot,
    sent: &str,
    focus_nonce: u64,
) -> FindBarSnapshot {
    let search = snapshot.search.as_ref();
    let current = search.is_some_and(|s| s.needle == sent);
    let total = search.filter(|_| current).map_or(0, |s| s.total);
    let count_label = if sent.is_empty() {
        String::new()
    } else if !current {
        "…".to_string()
    } else if total == 0 {
        "无结果".to_string()
    } else {
        match search.and_then(|s| s.selected) {
            Some(index) => format!("{}/{}", index + 1, total),
            None => format!("0/{total}"),
        }
    };
    FindBarSnapshot {
        needle: sent.to_string(),
        count_label,
        has_matches: current && total > 0,
        focus_nonce,
    }
}
