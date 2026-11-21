//! Pure Rust NSH full-window modal dialog host for Kumu's main window.
//!
//! Mounted ONLY while a dialog is open (as a full-window overlay over
//! `KumuApp`), so closed dialogs have zero rendering or input overhead and
//! never interfere with live terminals.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui_kit::component::Theme;
use gpui_kit::{AppContext, Context, Entity, Task, Window};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::theme::{appearance, nsh_theme};

/// One entry in the "open existing worktree" picker dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeChoice {
    pub name: String,
    pub path: PathBuf,
    pub open_project_id: Option<i64>,
}

/// Specification of a modal dialog rendered in Pure Rust NSH.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogSpec {
    TextInput {
        title: String,
        description: Option<String>,
        placeholder: String,
        initial_value: String,
        ok_label: String,
        cancel_label: String,
        require_non_empty: bool,
    },
    Confirm {
        title: String,
        description: String,
        ok_label: String,
        cancel_label: String,
        destructive: bool,
    },
    Message {
        title: String,
        description: String,
        ok_label: String,
    },
    WorktreePicker {
        title: String,
        description: String,
        items: Vec<WorktreeChoice>,
    },
}

/// Outcome returned when the user submits, picks an item, or dismisses the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogOutcome {
    SubmittedText(String),
    Confirmed,
    PickedWorktree(WorktreeChoice),
    Dismissed,
}

struct DialogMount {
    spec: DialogSpec,
    outcome_tx: UnboundedSender<DialogOutcome>,
}

static NEXT_MOUNT_ID: AtomicU64 = AtomicU64::new(1);
static PENDING_MOUNTS: OnceLock<Mutex<HashMap<u64, DialogMount>>> = OnceLock::new();

fn pending_mounts() -> &'static Mutex<HashMap<u64, DialogMount>> {
    PENDING_MOUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_mount(mount: DialogMount) -> u64 {
    let id = NEXT_MOUNT_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut map) = pending_mounts().lock() {
        map.insert(id, mount);
    }
    id
}

fn take_mount(id: u64) -> Option<DialogMount> {
    pending_mounts().lock().ok()?.remove(&id)
}

mod view {
    use futures::channel::mpsc::UnboundedSender;
    use nsh_base::theme as active_theme;
    use nsh_core::element::{AccessibilityRole, scroll};
    use nsh_core::prelude::*;

    use super::{DialogOutcome, DialogSpec, take_mount};
    use crate::nsh_ui::components::{
        ButtonVariant, kumu_button, kumu_input, meta_text, row_title, section_title,
    };

    #[derive(Clone, Debug, PartialEq, Props)]
    pub(super) struct DialogRootProps {
        pub(super) mount_id: Prop<u64>,
    }

    fn send(tx: &Option<UnboundedSender<DialogOutcome>>, outcome: DialogOutcome) {
        if let Some(tx) = tx {
            let _ = tx.unbounded_send(outcome);
        }
    }

    #[component]
    pub(super) fn DialogRoot(props: DialogRootProps) -> impl IntoElement {
        let mount = take_mount(props.mount_id.get_untracked());
        let (spec, tx) = match mount {
            Some(m) => (m.spec, Some(m.outcome_tx)),
            None => (
                DialogSpec::Message {
                    title: String::new(),
                    description: String::new(),
                    ok_label: "关闭".into(),
                },
                None,
            ),
        };

        let tx_backdrop = tx.clone();
        let tx_escape = tx.clone();
        let card: AnyElement = match spec {
            DialogSpec::TextInput {
                title,
                description,
                placeholder,
                initial_value,
                ok_label,
                cancel_label,
                require_non_empty,
            } => {
                let draft = signal(initial_value);
                let tx_ok = tx.clone();
                let tx_submit = tx.clone();
                let tx_cancel = tx.clone();

                let submit_now = move |tx: &Option<UnboundedSender<DialogOutcome>>| {
                    let val = draft.get_untracked().trim().to_string();
                    if require_non_empty && val.is_empty() {
                        return;
                    }
                    send(tx, DialogOutcome::SubmittedText(val));
                };

                let mut col = view()
                    .flex_col()
                    .w(px(380.0))
                    .p_5()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(move || active_theme().popover)
                    .border_1()
                    .border_color(move || active_theme().border)
                    .child(section_title(title));
                if let Some(desc) = description {
                    col = col.child(meta_text(desc));
                }
                col = col
                    .child(
                        kumu_input(placeholder)
                            .auto_focus(true)
                            .w_full()
                            .value(move || draft.get())
                            .on_change(move |v: &str| draft.set(v.to_owned()))
                            .on_submit(move |_: &str| submit_now(&tx_submit)),
                    )
                    .child(
                        view()
                            .mt_1()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                kumu_button(cancel_label, ButtonVariant::Outline)
                                    .on_press(move || send(&tx_cancel, DialogOutcome::Dismissed)),
                            )
                            .child(
                                kumu_button(ok_label, ButtonVariant::Primary)
                                    .disabled(move || {
                                        require_non_empty && draft.get().trim().is_empty()
                                    })
                                    .on_press(move || submit_now(&tx_ok)),
                            ),
                    );
                col.into_element()
            }
            DialogSpec::Confirm {
                title,
                description,
                ok_label,
                cancel_label,
                destructive,
            } => {
                let tx_ok = tx.clone();
                let tx_cancel = tx.clone();
                let variant = if destructive {
                    ButtonVariant::Danger
                } else {
                    ButtonVariant::Primary
                };
                view()
                    .flex_col()
                    .w(px(380.0))
                    .p_5()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(move || active_theme().popover)
                    .border_1()
                    .border_color(move || active_theme().border)
                    .child(section_title(title))
                    .child(meta_text(description))
                    .child(
                        view()
                            .mt_1()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                kumu_button(cancel_label, ButtonVariant::Outline)
                                    .on_press(move || send(&tx_cancel, DialogOutcome::Dismissed)),
                            )
                            .child(
                                kumu_button(ok_label, variant)
                                    .on_press(move || send(&tx_ok, DialogOutcome::Confirmed)),
                            ),
                    )
                    .into_element()
            }
            DialogSpec::Message {
                title,
                description,
                ok_label,
            } => {
                let tx_ok = tx.clone();
                view()
                    .flex_col()
                    .w(px(380.0))
                    .p_5()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(move || active_theme().popover)
                    .border_1()
                    .border_color(move || active_theme().border)
                    .child(section_title(title))
                    .child(meta_text(description))
                    .child(
                        view().mt_1().flex_row().justify_end().child(
                            kumu_button(ok_label, ButtonVariant::Primary)
                                .on_press(move || send(&tx_ok, DialogOutcome::Dismissed)),
                        ),
                    )
                    .into_element()
            }
            DialogSpec::WorktreePicker {
                title,
                description,
                items,
            } => {
                let tx_close = tx.clone();
                let mut rows = view().flex_col().w_full().gap_1();
                for choice in items {
                    let shown = choice.path.to_string_lossy().into_owned();
                    let name = choice.name.clone();
                    let already_open = choice.open_project_id.is_some();
                    let tx_pick = tx.clone();
                    let picked = choice.clone();
                    let mut top = view()
                        .flex_row()
                        .gap_2()
                        .justify_between()
                        .items_center()
                        .child(row_title(name));
                    if already_open {
                        top = top.child(meta_text("已打开"));
                    }
                    rows = rows.child(
                        view()
                            .role(AccessibilityRole::Button)
                            .flex_col()
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_0p5()
                            .rounded(move || active_theme().radius)
                            .hover(|s| s.bg(move || active_theme().accent.opacity(0.5)))
                            .on_press(move || {
                                send(&tx_pick, DialogOutcome::PickedWorktree(picked.clone()));
                            })
                            .child(top)
                            .child(meta_text(shown)),
                    );
                }

                view()
                    .flex_col()
                    .w(px(420.0))
                    .p_5()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(move || active_theme().popover)
                    .border_1()
                    .border_color(move || active_theme().border)
                    .child(section_title(title))
                    .child(meta_text(description))
                    .child(scroll().w_full().max_h(px(320.0)).child(rows))
                    .child(
                        view().mt_1().flex_row().justify_end().child(
                            kumu_button("关闭", ButtonVariant::Outline)
                                .on_press(move || send(&tx_close, DialogOutcome::Dismissed)),
                        ),
                    )
                    .into_element()
            }
        };

        // Full-window dimmed backdrop: clicking outside the centered card or
        // pressing Escape dismisses.
        view()
            .relative()
            .flex_1()
            .w_full()
            .h_full()
            .items_center()
            .justify_center()
            .on_key_down(move |ev: &nsh_core::KeyEvent| {
                if ev.key == "Escape" {
                    send(&tx_escape, DialogOutcome::Dismissed);
                }
            })
            .font_family(move || {
                active_theme()
                    .font_sans
                    .clone()
                    .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
            })
            .child(
                view()
                    .absolute()
                    .inset(px(0.0))
                    .bg(move || active_theme().foreground.opacity(0.28))
                    .on_press(move || send(&tx_backdrop, DialogOutcome::Dismissed)),
            )
            .child(card)
    }
}

/// Active modal dialog overlay owned by `KumuApp`.
pub struct ActiveDialog {
    mount_id: u64,
    pub app_view: Entity<AppView>,
    _task: Task<()>,
}

impl Drop for ActiveDialog {
    fn drop(&mut self) {
        let _ = take_mount(self.mount_id);
    }
}

impl ActiveDialog {
    pub fn open<V: 'static>(
        spec: DialogSpec,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<V>,
        on_outcome: impl FnOnce(&mut V, DialogOutcome, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        let (outcome_tx, mut outcome_rx) = unbounded::<DialogOutcome>();
        let mount_id = register_mount(DialogMount { spec, outcome_tx });

        let mapped = nsh_theme(theme);
        let mode = appearance(theme.mode);
        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu Dialog".to_owned(),
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
            view.load(nsh_platform_gpui::App::Rust(Root::new::<view::DialogRoot>(
                move || view::DialogRootProps {
                    mount_id: mount_id.into(),
                },
            )));
            view
        });

        let task = cx.spawn_in(window, async move |this, cx| {
            let outcome = outcome_rx.next().await.unwrap_or(DialogOutcome::Dismissed);
            let _ = this.update_in(cx, |host, window, cx| {
                on_outcome(host, outcome, window, cx);
            });
        });

        Self {
            mount_id,
            app_view,
            _task: task,
        }
    }

    pub fn sync_theme(&self, theme: &Theme, cx: &mut gpui_kit::App) {
        let (mapped, mode) = (nsh_theme(theme), appearance(theme.mode));
        self.app_view.update(cx, |view, cx| {
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_engine::EngineConfig;

    #[test]
    fn confirm_dialog_emits_confirmed_or_dismissed() {
        let (tx, mut rx) = unbounded::<DialogOutcome>();
        let mount_id = register_mount(DialogMount {
            spec: DialogSpec::Confirm {
                title: "删除项目".into(),
                description: "此操作无法撤销。".into(),
                ok_label: "删除".into(),
                cancel_label: "取消".into(),
                destructive: true,
            },
            outcome_tx: tx,
        });
        let root =
            nsh_core::element::Root::new::<view::DialogRoot>(move || view::DialogRootProps {
                mount_id: mount_id.into(),
            });
        let mut app =
            nsh_testing::Headless::start(EngineConfig::new(root)).expect("start dialog NSH");
        app.wait_for("删除项目").expect("title renders");
        app.press("删除").expect("press confirm");
        assert_eq!(rx.try_recv().ok(), Some(DialogOutcome::Confirmed));
    }
}
