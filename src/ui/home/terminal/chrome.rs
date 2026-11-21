use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{TerminalEvent, TerminalView};
use crate::agent::{AGENTS, AgentState, installed};
use crate::app::layout::PaneId;
use crate::term::Snapshot;
use crate::ui::tooltip::HoverLabel as _;

impl TerminalView {
    /// The floating find bar over this pane's grid: input, match count, older
    /// / newer buttons and close.
    pub(super) fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bar = self.find.as_ref()?;
        let theme = cx.theme();
        let (muted, radius, hover) = (
            theme.muted_foreground,
            theme.radius,
            theme.accent.opacity(0.5),
        );
        let (border, background) = (theme.border, theme.secondary);
        let search = self.snapshot.search.as_ref();
        // The session thread answers a query a frame or more later; until its
        // result for this needle arrives, say nothing rather than "no
        // results" or the previous query's count.
        let current = search.is_some_and(|search| search.needle == bar.sent);
        let total = search.filter(|_| current).map_or(0, |search| search.total);
        let count = if bar.sent.is_empty() {
            String::new()
        } else if !current {
            "…".to_string()
        } else if total == 0 {
            "无结果".to_string()
        } else {
            match search.and_then(|search| search.selected) {
                Some(index) => format!("{}/{}", index + 1, total),
                None => format!("0/{total}"),
            }
        };
        let dim = if !current || total == 0 { 0.4 } else { 1. };
        let older = find_step_button(
            format!("find-older-{}", self.pane.0),
            IconName::ChevronUp,
            "查找更早的内容（Enter）",
            true,
            dim,
            muted,
            radius,
            hover,
            cx,
        );
        let newer = find_step_button(
            format!("find-newer-{}", self.pane.0),
            IconName::ChevronDown,
            "查找更新的内容（⇧Enter）",
            false,
            dim,
            muted,
            radius,
            hover,
            cx,
        );

        Some(
            h_flex()
                .id(format!("pane-find-{}", self.pane.0))
                .absolute()
                .top_2()
                .right_3()
                .w(px(340.))
                // Narrow split panes must keep the input reachable instead of
                // clipping the bar's left edge.
                .max_w(relative(0.9))
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(radius)
                .border_1()
                .border_color(border)
                .bg(background)
                .shadow_sm()
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        this.close_find(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .child(
                    Input::new(&bar.input)
                        .small()
                        .appearance(false)
                        .flex_1()
                        .min_w_0(),
                )
                .when(!count.is_empty(), |row| {
                    row.child(div().flex_none().text_xs().text_color(muted).child(count))
                })
                .child(older)
                .child(newer)
                .child(
                    div()
                        .id(format!("find-close-{}", self.pane.0))
                        .rounded(radius)
                        .p_0p5()
                        .cursor_pointer()
                        .text_color(muted)
                        .hover(move |style| style.bg(hover))
                        .hover_label("关闭查找")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.close_find(window, cx);
                        }))
                        .child(
                            Icon::new(IconName::Close)
                                .with_size(px(14.))
                                .text_color(muted),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// The full status pill the pane header keeps: one glyph and a short label,
/// tinted with the state's semantic color.
fn agent_status_badge(
    state: AgentState,
    link: Hsla,
    warning: Hsla,
    success: Hsla,
    danger: Hsla,
    radius: Pixels,
) -> AnyElement {
    let color = match state {
        AgentState::Running => link,
        AgentState::Waiting => warning,
        AgentState::Completed => success,
        AgentState::Failed => danger,
    };
    let glyph = match state {
        AgentState::Running => gpui_kit::component::spinner::Spinner::new()
            .with_size(px(12.))
            .color(color)
            .into_any_element(),
        AgentState::Waiting => div()
            .size(px(7.))
            .rounded_full()
            .bg(color)
            .into_any_element(),
        AgentState::Completed => Icon::new(IconName::Check)
            .with_size(px(12.))
            .text_color(color)
            .into_any_element(),
        AgentState::Failed => Icon::new(IconName::TriangleAlert)
            .with_size(px(12.))
            .text_color(color)
            .into_any_element(),
    };
    // The design's tinted badge backgrounds are 14–16% of the state color.
    let tint = if state == AgentState::Running {
        0.14
    } else {
        0.16
    };
    h_flex()
        .flex_none()
        .h(px(20.))
        .gap_1()
        .px_1p5()
        .items_center()
        .rounded(radius)
        .bg(color.opacity(tint))
        .text_xs()
        .font_medium()
        .text_color(color)
        .child(glyph)
        .child(state.label())
        .into_any_element()
}

/// Top bar like the reference pane chrome: mark, name, shortened cwd, split /
/// close actions.
#[allow(clippy::too_many_arguments)]
pub(super) fn terminal_header(
    pane: PaneId,
    mark: AnyElement,
    name: String,
    cwd: String,
    note: Option<String>,
    status: Option<AgentState>,
    header_bg: Hsla,
    can_close: bool,
    muted: Hsla,
    fg: Hsla,
    warning: Hsla,
    radius: Pixels,
    hover: Hsla,
    cx: &mut Context<TerminalView>,
) -> impl IntoElement {
    let path = short_cwd(&cwd);
    let (link, success, danger) = {
        let theme = cx.theme();
        (theme.link, theme.success, theme.danger)
    };

    h_flex()
        .id(format!("pane-head-{}", pane.0))
        .flex_none()
        .items_center()
        .px_3()
        .h(px(34.))
        .gap_2()
        .min_w_0()
        .bg(header_bg)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.focus.focus(window, cx);
                cx.emit(TerminalEvent::Activated);
                cx.notify();
            }),
        )
        .child(div().size(px(14.)).flex_none().child(mark))
        .child(
            div()
                .flex_none()
                .max_w(px(180.))
                .min_w_0()
                .truncate()
                .text_xs()
                .font_medium()
                .text_color(fg)
                .child(name),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(muted)
                .truncate()
                .child(path),
        )
        .when_some(note, |row, text| {
            row.child(div().flex_none().text_xs().text_color(warning).child(text))
        })
        .when_some(status, |row, status| {
            row.child(agent_status_badge(
                status, link, warning, success, danger, radius,
            ))
        })
        .child(agent_menu_button(pane, muted, hover, cx))
        .child(chrome_button(
            format!("split-right-{}", pane.0),
            "split-right.svg",
            "向右分屏 ⌘D",
            muted,
            radius,
            hover,
            cx.listener(|_, _, _, cx| {
                cx.emit(TerminalEvent::SplitRight);
                cx.stop_propagation();
            }),
        ))
        .child(chrome_button(
            format!("split-down-{}", pane.0),
            "split-down.svg",
            "向下分屏 ⌘⇧D",
            muted,
            radius,
            hover,
            cx.listener(|_, _, _, cx| {
                cx.emit(TerminalEvent::SplitDown);
                cx.stop_propagation();
            }),
        ))
        .when(can_close, |row| {
            row.child(chrome_button(
                format!("close-pane-{}", pane.0),
                "icons/close.svg",
                "关闭窗格",
                muted,
                radius,
                hover,
                cx.listener(|_, _, _, cx| {
                    cx.emit(TerminalEvent::Close);
                    cx.stop_propagation();
                }),
            ))
        })
}

/// The agent picker: a menu rather than a second split button, because a new
/// pane is a shell unless the user names an agent.
fn agent_menu_button(
    pane: PaneId,
    muted: Hsla,
    hover: Hsla,
    cx: &mut Context<TerminalView>,
) -> impl IntoElement {
    let view = cx.entity().downgrade();
    Button::new(format!("agent-menu-{}", pane.0))
        .custom(
            ButtonCustomVariant::new(cx)
                .color(cx.theme().transparent)
                .foreground(muted)
                .hover(hover),
        )
        .compact()
        .icon(IconName::Bot)
        .text_color(muted)
        .tooltip("新建 Agent 窗格")
        .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
            let available: Vec<_> = AGENTS.iter().filter(|agent| installed(agent.id)).collect();
            if available.is_empty() {
                return menu.item(PopupMenuItem::new("未安装 Agent").disabled(true));
            }
            let mut menu = menu;
            for agent in available {
                let view = view.clone();
                menu = menu.item(PopupMenuItem::new(agent.name).on_click(move |_, _, cx| {
                    let _ = view.update(cx, |_, cx| {
                        cx.emit(TerminalEvent::AgentPane(agent.id.to_string()));
                    });
                }));
            }
            menu
        })
}

#[allow(clippy::too_many_arguments)]
fn chrome_button(
    id: String,
    icon_path: &'static str,
    label: &'static str,
    muted: Hsla,
    radius: Pixels,
    hover: Hsla,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .rounded(radius)
        .p_0p5()
        .cursor_pointer()
        .text_color(muted)
        .hover(move |style| style.bg(hover))
        .hover_label(label)
        .on_click(on_click)
        .child(
            Icon::empty()
                .path(icon_path)
                .with_size(px(14.))
                .text_color(muted),
        )
}

/// One direction button in the find bar. Clicking it keeps the input focused,
/// so Enter keeps walking from the same place.
#[allow(clippy::too_many_arguments)]
fn find_step_button(
    id: String,
    icon: IconName,
    label: &'static str,
    older: bool,
    opacity: f32,
    muted: Hsla,
    radius: Pixels,
    hover: Hsla,
    cx: &mut Context<TerminalView>,
) -> AnyElement {
    div()
        .id(id)
        .flex_none()
        .rounded(radius)
        .p_0p5()
        .opacity(opacity)
        .cursor_pointer()
        .text_color(muted)
        .hover(move |style| style.bg(hover))
        .hover_label(label)
        .on_click(cx.listener(move |this, _, window, cx| {
            this.step_find(older, window, cx);
        }))
        .child(Icon::new(icon).with_size(px(14.)).text_color(muted))
        .into_any_element()
}

pub(super) fn header_identity(snapshot: &Snapshot, cx: &App) -> (AnyElement, String, String) {
    let muted = cx.theme().muted_foreground;
    let mark = Icon::new(IconName::SquareTerminal)
        .with_size(px(14.))
        .text_color(muted)
        .into_any_element();
    (mark, header_name(snapshot), snapshot.cwd.clone())
}

/// The pane header's label: what the program calls itself, or a placeholder
/// before anything has run.
pub(super) fn header_name(snapshot: &Snapshot) -> String {
    match snapshot.label() {
        label if label.is_empty() => "终端".into(),
        label => label,
    }
}

pub(super) fn header_note(snapshot: &Snapshot) -> Option<String> {
    if let Some(error) = &snapshot.spawn_error {
        return Some(format!("启动失败：{error}"));
    }
    if snapshot.exited && !snapshot.command.is_empty() {
        return Some("已退出".into());
    }
    if snapshot.command.is_empty() {
        return Some("还没有终端".into());
    }
    None
}

pub(super) fn short_cwd(cwd: &str) -> String {
    static HOME: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| {
        std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
            .map(std::path::PathBuf::from)
    });
    if let Some(home) = home.as_ref()
        && let Ok(stripped) = std::path::Path::new(cwd).strip_prefix(home)
    {
        let rest = stripped.to_string_lossy();
        if rest.is_empty() {
            return "~".into();
        }
        return format!("~/{rest}");
    }
    cwd.to_string()
}
