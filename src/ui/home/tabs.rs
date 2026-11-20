use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::KumuApp;
use super::panes::{PaneId, PaneNode, ProjectTab, ProjectTabs, TabId};
use crate::ui::tooltip::HoverLabel as _;

impl KumuApp {
    /// The project and tab that own `pane`.
    pub(super) fn pane_tab(&self, pane: PaneId) -> Option<(i64, usize)> {
        self.tabs_by_project.iter().find_map(|(project, tabs)| {
            tabs.tabs
                .iter()
                .position(|tab| tab.layout.contains(pane))
                .map(|index| (*project, index))
        })
    }

    /// Whether `pane` belongs to the project and tab currently on screen.
    pub(super) fn pane_is_visible(&self, pane: PaneId) -> bool {
        let Some((project, tab_index)) = self.pane_tab(pane) else {
            return false;
        };
        self.selected == Some(project)
            && self
                .tabs_by_project
                .get(&project)
                .is_some_and(|tabs| tabs.selected == tab_index)
    }

    /// The visible tab of `project`.
    pub(super) fn active_tab(&self, project: i64) -> Option<&ProjectTab> {
        self.tabs_by_project.get(&project)?.active()
    }

    pub(super) fn active_tab_mut(&mut self, project: i64) -> Option<&mut ProjectTab> {
        self.tabs_by_project.get_mut(&project)?.active_mut()
    }

    /// The visible tab name, with a numbered fallback until the user renames it.
    pub(super) fn tab_name(tabs: &ProjectTabs, index: usize) -> String {
        let name = tabs
            .tabs
            .get(index)
            .map(|tab| tab.name.trim())
            .unwrap_or("");
        if name.is_empty() {
            format!("工作区 {}", index + 1)
        } else {
            name.to_string()
        }
    }

    /// Materialize and attach every pane of one tab. The other tabs of the
    /// project are hidden but keep their processes.
    pub(super) fn show_tab_panes(&mut self, project: i64, index: usize, cx: &mut Context<Self>) {
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return;
        };
        let hidden: Vec<PaneId> = tabs
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .flat_map(|(_, tab)| tab.layout.panes())
            .collect();
        let visible = tabs
            .tabs
            .get(index)
            .map(|tab| tab.layout.panes())
            .unwrap_or_default();
        for pane in hidden {
            if let Some(terminal) = self.panes.get(&pane).cloned() {
                terminal.update(cx, |terminal, cx| terminal.hide(cx));
            }
        }
        for pane in visible {
            let terminal = if let Some(terminal) = self.panes.get(&pane).cloned() {
                terminal
            } else {
                self.install_pane(pane, cx)
            };
            let launch = self.launches.get(&pane).cloned().unwrap_or_default();
            terminal.update(cx, |terminal, cx| terminal.show(project, &launch, cx));
        }
    }

    /// Show the tab the project had selected.
    pub(super) fn show_selected_tab(&mut self, project: i64, cx: &mut Context<Self>) {
        let Some(index) = self.tabs_by_project.get(&project).map(|tabs| tabs.selected) else {
            return;
        };
        self.show_tab_panes(project, index, cx);
    }

    /// Open a new tab with one shell, after the visible one, and focus it.
    pub(super) fn new_tab(&mut self, cx: &mut Context<Self>) {
        let project_id = self.selected.or_else(|| {
            self.workspace
                .update(cx, |workspace, cx| workspace.ensure_project(cx))
        });
        let Some(project_id) = project_id else {
            return;
        };
        if !self.tabs_by_project.contains_key(&project_id) {
            self.ensure_project_layout(project_id, cx);
            self.select_project(project_id, cx);
            return;
        }
        let pane = PaneId(self.next_pane);
        let Some(tabs) = self.tabs_by_project.get_mut(&project_id) else {
            return;
        };
        let index = (tabs.selected + 1).min(tabs.tabs.len());
        tabs.tabs.insert(
            index,
            ProjectTab::new(String::new(), PaneNode::leaf(pane), pane),
        );
        tabs.selected = index;
        self.next_pane += 1;
        self.launches.remove(&pane);
        if self.selected != Some(project_id) {
            if let Some(previous) = self.selected {
                self.hide_project_panes(previous, cx);
            }
            self.selected = Some(project_id);
        }
        self.focused = pane;
        self.pending_focus = Some(pane);
        self.pending_close_tab = None;
        // Installs the view; show_selected_tab attaches it and hides the
        // tab that was on screen.
        self.show_selected_tab(project_id, cx);
        self.sync_pane_chrome(cx);
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    /// Move to the next (+1) or previous (-1) tab, wrapping.
    pub(super) fn switch_tab(&mut self, offset: isize, cx: &mut Context<Self>) {
        let Some(project) = self.selected else {
            return;
        };
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return;
        };
        let count = tabs.tabs.len();
        if count <= 1 {
            return;
        }
        let next = (tabs.selected as isize + offset).rem_euclid(count as isize) as usize;
        self.select_tab(next, cx);
    }

    /// Show tab `index` (0-based) of the selected project.
    pub(super) fn select_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(project) = self.selected else {
            return;
        };
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return;
        };
        if index >= tabs.tabs.len() || tabs.selected == index {
            return;
        }
        self.pending_close_tab = None;
        if let Some(tabs) = self.tabs_by_project.get_mut(&project) {
            tabs.selected = index;
        }
        if let Some(focused) = self.active_tab(project).map(|tab| tab.focused) {
            self.focused = focused;
            self.pending_focus = Some(focused);
        }
        self.show_selected_tab(project, cx);
        self.sync_pane_chrome(cx);
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    pub(super) fn tab_index_by_id(&self, project: i64, tab_id: TabId) -> Option<usize> {
        self.tabs_by_project
            .get(&project)?
            .tabs
            .iter()
            .position(|tab| tab.id == tab_id)
    }

    /// Close request from `⌘W` or the tab's `×`. A tab with several panes asks
    /// first, because closing it ends every terminal in it.
    pub(super) fn request_close_tab(
        &mut self,
        project: i64,
        tab_id: TabId,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(project, tab_id) else {
            return;
        };
        let Some(panes) = self
            .tabs_by_project
            .get(&project)
            .and_then(|tabs| tabs.tabs.get(index))
            .map(|tab| tab.layout.panes().len())
        else {
            return;
        };
        if panes > 1 {
            self.pending_close_tab = Some((project, tab_id));
            cx.notify();
            return;
        }
        self.finish_close_tab(project, index, cx);
    }

    pub(super) fn finish_close_tab_by_id(
        &mut self,
        project: i64,
        tab_id: TabId,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(project, tab_id) else {
            self.pending_close_tab = None;
            cx.notify();
            return;
        };
        self.finish_close_tab(project, index, cx);
    }

    pub(super) fn close_selected_tab(&mut self, cx: &mut Context<Self>) {
        // While a close confirmation is on screen, ⌘W answers it instead of
        // closing whichever tab happens to be selected.
        if let Some((project, tab_id)) = self.pending_close_tab {
            self.finish_close_tab_by_id(project, tab_id, cx);
            return;
        }
        let Some(project) = self.selected else {
            return;
        };
        let Some(tab_id) = self
            .tabs_by_project
            .get(&project)
            .and_then(ProjectTabs::active)
            .map(|tab| tab.id)
        else {
            return;
        };
        self.request_close_tab(project, tab_id, cx);
    }

    /// Rename a tab; the dialog starts on the name the strip shows.
    pub(super) fn prompt_rename_tab(
        &self,
        project: i64,
        tab_id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(project, tab_id) else {
            return;
        };
        let Some(current) = self
            .tabs_by_project
            .get(&project)
            .map(|tabs| Self::tab_name(tabs, index))
        else {
            return;
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current));
        let weak = cx.weak_entity();
        let dialog_input = input.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("重命名页签")
                .child(Input::new(&dialog_input).small())
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("保存")
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .on_ok({
                    let weak = weak.clone();
                    let input = dialog_input.clone();
                    move |_, _, cx| {
                        let name = input.read(cx).value().trim().to_string();
                        if !name.is_empty() {
                            let _ = weak.update(cx, move |app, cx| {
                                if let Some(tabs) = app.tabs_by_project.get_mut(&project)
                                    && let Some(tab) =
                                        tabs.tabs.iter_mut().find(|tab| tab.id == tab_id)
                                {
                                    tab.name = name;
                                }
                                app.persist_workspace(cx);
                                cx.notify();
                            });
                        }
                        true
                    }
                })
                .centered()
        });
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
    }

    /// Remove a tab and end every terminal in it.
    pub(super) fn finish_close_tab(&mut self, project: i64, index: usize, cx: &mut Context<Self>) {
        self.pending_close_tab = None;
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return;
        };
        let Some(tab) = tabs.tabs.get(index) else {
            return;
        };
        let panes = tab.layout.panes();
        let was_selected = tabs.selected == index;
        let leaving_project = tabs.tabs.len() == 1;
        let new_tab = if leaving_project {
            None
        } else if was_selected {
            Some(index.min(tabs.tabs.len() - 2))
        } else if index < tabs.selected {
            Some(tabs.selected - 1)
        } else {
            Some(tabs.selected)
        };
        if leaving_project {
            self.tabs_by_project.remove(&project);
        } else if let Some(tabs) = self.tabs_by_project.get_mut(&project) {
            tabs.tabs.remove(index);
            if let Some(new_tab) = new_tab {
                tabs.selected = new_tab;
            }
        }
        for pane in panes {
            self.workspace
                .update(cx, |workspace, _| workspace.drop_terminal(pane.0));
            self.panes.remove(&pane);
            self.launches.remove(&pane);
        }
        if self.selected == Some(project) {
            if leaving_project {
                self.pending_focus = None;
            } else {
                if let Some(focused) = self.active_tab(project).map(|tab| tab.focused) {
                    self.focused = focused;
                    self.pending_focus = Some(focused);
                }
                self.show_selected_tab(project, cx);
            }
        }
        self.sync_pane_chrome(cx);
        self.sync_plugin_project(cx);
        self.persist_workspace(cx);
        cx.notify();
    }

    /// The project-level tab strip above the panes. Each tab is one whole
    /// layout; the `+` opens another, `…` renames or closes the visible one.
    pub(super) fn render_tab_bar(&self, cx: &Context<Self>) -> AnyElement {
        let Some(project) = self.selected else {
            return div().into_any_element();
        };
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let (muted, fg, hover) = (
            theme.muted_foreground,
            theme.foreground,
            theme.accent.opacity(0.5),
        );
        if let Some((pending_project, tab_id)) = self
            .pending_close_tab
            .filter(|(pending_project, _)| *pending_project == project)
            && let Some(index) = tabs.tabs.iter().position(|tab| tab.id == tab_id)
            && let Some(tab) = tabs.tabs.get(index)
        {
            let name = Self::tab_name(tabs, index);
            let count = tab.layout.panes().len();
            return h_flex()
                .id("project-tab-close-confirm")
                .flex_none()
                .h(px(36.))
                .px_3()
                .gap_2()
                .items_center()
                .bg(theme.warning.opacity(0.12))
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(format!("关闭「{name}」会结束 {count} 个终端。")),
                )
                .child(
                    Button::new("tab-close-cancel")
                        .ghost()
                        .label("取消")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.pending_close_tab = None;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("tab-close-confirm")
                        .label("关闭页签")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.finish_close_tab_by_id(pending_project, tab_id, cx);
                        })),
                )
                .into_any_element();
        }

        let mut strip = h_flex()
            .id("project-tabs-strip")
            .flex_1()
            .min_w_0()
            .gap_1()
            .overflow_x_scroll();
        for index in 0..tabs.tabs.len() {
            let active = index == tabs.selected;
            let name = Self::tab_name(tabs, index);
            let tab_id = tabs.tabs[index].id;
            strip = strip.child(
                h_flex()
                    .id(format!("project-tab-{project}-{}", tab_id.0))
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .h(px(26.))
                    .px_2()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(active, |this| {
                        this.bg(theme.background)
                            .border_1()
                            .border_color(theme.border)
                    })
                    .when(!active, |this| this.hover(move |style| style.bg(hover)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(idx) = this.tab_index_by_id(project, tab_id) {
                            this.select_tab(idx, cx);
                        }
                        cx.stop_propagation();
                    }))
                    .child(
                        Icon::new(IconName::LayoutDashboard)
                            .with_size(px(14.))
                            .text_color(muted),
                    )
                    .child(
                        div()
                            .max_w(px(160.))
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .font_medium()
                            .text_color(if active { fg } else { muted })
                            .child(name),
                    )
                    .child(
                        div()
                            .id(format!("project-tab-close-{project}-{}", tab_id.0))
                            .flex_none()
                            .rounded(theme.radius)
                            .p_0p5()
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .hover_label("关闭页签 ⌘W")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.request_close_tab(project, tab_id, cx);
                            }))
                            .child(
                                Icon::new(IconName::Close)
                                    .with_size(px(12.))
                                    .text_color(muted)
                                    .opacity(if active { 1. } else { 0.55 }),
                            ),
                    ),
            );
        }

        let selected_tab_id = tabs.active().map(|tab| tab.id);
        let menu_view = cx.entity().downgrade();
        h_flex()
            .id("project-tabs")
            .flex_none()
            .h(px(36.))
            .items_center()
            .gap_2()
            .px_2()
            .bg(theme.secondary.opacity(0.45))
            .border_b_1()
            .border_color(theme.border)
            .child(strip)
            .child(
                div()
                    .id("project-tab-add")
                    .flex_none()
                    .rounded(theme.radius)
                    .p_0p5()
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .hover_label("新建页签 ⌘T")
                    .on_click(cx.listener(|this, _, _, cx| this.new_tab(cx)))
                    .child(
                        Icon::new(IconName::Plus)
                            .with_size(px(14.))
                            .text_color(muted),
                    ),
            )
            .child(
                Button::new("project-tab-menu")
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(theme.transparent)
                            .foreground(muted)
                            .hover(hover),
                    )
                    .compact()
                    .icon(IconName::Ellipsis)
                    .text_color(muted)
                    .tooltip("页签操作")
                    .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
                        let rename = menu_view.clone();
                        let close = menu_view.clone();
                        menu.item(PopupMenuItem::new("重命名页签").on_click(
                            move |_, window, cx| {
                                if let Some(tab_id) = selected_tab_id {
                                    let _ = rename.update(cx, |this, cx| {
                                        this.prompt_rename_tab(project, tab_id, window, cx)
                                    });
                                }
                            },
                        ))
                        .item(PopupMenuItem::new("关闭页签").on_click(move |_, _, cx| {
                            if let Some(tab_id) = selected_tab_id {
                                let _ = close.update(cx, |this, cx| {
                                    this.request_close_tab(project, tab_id, cx)
                                });
                            }
                        }))
                    }),
            )
            .into_any_element()
    }
}
