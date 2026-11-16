//! The Git panel: the selected project's uncommitted changes and its commit
//! history.
//!
//! The panel reads both on the background executor and polls while it is
//! alive: a poll reads a cheap ref fingerprint and the status listing, and only
//! takes the full log when a ref moved. Rows stay virtualized, so a repository
//! with a long history costs the same to draw as a short one. The home window
//! creates a panel when the user opens it and drops it when they close it, so a
//! closed panel costs nothing.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::diff::{
    Diff, DiffAnnotation, DiffFile, DiffLinePosition, DiffLineRange, DiffParseOptions, DiffSide,
    DiffState,
};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{Plugin, PluginHost, PluginProject, SendOutcome};
use crate::git::{Change, Changes, Commit, History};

/// How often an open panel checks whether the repository moved. Polling rather
/// than a filesystem watcher stays correct for commits a terminal's git
/// commands make without adding a platform watcher to the app.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Every commit row has the same height, which is what lets the uniform list
/// virtualize a full history. One compact line: refs, hash and subject.
const ROW_HEIGHT: f32 = 32.;

/// A change row and its section heading share this height, because the list of
/// both draws through one uniform list.
const CHANGE_ROW_HEIGHT: f32 = 28.;

/// Which of the panel's two lists is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelTab {
    Changes,
    History,
}

/// One poll's read of the repository: the ref fingerprint, the uncommitted
/// changes, and the log only when a ref moved since the previous poll.
struct RepoRead {
    fingerprint: String,
    changes: Changes,
    history: Option<History>,
}

pub struct GitLogPanel {
    project: Option<PluginProject>,
    history: Option<History>,
    /// The same commits as `history`, shared with the list closure.
    commits: Arc<Vec<Commit>>,
    /// The uncommitted changes, refreshed by every poll.
    changes: Changes,
    /// Which list the panel shows; changes are what a working session reads.
    tab: PanelTab,
    /// What the repository looked like when the rows were read. A poll that
    /// reads the same fingerprint leaves the log alone.
    fingerprint: Option<String>,
    /// Commit indices matching `query`, or `None` when the search is empty.
    filter: Option<Rc<Vec<usize>>>,
    /// Bumped for every filter rebuild, so the scan for an older query is
    /// dropped instead of overwriting a newer one.
    filter_generation: u64,
    query: String,
    search: Entity<InputState>,
    error: Option<String>,
    /// A read is running; another request waits for it instead of running a
    /// second git process.
    loading: bool,
    dirty: bool,
    /// Changes when the selected project changes, so a read from the previous
    /// project cannot overwrite the log.
    generation: u64,
    /// How a patch sheet hands its review comments to the focused terminal.
    host: PluginHost,
}

impl Plugin for GitLogPanel {
    const ID: &'static str = "git-log";
    const TITLE: &'static str = "Git";
    const ICON: &'static str = "icons/git-branch.svg";

    fn new(
        project: Option<PluginProject>,
        host: PluginHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索提交信息"));
        cx.subscribe(&search, |panel, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                panel.query = panel.search.read(cx).value().to_string();
                panel.rebuild_filter(cx);
                cx.notify();
            }
        })
        .detach();
        let mut panel = Self {
            project,
            history: None,
            commits: Arc::default(),
            changes: Changes::default(),
            tab: PanelTab::Changes,
            fingerprint: None,
            filter: None,
            filter_generation: 0,
            query: String::new(),
            search,
            error: None,
            loading: false,
            dirty: false,
            generation: 0,
            host,
        };
        panel.reload(cx);
        Self::watch(cx);
        panel
    }

    fn set_project(&mut self, project: Option<PluginProject>, cx: &mut Context<Self>) {
        if self.project == project {
            return;
        }
        self.project = project;
        self.generation += 1;
        self.clear_rows();
        self.error = None;
        if self.loading {
            // The read in flight belongs to the old project. Its completion
            // starts the read for this one.
            self.dirty = true;
        } else {
            self.reload(cx);
        }
        cx.notify();
    }
}

impl GitLogPanel {
    /// Poll while the panel exists. Dropping the panel makes the next update
    /// fail, which ends the loop.
    fn watch(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                if this.update(cx, |panel, cx| panel.reload(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Forget the rows and the fingerprint they came from. Bumping the filter
    /// generation also drops a scan that is still running, whose indices
    /// address the list this clears.
    fn clear_rows(&mut self) {
        self.history = None;
        self.commits = Arc::default();
        self.changes = Changes::default();
        self.fingerprint = None;
        self.filter = None;
        self.filter_generation += 1;
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        if self.loading {
            self.dirty = true;
            return;
        }
        self.loading = true;
        let generation = self.generation;
        let known = self.fingerprint.clone();
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_spawn(async move {
                    // The fingerprint is read first: one taken after the log
                    // could be newer than the rows it would be stored with.
                    let fingerprint = crate::git::fingerprint(&project.path)?;
                    let history = if known.as_deref() == Some(fingerprint.as_str()) {
                        None
                    } else {
                        Some(crate::git::history(&project.path)?)
                    };
                    // Read last so the changes reflect everything the user
                    // saved while the log was being walked.
                    let changes = crate::git::changes(&project.path)?;
                    Ok::<_, String>(RepoRead {
                        fingerprint,
                        changes,
                        history,
                    })
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.loading = false;
                if panel.generation == generation {
                    panel.apply(read, cx);
                }
                if panel.dirty {
                    panel.dirty = false;
                    panel.reload(cx);
                }
            });
        })
        .detach();
    }

    /// Take one read: nothing new, a new history, new changes, or the error to
    /// show instead.
    fn apply(&mut self, read: Result<RepoRead, String>, cx: &mut Context<Self>) {
        match read {
            Ok(read) => {
                let mut changed = self.error.take().is_some();
                if let Some(history) = read.history {
                    self.fingerprint = Some(read.fingerprint);
                    if self.history.as_ref() != Some(&history) {
                        self.commits = history.commits.clone();
                        self.history = Some(history);
                        // The filter indexes the commits that just went away.
                        self.filter = None;
                        self.rebuild_filter(cx);
                        changed = true;
                    }
                }
                if self.changes != read.changes {
                    self.changes = read.changes;
                    changed = true;
                }
                if changed {
                    cx.notify();
                }
            }
            Err(error) => {
                // A failed read says nothing about the fingerprint, so the next
                // poll tries the repository again.
                self.clear_rows();
                if self.error.as_ref() != Some(&error) {
                    self.error = Some(error);
                    cx.notify();
                }
            }
        }
    }

    /// Rebuild the row filter for `query`. The scan runs on the background
    /// executor so a keystroke never walks a long history on the UI thread; the
    /// result lands a frame later unless a newer query replaced it first.
    fn rebuild_filter(&mut self, cx: &mut Context<Self>) {
        self.filter_generation += 1;
        let generation = self.filter_generation;
        let query = self.query.trim().to_lowercase();
        if query.is_empty() {
            self.filter = None;
            return;
        }
        let commits = self.commits.clone();
        if commits.is_empty() {
            self.filter = Some(Rc::new(Vec::new()));
            return;
        }
        cx.spawn(async move |this, cx| {
            let matched = cx
                .background_spawn(async move {
                    commits
                        .iter()
                        .enumerate()
                        .filter(|(_, commit)| {
                            commit.subject.to_lowercase().contains(&query)
                                || commit.body.to_lowercase().contains(&query)
                        })
                        .map(|(index, _)| index)
                        .collect::<Vec<usize>>()
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.filter_generation == generation {
                    panel.filter = Some(Rc::new(matched));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn visible_count(&self) -> usize {
        self.filter
            .as_ref()
            .map_or(self.commits.len(), |filter| filter.len())
    }

    /// Show the commit's patch in a sheet. The sheet's view owns the read, so
    /// closing the sheet drops the work with it and this panel keeps none of it.
    fn open_commit(&mut self, commit: Commit, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        let meta = if commit.email.is_empty() {
            format!("{} · {} · {}", commit.short, commit.author, commit.date)
        } else {
            format!(
                "{} · {} <{}> · {}",
                commit.short, commit.author, commit.email, commit.date
            )
        };
        self.open_patch(
            PatchRequest {
                project: project.id,
                title: commit.subject.clone(),
                meta,
                source: PatchSource::Commit {
                    root: project.path,
                    hash: commit.hash.clone(),
                },
                message: commit.body.clone(),
                // The patch's line numbers belong to this commit, not to the
                // working tree the agent will edit.
                context: format!("提交 {}", commit.short),
            },
            window,
            cx,
        );
    }

    /// Show one uncommitted change's patch in a sheet, read from the index for
    /// a staged change and from the working tree for an unstaged one.
    fn open_change(&mut self, change: Change, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        let status = if change.staged {
            "已暂存改动"
        } else {
            "未暂存改动"
        };
        let mut meta = format!("{status} · {}", status_label(change.status));
        if let Some(original) = &change.original {
            meta.push_str(&format!(" · 原路径 {original}"));
        }
        self.open_patch(
            PatchRequest {
                project: project.id,
                title: change.path.clone(),
                meta,
                source: PatchSource::Change {
                    root: project.path,
                    change,
                },
                message: String::new(),
                context: status.to_string(),
            },
            window,
            cx,
        );
    }

    /// Open a sheet around a [`PatchView`], which reads the patch itself.
    fn open_patch(&self, request: PatchRequest, window: &mut Window, cx: &mut Context<Self>) {
        let PatchRequest {
            project,
            title,
            meta,
            source,
            message,
            context,
        } = request;
        let view =
            cx.new(|cx| PatchView::new(project, source, message, context, self.host.clone(), cx));
        let footer = cx.new(|cx| PatchFooter::new(view.clone(), cx));
        window.open_sheet(cx, move |sheet, window, cx| {
            let theme = cx.theme();
            // The sheet is right-anchored at a fixed width, so a narrow window
            // has to shrink it: the diff's line numbers sit on the left edge.
            let width = px(760.).min(window.viewport_size().width - px(96.));
            sheet
                .size(width.max(px(320.)))
                .title(
                    v_flex()
                        .gap_0p5()
                        .min_w_0()
                        .child(div().text_sm().truncate().child(title.clone()))
                        .child(
                            div()
                                .text_xs()
                                .font_normal()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(meta.clone()),
                        ),
                )
                .child(
                    view.clone()
                        .cached(StyleRefinement::default().size_full().min_h_0().min_w_0()),
                )
                .footer(footer.clone())
        });
    }

    fn render_commit(
        index: usize,
        total: usize,
        commit: &Commit,
        theme: &Theme,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        let line = theme.sidebar_border;
        let first = index == 0;
        let last = index + 1 == total;

        // A single timeline lane. Merge commits get a warning-colored dot so
        // they stand out without drawing a full multi-branch graph.
        let lane = div()
            .relative()
            .flex_none()
            .w(px(16.))
            .h_full()
            .child(
                div()
                    .absolute()
                    .left(px(7.))
                    .w(px(1.))
                    .bg(line)
                    .top(px(if first { 16. } else { 0. }))
                    .bottom(px(if last { 16. } else { 0. })),
            )
            .child(
                div()
                    .absolute()
                    .left(px(4.))
                    .top(px(12.))
                    .size_2()
                    .rounded_full()
                    .bg(if commit.parents > 1 {
                        theme.warning
                    } else {
                        theme.link
                    }),
            );

        let row = h_flex()
            .id(format!("git-log-row-{}", commit.hash))
            .w_full()
            .h(px(ROW_HEIGHT))
            .px_2()
            .gap_1()
            .items_center()
            .hover(move |style| style.bg(theme.accent.opacity(0.35)))
            .on_click(on_click)
            .child(lane)
            .child(
                div()
                    .flex_none()
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .font_medium()
                    .text_color(theme.link)
                    .child(commit.short.clone()),
            )
            .children(ref_badges(&commit.refs, theme))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .child(commit.subject.clone()),
            );

        row.into_any_element()
    }
}

/// Where a [`PatchView`] reads its patch from.
enum PatchSource {
    /// One commit in the repository at `root`.
    Commit { root: PathBuf, hash: String },
    /// One uncommitted change in the repository at `root`.
    Change { root: PathBuf, change: Change },
}

/// Everything a patch sheet needs before it reads the patch.
struct PatchRequest {
    project: i64,
    title: String,
    meta: String,
    source: PatchSource,
    /// The commit message, shown above the patch; empty for a change.
    message: String,
    /// Where the patch's line numbers point, named when they are sent to the
    /// agent: a commit's numbers are not the working tree's.
    context: String,
}

/// The sheet body: one patch. The read and the patch parse run on the
/// background executor; the diff state is built on the UI thread once the files
/// are in hand, because it prepares syntax off-thread itself.
struct PatchView {
    /// The project whose focused pane receives the review comments.
    project: i64,
    /// How this sheet reaches that pane.
    host: PluginHost,
    /// Review comments, each anchored to the patch lines it discusses.
    comments: Vec<ReviewComment>,
    /// Distinguishes comment element ids after a removal and a new comment.
    next_comment: u64,
    /// What the patch belongs to, so the agent knows which line numbers the
    /// comments refer to.
    context: String,
    /// Why the last send did not go through, shown beside the send button.
    notice: Option<SendNotice>,
    /// The commit message, shown above the patch; empty for a change.
    message: String,
    state: Option<Entity<DiffState>>,
    truncated: bool,
    error: Option<String>,
}

/// One review comment, edited beneath the line it targets.
struct ReviewComment {
    /// Distinguishes this comment's elements from an earlier one's.
    index: u64,
    range: DiffLineRange,
    input: Entity<TextareaState>,
}

impl ReviewComment {
    fn annotation_id(&self) -> ElementId {
        ElementId::named_usize("git-diff-comment", self.index as usize)
    }
}

/// Why the sheet could not submit, or why it left the text on a shell's
/// command line instead.
enum SendNotice {
    /// The text is on a shell's command line, where Enter would run it.
    Pasted,
    Hint(String),
}

impl PatchView {
    fn new(
        project: i64,
        source: PatchSource,
        message: String,
        context: String,
        host: PluginHost,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_spawn(async move {
                    let diff = match source {
                        PatchSource::Commit { root, hash } => crate::git::commit_diff(&root, &hash),
                        PatchSource::Change { root, change } => {
                            crate::git::change_diff(&root, &change)
                        }
                    }?;
                    // A cut patch ends in the middle of a hunk, which the
                    // parser takes when it is told to count the lines present.
                    let files = DiffFile::parse_with(
                        &diff.patch,
                        DiffParseOptions::new().truncated_hunks(diff.truncated),
                    )
                    .map_err(|error| format!("无法解析补丁：{error}"))?;
                    Ok::<_, String>((diff.truncated, files))
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                match read {
                    Ok((truncated, files)) => {
                        let state = cx.new(|cx| DiffState::new(files, cx));
                        // Scrolling and selection notify the state; this view
                        // rebuilds the element so the new frame is painted.
                        cx.observe(&state, |_, _, cx| cx.notify()).detach();
                        view.truncated = truncated;
                        view.state = Some(state);
                    }
                    Err(error) => view.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        Self {
            project,
            host,
            comments: Vec::new(),
            next_comment: 0,
            context,
            notice: None,
            message,
            state: None,
            truncated: false,
            error: None,
        }
    }

    /// Anchor a comment beneath `range`, or focus the comment already there.
    fn add_comment(&mut self, range: DiffLineRange, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(comment) = self.comments.iter().find(|comment| comment.range == range) {
            let focus = comment.input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            return;
        }
        let input = cx.new(|cx| {
            TextareaState::new(&mut *window, cx)
                .placeholder("写下对这个改动片段的意见")
                .auto_grow(1, 6)
        });
        // A notice describes the previous text, so editing makes it stale.
        cx.subscribe(&input, |view, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) && view.notice.is_some() {
                view.notice = None;
                cx.notify();
            }
        })
        .detach();
        let index = self.next_comment;
        self.next_comment += 1;
        self.notice = None;
        window.focus(&input.read(cx).focus_handle(cx), cx);
        self.comments.push(ReviewComment {
            index,
            range,
            input,
        });
        cx.notify();
    }

    fn remove_comment(&mut self, index: u64, cx: &mut Context<Self>) {
        let before = self.comments.len();
        self.comments.retain(|comment| comment.index != index);
        if self.comments.len() != before {
            self.notice = None;
            cx.notify();
        }
    }

    /// Hand every non-empty comment to the focused terminal as one instruction.
    /// Returns whether the terminal took it, which is when the sheet can close.
    fn send_comments(&mut self, cx: &mut Context<Self>) -> bool {
        let comments = self
            .comments
            .iter()
            .filter_map(|comment| {
                let text = comment.input.read(cx).value().trim().to_string();
                (!text.is_empty()).then(|| (comment.range.clone(), text))
            })
            .collect::<Vec<_>>();
        if comments.is_empty() {
            self.notice = Some(SendNotice::Hint("先写下评论再发送".to_string()));
            cx.notify();
            return false;
        }
        let message = compose_comments(&self.context, &comments);
        match self.host.send_to_focused_pane(self.project, &message, cx) {
            Ok(SendOutcome::Submitted) => {
                self.comments.clear();
                cx.notify();
                true
            }
            // The notes sit on a shell's command line. Keeping the editors
            // lets the user resend them after focusing an agent pane.
            Ok(SendOutcome::Pasted) => {
                self.notice = Some(SendNotice::Pasted);
                cx.notify();
                false
            }
            Err(problem) => {
                self.notice = Some(SendNotice::Hint(problem));
                cx.notify();
                false
            }
        }
    }

    /// The footer's one line: what to do next, or why the last send stopped.
    fn footer_hint(&self, theme: &Theme) -> (String, Hsla) {
        match &self.notice {
            Some(SendNotice::Pasted) => {
                ("已粘贴到当前终端，按 Enter 提交".to_string(), theme.warning)
            }
            Some(SendNotice::Hint(problem)) => (problem.clone(), theme.danger),
            None if self.comments.is_empty() => (
                "悬停 diff 行，点 + 添加评论".to_string(),
                theme.muted_foreground,
            ),
            None => (
                format!("{} 条评论", self.comments.len()),
                theme.muted_foreground,
            ),
        }
    }
}

impl Render for PatchView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let message = self.message.trim_end();
        let body: AnyElement = match (&self.error, &self.state) {
            (Some(error), _) => empty(error, theme.danger),
            (None, Some(state)) if state.read(cx).files().is_empty() => {
                empty("没有内容变更", muted)
            }
            (None, Some(state)) => {
                let comments = Rc::new(
                    self.comments
                        .iter()
                        .map(|comment| {
                            (
                                comment.annotation_id(),
                                comment.index,
                                comment.range.clone(),
                                comment.input.clone(),
                            )
                        })
                        .collect::<Vec<_>>(),
                );
                let view = cx.entity().downgrade();
                let annotations = comments.iter().map(|(id, _, range, _)| {
                    DiffAnnotation::line(
                        id.clone(),
                        DiffLinePosition::new(range.path().clone(), range.side(), range.start()),
                    )
                });
                let annotation_content = {
                    let comments = comments.clone();
                    let view = view.clone();
                    move |annotation: &DiffAnnotation, _window: &mut Window, cx: &mut App| {
                        let Some((_, index, range, input)) =
                            comments.iter().find(|(id, _, _, _)| id == annotation.id())
                        else {
                            return div().into_any_element();
                        };
                        render_comment(*index, range.clone(), input.clone(), view.clone(), cx)
                    }
                };
                let add_annotation = {
                    let view = view.clone();
                    move |range: &DiffLineRange, window: &mut Window, cx: &mut App| {
                        let range = range.clone();
                        if let Some(view) = view.upgrade() {
                            view.update(cx, |this, cx| this.add_comment(range, window, cx));
                        }
                    }
                };
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(
                        Diff::new(state)
                            .annotations(annotations)
                            .render_annotation(annotation_content)
                            .on_add_annotation(add_annotation)
                            .size_full(),
                    )
                    .into_any_element()
            }
            (None, None) => empty("正在读取补丁…", muted),
        };

        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .text_color(theme.sidebar_foreground)
            // The commit message lives here now that rows no longer open a
            // hover card, which would have been drawn over this sheet.
            .when(!message.is_empty(), |this| {
                this.child(
                    div()
                        .id("commit-diff-message")
                        .w_full()
                        .max_h(rems(12.))
                        .overflow_y_scroll()
                        .pb_2()
                        .text_sm()
                        .text_color(muted)
                        .child(message.to_string()),
                )
            })
            .when(self.truncated, |this| {
                this.child(
                    div()
                        .pb_2()
                        .text_xs()
                        .text_color(theme.warning)
                        .child("补丁过大，只显示了前面的一部分"),
                )
            })
            .child(body)
    }
}

/// The sheet footer. It lives outside the body's padding, so it is its own
/// view and follows the patch it belongs to.
struct PatchFooter {
    patch: Entity<PatchView>,
}

impl PatchFooter {
    fn new(patch: Entity<PatchView>, cx: &mut Context<Self>) -> Self {
        cx.observe(&patch, |_, _, cx| cx.notify()).detach();
        Self { patch }
    }
}

impl Render for PatchFooter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let patch = self.patch.clone();
        let (count, (hint, color)) = {
            let view = patch.read(cx);
            (view.comments.len(), view.footer_hint(theme))
        };
        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(color)
                    .child(hint),
            )
            .when(count > 0, |this| {
                this.child(
                    Button::new("git-diff-send")
                        .small()
                        .primary()
                        .label("发送到终端")
                        .on_click({
                            let patch = patch.clone();
                            move |_, window, cx| {
                                if patch.update(cx, |patch, cx| patch.send_comments(cx)) {
                                    window.close_sheet(cx);
                                }
                            }
                        }),
                )
            })
    }
}

/// One comment's editor, drawn beneath the line it targets.
fn render_comment(
    index: u64,
    range: DiffLineRange,
    input: Entity<TextareaState>,
    view: WeakEntity<PatchView>,
    cx: &mut App,
) -> AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let delete = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
        if let Some(view) = view.upgrade() {
            view.update(cx, |this, cx| this.remove_comment(index, cx));
        }
    };
    v_flex()
        .w_full()
        .gap_1()
        .p_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .font_medium()
                        .text_color(muted)
                        .child(comment_location(&range)),
                )
                .child(
                    Button::new(ElementId::named_usize(
                        "git-diff-comment-delete",
                        index as usize,
                    ))
                    .compact()
                    .ghost()
                    .icon(Icon::new(IconName::Close).xsmall())
                    .tooltip("删除评论")
                    .on_click(delete),
                ),
        )
        .child(Textarea::new(&input).appearance(false).bordered(false))
        .into_any_element()
}

/// Where a comment points, in the form the agent reads in the patch. A range
/// can start on one side and end on the other in the unified view.
fn comment_location(range: &DiffLineRange) -> String {
    let path = range.path();
    if range.side() != range.end_side() {
        return format!(
            "{path}:{}（改动前）-{}（改动后）",
            range.start(),
            range.end()
        );
    }
    let lines = if range.start() == range.end() {
        range.start().to_string()
    } else {
        format!("{}-{}", range.start(), range.end())
    };
    if range.side() == DiffSide::Original {
        format!("{path}:{lines}（改动前）")
    } else {
        format!("{path}:{lines}")
    }
}

/// Every comment as one instruction, each under the line it targets.
fn compose_comments(context: &str, comments: &[(DiffLineRange, String)]) -> String {
    let mut message = if context.is_empty() {
        String::from("请根据以下代码审查意见修改：")
    } else {
        format!("请根据以下代码审查意见修改（{context}）：")
    };
    for (range, comment) in comments {
        message.push_str("\n\n");
        message.push_str(&comment_location(range));
        message.push('\n');
        message.push_str(comment);
    }
    message
}

impl Render for GitLogPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let border = theme.sidebar_border;
        let branch = self.history.as_ref().map(|history| history.branch.clone());
        let total = self.commits.len();
        let visible = self.visible_count();
        let changes = self.changes.len();
        let changes_label = if changes > 0 {
            format!("改动 {changes}")
        } else {
            "改动".to_string()
        };
        let history_label = if total > 0 {
            format!("历史 {total}")
        } else {
            "历史".to_string()
        };

        let toolbar = self.project.is_some().then(|| {
            let panel = cx.entity();
            v_flex()
                .child(
                    h_flex()
                        .px_3()
                        .pt_1p5()
                        .pb_1()
                        .gap_2()
                        .items_center()
                        .border_b_1()
                        .border_color(border)
                        .child(
                            TabBar::new("git-panel-tabs")
                                .xsmall()
                                .flex_none()
                                .segmented()
                                .selected_index(match self.tab {
                                    PanelTab::Changes => 0,
                                    PanelTab::History => 1,
                                })
                                .child(Tab::new().label(changes_label))
                                .child(Tab::new().label(history_label))
                                .on_click(move |index, _, cx| {
                                    let tab = match index {
                                        0 => PanelTab::Changes,
                                        _ => PanelTab::History,
                                    };
                                    panel.update(cx, |panel, cx| {
                                        if panel.tab != tab {
                                            panel.tab = tab;
                                            cx.notify();
                                        }
                                    });
                                }),
                        )
                        .child(
                            // The branch gives way to the tabs when the panel
                            // is dragged narrow.
                            div().flex_1().min_w_0().flex().justify_end().child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(branch.unwrap_or_else(|| "—".to_string())),
                            ),
                        ),
                )
                .when(self.tab == PanelTab::History, |this| {
                    this.child(
                        div()
                            .px_2()
                            .py_1p5()
                            .border_b_1()
                            .border_color(border)
                            .child(
                                Input::new(&self.search)
                                    .small()
                                    .cleanable(true)
                                    .prefix(Icon::new(IconName::Search).small().text_color(muted)),
                            ),
                    )
                })
        });

        let body: AnyElement = if self.project.is_none() {
            empty("没有选中的项目", muted)
        } else if let Some(error) = &self.error {
            empty(error, theme.danger)
        } else if self.history.is_none() {
            empty("正在读取…", muted)
        } else if self.tab == PanelTab::Changes {
            if self.changes.is_empty() {
                empty("工作区没有未提交的改动", muted)
            } else {
                let rows = Rc::new(self.change_rows());
                let panel = cx.entity();
                uniform_list("git-change-list", rows.len(), move |range, _window, cx| {
                    let theme = cx.theme();
                    range
                        .map(|row| {
                            let row = &rows[row];
                            let panel = panel.clone();
                            let clicked = match row {
                                ChangeRow::File(change) => Some(change.clone()),
                                ChangeRow::Header(_) => None,
                            };
                            Self::render_change(
                                row,
                                theme,
                                move |_, window: &mut Window, cx: &mut App| {
                                    if let Some(change) = clicked.clone() {
                                        panel.update(cx, |panel, cx| {
                                            panel.open_change(change, window, cx);
                                        });
                                    }
                                },
                            )
                        })
                        .collect()
                })
                .flex_1()
                .min_h_0()
                .into_any_element()
            }
        } else if visible == 0 {
            if self.filter.is_some() {
                empty("没有匹配的提交", muted)
            } else {
                empty("这个仓库还没有提交", muted)
            }
        } else {
            let commits = self.commits.clone();
            let filter = self.filter.clone();
            let panel = cx.entity();
            uniform_list("git-log-list", visible, move |range, _window, cx| {
                let theme = cx.theme();
                range
                    .map(|row| {
                        let index = filter.as_ref().map_or(row, |filter| filter[row]);
                        let commit = &commits[index];
                        let panel = panel.clone();
                        let clicked = commit.clone();
                        Self::render_commit(
                            row,
                            visible,
                            commit,
                            theme,
                            move |_, window: &mut Window, cx: &mut App| {
                                panel.update(cx, |panel, cx| {
                                    panel.open_commit(clicked.clone(), window, cx);
                                });
                            },
                        )
                    })
                    .collect()
            })
            .flex_1()
            .min_h_0()
            .into_any_element()
        };

        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .text_color(theme.sidebar_foreground)
            .children(toolbar)
            .child(body)
    }
}

/// A row of the changes tab: a section heading or one file.
enum ChangeRow {
    Header(&'static str),
    File(Change),
}

impl GitLogPanel {
    /// The changes tab's rows: a heading above each non-empty section.
    fn change_rows(&self) -> Vec<ChangeRow> {
        let mut rows = Vec::with_capacity(self.changes.len() + 2);
        for (label, changes) in [
            ("已暂存", &self.changes.staged),
            ("未暂存", &self.changes.unstaged),
        ] {
            if changes.is_empty() {
                continue;
            }
            rows.push(ChangeRow::Header(label));
            rows.extend(changes.iter().cloned().map(ChangeRow::File));
        }
        rows
    }

    fn render_change(
        row: &ChangeRow,
        theme: &Theme,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        let muted = theme.muted_foreground;
        let change = match row {
            ChangeRow::Header(label) => {
                return h_flex()
                    .w_full()
                    .h(px(CHANGE_ROW_HEIGHT))
                    .px_2()
                    .items_center()
                    .text_xs()
                    .font_medium()
                    .text_color(muted)
                    .child(*label)
                    .into_any_element();
            }
            ChangeRow::File(change) => change,
        };

        // The name keeps the room; the directory (or where a rename came from)
        // gives way when the panel is narrow.
        let trailing = match &change.original {
            Some(original) => format!("← {original}"),
            None => parent_dir(&change.path).to_string(),
        };
        h_flex()
            .id(format!(
                "git-change-{}-{}",
                if change.staged { "index" } else { "worktree" },
                change.path
            ))
            .w_full()
            .h(px(CHANGE_ROW_HEIGHT))
            .px_2()
            .gap_1p5()
            .items_center()
            .hover(move |style| style.bg(theme.accent.opacity(0.35)))
            .on_click(on_click)
            .child(
                div()
                    .flex_none()
                    .w(px(12.))
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .font_medium()
                    .text_color(status_color(change.status, theme))
                    .child(change.status.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .child(file_name(&change.path).to_string()),
            )
            .when(!trailing.is_empty(), |row| {
                row.child(
                    div()
                        .flex_none()
                        .max_w(relative(0.5))
                        .truncate()
                        .text_xs()
                        .text_color(muted)
                        .child(trailing),
                )
            })
            .into_any_element()
    }
}

/// Branch, tag and remote refs on a commit, capped so a busy commit cannot
/// crowd out its subject.
fn ref_badges(refs: &[String], theme: &Theme) -> Vec<AnyElement> {
    let shown = refs.len().min(1);
    let mut badges = Vec::new();
    for ref_name in refs.iter().take(shown) {
        let (label, color) = if let Some(name) = ref_name.strip_prefix("HEAD -> ") {
            (name, theme.link)
        } else if let Some(name) = ref_name.strip_prefix("tag: ") {
            (name, theme.warning)
        } else {
            (ref_name.as_str(), theme.link)
        };
        badges.push(
            div()
                .flex_none()
                .max_w(px(96.))
                .truncate()
                .px_1()
                .py_0p5()
                .rounded(theme.radius)
                .bg(color.opacity(0.14))
                .text_xs()
                .text_color(color)
                .child(label.to_string())
                .into_any_element(),
        );
    }
    if refs.len() > shown {
        badges.push(
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!("+{}", refs.len() - shown))
                .into_any_element(),
        );
    }
    badges
}

fn empty(message: &str, color: Hsla) -> AnyElement {
    div()
        .flex_1()
        .w_full()
        .px_3()
        .py_4()
        .text_xs()
        .text_color(color)
        .child(message.to_string())
        .into_any_element()
}

/// The last component of a repository-relative path.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Everything before a repository-relative path's last component, or an empty
/// string for a file at the root.
fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// The color of a change's status letter: what happened to the file, not which
/// side it was read from.
fn status_color(status: char, theme: &Theme) -> Hsla {
    match status {
        'M' | 'T' => theme.warning,
        'A' | '?' => theme.success,
        'D' | 'U' => theme.danger,
        'R' | 'C' => theme.link,
        _ => theme.muted_foreground,
    }
}

/// The status of a change in words, for the sheet's subtitle.
fn status_label(status: char) -> &'static str {
    match status {
        'M' => "已修改",
        'A' => "已添加",
        'D' => "已删除",
        'R' => "已重命名",
        'C' => "已复制",
        'T' => "类型变更",
        'U' => "有冲突",
        '?' => "新文件",
        _ => "已修改",
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use gpui_kit::{
        AppContext, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
        TestAppContext, Window, div,
    };

    use super::{PatchSource, PatchView, Plugin, compose_comments};
    use gpui_kit::component::diff::{DiffLineRange, DiffSide};

    /// A host whose terminal is never asked for text in these tests.
    fn test_host() -> super::PluginHost {
        super::PluginHost::new(|_, _, _| Ok(super::SendOutcome::Pasted))
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
        let view = cx.new(|cx| {
            PatchView::new(
                1,
                source,
                commit.body.clone(),
                "提交 abc1234".into(),
                test_host(),
                cx,
            )
        });
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
        let view = cx.new(|cx| {
            PatchView::new(
                1,
                source,
                String::new(),
                "未暂存改动".into(),
                test_host(),
                cx,
            )
        });
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
    /// and clears the editors once the terminal took it.
    #[gpui::test]
    fn a_review_comment_reaches_the_host_with_its_location(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let sent = Rc::new(RefCell::new(None::<(i64, String)>));
        let host = super::PluginHost::new({
            let sent = sent.clone();
            move |project, text, _| {
                *sent.borrow_mut() = Some((project, text.to_string()));
                Ok(super::SendOutcome::Submitted)
            }
        });
        // The patch itself is irrelevant here: the comment is added and sent
        // before any read could fail or finish.
        let source = PatchSource::Commit {
            root: PathBuf::from("/nonexistent"),
            hash: "HEAD".to_string(),
        };
        let view =
            cx.new(|cx| PatchView::new(3, source, String::new(), "提交 abc1234".into(), host, cx));
        let (_, cx) = cx.add_window_view(|_, _| Host { view: view.clone() });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.add_comment(
                    DiffLineRange::new("src/git.rs", DiffSide::Modified, 12, 14),
                    window,
                    cx,
                );
                let input = view.comments[0].input.clone();
                input.update(cx, |input, cx| {
                    input.set_value("这里应该先检查状态".to_string(), window, cx)
                });
                view.send_comments(cx);
                assert!(view.comments.is_empty());
            });
        });
        assert_eq!(
            sent.borrow().as_ref(),
            Some(&(
                3,
                "请根据以下代码审查意见修改（提交 abc1234）：\n\nsrc/git.rs:12-14\n这里应该先检查状态".to_string()
            ))
        );
    }

    struct PanelHost {
        panel: Entity<super::GitLogPanel>,
    }

    impl Render for PanelHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(gpui_kit::px(320.))
                .h(gpui_kit::px(600.))
                .child(self.panel.clone())
        }
    }

    /// Drawing is what would catch a broken row or list: the panel shows the
    /// changes it read, then the history on the other tab.
    #[gpui::test]
    fn the_panel_draws_changes_and_history(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (dir, path) = fixture();
        std::fs::write(path.join("b"), "b\n").unwrap();
        let project = super::PluginProject {
            id: 1,
            path: path.clone(),
        };

        let (host, cx) = cx.add_window_view(|window, cx| {
            let panel = cx
                .new(|cx| super::GitLogPanel::new(Some(project.clone()), test_host(), window, cx));
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
}
