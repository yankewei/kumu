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

use gpui_kit::component::diff::{Diff, DiffFile, DiffParseOptions, DiffState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{Plugin, PluginProject};
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
}

impl Plugin for GitLogPanel {
    const ID: &'static str = "git-log";
    const TITLE: &'static str = "Git";
    const ICON: &'static str = "icons/git-branch.svg";

    fn new(project: Option<PluginProject>, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        Self::open_patch(
            commit.subject.clone(),
            meta,
            PatchSource::Commit {
                root: project.path,
                hash: commit.hash.clone(),
            },
            commit.body.clone(),
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
        let mut meta = format!(
            "{} · {}",
            if change.staged {
                "已暂存"
            } else {
                "未暂存"
            },
            status_label(change.status)
        );
        if let Some(original) = &change.original {
            meta.push_str(&format!(" · 原路径 {original}"));
        }
        Self::open_patch(
            change.path.clone(),
            meta,
            PatchSource::Change {
                root: project.path,
                change,
            },
            String::new(),
            window,
            cx,
        );
    }

    /// Open a sheet around a [`PatchView`], which reads the patch itself.
    fn open_patch(
        title: String,
        meta: String,
        source: PatchSource,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.new(|cx| PatchView::new(source, message, cx));
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

/// The sheet body: one patch. The read and the patch parse run on the
/// background executor; the diff state is built on the UI thread once the files
/// are in hand, because it prepares syntax off-thread itself.
struct PatchView {
    /// The commit message, shown above the patch; empty for a change.
    message: String,
    state: Option<Entity<DiffState>>,
    truncated: bool,
    error: Option<String>,
}

impl PatchView {
    fn new(source: PatchSource, message: String, cx: &mut Context<Self>) -> Self {
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
            message,
            state: None,
            truncated: false,
            error: None,
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
            (None, Some(state)) => div()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(Diff::new(state).size_full())
                .into_any_element(),
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
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use gpui_kit::{
        AppContext, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
        TestAppContext, Window, div,
    };

    use super::{PatchSource, PatchView, Plugin};

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
        let view = cx.new(|cx| PatchView::new(source, commit.body.clone(), cx));
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
        // what proves the parsed patch reaches the diff component.
        let (_, cx) = cx.add_window_view(|_, _| Host { view: view.clone() });
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
        let view = cx.new(|cx| PatchView::new(source, String::new(), cx));
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
            let panel = cx.new(|cx| super::GitLogPanel::new(Some(project.clone()), window, cx));
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
