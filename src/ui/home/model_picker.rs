//! Composer model picker: agent tabs, search, and model selection.

use std::collections::HashMap;

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use gpui_kit::component::input::Input;
use gpui_kit::component::popover::Popover;

use super::{KumuApp, PickerAxis};
use crate::agent;
use crate::agent::AGENTS;
use crate::app::discovery::AgentSessionOptions;
use crate::app::discovery::SessionOptions;
use crate::db::Session;
use crate::ui::widgets::{PickerChrome, agent_logo, picker_chip, picker_list, select_row};

/// The model chip's label: the value in force — the stored pick, else the
/// agent's own active report — resolved through the catalogue to its name.
///
/// Unlike the other axes this chip never disappears: an agent with no
/// catalogue yet (no probe, no session) still needs a chip to open, so the
/// fallback is the agent's own name rather than [`crate::ui::widgets::inherited_label`]
/// — there is no "this axis does not exist" case for the model picker the way
/// there is for mode/access/sandbox/reasoning.
fn model_label(catalog: Option<&SessionOptions>, stored: Option<&str>, agent_name: &str) -> String {
    let model_value = stored.or(catalog.and_then(|c| c.current_model.as_deref()));
    match model_value {
        Some(value) => catalog
            .and_then(|c| c.models.iter().find(|m| m.value == value))
            .map(|m| m.name.clone())
            .unwrap_or_else(|| value.to_string()),
        None => agent_name.to_string(),
    }
}

impl KumuApp {
    /// Switch the selected session's agent and/or model (see
    /// [`crate::app::Workspace::pick_model`]).
    fn pick_model(&mut self, agent_id: &'static str, model_value: String, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected else {
            return;
        };
        self.workspace.update(cx, |workspace, cx| {
            workspace.pick_model(session_id, agent_id, model_value, cx)
        });
    }

    /// Label for the composer's model picker chip: the selected model's name
    /// (the chip's icon already identifies the agent). Fresh sessions fall
    /// back to the model the agent reports as active, then the agent name.
    fn picker_label(&self, session: &Session, cx: &App) -> String {
        let catalog = cx
            .try_global::<AgentSessionOptions>()
            .and_then(|g| g.0.get(session.agent.as_str()));
        model_label(
            catalog,
            session.model.as_deref(),
            &self.agent_name(&session.agent),
        )
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
                    .and_then(|app| app.read(cx).selected_session(cx))
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
                    let matched: Vec<(&'static str, &agent::ModelInfo)> = AGENTS
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

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use core::prelude::v1::test;

    use super::model_label;
    use crate::agent::ModelInfo;
    use crate::app::discovery::SessionOptions;

    fn model(value: &str, name: &str) -> ModelInfo {
        ModelInfo {
            value: value.into(),
            name: name.into(),
            provider: "test".into(),
        }
    }

    fn catalog(current_model: Option<&str>) -> SessionOptions {
        SessionOptions {
            models: vec![model("gpt-5", "GPT-5"), model("opus", "Opus")],
            current_model: current_model.map(str::to_owned),
            ..Default::default()
        }
    }

    /// The stored pick wins over the agent's active report, and either
    /// resolves through the catalogue to the agent's own name for it.
    #[test]
    fn model_label_prefers_the_stored_pick() {
        let catalog = catalog(Some("gpt-5"));
        assert_eq!(
            model_label(Some(&catalog), Some("opus"), "Claude Code"),
            "Opus"
        );
        assert_eq!(model_label(Some(&catalog), None, "Claude Code"), "GPT-5");
    }

    /// A value the catalogue no longer lists still shows as itself rather than
    /// leaving the chip blank.
    #[test]
    fn model_label_falls_back_to_the_raw_value() {
        let catalog = catalog(None);
        assert_eq!(
            model_label(Some(&catalog), Some("gone"), "Claude Code"),
            "gone"
        );
    }

    /// Unlike the other axes, the model chip never disappears: with nothing in
    /// force yet — no stored pick, no catalogue at all — it falls back to the
    /// agent's own name rather than going blank or hiding.
    #[test]
    fn model_label_falls_back_to_the_agent_name_with_nothing_in_force() {
        assert_eq!(model_label(None, None, "Claude Code"), "Claude Code");
        assert_eq!(
            model_label(Some(&SessionOptions::default()), None, "Claude Code"),
            "Claude Code"
        );
    }
}
