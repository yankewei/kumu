//! Hosts Kumu's existing GPU-accelerated [`TerminalView`] inside an NSH layout
//! tree via `nsh_platform_gpui::NativeComponent` (`native("kumu.terminal")`).
//!
//! Crucially, [`TerminalNativeComponent::focus_handle`] intentionally returns
//! `None` so `nsh-platform-gpui`'s `AppView::tab_entered` hook does not steal
//! `Tab` / `Shift-Tab` completion keys away from the focused terminal.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::{
    AnyElement, App, Entity, FocusHandle, IntoElement, ParentElement, Styled, Window, div,
};
use nsh_platform_gpui::{ComponentContext, NativeComponent};

use crate::ui::home::TerminalView;

/// Main-thread lookup table mapping a Kumu pane id to its GPUI [`TerminalView`].
#[derive(Clone, Default)]
pub struct TerminalRegistry {
    panes: Rc<RefCell<HashMap<u64, Entity<TerminalView>>>>,
}

impl TerminalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, pane_id: u64, view: Entity<TerminalView>) {
        self.panes.borrow_mut().insert(pane_id, view);
    }

    #[allow(dead_code)]
    pub fn remove(&self, pane_id: u64) {
        self.panes.borrow_mut().remove(&pane_id);
    }

    pub fn get(&self, pane_id: u64) -> Option<Entity<TerminalView>> {
        self.panes.borrow().get(&pane_id).cloned()
    }
}

/// Registered native component name used in Pure Rust NSH via
/// `nsh_core::element::native(TERMINAL_COMPONENT_NAME)`.
pub const TERMINAL_COMPONENT_NAME: &str = "kumu.terminal";

pub struct TerminalNativeComponent {
    cx: ComponentContext,
    registry: TerminalRegistry,
    pane_id: u64,
}

impl TerminalNativeComponent {
    pub fn factory(
        registry: TerminalRegistry,
    ) -> Rc<dyn Fn(ComponentContext) -> Box<dyn NativeComponent>> {
        Rc::new(move |cx| {
            Box::new(Self {
                cx,
                registry: registry.clone(),
                pane_id: 0,
            })
        })
    }
}

impl NativeComponent for TerminalNativeComponent {
    fn render(&mut self, _window: &mut Window, _cx: &mut App) -> Option<AnyElement> {
        let Some(terminal) = self.registry.get(self.pane_id) else {
            return Some(div().size_full().into_any_element());
        };
        Some(div().size_full().child(terminal).into_any_element())
    }

    /// Deliberately `None`: terminals handle `Tab` / `Shift-Tab` as shell
    /// completion via `KeyContext("Terminal")` and must stay outside NSH's
    /// Tab focus-traversal chain (`AppView::tab_entered`).
    fn focus_handle(&self) -> Option<FocusHandle> {
        None
    }

    fn set_prop(
        &mut self,
        name: &str,
        value: Option<&serde_json::Value>,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        if name == "pane" {
            self.pane_id = value.and_then(|v| v.as_u64()).unwrap_or(0);
            self.cx.redraw();
        }
    }
}
