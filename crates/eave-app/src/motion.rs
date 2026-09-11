//! The frame drive (Slice N1), the tweens it drives (Slice N2), and the
//! loaders that ride its clock (Slice N3).
//!
//! N1 built this as machinery with no consumers at all, behind a blanket
//! `#![allow(dead_code)]`. N2 was the first consumer and took the allow
//! off; N3 is the first consumer of the *clock* half. What is still
//! unreached carries a **named** allow saying which inventory item will
//! reach it, so the compiler keeps every other line of the module honest —
//! which was the point of the exercise: under the blanket, a wrong API
//! shape was invisible, because the only callers were tests written by the
//! same pass that wrote the code.
//!
//! Everything that moves needs a per-frame source, and gpui offers exactly
//! one out of the box: `with_animation`, which requests a redraw every
//! *display* frame for as long as its element stays mounted. comet shipped
//! that shape first and measured it — one Working row pinned the whole
//! window at 120 Hz and cost **36% CPU** on an M-series laptop, because the
//! Metal pipeline stays hot and holds hundreds of MB of graphics buffers to
//! do it (`docs/research/polish-inventory.md` §1.16). What replaced it is
//! this module:
//!
//!   - one shared 30 fps clock, so N mounted loaders cost one timer between
//!     them rather than N,
//!   - one epoch behind all of them, so multi-instance loaders stay
//!     phase-locked instead of each starting from its own mount time,
//!   - a per-view lease renewed on every paint, so the clock parks itself
//!     the moment the last consumer stops asking, and
//!   - a render tail that requests the next frame ONLY while something is
//!     mid-flight.
//!
//! The invariant the whole shape exists to hold: **a window with nothing
//! mid-flight schedules nothing at all.** The two tests that pin it are
//! `nothing_mounted_schedules_no_frames` and
//! `the_clock_parks_when_the_last_lease_expires`; if either has to be
//! relaxed, the design changed, not the test.
//!
//! Time comes from `BackgroundExecutor::now()`, never `Instant::now()` —
//! gpui's own advice, and the reason the lease expiry and the tick interval
//! below are *asserted* rather than argued: the test scheduler's fake clock
//! drives them. (comet reads wall time here and keeps a separate pure core
//! to stay testable; one time source is simpler and strictly stronger.)

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{App, EntityId, Global, Hsla, Rgba, StatefulInteractiveElement, Styled, Window};

// ---------------------------------------------------------------------------
// The measurement knob

/// Slowest and fastest the knob will go. A stretch beyond 100× outlives any
/// capture; below 0.01× the timeline is shorter than a frame and there is
/// nothing left to sample.
const MOTION_SCALE_MIN: f32 = 0.01;
const MOTION_SCALE_MAX: f32 = 100.0;

/// Dev/measurement knob (`EAVE_MOTION_SCALE`, default 1): stretches every
/// timeline in the catalog by this factor, so a screenshot burst can sample
/// a 200 ms tween's geometry at 2 s. Read once for the life of the process;
/// never set in production.
///
/// It scales *timelines*, never [`PULSE_TICK`] — the tick is the sample
/// rate, not a duration anyone is watching. Stretching it too would just
/// make a slowed capture choppy.
pub fn motion_scale() -> f32 {
    static SCALE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *SCALE.get_or_init(|| parse_motion_scale(std::env::var("EAVE_MOTION_SCALE").ok().as_deref()))
}

/// The pure half of [`motion_scale`], split out because the cache above can
/// only ever be observed on one branch: whichever the process saw first.
/// Same split as `reduce_motion_override` in `ui` (Slice M).
///
/// Anything that is not a finite positive number falls back to 1.0 rather
/// than clamping. comet clamps instead, which quietly turns a typo'd `-1`
/// into `0.01` — a 100× *speed-up* from a value that reads like a slow-down.
fn parse_motion_scale(var: Option<&str>) -> f32 {
    var.and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(|s| s.clamp(MOTION_SCALE_MIN, MOTION_SCALE_MAX))
        .unwrap_or(1.0)
}

// ---------------------------------------------------------------------------
// Cubic bezier

/// A CSS `cubic-bezier(x1, y1, x2, y2)` timing function, endpoints fixed at
/// (0,0) and (1,1). Evaluation solves `x(t) = input` by Newton iteration with
/// a bisection fallback — the standard UnitBezier approach. Pure: no gpui, no
/// clock, no state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CubicBezier {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl CubicBezier {
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    /// Polynomial coefficients of the 1-D bezier through 0, `a`, `b`, 1.
    fn coefficients(a: f32, b: f32) -> (f32, f32, f32) {
        let c = 3.0 * a;
        let bb = 3.0 * (b - a) - c;
        let aa = 1.0 - c - bb;
        (aa, bb, c)
    }

    fn sample_x(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.x1, self.x2);
        ((a * t + b) * t + c) * t
    }

    fn sample_y(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.y1, self.y2);
        ((a * t + b) * t + c) * t
    }

    fn sample_x_derivative(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.x1, self.x2);
        (3.0 * a * t + 2.0 * b) * t + c
    }

    /// Curve parameter `t` for a given progress `x` (both 0..1).
    fn solve_t_for_x(&self, x: f32) -> f32 {
        // Newton–Raphson: converges in two or three steps for the curves in
        // the catalog, and the derivative only vanishes at the flat ends.
        let mut t = x;
        for _ in 0..8 {
            let err = self.sample_x(t) - x;
            if err.abs() < 1e-6 {
                return t;
            }
            let d = self.sample_x_derivative(t);
            if d.abs() < 1e-6 {
                break;
            }
            t -= err / d;
        }
        // Bisection fallback — x(t) is monotonic for any valid CSS bezier, so
        // this always converges even where Newton stalls on a flat tangent.
        let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
        for _ in 0..32 {
            let mid = (lo + hi) / 2.0;
            if self.sample_x(mid) < x {
                lo = mid
            } else {
                hi = mid
            }
        }
        (lo + hi) / 2.0
    }

    /// Eased output for input progress `x ∈ [0,1]` (clamped).
    pub fn eval(&self, x: f32) -> f32 {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        // f32 rounding can push `sample_y` a hair past 1.0 — comet observed
        // 1.000000119 near the tail of EASE_OUT_EXPO, and gpui's animation
        // element `debug_assert!`s `delta ∈ [0,1]`, so an unclamped return
        // aborts the process in a debug build. Clamp hard; the sweep test
        // below is the regression.
        self.sample_y(self.solve_t_for_x(x)).clamp(0.0, 1.0)
    }
}

/// CSS `cubic-bezier(0.16, 1, 0.3, 1)` — the signature entrance curve:
/// almost all of the distance in the first third, then a long settle.
#[allow(dead_code)] // inventory §1.3 (fade-in) is its first consumer.
pub const EASE_OUT_EXPO: CubicBezier = CubicBezier::new(0.16, 1.0, 0.3, 1.0);
/// CSS `ease-out` — size transitions.
#[allow(dead_code)] // inventory §1.8 (200 ms pane/rail size transitions) is its caller; unbuilt.
pub const EASE_OUT: CubicBezier = CubicBezier::new(0.0, 0.0, 0.58, 1.0);
/// CSS `ease` — quick fades, menu/dialog pops. The catalog's busiest
/// curve: [`FADE_QUICK`], [`DIALOG_IN`], and both halves of the menu pair
/// below ride it.
pub const EASE: CubicBezier = CubicBezier::new(0.25, 0.1, 0.25, 1.0);
/// CSS `cubic-bezier(0.22, 1, 0.36, 1)` — reorder glides.
// Inventory §1.17 (the FLIP reorder) is BUILT — Slice RS1's rail glide,
// which reads this through [`glide_t`]. §1.9 (drag-reorder) is still
// unbuilt and is the second caller when it lands.
pub const EASE_RESORT: CubicBezier = CubicBezier::new(0.22, 1.0, 0.36, 1.0);
/// CSS `ease-in-out` — scroll glides (the browser smooth-scroll shape:
/// gentle start, cruise, gentle landing).
#[allow(dead_code)] // inventory §1.12 (smooth scroll-to-row) is its caller; unbuilt.
pub const EASE_IN_OUT: CubicBezier = CubicBezier::new(0.42, 0.0, 0.58, 1.0);
/// Tailwind's default transition curve — CSS `cubic-bezier(0.4, 0, 0.2, 1)`.
/// Every `transition-colors` hover wash in a Tailwind UI rides this unless
/// it is overridden, which is why the hover fade below carries it.
#[allow(dead_code)] // reached only through HOVER_FADE, itself unreached.
pub const EASE_TAILWIND: CubicBezier = CubicBezier::new(0.4, 0.0, 0.2, 1.0);

// ---------------------------------------------------------------------------
// Motion specs

/// One timeline: duration + optional delay + curve, as a single value.
///
/// The delay is folded into the timeline rather than handed to gpui (whose
/// `Animation` has no native delay): the animation runs for `delay +
/// duration` and [`progress`](Self::progress) holds 0 until the delay has
/// elapsed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionSpec {
    pub duration_ms: u64,
    pub delay_ms: u64,
    pub curve: CubicBezier,
}

impl MotionSpec {
    pub const fn new(duration_ms: u64, curve: CubicBezier) -> Self {
        Self {
            duration_ms,
            delay_ms: 0,
            curve,
        }
    }

    /// Unreached in production: the only spec in the catalog that wants a
    /// delay is comet's boot splash (inventory §1.7), and eave has no
    /// splash. The delay path through [`progress`](Self::progress) is live
    /// and tested regardless, because the fade store runs every spec
    /// through it.
    #[allow(dead_code)] // §1.7 is its caller; unbuilt.
    pub const fn with_delay(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }

    /// Wall-clock span of the whole timeline (delay + duration), as authored.
    pub fn total(&self) -> Duration {
        Duration::from_millis(self.delay_ms + self.duration_ms)
    }

    /// [`total`](Self::total) as it actually runs, after the measurement
    /// knob. Everything that puts a timeline on a clock uses this one;
    /// `total` is the authored value and is only interesting to a test.
    pub fn scaled_total(&self) -> Duration {
        self.scaled_total_by(motion_scale())
    }

    /// The pure half of [`scaled_total`](Self::scaled_total). `motion_scale`
    /// caches its first read for the life of the process, so a test can only
    /// ever observe one factor through it; the arithmetic lives here where
    /// every factor is reachable.
    fn scaled_total_by(&self, scale: f32) -> Duration {
        self.total().mul_f32(scale)
    }

    /// Eased progress (0..1) for a raw timeline delta (0..1 across the whole
    /// span). Pure — unit-testable with no window and no clock. The scale
    /// knob does not appear here on purpose: it stretches the wall-clock
    /// span, and the delta arriving here is already normalised over it, so
    /// a scaled run traces exactly the same curve more slowly.
    // Unreached in production (the tests drive it). N3's loaders read the
    // curve directly for a repeating wave; `progress` is the oneshot half,
    // and the entrances (§1.3) are what will run on it. eave's one shipped
    // oneshot, the veil, still runs `theme::veil_ease` — migrating it is a
    // visual change to a shipped animation (N1 caveat 8).
    #[allow(dead_code)]
    pub fn progress(&self, raw_delta: f32) -> f32 {
        let total = (self.delay_ms + self.duration_ms) as f32;
        if total <= 0.0 || self.duration_ms == 0 {
            return 1.0;
        }
        let t =
            (raw_delta.clamp(0.0, 1.0) * total - self.delay_ms as f32) / self.duration_ms as f32;
        self.curve.eval(t.clamp(0.0, 1.0))
    }

    // There WAS a `fn animation(&self) -> gpui::Animation` here, N1's one
    // gpui-facing method, and N2 deleted it unused. That is a finding, not
    // tidying: gpui's `Animation` is only reachable through
    // `with_animation`, which keys its start time by the element-id *path*
    // and therefore replays whenever an element misses a frame — and every
    // tween this slice landed is on exactly that path (eave's floating
    // chrome mounts and unmounts on state changes; the ↓-bottom pill is the
    // clearest case). So all five are hand-driven off the fade store below,
    // and the door to gpui's animator was never opened. `progress` is the
    // shape that survived contact: it is pure, and it is what the store
    // calls. If the streaming veil ever migrates onto this catalog (§1.21),
    // put the method back then — with the remount question answered for
    // that site rather than assumed.
}

/// CSS `transition-colors`: 150 ms over [`EASE_TAILWIND`] — the temporal
/// blend an interactive hover wash rides. The catalog stops here on purpose.
/// Loader periods and entrance timings are design decisions belonging to the
/// slice that renders them; only the machinery lands in N1, and the hover
/// fade is the one entry whose value is a CSS default rather than a taste.
#[allow(dead_code)] // inventory §1.13 — every hover wash still snaps.
pub const HOVER_FADE: MotionSpec = MotionSpec::new(150, EASE_TAILWIND);

/// The standard entrance (inventory §1.3): 500 ms over [`EASE_OUT_EXPO`],
/// opacity 0→1 with a 4 px rise. The rise is applied as a relative-position
/// inset, never as a margin — taffy resolves relative insets *after* layout,
/// so the element slides the way a CSS transform would and its siblings
/// never move. See [`ENTRANCE_RISE`].
pub const FADE_IN: MotionSpec = MotionSpec::new(500, EASE_OUT_EXPO);

/// Opacity-only, 150 ms (inventory §1.4) — a row that has just arrived in a
/// rail. No rise: a list of rows sliding independently reads as noise, and
/// the arrival is the only fact being stated.
pub const FADE_QUICK: MotionSpec = MotionSpec::new(150, EASE);

/// 180 ms (inventory §1.6) — the ↓-bottom pill. comet's `dialog-in`
/// approximates a `scale 0.96→1` with fade + a 2 px rise, because gpui divs
/// have no scale transform at the pinned rev; eave inherits both the
/// approximation and the reason.
pub const DIALOG_IN: MotionSpec = MotionSpec::new(180, EASE);

/// How far [`FADE_IN`] rises, in px.
pub const ENTRANCE_RISE: f32 = 4.0;
/// How far [`DIALOG_IN`] rises, in px — half of [`ENTRANCE_RISE`], the
/// standing-in-for-scale value.
pub const DIALOG_RISE: f32 = 2.0;

/// The popover entrance (inventory §1.5): 140 ms over [`EASE`].
///
/// Faster than every other entrance in the catalog, and that is the
/// point — a menu is a *response to a click that already happened*, so
/// any perceptible delay reads as the app being slow rather than as the
/// menu arriving. It also starts from [`MENU_OPACITY_FROM`] rather than
/// from nothing, which is the same argument from the other side: at
/// 140 ms a fade from zero never resolves into a shape, it just flickers.
pub const MENU_IN: MotionSpec = MotionSpec::new(140, EASE);

/// The popover exit (inventory §1.5): 100 ms, **deliberately shorter than
/// [`MENU_IN`]** — comet's stated rule, "exits should get out of the way".
/// A dismissal is an instruction that has already been given; the only
/// job left is to not snap.
///
/// This one is not driven the way every other tween in this module is.
/// The exit's `t` is computed from the popup's own closing instant, held
/// in its state machine (`crate::overlay::Popup`), and NOT from the fade
/// store — because an exit is the one tween that plays while its element
/// is on its way out, so a store entry keyed by "was this read last
/// frame" is exactly the wrong instrument for it. See the scar in
/// [`MotionSpec`]'s trailing note: a replay mid-exit is a full-opacity
/// flash, and the whole reason `Popup` exists is to make that
/// unrepresentable.
pub const MENU_OUT: MotionSpec = MotionSpec::new(100, EASE);

/// Where [`MENU_IN`]'s opacity starts (and where [`MENU_OUT`] would end
/// if it ended anywhere but zero — it does not; see `overlay`).
pub const MENU_OPACITY_FROM: f32 = 0.3;

/// How far a menu rises over [`MENU_IN`], in px — the same 2 px
/// standing-in-for-scale value as [`DIALOG_RISE`], because it is standing
/// in for the same absent transform.
pub const MENU_RISE: f32 = 2.0;

/// Caret blink half-period (inventory §1.20): 500 ms solid, 500 ms hidden —
/// the standard textarea cadence. Not a [`MotionSpec`]: a blink is a step
/// function, so there is no curve to ease and no progress to interpolate,
/// and dressing it as a tween would only invite someone to ease it.
pub const CARET_HALF_PERIOD: Duration = Duration::from_millis(500);

/// Is the caret drawn, `elapsed` after the anchor?
///
/// **Solid through the first half-period**, which is the whole point of the
/// anchor: each keystroke resets it, so a typing burst never blinks — the
/// caret only starts flashing once the typist has stopped for half a second.
/// The scale knob applies, so a capture can slow the cadence down.
pub fn caret_visible(elapsed: Duration) -> bool {
    let half = CARET_HALF_PERIOD.mul_f32(motion_scale());
    if half.is_zero() {
        return true;
    }
    (elapsed.as_nanos() / half.as_nanos()).is_multiple_of(2)
}

// ---------------------------------------------------------------------------
// The pulse clock

/// Repeat-tick interval for everything on the shared clock (~30 fps).
///
/// 30 fps is visually equivalent to the display rate for the chunky, slow
/// shapes that need a repeating drive — cell waves, pulses, spinner phases —
/// at a quarter of the redraws of a 120 Hz panel. It is deliberately NOT the
/// display rate: matching the panel is what cost comet 36% CPU.
const PULSE_TICK: Duration = Duration::from_millis(33);

/// Repeat-tick interval for a **caret**, and the reason the clock has two.
///
/// A blink is a step function with a 500 ms half-period, so the only thing a
/// tick has to do is land close enough to an edge that nobody sees it arrive
/// late. 100 ms bounds that error at a fifth of the half-period — under the
/// threshold where a blink starts reading as ragged — for **a third of the
/// redraws [`PULSE_TICK`] costs**.
///
/// That ratio is the whole reason this constant exists, and the history is
/// worth the paragraph. Before it there was one rate, so a caret leased the
/// same 30 fps drive as a spinner — and eave's answer to "what does a caret
/// cost at rest" was to not blink one. The composer's went solid whenever
/// the draft was empty (gated, with a comment saying the gate was there to
/// keep N4's idle floor reachable); the rail's rename field was solid by a
/// comment that says so in as many words; and the omnibox's was solid by
/// accident, because it read the *composer's* draft, which is empty by
/// definition while somebody is typing somewhere else. Three fields, no
/// blink, and the operator's report was the obvious one: *I can never tell
/// where my cursor is.*
///
/// A caret is still not free, and this does not pretend otherwise — it is
/// the one thing in eave that runs while nothing else is happening. That is
/// the correct trade rather than a regression against N4: the person
/// sitting in front of a focused text field **is** what is happening, and
/// the floor N4 asserts is about a window nobody is typing into. It stays
/// exactly where it was, because a caret leases only while its field owns
/// the keyboard.
const CARET_TICK: Duration = Duration::from_millis(100);

/// How fast a leaseholder needs the clock.
///
/// The tick is the **fastest live lease's**, so a caret on its own runs the
/// window at [`CARET_TICK`] and a caret beside a spinner costs the spinner's
/// rate and nothing on top of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Cadence {
    /// Anything with a shape to move: loaders, pulses, spinner phases.
    Fast,
    /// A blink, and only a blink.
    Caret,
}

impl Cadence {
    fn tick(self) -> Duration {
        match self {
            Cadence::Fast => PULSE_TICK,
            Cadence::Caret => CARET_TICK,
        }
    }
}

/// How long a view stays on the tick list after its last ask. One lease
/// outlives a few missed frames, so a consumer that skips a paint does not
/// stutter; a consumer that unmounts stops renewing and drops off, and with
/// the last lease gone the clock parks.
const PULSE_LEASE: Duration = Duration::from_millis(300);

/// Shared drive for every repeating consumer in the window.
struct PulseClock {
    /// One origin for all of them: two loaders mounted a second apart are
    /// still in phase, because phase is a function of this instant and not
    /// of when the consumer appeared.
    epoch: Instant,
    /// Who still wants ticks, at what cadence, and until when.
    ///
    /// Keyed by **both**, because one view leases twice: eave's shell draws
    /// a caret and a working dot from the same render pass, and a map keyed
    /// by view alone would let whichever asked last decide the rate for
    /// both — which is a caret that silently pins the window at 30 fps, or
    /// a loader that silently drops to 10.
    leases: HashMap<(EntityId, Cadence), Instant>,
    /// Whether the tick task is alive. The whole point: false means no
    /// timer is pending and the window is scheduling nothing.
    running: bool,
}

impl Global for PulseClock {}

/// Is the shared clock ticking? Test-only, and the honest instrument for
/// "does this state of the shell cost anything at rest" — a `false` means no
/// timer is pending anywhere in the window.
#[cfg(test)]
pub fn clock_running(cx: &App) -> bool {
    cx.has_global::<PulseClock>() && cx.global::<PulseClock>().running
}

/// The rate the clock is ticking at, or `None` when no timer is pending.
///
/// Test-only, and the instrument that had to arrive with [`CARET_TICK`].
/// "Is a timer pending" and "is anything animating" were the same question
/// for as long as there was one rate; a blinking caret is a legitimate
/// reason for a timer to exist, so they are two questions now. A test that
/// means the second one asks [`animating`].
#[cfg(test)]
pub fn clock_tick(cx: &App) -> Option<Duration> {
    if !cx.has_global::<PulseClock>() {
        return None;
    }
    let clock = cx.global::<PulseClock>();
    if !clock.running {
        return None;
    }
    clock.leases.keys().map(|(_, cadence)| cadence.tick()).min()
}

/// Is anything **but a caret** driving the clock?
///
/// The successor to `clock_running` for every test whose subject is a
/// moving shape — a skeleton, a loader, a working dot. Those tests read
/// "the window is still drawing this thing", and a caret blinking in a text
/// box the operator is sitting in front of is not that.
#[cfg(test)]
pub fn animating(cx: &App) -> bool {
    clock_tick(cx) == Some(PULSE_TICK)
}

/// [`CARET_TICK`], for the tests that pin what a focused field costs.
#[cfg(test)]
pub const fn caret_tick() -> Duration {
    CARET_TICK
}

/// The clock, created on first use. Deliberately not `default_global`: the
/// epoch has to come from the executor's clock, and a `Default` impl has no
/// way to reach it.
fn clock_mut(cx: &mut App, now: Instant) -> &mut PulseClock {
    if !cx.has_global::<PulseClock>() {
        cx.set_global(PulseClock {
            epoch: now,
            leases: HashMap::new(),
            running: false,
        });
    }
    cx.global_mut::<PulseClock>()
}

/// Renew `view`'s tick lease and hand back the clock's `now`. Call it from
/// the consumer's render; stop calling it and the lease lapses, and with the
/// last lease gone the clock parks.
///
/// `None` under reduced motion — and nothing is touched on that path: no
/// lease, no global, no timer. That is where the preference is honoured, so
/// no consumer can forget to; a `None` means "render your resting state".
///
/// **Slice N2 split this out of [`pulse_delta`], because the first real
/// consumer could not use `pulse_delta` at all.** The clock's phase is a
/// function of the *shared epoch* — that is the property that keeps N
/// loaders in step — but the caret's cadence has to restart from its own
/// anchor on every keystroke, and there is no epoch-relative phase that can
/// express "restart now". What that consumer needs from the clock is the two
/// things underneath the phase: a lease, and a time. So those are what this
/// returns, and `pulse_delta` is a thin epoch-relative wrapper over it.
pub fn pulse_now(view: EntityId, cx: &mut App) -> Option<Instant> {
    lease(view, Cadence::Fast, cx)
}

/// Renew `view`'s **caret** lease and hand back the clock's `now`.
///
/// [`pulse_now`] for a blink, and the only difference is the rate it asks
/// the window to wake at — see [`CARET_TICK`] for why a caret gets its own.
/// Call it from the render of whichever field owns the keyboard; stop
/// calling it — because the field closed, or the keyboard moved to another
/// one — and the lease lapses.
///
/// `None` under reduced motion, on exactly [`pulse_now`]'s terms. A caller
/// that gets `None` draws a **solid** caret, which is the resting state and
/// not a missing one: the preference asked for less motion, not for less
/// insertion point.
pub fn caret_now(view: EntityId, cx: &mut App) -> Option<Instant> {
    lease(view, Cadence::Caret, cx)
}

fn lease(view: EntityId, cadence: Cadence, cx: &mut App) -> Option<Instant> {
    if cx.reduce_motion() {
        return None;
    }
    let now = cx.background_executor().now();
    let start = {
        let clock = clock_mut(cx, now);
        clock.leases.insert((view, cadence), now + PULSE_LEASE);
        let start = !clock.running;
        clock.running = true;
        start
    };
    if start {
        spawn_ticker(cx);
    }
    Some(now)
}

/// Current phase `[0,1)` of a repeating spec, measured from the shared
/// epoch, plus the lease [`pulse_now`] takes. Two consumers mounted a second
/// apart read the same phase; that is the whole reason this exists rather
/// than each loader timing itself.
///
/// Slice N3 is the caller that pays for the phase lock: every working
/// indicator in the window reads its phase from here on the same frame
/// (inventory §1.14, §1.15).
pub fn pulse_delta(spec: &MotionSpec, view: EntityId, cx: &mut App) -> f32 {
    let Some(now) = pulse_now(view, cx) else {
        return 0.0;
    };
    let period = spec.scaled_total().as_secs_f32();
    if period <= 0.0 {
        return 0.0;
    }
    (now.saturating_duration_since(epoch(cx)).as_secs_f32() / period).fract()
}

fn epoch(cx: &App) -> Instant {
    cx.global::<PulseClock>().epoch
}

/// The one timer. Wakes at the fastest live lease's rate, reaps expired
/// leases, notifies whoever is left, and *breaks* — dropping the task —
/// when nobody is.
fn spawn_ticker(cx: &mut App) {
    cx.spawn(async move |cx| loop {
        // Read the rate BEFORE sleeping, not after. A caret-only clock that
        // a loader joins has to speed up on the *next* turn; deciding the
        // interval on the way out instead would play the first tenth of a
        // second of every loader at the caret's rate.
        let tick = cx.update(tick_interval);
        cx.background_executor().timer(tick).await;
        let parked = cx.update(|cx| {
            let now = cx.background_executor().now();
            let clock = cx.global_mut::<PulseClock>();
            clock.leases.retain(|_, until| *until > now);
            if clock.leases.is_empty() {
                clock.running = false;
                return true;
            }
            // One notify per VIEW, not per lease. A shell holding a caret
            // lease and a loader lease is still one redraw.
            let mut views: Vec<EntityId> = clock.leases.keys().map(|(view, _)| *view).collect();
            views.sort_unstable();
            views.dedup();
            for view in views {
                cx.notify(view);
            }
            false
        });
        if parked {
            break;
        }
    })
    .detach();
}

/// The fastest cadence anybody is holding.
///
/// [`PULSE_TICK`] when the clock has not been constructed, which is not a
/// live case — the ticker only exists because a lease was taken — but is
/// the honest answer rather than a panic.
fn tick_interval(cx: &mut App) -> Duration {
    if !cx.has_global::<PulseClock>() {
        return PULSE_TICK;
    }
    cx.global::<PulseClock>()
        .leases
        .keys()
        .map(|(_, cadence)| cadence.tick())
        .min()
        .unwrap_or(PULSE_TICK)
}

// ---------------------------------------------------------------------------
// The motion-active render tail

thread_local! {
    /// Claims made by this frame, waiting for the root's tail to consume
    /// them. Thread-local rather than a gpui global because the render path
    /// reaches it without an `App` in hand, and because there is exactly one
    /// render thread.
    static MOTION_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// "I am mid-flight; give me another frame."
///
/// **Call this during the root render pass** — from a `render` body, or from
/// an event handler that runs before the next one. A claim made later than
/// that (from an element's `prepaint`/`paint`, which run after the root
/// render has already returned) misses this frame's tail, and if nothing
/// else schedules a frame it is never consumed. Hand-driven tweens compute
/// their eased value in `render` anyway — you need the value to build the
/// element — so the constraint costs nothing in practice, but it is a real
/// edge and `a_claim_survives_until_a_tail_consumes_it` pins the behaviour.
///
/// Render-thread only, and one window only. A claim raised on a background
/// thread lands in that thread's flag and is never seen; a second window
/// would consume the flag in whichever root rendered first. eave has one
/// window and renders on one thread, so neither is a live problem — but
/// both are properties of the `thread_local`, not accidents to be relied
/// on if that changes.
// Slice P1 is the first caller, and it took three slices to arrive because
// of what the tail is FOR. N3's loaders are all REPEATING, and a repeating
// consumer is driven by the clock's own `cx.notify`; N2's fades are driven
// by the store, which the root asks about once in its own tail. The tail
// proper exists for hand-driven ONESHOTS — a tween whose whole clock is one
// instant a caller is holding — and the popover exit is exactly that: it
// cannot use the store, because the store's liveness rule is "was this read
// last frame" and an exit plays while its element is leaving.
pub fn mark_active() {
    MOTION_ACTIVE.with(|f| f.set(true));
}

/// The root render's tail: consume this frame's claims and answer whether
/// the window should schedule another.
///
/// Read-and-clear here, rather than comet's clear-at-the-top-of-render, so
/// that a claim raised outside a render pass (an event handler starting a
/// tween) survives to the next tail instead of being wiped by it.
pub fn take_active() -> bool {
    MOTION_ACTIVE.with(|f| f.replace(false))
}

// ---------------------------------------------------------------------------
// The fade store — hover washes (§1.13) and entrances (§1.3, §1.4, §1.6)

/// Linear interpolation, the one arithmetic both fades share.
fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// One keyed tween: `origin → target` over `spec`, started at `started`.
///
/// A hover entry is created by a pointer event and re-anchored **at its
/// current value** whenever the pointer flips direction mid-flight, so a
/// reversal is continuous instead of a jump back to the far end. An entrance
/// entry is created by its first render and never flips.
#[derive(Debug, Clone, Copy)]
struct FadeEntry {
    origin: f32,
    target: f32,
    started: Instant,
    spec: MotionSpec,
    /// Frame counter at the last read — the liveness stamp. An element that
    /// unmounts mid-hover never gets its leave event, so nothing but "it
    /// stopped being read" says the entry is dead.
    seen: u64,
}

impl FadeEntry {
    fn value(&self, now: Instant) -> f32 {
        let span = self.spec.scaled_total();
        let elapsed = now.saturating_duration_since(self.started);
        if span.is_zero() || elapsed >= span {
            return self.target;
        }
        let raw = elapsed.as_secs_f32() / span.as_secs_f32();
        lerp(self.origin, self.target, self.spec.progress(raw))
    }

    fn settled(&self, now: Instant) -> bool {
        self.origin == self.target
            || now.saturating_duration_since(self.started) >= self.spec.scaled_total()
    }
}

/// One key's place in a list, and whatever displacement it still owes
/// (Slice RS1 — inventory §1.17, the FLIP reorder).
///
/// A FLIP glide is not a tween between two *values*; it is a tween whose
/// only job is to be **wrong on purpose** for a moment. The row is already
/// laid out at its new position — layout never waits — and this carries the
/// paint back to where the row used to be, then pays that displacement down
/// to zero. Nothing here is ever asked what the row's position *is*; the
/// answer to that is the layout's, every frame.
///
/// `y` is the key's position in the LIST's own coordinates, not the
/// window's, and that is what the caller has to supply — see [`glide_t`].
/// Without it a scroll would read as every row moving at once.
#[derive(Debug, Clone, Copy)]
struct GlideEntry {
    /// Where the key sat in list coordinates at its last read.
    y: f32,
    /// The displacement owed at `started`, in px, paid down to zero. Sign
    /// is the paint's: a row whose layout moved DOWN owes a NEGATIVE
    /// offset, because it has to be painted back up where it was.
    origin: f32,
    started: Instant,
    spec: MotionSpec,
    /// Frame counter at the last read — [`FadeEntry::seen`]'s job exactly,
    /// and the only signal that a row has left the list.
    seen: u64,
}

impl GlideEntry {
    fn value(&self, now: Instant) -> f32 {
        let span = self.spec.scaled_total();
        let elapsed = now.saturating_duration_since(self.started);
        if span.is_zero() || elapsed >= span {
            return 0.0;
        }
        lerp(
            self.origin,
            0.0,
            self.spec
                .progress(elapsed.as_secs_f32() / span.as_secs_f32()),
        )
    }

    fn settled(&self, now: Instant) -> bool {
        self.origin == 0.0
            || now.saturating_duration_since(self.started) >= self.spec.scaled_total()
    }
}

/// Per-key fade progress. The pure core: every method takes `now`
/// explicitly, so the whole thing is unit-testable with no window and no
/// clock. The thread-local wrappers below are what the shell calls.
#[derive(Default)]
struct Fades {
    entries: HashMap<String, FadeEntry>,
    /// Where each glide key sat last frame, and what it still owes. A
    /// SECOND map rather than a variant of [`FadeEntry`]: a fade is a
    /// value that animates and a glide is a position that does not, and
    /// the pruning rules differ — a settled fade at 0 is indistinguishable
    /// from an absent one and goes, while a settled glide still has to
    /// remember `y` or the next reorder would measure from nothing and
    /// refuse to animate.
    glides: HashMap<String, GlideEntry>,
    /// Renders of the root view, not display frames — see [`begin_frame`].
    frame: u64,
    /// The instant every read in this frame answers at. `None` before the
    /// first render.
    now: Option<Instant>,
    /// The window's reduced-motion preference, read once per frame. Held
    /// here rather than passed to every entrance site because a site that
    /// forgot it would animate on a machine whose owner asked for less
    /// motion, and there is no way to notice that on a machine where the
    /// preference is off.
    reduced: bool,
}

impl Fades {
    /// Open a frame: advance the counter, stamp the instant every read in
    /// this frame will use, and **prune first, before anything reads**.
    ///
    /// Two things are dropped here. An entry unread through the whole
    /// previous frame belongs to an element that is gone — that is the only
    /// signal there is, because an element that unmounts under the pointer
    /// never sends its leave event. And a hover entry that has settled back
    /// to rest is indistinguishable from an absent one, so it goes too and
    /// the map stays the size of what is actually on screen.
    ///
    /// The counter advances **once per render, not once per display frame**,
    /// which is what makes the staleness rule safe: a window that renders
    /// nothing for ten seconds does not age anything, so a settled entrance
    /// cannot be pruned by idleness and replay when the window next wakes.
    fn begin(&mut self, now: Instant, reduced: bool) {
        self.frame += 1;
        self.now = Some(now);
        self.reduced = reduced;
        let frame = self.frame;
        self.entries.retain(|_, e| {
            // One frame of grace, not zero: the approve/deny buttons live
            // inside the virtualized list, whose rows are built during
            // prepaint — after the root render has returned — so their reads
            // land one frame later than everything else's.
            if e.seen + 1 < frame {
                return false;
            }
            !(e.target == 0.0 && e.settled(now))
        });
        // Glides prune on STALENESS only. A settled glide is a row sitting
        // still, and its `y` is the memory the next reorder measures
        // against — dropping it the moment it stops moving would make
        // every second reorder a first fill, which by rule does not
        // animate. Rows that leave the list stop being read and go on the
        // same one-frame grace as a fade.
        self.glides.retain(|_, g| g.seen + 1 >= frame);
    }

    /// The instant this frame answers at.
    fn now(&self) -> Option<Instant> {
        self.now
    }

    /// The pointer entered (`hovered`) or left the element behind `key`.
    /// Reduced motion snaps to the endpoint rather than declining to record
    /// it: the wash still has to appear, it just must not take time.
    fn set(&mut self, key: &str, hovered: bool, reduced: bool, now: Instant) {
        let target = if hovered { 1.0 } else { 0.0 };
        let Some(current) =
            self.entries
                .get(key)
                .map(|e| e.value(now))
                .or(if hovered { Some(0.0) } else { None })
        else {
            // A leave for an element that was never hovered — a stale entry
            // pruned mid-flight will do this. Nothing to fade back from.
            return;
        };
        let seen = self.frame;
        self.entries.insert(
            key.to_string(),
            FadeEntry {
                origin: if reduced { target } else { current },
                target,
                started: now,
                spec: HOVER_FADE,
                seen,
            },
        );
    }

    /// Hover progress (0..1) for `key`; stamps liveness. An absent key is at
    /// rest, which is the correct answer for anything never hovered.
    fn hover(&mut self, key: &str, now: Instant) -> f32 {
        let frame = self.frame;
        let reduced = self.reduced;
        match self.entries.get_mut(key) {
            Some(e) => {
                e.seen = frame;
                if reduced {
                    e.target
                } else {
                    e.value(now)
                }
            }
            None => 0.0,
        }
    }

    /// Entrance progress (0..1) for `key`, starting the tween on the first
    /// read and stamping liveness on every read after it.
    ///
    /// **This is the remount fix.** gpui keys `with_animation`'s start time
    /// by the element-id *path*, so an element that vanishes for one frame —
    /// or whose ancestors' ids shift — silently replays its entrance from
    /// zero. eave's floating chrome stack mounts and unmounts on state
    /// changes, which is exactly that shape. Keying the clock here instead
    /// means the tween is a function of the caller's own key and of whether
    /// the shell drew it last frame, and nothing else.
    fn entrance(&mut self, key: &str, spec: MotionSpec, now: Instant) -> f32 {
        let frame = self.frame;
        let reduced = self.reduced;
        if let Some(e) = self.entries.get_mut(key) {
            e.seen = frame;
            return if reduced { e.target } else { e.value(now) };
        }
        let entry = FadeEntry {
            origin: if reduced { 1.0 } else { 0.0 },
            target: 1.0,
            started: now,
            spec,
            seen: frame,
        };
        let value = entry.value(now);
        self.entries.insert(key.to_string(), entry);
        value
    }

    /// Record where `key` sits this frame and hand back the paint offset
    /// that hides the move — see [`glide_t`] for the contract.
    fn glide(&mut self, key: &str, y: f32, spec: MotionSpec, now: Instant) -> f32 {
        let frame = self.frame;
        let reduced = self.reduced;
        let Some(e) = self.glides.get_mut(key) else {
            // FIRST FILL NEVER ANIMATES. A key with no recorded position
            // has not moved — it has arrived — and a list drawing itself
            // for the first time must not play every row's arrival as a
            // slide from y=0. Record where it is and paint it there.
            self.glides.insert(
                key.to_string(),
                GlideEntry {
                    y,
                    origin: 0.0,
                    started: now,
                    spec,
                    seen: frame,
                },
            );
            return 0.0;
        };
        e.seen = frame;
        // What the row is painting at right now, relative to its own
        // layout box. Read BEFORE `y` is overwritten: a row displaced
        // again mid-flight has to continue from where the eye last saw it,
        // not restart from the full new distance.
        let carried = e.value(now);
        let moved = y - e.y;
        e.y = y;
        if reduced {
            // The preference is honoured here, once, rather than at every
            // call site — the same place [`lease`] honours it. Nothing is
            // recorded as owed, so nothing is mid-flight and the tail asks
            // for no frames.
            e.origin = 0.0;
            return 0.0;
        }
        if moved != 0.0 {
            // The row was painting at `e.y + carried` and is now laid out
            // at `y`. Staying visually still means an offset of
            // `(e.y + carried) - y` — which is `carried - moved`, and
            // which composes: a second move mid-glide adds its own
            // distance to whatever was left of the first.
            e.origin = carried - moved;
            e.started = now;
        }
        e.value(now)
    }

    /// Is anything still mid-flight? The root render's tail asks once and
    /// turns a `true` into a claim on the next frame.
    fn active(&self, now: Instant) -> bool {
        // Reduced motion is answered per FRAME rather than per entry: the
        // preference the window holds right now is the one that counts, so
        // a store full of half-run tweens still asks for nothing the moment
        // it is set. (The tail checks the flag too — this is the store
        // holding its own end, not a duplicate of that check.)
        !self.reduced
            && (self.entries.values().any(|e| !e.settled(now))
                // A glide is driven by the same tail. Without this the
                // first frame of a reorder would paint the offset and
                // then nothing would ask for a second frame, so the rows
                // would sit displaced until something else redrew them —
                // which is worse than not animating at all.
                || self.glides.values().any(|g| !g.settled(now)))
    }
}

thread_local! {
    /// Render-thread only, one window only — the same `thread_local`
    /// argument as [`MOTION_ACTIVE`], and for the same reason: the free
    /// element builders reach this without an `App` in hand.
    static FADES: RefCell<Fades> = RefCell::new(Fades::default());
}

/// Open the frame. **Call this first in the root view's `render`**, once per
/// render, before anything reads a fade — the prune runs here, so a read
/// that beats it would see a dead entry and skip its own entrance.
pub fn begin_frame(now: Instant, reduced: bool) {
    FADES.with(|f| f.borrow_mut().begin(now, reduced));
}

/// Hover progress (0..1) for `key` this frame; 0 before the first
/// [`begin_frame`].
pub fn hover_t(key: &str) -> f32 {
    FADES.with(|f| {
        let mut fades = f.borrow_mut();
        match fades.now() {
            Some(now) => fades.hover(key, now),
            None => 0.0,
        }
    })
}

/// Entrance progress (0..1) for `key` this frame; 1 (i.e. fully arrived,
/// no animation) before the first [`begin_frame`].
pub fn entrance_t(key: &str, spec: MotionSpec) -> f32 {
    FADES.with(|f| {
        let mut fades = f.borrow_mut();
        match fades.now() {
            Some(now) => fades.entrance(key, spec, now),
            None => 1.0,
        }
    })
}

/// **The FLIP glide** (Slice RS1, inventory §1.17): tell the store where
/// `key` sits in its list this frame; get back the vertical paint offset
/// that makes it look like it has not moved yet. 0 before the first
/// [`begin_frame`], 0 under reduced motion, and 0 for a key the store has
/// never seen.
///
/// **`y` must be the key's position in the LIST's own coordinates**, not
/// the window's. A row inside a scroller changes window-y every time
/// anybody scrolls, and a glide fed window-y would read a scroll as every
/// row reordering at once — a whole list smearing under a trackpad. The
/// caller subtracts the scroller's origin and its offset; see
/// [`crate::glide::Glide`], which is the only correct way to call this and
/// exists so no future caller has to rediscover that subtraction.
///
/// It is **paint-only, by construction**: the caller has already been
/// handed its layout bounds by the time it can ask, so the value can only
/// ever be spent on `with_element_offset`. Nothing here can change a row's
/// height, the list's height, or where the next row starts — which is the
/// property the replay assert measures and the reason a glide is safe to
/// add to a rail that holds a scroll anchor.
pub fn glide_t(key: &str, y: f32, spec: MotionSpec) -> f32 {
    FADES.with(|f| {
        let mut fades = f.borrow_mut();
        match fades.now() {
            Some(now) => fades.glide(key, y, spec, now),
            None => 0.0,
        }
    })
}

/// An `.on_hover` listener driving `key`'s fade — pair it with a
/// [`hover_blend`] read of the same key on the same element.
///
/// `window.refresh()` rather than `request_animation_frame`: this runs in
/// event dispatch, and `request_animation_frame` resolves the *current view*
/// and is draw-phase-only. `refresh` marks the window dirty; the root render
/// then re-evaluates the blend and its tail keeps frames coming for as long
/// as the fade is mid-flight.
pub fn hover_listener(key: impl Into<String>) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
    let key = key.into();
    move |hovered, window, cx| {
        let now = cx.background_executor().now();
        let reduced = cx.reduce_motion();
        FADES.with(|f| f.borrow_mut().set(&key, *hovered, reduced, now));
        window.refresh();
    }
}

/// Empty the store. Test-only, and there is exactly one caller: the UI
/// harness, which pins the reduced-motion preference to full motion *after*
/// `bootstrap` has already opened the window and drawn its first frame. Any
/// entrance on that frame recorded itself as already-arrived under the
/// machine's real preference, and would then never play. Production cannot
/// reach this — `reduce_motion_preference()` is read once at bootstrap and
/// the flag never changes again for the life of the process.
#[cfg(test)]
pub fn reset_for_test() {
    FADES.with(|f| *f.borrow_mut() = Fades::default());
}

/// Frame-drive hook: true while any fade is mid-flight. Call it once, from
/// the root render's tail, and turn a `true` into [`mark_active`].
pub fn fades_active() -> bool {
    FADES.with(|f| {
        let fades = f.borrow();
        fades.now().is_some_and(|now| fades.active(now))
    })
}

/// Blend two colours the way a browser transitions them: component
/// interpolation in sRGB with **premultiplied alpha**.
///
/// The premultiplication is the whole point. eave's washes fade in from
/// fully transparent, and a transparent colour still carries *some* RGB —
/// `transparent_black` carries black. Interpolating the components straight
/// drags the midpoint through that black, so a light wash appearing over a
/// dark surface flashes grey on its way in. Premultiplying weights each
/// endpoint's colour by its own alpha, so a zero-alpha endpoint contributes
/// nothing but its transparency and the wash simply brightens.
pub fn mix(from: Hsla, to: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 {
        return from;
    }
    if t >= 1.0 {
        return to;
    }
    let (f, g) = (Rgba::from(from), Rgba::from(to));
    let a = lerp(f.a, g.a, t);
    if a <= f32::EPSILON {
        // Both ends (effectively) transparent: there is no colour to carry,
        // and dividing by `a` here would be a division by zero.
        return Hsla::from(Rgba { a: 0.0, ..g });
    }
    Hsla::from(Rgba {
        r: lerp(f.r * f.a, g.r * g.a, t) / a,
        g: lerp(f.g * f.a, g.g * g.a, t) / a,
        b: lerp(f.b * f.a, g.b * g.a, t) / a,
        a,
    })
}

/// The standard hover blend: `rest → hovered` at `key`'s current progress.
pub fn hover_blend(key: &str, rest: Hsla, hovered: Hsla) -> Hsla {
    mix(rest, hovered, hover_t(key))
}

/// `.hover_fade(key, rest, hovered)` — the replacement for gpui's
/// `.hover(|s| s.bg(..))`, which snaps by construction because the style
/// applies whole on the frame the pointer enters.
///
/// It exists as one call because the blend and the listener are useless
/// apart: a blend with no listener never moves, and a listener with no blend
/// pumps frames for a colour nobody reads. Splitting them across a call site
/// is the one way to get this wrong, so there is no way to write it.
pub trait HoverFade: Sized {
    fn hover_fade(self, key: impl Into<String>, rest: Hsla, hovered: Hsla) -> Self;
}

impl<E> HoverFade for E
where
    E: Styled + StatefulInteractiveElement,
{
    fn hover_fade(self, key: impl Into<String>, rest: Hsla, hovered: Hsla) -> Self {
        let key = key.into();
        self.bg(hover_blend(&key, rest, hovered))
            .on_hover(hover_listener(key))
    }
}

/// `.hover_rim(key, rest, hovered)` — the element's EDGE fades under the
/// pointer instead of its fill.
///
/// Written for SP1's split panes and CV1's cards on 2026-09-10, when the
/// operator asked that *"all four sides highlight … and are moveable/grab
/// able"* and then, of the first build, that it be *"a clean sleek border
/// not going into the screen"*. A pane already lays out a one-point
/// border at every moment — moving only its colour covers no content and
/// reflows nothing, where a filled band inside the edge does both.
///
/// Same bargain and the same one-call shape as [`HoverFade`]: the blend
/// and the listener are useless apart, so there is no way to write one
/// without the other.
pub trait HoverRim: Sized {
    fn hover_rim(self, key: impl Into<String>, rest: Hsla, hovered: Hsla) -> Self;
}

impl<E> HoverRim for E
where
    E: Styled + StatefulInteractiveElement,
{
    fn hover_rim(self, key: impl Into<String>, rest: Hsla, hovered: Hsla) -> Self {
        let key = key.into();
        self.border_color(hover_blend(&key, rest, hovered))
            .on_hover(hover_listener(key))
    }
}

/// `.hover_lift(key, rise, shadow)` — the control RISES under the pointer
/// instead of changing colour.
///
/// Same bargain as [`HoverFade`], and the same one-call shape for the same
/// reason: the offset and the listener are useless apart, so there is no
/// way to write one without the other. It rides `hover_t` like every other
/// hover affordance here, which is what keeps it off gpui's `.hover(|s|
/// ..)` — that snaps whole on the frame the pointer enters, and the shell
/// has an inventory rule against it.
///
/// **The rise is spent on BOTH margins**, up on top and down on the
/// bottom, so the control moves without its neighbours moving: a lift that
/// changed the box's height would walk the rest of the column down the
/// screen every time the pointer crossed it.
///
/// The shadow is passed in rather than chosen here — ink belongs to
/// `theme`, and this module knows about time, not colour.
pub trait HoverLift: Sized {
    fn hover_lift(
        self,
        key: impl Into<String>,
        rise: f32,
        shadow: impl Fn(f32) -> Vec<gpui::BoxShadow>,
    ) -> Self;
}

impl<E> HoverLift for E
where
    E: Styled + StatefulInteractiveElement,
{
    fn hover_lift(
        self,
        key: impl Into<String>,
        rise: f32,
        shadow: impl Fn(f32) -> Vec<gpui::BoxShadow>,
    ) -> Self {
        let key = key.into();
        let t = hover_t(&key);
        self.mt(gpui::px(-rise * t))
            .mb(gpui::px(rise * t))
            .shadow(shadow(t))
            .on_hover(hover_listener(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{div, hsla, Context, IntoElement, Render, TestAppContext, Window};
    use std::cell::RefCell;
    use std::rc::Rc;

    // -----------------------------------------------------------------
    // The curve

    /// The regression comet paid for in a SIGABRT: f32 rounding walks
    /// `sample_y` a hair past 1.0 near the tail of a steep ease-out
    /// (1.000000119 was the observed value), and gpui's animation element
    /// `debug_assert!`s that its delta is in [0,1]. Sweep every catalog
    /// curve, and lean on the values right below 1.0 where Newton lands
    /// closest to the endpoint.
    #[test]
    fn a_curve_never_returns_a_value_outside_the_unit_interval() {
        let curves = [
            EASE_OUT_EXPO,
            EASE_OUT,
            EASE,
            EASE_RESORT,
            EASE_IN_OUT,
            EASE_TAILWIND,
        ];
        for curve in curves {
            for i in 0..=20_000u32 {
                let x = i as f32 / 20_000.0;
                let y = curve.eval(x);
                assert!((0.0..=1.0).contains(&y), "eval({x}) = {y} escaped [0,1]");
            }
            for x in [0.999_999f32, 0.999_999_9, 1.0 - f32::EPSILON, 1.0] {
                let y = curve.eval(x);
                assert!((0.0..=1.0).contains(&y), "eval({x}) = {y} escaped [0,1]");
            }
            // Out of range in both directions: clamped, not extrapolated.
            assert_eq!(curve.eval(-1.0), 0.0);
            assert_eq!(curve.eval(2.0), 1.0);
        }
    }

    /// A timing function is only useful if it is monotonic — a curve that
    /// went backwards would run the animation backwards mid-flight.
    #[test]
    fn a_curve_is_monotonic_and_lands_on_both_endpoints() {
        for curve in [EASE_OUT_EXPO, EASE_OUT, EASE, EASE_RESORT, EASE_IN_OUT] {
            assert_eq!(curve.eval(0.0), 0.0);
            assert_eq!(curve.eval(1.0), 1.0);
            let mut prev = 0.0;
            for i in 0..=1_000u32 {
                let y = curve.eval(i as f32 / 1_000.0);
                assert!(
                    y >= prev - 1e-6,
                    "curve went backwards at {i}: {prev} -> {y}"
                );
                prev = y;
            }
        }
    }

    /// Newton has to actually solve the curve, not approximate it with the
    /// input. The expo-out is the sharpest case in the catalog: a quarter
    /// of the way through the clock it is already most of the way home,
    /// and a broken solver would return something near 0.25.
    #[test]
    fn the_solver_reproduces_the_shape_the_curve_was_authored_as() {
        assert!(
            EASE_OUT_EXPO.eval(0.25) > 0.7,
            "expo-out should be front-loaded, got {}",
            EASE_OUT_EXPO.eval(0.25)
        );
        // ease-in-out is symmetric about the midpoint by construction.
        assert!((EASE_IN_OUT.eval(0.5) - 0.5).abs() < 1e-3);
        // Round-trip: solving x for the curve's own sample must return t.
        for i in 1..100u32 {
            let t = i as f32 / 100.0;
            let x = EASE_OUT_EXPO.sample_x(t);
            assert!(
                (EASE_OUT_EXPO.solve_t_for_x(x) - t).abs() < 1e-3,
                "solve_t_for_x did not invert sample_x at t={t}"
            );
        }
    }

    // -----------------------------------------------------------------
    // Specs

    /// The delay is folded into one timeline rather than handed to gpui,
    /// so the first `delay/total` of the run must be flat at zero and the
    /// curve must still finish exactly on 1.
    #[test]
    fn a_delay_holds_the_timeline_at_zero_and_then_runs_the_whole_curve() {
        let spec = MotionSpec::new(400, EASE).with_delay(100);
        assert_eq!(spec.total(), Duration::from_millis(500));
        // 100/500 of the span is the delay.
        assert_eq!(spec.progress(0.0), 0.0);
        assert_eq!(spec.progress(0.1), 0.0);
        assert_eq!(spec.progress(0.2), 0.0);
        assert!(spec.progress(0.3) > 0.0);
        assert_eq!(spec.progress(1.0), 1.0);
        // A zero-duration spec is already over rather than dividing by zero.
        assert_eq!(MotionSpec::new(0, EASE).progress(0.0), 1.0);
    }

    /// The knob stretches the wall-clock span and leaves the curve alone:
    /// a 10× capture must trace exactly the same shape, more slowly. If
    /// scaling changed `progress`, a slowed capture would be measuring a
    /// different animation than the one that ships.
    #[test]
    fn the_measurement_knob_stretches_the_span_and_not_the_shape() {
        let spec = MotionSpec::new(200, EASE_OUT_EXPO);
        assert_eq!(spec.scaled_total_by(1.0), Duration::from_millis(200));
        assert_eq!(spec.scaled_total_by(10.0), Duration::from_millis(2_000));
        assert_eq!(spec.scaled_total_by(0.5), Duration::from_millis(100));
        for i in 0..=100u32 {
            let d = i as f32 / 100.0;
            assert_eq!(spec.progress(d), spec.progress(d));
        }
    }

    /// Anything that is not a finite positive number falls back to 1.0.
    /// The negative case is the one that matters: clamping `-1` would
    /// yield 0.01 — a 100× speed-up from a value that reads like a
    /// slow-down — so it is rejected outright.
    #[test]
    fn the_measurement_knob_refuses_everything_that_is_not_a_positive_number() {
        assert_eq!(parse_motion_scale(None), 1.0);
        assert_eq!(parse_motion_scale(Some("")), 1.0);
        assert_eq!(parse_motion_scale(Some("slow")), 1.0);
        assert_eq!(parse_motion_scale(Some("0")), 1.0);
        assert_eq!(parse_motion_scale(Some("-1")), 1.0);
        assert_eq!(parse_motion_scale(Some("nan")), 1.0);
        assert_eq!(parse_motion_scale(Some("inf")), 1.0);
        assert_eq!(parse_motion_scale(Some("10")), 10.0);
        assert_eq!(parse_motion_scale(Some(" 2.5 ")), 2.5);
        assert_eq!(parse_motion_scale(Some("1e9")), MOTION_SCALE_MAX);
        assert_eq!(parse_motion_scale(Some("0.0001")), MOTION_SCALE_MIN);
    }

    // -----------------------------------------------------------------
    // The render tail

    /// A claim is consumed by exactly one tail. Read-and-clear, so a claim
    /// raised outside a render pass still reaches the next tail instead of
    /// being wiped by a clear-at-the-top.
    #[test]
    fn a_claim_survives_until_a_tail_consumes_it() {
        assert!(!take_active(), "the tail should start empty");
        mark_active();
        mark_active();
        assert!(take_active(), "the tail should see the claim");
        assert!(!take_active(), "and should have consumed it");
    }

    // -----------------------------------------------------------------
    // The clock
    //
    // The directive's test-only consumer. It asks the clock for a phase on
    // every render, exactly as a loader would, and records what it was
    // handed. No production code has one — Slice N1 ships machinery only.

    struct Ticker {
        renders: Rc<Cell<usize>>,
        phases: Rc<RefCell<Vec<f32>>>,
        subscribed: bool,
        spec: MotionSpec,
    }

    impl Ticker {
        // The tuple names three test handles; a type alias for a helper used
        // twice in one module would be indirection, not clarity.
        #[allow(clippy::type_complexity)]
        fn new(subscribed: bool) -> (Self, Rc<Cell<usize>>, Rc<RefCell<Vec<f32>>>) {
            let renders = Rc::new(Cell::new(0));
            let phases = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    renders: renders.clone(),
                    phases: phases.clone(),
                    subscribed,
                    // A one-second period keeps the phase arithmetic
                    // legible: at 33 ms a tick, one turn is ~30 frames.
                    spec: MotionSpec::new(1_000, EASE),
                },
                renders,
                phases,
            )
        }
    }

    impl Render for Ticker {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.renders.set(self.renders.get() + 1);
            if self.subscribed {
                let id = cx.entity_id();
                let phase = pulse_delta(&self.spec, id, cx);
                self.phases.borrow_mut().push(phase);
            }
            div()
        }
    }

    fn running(cx: &TestAppContext) -> bool {
        cx.update(|cx| clock_running(cx))
    }

    /// The invariant the whole design exists to hold. A window whose views
    /// never ask for a tick must cost nothing while it sits there: no
    /// global, no timer, no redraws. comet's naive version failed exactly
    /// here — a mounted element requested a frame every display frame
    /// whether or not anything was moving.
    #[gpui::test]
    fn nothing_mounted_schedules_no_frames(cx: &mut TestAppContext) {
        let (ticker, renders, _) = Ticker::new(false);
        let _window = cx.add_window(|_, _| ticker);
        cx.run_until_parked();
        let after_boot = renders.get();
        assert!(after_boot >= 1, "the window should have drawn once");

        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();

        assert_eq!(
            renders.get(),
            after_boot,
            "a second of wall clock produced redraws with nothing mounted"
        );
        assert!(
            !cx.update(|cx| cx.has_global::<PulseClock>()),
            "the clock was created even though nobody asked it for anything"
        );
    }

    /// One consumer runs at the clock's rate, not the panel's. At 120 Hz a
    /// display-rate drive would produce ~120 redraws a second; this is the
    /// number that was 36% CPU on comet's laptop.
    #[gpui::test]
    fn one_consumer_ticks_at_the_clocks_rate_not_the_display_rate(cx: &mut TestAppContext) {
        let (ticker, renders, _) = Ticker::new(true);
        let _window = cx.add_window(|_, _| ticker);
        cx.run_until_parked();
        let after_boot = renders.get();
        assert!(running(cx), "one consumer should have started the clock");

        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();

        // Thirty, written out rather than derived from PULSE_TICK: an
        // expectation computed from the constant under test passes for any
        // value of it, including the display rate this exists to avoid.
        let ticks = renders.get() - after_boot;
        assert!(
            (29..=31).contains(&ticks),
            "expected ~30 redraws a second, got {ticks} — the drive is not \
             running at the clock's rate"
        );
    }

    /// The interval itself, to the millisecond: 32 ms buys nothing and 33
    /// buys exactly one redraw. Written as literals for the same reason as
    /// above — the point is the number, not that the code agrees with
    /// itself.
    #[gpui::test]
    fn the_tick_interval_is_the_clocks_and_nothing_elses(cx: &mut TestAppContext) {
        let (ticker, renders, _) = Ticker::new(true);
        let _window = cx.add_window(|_, _| ticker);
        cx.run_until_parked();
        let before = renders.get();

        cx.executor().advance_clock(Duration::from_millis(32));
        cx.run_until_parked();
        assert_eq!(
            renders.get(),
            before,
            "a redraw arrived before the tick did"
        );

        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(renders.get(), before + 1, "the tick produced one redraw");
    }

    /// The lease is what lets the clock park. A consumer that stops asking
    /// keeps its lease for [`PULSE_LEASE`] — long enough to survive a few
    /// missed frames — and then falls off, and with the last lease gone the
    /// tick task ends rather than spinning on an empty list.
    #[gpui::test]
    fn the_clock_parks_when_the_last_lease_expires(cx: &mut TestAppContext) {
        let (ticker, renders, _) = Ticker::new(true);
        let window = cx.add_window(|_, _| ticker);
        cx.run_until_parked();
        assert!(running(cx));

        // The consumer goes quiet: it still renders, it just stops asking.
        window
            .update(cx, |ticker, _, cx| {
                ticker.subscribed = false;
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();

        // Inside the lease the clock is still live — one skipped ask must
        // not stop the drive.
        cx.executor().advance_clock(Duration::from_millis(250));
        cx.run_until_parked();
        assert!(running(cx), "the clock dropped a consumer inside its lease");

        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        assert!(
            !running(cx),
            "the clock never parked after the lease lapsed"
        );

        let quiet = renders.get();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(renders.get(), quiet, "a parked clock is still redrawing");
    }

    /// Item 4: the preference is honoured in the clock itself, so no
    /// consumer can forget to. Static phase, no lease, no global, no timer.
    #[gpui::test]
    fn reduced_motion_returns_a_static_phase_and_schedules_nothing(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let (ticker, renders, phases) = Ticker::new(true);
        let _window = cx.add_window(|_, _| ticker);
        cx.run_until_parked();
        let after_boot = renders.get();

        assert!(
            phases.borrow().iter().all(|p| *p == 0.0),
            "reduced motion handed out a moving phase: {:?}",
            phases.borrow()
        );
        assert!(
            !cx.update(|cx| cx.has_global::<PulseClock>()),
            "reduced motion still created the clock"
        );

        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(
            renders.get(),
            after_boot,
            "reduced motion still scheduled frames"
        );
    }

    /// One epoch behind all of them: two consumers mounted a long way apart
    /// read the same phase on the same frame. This is what keeps two
    /// loaders on screen from wobbling against each other — the reason the
    /// epoch lives on the clock and not on the consumer.
    #[gpui::test]
    fn every_consumer_reads_the_same_epoch_however_late_it_mounts(cx: &mut TestAppContext) {
        let (first, _, first_phases) = Ticker::new(true);
        let _w1 = cx.add_window(|_, _| first);
        cx.run_until_parked();

        // The second consumer appears half a period later.
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        let (second, _, second_phases) = Ticker::new(true);
        let _w2 = cx.add_window(|_, _| second);
        cx.run_until_parked();

        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();

        let a = *first_phases.borrow().last().unwrap();
        let b = *second_phases.borrow().last().unwrap();
        assert!(
            (a - b).abs() < 1e-3,
            "consumers drifted apart: {a} vs {b} — they are not sharing an epoch"
        );
        assert!(b > 0.0, "the late consumer started from its own mount time");
    }

    /// Two consumers, one timer. The count is the point: a per-element
    /// drive would schedule one repeating source each.
    #[gpui::test]
    fn many_consumers_ride_one_timer(cx: &mut TestAppContext) {
        let (first, first_renders, _) = Ticker::new(true);
        let (second, second_renders, _) = Ticker::new(true);
        let _w1 = cx.add_window(|_, _| first);
        let _w2 = cx.add_window(|_, _| second);
        cx.run_until_parked();
        let (a0, b0) = (first_renders.get(), second_renders.get());

        cx.executor().advance_clock(Duration::from_millis(330));
        cx.run_until_parked();

        let (a, b) = (first_renders.get() - a0, second_renders.get() - b0);
        assert_eq!(a, b, "the two consumers did not tick together");
        assert!(
            a.abs_diff(10) <= 1,
            "ten ticks' worth of clock produced {a} redraws"
        );
    }

    // -----------------------------------------------------------------
    // The fade store (Slice N2)

    /// A frame, as the store sees one: open it, and hand back the instant.
    fn frame(fades: &mut Fades, at: Instant, reduced: bool) -> Instant {
        fades.begin(at, reduced);
        at
    }

    const KEY: &str = "some-button";

    // -----------------------------------------------------------------
    // The glide store (Slice RS1 — inventory §1.17)

    const ROW: &str = "rail-y-aaaa1111";
    const GLIDE: MotionSpec = MotionSpec::new(260, EASE_RESORT);

    /// The four rules a FLIP glide is made of, in the order they bite.
    ///
    /// comet states all four (`shell.rs:3820-3870`) and the first is the
    /// one that is easy to lose: a **first fill never animates**. Without
    /// it every row of a list drawing itself for the first time slides in
    /// from wherever the store thought it was, which on an empty store is
    /// y=0 — a rail that unfurls on every launch.
    #[test]
    fn a_first_fill_never_animates_and_a_move_pays_itself_down() {
        let mut f = Fades::default();
        let t0 = Instant::now();
        frame(&mut f, t0, false);

        // 1. First fill: recorded, not animated.
        assert_eq!(f.glide(ROW, 100.0, GLIDE, t0), 0.0);
        assert!(!f.active(t0), "a first fill asked the window for frames");

        // 2. Sitting still costs nothing, however many frames pass.
        let t1 = t0 + Duration::from_millis(16);
        frame(&mut f, t1, false);
        assert_eq!(f.glide(ROW, 100.0, GLIDE, t1), 0.0);

        // 3. A move: the row is laid out 40 px lower and paints 40 px
        //    HIGHER, so that the first frame after the reorder looks
        //    exactly like the last frame before it.
        let t2 = t1 + Duration::from_millis(16);
        frame(&mut f, t2, false);
        assert_eq!(f.glide(ROW, 140.0, GLIDE, t2), -40.0);
        assert!(f.active(t2), "a glide has to keep asking for frames");

        // 4. And it pays down to nothing, monotonically, inside the spec.
        let mid = t2 + Duration::from_millis(130);
        frame(&mut f, mid, false);
        let half = f.glide(ROW, 140.0, GLIDE, mid);
        assert!(
            (-40.0..0.0).contains(&half),
            "half a glide in, the offset was {half} — outside the distance \
             it is supposed to be covering"
        );
        let end = t2 + Duration::from_millis(261);
        frame(&mut f, end, false);
        assert_eq!(f.glide(ROW, 140.0, GLIDE, end), 0.0);
        assert!(!f.active(end), "a settled glide is still asking for frames");
    }

    /// A row displaced again while it is still moving continues from where
    /// the eye last saw it. The mutation this guards is the easy one —
    /// re-anchoring at the full new distance — which reads as the row
    /// jumping backwards before setting off again, and which is exactly
    /// the bug `a_hover_that_reverses_mid_flight_re_anchors_at_its_current_value`
    /// guards for washes.
    #[test]
    fn a_second_move_mid_glide_composes_instead_of_restarting() {
        let mut f = Fades::default();
        let t0 = Instant::now();
        frame(&mut f, t0, false);
        f.glide(ROW, 0.0, GLIDE, t0);

        let t1 = t0 + Duration::from_millis(16);
        frame(&mut f, t1, false);
        assert_eq!(f.glide(ROW, 40.0, GLIDE, t1), -40.0);

        // Halfway down: still owed something, and less than the whole.
        let mid = t1 + Duration::from_millis(130);
        frame(&mut f, mid, false);
        let carried = f.glide(ROW, 40.0, GLIDE, mid);

        // A second push of 40 px, from that position. The offset the next
        // frame paints must be the carried one plus the new distance —
        // not −40, which would be a restart, and not −80, which would be
        // the two distances summed as if nothing had been paid off.
        frame(&mut f, mid, false);
        let composed = f.glide(ROW, 80.0, GLIDE, mid);
        assert!(
            (composed - (carried - 40.0)).abs() < 0.01,
            "carried {carried}, pushed 40 more, and painted {composed}"
        );
        assert!(
            composed > -80.0 && composed < -40.0,
            "the composed offset {composed} is either a restart or a sum"
        );
    }

    /// Reduced motion snaps, and takes nothing with it: no offset, and
    /// nothing left mid-flight for the tail to keep waking the window for.
    #[test]
    fn reduced_motion_moves_the_row_and_never_glides_it() {
        let mut f = Fades::default();
        let t0 = Instant::now();
        frame(&mut f, t0, true);
        assert_eq!(f.glide(ROW, 0.0, GLIDE, t0), 0.0);
        let t1 = t0 + Duration::from_millis(16);
        frame(&mut f, t1, true);
        assert_eq!(
            f.glide(ROW, 400.0, GLIDE, t1),
            0.0,
            "a 400 px reorder under reduced motion has to be a cut"
        );
        assert!(!f.active(t1));
    }

    /// A row that leaves the list is forgotten, and a row that stays is
    /// not. The distinction is the whole reason glides prune on staleness
    /// rather than on settling: a settled glide still owns the `y` the
    /// NEXT reorder measures against, so dropping it would make every
    /// second reorder a first fill — which, by the rule above, does not
    /// animate.
    #[test]
    fn a_settled_row_keeps_its_place_and_a_departed_one_is_forgotten() {
        let mut f = Fades::default();
        let t0 = Instant::now();
        frame(&mut f, t0, false);
        f.glide(ROW, 100.0, GLIDE, t0);

        // Two frames of sitting still — long enough that a settle-prune
        // would have dropped it.
        let mut t = t0;
        for _ in 0..2 {
            t += Duration::from_millis(300);
            frame(&mut f, t, false);
            f.glide(ROW, 100.0, GLIDE, t);
        }
        t += Duration::from_millis(16);
        frame(&mut f, t, false);
        assert_eq!(
            f.glide(ROW, 60.0, GLIDE, t),
            40.0,
            "a row that had been sitting still glided from nowhere — its \
             place was pruned out from under it"
        );

        // Now stop reading it: the row is gone from the list. One frame of
        // grace, then forgotten — and a key the store has never heard of
        // is a first fill, which does not animate.
        for _ in 0..3 {
            t += Duration::from_millis(16);
            frame(&mut f, t, false);
        }
        assert_eq!(f.glide(ROW, 500.0, GLIDE, t), 0.0);
    }

    /// The property that makes a fade feel like a fade rather than a
    /// toggle: flick the pointer off an element halfway through its wash
    /// and the wash falls **from where it was**, not from full.
    ///
    /// The mutation this is really guarding against is the easy one —
    /// re-anchoring at `target` instead of at the current value, which
    /// looks right on a slow pass and flashes on a fast one.
    #[test]
    fn a_hover_that_reverses_mid_flight_re_anchors_at_its_current_value() {
        let mut f = Fades::default();
        let t0 = Instant::now();
        frame(&mut f, t0, false);
        f.set(KEY, true, false, t0);

        // Halfway through the 150 ms wash.
        let mid = t0 + Duration::from_millis(75);
        frame(&mut f, mid, false);
        let rising = f.hover(KEY, mid);
        assert!(
            (0.05..=0.95).contains(&rising),
            "half a HOVER_FADE in, the wash was at {rising} — that is an \
             endpoint, not a mid-flight value"
        );

        // The pointer leaves at exactly that moment.
        f.set(KEY, false, false, mid);
        let falling = f.hover(KEY, mid);
        assert!(
            (falling - rising).abs() < 1e-4,
            "the reversal jumped from {rising} to {falling} instead of \
             continuing from where it was"
        );

        let later = mid + Duration::from_millis(40);
        frame(&mut f, later, false);
        assert!(
            f.hover(KEY, later) < falling,
            "the reversed fade did not fall"
        );

        let rest = mid + Duration::from_millis(200);
        frame(&mut f, rest, false);
        assert_eq!(f.hover(KEY, rest), 0.0, "the wash never reached rest");
    }

    /// An element that unmounts while the pointer is inside it never sends
    /// a leave event — there is nothing to send it. So the only signal the
    /// entry is dead is that the shell stopped reading it, and pruning on
    /// that signal is the whole reason the store carries a frame counter.
    ///
    /// Without this, a rail row hovered and then filtered out of the
    /// snapshot leaves a wash entry behind, and the next row to reuse that
    /// key inherits it.
    #[test]
    fn an_element_that_unmounts_mid_hover_is_pruned_because_its_leave_never_comes() {
        let mut f = Fades::default();
        let t0 = Instant::now();
        frame(&mut f, t0, false);
        f.set(KEY, true, false, t0);
        let settled = t0 + Duration::from_millis(200);
        frame(&mut f, settled, false);
        assert_eq!(f.hover(KEY, settled), 1.0);

        // The shell renders twice more and never draws it again.
        let a = settled + Duration::from_millis(8);
        frame(&mut f, a, false);
        assert!(
            f.entries.contains_key(KEY),
            "one frame of grace is required — the approve/deny buttons are \
             read in prepaint, a frame behind everything else"
        );
        let b = a + Duration::from_millis(8);
        frame(&mut f, b, false);
        assert!(
            f.entries.is_empty(),
            "a hover entry outlived the element it belongs to"
        );
    }

    /// The two settled states are not the same state. A hover that has
    /// fallen back to rest is indistinguishable from an element that was
    /// never hovered, so it goes. A settled *entrance* is the record that
    /// says "this already arrived" — drop that and the element replays its
    /// entrance on the very next frame, forever.
    #[test]
    fn a_settled_entrance_is_kept_and_a_hover_settled_at_rest_is_dropped() {
        let mut f = Fades::default();
        let t0 = Instant::now();
        frame(&mut f, t0, false);
        f.set(KEY, true, false, t0);
        f.entrance("in:panel", FADE_IN, t0);

        // Both run to completion, then the pointer leaves and both are read
        // again on every frame after that.
        let done = t0 + Duration::from_millis(600);
        frame(&mut f, done, false);
        f.hover(KEY, done);
        f.entrance("in:panel", FADE_IN, done);
        f.set(KEY, false, false, done);

        let rested = done + Duration::from_millis(200);
        frame(&mut f, rested, false);
        f.hover(KEY, rested);
        f.entrance("in:panel", FADE_IN, rested);
        let after = rested + Duration::from_millis(8);
        frame(&mut f, after, false);

        assert!(
            !f.entries.contains_key(KEY),
            "a hover back at rest is steady state and should not be stored"
        );
        assert!(
            f.entries.contains_key("in:panel"),
            "the arrived record was dropped — the panel will replay its \
             entrance on the next frame"
        );
        assert_eq!(f.entrance("in:panel", FADE_IN, after), 1.0);
    }

    /// The remount rule, stated in both directions.
    ///
    /// This is what eave has instead of gpui's `with_animation`, whose
    /// start time is keyed by the element-id *path* and replays whenever an
    /// element misses a frame or its ancestors' ids shift. Here the replay
    /// is a function of one thing only: did the shell draw this key last
    /// render.
    #[test]
    fn an_entrance_replays_when_the_shell_stops_drawing_it_and_not_before() {
        let mut f = Fades::default();
        let t0 = Instant::now();
        frame(&mut f, t0, false);
        assert_eq!(
            f.entrance("in:pill", DIALOG_IN, t0),
            0.0,
            "it should start at zero"
        );

        let arrived = t0 + Duration::from_millis(300);
        frame(&mut f, arrived, false);
        assert_eq!(f.entrance("in:pill", DIALOG_IN, arrived), 1.0);

        // Still drawn, many frames later: no replay.
        let mut at = arrived;
        for _ in 0..10 {
            at += Duration::from_millis(8);
            frame(&mut f, at, false);
            assert_eq!(
                f.entrance("in:pill", DIALOG_IN, at),
                1.0,
                "a mounted entrance replayed"
            );
        }

        // The pill's condition goes false; the shell renders twice without
        // it, then it comes back.
        at += Duration::from_millis(8);
        frame(&mut f, at, false);
        at += Duration::from_millis(8);
        frame(&mut f, at, false);
        assert_eq!(
            f.entrance("in:pill", DIALOG_IN, at),
            0.0,
            "the pill came back and did not re-enter"
        );
    }

    /// The staleness rule counts **renders, not display frames** — and that
    /// distinction is load-bearing, not pedantry.
    ///
    /// eave's window schedules nothing while nothing is mid-flight (N1), so
    /// a settled shell can go minutes between renders. If the counter
    /// advanced with wall time, every one of those gaps would age the
    /// arrived-records out and the whole floating chrome stack would
    /// re-enter the moment anything woke the window.
    #[test]
    fn an_idle_window_never_ages_an_entrance_into_a_replay() {
        let mut f = Fades::default();
        let t0 = Instant::now();
        frame(&mut f, t0, false);
        f.entrance("in:panel", FADE_IN, t0);
        let arrived = t0 + Duration::from_secs(1);
        frame(&mut f, arrived, false);
        assert_eq!(f.entrance("in:panel", FADE_IN, arrived), 1.0);

        // An hour of nothing happening: one render at the end of it.
        let woken = arrived + Duration::from_secs(3600);
        frame(&mut f, woken, false);
        assert_eq!(
            f.entrance("in:panel", FADE_IN, woken),
            1.0,
            "an idle hour replayed an entrance that had already arrived"
        );
    }

    /// Reduced motion does not mean "no wash" — it means "no time". Both
    /// kinds land on their endpoint immediately, and nothing is left
    /// mid-flight for the tail to request a frame for.
    #[test]
    fn reduced_motion_puts_both_kinds_straight_on_their_endpoint() {
        let mut f = Fades::default();
        let t0 = Instant::now();
        frame(&mut f, t0, true);
        f.set(KEY, true, true, t0);
        assert_eq!(f.hover(KEY, t0), 1.0, "the hover wash did not appear");
        assert_eq!(
            f.entrance("in:panel", FADE_IN, t0),
            1.0,
            "the panel did not arrive"
        );
        assert!(!f.active(t0), "reduced motion left something mid-flight");
    }

    /// The frame-drive hook. `active` is what the root render's tail turns
    /// into a claim, so a store that answered `true` at rest would pin the
    /// window at the display rate forever — the exact failure N1 exists to
    /// avoid — and one that answered `false` mid-flight would freeze every
    /// fade on its first frame.
    #[test]
    fn the_store_asks_for_frames_only_while_something_is_mid_flight() {
        let mut f = Fades::default();
        let t0 = Instant::now();
        frame(&mut f, t0, false);
        assert!(!f.active(t0), "an empty store asked for a frame");

        f.set(KEY, true, false, t0);
        assert!(f.active(t0), "a wash that just started asked for nothing");

        let mid = t0 + Duration::from_millis(75);
        frame(&mut f, mid, false);
        f.hover(KEY, mid);
        assert!(f.active(mid), "the wash stopped asking mid-flight");

        // 150 ms is HOVER_FADE's whole span; a literal, not a value derived
        // from the constant under test.
        let done = t0 + Duration::from_millis(150);
        frame(&mut f, done, false);
        f.hover(KEY, done);
        assert!(!f.active(done), "a landed wash kept asking for frames");
    }

    /// Premultiplied alpha, which is the difference between a wash that
    /// brightens in and one that smears through grey on the way.
    ///
    /// `wash_rest` is transparent BLACK — a real colour at zero alpha. Blend
    /// the components straight and the midpoint carries half that black;
    /// premultiply and a zero-alpha end contributes nothing but its
    /// transparency, so the hue is the target's the whole way and only the
    /// alpha ramps. The naive value is computed here rather than described,
    /// so the assertion fails if someone "simplifies" `mix`.
    #[test]
    fn a_wash_fading_in_from_transparent_never_passes_through_grey() {
        let from: Hsla = gpui::transparent_black();
        let to: Hsla = hsla(220.0 / 360.0, 0.10, 0.90, 0.05);
        let target = Rgba::from(to);

        for t in [0.05f32, 0.25, 0.5, 0.75, 0.95] {
            let got = Rgba::from(mix(from, to, t));
            assert!(
                (got.r - target.r).abs() < 1e-3
                    && (got.g - target.g).abs() < 1e-3
                    && (got.b - target.b).abs() < 1e-3,
                "at t={t} the wash's colour was ({}, {}, {}), not the \
                 target's ({}, {}, {})",
                got.r,
                got.g,
                got.b,
                target.r,
                target.g,
                target.b,
            );
            assert!(
                (got.a - target.a * t).abs() < 1e-3,
                "at t={t} the alpha was {} rather than the ramp",
                got.a
            );
            // What straight component interpolation would have produced,
            // and by exactly how much it would have been too dark.
            let naive = target.r * t;
            assert!(
                got.r - naive - target.r * (1.0 - t) < 1e-3,
                "at t={t} the blend ({}) is not the premultiplied value; \
                 straight interpolation would give {naive}",
                got.r
            );
            assert!(
                got.r > naive,
                "at t={t} the blend ({}) is no brighter than the naive one \
                 ({naive}) — mix is not premultiplying",
                got.r
            );
        }

        // Both ends transparent: no colour to carry, and no divide by zero.
        let ghost = mix(gpui::transparent_black(), gpui::transparent_black(), 0.5);
        assert_eq!(Rgba::from(ghost).a, 0.0);
    }

    /// Two opaque pigments still blend the ordinary way — the `attn-kill`
    /// button's case, where both ends are a danger wash.
    #[test]
    fn a_blend_between_two_opaque_colours_lands_on_both_ends_and_midway() {
        let from: Hsla = hsla(0.0, 0.0, 0.2, 1.0);
        let to: Hsla = hsla(0.0, 0.0, 0.8, 1.0);
        assert_eq!(mix(from, to, 0.0), from);
        assert_eq!(mix(from, to, 1.0), to);
        assert_eq!(mix(from, to, -5.0), from, "t below 0 was extrapolated");
        assert_eq!(mix(from, to, 5.0), to, "t above 1 was extrapolated");
        let mid = Rgba::from(mix(from, to, 0.5));
        let (a, b) = (Rgba::from(from), Rgba::from(to));
        assert!((mid.r - (a.r + b.r) / 2.0).abs() < 1e-3);
    }

    /// §1.20's whole point: the caret is **solid** through its first half-
    /// period. A keystroke resets the anchor, so a typing burst holds the
    /// caret lit and it only starts flashing once the typist stops.
    ///
    /// The expectations are literal milliseconds on purpose — deriving them
    /// from `CARET_HALF_PERIOD` would make the test pass for any cadence,
    /// including one so fast it strobes (N1's trap, `progress.md` §Traps).
    #[test]
    fn the_caret_is_solid_through_its_first_half_period() {
        assert!(caret_visible(Duration::ZERO), "the caret was dark on reset");
        assert!(caret_visible(Duration::from_millis(1)));
        assert!(caret_visible(Duration::from_millis(499)));
        assert!(!caret_visible(Duration::from_millis(500)));
        assert!(!caret_visible(Duration::from_millis(999)));
        assert!(caret_visible(Duration::from_millis(1000)));
        assert!(!caret_visible(Duration::from_millis(1500)));

        // A burst of keystrokes 100 ms apart never lets `elapsed` reach the
        // half-period, so the caret is lit at every one of them.
        for keystroke in 0..12 {
            assert!(
                caret_visible(Duration::from_millis(100 * keystroke % 500)),
                "a typing burst blinked"
            );
        }
    }

    /// Every wash in the shell, swept end to end: **no hover fade ever gets
    /// darker than where it started.**
    ///
    /// This is the assertion standing in for a screenshot. "The wash flashes
    /// on its way in" is a perceptual claim, but it has an exact mechanical
    /// signature — a midpoint whose composited luminance dips below the
    /// resting one — and that is checkable. It is also the one thing
    /// premultiplied alpha exists to prevent, and the one thing that would
    /// be invisible on this machine, where the OS reduced-motion preference
    /// is on and every fade is a snap unless it is deliberately overridden.
    ///
    /// The pairs are the real ones from `theme`, in the same order as the
    /// thirteen controls in `ui`, so a token retuned to a darker hover step
    /// fails here rather than in someone's eye a week later.
    #[test]
    fn no_hover_wash_in_the_shell_ever_darkens_on_its_way_in() {
        use crate::theme;
        // (name, resting fill, hovered fill) — an unfilled control rests at
        // `wash_rest`, which is transparent black; a filled one rests at its
        // own pigment.
        let pairs: [(&str, Hsla, Hsla); 6] = [
            (
                "unfilled controls (9 of them)",
                theme::wash_rest(),
                theme::overlay(),
            ),
            (
                "the selected rail row",
                theme::overlay_strong(),
                theme::overlay_strong(),
            ),
            ("an expanded file row", theme::overlay(), theme::overlay()),
            (
                "attn-kill",
                theme::danger_wash_strong(),
                theme::danger_wash_hover(),
            ),
            ("send, armed", theme::accent(), theme::accent_hover()),
            (
                "send, unarmed",
                theme::overlay_strong(),
                theme::overlay_strongest(),
            ),
        ];
        // Composite over eave's dark ground and report luminance. The
        // ground is the literal `#1A1A1A` of `canvas()`'s opaque branch
        // rather than `canvas()` itself, and deliberately: in glass mode
        // every surface token is a translucent WASH whose own rgb is 0, and
        // compositing over an rgb-zero ground makes the assertion vacuous —
        // nothing can be darker than black. A fixed dark ground is what the
        // washes actually land on in either render mode, and it is the only
        // ground on which "this got darker" means anything.
        const GROUND: f32 = 0x1a as f32 / 255.0;
        let over = |c: Hsla| {
            let c = Rgba::from(c);
            let mix1 = |f: f32| f * c.a + GROUND * (1.0 - c.a);
            0.2126 * mix1(c.r) + 0.7152 * mix1(c.g) + 0.0722 * mix1(c.b)
        };
        for (what, rest, hovered) in pairs.into_iter().chain([(
            "the ↓-bottom pill",
            theme::chrome_glass(),
            theme::chrome_glass_hover(),
        )]) {
            let floor = over(rest).min(over(hovered));
            for i in 0..=100 {
                let t = i as f32 / 100.0;
                let lum = over(mix(rest, hovered, t));
                assert!(
                    lum >= floor - 1e-4,
                    "{what}: at t={t} the wash sits at luminance {lum}, below \
                     both of its own endpoints ({floor}) — it darkens on the \
                     way through, which is exactly what premultiplied alpha \
                     is here to prevent"
                );
            }
        }
    }

    /// The lease `pulse_delta` is built on, checked directly: it is what a
    /// consumer with its own anchor (the caret) reaches for, and the reason
    /// it exists is that such a consumer cannot use an epoch-relative phase.
    #[gpui::test]
    fn the_pulse_lease_hands_back_a_time_and_declines_under_reduced_motion(
        cx: &mut TestAppContext,
    ) {
        let (view, _, _) = Ticker::new(false);
        let window = cx.add_window(|_, _| view);
        let id = window.root(cx).unwrap().entity_id();

        cx.update(|cx| {
            cx.set_reduce_motion(true);
            assert!(
                pulse_now(id, cx).is_none(),
                "reduced motion still took a lease"
            );
            assert!(
                !cx.has_global::<PulseClock>(),
                "reduced motion constructed the clock"
            );

            cx.set_reduce_motion(false);
            let a = pulse_now(id, cx).expect("a lease and a time");
            let b = pulse_now(id, cx).expect("a lease and a time");
            assert_eq!(a, b, "two reads in one frame saw different times");
            assert!(cx.has_global::<PulseClock>());
        });
    }
}
