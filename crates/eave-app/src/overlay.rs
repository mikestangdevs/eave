//! Slice P1 — the overlay layer. Popovers, menus, context menus: the
//! floating surface eave has never had.
//!
//! Before this slice the app had **zero** deferred or anchored elements
//! (polish-inventory §1.5, verified). Every affordance was a permanently
//! visible button or a word, which is the structural reason the shell
//! reads as flat: there was nowhere for anything to be *above* anything
//! else, so nothing could be tucked away and nothing could arrive.
//!
//! This module is machinery. **It has no product consumers** — the only
//! thing that opens a menu is a demo trigger in `ui` that renders when
//! `EAVE_OVERLAY_DEMO` is set, plus the tests. What it settles is the
//! shape a consumer will inherit, and there are four decisions in it:
//!
//!   - **The exit needs a state machine, not just a tween.** An entrance
//!     can be driven by a store keyed on "did the shell draw this last
//!     frame", because it plays while the element is on screen. An exit
//!     plays while the element is *leaving*, so that instrument answers
//!     "gone" at exactly the moment the animation needs it most. Hence
//!     [`Popup`]: the state stays mounted through the exit and a timer
//!     reaps it.
//!   - **The exit's clock is the popup's own closing instant.** Never an
//!     element-id-keyed animation clock. gpui keys `with_animation` by
//!     the element-id *path*, so a remount replays it from zero — and a
//!     replay mid-exit is a flash back to full opacity on a thing the
//!     user has already dismissed. comet paid for that one and wrote it
//!     down (§1.5); [`Popup::motion_at`] is why it cannot happen here.
//!   - **Occlusion is the layer's job, not the row's.** A menu that
//!     does not kill the hitboxes behind it is a menu you can click
//!     through, and every click through a menu lands on something the
//!     user could not see. The layer occludes for its whole life,
//!     including the exit — a click during those 100 ms must do nothing
//!     at all, rather than hit whatever the menu was covering.
//!   - **Dead hit-testing is not the same as gone.** During the exit the
//!     surface is still painted and still occluding, but no row will
//!     take a click and no key will move the highlight.
//!
//! [`Popup`] itself is pure: no gpui, no globals, no thread-local, every
//! method takes the instant it should answer at. That is deliberate — it
//! is the half with the interesting behaviour, so it is the half that has
//! to be testable without a window.

use std::time::Instant;

use gpui::{
    anchored, deferred, div, prelude::*, px, Anchor, AnyElement, App, Context, MouseButton,
    MouseDownEvent, Pixels, Point, SharedString, Window,
};

use crate::motion::{self, HoverFade as _, MENU_IN, MENU_OPACITY_FROM, MENU_OUT, MENU_RISE};
use crate::theme;

// ---------------------------------------------------------------------------
// The demo gate

thread_local! {
    /// `EAVE_OVERLAY_DEMO`, read once per thread.
    ///
    /// Thread-local rather than a process-wide `OnceLock` for the same
    /// reason `motion`'s stores are: `#[gpui::test]` runs each test on its
    /// own thread inside one process, so a global would let one test's
    /// override decide another test's render. Production has one render
    /// thread and reads the env var, so the two agree there.
    static DEMO: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Is the demo trigger mounted?
///
/// P1 has **no product consumer** — this is the whole of the answer to
/// "prove it works". Off unless `EAVE_OVERLAY_DEMO` is set to something
/// other than `0`, so the shipped shell is byte-for-byte what it was; on
/// in the handful of tests that ask for it.
pub fn demo_enabled() -> bool {
    DEMO.with(|d| match d.get() {
        Some(v) => v,
        None => {
            let v = std::env::var("EAVE_OVERLAY_DEMO")
                .ok()
                .is_some_and(|v| v.trim() != "0" && !v.trim().is_empty());
            d.set(Some(v));
            v
        }
    })
}

/// `EAVE_OVERLAY_DEMO=open` — mount the trigger *and* open the menu on
/// the first frame.
///
/// It exists for one reason: the only way into the menu is a click, and a
/// screenshot cannot click. Without this the surface could be asserted
/// but never *looked at*, and every colour and metric in it would ship on
/// arithmetic alone. Not reachable from the tests, which click.
pub fn demo_opens_at_boot() -> bool {
    std::env::var("EAVE_OVERLAY_DEMO").is_ok_and(|v| v.trim() == "open")
}

#[cfg(test)]
pub fn enable_demo_for_test() {
    DEMO.with(|d| d.set(Some(true)));
}

// ---------------------------------------------------------------------------
// The state machine

/// One popup's life: `Closed → Open → Closing → Closed`.
///
/// `T` is whatever the opening site needs to remember for as long as the
/// popup is up — an anchor point, a row id, a session id. It is carried
/// through the exit as well as the open phase, because the surface still
/// has to paint itself while it leaves.
pub struct Popup<T> {
    phase: Phase<T>,
}

enum Phase<T> {
    Closed,
    Open(Live<T>),
    Closing(Exit<T>),
}

struct Live<T> {
    data: T,
    /// When the entrance started. The popup's own clock — see the module
    /// note on why this is not an element-id-keyed one.
    opened: Instant,
    /// Which rows can be chosen. Held here rather than left to the render
    /// because **keyboard navigation has to skip disabled rows**, and a
    /// highlight that can land on a dead row is a highlight that lies.
    /// It is the one piece of the row model the state machine genuinely
    /// needs; everything else about a row lives in the element.
    enabled: Vec<bool>,
    highlighted: Option<usize>,
}

struct Exit<T> {
    data: T,
    /// The closing instant. **This** is what the exit's `t` is computed
    /// from.
    started: Instant,
    /// Opacity at that instant, so an exit that interrupts an entrance
    /// starts from where the entrance actually got to rather than
    /// snapping to full first.
    from: f32,
}

impl<T> Default for Popup<T> {
    fn default() -> Self {
        Self {
            phase: Phase::Closed,
        }
    }
}

impl<T> Popup<T> {
    /// Open it, replacing whatever was there — including a popup part-way
    /// through its own exit. Re-opening mid-exit restarts the entrance
    /// from [`MENU_OPACITY_FROM`] rather than from the dying value, which
    /// is the honest picture: the user asked for it again, so it arrives
    /// again.
    pub fn open(&mut self, data: T, enabled: Vec<bool>, now: Instant) {
        self.phase = Phase::Open(Live {
            data,
            opened: now,
            enabled,
            highlighted: None,
        });
    }

    /// Start the exit. Returns whether the popup is still mounted and so
    /// still needs reaping — `false` under reduced motion, where there is
    /// no exit to play and the popup closes outright.
    ///
    /// A `begin_close` on something already closing is a no-op, not a
    /// restart: a second escape keypress must not extend the exit.
    pub fn begin_close(&mut self, now: Instant, reduced: bool) -> bool {
        let Phase::Open(live) = &self.phase else {
            return matches!(self.phase, Phase::Closing(_));
        };
        if reduced {
            self.phase = Phase::Closed;
            return false;
        }
        let from = entrance_opacity(now.saturating_duration_since(live.opened));
        let Phase::Open(live) = std::mem::replace(&mut self.phase, Phase::Closed) else {
            unreachable!("checked immediately above")
        };
        self.phase = Phase::Closing(Exit {
            data: live.data,
            started: now,
            from,
        });
        true
    }

    /// Drop a closing popup, **once its exit has actually elapsed**.
    ///
    /// The elapsed check is what makes this safe to call from more than
    /// one place. The reap timer is the real reaper, but a render pass
    /// that notices a finished exit may call it too, and a stray early
    /// call — a timer from a *previous* close that outlived a re-open —
    /// must not take down a popup that is legitimately up.
    pub fn finish_close(&mut self, now: Instant) {
        if let Phase::Closing(exit) = &self.phase {
            if now.saturating_duration_since(exit.started) >= MENU_OUT.scaled_total() {
                self.phase = Phase::Closed;
            }
        }
    }

    /// Is it accepting input? Open only — a closing popup paints and
    /// occludes but takes nothing.
    pub fn is_open(&self) -> bool {
        matches!(self.phase, Phase::Open(_))
    }

    /// Is it on screen at all (open **or** leaving)?
    ///
    /// Unreached in production, and named rather than blanket-allowed
    /// (N1's rule): the demo reads [`data`](Self::data) instead, because
    /// it needs the anchor on the same line it needs the answer. A real
    /// consumer that has its anchor elsewhere — a menu hung off a fixed
    /// affordance — wants this one. The tests use it.
    #[allow(dead_code)]
    pub fn is_mounted(&self) -> bool {
        !matches!(self.phase, Phase::Closed)
    }

    pub fn data(&self) -> Option<&T> {
        match &self.phase {
            Phase::Closed => None,
            Phase::Open(l) => Some(&l.data),
            Phase::Closing(e) => Some(&e.data),
        }
    }

    pub fn highlighted(&self) -> Option<usize> {
        match &self.phase {
            Phase::Open(l) => l.highlighted,
            _ => None,
        }
    }

    /// This frame's `(opacity, dy)`, or `None` when nothing is mounted.
    /// `dy` is in px and negative means "still above where it lands" —
    /// applied as a *relative* inset by [`menu_layer`], never a margin,
    /// so nothing around it moves.
    ///
    /// Under reduced motion both phases answer with their endpoint. A
    /// closing popup answering `(0.0, …)` rather than being dropped here
    /// is on purpose: this method takes `&self`, and the state machine
    /// has exactly one place that changes phase per transition.
    pub fn motion_at(&self, now: Instant, reduced: bool) -> Option<(f32, f32)> {
        match &self.phase {
            Phase::Closed => None,
            Phase::Open(l) => {
                if reduced {
                    return Some((1.0, 0.0));
                }
                let t = eased(&MENU_IN, now.saturating_duration_since(l.opened));
                Some((lerp(MENU_OPACITY_FROM, 1.0, t), -MENU_RISE * (1.0 - t)))
            }
            Phase::Closing(e) => {
                if reduced {
                    return Some((0.0, 0.0));
                }
                let t = eased(&MENU_OUT, now.saturating_duration_since(e.started));
                Some((lerp(e.from, 0.0, t), -MENU_RISE * t))
            }
        }
    }

    /// Is a tween still running? The render turns a `true` into one
    /// [`motion::mark_active`] claim, and that is the only thing keeping
    /// frames coming for a menu — nothing here rides the fade store or
    /// the pulse clock.
    pub fn in_flight(&self, now: Instant, reduced: bool) -> bool {
        if reduced {
            return false;
        }
        match &self.phase {
            Phase::Closed => false,
            Phase::Open(l) => now.saturating_duration_since(l.opened) < MENU_IN.scaled_total(),
            Phase::Closing(e) => now.saturating_duration_since(e.started) < MENU_OUT.scaled_total(),
        }
    }

    /// Move the highlight down (`+1`) or up (`-1`), wrapping, **skipping
    /// disabled rows**, and doing nothing at all when every row is
    /// disabled. The scan is bounded by the row count for exactly that
    /// last case: an all-disabled menu must not spin.
    pub fn highlight_next(&mut self) {
        self.step(1);
    }

    pub fn highlight_prev(&mut self) {
        self.step(-1);
    }

    fn step(&mut self, delta: isize) {
        let Phase::Open(live) = &mut self.phase else {
            return;
        };
        let n = live.enabled.len() as isize;
        if n == 0 {
            return;
        }
        // Nothing highlighted yet: down lands on the first row, up on the
        // last. Starting one step outside the range in the direction of
        // travel gets both from the same arithmetic.
        let start = match live.highlighted {
            Some(ix) => ix as isize,
            None => {
                if delta > 0 {
                    -1
                } else {
                    n
                }
            }
        };
        for k in 1..=n {
            let ix = (start + delta * k).rem_euclid(n) as usize;
            if live.enabled[ix] {
                live.highlighted = Some(ix);
                return;
            }
        }
    }
}

fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// Eased progress of `spec`, `elapsed` after it started.
fn eased(spec: &motion::MotionSpec, elapsed: std::time::Duration) -> f32 {
    let span = spec.scaled_total();
    if span.is_zero() || elapsed >= span {
        return 1.0;
    }
    spec.progress(elapsed.as_secs_f32() / span.as_secs_f32())
}

/// The entrance's opacity `elapsed` in — the value an interrupting exit
/// starts from.
fn entrance_opacity(elapsed: std::time::Duration) -> f32 {
    lerp(MENU_OPACITY_FROM, 1.0, eased(&MENU_IN, elapsed))
}

// ---------------------------------------------------------------------------
// The reaper

/// Drop a closing popup after its exit has played.
///
/// A timer rather than the render tail, because the tail is not
/// guaranteed to run: it only schedules frames while something claims it,
/// and a window that stops rendering for any other reason would strand
/// the popup mounted-but-invisible, still occluding everything behind it.
/// The timer is the one thing that fires regardless.
///
/// `MENU_OUT.scaled_total()`, so `EAVE_MOTION_SCALE` stretches the mounted
/// life along with the tween it is there to outlast.
pub fn reap<V: 'static>(
    cx: &mut Context<V>,
    finish: impl FnOnce(&mut V, &mut Context<V>) + 'static,
) {
    let span = MENU_OUT.scaled_total();
    cx.spawn(async move |view, cx| {
        cx.background_executor().timer(span).await;
        let _ = view.update(cx, |v, cx| {
            finish(v, cx);
            cx.notify();
        });
    })
    .detach();
}

// ---------------------------------------------------------------------------
// The row primitive

/// One menu row's content. The element is built by [`menu_row`]; this is
/// only what the row *says*.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuItem {
    pub label: SharedString,
    /// Trailing state — a keybinding, a count, a checkmark. Optional
    /// because most rows are just a label, and a row that reserves the
    /// slot when it has nothing to put in it pads every menu for nothing.
    pub trailing: Option<SharedString>,
    pub enabled: bool,
}

impl MenuItem {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            trailing: None,
            enabled: true,
        }
    }

    pub fn trailing(mut self, trailing: impl Into<SharedString>) -> Self {
        self.trailing = Some(trailing.into());
        self
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

/// One row. `highlighted` is the keyboard cursor; `live` is whether the
/// popup is still taking input (false through the exit).
///
/// **The keyboard highlight paints as the hover wash, not as a second
/// colour**, which is why it arrives by moving the *rest* end of the fade
/// rather than by adding a style on top. A row the keyboard has landed on
/// and a row the pointer is over are the same fact — "this is the one
/// that will fire" — and drawing them differently would invent a
/// distinction the user does not have.
///
/// That does mean the highlight *steps* rather than tweening: moving the
/// rest end changes both ends of the blend at once, so the wash arrives
/// whole on the frame the arrow key lands. Same shape as the send
/// button's arming (Slice M), same reason, and correct here — a keyboard
/// highlight that eased in would lag behind a held arrow key.
///
/// **If you add a leading icon here, colour the `svg` itself.** The
/// `.text_color()` below cascades to the label because a div pushes its
/// text style onto the window's text stack for its children
/// (`div.rs`, `with_text_style`). An `svg` does NOT read that stack:
/// `Svg::paint` guards on `style.text.color` from its *own* computed
/// style, which is `Style::default()` refined only by the element's own
/// base style — so an uncoloured glyph silently paints nothing, at the
/// right size, in the right slot, and every bounds-based assertion still
/// passes. Slice P3 shipped six sites that way and 254 tests agreed with
/// it; see `docs/reports/stage-p3-icons.md`.
pub fn menu_row(
    ix: usize,
    item: &MenuItem,
    highlighted: bool,
    live: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let rest = if highlighted {
        theme::overlay()
    } else {
        theme::wash_rest()
    };
    let body = div()
        .id(("menu-row", ix))
        .debug_selector(move || format!("menu-row-{ix}"))
        .h(px(theme::MENU_ROW_H))
        .mx(px(theme::MENU_ROW_INSET))
        .px(px(theme::MENU_ROW_PAD_X))
        .rounded(px(theme::RADIUS_MENU_ROW))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::MENU_ROW_GAP))
        .text_size(px(theme::LABEL_SIZE))
        .line_height(px(theme::LABEL_LINE))
        .text_color(if item.enabled {
            theme::text()
        } else {
            // By token, not by opacity — see the note in `launcher::tile`.
            theme::text_disabled()
        })
        .child(div().flex_1().min_w(px(0.)).child(item.label.clone()))
        .children(item.trailing.clone().map(|t| {
            // **Slice AQ: no `text_size` here.** The trailing state used to
            // be `DETAIL_SIZE` against the row's `LABEL_SIZE` — 12.0 against
            // 12.5, half a pixel apart. Half a pixel is not a rank; it is
            // two type sizes in one row pretending to be a hierarchy, and
            // the only thing it actually achieved was putting the keycap on
            // a different baseline grid from the word it belongs to.
            //
            // The rank is real and it is already carried, by INK: `text` for
            // the label, `text_tertiary` for the state. One channel, spent
            // once. (Same lesson as the decision buttons: size, weight and
            // ink are three channels and a rank is a budget across them.)
            div()
                .flex_none()
                .text_color(theme::text_tertiary())
                .child(t)
        }));

    if item.enabled {
        body.hover_fade(format!("menu-row:{ix}"), rest, theme::overlay())
            .cursor_pointer()
            .when(live, |d| d.on_click(on_click))
            .into_any_element()
    } else {
        // Disabled (polish-inventory §2.6): dimmed whole, no handler, and
        // **still occluding**. The occlusion is redundant inside
        // [`menu_layer`], whose surface already blocks everything behind
        // it — it is here because a row is a primitive, and a primitive
        // that leaks clicks the moment it is used outside its usual
        // wrapper is a trap rather than a component.
        body.bg(rest).occlude().into_any_element()
    }
}

// ---------------------------------------------------------------------------
// The surface

/// The anchored, occluding overlay surface — mount it at the **root**,
/// and nowhere else.
///
/// Root-mounted for two reasons. `deferred` keeps its child's layout in
/// the tree where it is written but paints it after every ancestor, so a
/// menu written into a row would still paint on top — but its layout
/// would be a child of that row, and eave's transcript is a virtualized
/// list that **caches row heights**. A surface that participates in one
/// is a surface that changes it. Mounted at the root, over a full-window
/// absolute catcher, it can influence nothing's size by construction.
///
/// The catcher is the other half of the dismissal: a full-window
/// occluding div *behind* the menu. A click on the menu never reaches it
/// (the surface occludes, so the catcher's hitbox is not hovered); a
/// click anywhere else hits the catcher, dismisses, and — because the
/// catcher occludes too — does **not** also land on whatever it was over.
/// That last part is the whole point. Dismissing a menu and activating
/// the button underneath it in one click is the classic overlay bug.
pub fn menu_layer<T>(
    popup: &Popup<T>,
    anchor: Point<Pixels>,
    now: Instant,
    reduced: bool,
    rows: Vec<AnyElement>,
    on_dismiss: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Option<AnyElement> {
    let (opacity, dy) = popup.motion_at(now, reduced)?;

    // The claim, raised from inside the root render pass — which is where
    // this function is called from, and the only place it may be called
    // from (N1's `mark_active` contract: a claim raised during prepaint or
    // paint misses this frame's tail and is never consumed).
    if popup.in_flight(now, reduced) {
        motion::mark_active();
    }

    let live = popup.is_open();
    Some(
        deferred(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .occlude()
                .when(live, |d| d.on_mouse_down(MouseButton::Left, on_dismiss))
                .child(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .position(anchor)
                        .snap_to_window_with_margin(px(theme::MENU_SCREEN_MARGIN))
                        .child(
                            div()
                                .id("overlay-menu")
                                .debug_selector(|| "overlay-menu".into())
                                // Occludes for its whole mounted life,
                                // exit included: a click during the 100 ms
                                // exit must do nothing, not hit what the
                                // menu was covering.
                                .occlude()
                                .opacity(opacity)
                                // Relative inset, never a margin — taffy
                                // resolves relative insets after layout,
                                // so the rise moves the surface the way a
                                // CSS transform would (motion.rs,
                                // `ENTRANCE_RISE`).
                                .relative()
                                .top(px(dy))
                                .min_w(px(theme::MENU_MIN_W))
                                .max_w(px(theme::MENU_MAX_W))
                                .py(px(theme::MENU_PAD_Y))
                                .rounded(px(theme::RADIUS_MENU))
                                .border_1()
                                .border_color(theme::edge_modal())
                                .shadow(theme::shadow_modal())
                                .bg(theme::menu_glass())
                                .flex()
                                .flex_col()
                                .children(rows),
                        ),
                ),
        )
        .into_any_element(),
    )
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t0() -> Instant {
        Instant::now()
    }

    fn menu() -> (Popup<u8>, Instant) {
        let now = t0();
        let mut p = Popup::default();
        p.open(7, vec![true, true, false, true], now);
        (p, now)
    }

    /// **Slice AQ: a menu row declares its type size ONCE.**
    ///
    /// The trailing state — a keybinding, a count, a checkmark — used to
    /// carry `DETAIL_SIZE` against the row's own `LABEL_SIZE`. 12.0 against
    /// 12.5. Half a pixel is not a rank; it is two type sizes in one row
    /// pretending to be a hierarchy, and what it actually bought was a
    /// keycap sitting on a different baseline from the word it belongs to.
    ///
    /// The rank between a label and its trailing state is real and it is
    /// already carried by INK (`text` against `text_tertiary`), which is one
    /// channel spent once. Size, weight and ink are three separate channels
    /// and a rank is a budget across them, not a licence to spend all three.
    ///
    /// Structural, because gpui resolves the text style into its own stack
    /// and a `debug_bounds` box on the Noop text system reports the same
    /// height either way — the defect is invisible to every instrument this
    /// project has except this one and a photograph.
    #[test]
    fn a_menu_row_declares_its_type_size_once() {
        let src = include_str!("overlay.rs");
        let at = src.find("pub fn menu_row(").expect("menu_row");
        let end = at + src[at..].find("\n// ---").expect("end of the row section");
        let body = &src[at..end];
        assert_eq!(
            body.matches(".text_size(").count(),
            1,
            "a menu row set its type size more than once — the second one is a \
             rank being paid for twice"
        );
        assert!(
            body.contains("theme::text_tertiary()"),
            "the trailing state stopped being distinguished by ink, which is \
             now the ONLY channel carrying that rank"
        );
    }

    /// The durations, as literals.
    ///
    /// Written out rather than compared to each other (`assert!(OUT < IN)`)
    /// because the design decision is not "the exit is shorter" — that is
    /// the *consequence*. The decision is 140 and 100, and a comparison
    /// would still pass if someone halved both and quietly made every menu
    /// in the app twitch. Same rule N1 learned on the tick literal.
    #[test]
    fn the_exit_is_shorter_than_the_entrance() {
        assert_eq!(MENU_IN.duration_ms, 140);
        assert_eq!(MENU_OUT.duration_ms, 100);
        assert_eq!(MENU_OPACITY_FROM, 0.3);
    }

    /// The scar, stated as a property.
    ///
    /// comet's failure was an exit driven by an element-id-keyed clock: the
    /// element remounts mid-exit, the clock restarts, and the menu the user
    /// just dismissed flashes back to full opacity. The fix is not a
    /// smarter clock, it is that the exit's value is a pure function of two
    /// instants the popup owns — so there is no state anywhere that a
    /// remount could reset. This asserts exactly that: the opacity never
    /// rises, it reaches zero, and reading it a hundred times does not
    /// change it.
    #[test]
    fn an_exit_only_ever_falls_and_cannot_be_replayed() {
        let (mut p, now) = menu();
        // Let the entrance finish, so a replay would be maximally visible.
        let closed_at = now + MENU_IN.scaled_total();
        assert!(p.begin_close(closed_at, false));

        let mut last = f32::INFINITY;
        for ms in [0u64, 10, 25, 50, 75, 99] {
            let at = closed_at + Duration::from_millis(ms);
            let (o, _) = p
                .motion_at(at, false)
                .expect("still mounted through the exit");
            assert!(o <= last, "opacity rose during the exit: {o} after {last}");
            // Sampling is not a side effect.
            for _ in 0..100 {
                assert_eq!(p.motion_at(at, false).unwrap().0, o);
            }
            last = o;
        }
        let (o, _) = p
            .motion_at(closed_at + MENU_OUT.scaled_total(), false)
            .expect("mounted until it is reaped, not until it is invisible");
        assert_eq!(o, 0.0, "the exit must land on zero, not near it");
    }

    /// An exit that interrupts an entrance starts from where the entrance
    /// actually got to. Snapping to full first — the obvious implementation
    /// — is a visible pop on exactly the fast double-click this is for.
    #[test]
    fn an_exit_starts_from_where_the_entrance_got_to() {
        let (mut p, now) = menu();
        let mid = now + MENU_IN.scaled_total() / 2;
        let (entering, _) = p.motion_at(mid, false).unwrap();
        assert!(
            entering > MENU_OPACITY_FROM && entering < 1.0,
            "the fixture must actually be mid-entrance, got {entering}"
        );
        p.begin_close(mid, false);
        let (leaving, _) = p.motion_at(mid, false).unwrap();
        assert_eq!(leaving, entering, "the exit snapped instead of continuing");
    }

    /// Re-opening mid-exit replays the entrance rather than resuming the
    /// dying value: the user asked for it again, so it arrives again.
    #[test]
    fn reopening_replays_the_entrance() {
        let (mut p, now) = menu();
        let closed_at = now + Duration::from_millis(200);
        p.begin_close(closed_at, false);
        let again = closed_at + Duration::from_millis(50);
        p.open(9, vec![true], again);
        assert_eq!(p.motion_at(again, false).unwrap().0, MENU_OPACITY_FROM);
        assert_eq!(p.data(), Some(&9));
    }

    /// The reap is guarded by elapsed time, not by being called. A timer
    /// left over from an earlier close must not take down a popup that has
    /// since been re-opened and closed again.
    #[test]
    fn a_reap_that_arrives_early_is_ignored() {
        let (mut p, now) = menu();
        p.begin_close(now, false);
        p.finish_close(now + MENU_OUT.scaled_total() / 2);
        assert!(p.is_mounted(), "an early reap dropped a live exit");
        p.finish_close(now + MENU_OUT.scaled_total());
        assert!(!p.is_mounted());
    }

    /// Mounted is not the same as interactive. Through the exit the surface
    /// still paints and still occludes, but takes nothing.
    #[test]
    fn a_closing_popup_paints_but_takes_no_input() {
        let (mut p, now) = menu();
        p.highlight_next();
        assert_eq!(p.highlighted(), Some(0));
        p.begin_close(now, false);
        assert!(p.is_mounted() && !p.is_open());
        assert_eq!(p.highlighted(), None, "a closing menu still had a cursor");
        p.highlight_next();
        assert_eq!(p.highlighted(), None, "a closing menu moved its cursor");
        assert!(
            p.data().is_some(),
            "the exit lost the data it must paint with"
        );
    }

    /// Reduced motion has no exit to play, so there is nothing to keep
    /// mounted and nothing to reap — and `begin_close` says so, which is
    /// what stops the caller spawning a timer for an animation that will
    /// never run.
    #[test]
    fn reduced_motion_closes_outright() {
        let (mut p, now) = menu();
        assert!(!p.begin_close(now, true));
        assert!(!p.is_mounted());
        assert!(p.motion_at(now, true).is_none());
    }

    /// Under reduced motion an open popup is simply *there*, at its
    /// endpoint, and claims no frames.
    #[test]
    fn reduced_motion_never_reports_anything_in_flight() {
        let (p, now) = menu();
        assert_eq!(p.motion_at(now, true), Some((1.0, 0.0)));
        assert!(!p.in_flight(now, true));
        assert!(p.in_flight(now, false), "the fixture is not mid-entrance");
    }

    /// The invariant N1 exists for, at this module's own boundary: once
    /// both tweens have run their span, nothing here asks for another
    /// frame.
    #[test]
    fn nothing_is_in_flight_once_the_span_has_run() {
        let (mut p, now) = menu();
        assert!(p.in_flight(now, false));
        let settled = now + MENU_IN.scaled_total();
        assert!(
            !p.in_flight(settled, false),
            "the entrance outlived its span"
        );
        p.begin_close(settled, false);
        assert!(p.in_flight(settled, false));
        assert!(!p.in_flight(settled + MENU_OUT.scaled_total(), false));
    }

    /// Keyboard navigation skips disabled rows and wraps. The fixture's
    /// third row is disabled, so `down` from the second must land on the
    /// fourth and never on the third.
    #[test]
    fn the_highlight_skips_disabled_rows_and_wraps() {
        let (mut p, _) = menu();
        // First press lands on the first row rather than the second.
        p.highlight_next();
        assert_eq!(p.highlighted(), Some(0));
        p.highlight_next();
        assert_eq!(p.highlighted(), Some(1));
        p.highlight_next();
        assert_eq!(
            p.highlighted(),
            Some(3),
            "the highlight landed on a disabled row"
        );
        p.highlight_next();
        assert_eq!(p.highlighted(), Some(0), "the highlight did not wrap");
        p.highlight_prev();
        assert_eq!(p.highlighted(), Some(3), "up did not wrap the other way");
        p.highlight_prev();
        assert_eq!(p.highlighted(), Some(1), "up did not skip the disabled row");
    }

    /// Up from nothing lands on the last row, not the first.
    #[test]
    fn the_first_press_enters_from_the_direction_of_travel() {
        let mut p: Popup<u8> = Popup::default();
        p.open(0, vec![true, true, true], t0());
        p.highlight_prev();
        assert_eq!(p.highlighted(), Some(2));
    }

    /// A menu whose every row is disabled highlights nothing — and, more
    /// to the point, *terminates*. The scan is bounded by the row count
    /// precisely so this cannot spin.
    #[test]
    fn an_all_disabled_menu_highlights_nothing_and_terminates() {
        let mut p: Popup<u8> = Popup::default();
        p.open(0, vec![false, false, false], t0());
        p.highlight_next();
        p.highlight_prev();
        assert_eq!(p.highlighted(), None);

        // And an empty one, which has no rows to scan at all.
        let mut empty: Popup<u8> = Popup::default();
        empty.open(0, Vec::new(), t0());
        empty.highlight_next();
        assert_eq!(empty.highlighted(), None);
    }

    /// A second dismissal must not extend the exit — otherwise holding
    /// escape keeps a dismissed menu on screen.
    #[test]
    fn a_second_dismissal_does_not_restart_the_exit() {
        let (mut p, now) = menu();
        p.begin_close(now, false);
        let later = now + Duration::from_millis(60);
        assert!(
            p.begin_close(later, false),
            "still mounted, still needs reaping"
        );
        // Reaping on the ORIGINAL schedule, not a restarted one.
        p.finish_close(now + MENU_OUT.scaled_total());
        assert!(!p.is_mounted(), "the second escape extended the exit");
    }
}
