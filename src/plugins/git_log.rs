//! The Git panel: the selected project's uncommitted changes and its commit
//! history.
//!
//! The panel reads both on the background executor and polls while it is
//! alive: a poll reads a cheap ref fingerprint and the status listing, and only
//! takes the full log when a ref moved. Rows stay virtualized, so a repository
//! with a long history costs the same to draw as a short one. The home window
//! creates a panel when the user opens it and drops it when they close it, so a
//! closed panel costs nothing.
//!
//! A patch sheet's review comments belong to the panel, keyed by the patch they
//! discuss, so closing the sheet keeps the draft and the panel can mark the row
//! that still has one.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
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
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;
use sha2::{Digest, Sha256};

use super::{PaneTarget, Plugin, PluginHost, PluginProject, SendOutcome};
use crate::git::{Change, Changes, Commit, History, Patch};

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
    /// Review comments and the records of reviews already sent, kept across
    /// sheets so closing one does not throw away what the user wrote.
    reviews: Rc<RefCell<ReviewStore>>,
    /// How a patch sheet hands its review comments to a terminal.
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
            reviews: Rc::new(RefCell::new(ReviewStore::default())),
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
        let key = source.key(project);
        let reviews = self.reviews.clone();
        // Sending clears the drafts, which the panel's rows draw; tell it so a
        // row does not wait for the next poll to drop its draft mark.
        let panel = cx.entity().downgrade();
        let notify: Rc<dyn Fn(&mut App)> = Rc::new(move |cx| {
            if let Some(panel) = panel.upgrade() {
                panel.update(cx, |_, cx| cx.notify());
            }
        });
        let view = cx.new(|cx| {
            PatchView::new(
                PatchSheet {
                    project,
                    key,
                    source,
                    message,
                    context,
                    host: self.host.clone(),
                    reviews,
                    notify_panel: notify,
                },
                cx,
            )
        });
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
        mark: Option<AnyElement>,
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
            )
            .children(mark);

        row.into_any_element()
    }
}

/// Where a [`PatchView`] reads its patch from.
#[derive(Clone)]
enum PatchSource {
    /// One commit in the repository at `root`.
    Commit { root: PathBuf, hash: String },
    /// One uncommitted change in the repository at `root`.
    Change { root: PathBuf, change: Change },
}

impl PatchSource {
    /// Where drafts and sent records are filed: one entry per patch the user
    /// can open.
    fn key(&self, project: i64) -> PatchKey {
        match self {
            Self::Commit { hash, .. } => PatchKey::commit(project, hash),
            Self::Change { change, .. } => PatchKey::change(project, change),
        }
    }

    /// Read the patch from git without parsing it.
    fn read(&self) -> Result<Patch, String> {
        match self {
            Self::Commit { root, hash } => crate::git::commit_diff(root, hash),
            Self::Change { root, change } => crate::git::change_diff(root, change),
        }
    }
}

/// One patch's identity inside a project. Drafts and sent records survive a
/// closed sheet under this key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PatchKey {
    project: i64,
    target: String,
}

impl PatchKey {
    fn commit(project: i64, hash: &str) -> Self {
        Self {
            project,
            target: format!("commit:{hash}"),
        }
    }

    fn change(project: i64, change: &Change) -> Self {
        Self {
            project,
            target: format!(
                "change:{}:{}",
                if change.staged { "index" } else { "worktree" },
                change.path
            ),
        }
    }
}

/// One comment as it was when its sheet closed, so reopening the patch brings
/// the editors back with what the user typed.
#[derive(Clone)]
struct DraftComment {
    range: DiffLineRange,
    text: String,
}

/// The last review a patch sent, shown on its row as a receipt.
#[derive(Clone)]
struct SentReview {
    count: usize,
    target: String,
}

/// The panel's review memory: unsent drafts and the reviews already sent.
#[derive(Default)]
struct ReviewStore {
    drafts: HashMap<PatchKey, Vec<DraftComment>>,
    sent: HashMap<PatchKey, SentReview>,
}

/// A snapshot of [`ReviewStore`] the panel's row closures can carry.
#[derive(Default, Clone)]
struct ReviewMarks {
    drafts: HashSet<PatchKey>,
    sent: HashMap<PatchKey, SentReview>,
}

impl ReviewMarks {
    fn of(store: &ReviewStore) -> Self {
        Self {
            drafts: store.drafts.keys().cloned().collect(),
            sent: store.sent.clone(),
        }
    }

    /// The small mark a patch row wears: a draft marker while a comment waits,
    /// or a receipt once a review reached a pane. A draft takes precedence,
    /// because it is the one thing still asking for the user.
    fn mark(&self, key: &PatchKey, theme: &Theme) -> Option<AnyElement> {
        if self.drafts.contains(key) {
            return Some(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.warning)
                    .child("草稿")
                    .into_any_element(),
            );
        }
        self.sent.get(key).map(|_| {
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.success)
                .child("已审查")
                .into_any_element()
        })
    }
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
    /// The project whose pane receives the review comments.
    project: i64,
    /// How this sheet reaches that pane.
    host: PluginHost,
    /// What this patch is, for finding its drafts again.
    key: PatchKey,
    /// The panel's shared review memory.
    reviews: Rc<RefCell<ReviewStore>>,
    /// Tells the panel a row marker changed while the sheet was open.
    notify_panel: Rc<dyn Fn(&mut App)>,
    /// Where the patch came from, so a stale sheet can be read again.
    source: PatchSource,
    /// Review comments, each anchored to the patch lines it discusses.
    comments: Vec<ReviewComment>,
    /// Distinguishes comment element ids after a removal and a new comment.
    next_comment: u64,
    /// The pane the user chose, or `None` for the project's focused pane.
    target: Option<u64>,
    /// A digest of what the sheet read, compared by the watcher to notice the
    /// repository moving under the review.
    digest: Option<String>,
    /// The patch changed since it was read; the sheet keeps showing the old
    /// text until the user asks for the new one.
    stale: bool,
    /// A read is running; a watcher tick does not start a second one.
    reading: bool,
    /// Drops a read that was replaced by a later one.
    read_generation: u64,
    /// What the patch belongs to, so the agent knows which line numbers the
    /// comments refer to.
    context: String,
    /// The last review this patch sent, shown until a new comment is written.
    sent: Option<SentReview>,
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
    /// The latest text, kept even before a textarea exists, so a restored
    /// comment is never mistaken for an empty one.
    text: String,
    /// Created on the first paint, which is the first moment a window exists.
    input: Option<Entity<TextareaState>>,
}

impl ReviewComment {
    fn annotation_id(&self) -> ElementId {
        ElementId::named_usize("git-diff-comment", self.index as usize)
    }
}

/// Why the sheet could not submit, or why it left the text on a shell's
/// command line instead.
enum SendNotice {
    /// The text is on the named pane's command line, where Enter would run it.
    Pasted(String),
    Hint(String),
}

/// What a sheet opens with: the patch it shows, where the patch comes from,
/// and the panel's review memory it reads and writes.
struct PatchSheet {
    project: i64,
    key: PatchKey,
    source: PatchSource,
    /// The commit message, shown above the patch; empty for a change.
    message: String,
    /// Where the patch's line numbers point, named when they are sent.
    context: String,
    host: PluginHost,
    reviews: Rc<RefCell<ReviewStore>>,
    notify_panel: Rc<dyn Fn(&mut App)>,
}

impl PatchView {
    fn new(sheet: PatchSheet, cx: &mut Context<Self>) -> Self {
        let PatchSheet {
            project,
            key,
            source,
            message,
            context,
            host,
            reviews,
            notify_panel,
        } = sheet;
        let targets = host.panes(project, cx);
        let target = targets
            .iter()
            .find(|target| target.focused)
            .or_else(|| targets.first())
            .map(|target| target.pane);
        let drafts = reviews
            .borrow()
            .drafts
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let sent = reviews.borrow().sent.get(&key).cloned();
        let mut comments = Vec::with_capacity(drafts.len());
        for (index, draft) in drafts.into_iter().enumerate() {
            comments.push(ReviewComment {
                index: index as u64,
                range: draft.range,
                text: draft.text,
                input: None,
            });
        }
        let next_comment = comments.len() as u64;
        let mut view = Self {
            project,
            host,
            key,
            reviews,
            notify_panel,
            source,
            comments,
            next_comment,
            target,
            digest: None,
            stale: false,
            reading: false,
            read_generation: 0,
            context,
            sent,
            notice: None,
            message,
            state: None,
            truncated: false,
            error: None,
        };
        view.reload(cx);
        view.watch(cx);
        view
    }

    /// Read the patch again, replacing what the sheet shows. Drafts stay in
    /// their editors; a line that moved keeps the comment where it was written.
    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.reading {
            return;
        }
        self.reading = true;
        self.read_generation += 1;
        let generation = self.read_generation;
        let source = self.source.clone();
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_spawn(async move {
                    let patch = source.read()?;
                    // A cut patch ends in the middle of a hunk, which the
                    // parser takes when it is told to count the lines present.
                    let files = DiffFile::parse_with(
                        &patch.patch,
                        DiffParseOptions::new().truncated_hunks(patch.truncated),
                    )
                    .map_err(|error| format!("无法解析补丁：{error}"))?;
                    Ok::<_, String>((patch, files))
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.read_generation != generation {
                    return;
                }
                view.reading = false;
                match read {
                    Ok((patch, files)) => {
                        let state = cx.new(|cx| DiffState::new(files, cx));
                        // Scrolling and selection notify the state; this view
                        // rebuilds the element so the new frame is painted.
                        cx.observe(&state, |_, _, cx| cx.notify()).detach();
                        view.truncated = patch.truncated;
                        view.digest = Some(patch_digest(&patch));
                        view.stale = false;
                        view.error = None;
                        view.state = Some(state);
                    }
                    Err(error) => view.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Notice a patch that changed since the sheet read it. The watcher never
    /// replaces the diff by itself: yanking the text while someone writes a
    /// comment is worse than an outdated frame with a refresh button.
    fn watch(&self, cx: &mut Context<Self>) {
        let source = self.source.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                // A read already running will bring its own digest; comparing
                // against the old one would call a fresh patch stale.
                let generation = match this.update(cx, |view, _| {
                    (!view.reading).then_some(view.read_generation)
                }) {
                    Ok(Some(generation)) => generation,
                    Ok(None) => continue,
                    Err(_) => break,
                };
                let source = source.clone();
                let digest = cx
                    .background_spawn(
                        async move { source.read().map(|patch| patch_digest(&patch)) },
                    )
                    .await;
                let Ok(digest) = digest else {
                    continue;
                };
                let current = this.update(cx, |view, cx| {
                    if view.read_generation == generation
                        && !view.reading
                        && !view.stale
                        && view.digest.as_ref() != Some(&digest)
                    {
                        view.stale = true;
                        cx.notify();
                    }
                });
                if current.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Give every restored comment its editor. Restored text cannot be put in
    /// a textarea before the sheet has a window, so the first paint is when
    /// they come back.
    fn ensure_editors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let missing: Vec<(usize, u64, String)> = self
            .comments
            .iter()
            .enumerate()
            .filter(|(_, comment)| comment.input.is_none())
            .map(|(position, comment)| (position, comment.index, comment.text.clone()))
            .collect();
        for (position, index, text) in missing {
            let input = self.create_editor(&text, window, cx);
            self.listen(index, &input, cx);
            self.comments[position].input = Some(input);
        }
    }

    fn create_editor(
        &self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextareaState> {
        let input = cx.new(|cx| {
            TextareaState::new(&mut *window, cx)
                .placeholder("写下对这个改动片段的意见")
                .auto_grow(1, 6)
        });
        if !text.is_empty() {
            input.update(cx, |state, cx| {
                state.set_value(text.to_string(), window, cx)
            });
        }
        input
    }

    /// Follow one editor's changes into the comment and the panel's drafts.
    fn listen(&self, index: u64, input: &Entity<TextareaState>, cx: &mut Context<Self>) {
        cx.subscribe(input, move |view, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                view.sync_comment(index, cx);
            }
        })
        .detach();
    }

    fn sync_comment(&mut self, index: u64, cx: &mut Context<Self>) {
        let text = {
            let Some(comment) = self.comments.iter().find(|comment| comment.index == index) else {
                return;
            };
            let Some(input) = comment.input.as_ref() else {
                return;
            };
            input.read(cx).value().to_string()
        };
        if let Some(comment) = self
            .comments
            .iter_mut()
            .find(|comment| comment.index == index)
        {
            comment.text = text;
        }
        // A notice describes the previous text, so editing makes it stale.
        self.notice = None;
        self.save_drafts();
        cx.notify();
    }

    /// Write what the sheet holds into the panel's memory. A comment the user
    /// emptied stops being a draft; the editor stays until the sheet closes.
    fn save_drafts(&self) {
        let drafts = self
            .comments
            .iter()
            .filter_map(|comment| {
                let text = comment.text.trim();
                (!text.is_empty()).then(|| DraftComment {
                    range: comment.range.clone(),
                    text: text.to_string(),
                })
            })
            .collect::<Vec<_>>();
        let mut store = self.reviews.borrow_mut();
        if drafts.is_empty() {
            store.drafts.remove(&self.key);
        } else {
            store.drafts.insert(self.key.clone(), drafts);
        }
    }

    /// Anchor a comment beneath `range`, or focus the comment already there.
    fn add_comment(&mut self, range: DiffLineRange, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(position) = self
            .comments
            .iter()
            .position(|comment| comment.range == range)
        {
            self.ensure_editors(window, cx);
            if let Some(input) = self.comments[position].input.clone() {
                let focus = input.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            }
            return;
        }
        let index = self.next_comment;
        self.next_comment += 1;
        let input = self.create_editor("", window, cx);
        self.listen(index, &input, cx);
        self.notice = None;
        window.focus(&input.read(cx).focus_handle(cx), cx);
        self.comments.push(ReviewComment {
            index,
            range,
            text: String::new(),
            input: Some(input),
        });
        cx.notify();
    }

    fn remove_comment(&mut self, index: u64, cx: &mut Context<Self>) {
        let before = self.comments.len();
        self.comments.retain(|comment| comment.index != index);
        if self.comments.len() != before {
            self.notice = None;
            self.save_drafts();
            cx.notify();
        }
    }

    /// The pane this sheet would send to: the chosen one when it still exists,
    /// otherwise the project's focused pane.
    fn active_target<'a>(&self, targets: &'a [PaneTarget]) -> Option<&'a PaneTarget> {
        self.target
            .and_then(|pane| targets.iter().find(|target| target.pane == pane))
            .or_else(|| targets.iter().find(|target| target.focused))
            .or_else(|| targets.first())
    }

    fn set_target(&mut self, pane: u64, cx: &mut Context<Self>) {
        if self.target != Some(pane) {
            self.target = Some(pane);
            cx.notify();
        }
    }

    /// Hand every non-empty comment to the chosen terminal as one instruction.
    /// Returns whether the terminal took it, which is when the sheet can close.
    fn send_comments(&mut self, cx: &mut Context<Self>) -> bool {
        let comments = self
            .comments
            .iter()
            .filter_map(|comment| {
                // The editor is the source of truth; its Change event is what
                // usually lands in `text`, but a programmatic value may not
                // emit one.
                let text = match comment.input.as_ref() {
                    Some(input) => input.read(cx).value().to_string(),
                    None => comment.text.clone(),
                };
                let text = text.trim().to_string();
                (!text.is_empty()).then(|| (comment.range.clone(), text))
            })
            .collect::<Vec<_>>();
        if comments.is_empty() {
            self.notice = Some(SendNotice::Hint("先写下评论再发送".to_string()));
            cx.notify();
            return false;
        }
        let count = comments.len();
        let message = compose_comments(&self.context, &comments);
        let targets = self.host.panes(self.project, cx);
        let target = self.active_target(&targets).cloned();
        let outcome = match &target {
            Some(target) => self
                .host
                .send_to_pane(self.project, target.pane, &message, cx),
            None => self.host.send_to_focused_pane(self.project, &message, cx),
        };
        match outcome {
            Ok(SendOutcome::Submitted) => {
                let record = SentReview {
                    count,
                    target: target
                        .map(|target| target.label())
                        .unwrap_or_else(|| "当前窗格".to_string()),
                };
                {
                    let mut store = self.reviews.borrow_mut();
                    store.drafts.remove(&self.key);
                    store.sent.insert(self.key.clone(), record.clone());
                }
                self.sent = Some(record);
                (self.notify_panel)(cx);
                self.comments.clear();
                self.notice = None;
                cx.notify();
                true
            }
            // The notes sit on a shell's command line. Keeping the editors
            // lets the user resend them after focusing an agent pane.
            Ok(SendOutcome::Pasted) => {
                let pane = target
                    .map(|target| target.label())
                    .unwrap_or_else(|| "当前窗格".to_string());
                self.notice = Some(SendNotice::Pasted(pane));
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
            Some(SendNotice::Pasted(pane)) => {
                (format!("已粘贴到 {pane}，按 Enter 提交"), theme.warning)
            }
            Some(SendNotice::Hint(problem)) => (problem.clone(), theme.danger),
            None if self.comments.is_empty() => match &self.sent {
                Some(sent) => (
                    format!("上次已发送 {} 条评论到 {}", sent.count, sent.target),
                    theme.muted_foreground,
                ),
                None => (
                    "悬停 diff 行，点 + 添加评论".to_string(),
                    theme.muted_foreground,
                ),
            },
            None => (
                format!("{} 条评论", self.comments.len()),
                theme.muted_foreground,
            ),
        }
    }
}

/// A patch's identity for the watcher: the text and whether it was cut.
fn patch_digest(patch: &Patch) -> String {
    let mut hasher = Sha256::new();
    hasher.update(patch.patch.as_bytes());
    hasher.update([patch.truncated as u8]);
    format!("{:x}", hasher.finalize())
}

impl Render for PatchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_editors(window, cx);
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
                        .filter_map(|comment| {
                            Some((
                                comment.annotation_id(),
                                comment.index,
                                comment.range.clone(),
                                comment.input.clone()?,
                            ))
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
            .when(self.stale, |this| {
                let view = cx.entity().downgrade();
                this.child(
                    h_flex()
                        .w_full()
                        .pb_2()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(theme.warning)
                                .child("补丁已经变化，当前显示的是旧内容"),
                        )
                        .child(
                            Button::new("git-diff-refresh")
                                .small()
                                .ghost()
                                .label("重新读取")
                                .on_click(move |_, _, cx| {
                                    if let Some(view) = view.upgrade() {
                                        view.update(cx, |view, cx| view.reload(cx));
                                    }
                                }),
                        ),
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
        let (count, (hint, color), project, host, target) = {
            let view = patch.read(cx);
            (
                view.comments.len(),
                view.footer_hint(theme),
                view.project,
                view.host.clone(),
                view.target,
            )
        };
        let targets = host.panes(project, cx);
        let active = target
            .and_then(|pane| targets.iter().find(|target| target.pane == pane))
            .or_else(|| targets.iter().find(|target| target.focused))
            .or_else(|| targets.first())
            .map(|target| (target.pane, target.label()));
        let target_button = active.as_ref().map(|(active_pane, label)| {
            let active_pane = *active_pane;
            let label = label.clone();
            let targets = targets.clone();
            let patch = patch.clone();
            Button::new("git-diff-target")
                .small()
                .ghost()
                .icon(Icon::new(IconName::ChevronDown).xsmall())
                .label(label)
                .tooltip("选择接收审查意见的窗格")
                .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
                    let mut menu = menu;
                    for target in &targets {
                        let patch = patch.clone();
                        let pane = target.pane;
                        menu = menu.item(
                            PopupMenuItem::new(target.label())
                                .checked(pane == active_pane)
                                .on_click(move |_, _, cx| {
                                    patch.update(cx, |view, cx| view.set_target(pane, cx));
                                }),
                        );
                    }
                    menu
                })
        });
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
            .when(count > 0, |this| this.children(target_button))
            .when(count > 0, |this| {
                this.child(
                    Button::new("git-diff-send")
                        .small()
                        .primary()
                        .label("发送")
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
        let project_id = self.project.as_ref().map(|project| project.id);
        let marks = {
            let store = self.reviews.borrow();
            Rc::new(ReviewMarks::of(&store))
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
                let marks = marks.clone();
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
                            let mark = match (row, project_id) {
                                (ChangeRow::File(change), Some(project)) => {
                                    marks.mark(&PatchKey::change(project, change), theme)
                                }
                                _ => None,
                            };
                            Self::render_change(
                                row,
                                theme,
                                mark,
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
            let marks = marks.clone();
            uniform_list("git-log-list", visible, move |range, _window, cx| {
                let theme = cx.theme();
                range
                    .map(|row| {
                        let index = filter.as_ref().map_or(row, |filter| filter[row]);
                        let commit = &commits[index];
                        let panel = panel.clone();
                        let clicked = commit.clone();
                        let mark = project_id.and_then(|project| {
                            marks.mark(&PatchKey::commit(project, &commit.hash), theme)
                        });
                        Self::render_commit(
                            row,
                            visible,
                            commit,
                            theme,
                            mark,
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
        mark: Option<AnyElement>,
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
            .children(mark)
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

    use super::{PatchSource, PatchView, Plugin, ReviewStore, compose_comments};
    use gpui_kit::component::diff::{DiffLineRange, DiffSide};

    /// A host whose terminal is never asked for text in these tests.
    fn test_host() -> super::PluginHost {
        super::PluginHost::with_panes(
            |_, _, _, _| Ok(super::SendOutcome::Pasted),
            |_, _| Vec::new(),
        )
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
        host: super::PluginHost,
        store: Rc<RefCell<ReviewStore>>,
    ) -> Entity<PatchView> {
        let key = source.key(project);
        let message = message.to_string();
        let context = context.to_string();
        cx.new(|cx| {
            PatchView::new(
                super::PatchSheet {
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
        let host = super::PluginHost::with_panes(
            {
                let sent = sent.clone();
                move |project, pane, text, _| {
                    *sent.borrow_mut() = Some((project, pane, text.to_string()));
                    Ok(super::SendOutcome::Submitted)
                }
            },
            |_, _| {
                vec![
                    super::PaneTarget {
                        pane: 7,
                        ordinal: 1,
                        title: "codex".to_string(),
                        agent: Some("codex".to_string()),
                        focused: true,
                    },
                    super::PaneTarget {
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
                "请根据以下代码审查意见修改（提交 abc1234）：\n\nsrc/git.rs:12-14\n这里应该先检查状态".to_string()
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
        first.update(cx, |view, _| {
            view.comments.push(super::ReviewComment {
                index: 0,
                range: DiffLineRange::new("src/git.rs", DiffSide::Modified, 12, 14),
                text: "这里应该先检查状态".to_string(),
                input: None,
            });
            view.next_comment = 1;
            view.save_drafts();
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

    struct FooterHost {
        footer: Entity<super::PatchFooter>,
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
        let host = super::PluginHost::with_panes(
            |_, _, _, _| Ok(super::SendOutcome::Submitted),
            |_, _| {
                vec![
                    super::PaneTarget {
                        pane: 1,
                        ordinal: 1,
                        title: "codex".to_string(),
                        agent: Some("codex".to_string()),
                        focused: true,
                    },
                    super::PaneTarget {
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
        let footer = cx.new(|cx| super::PatchFooter::new(view.clone(), cx));
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
