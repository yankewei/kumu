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
mod todo;

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::agent::AgentState;
use crate::app::Workspace;
use crate::db::Project;

/// What happened to text a panel handed to a terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// The pane runs an agent, so the text was pasted and submitted.
    Submitted,
    /// The pane is a shell, or nothing could say otherwise, so the text waits
    /// on the command line for the user's Enter.
    Pasted,
}

/// One pane a panel can hand text to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneTarget {
    pub pane: u64,
    /// 1-based position in the visible tab's layout, so two panes running the
    /// same agent stay distinguishable in a menu.
    pub ordinal: usize,
    /// What the pane header calls the pane.
    pub title: String,
    /// The registry id when the pane was started as an agent.
    pub agent: Option<String>,
    /// The pane the project had focused when the panel asked.
    pub focused: bool,
}

impl PaneTarget {
    /// How the target reads in a button or a record: `窗格 2 · codex`.
    pub fn label(&self) -> String {
        format!("窗格 {} · {}", self.ordinal, self.title)
    }
}

/// One live agent pane across a repository and its worktrees, with real-time
/// status from the home window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPaneInfo {
    pub pane: u64,
    pub project_id: i64,
    pub project_name: String,
    pub tab_name: String,
    pub ordinal: usize,
    pub agent: String,
    pub title: String,
    pub state: Option<AgentState>,
    pub focused: bool,
}

/// How a TODO card starts a fresh Agent pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchMode {
    /// Split beside the active pane in the selected project's current tab.
    SplitRight,
    /// Open a new tab named after the task and start the agent inside it.
    NewTab { tab_name: String },
}

/// The window's send function, erased so a panel holds no window type. `None`
/// means the focused pane, which is what a panel without a target list uses.
type SendFn = dyn Fn(i64, Option<u64>, &str, &mut App) -> Result<SendOutcome, String>;

/// The window's pane listing, erased the same way.
type PanesFn = dyn Fn(i64, &App) -> Vec<PaneTarget>;

type AgentPanesFn = dyn Fn(i64, &App) -> Vec<AgentPaneInfo>;
type DispatchFn = dyn Fn(i64, &str, DispatchMode, String, &mut App) -> Result<u64, String>;
type SendAnyPaneFn = dyn Fn(u64, &str, &mut App) -> Result<SendOutcome, String>;
type RevealPaneFn = dyn Fn(u64, &mut Window, &mut App);

/// The panes a panel may hand text to, as the host sees them.
///
/// A panel shows facts about a project; when the user wants the agent working
/// in that project to act on them, the panel hands the text here and the window
/// routes it to the chosen pane. The closures keep plugins free of the window
/// type.
#[derive(Clone)]
pub struct PluginHost {
    send: Rc<SendFn>,
    panes: Rc<PanesFn>,
    workspace: Option<Entity<Workspace>>,
    agent_panes: Option<Rc<AgentPanesFn>>,
    dispatch: Option<Rc<DispatchFn>>,
    send_any: Option<Rc<SendAnyPaneFn>>,
    reveal: Option<Rc<RevealPaneFn>>,
}

impl PluginHost {
    /// A host that can also name every pane in a project.
    pub fn with_panes(
        send: impl Fn(i64, Option<u64>, &str, &mut App) -> Result<SendOutcome, String> + 'static,
        panes: impl Fn(i64, &App) -> Vec<PaneTarget> + 'static,
    ) -> Self {
        Self {
            send: Rc::new(send),
            panes: Rc::new(panes),
            workspace: None,
            agent_panes: None,
            dispatch: None,
            send_any: None,
            reveal: None,
        }
    }

    /// Attach workspace access and cross-tab agent dispatch/reveal capabilities
    /// used by task-oriented panels such as the TODO board.
    pub fn with_agent_tools(
        mut self,
        workspace: Entity<Workspace>,
        agent_panes: impl Fn(i64, &App) -> Vec<AgentPaneInfo> + 'static,
        dispatch: impl Fn(i64, &str, DispatchMode, String, &mut App) -> Result<u64, String> + 'static,
        send_any: impl Fn(u64, &str, &mut App) -> Result<SendOutcome, String> + 'static,
        reveal: impl Fn(u64, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.workspace = Some(workspace);
        self.agent_panes = Some(Rc::new(agent_panes));
        self.dispatch = Some(Rc::new(dispatch));
        self.send_any = Some(Rc::new(send_any));
        self.reveal = Some(Rc::new(reveal));
        self
    }

    pub fn workspace(&self) -> Option<Entity<Workspace>> {
        self.workspace.clone()
    }

    /// Write `text` into the focused pane of `project`.
    pub fn send_to_focused_pane(
        &self,
        project: i64,
        text: &str,
        cx: &mut App,
    ) -> Result<SendOutcome, String> {
        (self.send)(project, None, text, cx)
    }

    /// Write `text` into one pane of `project`.
    pub fn send_to_pane(
        &self,
        project: i64,
        pane: u64,
        text: &str,
        cx: &mut App,
    ) -> Result<SendOutcome, String> {
        (self.send)(project, Some(pane), text, cx)
    }

    /// Write `text` into any live pane (across tabs or worktrees).
    pub fn send_to_any_pane(
        &self,
        pane: u64,
        text: &str,
        cx: &mut App,
    ) -> Result<SendOutcome, String> {
        let Some(send_any) = &self.send_any else {
            return Err("当前宿主不支持跨页签发送".to_string());
        };
        (send_any)(pane, text, cx)
    }

    /// The panes `project` shows, in layout order. Empty when the host cannot
    /// list them, which leaves a panel on the focused pane alone.
    pub fn panes(&self, project: i64, cx: &App) -> Vec<PaneTarget> {
        (self.panes)(project, cx)
    }

    /// Live agent panes belonging to `board_id` and its worktrees, across all tabs.
    pub fn agent_panes(&self, board_id: i64, cx: &App) -> Vec<AgentPaneInfo> {
        self.agent_panes
            .as_ref()
            .map(|list| (list)(board_id, cx))
            .unwrap_or_default()
    }

    /// Launch a new agent pane in `project_id`, wait for its prompt, and submit `prompt`.
    pub fn dispatch_agent(
        &self,
        project_id: i64,
        agent_id: &str,
        mode: DispatchMode,
        prompt: String,
        cx: &mut App,
    ) -> Result<u64, String> {
        let Some(dispatch) = &self.dispatch else {
            return Err("当前宿主不支持派发 Agent".to_string());
        };
        (dispatch)(project_id, agent_id, mode, prompt, cx)
    }

    /// Switch to the project/tab owning `pane` and focus its terminal.
    pub fn reveal_pane(&self, pane: u64, window: &mut Window, cx: &mut App) {
        if let Some(reveal) = &self.reveal {
            (reveal)(pane, window, cx);
        }
    }
}

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

    fn new(
        project: Option<PluginProject>,
        host: PluginHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self;

    fn set_project(&mut self, project: Option<PluginProject>, cx: &mut Context<Self>);
}

/// One registered plugin. `Plugin` is generic over its own context, so the
/// registry erases it to a constructor and a project setter.
#[derive(Clone)]
pub struct PluginSpec {
    pub id: &'static str,
    pub title: &'static str,
    pub icon: &'static str,
    create: fn(Option<PluginProject>, PluginHost, &mut Window, &mut App) -> AnyView,
    set_project: fn(&AnyView, Option<PluginProject>, &mut App),
}

impl PluginSpec {
    fn of<P: Plugin>() -> Self {
        Self {
            id: P::ID,
            title: P::TITLE,
            icon: P::ICON,
            create: |project, host, window, cx| {
                cx.new(|cx| P::new(project, host, window, cx)).into()
            },
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
        host: PluginHost,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        (self.create)(project, host, window, cx)
    }

    pub fn set_project(&self, view: &AnyView, project: Option<PluginProject>, cx: &mut App) {
        (self.set_project)(view, project, cx)
    }
}

/// Every plugin, in title-bar order.
pub fn specs() -> impl Iterator<Item = PluginSpec> {
    [
        PluginSpec::of::<git_log::GitLogPanel>(),
        PluginSpec::of::<todo::TodoPanel>(),
    ]
    .into_iter()
}

pub fn spec(id: &str) -> Option<PluginSpec> {
    specs().find(|spec| spec.id == id)
}
