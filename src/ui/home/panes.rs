//! Pane layout tree: leaves hold a pane id; groups split along an axis.
//!
//! Each sidebar session owns a pane tree. Splitting adds another pane to that
//! session; switching sessions restores its own tree.

use std::collections::{HashMap, HashSet};

use gpui_kit::Axis;

/// Stable id for one terminal pane in the home window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

/// Structure version of [`SavedWorkspace`]. Bump it when the JSON shape
/// changes; an unknown version is ignored rather than guessed at.
pub const WORKSPACE_STATE_VERSION: u32 = 2;

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
    /// Version 2 payload: independent layouts owned by their sessions.
    SessionLayouts {
        selected: Option<i64>,
        sessions: Vec<SavedSessionLayout>,
    },
}

/// One saved pane → session binding in the legacy single-tree format.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SavedPane {
    pub pane: u64,
    pub session: i64,
}

/// One session's saved pane tree and last focused pane.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SavedSessionLayout {
    pub session: i64,
    pub layout: SavedNode,
    pub focused: u64,
}

/// The home window's saved session layouts.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SavedWorkspace {
    pub version: u32,
    pub layout: SavedNode,
    pub panes: Vec<SavedPane>,
    pub focused: u64,
    #[serde(default)]
    pub collapsed_projects: Vec<i64>,
}

/// Persisted layouts that passed validation.
pub struct RestoredWorkspace {
    /// Compatibility view of the selected session's layout.
    pub layout: PaneNode,
    /// Compatibility mapping of the selected session's leaves.
    pub sessions: HashMap<u64, i64>,
    pub focused: PaneId,
    pub layouts: HashMap<i64, PaneNode>,
    pub focused_by_session: HashMap<i64, PaneId>,
    pub selected: Option<i64>,
    pub collapsed_projects: Vec<i64>,
    /// Group id → normalized child sizes. Every restored group has an entry;
    /// an empty vector means "no usable sizes, divide evenly".
    pub group_sizes: HashMap<u64, Vec<f32>>,
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
    pub fn restore(
        saved: &str,
        session_exists: impl Fn(i64) -> bool,
        project_exists: impl Fn(i64) -> bool,
    ) -> Option<RestoredWorkspace> {
        let state: Self = serde_json::from_str(saved).ok()?;
        if state.version != WORKSPACE_STATE_VERSION && state.version != 1 {
            return None;
        }
        if let SavedNode::SessionLayouts { selected, sessions } = &state.layout {
            if state.version != WORKSPACE_STATE_VERSION {
                return None;
            }
            return restore_session_layouts(
                sessions,
                *selected,
                &state.collapsed_projects,
                session_exists,
                project_exists,
            );
        }
        // Migrate older global pane trees by retaining each session's leaves
        // and collapsing groups that only contain panes from other sessions.
        let mut grouped: HashMap<i64, HashSet<u64>> = HashMap::new();
        for pane in &state.panes {
            if session_exists(pane.session) {
                grouped.entry(pane.session).or_default().insert(pane.pane);
            }
        }
        let mut next_group = 1;
        let mut group_sizes = HashMap::new();
        let mut layouts = HashMap::new();
        let mut focused_by_session = HashMap::new();
        let mut seen_panes = HashSet::new();
        for (session, valid_panes) in grouped {
            let mut local_groups = HashMap::new();
            let mut local_next_group = next_group;
            let layout =
                state
                    .layout
                    .restore(&valid_panes, &mut local_next_group, &mut local_groups)?;
            let leaves = layout.panes();
            if leaves.is_empty() || leaves.len() > MAX_RESTORED_PANES {
                continue;
            }
            if leaves
                .iter()
                .any(|pane| pane.0 == 0 || pane.0 > MAX_SAVED_PANE_ID || !seen_panes.insert(pane.0))
            {
                continue;
            }
            next_group = local_next_group;
            group_sizes.extend(local_groups);
            let focused = if leaves.contains(&PaneId(state.focused)) {
                PaneId(state.focused)
            } else {
                leaves[0]
            };
            focused_by_session.insert(session, focused);
            layouts.insert(session, layout);
        }
        let selected = state
            .panes
            .iter()
            .find(|pane| pane.pane == state.focused && layouts.contains_key(&pane.session))
            .map(|pane| pane.session)
            .or_else(|| layouts.keys().next().copied());
        let collapsed_projects = state
            .collapsed_projects
            .iter()
            .copied()
            .filter(|id| project_exists(*id))
            .collect();
        let next_pane = seen_panes
            .iter()
            .copied()
            .max()
            .unwrap_or(1)
            .saturating_add(1)
            .max(2);
        let selected = selected?;
        let (layout, sessions, focused) = if state.version == 1 {
            let valid: HashSet<u64> = state
                .panes
                .iter()
                .filter(|pane| session_exists(pane.session))
                .map(|pane| pane.pane)
                .collect();
            let mut legacy_groups = HashMap::new();
            let mut legacy_next = 1;
            let legacy_layout =
                state
                    .layout
                    .restore(&valid, &mut legacy_next, &mut legacy_groups)?;
            for (group, sizes) in legacy_groups {
                group_sizes.entry(group).or_insert(sizes);
            }
            let legacy_focus = if legacy_layout.panes().contains(&PaneId(state.focused)) {
                PaneId(state.focused)
            } else {
                legacy_layout.panes()[0]
            };
            let bindings = state
                .panes
                .iter()
                .filter(|pane| session_exists(pane.session))
                .map(|pane| (pane.pane, pane.session))
                .collect();
            (legacy_layout, bindings, legacy_focus)
        } else {
            let layout = layouts.get(&selected)?.clone();
            let focused = focused_by_session
                .get(&selected)
                .copied()
                .unwrap_or(PaneId(1));
            let sessions = layout
                .panes()
                .into_iter()
                .map(|pane| (pane.0, selected))
                .collect();
            (layout, sessions, focused)
        };
        Some(RestoredWorkspace {
            layout,
            sessions,
            focused,
            layouts,
            focused_by_session,
            selected: Some(selected),
            collapsed_projects,
            group_sizes,
            next_pane,
            next_group,
        })
    }
}

fn restore_session_layouts(
    saved_sessions: &[SavedSessionLayout],
    selected: Option<i64>,
    collapsed_projects: &[i64],
    session_exists: impl Fn(i64) -> bool,
    project_exists: impl Fn(i64) -> bool,
) -> Option<RestoredWorkspace> {
    let mut next_group = 1;
    let mut next_pane = 2;
    let mut group_sizes = HashMap::new();
    let mut layouts = HashMap::new();
    let mut focused_by_session = HashMap::new();
    let mut seen = HashSet::new();
    for saved in saved_sessions {
        if !session_exists(saved.session) || layouts.contains_key(&saved.session) {
            continue;
        }
        let ids = saved.layout.pane_ids();
        let mut local_groups = HashMap::new();
        let mut local_next_group = next_group;
        let layout = saved
            .layout
            .restore(&ids, &mut local_next_group, &mut local_groups)?;
        let leaves = layout.panes();
        if leaves.is_empty() || leaves.len() > MAX_RESTORED_PANES {
            continue;
        }
        if leaves
            .iter()
            .any(|pane| pane.0 == 0 || pane.0 > MAX_SAVED_PANE_ID || !seen.insert(pane.0))
        {
            continue;
        }
        next_group = local_next_group;
        group_sizes.extend(local_groups);
        next_pane = next_pane.max(
            leaves
                .iter()
                .map(|pane| pane.0.saturating_add(1))
                .max()
                .unwrap_or(2),
        );
        focused_by_session.insert(
            saved.session,
            if leaves.contains(&PaneId(saved.focused)) {
                PaneId(saved.focused)
            } else {
                leaves[0]
            },
        );
        layouts.insert(saved.session, layout);
    }
    let selected = selected
        .filter(|session| layouts.contains_key(session))
        .or_else(|| layouts.keys().next().copied())?;
    let layout = layouts.get(&selected)?.clone();
    let focused = focused_by_session
        .get(&selected)
        .copied()
        .unwrap_or(PaneId(1));
    let sessions = layout
        .panes()
        .into_iter()
        .map(|pane| (pane.0, selected))
        .collect();
    Some(RestoredWorkspace {
        layout,
        sessions,
        focused,
        layouts,
        focused_by_session,
        selected: Some(selected),
        collapsed_projects: collapsed_projects
            .iter()
            .copied()
            .filter(|id| project_exists(*id))
            .collect(),
        group_sizes,
        next_pane,
        next_group,
    })
}

impl SavedNode {
    fn pane_ids(&self) -> HashSet<u64> {
        match self {
            Self::Leaf { pane } => HashSet::from([*pane]),
            Self::Group { children, .. } => children.iter().flat_map(Self::pane_ids).collect(),
            Self::SessionLayouts { sessions, .. } => sessions
                .iter()
                .flat_map(|session| session.layout.pane_ids())
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
    /// Rebuild the layout, keeping only panes in `valid` (their session still
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
            Self::SessionLayouts { .. } => None,
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

    /// A layout written before group ratios existed must still open; the new
    /// field defaults to an even split.
    #[test]
    fn a_group_saved_before_ratios_still_parses() {
        let json = r#"{
            "version": 1,
            "layout": {
                "Group": {
                    "horizontal": true,
                    "children": [
                        {"Leaf": {"pane": 1}},
                        {"Leaf": {"pane": 2}}
                    ]
                }
            },
            "panes": [
                {"pane": 1, "session": 10},
                {"pane": 2, "session": 20}
            ],
            "focused": 1
        }"#;
        let saved: SavedWorkspace = serde_json::from_str(json).unwrap();
        let restored =
            SavedWorkspace::restore(&serde_json::to_string(&saved).unwrap(), |_| true, |_| true)
                .expect("the old layout is still usable");
        assert_eq!(restored.layout.panes(), vec![PaneId(1), PaneId(2)]);
        assert_eq!(restored.group_sizes.get(&1), Some(&Vec::new()));
    }

    /// A deleted session takes its pane with it; groups that lose all but one
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

    /// The JSON shape is what the home window persists; a version bump is the
    /// only supported way to change it.
    #[test]
    fn the_saved_workspace_serializes_with_its_version() {
        let saved = SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: SavedNode::Leaf { pane: 7 },
            panes: vec![SavedPane {
                pane: 7,
                session: 3,
            }],
            focused: 7,
            collapsed_projects: vec![2],
        };
        let json = serde_json::to_string(&saved).unwrap();
        assert_eq!(
            serde_json::from_str::<SavedWorkspace>(&json).unwrap(),
            saved
        );
        assert!(json.contains(&format!("\"version\":{WORKSPACE_STATE_VERSION}")));
    }

    fn two_pane_state() -> SavedWorkspace {
        SavedWorkspace {
            version: WORKSPACE_STATE_VERSION,
            layout: SavedNode::Group {
                horizontal: true,
                sizes: vec![0.5, 0.5],
                children: vec![SavedNode::Leaf { pane: 1 }, SavedNode::Leaf { pane: 2 }],
            },
            panes: vec![
                SavedPane {
                    pane: 1,
                    session: 10,
                },
                SavedPane {
                    pane: 2,
                    session: 20,
                },
            ],
            focused: 2,
            collapsed_projects: vec![3, 4],
        }
    }

    /// Restoring keeps only live sessions, lands focus on a surviving pane, and
    /// drops collapsed-project ids that no longer exist.
    #[test]
    fn restoring_keeps_only_live_sessions_and_projects() {
        let json = serde_json::to_string(&two_pane_state()).unwrap();
        let restored =
            SavedWorkspace::restore(&json, |session| session == 20, |project| project == 3)
                .expect("one live pane");
        assert_eq!(restored.layout.panes(), vec![PaneId(2)]);
        assert_eq!(restored.focused, PaneId(2));
        assert_eq!(restored.collapsed_projects, vec![3]);
        assert_eq!(restored.next_pane, 3);
    }

    /// Malformed JSON, an unknown version, all-dead sessions, and duplicate
    /// pane ids all mean "open the default pane", never a startup failure.
    #[test]
    fn an_untrusted_saved_workspace_is_rejected() {
        assert!(SavedWorkspace::restore("{", |_| true, |_| true).is_none());

        let state = two_pane_state();
        let json = serde_json::to_string(&state).unwrap();
        assert!(
            SavedWorkspace::restore(&json, |_| false, |_| true).is_none(),
            "every session is gone"
        );

        let mut future = state.clone();
        future.version = WORKSPACE_STATE_VERSION + 1;
        let json = serde_json::to_string(&future).unwrap();
        assert!(
            SavedWorkspace::restore(&json, |_| true, |_| true).is_none(),
            "an unknown structure version is not guessed at"
        );

        let mut duplicate = state;
        duplicate.layout = SavedNode::Group {
            horizontal: true,
            sizes: vec![0.5, 0.5],
            children: vec![SavedNode::Leaf { pane: 1 }, SavedNode::Leaf { pane: 1 }],
        };
        let json = serde_json::to_string(&duplicate).unwrap();
        assert!(
            SavedWorkspace::restore(&json, |_| true, |_| true).is_none(),
            "two leaves cannot share a pane"
        );
    }
}
