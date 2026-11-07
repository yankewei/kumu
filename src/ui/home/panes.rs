//! Pane layout tree: leaves hold a pane id; groups split along an axis.
//!
//! A pane shows one session. Splitting is a view-layout change only — sessions
//! stay owned by their project. Same-axis splits reuse the parent; otherwise
//! the target is wrapped (herdr / Zed style).

use gpui_kit::Axis;

/// Stable id for one terminal pane in the home window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

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
}
