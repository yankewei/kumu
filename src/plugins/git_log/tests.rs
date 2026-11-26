use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::channel::mpsc::unbounded;
use gpui::{AppContext, Entity, TestAppContext};
use nsh_engine::EngineConfig;

use super::patch_view::PatchView;
use super::review::{
    PatchSheet, PatchSource, ReviewComment, ReviewRange, ReviewSide, ReviewStore, compose_comments,
    normalize_and_index_patch,
};
use super::{
    GitHostEvent, GitPanelAction, GitPanelMount, GitPanelSnapshot, PanelTab, PluginHost,
    change_rows_from, register_panel_mount, view,
};
use crate::plugins::{PaneTarget, SendOutcome};

fn test_host() -> PluginHost {
    PluginHost::with_panes(|_, _, _, _| Ok(SendOutcome::Pasted), |_, _| Vec::new())
}

fn empty_store() -> Rc<RefCell<ReviewStore>> {
    Rc::new(RefCell::new(ReviewStore::default()))
}

fn patch_view(
    cx: &mut TestAppContext,
    project: i64,
    source: PatchSource,
    message: &str,
    context: &str,
    host: PluginHost,
    store: Rc<RefCell<ReviewStore>>,
) -> Entity<PatchView> {
    let key = source.key(project);
    let message = message.to_string();
    let context = context.to_string();
    cx.new(|cx| {
        PatchView::new(
            PatchSheet {
                project,
                title: "test patch".into(),
                meta: "test meta".into(),
                key,
                source,
                message,
                context,
                initial_file: None,
                host,
                reviews: store,
                notify_panel: Rc::new(|_| {}),
            },
            cx,
        )
    })
}

fn git(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

fn fixture() -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "kumu-panel-{}-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["config", "tag.gpgsign", "false"]);
    std::fs::write(repo.join("a"), "a\n").unwrap();
    git(&repo, &["add", "a"]);
    git(&repo, &["commit", "-qm", "init"]);
    (dir, repo)
}

#[gpui::test]
fn the_diff_view_parses_the_commits_patch(cx: &mut TestAppContext) {
    let (dir, path) = fixture();
    let history = crate::git::history(&path).unwrap();
    let commit = history.commits[0].clone();

    let source = PatchSource::Commit {
        root: path,
        hash: commit.hash.clone(),
    };
    let view = patch_view(
        cx,
        1,
        source,
        &commit.body,
        "提交 abc1234",
        test_host(),
        empty_store(),
    );
    cx.run_until_parked();

    let (files, error, has_patch) = view.read_with(cx, |view, _| {
        (
            view.index.file_count,
            view.error.clone(),
            view.patch_text.is_some(),
        )
    });
    assert_eq!(error, None);
    assert_eq!(files, 1);
    assert!(has_patch);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[gpui::test]
fn the_diff_view_parses_combined_working_tree_changes(cx: &mut TestAppContext) {
    let (dir, path) = fixture();
    std::fs::write(path.join("a"), "a\nmodified\n").unwrap();
    std::fs::write(path.join("new.rs"), "fn hello() {}\n").unwrap();

    let source = PatchSource::WorkingTree {
        root: path,
        scope: crate::git::WorkingTreeDiffScope::All,
    };
    let view = patch_view(
        cx,
        1,
        source,
        "",
        "工作区未提交改动",
        test_host(),
        empty_store(),
    );
    cx.run_until_parked();

    let (files, paths, error) = view.read_with(cx, |view, _| {
        (
            view.index.file_count,
            view.index
                .files
                .iter()
                .map(|f| f.path.clone())
                .collect::<Vec<_>>(),
            view.error.clone(),
        )
    });
    assert_eq!(error, None);
    assert_eq!(files, 2);
    assert!(paths.contains(&"a".to_string()));
    assert!(paths.contains(&"new.rs".to_string()));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn comments_compose_as_one_instruction_with_their_locations() {
    let comment = |start, end| ReviewRange::new("src/git.rs", ReviewSide::Modified, start, end);
    let message = compose_comments(
        "提交 abc1234",
        &[
            (comment(120, 124), "这里应该先检查状态".into()),
            (comment(88, 88), "可以拆成两个函数".into()),
        ],
    );
    assert_eq!(
        message,
        "请根据以下代码审查意见修改（提交 abc1234）：\n\n\
         src/git.rs:120-124\n这里应该先检查状态\n\n\
         src/git.rs:88\n可以拆成两个函数"
    );
}

#[test]
fn a_range_across_sides_names_both_sides() {
    let range = ReviewRange::new("src/git.rs", ReviewSide::Original, 12, 14)
        .with_end_side(ReviewSide::Modified);
    let message = compose_comments("未暂存改动", &[(range, "这一段要一起看".into())]);
    assert_eq!(
        message,
        "请根据以下代码审查意见修改（未暂存改动）：\n\n\
         src/git.rs:12（改动前）-14（改动后）\n这一段要一起看"
    );
}

#[test]
fn truncated_patch_normalizes_and_maps_joined_lines_back_to_files() {
    let raw = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,2 +10,3 @@ fn a()
 line10
+added11
 line11
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -20,5 +20,5 @@ fn b()
 keep20
+added21
";
    let (normalized, index) = normalize_and_index_patch(raw, true);
    assert_eq!(index.file_count, 2);
    assert!(normalized.contains("@@ -20,1 +20,2 @@ fn b()"));
    // Joined new line 4 (0-based) is `added21` at `src/b.rs:21`.
    let mut range = nsh_base::DiffLineRange::default();
    range.side = nsh_base::DiffSide::New;
    range.start = 4;
    range.end = 4;
    let resolved = index.resolve_range(range).expect("maps back to src/b.rs");
    assert_eq!(resolved.path(), "src/b.rs");
    assert_eq!(resolved.start(), 21);
    assert_eq!(
        index.anchor_for(&resolved),
        Some((nsh_base::DiffSide::New, 4))
    );
}

#[gpui::test]
fn a_review_comment_reaches_the_chosen_pane_with_its_location(cx: &mut TestAppContext) {
    let sent = Rc::new(RefCell::new(None::<(i64, Option<u64>, String)>));
    let host = PluginHost::with_panes(
        {
            let sent = sent.clone();
            move |project, pane, text, _| {
                *sent.borrow_mut() = Some((project, pane, text.to_string()));
                Ok(SendOutcome::Submitted)
            }
        },
        |_, _| {
            vec![
                PaneTarget {
                    pane: 7,
                    ordinal: 1,
                    tab_name: "主工作区".to_string(),
                    title: "优化 Diff 视图".to_string(),
                    agent: Some("codex".to_string()),
                    state: None,
                    focused: true,
                },
                PaneTarget {
                    pane: 8,
                    ordinal: 2,
                    tab_name: "主工作区".to_string(),
                    title: "修复终端换行".to_string(),
                    agent: Some("claude".to_string()),
                    state: None,
                    focused: false,
                },
            ]
        },
    );
    let source = PatchSource::Commit {
        root: PathBuf::from("/nonexistent"),
        hash: "HEAD".to_string(),
    };
    let store = empty_store();
    let view = patch_view(cx, 3, source, "", "提交 abc1234", host, store.clone());
    view.update(cx, |view, cx| {
        view.add_comment(
            ReviewRange::new("src/git.rs", ReviewSide::Modified, 12, 14),
            cx,
        );
        view.update_comment(0, "这里应该先检查状态".to_string(), cx);
        view.set_target(8, cx);
        view.send_comments(cx);
        assert!(view.comments.is_empty());
    });
    assert_eq!(
        sent.borrow().as_ref(),
        Some(&(
            3,
            Some(8),
            "请根据以下代码审查意见修改（提交 abc1234）：\n\nsrc/git.rs:12-14\n这里应该先检查状态"
                .to_string()
        ))
    );
    let store = store.borrow();
    assert!(store.drafts.is_empty(), "a sent draft is no longer a draft");
    let record = store.sent.values().next().expect("the send is recorded");
    assert_eq!(record.count, 1);
    assert_eq!(record.target, "修复终端换行");
}

#[gpui::test]
fn a_closed_sheet_restores_its_draft(cx: &mut TestAppContext) {
    let store = empty_store();
    let commit = PatchSource::Commit {
        root: PathBuf::from("/nonexistent"),
        hash: "HEAD".to_string(),
    };
    let first = patch_view(
        cx,
        1,
        commit.clone(),
        "",
        "提交 abc1234",
        test_host(),
        store.clone(),
    );
    first.update(cx, |view, cx| {
        view.comments.push(ReviewComment {
            index: 0,
            range: ReviewRange::new("src/git.rs", ReviewSide::Modified, 12, 14),
            text: "这里应该先检查状态".to_string(),
        });
        view.next_comment = 1;
        view.save_drafts(cx);
    });
    drop(first);

    let second = patch_view(cx, 1, commit, "", "提交 abc1234", test_host(), store);
    let text = second.read_with(cx, |view, _| view.comments[0].text.clone());
    assert_eq!(text, "这里应该先检查状态");
}

#[test]
fn the_panel_draws_changes_switches_tabs_and_opens_commits() {
    let (dir, path) = fixture();
    std::fs::write(path.join("b"), "b\n").unwrap();
    let history = crate::git::history(&path).unwrap();
    let changes = crate::git::changes(&path).unwrap();
    let rows = change_rows_from(&changes);
    assert_eq!(rows.len(), 2);

    let (events_tx, events_rx) = unbounded::<GitHostEvent>();
    let (actions_tx, mut actions_rx) = unbounded::<GitPanelAction>();
    let initial = GitPanelSnapshot {
        project_id: Some(1),
        tab: PanelTab::Changes,
        branch: Some(history.branch.clone()),
        branch_diff: Some(crate::git::BranchDiffSummary {
            base: "main".into(),
            head: "feat/demo".into(),
            commits: 2,
            files: 3,
        }),
        loaded: true,
        error: None,
        change_count: changes.len(),
        change_rows: rows,
        commits: history.commits.clone(),
        filter: None,
        marks: Default::default(),
    };
    let mount_id = register_panel_mount(GitPanelMount {
        initial: initial.clone(),
        events_rx,
        actions_tx,
    });
    let root = nsh_core::element::Root::new::<view::GitLogRoot>(move || view::GitLogRootProps {
        mount_id: mount_id.into(),
    });
    let mut app = nsh_testing::Headless::start(EngineConfig::new(root)).expect("start git NSH");
    app.wait_for("未暂存").expect("shows unstaged section");
    app.wait_for("b").expect("shows untracked file");

    app.press("历史 1").expect("switch to history tab");
    assert!(matches!(
        actions_rx.try_recv().ok(),
        Some(GitPanelAction::SelectTab(PanelTab::History))
    ));

    let mut history_snap = initial;
    history_snap.tab = PanelTab::History;
    events_tx
        .unbounded_send(GitHostEvent::Snapshot(history_snap))
        .expect("switch to history");
    app.wait_for("init").expect("shows initial commit");
    app.press("init").expect("click commit row");
    assert!(matches!(
        actions_rx.try_recv().ok(),
        Some(GitPanelAction::OpenCommit(_))
    ));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_long_branch_name_stays_inside_the_git_panel_header() {
    let branch = "feature/DEV-20897-saml-adapter";
    let commit = crate::git::Commit {
        hash: "0".repeat(40),
        short: "0000000".into(),
        refs: Vec::new(),
        author: String::new(),
        email: String::new(),
        date: String::new(),
        subject: "init".into(),
        body: String::new(),
        parents: 0,
    };
    let (_events_tx, events_rx) = unbounded::<GitHostEvent>();
    let (actions_tx, _actions_rx) = unbounded::<GitPanelAction>();
    let initial = GitPanelSnapshot {
        project_id: Some(1),
        tab: PanelTab::Changes,
        branch: Some(branch.to_string()),
        branch_diff: None,
        loaded: true,
        error: None,
        change_count: 0,
        change_rows: Vec::new(),
        commits: Arc::new(vec![commit; 5000]),
        filter: None,
        marks: Default::default(),
    };
    let mount_id = register_panel_mount(GitPanelMount {
        initial,
        events_rx,
        actions_tx,
    });
    let root = nsh_core::element::Root::new::<view::GitLogRoot>(move || view::GitLogRootProps {
        mount_id: mount_id.into(),
    });
    let mut app = nsh_testing::Headless::start(EngineConfig::new(root)).expect("start git NSH");
    app.set_viewport(nsh_core::Size::new(
        nsh_core::Points(300.0),
        nsh_core::Points(400.0),
    ))
    .expect("set the panel viewport");
    app.wait_for("工作区没有未提交的改动")
        .expect("panel body renders");

    let nodes = app.screen().layout_nodes();
    let text_node = |needle: &str| {
        nodes
            .iter()
            .find(|n| n.text.as_ref().is_some_and(|t| t.text.text == needle))
            .unwrap_or_else(|| panic!("no {needle} text node"))
    };
    let history_tab = text_node("历史 5000");
    let tab_button = nodes
        .iter()
        .find(|n| n.id == history_tab.parent)
        .expect("the history tab button around its label");
    let branch_label = text_node(branch).bounds;
    assert!(
        branch_label.origin.x >= tab_button.bounds.right(),
        "branch label {branch_label:?} overlaps the tab bar ending at {:?}",
        tab_button.bounds
    );
    assert!(
        branch_label.right() <= nsh_core::Points(300.0),
        "branch label {branch_label:?} overflows the 300pt panel"
    );
}
