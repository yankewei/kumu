use std::collections::HashSet;
use std::path::PathBuf;

use futures::channel::mpsc::unbounded;
use nsh_core::element::Root;
use nsh_engine::EngineConfig;

use super::{
    ArtifactFilter, ArtifactGroup, ArtifactRow, ArtifactsAction, ArtifactsMount, ArtifactsSnapshot,
    group_artifacts, register_mount, view,
};
use crate::agent::AgentState;
use crate::artifacts::{Artifact, ArtifactKind, ArtifactMeta, SessionInfo};
use crate::plugins::PaneTarget;

fn artifact(id: &str, session: SessionInfo, updated_at: &str, title: &str) -> Artifact {
    Artifact {
        meta: ArtifactMeta {
            version: 1,
            id: id.into(),
            title: title.into(),
            kind: ArtifactKind::Markdown,
            entry: "index.md".into(),
            created_at: updated_at.into(),
            updated_at: updated_at.into(),
            source: None,
            tags: Vec::new(),
            session,
        },
        dir: PathBuf::from("/tmp").join(id),
    }
}

fn pane(id: u64, session: &str, title: &str, focused: bool) -> PaneTarget {
    PaneTarget {
        pane: id,
        ordinal: id as usize,
        tab_name: "主工作区".into(),
        title: title.into(),
        agent: Some("claude".into()),
        agent_session_id: Some(session.into()),
        state: Some(AgentState::Running),
        focused,
    }
}

fn session(id: &str, title: &str) -> SessionInfo {
    SessionInfo::new(
        Some(7),
        Some("claude".into()),
        Some(id.into()),
        Some(title.into()),
        Some("主工作区".into()),
        None,
    )
}

#[test]
fn groups_order_current_then_live_then_ended_then_unattached() {
    let artifacts = vec![
        artifact(
            "a",
            session("ended", "已结束"),
            "2026-01-01T00:00:00Z",
            "ended",
        ),
        artifact(
            "b",
            SessionInfo::unattached(),
            "2026-01-02T00:00:00Z",
            "unattached",
        ),
        artifact(
            "c",
            session("current", "当前"),
            "2025-12-31T00:00:00Z",
            "current",
        ),
        artifact(
            "d",
            session("other", "其它"),
            "2026-01-03T00:00:00Z",
            "other",
        ),
    ];
    let panes = vec![
        pane(7, "current", "当前", true),
        pane(8, "other", "其它", false),
    ];
    let live: HashSet<String> = panes
        .iter()
        .map(|pane| {
            crate::artifacts::session_key(
                Some(pane.pane),
                pane.agent.as_deref(),
                pane.agent_session_id.as_deref(),
            )
        })
        .collect();
    let groups = group_artifacts(
        &artifacts,
        &panes,
        Some("agent-session:current"),
        &live,
        ArtifactFilter::All,
        "",
    );
    let titles: Vec<_> = groups.iter().map(|group| group.title.as_str()).collect();
    assert_eq!(titles, ["当前", "其它", "已结束", "未关联"]);
    assert!(groups[0].current);
    assert!(groups[2].ended);
}

#[test]
fn current_session_filter_and_search_narrow_the_list() {
    let artifacts = vec![
        artifact(
            "a",
            session("current", "当前"),
            "2026-01-01T00:00:00Z",
            "登录页方案",
        ),
        artifact(
            "b",
            session("other", "其它"),
            "2026-01-02T00:00:00Z",
            "支付流程",
        ),
    ];
    let panes = vec![pane(7, "current", "当前", true)];
    let live: HashSet<String> = ["agent-session:current".to_string()].into_iter().collect();
    let groups = group_artifacts(
        &artifacts,
        &panes,
        Some("agent-session:current"),
        &live,
        ArtifactFilter::CurrentSession,
        "",
    );
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].rows[0].title, "登录页方案");

    // No focused agent pane means no current session, not "everything".
    let groups = group_artifacts(
        &artifacts,
        &[],
        None,
        &live,
        ArtifactFilter::CurrentSession,
        "",
    );
    assert!(groups.is_empty());

    let groups = group_artifacts(
        &artifacts,
        &panes,
        Some("agent-session:current"),
        &live,
        ArtifactFilter::All,
        "支付",
    );
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].title, "其它");
}

#[test]
fn the_drawer_draws_a_group_and_opens_its_row() {
    let (events_tx, events_rx) = unbounded();
    let (actions_tx, mut actions_rx) = unbounded();
    let _ = events_tx;
    let initial = ArtifactsSnapshot {
        project_id: Some(1),
        loaded: true,
        error: None,
        filter: ArtifactFilter::All,
        total: 1,
        groups: vec![ArtifactGroup {
            key: "agent-session:current".into(),
            title: "登录页重构".into(),
            meta: "Claude Code · 主工作区 · 窗格 7".into(),
            current: true,
            ended: false,
            rank: 0,
            rows: vec![ArtifactRow {
                id: "20261128-153012-a1b2".into(),
                title: "登录页方案".into(),
                kind: ArtifactKind::Markdown,
                updated_at: "2026-11-28T07:30:12Z".into(),
                updated: "刚刚".into(),
                size: "1.2 KB".into(),
                entry: PathBuf::from("/tmp/project/.kumu/artifacts/a/index.md"),
                dir: PathBuf::from("/tmp/project/.kumu/artifacts/a"),
            }],
        }],
    };
    let mount_id = register_mount(ArtifactsMount {
        initial,
        events_rx,
        actions_tx,
    });
    let root = Root::new::<view::ArtifactsRoot>(move || view::ArtifactsRootProps {
        mount_id: mount_id.into(),
    });
    let mut app =
        nsh_testing::Headless::start(EngineConfig::new(root)).expect("start artifacts NSH");
    app.wait_for("登录页重构").expect("shows the session group");
    app.wait_for("登录页方案").expect("shows the artifact row");
    app.press("登录页方案").expect("press the artifact row");
    assert!(matches!(
        actions_rx.try_recv().ok(),
        Some(ArtifactsAction::Open(id)) if id == "20261128-153012-a1b2"
    ));
}
