//! The Git panel rendered in Pure Rust NSH (`nsh-platform-gpui`): uncommitted
//! changes and virtualized commit history, with an NSH `DiffView` review sheet.
//!
//! The panel reads both on the background executor and polls while it is
//! alive: a poll reads a cheap ref fingerprint and the status listing, and only
//! takes the full log when a ref moved.

mod patch_view;
mod review;
#[cfg(test)]
mod tests;
mod view;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription,
    Task, Window, div,
};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use self::patch_view::PatchView;
use self::review::{PatchRequest, PatchSheet, PatchSource, ReviewMarks, ReviewStore};
use super::{Plugin, PluginHost, PluginProject};
use crate::git::{Change, Changes, Commit, History};
use crate::nsh_ui::theme::{appearance, nsh_theme};
use crate::theme::KumuTheme;

/// How often an open panel checks whether the repository moved.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Commit row height for `virtual_list`.
const ROW_HEIGHT: f32 = 32.;

/// Change row and section header height for `virtual_list`.
const CHANGE_ROW_HEIGHT: f32 = 28.;

/// Which of the panel's two lists is showing.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum PanelTab {
    #[default]
    Changes,
    History,
}

/// A row of the changes tab: a section heading or one file.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ChangeRow {
    Header(&'static str),
    File(Change),
}

/// Immutable snapshot pushed from the GPUI host to the Pure Rust NSH Git view.
#[derive(Clone, Default)]
struct GitPanelSnapshot {
    project_id: Option<i64>,
    tab: PanelTab,
    branch: Option<String>,
    loaded: bool,
    error: Option<String>,
    change_count: usize,
    change_rows: Vec<ChangeRow>,
    commits: Arc<Vec<Commit>>,
    filter: Option<Arc<Vec<usize>>>,
    marks: ReviewMarks,
}

#[derive(Clone)]
enum GitHostEvent {
    Snapshot(GitPanelSnapshot),
}

#[derive(Clone, Debug)]
enum GitPanelAction {
    SelectTab(PanelTab),
    SetQuery(String),
    OpenChange(Change),
    OpenCommit(Commit),
}

struct GitPanelMount {
    initial: GitPanelSnapshot,
    events_rx: UnboundedReceiver<GitHostEvent>,
    actions_tx: UnboundedSender<GitPanelAction>,
}

static NEXT_MOUNT_ID: AtomicU64 = AtomicU64::new(1);
static PENDING_MOUNTS: OnceLock<Mutex<HashMap<u64, GitPanelMount>>> = OnceLock::new();

fn pending_mounts() -> &'static Mutex<HashMap<u64, GitPanelMount>> {
    PENDING_MOUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_panel_mount(mount: GitPanelMount) -> u64 {
    let id = NEXT_MOUNT_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut map) = pending_mounts().lock() {
        map.insert(id, mount);
    }
    id
}

fn take_panel_mount(id: u64) -> Option<GitPanelMount> {
    pending_mounts().lock().ok()?.remove(&id)
}

struct RepoRead {
    fingerprint: String,
    changes: Changes,
    history: Option<History>,
}

pub struct GitLogPanel {
    mount_id: u64,
    project: Option<PluginProject>,
    history: Option<History>,
    commits: Arc<Vec<Commit>>,
    changes: Changes,
    tab: PanelTab,
    fingerprint: Option<String>,
    filter: Option<Arc<Vec<usize>>>,
    filter_generation: u64,
    query: String,
    error: Option<String>,
    loading: bool,
    dirty: bool,
    generation: u64,
    reviews: Rc<RefCell<ReviewStore>>,
    host: PluginHost,
    app_view: Entity<AppView>,
    events_tx: UnboundedSender<GitHostEvent>,
    _theme_sub: Subscription,
    _actions_task: Task<()>,
}

impl Drop for GitLogPanel {
    fn drop(&mut self) {
        let _ = take_panel_mount(self.mount_id);
    }
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
        let (events_tx, events_rx) = unbounded::<GitHostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<GitPanelAction>();

        let initial = GitPanelSnapshot {
            project_id: project.as_ref().map(|p| p.id),
            tab: PanelTab::Changes,
            ..Default::default()
        };
        let mount_id = register_panel_mount(GitPanelMount {
            initial,
            events_rx,
            actions_tx,
        });

        let kumu_theme = KumuTheme::global(cx);
        let initial_nsh_theme = nsh_theme(kumu_theme);
        let initial_appearance = appearance(kumu_theme.mode);

        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu Git".to_owned(),
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.set_theme(Some(initial_nsh_theme), cx);
            view.set_appearance(initial_appearance, cx);
            view.load(nsh_platform_gpui::App::Rust(Root::new::<view::GitLogRoot>(
                move || view::GitLogRootProps {
                    mount_id: mount_id.into(),
                },
            )));
            view
        });

        let theme_sub = cx.observe_global::<KumuTheme>(|panel: &mut Self, cx| {
            let kumu_theme = KumuTheme::global(cx);
            let (mapped, mode) = (nsh_theme(kumu_theme), appearance(kumu_theme.mode));
            panel.app_view.update(cx, |view, cx| {
                view.set_theme(Some(mapped), cx);
                view.set_appearance(mode, cx);
            });
        });

        let actions_task = cx.spawn_in(window, async move |this, cx| {
            while let Some(action) = actions_rx.next().await {
                let res = this.update_in(cx, |panel, window, cx| match action {
                    GitPanelAction::SelectTab(tab) => {
                        if panel.tab != tab {
                            panel.tab = tab;
                            panel.push_snapshot();
                            cx.notify();
                        }
                    }
                    GitPanelAction::SetQuery(query) => {
                        panel.query = query;
                        panel.rebuild_filter(cx);
                        panel.push_snapshot();
                    }
                    GitPanelAction::OpenChange(change) => {
                        panel.open_change(change, window, cx);
                    }
                    GitPanelAction::OpenCommit(commit) => {
                        panel.open_commit(commit, window, cx);
                    }
                });
                if res.is_err() {
                    break;
                }
            }
        });

        let mut panel = Self {
            mount_id,
            project,
            history: None,
            commits: Arc::default(),
            changes: Changes::default(),
            tab: PanelTab::Changes,
            fingerprint: None,
            filter: None,
            filter_generation: 0,
            query: String::new(),
            error: None,
            loading: false,
            dirty: false,
            generation: 0,
            reviews: Rc::new(RefCell::new(ReviewStore::default())),
            host,
            app_view,
            events_tx,
            _theme_sub: theme_sub,
            _actions_task: actions_task,
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
            self.dirty = true;
        } else {
            self.reload(cx);
        }
        self.push_snapshot();
        cx.notify();
    }
}

impl GitLogPanel {
    fn watch(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut interval = REFRESH_INTERVAL;
            loop {
                cx.background_executor().timer(interval).await;
                let next = this.update(cx, |panel, cx| {
                    if !panel.loading {
                        panel.reload(cx);
                    }
                    if panel.error.is_some() {
                        Duration::from_secs(10)
                    } else {
                        REFRESH_INTERVAL
                    }
                });
                match next {
                    Ok(wait) => interval = wait,
                    Err(_) => break,
                }
            }
        })
        .detach();
    }

    fn build_snapshot(&self) -> GitPanelSnapshot {
        let marks = {
            let store = self.reviews.borrow();
            ReviewMarks::of(&store)
        };
        GitPanelSnapshot {
            project_id: self.project.as_ref().map(|p| p.id),
            tab: self.tab,
            branch: self.history.as_ref().map(|h| h.branch.clone()),
            loaded: self.history.is_some(),
            error: self.error.clone(),
            change_count: self.changes.len(),
            change_rows: self.change_rows(),
            commits: self.commits.clone(),
            filter: self.filter.clone(),
            marks,
        }
    }

    fn push_snapshot(&self) {
        let _ = self
            .events_tx
            .unbounded_send(GitHostEvent::Snapshot(self.build_snapshot()));
    }

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
                    let fingerprint = crate::git::fingerprint(&project.path)?;
                    let history = if known.as_deref() == Some(fingerprint.as_str()) {
                        None
                    } else {
                        Some(crate::git::history(&project.path)?)
                    };
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

    fn apply(&mut self, read: Result<RepoRead, String>, cx: &mut Context<Self>) {
        match read {
            Ok(read) => {
                let mut changed = self.error.take().is_some();
                if let Some(history) = read.history {
                    self.fingerprint = Some(read.fingerprint);
                    if self.history.as_ref() != Some(&history) {
                        self.commits = history.commits.clone();
                        self.history = Some(history);
                        self.filter = if self.query.trim().is_empty() {
                            None
                        } else {
                            Some(Arc::new(Vec::new()))
                        };
                        self.rebuild_filter(cx);
                        changed = true;
                    }
                }
                if self.changes != read.changes {
                    self.changes = read.changes;
                    changed = true;
                }
                if changed {
                    self.push_snapshot();
                    cx.notify();
                }
            }
            Err(error) => {
                self.clear_rows();
                if self.error.as_ref() != Some(&error) {
                    self.error = Some(error);
                    self.push_snapshot();
                    cx.notify();
                }
            }
        }
    }

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
            self.filter = Some(Arc::new(Vec::new()));
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let still_current = this
                .read_with(cx, |panel, _| panel.filter_generation == generation)
                .unwrap_or(false);
            if !still_current {
                return;
            }
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
                    panel.filter = Some(Arc::new(matched));
                    panel.push_snapshot();
                    cx.notify();
                }
            });
        })
        .detach();
    }

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
                context: format!("提交 {}", commit.short),
            },
            window,
            cx,
        );
    }

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
        let panel = cx.entity().downgrade();
        let notify: Rc<dyn Fn(&mut App)> = Rc::new(move |cx| {
            if let Some(panel) = panel.upgrade() {
                panel.update(cx, |panel, cx| {
                    panel.push_snapshot();
                    cx.notify();
                });
            }
        });
        let view = cx.new(|cx| {
            let mut view = PatchView::new(
                PatchSheet {
                    project,
                    title,
                    meta,
                    key,
                    source,
                    message,
                    context,
                    host: self.host.clone(),
                    reviews,
                    notify_panel: notify,
                },
                cx,
            );
            view.ensure_nsh_mounted(window, cx);
            view
        });
        self.host.open_sheet(view.into(), window, cx);
    }

    /// The changes tab's rows: a heading above each non-empty section.
    fn change_rows(&self) -> Vec<ChangeRow> {
        change_rows_from(&self.changes)
    }
}

fn change_rows_from(changes: &Changes) -> Vec<ChangeRow> {
    if changes.is_empty() {
        return Vec::new();
    }
    let mut rows = Vec::with_capacity(changes.len() + 2);
    for (label, list) in [("已暂存", &changes.staged), ("未暂存", &changes.unstaged)] {
        if list.is_empty() {
            continue;
        }
        rows.push(ChangeRow::Header(label));
        rows.extend(list.iter().cloned().map(ChangeRow::File));
    }
    rows
}

impl Render for GitLogPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.app_view.clone())
    }
}

/// The last component of a repository-relative path.
pub(super) fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Everything before a repository-relative path's last component, or an empty
/// string for a file at the root.
pub(super) fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
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
