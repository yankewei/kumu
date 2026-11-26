//! Combined collapsible Diff review sheet rendered with Pure Rust NSH
//! (`nsh_base::diff_view`) + inline review annotations, collapsible per-file
//! headers with "已查看" (Viewed) tracking, a left file navigation sidebar,
//! and an Agent-title-first target selector.

use std::cell::RefCell;
use std::rc::Rc;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription,
    Task, Window, div,
};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::REFRESH_INTERVAL;
use super::review::{
    DraftComment, PatchIndex, PatchKey, PatchSheet, PatchSource, ReviewComment, ReviewRange,
    ReviewStore, SendNotice, SentReview, compose_comments, normalize_and_index_patch, patch_digest,
};
use crate::git::WorkingTreeDiffScope;
use crate::nsh_ui::mount::MountRegistry;
use crate::nsh_ui::theme::{appearance, nsh_theme};
use crate::plugins::{PaneTarget, PluginHost, SendOutcome};
use crate::theme::KumuTheme;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PatchSnapshot {
    pub(super) title: String,
    pub(super) meta: String,
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
    pub(super) working_tree_scope: Option<WorkingTreeDiffScope>,
    pub(super) initial_file: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) enum PatchHostEvent {
    Snapshot(Box<PatchSnapshot>),
    Dismiss,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PatchAction {
    Reload,
    SetScope(WorkingTreeDiffScope),
    AddComment(ReviewRange),
    UpdateComment { index: u64, text: String },
    RemoveComment(u64),
    SetTarget(u64),
    SendComments,
    Close,
}

struct PatchMount {
    initial: PatchSnapshot,
    events_rx: UnboundedReceiver<PatchHostEvent>,
    actions_tx: UnboundedSender<PatchAction>,
}

static MOUNTS: MountRegistry<PatchMount> = MountRegistry::new();

fn register_mount(mount: PatchMount) -> u64 {
    MOUNTS.register(mount)
}

fn take_mount(id: u64) -> Option<PatchMount> {
    MOUNTS.take(id)
}

mod nsh_view {
    use std::collections::HashSet;

    use futures::StreamExt;
    use futures::channel::mpsc::UnboundedSender;
    use nsh_base::{
        DiffAnnotation, DiffFile, DiffFileStatus, DiffIndicators, DiffLayout, DiffLineRange,
        DiffSeparator, DiffStats, DiffView, DiffViewApi, diff_view, input, sheet,
        theme as active_theme,
    };
    use nsh_core::element::{AccessibilityRole, SignalEquals, scroll, signal_with};
    use nsh_core::prelude::*;

    use super::{PatchAction, PatchHostEvent, PatchSnapshot, take_mount};
    use crate::agent::AgentState;
    use crate::git::WorkingTreeDiffScope;
    use crate::nsh_ui::assets::{agent_logo, icon_svg};
    use crate::nsh_ui::components::{
        ButtonVariant, kumu_button, kumu_tooltip, meta_text, section_title,
    };
    use crate::nsh_ui::theme::{
        TOKEN_LINK, TOKEN_SUCCESS, TOKEN_WARNING, kumu_color, text_sm_size, text_xs_size,
    };
    use crate::plugins::PaneTarget;
    use crate::plugins::git_log::review::{
        PatchFileSummary, PatchIndex, ReviewComment, SendNotice, SentReview, comment_location,
    };
    use crate::plugins::git_log::{file_name, parent_dir};

    #[derive(Clone, Debug, PartialEq, Props)]
    pub(super) struct PatchRootProps {
        pub(super) mount_id: Prop<u64>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum DiffBodyState {
        Error(String),
        Loading,
        Empty,
        Ready,
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

    fn success_color() -> AdaptiveColor {
        kumu_color(TOKEN_SUCCESS, |t| t.primary)
    }

    fn status_badge_color(status: char) -> AdaptiveColor {
        match status {
            'M' | 'T' => warning_color(),
            'A' | '?' => success_color(),
            'D' | 'U' => active_theme().destructive,
            'R' | 'C' => link_color(),
            _ => active_theme().muted_foreground,
        }
    }

    fn diff_file_status_char(status: DiffFileStatus) -> char {
        match status {
            DiffFileStatus::Modified => 'M',
            DiffFileStatus::Added => 'A',
            DiffFileStatus::Deleted => 'D',
            DiffFileStatus::Renamed => 'R',
            DiffFileStatus::Copied => 'C',
        }
    }

    fn agent_state_color(state: Option<AgentState>) -> AdaptiveColor {
        match state {
            Some(AgentState::Running) => link_color(),
            Some(AgentState::Waiting) => warning_color(),
            Some(AgentState::Failed) => active_theme().destructive,
            Some(AgentState::Completed) | None => success_color(),
        }
    }

    fn agent_state_label(target: &PaneTarget) -> &'static str {
        if target.agent.is_none() {
            return "仅粘贴";
        }
        match target.state {
            Some(AgentState::Running) => "运行中",
            Some(AgentState::Waiting) => "等待输入",
            Some(AgentState::Completed) => "已完成",
            Some(AgentState::Failed) => "异常",
            None => "空闲",
        }
    }

    fn render_target_icon(target: &PaneTarget, size: f32) -> AnyElement {
        if target.agent.is_some() {
            agent_logo(target.agent_logo(), size)
        } else {
            icon_svg("icons/square-terminal.svg", size)
                .text_color(move || active_theme().muted_foreground)
                .into_element()
        }
    }

    fn mono_font() -> SharedString {
        active_theme()
            .font_mono
            .clone()
            .unwrap_or_else(|| SharedString::from(crate::fonts::TERM_FONT))
    }

    #[component]
    pub(super) fn PatchRoot(props: PatchRootProps) -> impl IntoElement {
        let mount = take_mount(props.mount_id.get_untracked());
        let (initial, events_rx, actions_tx) = match mount {
            Some(m) => (m.initial, Some(m.events_rx), Some(m.actions_tx)),
            None => (PatchSnapshot::default(), None, None),
        };

        // Fine-grained signals so updating comments / targets / notices NEVER
        // remounts `DiffView` or loses scroll position / collapsed state.
        let title_sig = signal(initial.title.clone());
        let meta_sig = signal(initial.meta.clone());
        let message_sig = signal(initial.message.clone());
        let truncated_sig = signal(initial.truncated);
        let stale_sig = signal(initial.stale);
        let error_sig = signal(initial.error.clone());
        let scope_sig = signal(initial.working_tree_scope);
        let patch_text_sig = signal(initial.patch_text.clone());
        let index_sig: Signal<PatchIndex> = signal_with(initial.index.clone(), SignalEquals::Never);
        let files_sig: Signal<Vec<PatchFileSummary>> = signal(initial.index.files.clone());
        let comments_sig: Signal<Vec<ReviewComment>> = signal(initial.comments.clone());
        let targets_sig: Signal<Vec<PaneTarget>> = signal(initial.targets.clone());
        let active_target_sig: Signal<Option<u64>> = signal(initial.active_target);
        let sent_sig: Signal<Option<SentReview>> = signal(initial.sent.clone());
        let notice_sig: Signal<Option<SendNotice>> = signal(initial.notice.clone());

        // Reading controls & UI state.
        let layout = signal(DiffLayout::Unified);
        let soft_wrap = signal(false);
        let wide_mode = signal(false);
        let sidebar_open = signal(true);
        let collapsed_files: Signal<HashSet<String>> = signal(HashSet::new());
        let viewed_files: Signal<HashSet<String>> = signal(HashSet::new());
        let active_file: Signal<Option<String>> = signal(initial.initial_file.clone());
        let pending_focus_file: Signal<Option<String>> = signal(initial.initial_file.clone());
        let current_hunk: Signal<Option<u32>> = signal(None);
        let total_hunks: Signal<u32> = signal(0);
        let target_menu_open = signal(false);
        let open = signal(true);

        let dv_ref = component_ref::<DiffView>();

        if let Some(mut rx) = events_rx {
            spawn(async move {
                while let Some(event) = rx.next().await {
                    match event {
                        PatchHostEvent::Snapshot(next) => {
                            let snap = *next;
                            if title_sig.get_untracked() != snap.title {
                                title_sig.set(snap.title);
                            }
                            if meta_sig.get_untracked() != snap.meta {
                                meta_sig.set(snap.meta);
                            }
                            if message_sig.get_untracked() != snap.message {
                                message_sig.set(snap.message);
                            }
                            if truncated_sig.get_untracked() != snap.truncated {
                                truncated_sig.set(snap.truncated);
                            }
                            if stale_sig.get_untracked() != snap.stale {
                                stale_sig.set(snap.stale);
                            }
                            if error_sig.get_untracked() != snap.error {
                                error_sig.set(snap.error);
                            }
                            if scope_sig.get_untracked() != snap.working_tree_scope {
                                scope_sig.set(snap.working_tree_scope);
                                // Clear per-file view state when switching scopes.
                                collapsed_files.set(HashSet::new());
                                viewed_files.set(HashSet::new());
                                current_hunk.set(None);
                            }
                            if patch_text_sig.get_untracked() != snap.patch_text {
                                patch_text_sig.set(snap.patch_text);
                                files_sig.set(snap.index.files.clone());
                                index_sig.set(snap.index);
                                if active_file.get_untracked().is_none()
                                    && let Some(first) =
                                        files_sig.get_untracked().first().map(|f| f.path.clone())
                                {
                                    active_file.set(Some(first));
                                }
                            } else if index_sig.get_untracked() != snap.index {
                                files_sig.set(snap.index.files.clone());
                                index_sig.set(snap.index);
                            }
                            if comments_sig.get_untracked() != snap.comments {
                                comments_sig.set(snap.comments);
                            }
                            if targets_sig.get_untracked() != snap.targets {
                                targets_sig.set(snap.targets);
                            }
                            if active_target_sig.get_untracked() != snap.active_target {
                                active_target_sig.set(snap.active_target);
                            }
                            if sent_sig.get_untracked() != snap.sent {
                                sent_sig.set(snap.sent);
                            }
                            if notice_sig.get_untracked() != snap.notice {
                                notice_sig.set(snap.notice);
                            }
                        }
                        PatchHostEvent::Dismiss => open.set(false),
                    }
                }
            });
        }

        let body_state = computed(move || {
            if let Some(err) = error_sig.get() {
                return DiffBodyState::Error(err);
            }
            if patch_text_sig.get().is_none() {
                return DiffBodyState::Loading;
            }
            if files_sig.get().is_empty() {
                return DiffBodyState::Empty;
            }
            DiffBodyState::Ready
        });

        let annotations_memo = computed(move || {
            let index = index_sig.get();
            comments_sig
                .get()
                .into_iter()
                .filter_map(|comment| {
                    let (side, line) = index.anchor_for(&comment.range)?;
                    let mut ann = DiffAnnotation::default();
                    ann.id = comment.annotation_id().into();
                    ann.side = side;
                    ann.line = line;
                    Some(ann)
                })
                .collect::<Vec<DiffAnnotation>>()
        });

        // --- Header & Top Reading Toolbar ---
        let tx_scope = actions_tx.clone();
        let dv_hunk_prev = dv_ref;
        let dv_hunk_next = dv_ref;

        let header = dynamic(move || {
            let title = title_sig.get();
            let meta = meta_sig.get();
            let files = files_sig.get();
            let file_count = files.len();
            let total_added: usize = files.iter().map(|f| f.added).sum();
            let total_deleted: usize = files.iter().map(|f| f.deleted).sum();
            let viewed_count = viewed_files
                .get()
                .iter()
                .filter(|p| files.iter().any(|f| &f.path == *p))
                .count();
            let scope = scope_sig.get();
            let cur_layout = layout.get();
            let cur_wrap = soft_wrap.get();
            let cur_wide = wide_mode.get();
            let cur_sidebar = sidebar_open.get();
            let hunks_total = total_hunks.get();
            let hunk_pos = current_hunk.get();

            // Left: title + meta + stats + optional working-tree scope switch
            let mut left = view()
                .flex_row()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap_2p5()
                .child(
                    view()
                        .flex_col()
                        .min_w_0()
                        .gap_0p5()
                        .child(section_title(title).app_class("truncate"))
                        .child(
                            view()
                                .flex_row()
                                .items_center()
                                .gap_1p5()
                                .min_w_0()
                                .child(meta_text(meta).app_class("truncate"))
                                .when(file_count > 0, |row| {
                                    row.child(meta_text("·"))
                                        .child(
                                            text(format!("+{total_added}"))
                                                .font_family(mono_font)
                                                .text_size(text_xs_size)
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(success_color),
                                        )
                                        .child(
                                            text(format!("−{total_deleted}"))
                                                .font_family(mono_font)
                                                .text_size(text_xs_size)
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(move || active_theme().destructive),
                                        )
                                }),
                        ),
                );

            if let Some(active_scope) = scope {
                let make_scope_btn = |label: &'static str, target: WorkingTreeDiffScope| {
                    let active = active_scope == target;
                    let tx = tx_scope.clone();
                    let mut btn = view()
                        .role(AccessibilityRole::Button)
                        .label(label)
                        .px_2()
                        .py_0p5()
                        .rounded(px(4.0))
                        .app_class("cursor-pointer")
                        .on_press(move || {
                            if !active {
                                send(&tx, PatchAction::SetScope(target));
                            }
                        });
                    if active {
                        btn = btn.bg(move || active_theme().background);
                    } else {
                        btn = btn.hover(|s| s.bg(move || active_theme().accent.opacity(0.45)));
                    }
                    btn.child(
                        text(label)
                            .text_size(text_xs_size)
                            .font_weight(if active {
                                FontWeight::MEDIUM
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(move || {
                                if active {
                                    active_theme().foreground
                                } else {
                                    active_theme().muted_foreground
                                }
                            }),
                    )
                };

                left = left.child(
                    view()
                        .flex_row()
                        .flex_shrink_0()
                        .p_0p5()
                        .gap_0p5()
                        .rounded(move || active_theme().radius)
                        .bg(move || active_theme().muted)
                        .child(make_scope_btn("全部", WorkingTreeDiffScope::All))
                        .child(make_scope_btn("已暂存", WorkingTreeDiffScope::Staged))
                        .child(make_scope_btn("未暂存", WorkingTreeDiffScope::Unstaged)),
                );
            }

            // Right: viewed progress, layout segmented switch, wrap, hunk stepper, sidebar, wide, close
            let mut right = view().flex_row().flex_shrink_0().items_center().gap_1p5();

            if file_count > 1 {
                right = right.child(
                    view()
                        .px_2()
                        .py_0p5()
                        .rounded(move || active_theme().radius)
                        .bg(move || active_theme().muted)
                        .child(
                            text(format!("{viewed_count} / {file_count} 已查看"))
                                .text_size(text_xs_size)
                                .text_color(move || {
                                    if viewed_count == file_count {
                                        success_color()
                                    } else {
                                        active_theme().muted_foreground
                                    }
                                }),
                        ),
                );
            }

            // Unified / Split segmented switch
            let make_layout_btn = |label: &'static str, target: DiffLayout| {
                let active = cur_layout == target;
                let mut btn = view()
                    .role(AccessibilityRole::Button)
                    .label(label)
                    .px_2()
                    .py_0p5()
                    .rounded(px(4.0))
                    .app_class("cursor-pointer")
                    .on_press(move || layout.set(target));
                if active {
                    btn = btn.bg(move || active_theme().background);
                } else {
                    btn = btn.hover(|s| s.bg(move || active_theme().accent.opacity(0.45)));
                }
                btn.child(
                    text(label)
                        .text_size(text_xs_size)
                        .font_weight(if active {
                            FontWeight::MEDIUM
                        } else {
                            FontWeight::NORMAL
                        })
                        .text_color(move || {
                            if active {
                                active_theme().foreground
                            } else {
                                active_theme().muted_foreground
                            }
                        }),
                )
            };
            right = right.child(
                view()
                    .flex_row()
                    .flex_shrink_0()
                    .p_0p5()
                    .gap_0p5()
                    .rounded(move || active_theme().radius)
                    .bg(move || active_theme().muted)
                    .child(make_layout_btn("合并", DiffLayout::Unified))
                    .child(make_layout_btn("分栏", DiffLayout::Split)),
            );

            // Soft wrap toggle
            right = right.child(
                view()
                    .role(AccessibilityRole::Button)
                    .label("自动换行")
                    .tooltip(kumu_tooltip(if cur_wrap {
                        "关闭自动换行"
                    } else {
                        "开启自动换行"
                    }))
                    .px_2()
                    .py_1()
                    .rounded(move || active_theme().radius)
                    .border_1()
                    .border_color(move || {
                        if cur_wrap {
                            link_color().opacity(0.6)
                        } else {
                            active_theme().border
                        }
                    })
                    .bg(move || {
                        if cur_wrap {
                            link_color().opacity(0.14)
                        } else {
                            AdaptiveColor::TRANSPARENT
                        }
                    })
                    .app_class("cursor-pointer")
                    .hover(|s| s.bg(move || active_theme().accent.opacity(0.5)))
                    .on_press(move || soft_wrap.update(|w| *w = !*w))
                    .child(text("换行").text_size(text_xs_size).text_color(move || {
                        if cur_wrap {
                            link_color()
                        } else {
                            active_theme().muted_foreground
                        }
                    })),
            );

            // Hunk navigation stepper
            if hunks_total > 0 {
                let dv_prev = dv_hunk_prev;
                let dv_next = dv_hunk_next;
                let counter = match hunk_pos {
                    Some(i) => format!("{} / {}", i + 1, hunks_total),
                    None => format!("{hunks_total} 处"),
                };
                right = right.child(
                    view()
                        .flex_row()
                        .items_center()
                        .rounded(move || active_theme().radius)
                        .border_1()
                        .border_color(move || active_theme().border)
                        .child(
                            view()
                                .role(AccessibilityRole::Button)
                                .label("上一处改动")
                                .tooltip(kumu_tooltip("上一处改动"))
                                .p_1()
                                .app_class("cursor-pointer")
                                .hover(|s| s.bg(move || active_theme().accent.opacity(0.5)))
                                .on_press(move || {
                                    if let Some(api) = dv_prev.api::<DiffViewApi>() {
                                        current_hunk.set(api.previous_hunk());
                                    }
                                })
                                .child(
                                    icon_svg("icons/chevron-up.svg", 12.0)
                                        .text_color(move || active_theme().muted_foreground),
                                ),
                        )
                        .child(
                            text(counter)
                                .px_1()
                                .font_family(mono_font)
                                .text_size(text_xs_size)
                                .text_color(move || active_theme().muted_foreground),
                        )
                        .child(
                            view()
                                .role(AccessibilityRole::Button)
                                .label("下一处改动")
                                .tooltip(kumu_tooltip("下一处改动"))
                                .p_1()
                                .app_class("cursor-pointer")
                                .hover(|s| s.bg(move || active_theme().accent.opacity(0.5)))
                                .on_press(move || {
                                    if let Some(api) = dv_next.api::<DiffViewApi>() {
                                        current_hunk.set(api.next_hunk());
                                    }
                                })
                                .child(
                                    icon_svg("icons/chevron-down.svg", 12.0)
                                        .text_color(move || active_theme().muted_foreground),
                                ),
                        ),
                );
            }

            // File sidebar toggle (when multi-file)
            if file_count > 1 {
                right = right.child(
                    view()
                        .role(AccessibilityRole::Button)
                        .label("切换文件列表")
                        .tooltip(kumu_tooltip(if cur_sidebar {
                            "隐藏文件列表"
                        } else {
                            "显示文件列表"
                        }))
                        .p_1()
                        .rounded(move || active_theme().radius)
                        .bg(move || {
                            if cur_sidebar {
                                active_theme().accent.opacity(0.6)
                            } else {
                                AdaptiveColor::TRANSPARENT
                            }
                        })
                        .app_class("cursor-pointer")
                        .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                        .on_press(move || sidebar_open.update(|s| *s = !*s))
                        .child(icon_svg("icons/panel-left.svg", 14.0).text_color(move || {
                            if cur_sidebar {
                                active_theme().foreground
                            } else {
                                active_theme().muted_foreground
                            }
                        })),
                );
            }

            // Wide mode toggle
            right = right.child(
                view()
                    .role(AccessibilityRole::Button)
                    .label("切换宽屏阅读")
                    .tooltip(kumu_tooltip(if cur_wide {
                        "恢复默认宽度"
                    } else {
                        "宽屏阅读"
                    }))
                    .p_1()
                    .rounded(move || active_theme().radius)
                    .app_class("cursor-pointer")
                    .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                    .on_press(move || wide_mode.update(|w| *w = !*w))
                    .child(
                        icon_svg(
                            if cur_wide {
                                "icons/minimize-2.svg"
                            } else {
                                "icons/maximize-2.svg"
                            },
                            13.0,
                        )
                        .text_color(move || active_theme().muted_foreground),
                    ),
            );

            // Close button
            right = right.child(
                view()
                    .role(AccessibilityRole::Button)
                    .label("关闭 Diff 面板")
                    .tooltip(kumu_tooltip("关闭 (Esc)"))
                    .flex_shrink_0()
                    .p_1()
                    .rounded(move || active_theme().radius)
                    .app_class("cursor-pointer")
                    .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                    .on_press(move || {
                        open.set(false);
                    })
                    .child(
                        icon_svg("icons/x.svg", 14.0)
                            .text_color(move || active_theme().muted_foreground),
                    ),
            );

            view()
                .flex_row()
                .flex_shrink_0()
                .w_full()
                .items_center()
                .gap_3()
                .px_4()
                .py_2p5()
                .border_b_1()
                .border_b_color(move || active_theme().border)
                .child(left)
                .child(right)
        });

        // --- Banners (Commit message / truncated / stale) with proper horizontal padding ---
        let tx_banners = actions_tx.clone();
        let banners = dynamic(move || {
            let message = message_sig.get().trim_end().to_string();
            let truncated = truncated_sig.get();
            let stale = stale_sig.get();
            if message.is_empty() && !truncated && !stale {
                return view().into_element();
            }
            let mut col = view()
                .flex_col()
                .flex_shrink_0()
                .w_full()
                .px_4()
                .py_2()
                .gap_1p5()
                .border_b_1()
                .border_b_color(move || active_theme().border)
                .bg(move || active_theme().card);
            if !message.is_empty() {
                col = col.child(
                    scroll().w_full().max_h(px(120.0)).child(
                        text(message)
                            .text_size(text_sm_size)
                            .text_color(move || active_theme().muted_foreground),
                    ),
                );
            }
            if truncated {
                col = col.child(
                    text("补丁过大，只显示了前面的一部分")
                        .text_size(text_xs_size)
                        .text_color(warning_color),
                );
            }
            if stale {
                let tx_reload = tx_banners.clone();
                col = col.child(
                    view()
                        .flex_row()
                        .w_full()
                        .gap_2()
                        .items_center()
                        .child(
                            text("工作区补丁已经变化，当前显示的是旧内容")
                                .flex_1()
                                .min_w_0()
                                .text_size(text_xs_size)
                                .text_color(warning_color),
                        )
                        .child(
                            kumu_button("重新读取", ButtonVariant::Outline)
                                .on_press(move || send(&tx_reload, PatchAction::Reload)),
                        ),
                );
            }
            col.into_element()
        });

        // --- Left Collapsible File List Sidebar ---
        let dv_sidebar = dv_ref;
        let file_sidebar = dynamic(move || {
            let files = files_sig.get();
            if files.len() <= 1 || !sidebar_open.get() {
                return view().into_element();
            }
            let active = active_file.get();
            let viewed = viewed_files.get();
            let collapsed = collapsed_files.get();

            let dv_collapse_all = dv_sidebar;
            let dv_expand_all = dv_sidebar;
            let files_for_collapse = files.clone();
            let files_for_expand = files.clone();

            let mut list = view().flex_col().w_full().py_1();
            for file in &files {
                let path = file.path.clone();
                let is_active = active.as_deref() == Some(path.as_str());
                let is_viewed = viewed.contains(&path);
                let is_collapsed = collapsed.contains(&path);
                let status = file.status;
                let fname = file_name(&path).to_string();
                let dir = parent_dir(&path).to_string();
                let added = file.added;
                let deleted = file.deleted;
                let dv_jump = dv_sidebar;
                let jump_path = path.clone();

                let mut row = view()
                    .role(AccessibilityRole::Button)
                    .label(format!("跳转到 {path}"))
                    .flex_row()
                    .w_full()
                    .px_2p5()
                    .py_1p5()
                    .gap_1p5()
                    .items_center()
                    .app_class("cursor-pointer")
                    .bg(move || {
                        if is_active {
                            active_theme().accent
                        } else {
                            AdaptiveColor::TRANSPARENT
                        }
                    })
                    .hover(|s| s.bg(move || active_theme().accent.opacity(0.55)))
                    .on_press(move || {
                        active_file.set(Some(jump_path.clone()));
                        if let Some(api) = dv_jump.api::<DiffViewApi>() {
                            if collapsed_files.get_untracked().contains(&jump_path) {
                                api.expand_file(jump_path.clone());
                                collapsed_files.update(|set| {
                                    set.remove(&jump_path);
                                });
                            }
                            api.scroll_to_file(jump_path.clone());
                        }
                    })
                    .child(
                        text(if is_collapsed { "▸" } else { "▾" })
                            .w(px(10.0))
                            .flex_shrink_0()
                            .text_size(text_xs_size)
                            .text_color(move || active_theme().muted_foreground),
                    )
                    .child(
                        text(status.to_string())
                            .w(px(12.0))
                            .flex_shrink_0()
                            .font_family(mono_font)
                            .text_size(text_xs_size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(move || status_badge_color(status)),
                    )
                    .child(
                        view()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                text(fname)
                                    .app_class("truncate")
                                    .text_size(text_xs_size)
                                    .font_weight(if is_active {
                                        FontWeight::MEDIUM
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .text_color(move || {
                                        if is_viewed {
                                            active_theme().muted_foreground
                                        } else {
                                            active_theme().foreground
                                        }
                                    }),
                            )
                            .when(!dir.is_empty(), |col| {
                                col.child(meta_text(dir).app_class("truncate"))
                            }),
                    );

                if added > 0 || deleted > 0 {
                    row = row.child(
                        view()
                            .flex_row()
                            .flex_shrink_0()
                            .gap_1()
                            .when(added > 0, |r| {
                                r.child(
                                    text(format!("+{added}"))
                                        .font_family(mono_font)
                                        .text_size(text_xs_size)
                                        .text_color(success_color),
                                )
                            })
                            .when(deleted > 0, |r| {
                                r.child(
                                    text(format!("−{deleted}"))
                                        .font_family(mono_font)
                                        .text_size(text_xs_size)
                                        .text_color(move || active_theme().destructive),
                                )
                            }),
                    );
                }

                if is_viewed {
                    row = row.child(icon_svg("icons/check.svg", 12.0).text_color(success_color));
                }

                list = list.child(row);
            }

            view()
                .flex_col()
                .flex_shrink_0()
                .w(px(250.0))
                .h_full()
                .border_r_1()
                .border_r_color(move || active_theme().border)
                .bg(move || active_theme().sidebar)
                .child(
                    view()
                        .flex_row()
                        .flex_shrink_0()
                        .items_center()
                        .px_2p5()
                        .py_1p5()
                        .gap_1()
                        .border_b_1()
                        .border_b_color(move || active_theme().border)
                        .child(
                            text(format!("文件 ({})", files.len()))
                                .flex_1()
                                .min_w_0()
                                .text_size(text_xs_size)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(move || active_theme().muted_foreground),
                        )
                        .child(
                            view()
                                .role(AccessibilityRole::Button)
                                .label("全部折叠")
                                .px_1p5()
                                .py_0p5()
                                .rounded(px(4.0))
                                .app_class("cursor-pointer")
                                .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                                .on_press(move || {
                                    if let Some(api) = dv_collapse_all.api::<DiffViewApi>() {
                                        let mut next = HashSet::new();
                                        for f in &files_for_collapse {
                                            api.collapse_file(f.path.clone());
                                            next.insert(f.path.clone());
                                        }
                                        collapsed_files.set(next);
                                    }
                                })
                                .child(meta_text("折叠")),
                        )
                        .child(
                            view()
                                .role(AccessibilityRole::Button)
                                .label("全部展开")
                                .px_1p5()
                                .py_0p5()
                                .rounded(px(4.0))
                                .app_class("cursor-pointer")
                                .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                                .on_press(move || {
                                    if let Some(api) = dv_expand_all.api::<DiffViewApi>() {
                                        for f in &files_for_expand {
                                            api.expand_file(f.path.clone());
                                        }
                                        collapsed_files.set(HashSet::new());
                                    }
                                })
                                .child(meta_text("展开")),
                        ),
                )
                .child(scroll().flex_1().w_full().child(list))
                .into_element()
        });

        // --- Diff Viewport (Mounted once per Ready state; updates reactively without losing scroll!) ---
        let tx_diff = actions_tx.clone();
        let dv_for_body = dv_ref;
        let diff_viewport = dynamic(move || match body_state.get() {
            DiffBodyState::Error(error) => view()
                .flex_1()
                .px_4()
                .py_4()
                .child(
                    text(error)
                        .text_size(text_xs_size)
                        .text_color(move || active_theme().destructive),
                )
                .into_element(),
            DiffBodyState::Loading => view()
                .flex_1()
                .px_4()
                .py_4()
                .child(meta_text("正在读取补丁…"))
                .into_element(),
            DiffBodyState::Empty => view()
                .flex_1()
                .px_4()
                .py_4()
                .child(meta_text("当前范围没有内容变更"))
                .into_element(),
            DiffBodyState::Ready => {
                let tx_add = tx_diff.clone();
                let tx_edit = tx_diff.clone();
                let dv_header = dv_for_body;
                let dv_stats = dv_for_body;

                diff_view()
                    .component_ref(&dv_for_body)
                    .flex_1()
                    .min_w_0()
                    .layout(move || layout.get())
                    .soft_wrap(move || soft_wrap.get())
                    .indicators(DiffIndicators::Classic)
                    .patch(move || patch_text_sig.get().map(SharedString::from))
                    .annotations(move || annotations_memo.get())
                    .on_stats_change(move |stats: &DiffStats| {
                        total_hunks.set(stats.hunks);
                        if let Some(target_file) = pending_focus_file.get_untracked()
                            && let Some(api) = dv_stats.api::<DiffViewApi>()
                        {
                            api.expand_file(target_file.clone());
                            api.scroll_to_file(target_file.clone());
                            active_file.set(Some(target_file));
                            pending_focus_file.set(None);
                        }
                    })
                    .on_file_toggle(move |file: &DiffFile| {
                        let path = file.path.to_string();
                        collapsed_files.update(|set| {
                            if file.collapsed {
                                set.insert(path);
                            } else {
                                set.remove(&path);
                            }
                        });
                    })
                    .render_file_header(move |file: &DiffFile| {
                        let path = file.path.to_string();
                        let display_name = match &file.old_path {
                            Some(old) => format!("{old} → {path}"),
                            None => path.clone(),
                        };
                        let status = diff_file_status_char(file.status);
                        let collapsed = file.collapsed;
                        let added = file.added;
                        let deleted = file.deleted;
                        let binary = file.binary;

                        let dv_toggle = dv_header;
                        let toggle_path = path.clone();
                        let dv_viewed = dv_header;
                        let viewed_path = path.clone();
                        let viewed_path_check = path.clone();

                        view()
                            .flex_row()
                            .w_full()
                            .h_full()
                            .items_center()
                            .px_3()
                            .gap_2()
                            .bg(move || active_theme().secondary)
                            .border_b_1()
                            .border_b_color(move || active_theme().border)
                            .child(
                                view()
                                    .role(AccessibilityRole::Button)
                                    .label(format!("折叠或展开 {path}"))
                                    .flex_row()
                                    .flex_1()
                                    .min_w_0()
                                    .items_center()
                                    .gap_1p5()
                                    .app_class("cursor-pointer")
                                    .on_press(move || {
                                        active_file.set(Some(toggle_path.clone()));
                                        if let Some(api) = dv_toggle.api::<DiffViewApi>() {
                                            api.toggle_file(toggle_path.clone());
                                        }
                                    })
                                    .child(
                                        text(if collapsed { "▸" } else { "▾" })
                                            .w(px(10.0))
                                            .flex_shrink_0()
                                            .text_size(text_xs_size)
                                            .text_color(move || active_theme().muted_foreground),
                                    )
                                    .child(
                                        text(status.to_string())
                                            .w(px(12.0))
                                            .flex_shrink_0()
                                            .font_family(mono_font)
                                            .text_size(text_xs_size)
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(move || status_badge_color(status)),
                                    )
                                    .child(
                                        text(display_name)
                                            .flex_1()
                                            .min_w_0()
                                            .app_class("truncate")
                                            .font_family(mono_font)
                                            .text_size(text_xs_size)
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(move || active_theme().foreground),
                                    ),
                            )
                            .when(binary, |r| r.child(meta_text("二进制文件")))
                            .when(!binary && (added > 0 || deleted > 0), |r| {
                                r.child(
                                    view()
                                        .flex_row()
                                        .flex_shrink_0()
                                        .gap_1()
                                        .child(
                                            text(format!("+{added}"))
                                                .font_family(mono_font)
                                                .text_size(text_xs_size)
                                                .text_color(success_color),
                                        )
                                        .child(
                                            text(format!("−{deleted}"))
                                                .font_family(mono_font)
                                                .text_size(text_xs_size)
                                                .text_color(move || active_theme().destructive),
                                        ),
                                )
                            })
                            .child(
                                view()
                                    .role(AccessibilityRole::Button)
                                    .label(format!("标记 {path} 为已查看"))
                                    .flex_row()
                                    .flex_shrink_0()
                                    .items_center()
                                    .gap_1()
                                    .px_1p5()
                                    .rounded(px(4.0))
                                    .app_class("cursor-pointer")
                                    .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                                    .on_press(move || {
                                        let now_viewed =
                                            !viewed_files.get_untracked().contains(&viewed_path);
                                        viewed_files.update(|set| {
                                            if now_viewed {
                                                set.insert(viewed_path.clone());
                                            } else {
                                                set.remove(&viewed_path);
                                            }
                                        });
                                        if let Some(api) = dv_viewed.api::<DiffViewApi>() {
                                            if now_viewed {
                                                api.collapse_file(viewed_path.clone());
                                            } else {
                                                api.expand_file(viewed_path.clone());
                                            }
                                        }
                                    })
                                    .child(
                                        text(move || {
                                            if viewed_files.get().contains(&viewed_path_check) {
                                                "✓ 已查看"
                                            } else {
                                                "已查看"
                                            }
                                        })
                                        .text_size(text_xs_size)
                                        .text_color(
                                            move || {
                                                if viewed_files.get().contains(&path) {
                                                    success_color()
                                                } else {
                                                    active_theme().muted_foreground
                                                }
                                            },
                                        ),
                                    ),
                            )
                            .into_element()
                    })
                    .render_separator(move |sep: &DiffSeparator| {
                        let label = match &sep.header {
                            Some(h) if !h.trim().is_empty() => format!("@@ {}", h.trim()),
                            Some(_) => "@@ 改动片段".to_string(),
                            None => format!("⋯ 展开 {} 行上下文", sep.lines),
                        };
                        view()
                            .flex_row()
                            .w_full()
                            .h_full()
                            .items_center()
                            .px_3()
                            .bg(move || active_theme().muted.opacity(0.65))
                            .child(
                                text(label)
                                    .app_class("truncate")
                                    .font_family(mono_font)
                                    .text_size(text_xs_size)
                                    .text_color(move || active_theme().muted_foreground),
                            )
                            .into_element()
                    })
                    .render_add_annotation(move |range: &DiffLineRange| {
                        let resolved = index_sig.get_untracked().resolve_range(*range);
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
                            .app_class("cursor-pointer")
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
                        let Some(comment) = comments_sig
                            .get_untracked()
                            .into_iter()
                            .find(|c| c.annotation_id() == ann.id.as_ref())
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
                            .gap_1p5()
                            .p_2p5()
                            .rounded(move || active_theme().radius)
                            .border_1()
                            .border_color(move || active_theme().border)
                            .bg(move || active_theme().card)
                            .child(
                                view()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        text(loc)
                                            .flex_1()
                                            .min_w_0()
                                            .font_family(mono_font)
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
                                            .app_class("cursor-pointer")
                                            .hover(|s| {
                                                s.bg(move || active_theme().accent.opacity(0.6))
                                            })
                                            .on_press(move || {
                                                send(&tx_remove, PatchAction::RemoveComment(idx));
                                            })
                                            .child(icon_svg("icons/x.svg", 11.0).text_color(
                                                move || active_theme().muted_foreground,
                                            )),
                                    ),
                            )
                            .child(
                                input()
                                    .w_full()
                                    .min_h(px(44.0))
                                    .max_h(px(140.0))
                                    .multiline(true)
                                    .placeholder("写下对这个改动片段的意见")
                                    .placeholder_color(move || {
                                        active_theme().muted_foreground.light
                                    })
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
            }
        });

        // --- Bottom Anchored Review Footer with Agent-Title-First Target Picker ---
        let tx_footer = actions_tx.clone();
        let footer = dynamic(move || {
            let comments = comments_sig.get();
            let count = comments.len();
            let notice = notice_sig.get();
            let sent = sent_sig.get();
            let targets = targets_sig.get();
            let active_target = active_target_sig.get();
            let menu_open = target_menu_open.get();

            let (hint_text, hint_kind) = match &notice {
                Some(SendNotice::Pasted(pane)) => (format!("已粘贴到 {pane}，按 Enter 提交"), 1u8),
                Some(SendNotice::Hint(problem)) => (problem.clone(), 2u8),
                None if count == 0 => match &sent {
                    Some(sent) => (
                        format!("上次已发送 {} 条评论到 {}", sent.count, sent.target),
                        0u8,
                    ),
                    None => ("悬停代码行点击 + 添加审查意见".to_string(), 0u8),
                },
                None => (format!("{count} 条评论待发送"), 0u8),
            };
            let hint_color = move || match hint_kind {
                1 => warning_color(),
                2 => active_theme().destructive,
                _ => active_theme().muted_foreground,
            };

            let active_pane = active_target
                .and_then(|pane| targets.iter().find(|t| t.pane == pane))
                .or_else(|| targets.iter().find(|t| t.focused))
                .or_else(|| targets.first())
                .cloned();

            let mut bar = view()
                .relative()
                .flex_row()
                .flex_shrink_0()
                .w_full()
                .px_4()
                .py_2p5()
                .gap_2()
                .items_center()
                .border_t_1()
                .border_t_color(move || active_theme().border)
                .bg(move || active_theme().background)
                .child(
                    text(hint_text)
                        .flex_1()
                        .min_w_0()
                        .app_class("truncate")
                        .text_size(text_xs_size)
                        .text_color(hint_color),
                );

            if let Some(active) = active_pane {
                let active_id = active.pane;
                let state = active.state;
                let has_agent = active.agent.is_some();
                let trigger_title = active.label();
                let trigger_icon = render_target_icon(&active, 14.0);

                let mut picker_wrapper = view().relative().flex_row().items_center().gap_1p5();
                picker_wrapper = picker_wrapper.child(meta_text("发送到"));

                let trigger = view()
                    .role(AccessibilityRole::Button)
                    .label("选择接收审查意见的 Agent 窗格")
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .max_w(px(280.0))
                    .px_2()
                    .py_1()
                    .rounded(move || active_theme().radius)
                    .border_1()
                    .border_color(move || {
                        if menu_open {
                            link_color()
                        } else {
                            active_theme().border
                        }
                    })
                    .bg(move || active_theme().card)
                    .app_class("cursor-pointer")
                    .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                    .on_press(move || target_menu_open.update(|v| *v = !*v))
                    .child(trigger_icon)
                    .child(
                        text(trigger_title)
                            .flex_1()
                            .min_w_0()
                            .app_class("truncate")
                            .text_size(text_xs_size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(move || active_theme().foreground),
                    )
                    .when(has_agent, |btn| {
                        btn.child(
                            view()
                                .w(px(6.0))
                                .h(px(6.0))
                                .flex_shrink_0()
                                .rounded(px(3.0))
                                .bg(move || agent_state_color(state)),
                        )
                    })
                    .child(
                        icon_svg("icons/chevron-down.svg", 11.0)
                            .text_color(move || active_theme().muted_foreground),
                    );

                picker_wrapper = picker_wrapper.child(trigger);

                if menu_open {
                    let mut menu = view()
                        .absolute()
                        .right_0()
                        .bottom(px(34.0))
                        .w(px(340.0))
                        .max_h(px(280.0))
                        .flex_col()
                        .p_1()
                        .gap_0p5()
                        .rounded(move || active_theme().radius)
                        .border_1()
                        .border_color(move || active_theme().border)
                        .bg(move || active_theme().popover)
                        .app_class("shadow-lg");

                    let mut list = view().flex_col().w_full().gap_0p5();
                    for target in &targets {
                        let pane = target.pane;
                        let selected = pane == active_id;
                        let title = target.label();
                        let sub = target.subtitle();
                        let state_text = agent_state_label(target);
                        let st = target.state;
                        let is_agent = target.agent.is_some();
                        let icon = render_target_icon(target, 15.0);
                        let tx_pick = tx_footer.clone();

                        list = list.child(
                            view()
                                .role(AccessibilityRole::Button)
                                .label(format!("选择 {title}"))
                                .flex_row()
                                .w_full()
                                .px_2()
                                .py_1p5()
                                .gap_2()
                                .items_center()
                                .rounded(px(4.0))
                                .app_class("cursor-pointer")
                                .bg(move || {
                                    if selected {
                                        active_theme().accent
                                    } else {
                                        AdaptiveColor::TRANSPARENT
                                    }
                                })
                                .hover(|s| s.bg(move || active_theme().accent.opacity(0.65)))
                                .on_press(move || {
                                    target_menu_open.set(false);
                                    send(&tx_pick, PatchAction::SetTarget(pane));
                                })
                                .child(icon)
                                .child(
                                    view()
                                        .flex_col()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_0p5()
                                        .child(
                                            view()
                                                .flex_row()
                                                .items_center()
                                                .gap_1p5()
                                                .min_w_0()
                                                .child(
                                                    text(title)
                                                        .flex_1()
                                                        .min_w_0()
                                                        .app_class("truncate")
                                                        .text_size(text_xs_size)
                                                        .font_weight(FontWeight::MEDIUM)
                                                        .text_color(move || {
                                                            active_theme().popover_foreground
                                                        }),
                                                )
                                                .child(
                                                    view()
                                                        .flex_row()
                                                        .flex_shrink_0()
                                                        .items_center()
                                                        .gap_1()
                                                        .when(is_agent, |s| {
                                                            s.child(
                                                                view()
                                                                    .w(px(6.0))
                                                                    .h(px(6.0))
                                                                    .rounded(px(3.0))
                                                                    .bg(move || {
                                                                        agent_state_color(st)
                                                                    }),
                                                            )
                                                        })
                                                        .child(
                                                            text(state_text)
                                                                .text_size(text_xs_size)
                                                                .text_color(move || {
                                                                    if is_agent {
                                                                        agent_state_color(st)
                                                                    } else {
                                                                        active_theme()
                                                                            .muted_foreground
                                                                    }
                                                                }),
                                                        ),
                                                ),
                                        )
                                        .child(meta_text(sub).app_class("truncate")),
                                )
                                .when(selected, |row| {
                                    row.child(
                                        icon_svg("icons/check.svg", 13.0).text_color(link_color),
                                    )
                                }),
                        );
                    }
                    menu = menu.child(scroll().w_full().max_h(px(268.0)).child(list));
                    picker_wrapper = picker_wrapper.child(menu);
                }

                bar = bar.child(picker_wrapper);
            }

            if count > 0 {
                let tx_send = tx_footer.clone();
                bar = bar.child(
                    kumu_button(format!("发送 ({count})"), ButtonVariant::Primary).on_press(
                        move || {
                            target_menu_open.set(false);
                            send(&tx_send, PatchAction::SendComments);
                        },
                    ),
                );
            }

            bar
        });

        let main_body = view()
            .flex_row()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(file_sidebar)
            .child(diff_viewport);

        let panel = view()
            .flex_col()
            .flex_1()
            .w_full()
            .h_full()
            .bg(move || active_theme().background)
            .border_l_1()
            .border_l_color(move || active_theme().border)
            .font_family(move || {
                active_theme()
                    .font_sans
                    .clone()
                    .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
            })
            .child(header)
            .child(banners)
            .child(main_body)
            .child(footer);

        let tx_sheet = actions_tx.clone();

        sheet()
            .open(move || Some(open.get()))
            .side(Placement::Right)
            .close_on_escape(true)
            .close_on_backdrop_press(true)
            .backdrop_class("bg-black/35")
            .popup_class(move || {
                if wide_mode.get() {
                    SharedString::from("w-[96%] max-w-[96%] h-full shadow-xl")
                } else {
                    SharedString::from("w-[1040px] max-w-[94%] h-full shadow-xl")
                }
            })
            .on_open_change(move |ev: &nsh_base::SheetEvent| {
                if matches!(ev, nsh_base::SheetEvent::OpenChange(false, _)) {
                    open.set(false);
                }
            })
            .on_close(move || {
                send(&tx_sheet, PatchAction::Close);
            })
            .child(panel)
    }
}

/// The sheet body: one patch (single-file or multi-file combined diff) rendered
/// via Pure Rust NSH `DiffView`, with background git reading, staleness polling,
/// and review draft persistence.
pub(super) struct PatchView {
    mount_id: u64,
    project: i64,
    title: String,
    meta: String,
    host: PluginHost,
    key: PatchKey,
    reviews: Rc<RefCell<ReviewStore>>,
    notify_panel: Rc<dyn Fn(&mut App)>,
    source: PatchSource,
    initial_file: Option<String>,
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
            title,
            meta,
            key,
            source,
            message,
            context,
            initial_file,
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
        let (comments, next_comment, sent) = Self::load_review_state(&reviews, &key);
        let (events_tx, events_rx) = unbounded::<PatchHostEvent>();
        let (actions_tx, actions_rx) = unbounded::<PatchAction>();

        let initial = PatchSnapshot {
            title: title.clone(),
            meta: meta.clone(),
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
            working_tree_scope: source.working_tree_scope(),
            initial_file: initial_file.clone(),
        };
        let mount_id = register_mount(PatchMount {
            initial,
            events_rx,
            actions_tx,
        });

        let should_watch = matches!(source, PatchSource::WorkingTree { .. });
        let mut view = Self {
            mount_id,
            project,
            title,
            meta,
            host,
            key,
            reviews,
            notify_panel,
            source,
            initial_file,
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
        if should_watch {
            view.watch(cx);
        }
        view
    }

    fn load_review_state(
        reviews: &Rc<RefCell<ReviewStore>>,
        key: &PatchKey,
    ) -> (Vec<ReviewComment>, u64, Option<SentReview>) {
        let drafts = reviews
            .borrow()
            .drafts
            .get(key)
            .cloned()
            .unwrap_or_default();
        let sent = reviews.borrow().sent.get(key).cloned();
        let mut comments = Vec::with_capacity(drafts.len());
        for (index, draft) in drafts.into_iter().enumerate() {
            comments.push(ReviewComment {
                index: index as u64,
                range: draft.range,
                text: draft.text,
            });
        }
        let next_comment = comments.len() as u64;
        (comments, next_comment, sent)
    }

    /// Mount the Pure Rust NSH `AppView` on the first window paint (or when opened
    /// in a sheet).
    pub(super) fn ensure_nsh_mounted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.app_view.is_some() {
            return;
        }
        let kumu_theme = KumuTheme::global(cx);
        let initial_theme = nsh_theme(kumu_theme);
        let initial_appearance = appearance(kumu_theme.mode);
        let mount_id = self.mount_id;

        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu Diff Review".to_owned(),
                // Keep the overlay surface transparent so the underlying Kumu
                // workspace shows through the sheet's dimmed backdrop.
                transparent: true,
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

        self._theme_sub = Some(cx.observe_global::<KumuTheme>(|this: &mut Self, cx| {
            let kumu_theme = KumuTheme::global(cx);
            let (mapped, mode) = (nsh_theme(kumu_theme), appearance(kumu_theme.mode));
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
                        PatchAction::SetScope(scope) => view.set_scope(scope, cx),
                        PatchAction::AddComment(range) => view.add_comment(range, cx),
                        PatchAction::UpdateComment { index, text } => {
                            view.update_comment(index, text, cx);
                        }
                        PatchAction::RemoveComment(index) => view.remove_comment(index, cx),
                        PatchAction::SetTarget(pane) => view.set_target(pane, cx),
                        PatchAction::SendComments => {
                            if view.send_comments(cx) {
                                view.request_close();
                            }
                        }
                        PatchAction::Close => {
                            view.reviews.borrow().persist();
                            view.host.close_sheet(window, cx);
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
            title: self.title.clone(),
            meta: self.meta.clone(),
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
            working_tree_scope: self.source.working_tree_scope(),
            initial_file: self.initial_file.clone(),
        }
    }

    fn push_snapshot(&self, cx: &App) {
        let _ = self
            .events_tx
            .unbounded_send(PatchHostEvent::Snapshot(Box::new(self.snapshot(cx))));
    }

    /// Ask the NSH `Sheet` to animate shut; once its exit slide finishes,
    /// `SheetEvent::Close` emits `PatchAction::Close` to unmount the host view.
    pub(super) fn request_close(&self) {
        let _ = self.events_tx.unbounded_send(PatchHostEvent::Dismiss);
    }

    fn set_scope(&mut self, scope: WorkingTreeDiffScope, cx: &mut Context<Self>) {
        let PatchSource::WorkingTree {
            root,
            scope: current,
        } = &self.source
        else {
            return;
        };
        if *current == scope {
            return;
        }
        self.save_drafts(cx);
        let root = root.clone();
        self.source = PatchSource::WorkingTree { root, scope };
        self.key = self.source.key(self.project);
        self.context = match scope {
            WorkingTreeDiffScope::All => "工作区未提交改动".to_string(),
            WorkingTreeDiffScope::Staged => "已暂存改动".to_string(),
            WorkingTreeDiffScope::Unstaged => "未暂存改动".to_string(),
        };
        let (comments, next_comment, sent) = Self::load_review_state(&self.reviews, &self.key);
        self.comments = comments;
        self.next_comment = next_comment;
        self.sent = sent;
        self.notice = None;
        self.digest = None;
        self.stale = false;
        self.error = None;
        self.patch_text = None;
        self.index = PatchIndex::default();
        self.reading = false;
        self.push_snapshot(cx);
        self.reload(cx);
        cx.notify();
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
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                let next = match this.update(cx, |view, _| {
                    (!view.reading && !view.stale)
                        .then(|| (view.read_generation, view.source.clone()))
                }) {
                    Ok(Some(pair)) => pair,
                    Ok(None) => continue,
                    Err(_) => break,
                };
                let (generation, source) = next;
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
            self.reviews.borrow().persist();
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
                self.reviews.borrow().persist();
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

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_engine::EngineConfig;

    #[test]
    fn patch_sheet_nsh_renders_combined_diff_and_agent_target() {
        let (_events_tx, events_rx) = unbounded::<PatchHostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<PatchAction>();

        let raw = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,2 @@
-let old = 1;
+let new = 2;
diff --git a/src/util.rs b/src/util.rs
--- a/src/util.rs
+++ b/src/util.rs
@@ -1,1 +1,2 @@
 fn a() {}
+fn b() {}
";
        let (normalized, index) = normalize_and_index_patch(raw, false);
        assert_eq!(index.files.len(), 2);
        let initial = PatchSnapshot {
            title: "未提交改动".into(),
            meta: "main · 2 个改动项".into(),
            patch_text: Some(normalized),
            index,
            working_tree_scope: Some(WorkingTreeDiffScope::All),
            targets: vec![PaneTarget {
                pane: 1,
                ordinal: 1,
                tab_name: "主工作区".into(),
                title: "优化 Git Diff 折叠视图".into(),
                agent: Some("codex".into()),
                state: None,
                focused: true,
            }],
            active_target: Some(1),
            ..Default::default()
        };

        let mount_id = register_mount(PatchMount {
            initial,
            events_rx,
            actions_tx,
        });
        let root =
            nsh_core::element::Root::new::<nsh_view::PatchRoot>(move || nsh_view::PatchRootProps {
                mount_id: mount_id.into(),
            });
        let mut app = nsh_testing::Headless::start(EngineConfig::new(root))
            .expect("start headless diff sheet");
        app.set_viewport(nsh_core::Size::new(
            nsh_core::Points(1280.0),
            nsh_core::Points(800.0),
        ))
        .expect("set viewport");

        app.wait_for("text:未提交改动")
            .expect("diff sheet header renders");
        app.wait_for("文件 (2)")
            .expect("multi-file sidebar renders");
        app.wait_for("优化 Git Diff 折叠视图")
            .expect("agent session title renders in target selector");
        app.press("label:关闭 Diff 面板")
            .expect("press close diff sheet button");
        assert!(matches!(
            actions_rx.try_recv().ok(),
            Some(PatchAction::Close)
        ));
    }
}
