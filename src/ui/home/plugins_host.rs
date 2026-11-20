use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::panes::ProjectTabs;
use super::{KumuApp, agent_display_name, shell_prompt, submits_after_paste};
use crate::agent::launch::PaneLaunch;
use crate::plugins::{PaneTarget, PluginHost, PluginProject, PluginSpec, SendOutcome};
use crate::term::KeyInput;

/// The plugin panel's opening width and the range its divider allows.
pub(super) const PLUGIN_WIDTH: f32 = 280.;
pub(super) const PLUGIN_MIN_WIDTH: f32 = 220.;
pub(super) const PLUGIN_MAX_WIDTH: f32 = 480.;

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
    }

    /// The panes a plugin may hand text to, in layout order. The header's label
    /// gives way to the agent's name only when no program has set one yet.
    pub(super) fn plugin_panes(&self, project: i64, cx: &App) -> Vec<PaneTarget> {
        let Some(tabs) = self.tabs_by_project.get(&project) else {
            return Vec::new();
        };
        let Some(tab) = tabs.active() else {
            return Vec::new();
        };
        let focused = tab.focused;
        let workspace = self.workspace.read(cx);
        tab.layout
            .panes()
            .into_iter()
            .enumerate()
            .map(|(index, pane)| {
                let agent = match self.launches.get(&pane) {
                    Some(PaneLaunch::Agent { agent, .. }) => Some(agent.clone()),
                    _ => None,
                };
                let title = workspace
                    .terminal_handle(pane.0)
                    .map(|term| term.label())
                    .filter(|label| !label.is_empty())
                    .or_else(|| agent.as_deref().and_then(agent_display_name))
                    .unwrap_or_else(|| "终端".to_string());
                PaneTarget {
                    pane: pane.0,
                    ordinal: index + 1,
                    title,
                    agent,
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

    /// Open `spec`'s panel, or close it when it is already the open one. The
    /// panel is built on first open and dropped on close so its refresh loop
    /// stops with it.
    pub(super) fn toggle_plugin(
        &mut self,
        spec: PluginSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_plugin == Some(spec.id) {
            self.open_plugin = None;
            self.plugin_views.clear();
        } else {
            self.open_plugin = Some(spec.id);
            self.plugin_views
                .retain(|id, _| Some(*id) == self.open_plugin);
            if !self.plugin_views.contains_key(spec.id) {
                let project = self.plugin_project(cx);
                let host = self.plugin_host(cx);
                let view = spec.create(project, host, window, cx);
                self.plugin_views.insert(spec.id, view);
            }
        }
        self.sync_plugin_project(cx);
        cx.notify();
    }

    /// Tell the open panel which project is selected. The panel ignores a
    /// project it is already showing.
    pub(super) fn sync_plugin_project(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.open_plugin else {
            return;
        };
        let Some(view) = self.plugin_views.get(id).cloned() else {
            return;
        };
        let Some(spec) = crate::plugins::spec(id) else {
            return;
        };
        let project = self.plugin_project(cx);
        spec.set_project(&view, project, cx);
    }

    /// One title-bar entry per registered plugin; the open panel's entry stays
    /// highlighted.
    pub(super) fn render_plugin_button(&self, spec: PluginSpec, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let active = self.open_plugin == Some(spec.id);
        let background = if active {
            theme.accent
        } else {
            theme.transparent
        };
        let foreground = if active {
            theme.link
        } else {
            theme.muted_foreground
        };
        Button::new(format!("plugin-toggle-{}", spec.id))
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(background)
                    .foreground(foreground)
                    .hover(theme.accent.opacity(0.5)),
            )
            .compact()
            .icon(Icon::empty().path(spec.icon))
            .tooltip(spec.title)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle_plugin(spec.clone(), window, cx);
            }))
            .into_any_element()
    }

    /// The open plugin panel: host chrome (title and close) above the plugin's
    /// own content.
    pub(super) fn render_plugin_panel(&self, cx: &Context<Self>) -> AnyElement {
        let Some(id) = self.open_plugin else {
            return div().into_any_element();
        };
        let Some(spec) = crate::plugins::spec(id) else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.sidebar_border);
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(border)
                    .child(Icon::empty().path(spec.icon).small().text_color(muted))
                    .child(div().text_xs().font_medium().child(spec.title))
                    .child(div().flex_1())
                    .child(
                        Button::new(format!("plugin-close-{}", spec.id))
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .color(theme.transparent)
                                    .foreground(muted)
                                    .hover(theme.accent.opacity(0.5)),
                            )
                            .compact()
                            .icon(IconName::Close)
                            .tooltip("收起面板")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_plugin = None;
                                this.plugin_views.clear();
                                cx.notify();
                            })),
                    ),
            )
            .children(self.plugin_views.get(id).cloned().map(|view| {
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(view.cached(StyleRefinement::default().size_full().min_h_0().min_w_0()))
            }))
            .into_any_element()
    }
}
