//! Context and dispatch popup menu builders for TODO cards and the detail view.

use futures::channel::mpsc::UnboundedSender;
use nsh_core::element::desktop::MenuItem;

use super::send_action;
use crate::agent::AgentState;
use crate::db::{TodoItem, TodoStatus};
use crate::plugins::AgentPaneInfo;
use crate::plugins::todo::{TodoAction, TodoDispatchTarget, short_agent_name};

/// Arguments for building the right-click context menu of a TODO card or row.
pub(super) struct CardContextMenuArgs<F> {
    pub tx: Option<UnboundedSender<TodoAction>>,
    pub open_detail: F,
    pub item: TodoItem,
    pub jump_pane: Option<u64>,
    pub installed_agents: Vec<(String, String)>,
    pub live_panes: Vec<AgentPaneInfo>,
    pub default_agent: Option<String>,
}

/// Build the shared dispatch/bind menu items for a TODO card or detail view.
pub(super) fn build_dispatch_menu_items(
    tx: Option<UnboundedSender<TodoAction>>,
    item: TodoItem,
    installed_agents: Vec<(String, String)>,
    live_panes: Vec<AgentPaneInfo>,
    default_agent: Option<String>,
) -> Vec<MenuItem> {
    let mut items = Vec::new();
    if installed_agents.is_empty() {
        items.push(MenuItem::new("未检测到已安装的 Agent").disabled(true));
    } else {
        for (agent_id, agent_name) in &installed_agents {
            let is_default = default_agent.as_deref() == Some(agent_id.as_str());
            let label = if is_default {
                format!("分屏启动 {agent_name}（默认）")
            } else {
                format!("分屏启动 {agent_name}")
            };
            let tx = tx.clone();
            let todo_id = item.id;
            let agent_id = agent_id.clone();
            items.push(MenuItem::new(label).on_select(move || {
                send_action(
                    &tx,
                    TodoAction::DispatchNewAgent {
                        id: todo_id,
                        agent_id: agent_id.clone(),
                        target: TodoDispatchTarget::SplitRight,
                    },
                );
            }));
        }
        let sub_items: Vec<MenuItem> = installed_agents
            .iter()
            .map(|(agent_id, agent_name)| {
                let tx = tx.clone();
                let todo_id = item.id;
                let agent_id = agent_id.clone();
                MenuItem::new(agent_name.clone()).on_select(move || {
                    send_action(
                        &tx,
                        TodoAction::DispatchNewAgent {
                            id: todo_id,
                            agent_id: agent_id.clone(),
                            target: TodoDispatchTarget::NewTab,
                        },
                    );
                })
            })
            .collect();
        items.push(MenuItem::submenu("在新页签中启动", sub_items));

        let wt_items: Vec<MenuItem> = installed_agents
            .iter()
            .map(|(agent_id, agent_name)| {
                let tx = tx.clone();
                let todo_id = item.id;
                let agent_id = agent_id.clone();
                MenuItem::new(agent_name.clone()).on_select(move || {
                    send_action(
                        &tx,
                        TodoAction::DispatchNewAgent {
                            id: todo_id,
                            agent_id: agent_id.clone(),
                            target: TodoDispatchTarget::NewWorktree,
                        },
                    );
                })
            })
            .collect();
        items.push(MenuItem::submenu("在新建 Worktree 中启动", wt_items));
    }

    if !live_panes.is_empty() {
        items.push(MenuItem::separator());
        for pane in &live_panes {
            let status = pane.state.map_or("空闲", AgentState::label);
            let label = format!(
                "发送到 窗格 {} · {}（{}）",
                pane.ordinal,
                short_agent_name(&pane.agent),
                status
            );
            let tx = tx.clone();
            let todo_id = item.id;
            let pane_info = pane.clone();
            items.push(MenuItem::new(label).on_select(move || {
                send_action(
                    &tx,
                    TodoAction::BindExistingPane {
                        id: todo_id,
                        pane: pane_info.clone(),
                        send_prompt: true,
                    },
                );
            }));
        }
        let link_items: Vec<MenuItem> = live_panes
            .iter()
            .map(|pane| {
                let status = pane.state.map_or("空闲", AgentState::label);
                let label = format!(
                    "窗格 {} · {}（{}）",
                    pane.ordinal,
                    short_agent_name(&pane.agent),
                    status
                );
                let tx = tx.clone();
                let todo_id = item.id;
                let pane_info = pane.clone();
                MenuItem::new(label).on_select(move || {
                    send_action(
                        &tx,
                        TodoAction::BindExistingPane {
                            id: todo_id,
                            pane: pane_info.clone(),
                            send_prompt: false,
                        },
                    );
                })
            })
            .collect();
        items.push(MenuItem::separator());
        items.push(MenuItem::submenu("仅关联已有窗格（不发送）", link_items));
    }
    items
}

/// Build the context menu items for a TODO card or done row.
pub(super) fn build_card_context_menu_items<F>(args: CardContextMenuArgs<F>) -> Vec<MenuItem>
where
    F: Fn(TodoItem) + Clone + 'static,
{
    let CardContextMenuArgs {
        tx,
        open_detail,
        item,
        jump_pane,
        installed_agents,
        live_panes,
        default_agent,
    } = args;

    let mut items = Vec::new();
    let id = item.id;
    if let Some(pane) = jump_pane {
        let tx = tx.clone();
        items.push(MenuItem::new("跳转到 Agent 窗格").on_select(move || {
            send_action(&tx, TodoAction::RevealPane(pane));
        }));
    }
    {
        let detail_item = item.clone();
        let open_detail = open_detail.clone();
        items.push(MenuItem::new("查看与编辑").on_select(move || {
            open_detail(detail_item.clone());
        }));
    }
    if item.status != TodoStatus::Done {
        let sub = build_dispatch_menu_items(
            tx.clone(),
            item.clone(),
            installed_agents,
            live_panes,
            default_agent,
        );
        items.push(MenuItem::submenu("派发 / 关联 Agent", sub));
    }
    items.push(MenuItem::separator());
    for status in [TodoStatus::Todo, TodoStatus::InProgress, TodoStatus::Done] {
        if item.status == status {
            continue;
        }
        let tx = tx.clone();
        let label = match status {
            TodoStatus::Todo => "移至待办",
            TodoStatus::InProgress => "移至进行中",
            TodoStatus::Done => "标记为已完成",
        };
        items.push(MenuItem::new(label).on_select(move || {
            send_action(&tx, TodoAction::SetStatus { id, status });
        }));
    }
    if item.pane_id.is_some() {
        let tx = tx.clone();
        items.push(MenuItem::new("解除 Agent 关联").on_select(move || {
            send_action(&tx, TodoAction::UnbindAgent(id));
        }));
    }
    items.push(MenuItem::separator());
    let tx_del = tx.clone();
    items.push(
        MenuItem::new("删除任务")
            .destructive(true)
            .on_select(move || {
                send_action(&tx_del, TodoAction::DeleteItem(id));
            }),
    );
    items
}
