//! The Git panel: the selected project's commit history.
//!
//! The panel reads the log on the background executor and polls while it is
//! alive, but a poll reads a cheap fingerprint first and only takes the full
//! log when a ref moved. Rows stay virtualized, so a repository with a long
//! history costs the same to draw as a short one. The home window creates a
//! panel when the user opens it and drops it when they close it, so a closed
//! panel costs nothing.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::hover_card::HoverCard;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{Plugin, PluginProject};
use crate::git::{Commit, History};

/// How often an open panel checks whether the repository moved. Polling rather
/// than a filesystem watcher stays correct for commits a terminal's git
/// commands make without adding a platform watcher to the app.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Every commit row has the same height, which is what lets the uniform list
/// virtualize a full history. One compact line: refs, hash and subject.
const ROW_HEIGHT: f32 = 32.;

pub struct GitLogPanel {
    project: Option<PluginProject>,
    history: Option<History>,
    /// The same commits as `history`, shared with the list closure.
    commits: Arc<Vec<Commit>>,
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
    const TITLE: &'static str = "Git 历史";
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
                    if known.as_deref() == Some(fingerprint.as_str()) {
                        return Ok(None);
                    }
                    crate::git::history(&project.path).map(|history| Some((fingerprint, history)))
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

    /// Take one read: nothing new, a new history, or the error to show instead.
    fn apply(&mut self, read: Result<Option<(String, History)>, String>, cx: &mut Context<Self>) {
        match read {
            // Same fingerprint: nothing the panel draws has changed.
            Ok(None) => {}
            Ok(Some((fingerprint, history))) => {
                self.fingerprint = Some(fingerprint);
                let mut changed = self.error.take().is_some();
                if self.history.as_ref() != Some(&history) {
                    self.commits = history.commits.clone();
                    self.history = Some(history);
                    // The filter indexes the commits that just went away.
                    self.filter = None;
                    self.rebuild_filter(cx);
                    changed = true;
                }
                if changed {
                    cx.notify();
                }
            }
            Err(error) => {
                // A failed read says nothing about the fingerprint, so the next
                // poll tries the log again.
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

    fn render_commit(index: usize, total: usize, commit: &Commit, theme: &Theme) -> AnyElement {
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

        let detail = commit.clone();
        HoverCard::new(format!("git-log-hover-{}", commit.hash))
            .anchor(Anchor::RightCenter)
            .open_delay(Duration::from_millis(250))
            .trigger_style(StyleRefinement::default().w_full())
            .trigger(row)
            .content(move |_, _, cx| {
                let theme = cx.theme();
                commit_detail(&detail, theme)
            })
            .into_any_element()
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

        let toolbar = self.project.is_some().then(|| {
            v_flex()
                .child(
                    h_flex()
                        .px_3()
                        .pt_1p5()
                        .pb_1()
                        .gap_1p5()
                        .items_center()
                        .child(div().flex_none().text_xs().text_color(muted).child("分支"))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .font_medium()
                                .child(branch.unwrap_or_else(|| "—".to_string())),
                        )
                        .when(total > 0, |row| {
                            row.child(div().flex_none().text_xs().text_color(muted).child(
                                if visible == total {
                                    format!("{total} 个提交")
                                } else {
                                    format!("{visible} / {total}")
                                },
                            ))
                        }),
                )
                .child(
                    div()
                        .px_2()
                        .pb_1p5()
                        .border_b_1()
                        .border_color(border)
                        .child(
                            Input::new(&self.search)
                                .small()
                                .cleanable(true)
                                .prefix(Icon::new(IconName::Search).small().text_color(muted)),
                        ),
                )
        });

        let body: AnyElement = if self.project.is_none() {
            empty("没有选中的项目", muted)
        } else if let Some(error) = &self.error {
            empty(error, theme.danger)
        } else if self.history.is_none() {
            empty("正在读取…", muted)
        } else if visible == 0 {
            if self.filter.is_some() {
                empty("没有匹配的提交", muted)
            } else {
                empty("这个仓库还没有提交", muted)
            }
        } else {
            let commits = self.commits.clone();
            let filter = self.filter.clone();
            uniform_list("git-log-list", visible, move |range, _window, cx| {
                let theme = cx.theme();
                range
                    .map(|row| {
                        let index = filter.as_ref().map_or(row, |filter| filter[row]);
                        Self::render_commit(row, visible, &commits[index], theme)
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

/// The hover card: the whole message and who wrote it.
fn commit_detail(commit: &Commit, theme: &Theme) -> Div {
    let muted = theme.muted_foreground;
    let body = commit.body.trim_end();
    v_flex()
        .w(rems(20.))
        .gap_2()
        .child(div().text_sm().font_medium().child(commit.subject.clone()))
        .when(!body.is_empty(), |card| {
            card.child(
                div()
                    .id("git-log-detail-body")
                    .max_h(rems(14.))
                    .overflow_y_scroll()
                    .text_sm()
                    .text_color(muted)
                    .child(body.to_string()),
            )
        })
        .child(
            v_flex()
                .gap_0p5()
                .border_t_1()
                .border_color(theme.border)
                .pt_2()
                .child(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .child(div().text_xs().font_medium().child(commit.author.clone()))
                        .child(div().text_xs().text_color(muted).child("·"))
                        .child(div().text_xs().text_color(muted).child(commit.date.clone())),
                )
                .when(!commit.email.is_empty(), |meta| {
                    meta.child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(commit.email.clone()),
                    )
                })
                .child(
                    div()
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .text_color(theme.link)
                        .child(commit.short.clone()),
                ),
        )
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
