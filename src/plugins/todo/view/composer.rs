//! Quick-add bar and expanded multi-line task composer at the top of the board.

use futures::channel::mpsc::UnboundedSender;
use nsh_base::{input, theme as active_theme};
use nsh_core::element::AccessibilityRole;
use nsh_core::prelude::*;

use super::{render_notice_banner, send_action};
use crate::nsh_ui::assets::icon_svg;
use crate::nsh_ui::components::{ButtonVariant, kumu_button, meta_text};
use crate::nsh_ui::theme::text_xs_size;
use crate::plugins::todo::TodoAction;

#[derive(Clone, Copy)]
pub(super) struct ComposerSignals {
    pub notice: Signal<Option<(String, bool)>>,
    pub quick_draft: Signal<String>,
    pub composer_open: Signal<bool>,
    pub composer_title: Signal<String>,
    pub composer_desc: Signal<String>,
    pub import_open: Signal<bool>,
    pub import_draft: Signal<String>,
    pub importing: Signal<bool>,
}

pub(super) fn render_composer_section(
    tx: Option<UnboundedSender<TodoAction>>,
    signals: ComposerSignals,
    shared_board_name: Option<&str>,
    default_label: String,
) -> View {
    let submit_quick = {
        let tx = tx.clone();
        move |dispatch: bool| {
            let title = signals.quick_draft.get_untracked().trim().to_string();
            if title.is_empty() {
                return;
            }
            signals.quick_draft.set(String::new());
            signals.notice.set(None);
            send_action(
                &tx,
                TodoAction::CreateTodo {
                    title,
                    description: String::new(),
                    dispatch_default: dispatch,
                },
            );
        }
    };

    let submit_composer = {
        let tx = tx.clone();
        move |dispatch: bool| {
            let title = signals.composer_title.get_untracked().trim().to_string();
            if title.is_empty() {
                return;
            }
            let description = signals.composer_desc.get_untracked().trim().to_string();
            signals.composer_title.set(String::new());
            signals.composer_desc.set(String::new());
            signals.composer_open.set(false);
            signals.notice.set(None);
            send_action(
                &tx,
                TodoAction::CreateTodo {
                    title,
                    description,
                    dispatch_default: dispatch,
                },
            );
        }
    };

    let submit_import = {
        let tx = tx.clone();
        move |dispatch: bool| {
            if signals.importing.get_untracked() {
                return;
            }
            let input_val = signals.import_draft.get_untracked().trim().to_string();
            if input_val.is_empty() {
                return;
            }
            signals.importing.set(true);
            signals.notice.set(None);
            send_action(
                &tx,
                TodoAction::ImportTask {
                    input: input_val,
                    dispatch_default: dispatch,
                },
            );
        }
    };

    let mut quick_wrap = view()
        .flex_col()
        .flex_shrink_0()
        .px_2p5()
        .py_2()
        .gap_2()
        .border_b_1()
        .border_b_color(move || active_theme().sidebar_border);

    if let Some(shared_from) = shared_board_name
        && !shared_from.is_empty()
    {
        quick_wrap = quick_wrap.child(meta_text(format!("共享自 {shared_from}")));
    }

    if signals.composer_open.get() {
        let submit_add = submit_composer.clone();
        let submit_run = submit_composer.clone();
        let submit_enter = submit_composer;
        quick_wrap = quick_wrap.child(
            view()
                .flex_col()
                .p_2()
                .gap_1p5()
                .rounded(move || active_theme().radius)
                .bg(move || active_theme().background)
                .border_1()
                .border_color(move || active_theme().border)
                .child(
                    input()
                        .w_full()
                        .px_2()
                        .py_1()
                        .rounded(move || active_theme().radius)
                        .border_1()
                        .border_color(move || active_theme().input)
                        .bg(move || active_theme().background)
                        .placeholder("任务标题")
                        .placeholder_color(move || active_theme().muted_foreground.light)
                        .text_size(text_xs_size)
                        .text_color(move || active_theme().foreground)
                        .value(move || signals.composer_title.get())
                        .on_change(move |v: &str| signals.composer_title.set(v.to_owned()))
                        .on_submit(move |_: &str| submit_enter(false)),
                )
                .child(
                    input()
                        .w_full()
                        .min_h(px(56.0))
                        .max_h(px(120.0))
                        .multiline(true)
                        .px_2()
                        .py_1()
                        .rounded(move || active_theme().radius)
                        .bg(move || active_theme().sidebar.opacity(0.65))
                        .placeholder("详细说明 / Agent Prompt（可选）…")
                        .placeholder_color(move || active_theme().muted_foreground.light)
                        .text_size(text_xs_size)
                        .text_color(move || active_theme().foreground)
                        .value(move || signals.composer_desc.get())
                        .on_change(move |v: &str| signals.composer_desc.set(v.to_owned())),
                )
                .child(
                    view()
                        .flex_row()
                        .gap_1p5()
                        .items_center()
                        .child(
                            kumu_button("仅添加", ButtonVariant::Ghost)
                                .on_press(move || submit_add(false)),
                        )
                        .child(
                            kumu_button(
                                format!("添加并交给 {default_label}"),
                                ButtonVariant::Primary,
                            )
                            .on_press(move || submit_run(true)),
                        )
                        .child(view().flex_1())
                        .child(
                            view()
                                .role(AccessibilityRole::Button)
                                .label("收起")
                                .p_1()
                                .rounded(move || active_theme().radius)
                                .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                                .on_press(move || signals.composer_open.set(false))
                                .child(
                                    icon_svg("icons/x.svg", 12.0)
                                        .text_color(move || active_theme().muted_foreground),
                                ),
                        ),
                ),
        );
    } else if signals.import_open.get() {
        let is_importing = signals.importing.get();
        let submit_only = submit_import.clone();
        let submit_dispatch = submit_import.clone();
        let submit_enter = submit_import;
        let import_label = if is_importing {
            "导入中…"
        } else {
            "导入"
        };
        let import_dispatch_label = if is_importing {
            "导入中…".to_string()
        } else {
            format!("导入并交给 {default_label}")
        };
        quick_wrap = quick_wrap.child(
            view()
                .flex_col()
                .p_2()
                .gap_1p5()
                .rounded(move || active_theme().radius)
                .bg(move || active_theme().background)
                .border_1()
                .border_color(move || active_theme().border)
                .child(
                    input()
                        .w_full()
                        .px_2()
                        .py_1()
                        .rounded(move || active_theme().radius)
                        .border_1()
                        .border_color(move || active_theme().input)
                        .bg(move || active_theme().background)
                        .placeholder("粘贴 GitHub / GitLab / Jira 链接，或 #40 / PROJ-123")
                        .placeholder_color(move || active_theme().muted_foreground.light)
                        .text_size(text_xs_size)
                        .text_color(move || active_theme().foreground)
                        .value(move || signals.import_draft.get())
                        .on_change(move |v: &str| signals.import_draft.set(v.to_owned()))
                        .on_submit(move |_: &str| submit_enter(false)),
                )
                .child(
                    view()
                        .flex_row()
                        .gap_1p5()
                        .items_center()
                        .child(
                            kumu_button(import_label, ButtonVariant::Ghost)
                                .disabled(is_importing)
                                .on_press(move || submit_only(false)),
                        )
                        .child(
                            kumu_button(import_dispatch_label, ButtonVariant::Primary)
                                .disabled(is_importing)
                                .on_press(move || submit_dispatch(true)),
                        )
                        .child(view().flex_1())
                        .child(
                            view()
                                .role(AccessibilityRole::Button)
                                .label("关闭导入")
                                .p_1()
                                .rounded(move || active_theme().radius)
                                .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                                .on_press(move || signals.import_open.set(false))
                                .child(
                                    icon_svg("icons/x.svg", 12.0)
                                        .text_color(move || active_theme().muted_foreground),
                                ),
                        ),
                ),
        );
    } else {
        let tx_quick_import = tx;
        quick_wrap = quick_wrap
            .child(
                view()
                    .flex_row()
                    .gap_1p5()
                    .items_center()
                    .child(
                        input()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded(move || active_theme().radius)
                            .border_1()
                            .border_color(move || active_theme().input)
                            .bg(move || active_theme().background)
                            .placeholder("添加待办…")
                            .placeholder_color(move || active_theme().muted_foreground.light)
                            .text_size(text_xs_size)
                            .text_color(move || active_theme().foreground)
                            .value(move || signals.quick_draft.get())
                            .on_change(move |v: &str| signals.quick_draft.set(v.to_owned()))
                            .on_submit(move |_: &str| submit_quick(false)),
                    )
                    .child(
                        view()
                            .role(AccessibilityRole::Button)
                            .label("展开多行说明")
                            .p_1p5()
                            .rounded(move || active_theme().radius)
                            .bg(move || active_theme().background)
                            .border_1()
                            .border_color(move || active_theme().border)
                            .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                            .on_press(move || {
                                let draft = signals.quick_draft.get_untracked();
                                if !draft.trim().is_empty() {
                                    signals.composer_title.set(draft);
                                    signals.quick_draft.set(String::new());
                                }
                                signals.import_open.set(false);
                                signals.composer_open.set(true);
                            })
                            .child(
                                icon_svg("icons/plus.svg", 13.0)
                                    .text_color(move || active_theme().foreground),
                            ),
                    )
                    .child(
                        view()
                            .role(AccessibilityRole::Button)
                            .label("从外部导入")
                            .p_1p5()
                            .rounded(move || active_theme().radius)
                            .bg(move || active_theme().background)
                            .border_1()
                            .border_color(move || active_theme().border)
                            .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                            .on_press(move || {
                                let draft = signals.quick_draft.get_untracked();
                                if crate::tracker::looks_like_tracker_url(&draft) {
                                    signals.import_draft.set(draft.trim().to_string());
                                    signals.quick_draft.set(String::new());
                                }
                                signals.composer_open.set(false);
                                signals.import_open.set(true);
                            })
                            .child(
                                icon_svg("icons/git-branch.svg", 13.0)
                                    .text_color(move || active_theme().foreground),
                            ),
                    ),
            )
            .child(dynamic(move || {
                let draft = signals.quick_draft.get();
                if !crate::tracker::looks_like_tracker_url(&draft) {
                    return view().into_element();
                }
                let is_importing = signals.importing.get();
                let tx_btn = tx_quick_import.clone();
                view()
                    .flex_row()
                    .px_2()
                    .py_1()
                    .gap_1p5()
                    .rounded(move || active_theme().radius)
                    .bg(move || active_theme().sidebar_accent.opacity(0.55))
                    .items_center()
                    .child(meta_text("识别为外部任务链接"))
                    .child(view().flex_1())
                    .child(
                        kumu_button(
                            if is_importing {
                                "导入中…"
                            } else {
                                "直接导入"
                            },
                            ButtonVariant::Ghost,
                        )
                        .disabled(is_importing)
                        .on_press(move || {
                            if signals.importing.get_untracked() {
                                return;
                            }
                            let url = signals.quick_draft.get_untracked().trim().to_string();
                            if url.is_empty() {
                                return;
                            }
                            signals.quick_draft.set(String::new());
                            signals.importing.set(true);
                            signals.notice.set(None);
                            send_action(
                                &tx_btn,
                                TodoAction::ImportTask {
                                    input: url,
                                    dispatch_default: false,
                                },
                            );
                        }),
                    )
                    .into_element()
            }));
    }

    quick_wrap.child(render_notice_banner(signals.notice))
}
