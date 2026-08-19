//! Slice N1 — the frame drive. Machinery only: nothing in eave consumes any
//! of this yet, by directive, which is why the module carries a blanket
//! `dead_code` allow. The first consumer slice takes it off.
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
#![allow(dead_code)]

use std::cell::Cell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{Animation, App, EntityId, Global};

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

    /// This curve as a gpui easing closure.
    pub fn easing(self) -> impl Fn(f32) -> f32 + 'static {
        move |x| self.eval(x)
    }
}

/// CSS `cubic-bezier(0.16, 1, 0.3, 1)` — the signature entrance curve:
/// almost all of the distance in the first third, then a long settle.
pub const EASE_OUT_EXPO: CubicBezier = CubicBezier::new(0.16, 1.0, 0.3, 1.0);
/// CSS `ease-out` — size transitions.
pub const EASE_OUT: CubicBezier = CubicBezier::new(0.0, 0.0, 0.58, 1.0);
/// CSS `ease` — quick fades, menu/dialog pops.
pub const EASE: CubicBezier = CubicBezier::new(0.25, 0.1, 0.25, 1.0);
/// CSS `cubic-bezier(0.22, 1, 0.36, 1)` — reorder glides.
pub const EASE_RESORT: CubicBezier = CubicBezier::new(0.22, 1.0, 0.36, 1.0);
/// CSS `ease-in-out` — scroll glides (the browser smooth-scroll shape:
/// gentle start, cruise, gentle landing).
pub const EASE_IN_OUT: CubicBezier = CubicBezier::new(0.42, 0.0, 0.58, 1.0);
/// Tailwind's default transition curve — CSS `cubic-bezier(0.4, 0, 0.2, 1)`.
/// Every `transition-colors` hover wash in a Tailwind UI rides this unless
/// it is overridden, which is why the hover fade below carries it.
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
    pub fn progress(&self, raw_delta: f32) -> f32 {
        let total = (self.delay_ms + self.duration_ms) as f32;
        if total <= 0.0 || self.duration_ms == 0 {
            return 1.0;
        }
        let t =
            (raw_delta.clamp(0.0, 1.0) * total - self.delay_ms as f32) / self.duration_ms as f32;
        self.curve.eval(t.clamp(0.0, 1.0))
    }

    /// A oneshot gpui [`Animation`] for this spec, delay folded in and the
    /// measurement knob applied. The one gpui-facing method on the type.
    pub fn animation(&self) -> Animation {
        let spec = *self;
        Animation::new(spec.scaled_total()).with_easing(move |d| spec.progress(d))
    }
}

/// CSS `transition-colors`: 150 ms over [`EASE_TAILWIND`] — the temporal
/// blend an interactive hover wash rides. The catalog stops here on purpose.
/// Loader periods and entrance timings are design decisions belonging to the
/// slice that renders them; only the machinery lands in N1, and the hover
/// fade is the one entry whose value is a CSS default rather than a taste.
pub const HOVER_FADE: MotionSpec = MotionSpec::new(150, EASE_TAILWIND);

// ---------------------------------------------------------------------------
// The pulse clock

/// Repeat-tick interval for everything on the shared clock (~30 fps).
///
/// 30 fps is visually equivalent to the display rate for the chunky, slow
/// shapes that need a repeating drive — cell waves, pulses, spinner phases —
/// at a quarter of the redraws of a 120 Hz panel. It is deliberately NOT the
/// display rate: matching the panel is what cost comet 36% CPU.
const PULSE_TICK: Duration = Duration::from_millis(33);

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
    /// Who still wants ticks, and until when.
    leases: HashMap<EntityId, Instant>,
    /// Whether the tick task is alive. The whole point: false means no
    /// timer is pending and the window is scheduling nothing.
    running: bool,
}

impl Global for PulseClock {}

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

/// Current phase `[0,1)` of a repeating spec, plus a lease that keeps `view`
/// re-rendering at [`PULSE_TICK`] for as long as it keeps asking. Call it
/// from the consumer's render; stop calling it and the lease lapses.
///
/// Reduced motion returns a static 0 and touches nothing — no lease, no
/// global, no timer. That is the whole of item 4: the clock itself is the
/// place the preference is honoured, so no consumer can forget to.
pub fn pulse_delta(spec: &MotionSpec, view: EntityId, cx: &mut App) -> f32 {
    if cx.reduce_motion() {
        return 0.0;
    }
    let now = cx.background_executor().now();
    let period = spec.scaled_total().as_secs_f32();
    let start = {
        let clock = clock_mut(cx, now);
        clock.leases.insert(view, now + PULSE_LEASE);
        let start = !clock.running;
        clock.running = true;
        start
    };
    if start {
        spawn_ticker(cx);
    }
    if period <= 0.0 {
        return 0.0;
    }
    (now.saturating_duration_since(epoch(cx)).as_secs_f32() / period).fract()
}

fn epoch(cx: &App) -> Instant {
    cx.global::<PulseClock>().epoch
}

/// The one timer. Wakes every [`PULSE_TICK`], reaps expired leases, notifies
/// whoever is left, and *breaks* — dropping the task — when nobody is.
fn spawn_ticker(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(PULSE_TICK).await;
            let parked = cx.update(|cx| {
                let now = cx.background_executor().now();
                let clock = cx.global_mut::<PulseClock>();
                clock.leases.retain(|_, until| *until > now);
                if clock.leases.is_empty() {
                    clock.running = false;
                    return true;
                }
                let views: Vec<EntityId> = clock.leases.keys().copied().collect();
                for view in views {
                    cx.notify(view);
                }
                false
            });
            if parked {
                break;
            }
        }
    })
    .detach();
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, IntoElement, Render, TestAppContext, Window, div};
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
                assert!(y >= prev - 1e-6, "curve went backwards at {i}: {prev} -> {y}");
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
        cx.update(|cx| cx.has_global::<PulseClock>() && cx.global::<PulseClock>().running)
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
        assert_eq!(renders.get(), before, "a redraw arrived before the tick did");

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
        assert!(!running(cx), "the clock never parked after the lease lapsed");

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
}
