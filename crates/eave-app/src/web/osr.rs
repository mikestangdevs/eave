//! Off-screen rendering: CEF's frame becomes a `CVPixelBuffer` we own.
//!
//! # The contract, which is why this is more than ten lines
//! `cef_render_handler.h`, on `OnAcceleratedPaint`:
//!
//! > The underlying implementation uses a pool to deliver frames. As a
//! > result, the handle may differ every frame [...] The handle's resource
//! > cannot be cached and cannot be accessed outside of this callback. It
//! > should be reopened each time this callback is executed and the contents
//! > should be copied to a texture owned by the client application.
//!
//! So "shared texture" does not mean "hand the page's IOSurface to the
//! compositor and forget about it". It means: open it, blit out of it, and
//! give it back before returning. The spike measured **574 of 574 handles
//! distinct** over a heavy page — the pool never repeated one — so any cache
//! keyed on the handle would be a cache that never hits and a lifetime bug
//! waiting for the pool to wrap. There is no such cache here.
//!
//! # The destination is the thing gpui takes
//! An IOSurface-backed BGRA `CVPixelBuffer`: simultaneously our blit
//! destination and the handle `Window::paint_surface` wants. One allocation
//! serves both, so there is no second copy on the way to the scene.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use cef::*;
use core_foundation::{
    base::TCFType, dictionary::CFDictionary, number::CFNumber, string::CFString,
};
use core_video::pixel_buffer::{kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferKeys};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLOrigin, MTLPixelFormat, MTLSize, MTLStorageMode,
    MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

unsafe extern "C" {
    fn CVPixelBufferGetIOSurface(
        buffer: core_video::pixel_buffer::CVPixelBufferRef,
    ) -> *mut std::ffi::c_void;
}

/// Frames CEF has delivered into the scene, ever. The pump compares this
/// against what it last redrew for, which is what makes an idle page cost
/// nothing.
static FRAMES: AtomicU64 = AtomicU64::new(0);

/// Popup frames delivered, for the capture rig. Counted separately from
/// `FRAMES` because "the page painted" and "the `<select>` painted" are
/// different claims and a capture that cannot tell them apart proves
/// neither.
static POPUP_FRAMES: AtomicU64 = AtomicU64::new(0);

/// Which page frame `EAVE_WEB_PROBE_CLICK` rides.
///
/// Not frame 0: that one arrives at the pre-resize size (1600x1200 in this
/// rig) and is a partly-laid-out document — clicking into it finds no boxes,
/// which is what moving the probe off `on_load_end` was meant to fix.
///
/// Frame 1 is the settled one (1632x1394, the view's real size), and it is
/// also the LAST one a static page ever produces. An earlier version waited
/// for frame 2 and simply never fired: a page with nothing moving in it
/// paints twice and then stops, so "wait a few frames" is not a thing a
/// static page will do for you.
const PROBE_AFTER_FRAMES: u64 = 1;

pub fn frames_delivered() -> u64 {
    FRAMES.load(Ordering::Relaxed)
}

thread_local! {
    /// The latest completed frame per tab, as a buffer WE own — CEF's
    /// surface may not outlive its callback.
    ///
    /// **The size travels with the buffer, and Slice E1 is why.** A
    /// `CVPixelBuffer` on its own does not say what geometry it was drawn
    /// for, and `paint_surface` scales whatever it is given to fill the
    /// bounds it is given. So a frame produced at the old size, painted
    /// into the new bounds during a live resize, is a *stretched* page —
    /// and it stays stretched for as many frames as CEF takes to answer.
    /// Carrying the coded size is what lets the element ask "is this frame
    /// the right size?" and refuse to scale when the answer is no.
    static PAGES: RefCell<HashMap<String, Frame>> = RefCell::new(HashMap::new());

    /// Where each tab's page element was laid out, in device pixels, and at
    /// what scale. Published by the element at paint; read here to answer
    /// CEF's `view_rect`, and by `input` to convert window space to page
    /// space. One source of truth, so the two can never disagree.
    static VIEWS: RefCell<HashMap<String, View>> = RefCell::new(HashMap::new());

    /// The latest popup frame per tab — a `<select>` dropdown, an autofill
    /// list, a colour picker. Separate from `PAGES` because it is a
    /// separate surface with its own size, not a region of the page.
    static POPUPS: RefCell<HashMap<String, CVPixelBuffer>> = RefCell::new(HashMap::new());

    /// Where each tab's popup sits and whether it is up. `None` means no
    /// popup is showing, which is the state that must survive
    /// `on_popup_show(false)` even though a stale frame is still in
    /// `POPUPS` — Chromium does not paint a blank frame on the way down,
    /// so a shown-flag is the only thing that can retract the widget.
    static POPUP_RECTS: RefCell<HashMap<String, PopupRect>> = RefCell::new(HashMap::new());

    static GPU: RefCell<Option<Gpu>> = const { RefCell::new(None) };
}

/// The popup for a tab: its frame and where to put it. `None` when no popup
/// is showing.
///
/// Both halves are required, and that is the whole reason this is one
/// function rather than two. A frame with no rect cannot be placed, and a
/// rect with no frame would paint a hole — so a popup that is mid-open,
/// with a size but no pixels yet, correctly draws nothing for one frame
/// instead of a black rectangle.
fn show_popup(tab_id: &str, rect: PopupRect) {
    POPUP_RECTS.with(|r| {
        r.borrow_mut().insert(tab_id.to_string(), rect);
    });
}

/// Take the popup down.
///
/// Clears the RECT and deliberately leaves the last frame in `POPUPS`.
/// Chromium does not paint an empty frame on the way down, so the pixels
/// are still there and always will be; the rect is the thing that decides
/// whether they are drawn. Clearing the frame instead would work too, and
/// would throw away a buffer that the next popup on this tab can reuse.
fn hide_popup(tab_id: &str) {
    POPUP_RECTS.with(|r| {
        r.borrow_mut().remove(tab_id);
    });
}

/// Whether a popup is up for this tab, whatever has been painted.
///
/// Test-only for now. The product asks the same question through [`popup`],
/// which needs the frame as well; this is the half that isolates the state
/// machine from whether Metal ever ran.
#[cfg(test)]
fn popup_shown(tab_id: &str) -> bool {
    POPUP_RECTS.with(|r| r.borrow().contains_key(tab_id))
}

pub fn popup(tab_id: &str) -> Option<(CVPixelBuffer, PopupRect)> {
    let rect = POPUP_RECTS.with(|r| r.borrow().get(tab_id).copied())?;
    let frame = POPUPS.with(|p| p.borrow().get(tab_id).cloned())?;
    Some((frame, rect))
}

/// A finished page frame, and the geometry Chromium coded it at.
///
/// The two are one value because they are only ever true together: the
/// moment they are separated, something will paint the pixels at a size
/// they were not drawn for and call it a resize.
#[derive(Clone)]
pub struct Frame {
    pub buffer: CVPixelBuffer,
    /// **Device** pixels — `AcceleratedPaintInfo::coded_size`, which is the
    /// logical size times the scale CEF was told about.
    pub width: u32,
    pub height: u32,
}

impl Frame {
    /// The size this frame is 1:1 at under `scale`, in logical pixels.
    ///
    /// Not "the size it was drawn for": if the scale has changed since, the
    /// honest answer is still one device pixel per device pixel, because
    /// that is the only rendering of these pixels that invents nothing.
    pub fn logical(&self, scale: f32) -> (f32, f32) {
        let scale = if scale > 0. { scale } else { 1. };
        (self.width as f32 / scale, self.height as f32 / scale)
    }
}

/// What changed about where a page is laid out — and therefore which of
/// CEF's two notification calls it owes.
///
/// **They are not the same call and one does not imply the other.**
/// `WasResized` re-asks `GetViewRect`; the device scale factor lives in
/// `GetScreenInfo`, which CEF caches and re-reads only on
/// `NotifyScreenInfoChanged`. A build that sends only `WasResized` renders
/// a retina page at 1× after a drag to an external display — the geometry
/// is right, the resolution is a display behind, and nothing errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewChange {
    /// Same box, same scale. CEF is told nothing.
    Same,
    /// New box, same scale. `WasResized`.
    Resized,
    /// New scale, whatever the box did. `NotifyScreenInfoChanged` **and**
    /// `WasResized` — the second because a scale change is also a change in
    /// the device-pixel size of the same logical box.
    Rescaled,
}

#[derive(Clone, Copy, Debug)]
pub struct View {
    /// Logical size, which is what CEF is told. Device pixels are this times
    /// `scale`.
    pub width: i32,
    pub height: i32,
    pub scale: f32,
    /// Window-space origin of the element, in logical pixels.
    pub origin_x: f32,
    pub origin_y: f32,
}

impl Default for View {
    fn default() -> Self {
        Self {
            width: 800,
            height: 600,
            scale: 2.0,
            origin_x: 0.,
            origin_y: 0.,
        }
    }
}

pub fn view(tab_id: &str) -> View {
    VIEWS.with(|v| v.borrow().get(tab_id).copied().unwrap_or_default())
}

/// Publish where the element was laid out. Returns what CEF has to be told
/// about, which is never the origin — that is ours, for input.
pub fn set_view(tab_id: &str, next: View) -> ViewChange {
    VIEWS.with(|v| {
        let mut v = v.borrow_mut();
        let previous = v.insert(tab_id.to_string(), next);
        match previous {
            Some(p) if p.scale != next.scale => ViewChange::Rescaled,
            Some(p) if p.width != next.width || p.height != next.height => ViewChange::Resized,
            Some(_) => ViewChange::Same,
            // First layout. The browser was created at whatever
            // `View::default` said, so this is a real change even when the
            // numbers happen to agree.
            None => ViewChange::Rescaled,
        }
    })
}

/// The newest frame for a tab, if one has arrived.
pub fn frame(tab_id: &str) -> Option<Frame> {
    PAGES.with(|p| p.borrow().get(tab_id).cloned())
}

pub fn forget(tab_id: &str) {
    PAGES.with(|p| {
        p.borrow_mut().remove(tab_id);
    });
    POPUPS.with(|p| {
        p.borrow_mut().remove(tab_id);
    });
    POPUP_RECTS.with(|r| {
        r.borrow_mut().remove(tab_id);
    });
    VIEWS.with(|v| {
        v.borrow_mut().remove(tab_id);
    });
    // The destination buffers too, or the cache grows by a tab's worth of
    // 24 MB allocations every time a tab is closed and never shrinks.
    GPU.with(|cell| {
        if let Some(gpu) = cell.borrow_mut().as_mut() {
            gpu.forget_tab(tab_id);
        }
    });
}

// ---------------------------------------------------------------------------
// Metal
// ---------------------------------------------------------------------------

/// A destination buffer and the Metal texture that aliases it. One thing,
/// two views: the `CVPixelBuffer` is what gpui's scene takes, the
/// `MTLTexture` is what the blit writes into.
type Dest = (CVPixelBuffer, Retained<ProtocolObject<dyn MTLTexture>>);

/// Which surface of which tab a destination buffer belongs to.
///
/// A browser under off-screen rendering paints TWO things, and CEF
/// distinguishes them only by a `PaintElementType` on the callback: the
/// page ([`PaintElementType::VIEW`]) and, when a `<select>` is open or
/// Chromium wants to draw its own autofill list or colour picker, a popup
/// ([`PaintElementType::POPUP`]) with its own size and its own position
/// inside the view.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct SurfaceKey {
    tab_id: String,
    popup: bool,
}

/// Where a tab's popup is, inside the page, in logical pixels.
///
/// CEF gives this in VIEW coordinates — relative to the browser, not to the
/// window and not to the screen — which is exactly what the page element
/// needs, because the element paints the page at its own bounds and the
/// popup is offset from there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopupRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

struct Gpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// One destination per SURFACE per size, reused.
    ///
    /// **The key carries the surface and not just the size, and that is a
    /// fix rather than a detail.** Keyed on `(w, h)` alone — as it was —
    /// every tab laid out at the same size shared one `CVPixelBuffer`,
    /// because `CVPixelBuffer::clone` is a retain and not a copy. Two page
    /// tabs are the same size in the common case (a background tab is
    /// never told it was hidden, so it keeps painting at the size the
    /// foreground tab last published), so a background tab's frames landed
    /// in the buffer the foreground tab was displaying. The page's own
    /// `<select>` popup would have collided the same way the moment it
    /// matched the page's dimensions.
    ///
    /// Reallocating per frame is still not the answer — that would be a
    /// 24 MB malloc 60 times a second — so the cache stays and the key
    /// gets wider.
    dest: HashMap<(SurfaceKey, u32, u32), Dest>,
}

impl Gpu {
    fn new() -> Option<Self> {
        let device = MTLCreateSystemDefaultDevice()?;
        let queue = device.newCommandQueue()?;
        Some(Self {
            device,
            queue,
            dest: HashMap::new(),
        })
    }

    fn dest_texture(&mut self, key: &SurfaceKey, w: u32, h: u32) -> Option<Dest> {
        if let Some(found) = self.dest.get(&(key.clone(), w, h)) {
            return Some(found.clone());
        }
        let made = make_dest(&self.device, w, h)?;
        self.dest.insert((key.clone(), w, h), made.clone());
        Some(made)
    }

    /// Drop every destination belonging to a tab. Called when its browser
    /// closes — without this the cache is a leak keyed by a tab id that
    /// will never be asked for again, at 24 MB a size.
    fn forget_tab(&mut self, tab_id: &str) {
        self.dest.retain(|(k, _, _), _| k.tab_id != tab_id);
    }
}

/// Allocate an IOSurface-backed BGRA `CVPixelBuffer` and a Metal texture
/// that aliases it. This is the shape the destination has to have anyway:
/// `CVPixelBuffer` is what gpui's surface primitive takes, and
/// IOSurface-backed is what its `CVMetalTextureCache` requires.
fn make_dest(device: &Retained<ProtocolObject<dyn MTLDevice>>, w: u32, h: u32) -> Option<Dest> {
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
    let buffer = CVPixelBuffer::new(
        kCVPixelFormatType_32BGRA,
        w as usize,
        h as usize,
        Some(&attrs),
    )
    .ok()?;
    let io_surface = unsafe { CVPixelBufferGetIOSurface(buffer.as_concrete_TypeRef()) };
    let surface = std::ptr::NonNull::new(io_surface.cast::<IOSurfaceRef>())?;

    let desc = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm,
            w as usize,
            h as usize,
            false,
        )
    };
    desc.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);
    desc.setStorageMode(MTLStorageMode::Shared);
    let texture =
        unsafe { device.newTextureWithDescriptor_iosurface_plane(&desc, surface.as_ref(), 0)? };
    Some((buffer, texture))
}

/// Open CEF's IOSurface as a texture and copy it into one we own.
///
/// **Commit and go, never `waitUntilCompleted`.** The spike measured 0.077 ms
/// committing and 0.637 ms waiting; the wait existed only so the measurement
/// would be GPU work rather than encode time. Blocking the pump on the GPU
/// every frame would be paying 8× for a number nobody reads at runtime — and
/// the copy is ordered against the read by Metal regardless.
fn import_and_blit(
    tab_id: &str,
    popup: bool,
    io_surface: *mut std::ffi::c_void,
    w: u32,
    h: u32,
) -> bool {
    if w == 0 || h == 0 {
        return false;
    }
    let key = SurfaceKey {
        tab_id: tab_id.to_string(),
        popup,
    };
    GPU.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Gpu::new();
        }
        let Some(gpu) = slot.as_mut() else {
            return false;
        };
        let Some(surface) = std::ptr::NonNull::new(io_surface.cast::<IOSurfaceRef>()) else {
            return false;
        };

        // Reopened every frame, by contract. See the module docs — the pool
        // never repeats a handle, so caching by one is worse than useless.
        let desc = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                w as usize,
                h as usize,
                false,
            )
        };
        desc.setUsage(MTLTextureUsage::ShaderRead);
        desc.setStorageMode(MTLStorageMode::Private);
        let Some(src) = (unsafe {
            gpu.device
                .newTextureWithDescriptor_iosurface_plane(&desc, surface.as_ref(), 0)
        }) else {
            return false;
        };

        let Some((buffer, dest)) = gpu.dest_texture(&key, w, h) else {
            return false;
        };
        let Some(cb) = gpu.queue.commandBuffer() else {
            return false;
        };
        let Some(encoder) = cb.blitCommandEncoder() else {
            return false;
        };
        unsafe {
            encoder.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                &src,
                0,
                0,
                MTLOrigin { x: 0, y: 0, z: 0 },
                MTLSize { width: w as usize, height: h as usize, depth: 1 },
                &dest,
                0,
                0,
                MTLOrigin { x: 0, y: 0, z: 0 },
            );
        }
        encoder.endEncoding();
        cb.commit();

        // A POPUP keeps a bare buffer and a PAGE carries its coded size.
        // That asymmetry is deliberate: `Frame` exists so a page can be
        // painted 1:1 when Chromium is a resize behind, and a popup is
        // placed by its own rect and must NOT go through that clipped path.
        let evicted = if popup {
            POPUPS.with(|p| p.borrow_mut().insert(tab_id.to_string(), buffer));
            None
        } else {
            PAGES.with(|p| {
                p.borrow_mut().insert(
                    tab_id.to_string(),
                    Frame {
                        buffer,
                        width: w,
                        height: h,
                    },
                )
            })
        };
        // Dropped outside the borrow. A `CVPixelBuffer` release is not a CEF
        // call, so this is not the `with_browser` hazard — but the shape is
        // the one this module keeps, and a `Frame` will grow fields.
        drop(evicted);
        let n = FRAMES.fetch_add(1, Ordering::Relaxed);
        // `EAVE_WEB_PROBE_CLICK` fires HERE, on a page frame, and only on
        // the one at `PROBE_AFTER_FRAMES` — see `tabs::probe_click` for why
        // load-end was too early. Once, because a probe that clicked on
        // every frame would reopen the popup it just closed forever.
        if !popup && n == PROBE_AFTER_FRAMES {
            super::tabs::probe_click(tab_id);
        }
        true
    })
}

// ---------------------------------------------------------------------------
// The handler CEF calls
// ---------------------------------------------------------------------------

wrap_render_handler! {
    pub struct PageRenderHandler {
        tab_id: String,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                let v = view(&self.tab_id);
                rect.x = 0;
                rect.y = 0;
                rect.width = v.width;
                rect.height = v.height;
            }
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, info: Option<&mut ScreenInfo>) -> i32 {
            let Some(info) = info else { return 0 };
            let v = view(&self.tab_id);
            info.device_scale_factor = v.scale;
            info.depth = 32;
            info.depth_per_component = 8;
            info.is_monochrome = 0;
            info.rect = Rect { x: 0, y: 0, width: v.width, height: v.height };
            info.available_rect = info.rect.clone();
            1
        }

        fn screen_point(
            &self,
            _browser: Option<&mut Browser>,
            view_x: i32,
            view_y: i32,
            screen_x: Option<&mut i32>,
            screen_y: Option<&mut i32>,
        ) -> i32 {
            // Page space and element space are the same space, because the
            // element publishes the bounds it was laid out at. Nothing to
            // convert, and no fudge factor to get wrong.
            if let Some(x) = screen_x { *x = view_x; }
            if let Some(y) = screen_y { *y = view_y; }
            1
        }

        /// The GPU path — the one that runs.
        /// A popup opened or closed.
        ///
        /// The retraction is the half that is easy to miss: Chromium does
        /// NOT paint an empty frame on the way down, so the last popup
        /// frame stays in `POPUPS` after the menu closes. Clearing the RECT
        /// is what takes the widget off the screen — [`popup`] needs both,
        /// so a frame with no rect is not drawn.
        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: i32) {
            if super::tabs::trace() {
                eprintln!("[web] on_popup_show show={show} tab={}", self.tab_id);
            }
            if show == 0 {
                hide_popup(&self.tab_id);
                // Count it: the page underneath is now un-occluded and the
                // window has to be redrawn to show that, and the pump only
                // redraws when this counter moves.
                FRAMES.fetch_add(1, Ordering::Relaxed);
            }
        }

        /// Where the popup goes, in view coordinates.
        ///
        /// Always called before the first popup paint, so storing the rect
        /// here and the pixels there cannot race: the rect is present by
        /// the time a frame exists to place.
        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            let Some(rect) = rect else { return };
            let next = PopupRect {
                x: rect.x as f32,
                y: rect.y as f32,
                width: rect.width as f32,
                height: rect.height as f32,
            };
            if super::tabs::trace() {
                eprintln!(
                    "[web] on_popup_size {}x{} at {},{}",
                    next.width, next.height, next.x, next.y
                );
            }
            show_popup(&self.tab_id, next);
        }

        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            let Some(info) = info else { return };
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                let n = FRAMES.load(Ordering::Relaxed);
                if n < 4 || n.is_multiple_of(200) {
                    eprintln!("[web] accel_paint #{n} type={:?} {}x{}", type_,
                        info.extra.coded_size.width, info.extra.coded_size.height);
                }
            }
            // A `<select>` dropdown is a SECOND surface with its own
            // geometry, and it arrives here with `POPUP` on the callback and
            // nothing else to tell it apart. It used to be dropped, which
            // meant every `<select>` on the web opened a menu that could not
            // be seen and still swallowed the next click. It is now blitted
            // into its own destination and painted over the page at the rect
            // `on_popup_size` gave us.
            let popup = type_ == PaintElementType::POPUP;
            let w = info.extra.coded_size.width.max(0) as u32;
            let h = info.extra.coded_size.height.max(0) as u32;
            if popup && super::tabs::trace() {
                let n = POPUP_FRAMES.fetch_add(1, Ordering::Relaxed);
                eprintln!("[web] popup frame #{n} {w}x{h}");
            }
            import_and_blit(&self.tab_id, popup, info.shared_texture_io_surface, w, h);
        }

        /// The CPU fallback. Reached only if the GPU path is unavailable —
        /// on this machine it never is, so this exists so that "no shared
        /// texture" degrades to a slow page rather than a blank one.
        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            _buffer: *const u8,
            _width: i32,
            _height: i32,
        ) {
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                eprintln!("[web] cpu on_paint {_width}x{_height}");
            }
            let _ = type_;
            // Not implemented: the accelerated path is the one the spike
            // measured and the one CEF takes here. Saying so beats a silent
            // half-copy that would look like a rendering bug.
            static WARNED: AtomicU64 = AtomicU64::new(0);
            if WARNED.fetch_add(1, Ordering::Relaxed) == 0 {
                eprintln!("eave: CEF fell back to CPU painting; the page will not draw");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// The popup state machine and the surface keying, which are eave's own
// reasoning. Nothing here starts CEF or touches Metal.

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> PopupRect {
        PopupRect {
            x: 12.,
            y: 40.,
            width: 180.,
            height: 220.,
        }
    }

    #[test]
    fn no_popup_until_one_is_sized() {
        assert!(!popup_shown("p1"));
        show_popup("p1", rect());
        assert!(popup_shown("p1"));
    }

    /// The retraction. Chromium paints no empty frame on the way down, so
    /// if the rect did not decide visibility the last `<select>` a tab
    /// opened would stay on screen for the life of the tab.
    #[test]
    fn closing_a_popup_takes_it_off_the_screen() {
        show_popup("p2", rect());
        hide_popup("p2");
        assert!(!popup_shown("p2"));
    }

    /// A popup that has been sized but not yet painted must draw NOTHING
    /// rather than a black rectangle — `popup()` needs both halves, which
    /// is why it is one function and not two.
    #[test]
    fn a_sized_popup_with_no_frame_yet_paints_nothing() {
        show_popup("p3", rect());
        assert!(popup_shown("p3"));
        assert!(
            popup("p3").is_none(),
            "a rect with no pixels is not something to paint"
        );
    }

    #[test]
    fn tabs_do_not_share_a_popup() {
        show_popup("p4a", rect());
        assert!(popup_shown("p4a"));
        assert!(!popup_shown("p4b"));
    }

    #[test]
    fn closing_a_tab_takes_its_popup_with_it() {
        show_popup("p5", rect());
        forget("p5");
        assert!(!popup_shown("p5"));
    }

    /// The bug the wider key fixes. Two tabs laid out at the same size --
    /// the COMMON case, because a background tab is never told it was
    /// hidden and keeps the size the foreground tab published -- must not
    /// resolve to the same destination buffer, or one tab's frames land in
    /// the buffer the other is displaying.
    #[test]
    fn two_tabs_at_the_same_size_are_different_surfaces() {
        let a = SurfaceKey {
            tab_id: "t-a".into(),
            popup: false,
        };
        let b = SurfaceKey {
            tab_id: "t-b".into(),
            popup: false,
        };
        assert_ne!(a, b);
    }

    /// And a tab's own popup is not its page, however the sizes land.
    #[test]
    fn a_tabs_popup_is_not_its_page() {
        let page = SurfaceKey {
            tab_id: "t-c".into(),
            popup: false,
        };
        let menu = SurfaceKey {
            tab_id: "t-c".into(),
            popup: true,
        };
        assert_ne!(page, menu);
    }
}
