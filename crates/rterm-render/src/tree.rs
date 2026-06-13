//! Binary-space-partition tree for pane layout.
//!
//! Generic over the leaf type `L` so the data structure can be tested in
//! isolation without spawning real `Pane`s. Phase 1: standalone types and
//! operations. Phase 2 (next iter) wires this into `Tab`.

use crate::{PaneRect, SplitDir};

/// A path through a `Tree`: each `bool` picks a side of an internal `Split`.
/// `false` = first child (a), `true` = second child (b).
pub type TreePath = Vec<bool>;

/// One internal Split with its bounding rect and the rects of its children.
#[derive(Debug, Clone)]
pub struct SplitFrame {
    pub path: TreePath,
    pub dir: SplitDir,
    pub outer: PaneRect,
    pub a_rect: PaneRect,
    pub b_rect: PaneRect,
}

/// BSP-style pane tree.
///
/// The `Hole` variant is an internal placeholder used during structural
/// mutations (`split_leaf`, `close_leaf`, `swap_leaves`) to satisfy the
/// borrow checker via `mem::replace`. It is **not** part of the
/// observable shape: every public method that returns or accepts a
/// `Tree<L>` guarantees no `Hole` is reachable at rest.
///
/// `#[non_exhaustive]` is intentional — it prevents callers outside
/// the crate from writing exhaustive matches that would have to
/// handle `Hole`. The module itself is `pub(crate)` so external
/// crates can't reach this type at all; the attribute is defence in
/// depth against any future `pub` re-export accidentally widening the
/// surface.
#[derive(Debug)]
#[non_exhaustive]
pub enum Tree<L> {
    Leaf(L),
    Split {
        dir: SplitDir,
        /// Fraction of the parent rect occupied by `a` (the rest goes to `b`).
        ratio: f32,
        a: Box<Tree<L>>,
        b: Box<Tree<L>>,
    },
    /// Internal placeholder used during structural mutations (split, close)
    /// to satisfy the borrow checker. Should not be observed at rest.
    #[doc(hidden)]
    Hole,
}

impl<L> Tree<L> {
    pub fn new(leaf: L) -> Self {
        Tree::Leaf(leaf)
    }

    pub fn is_hole(&self) -> bool {
        matches!(self, Tree::Hole)
    }

    pub fn count_leaves(&self) -> usize {
        match self {
            Tree::Leaf(_) => 1,
            Tree::Split { a, b, .. } => a.count_leaves() + b.count_leaves(),
            Tree::Hole => 0,
        }
    }

    /// Collect all leaves in DFS order (a-first).
    pub fn leaves(&self) -> Vec<&L> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    /// Collect all leaf paths in DFS order (a-first).
    pub fn leaf_paths(&self) -> Vec<TreePath> {
        let mut out = Vec::new();
        self.collect_leaf_paths(Vec::new(), &mut out);
        out
    }

    fn collect_leaf_paths(&self, prefix: TreePath, out: &mut Vec<TreePath>) {
        match self {
            Tree::Leaf(_) => out.push(prefix),
            Tree::Split { a, b, .. } => {
                let mut pa = prefix.clone();
                pa.push(false);
                a.collect_leaf_paths(pa, out);
                let mut pb = prefix;
                pb.push(true);
                b.collect_leaf_paths(pb, out);
            }
            Tree::Hole => {}
        }
    }

    fn collect_leaves<'a>(&'a self, out: &mut Vec<&'a L>) {
        match self {
            Tree::Leaf(l) => out.push(l),
            Tree::Split { a, b, .. } => {
                a.collect_leaves(out);
                b.collect_leaves(out);
            }
            Tree::Hole => {}
        }
    }

    /// Get a shared reference to the subtree at `path` (Leaf, Split or
    /// Hole). Returns `None` if the path descends past a Leaf.
    fn slot_at(&self, path: &[bool]) -> Option<&Tree<L>> {
        let mut node = self;
        for &right in path {
            match node {
                Tree::Split { a, b, .. } => {
                    node = if right { b.as_ref() } else { a.as_ref() };
                }
                _ => return None,
            }
        }
        Some(node)
    }

    /// Get a mutable reference to the *subtree* at `path` (which may be a
    /// Split — used for in-place mutations).
    fn slot_at_mut(&mut self, path: &[bool]) -> Option<&mut Tree<L>> {
        let mut node = self;
        for &right in path {
            match node {
                Tree::Split { a, b, .. } => {
                    node = if right { b.as_mut() } else { a.as_mut() };
                }
                _ => return None,
            }
        }
        Some(node)
    }

    /// Navigate to a leaf by path.
    pub fn leaf_at(&self, path: &[bool]) -> Option<&L> {
        match self.slot_at(path)? {
            Tree::Leaf(l) => Some(l),
            _ => None,
        }
    }

    pub fn leaf_at_mut(&mut self, path: &[bool]) -> Option<&mut L> {
        match self.slot_at_mut(path)? {
            Tree::Leaf(l) => Some(l),
            _ => None,
        }
    }

    /// Replace the leaf at `path` with a new Split whose first child is the
    /// existing leaf and second child is `new`. `ratio` is the size of the
    /// existing leaf in the new split (0.0..1.0). Returns false if the path
    /// doesn't end on a leaf.
    pub fn split_leaf(
        &mut self,
        path: &[bool],
        new: L,
        dir: SplitDir,
        ratio: f32,
    ) -> bool {
        let slot = match self.slot_at_mut(path) {
            Some(s) => s,
            None => return false,
        };
        if !matches!(slot, Tree::Leaf(_)) {
            return false;
        }
        let old = std::mem::replace(slot, Tree::Hole);
        *slot = Tree::Split {
            dir,
            ratio: ratio.clamp(0.05, 0.95),
            a: Box::new(old),
            b: Box::new(Tree::Leaf(new)),
        };
        true
    }

    /// Close (delete) the leaf at `path`. The leaf's parent Split is
    /// replaced by the surviving sibling. Closing the root leaf turns the
    /// tree into `Tree::Hole`. Returns false if the path is invalid.
    pub fn close_leaf(&mut self, path: &[bool]) -> bool {
        if path.is_empty() {
            // Closing the root.
            if matches!(self, Tree::Leaf(_)) {
                *self = Tree::Hole;
                return true;
            }
            return false;
        }
        // Refuse a non-leaf target. The doc contract is "close the LEAF
        // at path"; without this guard, handing an internal-split path
        // would silently delete every leaf in that subtree (the
        // sibling-hoist below would drop a whole branch). All current
        // callers pass leaf paths, but the guard makes the invariant
        // enforced rather than assumed.
        if !matches!(self.slot_at(path), Some(Tree::Leaf(_))) {
            return false;
        }
        let (parent_path, last) = path.split_at(path.len() - 1);
        let close_right = last[0];
        let parent_slot = match self.slot_at_mut(parent_path) {
            Some(s) => s,
            None => return false,
        };
        // Replace parent_slot with the sibling subtree.
        let (taken_a, taken_b, _dir, _ratio) = match std::mem::replace(parent_slot, Tree::Hole) {
            Tree::Split { a, b, dir, ratio } => (*a, *b, dir, ratio),
            other => {
                // Not a split — put it back, fail.
                *parent_slot = other;
                return false;
            }
        };
        let sibling = if close_right { taken_a } else { taken_b };
        *parent_slot = sibling;
        true
    }

    /// Compute pane rects for every leaf. Adjacent splits get `gap` pixels
    /// of space between them.
    pub fn layout(&self, rect: PaneRect, gap: f32) -> Vec<(TreePath, PaneRect, &L)> {
        let mut out = Vec::new();
        self.layout_recursive(rect, gap, Vec::new(), &mut out);
        out
    }

    /// Enumerate every internal Split node with its containing rect and
    /// the rects of its two children. Used for gap hit-testing.
    pub fn splits(&self, rect: PaneRect, gap: f32) -> Vec<SplitFrame> {
        let mut out = Vec::new();
        self.splits_recursive(rect, gap, Vec::new(), &mut out);
        out
    }

    fn splits_recursive(
        &self,
        rect: PaneRect,
        gap: f32,
        path: TreePath,
        out: &mut Vec<SplitFrame>,
    ) {
        if let Tree::Split { dir, ratio, a, b } = self {
            let (ra, rb) = split_rect(rect, *dir, *ratio, gap);
            out.push(SplitFrame {
                path: path.clone(),
                dir: *dir,
                outer: rect,
                a_rect: ra,
                b_rect: rb,
            });
            let mut pa = path.clone();
            pa.push(false);
            a.splits_recursive(ra, gap, pa, out);
            let mut pb = path;
            pb.push(true);
            b.splits_recursive(rb, gap, pb, out);
        }
    }

    /// Rect of the subtree at `path` inside the laid-out `outer` rect.
    pub fn rect_at(&self, outer: PaneRect, gap: f32, path: &[bool]) -> Option<PaneRect> {
        let mut node = self;
        let mut current = outer;
        for &right in path {
            match node {
                Tree::Split { dir, ratio, a, b } => {
                    let (ra, rb) = split_rect(current, *dir, *ratio, gap);
                    if right {
                        current = rb;
                        node = b;
                    } else {
                        current = ra;
                        node = a;
                    }
                }
                _ => return None,
            }
        }
        Some(current)
    }

    /// Reset every Split's ratio to 0.5 — gives the user a quick "balance"
    /// when a chain of resizes has skewed the layout.
    pub fn balance(&mut self) {
        if let Tree::Split { ratio, a, b, .. } = self {
            *ratio = 0.5;
            a.balance();
            b.balance();
        }
    }

    /// Inspect the Split at `path`. Returns `(dir, ratio)` or `None` if the
    /// path points at a Leaf / Hole or is out of range.
    pub fn split_info(&self, path: &[bool]) -> Option<(SplitDir, f32)> {
        match self.slot_at(path)? {
            Tree::Split { dir, ratio, .. } => Some((*dir, *ratio)),
            _ => None,
        }
    }

    /// Set the ratio of the Split at `path`. Returns false if `path` doesn't
    /// point at a Split.
    pub fn set_split_ratio(&mut self, path: &[bool], new_ratio: f32) -> bool {
        match self.slot_at_mut(path) {
            Some(Tree::Split { ratio, .. }) => {
                *ratio = new_ratio.clamp(0.05, 0.95);
                true
            }
            _ => false,
        }
    }

    /// Swap the two leaves at `p_a` and `p_b`. Works structurally — the
    /// `Tree::Leaf` subtree at each slot is moved, no leaf data is copied or
    /// defaulted — so `L` doesn't need `Default`. Returns `false` if either
    /// path is invalid, not a leaf, or both paths point at the same leaf.
    pub fn swap_leaves(&mut self, p_a: &[bool], p_b: &[bool]) -> bool {
        if p_a == p_b {
            return false;
        }
        // Validate both targets are reachable leaves *before* touching the
        // tree. A leaf path can't be a prefix of any other valid path (the
        // traversal stops at a Leaf), so the two physical slots are
        // guaranteed independent — every `slot_at_mut` below is sound.
        if self.leaf_at(p_a).is_none() || self.leaf_at(p_b).is_none() {
            return false;
        }
        let leaf_a = std::mem::replace(
            self.slot_at_mut(p_a).expect("validated above"),
            Tree::Hole,
        );
        let leaf_b = std::mem::replace(
            self.slot_at_mut(p_b).expect("validated above"),
            leaf_a,
        );
        *self.slot_at_mut(p_a).expect("validated above") = leaf_b;
        true
    }

    fn layout_recursive<'a>(
        &'a self,
        rect: PaneRect,
        gap: f32,
        path: TreePath,
        out: &mut Vec<(TreePath, PaneRect, &'a L)>,
    ) {
        match self {
            Tree::Leaf(l) => out.push((path, rect, l)),
            Tree::Split { dir, ratio, a, b } => {
                let (ra, rb) = split_rect(rect, *dir, *ratio, gap);
                let mut pa = path.clone();
                pa.push(false);
                a.layout_recursive(ra, gap, pa, out);
                let mut pb = path;
                pb.push(true);
                b.layout_recursive(rb, gap, pb, out);
            }
            Tree::Hole => {}
        }
    }
}

fn split_rect(rect: PaneRect, dir: SplitDir, ratio: f32, gap: f32) -> (PaneRect, PaneRect) {
    let r = ratio.clamp(0.0, 1.0);
    match dir {
        SplitDir::Horizontal => {
            // Partition `inner` EXACTLY between the two children:
            // `wa + wb == inner`, so `b.left + b.width` always lands on
            // `rect.left + rect.width`. The old `.max(1.0)` on BOTH
            // halves summed to more than `inner` for a sub-gap-wide
            // parent (deep nesting), pushing the b-child past the
            // parent's right edge into a neighbour. A degenerate
            // zero-width child is fine — `sync_terminal_size` skips
            // panes under half a pixel.
            // Clamp the gap to the parent width too: a parent narrower
            // than the gap (absurd nesting) would otherwise place the
            // b-child's left past its own right edge.
            let g = gap.min(rect.width.max(0.0));
            let inner = (rect.width - g).max(0.0);
            let wa = (inner * r).clamp(0.0, inner);
            let wb = inner - wa;
            (
                PaneRect { left: rect.left, top: rect.top, width: wa, height: rect.height },
                PaneRect {
                    left: rect.left + wa + g,
                    top: rect.top,
                    width: wb,
                    height: rect.height,
                },
            )
        }
        SplitDir::Vertical => {
            let g = gap.min(rect.height.max(0.0));
            let inner = (rect.height - g).max(0.0);
            let ha = (inner * r).clamp(0.0, inner);
            let hb = inner - ha;
            (
                PaneRect { left: rect.left, top: rect.top, width: rect.width, height: ha },
                PaneRect {
                    left: rect.left,
                    top: rect.top + ha + g,
                    width: rect.width,
                    height: hb,
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: f32, h: f32) -> PaneRect {
        PaneRect { left: 0.0, top: 0.0, width: w, height: h }
    }

    #[test]
    fn new_tree_has_one_leaf() {
        let t: Tree<u32> = Tree::new(42);
        assert_eq!(t.count_leaves(), 1);
        assert_eq!(t.leaves(), vec![&42]);
        assert_eq!(t.leaf_at(&[]).copied(), Some(42));
    }

    #[test]
    fn close_leaf_refuses_non_leaf_path() {
        // `[false]` addresses the internal split, not a leaf. Closing
        // it must fail and leave the tree intact rather than deleting
        // the whole sub-branch.
        let mut t = Tree::new(1u32);
        assert!(t.split_leaf(&[], 2, SplitDir::Horizontal, 0.5));
        assert!(t.split_leaf(&[false], 3, SplitDir::Vertical, 0.5));
        // Now `[false]` is a Split{1,3}; `[false,false]`=1, etc.
        assert_eq!(t.count_leaves(), 3);
        assert!(!t.close_leaf(&[false]), "non-leaf path must be rejected");
        assert_eq!(t.count_leaves(), 3, "tree unchanged after refusal");
        // A real leaf path still closes.
        assert!(t.close_leaf(&[false, false]));
        assert_eq!(t.count_leaves(), 2);
    }

    #[test]
    fn split_rect_children_never_exceed_parent() {
        // Even at a sub-gap-wide parent the b-child's right edge must
        // not poke past the parent (which overlapped a neighbour).
        for w in [1.0_f32, 2.0, 3.0, 4.0, 50.0] {
            let (a, b) = split_rect(rect(w, 10.0), SplitDir::Horizontal, 0.5, 3.0);
            let parent_right = w;
            assert!(
                b.left + b.width <= parent_right + 0.01,
                "w={w}: b right {} > parent {parent_right}",
                b.left + b.width
            );
            assert!(a.width >= 0.0 && b.width >= 0.0);
        }
        // Vertical mirror.
        let (a, b) = split_rect(rect(10.0, 2.0), SplitDir::Vertical, 0.5, 3.0);
        assert!(b.top + b.height <= 2.0 + 0.01);
        assert!(a.height >= 0.0 && b.height >= 0.0);
    }

    #[test]
    fn split_creates_two_leaves() {
        let mut t = Tree::new(1u32);
        assert!(t.split_leaf(&[], 2, SplitDir::Horizontal, 0.5));
        assert_eq!(t.count_leaves(), 2);
        assert_eq!(t.leaf_at(&[false]).copied(), Some(1));
        assert_eq!(t.leaf_at(&[true]).copied(), Some(2));
        assert_eq!(t.leaves(), vec![&1, &2]);
    }

    #[test]
    fn nested_split_4_leaves_in_2x2() {
        let mut t = Tree::new(1u32);
        // Horizontal split first → [a=1, b=2]
        assert!(t.split_leaf(&[], 2, SplitDir::Horizontal, 0.5));
        // Vertical split on the LEFT leaf → [a=Split{1, 3}, b=2]
        assert!(t.split_leaf(&[false], 3, SplitDir::Vertical, 0.5));
        // Vertical split on the RIGHT leaf → [a=Split{1,3}, b=Split{2,4}]
        assert!(t.split_leaf(&[true], 4, SplitDir::Vertical, 0.5));
        assert_eq!(t.count_leaves(), 4);
        assert_eq!(t.leaves(), vec![&1, &3, &2, &4]);
    }

    #[test]
    fn close_leaf_collapses_parent_split() {
        let mut t = Tree::new(1u32);
        t.split_leaf(&[], 2, SplitDir::Horizontal, 0.5);
        // Close the right leaf — sibling (1) becomes root.
        assert!(t.close_leaf(&[true]));
        assert_eq!(t.count_leaves(), 1);
        assert_eq!(t.leaf_at(&[]).copied(), Some(1));
    }

    #[test]
    fn close_root_leaf_yields_hole() {
        let mut t = Tree::new(99u32);
        assert!(t.close_leaf(&[]));
        assert!(t.is_hole());
        assert_eq!(t.count_leaves(), 0);
    }

    #[test]
    fn layout_horizontal_50_50_no_gap() {
        let mut t = Tree::new(1u32);
        t.split_leaf(&[], 2, SplitDir::Horizontal, 0.5);
        let cells = t.layout(rect(100.0, 50.0), 0.0);
        assert_eq!(cells.len(), 2);
        let (_, ra, _) = &cells[0];
        let (_, rb, _) = &cells[1];
        assert!((ra.width - 50.0).abs() < 0.01);
        assert!((rb.width - 50.0).abs() < 0.01);
        assert!((ra.left - 0.0).abs() < 0.01);
        assert!((rb.left - 50.0).abs() < 0.01);
        assert!((ra.height - 50.0).abs() < 0.01);
    }

    #[test]
    fn layout_vertical_split_divides_height() {
        let mut t = Tree::new(1u32);
        t.split_leaf(&[], 2, SplitDir::Vertical, 0.25);
        let cells = t.layout(rect(80.0, 100.0), 0.0);
        let (_, ra, _) = &cells[0];
        let (_, rb, _) = &cells[1];
        assert!((ra.height - 25.0).abs() < 0.01);
        assert!((rb.height - 75.0).abs() < 0.01);
        assert!((ra.top - 0.0).abs() < 0.01);
        assert!((rb.top - 25.0).abs() < 0.01);
    }

    #[test]
    fn layout_paths_correspond_to_leaves() {
        let mut t = Tree::new(10u32);
        t.split_leaf(&[], 20, SplitDir::Horizontal, 0.5);
        t.split_leaf(&[false], 30, SplitDir::Vertical, 0.5);
        let cells = t.layout(rect(100.0, 100.0), 0.0);
        // Paths in DFS order: [false,false]=10, [false,true]=30, [true]=20
        let paths: Vec<&TreePath> = cells.iter().map(|(p, _, _)| p).collect();
        assert_eq!(paths.len(), 3);
        assert_eq!(paths[0], &vec![false, false]);
        assert_eq!(paths[1], &vec![false, true]);
        assert_eq!(paths[2], &vec![true]);
        let leaves: Vec<&u32> = cells.iter().map(|(_, _, l)| *l).collect();
        assert_eq!(leaves, vec![&10, &30, &20]);
    }

    #[test]
    fn split_invalid_path_returns_false() {
        let mut t = Tree::new(1u32);
        // Path goes through a leaf — can't descend further.
        assert!(!t.split_leaf(&[false], 2, SplitDir::Horizontal, 0.5));
    }

    #[test]
    fn splits_enumerates_internal_nodes() {
        let mut t = Tree::new(1u32);
        t.split_leaf(&[], 2, SplitDir::Horizontal, 0.5);
        t.split_leaf(&[false], 3, SplitDir::Vertical, 0.5);
        let frames = t.splits(rect(100.0, 100.0), 0.0);
        // Two internal Splits: root H + nested V on a-side.
        assert_eq!(frames.len(), 2);
        // Root frame at path [] with horizontal dir.
        assert_eq!(frames[0].path, Vec::<bool>::new());
        assert!(matches!(frames[0].dir, SplitDir::Horizontal));
        // Nested frame at path [false] with vertical dir.
        assert_eq!(frames[1].path, vec![false]);
        assert!(matches!(frames[1].dir, SplitDir::Vertical));
    }

    #[test]
    fn rect_at_returns_subtree_rect() {
        let mut t = Tree::new(1u32);
        t.split_leaf(&[], 2, SplitDir::Horizontal, 0.5);
        let r = t.rect_at(rect(100.0, 50.0), 0.0, &[true]).unwrap();
        assert!((r.left - 50.0).abs() < 0.01);
        assert!((r.width - 50.0).abs() < 0.01);
    }

    #[test]
    fn set_split_ratio_updates_layout() {
        let mut t = Tree::new(1u32);
        t.split_leaf(&[], 2, SplitDir::Horizontal, 0.5);
        assert!(t.set_split_ratio(&[], 0.75));
        let cells = t.layout(rect(100.0, 50.0), 0.0);
        assert!((cells[0].1.width - 75.0).abs() < 0.01);
        assert!((cells[1].1.width - 25.0).abs() < 0.01);
        // Invalid path → false.
        assert!(!t.set_split_ratio(&[false], 0.5));
    }

    #[test]
    fn close_invalid_path_returns_false() {
        let mut t = Tree::new(1u32);
        // No parent for the root leaf at a non-empty path.
        assert!(!t.close_leaf(&[true]));
    }

    #[test]
    fn balance_resets_all_ratios_to_half() {
        let mut t = Tree::new(1u32);
        assert!(t.split_leaf(&[], 2, SplitDir::Horizontal, 0.2));
        assert!(t.split_leaf(&[true], 3, SplitDir::Vertical, 0.8));
        t.balance();
        let (_, r0) = t.split_info(&[]).unwrap();
        let (_, r1) = t.split_info(&[true]).unwrap();
        assert!((r0 - 0.5).abs() < 1e-6);
        assert!((r1 - 0.5).abs() < 1e-6);
    }

    #[test]
    fn split_info_reports_dir_and_ratio() {
        let mut t = Tree::new(1u32);
        assert!(t.split_info(&[]).is_none());
        assert!(t.split_leaf(&[], 2, SplitDir::Horizontal, 0.3));
        let (dir, ratio) = t.split_info(&[]).expect("root is now a split");
        assert_eq!(dir, SplitDir::Horizontal);
        assert!((ratio - 0.3).abs() < 1e-6);
        // Leaf path returns None.
        assert!(t.split_info(&[false]).is_none());
    }

    #[test]
    fn swap_leaves_exchanges_two_siblings() {
        let mut t = Tree::new(1u32);
        assert!(t.split_leaf(&[], 2, SplitDir::Horizontal, 0.5));
        assert_eq!(t.leaves(), vec![&1, &2]);
        assert!(t.swap_leaves(&[false], &[true]));
        assert_eq!(t.leaves(), vec![&2, &1]);
    }

    #[test]
    fn swap_leaves_works_across_subtrees() {
        let mut t = Tree::new(1u32);
        t.split_leaf(&[], 2, SplitDir::Horizontal, 0.5);
        t.split_leaf(&[false], 3, SplitDir::Vertical, 0.5);
        t.split_leaf(&[true], 4, SplitDir::Vertical, 0.5);
        // 2x2 layout, dfs order: 1, 3, 2, 4
        assert_eq!(t.leaves(), vec![&1, &3, &2, &4]);
        // Swap the top-left (1 at [false,false]) with bottom-right (4 at [true,true]).
        assert!(t.swap_leaves(&[false, false], &[true, true]));
        assert_eq!(t.leaves(), vec![&4, &3, &2, &1]);
    }

    #[test]
    fn swap_leaves_rejects_same_path() {
        let mut t = Tree::new(1u32);
        assert!(!t.swap_leaves(&[], &[]));
        t.split_leaf(&[], 2, SplitDir::Horizontal, 0.5);
        assert!(!t.swap_leaves(&[false], &[false]));
    }

    #[test]
    fn swap_leaves_rejects_invalid_path() {
        let mut t = Tree::new(1u32);
        t.split_leaf(&[], 2, SplitDir::Horizontal, 0.5);
        // Path that goes past a leaf is invalid.
        assert!(!t.swap_leaves(&[false], &[false, true]));
        // Path to a non-existent side is invalid (root only has one split, so
        // [true, true] descends into a leaf at [true], then asks for the
        // "right" child of a leaf — not a split).
        assert!(!t.swap_leaves(&[false], &[true, true]));
    }

    #[test]
    fn swap_leaves_handles_deep_asymmetric_paths() {
        // Build a deliberately lopsided tree so the two leaves sit at
        // different depths and on different sides — exercises the
        // "paths are independent" property the new mem::replace shape
        // relies on:
        //
        //          [root: H]
        //          /        \
        //       [v]          4 (at [true])
        //      /   \
        //     1     [v]      (at [false, false] = 1)
        //          /   \
        //         2     3    (at [false, true, false] = 2,
        //                     [false, true, true] = 3)
        let mut t = Tree::new(1u32);
        t.split_leaf(&[], 4, SplitDir::Horizontal, 0.5);
        t.split_leaf(&[false], 2, SplitDir::Vertical, 0.5);
        t.split_leaf(&[false, true], 3, SplitDir::Vertical, 0.5);
        // DFS order: 1, 2, 3, 4.
        assert_eq!(t.leaves(), vec![&1, &2, &3, &4]);
        // Swap the deepest left leaf (2 at [false, true, false]) with
        // the shallow right leaf (4 at [true]).
        assert!(t.swap_leaves(&[false, true, false], &[true]));
        assert_eq!(t.leaves(), vec![&1, &4, &3, &2]);
        // Swap back to make sure the operation round-trips.
        assert!(t.swap_leaves(&[true], &[false, true, false]));
        assert_eq!(t.leaves(), vec![&1, &2, &3, &4]);
    }

    #[test]
    fn swap_leaves_no_op_when_path_is_internal_split() {
        let mut t = Tree::new(1u32);
        t.split_leaf(&[], 2, SplitDir::Horizontal, 0.5);
        t.split_leaf(&[false], 3, SplitDir::Vertical, 0.5);
        // [false] now points at a Split, not a Leaf.
        assert!(t.split_info(&[false]).is_some());
        assert!(!t.swap_leaves(&[false], &[true]));
        // Tree must be unchanged.
        assert_eq!(t.leaves(), vec![&1, &3, &2]);
    }
}
