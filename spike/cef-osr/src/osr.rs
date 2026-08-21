//! The off-screen-rendering arm of the probe.
//!
//! This exists to answer one question with numbers rather than opinion: when
//! CEF renders windowlessly, what does it actually hand us each frame, how
//! often, and what does it cost us to turn that into something a Metal
//! renderer can sample?
//!
//! CEF's contract (`cef_render_handler.h`, `OnAcceleratedPaint`) is the whole
//! reason this file is more than ten lines:
//!
//! > The underlying implementation uses a pool to deliver frames. As a result,
//! > the handle may differ every frame [...] The handle's resource cannot be
//! > cached and cannot be accessed outside of this callback. It should be
//! > reopened each time this callback is executed and the contents should be
//! > copied to a texture owned by the client application.
//!
//! So "shared texture" does NOT mean "hand the page's IOSurface to the
//! compositor and forget about it". It means: open it, blit out of it, and
//! give it back before you return. That per-frame blit is the cost this
//! probe measures — `import_us` (open the IOSurface as an `MTLTexture`) and
//! `blit_us` (copy it into a texture we own).

use cef::*;
use std::cell::RefCell;
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLOrigin, MTLPixelFormat, MTLSize, MTLStorageMode,
    MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

use core_foundation::{base::TCFType, dictionary::CFDictionary, number::CFNumber, string::CFString};
use core_video::pixel_buffer::{kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferKeys};

unsafe extern "C" {
    fn CVPixelBufferGetIOSurface(
        buffer: core_video::pixel_buffer::CVPixelBufferRef,
    ) -> *mut std::ffi::c_void;
}

/// Allocate the texture we own as an IOSurface-backed BGRA `CVPixelBuffer`.
///
/// This is the shape the destination has to have anyway: `CVPixelBuffer` is
/// what gpui's surface primitive takes, and IOSurface-backed is what its
/// `CVMetalTextureCache` requires. Blitting into a plain private texture
/// would have measured a slightly cheaper thing than the real one.
fn make_dest(
    device: &Retained<ProtocolObject<dyn MTLDevice>>,
    w: u32,
    h: u32,
) -> Option<(CVPixelBuffer, Retained<ProtocolObject<dyn MTLTexture>>)> {
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
    let buffer =
        CVPixelBuffer::new(kCVPixelFormatType_32BGRA, w as usize, h as usize, Some(&attrs)).ok()?;
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
    unsafe {
        desc.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);
        desc.setStorageMode(MTLStorageMode::Shared);
    }
    let texture = unsafe {
        device.newTextureWithDescriptor_iosurface_plane(&desc, surface.as_ref(), 0)?
    };
    Some((buffer, texture))
}

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => !(v == "0" || v.is_empty()),
        Err(_) => default,
    }
}

fn env_num<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// `EAVE_CEF_OSR=1` selects the windowless arm.
pub fn osr_enabled() -> bool {
    env_flag("EAVE_CEF_OSR", false) || crate::gpui_host::gpui_mode()
}

/// `EAVE_CEF_SHARED=0` forces the CPU `OnPaint` path so the two upload
/// routes can be compared on the same page and the same machine.
pub fn shared_texture() -> bool {
    env_flag("EAVE_CEF_SHARED", true)
}

/// Logical size of the off-screen view. Device pixels are this times
/// [`scale`], which is what actually gets copied.
pub fn view_size() -> (i32, i32) {
    (env_num("EAVE_CEF_W", 1600), env_num("EAVE_CEF_H", 1000))
}

pub fn scale() -> f32 {
    env_num("EAVE_CEF_SCALE", 2.0)
}

/// The requested `windowless_frame_rate`. CEF's own default is 30, which is
/// not a property of the display and not a property of the page.
pub fn frame_rate() -> i32 {
    env_num("EAVE_CEF_FPS", 30)
}

/// `EAVE_CEF_CACHE=1` keys imported `MTLTexture`s by IOSurface pointer.
/// CEF cycles a small pool of surfaces, so the same handles recur — the
/// question is whether the per-frame `newTextureWithDescriptor:iosurface:`
/// cost is a genuine tax or just an unamortised one.
pub fn cache_textures() -> bool {
    env_flag("EAVE_CEF_CACHE", false)
}

/// `EAVE_CEF_WAIT=0` drops the `waitUntilCompleted` after the blit. Waiting
/// measures GPU work honestly but serialises it; a real integration would
/// commit and let the compositor pick the texture up a frame later. Both
/// numbers matter, for different reasons.
pub fn wait_for_blit() -> bool {
    env_flag("EAVE_CEF_WAIT", true)
}

/// Seconds to sample before printing the report and quitting.
pub fn duration_secs() -> u64 {
    env_num("EAVE_CEF_SECS", 12)
}

// ---------------------------------------------------------------------------
// Metal side
// ---------------------------------------------------------------------------

/// Everything Metal, kept thread-local because `OnAcceleratedPaint` arrives on
/// CEF's UI thread and `Retained<_>` is not `Send`. In a real integration this
/// is the piece that would have to be handed across to gpui's render thread;
/// here it stays put, which is exactly why the numbers below are a *floor*.
struct Gpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// The texture we own and CEF does not, plus the `CVPixelBuffer` that
    /// wraps the same IOSurface — the handle gpui's scene takes.
    dest: Option<(u32, u32, Retained<ProtocolObject<dyn MTLTexture>>, CVPixelBuffer)>,
    /// Imported views onto CEF's pool surfaces, keyed by IOSurface pointer.
    imported: Vec<(usize, Retained<ProtocolObject<dyn MTLTexture>>)>,
}

thread_local! {
    static GPU: RefCell<Option<Gpu>> = const { RefCell::new(None) };
}

impl Gpu {
    fn new() -> Option<Self> {
        let device = MTLCreateSystemDefaultDevice()?;
        let queue = device.newCommandQueue()?;
        Some(Self {
            device,
            queue,
            dest: None,
            imported: Vec::new(),
        })
    }

    fn dest_texture(
        &mut self,
        w: u32,
        h: u32,
    ) -> Option<(Retained<ProtocolObject<dyn MTLTexture>>, CVPixelBuffer)> {
        if let Some((dw, dh, tex, buf)) = &self.dest {
            if *dw == w && *dh == h {
                return Some((tex.clone(), buf.clone()));
            }
        }
        let (buffer, texture) = make_dest(&self.device, w, h)?;
        self.dest = Some((w, h, texture.clone(), buffer.clone()));
        Some((texture, buffer))
    }
}

/// Open CEF's IOSurface as an `MTLTexture` and copy it into a texture we own.
///
/// Returns `(import_us, blit_us, width, height)`. The blit is timed with
/// `waitUntilCompleted` so the number is GPU work, not just encode time —
/// pessimistic on purpose, since a real integration would pipeline it.
fn import_and_blit(io_surface: *mut std::ffi::c_void, w: u32, h: u32) -> Option<(u128, u128, u32, u32)> {
    GPU.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Gpu::new();
        }
        let gpu = slot.as_mut()?;

        let surface = std::ptr::NonNull::new(io_surface.cast::<IOSurfaceRef>())?;

        let t0 = Instant::now();
        let key = io_surface as usize;
        if cache_textures() {
            if let Some((_, tex)) = gpu.imported.iter().find(|(k, _)| *k == key) {
                let src = tex.clone();
                let import_us = t0.elapsed().as_micros();
                let (dest, buffer) = gpu.dest_texture(w, h)?;
                let (blit_us, _) = blit(gpu, &src, &dest, w, h)?;
                crate::gpui_host::publish(buffer);
                return Some((import_us, blit_us, w, h));
            }
        }
        let desc = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                w as usize,
                h as usize,
                false,
            )
        };
        unsafe {
            desc.setUsage(MTLTextureUsage::ShaderRead);
            desc.setStorageMode(MTLStorageMode::Private);
        }
        let src = unsafe {
            gpu.device
                .newTextureWithDescriptor_iosurface_plane(&desc, surface.as_ref(), 0)?
        };
        let import_us = t0.elapsed().as_micros();
        if cache_textures() {
            gpu.imported.push((key, src.clone()));
        }

        let (dest, buffer) = gpu.dest_texture(w, h)?;
        let (blit_us, _) = blit(gpu, &src, &dest, w, h)?;
        crate::gpui_host::publish(buffer);
        Some((import_us, blit_us, w, h))
    })
}

fn blit(
    gpu: &Gpu,
    src: &Retained<ProtocolObject<dyn MTLTexture>>,
    dest: &Retained<ProtocolObject<dyn MTLTexture>>,
    w: u32,
    h: u32,
) -> Option<(u128, u32)> {
    let t1 = Instant::now();
    let cb = gpu.queue.commandBuffer()?;
    let encoder = cb.blitCommandEncoder()?;
    unsafe {
        encoder.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
            src,
            0,
            0,
            MTLOrigin { x: 0, y: 0, z: 0 },
            MTLSize { width: w as usize, height: h as usize, depth: 1 },
            dest,
            0,
            0,
            MTLOrigin { x: 0, y: 0, z: 0 },
        );
    }
    encoder.endEncoding();
    cb.commit();
    if wait_for_blit() {
        cb.waitUntilCompleted();
    }
    Some((t1.elapsed().as_micros(), 0))
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Stats {
    started: Option<Instant>,
    last_frame: Option<Instant>,
    intervals_us: Vec<u128>,
    import_us: Vec<u128>,
    blit_us: Vec<u128>,
    cpu_copy_us: Vec<u128>,
    accelerated: u64,
    cpu_paint: u64,
    popup_paint: u64,
    last_size: (u32, u32),
    surfaces_seen: Vec<usize>,
    distinct_surfaces: usize,
}

fn pct(v: &mut Vec<u128>, p: f64) -> u128 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let i = (((v.len() - 1) as f64) * p).round() as usize;
    v[i]
}

impl Stats {
    fn tick(&mut self) {
        let now = Instant::now();
        if self.started.is_none() {
            self.started = Some(now);
        }
        if let Some(prev) = self.last_frame {
            self.intervals_us.push(now.duration_since(prev).as_micros());
        }
        self.last_frame = Some(now);
    }

    pub fn report(&mut self) {
        let elapsed = self
            .started
            .map(|s| s.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let frames = self.accelerated + self.cpu_paint;
        println!("\n===== CEF OSR probe =====");
        println!(
            "mode                : {}",
            if shared_texture() {
                "shared texture (OnAcceleratedPaint / IOSurface)"
            } else {
                "cpu buffer (OnPaint / memcpy)"
            }
        );
        println!(
            "view                : {}x{} logical @ {}x  ->  {}x{} device px",
            view_size().0,
            view_size().1,
            scale(),
            self.last_size.0,
            self.last_size.1
        );
        println!(
            "requested fps       : {}   (CEF windowless_frame_rate; its default is 30)",
            frame_rate()
        );
        println!("sampled             : {elapsed:.1} s");
        println!(
            "frames              : {frames} view ({} accelerated, {} cpu), {} popup-widget",
            self.accelerated, self.cpu_paint, self.popup_paint
        );
        if elapsed > 0.0 {
            println!("delivered fps       : {:.1}", frames as f64 / elapsed);
        }
        let p50 = pct(&mut self.intervals_us, 0.50);
        let p95 = pct(&mut self.intervals_us, 0.95);
        println!(
            "frame interval      : p50 {:.2} ms   p95 {:.2} ms",
            p50 as f64 / 1000.0,
            p95 as f64 / 1000.0
        );
        if !self.import_us.is_empty() {
            println!(
                "IOSurface -> MTLTex : p50 {:.3} ms   p95 {:.3} ms",
                pct(&mut self.import_us, 0.50) as f64 / 1000.0,
                pct(&mut self.import_us, 0.95) as f64 / 1000.0
            );
            println!(
                "blit to our texture : p50 {:.3} ms   p95 {:.3} ms  ({})",
                pct(&mut self.blit_us, 0.50) as f64 / 1000.0,
                pct(&mut self.blit_us, 0.95) as f64 / 1000.0,
                if wait_for_blit() { "GPU work, waited" } else { "encode + commit only" }
            );
        }
        if !self.cpu_copy_us.is_empty() {
            let bytes = (self.last_size.0 as u64) * (self.last_size.1 as u64) * 4;
            println!(
                "cpu memcpy of frame : p50 {:.3} ms   p95 {:.3} ms  ({:.1} MB/frame)",
                pct(&mut self.cpu_copy_us, 0.50) as f64 / 1000.0,
                pct(&mut self.cpu_copy_us, 0.95) as f64 / 1000.0,
                bytes as f64 / (1024.0 * 1024.0)
            );
        }
        println!(
            "distinct IOSurfaces : {}   (CEF's delivery pool)",
            self.distinct_surfaces
        );
        println!("=========================\n");
    }
}

thread_local! {
    static STATS: RefCell<Stats> = RefCell::new(Stats::default());
    static CPU_STAGING: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

pub fn report_and_quit() {
    STATS.with(|s| s.borrow_mut().report());
    quit_message_loop();
}

// ---------------------------------------------------------------------------
// The handler CEF actually calls
// ---------------------------------------------------------------------------

wrap_render_handler! {
    pub struct OsrRenderHandler;

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                let (w, h) = view_size();
                rect.x = 0;
                rect.y = 0;
                rect.width = w;
                rect.height = h;
            }
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, info: Option<&mut ScreenInfo>) -> i32 {
            let Some(info) = info else { return 0 };
            let (w, h) = view_size();
            info.device_scale_factor = scale();
            info.depth = 32;
            info.depth_per_component = 8;
            info.is_monochrome = 0;
            info.rect = Rect { x: 0, y: 0, width: w, height: h };
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
            if let Some(x) = screen_x { *x = view_x; }
            if let Some(y) = screen_y { *y = view_y; }
            1
        }

        /// The popup widget: a `<select>` dropdown in OSR is a SECOND
        /// surface, painted through the same callbacks with
        /// `PaintElementType::POPUP`, positioned by `on_popup_size`. Counting
        /// it separately is the point — it is the part of OSR input that has
        /// no equivalent in the native-view arm.
        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: i32) {
            println!("[osr] popup widget {}", if show != 0 { "shown" } else { "hidden" });
        }

        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            if let Some(r) = rect {
                println!("[osr] popup rect {}x{} at {},{}", r.width, r.height, r.x, r.y);
            }
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: i32,
            height: i32,
        ) {
            STATS.with(|s| {
                let mut s = s.borrow_mut();
                if type_ == PaintElementType::POPUP {
                    s.popup_paint += 1;
                    return;
                }
                s.tick();
                s.cpu_paint += 1;
                s.last_size = (width as u32, height as u32);

                // What the CPU path actually costs: the frame has to be copied
                // out of CEF's buffer before the callback returns, exactly as
                // the shared-texture path has to blit before it returns.
                let len = (width as usize) * (height as usize) * 4;
                if !buffer.is_null() && len > 0 {
                    // A persistent staging buffer, not a fresh allocation:
                    // charging the CPU arm for a 24 MB malloc every frame
                    // would flatter the shared-texture arm for no reason.
                    CPU_STAGING.with(|st| {
                        let mut st = st.borrow_mut();
                        if st.len() != len {
                            st.resize(len, 0);
                        }
                        let t = Instant::now();
                        let src = unsafe { std::slice::from_raw_parts(buffer, len) };
                        st.copy_from_slice(src);
                        std::hint::black_box(&*st);
                        s.cpu_copy_us.push(t.elapsed().as_micros());
                    });
                }
            });
        }

        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            let Some(info) = info else { return };
            if type_ == PaintElementType::POPUP {
                STATS.with(|s| s.borrow_mut().popup_paint += 1);
                return;
            }
            let w = info.extra.coded_size.width.max(0) as u32;
            let h = info.extra.coded_size.height.max(0) as u32;
            let key = info.shared_texture_io_surface as usize;
            let result = import_and_blit(info.shared_texture_io_surface, w, h);
            STATS.with(|s| {
                let mut s = s.borrow_mut();
                s.tick();
                s.accelerated += 1;
                s.last_size = (w, h);
                if !s.surfaces_seen.contains(&key) {
                    s.surfaces_seen.push(key);
                    s.distinct_surfaces = s.surfaces_seen.len();
                }
                if let Some((import_us, blit_us, _, _)) = result {
                    s.import_us.push(import_us);
                    s.blit_us.push(blit_us);
                } else if s.accelerated == 1 {
                    println!("[osr] IOSurface import FAILED — handle {:?}", info.shared_texture_io_surface);
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------
//
// A page that is merely *loaded* produces almost no frames — Chromium does not
// composite what does not change. Measuring "frame time on a heavy page" means
// keeping the page busy, and in OSR the only way to do that from outside is to
// forward input by hand. So the driver doubles as the first half of the input
// answer: every scroll below is a `SendMouseWheelEvent` we constructed
// ourselves, because in windowless mode nothing else will.

thread_local! {
    static BROWSER: RefCell<Option<Browser>> = const { RefCell::new(None) };
    static SCROLL_Y: RefCell<i32> = const { RefCell::new(0) };
}

wrap_task! {
    struct ScrollTick;

    impl Task {
        fn execute(&self) {
            BROWSER.with(|b| {
                let mut b = b.borrow_mut();
                let Some(browser) = b.as_mut() else { return };
                let Some(host) = browser.host() else { return };
                let (w, h) = view_size();
                let event = MouseEvent { x: w / 2, y: h / 2, modifiers: 0 };
                // Alternate direction so a finite page keeps producing frames
                // instead of parking at the bottom.
                let dir = SCROLL_Y.with(|s| {
                    let mut s = s.borrow_mut();
                    *s += 1;
                    if (*s / 40) % 2 == 0 { -1 } else { 1 }
                });
                host.send_mouse_wheel_event(Some(&event), 0, dir * 120);
            });

            let mut next = ScrollTick::new();
            post_delayed_task(ThreadId::UI, Some(&mut next), 16);
        }
    }
}

wrap_task! {
    struct Finish;

    impl Task {
        fn execute(&self) {
            report_and_quit();
        }
    }
}

/// Called from `on_after_created`: park the browser handle, start scrolling,
/// and arm the end of the measurement window.
pub fn start_driver() {
    // Give the page a moment to load before the clock starts.
    let mut warmup = StartScrolling::new();
    post_delayed_task(ThreadId::UI, Some(&mut warmup), 2000);

    let mut finish = Finish::new();
    post_delayed_task(
        ThreadId::UI,
        Some(&mut finish),
        (2 + duration_secs()) as i64 * 1000,
    );
}

wrap_task! {
    struct StartScrolling;

    impl Task {
        fn execute(&self) {
            if env_flag("EAVE_CEF_SCROLL", true) {
                let mut tick = ScrollTick::new();
                post_delayed_task(ThreadId::UI, Some(&mut tick), 0);
            }
        }
    }
}

/// Stash the browser so the driver can reach its host. Called from the
/// life-span handler.
pub fn remember_browser(browser: Option<&mut Browser>) {
    if let Some(browser) = browser {
        BROWSER.with(|b| *b.borrow_mut() = Some(browser.clone()));
    }
}
