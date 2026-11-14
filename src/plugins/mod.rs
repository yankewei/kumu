//! Right-hand plugin panels.
//!
//! A plugin is a panel the home window opens from the title bar. The registry
//! below is the single place that lists them, so adding a panel means
//! implementing [`Plugin`] and adding one entry to [`specs`]. The host draws
//! the panel's title bar and close control; the plugin draws its content.
//!
//! Plugins own their refresh loop: the home window only tells a panel which
//! project is selected. A panel that polls a repository can keep itself
//! current without the workspace knowing anything about it.

mod git_log;

use std::path::PathBuf;

use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::db::Project;

/// The project a plugin panel is showing.
#[derive(Clone, PartialEq, Eq)]
pub struct PluginProject {
    pub id: i64,
    pub path: PathBuf,
}

impl From<&Project> for PluginProject {
    fn from(project: &Project) -> Self {
        Self {
            id: project.id,
            path: PathBuf::from(&project.path),
        }
    }
}

/// What a plugin panel implements to join the registry.
///
/// `new` builds the panel for the selected project (or none) and starts
/// whatever work it needs. `set_project` follows the selection; a panel whose
/// project did not change can ignore the call.
pub trait Plugin: Render + 'static {
    const ID: &'static str;
    const TITLE: &'static str;
    /// Asset path for the title-bar button and the panel header.
    const ICON: &'static str;

    fn new(project: Option<PluginProject>, window: &mut Window, cx: &mut Context<Self>) -> Self;

    fn set_project(&mut self, project: Option<PluginProject>, cx: &mut Context<Self>);
}

/// One registered plugin. `Plugin` is generic over its own context, so the
/// registry erases it to a constructor and a project setter.
#[derive(Clone)]
pub struct PluginSpec {
    pub id: &'static str,
    pub title: &'static str,
    pub icon: &'static str,
    create: fn(Option<PluginProject>, &mut Window, &mut App) -> AnyView,
    set_project: fn(&AnyView, Option<PluginProject>, &mut App),
}

impl PluginSpec {
    fn of<P: Plugin>() -> Self {
        Self {
            id: P::ID,
            title: P::TITLE,
            icon: P::ICON,
            create: |project, window, cx| cx.new(|cx| P::new(project, window, cx)).into(),
            set_project: |view, project, cx| {
                let Ok(panel) = view.clone().downcast::<P>() else {
                    return;
                };
                panel.update(cx, |panel, cx| panel.set_project(project, cx));
            },
        }
    }

    pub fn create(
        &self,
        project: Option<PluginProject>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        (self.create)(project, window, cx)
    }

    pub fn set_project(&self, view: &AnyView, project: Option<PluginProject>, cx: &mut App) {
        (self.set_project)(view, project, cx)
    }
}

/// Every plugin, in title-bar order.
pub fn specs() -> impl Iterator<Item = PluginSpec> {
    [PluginSpec::of::<git_log::GitLogPanel>()].into_iter()
}

pub fn spec(id: &str) -> Option<PluginSpec> {
    specs().find(|spec| spec.id == id)
}
