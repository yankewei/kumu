//! Pane layout tree: leaves hold a pane id; groups split along an axis.
//!
//! Each project owns a pane tree. Splitting adds another pane to that project;
//! switching projects restores its own tree.

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
pub const WORKSPACE_STATE_VERSION: u32 = 3;

/// A saved layout is restored only up to this many panes: every restored pane
/// starts a process, so a corrupt document must not be able to launch hundreds.
const MAX_RESTORED_PANES: usize = 12;

/// Saved pane ids are kept below this bound so `next_pane` cannot overflow on
/// the next split after a corrupt document.
const MAX_SAVED_PANE_ID: u64 = u32::MAX as u64;

/// A restored group drops ratios this small: the panel's own 100px minimum
/// would make them wrong anyway, so an even split is the honest fallback.
const MIN_RATIO: f32 = 0.02;

/// The pane tree as persisted. Group ids are a rendering detail — they key the
/// resizable element — so they are assigned fresh on restore instead of saved.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum SavedNode {
    Leaf {
        pane: u64,
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
    /// Version 3 payload: one pane tree per project.
    ProjectLayouts {
        selected: Option<i64>,
        projects: Vec<SavedProjectLayout>,
        /// Pane id → what it runs, for panes that are not plain shells.
        /// Omitted when empty, so a shell-only layout looks exactly like one
        /// written before agent panes existed.
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        launches: HashMap<u64, PaneLaunch>,
    },
}

/// One project's saved pane tree and last focused pane.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SavedProjectLayout {
    pub project: i64,
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
    pub layouts: HashMap<i64, PaneNode>,
    pub focused_by_project: HashMap<i64, PaneId>,
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
        if state.version != WORKSPACE_STATE_VERSION {
            return None;
        }
        let SavedNode::ProjectLayouts {
            selected,
            projects,
            launches,
        } = &state.layout
        else {
            return None;
        };
        restore_project_layouts(projects, *selected, launches, project_exists)
    }
}

fn restore_project_layouts(
    saved_projects: &[SavedProjectLayout],
    selected: Option<i64>,
    saved_launches: &HashMap<u64, PaneLaunch>,
    project_exists: impl Fn(i64) -> bool,
) -> Option<RestoredWorkspace> {
    let mut next_group = 1;
    let mut next_pane = 2;
    let mut group_sizes = HashMap::new();
    let mut layouts = HashMap::new();
    let mut focused_by_project = HashMap::new();
    let mut seen = HashSet::new();
    for saved in saved_projects {
        if !project_exists(saved.project) || layouts.contains_key(&saved.project) {
            continue;
        }
        let ids = saved.layout.pane_ids();
        let mut local_groups = HashMap::new();
        let mut local_next_group = next_group;
        let Some(layout) = saved
            .layout
            .restore(&ids, &mut local_next_group, &mut local_groups)
        else {
            continue;
        };
        let leaves = layout.panes();
        if leaves.is_empty() || leaves.len() > MAX_RESTORED_PANES {
            continue;
        }
        if ids.len() != leaves.len()
            || leaves
                .iter()
                .any(|pane| pane.0 == 0 || pane.0 > MAX_SAVED_PANE_ID || seen.contains(&pane.0))
        {
            continue;
        }
        seen.extend(ids);
        next_group = local_next_group;
        group_sizes.extend(local_groups);
        next_pane = next_pane.max(
            leaves
                .iter()
                .map(|pane| pane.0.saturating_add(1))
                .max()
                .unwrap_or(2),
        );
        focused_by_project.insert(
            saved.project,
            if leaves.contains(&PaneId(saved.focused)) {
                PaneId(saved.focused)
            } else {
                leaves[0]
            },
        );
        layouts.insert(saved.project, layout);
    }
    let selected = selected
        .filter(|project| {
            project_exists(*project)
                && (layouts.contains_key(project)
                    || !saved_projects.iter().any(|saved| saved.project == *project))
        })
        .or_else(|| {
            saved_projects
                .iter()
                .find(|saved| layouts.contains_key(&saved.project))
                .map(|saved| saved.project)
        });
    if selected.is_none() && !saved_projects.is_empty() {
        return None;
    }
    // A launch belongs to the pane tree that survived validation: a stale or
    // duplicate entry would otherwise start a process no pane owns.
    let launches: HashMap<PaneId, PaneLaunch> = saved_launches
        .iter()
        .filter(|(pane, launch)| {
            !launch.is_shell()
                && layouts
                    .values()
                    .any(|layout| layout.panes().contains(&PaneId(**pane)))
        })
        .map(|(pane, launch)| (PaneId(*pane), launch.clone()))
        .collect();
    Some(RestoredWorkspace {
        layouts,
        focused_by_project,
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
            Self::Group { children, .. } => children.iter().flat_map(Self::pane_ids).collect(),
            Self::ProjectLayouts { projects, .. } => projects
                .iter()
                .flat_map(|project| project.layout.pane_ids())
                .collect(),
        }
    }
}

/// Recursive layout of panes in the content area.
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

    /// Every leaf id, depth-first.
    pub fn panes(&self) -> Vec<PaneId> {
        match self {
            Self::Leaf(id) => vec![*id],
            Self::Group { children, .. } => children.iter().flat_map(Self::panes).collect(),
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

    /// Neighbor to focus after `id` is removed: the previous sibling leaf, else
    /// the next, else any remaining leaf.
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
            let Some(i) = children.iter().position(|c| c.is_leaf(target)) else {
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

    fn is_leaf(&self, id: PaneId) -> bool {
        matches!(self, Self::Leaf(leaf) if *leaf == id)
    }

    fn parent_group_id(&self, id: PaneId) -> Option<u64> {
        match self {
            Self::Leaf(_) => None,
            Self::Group {
                id: group_id,
                children,
                ..
            } => {
                if children.iter().any(|c| c.is_leaf(id)) {
                    Some(*group_id)
                } else {
                    children.iter().find_map(|c| c.parent_group_id(id))
                }
            }
        }
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
    if let Some(i) = children.iter().position(|c| c.is_leaf(id)) {
        children.remove(i);
        return true;
    }
    children.iter_mut().any(|c| remove_inner(c, id))
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

impl SavedNode {
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
                .then_some(PaneNode::Leaf(PaneId(*pane))),
            Self::ProjectLayouts { .. } => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(n: u64) -> PaneNode {
        PaneNode::leaf(PaneId(n))
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

    fn two_project_state() -> SavedWorkspace {
        SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: SavedNode::ProjectLayouts {
                selected: Some(20),
                projects: vec![
                    SavedProjectLayout {
                        project: 10,
                        layout: SavedNode::Leaf { pane: 1 },
                        focused: 1,
                    },
                    SavedProjectLayout {
                        project: 20,
                        layout: SavedNode::Leaf { pane: 2 },
                        focused: 2,
                    },
                ],
                launches: HashMap::new(),
            },
            focused: 2,
        }
    }

    /// Restoring keeps only projects that still exist and lands focus on a
    /// surviving pane.
    #[test]
    fn restoring_keeps_only_live_projects() {
        let json = serde_json::to_string(&two_project_state()).unwrap();
        let restored =
            SavedWorkspace::restore(&json, |project| project == 20).expect("one live project");
        assert_eq!(restored.selected, Some(20));
        assert_eq!(
            restored.layouts.get(&20).map(PaneNode::panes),
            Some(vec![PaneId(2)])
        );
        assert_eq!(restored.focused_by_project.get(&20), Some(&PaneId(2)));
        assert_eq!(restored.next_pane, 3);
    }

    #[test]
    fn restoring_keeps_an_empty_project_selected() {
        let mut state = two_project_state();
        if let SavedNode::ProjectLayouts { projects, .. } = &mut state.layout {
            projects.retain(|project| project.project != 20);
        }
        let json = serde_json::to_string(&state).unwrap();
        let restored = SavedWorkspace::restore(&json, |_| true).unwrap();
        assert_eq!(restored.selected, Some(20));
        assert!(!restored.layouts.contains_key(&20));
        assert!(restored.layouts.contains_key(&10));

        if let SavedNode::ProjectLayouts { projects, .. } = &mut state.layout {
            projects.clear();
        }
        let json = serde_json::to_string(&state).unwrap();
        let restored = SavedWorkspace::restore(&json, |_| true).unwrap();
        assert_eq!(restored.selected, Some(20));
        assert!(restored.layouts.is_empty());
    }

    #[test]
    fn an_invalid_project_layout_does_not_discard_later_projects() {
        let mut state = two_project_state();
        if let SavedNode::ProjectLayouts {
            projects, selected, ..
        } = &mut state.layout
        {
            *selected = Some(10);
            projects[0].layout = SavedNode::Group {
                horizontal: true,
                sizes: Vec::new(),
                children: Vec::new(),
            };
        }
        let json = serde_json::to_string(&state).unwrap();
        let restored = SavedWorkspace::restore(&json, |_| true).unwrap();
        assert_eq!(restored.selected, Some(20));
        assert_eq!(restored.layouts.len(), 1);

        if let SavedNode::ProjectLayouts { projects, .. } = &mut state.layout {
            projects[0].layout = SavedNode::Group {
                horizontal: true,
                sizes: Vec::new(),
                children: vec![SavedNode::Leaf { pane: 2 }, SavedNode::Leaf { pane: 2 }],
            };
        }
        let json = serde_json::to_string(&state).unwrap();
        let restored = SavedWorkspace::restore(&json, |_| true).unwrap();
        assert_eq!(restored.selected, Some(20));
        assert_eq!(restored.layouts[&20].panes(), vec![PaneId(2)]);
    }

    /// Malformed JSON, an unknown version, a missing project, and duplicate
    /// pane ids all mean "open the default pane", never a startup failure.
    #[test]
    fn an_untrusted_saved_workspace_is_rejected() {
        assert!(SavedWorkspace::restore("{", |_| true).is_none());

        let state = two_project_state();
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
        duplicate.layout = SavedNode::ProjectLayouts {
            selected: Some(10),
            projects: vec![SavedProjectLayout {
                project: 10,
                layout: SavedNode::Group {
                    horizontal: true,
                    sizes: vec![0.5, 0.5],
                    children: vec![SavedNode::Leaf { pane: 1 }, SavedNode::Leaf { pane: 1 }],
                },
                focused: 1,
            }],
            launches: HashMap::new(),
        };
        let json = serde_json::to_string(&duplicate).unwrap();
        assert!(
            SavedWorkspace::restore(&json, |_| true).is_none(),
            "two leaves cannot share a pane"
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
        assert_eq!(restored.layouts[&3].panes(), vec![PaneId(7)]);

        let saved = SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: SavedNode::ProjectLayouts {
                selected: Some(3),
                projects: vec![SavedProjectLayout {
                    project: 3,
                    layout: SavedNode::Leaf { pane: 7 },
                    focused: 7,
                }],
                launches: HashMap::new(),
            },
            focused: 7,
        };
        let json = serde_json::to_string(&saved).unwrap();
        assert!(!json.contains("launches"), "shells write no launch data");
    }

    #[test]
    fn an_agent_pane_survives_a_save_and_restore() {
        let launch = PaneLaunch::Agent {
            agent: "codex".into(),
            session: Some("session-1".into()),
        };
        let saved = SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: SavedNode::ProjectLayouts {
                selected: Some(3),
                projects: vec![SavedProjectLayout {
                    project: 3,
                    layout: SavedNode::Leaf { pane: 7 },
                    focused: 7,
                }],
                launches: HashMap::from([(7, launch.clone())]),
            },
            focused: 7,
        };
        let json = serde_json::to_string(&saved).unwrap();
        let restored = SavedWorkspace::restore(&json, |_| true).unwrap();
        assert_eq!(restored.launches[&PaneId(7)], launch);
    }

    /// A launch whose pane is no longer in any tree must not start a process
    /// no pane can show.
    #[test]
    fn a_launch_for_a_missing_pane_is_dropped() {
        let saved = SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: SavedNode::ProjectLayouts {
                selected: Some(3),
                projects: vec![SavedProjectLayout {
                    project: 3,
                    layout: SavedNode::Leaf { pane: 7 },
                    focused: 7,
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
