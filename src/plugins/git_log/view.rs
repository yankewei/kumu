//! Pure Rust NSH UI for the right-hand Git panel (uncommitted changes &
//! virtualized commit history).

use std::sync::Arc;

use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use nsh_base::{input, theme as active_theme};
use nsh_core::element::{AccessibilityRole, SignalEquals, signal_with, virtual_list};
use nsh_core::prelude::*;

use super::review::{PatchKey, ReviewMark, ReviewMarks};
use super::{
    CHANGE_ROW_HEIGHT, ChangeRow, GitHostEvent, GitPanelAction, GitPanelSnapshot, PanelTab,
    ROW_HEIGHT, file_name, parent_dir, take_panel_mount,
};
use crate::git::Commit;
use crate::nsh_ui::assets::icon_svg;
use crate::nsh_ui::components::meta_text;
use crate::nsh_ui::theme::{TOKEN_LINK, TOKEN_SUCCESS, TOKEN_WARNING, kumu_color, text_xs_size};

#[derive(Clone, Debug, PartialEq, Props)]
pub(super) struct GitLogRootProps {
    pub(super) mount_id: Prop<u64>,
}

fn send_action(tx: &Option<UnboundedSender<GitPanelAction>>, action: GitPanelAction) {
    if let Some(tx) = tx {
        let _ = tx.unbounded_send(action);
    }
}

fn link_color() -> AdaptiveColor {
    kumu_color(TOKEN_LINK, |t| t.primary)
}

fn warning_color() -> AdaptiveColor {
    kumu_color(TOKEN_WARNING, |t| t.destructive)
}

fn success_color() -> AdaptiveColor {
    kumu_color(TOKEN_SUCCESS, |t| t.primary)
}

fn status_color(status: char) -> AdaptiveColor {
    match status {
        'M' | 'T' => warning_color(),
        'A' | '?' => success_color(),
        'D' | 'U' => active_theme().destructive,
        'R' | 'C' => link_color(),
        _ => active_theme().muted_foreground,
    }
}

fn render_mark(mark: Option<ReviewMark>) -> Option<AnyElement> {
    match mark? {
        ReviewMark::Draft => Some(
            text("草稿")
                .flex_shrink_0()
                .text_size(text_xs_size)
                .text_color(warning_color)
                .into_element(),
        ),
        ReviewMark::Sent => Some(
            text("已审查")
                .flex_shrink_0()
                .text_size(text_xs_size)
                .text_color(success_color)
                .into_element(),
        ),
    }
}

fn render_ref_badges(refs: &[String]) -> Vec<AnyElement> {
    let shown = refs.len().min(1);
    let mut badges = Vec::new();
    for ref_name in refs.iter().take(shown) {
        let (label, is_tag) = if let Some(name) = ref_name.strip_prefix("HEAD -> ") {
            (name, false)
        } else if let Some(name) = ref_name.strip_prefix("tag: ") {
            (name, true)
        } else {
            (ref_name.as_str(), false)
        };
        let color = move || {
            if is_tag {
                warning_color()
            } else {
                link_color()
            }
        };
        badges.push(
            view()
                .flex_shrink_0()
                .max_w(px(96.0))
                .px_1()
                .py_0p5()
                .rounded(move || active_theme().radius)
                .bg(move || color().opacity(0.14))
                .child(
                    text(label.to_string())
                        .app_class("truncate")
                        .text_size(text_xs_size)
                        .text_color(color),
                )
                .into_element(),
        );
    }
    if refs.len() > shown {
        badges.push(
            text(format!("+{}", refs.len() - shown))
                .flex_shrink_0()
                .text_size(text_xs_size)
                .text_color(move || active_theme().muted_foreground)
                .into_element(),
        );
    }
    badges
}

fn empty_view(message: impl Into<SharedString>, is_error: bool) -> AnyElement {
    let msg: SharedString = message.into();
    view()
        .flex_1()
        .w_full()
        .px_3()
        .py_4()
        .child(text(msg).text_size(text_xs_size).text_color(move || {
            if is_error {
                active_theme().destructive
            } else {
                active_theme().muted_foreground
            }
        }))
        .into_element()
}

#[component]
pub(super) fn GitLogRoot(props: GitLogRootProps) -> impl IntoElement {
    let mount = take_panel_mount(props.mount_id.get_untracked());
    let (initial, events_rx, actions_tx) = match mount {
        Some(m) => (m.initial, Some(m.events_rx), Some(m.actions_tx)),
        None => (GitPanelSnapshot::default(), None, None),
    };

    let state = signal_with(initial, SignalEquals::Never);
    let search_draft = signal(String::new());

    if let Some(mut rx) = events_rx {
        spawn(async move {
            while let Some(event) = rx.next().await {
                match event {
                    GitHostEvent::Snapshot(next) => {
                        state.set(next);
                    }
                }
            }
        });
    }

    let tx_tabs = actions_tx.clone();
    let tx_search = actions_tx.clone();
    let tx_clear_search = actions_tx.clone();
    let tx_body = actions_tx.clone();

    let toolbar = dynamic(move || {
        let snap = state.get();
        if snap.project_id.is_none() {
            return view().into_element();
        }
        let changes_count = snap.change_count;
        let total_commits = snap.commits.len();
        let changes_label = if changes_count > 0 {
            format!("改动 {changes_count}")
        } else {
            "改动".to_string()
        };
        let history_label = if total_commits > 0 {
            format!("历史 {total_commits}")
        } else {
            "历史".to_string()
        };
        let active_tab = snap.tab;
        let branch = snap.branch.clone().unwrap_or_else(|| "—".to_string());

        let tx_c = tx_tabs.clone();
        let tx_h = tx_tabs.clone();
        let make_tab_btn =
            move |label: String, target: PanelTab, tx: Option<UnboundedSender<GitPanelAction>>| {
                let active = active_tab == target;
                let mut btn = view()
                    .role(AccessibilityRole::Button)
                    .px_2()
                    .py_0p5()
                    .rounded(px(4.0))
                    .items_center()
                    .justify_center()
                    .app_class("cursor-pointer")
                    .on_press(move || send_action(&tx, GitPanelAction::SelectTab(target)));
                if active {
                    btn = btn.bg(move || active_theme().background);
                } else {
                    btn = btn.hover(|s| s.bg(move || active_theme().accent.opacity(0.4)));
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

        let mut bar = view().flex_col().flex_shrink_0().child(
            view()
                .flex_row()
                .px_3()
                .pt_1p5()
                .pb_1()
                .gap_2()
                .items_center()
                .border_b_1()
                .border_b_color(move || active_theme().sidebar_border)
                .child(
                    view()
                        .flex_row()
                        .flex_shrink_0()
                        .p_0p5()
                        .gap_0p5()
                        .rounded(move || active_theme().radius)
                        .bg(move || active_theme().muted)
                        .child(make_tab_btn(changes_label, PanelTab::Changes, tx_c))
                        .child(make_tab_btn(history_label, PanelTab::History, tx_h)),
                )
                .child(
                    view()
                        .flex_1()
                        .min_w_0()
                        .flex_row()
                        .justify_end()
                        .child(
                            meta_text(branch)
                                .flex_shrink(1.0)
                                .min_w_0()
                                .app_class("truncate"),
                        ),
                ),
        );

        if active_tab == PanelTab::History {
            let tx_q = tx_search.clone();
            let tx_clr = tx_clear_search.clone();
            bar = bar.child(
                view()
                    .px_2()
                    .py_1p5()
                    .border_b_1()
                    .border_b_color(move || active_theme().sidebar_border)
                    .child(
                        view()
                            .flex_row()
                            .items_center()
                            .gap_1p5()
                            .px_2()
                            .h(px(26.0))
                            .rounded(move || active_theme().radius)
                            .border_1()
                            .border_color(move || active_theme().input)
                            .bg(move || active_theme().background)
                            .child(
                                icon_svg("icons/search.svg", 12.0)
                                    .text_color(move || active_theme().muted_foreground),
                            )
                            .child(
                                input()
                                    .flex_1()
                                    .placeholder("搜索提交信息")
                                    .placeholder_color(move || {
                                        active_theme().muted_foreground.light
                                    })
                                    .text_size(text_xs_size)
                                    .text_color(move || active_theme().foreground)
                                    .value(move || search_draft.get())
                                    .on_change(move |v: &str| {
                                        search_draft.set(v.to_owned());
                                        send_action(&tx_q, GitPanelAction::SetQuery(v.to_owned()));
                                    }),
                            )
                            .child(when(
                                move || !search_draft.get().is_empty(),
                                move || {
                                    let tx_clr = tx_clr.clone();
                                    view()
                                        .role(AccessibilityRole::Button)
                                        .label("清空搜索")
                                        .app_class("cursor-pointer")
                                        .on_press(move || {
                                            search_draft.set(String::new());
                                            send_action(
                                                &tx_clr,
                                                GitPanelAction::SetQuery(String::new()),
                                            );
                                        })
                                        .child(
                                            icon_svg("icons/x.svg", 11.0).text_color(move || {
                                                active_theme().muted_foreground
                                            }),
                                        )
                                },
                            )),
                    ),
            );
        }

        bar.into_element()
    });

    let body = dynamic(move || {
        let snap = state.get();
        let Some(project_id) = snap.project_id else {
            return empty_view("没有选中的项目", false);
        };
        if let Some(error) = snap.error {
            return empty_view(error, true);
        }
        if !snap.loaded {
            return empty_view("正在读取…", false);
        }

        let marks: Arc<ReviewMarks> = Arc::new(snap.marks);

        if snap.tab == PanelTab::Changes {
            let branch_banner = snap.branch_diff.clone().map(|summary| {
                let mark = marks.mark(&PatchKey::branch_diff(
                    project_id,
                    &summary.base,
                    &summary.head,
                ));
                let tx_branch = tx_body.clone();
                let title = format!("分支改动 vs {}", summary.base);
                let subtitle = format!("{} 提交 · {} 文件", summary.commits, summary.files);
                let mut card = view()
                    .role(AccessibilityRole::Button)
                    .flex_row()
                    .w_full()
                    .px_2p5()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .rounded(move || active_theme().radius)
                    .border_1()
                    .border_color(move || active_theme().sidebar_border)
                    .bg(move || active_theme().background)
                    .app_class("cursor-pointer")
                    .hover(|s| s.bg(move || active_theme().accent.opacity(0.45)))
                    .on_press(move || send_action(&tx_branch, GitPanelAction::OpenBranchDiff))
                    .child(icon_svg("icons/git-branch.svg", 13.0).text_color(link_color))
                    .child(
                        view()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                text(title)
                                    .app_class("truncate")
                                    .text_size(text_xs_size)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(move || active_theme().foreground),
                            )
                            .child(meta_text(subtitle).app_class("truncate")),
                    );
                if let Some(mark_el) = render_mark(mark) {
                    card = card.child(mark_el);
                }
                card = card.child(
                    icon_svg("icons/chevron-right.svg", 12.0)
                        .text_color(move || active_theme().muted_foreground),
                );
                view()
                    .flex_shrink_0()
                    .w_full()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .child(card)
            });

            if snap.change_rows.is_empty() {
                if let Some(banner) = branch_banner {
                    return view()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .child(banner)
                        .child(empty_view("工作区没有未提交的改动", false))
                        .into_element();
                }
                return empty_view("工作区没有未提交的改动", false);
            }
            let rows = Arc::new(snap.change_rows);
            let rows_len = rows.len();
            let tx = tx_body.clone();
            let list = virtual_list(rows_len, move |pos| {
                let Some(row) = rows.get(pos.index()) else {
                    return view().h(px(CHANGE_ROW_HEIGHT)).into_element();
                };
                match row {
                    ChangeRow::Header(label) => view()
                        .flex_row()
                        .w_full()
                        .h(px(CHANGE_ROW_HEIGHT))
                        .px_2()
                        .items_center()
                        .child(
                            text(*label)
                                .text_size(text_xs_size)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(move || active_theme().muted_foreground),
                        )
                        .into_element(),
                    ChangeRow::File(change) => {
                        let trailing = match &change.original {
                            Some(original) => format!("← {original}"),
                            None => parent_dir(&change.path).to_string(),
                        };
                        let status = change.status;
                        let mark = marks.mark(&PatchKey::change(project_id, change));
                        let clicked = change.clone();
                        let tx = tx.clone();

                        let mut row_el = view()
                            .role(AccessibilityRole::Button)
                            .flex_row()
                            .w_full()
                            .h(px(CHANGE_ROW_HEIGHT))
                            .px_2()
                            .gap_1p5()
                            .items_center()
                            .app_class("cursor-pointer")
                            .hover(|s| s.bg(move || active_theme().accent.opacity(0.35)))
                            .on_press(move || {
                                send_action(&tx, GitPanelAction::OpenChange(clicked.clone()));
                            })
                            .child(
                                text(status.to_string())
                                    .w(px(12.0))
                                    .flex_shrink_0()
                                    .font_family(move || {
                                        active_theme().font_mono.clone().unwrap_or_else(|| {
                                            SharedString::from(crate::fonts::TERM_FONT)
                                        })
                                    })
                                    .text_size(text_xs_size)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(move || status_color(status)),
                            )
                            .child(
                                text(file_name(&change.path).to_string())
                                    .flex_1()
                                    .min_w_0()
                                    .app_class("truncate")
                                    .text_size(text_xs_size)
                                    .text_color(move || active_theme().sidebar_foreground),
                            );
                        if !trailing.is_empty() {
                            row_el = row_el.child(
                                meta_text(trailing)
                                    .max_w(px(120.0))
                                    .min_w_0()
                                    .app_class("truncate"),
                            );
                        }
                        if let Some(mark_el) = render_mark(mark) {
                            row_el = row_el.child(mark_el);
                        }
                        row_el.into_element()
                    }
                }
            })
            .estimated_item_size(px(CHANGE_ROW_HEIGHT))
            .flex_1()
            .into_element();
            if let Some(banner) = branch_banner {
                return view()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(banner)
                    .child(list)
                    .into_element();
            }
            return list;
        }

        // History tab
        let visible = snap.filter.as_ref().map_or(snap.commits.len(), |f| f.len());
        if visible == 0 {
            return if snap.filter.is_some() {
                empty_view("没有匹配的提交", false)
            } else {
                empty_view("这个仓库还没有提交", false)
            };
        }

        let commits: Arc<Vec<Commit>> = snap.commits;
        let filter: Option<Arc<Vec<usize>>> = snap.filter;
        let tx = tx_body.clone();
        virtual_list(visible, move |pos| {
            let row = pos.index();
            let commit_idx = filter.as_ref().map_or(row, |f| f[row]);
            let Some(commit) = commits.get(commit_idx) else {
                return view().h(px(ROW_HEIGHT)).into_element();
            };
            let first = row == 0;
            let last = row + 1 == visible;
            let is_merge = commit.parents > 1;
            let mark = marks.mark(&PatchKey::commit(project_id, &commit.hash));
            let clicked = commit.clone();
            let tx = tx.clone();

            let lane = view()
                .relative()
                .flex_shrink_0()
                .w(px(16.0))
                .h_full()
                .child(
                    view()
                        .absolute()
                        .left(px(7.0))
                        .w(px(1.0))
                        .top(px(if first { 16.0 } else { 0.0 }))
                        .bottom(px(if last { 16.0 } else { 0.0 }))
                        .bg(move || active_theme().sidebar_border),
                )
                .child(
                    view()
                        .absolute()
                        .left(px(4.0))
                        .top(px(12.0))
                        .w(px(8.0))
                        .h(px(8.0))
                        .rounded(px(4.0))
                        .bg(move || {
                            if is_merge {
                                warning_color()
                            } else {
                                link_color()
                            }
                        }),
                );

            let mut row_el = view()
                .role(AccessibilityRole::Button)
                .flex_row()
                .w_full()
                .h(px(ROW_HEIGHT))
                .px_2()
                .gap_1()
                .items_center()
                .app_class("cursor-pointer")
                .hover(|s| s.bg(move || active_theme().accent.opacity(0.35)))
                .on_press(move || {
                    send_action(&tx, GitPanelAction::OpenCommit(clicked.clone()));
                })
                .child(lane)
                .child(
                    text(commit.short.clone())
                        .flex_shrink_0()
                        .font_family(move || {
                            active_theme()
                                .font_mono
                                .clone()
                                .unwrap_or_else(|| SharedString::from(crate::fonts::TERM_FONT))
                        })
                        .text_size(text_xs_size)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(link_color),
                );

            for badge in render_ref_badges(&commit.refs) {
                row_el = row_el.child(badge);
            }

            row_el = row_el.child(
                text(commit.subject.clone())
                    .flex_1()
                    .min_w_0()
                    .app_class("truncate")
                    .text_size(text_xs_size)
                    .text_color(move || active_theme().sidebar_foreground),
            );
            if let Some(mark_el) = render_mark(mark) {
                row_el = row_el.child(mark_el);
            }
            row_el.into_element()
        })
        .estimated_item_size(px(ROW_HEIGHT))
        .flex_1()
        .into_element()
    });

    view()
        .flex_col()
        .flex_1()
        .w_full()
        .h_full()
        .bg(move || active_theme().sidebar)
        .font_family(move || {
            active_theme()
                .font_sans
                .clone()
                .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
        })
        .child(toolbar)
        .child(body)
}
