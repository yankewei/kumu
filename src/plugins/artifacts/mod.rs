//! Artifacts drawer: every artifact published under `<project>/.kumu`, grouped
//! by the agent session that published it. Opening one mounts a reading sheet.
//!
//! The CLI owns writes. This panel only scans the directory and keeps a small
//! in-memory snapshot for the NSH list; it never writes artifact content.

mod sheet;
mod view;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription,
    Task, Window, div,
};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use self::sheet::ArtifactSheet;
use super::{ActiveSheet, PaneTarget, Plugin, PluginHost, PluginProject};
use crate::artifacts::{self, Artifact, ArtifactKind, session_key};
use crate::nsh_ui::mount::MountRegistry;
use crate::nsh_ui::theme::{appearance, nsh_theme};
use crate::theme::KumuTheme;

/// Plugin registry id and the key the title-bar button uses.
pub const ID: &str = "artifacts";
const TITLE: &str = "Artifacts";
const ICON: &str = "icons/artifact.svg";

/// Closing the reading sheet; the opener decides how the overlay is unmounted.
pub(crate) type CloseFn = dyn Fn(&mut Window, &mut App);

/// How often an open drawer rescans the directory. A publish also triggers an
/// immediate refresh through the control channel, so this only catches edits
/// made outside the CLI.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ArtifactFilter {
    #[default]
    All,
    CurrentSession,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ArtifactRow {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) kind: ArtifactKind,
    pub(super) updated_at: String,
    pub(super) updated: String,
    pub(super) size: String,
    pub(super) entry: PathBuf,
    pub(super) dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ArtifactGroup {
    pub(super) key: String,
    pub(super) title: String,
    pub(super) meta: String,
    pub(super) current: bool,
    pub(super) ended: bool,
    /// Current session, other live sessions, ended sessions, unattached.
    pub(super) rank: u8,
    pub(super) rows: Vec<ArtifactRow>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ArtifactsSnapshot {
    pub(super) project_id: Option<i64>,
    pub(super) loaded: bool,
    pub(super) error: Option<String>,
    pub(super) filter: ArtifactFilter,
    pub(super) groups: Vec<ArtifactGroup>,
    pub(super) total: usize,
}

#[derive(Clone, Debug)]
pub(super) enum ArtifactsAction {
    Reload,
    SetQuery(String),
    SetFilter(ArtifactFilter),
    Open(String),
    Reveal(PathBuf),
    CopyPath(PathBuf),
    CopyText(String),
    Remove(String),
    OpenExternal(PathBuf),
}

pub(super) struct ArtifactsMount {
    pub(super) initial: ArtifactsSnapshot,
    pub(super) events_rx: UnboundedReceiver<ArtifactsSnapshot>,
    pub(super) actions_tx: UnboundedSender<ArtifactsAction>,
}

static MOUNTS: MountRegistry<ArtifactsMount> = MountRegistry::new();

pub(super) fn register_mount(mount: ArtifactsMount) -> u64 {
    MOUNTS.register(mount)
}

pub(super) fn take_mount(id: u64) -> Option<ArtifactsMount> {
    MOUNTS.take(id)
}

pub struct ArtifactsPanel {
    mount_id: u64,
    project: Option<PluginProject>,
    artifacts: Vec<Artifact>,
    query: String,
    filter: ArtifactFilter,
    loaded: bool,
    error: Option<String>,
    loading: bool,
    dirty: bool,
    generation: u64,
    host: PluginHost,
    app_view: Entity<AppView>,
    events_tx: UnboundedSender<ArtifactsSnapshot>,
    _theme_sub: Subscription,
    _actions_task: Task<()>,
}

impl Drop for ArtifactsPanel {
    fn drop(&mut self) {
        let _ = take_mount(self.mount_id);
    }
}

impl Plugin for ArtifactsPanel {
    const ID: &'static str = ID;
    const TITLE: &'static str = TITLE;
    const ICON: &'static str = ICON;

    fn new(
        project: Option<PluginProject>,
        host: PluginHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (events_tx, events_rx) = unbounded::<ArtifactsSnapshot>();
        let (actions_tx, mut actions_rx) = unbounded::<ArtifactsAction>();

        let initial = ArtifactsSnapshot {
            project_id: project.as_ref().map(|project| project.id),
            ..Default::default()
        };
        let mount_id = register_mount(ArtifactsMount {
            initial,
            events_rx,
            actions_tx,
        });

        let kumu_theme = KumuTheme::global(cx);
        let initial_theme = nsh_theme(kumu_theme);
        let initial_appearance = appearance(kumu_theme.mode);
        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu Artifacts".to_owned(),
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.set_theme(Some(initial_theme), cx);
            view.set_appearance(initial_appearance, cx);
            view.load(nsh_platform_gpui::App::Rust(
                Root::new::<view::ArtifactsRoot>(move || view::ArtifactsRootProps {
                    mount_id: mount_id.into(),
                }),
            ));
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
                let result = this.update_in(cx, |panel, window, cx| match action {
                    ArtifactsAction::Reload => panel.reload(cx),
                    ArtifactsAction::SetQuery(query) => {
                        panel.query = query;
                        panel.push_snapshot(cx);
                    }
                    ArtifactsAction::SetFilter(filter) => {
                        panel.filter = filter;
                        panel.push_snapshot(cx);
                    }
                    ArtifactsAction::Open(id) => panel.open(&id, window, cx),
                    ArtifactsAction::Reveal(path) => cx.reveal_path(&path),
                    ArtifactsAction::CopyPath(path) => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                            path.to_string_lossy().into_owned(),
                        ));
                    }
                    ArtifactsAction::CopyText(text) => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                    }
                    ArtifactsAction::Remove(id) => panel.remove(&id, cx),
                    ArtifactsAction::OpenExternal(path) => open_external(&path),
                });
                if result.is_err() {
                    break;
                }
            }
        });

        let mut panel = Self {
            mount_id,
            project,
            artifacts: Vec::new(),
            query: String::new(),
            filter: ArtifactFilter::All,
            loaded: false,
            error: None,
            loading: false,
            dirty: false,
            generation: 0,
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
        self.artifacts.clear();
        self.loaded = false;
        self.error = None;
        if self.loading {
            self.dirty = true;
        } else {
            self.reload(cx);
        }
        self.push_snapshot(cx);
        cx.notify();
    }
}

impl ArtifactsPanel {
    /// Rescan after a publish notification from the control channel.
    pub fn refresh_now(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    fn watch(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                let keep = this.update(cx, |panel, cx| {
                    if !panel.loading {
                        panel.reload(cx);
                    }
                });
                if keep.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            self.loaded = true;
            self.push_snapshot(cx);
            return;
        };
        if self.loading {
            self.dirty = true;
            return;
        }
        self.loading = true;
        let generation = self.generation;
        let root = project.path.clone();
        cx.spawn(async move |this, cx| {
            let scanned = cx
                .background_spawn(async move { artifacts::scan(&root) })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.loading = false;
                if panel.generation == generation {
                    let changed =
                        !panel.loaded || panel.error.is_some() || panel.artifacts != scanned;
                    panel.artifacts = scanned;
                    panel.loaded = true;
                    panel.error = None;
                    if changed {
                        panel.push_snapshot(cx);
                        cx.notify();
                    }
                }
                if panel.dirty {
                    panel.dirty = false;
                    panel.reload(cx);
                }
            });
        })
        .detach();
    }

    fn push_snapshot(&self, cx: &App) {
        let _ = self.events_tx.unbounded_send(self.build_snapshot(cx));
    }

    fn build_snapshot(&self, cx: &App) -> ArtifactsSnapshot {
        let panes = self
            .project
            .as_ref()
            .map(|project| self.host.panes(project.id, cx))
            .unwrap_or_default();
        let current_key = panes
            .iter()
            .find(|pane| pane.focused)
            .map(pane_key)
            .or_else(|| panes.first().map(pane_key));
        let live_keys: HashSet<String> = panes.iter().map(pane_key).collect();
        let groups = group_artifacts(
            &self.artifacts,
            &panes,
            current_key.as_deref(),
            &live_keys,
            self.filter,
            &self.query,
        );
        ArtifactsSnapshot {
            project_id: self.project.as_ref().map(|project| project.id),
            loaded: self.loaded,
            error: self.error.clone(),
            filter: self.filter,
            groups,
            total: self.artifacts.len(),
        }
    }

    fn open(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        let Some(artifact) = self
            .artifacts
            .iter()
            .find(|artifact| artifact.id() == id)
            .cloned()
        else {
            return;
        };
        let close_host = self.host.clone();
        let on_close: Rc<CloseFn> = Rc::new(move |window, cx| close_host.close_sheet(window, cx));
        let sheet = reading_sheet(project.path, artifact, on_close, window, cx);
        self.host.open_sheet(sheet, window, cx);
    }

    fn remove(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(project) = self.project.as_ref() else {
            return;
        };
        match artifacts::move_to_trash(&project.path, id) {
            Ok(_) => {
                self.artifacts.retain(|artifact| artifact.id() != id);
                self.error = None;
                self.push_snapshot(cx);
                cx.notify();
            }
            Err(error) => {
                self.error = Some(error.to_string());
                self.push_snapshot(cx);
                cx.notify();
            }
        }
    }
}

impl Render for ArtifactsPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.app_view.clone())
    }
}

/// Build the full-window reading sheet for `artifact`. Shared by the drawer
/// and by `kumu artifact open` control requests, which have no panel mounted.
pub fn reading_sheet(
    project_root: PathBuf,
    artifact: Artifact,
    on_close: std::rc::Rc<CloseFn>,
    window: &mut Window,
    cx: &mut App,
) -> ActiveSheet {
    let view = cx.new(|cx| ArtifactSheet::new(artifact, project_root, on_close, cx));
    view.update(cx, |sheet, cx| sheet.ensure_nsh_mounted(window, cx));
    let dismiss = {
        let weak = view.downgrade();
        move |cx: &mut App| {
            let _ = weak.update(cx, |sheet, _| sheet.request_close());
        }
    };
    ActiveSheet::new(view.into(), dismiss)
}

fn pane_key(pane: &PaneTarget) -> String {
    session_key(
        Some(pane.pane),
        pane.agent.as_deref(),
        pane.agent_session_id.as_deref(),
    )
}

/// Group, filter, and order artifacts for the drawer. Pure so the ordering
/// rules stay testable without a window.
pub(super) fn group_artifacts(
    artifacts: &[Artifact],
    panes: &[PaneTarget],
    current_key: Option<&str>,
    live_keys: &HashSet<String>,
    filter: ArtifactFilter,
    query: &str,
) -> Vec<ArtifactGroup> {
    let query = query.trim().to_lowercase();
    let mut by_key: HashMap<String, ArtifactGroup> = HashMap::new();
    for artifact in artifacts {
        let meta = &artifact.meta;
        if filter == ArtifactFilter::CurrentSession {
            let Some(current_key) = current_key else {
                continue;
            };
            if meta.session.key != current_key {
                continue;
            }
        }
        if !query.is_empty() {
            let haystack = format!(
                "{} {} {}",
                meta.title.to_lowercase(),
                meta.id,
                meta.kind.as_str()
            );
            if !haystack.contains(&query) {
                continue;
            }
        }
        let key = meta.session.key.clone();
        let live = live_keys.contains(&key);
        let current = current_key == Some(key.as_str());
        let unattached = meta.session.is_unattached();
        let rank = if unattached {
            3
        } else if current {
            0
        } else if live {
            1
        } else {
            2
        };
        let pane = panes.iter().find(|pane| pane_key(pane) == key);
        let title = if unattached {
            "未关联".to_string()
        } else {
            meta.session
                .title
                .clone()
                .filter(|title| !title.trim().is_empty())
                .or_else(|| pane.map(|pane| pane.title.clone()))
                .or_else(|| meta.session.agent.as_deref().map(agent_name))
                .unwrap_or_else(|| "Agent 会话".to_string())
        };
        let mut parts = Vec::new();
        if unattached {
            parts.push("不在 Kumu 窗格内发布".to_string());
        } else {
            if let Some(agent) = meta.session.agent.as_deref() {
                parts.push(agent_name(agent));
            }
            if let Some(tab) = meta
                .session
                .tab_name
                .as_deref()
                .filter(|tab| !tab.is_empty())
            {
                parts.push(tab.to_string());
            }
            if let Some(pane) = meta.session.pane {
                parts.push(format!("窗格 {pane}"));
            }
            if !live {
                parts.push("已结束".to_string());
            }
        }
        let group = by_key.entry(key.clone()).or_insert_with(|| ArtifactGroup {
            key,
            title,
            meta: parts.join(" · "),
            current,
            ended: !live && !unattached,
            rank,
            rows: Vec::new(),
        });
        group.rows.push(ArtifactRow {
            id: meta.id.clone(),
            title: meta.title.clone(),
            kind: meta.kind,
            updated_at: meta.updated_at.clone(),
            updated: relative_time(&meta.updated_at),
            size: format_size(entry_size(artifact)),
            entry: artifact.entry_path(),
            dir: artifact.dir.clone(),
        });
    }
    let mut groups: Vec<ArtifactGroup> = by_key.into_values().collect();
    for group in &mut groups {
        group.rows.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.id.cmp(&a.id))
        });
    }
    groups.sort_by(|a, b| {
        a.rank.cmp(&b.rank).then_with(|| {
            let a = a
                .rows
                .first()
                .map(|row| row.updated_at.as_str())
                .unwrap_or("");
            let b = b
                .rows
                .first()
                .map(|row| row.updated_at.as_str())
                .unwrap_or("");
            b.cmp(a)
        })
    });
    groups
}

fn agent_name(id: &str) -> String {
    crate::agent::AGENTS
        .iter()
        .find(|agent| agent.id.eq_ignore_ascii_case(id))
        .map(|agent| agent.name.to_string())
        .unwrap_or_else(|| id.to_string())
}

pub(super) fn entry_size(artifact: &Artifact) -> u64 {
    std::fs::metadata(artifact.entry_path())
        .map(|meta| meta.len())
        .unwrap_or(0)
}

pub(super) fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn relative_time(value: &str) -> String {
    let Ok(updated) = chrono::DateTime::parse_from_rfc3339(value) else {
        return value.to_string();
    };
    let seconds = chrono::Utc::now()
        .signed_duration_since(updated.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    match seconds {
        0..=59 => "刚刚".to_string(),
        60..=3599 => format!("{} 分钟前", seconds / 60),
        3600..=86_399 => format!("{} 小时前", seconds / 3600),
        86_400..=604_799 => format!("{} 天前", seconds / 86_400),
        _ => updated.format("%Y-%m-%d").to_string(),
    }
}

pub(super) fn open_external(path: &Path) {
    let _ = std::process::Command::new("open").arg(path).spawn();
}

/// The sheet needs the same project root to resolve relative links.
pub(super) fn project_relative(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).ok()?;
    let candidate = std::fs::canonicalize(candidate).ok()?;
    candidate.starts_with(&root).then_some(candidate)
}
