//! macOS real-window verification harness (`KUMU_NSH_VERIFY=1 cargo run`):
//! opens an NSH Pure-Rust 3-pane window hosting a live Kumu `TerminalView`
//! (via `NativeComponent`), NSH native input (for CJK IME testing), theme sync,
//! and `nsh_base::DiffView` with an inline review annotation.

use std::sync::OnceLock;

use futures::StreamExt;
use gpui_kit::component::{ActiveTheme, Theme, ThemeMode};
use gpui_kit::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription,
    Task, Window, WindowBounds, WindowOptions, div, px, size,
};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::bridge::{HostAction, NshBridge};
use super::terminal_host::{TERMINAL_COMPONENT_NAME, TerminalNativeComponent, TerminalRegistry};
use super::theme::{appearance, nsh_theme};
use crate::agent::launch::PaneLaunch;
use crate::app::Workspace;
use crate::app::layout::PaneId;
use crate::ui::home::TerminalView;

static VERIFY_BRIDGE: OnceLock<NshBridge> = OnceLock::new();

mod root_view {
    use nsh_base::{
        DiffAnnotation, DiffLayout, DiffSide, diff_view, h_resizable, resizable_panel, theme,
    };
    use nsh_core::prelude::*;

    use super::VERIFY_BRIDGE;
    use crate::nsh_ui::bridge::HostAction;
    use crate::nsh_ui::components::{
        ButtonVariant, PillTone, body_text, kumu_button, kumu_input, meta_text, pill, section_card,
        section_title,
    };
    use crate::nsh_ui::terminal_host::TERMINAL_COMPONENT_NAME;

    #[component]
    pub fn VerifyRoot() -> impl IntoElement {
        let ime_draft = signal(String::new());
        let projects = VERIFY_BRIDGE
            .get()
            .map(crate::nsh_ui::bridge::NshBridge::projects)
            .unwrap_or_default();
        let project_summary = if projects.is_empty() {
            "暂无项目".to_string()
        } else {
            format!("{} 个项目（首个：{}）", projects.len(), projects[0].name)
        };

        let mut ann = DiffAnnotation::default();
        ann.id = "kumu-review-1".into();
        ann.side = DiffSide::New;
        ann.line = 1;
        let annotations = signal(vec![ann]);

        h_resizable()
            .flex_1()
            .bg(move || theme().background)
            .child(
                resizable_panel().size(px(280.0)).child(
                    view()
                        .flex_1()
                        .gap_3()
                        .p_3()
                        .bg(move || theme().sidebar)
                        .border_r_1()
                        .border_r_color(move || theme().sidebar_border)
                        .child(
                            view()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .child(section_title("Kumu × NSH 真窗验证"))
                                .child(pill("Phase 2", PillTone::Accent)),
                        )
                        .child(meta_text(project_summary))
                        .child(
                            section_card()
                                .child(meta_text("1. 中文 IME 输入验证"))
                                .child(
                                    kumu_input("在此输入中文测试候选框与上屏…")
                                        .value(move || ime_draft.get())
                                        .on_change(move |v: &str| ime_draft.set(v.to_owned())),
                                )
                                .child(body_text(move || {
                                    let val = ime_draft.get();
                                    if val.is_empty() {
                                        "等待输入".to_string()
                                    } else {
                                        format!("已输入：{val}")
                                    }
                                })),
                        )
                        .child(
                            section_card()
                                .child(meta_text("2. 主题联动与终端焦点"))
                                .child(
                                    kumu_button("切换深浅色主题", ButtonVariant::Primary).on_press(
                                        || {
                                            if let Some(b) = VERIFY_BRIDGE.get() {
                                                b.send(HostAction::ToggleThemeMode);
                                            }
                                        },
                                    ),
                                )
                                .child(
                                    kumu_button(
                                        "聚焦右侧终端 (测试 Tab 补全)",
                                        ButtonVariant::Default,
                                    )
                                    .on_press(|| {
                                        if let Some(b) = VERIFY_BRIDGE.get() {
                                            b.send(HostAction::FocusPane(1));
                                        }
                                    }),
                                ),
                        ),
                ),
            )
            .child(
                resizable_panel().child(
                    nsh_core::element::native(TERMINAL_COMPONENT_NAME)
                        .prop("pane", serde_json::json!(1u64))
                        .flex_1(),
                ),
            )
            .child(
                resizable_panel().size(px(340.0)).child(
                    view()
                        .flex_1()
                        .border_l_1()
                        .border_l_color(move || theme().border)
                        .child(
                            view()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_b_color(move || theme().border)
                                .child(section_title("3. NSH DiffView 验证")),
                        )
                        .child(
                            diff_view()
                                .flex_1()
                                .layout(DiffLayout::Unified)
                                .old_value("fn render() {\n    old_ui();\n}\n")
                                .new_value("fn render() {\n    nsh_ui();\n}\n")
                                .annotations(move || annotations.get()),
                        ),
                ),
            )
    }
}

struct VerifyHostWindow {
    _workspace: Entity<Workspace>,
    terminal: Entity<TerminalView>,
    app_view: Entity<AppView>,
    _theme_sub: Subscription,
    _actions: Task<()>,
}

impl VerifyHostWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = cx.new(Workspace::new);
        let project_id = workspace
            .update(cx, |w, cx| w.ensure_project(cx))
            .unwrap_or(1);

        let pane_id = PaneId(1);
        let terminal = cx.new(|cx| TerminalView::new(pane_id, workspace.clone(), cx));
        terminal.update(cx, |term, cx| term.show(project_id, &PaneLaunch::Shell, cx));

        let registry = TerminalRegistry::new();
        registry.insert(pane_id.0, terminal.clone());

        let (bridge, mut action_rx) = NshBridge::new(workspace.read(cx).shared_core());
        let _ = VERIFY_BRIDGE.set(bridge);

        let initial_theme = nsh_theme(cx.theme());
        let initial_appearance = appearance(cx.theme().mode);

        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu NSH Verification".to_owned(),
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.register_component(
                TERMINAL_COMPONENT_NAME,
                TerminalNativeComponent::factory(registry),
            );
            view.set_theme(Some(initial_theme), cx);
            view.set_appearance(initial_appearance, cx);
            view.load(nsh_platform_gpui::App::Rust(Root::of::<
                root_view::VerifyRoot,
            >()));
            view
        });

        let theme_sub = cx.observe_global::<Theme>(|this: &mut Self, cx| {
            let theme = cx.theme();
            let (mapped, mode) = (nsh_theme(theme), appearance(theme.mode));
            let term_colors = crate::theme::term_colors(cx);
            this._workspace.read(cx).set_terminal_colors(term_colors);
            this.app_view.update(cx, |view, cx| {
                view.set_theme(Some(mapped), cx);
                view.set_appearance(mode, cx);
            });
            cx.notify();
        });

        let actions = cx.spawn_in(window, async move |this, cx| {
            while let Some(action) = action_rx.next().await {
                let res = this.update_in(cx, |this, window, cx| match action {
                    HostAction::ToggleThemeMode => {
                        let next = if cx.theme().mode.is_dark() {
                            ThemeMode::Light
                        } else {
                            ThemeMode::Dark
                        };
                        crate::theme::change_mode(next, Some(window), cx);
                    }
                    HostAction::FocusPane(_) => {
                        this.terminal.update(cx, |term, cx| term.focus(window, cx));
                    }
                });
                if res.is_err() {
                    break;
                }
            }
        });

        Self {
            _workspace: workspace,
            terminal,
            app_view,
            _theme_sub: theme_sub,
            _actions: actions,
        }
    }
}

impl Render for VerifyHostWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.app_view.clone())
    }
}

/// Open the Phase 2 macOS real-window verification harness (`KUMU_NSH_VERIFY=1`).
pub fn open_verify_window(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(None, size(px(1280.), px(800.)), cx)),
        inactive_frame_interval: None,
        ..Default::default()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| VerifyHostWindow::new(window, cx))
    }) {
        Ok(_) => eprintln!("[nsh] verification window opened"),
        Err(e) => eprintln!("[nsh] verification window open FAILED: {e:?}"),
    }
}
