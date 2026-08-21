//! Is a browser frame just another scene primitive?
//!
//! The window below paints, bottom to top:
//!
//!   1. a BGRA `CVPixelBuffer` (IOSurface-backed — the same thing CEF's
//!      `OnAcceleratedPaint` hands over, minus CEF) filling the window, via
//!      `Window::paint_surface`;
//!   2. a `BackdropBlur` panel over it;
//!   3. ordinary gpui content — text and quads — above both.
//!
//! If the panel comes out FROSTED rather than a flat tint, the page is inside
//! gpui's render target, not merely behind it. That is the whole difference
//! between this and the WKWebView punchout, where the "hole" is an absence of
//! gpui pixels and there is nothing for the blur to sample.

use core_foundation::{base::TCFType, dictionary::CFDictionary, number::CFNumber, string::CFString};
use core_video::pixel_buffer::{
    kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferKeys,
};
use gpui::{
    div, prelude::*, px, rgb, size, App, Bounds, Corners, ElementId, GlobalElementId,
    Hsla, InspectorElementId, IntoElement, LayoutId, Pixels, Window, WindowBounds, WindowOptions,
};

/// A stand-in for a web page: a BGRA buffer with content sharp enough that
/// "blurred" and "merely dimmed" cannot be confused — fine text-like stripes,
/// a hard checkerboard, and saturated colour edges.
fn fake_page(width: usize, height: usize) -> CVPixelBuffer {
    let attrs = CFDictionary::from_CFType_pairs(&[
        (
            CFString::from(CVPixelBufferKeys::IOSurfaceProperties),
            CFDictionary::<CFString, CFNumber>::from_CFType_pairs(&[]).as_CFType(),
        ),
        (
            CFString::from(CVPixelBufferKeys::MetalCompatibility),
            CFNumber::from(1i32).as_CFType(),
        ),
    ]);
    let buffer = CVPixelBuffer::new(kCVPixelFormatType_32BGRA, width, height, Some(&attrs))
        .expect("CVPixelBufferCreate");

    buffer.lock_base_address(0);
    let stride = buffer.get_bytes_per_row();
    let base = unsafe { buffer.get_base_address() } as *mut u8;
    for y in 0..height {
        let row = unsafe { base.add(y * stride) };
        for x in 0..width {
            // Hairline horizontal rules every 8px: the giveaway for a real
            // gaussian, which smears them, versus a tint, which does not.
            let rule = y % 8 == 0;
            let check = ((x / 32) + (y / 32)) % 2 == 0;
            let (b, g, r) = if rule {
                (240u8, 240, 240)
            } else if check {
                (
                    (40 + (x * 200 / width)) as u8,
                    (30 + (y * 120 / height)) as u8,
                    90u8,
                )
            } else {
                (18u8, 20, 26)
            };
            unsafe {
                let px = row.add(x * 4);
                *px = b;
                *px.add(1) = g;
                *px.add(2) = r;
                *px.add(3) = 255;
            }
        }
    }
    buffer.unlock_base_address(0);
    buffer
}

/// Paints the pixel buffer as a scene primitive covering its own bounds.
struct PageSurface {
    buffer: CVPixelBuffer,
}

impl IntoElement for PageSurface {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PageSurface {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = gpui::Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) {
        window.paint_surface(bounds, self.buffer.clone());
    }
}

/// The frosted panel: a `BackdropBlur` at this element's bounds, then its
/// child painted on top.
struct Frost {
    child: gpui::AnyElement,
    radius: Pixels,
}

impl IntoElement for Frost {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Frost {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

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
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_layer(bounds, |window| {
            window.paint_backdrop_blur(bounds, Corners::all(self.radius), px(18.));
            self.child.paint(window, cx);
        });
    }
}

struct Root {
    page: CVPixelBuffer,
}

impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let panel = |title: &'static str, tint: Hsla| {
            Frost {
                radius: px(14.),
                child: div()
                    .w(px(300.))
                    .h(px(190.))
                    .bg(tint)
                    .rounded(px(14.))
                    .border_1()
                    .border_color(gpui::hsla(0., 0., 1., 0.14))
                    .p(px(16.))
                    .child(div().text_color(rgb(0xffffff)).child(title))
                    .child(
                        div()
                            .mt(px(10.))
                            .text_color(gpui::hsla(0., 0., 1., 0.7))
                            .child("Hairlines behind this panel are 1px every 8px."),
                    )
                    .into_any_element(),
            }
            .into_any_element()
        };

        div()
            .size_full()
            .relative()
            // 1. the "page"
            .child(div().absolute().inset_0().child(PageSurface {
                buffer: self.page.clone(),
            }))
            // 2. + 3. chrome over it
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .gap(px(28.))
                    .child(panel("BackdropBlur over the surface", gpui::hsla(0., 0., 0., 0.25)))
                    .child(
                        // The control: same tint, NO blur. If these two look
                        // the same, the blur is not seeing the page.
                        div()
                            .w(px(300.))
                            .h(px(190.))
                            .bg(gpui::hsla(0., 0., 0., 0.25))
                            .rounded(px(14.))
                            .border_1()
                            .border_color(gpui::hsla(0., 0., 1., 0.14))
                            .p(px(16.))
                            .child(div().text_color(rgb(0xffffff)).child("Flat tint, no blur"))
                            .child(
                                div()
                                    .mt(px(10.))
                                    .text_color(gpui::hsla(0., 0., 1., 0.7))
                                    .child("Control panel — nothing sampled."),
                            ),
                    ),
            )
    }
}

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        let page = fake_page(1600, 1000);
        let bounds = Bounds::centered(None, size(px(1100.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Root { page }),
        )
        .unwrap();
        cx.activate(true);
    });
}
