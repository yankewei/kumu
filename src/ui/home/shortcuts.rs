//! Window-level global keyboard shortcuts, sidebar collapse, pane maximize
//! (zoom), font scaling, and `⌘K` Command Palette host integration.

use gpui::{
    AnyElement, Context, Entity, InteractiveElement, IntoElement, KeyDownEvent, Keystroke,
    MouseButton, ParentElement, StatefulInteractiveElement, Styled, Window, deferred, div, px, svg,
};

use super::KumuApp;
use super::terminal::short_cwd_display;
use crate::agent::{AGENTS, AgentState, installed};
use crate::app::layout::{PaneDirection, PaneId, SplitAxis};
use crate::nsh_ui::command_palette::{
    ActiveCommandPalette, PaletteIcon, PaletteItem, PaletteOutcome, PaletteSection, PaletteTarget,
};
use crate::theme::{self, DEFAULT_FONT_SIZE, FONT_SIZES, KumuTheme, ThemeMode};

/// Global shortcuts intercepted at the home window root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WindowShortcut {
    ToggleCommandPalette,
    ToggleSidebar,
    OpenSettings,
    NewTab,
    CloseTab,
    ClosePane,
    PrevTab,
    NextTab,
    SelectTab(usize),
    SplitRight,
    SplitDown,
    ToggleZoomPane,
    FocusNeighbor(PaneDirection),
    ZoomIn,
    ZoomOut,
    ZoomReset,
}

fn pane_direction(key: &str) -> Option<PaneDirection> {
    match key {
        "left" => Some(PaneDirection::Left),
        "right" => Some(PaneDirection::Right),
        "up" => Some(PaneDirection::Up),
        "down" => Some(PaneDirection::Down),
        _ => None,
    }
}

/// Classify a GPUI keystroke into a window-level shortcut, returning `None`
/// for keys that belong to the focused terminal or text input.
pub(super) fn classify_window_shortcut(
    keystroke: &Keystroke,
    is_held: bool,
) -> Option<WindowShortcut> {
    let mods = &keystroke.modifiers;
    if !mods.platform {
        return None;
    }
    let key = keystroke.key.as_str();

    // ⌘⌥←/→/↑/↓ moves focus between split panes.
    if mods.alt && !mods.control && !mods.shift {
        return pane_direction(key).map(WindowShortcut::FocusNeighbor);
    }
    if mods.alt || mods.control {
        return None;
    }

    // Mutating or toggling shortcuts ignore key repeat while held so a held ⌘W
    // cannot accidentally confirm its own close prompt.
    if is_held && matches!(key, "k" | "b" | "," | "t" | "w" | "d" | "enter") {
        return None;
    }

    match key {
        "k" if !mods.shift => Some(WindowShortcut::ToggleCommandPalette),
        "b" if !mods.shift => Some(WindowShortcut::ToggleSidebar),
        "," if !mods.shift => Some(WindowShortcut::OpenSettings),
        "t" if !mods.shift => Some(WindowShortcut::NewTab),
        "w" if !mods.shift => Some(WindowShortcut::CloseTab),
        "w" if mods.shift => Some(WindowShortcut::ClosePane),
        "{" | "[" if mods.shift => Some(WindowShortcut::PrevTab),
        "}" | "]" if mods.shift => Some(WindowShortcut::NextTab),
        digit @ ("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9") if !mods.shift => digit
            .parse::<usize>()
            .ok()
            .map(|n| WindowShortcut::SelectTab(n - 1)),
        "d" if !mods.shift => Some(WindowShortcut::SplitRight),
        "d" if mods.shift => Some(WindowShortcut::SplitDown),
        "enter" if mods.shift => Some(WindowShortcut::ToggleZoomPane),
        "+" | "=" => Some(WindowShortcut::ZoomIn),
        "-" | "_" => Some(WindowShortcut::ZoomOut),
        "0" if !mods.shift => Some(WindowShortcut::ZoomReset),
        _ => None,
    }
}

impl KumuApp {
    /// Capture-phase window key handler: enforces modal guards first, then
    /// dispatches global shortcuts regardless of which sub-panel holds focus.
    pub(super) fn on_window_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mods = &event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        let plain_escape =
            key == "escape" && !mods.platform && !mods.control && !mods.alt && !mods.shift;

        // Topmost overlay guard: command palette.
        if self.active_palette.is_some() {
            let toggle_close = !event.is_held
                && mods.platform
                && !mods.alt
                && !mods.control
                && !mods.shift
                && key == "k";
            if plain_escape || toggle_close {
                self.active_palette = None;
                self.focus_terminal(window, cx);
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }

        // Modal dialog or full-window sheet guard: block workspace shortcuts.
        if self.active_dialog.is_some() || self.active_sheet.is_some() {
            if plain_escape {
                if self.active_dialog.is_some() {
                    self.active_dialog = None;
                } else {
                    self.active_sheet = None;
                }
                self.focus_terminal(window, cx);
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }

        let Some(shortcut) = classify_window_shortcut(&event.keystroke, event.is_held) else {
            return;
        };

        self.execute_window_shortcut(shortcut, window, cx);
        cx.stop_propagation();
    }

    /// Bubble-phase fallback: allows `⌘F` to open the focused terminal's find
    /// bar even when focus is on the sidebar, tab bar, or window chrome.
    pub(super) fn on_window_key_down_bubble(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_palette.is_some()
            || self.active_dialog.is_some()
            || self.active_sheet.is_some()
        {
            return;
        }
        let mods = &event.keystroke.modifiers;
        if mods.platform
            && !mods.shift
            && !mods.alt
            && !mods.control
            && event.keystroke.key == "f"
            && let Some(terminal) = self.focused_visible_terminal()
        {
            terminal.update(cx, |terminal, cx| terminal.open_find(window, cx));
            cx.stop_propagation();
        }
    }

    fn focused_visible_terminal(&self) -> Option<Entity<super::TerminalView>> {
        let project = self.selected?;
        let tab = self.active_tab(project)?;
        if !tab.layout.contains(self.focused) {
            return None;
        }
        self.panes.get(&self.focused).cloned()
    }

    fn execute_window_shortcut(
        &mut self,
        shortcut: WindowShortcut,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match shortcut {
            WindowShortcut::ToggleCommandPalette => self.open_command_palette(window, cx),
            WindowShortcut::ToggleSidebar => self.toggle_sidebar(cx),
            WindowShortcut::OpenSettings => self.open_settings(false, cx),
            WindowShortcut::NewTab => {
                self.new_tab(cx);
                self.focus_terminal(window, cx);
            }
            WindowShortcut::CloseTab => {
                self.close_selected_tab(cx);
                self.focus_terminal(window, cx);
            }
            WindowShortcut::ClosePane => {
                if self.focused_visible_terminal().is_some() {
                    self.close_pane(self.focused, cx);
                    self.focus_terminal(window, cx);
                }
            }
            WindowShortcut::PrevTab => {
                self.switch_tab(-1, cx);
                self.focus_terminal(window, cx);
            }
            WindowShortcut::NextTab => {
                self.switch_tab(1, cx);
                self.focus_terminal(window, cx);
            }
            WindowShortcut::SelectTab(index) => {
                self.select_tab(index, cx);
                self.focus_terminal(window, cx);
            }
            WindowShortcut::SplitRight => {
                if self.focused_visible_terminal().is_some() {
                    self.split_pane(self.focused, SplitAxis::Horizontal, cx);
                    self.focus_terminal(window, cx);
                }
            }
            WindowShortcut::SplitDown => {
                if self.focused_visible_terminal().is_some() {
                    self.split_pane(self.focused, SplitAxis::Vertical, cx);
                    self.focus_terminal(window, cx);
                }
            }
            WindowShortcut::ToggleZoomPane => {
                if self.focused_visible_terminal().is_some() {
                    self.toggle_zoom_pane(self.focused, window, cx);
                }
            }
            WindowShortcut::FocusNeighbor(direction) => {
                if self.focused_visible_terminal().is_some() {
                    self.focus_neighbor(self.focused, direction, cx);
                    self.focus_terminal(window, cx);
                }
            }
            WindowShortcut::ZoomIn => self.step_font_size(1, cx),
            WindowShortcut::ZoomOut => self.step_font_size(-1, cx),
            WindowShortcut::ZoomReset => self.reset_font_size(cx),
        }
    }

    /// Collapse or expand the left sidebar and persist the preference.
    pub(super) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        let _ = crate::db::set_setting(
            crate::db::SETTING_SIDEBAR_VISIBLE,
            if self.sidebar_visible { "1" } else { "0" },
        );
        cx.notify();
    }

    /// The currently zoomed pane if it still belongs to the active tab and that
    /// tab actually has multiple split panes.
    pub(super) fn active_zoomed_pane(&self) -> Option<PaneId> {
        let zoomed = self.zoomed_pane?;
        let project = self.selected?;
        let tab = self.active_tab(project)?;
        let panes = tab.layout.panes();
        (panes.len() > 1 && panes.contains(&zoomed)).then_some(zoomed)
    }

    /// Maximize `pane` over its split siblings, or restore the full split grid.
    pub(super) fn toggle_zoom_pane(
        &mut self,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let can_zoom = self
            .selected
            .and_then(|project| self.active_tab(project))
            .is_some_and(|tab| {
                let panes = tab.layout.panes();
                panes.len() > 1 && panes.contains(&pane)
            });
        if !can_zoom {
            if self.zoomed_pane.take().is_some() {
                self.sync_pane_chrome(cx);
                cx.notify();
            }
            return;
        }
        if self.zoomed_pane == Some(pane) {
            self.zoomed_pane = None;
        } else {
            self.focus_pane(pane, cx);
            self.zoomed_pane = Some(pane);
        }
        self.pending_focus = Some(pane);
        self.sync_pane_chrome(cx);
        self.focus_terminal(window, cx);
        cx.notify();
    }

    fn step_font_size(&mut self, delta: i32, cx: &mut Context<Self>) {
        let min = *FONT_SIZES.first().unwrap_or(&12) as i32;
        let max = *FONT_SIZES.last().unwrap_or(&20) as i32;
        let current = KumuTheme::global(cx).font_size.round() as i32;
        let next = (current + delta).clamp(min, max);
        if next == current {
            return;
        }
        let _ = crate::db::set_setting(crate::db::SETTING_FONT_SIZE, &next.to_string());
        theme::set_font_size(cx, next as f32);
        cx.notify();
    }

    fn reset_font_size(&mut self, cx: &mut Context<Self>) {
        let target = DEFAULT_FONT_SIZE as u32;
        if (KumuTheme::global(cx).font_size - DEFAULT_FONT_SIZE).abs() < f32::EPSILON {
            return;
        }
        let _ = crate::db::set_setting(crate::db::SETTING_FONT_SIZE, &target.to_string());
        theme::set_font_size(cx, DEFAULT_FONT_SIZE);
        cx.notify();
    }

    fn toggle_theme_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = if KumuTheme::global(cx).mode.is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        let _ = crate::db::set_setting(
            crate::db::SETTING_THEME_MODE,
            if next.is_dark() { "dark" } else { "light" },
        );
        theme::change_mode(next, Some(window), cx);
        cx.notify();
    }

    /// Compact title-bar toggle button beside the brand label so mouse users
    /// can always collapse or restore the sidebar.
    pub(super) fn render_sidebar_toggle_button(&self, cx: &Context<Self>) -> AnyElement {
        let theme = KumuTheme::global(cx);
        let colors = &theme.colors;
        let visible = self.sidebar_visible;
        let hovered = self.hovered_sidebar_tooltip;
        let hover_bg = colors.accent.opacity(0.5);
        let fg = if visible {
            colors.muted_foreground
        } else {
            colors.link
        };
        let tooltip = if visible {
            "折叠侧边栏 ⌘B"
        } else {
            "展开侧边栏 ⌘B"
        };

        let button = div()
            .id("titlebar-sidebar-toggle")
            .flex()
            .items_center()
            .justify_center()
            .w(px(24.))
            .h(px(24.))
            .rounded(px(6.))
            .cursor_pointer()
            .bg(if visible {
                colors.transparent
            } else {
                colors.accent
            })
            .hover(move |style| style.bg(hover_bg))
            .on_hover(cx.listener(|this, is_hovered: &bool, _, cx| {
                if this.hovered_sidebar_tooltip != *is_hovered {
                    this.hovered_sidebar_tooltip = *is_hovered;
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, window, cx| {
                this.hovered_sidebar_tooltip = false;
                this.toggle_sidebar(cx);
                this.focus_terminal(window, cx);
            }))
            .child(
                svg()
                    .path("icons/panel-left.svg")
                    .size(px(14.))
                    .text_color(fg),
            );

        let mut wrapper = div().relative().child(button);
        if hovered {
            wrapper = wrapper.child(
                deferred(
                    div()
                        .absolute()
                        .top(px(28.))
                        .left_0()
                        .px_2()
                        .py_1()
                        .rounded(px(theme.radius))
                        .bg(colors.popover)
                        .border_1()
                        .border_color(colors.border)
                        .shadow_md()
                        .whitespace_nowrap()
                        .text_xs()
                        .text_color(colors.popover_foreground)
                        .child(tooltip),
                )
                .with_priority(1),
            );
        }
        wrapper.into_any_element()
    }

    pub(super) fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.build_palette_items(cx);
        let theme = KumuTheme::global(cx).clone();
        self.active_palette = Some(ActiveCommandPalette::open(
            items,
            &theme,
            window,
            cx,
            |app, outcome, window, cx| {
                app.active_palette = None;
                match outcome {
                    PaletteOutcome::Selected(target) => {
                        app.execute_palette_target(target, window, cx);
                    }
                    PaletteOutcome::Dismissed => {
                        app.focus_terminal(window, cx);
                        cx.notify();
                    }
                }
            },
        ));
        cx.notify();
    }

    fn build_palette_items(&self, cx: &Context<Self>) -> Vec<PaletteItem> {
        let mut items = Vec::new();
        let sidebar = self.build_sidebar_snapshot(cx);

        // 1. Active agents across all projects & tabs (already priority-sorted).
        for agent in sidebar.agents {
            let def = AGENTS.iter().find(|def| def.id == agent.agent);
            let logo = def.and_then(|def| def.logo);
            let agent_name = def.map_or(agent.agent.as_str(), |def| def.name);
            items.push(PaletteItem {
                section: PaletteSection::Agents,
                title: agent.title,
                subtitle: Some(format!("{} · {}", agent.project_name, agent.tab_name)),
                keywords: format!(
                    "agent {} {} {} {}",
                    agent.agent,
                    agent_name,
                    agent.project_path,
                    agent.state.map_or("idle", AgentState::label)
                ),
                icon: PaletteIcon::AgentLogo(logo),
                agent_state: agent.state,
                badge: agent.focused.then(|| "当前".to_string()),
                shortcut: None,
                target: PaletteTarget::RevealAgent(agent.pane),
            });
        }

        // 2. Projects & worktrees.
        for project in sidebar.projects {
            let icon = if project.worktree {
                "icons/git-branch.svg"
            } else if project.active {
                "icons/folder-open.svg"
            } else {
                "icons/folder.svg"
            };
            let badge = if project.unread {
                Some("未读".to_string())
            } else if project.active {
                Some("当前".to_string())
            } else {
                None
            };
            items.push(PaletteItem {
                section: PaletteSection::Projects,
                title: project.name,
                subtitle: Some(short_cwd_display(&project.path)),
                keywords: format!(
                    "project repo {} {}",
                    if project.worktree { "worktree" } else { "" },
                    project.path
                ),
                icon: PaletteIcon::Svg(icon),
                agent_state: None,
                badge,
                shortcut: None,
                target: PaletteTarget::SelectProject(project.id),
            });
        }

        // 3. Tabs in the selected project.
        if let Some(project_id) = self.selected
            && let Some(tabs) = self.tabs_by_project.get(&project_id)
        {
            for (index, tab) in tabs.tabs.iter().enumerate() {
                let name = Self::tab_name(tabs, index);
                let pane_count = tab.layout.panes().len();
                items.push(PaletteItem {
                    section: PaletteSection::Tabs,
                    title: name,
                    subtitle: Some(format!("{pane_count} 个窗格")),
                    keywords: format!("tab workspace {}", index + 1),
                    icon: PaletteIcon::Svg("icons/layout-dashboard.svg"),
                    agent_state: None,
                    badge: (tabs.selected == index).then(|| "当前".to_string()),
                    shortcut: match index {
                        0 => Some("⌘1"),
                        1 => Some("⌘2"),
                        2 => Some("⌘3"),
                        3 => Some("⌘4"),
                        4 => Some("⌘5"),
                        5 => Some("⌘6"),
                        6 => Some("⌘7"),
                        7 => Some("⌘8"),
                        8 => Some("⌘9"),
                        _ => None,
                    },
                    target: PaletteTarget::SelectTab(index),
                });
            }
        }

        // 4. Quick actions.
        let has_pane = self.focused_visible_terminal().is_some();
        let can_zoom = self
            .selected
            .and_then(|project| self.active_tab(project))
            .is_some_and(|tab| tab.layout.panes().len() > 1);
        let is_zoomed = self.active_zoomed_pane().is_some();

        items.push(PaletteItem {
            section: PaletteSection::Actions,
            title: "新建页签".into(),
            subtitle: None,
            keywords: "new tab workspace".into(),
            icon: PaletteIcon::Svg("icons/plus.svg"),
            agent_state: None,
            badge: None,
            shortcut: Some("⌘T"),
            target: PaletteTarget::NewTab,
        });

        if has_pane {
            items.push(PaletteItem {
                section: PaletteSection::Actions,
                title: "向右分屏".into(),
                subtitle: None,
                keywords: "split right horizontal pane".into(),
                icon: PaletteIcon::Svg("split-right.svg"),
                agent_state: None,
                badge: None,
                shortcut: Some("⌘D"),
                target: PaletteTarget::SplitRight,
            });
            items.push(PaletteItem {
                section: PaletteSection::Actions,
                title: "向下分屏".into(),
                subtitle: None,
                keywords: "split down vertical pane".into(),
                icon: PaletteIcon::Svg("split-down.svg"),
                agent_state: None,
                badge: None,
                shortcut: Some("⌘⇧D"),
                target: PaletteTarget::SplitDown,
            });
            if can_zoom || is_zoomed {
                items.push(PaletteItem {
                    section: PaletteSection::Actions,
                    title: if is_zoomed {
                        "还原分屏布局".into()
                    } else {
                        "最大化当前窗格".into()
                    },
                    subtitle: None,
                    keywords: "zoom maximize restore pane focus".into(),
                    icon: PaletteIcon::Svg(if is_zoomed {
                        "icons/minimize-2.svg"
                    } else {
                        "icons/maximize-2.svg"
                    }),
                    agent_state: None,
                    badge: None,
                    shortcut: Some("⌘⇧↩"),
                    target: PaletteTarget::ToggleZoomPane,
                });
            }
            items.push(PaletteItem {
                section: PaletteSection::Actions,
                title: "关闭当前窗格".into(),
                subtitle: None,
                keywords: "close pane kill terminal".into(),
                icon: PaletteIcon::Svg("icons/close.svg"),
                agent_state: None,
                badge: None,
                shortcut: Some("⌘⇧W"),
                target: PaletteTarget::ClosePane,
            });
            for agent in AGENTS.iter().filter(|agent| installed(agent.id)) {
                items.push(PaletteItem {
                    section: PaletteSection::Actions,
                    title: format!("在右侧新建 {} 窗格", agent.name),
                    subtitle: Some(agent.desc.into()),
                    keywords: format!("new agent split {} {}", agent.id, agent.name),
                    icon: PaletteIcon::AgentLogo(agent.logo),
                    agent_state: None,
                    badge: None,
                    shortcut: None,
                    target: PaletteTarget::NewAgentPane(agent.id.to_string()),
                });
            }
        }

        for spec in crate::plugins::specs() {
            let open = self.open_plugin == Some(spec.id);
            items.push(PaletteItem {
                section: PaletteSection::Actions,
                title: if open {
                    format!("收起 {} 面板", spec.title)
                } else {
                    format!("打开 {} 面板", spec.title)
                },
                subtitle: None,
                keywords: format!("plugin panel {} {}", spec.id, spec.title),
                icon: PaletteIcon::Svg(spec.icon),
                agent_state: None,
                badge: open.then(|| "已打开".to_string()),
                shortcut: None,
                target: PaletteTarget::TogglePlugin(spec.id),
            });
        }

        items.push(PaletteItem {
            section: PaletteSection::Actions,
            title: if self.sidebar_visible {
                "折叠左侧边栏".into()
            } else {
                "展开左侧边栏".into()
            },
            subtitle: None,
            keywords: "sidebar toggle collapse hide show".into(),
            icon: PaletteIcon::Svg("icons/panel-left.svg"),
            agent_state: None,
            badge: None,
            shortcut: Some("⌘B"),
            target: PaletteTarget::ToggleSidebar,
        });

        let dark = KumuTheme::global(cx).mode.is_dark();
        items.push(PaletteItem {
            section: PaletteSection::Actions,
            title: if dark {
                "切换为亮色外观".into()
            } else {
                "切换为深色外观".into()
            },
            subtitle: None,
            keywords: "theme appearance light dark mode".into(),
            icon: PaletteIcon::Svg("icons/settings.svg"),
            agent_state: None,
            badge: None,
            shortcut: None,
            target: PaletteTarget::ToggleTheme,
        });

        items.push(PaletteItem {
            section: PaletteSection::Actions,
            title: "打开设置".into(),
            subtitle: None,
            keywords: "settings preferences config".into(),
            icon: PaletteIcon::Svg("icons/settings.svg"),
            agent_state: None,
            badge: None,
            shortcut: Some("⌘,"),
            target: PaletteTarget::OpenSettings,
        });

        items
    }

    fn execute_palette_target(
        &mut self,
        target: PaletteTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            PaletteTarget::RevealAgent(pane) => {
                self.reveal_agent(pane, window, cx);
            }
            PaletteTarget::SelectProject(project_id) => {
                self.select_project(project_id, cx);
                self.focus_terminal(window, cx);
            }
            PaletteTarget::SelectTab(index) => {
                self.select_tab(index, cx);
                self.focus_terminal(window, cx);
            }
            PaletteTarget::NewTab => {
                self.new_tab(cx);
                self.focus_terminal(window, cx);
            }
            PaletteTarget::SplitRight => {
                if self.focused_visible_terminal().is_some() {
                    self.split_pane(self.focused, SplitAxis::Horizontal, cx);
                    self.focus_terminal(window, cx);
                }
            }
            PaletteTarget::SplitDown => {
                if self.focused_visible_terminal().is_some() {
                    self.split_pane(self.focused, SplitAxis::Vertical, cx);
                    self.focus_terminal(window, cx);
                }
            }
            PaletteTarget::ToggleZoomPane => {
                if self.focused_visible_terminal().is_some() {
                    self.toggle_zoom_pane(self.focused, window, cx);
                }
            }
            PaletteTarget::ClosePane => {
                if self.focused_visible_terminal().is_some() {
                    self.close_pane(self.focused, cx);
                    self.focus_terminal(window, cx);
                }
            }
            PaletteTarget::NewAgentPane(agent) => {
                if self.focused_visible_terminal().is_some() {
                    self.split_pane_with(
                        self.focused,
                        SplitAxis::Horizontal,
                        crate::agent::launch::PaneLaunch::Agent {
                            agent,
                            session: None,
                        },
                        cx,
                    );
                    self.focus_terminal(window, cx);
                }
            }
            PaletteTarget::TogglePlugin(id) => {
                if let Some(spec) = crate::plugins::spec(id) {
                    self.toggle_plugin(spec, window, cx);
                }
                self.focus_terminal(window, cx);
            }
            PaletteTarget::ToggleSidebar => {
                self.toggle_sidebar(cx);
                self.focus_terminal(window, cx);
            }
            PaletteTarget::ToggleTheme => {
                self.toggle_theme_mode(window, cx);
                self.focus_terminal(window, cx);
            }
            PaletteTarget::OpenSettings => {
                self.open_settings(false, cx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(spec: &'static str) -> Keystroke {
        Keystroke::parse(spec).expect("valid keystroke")
    }

    #[test]
    fn classifies_window_shortcuts_and_guards_repeats() {
        assert_eq!(
            classify_window_shortcut(&key("cmd-k"), false),
            Some(WindowShortcut::ToggleCommandPalette)
        );
        assert_eq!(
            classify_window_shortcut(&key("cmd-b"), false),
            Some(WindowShortcut::ToggleSidebar)
        );
        assert_eq!(
            classify_window_shortcut(&key("cmd-,"), false),
            Some(WindowShortcut::OpenSettings)
        );
        assert_eq!(
            classify_window_shortcut(&key("cmd-w"), false),
            Some(WindowShortcut::CloseTab)
        );
        assert_eq!(
            classify_window_shortcut(&key("cmd-shift-w"), false),
            Some(WindowShortcut::ClosePane)
        );
        assert_eq!(
            classify_window_shortcut(&key("cmd-shift-enter"), false),
            Some(WindowShortcut::ToggleZoomPane)
        );
        assert_eq!(
            classify_window_shortcut(&key("cmd-alt-right"), false),
            Some(WindowShortcut::FocusNeighbor(PaneDirection::Right))
        );
        assert_eq!(
            classify_window_shortcut(&key("cmd-="), false),
            Some(WindowShortcut::ZoomIn)
        );
        assert_eq!(
            classify_window_shortcut(&key("cmd--"), false),
            Some(WindowShortcut::ZoomOut)
        );
        assert_eq!(
            classify_window_shortcut(&key("cmd-0"), false),
            Some(WindowShortcut::ZoomReset)
        );
        // Held mutating/toggling shortcuts are ignored.
        assert_eq!(classify_window_shortcut(&key("cmd-w"), true), None);
        assert_eq!(classify_window_shortcut(&key("cmd-k"), true), None);
        // Clipboard and find remain context-sensitive.
        assert_eq!(classify_window_shortcut(&key("cmd-c"), false), None);
        assert_eq!(classify_window_shortcut(&key("cmd-v"), false), None);
        assert_eq!(classify_window_shortcut(&key("cmd-f"), false), None);
    }
}
