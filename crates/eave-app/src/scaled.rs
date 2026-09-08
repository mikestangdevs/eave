//! Painting a card smaller than it is laid out.
//!
//! **CV2's answer to the operator's ruling, and the shape of it is the
//! point.** CV1's zoom multiplied a card's rectangle and nothing else, so
//! the card's contents re-laid-out inside a smaller box at the same type
//! size — a 560-point card at 40% became a 224-point column of unchanged
//! 13 px text. The operator's word for that was corruption, and the
//! reference he sent does the opposite: the cards scale *whole*, so a
//! zoomed-out card is a legible miniature of itself rather than a squeezed
//! one.
//!
//! There is no element transform in gpui, and there did not need to be one:
//! the framework already applies the element offset while bounds are being
//! computed and the scale factor when primitives enter the scene. Push a
//! scale onto the second and an offset onto the first and the two compose
//! into "paint this subtree at `s` about point `p`" — layout untouched,
//! every quad, border, shadow, image and SVG carried along, and glyphs
//! *re-rasterised* at the scaled size rather than magnified, because a
//! glyph is rasterised at `font_size × scale_factor` and that is the number
//! being changed. The fork carries the two accessors; this is the element
//! that spends them.
//!
//! What does not follow automatically is anything that kept a rectangle for
//! its own arithmetic — a drag handler measuring `event.position` against
//! bounds it captured at prepaint is comparing the window's space against
//! the card's. Those divide by the scale themselves.

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Point, Window, point,
};

/// Paint `child` at `scale`, holding its container's corner still.
///
/// `local` is where the child sits inside that container — the `left`/`top`
/// it was given. **The container's own window position is not passed in
/// because nothing that builds a card knows it**: it is settled during
/// layout, two phases later. Subtracting `local` from the bounds gpui hands
/// prepaint recovers it exactly, which is cheaper and more honest than
/// reconstructing the plane's origin from the rail width and the gutter and
/// hoping the two stay in step.
pub fn scaled(scale: f32, local: Point<Pixels>, child: AnyElement) -> Scaled {
    Scaled {
        child,
        scale,
        local,
    }
}

pub struct Scaled {
    child: AnyElement,
    scale: f32,
    local: Point<Pixels>,
}

impl Scaled {
    /// The element offset that turns a scale about the window origin into a
    /// scale about the container's corner `o`.
    ///
    /// Derived, not tuned. Bounds are `o + local` before the offset and the
    /// scene multiplies by `s` afterwards, so what lands on screen is
    /// `(o + local + d)·s`; the card belongs at `o + local·s`, its corner
    /// where the plane's arithmetic put it. Equate the two and `d = o/s − o`
    /// falls out, uniquely.
    ///
    /// Relative, not absolute: an ancestor may already be holding an offset
    /// of its own — a scroll container is the ordinary case — and replacing
    /// it would drop that scroll on the floor for the length of this
    /// subtree. `o` is read out of the bounds *after* that offset, so it is
    /// already accounted for.
    fn offset(&self, bounds: Bounds<Pixels>) -> Point<Pixels> {
        let s = self.scale;
        let o = point(bounds.origin.x - self.local.x, bounds.origin.y - self.local.y);
        point(o.x / s - o.x, o.y / s - o.y)
    }

    fn live(&self) -> bool {
        self.scale.is_finite() && self.scale > 0. && self.scale != 1.0
    }
}

impl IntoElement for Scaled {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Scaled {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    /// **Deliberately outside the scale.** Layout is what must not move: the
    /// card is measured, wrapped and line-broken exactly as it would be at
    /// 100%, and only the painting of it is smaller. That is the whole
    /// difference between a miniature and a reflow.
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        if !self.live() {
            self.child.prepaint(window, cx);
            return;
        }
        let offset = self.offset(bounds);
        window.with_element_scale(self.scale, |window| {
            window.with_element_offset(offset, |window| self.child.prepaint(window, cx));
        });
    }

    /// The offset is not pushed again here, and that is not an omission:
    /// bounds were settled in prepaint with it and paint reads them back.
    /// Pushing it twice would move the card by its own correction.
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        if !self.live() {
            self.child.paint(window, cx);
            return;
        }
        window.with_element_scale(self.scale, |window| self.child.paint(window, cx));
    }
}
