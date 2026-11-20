//! Pane layout tree: leaves hold one pane id; groups split along an axis.
//!
//! Each project owns tabs, and each tab owns a pane tree. A tab is one whole
//! arrangement of panes, like a Herdr/tmux window; switching tabs switches the
//! layout while every terminal keeps running. Switching projects restores the
//! project's selected tab and its tree.

use std::collections::{HashMap, HashSet};

use gpui_kit::Axis;

use crate::agent::launch::PaneLaunch;

/// Stable id for one terminal pane in the home window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

/// Direction of a keyboard focus move between panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneDirection {
    Left,
    Right,
    Up,
    Down,
}

/// Structure version of [`SavedWorkspace`]. Bump it when the JSON shape
/// changes; an unknown version is ignored rather than guessed at.
///
/// Version 4 added cell-level tabs (`SavedNode::Tabs`). Version 5 replaced
/// them with project-level tabs; the older wrappers stay readable so a saved
/// layout is never silently thrown away.
pub const WORKSPACE_STATE_VERSION: u32 = 5;

/// Structure versions this build can read.
const READABLE_WORKSPACE_VERSIONS: std::ops::RangeInclusive<u32> = 3..=WORKSPACE_STATE_VERSION;

/// A saved layout is restored only up to this many panes per project, so a
/// corrupt document cannot ask for hundreds of processes. This is a sanity
/// bound, not a working limit: a real layout above the old 12-pane cap used to
/// be dropped on the floor, taking the whole project tree with it.
const MAX_RESTORED_PANES: usize = 64;

/// Saved pane ids are kept below this bound so `next_pane` cannot overflow on
/// the next split after a corrupt document.
const MAX_SAVED_PANE_ID: u64 = u32::MAX as u64;

/// A restored group drops ratios this small: the panel's own 100px minimum
/// would make them wrong anyway, so an even split is the honest fallback.
const MIN_RATIO: f32 = 0.02;

/// One tab of a project: a named pane layout and the pane that had focus.
#[derive(Clone, Debug)]
pub struct ProjectTab {
    /// Empty until the user renames it; the UI shows a numbered fallback.
    pub name: String,
    pub layout: PaneNode,
    pub focused: PaneId,
}

/// A project's tabs and the one currently shown.
#[derive(Clone, Debug)]
pub struct ProjectTabs {
    pub tabs: Vec<ProjectTab>,
    pub selected: usize,
}

impl ProjectTabs {
    /// The visible tab, falling back to the first when `selected` is stale.
    pub fn active(&self) -> Option<&ProjectTab> {
        self.tabs.get(self.selected).or_else(|| self.tabs.first())
    }

    pub fn active_mut(&mut self) -> Option<&mut ProjectTab> {
        let index = if self.selected < self.tabs.len() {
            self.selected
        } else {
            return self.tabs.first_mut();
        };
        self.tabs.get_mut(index)
    }

    /// Every pane of every tab, in tab order.
    pub fn panes(&self) -> Vec<PaneId> {
        self.tabs
            .iter()
            .flat_map(|tab| tab.layout.panes())
            .collect()
    }
}

/// The pane tree as persisted. Group ids are a rendering detail — they key the
/// resizable element — so they are assigned fresh on restore instead of saved.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum SavedNode {
    Leaf {
        pane: u64,
    },
    /// Version 4 cell tabs. Read only: one cell could hold several terminals.
    Tabs {
        active: u64,
        panes: Vec<u64>,
    },
    Group {
        horizontal: bool,
        /// Normalized child sizes, oldest child first. Empty means "let the
        /// group divide its space evenly" — also what an older saved layout
        /// without this field decodes to.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sizes: Vec<f32>,
        children: Vec<SavedNode>,
    },
    /// Version 3/4 payload: one pane tree per project.
    ProjectLayouts {
        selected: Option<i64>,
        projects: Vec<SavedProjectLayout>,
        /// Pane id → what it runs, for panes that are not plain shells.
        /// Omitted when empty, so a shell-only layout looks exactly like one
        /// written before agent panes existed.
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        launches: HashMap<u64, PaneLaunch>,
    },
    /// Version 5 payload: one or more named layouts per project.
    ProjectTabs {
        selected: Option<i64>,
        projects: Vec<SavedProjectTabs>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        launches: HashMap<u64, PaneLaunch>,
    },
}

/// One project's saved pane tree and last focused pane, before tabs existed.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SavedProjectLayout {
    pub project: i64,
    pub layout: SavedNode,
    pub focused: u64,
}

/// One project's saved tab list.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SavedProjectTabs {
    pub project: i64,
    #[serde(default)]
    pub selected_tab: usize,
    pub tabs: Vec<SavedProjectTab>,
}

/// One saved tab.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SavedProjectTab {
    #[serde(default)]
    pub name: String,
    pub layout: SavedNode,
    pub focused: u64,
}

/// The home window's saved project layouts.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SavedWorkspace {
    pub version: u32,
    pub layout: SavedNode,
    pub focused: u64,
}

/// Persisted layouts that passed validation.
pub struct RestoredWorkspace {
    /// Project id → its tabs. Projects whose tabs all failed validation are
    /// absent, while `selected` may still name one (an empty project).
    pub tabs: HashMap<i64, ProjectTabs>,
    pub selected: Option<i64>,
    /// Group id → normalized child sizes. Every restored group has an entry;
    /// an empty vector means "no usable sizes, divide evenly".
    pub group_sizes: HashMap<u64, Vec<f32>>,
    /// Pane id → the agent pane it restores as. Panes absent here are shells.
    pub launches: HashMap<PaneId, PaneLaunch>,
    pub next_pane: u64,
    pub next_group: u64,
}

/// Child sizes as normalized fractions of their group. `None` when the
/// document does not name one valid fraction per child.
fn normalized_sizes(sizes: &[f32], children: usize) -> Option<Vec<f32>> {
    if sizes.len() != children || sizes.is_empty() {
        return None;
    }
    if sizes
        .iter()
        .any(|size| !size.is_finite() || *size < MIN_RATIO)
    {
        return None;
    }
    let total: f32 = sizes.iter().sum();
    if !total.is_finite() || total <= 0. {
        return None;
    }
    Some(sizes.iter().map(|size| size / total).collect())
}

impl SavedWorkspace {
    /// Parse and validate persisted state. Invalid or obsolete documents are
    /// ignored so the app can open a fresh default pane without a startup error.
    pub fn restore(saved: &str, project_exists: impl Fn(i64) -> bool) -> Option<RestoredWorkspace> {
        let state: Self = serde_json::from_str(saved).ok()?;
        if !READABLE_WORKSPACE_VERSIONS.contains(&state.version) {
            return None;
        }
        match &state.layout {
            SavedNode::ProjectTabs {
                selected,
                projects,
                launches,
            } => restore_project_tabs(projects, *selected, launches, project_exists),
            SavedNode::ProjectLayouts {
                selected,
                projects,
                launches,
            } => restore_project_layouts(projects, *selected, launches, project_exists),
            _ => None,
        }
    }
}

fn restore_project_tabs(
    saved_projects: &[SavedProjectTabs],
    selected: Option<i64>,
    saved_launches: &HashMap<u64, PaneLaunch>,
    project_exists: impl Fn(i64) -> bool,
) -> Option<RestoredWorkspace> {
    let mut next_group = 1;
    let mut next_pane = 2;
    let mut group_sizes = HashMap::new();
    let mut tabs_by_project = HashMap::new();
    let mut seen = HashSet::new();
    for saved in saved_projects {
        if !project_exists(saved.project) || tabs_by_project.contains_key(&saved.project) {
            continue;
        }
        let mut tabs = Vec::new();
        for saved_tab in &saved.tabs {
            let mut tab_groups = HashMap::new();
            let mut tab_next_group = next_group;
            let Some(layout) = saved_tab.layout.restore(
                &saved_tab.layout.pane_ids(),
                &mut tab_next_group,
                &mut tab_groups,
            ) else {
                continue;
            };
            let leaves = layout.panes();
            if leaves.is_empty() || leaves.len() > MAX_RESTORED_PANES {
                continue;
            }
            let unique: HashSet<u64> = leaves.iter().map(|pane| pane.0).collect();
            if unique.len() != leaves.len()
                || leaves
                    .iter()
                    .any(|pane| pane.0 == 0 || pane.0 > MAX_SAVED_PANE_ID || seen.contains(&pane.0))
            {
                continue;
            }
            seen.extend(unique);
            next_group = tab_next_group;
            group_sizes.extend(tab_groups);
            next_pane = next_pane.max(
                leaves
                    .iter()
                    .map(|pane| pane.0.saturating_add(1))
                    .max()
                    .unwrap_or(2),
            );
            let focused = if leaves.contains(&PaneId(saved_tab.focused)) {
                PaneId(saved_tab.focused)
            } else {
                leaves[0]
            };
            tabs.push(ProjectTab {
                name: saved_tab.name.clone(),
                layout,
                focused,
            });
        }
        if tabs.is_empty() {
            continue;
        }
        let selected_tab = saved.selected_tab.min(tabs.len() - 1);
        tabs_by_project.insert(
            saved.project,
            ProjectTabs {
                tabs,
                selected: selected_tab,
            },
        );
    }
    finish_restore(
        tabs_by_project,
        selected,
        saved_projects
            .iter()
            .map(|project| project.project)
            .collect(),
        saved_launches,
        group_sizes,
        next_group,
        &project_exists,
    )
}

/// Version 3/4 documents had one tree per project: that tree becomes the
/// project's first tab. Version 4 cell tabs collapse to their visible pane.
fn restore_project_layouts(
    saved_projects: &[SavedProjectLayout],
    selected: Option<i64>,
    saved_launches: &HashMap<u64, PaneLaunch>,
    project_exists: impl Fn(i64) -> bool,
) -> Option<RestoredWorkspace> {
    let mut next_group = 1;
    let mut next_pane = 2;
    let mut group_sizes = HashMap::new();
    let mut tabs_by_project = HashMap::new();
    let mut seen = HashSet::new();
    for saved in saved_projects {
        if !project_exists(saved.project) || tabs_by_project.contains_key(&saved.project) {
            continue;
        }
        let mut local_groups = HashMap::new();
        let mut local_next_group = next_group;
        let Some(layout) = saved.layout.restore(
            &saved.layout.pane_ids(),
            &mut local_next_group,
            &mut local_groups,
        ) else {
            continue;
        };
        let leaves = layout.panes();
        if leaves.is_empty() || leaves.len() > MAX_RESTORED_PANES {
            continue;
        }
        let unique: HashSet<u64> = leaves.iter().map(|pane| pane.0).collect();
        if unique.len() != leaves.len()
            || leaves
                .iter()
                .any(|pane| pane.0 == 0 || pane.0 > MAX_SAVED_PANE_ID || seen.contains(&pane.0))
        {
            continue;
        }
        seen.extend(unique);
        next_group = local_next_group;
        group_sizes.extend(local_groups);
        next_pane = next_pane.max(
            leaves
                .iter()
                .map(|pane| pane.0.saturating_add(1))
                .max()
                .unwrap_or(2),
        );
        let focused = if leaves.contains(&PaneId(saved.focused)) {
            PaneId(saved.focused)
        } else {
            leaves[0]
        };
        tabs_by_project.insert(
            saved.project,
            ProjectTabs {
                tabs: vec![ProjectTab {
                    name: String::new(),
                    layout,
                    focused,
                }],
                selected: 0,
            },
        );
    }
    finish_restore(
        tabs_by_project,
        selected,
        saved_projects
            .iter()
            .map(|project| project.project)
            .collect(),
        saved_launches,
        group_sizes,
        next_group,
        &project_exists,
    )
}

/// Validate the selection and keep only launches owned by a surviving pane.
fn finish_restore(
    tabs_by_project: HashMap<i64, ProjectTabs>,
    selected: Option<i64>,
    saved: Vec<i64>,
    saved_launches: &HashMap<u64, PaneLaunch>,
    group_sizes: HashMap<u64, Vec<f32>>,
    next_group: u64,
    project_exists: &impl Fn(i64) -> bool,
) -> Option<RestoredWorkspace> {
    let selected = selected
        .filter(|project| {
            project_exists(*project)
                && (tabs_by_project.contains_key(project) || !saved.contains(project))
        })
        .or_else(|| {
            saved
                .iter()
                .find(|project| tabs_by_project.contains_key(project))
                .copied()
        });
    if selected.is_none() && !saved.is_empty() {
        return None;
    }
    let launches: HashMap<PaneId, PaneLaunch> = saved_launches
        .iter()
        .filter(|(pane, launch)| {
            !launch.is_shell()
                && tabs_by_project
                    .values()
                    .any(|project| project.panes().contains(&PaneId(**pane)))
        })
        .map(|(pane, launch)| (PaneId(*pane), launch.clone()))
        .collect();
    let next_pane = tabs_by_project
        .values()
        .flat_map(|project| project.panes())
        .map(|pane| pane.0.saturating_add(1))
        .max()
        .unwrap_or(2);
    Some(RestoredWorkspace {
        tabs: tabs_by_project,
        selected,
        group_sizes,
        launches,
        next_pane,
        next_group,
    })
}

impl SavedNode {
    fn pane_ids(&self) -> HashSet<u64> {
        match self {
            Self::Leaf { pane } => HashSet::from([*pane]),
            Self::Tabs { panes, .. } => panes.iter().copied().collect(),
            Self::Group { children, .. } => children.iter().flat_map(Self::pane_ids).collect(),
            Self::ProjectLayouts { projects, .. } => projects
                .iter()
                .flat_map(|project| project.layout.pane_ids())
                .collect(),
            Self::ProjectTabs { projects, .. } => projects
                .iter()
                .flat_map(|project| project.tabs.iter().flat_map(|tab| tab.layout.pane_ids()))
                .collect(),
        }
    }

    /// Rebuild the layout, keeping only panes in `valid` (their project still
    /// exists) and collapsing groups left with a single child. Group ids are
    /// assigned from `next_group` so a restored tree never collides with one
    /// created later.
    pub fn restore(
        &self,
        valid: &HashSet<u64>,
        next_group: &mut u64,
        group_sizes: &mut HashMap<u64, Vec<f32>>,
    ) -> Option<PaneNode> {
        match self {
            Self::Leaf { pane } => valid
                .contains(pane)
                .then_some(PaneNode::leaf(PaneId(*pane))),
            // Version 4 only: a cell's tabs collapse to the pane it showed.
            Self::Tabs { active, panes } => {
                let kept: Vec<u64> = panes
                    .iter()
                    .copied()
                    .filter(|pane| valid.contains(pane))
                    .collect();
                let pane = if kept.contains(active) {
                    *active
                } else {
                    *kept.first()?
                };
                Some(PaneNode::leaf(PaneId(pane)))
            }
            Self::ProjectLayouts { .. } | Self::ProjectTabs { .. } => None,
            Self::Group {
                horizontal,
                sizes,
                children,
            } => {
                let mut restored: Vec<PaneNode> = children
                    .iter()
                    .filter_map(|child| child.restore(valid, next_group, group_sizes))
                    .collect();
                match restored.len() {
                    0 => None,
                    1 => restored.pop(),
                    _ => {
                        let id = *next_group;
                        *next_group += 1;
                        // Dropping a child invalidates the old fractions; keep
                        // them only when every original child survived.
                        group_sizes.insert(
                            id,
                            if restored.len() == children.len() {
                                normalized_sizes(sizes, restored.len()).unwrap_or_default()
                            } else {
                                Vec::new()
                            },
                        );
                        Some(PaneNode::Group {
                            id,
                            axis: if *horizontal {
                                Axis::Horizontal
                            } else {
                                Axis::Vertical
                            },
                            children: restored,
                        })
                    }
                }
            }
        }
    }
}

/// Recursive layout of panes in one tab.
#[derive(Clone, Debug)]
pub enum PaneNode {
    Leaf(PaneId),
    Group {
        /// Keyed id for the resizable group, stable across re-renders.
        id: u64,
        axis: Axis,
        children: Vec<PaneNode>,
    },
}

/// A pane's box with every group dividing its space evenly. Exact split
/// ratios do not matter for picking a direction: panes on opposite sides of a
/// split stay on those sides whatever the ratios are.
#[derive(Clone, Copy)]
struct Region {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Region {
    fn center_x(self) -> f32 {
        self.x + self.w / 2.
    }

    fn center_y(self) -> f32 {
        self.y + self.h / 2.
    }
}

impl PaneNode {
    pub fn leaf(id: PaneId) -> Self {
        Self::Leaf(id)
    }

    /// Every pane id, depth-first.
    pub fn panes(&self) -> Vec<PaneId> {
        match self {
            Self::Leaf(id) => vec![*id],
            Self::Group { children, .. } => children.iter().flat_map(Self::panes).collect(),
        }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            Self::Leaf(leaf) => *leaf == id,
            Self::Group { children, .. } => children.iter().any(|child| child.contains(id)),
        }
    }

    /// The pane next to `id` toward `direction`, crossing group boundaries.
    /// The nearest pane on that side wins, so a pane nested in another column
    /// is reachable without knowing the tree shape.
    pub fn neighbor_in(&self, id: PaneId, direction: PaneDirection) -> Option<PaneId> {
        let mut regions = Vec::new();
        self.collect_regions(
            Region {
                x: 0.,
                y: 0.,
                w: 1.,
                h: 1.,
            },
            &mut regions,
        );
        let target = *regions
            .iter()
            .find(|(pane, _)| *pane == id)
            .map(|(_, region)| region)?;
        let mut best: Option<(PaneId, f32, f32)> = None;
        for &(pane, region) in &regions {
            if pane == id {
                continue;
            }
            // `gap` is the distance along the movement axis: negative means the
            // pane is behind the target and cannot be the neighbor. `side` is
            // how far the two centers are apart on the other axis, breaking
            // ties between panes at the same distance.
            let (gap, side) = match direction {
                PaneDirection::Left => (
                    target.x - (region.x + region.w),
                    (region.center_y() - target.center_y()).abs(),
                ),
                PaneDirection::Right => (
                    region.x - (target.x + target.w),
                    (region.center_y() - target.center_y()).abs(),
                ),
                PaneDirection::Up => (
                    target.y - (region.y + region.h),
                    (region.center_x() - target.center_x()).abs(),
                ),
                PaneDirection::Down => (
                    region.y - (target.y + target.h),
                    (region.center_x() - target.center_x()).abs(),
                ),
            };
            // Panes share exact edges; float error must not hide those.
            if gap < -0.0001 {
                continue;
            }
            if best.is_none_or(|(_, best_gap, best_side)| (gap, side) < (best_gap, best_side)) {
                best = Some((pane, gap, side));
            }
        }
        best.map(|(pane, _, _)| pane)
    }

    fn collect_regions(&self, region: Region, out: &mut Vec<(PaneId, Region)>) {
        match self {
            Self::Leaf(id) => out.push((*id, region)),
            Self::Group { axis, children, .. } => {
                let count = children.len() as f32;
                for (i, child) in children.iter().enumerate() {
                    let offset = i as f32 / count;
                    let child_region = match axis {
                        Axis::Horizontal => Region {
                            x: region.x + region.w * offset,
                            y: region.y,
                            w: region.w / count,
                            h: region.h,
                        },
                        Axis::Vertical => Region {
                            x: region.x,
                            y: region.y + region.h * offset,
                            w: region.w,
                            h: region.h / count,
                        },
                    };
                    child.collect_regions(child_region, out);
                }
            }
        }
    }

    /// Neighbor to focus after `id` is removed: the previous pane, else the
    /// next, else any remaining pane.
    pub fn neighbor(&self, id: PaneId) -> Option<PaneId> {
        let panes = self.panes();
        let i = panes.iter().position(|p| *p == id)?;
        if i > 0 {
            Some(panes[i - 1])
        } else {
            panes.get(i + 1).copied()
        }
    }

    /// Put `new` beside `target` along `axis` (after it). Reuses a same-axis
    /// parent; otherwise wraps the target. `group_id` is used only when wrapping.
    pub fn split(&mut self, target: PaneId, axis: Axis, new: PaneId, group_id: u64) -> bool {
        if let Some(parent_id) = self.parent_group_id(target)
            && let Some(Self::Group {
                axis: group_axis,
                children,
                ..
            }) = self.find_group_mut(parent_id)
            && *group_axis == axis
        {
            let Some(i) = children.iter().position(|c| c.contains(target)) else {
                return false;
            };
            children.insert(i + 1, Self::Leaf(new));
            return true;
        }
        let Some(slot) = self.find_leaf_mut(target) else {
            return false;
        };
        *slot = Self::Group {
            id: group_id,
            axis,
            children: vec![Self::Leaf(target), Self::Leaf(new)],
        };
        true
    }

    /// Remove the leaf `id`. Collapses groups left with a single child.
    /// Returns false when `id` is the only leaf (caller should keep it).
    pub fn remove(&mut self, id: PaneId) -> bool {
        if self.panes().len() <= 1 {
            return false;
        }
        if !remove_inner(self, id) {
            return false;
        }
        collapse(self);
        true
    }

    /// The pane tree as it is written to the database. `ratios` is keyed by
    /// group id and holds each group's current child fractions.
    pub fn to_saved(&self, ratios: &HashMap<u64, Vec<f32>>) -> SavedNode {
        match self {
            Self::Leaf(id) => SavedNode::Leaf { pane: id.0 },
            Self::Group { id, axis, children } => SavedNode::Group {
                horizontal: *axis == Axis::Horizontal,
                sizes: ratios
                    .get(id)
                    .filter(|sizes| sizes.len() == children.len())
                    .cloned()
                    .unwrap_or_default(),
                children: children
                    .iter()
                    .map(|child| child.to_saved(ratios))
                    .collect(),
            },
        }
    }

    /// Every group id, depth-first.
    pub fn group_ids(&self) -> Vec<u64> {
        match self {
            Self::Leaf(_) => Vec::new(),
            Self::Group { id, children, .. } => std::iter::once(*id)
                .chain(children.iter().flat_map(Self::group_ids))
                .collect(),
        }
    }

    pub fn contains_group(&self, group_id: u64) -> bool {
        self.group_ids().contains(&group_id)
    }

    fn parent_group_id(&self, id: PaneId) -> Option<u64> {
        match self {
            Self::Leaf(_) => None,
            Self::Group {
                id: group_id,
                children,
                ..
            } => {
                // Only a direct leaf child means this is the target's parent;
                // `contains` would return the outermost group of a nested tree
                // and make a second same-axis split nest instead of reuse.
                if children.iter().any(|child| child.is_leaf(id)) {
                    Some(*group_id)
                } else {
                    children.iter().find_map(|child| child.parent_group_id(id))
                }
            }
        }
    }

    fn is_leaf(&self, id: PaneId) -> bool {
        matches!(self, Self::Leaf(leaf) if *leaf == id)
    }

    fn find_group_mut(&mut self, group_id: u64) -> Option<&mut Self> {
        match self {
            Self::Group { id, .. } if *id == group_id => Some(self),
            Self::Group { children, .. } => {
                children.iter_mut().find_map(|c| c.find_group_mut(group_id))
            }
            Self::Leaf(_) => None,
        }
    }

    fn find_leaf_mut(&mut self, id: PaneId) -> Option<&mut Self> {
        match self {
            Self::Leaf(leaf) if *leaf == id => Some(self),
            Self::Group { children, .. } => children.iter_mut().find_map(|c| c.find_leaf_mut(id)),
            Self::Leaf(_) => None,
        }
    }
}

fn remove_inner(node: &mut PaneNode, id: PaneId) -> bool {
    let PaneNode::Group { children, .. } = node else {
        return false;
    };
    if let Some(i) = children.iter().position(|c| c.contains(id)) {
        if let PaneNode::Leaf(leaf) = &children[i]
            && *leaf == id
        {
            children.remove(i);
            return true;
        }
        return remove_inner(&mut children[i], id);
    }
    false
}

fn collapse(node: &mut PaneNode) {
    let PaneNode::Group { children, .. } = node else {
        return;
    };
    for child in children.iter_mut() {
        collapse(child);
    }
    if children.len() == 1 {
        *node = children.pop().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(n: u64) -> PaneNode {
        PaneNode::leaf(PaneId(n))
    }

    fn tab(name: &str, layout: PaneNode, focused: u64) -> SavedProjectTab {
        SavedProjectTab {
            name: name.into(),
            layout: layout.to_saved(&HashMap::new()),
            focused,
        }
    }

    #[test]
    fn splits_reuse_a_same_axis_parent_and_wrap_otherwise() {
        let mut n = leaf(1);
        assert!(n.split(PaneId(1), Axis::Horizontal, PaneId(2), 10));
        assert!(matches!(
            &n,
            PaneNode::Group {
                axis: Axis::Horizontal,
                children,
                ..
            } if children.len() == 2
        ));
        assert!(n.split(PaneId(2), Axis::Horizontal, PaneId(3), 99));
        assert_eq!(n.panes(), vec![PaneId(1), PaneId(2), PaneId(3)]);
        assert!(n.split(PaneId(3), Axis::Vertical, PaneId(4), 11));
        let PaneNode::Group {
            axis: Axis::Horizontal,
            children,
            ..
        } = &n
        else {
            panic!("root should stay horizontal");
        };
        assert!(matches!(
            &children[2],
            PaneNode::Group {
                axis: Axis::Vertical,
                ..
            }
        ));
        assert_eq!(n.panes(), vec![PaneId(1), PaneId(2), PaneId(3), PaneId(4)]);
    }

    #[test]
    fn splitting_reuses_the_direct_same_axis_parent() {
        let mut n = leaf(1);
        n.split(PaneId(1), Axis::Horizontal, PaneId(2), 10);
        n.split(PaneId(2), Axis::Vertical, PaneId(3), 11);
        assert!(n.split(PaneId(3), Axis::Vertical, PaneId(4), 12));
        // H[1, V[2, 3, 4]]: the second vertical split joins V, it does not
        // wrap pane 3 in another vertical group.
        let PaneNode::Group { children, .. } = &n else {
            panic!("root group");
        };
        assert_eq!(children.len(), 2);
        let PaneNode::Group {
            axis: Axis::Vertical,
            children: inner,
            ..
        } = &children[1]
        else {
            panic!("right column is vertical");
        };
        assert_eq!(inner.len(), 3);
        assert_eq!(n.panes(), vec![PaneId(1), PaneId(2), PaneId(3), PaneId(4)]);
    }

    #[test]
    fn closing_collapses_single_child_groups() {
        let mut n = leaf(1);
        n.split(PaneId(1), Axis::Horizontal, PaneId(2), 10);
        n.split(PaneId(2), Axis::Vertical, PaneId(3), 11);
        assert!(n.remove(PaneId(3)));
        assert_eq!(n.panes(), vec![PaneId(1), PaneId(2)]);
        assert!(n.remove(PaneId(2)));
        assert!(matches!(n, PaneNode::Leaf(PaneId(1))));
        assert!(!n.remove(PaneId(1)));
    }

    #[test]
    fn neighbor_prefers_the_previous_leaf() {
        let mut n = leaf(1);
        n.split(PaneId(1), Axis::Horizontal, PaneId(2), 10);
        n.split(PaneId(2), Axis::Horizontal, PaneId(3), 10);
        assert_eq!(n.neighbor(PaneId(2)), Some(PaneId(1)));
        assert_eq!(n.neighbor(PaneId(1)), Some(PaneId(2)));
    }

    /// A direction crosses split boundaries instead of walking depth-first:
    /// from the bottom-right pane, left lands in the full-height left column.
    #[test]
    fn directional_focus_crosses_split_boundaries() {
        let mut n = leaf(1);
        n.split(PaneId(1), Axis::Horizontal, PaneId(2), 10);
        n.split(PaneId(2), Axis::Vertical, PaneId(3), 11);

        assert_eq!(
            n.neighbor_in(PaneId(1), PaneDirection::Right),
            Some(PaneId(2))
        );
        assert_eq!(
            n.neighbor_in(PaneId(3), PaneDirection::Left),
            Some(PaneId(1))
        );
        assert_eq!(
            n.neighbor_in(PaneId(2), PaneDirection::Down),
            Some(PaneId(3))
        );
        assert_eq!(n.neighbor_in(PaneId(3), PaneDirection::Up), Some(PaneId(2)));
        assert_eq!(n.neighbor_in(PaneId(1), PaneDirection::Left), None);
        assert_eq!(n.neighbor_in(PaneId(2), PaneDirection::Up), None);
    }

    /// In a 2×2 grid the pane sharing the edge wins over the diagonal one.
    #[test]
    fn directional_focus_prefers_the_aligned_pane() {
        let mut n = leaf(1);
        n.split(PaneId(1), Axis::Vertical, PaneId(2), 10);
        n.split(PaneId(1), Axis::Horizontal, PaneId(3), 11);
        n.split(PaneId(2), Axis::Horizontal, PaneId(4), 12);

        assert_eq!(
            n.neighbor_in(PaneId(1), PaneDirection::Right),
            Some(PaneId(3))
        );
        assert_eq!(
            n.neighbor_in(PaneId(1), PaneDirection::Down),
            Some(PaneId(2))
        );
        assert_eq!(
            n.neighbor_in(PaneId(3), PaneDirection::Down),
            Some(PaneId(4))
        );
        assert_eq!(
            n.neighbor_in(PaneId(4), PaneDirection::Left),
            Some(PaneId(2))
        );
        assert_eq!(
            n.neighbor_in(PaneId(2), PaneDirection::Right),
            Some(PaneId(4))
        );
    }

    /// A setup with more splits than the old 12-pane sanity cap must still
    /// restore: dropping the project's tree silently reset the window to one
    /// pane on the next launch.
    #[test]
    fn a_layout_beyond_the_old_pane_cap_still_restores() {
        let leaves = (1..=13).map(|pane| SavedNode::Leaf { pane }).collect();
        let saved = SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: SavedNode::ProjectTabs {
                selected: Some(3),
                projects: vec![SavedProjectTabs {
                    project: 3,
                    selected_tab: 0,
                    tabs: vec![SavedProjectTab {
                        name: String::new(),
                        layout: SavedNode::Group {
                            horizontal: true,
                            sizes: Vec::new(),
                            children: leaves,
                        },
                        focused: 1,
                    }],
                }],
                launches: HashMap::new(),
            },
            focused: 1,
        };
        let json = serde_json::to_string(&saved).unwrap();
        let restored = SavedWorkspace::restore(&json, |_| true).expect("layout restores");
        assert_eq!(restored.tabs[&3].panes().len(), 13);
    }

    /// A saved tree comes back with the same leaves and split axes, even though
    /// group ids are assigned fresh.
    #[test]
    fn a_saved_layout_round_trips_its_structure() {
        let mut n = leaf(1);
        n.split(PaneId(1), Axis::Horizontal, PaneId(2), 10);
        n.split(PaneId(2), Axis::Vertical, PaneId(3), 11);

        let ratios = HashMap::from([(10, vec![0.4, 0.6]), (11, vec![0.5, 0.5])]);
        let mut next_group = 1;
        let mut group_sizes = HashMap::new();
        let restored = n
            .to_saved(&ratios)
            .restore(&HashSet::from([1, 2, 3]), &mut next_group, &mut group_sizes)
            .expect("every pane is still live");
        assert_eq!(restored.panes(), vec![PaneId(1), PaneId(2), PaneId(3)]);
        assert_eq!(
            group_sizes.get(&1),
            Some(&vec![0.5, 0.5]),
            "the nested group was restored first"
        );
        assert_eq!(group_sizes.get(&2), Some(&vec![0.4, 0.6]));
        let PaneNode::Group {
            axis: Axis::Horizontal,
            children,
            ..
        } = &restored
        else {
            panic!("root should stay horizontal");
        };
        assert!(matches!(
            &children[1],
            PaneNode::Group {
                axis: Axis::Vertical,
                ..
            }
        ));
        assert_eq!(next_group, 3, "one id per surviving group");
    }

    /// Ratios are only used when there is one positive fraction per child.
    /// Anything else falls back to an even split rather than inventing sizes.
    #[test]
    fn invalid_group_sizes_fall_back_to_an_even_split() {
        assert_eq!(normalized_sizes(&[0.3, 0.7], 2), Some(vec![0.3, 0.7]));
        assert!(normalized_sizes(&[0.3], 2).is_none(), "wrong length");
        assert!(normalized_sizes(&[0.0, 1.0], 2).is_none(), "zero fraction");
        assert!(
            normalized_sizes(&[f32::NAN, 1.0], 2).is_none(),
            "non-finite fraction"
        );

        let saved = SavedNode::Group {
            horizontal: true,
            sizes: vec![0.0, 1.0],
            children: vec![SavedNode::Leaf { pane: 1 }, SavedNode::Leaf { pane: 2 }],
        };
        let mut next_group = 1;
        let mut group_sizes = HashMap::new();
        saved
            .restore(&HashSet::from([1, 2]), &mut next_group, &mut group_sizes)
            .expect("both panes survive");
        assert_eq!(group_sizes.get(&1), Some(&Vec::new()));
    }

    /// A deleted project takes its panes with it; groups that lose all but one
    /// child collapse rather than leaving a one-slot split.
    #[test]
    fn restoring_prunes_missing_panes_and_collapses_groups() {
        let saved = SavedNode::Group {
            horizontal: true,
            sizes: vec![0.3, 0.4, 0.3],
            children: vec![
                SavedNode::Leaf { pane: 1 },
                SavedNode::Leaf { pane: 2 },
                SavedNode::Group {
                    horizontal: false,
                    sizes: vec![0.5, 0.5],
                    children: vec![SavedNode::Leaf { pane: 3 }, SavedNode::Leaf { pane: 4 }],
                },
            ],
        };
        let mut next_group = 1;
        let mut group_sizes = HashMap::new();
        let restored = saved
            .restore(&HashSet::from([2, 3]), &mut next_group, &mut group_sizes)
            .expect("two panes remain");
        assert_eq!(restored.panes(), vec![PaneId(2), PaneId(3)]);

        let mut next_group = 1;
        let mut group_sizes = HashMap::new();
        assert!(
            saved
                .restore(&HashSet::new(), &mut next_group, &mut group_sizes)
                .is_none()
        );

        let mut next_group = 1;
        let mut group_sizes = HashMap::new();
        let one = saved
            .restore(&HashSet::from([3]), &mut next_group, &mut group_sizes)
            .expect("the nested pane is left");
        assert!(matches!(one, PaneNode::Leaf(PaneId(3))));
    }

    fn three_project_state() -> SavedWorkspace {
        SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: SavedNode::ProjectTabs {
                selected: Some(20),
                projects: vec![
                    SavedProjectTabs {
                        project: 10,
                        selected_tab: 0,
                        tabs: vec![tab("", leaf(1), 1)],
                    },
                    SavedProjectTabs {
                        project: 20,
                        selected_tab: 1,
                        tabs: vec![
                            tab(
                                "agents",
                                {
                                    let mut n = leaf(2);
                                    n.split(PaneId(2), Axis::Horizontal, PaneId(3), 10);
                                    n
                                },
                                3,
                            ),
                            tab("review", leaf(4), 4),
                        ],
                    },
                ],
                launches: HashMap::new(),
            },
            focused: 4,
        }
    }

    /// Restoring keeps only projects that still exist and lands focus on a
    /// surviving pane.
    #[test]
    fn restoring_keeps_only_live_projects() {
        let json = serde_json::to_string(&three_project_state()).unwrap();
        let restored =
            SavedWorkspace::restore(&json, |project| project == 20).expect("one live project");
        assert_eq!(restored.selected, Some(20));
        let project = &restored.tabs[&20];
        assert_eq!(project.tabs.len(), 2);
        assert_eq!(project.selected, 1);
        assert_eq!(project.tabs[0].name, "agents");
        assert_eq!(project.tabs[1].name, "review");
        assert_eq!(project.panes(), vec![PaneId(2), PaneId(3), PaneId(4)]);
    }

    /// Version 3 documents had one tree per project; that tree becomes the
    /// project's first tab.
    #[test]
    fn a_version_three_layout_becomes_one_tab() {
        let json = r#"{"version":3,"focused":7,"layout":{"ProjectLayouts":{"selected":3,"projects":[{"project":3,"layout":{"Leaf":{"pane":7}},"focused":7}]}}}"#;
        let restored = SavedWorkspace::restore(json, |_| true).unwrap();
        assert!(restored.launches.is_empty());
        let project = &restored.tabs[&3];
        assert_eq!(project.tabs.len(), 1);
        assert_eq!(project.selected, 0);
        assert_eq!(project.panes(), vec![PaneId(7)]);
    }

    /// Version 4 cell tabs collapse to the pane that cell was showing.
    #[test]
    fn version_four_cell_tabs_keep_the_visible_pane() {
        let json = r#"{"version":4,"focused":2,"layout":{"ProjectLayouts":{"selected":3,"projects":[{"project":3,"layout":{"Tabs":{"active":2,"panes":[1,2]}},"focused":2}]}}}"#;
        let restored = SavedWorkspace::restore(json, |_| true).unwrap();
        assert_eq!(restored.tabs[&3].panes(), vec![PaneId(2)]);
    }

    #[test]
    fn restoring_keeps_an_empty_project_selected() {
        let mut state = three_project_state();
        if let SavedNode::ProjectTabs { projects, .. } = &mut state.layout {
            projects.retain(|project| project.project != 20);
        }
        let json = serde_json::to_string(&state).unwrap();
        let restored = SavedWorkspace::restore(&json, |_| true).unwrap();
        assert_eq!(restored.selected, Some(20));
        assert!(!restored.tabs.contains_key(&20));
        assert!(restored.tabs.contains_key(&10));

        if let SavedNode::ProjectTabs { projects, .. } = &mut state.layout {
            projects.clear();
        }
        let json = serde_json::to_string(&state).unwrap();
        let restored = SavedWorkspace::restore(&json, |_| true).unwrap();
        assert_eq!(restored.selected, Some(20));
        assert!(restored.tabs.is_empty());
    }

    #[test]
    fn an_invalid_project_layout_does_not_discard_later_projects() {
        let mut state = three_project_state();
        if let SavedNode::ProjectTabs {
            projects, selected, ..
        } = &mut state.layout
        {
            *selected = Some(10);
            projects[0].tabs[0].layout = SavedNode::Group {
                horizontal: true,
                sizes: Vec::new(),
                children: Vec::new(),
            };
        }
        let json = serde_json::to_string(&state).unwrap();
        let restored = SavedWorkspace::restore(&json, |_| true).unwrap();
        assert_eq!(restored.selected, Some(20));
        assert_eq!(restored.tabs.len(), 1);
    }

    /// A selected tab beyond the surviving tabs falls back to the last one.
    #[test]
    fn a_stale_selected_tab_falls_back() {
        let mut state = three_project_state();
        if let SavedNode::ProjectTabs { projects, .. } = &mut state.layout {
            projects[1].selected_tab = 99;
        }
        let json = serde_json::to_string(&state).unwrap();
        let restored = SavedWorkspace::restore(&json, |_| true).unwrap();
        assert_eq!(restored.tabs[&20].selected, 1);
    }

    /// Malformed JSON, an unknown version, a missing project, and duplicate
    /// pane ids all mean "open the default pane", never a startup failure.
    #[test]
    fn an_untrusted_saved_workspace_is_rejected() {
        assert!(SavedWorkspace::restore("{", |_| true).is_none());

        let state = three_project_state();
        let json = serde_json::to_string(&state).unwrap();
        assert!(
            SavedWorkspace::restore(&json, |_| false).is_none(),
            "every project is gone"
        );

        let mut future = state.clone();
        future.version = WORKSPACE_STATE_VERSION + 1;
        let json = serde_json::to_string(&future).unwrap();
        assert!(
            SavedWorkspace::restore(&json, |_| true).is_none(),
            "an unknown structure version is not guessed at"
        );

        let mut duplicate = state;
        duplicate.layout = SavedNode::ProjectTabs {
            selected: Some(10),
            projects: vec![SavedProjectTabs {
                project: 10,
                selected_tab: 0,
                tabs: vec![tab("one", leaf(1), 1), tab("two", leaf(1), 1)],
            }],
            launches: HashMap::new(),
        };
        let json = serde_json::to_string(&duplicate).unwrap();
        let restored = SavedWorkspace::restore(&json, |_| true).expect("one tab survives");
        assert_eq!(
            restored.tabs[&10].tabs.len(),
            1,
            "a tab sharing a pane with an earlier tab is dropped"
        );
    }

    /// Layouts written before agent panes existed carry no `launches`, and a
    /// shell-only layout writes none back: upgrading must not rewrite or drop
    /// the user's trees.
    #[test]
    fn a_layout_without_launches_restores_as_shells() {
        let json = r#"{"version":3,"focused":7,"layout":{"ProjectLayouts":{"selected":3,"projects":[{"project":3,"layout":{"Leaf":{"pane":7}},"focused":7}]}}}"#;
        let restored = SavedWorkspace::restore(json, |_| true).unwrap();
        assert!(restored.launches.is_empty());
        assert_eq!(restored.tabs[&3].panes(), vec![PaneId(7)]);

        let saved = SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: SavedNode::ProjectTabs {
                selected: Some(3),
                projects: vec![SavedProjectTabs {
                    project: 3,
                    selected_tab: 0,
                    tabs: vec![tab("", leaf(7), 7)],
                }],
                launches: HashMap::new(),
            },
            focused: 7,
        };
        let json = serde_json::to_string(&saved).unwrap();
        assert!(!json.contains("launches"), "shells write no launch data");
        assert!(json.contains("ProjectTabs"));
    }

    #[test]
    fn an_agent_pane_survives_a_save_and_restore() {
        let launch = PaneLaunch::Agent {
            agent: "codex".into(),
            session: Some("session-1".into()),
        };
        let saved = SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: SavedNode::ProjectTabs {
                selected: Some(3),
                projects: vec![SavedProjectTabs {
                    project: 3,
                    selected_tab: 0,
                    tabs: vec![tab("", leaf(7), 7)],
                }],
                launches: HashMap::from([(7, launch.clone())]),
            },
            focused: 7,
        };
        let json = serde_json::to_string(&saved).unwrap();
        let restored = SavedWorkspace::restore(&json, |_| true).unwrap();
        assert_eq!(restored.launches[&PaneId(7)], launch);
    }

    /// A launch whose pane is no longer in any tab must not start a process
    /// no pane can show.
    #[test]
    fn a_launch_for_a_missing_pane_is_dropped() {
        let saved = SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: SavedNode::ProjectTabs {
                selected: Some(3),
                projects: vec![SavedProjectTabs {
                    project: 3,
                    selected_tab: 0,
                    tabs: vec![tab("", leaf(7), 7)],
                }],
                launches: HashMap::from([
                    (
                        7,
                        PaneLaunch::Agent {
                            agent: "codex".into(),
                            session: None,
                        },
                    ),
                    (
                        99,
                        PaneLaunch::Agent {
                            agent: "opencode".into(),
                            session: None,
                        },
                    ),
                ]),
            },
            focused: 7,
        };
        let json = serde_json::to_string(&saved).unwrap();
        let restored = SavedWorkspace::restore(&json, |_| true).unwrap();
        assert_eq!(
            restored.launches.keys().copied().collect::<Vec<_>>(),
            [PaneId(7)]
        );
    }
}
