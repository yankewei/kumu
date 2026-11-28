//! Pure Rust NSH list for the Artifacts drawer.

use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use nsh_base::{input, theme as active_theme};
use nsh_core::element::{AccessibilityRole, SignalEquals, scroll, signal_with};
use nsh_core::prelude::*;

use super::{
    ArtifactFilter, ArtifactGroup, ArtifactRow, ArtifactsAction, ArtifactsSnapshot, take_mount,
};
use crate::artifacts::ArtifactKind;
use crate::nsh_ui::assets::icon_svg;
use crate::nsh_ui::components::{
    ButtonVariant, ControlTone, kumu_button, kumu_icon_button, meta_text, section_title,
};
use crate::nsh_ui::theme::{text_sm_size, text_xs_size};

#[derive(Clone, Debug, PartialEq, Props)]
pub(super) struct ArtifactsRootProps {
    pub(super) mount_id: Prop<u64>,
}

fn send(tx: &Option<UnboundedSender<ArtifactsAction>>, action: ArtifactsAction) {
    if let Some(tx) = tx {
        let _ = tx.unbounded_send(action);
    }
}

fn kind_label(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Markdown => "MD",
        ArtifactKind::Html => "HTML",
        ArtifactKind::Text => "TXT",
    }
}

fn kind_color(kind: ArtifactKind) -> AdaptiveColor {
    match kind {
        ArtifactKind::Markdown => active_theme().primary,
        ArtifactKind::Html => active_theme().primary,
        ArtifactKind::Text => active_theme().muted_foreground,
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

fn filter_button(
    label: &'static str,
    active: bool,
    tx: Option<UnboundedSender<ArtifactsAction>>,
    filter: ArtifactFilter,
) -> impl IntoElement {
    let mut element = view()
        .role(AccessibilityRole::Button)
        .label(label)
        .flex_row()
        .items_center()
        .justify_center()
        .h(px(24.0))
        .px_2()
        .rounded(move || active_theme().radius)
        .app_class("cursor-pointer")
        .on_press(move || send(&tx, ArtifactsAction::SetFilter(filter)))
        .child(
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
        );
    if active {
        element = element.bg(move || active_theme().background);
    } else {
        element = element.hover(|s| s.bg(move || active_theme().accent.opacity(0.4)));
    }
    element
}

fn row_element(
    row: ArtifactRow,
    tx: Option<UnboundedSender<ArtifactsAction>>,
    remove: Signal<Option<String>>,
) -> impl IntoElement {
    let tx_open = tx.clone();
    let tx_reveal = tx.clone();
    let tx_copy = tx.clone();
    let tx_external = tx.clone();
    let id = row.id.clone();
    let title = row.title.clone();
    let entry = row.entry.clone();
    let dir = row.dir.clone();
    let html = row.kind == ArtifactKind::Html;
    let mut trailing = Vec::new();
    if html {
        let tx = tx_external;
        let entry = entry.clone();
        trailing.push(
            icon_action("在浏览器打开", "icons/external-link.svg", move || {
                send(&tx, ArtifactsAction::OpenExternal(entry.clone()))
            })
            .into_element(),
        );
    }
    {
        let tx = tx_reveal;
        let dir = dir.clone();
        trailing.push(
            icon_action("在 Finder 中显示", "icons/folder-open.svg", move || {
                send(&tx, ArtifactsAction::Reveal(dir.clone()))
            })
            .into_element(),
        );
    }
    {
        let tx = tx_copy;
        let entry = entry.clone();
        trailing.push(
            icon_action("复制路径", "icons/copy.svg", move || {
                send(&tx, ArtifactsAction::CopyPath(entry.clone()))
            })
            .into_element(),
        );
    }
    {
        let id = id.clone();
        trailing.push(
            icon_action("移入废纸篓", "icons/trash.svg", move || {
                remove.set(Some(id.clone()))
            })
            .into_element(),
        );
    }

    let tx_row = tx_open;
    let id_row = row.id.clone();
    view()
        .flex_row()
        .w_full()
        .min_h(px(44.0))
        .px_2()
        .py_1p5()
        .gap_1()
        .items_center()
        .hover(|s| s.bg(move || active_theme().accent.opacity(0.35)))
        .child(
            view()
                .role(AccessibilityRole::Button)
                .label(title.clone())
                .flex_1()
                .min_w_0()
                .flex_col()
                .gap_0p5()
                .app_class("cursor-pointer")
                .on_press(move || {
                    send(&tx_row, ArtifactsAction::Open(id_row.clone()));
                })
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .child(
                            view()
                                .flex_shrink_0()
                                .px_1()
                                .py_0p5()
                                .rounded(move || active_theme().radius)
                                .bg(move || kind_color(row.kind).opacity(0.14))
                                .child(
                                    text(kind_label(row.kind))
                                        .text_size(text_xs_size)
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(move || kind_color(row.kind)),
                                ),
                        )
                        .child(
                            text(title)
                                .flex_1()
                                .min_w_0()
                                .app_class("truncate")
                                .text_size(text_xs_size)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(move || active_theme().foreground),
                        )
                        .child(
                            text(row.updated)
                                .flex_shrink_0()
                                .text_size(text_xs_size)
                                .text_color(move || active_theme().muted_foreground),
                        ),
                )
                .child(meta_text(row.size).app_class("truncate")),
        )
        .children(trailing)
}

fn group_element(
    group: ArtifactGroup,
    tx: Option<UnboundedSender<ArtifactsAction>>,
    remove: Signal<Option<String>>,
) -> impl IntoElement {
    let count = group.rows.len();
    let mut header = view()
        .flex_row()
        .items_center()
        .gap_1p5()
        .px_2()
        .pt_2()
        .pb_1()
        .child(
            text(group.title)
                .flex_1()
                .min_w_0()
                .app_class("truncate")
                .text_size(text_xs_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || active_theme().foreground),
        );
    if group.current {
        header = header.child(
            view()
                .flex_shrink_0()
                .px_1()
                .py_0p5()
                .rounded(move || active_theme().radius)
                .bg(move || active_theme().primary.opacity(0.16))
                .child(
                    text("当前")
                        .text_size(text_xs_size)
                        .text_color(move || active_theme().primary),
                ),
        );
    }
    header = header.child(
        text(format!("{count}"))
            .flex_shrink_0()
            .text_size(text_xs_size)
            .text_color(move || active_theme().muted_foreground),
    );

    let mut section = view().flex_col().w_full().pb_1().child(header).child(
        text(group.meta)
            .px_2()
            .pb_1()
            .app_class("truncate")
            .text_size(text_xs_size)
            .text_color(move || active_theme().muted_foreground),
    );
    for row in group.rows {
        section = section.child(row_element(row, tx.clone(), remove));
    }
    section
}

fn empty_state(loaded: bool, tx: Option<UnboundedSender<ArtifactsAction>>) -> AnyElement {
    if !loaded {
        return view()
            .flex_1()
            .w_full()
            .px_3()
            .py_4()
            .child(meta_text("正在读取…"))
            .into_element();
    }
    const EXAMPLE_CMD: &str = "\"$KUMU_APP_BINARY\" artifact publish ./plan.md --title \"方案\"";
    view()
        .flex_1()
        .w_full()
        .px_3()
        .py_4()
        .flex_col()
        .gap_2()
        .child(
            text("还没有 artifacts")
                .text_size(text_sm_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || active_theme().foreground),
        )
        .child(meta_text("让 Agent 发布一份方案、设计稿或报告："))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_1p5()
                .w_full()
                .px_2()
                .py_1p5()
                .rounded(move || active_theme().radius)
                .bg(move || active_theme().background)
                .border_1()
                .border_color(move || active_theme().border)
                .child(
                    text(EXAMPLE_CMD)
                        .flex_1()
                        .min_w_0()
                        .text_size(text_xs_size)
                        .font_family(move || {
                            active_theme()
                                .font_mono
                                .clone()
                                .unwrap_or_else(|| SharedString::from(crate::fonts::TERM_FONT))
                        })
                        .text_color(move || active_theme().foreground),
                )
                .child(icon_action("复制命令", "icons/copy.svg", move || {
                    send(&tx, ArtifactsAction::CopyText(EXAMPLE_CMD.to_string()));
                })),
        )
        .into_element()
}

#[component]
pub(super) fn ArtifactsRoot(props: ArtifactsRootProps) -> impl IntoElement {
    let mount = take_mount(props.mount_id.get_untracked());
    let (initial, events_rx, actions_tx) = match mount {
        Some(mount) => (mount.initial, Some(mount.events_rx), Some(mount.actions_tx)),
        None => (ArtifactsSnapshot::default(), None, None),
    };
    let state = signal_with(initial, SignalEquals::Never);
    let query_draft = signal(String::new());
    let pending_remove = signal(Option::<String>::None);

    if let Some(mut rx) = events_rx {
        spawn(async move {
            while let Some(next) = rx.next().await {
                state.set(next);
            }
        });
    }

    let toolbar = {
        let tx_reload = actions_tx.clone();
        let tx_all = actions_tx.clone();
        let tx_current = actions_tx.clone();
        let tx_query = actions_tx.clone();
        let tx_clear = actions_tx.clone();
        dynamic(move || {
            let tx_reload = tx_reload.clone();
            let tx_all = tx_all.clone();
            let tx_current = tx_current.clone();
            let tx_query = tx_query.clone();
            let tx_clear = tx_clear.clone();
            let snapshot = state.get();
            let mut top = view()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .pt_2()
                .pb_1()
                .child(
                    section_title("Artifacts")
                        .flex_1()
                        .min_w_0()
                        .app_class("truncate"),
                )
                .child(meta_text(format!("{}", snapshot.total)).flex_shrink_0());
            top = top.child(icon_action(
                "刷新",
                "icons/loader-circle.svg",
                move || {
                    send(&tx_reload, ArtifactsAction::Reload);
                },
            ));
            let filter = view()
                .flex_row()
                .mx_2()
                .mb_1p5()
                .p_0p5()
                .gap_0p5()
                .rounded(move || active_theme().radius)
                .bg(move || active_theme().muted)
                .child(filter_button(
                    "全部",
                    snapshot.filter == ArtifactFilter::All,
                    tx_all.clone(),
                    ArtifactFilter::All,
                ))
                .child(filter_button(
                    "当前会话",
                    snapshot.filter == ArtifactFilter::CurrentSession,
                    tx_current.clone(),
                    ArtifactFilter::CurrentSession,
                ));
            let search = view().px_2().pb_1p5().child(
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
                            .placeholder("搜索 artifacts")
                            .placeholder_color(move || active_theme().muted_foreground.light)
                            .text_size(text_xs_size)
                            .text_color(move || active_theme().foreground)
                            .value(move || query_draft.get())
                            .on_change(move |value: &str| {
                                query_draft.set(value.to_owned());
                                send(&tx_query, ArtifactsAction::SetQuery(value.to_owned()));
                            }),
                    )
                    .child(when(
                        move || !query_draft.get().is_empty(),
                        move || {
                            let tx = tx_clear.clone();
                            view()
                                .role(AccessibilityRole::Button)
                                .label("清空搜索")
                                .app_class("cursor-pointer")
                                .on_press(move || {
                                    query_draft.set(String::new());
                                    send(&tx, ArtifactsAction::SetQuery(String::new()));
                                })
                                .child(
                                    icon_svg("icons/x.svg", 11.0)
                                        .text_color(move || active_theme().muted_foreground),
                                )
                        },
                    )),
            );
            let mut bar = view()
                .flex_col()
                .flex_shrink_0()
                .border_b_1()
                .border_b_color(move || active_theme().sidebar_border)
                .child(top)
                .child(filter)
                .child(search);
            if let Some(error) = snapshot.error {
                bar = bar.child(
                    view()
                        .mx_2()
                        .mb_1p5()
                        .px_2()
                        .py_1()
                        .rounded(move || active_theme().radius)
                        .bg(move || active_theme().destructive.opacity(0.12))
                        .child(
                            text(error)
                                .text_size(text_xs_size)
                                .text_color(move || active_theme().destructive),
                        ),
                );
            }
            bar
        })
    };

    let body = {
        let tx_body = actions_tx.clone();
        dynamic(move || {
            let snapshot = state.get();
            if snapshot.project_id.is_none() {
                return view()
                    .flex_1()
                    .w_full()
                    .px_3()
                    .py_4()
                    .child(meta_text("没有选中的项目"))
                    .into_element();
            }
            if snapshot.groups.is_empty() {
                return empty_state(snapshot.loaded, tx_body.clone());
            }
            let mut list = view().flex_col().w_full();
            for group in snapshot.groups {
                list = list.child(group_element(group, tx_body.clone(), pending_remove));
            }
            scroll()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(list)
                .into_element()
        })
    };

    let confirm = dynamic(move || {
        let Some(id) = pending_remove.get() else {
            return view().into_element();
        };
        let snapshot = state.get();
        let title = snapshot
            .groups
            .iter()
            .flat_map(|group| group.rows.iter())
            .find(|row| row.id == id)
            .map(|row| row.title.clone())
            .unwrap_or_else(|| id.clone());
        let tx_remove = actions_tx.clone();
        let id_remove = id.clone();
        view()
            .flex_row()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .px_2()
            .py_2()
            .border_t_1()
            .border_t_color(move || active_theme().border)
            .bg(move || active_theme().background)
            .child(
                text(format!("删除「{title}」？"))
                    .flex_1()
                    .min_w_0()
                    .app_class("truncate")
                    .text_size(text_xs_size)
                    .text_color(move || active_theme().foreground),
            )
            .child(kumu_button("取消", ButtonVariant::Ghost).on_press(move || {
                pending_remove.set(None);
            }))
            .child(
                kumu_button("移入废纸篓", ButtonVariant::Danger).on_press(move || {
                    pending_remove.set(None);
                    send(&tx_remove, ArtifactsAction::Remove(id_remove.clone()));
                }),
            )
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
        .child(confirm)
}
