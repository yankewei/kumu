//! The full-window reading sheet for one artifact. Markdown and plain text
//! render natively via NSH `TextView`; HTML renders inside an embedded NSH
//! `web_view` backed by a per-sheet loopback HTTP server, with an
//! external-browser fallback if WebKit is unavailable.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{
    AppContext, ClipboardItem, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Subscription, Task, Window, div,
};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::{CloseFn, entry_size, format_size, open_external, project_relative, relative_time};
use crate::artifacts::{Artifact, ArtifactKind, ArtifactServer};
use crate::nsh_ui::mount::MountRegistry;
use crate::nsh_ui::theme::{appearance, nsh_theme};
use crate::theme::KumuTheme;

const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(super) struct SheetSnapshot {
    pub(super) title: String,
    pub(super) kind: ArtifactKind,
    pub(super) meta: String,
    pub(super) body: Option<String>,
    pub(super) error: Option<String>,
    pub(super) notice: Option<String>,
    pub(super) entry: String,
    pub(super) source: String,
    pub(super) size: String,
    pub(super) updated: String,
    /// The loopback page the embedded web view loads (HTML artifacts).
    pub(super) webview_url: Option<String>,
    /// The loopback origin external navigations are compared against.
    pub(super) webview_origin: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) enum SheetEvent {
    Snapshot(Box<SheetSnapshot>),
    Dismiss,
}

#[derive(Clone, Debug)]
pub(super) enum SheetAction {
    Close,
    Reload,
    OpenExternal,
    CopyPath,
    Reveal,
    Link(String),
}

pub(super) struct SheetMount {
    pub(super) initial: SheetSnapshot,
    pub(super) events_rx: UnboundedReceiver<SheetEvent>,
    pub(super) actions_tx: UnboundedSender<SheetAction>,
}

static MOUNTS: MountRegistry<SheetMount> = MountRegistry::new();

fn register_mount(mount: SheetMount) -> u64 {
    MOUNTS.register(mount)
}

fn take_mount(id: u64) -> Option<SheetMount> {
    MOUNTS.take(id)
}

pub(super) struct ArtifactSheet {
    mount_id: Option<u64>,
    artifact: Artifact,
    project_root: PathBuf,
    inline: bool,
    body: Option<String>,
    error: Option<String>,
    notice: Option<String>,
    reload_generation: u64,
    app_view: Option<Entity<AppView>>,
    server: Option<ArtifactServer>,
    events_tx: Option<UnboundedSender<SheetEvent>>,
    events_rx: Option<UnboundedReceiver<SheetEvent>>,
    _theme_sub: Option<Subscription>,
    _actions_task: Option<Task<()>>,
    on_close: Rc<CloseFn>,
}

impl Drop for ArtifactSheet {
    fn drop(&mut self) {
        if let Some(id) = self.mount_id {
            let _ = take_mount(id);
        }
    }
}

impl ArtifactSheet {
    pub(super) fn new(
        artifact: Artifact,
        project_root: PathBuf,
        on_close: Rc<CloseFn>,
        _cx: &mut Context<Self>,
    ) -> Self {
        let (body, mut error) = read_body(&artifact);
        let server = if artifact.meta.kind == ArtifactKind::Html && error.is_none() {
            match ArtifactServer::start(&artifact.dir, &artifact.entry_path()) {
                Ok(server) => Some(server),
                Err(server_error) => {
                    error = Some(format!("HTML 预览启动失败：{server_error}"));
                    None
                }
            }
        } else {
            None
        };
        let (events_tx, events_rx) = unbounded::<SheetEvent>();
        Self {
            mount_id: None,
            artifact,
            project_root,
            inline: false,
            body,
            error,
            notice: None,
            server,
            reload_generation: 0,
            app_view: None,
            events_tx: Some(events_tx),
            events_rx: Some(events_rx),
            _theme_sub: None,
            _actions_task: None,
            on_close,
        }
    }

    /// Construct a read-only in-memory Markdown reading sheet without writing
    /// anything to disk (used by the onboarding wizard sample preview).
    pub(super) fn new_inline_markdown(
        title: String,
        body: String,
        on_close: Rc<CloseFn>,
        _cx: &mut Context<Self>,
    ) -> Self {
        let artifact = Artifact {
            dir: PathBuf::new(),
            meta: crate::artifacts::ArtifactMeta {
                version: 1,
                id: "onboarding-sample".into(),
                title,
                kind: ArtifactKind::Markdown,
                entry: "guide.md".into(),
                created_at: String::new(),
                updated_at: String::new(),
                session: crate::artifacts::SessionInfo::default(),
                source: Some("Kumu 内置示例".into()),
                tags: Vec::new(),
            },
        };
        let (events_tx, events_rx) = unbounded::<SheetEvent>();
        Self {
            mount_id: None,
            artifact,
            project_root: PathBuf::new(),
            inline: true,
            body: Some(body),
            error: None,
            notice: None,
            server: None,
            reload_generation: 0,
            app_view: None,
            events_tx: Some(events_tx),
            events_rx: Some(events_rx),
            _theme_sub: None,
            _actions_task: None,
            on_close,
        }
    }

    pub(super) fn ensure_nsh_mounted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.app_view.is_some() {
            return;
        }
        let (actions_tx, mut actions_rx) = unbounded::<SheetAction>();
        let mount_id = register_mount(SheetMount {
            initial: self.snapshot(),
            events_rx: self
                .events_rx
                .take()
                .expect("the sheet events receiver is registered once"),
            actions_tx,
        });
        self.mount_id = Some(mount_id);

        let kumu_theme = KumuTheme::global(cx);
        let initial_theme = nsh_theme(kumu_theme);
        let initial_appearance = appearance(kumu_theme.mode);
        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu Artifact".to_owned(),
                transparent: true,
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.set_theme(Some(initial_theme), cx);
            view.set_appearance(initial_appearance, cx);
            view.load(nsh_platform_gpui::App::Rust(Root::new::<
                nsh_view::ArtifactSheetRoot,
            >(move || {
                nsh_view::ArtifactSheetRootProps {
                    mount_id: mount_id.into(),
                }
            })));
            view
        });
        self._theme_sub = Some(cx.observe_global::<KumuTheme>(|this: &mut Self, cx| {
            let Some(app_view) = &this.app_view else {
                return;
            };
            let kumu_theme = KumuTheme::global(cx);
            let (mapped, mode) = (nsh_theme(kumu_theme), appearance(kumu_theme.mode));
            app_view.update(cx, |view, cx| {
                view.set_theme(Some(mapped), cx);
                view.set_appearance(mode, cx);
            });
        }));

        self._actions_task = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(action) = actions_rx.next().await {
                let result = this.update_in(cx, |sheet, window, cx| match action {
                    SheetAction::Close => (sheet.on_close)(window, cx),
                    SheetAction::Reload => {
                        if !sheet.inline {
                            sheet.reload(window, cx);
                        }
                    }
                    SheetAction::OpenExternal => {
                        if !sheet.inline {
                            open_external(&sheet.artifact.entry_path());
                        }
                    }
                    SheetAction::CopyPath => {
                        if !sheet.inline {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                sheet.artifact.entry_path().to_string_lossy().into_owned(),
                            ));
                        }
                    }
                    SheetAction::Reveal => {
                        if !sheet.inline {
                            cx.reveal_path(&sheet.artifact.dir);
                        }
                    }
                    SheetAction::Link(url) => sheet.open_link(&url, cx),
                });
                if result.is_err() {
                    break;
                }
            }
        }));
        self.app_view = Some(app_view);
        self.push_snapshot();
    }

    pub(super) fn request_close(&self) {
        if let Some(events_tx) = &self.events_tx {
            let _ = events_tx.unbounded_send(SheetEvent::Dismiss);
        }
    }

    fn reload(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.notice = None;
        let Some(fresh) = crate::artifacts::find(&self.project_root, self.artifact.id()) else {
            self.body = None;
            self.server = None;
            self.error = Some(format!("artifact {} 已不存在", self.artifact.id()));
            self.push_snapshot();
            cx.notify();
            return;
        };
        let entry_changed = fresh.dir != self.artifact.dir
            || fresh.meta.entry != self.artifact.meta.entry
            || fresh.meta.kind != self.artifact.meta.kind;
        self.artifact = fresh;
        if self.artifact.meta.kind == ArtifactKind::Html {
            self.body = None;
            self.error = None;
            if entry_changed || self.server.is_none() {
                self.server = None;
                match ArtifactServer::start(&self.artifact.dir, &self.artifact.entry_path()) {
                    Ok(server) => self.server = Some(server),
                    Err(server_error) => {
                        self.error = Some(format!("HTML 预览启动失败：{server_error}"));
                    }
                }
            }
            // The server reads files from disk on every request; a changed
            // query string makes the bound web view load the page again.
            self.reload_generation = self.reload_generation.wrapping_add(1);
        } else {
            self.server = None;
            let (body, error) = read_body(&self.artifact);
            self.body = body;
            self.error = error;
        }
        self.push_snapshot();
        cx.notify();
    }

    fn push_snapshot(&self) {
        if let Some(events_tx) = &self.events_tx {
            let _ = events_tx.unbounded_send(SheetEvent::Snapshot(Box::new(self.snapshot())));
        }
    }

    fn snapshot(&self) -> SheetSnapshot {
        let artifact = &self.artifact;
        let session = artifact
            .meta
            .session
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .map(|title| format!("{title} · "))
            .unwrap_or_default();
        let agent = artifact
            .meta
            .session
            .agent
            .as_deref()
            .map(|agent| {
                crate::agent::AGENTS
                    .iter()
                    .find(|candidate| candidate.id.eq_ignore_ascii_case(agent))
                    .map(|candidate| candidate.name.to_string())
                    .unwrap_or_else(|| agent.to_string())
            })
            .map(|agent| format!("{agent} · "))
            .unwrap_or_default();
        let webview_url = self.server.as_ref().map(|server| {
            let url = server.entry_url();
            if self.reload_generation == 0 {
                url
            } else {
                let separator = if url.contains('?') { '&' } else { '?' };
                format!("{url}{separator}v={}", self.reload_generation)
            }
        });
        let entry = artifact.entry_path().to_string_lossy().into_owned();
        let source = artifact
            .meta
            .source
            .as_deref()
            .filter(|source| !source.trim().is_empty())
            .unwrap_or(entry.as_str())
            .to_string();
        SheetSnapshot {
            title: artifact.meta.title.clone(),
            kind: artifact.meta.kind,
            meta: format!(
                "{}{}{} · {}",
                agent,
                session,
                artifact.meta.kind.as_str(),
                relative_time(&artifact.meta.updated_at)
            ),
            body: self.body.clone(),
            error: self.error.clone(),
            notice: self.notice.clone(),
            entry,
            source,
            size: format_size(entry_size(artifact)),
            updated: artifact.meta.updated_at.clone(),
            webview_url,
            webview_origin: self
                .server
                .as_ref()
                .map(|server| server.origin().to_string()),
        }
    }

    fn open_link(&mut self, url: &str, cx: &mut Context<Self>) {
        if url.starts_with('#') {
            return;
        }
        if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:") {
            self.notice = None;
            cx.open_url(url);
            return;
        }
        if url.contains(':') {
            self.notice = Some(format!("已拒绝不受支持的链接：{url}"));
            self.push_snapshot();
            cx.notify();
            return;
        }
        let clean = url.split(['#', '?']).next().unwrap_or(url);
        let target = Path::new(clean);
        let candidate = if target.is_absolute() {
            target.to_path_buf()
        } else {
            self.artifact.dir.join(target)
        };
        if let Some(resolved) = project_relative(&self.project_root, &candidate) {
            self.notice = None;
            open_external(&resolved);
        } else {
            self.notice = Some(format!("无法打开本地链接（不存在或超出项目范围）：{clean}"));
        }
        self.push_snapshot();
        cx.notify();
    }
}

impl Render for ArtifactSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_nsh_mounted(window, cx);
        match &self.app_view {
            Some(app_view) => div().size_full().child(app_view.clone()),
            None => div().size_full(),
        }
    }
}

fn read_body(artifact: &Artifact) -> (Option<String>, Option<String>) {
    if artifact.meta.kind == ArtifactKind::Html {
        return (None, None);
    }
    let entry = artifact.entry_path();
    match std::fs::metadata(&entry) {
        Ok(meta) if meta.len() > MAX_BODY_BYTES => (
            None,
            Some(format!(
                "{} 超过 {} MB，请在编辑器中打开",
                entry.display(),
                MAX_BODY_BYTES / (1024 * 1024)
            )),
        ),
        Ok(_) => match std::fs::read_to_string(&entry) {
            Ok(body) => (Some(body), None),
            Err(error) => (None, Some(format!("无法读取 {}: {error}", entry.display()))),
        },
        Err(error) => (None, Some(format!("无法读取 {}: {error}", entry.display()))),
    }
}

mod nsh_view {
    use futures::StreamExt;
    use futures::channel::mpsc::UnboundedSender;
    use nsh_base::{sheet, text_view, theme as active_theme};
    use nsh_core::element::{SignalEquals, scroll, signal_with};
    use nsh_core::prelude::*;

    use super::{SheetAction, SheetEvent, SheetSnapshot, take_mount};
    use crate::artifacts::ArtifactKind;
    use crate::nsh_ui::assets::icon_svg;
    use crate::nsh_ui::components::{
        ButtonVariant, ControlTone, kumu_button, kumu_icon_button, meta_text, section_title,
    };
    use crate::nsh_ui::theme::{text_sm_size, text_xs_size};

    #[derive(Clone, Debug, PartialEq, Props)]
    pub(super) struct ArtifactSheetRootProps {
        pub(super) mount_id: Prop<u64>,
    }

    fn send(tx: &Option<UnboundedSender<SheetAction>>, action: SheetAction) {
        if let Some(tx) = tx {
            let _ = tx.unbounded_send(action);
        }
    }

    fn icon_action(
        label: &'static str,
        icon: &'static str,
        on_press: impl Fn() + 'static,
    ) -> impl IntoElement {
        kumu_icon_button(
            label,
            icon_svg(icon, 12.0).text_color(move || active_theme().muted_foreground),
            ControlTone::Default,
            on_press,
        )
    }

    fn html_unavailable(
        entry: String,
        message: String,
        tx: Option<UnboundedSender<SheetAction>>,
    ) -> AnyElement {
        let tx = tx.clone();
        view()
            .flex_1()
            .w_full()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .px_8()
            .child(
                view()
                    .size(px(48.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(move || active_theme().radius)
                    .bg(move || active_theme().accent)
                    .child(
                        icon_svg("icons/artifact.svg", 24.0)
                            .text_color(move || active_theme().muted_foreground),
                    ),
            )
            .child(
                text("HTML artifact")
                    .text_size(text_sm_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(move || active_theme().foreground),
            )
            .child(meta_text(message).text_center())
            .child(
                kumu_button("在浏览器打开", ButtonVariant::Primary).on_press(move || {
                    send(&tx, SheetAction::OpenExternal);
                }),
            )
            .child(
                text(entry)
                    .max_w(px(520.0))
                    .app_class("truncate")
                    .text_size(text_xs_size)
                    .text_color(move || active_theme().muted_foreground),
            )
            .into_element()
    }

    #[component]
    pub(super) fn ArtifactSheetRoot(props: ArtifactSheetRootProps) -> impl IntoElement {
        let mount = take_mount(props.mount_id.get_untracked());
        let (initial, events_rx, actions_tx) = match mount {
            Some(mount) => (mount.initial, Some(mount.events_rx), Some(mount.actions_tx)),
            None => (
                SheetSnapshot {
                    title: "Artifact".into(),
                    kind: ArtifactKind::Text,
                    meta: String::new(),
                    body: None,
                    error: Some("artifact 已不存在".into()),
                    notice: None,
                    entry: String::new(),
                    source: String::new(),
                    size: String::new(),
                    updated: String::new(),
                    webview_url: None,
                    webview_origin: None,
                },
                None,
                None,
            ),
        };
        let state = signal_with(initial, SignalEquals::Never);
        let open = signal(true);
        let page = element_ref();
        let web_error = signal(Option::<String>::None);
        let last_external = signal(Option::<String>::None);

        if let Some(mut rx) = events_rx {
            spawn(async move {
                while let Some(event) = rx.next().await {
                    match event {
                        SheetEvent::Snapshot(next) => state.set(*next),
                        SheetEvent::Dismiss => open.set(false),
                    }
                }
            });
        }

        let header = {
            let tx_reload = actions_tx.clone();
            let tx_copy = actions_tx.clone();
            let tx_reveal = actions_tx.clone();
            let tx_external = actions_tx.clone();
            let tx_close = actions_tx.clone();
            dynamic(move || {
                let tx_external = tx_external.clone();
                let tx_copy = tx_copy.clone();
                let tx_reveal = tx_reveal.clone();
                let tx_reload = tx_reload.clone();
                let tx_close = tx_close.clone();
                let snapshot = state.get();
                let mut actions = view().flex_row().items_center().gap_0p5().flex_shrink_0();
                if snapshot.kind == ArtifactKind::Html {
                    actions = actions.child(icon_action(
                        "在浏览器打开",
                        "icons/external-link.svg",
                        move || send(&tx_external, SheetAction::OpenExternal),
                    ));
                }
                actions = actions
                    .child(icon_action("复制路径", "icons/copy.svg", move || {
                        send(&tx_copy, SheetAction::CopyPath)
                    }))
                    .child(icon_action(
                        "在 Finder 中显示",
                        "icons/folder-open.svg",
                        move || send(&tx_reveal, SheetAction::Reveal),
                    ))
                    .child(icon_action(
                        "重新加载",
                        "icons/loader-circle.svg",
                        move || send(&tx_reload, SheetAction::Reload),
                    ))
                    .child(icon_action("关闭", "icons/close.svg", move || {
                        send(&tx_close, SheetAction::Close)
                    }));
                view()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_2p5()
                    .border_b_1()
                    .border_b_color(move || active_theme().border)
                    .bg(move || active_theme().background)
                    .child(
                        view()
                            .flex_1()
                            .min_w_0()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                section_title(snapshot.title)
                                    .min_w_0()
                                    .app_class("truncate"),
                            )
                            .child(meta_text(snapshot.meta).app_class("truncate")),
                    )
                    .child(actions)
            })
        };

        let tx_body = actions_tx.clone();
        let body = dynamic(move || {
            let snapshot = state.get();
            if let Some(error) = snapshot.error {
                return view()
                    .flex_1()
                    .w_full()
                    .px_8()
                    .py_6()
                    .child(
                        text(error)
                            .text_size(text_sm_size)
                            .text_color(move || active_theme().destructive),
                    )
                    .into_element();
            }
            if snapshot.kind == ArtifactKind::Html {
                if let Some(message) = web_error.get() {
                    return html_unavailable(snapshot.entry, message, tx_body.clone());
                }
                if let Some(url) = snapshot.webview_url {
                    let raw_origin = snapshot.webview_origin.unwrap_or_default();
                    let origin_prefix = format!("{raw_origin}/");
                    let state_for_nav = state;
                    let page_for_nav = page;
                    let error_for_nav = web_error;
                    let external_for_nav = last_external;
                    return view()
                        .flex_1()
                        .w_full()
                        .min_h_0()
                        .child(
                            web_view(url)
                                .incognito(true)
                                .w_full()
                                .h_full()
                                .bg(move || active_theme().background)
                                .element_ref(&page_for_nav)
                                .on_navigation_state_change(move |nav: &WebNavigation| {
                                    if nav.url == raw_origin || nav.url.starts_with(&origin_prefix)
                                    {
                                        if external_for_nav.get_untracked().is_some() {
                                            external_for_nav.set(None);
                                        }
                                        return;
                                    }
                                    if nav.url.starts_with("http://")
                                        || nav.url.starts_with("https://")
                                    {
                                        if external_for_nav.get_untracked().as_deref()
                                            != Some(nav.url.as_str())
                                        {
                                            external_for_nav.set(Some(nav.url.clone()));
                                            drop(open_url(nav.url.clone()));
                                        }
                                        if let Some(target) = state_for_nav.get().webview_url
                                            && let Some(load) =
                                                nsh_core::WebLoad::new(target, Vec::new())
                                        {
                                            page_for_nav.load(load);
                                        }
                                    }
                                })
                                .on_unavailable(move |message: &str| {
                                    error_for_nav.set(Some(message.to_string()));
                                }),
                        )
                        .into_element();
                }
                return html_unavailable(
                    snapshot.entry,
                    "内嵌预览不可用，请在浏览器中查看。".into(),
                    tx_body.clone(),
                );
            }
            let Some(body) = snapshot.body else {
                return view()
                    .flex_1()
                    .w_full()
                    .px_8()
                    .py_6()
                    .child(meta_text("正在读取…"))
                    .into_element();
            };
            let tx = tx_body.clone();
            let markdown = if snapshot.kind == ArtifactKind::Text {
                format!("````\n{body}\n````")
            } else {
                body
            };
            scroll()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(
                    view().flex_row().w_full().justify_center().child(
                        view().flex_1().max_w(px(820.0)).px_8().py_6().child(
                            text_view()
                                .markdown(markdown)
                                .selectable(true)
                                .on_link_press(move |event: &nsh_base::TextViewLinkPressEvent| {
                                    send(&tx, SheetAction::Link(event.url.to_string()));
                                }),
                        ),
                    ),
                )
                .into_element()
        });

        let footer = dynamic(move || {
            let snapshot = state.get();
            let mut bar = view()
                .flex_col()
                .flex_shrink_0()
                .border_t_1()
                .border_t_color(move || active_theme().border)
                .bg(move || active_theme().background);
            if let Some(notice) = snapshot.notice {
                bar = bar.child(
                    view()
                        .px_4()
                        .py_1()
                        .bg(move || active_theme().destructive.opacity(0.1))
                        .child(
                            text(notice)
                                .text_size(text_xs_size)
                                .text_color(move || active_theme().destructive),
                        ),
                );
            }
            let right_meta = if snapshot.size.is_empty() {
                snapshot.updated
            } else {
                format!("{} · {}", snapshot.size, snapshot.updated)
            };
            bar.child(
                view()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_1p5()
                    .child(
                        text(snapshot.source)
                            .flex_1()
                            .min_w_0()
                            .app_class("truncate")
                            .text_size(text_xs_size)
                            .font_family(move || {
                                active_theme()
                                    .font_mono
                                    .clone()
                                    .unwrap_or_else(|| SharedString::from(crate::fonts::TERM_FONT))
                            })
                            .text_color(move || active_theme().muted_foreground),
                    )
                    .child(meta_text(right_meta).flex_shrink_0().app_class("truncate")),
            )
        });

        let tx_sheet = actions_tx.clone();

        sheet()
            .open(move || Some(open.get()))
            .side(Placement::Right)
            .close_on_escape(true)
            .close_on_backdrop_press(true)
            .backdrop_class("bg-black/35")
            .popup_class(SharedString::from(
                "w-[1040px] max-w-[94%] h-full shadow-xl",
            ))
            .on_open_change(move |event: &nsh_base::SheetEvent| {
                if matches!(event, nsh_base::SheetEvent::OpenChange(false, _)) {
                    open.set(false);
                }
            })
            .on_close(move || send(&tx_sheet, SheetAction::Close))
            .child(
                view()
                    .flex_col()
                    .size_full()
                    .w_full()
                    .h_full()
                    .bg(move || active_theme().background)
                    .font_family(move || {
                        active_theme()
                            .font_sans
                            .clone()
                            .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
                    })
                    .child(header)
                    .child(body)
                    .child(footer),
            )
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::mpsc::unbounded;
    use nsh_core::element::Root;
    use nsh_engine::EngineConfig;

    use super::nsh_view::{ArtifactSheetRoot, ArtifactSheetRootProps};
    use super::*;

    fn start(snapshot: SheetSnapshot) -> (nsh_testing::Headless, UnboundedReceiver<SheetAction>) {
        let (events_tx, events_rx) = unbounded();
        let (actions_tx, actions_rx) = unbounded();
        let _ = events_tx;
        let mount_id = register_mount(SheetMount {
            initial: snapshot,
            events_rx,
            actions_tx,
        });
        let root = Root::new::<ArtifactSheetRoot>(move || ArtifactSheetRootProps {
            mount_id: mount_id.into(),
        });
        let app = nsh_testing::Headless::start(EngineConfig::new(root)).expect("start sheet NSH");
        (app, actions_rx)
    }

    #[test]
    fn the_sheet_renders_markdown_and_closes() {
        let (mut app, mut actions) = start(SheetSnapshot {
            title: "登录页方案".into(),
            kind: ArtifactKind::Markdown,
            meta: "Claude Code · markdown · 刚刚".into(),
            body: Some("# 登录页方案\n\n正文".into()),
            error: None,
            notice: None,
            entry: "/tmp/plan.md".into(),
            source: "/tmp/plan.md".into(),
            size: "24 B".into(),
            updated: "2026-11-28T07:30:12Z".into(),
            webview_url: None,
            webview_origin: None,
        });
        app.wait_for("登录页方案").expect("shows the title");
        app.wait_for("正文").expect("shows the markdown body");
        app.press("关闭").expect("press close");
        assert!(matches!(actions.try_recv().ok(), Some(SheetAction::Close)));
    }

    #[test]
    fn an_html_artifact_shows_the_browser_action() {
        let (mut app, mut actions) = start(SheetSnapshot {
            title: "交互原型".into(),
            kind: ArtifactKind::Html,
            meta: "Claude Code · html · 刚刚".into(),
            body: None,
            error: None,
            notice: None,
            entry: "/tmp/index.html".into(),
            source: "/tmp/index.html".into(),
            size: "128 B".into(),
            updated: "2026-11-28T07:30:12Z".into(),
            webview_url: None,
            webview_origin: None,
        });
        app.wait_for("HTML artifact")
            .expect("shows the html placeholder");
        app.press("在浏览器打开").expect("press open");
        assert!(matches!(
            actions.try_recv().ok(),
            Some(SheetAction::OpenExternal)
        ));
    }
}
