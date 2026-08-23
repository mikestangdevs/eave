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
    /// Carrying the size it DEPICTS is what lets the element ask "is this
    /// frame the right size?" and refuse to scale when the answer is no.
    ///
    /// That size is `source_size`, and it is not the size of the buffer.
    /// Reading the buffer's own size instead is what painted Chromium's
    /// letterbox bars on screen for two slices — see [`Content`].
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

/// A finished page frame, the pixels it holds, and the view those pixels
/// are a rendering **of**.
///
/// The three are one value because they are only ever true together: the
/// moment they are separated, something will paint the pixels at a size
/// they were not drawn for and call it a resize.
#[derive(Clone)]
pub struct Frame {
    /// The pixels, cropped to `content_rect` — so the buffer's own width and
    /// height are the size of the picture, and there is no third size to
    /// carry here.
    pub buffer: CVPixelBuffer,
    /// **Device** pixels this frame DEPICTS — `source_size`. Equal to the
    /// buffer's own size in the steady state, and larger whenever Chromium
    /// answered a resize by scaling the old view down into a smaller buffer.
    /// This is the one thing the pixels cannot say about themselves, which
    /// is why it is the one thing stored beside them.
    pub src_width: u32,
    pub src_height: u32,
}

impl Frame {
    /// The logical size this frame is a picture of, under `scale`.
    ///
    /// **`src_*`, not `width`/`height`, and that distinction is the dark-band
    /// fix.** A letterboxed frame holds a 1439×1200 rendering of a 1672×1394
    /// view; asking "how big is this frame" and answering 1439×1200 makes the
    /// element treat a correct picture of the whole page as a stale frame that
    /// is 200 px too narrow. What the caller wants to know is what the pixels
    /// SHOW, which is the source size.
    pub fn logical(&self, scale: f32) -> (f32, f32) {
        let scale = if scale > 0. { scale } else { 1. };
        (
            self.src_width as f32 / scale,
            self.src_height as f32 / scale,
        )
    }
}

/// Which part of a delivered buffer is the page, and what view it is a
/// rendering of.
///
/// **CEF hands over three geometries and eave used to read one.** From a
/// live trace of the first four frames of a page (`coded` / `content` /
/// `source`):
///
/// ```text
/// #0  coded=1600x1200  content=80+0+1439x1200  source=1672x1394
/// #1  coded=1672x1394  content=0+0+1672x1394   source=1672x1394
/// ```
///
/// Frame #0 is the browser's creation-size buffer carrying a picture of the
/// view it had just been resized to. Chromium preserves aspect: 1672/1394 is
/// 1.1994, so a 1200-tall fit is 1439 wide and it is centred at x=80. **The
/// 80 and 81 columns either side are opaque black**, measured by reading the
/// buffer back — `left=80 right=81`, `(0,0,0,255)`.
///
/// Painting the whole `coded_size` therefore paints Chromium's letterbox
/// bars. They are black rather than dark, they sit inside the page's own
/// edges rather than eave's, and on a page that never repaints they are
/// permanent — which is exactly what the operator reported as "this weird
/// darken at the top and bottom". Whether the bars land on the sides or on
/// the top and bottom is just which way the aspect went.
///
/// So: crop to `content_rect`, and remember `source_size` as what the crop
/// depicts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Content {
    /// Where the picture is inside the delivered buffer, in device pixels.
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    /// The view size the picture is a rendering of, in device pixels.
    src_width: u32,
    src_height: u32,
}

impl Content {
    /// Read the three rects, defensively.
    ///
    /// **Every fallback is to the old behaviour**, because a frame we cannot
    /// interpret should be painted exactly as it was before this fix rather
    /// than dropped or cropped to nothing. A degenerate `content_rect`, one
    /// that escapes the buffer, or an absent `source_size` each fall back a
    /// single step, not all the way.
    fn of(coded_w: u32, coded_h: u32, content: &Rect, source: &Size, has_source: bool) -> Self {
        let fits = content.x >= 0
            && content.y >= 0
            && content.width > 0
            && content.height > 0
            && (content.x as i64 + content.width as i64) <= coded_w as i64
            && (content.y as i64 + content.height as i64) <= coded_h as i64;
        let (x, y, width, height) = if fits {
            (
                content.x as u32,
                content.y as u32,
                content.width as u32,
                content.height as u32,
            )
        } else {
            (0, 0, coded_w, coded_h)
        };
        let (src_width, src_height) = if has_source && source.width > 0 && source.height > 0 {
            (source.width as u32, source.height as u32)
        } else {
            (width, height)
        };
        Self {
            x,
            y,
            width,
            height,
            src_width,
            src_height,
        }
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

/// `EAVE_WEB_BANDPROBE=1` — read a delivered frame back and report the runs
/// of pure black at each edge.
///
/// **A knob rather than a deleted scratch script, for the reason E1 kept
/// `EAVE_WEB_STRETCH`:** this is the instrument that turned "there is a
/// weird darken at the top and bottom" into a root cause, and re-deriving it
/// costs an afternoon. It answers exactly one question — *are the edges of
/// what CEF handed us black?* — which separates "Chromium sent us bars" from
/// every other way a page can look wrong.
///
/// It costs a `waitUntilCompleted` per probed frame, which is why it probes
/// the first four and then stops, and why it is off unless asked for.
fn bandprobe() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("EAVE_WEB_BANDPROBE").is_ok())
}

fn probe_bands(n: u64, popup: bool, buffer: &CVPixelBuffer) {
    use core_video::pixel_buffer::kCVPixelBufferLock_ReadOnly as READ_ONLY;
    let what = if popup { "popup" } else { "page" };
    let w = buffer.get_width();
    let h = buffer.get_height();
    if w == 0 || h == 0 {
        eprintln!("[band] {what} #{n}: empty buffer");
        return;
    }
    if buffer.lock_base_address(READ_ONLY) != 0 {
        eprintln!("[band] {what} #{n}: could not lock");
        return;
    }
    let base = unsafe { buffer.get_base_address() } as *const u8;
    let stride = buffer.get_bytes_per_row();
    // BGRA, so the byte order is reversed on the way out.
    let px = |x: usize, y: usize| -> (u8, u8, u8, u8) {
        let o = unsafe { base.add(y * stride + x * 4) };
        unsafe { (*o.add(2), *o.add(1), *o, *o.add(3)) }
    };
    let black = |c: (u8, u8, u8, u8)| c.0 == 0 && c.1 == 0 && c.2 == 0;
    let run = |n: usize, f: &dyn Fn(usize) -> (u8, u8, u8, u8)| {
        let mut i = 0;
        while i < n && black(f(i)) {
            i += 1;
        }
        i
    };
    let (mx, my) = (w / 2, h / 2);
    let left = run(w, &|i| px(i, my));
    let right = run(w, &|i| px(w - 1 - i, my));
    let top = run(h, &|i| px(mx, i));
    let bottom = run(h, &|i| px(mx, h - 1 - i));
    eprintln!(
        "[band] {what} #{n} buf={w}x{h} black-run left={left} right={right} top={top} bottom={bottom} centre={:?}",
        px(mx, my)
    );
    buffer.unlock_base_address(READ_ONLY);
}

/// Open CEF's IOSurface as a texture and copy it into one we own.
///
/// **Commit and go, never `waitUntilCompleted`.** The spike measured 0.077 ms
/// committing and 0.637 ms waiting; the wait existed only so the measurement
/// would be GPU work rather than encode time. Blocking the pump on the GPU
/// every frame would be paying 8× for a number nobody reads at runtime — and
/// the copy is ordered against the read by Metal regardless.
///
/// **Cropped to `content_rect` on the way through**, so Chromium's letterbox
/// bars are never copied and never reach the screen. `coded_*` is the shape
/// of the surface CEF handed us — the only shape it can be opened as — and
/// `content` says which part of it is the page.
fn import_and_blit(
    tab_id: &str,
    popup: bool,
    io_surface: *mut std::ffi::c_void,
    coded_w: u32,
    coded_h: u32,
    content: Content,
) -> bool {
    if coded_w == 0 || coded_h == 0 || content.width == 0 || content.height == 0 {
        return false;
    }
    let (w, h) = (content.width, content.height);
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
        // The SOURCE is the whole coded buffer — that is the shape CEF
        // allocated and the only shape the IOSurface can be opened as. The
        // crop happens in the copy below, not here.
        let desc = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                coded_w as usize,
                coded_h as usize,
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
                MTLOrigin {
                    x: content.x as usize,
                    y: content.y as usize,
                    z: 0,
                },
                MTLSize { width: w as usize, height: h as usize, depth: 1 },
                &dest,
                0,
                0,
                MTLOrigin { x: 0, y: 0, z: 0 },
            );
        }
        encoder.endEncoding();
        cb.commit();

        // `EAVE_WEB_BANDPROBE=1` — read the delivered buffer back and say
        // whether it has black edges. See [`probe_bands`].
        if bandprobe() {
            let n = if popup {
                POPUP_FRAMES.load(Ordering::Relaxed)
            } else {
                FRAMES.load(Ordering::Relaxed)
            };
            if n < 4 {
                cb.waitUntilCompleted();
                probe_bands(n, popup, &buffer);
            }
        }

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
                        src_width: content.src_width,
                        src_height: content.src_height,
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
                if n < 8 || n.is_multiple_of(200) {
                    // All three geometries, because reading only the first
                    // is what put Chromium's letterbox bars on screen for
                    // two slices. `visible_rect` is omitted: it measured
                    // equal to `coded_size` in every frame of every page
                    // tried, and a field that never varies is noise.
                    let e = &info.extra;
                    eprintln!(
                        "[web] accel_paint #{n} type={:?} coded={}x{} content={}+{}+{}x{} source={}x{}",
                        type_,
                        e.coded_size.width, e.coded_size.height,
                        e.content_rect.x, e.content_rect.y,
                        e.content_rect.width, e.content_rect.height,
                        e.source_size.width, e.source_size.height,
                    );
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
            // Crop to the picture, not to the allocation. A frame whose
            // `content_rect` is a strict sub-rect of `coded_size` has
            // Chromium's letterbox bars in the remainder, and they are
            // opaque black — see [`Content`].
            let content = Content::of(
                w,
                h,
                &info.extra.content_rect,
                &info.extra.source_size,
                info.extra.has_source_size != 0,
            );
            if popup && super::tabs::trace() {
                let n = POPUP_FRAMES.fetch_add(1, Ordering::Relaxed);
                eprintln!("[web] popup frame #{n} {w}x{h}");
            }
            import_and_blit(
                &self.tab_id,
                popup,
                info.shared_texture_io_surface,
                w,
                h,
                content,
            );
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

    fn size(w: i32, h: i32) -> Size {
        Size {
            width: w,
            height: h,
        }
    }

    fn cef_rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    /// The steady state, and the case that must not change: one frame, one
    /// size, nothing to crop.
    #[test]
    fn a_full_bleed_frame_is_cropped_to_itself() {
        let c = Content::of(
            1672,
            1394,
            &cef_rect(0, 0, 1672, 1394),
            &size(1672, 1394),
            true,
        );
        assert_eq!((c.x, c.y, c.width, c.height), (0, 0, 1672, 1394));
        assert_eq!((c.src_width, c.src_height), (1672, 1394));
    }

    /// **The dark bands, as CEF actually delivered them.** These are not
    /// invented numbers: they are frame #0 of a page opened in this app,
    /// read off an `EAVE_WEB_TRACE` log, and the buffer was read back at the
    /// same moment to confirm the 80 columns to the left and the 81 to the
    /// right were opaque `(0,0,0,255)`.
    ///
    /// Chromium fitted a 1672x1394 view into the 1600x1200 buffer the
    /// browser was created with, preserving aspect: 1672/1394 = 1.1994, so
    /// 1200 tall is 1439 wide, centred at x = (1600-1439)/2 = 80.
    #[test]
    fn a_letterboxed_frame_crops_to_the_picture_and_remembers_what_it_shows() {
        let c = Content::of(
            1600,
            1200,
            &cef_rect(80, 0, 1439, 1200),
            &size(1672, 1394),
            true,
        );
        // The bars are never copied.
        assert_eq!((c.x, c.y, c.width, c.height), (80, 0, 1439, 1200));
        // And the crop is understood as a picture of the FULL view, so the
        // element stretches it back rather than treating it as a frame that
        // is 233 device pixels too narrow.
        assert_eq!((c.src_width, c.src_height), (1672, 1394));
    }

    /// The other way round — a buffer wider in aspect than its source gets
    /// bars on the top and bottom. This is the one the operator reported.
    #[test]
    fn bars_on_the_top_and_bottom_crop_the_same_way() {
        let c = Content::of(
            1672,
            1394,
            &cef_rect(0, 87, 1672, 1220),
            &size(2072, 1512),
            true,
        );
        assert_eq!((c.x, c.y, c.width, c.height), (0, 87, 1672, 1220));
        assert_eq!((c.src_width, c.src_height), (2072, 1512));
    }

    /// No `source_size` — an older CEF, or a frame that simply does not
    /// carry one. The crop still happens; what it depicts falls back to its
    /// own size, which is the honest answer and not the old behaviour.
    #[test]
    fn without_a_source_size_a_crop_depicts_itself() {
        let c = Content::of(1600, 1200, &cef_rect(80, 0, 1439, 1200), &size(0, 0), false);
        assert_eq!((c.x, c.y, c.width, c.height), (80, 0, 1439, 1200));
        assert_eq!((c.src_width, c.src_height), (1439, 1200));
    }

    /// **Every unreadable rect falls back to the whole buffer, which is
    /// exactly what this file did before the fix.** A frame we cannot
    /// interpret must be painted as it always was, not dropped and not
    /// cropped to nothing — a black band is a blemish, a blank page is an
    /// outage.
    #[test]
    fn a_content_rect_that_makes_no_sense_falls_back_to_the_whole_buffer() {
        for bad in [
            cef_rect(0, 0, 0, 1200),      // empty
            cef_rect(0, 0, 1439, 0),      // empty
            cef_rect(-4, 0, 1439, 1200),  // negative origin
            cef_rect(0, -4, 1439, 1200),  // negative origin
            cef_rect(400, 0, 1439, 1200), // runs off the right
            cef_rect(0, 400, 1439, 1200), // runs off the bottom
            cef_rect(0, 0, 9999, 9999),   // larger than the allocation
        ] {
            let c = Content::of(1600, 1200, &bad, &size(1672, 1394), true);
            assert_eq!(
                (c.x, c.y, c.width, c.height),
                (0, 0, 1600, 1200),
                "an uninterpretable content_rect {bad:?} must fall back to the coded size"
            );
        }
    }

    /// A `source_size` of zero is not a size. Falling for it would publish a
    /// frame that depicts nothing, and `Frame::logical` would divide it into
    /// a zero-sized paint.
    #[test]
    fn a_zero_source_size_is_ignored_even_when_it_is_flagged_present() {
        let c = Content::of(
            1672,
            1394,
            &cef_rect(0, 0, 1672, 1394),
            &size(0, 1394),
            true,
        );
        assert_eq!((c.src_width, c.src_height), (1672, 1394));
    }

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
