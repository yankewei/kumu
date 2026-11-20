use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::{
    AppContext, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    TestAppContext, Window, div,
};

use super::patch_view::{PatchFooter, PatchView};
use super::review::{PatchSheet, PatchSource, ReviewComment, ReviewStore, compose_comments};
use super::{GitLogPanel, Plugin, PluginHost, PluginProject};
use crate::plugins::{PaneTarget, SendOutcome};
use gpui_kit::component::diff::{DiffLineRange, DiffSide};

/// A host whose terminal is never asked for text in these tests.
fn test_host() -> PluginHost {
    PluginHost::with_panes(|_, _, _, _| Ok(SendOutcome::Pasted), |_, _| Vec::new())
}

fn empty_store() -> Rc<RefCell<ReviewStore>> {
    Rc::new(RefCell::new(ReviewStore::default()))
}

/// Build a sheet over `source`, sharing `store` with the panel that would
/// have opened it.
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
                key,
                source,
                message,
                context,
                host,
                reviews: store,
                notify_panel: Rc::new(|_| {}),
            },
            cx,
        )
    })
}

/// Run git in `repo`, failing the test with git's own message.
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

/// Parallel tests can read the clock in the same tick, so a counter keeps
/// their fixture directories apart.
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

/// A repository with one commit, plus the directory that owns it.
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

struct Host {
    view: Entity<PatchView>,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.view.clone())
    }
}

/// The view reads the commit on the background executor and hands git's
/// own patch to the diff parser, which is what the sheet draws.
#[gpui::test]
fn the_diff_view_parses_the_commits_patch(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
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

    let (files, error) = view.read_with(cx, |view, cx| {
        (
            view.state
                .as_ref()
                .map(|state| state.read(cx).files().len()),
            view.error.clone(),
        )
    });
    assert_eq!(error, None);
    assert_eq!(files, Some(1));

    // The sheet only adds chrome around this view, so drawing the view is
    // what proves the parsed patch reaches the diff component. A comment
    // is an annotation inside a row, so drawing with one also proves the
    // editor gets laid out with the code.
    let (_, cx) = cx.add_window_view(|_, _| Host { view: view.clone() });
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.add_comment(
                DiffLineRange::new("a", DiffSide::Modified, 1, 1),
                window,
                cx,
            );
        });
    });
    cx.run_until_parked();
    for _ in 0..3 {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

/// A working tree change reads without a commit to name it: the sheet for
/// an untracked file gets its patch from `--no-index`.
#[gpui::test]
fn the_diff_view_parses_an_uncommitted_change(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (dir, path) = fixture();
    std::fs::write(path.join("new"), "new\n").unwrap();
    let changes = crate::git::changes(&path).unwrap();
    let change = changes.unstaged[0].clone();

    let source = PatchSource::Change { root: path, change };
    let view = patch_view(cx, 1, source, "", "未暂存改动", test_host(), empty_store());
    cx.run_until_parked();

    let (files, error) = view.read_with(cx, |view, cx| {
        (
            view.state
                .as_ref()
                .map(|state| state.read(cx).files().len()),
            view.error.clone(),
        )
    });
    assert_eq!(error, None);
    assert_eq!(files, Some(1));

    std::fs::remove_dir_all(&dir).unwrap();
}

/// A comment points at the line the user marked, so the agent can open the
/// patch at the same place. Comments keep the order they were written in.
#[test]
fn comments_compose_as_one_instruction_with_their_locations() {
    let comment = |start, end| DiffLineRange::new("src/git.rs", DiffSide::Modified, start, end);
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

/// A unified selection can run from a deleted line to an added one, and
/// the two line numbers mean different things.
#[test]
fn a_range_across_sides_names_both_sides() {
    let range = DiffLineRange::new("src/git.rs", DiffSide::Original, 12, 14)
        .with_end_side(DiffSide::Modified);
    let message = compose_comments("未暂存改动", &[(range, "这一段要一起看".into())]);
    assert_eq!(
        message,
        "请根据以下代码审查意见修改（未暂存改动）：\n\n\
         src/git.rs:12（改动前）-14（改动后）\n这一段要一起看"
    );
}

/// The sheet hands the host one instruction naming each commented range,
/// clears the editors once the terminal took it, and records where it went.
#[gpui::test]
fn a_review_comment_reaches_the_chosen_pane_with_its_location(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
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
                    title: "codex".to_string(),
                    agent: Some("codex".to_string()),
                    focused: true,
                },
                PaneTarget {
                    pane: 8,
                    ordinal: 2,
                    title: "claude".to_string(),
                    agent: Some("claude".to_string()),
                    focused: false,
                },
            ]
        },
    );
    // The patch itself is irrelevant here: the comment is added and sent
    // before any read could fail or finish.
    let source = PatchSource::Commit {
        root: PathBuf::from("/nonexistent"),
        hash: "HEAD".to_string(),
    };
    let store = empty_store();
    let view = patch_view(cx, 3, source, "", "提交 abc1234", host, store.clone());
    let (_, cx) = cx.add_window_view(|_, _| Host { view: view.clone() });
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.add_comment(
                DiffLineRange::new("src/git.rs", DiffSide::Modified, 12, 14),
                window,
                cx,
            );
            let input = view.comments[0]
                .input
                .clone()
                .expect("a new comment has an editor");
            input.update(cx, |input, cx| {
                input.set_value("这里应该先检查状态".to_string(), window, cx)
            });
            // Sending goes where the user pointed, not where focus is.
            view.set_target(8, cx);
            view.send_comments(cx);
            assert!(view.comments.is_empty());
        });
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
    assert_eq!(record.target, "窗格 2 · claude");
}

/// Closing a sheet keeps its unsent comment, and the next sheet over the
/// same patch puts the text back in an editor.
#[gpui::test]
fn a_closed_sheet_restores_its_draft(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
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
            range: DiffLineRange::new("src/git.rs", DiffSide::Modified, 12, 14),
            text: "这里应该先检查状态".to_string(),
            input: None,
        });
        view.next_comment = 1;
        view.save_drafts(cx);
    });
    drop(first);

    let second = patch_view(cx, 1, commit, "", "提交 abc1234", test_host(), store);
    let (_, cx) = cx.add_window_view(|_, _| Host {
        view: second.clone(),
    });
    cx.run_until_parked();
    for _ in 0..3 {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
    }
    let (text, value) = second.read_with(cx, |view, cx| {
        let comment = &view.comments[0];
        (
            comment.text.clone(),
            comment
                .input
                .as_ref()
                .map(|input| input.read(cx).value().to_string()),
        )
    });
    assert_eq!(text, "这里应该先检查状态");
    assert_eq!(value.as_deref(), Some("这里应该先检查状态"));
}

struct PanelHost {
    panel: Entity<GitLogPanel>,
}

impl Render for PanelHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(gpui_kit::px(320.))
            .h(gpui_kit::px(600.))
            .child(self.panel.clone())
    }
}

struct FooterHost {
    footer: Entity<PatchFooter>,
}

impl Render for FooterHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.footer.clone())
    }
}

/// The footer names the pane the comments go to and offers the others in a
/// menu; drawing it is what proves the buttons and the dropdown build.
#[gpui::test]
fn the_footer_offers_the_projects_panes(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let host = PluginHost::with_panes(
        |_, _, _, _| Ok(SendOutcome::Submitted),
        |_, _| {
            vec![
                PaneTarget {
                    pane: 1,
                    ordinal: 1,
                    title: "codex".to_string(),
                    agent: Some("codex".to_string()),
                    focused: true,
                },
                PaneTarget {
                    pane: 2,
                    ordinal: 2,
                    title: "终端".to_string(),
                    agent: None,
                    focused: false,
                },
            ]
        },
    );
    let source = PatchSource::Commit {
        root: PathBuf::from("/nonexistent"),
        hash: "HEAD".to_string(),
    };
    let view = patch_view(cx, 1, source, "", "提交 abc1234", host, empty_store());
    let footer = cx.new(|cx| PatchFooter::new(view.clone(), cx));
    let (_, cx) = cx.add_window_view(|_, _| FooterHost { footer });
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.add_comment(
                DiffLineRange::new("a", DiffSide::Modified, 1, 1),
                window,
                cx,
            );
        });
    });
    cx.run_until_parked();
    for _ in 0..3 {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
    }
}

/// Drawing is what would catch a broken row or list: the panel shows the
/// changes it read, then the history on the other tab.
#[gpui::test]
fn the_panel_draws_changes_and_history(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (dir, path) = fixture();
    std::fs::write(path.join("b"), "b\n").unwrap();
    let project = PluginProject {
        id: 1,
        path: path.clone(),
    };

    let (host, cx) = cx.add_window_view(|window, cx| {
        let panel = cx.new(|cx| GitLogPanel::new(Some(project.clone()), test_host(), window, cx));
        PanelHost { panel }
    });
    cx.run_until_parked();
    for _ in 0..3 {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
    }
    // One heading and one file, which is what the changes tab drew.
    host.update(cx, |host, cx| {
        assert_eq!(host.panel.read(cx).change_rows().len(), 2);
    });

    cx.update(|_, cx| {
        host.update(cx, |host, cx| {
            host.panel.update(cx, |panel, cx| {
                panel.tab = super::PanelTab::History;
                cx.notify();
            });
        });
    });
    cx.run_until_parked();
    for _ in 0..3 {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
    }

    std::fs::remove_dir_all(&dir).unwrap();
}
