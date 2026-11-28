//! Pure Rust NSH 4-step onboarding wizard overlay (`ActiveOnboarding`).
//!
//! State transitions live in [`OnboardingState`] (pure Rust, unit-testable);
//! the NSH view renders state snapshots pushed from `KumuApp` and emits
//! [`OnboardingAction`]s back over a bidirectional channel (same pattern as
//! `SettingsWindow`).

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{AppContext, Context, Entity, Task, Window};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::mount::MountRegistry;
use super::theme::{appearance, nsh_theme};
use crate::agent::AGENTS;
use crate::agent::integration::{ComponentOutcome, IntegrationDetail, McpState, Status};
use crate::theme::KumuTheme;

pub const ONBOARDING_SAMPLE_TITLE: &str = "Kumu 快速上手指南";

pub const ONBOARDING_SAMPLE_MARKDOWN: &str = "\
# 欢迎使用 Kumu

这是 Kumu 的 **Artifacts 阅读页**（本示例直接在内存中渲染，未向你的项目目录写入任何文件）。

当你在 Kumu 窗格里让编码 Agent（如 Claude Code、Codex、OpenCode、Antigravity）产出**方案、设计稿、报告或可交互 HTML 原型**时，Agent 会自动将其发布到项目的 `.kumu/artifacts` 并在右侧 **Artifacts** 抽屉中按会话分组展示。

## 高频快捷键

| 快捷键 | 功能 |
| --- | --- |
| `⌘K` | 打开快捷命令面板（切换 Agent / 项目 / 页签 / 重开引导） |
| `⌘D` / `⌘⇧D` | 向右 / 向下拆分窗格 |
| `⌘⇧Enter` | 最大化 / 还原当前窗格 |
| `⌘,` | 打开设置 |

## 可以立刻试试的提示词

- 「帮我写一份当前项目的架构说明，发布到 artifacts」
- 「在右侧开一个 codex 窗格，让它 review 一下当前的 git diff」
- 「做一个可交互的 HTML 原型，发到 Kumu artifacts 给我看」
";

/// Pure decision function for first-run automatic onboarding display.
pub fn should_auto_open_onboarding(first_run: bool, onboarding_done: Option<&str>) -> bool {
    first_run && onboarding_done != Some("1")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnboardingStep {
    Agents = 0,
    Notifications = 1,
    Workspace = 2,
    Artifacts = 3,
}

impl OnboardingStep {
    pub const ALL: [Self; 4] = [
        Self::Agents,
        Self::Notifications,
        Self::Workspace,
        Self::Artifacts,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Agents => "接入 Agent",
            Self::Notifications => "通知提醒",
            Self::Workspace => "项目与窗格",
            Self::Artifacts => "体验 Artifacts",
        }
    }

    pub fn next(self) -> Option<Self> {
        match self {
            Self::Agents => Some(Self::Notifications),
            Self::Notifications => Some(Self::Workspace),
            Self::Workspace => Some(Self::Artifacts),
            Self::Artifacts => None,
        }
    }

    pub fn prev(self) -> Option<Self> {
        match self {
            Self::Agents => None,
            Self::Notifications => Some(Self::Agents),
            Self::Workspace => Some(Self::Notifications),
            Self::Artifacts => Some(Self::Workspace),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnboardingAgentRow {
    pub id: &'static str,
    pub name: &'static str,
    pub logo: Option<&'static str>,
    pub installed: bool,
    pub detail: IntegrationDetail,
    pub last_feedback: Option<String>,
}

impl OnboardingAgentRow {
    pub fn is_fully_registered(&self) -> bool {
        let notify_ok = matches!(self.detail.notify, Ok(Status::Registered));
        let mcp_ok = matches!(
            self.detail.mcp,
            McpState::ExtensionPartial | McpState::Supported(Ok(Status::Registered))
        );
        let skills_ok = matches!(self.detail.skills, Ok(Status::Registered));
        notify_ok && mcp_ok && skills_ok
    }

    pub fn status_summary(&self) -> String {
        let notify = status_label(&self.detail.notify);
        let mcp = match &self.detail.mcp {
            McpState::Supported(res) => status_label(res),
            McpState::ExtensionPartial => "扩展内置",
        };
        let skills = status_label(&self.detail.skills);
        format!("通知:{notify} · MCP:{mcp} · Skills:{skills}")
    }
}

fn status_label(res: &Result<Status, String>) -> &'static str {
    match res {
        Ok(Status::Registered) => "已接入",
        Ok(Status::NeedsRepair) => "待修复",
        Ok(Status::Missing) => "未接入",
        Err(_) => "异常",
    }
}

/// Pure-Rust state machine for the 4-step onboarding wizard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnboardingState {
    pub step: OnboardingStep,
    pub probing: bool,
    pub busy: bool,
    pub agents: Vec<OnboardingAgentRow>,
    pub notifications_enabled: bool,
    pub notification_sound: bool,
    pub notification_feedback: Option<String>,
    pub project_name: Option<String>,
    pub project_path: Option<String>,
    pub selected_agent: Option<&'static str>,
    pub pane_feedback: Option<Result<String, String>>,
}

impl OnboardingState {
    pub fn new(
        notifications_enabled: bool,
        notification_sound: bool,
        project_name: Option<String>,
        project_path: Option<String>,
    ) -> Self {
        let agents = AGENTS
            .iter()
            .map(|agent| OnboardingAgentRow {
                id: agent.id,
                name: agent.name,
                logo: agent.logo,
                installed: false,
                detail: IntegrationDetail {
                    notify: Ok(Status::Missing),
                    mcp: if crate::agent::integration::supports_mcp(agent.id) {
                        McpState::Supported(Ok(Status::Missing))
                    } else {
                        McpState::ExtensionPartial
                    },
                    skills: Ok(Status::Missing),
                },
                last_feedback: None,
            })
            .collect();
        Self {
            step: OnboardingStep::Agents,
            probing: true,
            busy: false,
            agents,
            notifications_enabled,
            notification_sound,
            notification_feedback: None,
            project_name,
            project_path,
            selected_agent: None,
            pane_feedback: None,
        }
    }

    pub fn installed_agents(&self) -> Vec<&OnboardingAgentRow> {
        self.agents.iter().filter(|a| a.installed).collect()
    }

    pub fn next_step(&mut self) -> bool {
        if let Some(next) = self.step.next() {
            self.step = next;
            true
        } else {
            false
        }
    }

    pub fn prev_step(&mut self) -> bool {
        if let Some(prev) = self.step.prev() {
            self.step = prev;
            true
        } else {
            false
        }
    }

    pub fn apply_probe(&mut self, probed: Vec<(&'static str, bool, IntegrationDetail)>) {
        self.probing = false;
        for (id, installed, detail) in probed {
            if let Some(row) = self.agents.iter_mut().find(|r| r.id == id) {
                row.installed = installed;
                row.detail = detail;
            }
        }
        if self.selected_agent.is_none() {
            self.selected_agent = self.agents.iter().find(|a| a.installed).map(|a| a.id);
        }
    }

    pub fn apply_agent_outcome(
        &mut self,
        id: &'static str,
        outcome: &ComponentOutcome,
        detail: IntegrationDetail,
    ) {
        if let Some(row) = self.agents.iter_mut().find(|r| r.id == id) {
            row.detail = detail;
            row.last_feedback = Some(summarize_outcome(id, outcome));
        }
    }

    pub fn apply_project(&mut self, name: Option<String>, path: Option<String>) {
        self.project_name = name;
        self.project_path = path;
    }
}

pub fn summarize_outcome(agent: &str, outcome: &ComponentOutcome) -> String {
    if outcome.is_ok() {
        if agent == "codex" {
            return "接入完成；重启 Codex 后请在 /hooks 中审查并信任。".into();
        }
        return "接入完成；请重新启动对应 Agent。".into();
    }
    let mut parts = Vec::new();
    if let Err(err) = &outcome.notify {
        parts.push(format!("通知失败({err})"));
    }
    if let Some(Err(err)) = &outcome.mcp {
        parts.push(format!("MCP 失败({err})"));
    }
    if let Err(err) = &outcome.skills {
        parts.push(format!("Skills 失败({err})"));
    }
    format!("部分失败：{}", parts.join("；"))
}

#[derive(Clone, Debug)]
pub enum OnboardingHostEvent {
    State(Box<OnboardingState>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OnboardingAction {
    NextStep,
    PrevStep,
    GoToStep(OnboardingStep),
    InstallAllDetected,
    InstallOne(&'static str),
    SetNotificationsEnabled(bool),
    SetNotificationSound(bool),
    SendTestNotification,
    OpenSystemNotificationSettings,
    PickProject,
    SelectAgent(&'static str),
    OpenAgentPane(&'static str),
    PreviewSampleArtifact,
    Complete,
    SkipForever,
    DismissOnce,
}

struct OnboardingMount {
    initial: OnboardingState,
    events_rx: UnboundedReceiver<OnboardingHostEvent>,
    actions_tx: UnboundedSender<OnboardingAction>,
}

static MOUNTS: MountRegistry<OnboardingMount> = MountRegistry::new();

mod view {
    use futures::StreamExt;
    use futures::channel::mpsc::UnboundedSender;
    use nsh_base::theme as active_theme;
    use nsh_core::element::{AccessibilityRole, scroll};
    use nsh_core::prelude::*;

    use super::{MOUNTS, OnboardingAction, OnboardingHostEvent, OnboardingState, OnboardingStep};
    use crate::nsh_ui::components::{
        ButtonVariant, PillTone, body_text, kumu_button, kumu_switch_bare, meta_text, pill,
        row_title, section_title,
    };

    #[derive(Clone, Debug, PartialEq, Props)]
    pub(super) struct OnboardingRootProps {
        pub(super) mount_id: Prop<u64>,
    }

    fn send(tx: &Option<UnboundedSender<OnboardingAction>>, action: OnboardingAction) {
        if let Some(tx) = tx {
            let _ = tx.unbounded_send(action);
        }
    }

    #[component]
    pub(super) fn OnboardingRoot(props: OnboardingRootProps) -> impl IntoElement {
        let mount = MOUNTS.take(props.mount_id.get_untracked());
        let (initial, events_rx, tx) = match mount {
            Some(m) => (m.initial, Some(m.events_rx), Some(m.actions_tx)),
            None => (OnboardingState::new(true, true, None, None), None, None),
        };

        let state = signal(initial);
        if let Some(mut rx) = events_rx {
            let state_sink = state;
            spawn(async move {
                while let Some(event) = rx.next().await {
                    match event {
                        OnboardingHostEvent::State(next) => state_sink.set(*next),
                    }
                }
            });
        }

        let tx_close = tx.clone();
        let tx_skip = tx.clone();
        let tx_prev = tx.clone();
        let tx_next = tx.clone();
        let tx_steps = tx.clone();
        let tx_body = tx.clone();

        let header = view()
            .flex_row()
            .items_center()
            .justify_between()
            .child(
                view()
                    .flex_col()
                    .gap_0p5()
                    .child(section_title("欢迎使用 Kumu"))
                    .child(meta_text(move || {
                        let s = state.get();
                        format!(
                            "第 {} / {} 步 · {}",
                            s.step.index() + 1,
                            OnboardingStep::ALL.len(),
                            s.step.label()
                        )
                    })),
            )
            .child(kumu_button("关闭", ButtonVariant::Ghost).on_press(move || {
                send(&tx_close, OnboardingAction::DismissOnce);
            }));

        let stepper = view().flex_row().gap_1p5().children(
            OnboardingStep::ALL
                .into_iter()
                .map(move |step| {
                    let tx_step = tx_steps.clone();
                    let label = format!("{}. {}", step.index() + 1, step.label());
                    kumu_button(label, ButtonVariant::Ghost)
                        .flex_1()
                        .bg(move || {
                            if state.get().step == step {
                                active_theme().accent
                            } else {
                                active_theme().muted
                            }
                        })
                        .on_press(move || send(&tx_step, OnboardingAction::GoToStep(step)))
                })
                .collect::<Vec<_>>(),
        );

        let content = scroll()
            .flex_1()
            .min_h(px(260.0))
            .max_h(px(380.0))
            .child(dynamic(move || render_step(state.get(), tx_body.clone())));

        let footer = view()
            .flex_row()
            .items_center()
            .justify_between()
            .pt_2()
            .border_t_1()
            .border_color(move || active_theme().border)
            .child(
                kumu_button("跳过，不再自动显示", ButtonVariant::Ghost).on_press(move || {
                    send(&tx_skip, OnboardingAction::SkipForever);
                }),
            )
            .child(
                view()
                    .flex_row()
                    .gap_2()
                    .child(dynamic(move || {
                        let tx_prev = tx_prev.clone();
                        if state.get().step.prev().is_some() {
                            kumu_button("上一步", ButtonVariant::Outline)
                                .on_press(move || send(&tx_prev, OnboardingAction::PrevStep))
                                .into_element()
                        } else {
                            view().into_element()
                        }
                    }))
                    .child(dynamic(move || {
                        let tx_next = tx_next.clone();
                        let is_last = state.get().step == OnboardingStep::Artifacts;
                        let label = if is_last { "完成引导" } else { "下一步" };
                        kumu_button(label, ButtonVariant::Primary)
                            .on_press(move || {
                                send(
                                    &tx_next,
                                    if is_last {
                                        OnboardingAction::Complete
                                    } else {
                                        OnboardingAction::NextStep
                                    },
                                );
                            })
                            .into_element()
                    })),
            );

        let card = view()
            .flex_col()
            .w(px(560.0))
            .p_5()
            .gap_3()
            .rounded(px(10.0))
            .bg(move || active_theme().popover)
            .border_1()
            .border_color(move || active_theme().border)
            .role(AccessibilityRole::Dialog)
            .child(header)
            .child(stepper)
            .child(content)
            .child(footer);

        view()
            .size_full()
            .flex_col()
            .items_center()
            .justify_center()
            .bg(move || active_theme().foreground.opacity(0.36))
            .child(card)
    }

    fn render_step(
        state: OnboardingState,
        tx: Option<UnboundedSender<OnboardingAction>>,
    ) -> AnyElement {
        match state.step {
            OnboardingStep::Agents => render_agents_step(state, tx),
            OnboardingStep::Notifications => render_notifications_step(state, tx),
            OnboardingStep::Workspace => render_workspace_step(state, tx),
            OnboardingStep::Artifacts => render_artifacts_step(tx),
        }
    }

    fn render_agents_step(
        state: OnboardingState,
        tx: Option<UnboundedSender<OnboardingAction>>,
    ) -> AnyElement {
        let installed: Vec<_> = state
            .agents
            .iter()
            .filter(|a| a.installed)
            .cloned()
            .collect();
        let not_installed_count = state.agents.len().saturating_sub(installed.len());

        let mut col = view()
            .flex_col()
            .gap_2p5()
            .child(body_text(
                "接入后，Kumu 会自动接收 Agent 的完成/等待通知，并注入 MCP 工具与配套 Skills。",
            ))
            .child(meta_text(
                "注意：点击接入会向对应 Agent 的配置文件写入通知钩子、MCP 配置与 Skills 软链。",
            ));

        if state.probing {
            return col
                .child(meta_text("正在检测本机已安装的编码 Agent…"))
                .into_element();
        }

        if installed.is_empty() {
            return col
                .child(
                    view()
                        .flex_col()
                        .gap_1()
                        .p_3()
                        .rounded(px(8.0))
                        .bg(move || active_theme().muted)
                        .child(row_title("暂未检测到支持的编码 Agent CLI"))
                        .child(meta_text(
                            "可先安装 Claude Code、Codex、OpenCode、Cursor CLI、Antigravity CLI 等，稍后随时通过 ⌘K 或 `kumu integration install` 完成接入。",
                        )),
                )
                .into_element();
        }

        let all_done = installed.iter().all(|a| a.is_fully_registered());
        let tx_all = tx.clone();
        col = col.child(
            view()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    kumu_button(
                        if state.busy {
                            "正在接入…"
                        } else if all_done {
                            "已全部接入（可重新检查）"
                        } else {
                            "一键接入全部已安装 Agent"
                        },
                        ButtonVariant::Primary,
                    )
                    .disabled(state.busy)
                    .on_press(move || send(&tx_all, OnboardingAction::InstallAllDetected)),
                )
                .child(meta_text(format!(
                    "已检测到 {} 个 Agent（另有 {} 个未安装）",
                    installed.len(),
                    not_installed_count
                ))),
        );

        for row in installed {
            let tx_one = tx.clone();
            let id = row.id;
            let done = row.is_fully_registered();
            let summary = row.status_summary();
            let feedback = row.last_feedback.clone();
            let mut item = view()
                .flex_col()
                .gap_1()
                .p_2p5()
                .rounded(px(8.0))
                .border_1()
                .border_color(move || active_theme().border)
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            view()
                                .flex_col()
                                .gap_0p5()
                                .child(
                                    view()
                                        .flex_row()
                                        .items_center()
                                        .gap_1p5()
                                        .child(row_title(row.name))
                                        .child(pill(
                                            if done { "就绪" } else { "待接入" },
                                            if done {
                                                PillTone::Accent
                                            } else {
                                                PillTone::Neutral
                                            },
                                        )),
                                )
                                .child(meta_text(summary)),
                        )
                        .child(
                            kumu_button(
                                if done { "重新接入" } else { "接入" },
                                ButtonVariant::Outline,
                            )
                            .disabled(state.busy)
                            .on_press(move || send(&tx_one, OnboardingAction::InstallOne(id))),
                        ),
                );
            if let Some(msg) = feedback {
                item = item.child(meta_text(msg));
            }
            col = col.child(item);
        }

        col.into_element()
    }

    fn render_notifications_step(
        state: OnboardingState,
        tx: Option<UnboundedSender<OnboardingAction>>,
    ) -> AnyElement {
        let tx_enable = tx.clone();
        let tx_sound = tx.clone();
        let tx_test = tx.clone();
        let tx_sys = tx.clone();

        let enabled = state.notifications_enabled;
        let sound = state.notification_sound;

        let mut col = view()
            .flex_col()
            .gap_3()
            .child(body_text(
                "当 Agent 完成任务、等待权限或需要输入时，Kumu 会在后台发送 macOS 系统通知，前台显示轻量提醒。",
            ))
            .child(
                view()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(move || active_theme().border)
                    .child(
                        view()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                view()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(row_title("启用 Agent 通知"))
                                    .child(meta_text("关闭后仅保留侧栏状态，不弹通知")),
                            )
                            .child(kumu_switch_bare(move || enabled).on_change(move |checked: &bool| {
                                send(&tx_enable, OnboardingAction::SetNotificationsEnabled(*checked));
                            })),
                    )
                    .child(
                        view()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                view()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(row_title("播放提示音"))
                                    .child(meta_text("收到需要关注的事件时播放系统提示音")),
                            )
                            .child(kumu_switch_bare(move || sound).on_change(move |checked: &bool| {
                                send(&tx_sound, OnboardingAction::SetNotificationSound(*checked));
                            })),
                    ),
            )
            .child(
                view()
                    .flex_row()
                    .gap_2()
                    .child(
                        kumu_button("发送测试通知", ButtonVariant::Default)
                            .on_press(move || send(&tx_test, OnboardingAction::SendTestNotification)),
                    )
                    .child(
                        kumu_button("打开系统通知设置", ButtonVariant::Outline).on_press(move || {
                            send(&tx_sys, OnboardingAction::OpenSystemNotificationSettings);
                        }),
                    ),
            )
            .child(meta_text(
                "提示：macOS 仅对打包后的 Kumu.app 弹出系统通知权限与横幅（开发模式 cargo run 不会弹窗）。",
            ));

        if let Some(msg) = state.notification_feedback {
            col = col.child(meta_text(msg));
        }

        col.into_element()
    }

    fn render_workspace_step(
        state: OnboardingState,
        tx: Option<UnboundedSender<OnboardingAction>>,
    ) -> AnyElement {
        let tx_pick = tx.clone();
        let tx_open = tx.clone();
        let installed: Vec<_> = state
            .agents
            .iter()
            .filter(|a| a.installed)
            .cloned()
            .collect();
        let selected = state.selected_agent;

        let project_title = state
            .project_name
            .clone()
            .unwrap_or_else(|| "尚未选择项目".into());
        let project_desc = state
            .project_path
            .clone()
            .unwrap_or_else(|| "点击右侧按钮选择一个代码仓库目录".into());

        let mut col = view()
            .flex_col()
            .gap_3()
            .child(body_text(
                "选择一个项目目录，并启动你的第一个编码 Agent 窗格（随时可用 ⌘D 向右拆分多个 Agent 并行协作）。",
            ))
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .p_3()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(move || active_theme().border)
                    .child(
                        view()
                            .flex_col()
                            .gap_0p5()
                            .child(row_title(project_title))
                            .child(meta_text(project_desc)),
                    )
                    .child(
                        kumu_button("选择项目目录", ButtonVariant::Outline)
                            .on_press(move || send(&tx_pick, OnboardingAction::PickProject)),
                    ),
            );

        if installed.is_empty() {
            col = col.child(meta_text(
                "未检测到已安装的 Agent CLI，可先使用默认 Shell 窗格，直接进入下一步。",
            ));
        } else {
            let mut choices = view().flex_row().flex_wrap().gap_1p5();
            for row in &installed {
                let id = row.id;
                let tx_sel = tx.clone();
                let is_selected = selected == Some(id);
                choices = choices.child(
                    kumu_button(
                        row.name,
                        if is_selected {
                            ButtonVariant::Default
                        } else {
                            ButtonVariant::Outline
                        },
                    )
                    .on_press(move || send(&tx_sel, OnboardingAction::SelectAgent(id))),
                );
            }
            col = col
                .child(
                    view()
                        .flex_col()
                        .gap_1p5()
                        .child(row_title("选择要启动的 Agent"))
                        .child(choices),
                )
                .child(
                    kumu_button("在当前项目打开 Agent 窗格", ButtonVariant::Default)
                        .disabled(selected.is_none())
                        .on_press(move || {
                            if let Some(id) = selected {
                                send(&tx_open, OnboardingAction::OpenAgentPane(id));
                            }
                        }),
                );
        }

        if let Some(feedback) = state.pane_feedback {
            let text = match feedback {
                Ok(msg) => msg,
                Err(err) => format!("创建失败：{err}"),
            };
            col = col.child(meta_text(text));
        }

        col.into_element()
    }

    fn render_artifacts_step(tx: Option<UnboundedSender<OnboardingAction>>) -> AnyElement {
        let tx_preview = tx.clone();
        view()
            .flex_col()
            .gap_3()
            .child(body_text(
                "当 Agent 输出方案、评审报告或可交互 HTML 原型时，Kumu 会在右侧 Artifacts 抽屉按会话归档，并用原生阅读页与内嵌 WebView 渲染。",
            ))
            .child(
                view()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(move || active_theme().border)
                    .child(row_title("内置示例：Kumu 快速上手指南"))
                    .child(meta_text(
                        "纯内存预览，不会向你的项目写入任何文件；预览页关闭后可随时继续使用 Kumu。",
                    ))
                    .child(
                        kumu_button("预览示例 Artifact", ButtonVariant::Default).on_press(
                            move || send(&tx_preview, OnboardingAction::PreviewSampleArtifact),
                        ),
                    ),
            )
            .child(meta_text(
                "常用快捷键：⌘K 命令面板 · ⌘D 拆分窗格 · ⌘⇧Enter 最大化窗格 · ⌘, 设置。点击「完成引导」后不再自动弹出。",
            ))
            .into_element()
    }
}

/// Live NSH onboarding wizard overlay owned by `KumuApp`.
pub struct ActiveOnboarding {
    view: Entity<AppView>,
    events_tx: UnboundedSender<OnboardingHostEvent>,
    _task: Task<()>,
}

impl ActiveOnboarding {
    pub fn open<T: 'static>(
        initial: OnboardingState,
        theme: &KumuTheme,
        window: &mut Window,
        cx: &mut Context<T>,
        on_action: impl Fn(&mut T, OnboardingAction, &mut Window, &mut Context<T>) + 'static,
    ) -> Self {
        let (events_tx, events_rx) = unbounded::<OnboardingHostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<OnboardingAction>();
        let mount_id = MOUNTS.register(OnboardingMount {
            initial,
            events_rx,
            actions_tx,
        });

        let mapped = nsh_theme(theme);
        let mode = appearance(theme.mode);
        let view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu 新手引导".to_owned(),
                transparent: true,
                ..Default::default()
            };
            let app_view = AppView::new(options, window, cx);
            app_view.set_theme(Some(mapped), cx);
            app_view.set_appearance(mode, cx);
            app_view.load(nsh_platform_gpui::App::Rust(Root::new::<
                view::OnboardingRoot,
            >(move || {
                view::OnboardingRootProps {
                    mount_id: mount_id.into(),
                }
            })));
            app_view
        });

        let task = cx.spawn_in(window, async move |this, cx| {
            while let Some(action) = actions_rx.next().await {
                let res = this.update_in(cx, |host, window, cx| {
                    on_action(host, action, window, cx);
                });
                if res.is_err() {
                    break;
                }
            }
        });

        Self {
            view,
            events_tx,
            _task: task,
        }
    }

    pub fn push_state(&self, state: OnboardingState) {
        let _ = self
            .events_tx
            .unbounded_send(OnboardingHostEvent::State(Box::new(state)));
    }

    pub fn sync_theme(&self, theme: &KumuTheme, cx: &mut gpui::App) {
        let mapped = nsh_theme(theme);
        let mode = appearance(theme.mode);
        self.view.update(cx, |v, cx| {
            v.set_theme(Some(mapped), cx);
            v.set_appearance(mode, cx);
        });
    }

    pub fn view(&self) -> &Entity<AppView> {
        &self.view
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_open_triggers_only_on_first_run_when_not_done() {
        assert!(should_auto_open_onboarding(true, None));
        assert!(should_auto_open_onboarding(true, Some("0")));
        assert!(!should_auto_open_onboarding(true, Some("1")));
        assert!(!should_auto_open_onboarding(false, None));
        assert!(!should_auto_open_onboarding(false, Some("1")));
    }

    #[test]
    fn onboarding_state_steps_probe_and_partial_failure_summary() {
        let mut state = OnboardingState::new(true, true, None, None);
        assert_eq!(state.step, OnboardingStep::Agents);
        assert!(!state.prev_step());
        assert!(state.next_step());
        assert_eq!(state.step, OnboardingStep::Notifications);
        assert!(state.next_step());
        assert!(state.next_step());
        assert_eq!(state.step, OnboardingStep::Artifacts);
        assert!(!state.next_step());

        state.apply_probe(vec![(
            "codex",
            true,
            IntegrationDetail {
                notify: Ok(Status::Missing),
                mcp: McpState::Supported(Ok(Status::Missing)),
                skills: Ok(Status::Missing),
            },
        )]);
        assert!(!state.probing);
        assert_eq!(state.installed_agents().len(), 1);
        assert_eq!(state.selected_agent, Some("codex"));

        let partial = ComponentOutcome {
            notify: Ok(()),
            mcp: Some(Ok(())),
            skills: Err("occupied".into()),
        };
        state.apply_agent_outcome(
            "codex",
            &partial,
            IntegrationDetail {
                notify: Ok(Status::Registered),
                mcp: McpState::Supported(Ok(Status::Registered)),
                skills: Err("occupied".into()),
            },
        );
        let codex = state.agents.iter().find(|a| a.id == "codex").unwrap();
        assert!(
            codex
                .last_feedback
                .as_deref()
                .unwrap_or("")
                .contains("Skills 失败(occupied)")
        );
    }
}
