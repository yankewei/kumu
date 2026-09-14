//! Composer model picker: agent tabs, search, and model selection.

use std::collections::HashMap;

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use gpui_kit::component::input::Input;
use gpui_kit::component::popover::Popover;

use super::{KumuApp, PickerAxis};
use crate::agents::AGENTS;
use crate::backend;
use crate::db::Session;
use crate::ui::AgentSessionOptions;
use crate::ui::widgets::{PickerChrome, agent_logo, picker_chip, picker_list, select_row};

impl KumuApp {
    /// Switch a session's agent and/or model: persist the choice, drop the old
    /// agent's connection when the agent changes, then apply the model over the
    /// session's model config option. A session with no live connection just
    /// stores the pick, which is re-applied when one is opened.
    ///
    /// The agent half is only available while the session has no conversation —
    /// see the guard below.
    fn pick_model(&mut self, agent_id: &'static str, model_value: String, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected else {
            return;
        };
        let Some(session) = self.selected_session() else {
            return;
        };
        let agent_changed = session.agent != agent_id;
        // A session keeps its agent once it has a conversation: the transcript
        // was written by that agent, and a different one would be handed
        // history it never produced (the transcript is persisted, but the
        // agent's own context is not, so the switch is a silent amnesia even
        // for the same agent's later process). The picker does not offer the
        // switch in this state — this is the backstop that keeps the rule true
        // for anything else that reaches here.
        if agent_changed && session.has_conversation {
            return;
        }
        self.store
            .update_session_agent_model(session_id, agent_id, Some(&model_value))
            .ok();
        if agent_changed {
            // Mode ids, sandbox values and effort values are the agent's own
            // vocabulary, so the stored ones stop meaning anything: drop them,
            // and the pickers fall back to the new agent's defaults until the
            // user chooses again. Written before the refresh below so the
            // re-applied values come from the fresh row.
            //
            // A model change *within* an agent is deliberately not cleared here:
            // whether a pick still applies is the backend's call — Codex drops
            // one the new model does not advertise and restores it when a later
            // model does, and an ACP agent's level does not depend on the model
            // at all.
            self.store.update_session_mode(session_id, None).ok();
            self.store.update_session_sandbox(session_id, None).ok();
            self.store.update_session_reasoning(session_id, None).ok();
            // Dropping the handle closes the old agent subprocess; the fresh
            // connection below re-applies the stored model on session/new.
            // Any prompt still waiting on the old connection is settled first,
            // and the in-flight reply (if any) is abandoned with it.
            self.cancel_pending_permissions(session_id);
            self.conns.remove(&session_id);
            self.turns.remove(&session_id);
        }
        self.refresh();
        // Opening a connection here just to deliver the value would start an
        // agent subprocess the user never prompted — and for an agent that
        // cannot run a session, that is an error written into a conversation
        // that has not begun.
        if let Some(conn) = self.conns.get(&session_id) {
            conn.send(backend::Cmd::SetModel(model_value));
        }
        cx.notify();
    }

    /// Label for the composer's model picker chip: the selected model's name
    /// (the chip's icon already identifies the agent). Fresh sessions fall
    /// back to the model the agent reports as active, then the agent name.
    fn picker_label(&self, session: &Session, cx: &App) -> String {
        let catalog = cx
            .try_global::<AgentSessionOptions>()
            .and_then(|g| g.0.get(session.agent.as_str()));
        let model_value = session
            .model
            .as_deref()
            .or(catalog.and_then(|c| c.current_model.as_deref()));
        match model_value {
            Some(value) => catalog
                .and_then(|c| c.models.iter().find(|m| m.value == value))
                .map(|m| m.name.clone())
                .unwrap_or_else(|| value.to_string()),
            None => self.agent_name(&session.agent),
        }
    }

    /// The composer's model picker, Waku-style: a chip trigger opening a
    /// controlled panel with an agent rail on the left and a searchable model
    /// list on the right. Picking a model switches the session's agent with it.
    pub(super) fn model_picker(&self, session: &Session, cx: &Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let label = self.picker_label(session, cx);
        let chip_logo = AGENTS
            .iter()
            .find(|a| a.id == session.agent)
            .and_then(|a| a.logo);
        let chrome = PickerChrome::new(cx.theme());
        let search = self.model_search.clone();
        let open = self.open_picker == Some(PickerAxis::Model);

        let chip = picker_chip(
            "model-picker-chip",
            &chrome,
            label,
            Some(agent_logo(chip_logo, px(13.), chrome.muted_fg)),
            open,
        );

        Popover::new("model-picker")
            .appearance(false)
            .anchor(Anchor::BottomLeft)
            .open(open)
            .on_open_change({
                let weak = weak.clone();
                let search = search.clone();
                move |open, window, cx| {
                    let open = *open;
                    let _ = weak.update(cx, |app, cx| {
                        app.set_picker_open(PickerAxis::Model, open);
                        if open {
                            // Waku resets onto the session's agent and clears
                            // the filter each time the panel opens.
                            app.model_picker_tab = None;
                            search.update(cx, |state, cx| state.set_value("", window, cx));
                        }
                        cx.notify();
                    });
                }
            })
            .trigger(chip)
            .content(move |_state, _window, cx| {
                let cache = if cx.has_global::<AgentSessionOptions>() {
                    cx.global::<AgentSessionOptions>().0.clone()
                } else {
                    HashMap::new()
                };
                let (cur_agent, cur_model, has_conversation) = weak
                    .upgrade()
                    .and_then(|app| app.read(cx).selected_session().cloned())
                    .map(|s| (s.agent, s.model, s.has_conversation))
                    .unwrap_or_default();
                let tab = weak.upgrade().and_then(|app| app.read(cx).model_picker_tab);
                let query = search.read(cx).value().trim().to_lowercase();
                let searching = !query.is_empty();
                // A session with a conversation keeps its agent, so nothing may
                // browse away from it — not a stale tab, not a search hit.
                let tab = tab.filter(|_| !has_conversation);
                // The rail highlights the active tab; with no tab picked it
                // follows the session's current agent.
                let active_agent = tab.unwrap_or_else(|| cur_agent.as_str());

                // ---- Left rail: one logo tab per agent (Waku-style). ----
                let mut rail = v_flex()
                    .w_12()
                    .h_full()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .py_2()
                    .bg(chrome.muted.opacity(0.4))
                    .border_r_1()
                    .border_color(chrome.border);
                for a in AGENTS.iter() {
                    let selected = active_agent == a.id;
                    let tab_weak = weak.clone();
                    rail = rail.child(
                        div()
                            .id(format!("model-tab-{}", a.id))
                            .size(px(34.))
                            .rounded(chrome.radius)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .when(selected, |el| el.bg(chrome.accent))
                            .when(!selected, |el| el.hover(move |s| s.bg(chrome.muted)))
                            .child(agent_logo(
                                a.logo,
                                px(18.),
                                if selected {
                                    chrome.accent_fg
                                } else {
                                    chrome.muted_fg
                                },
                            ))
                            .on_click(move |_, _, cx| {
                                let _ = tab_weak.update(cx, |app, cx| {
                                    app.model_picker_tab = Some(a.id);
                                    cx.notify();
                                });
                            }),
                    );
                }

                // ---- Right: search field + model rows. ----
                let mut list = picker_list();
                if searching {
                    let matched: Vec<(&'static str, &backend::ModelInfo)> = AGENTS
                        .iter()
                        // A locked session only searches its own agent, so a hit
                        // from another one cannot offer an impossible switch.
                        .filter(|a| !has_conversation || a.id == cur_agent.as_str())
                        .flat_map(|a| {
                            cache
                                .get(a.id)
                                .map(|c| c.models.as_slice())
                                .into_iter()
                                .flatten()
                                .filter(|m| {
                                    m.name.to_lowercase().contains(&query)
                                        || m.value.to_lowercase().contains(&query)
                                })
                                .map(|m| (a.id, m))
                        })
                        .collect();
                    if matched.is_empty() {
                        list = list.child(empty_list_state("没有匹配的模型", chrome.muted_fg));
                    }
                    for (agent_id, model) in matched {
                        let agent_name = AGENTS
                            .iter()
                            .find(|a| a.id == agent_id)
                            .map(|a| a.name)
                            .unwrap_or("?");
                        list = list.child(select_row(
                            &chrome,
                            &model.name,
                            Some(format!("{agent_name} · {}", model.provider)),
                            agent_id == cur_agent
                                && cur_model.as_deref() == Some(model.value.as_str()),
                            {
                                let weak = weak.clone();
                                let value = model.value.clone();
                                move |_, _, cx| {
                                    let _ = weak.update(cx, |app, cx| {
                                        app.pick_model(agent_id, value.clone(), cx);
                                        app.open_picker = None;
                                        cx.notify();
                                    });
                                }
                            },
                        ));
                    }
                } else {
                    let list_agent =
                        tab.or_else(|| AGENTS.iter().find(|a| a.id == cur_agent).map(|a| a.id));
                    match list_agent.and_then(|id| cache.get(id)) {
                        Some(catalog) if !catalog.models.is_empty() => {
                            let agent_id = list_agent.unwrap();
                            for model in catalog.models.iter() {
                                list = list.child(select_row(
                                    &chrome,
                                    &model.name,
                                    Some(model.provider.clone()),
                                    agent_id == cur_agent
                                        && cur_model.as_deref() == Some(model.value.as_str()),
                                    {
                                        let weak = weak.clone();
                                        let value = model.value.clone();
                                        move |_, _, cx| {
                                            let _ = weak.update(cx, |app, cx| {
                                                app.pick_model(agent_id, value.clone(), cx);
                                                app.open_picker = None;
                                                cx.notify();
                                            });
                                        }
                                    },
                                ));
                            }
                        }
                        Some(_) => {
                            list = list.child(empty_list_state("无可用模型", chrome.muted_fg));
                        }
                        None => {
                            list = list.child(empty_list_state("检测模型…", chrome.muted_fg));
                        }
                    }
                }

                h_flex()
                    .w(px(460.))
                    .h(px(340.))
                    .rounded(chrome.radius)
                    .border_1()
                    .border_color(chrome.border)
                    .bg(chrome.background)
                    .shadow_lg()
                    .overflow_hidden()
                    // The rail exists only to choose an agent. Once the session
                    // has a conversation its agent is fixed, so the chooser goes
                    // away rather than being shown with every choice disabled —
                    // a control that cannot act is worse than no control.
                    .when(!has_conversation, |el| el.child(rail))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(
                                div().p_2().border_b_1().border_color(chrome.border).child(
                                    Input::new(&search).small().prefix(
                                        Icon::new(IconName::Search)
                                            .small()
                                            .text_color(chrome.muted_fg),
                                    ),
                                ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .id("model-picker-list")
                                    .overflow_y_scroll()
                                    .child(list),
                            ),
                    )
            })
    }
}

/// Centered placeholder text for an empty model list.
fn empty_list_state(label: &'static str, muted_fg: Hsla) -> Div {
    div()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(muted_fg)
        .child(label)
}
