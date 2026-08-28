//! Scroll-edge fades (Slice M) — [`edge_faded`] wraps a subtree so every
//! primitive inside it paints under a [`gpui::EdgeFade`] scope: quads,
//! glyphs and sprites fade by their distance to the wrapper's own edges,
//! at PER-PRIMITIVE granularity. That is a true static gradient, not
//! whole-row opacity, and it is the only thing that works over glass:
//! with a see-through blurred backdrop, no painted overlay can fade
//! content out, because "what is behind the window" is not a paintable
//! colour.
//!
//! This is a port of comet's `crates/ui/src/edge_fade.rs` (their file is
//! the origin of every design note below, including the two failure modes
//! they hit in production). The primitive itself is fork-only: `EdgeFade`
//! + `Window::with_edge_fade` are 7 of the 17 commits in our pin
//! (`eave-app/Cargo.toml` §fork ledger), and this is the first thing in
//! eave to use them.
//!
//! Structurally identical to [`crate::ui::Frost`]: layout is the child's
//! (`request_layout` forwards the child's `LayoutId`, so the faded region
//! is exactly the child's bounds and the wrapper is invisible to flex),
//! and the only real work happens in `paint`.

use gpui::{
    px, AnyElement, App, Bounds, EdgeFade, Element, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, ScrollHandle, Window,
};

/// Fade the child's content at its own top/bottom edges. `band` is the
/// ramp height in px; `top`/`bottom` select the edges. Both are ENABLES
/// — [`EdgeFaded::fade_overflow_y`] can still switch one off per frame.
pub fn edge_faded(band: f32, top: bool, bottom: bool, child: impl IntoElement) -> EdgeFaded {
    EdgeFaded {
        band,
        band_top: None,
        band_bottom: None,
        inset_top: 0.0,
        top,
        bottom,
        scroll_y: None,
        child: child.into_any_element(),
    }
}

pub struct EdgeFaded {
    band: f32,
    band_top: Option<f32>,
    band_bottom: Option<f32>,
    inset_top: f32,
    top: bool,
    bottom: bool,
    scroll_y: Option<ScrollHandle>,
    child: AnyElement,
}

impl EdgeFaded {
    /// Override the ramp height at the TOP edge only. Asymmetric bands
    /// let content fade across chrome of different heights — in eave, a
    /// 24 px resting pad above the transcript against a 116 px floating
    /// composer stack below it.
    pub fn band_top(mut self, px: f32) -> Self {
        self.band_top = Some(px);
        self
    }

    /// [`Self::band_top`], for the bottom edge.
    pub fn band_bottom(mut self, px: f32) -> Self {
        self.band_bottom = Some(px);
        self
    }

    /// Gate the fades on the handle's overflow, read at PAINT time —
    /// after the tracked div's prepaint has clamped this frame's offset.
    /// Render-time gating rides the LAST frame's offset, which goes stale
    /// on the final frame of a content shrink (rows removed while
    /// scrolled): prepaint clamps the offset to fit, nothing re-renders,
    /// and a fade with no overflow sticks on screen. comet shipped the
    /// render-time version first and got a user report for exactly this.
    ///
    /// A scroller that fits its content therefore shows no ramp at all,
    /// which is what makes an always-on top band safe on a rail with no
    /// resting top pad.
    pub fn fade_overflow_y(mut self, handle: &ScrollHandle) -> Self {
        self.scroll_y = Some(handle.clone());
        self
    }

    /// Pull the fade's TOP edge `px` inside the wrapper: gpui clamps the
    /// ramp to 0 past an active edge, so content between the wrapper's
    /// real top and the inset edge paints fully transparent — content
    /// under opaque chrome (comet's case: titlebar TEXT) vanishes before
    /// it can overlap.
    ///
    /// **Unused in eave, deliberately.** Nothing here is layered over a
    /// scroller's top edge: the titlebar is a flex sibling ABOVE an inset
    /// content card, not a layer over it, and both rails put their header
    /// and footer lines outside the scroller. Carried because it is the
    /// third of comet's three refinements and costs six lines — the day
    /// any chrome floats over a scroller's top, the answer is already
    /// here rather than rediscovered.
    #[allow(dead_code)]
    pub fn inset_top(mut self, px: f32) -> Self {
        self.inset_top = px;
        self
    }
}

impl Element for EdgeFaded {
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
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (mut top, mut bottom) = (self.top, self.bottom);
        if let Some(scroll) = &self.scroll_y {
            let scrolled = -f32::from(scroll.offset().y);
            let max_scroll = f32::from(scroll.max_offset().y);
            top &= scrolled > 1.0;
            bottom &= scrolled < max_scroll - 1.0;
        }
        let fade = (top || bottom).then(|| {
            let mut bounds = bounds;
            bounds.origin.y += px(self.inset_top);
            bounds.size.height -= px(self.inset_top);
            EdgeFade {
                bounds,
                band: px(self.band),
                band_top: self.band_top.map(px),
                band_bottom: self.band_bottom.map(px),
                top,
                bottom,
                left: false,
                right: false,
            }
        });
        // `with_edge_fade(None, ..)` is a plain passthrough, so a scroller
        // with nothing hidden costs exactly what it cost before.
        window.with_edge_fade(fade, |window| self.child.paint(window, cx));
    }
}

impl IntoElement for EdgeFaded {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
