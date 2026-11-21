//! Diff review sheet rendered with Pure Rust NSH (`nsh_base::diff_view`) +
//! inline review annotations (`render_annotation` / `render_add_annotation`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui_kit::component::{ActiveTheme, Theme, WindowExt};
use gpui_kit::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription,
    Task, Window, div,
};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::REFRESH_INTERVAL;
use super::review::{
    DraftComment, PatchIndex, PatchKey, PatchSheet, PatchSource, ReviewComment, ReviewRange,
    ReviewStore, SendNotice, SentReview, compose_comments, normalize_and_index_patch, patch_digest,
};
use crate::nsh_ui::theme::{appearance, nsh_theme};
use crate::plugins::{PaneTarget, PluginHost, SendOutcome};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PatchSnapshot {
    pub(super) message: String,
    pub(super) patch_text: Option<String>,
    pub(super) index: PatchIndex,
    pub(super) truncated: bool,
    pub(super) stale: bool,
    pub(super) error: Option<String>,
    pub(super) comments: Vec<ReviewComment>,
    pub(super) targets: Vec<PaneTarget>,
    pub(super) active_target: Option<u64>,
    pub(super) sent: Option<SentReview>,
    pub(super) notice: Option<SendNotice>,
}

#[derive(Clone, Debug)]
pub(super) enum PatchHostEvent {
    Snapshot(PatchSnapshot),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PatchAction {
    Reload,
    AddComment(ReviewRange),
    UpdateComment { index: u64, text: String },
    RemoveComment(u64),
    SetTarget(u64),
    SendComments,
}

struct PatchMount {
    initial: PatchSnapshot,
    events_rx: UnboundedReceiver<PatchHostEvent>,
    actions_tx: UnboundedSender<PatchAction>,
}

static NEXT_MOUNT_ID: AtomicU64 = AtomicU64::new(1);
static PENDING_MOUNTS: OnceLock<Mutex<HashMap<u64, PatchMount>>> = OnceLock::new();

fn pending_mounts() -> &'static Mutex<HashMap<u64, PatchMount>> {
    PENDING_MOUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_mount(mount: PatchMount) -> u64 {
    let id = NEXT_MOUNT_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut map) = pending_mounts().lock() {
        map.insert(id, mount);
    }
    id
}

fn take_mount(id: u64) -> Option<PatchMount> {
    pending_mounts().lock().ok()?.remove(&id)
}

mod nsh_view {
    use futures::StreamExt;
    use futures::channel::mpsc::UnboundedSender;
    use nsh_base::{
        DiffAnnotation, DiffLayout, DiffLineRange, diff_view, input, theme as active_theme,
    };
    use nsh_core::element::desktop::{MenuItem, popup_menu};
    use nsh_core::element::{AccessibilityRole, SignalEquals, scroll, signal_with};
    use nsh_core::prelude::*;

    use super::{PatchAction, PatchHostEvent, PatchSnapshot, take_mount};
    use crate::nsh_ui::assets::icon_svg;
    use crate::nsh_ui::components::{ButtonVariant, kumu_button, meta_text};
    use crate::nsh_ui::theme::{TOKEN_LINK, TOKEN_WARNING, kumu_color, text_sm_size, text_xs_size};
    use crate::plugins::git_log::review::{SendNotice, comment_location};

    #[derive(Clone, Debug, PartialEq, Props)]
    pub(super) struct PatchRootProps {
        pub(super) mount_id: Prop<u64>,
    }

    fn send(tx: &Option<UnboundedSender<PatchAction>>, action: PatchAction) {
        if let Some(tx) = tx {
            let _ = tx.unbounded_send(action);
        }
    }

    fn warning_color() -> AdaptiveColor {
        kumu_color(TOKEN_WARNING, |t| t.destructive)
    }

    fn link_color() -> AdaptiveColor {
        kumu_color(TOKEN_LINK, |t| t.primary)
    }

    #[component]
    pub(super) fn PatchRoot(props: PatchRootProps) -> impl IntoElement {
        let mount = take_mount(props.mount_id.get_untracked());
        let (initial, events_rx, actions_tx) = match mount {
            Some(m) => (m.initial, Some(m.events_rx), Some(m.actions_tx)),
            None => (PatchSnapshot::default(), None, None),
        };

        let state = signal_with(initial, SignalEquals::Never);
        let layout = signal(DiffLayout::Unified);

        if let Some(mut rx) = events_rx {
            spawn(async move {
                while let Some(event) = rx.next().await {
                    match event {
                        PatchHostEvent::Snapshot(next) => state.set(next),
                    }
                }
            });
        }

        let tx_banners = actions_tx.clone();
        let banners = dynamic(move || {
            let snap = state.get();
            let message = snap.message.trim_end().to_string();
            let mut col = view().flex_col().flex_shrink_0();
            if !message.is_empty() {
                col = col.child(
                    scroll().w_full().max_h(px(192.0)).child(
                        view().pb_2().child(
                            text(message)
                                .text_size(text_sm_size)
                                .text_color(move || active_theme().muted_foreground),
                        ),
                    ),
                );
            }
            if snap.truncated {
                col = col.child(
                    view().pb_2().child(
                        text("补丁过大，只显示了前面的一部分")
                            .text_size(text_xs_size)
                            .text_color(warning_color),
                    ),
                );
            }
            if snap.stale {
                let tx_reload = tx_banners.clone();
                col = col.child(
                    view()
                        .flex_row()
                        .w_full()
                        .pb_2()
                        .gap_2()
                        .items_center()
                        .child(
                            text("补丁已经变化，当前显示的是旧内容")
                                .flex_1()
                                .min_w_0()
                                .text_size(text_xs_size)
                                .text_color(warning_color),
                        )
                        .child(
                            kumu_button("重新读取", ButtonVariant::Ghost)
                                .on_press(move || send(&tx_reload, PatchAction::Reload)),
                        ),
                );
            }
            col
        });

        let tx_diff = actions_tx.clone();
        let diff_body = dynamic(move || {
            let snap = state.get();
            if let Some(error) = snap.error {
                return view()
                    .flex_1()
                    .px_3()
                    .py_4()
                    .child(
                        text(error)
                            .text_size(text_xs_size)
                            .text_color(move || active_theme().destructive),
                    )
                    .into_element();
            }
            let Some(patch_text) = snap.patch_text else {
                return view()
                    .flex_1()
                    .px_3()
                    .py_4()
                    .child(meta_text("正在读取补丁…"))
                    .into_element();
            };
            if snap.index.file_count == 0 {
                return view()
                    .flex_1()
                    .px_3()
                    .py_4()
                    .child(meta_text("没有内容变更"))
                    .into_element();
            }

            let index_for_add = snap.index.clone();
            let tx_add = tx_diff.clone();
            let tx_edit = tx_diff.clone();
            let comments_for_render = snap.comments.clone();
            let annotations: Vec<DiffAnnotation> = snap
                .comments
                .iter()
                .filter_map(|comment| {
                    let (side, line) = snap.index.anchor_for(&comment.range)?;
                    let mut ann = DiffAnnotation::default();
                    ann.id = comment.annotation_id().into();
                    ann.side = side;
                    ann.line = line;
                    Some(ann)
                })
                .collect();

            diff_view()
                .flex_1()
                .layout(move || layout.get())
                .patch(patch_text)
                .annotations(annotations)
                .render_add_annotation(move |range: &DiffLineRange| {
                    let resolved = index_for_add.resolve_range(*range);
                    let tx = tx_add.clone();
                    view()
                        .role(AccessibilityRole::Button)
                        .label("添加评论")
                        .w(px(18.0))
                        .h(px(18.0))
                        .rounded(px(4.0))
                        .bg(link_color)
                        .items_center()
                        .justify_center()
                        .on_press(move || {
                            if let Some(r) = resolved.clone() {
                                send(&tx, PatchAction::AddComment(r));
                            }
                        })
                        .child(
                            icon_svg("icons/plus.svg", 11.0)
                                .text_color(move || active_theme().primary_foreground),
                        )
                })
                .render_annotation(move |ann: &DiffAnnotation| {
                    let Some(comment) = comments_for_render
                        .iter()
                        .find(|c| c.annotation_id() == ann.id.as_ref())
                        .cloned()
                    else {
                        return view().into_element();
                    };
                    let idx = comment.index;
                    let loc = comment_location(&comment.range);
                    let draft = signal(comment.text);
                    let tx_change = tx_edit.clone();
                    let tx_remove = tx_edit.clone();

                    view()
                        .flex_col()
                        .w_full()
                        .gap_1()
                        .p_2()
                        .rounded(move || active_theme().radius)
                        .border_1()
                        .border_color(move || active_theme().border)
                        .bg(move || active_theme().background)
                        .child(
                            view()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .child(
                                    text(loc)
                                        .flex_1()
                                        .min_w_0()
                                        .font_family(move || {
                                            active_theme().font_mono.clone().unwrap_or_else(|| {
                                                SharedString::from(crate::fonts::TERM_FONT)
                                            })
                                        })
                                        .text_size(text_xs_size)
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(move || active_theme().muted_foreground),
                                )
                                .child(
                                    view()
                                        .role(AccessibilityRole::Button)
                                        .label("删除评论")
                                        .p_0p5()
                                        .rounded(move || active_theme().radius)
                                        .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                                        .on_press(move || {
                                            send(&tx_remove, PatchAction::RemoveComment(idx));
                                        })
                                        .child(
                                            icon_svg("icons/x.svg", 11.0).text_color(move || {
                                                active_theme().muted_foreground
                                            }),
                                        ),
                                ),
                        )
                        .child(
                            input()
                                .w_full()
                                .min_h(px(44.0))
                                .max_h(px(140.0))
                                .multiline(true)
                                .placeholder("写下对这个改动片段的意见")
                                .placeholder_color(move || active_theme().muted_foreground.light)
                                .text_size(text_xs_size)
                                .text_color(move || active_theme().foreground)
                                .value(move || draft.get())
                                .on_change(move |v: &str| {
                                    draft.set(v.to_owned());
                                    send(
                                        &tx_change,
                                        PatchAction::UpdateComment {
                                            index: idx,
                                            text: v.to_owned(),
                                        },
                                    );
                                }),
                        )
                        .into_element()
                })
                .into_element()
        });

        let tx_footer = actions_tx.clone();
        let footer = dynamic(move || {
            let snap = state.get();
            let count = snap.comments.len();
            let (hint_text, hint_kind) = match &snap.notice {
                Some(SendNotice::Pasted(pane)) => (format!("已粘贴到 {pane}，按 Enter 提交"), 1u8),
                Some(SendNotice::Hint(problem)) => (problem.clone(), 2u8),
                None if count == 0 => match &snap.sent {
                    Some(sent) => (
                        format!("上次已发送 {} 条评论到 {}", sent.count, sent.target),
                        0u8,
                    ),
                    None => ("悬停 diff 行，点 + 添加评论".to_string(), 0u8),
                },
                None => (format!("{count} 条评论"), 0u8),
            };
            let hint_color = move || match hint_kind {
                1 => warning_color(),
                2 => active_theme().destructive,
                _ => active_theme().muted_foreground,
            };

            let active_pane = snap
                .active_target
                .and_then(|pane| snap.targets.iter().find(|t| t.pane == pane))
                .or_else(|| snap.targets.iter().find(|t| t.focused))
                .or_else(|| snap.targets.first())
                .cloned();

            let layout_btn = kumu_button(
                move || match layout.get() {
                    DiffLayout::Unified => "分栏视图",
                    DiffLayout::Split => "合并视图",
                },
                ButtonVariant::Ghost,
            )
            .on_press(move || {
                layout.update(|cur| {
                    *cur = match *cur {
                        DiffLayout::Unified => DiffLayout::Split,
                        DiffLayout::Split => DiffLayout::Unified,
                    };
                });
            });

            let mut bar = view()
                .flex_row()
                .flex_shrink_0()
                .w_full()
                .pt_2()
                .mt_2()
                .gap_2()
                .items_center()
                .border_t_1()
                .border_t_color(move || active_theme().border)
                .child(
                    text(hint_text)
                        .flex_1()
                        .min_w_0()
                        .text_size(text_xs_size)
                        .text_color(hint_color),
                )
                .child(layout_btn);

            if count > 0 {
                if let Some(active) = active_pane {
                    let targets = snap.targets.clone();
                    let active_id = active.pane;
                    let label = active.label();
                    let tx_target = tx_footer.clone();
                    bar = bar.child(
                        view()
                            .role(AccessibilityRole::Button)
                            .label("选择接收审查意见的窗格")
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .py_1()
                            .rounded(move || active_theme().radius)
                            .hover(|s| s.bg(move || active_theme().accent))
                            .on_press(move || {
                                let menu_items: Vec<MenuItem> = targets
                                    .iter()
                                    .map(|target| {
                                        let pane = target.pane;
                                        let tx = tx_target.clone();
                                        MenuItem::new(target.label())
                                            .checked(pane == active_id)
                                            .on_select(move || {
                                                send(&tx, PatchAction::SetTarget(pane));
                                            })
                                    })
                                    .collect();
                                drop(popup_menu(menu_items));
                            })
                            .child(
                                text(label)
                                    .text_size(text_xs_size)
                                    .text_color(move || active_theme().foreground),
                            )
                            .child(
                                icon_svg("icons/chevron-down.svg", 11.0)
                                    .text_color(move || active_theme().muted_foreground),
                            ),
                    );
                }

                let tx_send = tx_footer.clone();
                bar = bar.child(
                    kumu_button("发送", ButtonVariant::Primary)
                        .on_press(move || send(&tx_send, PatchAction::SendComments)),
                );
            }

            bar
        });

        view()
            .flex_col()
            .flex_1()
            .w_full()
            .h_full()
            .bg(move || active_theme().background)
            .font_family(move || {
                active_theme()
                    .font_sans
                    .clone()
                    .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
            })
            .child(banners)
            .child(diff_body)
            .child(footer)
    }
}

/// The sheet body: one patch rendered via Pure Rust NSH `DiffView`, with
/// background git reading, staleness polling, and review draft persistence.
pub(super) struct PatchView {
    mount_id: u64,
    project: i64,
    host: PluginHost,
    key: PatchKey,
    reviews: Rc<RefCell<ReviewStore>>,
    notify_panel: Rc<dyn Fn(&mut App)>,
    source: PatchSource,
    pub(super) comments: Vec<ReviewComment>,
    pub(super) next_comment: u64,
    pub(super) target: Option<u64>,
    digest: Option<String>,
    pub(super) stale: bool,
    reading: bool,
    read_generation: u64,
    context: String,
    sent: Option<SentReview>,
    notice: Option<SendNotice>,
    message: String,
    pub(super) patch_text: Option<String>,
    pub(super) index: PatchIndex,
    pub(super) truncated: bool,
    pub(super) error: Option<String>,
    app_view: Option<Entity<AppView>>,
    events_tx: UnboundedSender<PatchHostEvent>,
    actions_rx: Option<UnboundedReceiver<PatchAction>>,
    _theme_sub: Option<Subscription>,
    _actions_task: Option<Task<()>>,
}

impl Drop for PatchView {
    fn drop(&mut self) {
        let _ = take_mount(self.mount_id);
    }
}

impl PatchView {
    pub(super) fn new(sheet: PatchSheet, cx: &mut Context<Self>) -> Self {
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
            });
        }
        let next_comment = comments.len() as u64;
        let (events_tx, events_rx) = unbounded::<PatchHostEvent>();
        let (actions_tx, actions_rx) = unbounded::<PatchAction>();

        let initial = PatchSnapshot {
            message: message.clone(),
            patch_text: None,
            index: PatchIndex::default(),
            truncated: false,
            stale: false,
            error: None,
            comments: comments.clone(),
            targets,
            active_target: target,
            sent: sent.clone(),
            notice: None,
        };
        let mount_id = register_mount(PatchMount {
            initial,
            events_rx,
            actions_tx,
        });

        let mut view = Self {
            mount_id,
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
            patch_text: None,
            index: PatchIndex::default(),
            truncated: false,
            error: None,
            app_view: None,
            events_tx,
            actions_rx: Some(actions_rx),
            _theme_sub: None,
            _actions_task: None,
        };
        view.reload(cx);
        if matches!(view.source, PatchSource::Change { .. }) {
            view.watch(cx);
        }
        view
    }

    /// Mount the Pure Rust NSH `AppView` on the first window paint (or when opened
    /// in a sheet).
    pub(super) fn ensure_nsh_mounted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.app_view.is_some() {
            return;
        }
        let gpui_theme = cx.theme();
        let initial_theme = nsh_theme(gpui_theme);
        let initial_appearance = appearance(gpui_theme.mode);
        let mount_id = self.mount_id;

        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu Diff Review".to_owned(),
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.set_theme(Some(initial_theme), cx);
            view.set_appearance(initial_appearance, cx);
            view.load(nsh_platform_gpui::App::Rust(
                Root::new::<nsh_view::PatchRoot>(move || nsh_view::PatchRootProps {
                    mount_id: mount_id.into(),
                }),
            ));
            view
        });

        self._theme_sub = Some(cx.observe_global::<Theme>(|this: &mut Self, cx| {
            let gpui_theme = cx.theme();
            let (mapped, mode) = (nsh_theme(gpui_theme), appearance(gpui_theme.mode));
            if let Some(app_view) = &this.app_view {
                app_view.update(cx, |view, cx| {
                    view.set_theme(Some(mapped), cx);
                    view.set_appearance(mode, cx);
                });
            }
        }));

        if let Some(mut actions_rx) = self.actions_rx.take() {
            self._actions_task = Some(cx.spawn_in(window, async move |this, cx| {
                while let Some(action) = actions_rx.next().await {
                    let res = this.update_in(cx, |view, window, cx| match action {
                        PatchAction::Reload => view.reload(cx),
                        PatchAction::AddComment(range) => view.add_comment(range, cx),
                        PatchAction::UpdateComment { index, text } => {
                            view.update_comment(index, text, cx);
                        }
                        PatchAction::RemoveComment(index) => view.remove_comment(index, cx),
                        PatchAction::SetTarget(pane) => view.set_target(pane, cx),
                        PatchAction::SendComments => {
                            if view.send_comments(cx) {
                                window.close_sheet(cx);
                            }
                        }
                    });
                    if res.is_err() {
                        break;
                    }
                }
            }));
        }

        self.app_view = Some(app_view);
        self.push_snapshot(cx);
    }

    fn snapshot(&self, cx: &App) -> PatchSnapshot {
        PatchSnapshot {
            message: self.message.clone(),
            patch_text: self.patch_text.clone(),
            index: self.index.clone(),
            truncated: self.truncated,
            stale: self.stale,
            error: self.error.clone(),
            comments: self.comments.clone(),
            targets: self.host.panes(self.project, cx),
            active_target: self.target,
            sent: self.sent.clone(),
            notice: self.notice.clone(),
        }
    }

    fn push_snapshot(&self, cx: &App) {
        let _ = self
            .events_tx
            .unbounded_send(PatchHostEvent::Snapshot(self.snapshot(cx)));
    }

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
                    let (normalized, index) =
                        normalize_and_index_patch(&patch.patch, patch.truncated);
                    Ok::<_, String>((patch, normalized, index))
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.read_generation != generation {
                    return;
                }
                view.reading = false;
                match read {
                    Ok((patch, normalized, index)) => {
                        view.truncated = patch.truncated;
                        view.digest = Some(patch_digest(&patch));
                        view.stale = false;
                        view.error = None;
                        view.patch_text = Some(normalized);
                        view.index = index;
                    }
                    Err(error) => view.error = Some(error),
                }
                view.push_snapshot(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn watch(&self, cx: &mut Context<Self>) {
        let source = self.source.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                let generation = match this.update(cx, |view, _| {
                    (!view.reading && !view.stale).then_some(view.read_generation)
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
                        view.push_snapshot(cx);
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

    pub(super) fn save_drafts(&self, cx: &mut Context<Self>) {
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
        let changed = {
            let mut store = self.reviews.borrow_mut();
            let had_draft = store.drafts.contains_key(&self.key);
            if drafts.is_empty() {
                store.drafts.remove(&self.key);
            } else {
                store.drafts.insert(self.key.clone(), drafts);
            }
            had_draft != store.drafts.contains_key(&self.key)
        };
        if changed {
            (self.notify_panel)(cx);
        }
    }

    pub(super) fn add_comment(&mut self, range: ReviewRange, cx: &mut Context<Self>) {
        if self.comments.iter().any(|c| c.range == range) {
            return;
        }
        let index = self.next_comment;
        self.next_comment += 1;
        self.notice = None;
        self.comments.push(ReviewComment {
            index,
            range,
            text: String::new(),
        });
        self.push_snapshot(cx);
        cx.notify();
    }

    pub(super) fn update_comment(&mut self, index: u64, text: String, cx: &mut Context<Self>) {
        if let Some(comment) = self.comments.iter_mut().find(|c| c.index == index) {
            comment.text = text;
        }
        self.notice = None;
        self.save_drafts(cx);
    }

    fn remove_comment(&mut self, index: u64, cx: &mut Context<Self>) {
        let before = self.comments.len();
        self.comments.retain(|comment| comment.index != index);
        if self.comments.len() != before {
            self.notice = None;
            self.save_drafts(cx);
            self.push_snapshot(cx);
            cx.notify();
        }
    }

    fn active_target<'a>(&self, targets: &'a [PaneTarget]) -> Option<&'a PaneTarget> {
        self.target
            .and_then(|pane| targets.iter().find(|target| target.pane == pane))
            .or_else(|| targets.iter().find(|target| target.focused))
            .or_else(|| targets.first())
    }

    pub(super) fn set_target(&mut self, pane: u64, cx: &mut Context<Self>) {
        if self.target != Some(pane) {
            self.target = Some(pane);
            self.push_snapshot(cx);
            cx.notify();
        }
    }

    pub(super) fn send_comments(&mut self, cx: &mut Context<Self>) -> bool {
        let comments = self
            .comments
            .iter()
            .filter_map(|comment| {
                let text = comment.text.trim().to_string();
                (!text.is_empty()).then(|| (comment.range.clone(), text))
            })
            .collect::<Vec<_>>();
        if comments.is_empty() {
            self.notice = Some(SendNotice::Hint("先写下评论再发送".to_string()));
            self.push_snapshot(cx);
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
                self.push_snapshot(cx);
                cx.notify();
                true
            }
            Ok(SendOutcome::Pasted) => {
                let pane = target
                    .map(|target| target.label())
                    .unwrap_or_else(|| "当前窗格".to_string());
                self.notice = Some(SendNotice::Pasted(pane));
                self.push_snapshot(cx);
                cx.notify();
                false
            }
            Err(problem) => {
                self.notice = Some(SendNotice::Hint(problem));
                self.push_snapshot(cx);
                cx.notify();
                false
            }
        }
    }
}

impl Render for PatchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_nsh_mounted(window, cx);
        match &self.app_view {
            Some(app_view) => div().size_full().child(app_view.clone()),
            None => div().size_full(),
        }
    }
}
