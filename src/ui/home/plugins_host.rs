use std::time::Duration;

use gpui::*;

use super::{AgentEntry, KumuApp, agent_display_name, shell_prompt, submits_after_paste};
use crate::agent::launch::PaneLaunch;
use crate::app::layout::ProjectTabs;
use crate::plugins::{PaneTarget, PluginHost, PluginProject, PluginSpec, SendOutcome};
use crate::term::KeyInput;
use crate::theme::KumuTheme;

/// The plugin panel's opening width and the range its divider allows.
pub(super) const PLUGIN_WIDTH: f32 = 300.;
pub(super) const PLUGIN_MIN_WIDTH: f32 = 240.;
pub(super) const PLUGIN_MAX_WIDTH: f32 = 480.;

/// Safety upper bound before clearing deferred terminal resizes or a closed
/// plugin view if the NSH settle callback never arrives.
const PANEL_SETTLE_FALLBACK: Duration = Duration::from_millis(360);

impl KumuApp {
    /// The project a plugin panel shows, if one is selected.
    pub(super) fn plugin_project(&self, cx: &App) -> Option<PluginProject> {
        self.selected
            .and_then(|id| self.workspace.read(cx).project(id))
            .map(PluginProject::from)
    }

    /// How plugins hand text to the user's terminal. A panel names the pane it
    /// writes to, and the window resolves it only when the send happens, so a
    /// closed pane is an error rather than text aimed at a stale id.
    pub(super) fn plugin_host(&self, cx: &Context<Self>) -> PluginHost {
        let home = cx.entity().downgrade();
        let listing = home.clone();
        let sheet_opener = home.clone();
        let sheet_closer = home.clone();
        PluginHost::with_panes(
            move |project, pane, text, cx| {
                let Some(home) = home.upgrade() else {
                    return Err("窗口已关闭".to_string());
                };
                home.update(cx, |app, cx| app.send_text_to_pane(project, pane, text, cx))
            },
            move |project, cx| {
                listing
                    .upgrade()
                    .map(|home| home.read(cx).plugin_panes(project, cx))
                    .unwrap_or_default()
            },
        )
        .with_sheet_tools(
            move |view, _window, cx| {
                if let Some(home) = sheet_opener.upgrade() {
                    home.update(cx, |app, cx| {
                        app.active_sheet = Some(view);
                        cx.notify();
                    });
                }
            },
            move |window, cx| {
                if let Some(home) = sheet_closer.upgrade() {
                    home.update(cx, |app, cx| {
                        app.active_sheet = None;
                        app.focus_terminal(window, cx);
                        cx.notify();
                    });
                }
            },
        )
    }

    /// The panes a plugin may hand text to, in layout order. For agent panes,
    /// uses `AgentEntry::display_title()` (session title > first prompt > OSC
    /// title > agent name) alongside the agent id and live status.
    pub(super) fn plugin_panes(&self, project: i64, cx: &App) -> Vec<PaneTarget> {
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return Vec::new();
        };
        let Some(tab) = tabs.active() else {
            return Vec::new();
        };
        let tab_name = Self::tab_name(tabs, tabs.selected);
        let focused = tab.focused;
        let workspace = self.workspace.read(cx);
        tab.layout
            .panes()
            .into_iter()
            .enumerate()
            .map(|(index, pane)| {
                let agent_entry = self.agent_status.get(&pane);
                let agent = agent_entry.map(|entry| entry.agent.clone()).or_else(|| {
                    match self.launches.get(&pane) {
                        Some(PaneLaunch::Agent { agent, .. }) => Some(agent.clone()),
                        _ => None,
                    }
                });
                let state = agent_entry.and_then(|entry| entry.state);
                let title = agent_entry
                    .map(AgentEntry::display_title)
                    .filter(|label| !label.is_empty())
                    .or_else(|| {
                        workspace
                            .terminal_handle(pane.0)
                            .map(|term| term.label())
                            .filter(|label| !label.is_empty())
                    })
                    .or_else(|| agent.as_deref().and_then(agent_display_name))
                    .unwrap_or_else(|| "终端".to_string());
                PaneTarget {
                    pane: pane.0,
                    ordinal: index + 1,
                    tab_name: tab_name.clone(),
                    title,
                    agent,
                    state,
                    focused: pane == focused,
                }
            })
            .collect()
    }

    /// Paste `text` into one pane of `project`, or into its focused pane when
    /// `pane` is `None`. An agent pane also receives Enter, because the user is
    /// handing the agent an instruction; a shell keeps the text on its command
    /// line, where Enter would run it. Without bracketed paste a shell would run
    /// each pasted line, so a shell gets the message folded onto one line.
    pub(super) fn send_text_to_pane(
        &self,
        project: i64,
        pane: Option<u64>,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Result<SendOutcome, String> {
        if self.selected != Some(project) {
            return Err("请先切回该项目再发送".to_string());
        }
        let pane = match pane {
            Some(pane) => {
                let known = self
                    .tabs_by_project
                    .get(&project)
                    .and_then(ProjectTabs::active)
                    .is_some_and(|tab| tab.layout.panes().iter().any(|id| id.0 == pane));
                if !known {
                    return Err("目标窗格已关闭，请重新选择".to_string());
                }
                pane
            }
            None => {
                self.active_tab(project)
                    .map(|tab| tab.focused)
                    .unwrap_or(self.focused)
                    .0
            }
        };
        let Some(term) = self.workspace.read(cx).terminal_handle(pane) else {
            return Err("当前窗格没有运行中的终端".to_string());
        };
        let shell = self.workspace.read(cx).pane_is_shell(pane);
        if shell == Some(true) {
            term.paste(shell_prompt(text));
        } else {
            term.paste(text);
        }
        if !submits_after_paste(shell) {
            return Ok(SendOutcome::Pasted);
        }
        // A large bracketed paste can keep the TUI busy long enough to swallow
        // an Enter that arrives in the same read, so submit after it settles.
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            term.key(KeyInput {
                key: "enter".into(),
                key_char: Some("\r".into()),
                shift: false,
                control: false,
                alt: false,
            });
        })
        .detach();
        Ok(SendOutcome::Submitted)
    }

    /// Pace live PTY resizes on all panes while a left/right workspace panel
    /// slides open or shut, then flush the exact final column count on settle.
    pub(super) fn begin_panel_resize_defer(&mut self, cx: &mut Context<Self>) {
        self.panel_anim_gen = self.panel_anim_gen.wrapping_add(1);
        let generation = self.panel_anim_gen;
        for terminal in self.panes.values() {
            terminal.update(cx, |term, cx| term.set_panel_animating(true, cx));
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PANEL_SETTLE_FALLBACK).await;
            let _ = this.update(cx, |app, cx| {
                if app.panel_anim_gen == generation {
                    app.finish_panel_resize_defer(cx);
                    if app.open_plugin.is_none() {
                        app.plugin_views.clear();
                        app.native_registry.clear_plugins();
                    }
                }
            });
        })
        .detach();
    }

    /// End animation pacing and flush any remaining 1–2 column delta immediately.
    pub(super) fn finish_panel_resize_defer(&mut self, cx: &mut Context<Self>) {
        for terminal in self.panes.values() {
            terminal.update(cx, |term, cx| term.set_panel_animating(false, cx));
        }
    }

    /// Close the active right-hand plugin panel smoothly: keep its native view
    /// alive until `HomeShellAction::PluginCloseFinished` arrives after the
    /// slide-out animation completes.
    pub(super) fn close_plugin(&mut self, cx: &mut Context<Self>) {
        if self.open_plugin.is_none() {
            return;
        }
        self.begin_panel_resize_defer(cx);
        self.open_plugin = None;
        cx.notify();
    }

    /// Open `spec`'s panel, or close it when it is already the open one. The
    /// panel is built on first open and dropped once its close slide finishes
    /// so its refresh loop stops with it.
    pub(super) fn toggle_plugin(
        &mut self,
        spec: PluginSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_plugin == Some(spec.id) {
            self.close_plugin(cx);
            return;
        }
        let was_closed = self.open_plugin.is_none();
        if was_closed {
            self.begin_panel_resize_defer(cx);
        }
        self.open_plugin = Some(spec.id);
        self.plugin_views
            .retain(|id, _| Some(*id) == self.open_plugin);
        if let Some(existing) = self.plugin_views.get(spec.id).cloned() {
            self.native_registry.set_plugin(spec.id, existing);
        } else {
            let project = self.plugin_project(cx);
            let host = self.plugin_host(cx);
            let view = spec.create(project, host, window, cx);
            self.native_registry.set_plugin(spec.id, view.clone());
            self.plugin_views.insert(spec.id, view);
        }
        self.sync_plugin_project(cx);
        cx.notify();
    }

    /// Tell the open panel which project is selected. The panel ignores a
    /// project it is already showing.
    ///
    /// Deferred on `App` so a plugin callback into the host (dispatching an
    /// agent or revealing a pane) never re-enters `panel.update(...)` while
    /// the plugin entity is already leased on the call stack.
    pub(super) fn sync_plugin_project(&mut self, cx: &mut Context<Self>) {
        if self.open_plugin.is_none() {
            return;
        }
        let home = cx.entity().downgrade();
        (**cx).defer(move |cx| {
            let Some((spec, view, project)) = home
                .read_with(cx, |app, cx| {
                    let id = app.open_plugin?;
                    let view = app.plugin_views.get(id).cloned()?;
                    let spec = crate::plugins::spec(id)?;
                    Some((spec, view, app.plugin_project(cx)))
                })
                .ok()
                .flatten()
            else {
                return;
            };
            spec.set_project(&view, project, cx);
        });
    }

    /// One title-bar entry per registered plugin; the open panel's entry stays
    /// highlighted.
    pub(super) fn render_plugin_button(&self, spec: PluginSpec, cx: &Context<Self>) -> AnyElement {
        let theme = KumuTheme::global(cx);
        let colors = &theme.colors;
        let active = self.open_plugin == Some(spec.id);
        let hovered = self.hovered_plugin_tooltip == Some(spec.id);
        let background = if active {
            colors.accent
        } else {
            colors.transparent
        };
        let hover_bg = colors.accent.opacity(0.5);
        let foreground = if active {
            colors.link
        } else {
            colors.muted_foreground
        };
        let icon = spec.icon;
        let plugin_id = spec.id;
        let plugin_title = spec.title;
        let button = div()
            .id(SharedString::from(format!("plugin-toggle-{}", spec.id)))
            .flex()
            .items_center()
            .justify_center()
            .w(px(24.))
            .h(px(24.))
            .rounded(px(6.))
            .cursor_pointer()
            .bg(background)
            .hover(move |style| style.bg(hover_bg))
            .on_hover(cx.listener(move |this, is_hovered: &bool, _, cx| {
                let next = if *is_hovered {
                    Some(plugin_id)
                } else if this.hovered_plugin_tooltip == Some(plugin_id) {
                    None
                } else {
                    this.hovered_plugin_tooltip
                };
                if this.hovered_plugin_tooltip != next {
                    this.hovered_plugin_tooltip = next;
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.hovered_plugin_tooltip = None;
                this.toggle_plugin(spec.clone(), window, cx);
            }))
            .child(svg().path(icon).size(px(14.)).text_color(foreground));

        let mut wrapper = div().relative().child(button);
        if hovered {
            wrapper = wrapper.child(
                deferred(
                    div()
                        .absolute()
                        .top(px(28.))
                        .right_0()
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
                        .child(plugin_title),
                )
                .with_priority(1),
            );
        }
        wrapper.into_any_element()
    }
}
