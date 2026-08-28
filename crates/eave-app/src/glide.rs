//! The resort glide (Slice RS1 — AG2 gap #6) — [`glide`] wraps one row of a
//! list so that when the list reorders under it, the row is *painted* where
//! it used to be and slides to where it now is.
//!
//! This is the FLIP shape, and comet is the reference: they measure old
//! versus new y per key and animate a paint-only offset to zero, new rows
//! fade in, removals just go, and a first fill never animates
//! (`shell.rs:3820-3870`). All four of those rules live in
//! [`crate::motion::glide_t`]; this file is only the part that has to be an
//! `Element` — the two things a free function cannot reach are the row's
//! laid-out bounds and `with_element_offset`.
//!
//! **Nothing here touches layout.** `request_layout` forwards the child's
//! own `LayoutId`, exactly as [`crate::edge_fade::EdgeFaded`] does, so flex
//! cannot tell the wrapper is there; the offset is applied inside
//! `prepaint`, after every height in the list is already decided. A row
//! mid-glide is drawn somewhere other than its box and the box has not
//! moved — which is what makes this safe to put on a rail whose scroll
//! anchor is under assertion, and it is the property the replay sweep is
//! the gate for.
//!
//! Hitboxes ride the offset, because they are registered during the same
//! prepaint. That is correct rather than incidental: for the ~260 ms a row
//! is displaced, the thing under the pointer is the thing the pointer is
//! pointing at.

use gpui::{
    px, AnyElement, App, Bounds, Element, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Point, ScrollHandle, Window,
};

use crate::motion::{self, MotionSpec};

/// Glide `child` whenever its position inside `scroll` changes between
/// frames. `key` identifies the row across reorders and must be stable for
/// the life of the row — a session id, not an index, because an index is
/// the one thing a reorder changes.
pub fn glide(
    key: impl Into<String>,
    scroll: &ScrollHandle,
    spec: MotionSpec,
    child: impl IntoElement,
) -> Glide {
    Glide {
        key: key.into(),
        scroll: scroll.clone(),
        spec,
        child: child.into_any_element(),
    }
}

pub struct Glide {
    key: String,
    scroll: ScrollHandle,
    spec: MotionSpec,
    child: AnyElement,
}

impl Element for Glide {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // The scroller's content origin: where a row at list-y 0 paints.
        // `offset().y` is negative once scrolled, so adding it here is what
        // takes the scroll back out of the measurement — without this the
        // store would read a trackpad flick as every row reordering at
        // once. The scroller's own origin comes out too, so chrome
        // appearing above the rail moves the list without smearing it.
        let anchor = self.scroll.bounds().origin.y + self.scroll.offset().y;
        let dy = motion::glide_t(&self.key, f32::from(bounds.origin.y - anchor), self.spec);
        if dy == 0.0 {
            // The common case by a wide margin — every row, every frame,
            // except during a reorder. `with_element_offset` short-circuits
            // a zero itself, but not before pushing and popping the stack.
            self.child.prepaint(window, cx);
            return;
        }
        window.with_element_offset(Point::new(px(0.), px(dy)), |window| {
            self.child.prepaint(window, cx)
        });
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

impl IntoElement for Glide {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
