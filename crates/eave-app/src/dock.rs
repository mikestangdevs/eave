//! The well's layout tree, and where a carried pane would land.
//!
//! # Why this is a tree and not a list
//! `Split` was a flat `Vec` of panes with one axis, and the operator's
//! own acceptance scenario cannot be *set up* in that model: it opens on
//! `A | (B / C)` — one pane down the left, two stacked on the right —
//! which is a split inside a split. The flat list could describe a row or
//! a column and nothing else, so *"exist in combinations of horizontal
//! and vertical splits"* was a shape the product could not hold.
//!
//! # Why the geometry is computed here rather than measured
//! Every answer this module gives is a function of the well's rectangle
//! and the tree: which pane is under the pointer, which of its edges is
//! nearest, where the insertion line goes. Deriving them from one rect
//! keeps the preview and the drop reading the *same* arithmetic — a
//! preview that can disagree with the landing is worse than no preview —
//! and it makes all of it testable without a window.
//!
//! The well rect itself IS measured (`ui::Shell::well_rect`), because the
//! arithmetic version of it was wrong in two places already: it counted
//! the divider at `RAIL_HANDLE_W` instead of `SPLIT_HANDLE_W` and it
//! assumed nothing was ever above the well.

use std::fmt::Write as _;

/// A rectangle in whatever space the caller is working in. The docking
/// functions take window coordinates and answer in them; [`Node::rects`]
/// takes the well and answers inside it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.right() && y >= self.y && y <= self.bottom()
    }

    /// Squared distance from `(x, y)` to the nearest point of the rect —
    /// zero inside it. Squared because it is only ever compared.
    pub fn dist2(&self, x: f32, y: f32) -> f32 {
        let dx = (self.x - x).max(0.0).max(x - self.right());
        let dy = (self.y - y).max(0.0).max(y - self.bottom());
        dx * dx + dy * dy
    }

    /// The rect moved into `space`'s coordinates — window to well-local.
    pub fn relative_to(&self, space: &Rect) -> Rect {
        Rect::new(self.x - space.x, self.y - space.y, self.w, self.h)
    }
}

/// Which edge of a pane — or of the whole well — a drop is against.
///
/// **A drop names an axis as well as a side.** Ruled on 2026-09-10:
/// *"i should be able to grab edges and put the above/below other tabs."*
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropEdge {
    Left,
    Right,
    Top,
    Bottom,
}

impl DropEdge {
    /// Does the dropped pane land AFTER the host in reading order?
    pub fn after(self) -> bool {
        matches!(self, DropEdge::Right | DropEdge::Bottom)
    }

    /// Is the split this drop makes stacked rather than side by side?
    pub fn vertical(self) -> bool {
        matches!(self, DropEdge::Top | DropEdge::Bottom)
    }
}

/// The edge a pointer at `(x_frac, y_frac)` of a box is nearest.
///
/// Four triangular zones meeting at the centre: the pointer's distance to
/// the nearest vertical edge is measured against its distance to the
/// nearest horizontal one, **each as a fraction of its own axis**, so the
/// zones do not skew when the box is much wider than it is tall — which
/// is the ordinary case here and would otherwise make top and bottom
/// almost unreachable.
///
/// Every point answers, including the centre. A dead middle would mean a
/// pointer over the heart of a pane showed no insertion line at all, and
/// *"no valid drop result"* is meant for the space outside the well, not
/// for the middle of a target.
///
/// Ties go to the horizontal, which is the arrangement the product had
/// before this existed.
pub fn drop_edge(x_frac: f32, y_frac: f32) -> DropEdge {
    let x = if x_frac.is_finite() { x_frac.clamp(0.0, 1.0) } else { 0.5 };
    let y = if y_frac.is_finite() { y_frac.clamp(0.0, 1.0) } else { 0.5 };
    let dx = x.min(1.0 - x);
    let dy = y.min(1.0 - y);
    if dy < dx {
        if y < 0.5 { DropEdge::Top } else { DropEdge::Bottom }
    } else if x < 0.5 {
        DropEdge::Left
    } else {
        DropEdge::Right
    }
}

/// [`drop_edge`] against a rect rather than a unit box.
pub fn edge_of_rect(r: &Rect, x: f32, y: f32) -> DropEdge {
    drop_edge((x - r.x) / r.w.max(1.0), (y - r.y) / r.h.max(1.0))
}

/// One pane, or a row/column of them.
///
/// Invariants, all restored by [`Node::tidy`]: a `Split` holds at least
/// two children, `shares` is the same length as `kids`, every share is
/// finite and positive, and the shares sum to 1.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Leaf(String),
    Split {
        /// Top-to-bottom rather than left-to-right.
        vertical: bool,
        kids: Vec<Node>,
        /// Each child's share of this node's extent along its axis.
        shares: Vec<f32>,
    },
}

/// The smallest share a resize will leave a pane. Mirrors
/// `theme::SPLIT_RATIO_MIN`, kept here so this module stays pure.
pub const RATIO_MIN: f32 = 0.20;

/// How much of the relevant dimension a pane docked against the *well's*
/// own edge takes. Ruled by §17 of the operator's spec: *"moving one pane
/// should not destroy the user's layout"* — the newcomer takes about a
/// third and everything already there keeps its proportions inside the
/// rest. A pane docked against ANOTHER pane takes half of that pane
/// instead, which touches nothing else at all.
pub const EDGE_SHARE: f32 = 0.32;

/// How near the well's outer boundary the pointer has to be for the drop
/// to mean the whole workspace rather than the pane under it.
///
/// **Outer targets win once the pointer is clearly at the edge** (§16), so
/// a drag obviously aimed past everything is not stolen by whichever pane
/// happens to be in the way.
pub const EDGE_ZONE: f32 = 26.0;

/// How far the pointer must travel before the insertion line will move to
/// a different target. §10: *"right → bottom → right → bottom while the
/// mouse barely moves"* is the failure this exists to stop.
pub const HYSTERESIS: f32 = 8.0;

impl Node {
    pub fn leaf(id: impl Into<String>) -> Self {
        Node::Leaf(id.into())
    }

    pub fn pair(first: impl Into<String>, second: impl Into<String>, vertical: bool) -> Self {
        Node::Split {
            vertical,
            kids: vec![Node::leaf(first), Node::leaf(second)],
            shares: vec![0.5, 0.5],
        }
    }

    pub fn is_leaf(&self) -> bool {
        matches!(self, Node::Leaf(_))
    }

    /// The root's axis, for the menu's one bit. A single pane has none.
    pub fn vertical(&self) -> bool {
        matches!(self, Node::Split { vertical: true, .. })
    }

    /// Every pane, in reading order: left to right, top to bottom.
    pub fn leaves(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.walk_leaves(&mut out);
        out
    }

    fn walk_leaves(&self, out: &mut Vec<String>) {
        match self {
            Node::Leaf(id) => out.push(id.clone()),
            Node::Split { kids, .. } => kids.iter().for_each(|k| k.walk_leaves(out)),
        }
    }

    pub fn leaf_count(&self) -> usize {
        match self {
            Node::Leaf(_) => 1,
            Node::Split { kids, .. } => kids.iter().map(Node::leaf_count).sum(),
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        match self {
            Node::Leaf(l) => l == id,
            Node::Split { kids, .. } => kids.iter().any(|k| k.contains(id)),
        }
    }

    /// Take a pane out. Its share goes to its siblings **in proportion**,
    /// so the rest of the arrangement keeps its shape, and a split left
    /// holding one child collapses into that child — otherwise closing a
    /// pane would leave a one-pane row nested inside the tree forever.
    ///
    /// A root that IS the pane answers `false`: one pane is a tab, not a
    /// layout, and the caller drops the whole arrangement.
    pub fn remove(&mut self, id: &str) -> bool {
        let Node::Split { kids, shares, .. } = self else {
            return false;
        };
        if let Some(ix) = kids
            .iter()
            .position(|k| matches!(k, Node::Leaf(l) if l == id))
        {
            kids.remove(ix);
            let freed = shares.remove(ix);
            spread(shares, freed);
            if kids.len() == 1 {
                *self = kids.remove(0);
            }
            return true;
        }
        for k in kids.iter_mut() {
            if k.remove(id) {
                return true;
            }
        }
        false
    }

    /// Put `id` against `host`'s `edge`.
    ///
    /// **The newcomer takes half of the host and nothing else moves.**
    /// That is the whole of §17 for this case: the operator's dividers
    /// elsewhere in the tree are not theirs to redistribute.
    ///
    /// When the host's own row already runs along the drop's axis the new
    /// pane joins it as a sibling; when it does not, the host leaf is
    /// replaced by a two-pane split of the other axis occupying exactly
    /// the space the host had. That second case is what makes a grid.
    pub fn insert_beside(&mut self, host: &str, id: &str, edge: DropEdge) -> bool {
        match self {
            Node::Leaf(l) if l == host => {
                let (a, b) = if edge.after() {
                    (Node::leaf(host), Node::leaf(id))
                } else {
                    (Node::leaf(id), Node::leaf(host))
                };
                *self = Node::Split {
                    vertical: edge.vertical(),
                    kids: vec![a, b],
                    shares: vec![0.5, 0.5],
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split {
                vertical,
                kids,
                shares,
            } => {
                if let Some(ix) = kids
                    .iter()
                    .position(|k| matches!(k, Node::Leaf(l) if l == host))
                {
                    if *vertical == edge.vertical() {
                        let half = shares[ix] * 0.5;
                        shares[ix] = half;
                        let at = ix + usize::from(edge.after());
                        kids.insert(at, Node::leaf(id));
                        shares.insert(at, half);
                    } else {
                        let host_node = kids[ix].clone();
                        let inner = if edge.after() {
                            vec![host_node, Node::leaf(id)]
                        } else {
                            vec![Node::leaf(id), host_node]
                        };
                        kids[ix] = Node::Split {
                            vertical: edge.vertical(),
                            kids: inner,
                            shares: vec![0.5, 0.5],
                        };
                    }
                    return true;
                }
                kids.iter_mut().any(|k| k.insert_beside(host, id, edge))
            }
        }
    }

    /// Put `id` against the WELL's `edge` — outside everything already
    /// there. §8's far-right drop, and the reason the scenario ends in
    /// three equal columns rather than C nested beside B.
    pub fn insert_at_edge(&mut self, id: &str, edge: DropEdge, share: f32) {
        let share = share.clamp(RATIO_MIN, 1.0 - RATIO_MIN);
        match self {
            Node::Split {
                vertical,
                kids,
                shares,
            } if *vertical == edge.vertical() => {
                let scale = 1.0 - share;
                shares.iter_mut().for_each(|s| *s *= scale);
                let at = if edge.after() { kids.len() } else { 0 };
                kids.insert(at, Node::leaf(id));
                shares.insert(at, share);
            }
            _ => {
                let old = std::mem::replace(self, Node::leaf(""));
                let (kids, shares) = if edge.after() {
                    (vec![old, Node::leaf(id)], vec![1.0 - share, share])
                } else {
                    (vec![Node::leaf(id), old], vec![share, 1.0 - share])
                };
                *self = Node::Split {
                    vertical: edge.vertical(),
                    kids,
                    shares,
                };
            }
        }
    }

    /// Restore the invariants after any surgery: drop empty and
    /// single-child splits, fix a shares list that does not match its
    /// kids, and renormalise every level to sum to 1.
    pub fn tidy(&mut self) {
        if let Node::Split { kids, shares, .. } = self {
            kids.iter_mut().for_each(Node::tidy);
            if shares.len() != kids.len() || shares.iter().any(|s| !s.is_finite() || *s <= 0.0) {
                *shares = vec![1.0 / kids.len().max(1) as f32; kids.len()];
            }
            let sum: f32 = shares.iter().sum();
            if sum > 0.0 && (sum - 1.0).abs() > 1e-4 {
                shares.iter_mut().for_each(|s| *s /= sum);
            }
            if kids.len() == 1 {
                let only = kids.remove(0);
                *self = only;
            }
        }
    }

    /// Is this a layout at all — two or more DISTINCT panes?
    pub fn is_viable(&self) -> bool {
        let leaves = self.leaves();
        if leaves.len() < 2 {
            return false;
        }
        let mut seen = leaves.clone();
        seen.sort();
        seen.dedup();
        seen.len() == leaves.len() && self.shares_are_sane()
    }

    fn shares_are_sane(&self) -> bool {
        match self {
            Node::Leaf(_) => true,
            Node::Split { kids, shares, .. } => {
                kids.len() >= 2
                    && shares.len() == kids.len()
                    && shares.iter().all(|s| s.is_finite() && *s > 0.0)
                    && kids.iter().all(Node::shares_are_sane)
            }
        }
    }

    /// Where every pane sits inside `area`, with `gap` points of ground
    /// between neighbours — the seam, which is also the divider's target.
    pub fn rects(&self, area: Rect, gap: f32) -> Vec<(String, Rect)> {
        let mut out = Vec::new();
        self.lay_out(area, gap, &mut out);
        out
    }

    fn lay_out(&self, area: Rect, gap: f32, out: &mut Vec<(String, Rect)>) {
        match self {
            Node::Leaf(id) => out.push((id.clone(), area)),
            Node::Split {
                vertical,
                kids,
                shares,
            } => {
                for (kid, sub) in kids.iter().zip(slots(area, shares, *vertical, gap)) {
                    kid.lay_out(sub, gap, out);
                }
            }
        }
    }

    /// Every divider, with the parent extent its share arithmetic is
    /// measured against. The path addresses the split it belongs to, so
    /// two dividers in different branches can never hear each other.
    pub fn handles(&self, area: Rect, gap: f32) -> Vec<Handle> {
        let mut out = Vec::new();
        self.walk_handles(area, gap, &mut Vec::new(), &mut out);
        out
    }

    fn walk_handles(&self, area: Rect, gap: f32, path: &mut Vec<usize>, out: &mut Vec<Handle>) {
        let Node::Split {
            vertical,
            kids,
            shares,
        } = self
        else {
            return;
        };
        let boxes = slots(area, shares, *vertical, gap);
        let extent = if *vertical {
            area.h - gap * (kids.len() - 1) as f32
        } else {
            area.w - gap * (kids.len() - 1) as f32
        };
        for ix in 0..kids.len().saturating_sub(1) {
            let before = boxes[ix];
            let rect = if *vertical {
                Rect::new(area.x, before.bottom(), area.w, gap)
            } else {
                Rect::new(before.right(), area.y, gap, area.h)
            };
            out.push(Handle {
                path: path.clone(),
                ix,
                vertical: *vertical,
                rect,
                extent: extent.max(1.0),
            });
        }
        for (i, kid) in kids.iter().enumerate() {
            path.push(i);
            kid.walk_handles(boxes[i], gap, path, out);
            path.pop();
        }
    }

    fn at_path_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        match path.split_first() {
            None => Some(self),
            Some((head, rest)) => match self {
                Node::Leaf(_) => None,
                Node::Split { kids, .. } => kids.get_mut(*head)?.at_path_mut(rest),
            },
        }
    }

    /// The share of child `ix` of the split at `path` — what a divider
    /// drag starts from.
    pub fn share_at(&self, path: &[usize], ix: usize) -> Option<f32> {
        match path.split_first() {
            None => match self {
                Node::Split { shares, .. } => shares.get(ix).copied(),
                Node::Leaf(_) => None,
            },
            Some((head, rest)) => match self {
                Node::Split { kids, .. } => kids.get(*head)?.share_at(rest, ix),
                Node::Leaf(_) => None,
            },
        }
    }

    /// Drag the divider after child `ix` of the split at `path` so that
    /// child wants `share`. The pair's sum is kept and each side held
    /// above [`RATIO_MIN`] of the pair — **the divider simply stops**
    /// (§3), it does not spring. Answers whether anything moved enough to
    /// be worth a repaint.
    pub fn set_share(&mut self, path: &[usize], ix: usize, share: f32) -> bool {
        let Some(Node::Split { shares, .. }) = self.at_path_mut(path) else {
            return false;
        };
        if ix + 1 >= shares.len() {
            return false;
        }
        let total = shares[ix] + shares[ix + 1];
        if total <= 2.0 * f32::EPSILON {
            return false;
        }
        // The minimum is a share of the WHOLE node, not of the pair —
        // otherwise the two narrowest panes of a four-pane row could be
        // driven under it by their own divider. When the pair has less
        // room than two minimums between them, they halve it instead of
        // the clamp inverting.
        let floor = RATIO_MIN.min(total * 0.5);
        let want = share.clamp(floor, total - floor);
        if (want - shares[ix]).abs() < 0.002 {
            return false;
        }
        shares[ix] = want;
        shares[ix + 1] = total - want;
        true
    }

    /// Drop every pane the strip no longer knows about.
    pub fn retain(&mut self, known: &mut dyn FnMut(&str) -> bool) {
        for id in self.leaves() {
            if !known(&id) {
                self.remove(&id);
            }
        }
        self.tidy();
    }

    /// A stable one-line rendering, for tests and for reading a layout in
    /// a failure message: `[a | (b / c)]`.
    pub fn sketch(&self) -> String {
        match self {
            Node::Leaf(id) => id.clone(),
            Node::Split {
                vertical, kids, ..
            } => {
                let sep = if *vertical { " / " } else { " | " };
                let mut s = String::from("(");
                for (i, k) in kids.iter().enumerate() {
                    if i > 0 {
                        let _ = s.write_str(sep);
                    }
                    let _ = s.write_str(&k.sketch());
                }
                s.push(')');
                s
            }
        }
    }
}

/// One divider.
#[derive(Clone, Debug, PartialEq)]
pub struct Handle {
    /// The split it belongs to, addressed from the root.
    pub path: Vec<usize>,
    /// It sits after this child.
    pub ix: usize,
    pub vertical: bool,
    /// The seam itself — `gap` thick. The hit target is wider; see
    /// `ui::Shell::split_handle`.
    pub rect: Rect,
    /// The parent's length along its axis, minus its seams: the
    /// denominator that turns a pointer delta into a share delta.
    pub extent: f32,
}

impl Handle {
    /// A DOM-ish id that is unique per divider and stable across paints.
    pub fn key(&self) -> String {
        let mut s = String::new();
        for p in &self.path {
            let _ = write!(s, "{p}-");
        }
        let _ = write!(s, "{}", self.ix);
        s
    }
}

/// Divide `area` along one axis by `shares`, leaving `gap` between.
fn slots(area: Rect, shares: &[f32], vertical: bool, gap: f32) -> Vec<Rect> {
    let n = shares.len();
    if n == 0 {
        return Vec::new();
    }
    let seams = gap * (n - 1) as f32;
    let span = (if vertical { area.h } else { area.w } - seams).max(0.0);
    let sum: f32 = shares.iter().sum();
    let sum = if sum > 0.0 { sum } else { n as f32 };
    let mut cur = if vertical { area.y } else { area.x };
    let mut out = Vec::with_capacity(n);
    for (i, s) in shares.iter().enumerate() {
        // The last slot takes the remainder so rounding cannot leave a
        // sliver of ground down the right-hand side of the well.
        let len = if i + 1 == n {
            (if vertical { area.bottom() } else { area.right() } - cur).max(0.0)
        } else {
            span * (s / sum)
        };
        out.push(if vertical {
            Rect::new(area.x, cur, area.w, len)
        } else {
            Rect::new(cur, area.y, len, area.h)
        });
        cur += len + gap;
    }
    out
}

/// Give `freed` back to the panes that are left, in proportion.
fn spread(shares: &mut [f32], freed: f32) {
    let sum: f32 = shares.iter().sum();
    if shares.is_empty() || sum <= 0.0 {
        let n = shares.len().max(1) as f32;
        shares.iter_mut().for_each(|s| *s = 1.0 / n);
        return;
    }
    shares.iter_mut().for_each(|s| *s += freed * (*s / sum));
}

/// Where a carried pane would land if it were let go right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DockTarget {
    /// Against one pane's edge.
    Pane { id: String, edge: DropEdge },
    /// Against the whole workspace's edge, outside everything.
    Workspace { edge: DropEdge },
}

impl DockTarget {
    pub fn edge(&self) -> DropEdge {
        match self {
            DockTarget::Pane { edge, .. } | DockTarget::Workspace { edge } => *edge,
        }
    }
}

/// The live target plus where the pointer was when it was last agreed —
/// see [`resolve`].
#[derive(Clone, Debug, PartialEq)]
pub struct Dock {
    pub target: DockTarget,
    /// Window coordinates of the last pointer position that classified to
    /// `target` on its own.
    pub anchor: (f32, f32),
}

/// What the pointer is over, with no memory.
///
/// The order is §16's, and the first step is the one that matters: an
/// outer-edge zone wins outright, so a nested pane cannot steal a drag
/// that is obviously aimed past everything at the workspace boundary.
pub fn classify(well: &Rect, panes: &[(String, Rect)], x: f32, y: f32) -> Option<DockTarget> {
    if !well.contains(x, y) {
        return None;
    }
    let d = [
        (x - well.x, DropEdge::Left),
        (well.right() - x, DropEdge::Right),
        (y - well.y, DropEdge::Top),
        (well.bottom() - y, DropEdge::Bottom),
    ];
    let (near, edge) = d
        .iter()
        .copied()
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .unwrap_or((f32::MAX, DropEdge::Right));
    if near <= EDGE_ZONE {
        return Some(DockTarget::Workspace { edge });
    }
    // Inside: the pane under the pointer, or — when it is over a seam —
    // the nearest one, so the indicator never blinks out crossing a
    // divider.
    let (id, rect) = panes
        .iter()
        .find(|(_, r)| r.contains(x, y))
        .or_else(|| panes.iter().min_by(|a, b| a.1.dist2(x, y).total_cmp(&b.1.dist2(x, y))))?;
    Some(DockTarget::Pane {
        id: id.clone(),
        edge: edge_of_rect(rect, x, y),
    })
}

/// [`classify`], with the stickiness §10 asks for.
///
/// **The anchor is the last position that AGREED with the live target**,
/// not the position where it was adopted. While the pointer keeps
/// classifying to the same target the anchor follows it, so the distance
/// that has to be travelled to switch is measured from the last moment
/// the two agreed — which is what makes a pointer wobbling over a corner
/// hold still, and a pointer deliberately crossing into another region
/// switch after [`HYSTERESIS`] points and not before.
///
/// Leaving the well clears the target immediately: *"disappear instantly
/// when leaving it"* is about the well's outside, not about a boundary
/// inside it.
pub fn resolve(well: &Rect, panes: &[(String, Rect)], x: f32, y: f32, current: Option<&Dock>) -> Option<Dock> {
    let plain = classify(well, panes, x, y)?;
    match current {
        Some(cur) if cur.target == plain => Some(Dock {
            target: plain,
            anchor: (x, y),
        }),
        Some(cur) => {
            let (dx, dy) = (x - cur.anchor.0, y - cur.anchor.1);
            if dx * dx + dy * dy < HYSTERESIS * HYSTERESIS {
                Some(cur.clone())
            } else {
                Some(Dock {
                    target: plain,
                    anchor: (x, y),
                })
            }
        }
        None => Some(Dock {
            target: plain,
            anchor: (x, y),
        }),
    }
}

/// The single insertion line, in `well`-local coordinates.
///
/// One line, always — §9. It is centred ON the edge it names rather than
/// laid beside it, so the same rule draws a seam between two panes and
/// the outer boundary of the well, and it is clamped inside the well so
/// an outer target is never drawn half off the screen.
pub fn indicator(well: &Rect, panes: &[(String, Rect)], target: &DockTarget, thick: f32) -> Option<Rect> {
    let against = match target {
        DockTarget::Workspace { .. } => *well,
        DockTarget::Pane { id, .. } => panes.iter().find(|(p, _)| p == id)?.1,
    };
    let r = match target.edge() {
        DropEdge::Left => Rect::new(against.x - thick * 0.5, against.y, thick, against.h),
        DropEdge::Right => Rect::new(against.right() - thick * 0.5, against.y, thick, against.h),
        DropEdge::Top => Rect::new(against.x, against.y - thick * 0.5, against.w, thick),
        DropEdge::Bottom => Rect::new(against.x, against.bottom() - thick * 0.5, against.w, thick),
    };
    let local = r.relative_to(well);
    Some(Rect::new(
        local.x.clamp(0.0, (well.w - local.w).max(0.0)),
        local.y.clamp(0.0, (well.h - local.h).max(0.0)),
        local.w,
        local.h,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> Node {
        // A | (B / C) — the operator's acceptance scenario, which the
        // flat model could not even describe.
        Node::Split {
            vertical: false,
            kids: vec![Node::leaf("a"), Node::pair("b", "c", true)],
            shares: vec![0.4, 0.6],
        }
    }

    #[test]
    fn a_split_can_hold_a_split() {
        let g = grid();
        assert_eq!(g.sketch(), "(a | (b / c))");
        assert_eq!(g.leaves(), ["a", "b", "c"]);
        assert_eq!(g.leaf_count(), 3);
        assert!(g.is_viable());
    }

    #[test]
    fn a_pane_lands_where_the_layout_is_laid_out() {
        let well = Rect::new(100.0, 50.0, 1000.0, 600.0);
        let rects = grid().rects(well, 8.0);
        let by = |id: &str| rects.iter().find(|(p, _)| p == id).unwrap().1;
        let a = by("a");
        let b = by("b");
        let c = by("c");
        assert!((a.w - (1000.0 - 8.0) * 0.4).abs() < 0.5, "{a:?}");
        assert_eq!(a.h, 600.0, "a runs the full height");
        assert!((b.x - a.right() - 8.0).abs() < 0.5, "the seam is the gap");
        assert_eq!(b.x, c.x, "b and c share a column");
        assert!((c.y - b.bottom() - 8.0).abs() < 0.5);
        assert!((b.h - c.h).abs() < 0.5, "an even stack");
    }

    #[test]
    fn the_far_right_of_the_workspace_is_a_third_column() {
        // §24: grab C, drop it at the far right. The answer is three
        // columns — NOT C nested beside B, which is what dropping on B's
        // right edge would give.
        let mut g = grid();
        assert!(g.remove("c"));
        assert_eq!(g.sketch(), "(a | b)", "the stack collapsed when it lost a pane");
        g.insert_at_edge("c", DropEdge::Right, EDGE_SHARE);
        assert_eq!(g.sketch(), "(a | b | c)");
        let shares = match &g {
            Node::Split { shares, .. } => shares.clone(),
            _ => unreachable!(),
        };
        assert!((shares[2] - EDGE_SHARE).abs() < 1e-5, "{shares:?}");
        assert!(
            (shares[0] / shares[1] - 0.4 / 0.6).abs() < 1e-3,
            "a and b kept their proportions: {shares:?}"
        );
    }

    #[test]
    fn the_far_left_puts_it_first() {
        let mut g = grid();
        g.remove("c");
        g.insert_at_edge("c", DropEdge::Left, EDGE_SHARE);
        assert_eq!(g.sketch(), "(c | a | b)");
    }

    #[test]
    fn a_cross_axis_drop_grows_a_grid_and_leaves_the_rest_alone() {
        let mut g = Node::pair("a", "b", false);
        assert!(g.insert_beside("b", "c", DropEdge::Bottom));
        assert_eq!(g.sketch(), "(a | (b / c))");
        let Node::Split { shares, .. } = &g else { unreachable!() };
        assert_eq!(shares, &[0.5, 0.5], "a never moved");
    }

    #[test]
    fn a_same_axis_drop_halves_only_the_host() {
        let mut g = Node::Split {
            vertical: false,
            kids: vec![Node::leaf("a"), Node::leaf("b")],
            shares: vec![0.7, 0.3],
        };
        assert!(g.insert_beside("a", "c", DropEdge::Right));
        assert_eq!(g.sketch(), "(a | c | b)");
        let Node::Split { shares, .. } = &g else { unreachable!() };
        assert!((shares[0] - 0.35).abs() < 1e-5, "{shares:?}");
        assert!((shares[1] - 0.35).abs() < 1e-5, "{shares:?}");
        assert!((shares[2] - 0.3).abs() < 1e-5, "b is untouched: {shares:?}");
    }

    #[test]
    fn a_lost_pane_gives_its_room_back_in_proportion() {
        let mut g = Node::Split {
            vertical: false,
            kids: vec![Node::leaf("a"), Node::leaf("b"), Node::leaf("c")],
            shares: vec![0.5, 0.3, 0.2],
        };
        assert!(g.remove("c"));
        let Node::Split { shares, .. } = &g else { unreachable!() };
        assert!((shares.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        assert!(
            (shares[0] / shares[1] - 0.5 / 0.3).abs() < 1e-3,
            "the survivors kept their ratio: {shares:?}"
        );
    }

    #[test]
    fn a_split_left_with_one_pane_becomes_that_pane() {
        let mut g = grid();
        g.remove("b");
        assert_eq!(g.sketch(), "(a | c)", "the stack is gone, c took its place");
    }

    #[test]
    fn the_outer_edge_beats_the_pane_under_the_pointer() {
        let well = Rect::new(0.0, 0.0, 1000.0, 600.0);
        let panes = grid().rects(well, 8.0);
        // Deep inside pane C, but hard against the workspace's right.
        let at = (995.0, 500.0);
        assert_eq!(
            classify(&well, &panes, at.0, at.1),
            Some(DockTarget::Workspace { edge: DropEdge::Right })
        );
        // A step in from the same row is C's own right edge.
        assert_eq!(
            classify(&well, &panes, 960.0, 500.0),
            Some(DockTarget::Pane { id: "c".into(), edge: DropEdge::Right })
        );
    }

    #[test]
    fn outside_the_well_there_is_no_target() {
        let well = Rect::new(100.0, 50.0, 400.0, 300.0);
        let panes = Node::pair("a", "b", false).rects(well, 8.0);
        assert_eq!(classify(&well, &panes, 20.0, 60.0), None);
    }

    #[test]
    fn a_seam_still_names_a_pane() {
        let well = Rect::new(0.0, 0.0, 1000.0, 600.0);
        let node = Node::pair("a", "b", false);
        let panes = node.rects(well, 8.0);
        let seam_x = panes[0].1.right() + 4.0;
        let got = classify(&well, &panes, seam_x, 300.0);
        assert!(
            matches!(got, Some(DockTarget::Pane { .. })),
            "the line must not blink out over a divider: {got:?}"
        );
    }

    #[test]
    fn the_target_does_not_flicker_while_the_pointer_barely_moves() {
        let well = Rect::new(0.0, 0.0, 1000.0, 600.0);
        let node = Node::pair("a", "b", false);
        let panes = node.rects(well, 8.0);
        // A corner of pane A, where right and bottom tie.
        let (x, y) = (400.0, 400.0);
        let first = resolve(&well, &panes, x, y, None).unwrap();
        // Two points either side of the diagonal, both within the
        // hysteresis of the first.
        let a = resolve(&well, &panes, x + 2.0, y - 2.0, Some(&first)).unwrap();
        let b = resolve(&well, &panes, x - 2.0, y + 2.0, Some(&a)).unwrap();
        assert_eq!(a.target, first.target);
        assert_eq!(b.target, first.target, "it moved twice and changed nothing");
        // A deliberate move does switch.
        let far = resolve(&well, &panes, x, y + 120.0, Some(&b)).unwrap();
        assert_eq!(far.target, DockTarget::Pane { id: "a".into(), edge: DropEdge::Bottom });
    }

    #[test]
    fn the_anchor_follows_agreement_so_stickiness_cannot_accumulate() {
        let well = Rect::new(0.0, 0.0, 1000.0, 600.0);
        let node = Node::pair("a", "b", false);
        let panes = node.rects(well, 8.0);
        // Walk a long way inside one target, then step across a boundary.
        let mut d = resolve(&well, &panes, 60.0, 300.0, None).unwrap();
        for step in 0..20 {
            d = resolve(&well, &panes, 60.0 + step as f32, 300.0, Some(&d)).unwrap();
        }
        assert_eq!(d.anchor.0, 79.0, "the anchor tracked the agreeing pointer");
    }

    #[test]
    fn one_indicator_and_it_sits_on_the_edge_it_names() {
        let well = Rect::new(100.0, 50.0, 1000.0, 600.0);
        let panes = grid().rects(well, 8.0);
        let line = indicator(
            &well,
            &panes,
            &DockTarget::Workspace { edge: DropEdge::Right },
            2.0,
        )
        .unwrap();
        assert!((line.x - 998.0).abs() < 0.01, "{line:?}");
        assert_eq!(line.h, 600.0, "a workspace edge spans the whole well");
        let b = panes.iter().find(|(p, _)| p == "b").unwrap().1;
        let under = indicator(
            &well,
            &panes,
            &DockTarget::Pane { id: "b".into(), edge: DropEdge::Bottom },
            2.0,
        )
        .unwrap();
        assert!((under.y + well.y - (b.bottom() - 1.0)).abs() < 0.01, "{under:?}");
        assert!((under.w - b.w).abs() < 0.01, "it spans the edge it names");
    }

    #[test]
    fn a_divider_knows_its_branch_and_what_its_share_is_measured_against() {
        let well = Rect::new(0.0, 0.0, 1000.0, 600.0);
        let hs = grid().handles(well, 8.0);
        assert_eq!(hs.len(), 2, "one per split: {hs:?}");
        let root = hs.iter().find(|h| h.path.is_empty()).unwrap();
        assert!(!root.vertical);
        assert!((root.extent - 992.0).abs() < 0.01, "the seam is not share-able space");
        let inner = hs.iter().find(|h| h.path == vec![1]).unwrap();
        assert!(inner.vertical);
        assert!((inner.extent - 592.0).abs() < 0.01);
        assert_ne!(root.key(), inner.key(), "two dividers, two ids");
    }

    #[test]
    fn a_divider_moves_only_its_own_pair_and_stops_at_the_floor() {
        let mut g = grid();
        assert!(g.set_share(&[1], 0, 0.7));
        let Node::Split { kids, shares, .. } = &g else { unreachable!() };
        assert!((shares[0] - 0.4).abs() < 1e-5, "the root divider did not move");
        let Node::Split { shares: inner, .. } = &kids[1] else { unreachable!() };
        assert!((inner[0] - 0.7).abs() < 1e-5, "{inner:?}");
        assert!(g.set_share(&[1], 0, 0.99), "it moved as far as it is allowed to");
        let Node::Split { kids, .. } = &g else { unreachable!() };
        let Node::Split { shares: inner, .. } = &kids[1] else { unreachable!() };
        assert!(
            (inner[1] - RATIO_MIN).abs() < 1e-5,
            "it stopped at the floor rather than springing off it: {inner:?}"
        );
        assert!(
            !g.set_share(&[1], 0, 0.995),
            "pushing past a divider that has stopped must not repaint"
        );
        assert!(!g.set_share(&[1], 0, 0.7995), "a hair's move must not repaint");
    }

    #[test]
    fn a_layout_survives_losing_every_pane_the_strip_forgot() {
        let mut g = grid();
        let mut known = |id: &str| id != "b";
        g.retain(&mut known);
        assert_eq!(g.sketch(), "(a | c)");
        let mut none = |_: &str| false;
        g.retain(&mut none);
        assert!(!g.is_viable(), "nothing left is not a layout");
    }

    #[test]
    fn tidy_fixes_what_hand_editing_a_frame_file_can_break() {
        let mut g = Node::Split {
            vertical: false,
            kids: vec![Node::leaf("a"), Node::leaf("b")],
            shares: vec![3.0, 1.0],
        };
        g.tidy();
        let Node::Split { shares, .. } = &g else { unreachable!() };
        assert!((shares.iter().sum::<f32>() - 1.0).abs() < 1e-5, "{shares:?}");
        let mut one = Node::Split {
            vertical: true,
            kids: vec![Node::leaf("only")],
            shares: vec![1.0],
        };
        one.tidy();
        assert_eq!(one, Node::leaf("only"));
    }
}
