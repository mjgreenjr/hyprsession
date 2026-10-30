//! Computed insertion order for Hyprland's dwindle layout.
//!
//! Dwindle grows a binary tree: inserting a window splits one existing leaf into (old
//! occupant, new window), the new window going second (right / bottom, `force_split = 2`)
//! and the split running along the leaf box's longer side. Spike #2 established which leaf
//! is split: the one nearest the mouse pointer (0 inside it). The tree a given number of
//! windows grows into is therefore a function of the pointer alone, so a saved tree can be
//! reproduced by inserting its windows in the order that makes the compositor grow exactly
//! that tree. If the saved tree is not one the compositor would grow at the current pointer
//! there is no such order and the caller falls back to reading order.
//!
//! Everything here is pure geometry; it never talks to the compositor.

use std::collections::HashMap;

/// x0, y0, x1, y1
pub type Rect = (f64, f64, f64, f64);

/// Pixels of slack when looking for the line between two groups of windows: the gap
/// between neighbours plus rounding (14 px gaps and up to 5 px rounding were measured)
pub const CUT_TOLERANCE: f64 = 24.0;
/// `general:gaps_out`, used when the compositor's value cannot be read
pub const DEFAULT_GAPS_OUT: f64 = 12.0;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Orient {
    /// Side by side (the first child is on the left)
    V,
    /// Stacked (the first child is on top)
    H,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tree {
    Leaf(usize),
    Split(Orient, Box<Tree>, Box<Tree>),
}

/// Split a node box along its longer side
fn split_box(b: Rect) -> (Orient, Rect, Rect) {
    let (x0, y0, x1, y1) = b;
    if (x1 - x0) > (y1 - y0) {
        let xm = (x0 + x1) / 2.0;
        (Orient::V, (x0, y0, xm, y1), (xm, y0, x1, y1))
    } else {
        let ym = (y0 + y1) / 2.0;
        (Orient::H, (x0, y0, x1, ym), (x0, ym, x1, y1))
    }
}

/// Distance from a point to a box (0 inside)
fn distance(p: (f64, f64), b: Rect) -> f64 {
    let dx = (b.0 - p.0).max(0.0).max(p.0 - b.2);
    let dy = (b.1 - p.1).max(0.0).max(p.1 - b.3);
    dx.hypot(dy)
}

/// Rebuild the guillotine tree of window rectangles. `None` when they are not a dwindle
/// partition (overlap, no straight cut, a fullscreen window among them, ...).
pub fn reconstruct(items: &[(usize, Rect)], tol: f64) -> Option<Tree> {
    if items.is_empty() {
        return None;
    }
    if items.len() == 1 {
        return Some(Tree::Leaf(items[0].0));
    }
    let x0 = items.iter().map(|i| i.1 .0).fold(f64::INFINITY, f64::min);
    let y0 = items.iter().map(|i| i.1 .1).fold(f64::INFINITY, f64::min);
    let x1 = items.iter().map(|i| i.1 .2).fold(f64::NEG_INFINITY, f64::max);
    let y1 = items.iter().map(|i| i.1 .3).fold(f64::NEG_INFINITY, f64::max);
    let prefer = if (x1 - x0) > (y1 - y0) { Orient::V } else { Orient::H };
    let other = if prefer == Orient::V { Orient::H } else { Orient::V };
    for orient in [prefer, other] {
        let side = |r: &Rect| if orient == Orient::V { (r.0, r.2) } else { (r.1, r.3) };
        let mut order: Vec<(usize, Rect)> = items.to_vec();
        order.sort_by(|a, b| {
            let (al, ah) = side(&a.1);
            let (bl, bh) = side(&b.1);
            al.partial_cmp(&bl).unwrap().then(ah.partial_cmp(&bh).unwrap())
        });
        let centre = if orient == Orient::V { (x0 + x1) / 2.0 } else { (y0 + y1) / 2.0 };
        let mut best: Option<(f64, usize)> = None;
        for i in 1..order.len() {
            let end = order[..i].iter().map(|it| side(&it.1).1).fold(f64::NEG_INFINITY, f64::max);
            let start = order[i..].iter().map(|it| side(&it.1).0).fold(f64::INFINITY, f64::min);
            if end <= start + tol {
                let score = (((end + start) / 2.0) - centre).abs();
                if best.map_or(true, |(s, _)| score < s) {
                    best = Some((score, i));
                }
            }
        }
        if let Some((_, i)) = best {
            let a = reconstruct(&order[..i], tol)?;
            let b = reconstruct(&order[i..], tol)?;
            return Some(Tree::Split(orient, Box::new(a), Box::new(b)));
        }
    }
    None
}

enum Node {
    Leaf(usize),
    Split(Orient, usize, usize),
}

fn freeze(nodes: &[Node], at: usize) -> Tree {
    match &nodes[at] {
        Node::Leaf(id) => Tree::Leaf(*id),
        Node::Split(o, a, b) => Tree::Split(*o, Box::new(freeze(nodes, *a)), Box::new(freeze(nodes, *b))),
    }
}

/// Grow `n` windows (leaf ids 1..=n are insertion indices) with the pointer at `pointer`.
/// Errors when the pointer is equally near two leaves: the compositor's tie-break is not
/// known, so no order can be trusted.
pub fn simulate(n: usize, pointer: (f64, f64), layout: Rect) -> Result<Tree, String> {
    let mut nodes = vec![Node::Leaf(1)];
    let mut boxes: HashMap<usize, Rect> = HashMap::new(); // node index -> box, leaves only
    boxes.insert(0, layout);
    let mut leaf_node: Vec<usize> = vec![0]; // insertion index - 1 -> node index
    for k in 2..=n {
        let mut ranked: Vec<(f64, usize)> =
            leaf_node.iter().enumerate().map(|(i, &node)| (distance(pointer, boxes[&node]), i)).collect();
        ranked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
        if ranked.len() > 1 && (ranked[1].0 - ranked[0].0).abs() < 1e-6 {
            return Err(format!(
                "the pointer is equally near windows #{} and #{} (on a boundary)",
                ranked[0].1 + 1,
                ranked[1].1 + 1
            ));
        }
        let target = ranked[0].1;
        let t = leaf_node[target];
        let (orient, first, second) = split_box(boxes.remove(&t).expect("leaf box"));
        let old = nodes.len();
        nodes.push(Node::Leaf(target + 1));
        let new = nodes.len();
        nodes.push(Node::Leaf(k));
        nodes[t] = Node::Split(orient, old, new);
        boxes.insert(old, first);
        boxes.insert(new, second);
        leaf_node[target] = old;
        leaf_node.push(new);
    }
    Ok(freeze(&nodes, 0))
}

/// Map saved-tree leaf keys to the insertion indices of the simulated tree when the shapes agree
fn matches(sim: &Tree, saved: &Tree, mapping: &mut HashMap<usize, usize>) -> bool {
    match (sim, saved) {
        (Tree::Leaf(i), Tree::Leaf(key)) => {
            mapping.insert(*key, *i);
            true
        }
        (Tree::Split(so, sa, sb), Tree::Split(vo, va, vb)) => {
            so == vo && matches(sa, va, mapping) && matches(sb, vb, mapping)
        }
        _ => false,
    }
}

/// The layout area the windows were tiled in: their bounding box grown by the outer gap
pub fn layout_box(rects: &[Rect], gaps_out: f64) -> Rect {
    let x0 = rects.iter().map(|r| r.0).fold(f64::INFINITY, f64::min) - gaps_out;
    let y0 = rects.iter().map(|r| r.1).fold(f64::INFINITY, f64::min) - gaps_out;
    let x1 = rects.iter().map(|r| r.2).fold(f64::NEG_INFINITY, f64::max) + gaps_out;
    let y1 = rects.iter().map(|r| r.3).fold(f64::NEG_INFINITY, f64::max) + gaps_out;
    (x0, y0, x1, y1)
}

/// The order in which to insert `rects` (saved tiled windows of one workspace) so that
/// dwindle regrows their tree with the pointer at `pointer`: indices into `rects`, first
/// insertion first. `Err(reason)` when no such order exists.
pub fn computed_order(rects: &[Rect], pointer: (f64, f64), gaps_out: f64, tol: f64) -> Result<Vec<usize>, String> {
    if rects.len() < 2 {
        return Ok((0..rects.len()).collect());
    }
    let items: Vec<(usize, Rect)> = rects.iter().copied().enumerate().collect();
    let saved = reconstruct(&items, tol).ok_or("the saved rectangles are not a dwindle partition")?;
    let sim = simulate(rects.len(), pointer, layout_box(rects, gaps_out))?;
    let mut mapping = HashMap::new();
    if !matches(&sim, &saved, &mut mapping) {
        return Err(format!(
            "the saved tree is not one dwindle grows with the pointer at ({}, {})",
            pointer.0 as i64, pointer.1 as i64
        ));
    }
    let mut order: Vec<usize> = (0..rects.len()).collect();
    order.sort_by_key(|key| mapping[key]);
    Ok(order)
}

/// Saved reading order: x, then y, then position in the snapshot
pub fn reading_order(rects: &[Rect]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rects.len()).collect();
    order.sort_by(|&a, &b| {
        rects[a].0.partial_cmp(&rects[b].0).unwrap().then(rects[a].1.partial_cmp(&rects[b].1).unwrap()).then(a.cmp(&b))
    });
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOX: Rect = (0.0, 26.0, 1920.0, 1080.0);
    const BL: (f64, f64) = (300.0, 900.0);
    const TL: (f64, f64) = (300.0, 200.0);
    const TR: (f64, f64) = (1500.0, 200.0);
    const BR: (f64, f64) = (1500.0, 900.0);

    // Spike #1 fixture "A": d1 d2 d3 d4 inserted in this order with the pointer bottom-left
    fn s4() -> Vec<Rect> {
        vec![
            (12.0, 38.0, 953.0, 546.0),
            (967.0, 38.0, 1908.0, 1068.0),
            (12.0, 560.0, 478.0, 1068.0),
            (492.0, 560.0, 953.0, 1068.0),
        ]
    }

    /// Window rectangles of a tree grown by the model: leaf boxes pulled in 12 px at the
    /// layout edge and 7 px between neighbours, as the compositor's gaps do
    fn grown(n: usize, pointer: (f64, f64)) -> Vec<Rect> {
        fn walk(t: &Tree, b: Rect, out: &mut HashMap<usize, Rect>) {
            match t {
                Tree::Leaf(i) => {
                    let inset = |edge: f64, at: f64| if (edge - at).abs() < 0.5 { 12.0 } else { 7.0 };
                    out.insert(
                        *i,
                        (
                            b.0 + inset(b.0, BOX.0),
                            b.1 + inset(b.1, BOX.1),
                            b.2 - inset(b.2, BOX.2),
                            b.3 - inset(b.3, BOX.3),
                        ),
                    );
                }
                Tree::Split(_, a, c) => {
                    let (_, fa, fb) = split_box(b);
                    walk(a, fa, out);
                    walk(c, fb, out);
                }
            }
        }
        let tree = simulate(n, pointer, BOX).unwrap();
        let mut out = HashMap::new();
        walk(&tree, BOX, &mut out);
        (1..=n).map(|i| out[&i]).collect()
    }

    fn leaf(i: usize) -> Box<Tree> {
        Box::new(Tree::Leaf(i))
    }

    #[test]
    fn test_pointer_rule_reproduces_the_measured_tree() {
        // spike #1: step 2 splits #1, step 3 splits #1, step 4 splits #3
        let tree = simulate(4, BL, BOX).unwrap();
        let expected = Tree::Split(
            Orient::V,
            Box::new(Tree::Split(Orient::H, leaf(1), Box::new(Tree::Split(Orient::V, leaf(3), leaf(4))))),
            leaf(2),
        );
        assert_eq!(tree, expected);
    }

    #[test]
    fn test_top_left_pointer_grows_the_anomaly_shape() {
        // spike #1's reverse/stage-1 cell: targets [1, 1, 1]
        let tree = simulate(4, TL, BOX).unwrap();
        let expected = Tree::Split(
            Orient::V,
            Box::new(Tree::Split(Orient::H, Box::new(Tree::Split(Orient::V, leaf(1), leaf(4))), leaf(3))),
            leaf(2),
        );
        assert_eq!(tree, expected);
    }

    #[test]
    fn test_reconstructs_the_fixture_tree() {
        let items: Vec<(usize, Rect)> = s4().into_iter().enumerate().collect();
        let tree = reconstruct(&items, CUT_TOLERANCE).unwrap();
        let expected = Tree::Split(
            Orient::V,
            Box::new(Tree::Split(Orient::H, leaf(0), Box::new(Tree::Split(Orient::V, leaf(2), leaf(3))))),
            leaf(1),
        );
        assert_eq!(tree, expected);
    }

    #[test]
    fn test_computed_order_of_the_fixture_is_its_build_order() {
        assert_eq!(computed_order(&s4(), BL, 12.0, CUT_TOLERANCE), Ok(vec![0, 1, 2, 3]));
    }

    #[test]
    fn test_computed_order_ignores_the_order_the_rectangles_are_listed_in() {
        // the same four windows listed in reverse: the order names the same windows
        let reversed: Vec<Rect> = s4().into_iter().rev().collect();
        assert_eq!(computed_order(&reversed, BL, 12.0, CUT_TOLERANCE), Ok(vec![3, 2, 1, 0]));
    }

    #[test]
    fn test_mirror_shape_is_computed_when_the_pointer_grows_it_and_not_otherwise() {
        // full-height window on the right, the other two stacked on the left (the s3 shape
        // spike #2 saved: at [967,38] 941x1030). Bottom-left pointer grows it ...
        let shape = vec![
            (12.0, 38.0, 953.0, 546.0),
            (967.0, 38.0, 1908.0, 1068.0),
            (12.0, 560.0, 953.0, 1068.0),
        ];
        assert_eq!(computed_order(&shape, BL, 12.0, CUT_TOLERANCE), Ok(vec![0, 1, 2]));
        // ... a pointer on the right half splits the full-height window instead
        let err = computed_order(&shape, TR, 12.0, CUT_TOLERANCE).unwrap_err();
        assert!(err.contains("not one dwindle grows"), "{err}");
    }

    #[test]
    fn test_full_height_left_shape_is_unreachable_at_the_left_pointer() {
        let shape = vec![
            (12.0, 38.0, 953.0, 1068.0),
            (967.0, 38.0, 1908.0, 546.0),
            (967.0, 560.0, 1908.0, 1068.0),
        ];
        assert!(computed_order(&shape, BL, 12.0, CUT_TOLERANCE).is_err());
        // a pointer on the right grows it: step 2 splits #1, step 3 splits #2
        assert_eq!(computed_order(&shape, TR, 12.0, CUT_TOLERANCE), Ok(vec![0, 1, 2]));
    }

    #[test]
    fn test_every_grown_shape_is_reproduced_for_every_pointer_area_and_count() {
        for pointer in [TL, TR, BL, BR] {
            for n in 2..=7 {
                let rects = grown(n, pointer);
                assert_eq!(
                    computed_order(&rects, pointer, 12.0, CUT_TOLERANCE),
                    Ok((0..n).collect::<Vec<_>>()),
                    "n={n} pointer={pointer:?}"
                );
                // listed back to front, the order names the windows back to front
                let rev: Vec<Rect> = rects.iter().rev().copied().collect();
                let expected: Vec<usize> = (0..n).rev().collect();
                assert_eq!(computed_order(&rev, pointer, 12.0, CUT_TOLERANCE), Ok(expected), "reversed n={n}");
            }
        }
    }

    #[test]
    fn test_a_shape_grown_at_one_pointer_is_refused_at_another_when_the_trees_differ() {
        let rects = grown(4, BL);
        assert!(computed_order(&rects, TL, 12.0, CUT_TOLERANCE).is_err());
    }

    #[test]
    fn test_non_partitions_are_refused() {
        // two overlapping windows
        let overlap = vec![(12.0, 38.0, 1000.0, 1068.0), (900.0, 38.0, 1908.0, 1068.0)];
        let err = computed_order(&overlap, BL, 12.0, CUT_TOLERANCE).unwrap_err();
        assert!(err.contains("not a dwindle partition"), "{err}");
    }

    #[test]
    fn test_pointer_on_a_boundary_is_refused() {
        // x = 960 is the shared edge of the first split's two halves
        let rects = grown(3, BL);
        let err = computed_order(&rects, (960.0, 600.0), 12.0, CUT_TOLERANCE).unwrap_err();
        assert!(err.contains("boundary"), "{err}");
    }

    #[test]
    fn test_cut_tolerance_edges() {
        // two side-by-side windows with a gap just inside / outside the tolerance
        let inside = vec![(12.0, 38.0, 940.0, 1068.0), (940.0 + 23.0, 38.0, 1908.0, 1068.0)];
        assert!(reconstruct(&inside.iter().copied().enumerate().collect::<Vec<_>>(), CUT_TOLERANCE).is_some());
        // overlapping by more than the tolerance has no cut
        let outside = vec![(12.0, 38.0, 960.0, 1068.0), (900.0, 38.0, 1908.0, 1068.0)];
        assert!(reconstruct(&outside.iter().copied().enumerate().collect::<Vec<_>>(), CUT_TOLERANCE).is_none());
    }

    #[test]
    fn test_single_and_empty_sets() {
        assert_eq!(computed_order(&[], BL, 12.0, CUT_TOLERANCE), Ok(vec![]));
        assert_eq!(computed_order(&[(12.0, 38.0, 1908.0, 1068.0)], BL, 12.0, CUT_TOLERANCE), Ok(vec![0]));
    }

    #[test]
    fn test_reading_order_is_x_then_y_then_position() {
        let rects = vec![(500.0, 0.0, 600.0, 10.0), (0.0, 500.0, 10.0, 600.0), (0.0, 0.0, 10.0, 10.0), (0.0, 0.0, 20.0, 20.0)];
        assert_eq!(reading_order(&rects), vec![2, 3, 1, 0]);
    }
}
