//! `nsh-platform-gpui` [`NativeComponent`] bridges that embed live GPUI views
//! inside Kumu's unified Pure Rust NSH home shell:
//!
//! - `"kumu.terminal"` — embeds a live `Entity<TerminalView>` (Ghostty VT GPU
//!   renderer + macOS CJK IME). Deliberately returns `focus_handle() -> None` so
//!   NSH's `AppView::tab_entered` never steals `Tab` / `Shift-Tab` completion.
//! - `"kumu.plugin"` — embeds the active right-hand plugin `AnyView`
//!   (such as `GitLogPanel`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    AnyElement, AnyView, App, Entity, FocusHandle, IntoElement, ParentElement, Styled, Window, div,
};
use nsh_platform_gpui::{AppView, ComponentContext, NativeComponent};
use serde_json::Value as JsonValue;

use super::TerminalView;

pub(super) const TERMINAL_COMPONENT: &str = "kumu.terminal";
pub(super) const PLUGIN_COMPONENT: &str = "kumu.plugin";

/// Shared lookup table owned by `KumuApp` and read by NSH native component
/// instances on the main UI thread.
#[derive(Clone, Default)]
pub(super) struct NativeViewRegistry {
    panes: Rc<RefCell<HashMap<u64, Entity<TerminalView>>>>,
    plugins: Rc<RefCell<HashMap<String, AnyView>>>,
}

impl NativeViewRegistry {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn register_pane(&self, pane_id: u64, view: Entity<TerminalView>) {
        self.panes.borrow_mut().insert(pane_id, view);
    }

    pub(super) fn unregister_pane(&self, pane_id: u64) {
        self.panes.borrow_mut().remove(&pane_id);
    }

    pub(super) fn set_plugin(&self, id: &str, view: AnyView) {
        let mut map = self.plugins.borrow_mut();
        map.clear();
        map.insert(id.to_owned(), view);
    }

    pub(super) fn clear_plugins(&self) {
        self.plugins.borrow_mut().clear();
    }

    pub(super) fn install_on(&self, app_view: &AppView) {
        let panes_reg = self.clone();
        app_view.register_component(
            TERMINAL_COMPONENT,
            Rc::new(move |cx: ComponentContext| {
                Box::new(TerminalNativeComponent {
                    cx,
                    registry: panes_reg.clone(),
                    pane_id: None,
                })
            }),
        );
        let plugins_reg = self.clone();
        app_view.register_component(
            PLUGIN_COMPONENT,
            Rc::new(move |cx: ComponentContext| {
                Box::new(PluginNativeComponent {
                    cx,
                    registry: plugins_reg.clone(),
                    plugin_id: None,
                })
            }),
        );
    }
}

struct TerminalNativeComponent {
    cx: ComponentContext,
    registry: NativeViewRegistry,
    pane_id: Option<u64>,
}

impl NativeComponent for TerminalNativeComponent {
    fn render(&mut self, _window: &mut Window, _cx: &mut App) -> Option<AnyElement> {
        let pane_id = self.pane_id?;
        let view = self.registry.panes.borrow().get(&pane_id).cloned()?;
        Some(
            div()
                .size_full()
                .min_h_0()
                .min_w_0()
                .child(view)
                .into_any_element(),
        )
    }

    fn focus_handle(&self) -> Option<FocusHandle> {
        // Deliberately `None`: `TerminalView` owns its own GPUI `FocusHandle`
        // with `KeyContext("Terminal")` so NSH never intercepts `Tab`/`Shift-Tab`.
        None
    }

    fn set_prop(
        &mut self,
        name: &str,
        value: Option<&JsonValue>,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        if name == "pane" {
            let next = value.and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            });
            if self.pane_id != next {
                self.pane_id = next;
                self.cx.redraw();
            }
        }
    }
}

struct PluginNativeComponent {
    cx: ComponentContext,
    registry: NativeViewRegistry,
    plugin_id: Option<String>,
}

impl NativeComponent for PluginNativeComponent {
    fn render(&mut self, _window: &mut Window, _cx: &mut App) -> Option<AnyElement> {
        let id = self.plugin_id.as_deref()?;
        let view = self.registry.plugins.borrow().get(id).cloned()?;
        Some(
            div()
                .size_full()
                .min_h_0()
                .min_w_0()
                .child(view)
                .into_any_element(),
        )
    }

    fn focus_handle(&self) -> Option<FocusHandle> {
        None
    }

    fn set_prop(
        &mut self,
        name: &str,
        value: Option<&JsonValue>,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        if name == "id" {
            let next = value.and_then(|v| v.as_str()).map(ToOwned::to_owned);
            if self.plugin_id != next {
                self.plugin_id = next;
                self.cx.redraw();
            }
        }
    }
}
