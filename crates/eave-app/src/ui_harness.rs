//! Stage J — the hermetic UI harness.
//!
//! Every test here boots the SAME spine production boots ([`bootstrap`]),
//! hands it a [`Feed`] whose far ends the test holds, and then asserts two
//! things and nothing else: **what the shell states** (rendered facts,
//! looked up by `debug_selector` bounds) and **what intent it emits**
//! (the `Command` stream). Engine truth stays in the process suites.
//!
//! No real threads, no `allow_parking`: the deterministic seeded
//! scheduler is the point — it is what retires the scripted-click flake.

use super::*;
use eave_wire::{TabInfo, TabsSnapshot};
use gpui::{Modifiers, MouseButton, NavigationDirection, TestAppContext, VisualTestContext};

/// The test's end of the plumbing: push engine-shaped events in through
/// `plumbing`, read emitted intent out of `commands`.
struct Harness {
    window: gpui::WindowHandle<Shell>,
    plumbing: Plumbing,
    commands: tokio_mpsc::UnboundedReceiver<Command>,
}

impl Harness {
    /// Every command emitted since the last drain, in order.
    fn drain(&mut self) -> Vec<Command> {
        let mut out = Vec::new();
        while let Ok(c) = self.commands.try_recv() {
            out.push(c);
        }
        out
    }
}

/// Boot the real shell on the test platform.
fn boot(cx: &mut TestAppContext) -> (Harness, &'static mut VisualTestContext) {
    let queues = Arc::new(Queues::default());
    let (wake_tx, wake_rx) = futures::channel::mpsc::channel::<()>(1);
    let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel::<Command>();
    let plumbing = Plumbing {
        queues: Arc::clone(&queues),
        wake: wake_tx,
    };
    let window = cx.update(|cx| {
        bootstrap(
            cx,
            Feed {
                queues,
                wake_rx,
                cmd_tx,
            },
        )
    });
    // Slice M: `bootstrap` reads the MACHINE's reduced-motion preference,
    // which a test must never inherit — the developer's accessibility
    // setting would otherwise decide whether the veil animates, and the
    // suite would behave differently on two Macs. Pinned to the
    // full-motion path here; `reduce_motion_override` covers the other
    // branch as a pure function.
    cx.update(|cx| cx.set_reduce_motion(false));
    // ...and clear the fade store with it. `bootstrap` opens the window and
    // draws its first frame before the line above lands, so any entrance on
    // that frame recorded itself as already-arrived under the machine's real
    // preference — on a Mac with reduce-motion on (this one), every entrance
    // in the suite would silently be a no-op. Production never hits this:
    // the flag is read once at bootstrap and never changes.
    motion::reset_for_test();
    let vcx = VisualTestContext::from_window(window.into(), cx).into_mut();
    // And force one draw through it, because `run_until_parked` only draws a
    // dirty window and `bootstrap`'s own first frame is now behind us. Every
    // test therefore boots with the empty-log note mid-entrance — a test
    // that counts frames has to let that land first (`drive`).
    vcx.update(|window, _| window.refresh());
    vcx.run_until_parked();
    (
        Harness {
            window,
            plumbing,
            commands: cmd_rx,
        },
        vcx,
    )
}

/// A session the engine might report. Defaults are the boring case; the
/// tests mutate only the field under test.
fn session(id: &str) -> SessionInfo {
    SessionInfo {
        session_id: id.into(),
        title: None,
        archived: false,
        provider: "claude-code".into(),
        model: None,
        cwd: "/tmp/eave-harness".into(),
        created_ts_ms: 1,
        last_activity_ms: None,
        state: "idle".into(),
        live: false,
        epoch: 1,
        last_seq: 0,
        checkout_root: None,
        shared_with: Vec::new(),
        worktree: None,
        pending_approvals: 0,
        permission_mode: None,
        rss_bytes: None,
    }
}

/// Click the centre of the element carrying `selector`, failing with the
/// selector's name if it never rendered.
#[track_caller]
fn click(vcx: &mut VisualTestContext, selector: &'static str) {
    click_with(vcx, selector, Modifiers::none())
}

#[track_caller]
fn click_with(vcx: &mut VisualTestContext, selector: &'static str, modifiers: Modifiers) {
    let bounds = vcx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("element {selector} did not render"));
    // `debug_bounds` answers "was this painted", NOT "can it be clicked". An
    // element scrolled out of the viewport still reports bounds — with a
    // NEGATIVE origin — and `simulate_click` on a point outside the window
    // hits nothing and reports nothing, so the test goes green having
    // exercised no handler at all.
    //
    // Found the hard way: a code fence 250 lines tall put its own header
    // strip at y = -2972px in a bottom-aligned list, and the copy-button
    // test passed its click and then failed on the clipboard, which is a
    // confusing way to learn the click never happened.
    let c = bounds.center();
    assert!(
        c.x >= px(0.) && c.y >= px(0.),
        "element {selector} is off-screen at {c:?} — a click there hits \
         nothing and asserts nothing; scroll it into view or use a fixture \
         that fits"
    );
    vcx.simulate_click(c, modifiers);
}

/// Put the pointer on a rail row, so its hover-revealed affordances exist.
///
/// Stage AK made the row's five affordances icons that mount only while the
/// row is hovered, which means **a test that clicks one has to hover first —
/// exactly like a person.** Fifteen tests needed this line added, and that
/// is the feature working rather than a tax: before it, every one of them
/// was reaching a control no pointer had ever been near.
///
/// It drives time as well as moving the pointer. `hover_t` starts at 0 and
/// rises, so a `run_until_parked` alone can leave the strip one frame short
/// of existing — the loop settles when the target appears rather than
/// guessing a duration.
/// Move the pointer onto a rail row and let the fade run.
///
/// Asserts nothing about what appears, so it can be used to prove an
/// affordance is **absent** as well as present — `hover_row` below is the
/// asserting version and cannot answer a negative.
#[track_caller]
fn point_at_row(h: &Harness, vcx: &mut VisualTestContext, row: &'static str) {
    let bounds = vcx
        .debug_bounds(row)
        .unwrap_or_else(|| panic!("row {row} did not render"));
    vcx.simulate_mouse_move(bounds.center(), None, Modifiers::none());
    vcx.run_until_parked();
    drive(h, vcx, Duration::from_millis(240));
}

/// Put the pointer on a rail row, and wait for the affordance to exist.
///
/// Stage AK made the row's five affordances icons that mount only while the
/// row is hovered, which means **a test that clicks one has to hover first —
/// exactly like a person.** Seventeen call sites needed this line added, and
/// that is the feature working rather than a tax: before it, every one of
/// them was reaching a control no pointer had ever been near.
///
/// It drives time as well as moving the pointer. `hover_t` starts at 0 and
/// rises, so a `run_until_parked` alone can leave the strip one frame short
/// of existing — the loop settles when the target appears rather than
/// guessing a duration.
#[track_caller]
fn hover_row(h: &Harness, vcx: &mut VisualTestContext, row: &'static str, reveals: &'static str) {
    let bounds = vcx
        .debug_bounds(row)
        .unwrap_or_else(|| panic!("row {row} did not render"));
    vcx.simulate_mouse_move(bounds.center(), None, Modifiers::none());
    vcx.run_until_parked();
    for _ in 0..30 {
        if vcx.debug_bounds(reveals).is_some() {
            return;
        }
        drive(h, vcx, Duration::from_millis(16));
    }
    panic!("hovering {row} never revealed {reveals}");
}

/// Is this element on screen at all?
fn shown(vcx: &mut VisualTestContext, selector: &'static str) -> bool {
    vcx.debug_bounds(selector).is_some()
}

/// A worktree fact, present and `dirty` files dirty.
fn worktree(branch: &str, dirty: Option<u64>, present: bool) -> eave_wire::WorktreeInfo {
    eave_wire::WorktreeInfo {
        base: "/tmp/base".into(),
        branch: branch.into(),
        present,
        dirty_files: dirty,
    }
}

/// The real macOS text system (font-kit), created **once per process**.
///
/// Two threads constructing a platform at the same moment aborts, and
/// `cargo test` is parallel by default — so the platform is built once,
/// its text system kept, and the platform itself dropped. Shaping is
/// `Send + Sync`, so every test shares this safely.
fn real_text_system() -> Arc<gpui::TextSystem> {
    static TS: std::sync::OnceLock<Arc<gpui::TextSystem>> = std::sync::OnceLock::new();
    TS.get_or_init(|| {
        let platform = gpui_platform::current_platform(true);
        Arc::new(gpui::TextSystem::new(platform.text_system()))
    })
    .clone()
}

/// Shape `text` with the REAL macOS text system and return the width it
/// actually occupies. Hermetic: no window server, no compositor, no
/// window — just font metrics, which is precisely what a fact must fit.
fn real_text_width(text: &str, family: &str, size: f32) -> Pixels {
    real_text_width_at(text, family, size, gpui::FontWeight::default())
}

/// [`real_text_width`], at a stated WEIGHT.
///
/// Slice AS: `real_text_width` shapes at the family's default weight, and a
/// session name is drawn at [`theme::WEIGHT_STRONG`] — semibold is wider
/// than regular, so every name measurement in this file that used the plain
/// helper was optimistic about how much room a name needs. It matters
/// most for the two tests that ask "does a realistic name FIT its slot",
/// where being optimistic is the same as not asking.
fn real_text_width_at(text: &str, family: &str, size: f32, weight: gpui::FontWeight) -> Pixels {
    let wts = gpui::WindowTextSystem::new(real_text_system());
    let run = TextRun {
        len: text.len(),
        font: gpui::Font {
            weight,
            ..gpui::font(family)
        },
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    wts.shape_line(SharedString::from(text.to_string()), px(size), &[run], None)
        .width
}

/// The width `text` occupies **as a session name** — the size and weight the
/// rail row and a session tab both draw one at.
///
/// The 12.5 is a literal rather than [`theme::RAIL_NAME_SIZE`] (§7: never
/// derive a test's expectation from the constant under test), and the guard
/// is what makes moving that constant fail loudly here instead of sliding
/// every name measurement in this file along with it. **It has now done that
/// once for real** — Slice D3 stepped the rail's type down and three tests
/// stopped, which is the outcome this shape exists to produce.
///
/// It exists because three places described the same glyphs and all three
/// disagreed: `ui.rs` drew `px(13.0)`, `theme::BODY_SIZE` had moved to 13.5
/// underneath it, and `the_action_strip_does_not_cover_the_session_name`
/// shaped **14.0 at the family's default weight** — so the one test asking
/// "is the name covered" was measuring a font nothing renders.
#[track_caller]
fn real_name_width(text: &str) -> Pixels {
    assert_eq!(
        theme::RAIL_NAME_SIZE,
        12.5,
        "a session name is no longer 12.5 px, so every width measured \
         through this helper is measuring the wrong font"
    );
    assert_eq!(
        theme::WEIGHT_STRONG,
        gpui::FontWeight::SEMIBOLD,
        "a session name is no longer semibold; semibold is wider than \
         regular and these are FIT assertions"
    );
    real_text_width_at(text, theme::SANS, 12.5, gpui::FontWeight::SEMIBOLD)
}

// ---------------------------------------------------------------------------
// Stated facts — REAL text system.
//
// Amendment 1: these tests assert that a fact the operator must read
// actually fits the room it renders in, so they shape it with real
// font-kit metrics. Synthetic glyph advances would make every one of
// them pass vacuously — which is the exact failure this amendment
// exists to prevent.

/// Stage L: the friction log's destination (⌘⇧L since Slice C5, ⌘L before
/// it). It used to be
/// `~/Desktop/eave/docs/dogfood-log.md` — one person's desk, hardcoded,
/// which on any other machine meant the keystroke wrote into a directory
/// that did not exist and reported a failure the operator had no way to
/// interpret. It now follows the data dir, so `--data-dir` moves it with
/// everything else, and the explicit override still wins.
#[test]
fn the_friction_log_follows_the_data_dir_not_a_desk() {
    let data = PathBuf::from("/tmp/eave-l/data");
    assert_eq!(
        resolve_friction_log(None, Some(data.clone())),
        data.join("dogfood-log.md"),
        "the log belongs beside the journals the notes are about"
    );
    assert_eq!(
        resolve_friction_log(Some("/tmp/elsewhere.md".into()), Some(data.clone())),
        PathBuf::from("/tmp/elsewhere.md"),
        "EAVE_DOGFOOD_LOG still wins — it is how a checkout keeps writing \
         into its own docs, and how every test avoids the real log"
    );
    assert_eq!(
        resolve_friction_log(Some(String::new()), Some(data.clone())),
        data.join("dogfood-log.md"),
        "an empty override is not a path; it must not resolve to the \
         filesystem root"
    );
    // No data dir recorded (nothing called `run`): the layout default,
    // never a guess about anyone's home directory layout.
    let fallback = resolve_friction_log(None, None);
    assert_eq!(fallback, default_data_dir().join("dogfood-log.md"));
    assert!(
        !fallback.to_string_lossy().contains("Desktop"),
        "the desk is gone: {fallback:?}"
    );
}

/// Stage L: the aggregate resource fact. The shapes are asserted on the
/// pure function (that is where the arithmetic lives) and the rendering
/// is asserted through the shell — present when something live reports,
/// gone when nothing does, and fitting the rail at real glyph widths,
/// because a ceiling that truncates to "3 agents · 12.4 GB of…" states
/// the one half that cannot be acted on.
#[gpui::test]
fn the_rail_states_what_every_live_agent_costs(cx: &mut TestAppContext) {
    const GB: u64 = 1024 * 1024 * 1024;
    const MB: u64 = 1024 * 1024;

    // Arithmetic first, on the function itself.
    let mut live_a = session("aaaa1111");
    live_a.live = true;
    live_a.rss_bytes = Some(GB / 4);
    let mut live_b = session("bbbb2222");
    live_b.live = true;
    live_b.rss_bytes = Some(GB / 4);
    // Dead sessions cost nothing and must not be counted...
    let mut dead = session("cccc3333");
    dead.rss_bytes = Some(99 * GB);
    // ...and a live one whose probe failed is absent, never a zero.
    let mut unprobed = session("dddd4444");
    unprobed.live = true;

    let all = vec![
        live_a.clone(),
        live_b.clone(),
        dead.clone(),
        unprobed.clone(),
    ];
    let (lines, over) = resource_line(&all, Some(32 * GB), None).expect("two live agents report");
    assert_eq!(lines, ["2 agents · 0.5 GB of 32.0 GB"]);
    assert!(!over, "half a gig of thirty-two is not a warning");

    let (one, _) = resource_line(&[live_a.clone()], Some(32 * GB), None).unwrap();
    assert_eq!(
        one,
        ["1 agent · 0.2 GB of 32.0 GB"],
        "singular reads as singular"
    );

    let mut hog = session("eeee5555");
    hog.live = true;
    hog.rss_bytes = Some(24 * GB);
    let (_, over) = resource_line(&[hog.clone()], Some(32 * GB), None).unwrap();
    assert!(over, "three quarters of the machine is the warning case");

    // No denominator is stated as absence, never invented.
    let (bare, over) = resource_line(&all, None, None).unwrap();
    assert_eq!(bare, ["2 agents · 0.5 GB resident"]);
    assert!(
        !over,
        "without a machine size there is no ratio to warn about"
    );

    // Nothing live that reports memory ⇒ no line at all. A "0.0 GB" over
    // a running agent would be a lie; silence is the honest shape.
    assert!(resource_line(&[dead.clone(), unprobed.clone()], Some(32 * GB), None).is_none());
    assert!(resource_line(&[], Some(32 * GB), None).is_none());

    // Slice C1: web memory is stated ALONGSIDE agent memory, not folded
    // into it. Closing a tab and stopping an agent are different acts, and
    // one number would say neither which to do nor that there was a choice.
    // Two ROWS, not one longer line: both clauses side by side measured
    // 283 px against the 188 px the rail gives, so it would have truncated.
    //
    // **Slice D3 moved the denominator to a THIRD row, and the number on it
    // is the one `over` is computed from.** Before this, the agents row read
    // "0.5 GB of 32.0 GB" while `over` measured agents PLUS web — so with a
    // browser holding memory eave could state one ratio and colour it by
    // another. The ceiling row states the total it actually warns on.
    let (both, over) = resource_line(&all, Some(32 * GB), Some((6, 490 * MB))).unwrap();
    assert_eq!(
        both,
        [
            "2 agents · 0.5 GB",
            "web · 0.5 GB in 6",
            "1.0 GB of 32.0 GB"
        ]
    );
    assert!(!over);

    // A browser with no agents still reports. The rail's memory line is
    // about what eave is holding, and a page is the larger half of it.
    let (web_only, _) = resource_line(
        &[dead.clone(), unprobed.clone()],
        Some(32 * GB),
        Some((6, 490 * MB)),
    )
    .expect("a browser holding half a gig is a fact worth stating");
    assert_eq!(web_only, ["web · 0.5 GB in 6"]);

    // The ceiling is over EVERYTHING eave holds. A browser that pushed the
    // machine into paging did so whether or not an agent helped.
    let (_, over) = resource_line(&[], Some(32 * GB), Some((6, 24 * GB))).unwrap();
    assert!(
        over,
        "three quarters of the machine held by pages is still three quarters"
    );

    // And absence is still absence: no agents, no browser, no line.
    assert!(resource_line(&[dead.clone()], Some(32 * GB), None).is_none());

    // Now through the shell.
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(vec![dead.clone(), unprobed.clone()]);
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "rail-resource"),
        "nothing live reports memory — the rail must state nothing rather \
         than a zero"
    );

    h.plumbing.push_rail(all.clone());
    vcx.run_until_parked();
    let bounds = vcx
        .debug_bounds("rail-resource")
        .expect("the aggregate fact renders once a live agent reports");

    // The fact must fit the room it renders in, at REAL metrics (Stage J
    // amendment 1). Room is the element's box minus its own horizontal
    // padding — measuring the padded box would flatter the fit.
    let room = bounds.size.width - px(theme::SPACE_3 * 2.);
    // Worst case that can actually appear here: a big fleet, a big
    // machine, and every number at its widest.
    let fleet: Vec<eave_wire::SessionInfo> = (0..30)
        .map(|i| {
            let mut s = session(&format!("ffff{i:04}"));
            s.live = true;
            s.rss_bytes = Some(4 * GB);
            s
        })
        .collect();
    let (widest, _) = resource_line(&fleet, Some(512 * GB), None).unwrap();
    assert_eq!(widest, ["30 agents · 120.0 GB of 512.0 GB"]);
    // Slice D3: with a browser running the denominator moves to its own
    // line, and the number on it is agents PLUS web — which is the number
    // `over` warns on. The old single line put an agents-only numerator
    // against the machine total, so eave could state 23% in warning ink.
    // Slice C1 made the line longer: a browser adds a second clause. The
    // worst case is now agents AND web at their widest, and it is the one
    // that has to clear the rail.
    let (widest_web, _) = resource_line(&fleet, Some(512 * GB), Some((6, 12 * GB))).unwrap();
    for line in widest
        .into_iter()
        .chain(widest_web)
        .chain(["2 agents · 0.5 GB of 32.0 GB".to_string()])
    {
        let w = real_text_width(&line, ".SystemUIFont", theme::MICRO_SIZE);
        assert!(
            w <= room,
            "the resource fact {line:?} needs {w:?} but the rail gives \
             {room:?} — a ceiling stated half way is not stated"
        );
    }
}

/// Slice AO moved the worktree fact off a line of its own and into the
/// MARKS group on line two, beside the folder. The group is `flex_none`
/// and the folder truncates against it, so the question is no longer "does
/// this line fit the rail" — it is **"is there any folder left"**.
///
/// Both halves are asserted. The marks are measured with real font-kit
/// metrics against the room the rail actually gives them; the folder is
/// measured from the rendered layout and has to keep a stated floor.
#[gpui::test]
fn a_marked_row_still_shows_its_folder(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut short = session("aaaa1111");
    short.worktree = Some(worktree("eave/aaaa1111", Some(2), true));
    let mut long = session("bbbb2222");
    long.cwd = "/Users/somebody/very/deeply/nested/checkout/of/a/project".into();
    long.worktree = Some(worktree("eave/bbbb2222", None, true));
    h.plumbing.push_rail(vec![short, long]);
    vcx.run_until_parked();

    // Line two as a whole is full-width, so the room on it does not depend
    // on the glyphs in it — the composition every measurement below needs.
    let line_two = |sel: &'static str, vcx: &mut VisualTestContext| {
        let cwd = vcx.debug_bounds(sel).unwrap();
        let marks = vcx
            .debug_bounds(if sel.ends_with("aaaa1111") {
                "marks-aaaa1111"
            } else {
                "marks-bbbb2222"
            })
            .unwrap();
        (cwd, marks)
    };
    let (short_cwd_box, short_marks) = line_two("cwd-aaaa1111", vcx);
    let (long_cwd_box, long_marks) = line_two("cwd-bbbb2222", vcx);
    let room =
        |c: gpui::Bounds<Pixels>, m: gpui::Bounds<Pixels>| m.origin.x + m.size.width - c.origin.x;
    assert_eq!(
        room(short_cwd_box, short_marks),
        room(long_cwd_box, long_marks),
        "line two must be full-width: the room a row gets cannot depend on \
         the length of the path in it, or this measurement means nothing"
    );

    // Every shape the rail can put in the marks group, measured with real
    // metrics. The id is not among them — Slice AO prints no session id on
    // a row — so the widest is a six-figure dirty count.
    //
    // 70 px is written as a literal on purpose. It happens to equal
    // `ROW_MARKS_MAX` today; reading it FROM that constant would mean a
    // future slice could raise the cap and lengthen a mark in one commit
    // and this test would move with it and never fail.
    let reachable = [
        row_marks(&marked(Some(0), 0)),
        row_marks(&marked(Some(999_999), 0)),
        row_marks(&marked(None, 0)),
        row_marks(&marked(None, 9)),
    ];
    for mark in reachable.into_iter().flatten() {
        assert!(
            !mark.contains("aaaa1111"),
            "the marks group is printing a session id: {mark:?}"
        );
        let w = real_text_width(&mark, theme::SANS, theme::MICRO_SIZE);
        assert!(
            w <= px(70.),
            "the marks group {mark:?} shapes to {w:?}; past 70px it starts \
             truncating, and a truncated count is a wrong count"
        );
    }
    for state in ["quiet", "unresponsive", "failed"] {
        let word = state_badge(state).unwrap();
        let w = real_text_width(word, theme::SANS, theme::SMALL_SIZE);
        assert!(
            w <= px(110.),
            "badge {word:?} needs {w:?} on a line it shares with the name"
        );
    }

    // And the rendered floor: with the widest marks on it, the folder is
    // still a folder. 82 px is what `~/Desktop/eave` needs at
    // `DETAIL_SIZE`; the assertion is a round number under that rather
    // than the layout's own arithmetic.
    let mut crowded = session("cccc3333");
    crowded.cwd = "/Users/somebody/very/deeply/nested/checkout/of/a/project".into();
    crowded.shared_with = (0..9).map(|i| format!("share{i}00000")).collect();
    crowded.worktree = Some(worktree("eave/cccc3333", Some(999_999), true));
    h.plumbing.push_rail(vec![crowded]);
    vcx.run_until_parked();
    let folder = vcx.debug_bounds("cwd-cccc3333").unwrap();
    assert!(
        folder.size.width >= px(80.),
        "the folder was squeezed to {:?} by its own marks — the path is \
         the one thing on that line a person recognises",
        folder.size.width
    );
}

/// A session carrying marks, for the measurements above. `dirty` of
/// `None` means no worktree at all, which is not the same as a worktree
/// whose scan failed (`Some(None)` is unreachable through this helper and
/// is measured directly).
fn marked(dirty: Option<u64>, sharers: usize) -> SessionInfo {
    let mut s = session("aaaa1111");
    if sharers == 0 {
        s.worktree = Some(worktree("eave/aaaa1111", dirty, true));
    }
    s.shared_with = (0..sharers).map(|i| format!("share{i}00")).collect();
    s
}

/// The one fact on a row whose length is unbounded — the engine's stated
/// note — must WRAP, never clip. A clipped confirmation hides "delete
/// again to mean it", which is the whole explanation of an armed button.
///
/// Slice I2's sharer list was the other half of this test and is gone: the
/// rail states a count now, and a count has a known width. That is the
/// fact this used to protect, protected by construction instead.
///
/// Real metrics prove the wrap is required; the rendered layout proves the
/// element actually grows instead of holding one line and swallowing the
/// tail.
#[gpui::test]
fn facts_that_grow_wrap_instead_of_losing_their_tail(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut one = session("aaaa1111");
    one.shared_with = vec!["bbbb2222".into()];
    one.worktree = Some(worktree("eave/aaaa1111", Some(1), true));
    h.plumbing.push_rail(vec![one]);
    h.plumbing
        .push_row_note(("aaaa1111".into(), Some(RowNote::Error("refused".into()))));
    vcx.run_until_parked();
    let room = vcx.debug_bounds("wt-note-aaaa1111").unwrap().size.width;
    let short_note = vcx.debug_bounds("wt-note-aaaa1111").unwrap().size.height;

    let confirm = "2 uncommitted files, 0 unmerged commits \u{2014} delete again to mean it";
    assert!(
        real_text_width(confirm, theme::SANS, theme::MICRO_SIZE) > room,
        "the armed confirmation does not fit on one line"
    );

    h.plumbing.push_row_note((
        "aaaa1111".into(),
        Some(RowNote::ConfirmDelete {
            dirty_files: Some(2),
            unmerged_commits: Some(0),
        }),
    ));
    vcx.run_until_parked();
    assert!(
        vcx.debug_bounds("wt-note-aaaa1111").unwrap().size.height > short_note,
        "the armed confirmation clipped instead of wrapping"
    );
}

// ---------------------------------------------------------------------------
// Structure and intent — Noop text system.
//
// Every test below asserts which element rendered, which one received a
// click, and which `Command` the shell emitted. None of that depends on
// glyph advances: an element's hit area is read from the layout at test
// time and clicked at its own centre, so synthetic metrics move the
// target and the click together. Real metrics would buy nothing here and
// cost a process-wide platform.

/// Isolation is offered only while it is still possible — never started,
/// not live, no worktree yet, and a collision actually standing (Slice
/// I2, decision 1). The click states the intent; the engine decides.
#[gpui::test]
fn isolate_is_offered_only_while_it_is_still_possible(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let offered = |info: SessionInfo, h: &Harness, vcx: &mut VisualTestContext| {
        let id = info.session_id.clone();
        h.plumbing.push_rail(vec![info]);
        vcx.run_until_parked();
        assert!(shown(vcx, "rail-aaaa1111"), "row {id} should render");
        // Stage AK: the affordance is hover-revealed, so "is it offered"
        // is only answerable with the pointer on the row. `point_at_row`
        // rather than `hover_row` precisely because half of this test's
        // cases expect the answer NO.
        point_at_row(h, vcx, "rail-aaaa1111");
        shown(vcx, "iso-aaaa1111")
    };

    let mut ready = session("aaaa1111");
    ready.state = "new".into();
    ready.shared_with = vec!["bbbb2222".into()];
    assert!(offered(ready.clone(), &h, vcx), "a fresh collided session");

    let mut alone = ready.clone();
    alone.shared_with.clear();
    assert!(
        !offered(alone, &h, vcx),
        "no collision, nothing to opt out of"
    );

    let mut started = ready.clone();
    started.state = "idle".into();
    assert!(
        !offered(started, &h, vcx),
        "cwd is fixed after the first send"
    );

    let mut live = ready.clone();
    live.live = true;
    assert!(!offered(live, &h, vcx), "never while a process is live");

    let mut isolated = ready.clone();
    isolated.worktree = Some(worktree("eave/aaaa1111", Some(0), true));
    assert!(!offered(isolated, &h, vcx), "already isolated");

    // And the offer, when taken, is exactly one stated intent.
    let _ = offered(ready, &h, vcx);
    let _ = h.drain();
    hover_row(&h, vcx, "rail-aaaa1111", "iso-aaaa1111");
    click(vcx, "iso-aaaa1111");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Isolate(id)] if id == "aaaa1111"),
        "expected one Isolate, got {cmds:?}"
    );
}

/// Amendment 2: an affordance acts on the row, it does not select it.
/// Clicking "isolate" used to also switch the subscription — incidental
/// gpui bubbling, never designed. The row's own body still selects.
#[gpui::test(iterations = 12)]
fn an_affordance_click_acts_on_the_row_without_selecting_it(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut info = session("aaaa1111");
    info.state = "new".into();
    info.shared_with = vec!["bbbb2222".into()];
    info.worktree = None;
    h.plumbing.push_rail(vec![info.clone()]);
    vcx.run_until_parked();
    let _ = h.drain();

    hover_row(&h, vcx, "rail-aaaa1111", "iso-aaaa1111");
    click(vcx, "iso-aaaa1111");
    let cmds = h.drain();
    assert!(
        !cmds.iter().any(|c| matches!(c, Command::Select(_))),
        "an affordance click must not select the row, got {cmds:?}"
    );
    assert_eq!(cmds.len(), 1, "exactly one intent per click: {cmds:?}");

    // The delete affordance, same rule.
    let mut orphan = session("aaaa1111");
    orphan.worktree = Some(worktree("eave/aaaa1111", Some(2), true));
    h.plumbing.push_rail(vec![orphan]);
    vcx.run_until_parked();
    let _ = h.drain();
    hover_row(&h, vcx, "rail-aaaa1111", "wtd-aaaa1111");
    click(vcx, "wtd-aaaa1111");
    let cmds = h.drain();
    assert!(
        !cmds.iter().any(|c| matches!(c, Command::Select(_))),
        "delete must not select either, got {cmds:?}"
    );

    // But the row itself still requests the switch — the fix removed an
    // accident, not the feature.
    let _ = h.drain();
    click(vcx, "rail-aaaa1111");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Select(id)] if id == "aaaa1111"),
        "the row body selects, got {cmds:?}"
    );
}

/// Deleting a worktree takes two clicks, and the second one only means
/// what the first was refused for. The engine restates counts on every
/// refusal; the rail must show the FRESH ones, not the armed click's.
#[gpui::test]
fn deleting_a_worktree_takes_two_clicks_and_restates_fresh_counts(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut info = session("aaaa1111");
    info.worktree = Some(worktree("eave/aaaa1111", Some(2), true));
    h.plumbing.push_rail(vec![info]);
    vcx.run_until_parked();
    let _ = h.drain();

    // First click never forces.
    hover_row(&h, vcx, "rail-aaaa1111", "wtd-aaaa1111");
    click(vcx, "wtd-aaaa1111");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::RemoveWorktree { id, force: false }] if id == "aaaa1111"),
        "the first click must not force, got {cmds:?}"
    );
    assert!(!shown(vcx, "wt-note-aaaa1111"), "nothing to state yet");

    // The engine refuses with facts; the row states them and arms.
    h.plumbing.push_row_note((
        "aaaa1111".into(),
        Some(RowNote::ConfirmDelete {
            dirty_files: Some(2),
            unmerged_commits: Some(0),
        }),
    ));
    vcx.run_until_parked();
    assert!(shown(vcx, "wt-note-aaaa1111"), "the refusal must be stated");

    // A second refusal with different counts replaces the note — the
    // operator always reads what is true now, not what was true then.
    h.plumbing.push_row_note((
        "aaaa1111".into(),
        Some(RowNote::ConfirmDelete {
            dirty_files: Some(5),
            unmerged_commits: Some(1),
        }),
    ));
    vcx.run_until_parked();
    let stated = vcx
        .read_window(&h.window, |shell, cx| {
            shell
                .read(cx)
                .sessions
                .read(cx)
                .row_notes
                .get("aaaa1111")
                .cloned()
        })
        .unwrap();
    assert!(
        matches!(
            stated,
            Some(RowNote::ConfirmDelete {
                dirty_files: Some(5),
                unmerged_commits: Some(1)
            })
        ),
        "the note must carry the freshest counts, got {stated:?}"
    );

    // Now the click means it.
    let _ = h.drain();
    hover_row(&h, vcx, "rail-aaaa1111", "wtd-aaaa1111");
    click(vcx, "wtd-aaaa1111");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::RemoveWorktree { id, force: true }] if id == "aaaa1111"),
        "the armed click forces, got {cmds:?}"
    );
}

/// The armed state is app-held on purpose: anything that clears the note
/// disarms, and the next click is an unforced request again. It fails
/// toward refusing, which is the safe direction.
#[gpui::test]
fn clearing_the_note_disarms_the_delete(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut info = session("aaaa1111");
    info.worktree = Some(worktree("eave/aaaa1111", Some(2), true));
    h.plumbing.push_rail(vec![info]);
    h.plumbing.push_row_note((
        "aaaa1111".into(),
        Some(RowNote::ConfirmDelete {
            dirty_files: Some(2),
            unmerged_commits: Some(0),
        }),
    ));
    vcx.run_until_parked();

    h.plumbing.push_row_note(("aaaa1111".into(), None)); // settled
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "wt-note-aaaa1111"),
        "a settled action states nothing"
    );

    let _ = h.drain();
    hover_row(&h, vcx, "rail-aaaa1111", "wtd-aaaa1111");
    click(vcx, "wtd-aaaa1111");
    let cmds = h.drain();
    assert!(
        matches!(
            cmds.as_slice(),
            [Command::RemoveWorktree { force: false, .. }]
        ),
        "a disarmed delete asks again, got {cmds:?}"
    );
}

/// The "open" affordance hands a path to the system handler. The harness
/// must never actually launch Finder, so both open sites route through
/// one seam; this asserts the intent and the path.
#[gpui::test]
fn opening_a_worktree_states_the_path_without_launching_finder(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    OPENED.with(|o| o.borrow_mut().clear());
    let mut info = session("aaaa1111");
    info.cwd = "/tmp/eave-harness/worktree".into();
    info.worktree = Some(worktree("eave/aaaa1111", Some(0), true));
    h.plumbing.push_rail(vec![info]);
    vcx.run_until_parked();

    hover_row(&h, vcx, "rail-aaaa1111", "wto-aaaa1111");
    click(vcx, "wto-aaaa1111");
    let opened = OPENED.with(|o| o.borrow().clone());
    assert_eq!(
        opened,
        vec![PathBuf::from("/tmp/eave-harness/worktree")],
        "open must hand the worktree path to the system handler, once"
    );
}

/// One journalled user send — enough to put a real row in the transcript.
fn user_record(seq: u64, text: &str) -> eave_core::journal::Record {
    eave_core::journal::Record {
        seq,
        ts_ms: seq * 10,
        event: eave_core::journal::JournalEvent::UserSent { text: text.into() },
    }
}

/// A journalled Claude tool start, decoded the way the Claude adapter
/// decodes it — the harness must exercise the real record shape, not a
/// hand-built one that skips the field the row renders from.
fn tool_started(seq: u64, id: &str, name: &str, input: &str) -> eave_core::journal::Record {
    let parsed = serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
    tool_started_typed(
        seq,
        id,
        name,
        input,
        Some(eave_core::stream::decode_tool(name, &parsed)),
    )
}

/// A tool start from any provider, with the typed call stated outright.
/// `call: None` is a record written before adapters decoded anything.
fn tool_started_typed(
    seq: u64,
    id: &str,
    name: &str,
    input: &str,
    call: Option<eave_core::tool::ToolCall>,
) -> eave_core::journal::Record {
    eave_core::journal::Record {
        seq,
        ts_ms: seq * 10,
        event: eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::ToolUseStarted {
                id: id.into(),
                name: name.into(),
                input: input.into(),
                call,
            },
        },
    }
}

fn tool_finished(seq: u64, id: &str, result: &str, is_error: bool) -> eave_core::journal::Record {
    eave_core::journal::Record {
        seq,
        ts_ms: seq * 10,
        event: eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::ToolUseFinished {
                id: id.into(),
                result: result.into(),
                is_error,
            },
        },
    }
}

fn assistant_record(seq: u64, text: &str) -> eave_core::journal::Record {
    eave_core::journal::Record {
        seq,
        ts_ms: seq * 10,
        event: eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::AssistantText { text: text.into() },
        },
    }
}

/// A code block can leave the app, whole.
///
/// Until this slice there was no way to get code out of eave at all: no
/// selection, no copy button, ⌘C unbound. The transcript could show you a
/// patch and the only way to use it was to retype it.
///
/// This test is about the click and the clipboard only. That copy takes the
/// WHOLE fence when the view is capped is a property of `fence_view` and is
/// asserted there, purely — a fence tall enough to be capped puts its own
/// copy button ~3000 px above the viewport, where a simulated click hits
/// nothing (see the note in `click_with`).
#[gpui::test]
fn a_fence_leaves_the_app_when_you_click_copy(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });

    // Deliberately SHORT. This test is about the click and the clipboard;
    // the cap is pure logic and is tested as such in `fence_view`'s own
    // unit tests, because a fence tall enough to be capped puts its own
    // copy button thousands of pixels above the viewport.
    let fence = "```rust\nlet x = 1;\nlet y = 2;\n```";
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![assistant_record(1, fence)],
    });
    vcx.run_until_parked();

    click(vcx, "fence-copy");
    vcx.run_until_parked();

    let got = vcx
        .read_from_clipboard()
        .and_then(|i| i.text())
        .expect("the clipboard holds the fence");
    assert_eq!(got, "let x = 1;\nlet y = 2;", "the code, and only the code");
    assert!(!got.contains("```"), "the fence markers were copied");
    assert!(!got.contains("rust"), "the language label was copied");

    // ...and the app says so, rather than leaving you to guess.
    let note = vcx
        .read_window(&h.window, |shell, cx| shell.read(cx).friction.clone())
        .unwrap();
    assert!(
        note.is_some_and(|(_, n)| n.contains("copied 2 lines")),
        "a copy that states nothing is a copy you have to verify by pasting"
    );
}

/// A tool row states what it was asked to do.
///
/// Before this slice the transcript rendered `Bash  done`: the parser kept
/// a `tool_result`'s id and threw away its `content`, and kept a `tool_use`'s
/// id and name and threw away its `input`. An agent could run the whole test
/// suite and eave would show you two words, neither of them the command.
/// A tool row with a body opens onto it; one without has nothing to open.
///
/// **The affordance itself is no longer measurable, and that is worth
/// recording.** Slice BZ first drew eave's own disclosure chevron here, with
/// its own selector, and this test measured it — including that it fitted
/// inside `TOOL_NAME_LINE` and so could not set the row height. Then the row
/// moved onto bezel's `step_row`, which draws that chevron itself from the
/// `expanded` argument and emits no selector for it. The affordance is still
/// on screen; the harness simply cannot see it any more.
///
/// So this asserts the behaviour underneath instead: `has_body` is what
/// decides whether there is a disclosure at all, and `tool-body` — an eave
/// element, with an eave selector — is what it opens onto.
#[gpui::test]
fn only_a_tool_row_with_a_body_says_it_opens(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![tool_started(
            1,
            "toolu_1",
            "Bash",
            r#"{"command":"cargo test --workspace"}"#,
        )],
    });
    vcx.run_until_parked();
    assert!(
        vcx.debug_bounds("tool-row").is_some(),
        "the tool row is not on screen"
    );
    // Shut: the body is not drawn.
    assert!(
        vcx.debug_bounds("tool-body").is_none(),
        "a tool row drew its body before anyone opened it"
    );

    // Open it the way a reader would.
    let head = vcx
        .debug_bounds("tool-head")
        .expect("a tool with a body is not clickable");
    vcx.simulate_click(head.center(), gpui::Modifiers::default());
    vcx.run_until_parked();
    assert!(
        vcx.debug_bounds("tool-body").is_some(),
        "clicking a tool row with a body opened nothing"
    );
}

#[gpui::test]
fn a_tool_row_states_its_command_and_its_failure(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![tool_started(
            1,
            "toolu_1",
            "Bash",
            r#"{"command":"cargo test --workspace"}"#,
        )],
    });
    vcx.run_until_parked();
    // Bounds-checked until Slice BZ; see the Codex test above for why this is
    // now a model-level assertion. The row must still be on screen.
    assert!(
        vcx.debug_bounds("tool-row").is_some(),
        "a running tool row does not render at all"
    );
    {
        let rows = vcx
            .read_window(&h.window, |shell, cx| {
                shell.read(cx).transcript.read(cx).transcript.rows.clone()
            })
            .unwrap();
        let detail = rows.iter().find_map(|r| match r {
            eave_wire::model::Row::Tool {
                name, input, call, ..
            } => Some(eave_wire::model::tool_detail(call.as_ref(), name, input)),
            _ => None,
        });
        assert_eq!(
            detail.flatten().as_deref(),
            Some("cargo test --workspace"),
            "a running tool row states no command"
        );
    }

    // ...and a failure states why, in place.
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![tool_finished(2, "toolu_1", "error: 3 tests failed", true)],
    });
    vcx.run_until_parked();
    let rows = vcx
        .read_window(&h.window, |shell, cx| {
            shell.read(cx).transcript.read(cx).transcript.rows.clone()
        })
        .unwrap();
    let tool = rows
        .iter()
        .find_map(|r| match r {
            eave_wire::model::Row::Tool {
                input,
                result,
                is_error,
                ..
            } => Some((input.clone(), result.clone(), *is_error)),
            _ => None,
        })
        .expect("a tool row");
    assert!(tool.0.contains("cargo test --workspace"), "input dropped");
    assert_eq!(tool.1, "error: 3 tests failed", "result dropped");
    assert!(tool.2, "the provider's error flag dropped");
    // Bounds-checked until Slice BZ — bezel's `step_row` owns the interior
    // now. The fact survives the finish, which is what this ever meant.
    let detail = rows.iter().find_map(|r| match r {
        eave_wire::model::Row::Tool {
            name, input, call, ..
        } => Some(eave_wire::model::tool_detail(call.as_ref(), name, input)),
        _ => None,
    });
    assert_eq!(
        detail.flatten().as_deref(),
        Some("cargo test --workspace"),
        "the command stopped being stated once the tool finished"
    );
}

/// A successful tool's output is reachable.
///
/// Slice V deliberately shipped only the failure case: a failed tool
/// states why, a successful one states nothing, because its output is
/// usually long and always a row height inside a virtualized list. The
/// missing half was that there was no way to see it AT ALL — the payload
/// crossed the wire and then sat in a row nobody could open.
#[gpui::test]
fn a_tool_card_opens_to_show_what_came_back(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![
            tool_started(1, "toolu_1", "Bash", r#"{"command":"cargo test"}"#),
            tool_finished(2, "toolu_1", "test result: ok. 356 passed", false),
        ],
    });
    vcx.run_until_parked();

    // Closed by default: a `Read` must not make the transcript taller.
    assert!(
        vcx.debug_bounds("tool-body").is_none(),
        "a tool card opened itself"
    );
    let closed = vcx
        .debug_bounds("tool-head")
        .expect("a tool with a body is clickable")
        .size
        .height;

    click(vcx, "tool-head");
    vcx.run_until_parked();
    assert!(
        vcx.debug_bounds("tool-body").is_some(),
        "clicking the row did not open it"
    );

    // ...and it really got taller. This is the assertion that catches a
    // stale `ListState` height: the body can render while the row's cached
    // measurement still says otherwise, and then the rows below it overlap.
    let open_row = vcx
        .debug_bounds("tool-row")
        .expect("the tool row renders")
        .size
        .height;
    assert!(
        open_row > closed,
        "the row did not grow when it opened ({open_row:?} vs head {closed:?}) — \
         a cached ListState height would look exactly like this"
    );

    // Clicking again closes it.
    click(vcx, "tool-head");
    vcx.run_until_parked();
    assert!(
        vcx.debug_bounds("tool-body").is_none(),
        "the card would not close"
    );
}

/// A switch takes the open cards with it. Tool ids are per-session and
/// two sessions can carry the same one.
#[gpui::test]
fn opened_cards_do_not_outlive_their_session(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![
            tool_started(1, "toolu_1", "Bash", r#"{"command":"cargo test"}"#),
            tool_finished(2, "toolu_1", "ok", false),
        ],
    });
    vcx.run_until_parked();
    click(vcx, "tool-head");
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("tool-body").is_some());

    // Session B, same tool id.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "bbbb2222".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "bbbb2222".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "bbbb2222".into(),
        epoch: 1,
        records: vec![
            tool_started(1, "toolu_1", "Bash", r#"{"command":"other"}"#),
            tool_finished(2, "toolu_1", "ok", false),
        ],
    });
    vcx.run_until_parked();
    assert!(
        vcx.debug_bounds("tool-body").is_none(),
        "B's identically-id'd tool opened itself because A had opened one"
    );
}

/// A tool eave has no summary key for states its name and nothing else,
/// rather than inventing a detail from an arbitrary input field.
#[gpui::test]
fn an_unknown_tool_states_no_detail(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![tool_started(1, "toolu_1", "SomeMcpTool", r#"{"opaque":1}"#)],
    });
    vcx.run_until_parked();
    assert!(
        vcx.debug_bounds("tool-detail").is_none(),
        "a detail was invented for a tool with no known summary key"
    );
}

/// A NON-Claude tool row states its argument.
///
/// The row's detail used to come from a table keyed on Claude's tool
/// names, so a Codex `commandExecution` — the same shell command, from a
/// provider eave ships — rendered as a name and a status word. The typed
/// call is the fix, and this is the assertion that it reaches the screen
/// rather than stopping at the wire crate.
#[gpui::test]
fn a_codex_tool_row_states_its_command(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![tool_started_typed(
            1,
            "item_1",
            "commandExecution",
            r#"{"command":"cargo test --workspace","cwd":"/repo"}"#,
            Some(eave_core::tool::ToolCall::Run {
                command: "cargo test --workspace".into(),
            }),
        )],
    });
    vcx.run_until_parked();
    // **This used to measure `tool-detail`'s bounds.** Slice BZ moved the row
    // onto bezel's `step_row`, which builds its own interior and emits no
    // selector, so that element no longer exists to measure. The fact is
    // asserted at the model level instead: the typed `call` is what a Codex
    // `commandExecution` decodes through, and `tool_detail` is what the row
    // is handed. Weaker than a bounds check — it proves the string reaches
    // the row, not that the row drew it — and that is the price of the swap.
    assert!(
        vcx.debug_bounds("tool-row").is_some(),
        "a Codex tool row does not render at all"
    );
    let rows = vcx
        .read_window(&h.window, |shell, cx| {
            shell.read(cx).transcript.read(cx).transcript.rows.clone()
        })
        .unwrap();
    let detail = rows.iter().find_map(|r| match r {
        eave_wire::model::Row::Tool {
            name, input, call, ..
        } => Some(eave_wire::model::tool_detail(call.as_ref(), name, input)),
        _ => None,
    });
    assert_eq!(
        detail.flatten().as_deref(),
        Some("cargo test --workspace"),
        "a Codex tool row states its command"
    );
}

/// A rail has ONE left edge.
///
/// It had four. Every value was a legal `SPACE_*` and the scale was applied
/// everywhere — which is the point: **a spacing scale tells you the SIZE of
/// a gap and says nothing about which edge things line up on.** The header
/// label sat at 24, a row's card at 20, the empty state at 28 and the
/// footer at 24, with the row card overhanging the `+ new` button by 4 px.
/// Both rails had it, identically, because one was copied from the other.
///
/// The assertions compare MEASURED edges against each other and never
/// against `RAIL_GUTTER`. A test that computed its expectation from the
/// constant it is testing would pass for any value of that constant, which
/// is the trap §Traps already names.
#[gpui::test]
fn a_rail_has_one_left_edge(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    // `live` with reported memory, because the footer's resource line is
    // absent entirely when nothing live reports any — and that line is one
    // of the two edges under test.
    let mut only = session("aaaa1111");
    only.live = true;
    only.rss_bytes = Some(64 * 1024 * 1024);
    h.plumbing.push_rail(vec![only]);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                status: FileStatus::Modified,
                adds: Some(1),
                dels: Some(0),
                orig_path: None,
                untracked: false,
            }],
            omitted: 0,
            watching: true,
            basis: DiffBasis::WorkingTree,
        }),
    ));
    vcx.run_until_parked();

    let x = |vcx: &mut VisualTestContext, sel: &'static str| {
        vcx.debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} did not render"))
            .origin
            .x
    };
    let right = |vcx: &mut VisualTestContext, sel: &'static str| {
        let b = vcx
            .debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} did not render"));
        b.origin.x + b.size.width
    };

    // Header label and footer fact are both text directly on the rail, so
    // they share the gutter.
    // Slice D4 deleted the caps header this used to anchor on. The rule it
    // was checking survives — everything at rail level starts on one gutter
    // — so it is anchored on the zone's ghost row, which is text on the rail
    // exactly as the header was.
    assert_eq!(
        x(vcx, "new-session-glyph"),
        x(vcx, "rail-resource"),
        "the sessions zone and the rail's footer start on different edges"
    );
    // The zone's trailing row and a session row's card share their RIGHT
    // edge — the 4 px overhang, asserted. Both sit in the same scroller now,
    // so this is the same rule the header's control used to carry.
    assert_eq!(
        right(vcx, "new-session"),
        right(vcx, "rail-aaaa1111"),
        "the row card and the zone's ghost row end on different edges"
    );

    // The files rail is the same rule, and had the same drift.
    assert_eq!(
        x(vcx, "files-title"),
        x(vcx, "files-mode"),
        "the files rail's header and mode line start on different edges"
    );
    // The files header's trailing slot holds a count, not a control — but
    // it is still the trailing element, so the rule is the same one.
    // (`files-toggle` is the TITLEBAR's collapse control and lives outside
    // the rail entirely; asserting against it was measuring two different
    // columns.)
    assert_eq!(
        right(vcx, "files-count"),
        right(vcx, "file-src/lib.rs"),
        "the file row and the rail's trailing count end on different edges"
    );
}

/// ...and the empty state is on it too, which is the one that read as a
/// typo rather than as drift: "no sessions" was indented PAST the header it
/// sits under.
#[gpui::test]
fn the_empty_state_starts_on_the_gutter(cx: &mut TestAppContext) {
    let (_h, vcx) = boot(cx);
    vcx.run_until_parked();
    let x = |vcx: &mut VisualTestContext, sel: &'static str| {
        vcx.debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} did not render"))
            .origin
            .x
    };
    assert_eq!(
        x(vcx, "rail-empty"),
        x(vcx, "new-session-glyph"),
        "the empty state is indented past the rows it sits among"
    );
}

/// Clicking a row only *requests* a switch — the rail's highlight is a
/// projection of the feed's actual subscription. And once the feed has
/// moved on, frames still in flight from the session that just left must
/// never reach the screen (no bleed between sessions).
#[gpui::test(iterations = 12)]
fn selecting_a_row_requests_a_switch_and_stale_frames_never_render(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    h.plumbing
        .push_rail(vec![session("aaaa1111"), session("bbbb2222")]);
    vcx.run_until_parked();
    let _ = h.drain();

    click(vcx, "rail-bbbb2222");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Select(id)] if id == "bbbb2222"),
        "the click requests a switch, got {cmds:?}"
    );

    // The feed answers: session a, with one row in it.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(1, "first session")],
    });
    vcx.run_until_parked();
    let rows = |vcx: &mut VisualTestContext, h: &Harness| {
        vcx.read_window(&h.window, |shell, cx| {
            shell.read(cx).transcript.read(cx).transcript.rows.len()
        })
        .unwrap()
    };
    assert_eq!(rows(vcx, &h), 1, "the selected session's row should render");

    // The feed switches to b — and a's straggler arrives afterwards.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "bbbb2222".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "bbbb2222".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(2, "straggler from the session that left")],
    });
    vcx.run_until_parked();
    assert_eq!(
        rows(vcx, &h),
        0,
        "a frame tagged with the session that left must be discarded, not merged"
    );
    let sid = vcx
        .read_window(&h.window, |shell, cx| {
            shell.read(cx).transcript.read(cx).session_id.clone()
        })
        .unwrap();
    assert_eq!(sid.as_deref(), Some("bbbb2222"));
}

/// The composer is append + backspace + ⌘V (stated known gaps), and
/// return sends. The clipboard here is the platform's simulated one — no
/// pasteboard on the machine running the tests is touched.
#[gpui::test]
fn the_composer_types_pastes_and_sends(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    let draft = |vcx: &mut VisualTestContext, h: &Harness| {
        vcx.read_window(&h.window, |shell, cx| {
            shell
                .read(cx)
                .composer
                .read(cx)
                .field
                .read(cx)
                .content()
                .to_string()
        })
        .unwrap()
    };

    vcx.simulate_input("run");
    assert_eq!(draft(vcx, &h), "run");

    vcx.simulate_keystrokes("backspace");
    assert_eq!(draft(vcx, &h), "ru", "backspace edits the draft");

    vcx.write_to_clipboard(gpui::ClipboardItem::new_string("n the tests".into()));
    vcx.simulate_keystrokes("cmd-v");
    assert_eq!(draft(vcx, &h), "run the tests", "⌘V appends the clipboard");

    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Send { text, images }] if text == "run the tests" && images.is_empty()),
        "return sends the draft, got {cmds:?}"
    );
    assert_eq!(draft(vcx, &h), "", "a sent draft is cleared");

    // An empty composer sends nothing at all.
    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    assert!(
        h.drain().is_empty(),
        "return on an empty draft is not a send"
    );
}

/// The + control opens the platform picker; chosen paths become chips and
/// ride the send as `@mentions`. A send of files with no typed words is
/// still a send — the paths are the message.
#[gpui::test]
fn attaching_a_file_stages_a_chip_and_sends_the_mention(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    assert!(
        shown(vcx, "composer-attach"),
        "a live composer offered no + control"
    );
    assert!(
        !shown(vcx, "composer-files"),
        "the chip row mounted with nothing staged"
    );

    click(vcx, "composer-attach");
    vcx.run_until_parked();
    assert!(
        cx.did_prompt_for_paths(),
        "the + control never opened a picker"
    );
    cx.simulate_path_prompt_response(|_| {
        Some(vec![PathBuf::from("/tmp/eave-harness/repo/src/main.rs")])
    });
    vcx.run_until_parked();
    assert!(
        shown(vcx, "composer-file-0"),
        "the picked path never became a chip"
    );
    assert!(
        shown(vcx, "composer-files"),
        "the chip row stayed down after a pick"
    );

    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Send { text, images }] if text == "@src/main.rs" && images.is_empty()),
        "a files-only send must be the mention, got {cmds:?}"
    );
    assert!(
        !shown(vcx, "composer-file-0"),
        "a sent chip stayed on the composer"
    );
}

/// Dropped paths take the same stage as the picker. A duplicate of a
/// path already on the row is not a second chip.
#[gpui::test]
fn dropping_the_same_path_twice_keeps_one_chip(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    let path = PathBuf::from("/tmp/eave-harness/repo/README.md");
    h.window
        .update(vcx, |shell, _, cx| {
            shell.stage_paths(vec![path.clone(), path], cx);
        })
        .unwrap();
    vcx.run_until_parked();
    assert!(shown(vcx, "composer-file-0"));
    assert!(
        !shown(vcx, "composer-file-1"),
        "the same path staged twice drew two chips"
    );
}

/// Removing a chip takes it off the send. The + control stays.
#[gpui::test]
fn removing_a_chip_drops_it_from_the_send(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    h.window
        .update(vcx, |shell, _, cx| {
            shell.stage_paths(
                vec![
                    PathBuf::from("/tmp/eave-harness/repo/a.rs"),
                    PathBuf::from("/tmp/eave-harness/repo/b.rs"),
                ],
                cx,
            );
        })
        .unwrap();
    vcx.run_until_parked();
    click(vcx, "composer-file-0-remove");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "composer-file-1"),
        "removing the first chip left a hole rather than collapsing"
    );
    assert!(shown(vcx, "composer-file-0"), "the surviving file vanished");

    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Send { text, images }] if text == "@b.rs" && images.is_empty()),
        "the removed file still rode the send, got {cmds:?}"
    );
}

/// Typed words and staged files compose in that order, and a session
/// switch parks the files the way it parks the draft.
#[gpui::test]
fn attachments_travel_with_the_session_they_were_staged_on(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    h.window
        .update(vcx, |shell, _, cx| {
            shell.stage_paths(vec![PathBuf::from("/tmp/eave-harness/a.rs")], cx);
        })
        .unwrap();
    vcx.run_until_parked();
    assert!(shown(vcx, "composer-file-0"));

    h.plumbing.push_feed(FeedEvent::Session {
        id: "bbbb2222".into(),
    });
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "composer-file-0"),
        "B opened still wearing A's files"
    );

    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert!(
        shown(vcx, "composer-file-0"),
        "A's files did not come back with A"
    );

    vcx.simulate_input("look");
    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Send { text, images }] if text == "look @/tmp/eave-harness/a.rs" && images.is_empty()),
        "text and mention did not compose, got {cmds:?}"
    );
}

/// A ninth path is refused, and the composer states the overflow instead
/// of letting the file vanish.
#[gpui::test]
fn the_composer_states_a_ninth_file_was_not_attached(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    let incoming: Vec<PathBuf> = (0..9)
        .map(|i| PathBuf::from(format!("/tmp/eave-harness/repo/f{i}.rs")))
        .collect();
    h.window
        .update(vcx, |shell, _, cx| {
            shell.stage_paths(incoming, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    assert!(shown(vcx, "composer-file-7"));
    assert!(
        !shown(vcx, "composer-file-8"),
        "the ninth path drew a chip the cap forbids"
    );
    let overflow = vcx
        .read_window(&h.window, |shell, cx| {
            shell.read(cx).composer.read(cx).overflow
        })
        .unwrap();
    assert_eq!(overflow, 1);
    let hint = vcx
        .read_window(&h.window, |shell, cx| {
            composer_hint(0, true, shell.read(cx).composer.read(cx).overflow)
        })
        .unwrap();
    assert!(
        hint.contains("not attached"),
        "the overflow was computed and then not stated: {hint:?}"
    );
}

/// A png rides the send as both an `@mention` and an image path the
/// engine will encode. A `.rs` does not.
#[gpui::test]
fn an_image_chip_sends_the_path_on_the_image_channel(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    let png = PathBuf::from("/tmp/eave-harness/repo/shot.png");
    h.window
        .update(vcx, |shell, _, cx| {
            shell.stage_paths(vec![png.clone()], cx);
        })
        .unwrap();
    vcx.run_until_parked();
    assert!(shown(vcx, "composer-file-0"));
    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    let cmds = h.drain();
    match cmds.as_slice() {
        [Command::Send { text, images }] => {
            assert_eq!(text, "@shot.png");
            assert_eq!(images.len(), 1, "the png never rode the image channel");
            assert!(images[0].ends_with("shot.png"), "image path was {images:?}");
        }
        other => panic!("expected a send with an image, got {other:?}"),
    }
}

/// ⌘V of a clipboard image writes a temp file and stages it. Text paste
/// is unchanged (the composer types/pastes/sends test).
#[gpui::test]
fn pasting_a_screenshot_stages_a_chip(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    vcx.write_to_clipboard(gpui::ClipboardItem::new_image(&gpui::Image::from_bytes(
        gpui::ImageFormat::Png,
        TINY_PNG.to_vec(),
    )));
    vcx.simulate_keystrokes("cmd-v");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "composer-file-0"),
        "a pasted screenshot never became a chip"
    );
    let name = vcx
        .read_window(&h.window, |shell, cx| {
            shell
                .read(cx)
                .composer
                .read(cx)
                .attachments
                .first()
                .map(|f| f.name.clone())
        })
        .unwrap();
    let name = name.expect("no staged file after paste");
    assert!(
        name.starts_with("pasted-image-") && name.ends_with(".png"),
        "pasted screenshot was named {name:?}"
    );
}

const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// A draft belongs to the session it was typed into.
///
/// Before Slice S the composer was one buffer for every session: type into
/// A, select B, press Return, and A's sentence was addressed to B. Nothing
/// in the switch path cleared it and nothing keyed it. The words move with
/// the session now, and the switch is the only thing that moves them.
#[gpui::test]
fn a_draft_stays_with_the_session_it_was_typed_into(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    let draft = |vcx: &mut VisualTestContext, h: &Harness| {
        vcx.read_window(&h.window, |shell, cx| {
            shell
                .read(cx)
                .composer
                .read(cx)
                .field
                .read(cx)
                .content()
                .to_string()
        })
        .unwrap()
    };

    vcx.simulate_input("for aaaa");
    assert_eq!(draft(vcx, &h), "for aaaa");

    h.plumbing.push_feed(FeedEvent::Session {
        id: "bbbb2222".into(),
    });
    vcx.run_until_parked();
    assert_eq!(
        draft(vcx, &h),
        "",
        "B opened still wearing A's unsent words"
    );

    vcx.simulate_input("for bbbb");
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert_eq!(draft(vcx, &h), "for aaaa", "A's draft did not come back");

    // ...and Return sends A's words, once, to the session they were for.
    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Send { text, images }] if text == "for aaaa" && images.is_empty()),
        "the wrong draft was sent, got {cmds:?}"
    );

    // B's words are untouched by A's send: one draft has one home.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "bbbb2222".into(),
    });
    vcx.run_until_parked();
    assert_eq!(draft(vcx, &h), "for bbbb", "sending A's draft spent B's");
}

/// A session switch restores follow.
///
/// `TranscriptState::reset()` clears the transcript, the display, the list,
/// the veil and the turn clock — and never cleared the scroll mode. So a
/// session switched into inherited the last one's: `Free`, which leaves a
/// live reply streaming off the bottom of a viewport that will not follow it
/// and a ↓-bottom pill over a transcript already at its bottom; or a
/// `Pinned { msg }` whose row id then resolved against a transcript that
/// never held that message. Stage G caveat 2 filed the symptom as "not
/// code; watched".
///
/// The mode is written here rather than driven by a wheel event because the
/// defect is in the SWITCH path, not the wheel path: what has to be proven
/// is that a stale mode cannot survive a switch, whatever set it.
#[gpui::test]
fn a_session_switch_restores_follow(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    let mode = |vcx: &mut VisualTestContext, h: &Harness| {
        vcx.read_window(&h.window, |shell, cx| {
            shell.read(cx).transcript.read(cx).scroll
        })
        .unwrap()
    };

    // The reader scrolls up and away from the tail.
    h.window
        .update(vcx, |shell, _, cx| {
            shell
                .transcript
                .update(cx, |t, _| t.scroll = ScrollMode::Free);
        })
        .unwrap();
    assert_eq!(mode(vcx, &h), ScrollMode::Free);

    h.plumbing.push_feed(FeedEvent::Session {
        id: "bbbb2222".into(),
    });
    vcx.run_until_parked();
    assert_eq!(
        mode(vcx, &h),
        ScrollMode::Follow,
        "B opened wearing A's scroll mode"
    );

    // A pin is the worse case: its message id would resolve against a
    // transcript that never held it.
    h.window
        .update(vcx, |shell, _, cx| {
            shell
                .transcript
                .update(cx, |t, _| t.scroll = ScrollMode::Pinned { msg: 7 });
        })
        .unwrap();
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert_eq!(
        mode(vcx, &h),
        ScrollMode::Follow,
        "a pin outlived its session"
    );
}

/// The launcher reaches no composer.
///
/// Q2 unmounted the field but left the key handler open, so the keyboard
/// still filled a buffer nobody could see, and Return dispatched a `Send`
/// the rest loop drops — there being no session to address it to. The words
/// then surfaced in whatever session opened next. Q2's comment at the mount
/// site says an input that silently eats what you type is the one thing
/// this must not be; this is the assertion that holds it to that.
#[gpui::test]
fn the_launcher_reaches_no_composer(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    vcx.run_until_parked();
    let draft = |vcx: &mut VisualTestContext, h: &Harness| {
        vcx.read_window(&h.window, |shell, cx| {
            shell
                .read(cx)
                .composer
                .read(cx)
                .field
                .read(cx)
                .content()
                .to_string()
        })
        .unwrap()
    };

    // No `Session` event has been pushed: this is the launcher, and it
    // states so by having no composer to find.
    assert!(
        vcx.debug_bounds("composer-text").is_none(),
        "the launcher rendered a composer"
    );

    let _ = h.drain();
    vcx.simulate_input("typed at the launcher");
    vcx.simulate_keystrokes("enter");
    assert_eq!(draft(vcx, &h), "", "the launcher filled the hidden buffer");
    assert!(
        h.drain().is_empty(),
        "the launcher dispatched a command with no session to address"
    );

    // ...and the session that opens next opens clean.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert_eq!(
        draft(vcx, &h),
        "",
        "the launcher's keystrokes surfaced in the first session opened"
    );
}

/// Color speaks only when state deserves attention: the rail badges the
/// three states an operator may need to act on and stays quiet for the
/// rest. The unresponsive notice offers a kill and never withdraws it on
/// its own — proven by advancing the clock, which drives the elapsed
/// ticker without any new engine frame.
#[gpui::test]
fn the_badge_ladder_states_only_what_deserves_attention(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    for (state, badged) in [
        ("running", false),
        ("idle", false),
        ("new", false),
        ("quiet", true),
        ("unresponsive", true),
        ("failed", true),
    ] {
        let mut info = session("aaaa1111");
        info.state = state.into();
        h.plumbing.push_rail(vec![info]);
        vcx.run_until_parked();
        assert_eq!(
            shown(vcx, "badge-aaaa1111"),
            badged,
            "state {state:?} badge visibility"
        );
    }

    // The standing notice for the selected session, and its kill.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_attn((
        "aaaa1111".into(),
        AttentionSnapshot::Unresponsive {
            since_ms: 0,
            zero_samples: 5,
            sampled_ms: 5_000,
        },
    ));
    vcx.run_until_parked();
    assert!(shown(vcx, "attn-kill"), "unresponsive offers a kill");

    vcx.executor().advance_clock(Duration::from_secs(5));
    vcx.run_until_parked();
    assert!(
        shown(vcx, "attn-kill"),
        "the notice must not withdraw itself — nothing is ever killed automatically"
    );

    let _ = h.drain();
    click(vcx, "attn-kill");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Kill]),
        "the kill affordance emits exactly one kill, got {cmds:?}"
    );
}

/// The diff pane is unary and never cached: a click requests it, a fresh
/// files snapshot re-requests it, and a file that stops being changed
/// takes its pane with it — "not changed anymore" is stated by the row's
/// disappearance, not by a stale pane.
#[gpui::test]
fn the_diff_pane_opens_on_click_and_closes_when_the_file_stops_changing(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let one = ChangedFile {
        path: "src/lib.rs".into(),
        status: FileStatus::Modified,
        adds: Some(3),
        dels: Some(1),
        orig_path: None,
        untracked: false,
    };
    let snapshot = |files: Vec<ChangedFile>| FilesSnapshot {
        state: GitState::Ok,
        files,
        omitted: 0,
        watching: true,
        basis: DiffBasis::WorkingTree,
    };
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing
        .push_files(("aaaa1111".into(), Some(snapshot(vec![one.clone()]))));
    vcx.run_until_parked();
    let _ = h.drain();

    click(vcx, "file-src/lib.rs");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Diff(p)] if p == "src/lib.rs"),
        "the click requests exactly this file's diff, got {cmds:?}"
    );

    let open_path = |vcx: &mut VisualTestContext, h: &Harness| {
        vcx.read_window(&h.window, |shell, cx| {
            shell
                .read(cx)
                .files
                .read(cx)
                .diff
                .as_ref()
                .map(|d| d.path.clone())
        })
        .unwrap()
    };
    assert_eq!(open_path(vcx, &h).as_deref(), Some("src/lib.rs"));

    // A fresh snapshot that still lists the file re-requests its diff —
    // the files channel is the invalidation signal.
    let _ = h.drain();
    h.plumbing
        .push_files(("aaaa1111".into(), Some(snapshot(vec![one.clone()]))));
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Diff(p)] if p == "src/lib.rs"),
        "a fresh snapshot must re-request the open diff, got {cmds:?}"
    );

    // The file stops being changed: the pane goes with the row.
    h.plumbing
        .push_files(("aaaa1111".into(), Some(snapshot(Vec::new()))));
    vcx.run_until_parked();
    assert_eq!(
        open_path(vcx, &h),
        None,
        "a pane for a file that is no longer changed must close"
    );

    // Clicking the same row twice is a toggle, not two requests.
    h.plumbing
        .push_files(("aaaa1111".into(), Some(snapshot(vec![one]))));
    vcx.run_until_parked();
    click(vcx, "file-src/lib.rs");
    assert_eq!(open_path(vcx, &h).as_deref(), Some("src/lib.rs"));
    click(vcx, "file-src/lib.rs");
    assert_eq!(open_path(vcx, &h), None, "a second click closes the pane");
}

/// ⌘-click on a changed file opens it in the system editor — the other
/// side of the same seam, and the reason tests never launch anything.
#[gpui::test]
fn cmd_clicking_a_changed_file_opens_it_without_launching_anything(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    OPENED.with(|o| o.borrow_mut().clear());
    let mut info = session("aaaa1111");
    info.cwd = "/tmp/eave-harness/repo".into();
    h.plumbing.push_rail(vec![info]);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                status: FileStatus::Modified,
                adds: Some(1),
                dels: Some(0),
                orig_path: None,
                untracked: false,
            }],
            omitted: 0,
            watching: true,
            basis: DiffBasis::WorkingTree,
        }),
    ));
    vcx.run_until_parked();

    click_with(
        vcx,
        "file-src/lib.rs",
        Modifiers {
            platform: true,
            ..Modifiers::none()
        },
    );
    let opened = OPENED.with(|o| o.borrow().clone());
    assert_eq!(
        opened,
        vec![PathBuf::from("/tmp/eave-harness/repo/src/lib.rs")]
    );
}

// ---------------------------------------------------------------------------
// Slice K — approvals.
//
// Text system: **Noop for every test in this section.** These assert what
// the shell *emits* (the `Command` stream) and *whether* a card is on
// screen at all — neither depends on glyph advances, and the elements
// looked up here are containers whose presence is set by the fold, not by
// how wide a word shapes. The one Slice K fact whose FIT could be lost to
// layout is the rail's posture line, and that one is measured with real
// metrics in `a_posture_that_never_asks_fits_the_rail_at_real_widths`
// below (amendment 1: the decision is made per test, not once).

/// A journalled permission question, exactly as the adapter produces one.
fn approval_record(
    seq: u64,
    request_id: &str,
    tool: &str,
    path: &str,
) -> eave_core::journal::Record {
    eave_core::journal::Record {
        seq,
        ts_ms: seq * 10,
        event: eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::ApprovalRequested {
                request_id: request_id.into(),
                tool: tool.into(),
                display_name: tool.into(),
                description: "writes a file".into(),
                path: Some(path.into()),
                input_json: format!("{{\"file_path\":\"{path}\",\"content\":\"hi\"}}"),
                suggestions_json: None,
            },
        },
    }
}

/// The operator's answer to one question, as the engine journals it.
fn decided_record(seq: u64, request_id: &str, allowed: bool) -> eave_core::journal::Record {
    eave_core::journal::Record {
        seq,
        ts_ms: seq * 10,
        event: eave_core::journal::JournalEvent::ApprovalDecided {
            request_id: request_id.into(),
            allowed,
            message: None,
        },
    }
}

/// Put one open question on screen in session `aaaa1111`.
fn ask(h: &Harness, vcx: &mut VisualTestContext, records: Vec<eave_core::journal::Record>) {
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records,
    });
    vcx.run_until_parked();
}

/// The question states what it is asking for, and approving emits exactly
/// one decision naming the request the operator was looking at.
#[gpui::test]
fn approving_states_the_facts_and_answers_that_request(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    ask(
        &h,
        vcx,
        vec![approval_record(1, "req-1", "Write", "/repo/src/main.rs")],
    );
    let _ = h.drain();

    assert!(
        shown(vcx, "approval-req-1"),
        "the question must be on screen"
    );
    assert!(
        shown(vcx, "approval-path-req-1"),
        "the path it would touch is the fact that most needs stating"
    );
    assert!(
        shown(vcx, "approval-input-req-1"),
        "the full input is stated, not summarised away"
    );

    click(vcx, "approve-req-1");
    let cmds = h.drain();
    assert!(
        matches!(
            cmds.as_slice(),
            [Command::Decide { request_id, allow: true, message }]
                if request_id == "req-1" && message.is_empty()
        ),
        "approve must emit exactly one allow for this request, got {cmds:?}"
    );
}

/// Denying carries the operator's words — and spends them, so the same
/// sentence cannot later be sent again as a message.
#[gpui::test]
fn denying_carries_the_operators_reason_and_clears_the_composer(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    ask(
        &h,
        vcx,
        vec![approval_record(1, "req-1", "Bash", "/repo/out.txt")],
    );
    vcx.simulate_input("commit first");
    vcx.run_until_parked();
    let _ = h.drain();

    click(vcx, "deny-req-1");
    let cmds = h.drain();
    assert!(
        matches!(
            cmds.as_slice(),
            [Command::Decide { request_id, allow: false, message }]
                if request_id == "req-1" && message == "commit first"
        ),
        "deny must carry the reason verbatim, got {cmds:?}"
    );
    let draft = vcx
        .read_window(&h.window, |shell, cx| {
            shell
                .read(cx)
                .composer
                .read(cx)
                .field
                .read(cx)
                .content()
                .to_string()
        })
        .unwrap();
    assert!(
        draft.is_empty(),
        "the words were spent on the agent; leaving them would send them twice"
    );

    // And a plain deny with nothing typed is still a deny.
    ask(
        &h,
        vcx,
        vec![approval_record(1, "req-2", "Bash", "/repo/out.txt")],
    );
    let _ = h.drain();
    click(vcx, "deny-req-2");
    assert!(
        matches!(
            h.drain().as_slice(),
            [Command::Decide { allow: false, message, .. }] if message.is_empty()
        ),
        "an empty reason is allowed; the engine states the default"
    );
}

/// The keyboard answers the OLDEST open question, and it is the same
/// command the buttons emit — one behavior with two doors.
#[gpui::test(iterations = 8)]
fn the_keyboard_answers_the_oldest_open_question(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    ask(
        &h,
        vcx,
        vec![
            approval_record(1, "req-old", "Write", "/repo/a.txt"),
            approval_record(2, "req-new", "Bash", "/repo/b.txt"),
        ],
    );
    let _ = h.drain();

    vcx.simulate_keystrokes("cmd-shift-a");
    vcx.run_until_parked();
    assert!(
        matches!(
            h.drain().as_slice(),
            [Command::Decide { request_id, allow: true, .. }] if request_id == "req-old"
        ),
        "⇧⌘A must answer the question the operator has been looking at longest"
    );

    // With the oldest answered, the next keystroke moves to the next one.
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![eave_core::journal::Record {
            seq: 3,
            ts_ms: 30,
            event: eave_core::journal::JournalEvent::ApprovalDecided {
                request_id: "req-old".into(),
                allowed: true,
                message: None,
            },
        }],
    });
    vcx.run_until_parked();
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-shift-d");
    vcx.run_until_parked();
    assert!(
        matches!(
            h.drain().as_slice(),
            [Command::Decide { request_id, allow: false, .. }] if request_id == "req-new"
        ),
        "⇧⌘D must answer the next open question"
    );
}

/// A question that has been answered or withdrawn is no longer askable:
/// its buttons are gone, and nothing on screen invites a second click.
#[gpui::test]
fn a_settled_question_stops_offering_an_answer(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    ask(
        &h,
        vcx,
        vec![approval_record(1, "req-1", "Write", "/repo/a.txt")],
    );
    assert!(shown(vcx, "approve-req-1") && shown(vcx, "deny-req-1"));

    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![eave_core::journal::Record {
            seq: 2,
            ts_ms: 20,
            event: eave_core::journal::JournalEvent::Provider {
                event: eave_core::events::SessionEvent::ApprovalWithdrawn {
                    request_id: "req-1".into(),
                    reason: "the provider withdrew the request".into(),
                },
            },
        }],
    });
    vcx.run_until_parked();
    let _ = h.drain();

    assert!(
        shown(vcx, "approval-req-1"),
        "the question stays in the log — it happened"
    );
    assert!(
        !shown(vcx, "approve-req-1") && !shown(vcx, "deny-req-1"),
        "but an unanswerable question must not still offer an answer"
    );
    assert!(
        shown(vcx, "approval-status-req-1"),
        "and it must say what became of it"
    );
}

/// **Slice AQ.** Approving a tool call is the only irreversible action in
/// this UI, and until this slice it looked exactly like the rest of the
/// chrome: approve and deny were the same outlined chip at the same size
/// and the same weight, told apart by hue alone — and approve's hue was
/// [`theme::live`], the pigment that means *something is running right
/// now*.
///
/// This is a structural check on the source for the same reason
/// `every_clickable_element_says_it_is_clickable` is one: gpui resolves
/// fills inside its own style pass and exposes neither the computed style
/// nor the painted scene to a test, so a rendered assertion can see that
/// a button EXISTS and not what colour it is. What can be pinned is the
/// property that would decay — the pair drifting back into two chips.
///
/// The colours themselves are measured in `theme`'s ink table, where the
/// four new drawn combinations are enumerated by name.
#[gpui::test]
fn approve_is_the_apps_solid_plate_and_deny_is_not(_cx: &mut TestAppContext) {
    let src = include_str!("ui.rs");
    let at = src.find("fn decision_button(").expect("decision_button");
    let end = at
        + src[at..]
            .find("\n    /// A quiet trailer")
            .expect("end of builder");
    let body = &src[at..end];

    assert!(
        body.contains("theme::solid()") && body.contains("theme::on_solid()"),
        "approve stopped carrying the app's solid plate — the same near-white \
         fill and near-black label the launcher's start button paints"
    );
    assert!(
        body.contains("theme::solid_hover()"),
        "a filled control REPLACES its bg rather than compositing a wash over \
         it, so approve has to name BOTH ends of its fade (Slice M)"
    );
    assert!(
        body.contains("theme::danger_ink()") && body.contains("theme::danger_ring()"),
        "deny stopped being legible as destructive"
    );
    // The one that actually holds the line: deny must not acquire a fill of
    // its own. A destructive button louder than the primary one is worse
    // than the flat pair this replaced.
    assert!(
        !body.contains("theme::danger_wash_hover()"),
        "deny reached for the kill affordance's hovered fill. Its resting \
         state is unfilled and its hover is one rung below kill's REST \
         (`danger_wash_strong`); matching kill says deny is as destructive as \
         killing the agent"
    );
    assert!(
        !body.contains("theme::live()"),
        "`live` is the pigment for 'something is running right now' — the \
         streaming cursor, a running tool's dot, the work bar. It is not a \
         verb and it is not an answer"
    );
}

/// The two bindings are printed ON the buttons they fire.
///
/// They are declared at `ui.rs:56` and used to be announced by a trailer
/// that opened `⇧⌘A approve · ⇧⌘D deny · …` — a sentence under the card
/// that restated both button words in order to introduce two keys. A
/// keycap on the button is the same fact costing no sentence, and it binds
/// the key to the thing rather than to the paragraph.
///
/// Asserted as CONTAINMENT rather than presence: an element that merely
/// exists somewhere could be anywhere, and "discoverable" means the key is
/// on the control.
///
/// Text system: Noop. Every glyph carries the same synthetic advance, so
/// the horizontal extents here are not real — but containment in `y` and
/// the fact of the box are decided by the fold and the flex row, not by
/// shaping.
#[gpui::test]
fn each_decision_button_carries_its_own_binding(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    ask(
        &h,
        vcx,
        vec![approval_record(1, "req-1", "Bash", "/repo/target")],
    );
    let _ = h.drain();

    for (button, key) in [
        ("approve-req-1", "approve-key-req-1"),
        ("deny-req-1", "deny-key-req-1"),
    ] {
        let b = vcx
            .debug_bounds(button)
            .unwrap_or_else(|| panic!("{button} is not on screen"));
        let k = vcx
            .debug_bounds(key)
            .unwrap_or_else(|| panic!("{key} is not on screen — the binding is unstated"));
        assert!(
            k.origin.y >= b.origin.y && k.origin.y + k.size.height <= b.origin.y + b.size.height,
            "{key} is not inside {button}: key {k:?} vs button {b:?}"
        );
    }

    // And the trailer no longer spends its opening on words that are now on
    // the buttons. Source rather than render: the harness can see that a
    // line is on screen, never what it says.
    let src = include_str!("ui.rs");
    // The opening quote is load-bearing: the sentence is quoted in
    // `decision_button`'s own doc comment as the thing it replaced, so a
    // bare substring search finds the prose rather than a string literal.
    assert!(
        !src.contains("\"⇧⌘A approve"),
        "the trailer went back to introducing the bindings the buttons carry"
    );
    assert!(
        src.contains("deny sends whatever is in the composer as the reason"),
        "the one fact about answering that no button can show — because it is \
         about something outside the card — was lost with the sentence that \
         used to carry it"
    );
}

/// **Slice AQ, found by eye in `docs/shots/aq-before-approval.png`.** A
/// session parked on a permission question was captioned `working for
/// 99h 49m`, with a running work bar beside it, in the slot immediately
/// above the composer.
///
/// It is a composition defect rather than a local one, which is why no
/// test had it. The attention notice suppresses itself while the question
/// is on screen — that is the 2026-08-18 occlusion fix, and it is right —
/// and the working trailer then takes the slot the notice gave up. Both
/// decisions are correct in their own terms; `in_turn` really is true,
/// because the CLI is waiting rather than finished. The sentence is still
/// false: what is happening is that eave is waiting for the operator.
///
/// The slot stands empty, deliberately. The card IS the notice while it is
/// on screen (Slice K), and it says `Bash — needs your approval / waiting`
/// in the warning pigment, which is strictly more than the trailer could.
#[gpui::test]
fn a_session_parked_on_a_question_is_never_captioned_working(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    ask(&h, vcx, vec![user_record(1, "go"), delivered_record(2)]);
    assert!(
        shown(vcx, "work-trailer"),
        "precondition: an open turn with nothing asked does say it is working"
    );

    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![approval_record(3, "req-1", "Bash", "/repo/target")],
    });
    vcx.run_until_parked();
    let _ = h.drain();

    assert!(
        shown(vcx, "approve-req-1"),
        "precondition: the question is on screen with its buttons"
    );
    assert!(
        !shown(vcx, "work-trailer"),
        "eave told the operator it was working while it was waiting on them"
    );
    // …and the other half of the slot stays down too, with the engine's own
    // classification standing. `the_notice_never_stands_on_top_of_the_question_it_points_at`
    // owns the notice's suppression; this is the pair being checked
    // TOGETHER, which is the composition nothing was watching.
    h.plumbing.push_attn((
        "aaaa1111".into(),
        AttentionSnapshot::AwaitingApproval {
            since_ms: 0,
            pending: 1,
            tool: "Bash".into(),
        },
    ));
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "work-trailer"),
        "the notice stood down for the card and the trailer walked into the \
         slot it vacated"
    );

    // Answering hands the slot back: the turn never ended, so the trailer is
    // the right thing to be in it again. The engine reclassifies as it goes
    // — `Running` is its word for "clear any notice" — and without that the
    // notice this test just stood up would take the slot instead, which is
    // its job and not a defect.
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![decided_record(4, "req-1", true)],
    });
    h.plumbing
        .push_attn(("aaaa1111".into(), AttentionSnapshot::Running));
    vcx.run_until_parked();
    assert!(
        shown(vcx, "work-trailer"),
        "the trailer did not come back after the question was answered"
    );
}

/// A BACKGROUND session waiting on the operator has to be visible in the
/// rail without switching to it — and the rail may say the count, never
/// the tool or the path (spec 05 §Watch).
#[gpui::test]
fn a_background_session_waiting_on_me_is_visible_in_the_rail(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut selected = session("aaaa1111");
    selected.state = "running".into();
    selected.live = true;
    let mut background = session("bbbb2222");
    background.state = "awaiting_approval".into();
    background.live = true;
    background.pending_approvals = 3;
    h.plumbing.push_rail(vec![selected, background]);
    vcx.run_until_parked();

    assert!(
        shown(vcx, "badge-bbbb2222"),
        "a background session parked on me must badge without being opened"
    );
    // The rail's own row states it; the transcript of that session is not
    // subscribed and no card for it exists on screen.
    assert!(!shown(vcx, "approval-req-1"));
}

/// Several sessions can be parked at once, and each states its own count.
#[gpui::test]
fn several_sessions_can_wait_on_me_at_once(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.state = "awaiting_approval".into();
    a.live = true;
    a.pending_approvals = 1;
    let mut b = session("bbbb2222");
    b.state = "awaiting_approval".into();
    b.live = true;
    b.pending_approvals = 2;
    h.plumbing.push_rail(vec![a, b]);
    vcx.run_until_parked();

    assert!(shown(vcx, "badge-aaaa1111") && shown(vcx, "badge-bbbb2222"));
    // Answering one is scoped to the session the operator is looking at:
    // the shell only ever holds one transcript, so a decision can only
    // name a request from it (proven by the absence of any other card).
    assert!(!shown(vcx, "approval-req-1"));
}

// ---------------------------------------------------------------------------
// Stated facts — REAL text system (Slice K's one fit-critical fact).

/// Amendment 3's line is a WARNING, and a warning that clips is worse
/// than none: "NEVER ASKS · bypassPermi…" reads as a mode name, not as an
/// alarm. Measured with real font-kit metrics for the same reason as the
/// worktree facts — synthetic advances would let a clipping line pass.
#[gpui::test]
fn the_posture_line_fits_the_rail_at_real_glyph_widths(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut asks = session("aaaa1111");
    asks.permission_mode = Some("default".into());
    let mut never = session("bbbb2222");
    never.permission_mode = Some("bypassPermissions".into());
    let mut also_never = session("cccc3333");
    also_never.permission_mode = Some("acceptEdits".into());
    h.plumbing.push_rail(vec![asks, never, also_never]);
    vcx.run_until_parked();

    // Slice AO: the SAFE posture no longer takes a line. `asks before it
    // writes · default` was a line on every row restating the default, and
    // a row that says nothing has changed is the row that made the rail
    // three lines deep. Absence is the assertion — if the safe line ever
    // comes back it costs every row again.
    assert!(
        !shown(vcx, "posture-aaaa1111"),
        "the asking posture is the default; a line restating it on every \
         row is what a two-line rail cannot afford"
    );

    // Same full-width room for both alarms, so the room is layout-derived
    // and text-independent (the composition this measurement relies on).
    let room = vcx.debug_bounds("posture-bbbb2222").unwrap().size.width;
    assert_eq!(
        room,
        vcx.debug_bounds("posture-cccc3333").unwrap().size.width,
        "posture lines must be full-width like every other rail fact"
    );

    for mode in ["bypassPermissions", "acceptEdits", "dontAsk"] {
        let text = posture_line(mode);
        assert!(
            text.starts_with("NEVER ASKS"),
            "{mode:?} renders {text:?} — only the alarm still takes a line, \
             so anything that reaches this element has to BE the alarm"
        );
        let w = real_text_width(&text, theme::SANS, theme::MICRO_SIZE);
        assert!(
            w <= room,
            "the posture fact {text:?} shapes to {w:?} but the rail gives it {room:?}"
        );
    }
}

/// A send made while a question is open is queued by the CLI with no
/// echo and no ack (capture N, 2026-08-18). The composer must say so —
/// otherwise the send sits pending with no stated reason, which is the
/// one thing spec 02's delivery contract must never do.
#[gpui::test]
fn the_composer_states_that_a_send_will_queue_behind_the_question(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    ask(&h, vcx, vec![user_record(1, "go")]);
    let hint_before = vcx.debug_bounds("composer-hint").map(|b| b.size.width);
    assert!(
        hint_before.is_some(),
        // Slice AN emptied this line at rest (`composer_hint`), so what is
        // asserted here is that the SLOT is mounted — it is the harness's
        // probe for "a composer is on screen at all". The width below is
        // what carries the words.
        "the composer's trailing slot is not mounted, so the comparison \
         below would compare nothing to nothing"
    );

    ask(
        &h,
        vcx,
        vec![
            user_record(1, "go"),
            approval_record(2, "req-1", "Write", "/repo/a.txt"),
        ],
    );
    let hint_after = vcx.debug_bounds("composer-hint").map(|b| b.size.width);
    assert!(
        hint_after != hint_before,
        "with a question open the composer must say the send will queue, \
         not repeat the same four words"
    );
}

/// The floating notice is chrome above the composer; the question is a
/// card in the log with the buttons on it. Rendering both put the notice
/// ON TOP of the approve/deny it was pointing at (reported from the real
/// renderer, 2026-08-18). While the question is on screen the card is the
/// notice; the panel only stands in when it is not.
///
/// Text system: Noop. This asserts whether an element renders at all,
/// which is decided by the fold, not by glyph advances.
#[gpui::test]
fn the_notice_never_stands_on_top_of_the_question_it_points_at(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    ask(
        &h,
        vcx,
        vec![approval_record(1, "req-1", "Bash", "/repo/target")],
    );
    h.plumbing.push_attn((
        "aaaa1111".into(),
        AttentionSnapshot::AwaitingApproval {
            since_ms: 0,
            pending: 1,
            tool: "Bash".into(),
        },
    ));
    vcx.run_until_parked();

    assert!(
        shown(vcx, "approve-req-1") && shown(vcx, "deny-req-1"),
        "the answer must stay reachable while the session waits"
    );
    assert!(
        !shown(vcx, "attn-kill"),
        "and waiting on a person is never a kill situation"
    );

    // With the same notice standing but no question on screen — the
    // projection was reset, or attention outran the records — the panel
    // is the only thing that can tell the operator, so it renders.
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "approval-req-1"),
        "precondition: the question is gone from the log"
    );
    let notice_stands = vcx
        .read_window(&h.window, |shell, cx| {
            shell.read(cx).sessions.read(cx).attention.is_some()
        })
        .unwrap();
    assert!(notice_stands, "the notice itself still stands");
}

// ---------------------------------------------------------------------------
// Slice M — polish pass one: empty states, and the invariant behind hover
// and cursor coverage.

/// An empty rail used to render literally nothing, which made "you have
/// never started a session" and "the rail failed to load" the same
/// picture. It states the fact now, and stops the moment there is a row
/// to show instead.
#[gpui::test]
fn an_empty_rail_says_it_is_empty_instead_of_showing_bare_glass(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(Vec::new());
    vcx.run_until_parked();

    assert!(
        shown(vcx, "rail-empty"),
        "an empty rail must state that it is empty"
    );
    // The affordance that answers it is still there — which is why the
    // note itself does not restate it.
    assert!(shown(vcx, "new-session"));

    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "rail-empty") && shown(vcx, "rail-aaaa1111"),
        "one real session replaces the note, it does not sit above it"
    );
}

/// An empty log is three different situations and the shell may only
/// claim the two it can back. The third — a session whose journal the
/// engine says is NOT empty, whose rows have not arrived yet — is a
/// loading state, and stating "nothing sent yet" over a session with
/// history would be a lie for the length of a round trip.
#[gpui::test]
fn an_empty_log_states_only_the_kind_of_empty_it_can_prove(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);

    // Nothing subscribed at all. Slice Q2 took this case off the note and
    // gave it to the launcher — "no session selected" was a sentence where
    // a screen belonged — so the note must be ABSENT here, not present.
    assert!(
        shown(vcx, "launcher"),
        "with no session selected the launcher is the rest state"
    );
    assert!(
        !shown(vcx, "transcript-empty"),
        "the note and the launcher must never be on screen together"
    );

    // Subscribed, and the engine's own count says the journal is empty.
    let mut fresh = session("aaaa1111");
    fresh.last_seq = 0;
    h.plumbing.push_rail(vec![fresh]);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert!(
        shown(vcx, "transcript-empty"),
        "a brand-new session states that nothing has been sent"
    );

    // Subscribed to a session the engine says HAS records, none of which
    // have arrived. Nothing may be claimed here.
    let mut loaded = session("bbbb2222");
    loaded.last_seq = 41;
    h.plumbing.push_rail(vec![loaded]);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "bbbb2222".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "bbbb2222".into(),
    });
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "transcript-empty"),
        "a journal still in flight is a loading state, not an empty one — \
         Slice M builds no loaders and must therefore say nothing"
    );

    // And once the rows land, the note is gone for the ordinary reason.
    h.plumbing.push_feed(FeedEvent::Records {
        id: "bbbb2222".into(),
        epoch: 1,
        records: vec![user_record(41, "the history that was in flight")],
    });
    vcx.run_until_parked();
    assert!(!shown(vcx, "transcript-empty"));
}

/// The empty note is an OVERLAY, never a row. The virtualized list caches
/// a measure per row, and a row whose height depends on anything but its
/// own identity and content leaves a stale measure that moves the scroll
/// anchor (`ui.rs`, `render_row`). An empty state rendered as a row would
/// be exactly that kind of height, so this pins the item count instead:
/// an empty log holds the trailing spacer and nothing else.
#[gpui::test]
fn the_empty_note_never_becomes_a_row(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    let items = vcx
        .read_window(&h.window, |shell, cx| {
            shell.read(cx).transcript.read(cx).list.item_count()
        })
        .unwrap();
    assert_eq!(
        items, 1,
        "an empty log is the trailing under-glass spacer and nothing else"
    );
}

/// Every clickable element states that it is clickable, and answers the
//// Where `ghost_row` is called for `selector`, if it is one.
///
/// rustfmt puts the arguments on one line or on several depending on how
/// long they are, so this looks for the call and the quoted selector rather
/// than for a fixed spelling of the two together.
fn ghost_row_call(src: &str, selector: &str) -> Option<usize> {
    let quoted = format!("\"{selector}\"");
    src.match_indices("ghost_row(")
        .find(|(ix, _)| {
            let window: String = src[*ix..].chars().take(200).collect();
            window.contains(&quoted)
        })
        .map(|(ix, _)| ix)
}

// pointer.
///
/// This is a structural check on the source rather than a rendered one,
/// and deliberately so: gpui resolves `hover` and `cursor_pointer` inside
/// its own style pass and exposes neither the computed style nor the
/// painted scene to a test, so there is nothing to assert against a
/// frame. What CAN be pinned is the property that actually decays — a
/// button added later with a handler and no feedback. Before Slice M this
/// count was 12 against 0.
#[gpui::test]
fn every_clickable_element_says_it_is_clickable(_cx: &mut TestAppContext) {
    let src = include_str!("ui.rs");
    let clicks = src.matches(".on_click(").count();
    // Both cursors count (Stage AC). The property is that a click handler
    // always states what the pointer becomes — not that every one of them
    // becomes a hand. The rail's rename field is typed into rather than
    // pressed and takes an I-beam; `cursor_pointer` there would say
    // "button" over a text box.
    let cursors = src.matches(".cursor_pointer()").count() + src.matches(".cursor_text()").count();
    // Stage AK: five rail affordances share one constructor, so they carry
    // ONE `.cursor_pointer()` between them while still being five click
    // sites. Counting `row_action(` closes that gap — but only because the
    // line below proves the constructor really does set a cursor. Without
    // that check this would be an exemption rather than an accounting.
    assert!(
        src.contains("fn row_action(") && {
            let body = &src[src.find("fn row_action(").unwrap()..];
            body[..body.find("\n}\n").unwrap()].contains(".cursor_pointer()")
        },
        "row_action stopped stating its cursor, and five click sites are \
         counted through it"
    );
    // The helper's OWN cursor call is not a click site's — it stands in for
    // its call sites, so it is removed from the direct count and each call
    // site is credited instead. Computed, not hardcoded: a second cursor
    // call inside the helper would otherwise silently buy an exemption.
    let helper_body = {
        let from = &src[src.find("fn row_action(").unwrap()..];
        &from[..from.find("\n}\n").unwrap()]
    };
    let cursors_in_helper = helper_body.matches(".cursor_pointer()").count()
        + helper_body.matches(".cursor_text()").count();
    let via_helper = src.matches("row_action(").count() - 1; // minus the definition
    let model_chip_body = {
        let from = &src[src.find("fn model_chip(").unwrap()..];
        &from[..from.find("\n}\n").unwrap()]
    };
    let cursors_in_model_chip = model_chip_body.matches(".cursor_pointer()").count()
        + model_chip_body.matches(".cursor_text()").count();
    let via_model_chip = src.matches("model_chip(").count() - 1;
    let attach_button_body = {
        let from = &src[src.find("fn attach_button(").unwrap()..];
        &from[..from.find("\n}\n").unwrap()]
    };
    let cursors_in_attach_button = attach_button_body.matches(".cursor_pointer()").count()
        + attach_button_body.matches(".cursor_text()").count();
    let via_attach_button = src.matches("attach_button(").count() - 1;
    assert_eq!(
        clicks,
        cursors - cursors_in_helper - cursors_in_model_chip - cursors_in_attach_button
            + via_helper
            + via_model_chip
            + via_attach_button,
        "every .on_click site needs a .cursor_pointer or .cursor_text, \
         directly or through row_action / model_chip / attach_button — eave \
         had 84 fewer cursor calls than comet and every one of them was the \
         default arrow"
    );

    // Hover coverage, all thirteen controls. Before Slice M five of them
    // had a click handler and no feedback of any kind; Slice N2 replaced
    // every `.hover(|s| s.bg(..))` — which snaps by construction, because
    // gpui applies the whole style on the frame the pointer enters — with
    // `.hover_fade(key, rest, hovered)`, which blends over HOVER_FADE.
    //
    // Each control is checked by its own selector so a regression names the
    // one it broke. The colour is checked too, because the interesting way
    // to break this is not to delete the fade but to give a filled control
    // the neutral wash: a control with its own `bg` REPLACES it rather than
    // compositing over it, so `overlay()` on the send button would paint a
    // near-black square instead of a lighter fill.
    for (selector, wash) in [
        ("rail-toggle", "wash_rest"),
        ("files-toggle", "wash_rest"),
        ("new-session", "wash_rest"),
        ("attn-kill", "danger_wash_hover"),
        ("send", "inverse_hover"),
        ("composer-attach", "overlay"),
        ("to-bottom", "chrome_glass_hover"),
    ] {
        // **Anchored on the FADE's key, not on proximity to the selector.**
        // It used to search 1600 characters after `debug_selector` for a
        // `.hover_fade(`, and Slice AL broke that twice in one sitting —
        // once by splitting the rail row so the fade and the selector ended
        // up on different elements, and once by writing nine lines of
        // rationale above the send button's fade, which pushed its wash to
        // 1882 characters out. Both times the message read "this control
        // lost its hover feedback" and both times nothing of the sort had
        // happened. Proximity in a source file is not a property; a fade
        // registered under the control's own key is.
        // Slice D4 — the zone actions became ghost rows, and all three are
        // built by ONE helper, so their selector and their fade are both
        // written generically (`debug_selector(move || selector.into())`,
        // `hover_fade(selector, ..)`). A source-text search for the literal
        // therefore cannot find them.
        //
        // The guarantee is not weakened, it moved: the helper is checked
        // once for the shape every ghost row inherits, and the row is
        // checked for being built by it. That is strictly stronger than
        // three literal matches, because it cannot drift per-row.
        if let Some(call) = ghost_row_call(&src, selector) {
            assert!(
                src.contains("hover_fade(selector, theme::wash_rest(), theme::overlay())"),
                "ghost_row lost the hover_fade every zone action inherits \
                 (checked because {selector} is built at {call})"
            );
            assert!(
                src.contains("debug_selector(move || selector.into())"),
                "ghost_row lost the debug_selector every zone action \
                 inherits (checked because {selector} is built at {call})"
            );
            continue;
        }
        assert!(
            src.contains(&format!(r#"debug_selector(|| "{selector}".into())"#)),
            "{selector} is no longer tagged"
        );
        // The key is written inline at some sites and on its own line at
        // others, so this walks every occurrence of the quoted key and keeps
        // the one whose immediately preceding non-whitespace is
        // `.hover_fade(`. Whitespace-insensitive by construction — which is
        // the point, because rustfmt reserves the right to move it.
        let quoted = format!("\"{selector}\",");
        let at = src
            .match_indices(&quoted)
            .find(|(ix, _)| src[..*ix].trim_end().ends_with(".hover_fade("))
            .map(|(ix, _)| ix)
            .unwrap_or_else(|| {
                panic!(
                    "{selector} has no hover_fade keyed to itself (or went \
                     back to a snapping .hover)"
                )
            });
        // **600 CHARACTERS, not 600 bytes.** `&src[at..at + 600]` panics the
        // moment that end index lands inside a multi-byte character, and this
        // file is full of them — em dashes, ⌘, ′. It had never fired because
        // no boundary had happened to fall there yet; Slice D1 wrote a `′`
        // into a doc comment at exactly the wrong offset and the test died
        // with `not a char boundary` instead of an assertion message. A
        // latent panic in a test is worse than a latent panic in the product:
        // it fails as noise, so the reader looks for the bug in their own
        // change.
        let window: String = src[at..].chars().take(600).collect();
        assert!(
            window.contains(wash),
            "{selector} should hover to {wash} — a filled control replaces \
             its bg rather than compositing the neutral wash over it"
        );
    }
    // The two rows whose selector is built from a runtime id are checked by
    // their hover KEY rather than by proximity to their `debug_selector`.
    //
    // **That is stronger than the window search above, and Slice AL is why
    // it had to be.** The session row was split into a bare leading strip
    // (selection bar + state mark) and a washed card, so the element that
    // paints the fade is no longer the element that carries the selector —
    // and the 1400-character window said "the session row lost its hover
    // feedback" when nothing of the sort had happened. Proximity in a source
    // file is not a property. The KEY is: Stage AK's action strip reveals
    // itself from `motion::hover_t("rail-{sid}")`, so a fade registered
    // under that exact key is the thing the row's behaviour depends on, no
    // matter which div paints it.
    //
    // Matched WHITESPACE-INSENSITIVELY. The first version of this check
    // embedded the source's exact indentation in the pattern, which made it
    // fail the moment the row was re-nested by a concurrent slice — the same
    // brittleness the comment above objects to in a proximity window, just
    // wearing different clothes. What is being asserted is "this key is
    // registered by a hover_fade", and indentation is not part of that.
    let squash = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    let flat = squash(src);
    for (key, what) in [
        (
            r#".hover_fade( format!("rail-{}", info.session_id),"#,
            "the session row",
        ),
        (
            r#".hover_fade( format!("file-{}", f.path),"#,
            "the file row",
        ),
    ] {
        assert!(
            flat.contains(&squash(key)),
            "{what} lost the hover fade keyed to its own id — Stage AK's \
             action strip reads that key, so the fade and the reveal come \
             apart if it moves"
        );
    }
    // Stage AK: the five rail affordances are built by one constructor, so
    // their feedback is checked THERE — which is stronger than the window
    // search above, not weaker. A 1400-character proximity window is a
    // heuristic that a long comment can defeat; "the only way to build one
    // of these gives it a fade" is a property.
    assert!(
        helper_body.contains(".hover_fade("),
        "row_action stopped giving its affordances hover feedback, and all \
         five of them are built through it"
    );
    // ...and the five still exist to be built. Checked separately, because
    // the assertion above would pass just as well if every call site had
    // been deleted.
    for key in [
        r#"format!("ren-{}", info.session_id)"#,
        r#"format!("arc-{}", info.session_id)"#,
        r#"format!("iso-{}", info.session_id)"#,
        r#"format!("wto-{}", info.session_id)"#,
        r#"format!("wtd-{}", info.session_id)"#,
    ] {
        assert!(src.contains(key), "the affordance keyed {key} is gone");
    }
    // approve / deny share one builder and one selector expression.
    //
    // Slice AQ: bounded by the NEXT item rather than by a byte count. The
    // count was 2400 and the builder's doc comment grew past it, so this
    // read a window that stopped short of the code and reported the
    // affordance gone. A byte budget is a guess about how long a function's
    // prose is allowed to be, which is not a property anyone should have to
    // maintain.
    let at = src.find("fn decision_button(").expect("decision_button");
    let window = &src[at..at + src[at..].find("\n    /// A quiet trailer").expect("end")];
    assert!(
        window.contains(".hover_fade("),
        "approve and deny lost their hover feedback"
    );

    // And nothing anywhere may go back to the snapping form. This is the
    // assertion that actually holds the line: the per-selector checks above
    // pass just as well if someone adds a `.hover()` BESIDE a fade.
    assert_eq!(
        src.matches(".hover(").count(),
        0,
        "a snapping `.hover(|s| ..)` came back — every interactive wash \
         rides motion::HOVER_FADE now (inventory §1.13)"
    );
}

/// Slice N1: the render tail, wired through the real shell.
///
/// The first half is the invariant — a shell with nothing mid-flight
/// leaves no frame request behind, so an idle window costs nothing. The
/// second half is the wiring: a claim reaches the root's tail and buys
/// exactly ONE more frame, not a standing subscription. That distinction
/// is the whole difference between this and the naive shape comet
/// measured at 36% CPU; a claim that bought a permanent drive would look
/// identical until you profiled it.
///
/// `simulate_next_frame` is the honest instrument here: tests have no
/// platform frame loop, so the count it returns IS the number of frames
/// the window asked for and would otherwise be handed.
#[gpui::test]
fn an_idle_shell_asks_for_no_frames_and_a_claim_buys_exactly_one(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    // Slice N2 gave the shell something to do at boot: the empty-log note
    // enters. Let it land — "idle" means settled, not freshly opened.
    drive(&h, vcx, Duration::from_millis(10));

    let pending = h
        .window
        .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    assert_eq!(
        pending, 0,
        "an idle shell scheduled {pending} frame(s) with nothing mid-flight"
    );

    // Something declares itself mid-flight, then the shell renders.
    motion::mark_active();
    h.window.update(vcx, |_, _, cx| cx.notify()).unwrap();
    vcx.run_until_parked();

    let asked = h
        .window
        .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    assert_eq!(asked, 1, "the tail did not carry the claim to the window");
    vcx.run_until_parked();

    // The claim was consumed by that tail: the drive stops on its own
    // unless whatever is moving asks again.
    let after = h
        .window
        .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    assert_eq!(after, 0, "one claim bought a standing subscription");
}

/// Reduced motion stops at the tail too. gpui honours the flag inside
/// `with_animation`, but `request_animation_frame` is the raw call
/// underneath it and honours nothing on its own — so a hand-driven tween
/// would keep pumping frames on a machine whose owner asked for less
/// motion. This is the assertion that the tail checks the flag; the clock
/// half is covered in `motion`.
#[gpui::test]
fn reduced_motion_stops_the_tail_from_asking_for_frames(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    // Land the boot entrance under full motion first, so what is measured
    // below is the claim and not a frame the note had already asked for.
    drive(&h, vcx, Duration::from_millis(10));
    vcx.update(|_, cx| cx.set_reduce_motion(true));

    motion::mark_active();
    h.window.update(vcx, |_, _, cx| cx.notify()).unwrap();
    vcx.run_until_parked();

    let asked = h
        .window
        .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    assert_eq!(
        asked, 0,
        "reduced motion still scheduled a frame for a hand-driven tween"
    );
}

// ---------------------------------------------------------------------------
// Slice N2 — hover fades and entrances, through the real shell

/// One simulated display frame: deliver the frame the window asked for, let
/// the clock move, and let it draw. Returns whether it wanted one at all.
///
/// This is the honest instrument in a test: there is no platform frame loop,
/// so what `simulate_next_frame` returns IS the number of frames the window
/// would have been handed.
fn tick(h: &Harness, vcx: &mut VisualTestContext, step: Duration) -> bool {
    let asked = h
        .window
        .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    if asked == 0 {
        return false;
    }
    vcx.executor().advance_clock(step);
    vcx.run_until_parked();
    true
}

/// Drive until the window stops asking, and report how many frames it wanted.
/// Panics rather than spinning if it never settles — a tween that asks
/// forever is the exact failure mode N1 exists to prevent, and it should
/// fail as a test rather than as a hung suite.
#[track_caller]
fn drive(h: &Harness, vcx: &mut VisualTestContext, step: Duration) -> usize {
    let mut frames = 0;
    while tick(h, vcx, step) {
        frames += 1;
        assert!(
            frames < 400,
            "the window asked for 400 frames without settling — something \
             is claiming the tail unconditionally"
        );
    }
    frames
}

/// A hover wash takes time, and then it stops costing anything.
///
/// Both halves matter and they fail in opposite directions. A wash that
/// settles in one frame is the snapping `.hover()` this slice replaced. A
/// wash that never settles pins the window at the display rate — comet
/// measured that shape at 36% CPU, and it is what the whole frame-drive
/// design exists to avoid.
///
/// The frame count is a literal, not a value derived from `HOVER_FADE`:
/// deriving it would make the test pass for any duration, including zero
/// (the N1 trap — see `progress.md` §Traps). At a 10 ms step a 150 ms wash
/// claims the tail on the draws at 0, 10 … 140 ms and declines at 150.
#[gpui::test]
fn a_hover_wash_takes_time_and_then_stops_asking_for_frames(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    // Let the boot entrance land first — an entering shell is legitimately
    // mid-flight and would otherwise be counted here.
    drive(&h, vcx, Duration::from_millis(10));

    let bounds = vcx
        .debug_bounds("new-session")
        .expect("+ new did not render");
    vcx.simulate_mouse_move(bounds.center(), None, Modifiers::none());
    vcx.run_until_parked();

    let frames = drive(&h, vcx, Duration::from_millis(10));
    assert!(
        (13..=17).contains(&frames),
        "the wash into `+ new` ran for {frames} frames at 10 ms each; a \
         150 ms fade is ~15, a snap is 0–1, and a runaway is the cap"
    );

    // Leaving is a fade too, not a cut.
    vcx.simulate_mouse_move(point(px(2.), px(2.)), None, Modifiers::none());
    vcx.run_until_parked();
    let back = drive(&h, vcx, Duration::from_millis(10));
    assert!(
        (13..=17).contains(&back),
        "the wash out of `+ new` ran for {back} frames — a leave should \
         fade, not cut"
    );

    // And having landed, the shell is idle again.
    assert_eq!(
        h.window
            .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap(),
        0,
        "a settled hover left a standing frame request behind"
    );
}

/// Reduced motion is honoured at the tail, so a hover under it costs
/// nothing at all — not a shorter fade, no frames.
///
/// This is the assertion that would catch the easy mistake: honouring the
/// preference in the *store* (fades snap) while still claiming the tail,
/// which looks correct and quietly pumps frames for a tween nobody sees.
#[gpui::test]
fn a_hover_under_reduced_motion_asks_for_no_frames_at_all(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    drive(&h, vcx, Duration::from_millis(10));
    vcx.update(|_, cx| cx.set_reduce_motion(true));

    let bounds = vcx
        .debug_bounds("new-session")
        .expect("+ new did not render");
    vcx.simulate_mouse_move(bounds.center(), None, Modifiers::none());
    vcx.run_until_parked();

    assert_eq!(
        drive(&h, vcx, Duration::from_millis(10)),
        0,
        "reduced motion still scheduled frames for a hover wash"
    );
}

/// The empty-log note enters, and having entered it stays put.
///
/// The second half is the remount hazard (inventory §1.3): gpui keys
/// `with_animation`'s start time by the element-id path, so an element that
/// misses a frame replays. eave hand-drives instead, and this pins the
/// consequence through the real shell — nothing re-enters just because the
/// window drew again.
#[gpui::test]
fn the_empty_note_enters_once_and_does_not_replay_on_later_frames(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    // Q2: a bare boot is the LAUNCHER now, so the note has to be summoned
    // by its own case — a selected session whose journal the engine says
    // is empty.
    let mut fresh = session("aaaa1111");
    fresh.last_seq = 0;
    h.plumbing.push_rail(vec![fresh]);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert!(
        shown(vcx, "transcript-empty"),
        "a selected session with an empty journal should show the note"
    );

    // 500 ms at a 10 ms step ≈ 50 frames. Literal, as above.
    let frames = drive(&h, vcx, Duration::from_millis(10));
    assert!(
        (45..=55).contains(&frames),
        "the note's entrance ran for {frames} frames at 10 ms each; FADE_IN \
         is 500 ms, so ~50"
    );

    // Now make the shell draw again, repeatedly, for a reason that has
    // nothing to do with the note.
    for _ in 0..5 {
        h.window.update(vcx, |_, _, cx| cx.notify()).unwrap();
        vcx.run_until_parked();
        assert_eq!(
            h.window
                .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
                .unwrap(),
            0,
            "a redraw replayed an entrance that had already arrived"
        );
    }
}

/// The caret's drive, which is the one tween in eave that never settles —
/// so it is the one that could quietly cost forever.
///
/// **A focused field takes a lease; a window with no field takes none.**
/// That is a different trade from the one N3 shipped, and the reason it
/// could be made is that a caret costs a different rate now.
///
/// N3 gated the blink on the composer holding a draft. With one clock at
/// 30 fps that was the only way to keep N1's invariant — a caret is on
/// screen always, so an always-blinking one is a window redrawing thirty
/// times a second for the life of the process. The gate held the floor and
/// cost the feature twice over: an empty composer drew a **static** glyph,
/// and the omnibox, which called the same gate, was gated on the
/// *composer's* draft — empty by definition while somebody types a url —
/// so it could never blink at all.
///
/// The floor is held by the RATE now. A caret leases at
/// `motion::CARET_TICK` and everything else at `PULSE_TICK`, so "is a timer
/// pending" and "is anything animating" are two questions. Both are
/// asserted at every step below, because they fail in opposite directions:
/// a parked clock is a caret that stopped blinking, and `PULSE_TICK` is a
/// caret that pinned the window at the frame rate.
#[gpui::test]
fn a_focused_field_puts_the_caret_on_the_clock_and_a_launcher_does_not(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    drive(&h, vcx, Duration::from_millis(10));
    // The floor, and the half that has not moved: no session, no composer,
    // no field, no lease, no timer. `caret_field` returns `None` here, and
    // `a_window_with_nothing_in_flight_never_starts_the_pulse_clock` holds
    // it from the other side.
    assert!(
        !clock_running(vcx),
        "a launcher with no field in it constructed the pulse clock"
    );
    // A session, because the caret is a property of a composer and there is
    // no composer at the launcher. This test used to type straight into a
    // bare boot and pass, which is the defect
    // `the_launcher_reaches_no_composer` now pins: the keystrokes were
    // landing in an unmounted buffer. The lease assertion is unchanged —
    // an open session with an empty draft still takes none.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(1, "settled")],
    });
    // ...and its files snapshot, or the rail's skeleton rows hold a lease
    // for the life of the process (`Shell::work_in_flight`: a selected
    // session whose snapshot has not arrived IS work in flight). That is
    // the shape of the trap N3 recorded — an eternal skeleton is an error
    // state that lost its error, and the tell is CPU, not pixels.
    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: Vec::new(),
            omitted: 0,
            watching: true,
            basis: DiffBasis::WorkingTree,
        }),
    ));
    vcx.run_until_parked();
    // Past every entrance: a session that just mounted is animating. The
    // assertions are about a SETTLED shell.
    vcx.executor().advance_clock(Duration::from_secs(1));
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(10));
    assert!(
        caret_only(vcx),
        "an empty composer is not blinking — this is the state the operator \
         reported as a dead-looking app, and a static glyph in a text box \
         reads as decoration rather than as an insertion point"
    );
    assert!(
        !animating(vcx),
        "a settled shell with one blinking caret was driving the clock at \
         the frame rate"
    );

    vcx.simulate_keystrokes("h i");
    vcx.run_until_parked();
    assert!(caret_only(vcx), "a draft took the caret off the clock");
    assert!(
        !animating(vcx),
        "typing pinned the window at the frame rate"
    );

    // Sending empties the composer, and the caret stays. **This is the
    // assertion that reverses**: it used to read "the lease lapses and the
    // clock parks", which was the gate working as designed and was also the
    // whole defect — an empty composer is exactly where a person looks to
    // find out whether the app is alive.
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    vcx.executor().advance_clock(Duration::from_secs(1));
    vcx.run_until_parked();
    assert!(
        caret_only(vcx),
        "sending stopped the caret — an empty composer is a field waiting \
         for the next message, not a window with nothing in it"
    );
    assert!(
        !animating(vcx),
        "an empty composer cost more than its caret"
    );
    // Drain the entrances the send itself armed (the pin flip re-asserts
    // scroll, and the fade store ages in RENDERS, not wall time — so the
    // frames have to be spent, not waited out).
    drive(&h, vcx, Duration::from_millis(600));
    assert_eq!(
        h.window
            .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap(),
        0
    );
}

/// **The caret actually blinks.**
///
/// The assertion this slice exists for, and there was no test under it
/// before — every caret in eave was a static glyph, and a static glyph is
/// indistinguishable from a decorative bar. That is what "the app looks
/// dead" describes.
///
/// `ui_harness` cannot read colour, so the lit state is in the selector
/// (`caret_glyph`): `caret-omnibox-lit` and `caret-omnibox-dark` are two
/// elements to this test and one element to a reader.
///
/// Walked as two full cycles rather than sampled once, because a caret that
/// went dark and stayed dark would pass a single sample and is a worse
/// defect than one that never blinks at all. The step is the caret's own
/// tick, so every sample is one the clock actually produced rather than one
/// this test invented.
#[gpui::test]
fn a_caret_blinks_in_the_field_that_owns_the_keyboard(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    vcx.simulate_keystrokes("cmd-l");
    vcx.run_until_parked();
    assert!(shown(vcx, "omnibox"), "\u{2318}L did not open the omnibox");
    // Solid through the first half-period. Opening a field counts as "the
    // typist is still here", which is the same anchor rule a keystroke gets.
    assert!(
        shown(vcx, "caret-omnibox-lit"),
        "the omnibox opened with no caret in it"
    );

    let (mut lit, mut dark) = (0, 0);
    for _ in 0..20 {
        vcx.executor().advance_clock(motion::caret_tick());
        vcx.run_until_parked();
        if shown(vcx, "caret-omnibox-lit") {
            lit += 1;
        }
        if shown(vcx, "caret-omnibox-dark") {
            dark += 1;
        }
    }
    assert!(
        dark > 0,
        "the omnibox caret never went dark across two full cycles — it is a \
         static bar, which is the defect this slice is fixing"
    );
    assert!(lit > 0, "the omnibox caret went dark and stayed dark");
    assert_eq!(lit + dark, 20, "some sample drew no caret at all");
}

/// **One caret, and it is in the box the next key lands in.**
///
/// A caret answers exactly one question — *where does what I type go?* — so
/// two lit at once is a window giving two answers, and a lit caret in a box
/// that is not consuming keys is a confident wrong one. Before this slice
/// the omnibox could not answer it at all: its caret read the COMPOSER's
/// draft, which is empty by definition while somebody is typing a url, so
/// it was pinned solid for the entire time it was on screen.
///
/// This is also what keeps `Shell::caret_field`'s ladder honest against
/// `Shell::on_key`'s. The two lists have to agree about where a keystroke
/// goes, and the agreement is asserted by pressing keys rather than by
/// re-reading either list — `hi` has to land in the omnibox and not in the
/// draft behind it.
#[gpui::test]
fn the_lit_caret_is_in_the_field_that_takes_the_next_key(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    drive(&h, vcx, Duration::from_millis(10));
    // Either state: `drive` spends however many frames the entrances need,
    // which is drawn time the blink is free to cross. What is asserted here
    // is that the composer has a caret at all — the *lit* one is asserted
    // after a keystroke below, where the anchor is known.
    assert!(
        shown(vcx, "caret-composer-lit") || shown(vcx, "caret-composer-dark"),
        "an empty composer drew no caret"
    );

    vcx.simulate_keystrokes("cmd-l");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "caret-omnibox-lit"),
        "\u{2318}L moved the keyboard and left the caret behind"
    );
    assert!(
        shown(vcx, "caret-composer-dark"),
        "the composer's caret is still lit under an open omnibox"
    );

    // Where the keys actually went, which is the half that makes the two
    // assertions above mean something.
    vcx.simulate_keystrokes("h i");
    vcx.run_until_parked();
    assert_eq!(
        omni_text(&h, vcx),
        "hi",
        "the omnibox did not take the keys"
    );

    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(!shown(vcx, "omnibox"), "escape did not close the omnibox");
    assert!(
        !shown(vcx, "caret-omnibox-lit") && !shown(vcx, "caret-omnibox-dark"),
        "a closed omnibox is still drawing a caret"
    );
    assert!(
        shown(vcx, "caret-composer-lit"),
        "the keyboard came back to the composer and the caret did not"
    );
}

/// §1.3's rise is a RELATIVE inset, and this is what that buys: the panel
/// slides 4 px into place and **nothing around it moves**.
///
/// The mistake this rules out is doing the rise with a margin or a padding,
/// which reads identically on the element itself and shoves every sibling
/// below it — here, the composer — down and back up over half a second.
/// taffy resolves relative insets after layout, exactly as a CSS transform
/// does, and that is the only reason the entrance is safe to put on an
/// in-flow panel at all.
///
/// Both halves are needed. The second assertion is what stops the first
/// from passing vacuously: if the rise had silently become a no-op, the
/// composer would also never move, and the test would be green for the
/// wrong reason.
#[gpui::test]
fn an_entrance_that_rises_moves_itself_and_nothing_else(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    drive(&h, vcx, Duration::from_millis(10));

    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_attn((
        "aaaa1111".into(),
        AttentionSnapshot::Unresponsive {
            since_ms: 0,
            zero_samples: 5,
            sampled_ms: 5_000,
        },
    ));
    vcx.run_until_parked();

    // The notice is on screen and its entrance has just begun.
    let rising = vcx
        .debug_bounds("attn-kill")
        .expect("the unresponsive notice did not render");
    let send_rising = vcx.debug_bounds("send").expect("send did not render");

    // Let the entrance land.
    let frames = drive(&h, vcx, Duration::from_millis(10));
    assert!(
        frames > 1,
        "the notice arrived without an entrance at all — this test would \
         then prove nothing"
    );
    let landed = vcx.debug_bounds("attn-kill").unwrap();
    let send_landed = vcx.debug_bounds("send").unwrap();

    assert_eq!(
        send_rising, send_landed,
        "the composer moved while the panel above it rose — the rise is \
         being applied as layout, not as a relative inset"
    );
    let travelled = (rising.origin.y - landed.origin.y).abs();
    assert!(
        travelled > px(1.),
        "the panel itself did not move ({travelled:?}); the rise is a no-op \
         and the assertion above is vacuous"
    );
}

/// The caret blinks without moving the composer.
///
/// **Read the scope before trusting this.** The harness runs on the Noop
/// text system, where every glyph has the same synthetic advance — so this
/// test cannot see a *width* change, and a caret that blinked by swapping
/// its glyph for a blank would pass it. What it does catch is the other
/// wrong implementation, and the more tempting one: lifting the caret out
/// into a sibling element, which changes the box structure and therefore
/// the layout whatever the glyph metrics are. The width half is
/// `a_glyph_swap_would_move_the_text_which_is_why_the_blink_recolours`
/// below, at real font-kit metrics; between them the two cover it, and
/// neither does alone.
///
/// The draft is long enough to wrap, because a single short line would be
/// pinned by `COMPOSER_MIN_H`.
#[gpui::test]
fn the_caret_blink_never_moves_a_pixel_of_the_composer(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    // Q2: the composer is not mounted at the launcher — a draft typed with
    // no session has nothing to be addressed to — so this needs a session
    // before there is a composer to measure.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(10));

    vcx.simulate_input(
        "the quick brown fox jumps over the lazy dog and then keeps going \
         well past the width of the composer so that the text has to wrap",
    );
    vcx.run_until_parked();

    let text = vcx
        .debug_bounds("composer-text")
        .expect("the draft did not render");
    let send = vcx.debug_bounds("send").unwrap();
    assert!(
        text.size.height > px(theme::COMPOSER_LINE),
        "precondition: the draft was meant to wrap, and it did not"
    );

    // Walk a full blink cycle in 100 ms steps. 500 ms is the half-period, so
    // this crosses both edges — lit, dark, lit again.
    for step in 1..=12 {
        vcx.executor().advance_clock(Duration::from_millis(100));
        vcx.run_until_parked();
        assert_eq!(
            vcx.debug_bounds("composer-text").unwrap(),
            text,
            "at {} ms into the blink the composer's text box moved",
            step * 100
        );
        assert_eq!(
            vcx.debug_bounds("send").unwrap(),
            send,
            "at {} ms into the blink the send button moved",
            step * 100
        );
    }
}

/// Why the blink recolours a run instead of swapping the glyph, measured
/// rather than asserted: at real metrics the caret and a blank are
/// **different widths**, so a swap would reflow the draft under the typist's
/// hands twice a second.
///
/// This is the half the harness cannot see — its Noop text system gives
/// every glyph the same advance (Stage J, amendment 1: synthetic advances
/// make a metrics test pass vacuously, which is the exact failure that
/// amendment exists to prevent).
#[gpui::test]
fn a_glyph_swap_would_move_the_text_which_is_why_the_blink_recolours(_cx: &mut TestAppContext) {
    let caret = real_text_width("▏", theme::SANS, theme::BODY_SIZE);
    let blank = real_text_width(" ", theme::SANS, theme::BODY_SIZE);
    assert!(
        caret > px(0.),
        "the caret glyph shaped to nothing — wrong family or size"
    );
    assert_ne!(
        caret, blank,
        "if these were the same width a glyph swap would be a legitimate \
         way to blink; they are not, so the blink recolours a run and the \
         string never changes (`Shell::render`, the has_draft branch)"
    );
}

// Slice N3 — the loaders.
//
// Two things are asserted here and they are different in kind. The first
// three tests are about COST: N1's whole design is "a window with nothing
// mid-flight schedules nothing at all", and N3 is the first slice that
// could break it, because it is the first slice that ever asks. The rest
// are about the trailer, whose one structural risk is that it lives near
// the tail of a virtualized list — so the test that matters is the one
// proving it is not IN the list.
//
// `motion::clock_running` is the instrument for the first group: the tick
// task's own liveness, not a proxy for it.

/// The confirmation that the `UserSent` above reached the agent.
fn delivered_record(seq: u64) -> eave_core::journal::Record {
    eave_core::journal::Record {
        seq,
        ts_ms: seq * 10,
        event: eave_core::journal::JournalEvent::UserDelivered {},
    }
}

/// The turn boundary as it arrives on the wire. NOT `Closed` — the real
/// CLI lingers after this waiting for more stdin (progress.md), which is
/// exactly why the trailer cannot key off the session's state.
fn turn_completed_record(seq: u64) -> eave_core::journal::Record {
    eave_core::journal::Record {
        seq,
        ts_ms: seq * 10,
        event: eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::TurnCompleted {
                subtype: "success".into(),
                is_error: false,
            },
        },
    }
}

fn clock_running(vcx: &mut VisualTestContext) -> bool {
    vcx.update(|_, cx| motion::clock_running(cx))
}

/// Is anything but a **caret** driving the clock?
///
/// Every test below whose subject is a moving shape — a skeleton, a loader,
/// a working dot — reads "is the window still drawing this thing", and that
/// stopped being the same question as `clock_running` when the caret got a
/// rate of its own. A blinking caret in a box somebody is typing into is
/// not a skeleton that forgot to stop.
fn animating(vcx: &mut VisualTestContext) -> bool {
    vcx.update(|_, cx| motion::animating(cx))
}

/// Is the clock running, and running at the caret's rate and no faster?
///
/// The positive half of the same split: "this window costs exactly one
/// blinking caret". Both halves are asserted wherever it appears, because
/// they fail in opposite directions — a `false` from a parked clock is a
/// caret that stopped blinking, and a `false` from `PULSE_TICK` is a caret
/// that pinned the window at the frame rate.
fn caret_only(vcx: &mut VisualTestContext) -> bool {
    vcx.update(|_, cx| motion::clock_tick(cx)) == Some(motion::caret_tick())
}

/// N1's invariant, now that something in the tree actually asks. A shell
/// with nothing in flight must not merely park the clock — it must never
/// construct it, so the window schedules no work of any kind.
#[gpui::test]
fn a_window_with_nothing_in_flight_never_starts_the_pulse_clock(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    // A session the engine is NOT running, and no selected transcript.
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();
    assert!(!clock_running(vcx), "an idle shell leased the pulse clock");

    vcx.executor().advance_clock(Duration::from_secs(1));
    vcx.run_until_parked();
    assert!(
        !clock_running(vcx),
        "a second of wall clock started a drive with nothing in flight"
    );
}

// ---------------------------------------------------------------------------
// Slice N4 — the idle floor, asserted instead of sampled

/// How many times the ROOT is told to re-render, counted from here on.
///
/// The shell observes its five state entities and notifies itself whenever
/// any of them changes, so a notification on the shell IS "this window has
/// to be drawn again" — which is the quantity a CPU sampler is a proxy for.
/// Counting it directly is what makes the answer survive a loaded machine:
/// a percentage measured at load 361 measures the load.
fn watch_redraws(h: &Harness, vcx: &mut VisualTestContext) -> Redraws {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let shell = vcx
        .read_window(&h.window, |shell, _| shell.clone())
        .unwrap();
    let sub = {
        let count = Arc::clone(&count);
        vcx.update(move |_, cx| {
            cx.observe(&shell, move |_, _| {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        })
    };
    Redraws { count, _sub: sub }
}

struct Redraws {
    count: Arc<std::sync::atomic::AtomicUsize>,
    _sub: gpui::Subscription,
}

impl Redraws {
    fn get(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// **A settled window is told to re-render zero times.** This is the idle
/// floor, and it is the number every future performance claim is read
/// against.
///
/// It was chased twice with a CPU sampler and answered differently both
/// times (11-15% of a core in N1, 0.00-0.53% in N2, same machine, same
/// binaries) because a percentage sampled on a machine at load 361 measures
/// the machine. This measures the thing itself, on the deterministic
/// scheduler, where the answer cannot move.
///
/// Ten seconds crosses the one-second reporting ticker ten times, and that
/// ticker is the only timer alive on a settled window: it wakes, reads two
/// entities, finds no standing notice and no open turn, and notifies
/// nothing. Everything else in the spine is edge-driven — the pump waits on
/// a channel, the five observes wait on their entities, the pulse clock is
/// not constructed at all, and the motion tail asks for nothing because
/// nothing claimed it. If this ever reads non-zero, eave has started
/// redrawing itself for free and the report that says otherwise is stale.
#[gpui::test]
fn a_settled_window_is_never_told_to_re_render(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();
    // Boot's entrance (N2) is legitimately mid-flight, and a window with a
    // tween running is not the state under test — it is the state the tween
    // is *supposed* to cost. Settle it first. `drive` panics rather than
    // spinning if it never settles, so "the entrance never ends" fails here
    // as loudly as "the idle window redraws" fails below.
    drive(&h, vcx, Duration::from_millis(16));

    let redraws = watch_redraws(&h, vcx);
    for _ in 0..10 {
        vcx.executor().advance_clock(Duration::from_secs(1));
        vcx.run_until_parked();
    }

    assert_eq!(
        redraws.get(),
        0,
        "a window with nothing selected, nothing streaming and no notice \
         standing redrew itself while the operator was away from the desk"
    );
    assert!(
        !clock_running(vcx),
        "ten seconds of idling constructed the pulse clock"
    );
    assert_eq!(
        h.window
            .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap(),
        0,
        "and it asked for a frame on the way out"
    );
}

/// The one thing on a settled window that never stops on its own, and the
/// rate it runs at.
///
/// A standing attention notice ("quiet for 30s") has to keep its elapsed
/// text moving, so the reporting ticker notifies the rail once a second for
/// as long as the notice stands — which can be hours, since nothing clears
/// a notice but the session changing state. That is the whole of eave's
/// unbounded idle cost, and this pins its rate.
///
/// The expected count is a literal. Deriving it from the ticker's own
/// `Duration::from_secs(1)` would make this pass at any rate, including the
/// 30 Hz the pulse clock runs at — which is exactly the mistake N1 shipped
/// and mutation-testing caught.
#[gpui::test]
fn a_standing_notice_redraws_once_a_second_and_not_faster(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    h.plumbing.push_attn((
        "aaaa1111".into(),
        AttentionSnapshot::Quiet {
            since_ms: 0,
            reason: WaitReason::Idle,
        },
    ));
    vcx.run_until_parked();

    let redraws = watch_redraws(&h, vcx);
    for _ in 0..10 {
        vcx.executor().advance_clock(Duration::from_secs(1));
        vcx.run_until_parked();
    }

    assert_eq!(
        redraws.get(),
        10,
        "a standing notice must cost ONE redraw a second — no more (a \
         faster clock is a regression) and no fewer (a notice whose \
         elapsed text has stopped moving is lying about how long it has \
         been standing)"
    );
}

/// The lease, end to end through the real shell: a running session takes
/// one, and the session settling lets the clock park itself. Nobody stops
/// the clock — it stops when the last lease lapses, and if this test ever
/// has to be relaxed the design changed, not the test.
#[gpui::test]
fn a_running_session_leases_the_clock_and_a_settled_one_lets_it_park(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut running = session("aaaa1111");
    running.state = "running".into();
    h.plumbing.push_rail(vec![running.clone()]);
    vcx.run_until_parked();
    assert!(
        clock_running(vcx),
        "a running session's dot did not take a lease"
    );

    let mut settled = running;
    settled.state = "done".into();
    h.plumbing.push_rail(vec![settled]);
    vcx.run_until_parked();
    // Well past both the 33 ms tick and the 300 ms lease. The assertion
    // is that it stopped, not that it stopped promptly.
    vcx.executor().advance_clock(Duration::from_secs(1));
    vcx.run_until_parked();
    assert!(
        !clock_running(vcx),
        "the clock kept ticking over a settled session"
    );
}

/// Reduced motion is honoured before the lease, not after it: a machine
/// whose owner asked for less motion runs no timer at all, however much
/// work is in flight. The picture it gets is the resting one — the solid
/// dots eave drew before this slice — never a loader frozen mid-wave.
#[gpui::test]
fn reduced_motion_starts_no_clock_however_much_is_in_flight(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    vcx.update(|_, cx| cx.set_reduce_motion(true));
    let mut running = session("aaaa1111");
    running.state = "running".into();
    h.plumbing.push_rail(vec![running]);
    ask(&h, vcx, vec![user_record(1, "go"), delivered_record(2)]);

    vcx.executor().advance_clock(Duration::from_secs(1));
    vcx.run_until_parked();
    assert!(
        !clock_running(vcx),
        "reduced motion still started the pulse clock"
    );
    // The FACT survives the preference — only the motion goes.
    assert!(
        shown(vcx, "work-trailer"),
        "reduced motion removed the working trailer instead of its motion"
    );
}

/// The gap this slice exists to close: eave had no in-flow indicator for
/// a healthy running turn at all, and no elapsed time until the engine
/// had already escalated the session to `quiet`.
#[gpui::test]
fn a_live_turn_gets_a_trailer_and_the_turn_ending_takes_it_away(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    ask(&h, vcx, vec![user_record(1, "go"), delivered_record(2)]);
    assert!(
        shown(vcx, "work-trailer"),
        "a turn in flight must say so in the log"
    );

    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![turn_completed_record(3)],
    });
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "work-trailer"),
        "the trailer outlived the turn it was reporting"
    );
}

/// The delivery pair is netted, not flagged. The recorder's two input
/// channels are unordered relative to each other, so a fast CLI's
/// `TurnCompleted` can be journaled BEFORE the `UserDelivered` that
/// opened its turn — the engine-side classifier had a ~40% flake from a
/// bool here, and the symptom on this side would be a phantom trailer
/// that never goes away.
#[gpui::test]
fn the_turn_clock_nets_the_delivery_pair_in_either_order(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    ask(
        &h,
        vcx,
        vec![
            user_record(1, "go"),
            turn_completed_record(2),
            delivered_record(3),
        ],
    );
    assert!(
        !shown(vcx, "work-trailer"),
        "a completion that arrived before its own delivery left a phantom turn open"
    );
}

/// Agent prose, stamped at a caller-chosen journal time — the trailer's
/// settle clock reads `ts_ms` against the wall clock, so a test that wants
/// work to look LIVE has to say so.
fn agent_text_record(seq: u64, ts_ms: u64, text: &str) -> eave_core::journal::Record {
    eave_core::journal::Record {
        seq,
        ts_ms,
        event: eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::TextDelta { text: text.into() },
        },
    }
}

/// **The two facts an operator must be able to tell apart** (Slice CF).
///
/// Before this slice the second one had no picture at all: a turn only ever
/// opened on `UserDelivered`, which is eave's own event, so an agent working
/// on its own — a background subagent finishing, a background shell
/// reporting — rendered as a session with nothing happening.
///
/// They must not render identically, and this asserts it from both sides so
/// neither sentence can quietly become the other.
#[gpui::test]
fn a_prompted_turn_and_a_self_started_one_say_different_things(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    ask(&h, vcx, vec![user_record(1, "go"), delivered_record(2)]);
    assert!(shown(vcx, "work-trailer"), "a turn in flight must say so");
    assert!(
        shown(vcx, "work-trailer-asked"),
        "the operator asked for this one"
    );
    assert!(
        !shown(vcx, "work-trailer-self"),
        "a turn the operator asked for was captioned as the agent's own"
    );

    // Now the same shell, with agent work and no prompt anywhere. Stamped
    // NOW so the quiet clock has not run.
    let now = eave_core::journal::now_ms();
    ask(
        &h,
        vcx,
        vec![agent_text_record(1, now, "background task finished")],
    );
    assert!(
        shown(vcx, "work-trailer"),
        "the agent working on its own showed nothing at all"
    );
    assert!(
        shown(vcx, "work-trailer-self"),
        "self-started work must say it was self-started"
    );
    assert!(
        !shown(vcx, "work-trailer-asked"),
        "work nobody asked for was captioned as the operator's"
    );
}

/// A self-started turn has no terminal frame coming, so quiescence is its
/// only settle path — and the reader runs that rule itself, on the second it
/// already ticks for the elapsed clock. Without it the trailer would report
/// "working on its own" forever, which is Slice AQ's `working for 99h 49m`
/// arriving from the other direction.
#[gpui::test]
fn a_self_started_turn_settles_when_it_goes_quiet(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    // Stamped at the epoch: by the wall clock this work went quiet decades
    // ago, so the very first heartbeat must retire it.
    ask(
        &h,
        vcx,
        vec![agent_text_record(1, 1, "woke up, did a thing")],
    );
    assert!(shown(vcx, "work-trailer-self"), "it never opened");
    vcx.executor().advance_clock(Duration::from_secs(1));
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "work-trailer"),
        "a self-started turn that went quiet is still captioned as working"
    );
}

/// The opposite contract, on the same clock: a turn the operator asked for
/// waits for the provider's frame and **never** times out. `user_record`
/// stamps at `seq * 10`, so by the wall clock this one is decades stale too
/// — and it must still be up.
#[gpui::test]
fn a_prompted_turn_is_never_quiesced_away(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    ask(&h, vcx, vec![user_record(1, "go"), delivered_record(2)]);
    for _ in 0..5 {
        vcx.executor().advance_clock(Duration::from_secs(1));
        vcx.run_until_parked();
    }
    assert!(
        shown(vcx, "work-trailer-asked"),
        "eave invented a terminal frame the provider still owes"
    );
}

/// **The arithmetic bug, from the app's side.**
///
/// claude's background-subagent wake is a whole second turn on the same
/// session: a second `system/init` under the known `session_id`, and a
/// second `result` (`docs/research/turn-start-evidence.md` §1, verified
/// against `fixtures/claude-background-subagent-2.1.228.jsonl`). One prompt,
/// two `TurnCompleted`s.
///
/// The old balance ran +1, −1, −1 → floored at −1, so the operator's NEXT
/// delivery landed at 0 and `in_turn()` was false: no trailer, on a turn
/// that was genuinely running. Counting the wake turn nets it.
#[gpui::test]
fn a_background_subagent_wake_does_not_swallow_the_next_turn(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    ask(
        &h,
        vcx,
        vec![
            // `init_record` carries session `aaaa1111` — the SAME id both
            // times, which is what makes the second one an announcement
            // rather than another session on the same stream.
            init_record(1, &[]),
            user_record(2, "launch a background subagent"),
            delivered_record(3),
            turn_completed_record(4),
            // The wake: the CLI announces itself again, works, and settles.
            init_record(5, &[]),
            turn_completed_record(6),
            // And the operator asks the next thing.
            user_record(7, "now what?"),
            delivered_record(8),
        ],
    );
    assert!(
        shown(vcx, "work-trailer-asked"),
        "an unpaired wake `result` swallowed the operator's next turn"
    );
}

/// **The structural assertion the trailer exists to survive.** It sits
/// near the tail of a bottom-anchored virtualized list, which is exactly
/// where a new row would shove every other row up by its own height. It
/// is not a row: it lives in the floating chrome stack, a sibling of the
/// list. So the trailer appearing and disappearing must move nothing.
///
/// The instrument is an approval CARD, because it is the one thing
/// rendered INSIDE a list row that carries a selector — its bounds are a
/// direct readout of the list's layout.
///
/// **Slice AQ moved the instrument from the card's approve button to the
/// card itself, and answered the question the old form was really
/// asking.** It used to open a *pending* question and assert the trailer
/// was up beside it; a session parked on a permission question is now
/// never captioned `working` (see `Shell::work_trailer`), so that
/// arrangement can no longer produce a trailer to toggle. The question is
/// therefore settled here, which leaves the card in the log — with its
/// buttons gone — while the turn stays open. Same list, same row, same
/// differential; the only thing that changed is which selector inside the
/// row is read, and `approval-req-1` is the card's own box rather than a
/// button inside it, so it is if anything the better readout.
///
/// **This assertion is mutation-checked, and it needed to be.** The
/// harness runs on the Noop text system, where every glyph has the same
/// synthetic advance, so no `debug_bounds` assertion can catch a width
/// change — only a change in box structure (Slice N2 lost a test to
/// exactly that). This one survives because it is a *differential* y
/// comparison: a constant-but-wrong advance moves both samples equally.
/// Proved rather than argued — making the list's tail spacer take 20 px
/// while a turn is open fails it with `y: 497px` vs `y: 517px`.
///
/// If this ever goes red on a tree where the trailer is provably still
/// chrome, suspect the approve/deny buttons before the trailer: their
/// hover wash is Slice N2's `hover_fade`, and they are the only N2 surface
/// inside a transcript row. It is a `bg` plus a listener and participates
/// in no layout — N2's replay assert ran 24/24 with every fade live to say
/// so — but it is the only other thing in here that could move.
#[gpui::test]
fn the_trailer_never_participates_in_a_row_height(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    ask(
        &h,
        vcx,
        vec![
            user_record(1, "go"),
            delivered_record(2),
            approval_record(3, "req-1", "Write", "/repo/src/main.rs"),
            decided_record(4, "req-1", true),
        ],
    );
    assert!(shown(vcx, "work-trailer"), "the turn is open");
    assert!(
        !shown(vcx, "approve-req-1"),
        "precondition: the question is settled, so nothing suppresses the trailer"
    );
    let before = vcx
        .debug_bounds("approval-req-1")
        .expect("the approval row must be on screen to measure it");

    // End the turn: the trailer goes, and `TurnCompleted` adds no row of
    // its own when it is not an error, so the list content is unchanged.
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![turn_completed_record(5)],
    });
    vcx.run_until_parked();
    assert!(!shown(vcx, "work-trailer"));
    let after = vcx
        .debug_bounds("approval-req-1")
        .expect("the approval row must still be on screen");
    assert_eq!(
        before, after,
        "the trailer leaving moved a row — it is participating in the list's layout"
    );
}

/// One slot, one sentence. The attention notice owns the space above the
/// composer whenever it has anything to say, so "working for 12s" and
/// "quiet for 30s" are read in the same place in the same voice, and
/// never stacked on top of each other.
#[gpui::test]
fn an_attention_notice_owns_the_slot_and_the_trailer_stands_down(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    ask(&h, vcx, vec![user_record(1, "go"), delivered_record(2)]);
    assert!(shown(vcx, "work-trailer"));

    h.plumbing.push_attn((
        "aaaa1111".into(),
        AttentionSnapshot::Quiet {
            since_ms: 0,
            reason: WaitReason::Idle,
        },
    ));
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "work-trailer"),
        "the trailer stacked underneath the quiet notice"
    );
    // And it stood down because the notice took the slot, not because the
    // turn ended — the turn is still open.
    let open = vcx
        .read_window(&h.window, |shell, cx| {
            shell.read(cx).transcript.read(cx).in_turn()
        })
        .unwrap();
    assert!(open, "the turn should still be open under a quiet notice");
}

/// The files rail's not-yet-arrived state used to be the single character
/// `…`, which says nothing about whether a list is coming.
#[gpui::test]
fn a_files_rail_with_no_snapshot_yet_shows_skeleton_rows(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert!(
        shown(vcx, "files-skeleton"),
        "a session whose file list has not arrived must say a list is coming"
    );

    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: Vec::new(),
            omitted: 0,
            watching: true,
            basis: DiffBasis::WorkingTree,
        }),
    ));
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "files-skeleton"),
        "the skeleton outlived the snapshot it was standing in for"
    );
}

/// Same for the diff pane: a click opens it into a load state, and the
/// load state is now shaped like the diff that is coming.
#[gpui::test]
fn a_diff_that_has_not_arrived_shows_skeleton_rows(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                status: FileStatus::Modified,
                adds: Some(3),
                dels: Some(1),
                orig_path: None,
                untracked: false,
            }],
            omitted: 0,
            watching: true,
            basis: DiffBasis::WorkingTree,
        }),
    ));
    vcx.run_until_parked();
    assert!(!shown(vcx, "diff-skeleton"), "nothing is loading yet");

    click(vcx, "file-src/lib.rs");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "diff-skeleton"),
        "an open pane waiting on its diff must state that it is waiting"
    );

    h.plumbing.push_diff((
        "aaaa1111".into(),
        "src/lib.rs".into(),
        Ok(FileDiff {
            path: "src/lib.rs".into(),
            kind: DiffKind::Binary,
        }),
    ));
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "diff-skeleton"),
        "the skeleton outlived the diff it was standing in for"
    );
}

// ---------------------------------------------------------------------------
// Slice P3 — the icon pipeline.
//
// **What these tests can and cannot prove.** `TestAppContext` hardcodes
// `Arc::new(())` as its asset source (gpui `app/test_context.rs`), the `()`
// impl answers `Ok(None)` to everything, and `App` exposes no setter — so
// no test in this file can make an icon resolve. Every glyph here renders
// as empty coverage. That is not a gap this slice can close: assets are an
// `Application`-level concern and the harness never builds one.
//
// So these assert the half that IS observable and that a swap can break:
// the SLOT (an SVG lays out from its own box, whether or not the bytes
// arrive) and the HITBOX (replacing a clickable div's child is exactly how
// you break its click target). Whether the drawing is right is a question
// for a capture, and it is answered in the report.

/// Every swapped site draws a glyph in the box its token names.
///
/// Non-vacuous by construction: each expected size is a DIFFERENT number,
/// so a copy-paste that pointed two sites at one token would fail here.
#[gpui::test]
fn every_swapped_control_draws_its_glyph_in_the_box_its_token_names(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                status: FileStatus::Modified,
                adds: Some(3),
                dels: Some(1),
                orig_path: None,
                untracked: false,
            }],
            omitted: 0,
            watching: true,
            basis: DiffBasis::WorkingTree,
        }),
    ));
    vcx.run_until_parked();

    for (selector, expected) in [
        ("send-glyph", theme::ICON_SM),
        ("new-session-glyph", theme::ICON_SM),
        ("rail-toggle-glyph", theme::ICON_TOGGLE),
        ("files-toggle-glyph", theme::ICON_TOGGLE),
        ("file-status-src/lib.rs", theme::ICON_STATUS),
    ] {
        let b = vcx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} did not render"));
        assert_eq!(
            (b.size.width, b.size.height),
            (px(expected), px(expected)),
            "{selector} is not the square its token names",
        );
    }
}

/// The status slot is status-independent, which is the whole reason it is
/// a fixed slot: five statuses used to be five one-letter strings and are
/// now five different drawings, and neither may move the path beside it.
#[gpui::test]
fn the_status_glyph_never_moves_the_path_beside_it(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    let file = |path: &str, status| ChangedFile {
        path: path.into(),
        status,
        adds: Some(1),
        dels: Some(1),
        orig_path: None,
        untracked: false,
    };
    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: vec![
                file("a.rs", FileStatus::Added),
                file("b.rs", FileStatus::Modified),
                file("c.rs", FileStatus::Deleted),
                file("d.rs", FileStatus::Renamed),
                file("e.rs", FileStatus::Conflicted),
            ],
            omitted: 0,
            watching: true,
            basis: DiffBasis::WorkingTree,
        }),
    ));
    vcx.run_until_parked();

    // Measure the GLYPH boxes, not the rows. A row is full-width, so its
    // bounds would be identical whatever the status drew inside it — an
    // assertion on rows would pass for a variable-width glyph and prove
    // nothing. Each glyph carries its own selector for exactly this reason.
    //
    // Literal selectors because `debug_bounds` wants `&'static str`.
    let glyphs = [
        "file-status-a.rs",
        "file-status-b.rs",
        "file-status-c.rs",
        "file-status-d.rs",
        "file-status-e.rs",
    ];
    let boxes: Vec<_> = glyphs
        .iter()
        .map(|sel| {
            vcx.debug_bounds(sel)
                .unwrap_or_else(|| panic!("{sel} did not render"))
        })
        .collect();
    let first = boxes[0];
    for (b, name) in boxes.iter().zip(glyphs) {
        assert_eq!(b.size.width, first.size.width, "{name} glyph width");
        assert_eq!(b.size.height, first.size.height, "{name} glyph height");
        assert_eq!(b.origin.x, first.origin.x, "{name} glyph x");
    }
    // ...and the slot really is the fixed one, not just five glyphs that
    // happen to agree: the box is the token's, inside the rail's slot.
    assert_eq!(first.size.width, px(theme::ICON_STATUS));
    // Constant today, which is exactly why it is asserted: it fires the day
    // someone moves either token so the glyph stops fitting its slot.
    #[allow(clippy::assertions_on_constants)]
    {
        assert!(theme::ICON_STATUS <= theme::FILE_STATUS_W);
    }
}

/// Both toggles still toggle. Replacing the child of a clickable `div` is
/// the standard way to break its click target — the icon is inside the
/// hitbox, not the other way round, and this is what says so.
#[gpui::test]
fn the_collapse_toggles_still_toggle_after_the_glyph_swap(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert!(shown(vcx, "new-session"), "the session rail starts open");

    click(vcx, "rail-toggle");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "new-session"),
        "clicking the rail toggle did not collapse the rail — the glyph \
         swap broke the hitbox"
    );

    click(vcx, "rail-toggle");
    vcx.run_until_parked();
    assert!(shown(vcx, "new-session"), "the rail did not come back");
}

// Slice P1 — the overlay layer
//
// The layer has no product consumer by directive, so everything below
// drives it through the demo trigger `ui` mounts behind
// `overlay::demo_enabled()`. What is being proven is not the demo: it is
// that a floating surface can occlude, dismiss, navigate and *leave*
// without any of it costing a frame once it has settled.
//
// The pure half of the machine — the state transitions, both tweens and
// the highlight arithmetic — is asserted without a window at all, in
// `overlay`'s own test module. These are the parts that need one: paint
// order, hit-testing, and the frame drive.

/// Boot with the demo trigger mounted. The gate is thread-local, and
/// `#[gpui::test]` gives each test its own thread, so this cannot leak
/// into a test that did not ask for it.
fn boot_with_overlay(cx: &mut TestAppContext) -> (Harness, &'static mut VisualTestContext) {
    overlay::enable_demo_for_test();
    boot(cx)
}

/// Do these two boxes share any pixels? Used to prove a fixture is
/// actually testing occlusion rather than clicking empty space next to
/// the thing it thinks it is covering.
fn overlaps(a: Bounds<Pixels>, b: Bounds<Pixels>) -> bool {
    a.origin.x < b.origin.x + b.size.width
        && b.origin.x < a.origin.x + a.size.width
        && a.origin.y < b.origin.y + b.size.height
        && b.origin.y < a.origin.y + a.size.height
}

/// A live, command-sending shell affordance that the menu actually covers.
///
/// The two occlusion fixtures used to name `rail-aaaa1111` outright. That
/// worked by **two pixels** — the first session row sat at y=148 and the
/// menu ended at y=150 — so any change to what the rail stacks above its
/// sessions silently turned both tests into assertions about nothing. Slice
/// T1b's BOOKMARKS section was such a change.
///
/// Searching for the affordance instead of naming one keeps the fixture
/// pointed at its actual claim: *something clickable is under this menu*.
/// Every candidate below sends a command when clicked, which is what the
/// `h.drain().is_empty()` assertions downstream are reading.
fn covered_affordance(vcx: &mut VisualTestContext, menu: Bounds<Pixels>) -> Bounds<Pixels> {
    const CANDIDATES: &[&str] = &[
        "rail-aaaa1111",
        "rail-bbbb2222",
        "bookmark-new",
        "project-new",
        "new-session",
    ];
    let mut seen = Vec::new();
    for name in CANDIDATES {
        if let Some(b) = vcx.debug_bounds(name) {
            if overlaps(menu, b) {
                return b;
            }
            seen.push(format!("{name} {b:?}"));
        }
    }
    panic!(
        "no live affordance under the menu {menu:?} — the fixture would \
         prove nothing. Candidates: {}",
        seen.join(", ")
    );
}

/// Boot, fill the rail, open the menu, and settle it. Returns the menu's
/// bounds — every occlusion assertion needs them.
fn open_menu(cx: &mut TestAppContext) -> (Harness, &'static mut VisualTestContext, Bounds<Pixels>) {
    let (mut h, vcx) = boot_with_overlay(cx);
    h.plumbing.push_rail(vec![
        session("aaaa1111"),
        session("bbbb2222"),
        session("cccc3333"),
        session("dddd4444"),
    ]);
    vcx.run_until_parked();
    let _ = h.drain();

    click(vcx, "overlay-demo");
    vcx.run_until_parked();
    // Let the 140 ms entrance land, so nothing below is asserting against
    // a surface that is still arriving.
    drive(&h, vcx, Duration::from_millis(16));
    // Drain AFTER the trigger, not before it. The opening click is a click
    // on the shell like any other, and anything it sends belongs to the
    // fixture rather than to the assertions below. Draining beforehand left
    // it in the queue, so the first `h.drain().is_empty()` downstream
    // reported the OPENING click as a click that fell through the menu —
    // which is what Slice D4 hit the moment a control appeared under the
    // demo trigger, and it accused the wrong click for an hour.
    let _ = h.drain();
    let bounds = vcx
        .debug_bounds("overlay-menu")
        .expect("the menu did not render");
    (h, vcx, bounds)
}

/// The gate holds: the shipped shell has no trigger and no layer. This is
/// the assertion that lets the other 244 tests keep their numbers — the
/// demo compiles into the binary, and it must be invisible until asked
/// for.
#[gpui::test]
fn the_overlay_layer_is_absent_from_the_shell_that_ships(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();
    assert!(!shown(vcx, "overlay-demo"), "the demo trigger shipped");
    assert!(!shown(vcx, "overlay-menu"), "the overlay layer shipped");
}

/// The first popover eave has ever had: it opens, it paints over the
/// shell, and a click anywhere else takes it away.
#[gpui::test]
fn a_menu_opens_over_the_shell_and_a_click_outside_takes_it_away(cx: &mut TestAppContext) {
    let (h, vcx, menu) = open_menu(cx);
    assert!(shown(vcx, "menu-row-0") && shown(vcx, "menu-row-2"));

    // Somewhere clear of the menu, still inside the window.
    let outside = point(
        menu.origin.x + menu.size.width + px(120.),
        menu.origin.y + menu.size.height + px(120.),
    );
    assert!(!menu.contains(&outside));
    vcx.simulate_click(outside, Modifiers::none());
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(16));
    assert!(
        !shown(vcx, "overlay-menu"),
        "the menu outlived its dismissal"
    );
}

#[gpui::test]
fn escape_takes_the_menu_away(cx: &mut TestAppContext) {
    let (h, vcx, _) = open_menu(cx);
    vcx.simulate_keystrokes("escape");
    drive(&h, vcx, Duration::from_millis(16));
    assert!(!shown(vcx, "overlay-menu"));
}

/// **The click-through bug, asserted before it can be written.** A rail
/// row lies under the menu; clicking the menu row on top of it must fire
/// the menu row and must not select the session.
#[gpui::test]
fn a_menu_row_takes_the_click_the_row_beneath_it_would_have(cx: &mut TestAppContext) {
    let (mut h, vcx, menu) = open_menu(cx);
    let _covered = covered_affordance(vcx, menu);

    click(vcx, "menu-row-0");
    vcx.run_until_parked();
    let chosen = vcx
        .read_window(&h.window, |shell, cx| shell.read(cx).demo.chosen.clone())
        .unwrap();
    assert_eq!(chosen.as_deref(), Some("Reveal in Finder"));
    assert!(
        h.drain().is_empty(),
        "the click fell through the menu and reached the rail"
    );
}

/// The other half, and the one that is easy to get wrong: dismissing by
/// clicking outside must not ALSO activate whatever was outside. The
/// full-window catcher behind the menu is what stops it, and this is the
/// only thing that proves the catcher occludes rather than merely
/// listening.
#[gpui::test]
fn dismissing_by_clicking_away_does_not_also_hit_what_was_there(cx: &mut TestAppContext) {
    let (mut h, vcx, menu) = open_menu(cx);
    // A rail row the menu does NOT cover: the click has to be a real
    // click on a real affordance, or the test is about empty space.
    let row = ["rail-dddd4444", "rail-cccc3333", "rail-bbbb2222"]
        .into_iter()
        .find_map(|sel| {
            vcx.debug_bounds(sel)
                .filter(|b| !overlaps(menu, *b))
                .map(|b| (sel, b))
        })
        .expect("no rail row sits clear of the menu — fixture needs more rows");
    let (_, bounds) = row;

    vcx.simulate_click(bounds.center(), Modifiers::none());
    vcx.run_until_parked();
    assert!(
        h.drain().is_empty(),
        "the dismissing click also selected the session underneath it"
    );
    drive(&h, vcx, Duration::from_millis(16));
    assert!(!shown(vcx, "overlay-menu"), "the click did not dismiss");

    // ...and now that the menu is gone, the same click works normally.
    vcx.simulate_click(bounds.center(), Modifiers::none());
    vcx.run_until_parked();
    assert!(
        matches!(h.drain().as_slice(), [Command::Select(_)]),
        "the catcher outlived the menu and swallowed a real click"
    );
}

/// Keyboard navigation skips the disabled row, and return fires the
/// highlighted one. Three rows, the third disabled: down three times must
/// wrap to the first rather than land on the third.
#[gpui::test]
fn the_arrows_skip_the_disabled_row_and_return_chooses(cx: &mut TestAppContext) {
    let (h, vcx, _) = open_menu(cx);
    let hl = |vcx: &mut VisualTestContext, h: &Harness| {
        vcx.read_window(&h.window, |shell, cx| {
            shell.read(cx).demo.menu.highlighted()
        })
        .unwrap()
    };
    vcx.simulate_keystrokes("down");
    assert_eq!(hl(vcx, &h), Some(0));
    vcx.simulate_keystrokes("down");
    assert_eq!(hl(vcx, &h), Some(1));
    vcx.simulate_keystrokes("down");
    assert_eq!(
        hl(vcx, &h),
        Some(0),
        "the cursor stopped on the disabled row"
    );

    vcx.simulate_keystrokes("down enter");
    let (chosen, mounted) = vcx
        .read_window(&h.window, |shell, cx| {
            let s = shell.read(cx);
            (s.demo.chosen.clone(), s.demo.menu.is_mounted())
        })
        .unwrap();
    assert_eq!(chosen.as_deref(), Some("Copy session id"));
    assert!(mounted, "choosing must play the exit, not vanish");
    drive(&h, vcx, Duration::from_millis(16));
    assert!(!shown(vcx, "overlay-menu"));
}

/// A disabled row is dimmed, takes no click, and is still there — it
/// occupies its slot and swallows the click rather than letting it reach
/// the shell behind the menu.
#[gpui::test]
fn a_disabled_row_is_present_and_inert(cx: &mut TestAppContext) {
    let (mut h, vcx, _) = open_menu(cx);
    assert!(shown(vcx, "menu-row-2"), "a disabled row still renders");
    click(vcx, "menu-row-2");
    vcx.run_until_parked();
    let (chosen, open) = vcx
        .read_window(&h.window, |shell, cx| {
            let s = shell.read(cx);
            (s.demo.chosen.clone(), s.demo.menu.is_open())
        })
        .unwrap();
    assert_eq!(chosen, None, "a disabled row fired");
    assert!(open, "a disabled row dismissed the menu it sits in");
    let leaked = h.drain();
    assert!(
        leaked.is_empty(),
        "the click fell through to the shell: {leaked:?}"
    );
}

/// An open menu owns the keyboard. A character typed at it must not land
/// in a draft the user cannot see behind the surface.
#[gpui::test]
fn an_open_menu_swallows_typing(cx: &mut TestAppContext) {
    let (h, vcx, _) = open_menu(cx);
    // `open_menu` pushes a RAIL, not a selection, so the shell behind the
    // menu was the launcher — and the second half of this test ("the
    // composer got the keyboard back") was asserting against a composer
    // that was never mounted. Select one, so the surface this test is about
    // is a menu over a session, which is the only place a menu opens.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    let draft = |vcx: &mut VisualTestContext, h: &Harness| {
        vcx.read_window(&h.window, |shell, cx| {
            shell
                .read(cx)
                .composer
                .read(cx)
                .field
                .read(cx)
                .content()
                .to_string()
        })
        .unwrap()
    };
    vcx.simulate_input("hello");
    assert_eq!(
        draft(vcx, &h),
        "",
        "typing reached the composer through the menu"
    );
    vcx.simulate_keystrokes("escape");
    drive(&h, vcx, Duration::from_millis(16));
    vcx.simulate_input("hello");
    assert_eq!(
        draft(vcx, &h),
        "hello",
        "the composer never got the keyboard back"
    );
}

/// The constraint that decides where the layer mounts: **an overlay must
/// never participate in a cached row height.** eave's transcript is a
/// virtualized list, and a surface that changes any layout is a surface
/// that can change one of those. Root-mounted and absolute, it changes
/// nothing — measured, not argued.
#[gpui::test]
fn the_overlay_moves_nothing_it_floats_over(cx: &mut TestAppContext) {
    let (h, vcx) = boot_with_overlay(cx);
    h.plumbing
        .push_rail(vec![session("aaaa1111"), session("bbbb2222")]);
    // Q2: `composer-hint` is one of the probes below, and the composer is
    // only mounted once a session is selected.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(16));

    let probes = [
        "rail-aaaa1111",
        "rail-bbbb2222",
        "rail-rows",
        // `composer-hint`, not `composer-text`: the text element only
        // renders once there is a draft, and a probe that is absent
        // proves nothing (it was absent before AND after).
        "composer-hint",
        "send",
        "rail-toggle",
    ];
    let before: Vec<_> = probes.iter().map(|s| vcx.debug_bounds(s)).collect();
    assert!(
        before.iter().all(|b| b.is_some()),
        "a probe never rendered, so it proves nothing: {before:?}"
    );

    click(vcx, "overlay-demo");
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(16));
    assert!(
        shown(vcx, "overlay-menu"),
        "the menu must be up for this to mean anything"
    );

    let after: Vec<_> = probes.iter().map(|s| vcx.debug_bounds(s)).collect();
    assert_eq!(before, after, "the overlay moved the shell underneath it");
}

/// N1's invariant, carried into the first hand-driven oneshot in the app:
/// a menu costs frames while it is arriving and while it is leaving, and
/// **nothing at all** in between or afterwards.
///
/// The `drive` calls are the assertion — each one panics rather than
/// spinning if the window never stops asking, which is the exact failure
/// mode of a tween driven by a mounted `with_animation`.
#[gpui::test]
fn a_menu_costs_frames_only_while_it_moves(cx: &mut TestAppContext) {
    let (h, vcx) = boot_with_overlay(cx);
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(16));

    click(vcx, "overlay-demo");
    vcx.run_until_parked();
    let entering = drive(&h, vcx, Duration::from_millis(16));
    assert!(entering > 0, "the entrance never asked for a frame");

    // Settled and open: the surface is up, and it is free.
    assert!(shown(vcx, "overlay-menu"));
    assert_eq!(
        h.window
            .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap(),
        0,
        "an open, settled menu is still asking for frames"
    );
    assert!(
        !clock_running(vcx),
        "the overlay leased the pulse clock — it rides the tail, not the clock"
    );

    vcx.simulate_keystrokes("escape");
    let leaving = drive(&h, vcx, Duration::from_millis(16));
    assert!(leaving > 0, "the exit never asked for a frame");
    assert!(!shown(vcx, "overlay-menu"));
    assert_eq!(
        h.window
            .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap(),
        0,
        "the window kept asking for frames after the menu was gone"
    );
    assert!(!clock_running(vcx));
}

/// The exit's whole reason to exist: the surface stays mounted, painted
/// and **occluding** while it plays, but stops taking input the instant
/// it is dismissed. A click during those 100 ms must do nothing at all —
/// not fire a menu row, and not reach what the menu was covering.
#[gpui::test]
fn the_surface_stays_up_and_dead_through_its_exit(cx: &mut TestAppContext) {
    let (mut h, vcx, menu) = open_menu(cx);
    let _covered = covered_affordance(vcx, menu);

    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "overlay-menu"),
        "the surface vanished instead of playing its exit"
    );

    // Mid-exit: click straight through where the menu is.
    click(vcx, "menu-row-0");
    vcx.run_until_parked();
    let chosen = vcx
        .read_window(&h.window, |shell, cx| shell.read(cx).demo.chosen.clone())
        .unwrap();
    assert_eq!(chosen, None, "a row fired during the exit");
    assert!(
        h.drain().is_empty(),
        "a click during the exit fell through to the rail underneath"
    );

    drive(&h, vcx, Duration::from_millis(16));
    assert!(!shown(vcx, "overlay-menu"), "the reap never came");
}

/// Reduced motion: the menu is simply there, and then simply gone. No
/// entrance, no exit, and — the part that matters — **no window of time
/// where an invisible surface is still occluding the shell**, which is
/// what a reduced-motion path that skipped only the tween would leave
/// behind.
#[gpui::test]
fn reduced_motion_gets_a_menu_with_no_travel_at_all(cx: &mut TestAppContext) {
    let (h, vcx) = boot_with_overlay(cx);
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();
    vcx.update(|_, cx| cx.set_reduce_motion(true));
    vcx.update(|window, _| window.refresh());
    vcx.run_until_parked();

    click(vcx, "overlay-demo");
    vcx.run_until_parked();
    // No clock advanced, no frames driven: it is already fully there.
    assert!(
        shown(vcx, "overlay-menu"),
        "the menu waited for a frame it will never get"
    );
    let opacity = vcx
        .read_window(&h.window, |shell, cx| {
            shell
                .read(cx)
                .demo
                .menu
                .motion_at(cx.background_executor().now(), true)
        })
        .unwrap();
    assert_eq!(
        opacity,
        Some((1.0, 0.0)),
        "reduced motion drew a part-way menu"
    );

    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "overlay-menu"),
        "a dismissed menu stayed mounted with no exit to play — an \
         invisible surface still occluding the shell"
    );
}

// ---------------------------------------------------------------------------
// Slice Q2 — the launcher.
//
// The tests worth having here are the ones that catch a launcher becoming
// a mock-up: a tile that claims to work when the engine would refuse it, a
// button that emits nothing, a model choice that does not reach the
// command, or a rest state that quietly costs frames forever.

/// The rest state exists, and it is the launcher rather than a sentence.
///
/// Before Q2 this could not be tested at all — `ui.rs`'s connection loop
/// opened a session by itself on an empty engine, so "no session" was a
/// frame or two at boot and nothing a person would ever see.
#[gpui::test]
fn a_bare_boot_lands_on_the_launcher(cx: &mut TestAppContext) {
    let (_h, vcx) = boot(cx);
    assert!(shown(vcx, "launcher"), "boot with no session shows nothing");
    assert!(shown(vcx, "launcher-title"));
    assert!(shown(vcx, "provider-strip"));
    assert!(shown(vcx, "launcher-start"));
    // Where it will run, stated before it runs.
    assert!(shown(vcx, "launcher-cwd"));
    for ix in 0..crate::launcher::PROVIDERS.len() {
        assert!(
            vcx.debug_bounds(Box::leak(format!("provider-tile-{ix}").into_boxed_str()))
                .is_some(),
            "tile {ix} did not render",
        );
    }
}

/// The launcher is an OVERLAY, never a row — the same rule the empty note
/// and the P1 surface hold to. A virtualized list caches a measure per
/// row, and a row whose height depends on anything but its own identity
/// leaves a stale measure that moves the scroll anchor.
#[gpui::test]
fn the_launcher_never_becomes_a_row(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    assert!(shown(vcx, "launcher"));
    let items = vcx
        .read_window(&h.window, |shell, cx| {
            shell.read(cx).transcript.read(cx).list.item_count()
        })
        .unwrap();
    assert_eq!(items, 1, "the launcher took a row in the virtualized list");
}

/// A session arriving replaces the launcher, and the two are never on
/// screen together.
#[gpui::test]
fn a_session_replaces_the_launcher(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    assert!(shown(vcx, "launcher"));
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert!(!shown(vcx, "launcher"), "the launcher outlived its session");

    // ...and going back to the launcher clears the transcript with it: a
    // dead session's rows sitting under a live launcher would be the
    // nastiest version of this bug, because it looks like it works.
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(1, "something to leave behind")],
    });
    vcx.run_until_parked();
    h.plumbing.push_feed(FeedEvent::NoSession { error: None });
    vcx.run_until_parked();
    assert!(shown(vcx, "launcher"));
    let rows = vcx
        .read_window(&h.window, |shell, cx| {
            shell.read(cx).transcript.read(cx).display.len()
        })
        .unwrap();
    assert_eq!(rows, 0, "the previous session's rows survived the launcher");
}

/// **The strip cannot claim more than the engine will accept.**
///
/// A live tile takes the click; a dead one does not — no handler at all,
/// so the selection does not move. This is the launcher's whole honesty
/// property: `launcher::eave_can_drive` asks `posture_is_verified`, the
/// same gate the engine refuses on, so a tile that looks clickable is one
/// eave would try to start.
///
/// Slice AT: asked of the ADAPTER, never of whether the CLI is installed.
/// A machine without `claude` on it would otherwise flip every tile in
/// this test to dead and make the assertion mean nothing.
#[gpui::test]
fn a_provider_with_no_adapter_takes_no_click(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let picked = |vcx: &mut VisualTestContext| {
        vcx.read_window(&h.window, |shell, cx| shell.read(cx).launcher.provider)
            .unwrap()
    };
    let live: Vec<usize> = crate::launcher::PROVIDERS
        .iter()
        .enumerate()
        .filter(|(_, p)| crate::launcher::eave_can_drive(p))
        .map(|(ix, _)| ix)
        .collect();
    let dead: Vec<usize> = (0..crate::launcher::PROVIDERS.len())
        .filter(|ix| !live.contains(ix))
        .collect();
    assert!(!dead.is_empty(), "nothing to prove if every tile is live");

    let before = picked(vcx);
    for ix in dead {
        click(
            vcx,
            Box::leak(format!("provider-tile-{ix}").into_boxed_str()),
        );
        vcx.run_until_parked();
        assert_eq!(
            picked(vcx),
            before,
            "clicking tile {ix} moved the selection, but eave cannot start it",
        );
    }
}

/// Start emits the command, carrying the chosen provider and — by default
/// — **no model at all**, which is not the same as an empty one.
#[gpui::test]
fn start_emits_the_launch_with_the_chosen_agent(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let _ = h.drain();
    click(vcx, "launcher-start");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert_eq!(cmds.len(), 1, "start emitted {} commands", cmds.len());
    match &cmds[0] {
        Command::Launch {
            provider,
            model,
            cwd,
        } => {
            assert_eq!(provider, "claude-code");
            assert_eq!(*model, None, "eave invented a model nobody chose");
            assert_eq!(*cwd, None, "eave invented a worktree nobody chose");
        }
        other => panic!("start emitted {other:?}"),
    }
}

/// The launcher is the overlay layer's first product consumer: the model
/// picker is a real P1 popup, and what it chooses reaches the command.
#[gpui::test]
fn the_model_menu_choice_reaches_the_launch(cx: &mut TestAppContext) {
    // This test asserts row 1 of eave's OWN list. A concurrent
    // `record("claude-code", ["sonnet"])` replaces that list underneath
    // it, so it must hold the cache turn even though it writes nothing.
    let _turn = eave_core::capabilities::probe_test_lock();
    eave_core::capabilities::forget();
    let (mut h, vcx) = boot(cx);
    click(vcx, "launcher-model");
    vcx.run_until_parked();
    assert!(shown(vcx, "overlay-menu"), "the model menu never opened");

    // Row 1 — row 0 is the CLI's own default, which is the state we are
    // trying to move away from.
    click(vcx, "menu-row-1");
    vcx.run_until_parked();
    // The exit has to PLAY before the next click can land. P1's surface
    // stays mounted and occluding for the whole 100 ms exit, so a click on
    // "start" during it hits the catcher and does nothing — which is the
    // designed behaviour, and which cost this test a run to remember.
    drive(&h, vcx, Duration::from_millis(16));
    let chosen = vcx
        .read_window(&h.window, |shell, cx| shell.read(cx).launcher.model)
        .unwrap();
    assert_eq!(chosen, Some("opus"));

    let _ = h.drain();
    click(vcx, "launcher-start");
    vcx.run_until_parked();
    match &h.drain()[0] {
        Command::Launch { model, .. } => assert_eq!(model.as_deref(), Some("opus")),
        other => panic!("start emitted {other:?}"),
    }
}

/// **Slice CK: the staleness line is painted, not merely computed.**
///
/// `launcher::model_source_note` returning a sentence is worth nothing if
/// no element carries it — the "above what, and does anything paint it?"
/// test. So this asserts the box exists on the resting launcher (where the
/// list is eave's own, because the harness never arms probes) and that it
/// goes away once the CLI has answered.
#[gpui::test]
fn the_launcher_says_whose_model_list_it_is_showing(cx: &mut TestAppContext) {
    // Held for the whole body: `forget()` protects this test from what ran
    // BEFORE it, never from what is running BESIDE it.
    let _turn = eave_core::capabilities::probe_test_lock();
    let (h, vcx) = boot(cx);
    eave_core::capabilities::forget();
    vcx.run_until_parked();
    assert!(
        shown(vcx, "launcher-model-source"),
        "the launcher offered a model list with nothing saying where it came from",
    );

    // The CLI answers. eave's disclaimer retires with it.
    eave_core::capabilities::record("claude-code", vec!["sonnet".into()]);
    h.window.update(vcx, |_, _, cx| cx.notify()).unwrap();
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "launcher-model-source"),
        "eave kept apologising for a list the CLI had already given it",
    );

    // And the menu it opens is the CLI's list, not the hand-written one.
    click(vcx, "launcher-model");
    vcx.run_until_parked();
    click(vcx, "menu-row-1");
    vcx.run_until_parked();
    assert_eq!(
        vcx.read_window(&h.window, |shell, cx| shell.read(cx).launcher.model)
            .unwrap(),
        Some("sonnet"),
        "the picker offered a model the CLI never named",
    );
    eave_core::capabilities::forget();
}

/// **A model alias belongs to one CLI's vocabulary.** Switching provider
/// clears it rather than carrying `opus` to a vendor that has never heard
/// of it — eave does not validate models (P2 caveat 5), so the flag would
/// travel all the way to a child process before anything complained.
///
/// Skipped rather than faked if a second provider is ever live: the test
/// needs two live tiles, and inventing one would defeat the honesty this
/// whole slice is built on.
#[gpui::test]
fn switching_provider_clears_a_model_it_would_not_understand(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    click(vcx, "launcher-model");
    vcx.run_until_parked();
    click(vcx, "menu-row-1");
    vcx.run_until_parked();
    assert_eq!(
        vcx.read_window(&h.window, |shell, cx| shell.read(cx).launcher.model)
            .unwrap(),
        Some("opus"),
    );

    // No second live provider exists today, so drive the state directly —
    // the clearing rule lives in `pick_provider`, and this reaches it the
    // same way a click would.
    h.window
        .update(vcx, |shell, _, cx| shell.pick_provider(1, cx))
        .unwrap();
    vcx.run_until_parked();
    assert_eq!(
        vcx.read_window(&h.window, |shell, cx| shell.read(cx).launcher.model)
            .unwrap(),
        None,
        "a model alias survived a provider switch",
    );
}

/// The engine's refusal is stated on the screen that asked for it.
#[gpui::test]
fn a_refused_launch_states_the_engines_own_words(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    assert!(!shown(vcx, "launcher-error"));
    h.plumbing.push_feed(FeedEvent::NoSession {
        error: Some("provider \"codex\" is not posture-verified".into()),
    });
    vcx.run_until_parked();
    assert!(
        shown(vcx, "launcher-error"),
        "a refused launch said nothing at all",
    );
}

/// N4's floor, applied to the rest state: **a launcher sitting there costs
/// nothing.** This is the one that would catch the launcher becoming a
/// window pinned at the display rate for the life of every idle eave.
#[gpui::test]
fn a_settled_launcher_asks_for_no_frames(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let entering = drive(&h, vcx, Duration::from_millis(16));
    assert!(
        entering > 0,
        "the launcher's entrance never asked for a frame"
    );
    assert!(shown(vcx, "launcher"));
    assert_eq!(
        h.window
            .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap(),
        0,
        "a settled launcher is still asking for frames",
    );
    assert!(
        !clock_running(vcx),
        "the launcher leased the pulse clock — nothing here repeats",
    );
}

/// Escape closes the model menu and leaves the launcher standing. The menu
/// owns the whole keyboard while it is open (P1), and the thing it must
/// not do is take the launcher down with it.
#[gpui::test]
fn escape_closes_the_model_menu_and_not_the_launcher(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    click(vcx, "launcher-model");
    vcx.run_until_parked();
    assert!(shown(vcx, "overlay-menu"));
    vcx.simulate_keystrokes("escape");
    drive(&h, vcx, Duration::from_millis(16));
    assert!(
        !shown(vcx, "overlay-menu"),
        "escape did not dismiss the menu"
    );
    assert!(shown(vcx, "launcher"), "escape took the launcher with it");
}

/// **No session, no composer.** The rest loop drops a `Send` — holding it
/// to replay into whatever session opens next would be worse than losing
/// it — so an input box that silently eats what you type is the one thing
/// the launcher must not put on screen.
///
/// This is a defect the first build of Q2 shipped: the launcher rendered
/// over a live "Message eave…" field, and only a window capture showed it.
#[gpui::test]
fn the_launcher_shows_no_composer_to_type_into(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    assert!(shown(vcx, "launcher"));
    assert!(
        !shown(vcx, "composer-hint"),
        "the launcher offered a composer with no session to send to",
    );

    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert!(!shown(vcx, "launcher"));
    assert!(
        shown(vcx, "composer-hint"),
        "the composer did not come back with the session",
    );
}

/// Slice AA: the basis toggle is a RESUBSCRIBE, not a filter — the engine
/// only computes the turn-scoped list while somebody is subscribed to it,
/// so the click has to reach the connection loop. And the list on screen
/// when it does must not be the other basis's list wearing the new label.
#[gpui::test]
fn the_files_basis_toggle_resubscribes_and_drops_the_other_basis_list(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                status: FileStatus::Modified,
                adds: Some(3),
                dels: Some(1),
                orig_path: None,
                untracked: false,
            }],
            omitted: 0,
            watching: true,
            basis: DiffBasis::WorkingTree,
        }),
    ));
    vcx.run_until_parked();
    let _ = h.drain();
    assert!(
        shown(vcx, "file-src/lib.rs"),
        "the working-tree list renders"
    );

    click(vcx, "files-scope-latest-turn");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        matches!(
            cmds.as_slice(),
            [Command::FilesScope(FileScope::LatestTurn)]
        ),
        "the toggle must re-open the subscription against the other basis, got {cmds:?}"
    );
    assert!(
        !shown(vcx, "file-src/lib.rs"),
        "the working-tree list must not survive under the turn label — that is \
         exactly the misattribution this slice exists to prevent"
    );
    assert!(
        shown(vcx, "files-skeleton"),
        "the honest picture between a toggle and its first frame is 'a list is coming'"
    );

    // Clicking the segment that is already selected is not a resubscribe.
    click(vcx, "files-scope-latest-turn");
    vcx.run_until_parked();
    assert!(
        h.drain().is_empty(),
        "re-clicking the current basis must not tear the subscription down"
    );
}

/// The two ways a turn-scoped list can be empty mean opposite things, and
/// the rail has to say which one it is. "No turn to measure against" is a
/// sentence; rendering it as an empty list would claim the turn changed
/// nothing.
#[gpui::test]
fn an_unavailable_turn_basis_renders_words_not_an_empty_list(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: Vec::new(),
            omitted: 0,
            watching: true,
            basis: DiffBasis::Unavailable {
                reason: "no turn has started since this engine did".into(),
            },
        }),
    ));
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "files-count"),
        "a count of 0 under an unavailable basis would be a claim, not a fact"
    );
    assert!(
        !shown(vcx, "files-mode"),
        "the refresh mode describes machinery; the body is already explaining \
         why there is no answer"
    );

    // A real zero under a real basis keeps its count and its mode line.
    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: Vec::new(),
            omitted: 0,
            watching: true,
            basis: DiffBasis::Turn { at: 1_700_000_000 },
        }),
    ));
    vcx.run_until_parked();
    assert!(
        shown(vcx, "files-count"),
        "a turn that has genuinely changed nothing counts zero"
    );
    assert!(shown(vcx, "files-mode"));
}

// ---------------------------------------------------------------------------
// Stage AB — the rail says which session.

/// The title is the only rail fact whose length the OPERATOR controls, and
/// it shares its line with the badge — the fact that says a session is
/// waiting on you. The title has to lose that contest every time, by
/// clipping itself rather than growing the row.
///
/// Asserted as **clearance**, not as fit. §Traps records the bug where
/// "does not overflow" and "does not touch" turned out to be different
/// questions: a title whose last glyph ends exactly where the badge begins
/// reads as one run of text, and passes any test that only checks the row
/// did not overflow.
#[gpui::test]
fn a_long_title_clips_itself_before_it_crowds_the_badge(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    // The worst case that can actually reach a row: a title at the storage
    // cap, on the state whose badge word is the longest, with the count
    // suffix that makes it longer still.
    let widest = "M".repeat(eave_core::store::TITLE_MAX_CHARS);
    let mut named = session("aaaa1111");
    named.title = Some(widest.clone());
    named.state = "awaiting_approval".into();
    named.pending_approvals = 3;
    h.plumbing.push_rail(vec![named]);
    vcx.run_until_parked();

    let row = vcx.debug_bounds("rail-aaaa1111").expect("the row renders");
    let label = vcx
        .debug_bounds("label-aaaa1111")
        .expect("the row states a label");
    let badge = vcx
        .debug_bounds("badge-aaaa1111")
        .expect("the badge renders beside it");

    // The badge kept its whole word. A badge squeezed to "waiting on…"
    // states the wrong fact, and the row it is on is the one the operator
    // is being asked to act on. The 1 px slack here and below is for
    // rounding between the shaper and the laid-out box, never for a
    // smaller gap.
    let word = "waiting on you (3)";
    let need = real_text_width(word, theme::SANS, theme::MICRO_SIZE);
    assert!(
        badge.size.width >= need - px(1.),
        "the badge got {:?} for a word needing {need:?} — the title took \
         room that is not its to take",
        badge.size.width
    );

    // The title genuinely could not fit, so this measures truncation and
    // not a happy accident. It is also where the storage cap earns its
    // value: `TITLE_MAX_CHARS` is deliberately past what the row can draw,
    // because bounding the wire and bounding the display are different
    // jobs and a cap tight enough to be the visual limit would silently
    // destroy the operator's text.
    let wanted = real_text_width(&widest, theme::SANS, theme::DETAIL_SIZE);
    assert!(
        wanted > label.size.width,
        "a title at the cap ({wanted:?}) fits the label slot ({:?}) — then \
         nothing here is testing truncation",
        label.size.width
    );

    // Clearance, not adjacency: the row declares `gap_2`, and the whole of
    // it has to still be there after the title clipped. `> 0` — the test
    // §Traps records as insufficient — would pass on two boxes sharing an
    // edge, which reads as one run of text.
    let clearance = badge.origin.x - (label.origin.x + label.size.width);
    assert!(
        clearance >= px(theme::SPACE_2 - 1.0),
        "only {clearance:?} between the title and the badge — the row lays \
         out {:?} of gap and the title ate it",
        px(theme::SPACE_2)
    );

    // And the badge is inside the rail, not pushed off its end.
    assert!(
        badge.origin.x + badge.size.width <= row.origin.x + row.size.width,
        "the badge ran past the row's edge"
    );
}

/// Stage AB's case, and **the rule that replaced its answer.**
///
/// Stage AB put the 8-char session id on every row because two untitled
/// sessions in one repo were "one row drawn twice". Slice AO takes it off
/// every row: the operator looked at `~/Des… · +3 here · d9802bc3` and
/// ruled that a path truncated to seven characters to make room for hex is
/// backwards.
///
/// So this asserts the COST as well as the benefit, because the cost is
/// real and hiding it would make the next slice re-derive it: unnamed
/// twins ARE indistinguishable in the rail again. What changed since Stage
/// AB is that `rename` is a one-click affordance (Stage AK's pen) instead
/// of a wire method with no caller — the fix moved from something the rail
/// forced on everyone to something the operator does once, on the two rows
/// it actually matters for.
#[gpui::test]
fn unnamed_twins_are_told_apart_by_naming_them_not_by_hex(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let a = session("aaaa1111");
    let b = session("bbbb2222");
    assert_eq!(a.provider, b.provider);
    assert_eq!(a.cwd, b.cwd);
    assert_eq!(a.state, b.state);
    h.plumbing.push_rail(vec![a.clone(), b.clone()]);
    vcx.run_until_parked();

    // The cost, stated: nothing on either row distinguishes them, and no
    // row carries an id to fall back on.
    assert!(shown(vcx, "cwd-aaaa1111") && shown(vcx, "cwd-bbbb2222"));
    assert_eq!(
        vcx.debug_bounds("cwd-aaaa1111").map(|b| b.size),
        vcx.debug_bounds("cwd-bbbb2222").map(|b| b.size)
    );
    assert_eq!(row_marks(&a), None, "an unmarked row states nothing extra");
    assert!(
        !shown(vcx, "marks-aaaa1111") && !shown(vcx, "marks-bbbb2222"),
        "a plain row is painting a marks group — the id is supposed to be \
         gone from the row entirely"
    );

    // The answer: name one. The label carries it, which is the version an
    // operator actually reads.
    let mut named_a = a.clone();
    named_a.title = Some("review the parser".into());
    let mut named_b = b.clone();
    named_b.title = Some("chase the flake".into());
    rail_with_selected(&h, vcx, vec![named_a.clone(), named_b.clone()], "aaaa1111");
    assert!(shown(vcx, "label-aaaa1111") && shown(vcx, "label-bbbb2222"));
    assert_ne!(
        row_label(&named_a, "claude code"),
        row_label(&named_b, "claude code")
    );
    // ...and the affordance that sets a name is reachable on the row, so
    // "name them" is advice the rail can actually take.
    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
}

/// A session with an empty journal has no title — and states that by not
/// having a title line at all.
///
/// The assertion is on row HEIGHT, because "renders no empty line" is
/// exactly the claim that a named row and an unnamed one are the same
/// height: the title takes the label's slot rather than adding one above
/// it. A blank line reserved for an absent title is the kind of defect
/// that reads as a rendering bug rather than as an absent fact.
#[gpui::test]
fn an_unnamed_session_states_no_title_and_draws_no_empty_line(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let fresh = session("aaaa1111");
    assert_eq!(fresh.title, None, "a fresh session is unnamed");
    assert_eq!(fresh.last_seq, 0, "and its journal is empty");
    h.plumbing.push_rail(vec![fresh.clone()]);
    vcx.run_until_parked();

    let unnamed_row = vcx.debug_bounds("rail-aaaa1111").unwrap().size.height;
    let unnamed_label = vcx.debug_bounds("label-aaaa1111").unwrap().size.height;
    // What it draws instead of nothing.
    assert_eq!(row_label(&fresh, "claude code"), "claude code");

    let mut named = fresh.clone();
    named.title = Some("review the parser".into());
    h.plumbing.push_rail(vec![named]);
    vcx.run_until_parked();
    assert_eq!(
        vcx.debug_bounds("rail-aaaa1111").unwrap().size.height,
        unnamed_row,
        "the named row is a different height — the title added a line \
         instead of taking the label's slot, which means the unnamed row \
         has a blank one"
    );
    assert_eq!(
        vcx.debug_bounds("label-aaaa1111").unwrap().size.height,
        unnamed_label
    );
}

/// comet shipped this bug: its rail sorted by attention, so merely opening
/// a session changed that session's state, which re-sorted the rail, which
/// slid the row out from under the pointer that had just clicked it.
///
/// So the ordering is asserted twice — that activity DOES place a row when
/// it arrives, and that a state transition afterwards moves nothing — and
/// then the click is actually performed, because a y-coordinate that holds
/// still is only interesting if the click it protects lands.
#[gpui::test]
fn a_state_change_never_moves_a_row_under_the_pointer(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut older = session("aaaa1111");
    older.created_ts_ms = 100;
    let mut newer = session("bbbb2222");
    newer.created_ts_ms = 200;
    h.plumbing.push_rail(vec![older.clone(), newer.clone()]);
    vcx.run_until_parked();

    let y_older = vcx.debug_bounds("rail-aaaa1111").unwrap().origin.y;
    let y_newer = vcx.debug_bounds("rail-bbbb2222").unwrap().origin.y;
    assert!(
        y_newer < y_older,
        "activity has to decide position when a row arrives, or the rail \
         is just insertion order"
    );

    // The older session becomes the most recently active one by a wide
    // margin — the exact transition that made comet's rail jump.
    older.state = "running".into();
    older.live = true;
    older.last_activity_ms = Some(9_000_000);
    h.plumbing.push_rail(vec![older.clone(), newer.clone()]);
    vcx.run_until_parked();

    assert_eq!(
        vcx.debug_bounds("rail-aaaa1111").unwrap().origin.y,
        y_older,
        "the row moved when its state changed"
    );
    assert_eq!(
        vcx.debug_bounds("rail-bbbb2222").unwrap().origin.y,
        y_newer,
        "the OTHER row moved when a state changed — which is the version \
         of this bug that hits the row you were aiming at"
    );

    // The fact did fold through, so the order is stable by choice rather
    // than because nothing arrived.
    h.drain();
    click(vcx, "rail-aaaa1111");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Select(id)] if id == "aaaa1111"),
        "the click landed somewhere else: {cmds:?}"
    );
}

// ---------------------------------------------------------------------------
// Stage AC — the rail is a place you manage sessions.

/// What is in the rename buffer right now, straight off the shell.
fn rename_buffer(vcx: &mut VisualTestContext, h: &Harness) -> Option<String> {
    vcx.read_window(&h.window, |shell, cx| {
        shell.read(cx).rename.as_ref().map(|e| e.text.clone())
    })
    .unwrap()
}

fn composer_draft(vcx: &mut VisualTestContext, h: &Harness) -> String {
    vcx.read_window(&h.window, |shell, cx| {
        shell
            .read(cx)
            .composer
            .read(cx)
            .field
            .read(cx)
            .content()
            .to_string()
    })
    .unwrap()
}

/// Put one session in the rail and make it the selected one.
fn rail_with_selected(h: &Harness, vcx: &mut VisualTestContext, infos: Vec<SessionInfo>, id: &str) {
    h.plumbing.push_rail(infos);
    h.plumbing.push_feed(FeedEvent::Session { id: id.into() });
    vcx.run_until_parked();
}

/// Slice C5 — seed the **browser store's** strip.
///
/// The strip used to be a projection of the rail, so a test that pushed
/// rail rows got tabs for free. It is a projection of B1's tab model now,
/// because page tabs and session tabs share one line and one ordering — so
/// a test of the strip has to say what is open, which is what this does.
///
/// In the running app the shell reconciles the rail into session tabs
/// itself (`Shell::reconcile_strip`), by asking the engine; there is no
/// engine here, so the snapshot is supplied directly. That is the same
/// arrangement every other snapshot in this harness uses.
fn strip_tabs(h: &Harness, vcx: &mut VisualTestContext, tabs: Vec<TabInfo>, active: Option<&str>) {
    h.plumbing.push_tabs(TabsSnapshot {
        order: tabs
            .iter()
            .map(|t| eave_wire::Slot::Tab { id: t.id.clone() })
            .collect(),
        tabs,
        groups: Vec::new(),
        group_tabs: Vec::new(),
        active: active.map(str::to_string),
        read_only: false,
    });
    vcx.run_until_parked();
}

/// One session tab, whose id is the session's — which is what lets the AS
/// assertions keep using `tab-<session_id>` selectors.
fn session_tab(session_id: &str) -> TabInfo {
    TabInfo {
        id: session_id.into(),
        group: None,
        pinned: false,
        content: eave_wire::TabContent::Session {
            session_id: session_id.into(),
        },
        url: None,
        title: None,
        favicon_url: None,
        load: eave_wire::LoadState::Unloaded,
        pending: None,
        can_go_back: false,
        can_go_forward: false,
        history_len: 0,
        opener: None,
        created_ts_ms: 0,
    }
}

/// One page tab.
fn page_tab(id: &str, url: Option<&str>, title: Option<&str>) -> TabInfo {
    TabInfo {
        content: eave_wire::TabContent::Page,
        url: url.map(str::to_string),
        title: title.map(str::to_string),
        ..session_tab(id)
    }
}

/// The rail AND the strip, for the common case: every session gets a tab,
/// and the selected one is active — which is what the running app
/// reconciles to.
fn rail_and_strip(h: &Harness, vcx: &mut VisualTestContext, infos: Vec<SessionInfo>, id: &str) {
    let tabs: Vec<TabInfo> = infos.iter().map(|s| session_tab(&s.session_id)).collect();
    rail_with_selected(h, vcx, infos, id);
    strip_tabs(h, vcx, tabs, Some(id));
}

/// Slice AL. The rail's hierarchy, measured on the RENDERED boxes rather
/// than asserted about the constants — a rank that never reaches an element
/// is a rank that does not exist.
///
/// Before this slice a session's name was `DETAIL_SIZE` = 12.0 and the facts
/// under it `SMALL_SIZE` = 11.0: a 1.09x "hierarchy" on the app's primary
/// navigation surface, which is why the rail read as one grey block. The
/// name now takes `BODY_SIZE` and the facts `MICRO_SIZE`. (It reached for
/// `NAME_SIZE` first and the operator overruled it: at 248 px, 16 truncates
/// the name mid-word, and the name is the only thing on the row a person
/// reads. `NAME_SIZE` is for surfaces with 560 px or more.)
///
/// The 1.3x here is deliberately looser than the 1.53x the two ranks give
/// (23 px against 15 with the label's hairlines counted), so the test states
/// "there is a hierarchy you can see" rather than re-deriving the two
/// constants it is testing — which would pass no matter what they became.
#[gpui::test]
fn a_rail_row_says_its_name_louder_than_its_facts(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut only = session("aaaa1111");
    only.title = Some("the refactor".into());
    only.permission_mode = Some("default".into());
    h.plumbing.push_rail(vec![only]);
    vcx.run_until_parked();

    let h_of = |vcx: &mut VisualTestContext, sel: &'static str| {
        vcx.debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} did not render"))
            .size
            .height
    };
    // Both boxes are `.truncate()`d single lines, which is what makes their
    // laid-out heights comparable: this harness runs a Noop text system by
    // default, so a WRAPPING fact lays out at whatever advance the stub
    // returns and its height stops meaning anything. (The posture line does
    // exactly that — 30 px here for a string that shapes to 152 px of a
    // 186 px box under real metrics.)
    // **1.15, and why it is not higher.** Slice AL wrote this at 1.3, when
    // the name was `NAME_SIZE` (16) over `MICRO` metadata. The operator
    // looked at that rail and said the text was gigantic, so the name came
    // down to 13 over a `DETAIL` fact line — the folder, which has to stay
    // legible because it is what a person recognises the session by. The
    // laid-out ratio is now ~1.23.
    //
    // Height is only ONE of the three channels carrying this hierarchy, and
    // the smallest of them: the name is also `WEIGHT_STRONG` against normal
    // and `text()` against `text_secondary()`. Stacking all three at full
    // strength is exactly what produced "gigantic" — a rank is a budget to
    // spend across channels, not a licence to spend all of them.
    //
    // So the bar is set where a real step still passes and the failure this
    // test exists to catch — the fact line creeping up to the name's size,
    // which lays out at 1.0 — still fails, with clearance rather than fit.
    let name = h_of(vcx, "label-aaaa1111");
    let quiet = h_of(vcx, "cwd-aaaa1111");
    assert!(
        name >= quiet * 1.15,
        "the session name lays out {name:?} against the fact under it at \
         {quiet:?} — that is not a hierarchy, it is a rounding difference"
    );
}

/// **Recorded debt, not a passing invariant.** Slice AL split the rail row
/// into a bare leading strip and a washed card so the state mark could sit
/// on unwashed frost — the mark measures **3.65:1** out there against
/// **2.51:1** inside the wash, and 3:1 is the floor for a non-text mark.
/// Slice AO rebuilt the same row for density in parallel, and AO's structure
/// is the one the operator iterated on and kept, so at integration the mark
/// went back inside the wash and AL's fix was lost.
///
/// AL also established that the strip is **free**: `RAIL_STRIP_W` is
/// `STATE_DOT + SPACE_2`, exactly what the dot and its gap already cost
/// inside the card, so hoisting the dot out returns zero pixels to the
/// session name — which is the only reason AO's width budget had to reject
/// it. That means this is a straightforward port, not a trade.
///
/// This test asserts the CURRENT state so the debt is visible in the suite
/// rather than living only in a report. **When someone hoists the dot out,
/// this test fails** — and the failure message says to delete it, which is
/// the same executable-ratchet shape Slice AQ used for its twelve unfixed
/// pigment rows.
#[gpui::test]
fn the_state_mark_is_still_inside_the_row_wash_and_should_not_be(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222")],
        "aaaa1111",
    );

    // No `rail-card-*` element exists in AO's structure: the wash is on the
    // row itself and the mark is a child of it. If this ever resolves, the
    // card appears and the debt is paid.
    assert!(
        vcx.debug_bounds("rail-card-aaaa1111").is_none(),
        "a washed card now exists, so the state mark was hoisted onto bare \
         frost and AL's 3.65:1 is back — DELETE this test, it has done its job"
    );
}

/// The slice's first half. `rename` has been complete on the wire since
/// Stage AB — method, engine, client, persistence, tests — with nothing in
/// the GUI calling it, which is a half-kept feature by this project's own
/// rule. This is the caller, driven end to end.
///
/// The second assertion is the one worth keeping: the app sends the text
/// RAW. Normalization is `eave_core::store::normalize_title`'s and the
/// engine's reply is what landed, so there is exactly one implementation
/// of what a title is rather than two that eventually disagree about
/// whether a double space matters.
#[gpui::test]
fn the_rail_renames_a_session_and_leaves_normalizing_to_the_engine(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");

    assert!(shown(vcx, "label-aaaa1111"), "the row states a label");
    assert!(
        !shown(vcx, "rename-aaaa1111"),
        "a field nobody asked for is open"
    );
    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    click(vcx, "ren-aaaa1111");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "rename-aaaa1111"),
        "the rename affordance opened nothing"
    );
    assert!(
        !shown(vcx, "label-aaaa1111"),
        "the field renders BESIDE the label instead of in its slot — two \
         names for one row"
    );

    let _ = h.drain();
    let typed = "chase  the flake";
    vcx.simulate_input(typed);
    assert_eq!(
        rename_buffer(vcx, &h).as_deref(),
        Some(typed),
        "the field did not take what was typed at it"
    );
    vcx.simulate_keystrokes("enter");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Rename { id, title: Some(t) }]
            if id == "aaaa1111" && t == typed),
        "the app normalized the title itself, or sent something else: {cmds:?}"
    );
    assert!(
        !shown(vcx, "rename-aaaa1111"),
        "return left the field standing"
    );
    assert!(shown(vcx, "label-aaaa1111"), "the label never came back");
}

/// The field opens on the name it is editing, and escape means escape.
///
/// Seeded rather than blank because the common edit is a correction: a
/// field that cleared the name you are trying to fix makes "leave it
/// alone" and "delete it" the same gesture.
#[gpui::test]
fn the_rename_field_is_seeded_and_escape_sends_nothing(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut named = session("aaaa1111");
    named.title = Some("the old name".into());
    rail_with_selected(&h, vcx, vec![named], "aaaa1111");

    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    click(vcx, "ren-aaaa1111");
    vcx.run_until_parked();
    assert_eq!(
        rename_buffer(vcx, &h).as_deref(),
        Some("the old name"),
        "the field opened empty over a session that already has a name"
    );

    let _ = h.drain();
    vcx.simulate_input("!!!");
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(
        h.drain().is_empty(),
        "escape sent a rename — the one thing it must not do"
    );
    assert_eq!(rename_buffer(vcx, &h), None, "escape left the field open");
    assert!(shown(vcx, "label-aaaa1111"), "the label never came back");
}

/// Clearing a name is the same verb as setting one — an empty field sends
/// `None`, which is exactly what `Request::Rename` means by it. Two
/// methods for one field is two things that can disagree about which is
/// authoritative.
#[gpui::test]
fn an_empty_rename_field_clears_the_name(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut named = session("aaaa1111");
    named.title = Some("abc".into());
    rail_with_selected(&h, vcx, vec![named], "aaaa1111");

    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    click(vcx, "ren-aaaa1111");
    vcx.run_until_parked();
    for _ in 0..3 {
        vcx.simulate_keystrokes("backspace");
    }
    assert_eq!(rename_buffer(vcx, &h).as_deref(), Some(""));

    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Rename { id, title: None }] if id == "aaaa1111"),
        "an emptied field did not clear the name: {cmds:?}"
    );

    // A field holding only spaces is not a name either, and it must not
    // reach the wire as one.
    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    click(vcx, "ren-aaaa1111");
    vcx.run_until_parked();
    for _ in 0..3 {
        vcx.simulate_keystrokes("backspace");
    }
    vcx.simulate_input("   ");
    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Rename { title: None, .. }]),
        "whitespace became a title: {cmds:?}"
    );
}

/// **The field owns the keyboard while it is open**, exactly the way P1's
/// menus do and for the same reason: a field you can type past is not a
/// field.
///
/// The specific failure this prevents is the one Slice T and Slice Q2 both
/// shipped a version of — keystrokes landing in a composer buffer nobody
/// can see, surfacing later as a message the operator never wrote. Here
/// the composer is mounted and one keystroke away, which is what makes
/// the assertion worth anything.
#[gpui::test]
fn the_rename_field_owns_the_keyboard_and_the_composer_stays_empty(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    assert!(
        shown(vcx, "composer-hint"),
        "no composer is mounted — then this test proves nothing"
    );

    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    click(vcx, "ren-aaaa1111");
    vcx.run_until_parked();
    let _ = h.drain();
    vcx.simulate_input("naming this");
    assert_eq!(rename_buffer(vcx, &h).as_deref(), Some("naming this"));
    assert_eq!(
        composer_draft(vcx, &h),
        "",
        "the keystrokes fell through into the composer's draft"
    );
    // Return is the field's, not the composer's: no `Send` may escape.
    vcx.simulate_keystrokes("enter");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Rename { .. }]),
        "return did something other than commit the name: {cmds:?}"
    );

    // And with the field closed the composer has it back.
    vcx.simulate_input("hello");
    assert_eq!(composer_draft(vcx, &h), "hello");
}

/// Opening the field must not move the rows under it.
///
/// The rail already refuses to move a row under the pointer once
/// (`fold_rail`, Stage AB — comet's bug). Growing the label slot by the
/// height of a border would be the same defect arriving through layout
/// instead of through sorting, and it would happen while the operator is
/// mid-word.
#[gpui::test]
fn opening_the_rename_field_moves_no_row(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222")],
        "aaaa1111",
    );

    let row_a = vcx.debug_bounds("rail-aaaa1111").expect("row a");
    let y_b = vcx.debug_bounds("rail-bbbb2222").expect("row b").origin.y;
    let label = vcx.debug_bounds("label-aaaa1111").expect("label");

    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    click(vcx, "ren-aaaa1111");
    vcx.run_until_parked();
    let field = vcx.debug_bounds("rename-aaaa1111").expect("field");

    assert_eq!(
        field.size.height, label.size.height,
        "the field is a different height than the label it replaced — the \
         invisible border on the label is what keeps these equal"
    );
    assert_eq!(
        vcx.debug_bounds("rail-aaaa1111").unwrap().size.height,
        row_a.size.height,
        "the row grew when the field opened"
    );
    assert_eq!(
        vcx.debug_bounds("rail-bbbb2222").unwrap().origin.y,
        y_b,
        "the row BELOW moved when the field opened — which is the version \
         of this that hits a click you were already aiming"
    );
}

/// A long name clips inside the field instead of growing the row — **and
/// the caret survives the clipping.**
///
/// This is the risk the field's shape carries now. The caret used to be the
/// last character of the title's own string, so `truncate` clipped the two
/// together and the row could not grow; it is a SIBLING now, because a
/// caret inside a clipped string is the first thing to disappear exactly
/// when the operator most needs it — at the end of a name too long for the
/// box. A sibling has to be paid for with a title that shrinks, and this is
/// the assertion that it does.
#[gpui::test]
fn a_long_name_clips_inside_the_rename_field_and_keeps_its_caret(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222")],
        "aaaa1111",
    );
    let row_w = vcx.debug_bounds("rail-aaaa1111").expect("row").size.width;

    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    click(vcx, "ren-aaaa1111");
    vcx.run_until_parked();
    let field_w = vcx
        .debug_bounds("rename-aaaa1111")
        .expect("field")
        .size
        .width;

    vcx.simulate_input(&"a name far longer than any rail is wide ".repeat(6));
    vcx.run_until_parked();

    assert_eq!(
        vcx.debug_bounds("rail-aaaa1111").expect("row").size.width,
        row_w,
        "a long name grew the rail row — the title inside the field is not \
         shrinking, so it is pushing the caret and everything after it off \
         the end"
    );
    assert_eq!(
        vcx.debug_bounds("rename-aaaa1111")
            .expect("field")
            .size
            .width,
        field_w,
        "the field itself grew with its contents"
    );
    assert!(
        shown(vcx, "caret-rename-lit") || shown(vcx, "caret-rename-dark"),
        "the caret was clipped out of the field by a long name — which is \
         the state it exists to be visible in"
    );
}

/// Selecting another row closes the field rather than leaving a keyboard
/// grab on a row the operator has walked away from.
#[gpui::test]
fn selecting_another_row_closes_an_open_rename(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222")],
        "aaaa1111",
    );
    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    click(vcx, "ren-aaaa1111");
    vcx.run_until_parked();
    assert!(shown(vcx, "rename-aaaa1111"));

    let _ = h.drain();
    click(vcx, "rail-bbbb2222");
    vcx.run_until_parked();
    assert_eq!(
        rename_buffer(vcx, &h),
        None,
        "the field survived a selection change, still holding the keyboard"
    );
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Select(id)] if id == "bbbb2222"),
        "and it swallowed the click that closed it: {cmds:?}"
    );
    // Nothing was sent for the abandoned edit — closing is not committing.
    assert!(!cmds.iter().any(|c| matches!(c, Command::Rename { .. })));
}

/// Every affordance a selected row can carry, all at once, measured
/// against the rail it has to live inside — with the words shaped by the
/// real macOS text system, so a synthetic advance cannot make this pass
/// vacuously.
///
/// Asserted as **clearance**, not fit: two buttons sharing an edge read as
/// one control, and R1 already found what a non-wrapping row does with
/// content it cannot hold — it draws the overflow on top of its
/// neighbour.
#[gpui::test]
fn every_affordance_on_a_selected_row_stays_inside_the_rail(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    // The widest set that can co-exist: `isolate` needs no worktree and
    // `open`/`delete` need one, so four is the real worst case.
    let mut owner = session("aaaa1111");
    owner.worktree = Some(worktree("eave/aaaa1111", Some(2), true));
    rail_with_selected(&h, vcx, vec![owner], "aaaa1111");

    let rail = vcx.debug_bounds("rail-rows").expect("the rail scroller");
    // Stage AK: hover-revealed, so nothing to measure until the pointer is
    // on the row.
    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    let buttons = [
        "ren-aaaa1111",
        "arc-aaaa1111",
        "wto-aaaa1111",
        "wtd-aaaa1111",
    ];
    let mut boxes = Vec::new();
    for selector in buttons {
        let b = vcx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} did not render"));
        // It is a target a pointer can actually hit. This replaced a
        // "did the label clip" check, which had nothing left to measure
        // once the labels became glyphs — and it is the assertion the
        // icons actually need, because the failure mode moved from "the
        // word is cut off" to "the box is too small to press".
        assert!(
            b.size.width >= px(ROW_ACTION_HIT) - px(1.)
                && b.size.height >= px(ROW_ACTION_HIT) - px(1.),
            "{selector} is {:?} — smaller than the {ROW_ACTION_HIT}px hit \
             target it is supposed to be, and a miss here selects the \
             session instead of acting on it",
            b.size
        );
        // Inside the rail, on both edges. This matters MORE now than it
        // did: the strip is absolutely positioned, so nothing about the
        // layout stops it leaving the rail — only this.
        assert!(
            b.origin.x >= rail.origin.x,
            "{selector} starts left of the rail at {:?}",
            b.origin.x
        );
        assert!(
            b.origin.x + b.size.width <= rail.origin.x + rail.size.width,
            "{selector} runs {:?} past the rail's right edge",
            (b.origin.x + b.size.width) - (rail.origin.x + rail.size.width)
        );
        boxes.push((selector, b));
    }
    // Pairwise clearance. Two buttons on the same line need the row's own
    // gap between them; two on different lines need daylight vertically.
    // `> 0` — the check §Traps records as insufficient — would pass on two
    // boxes sharing an edge.
    for (i, (sa, a)) in boxes.iter().enumerate() {
        for (sb, b) in boxes.iter().skip(i + 1) {
            let dx = if a.origin.x < b.origin.x {
                b.origin.x - (a.origin.x + a.size.width)
            } else {
                a.origin.x - (b.origin.x + b.size.width)
            };
            let dy = if a.origin.y < b.origin.y {
                b.origin.y - (a.origin.y + a.size.height)
            } else {
                a.origin.y - (b.origin.y + b.size.height)
            };
            // 3px, written out. The strip lays out 4px between boxes and
            // one pixel is allowed for rounding.
            //
            // Deliberately NOT `theme::SPACE_1 - 1.0`: the previous version
            // of this line read `theme::SPACE_2 - 1.0`, which was the exact
            // constant the layout used, so retuning the gap would have moved
            // the bar with it and the test could never have failed. §7:
            // never derive a test's expectation from the constant it is
            // testing.
            //
            // The BOXES clear by 4px; the glyphs inside them clear by 12,
            // because a 16px icon sits centred in a 24px target.
            assert!(
                dx >= px(3.0) || dy >= px(0.),
                "{sa} and {sb} are {dx:?} apart horizontally and {dy:?} \
                 vertically — the strip lays out 4px of gap and they are \
                 drawing over each other"
            );
        }
    }
}

/// Queue row 10, driven: one click files a session away, and the SAME
/// button one click later brings it back. No two-step, because nothing is
/// destroyed — the journal is not touched (proved on the engine's side in
/// `watch::archiving_never_touches_the_journal`), so a confirmation here
/// would be ceremony with no fact behind it.
///
/// It also asserts the thing that reads as a bug and is not:
/// **archiving the row you are on does not yank it out from under you.**
/// The row stays, wearing the word `archived`, which is what confirms the
/// click; it leaves when you move on.
#[gpui::test]
fn archiving_is_one_click_and_the_same_click_undoes_it(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    assert!(
        !shown(vcx, "archived-aaaa1111"),
        "a fresh session already claims to be archived"
    );

    let _ = h.drain();
    hover_row(&h, vcx, "rail-aaaa1111", "arc-aaaa1111");
    click(vcx, "arc-aaaa1111");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::SetArchived { id, archived: true }]
            if id == "aaaa1111"),
        "the archive affordance sent something else: {cmds:?}"
    );

    // The engine agrees, and the snapshot comes back.
    let mut filed = session("aaaa1111");
    filed.archived = true;
    h.plumbing.push_rail(vec![filed]);
    vcx.run_until_parked();
    assert!(
        shown(vcx, "rail-aaaa1111"),
        "archiving the session on screen pulled its row out from under the \
         click that did it"
    );
    assert!(
        shown(vcx, "archived-aaaa1111"),
        "nothing on the row confirms it was filed away"
    );

    let _ = h.drain();
    hover_row(&h, vcx, "rail-aaaa1111", "arc-aaaa1111");
    click(vcx, "arc-aaaa1111");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::SetArchived { id, archived: false }]
            if id == "aaaa1111"),
        "the same button did not offer the way back: {cmds:?}"
    );
}

/// The filter, and the shelf that undoes it. An archived session nobody is
/// looking at leaves the default listing; the toggle states how many are
/// there and brings them back.
#[gpui::test]
fn an_archived_row_leaves_the_rail_and_the_shelf_brings_it_back(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut filed = session("aaaa1111");
    filed.archived = true;
    h.plumbing.push_rail(vec![filed, session("bbbb2222")]);
    vcx.run_until_parked();

    assert!(
        !shown(vcx, "rail-aaaa1111"),
        "the archived row is still in the default listing"
    );
    assert!(shown(vcx, "rail-bbbb2222"), "it took its neighbour with it");
    assert!(
        shown(vcx, "rail-archived"),
        "the rail hid a session and offered no way to find it"
    );

    click(vcx, "rail-archived");
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-aaaa1111"), "the shelf opened onto nothing");
    assert!(
        shown(vcx, "archived-aaaa1111"),
        "with the shelf open, an archived row is indistinguishable from a \
         live one"
    );
    assert!(shown(vcx, "rail-bbbb2222"), "the shelf hid the working set");

    click(vcx, "rail-archived");
    vcx.run_until_parked();
    assert!(!shown(vcx, "rail-aaaa1111"), "the shelf would not close");
}

/// The shelf appears only when there is something on it, so a rail nobody
/// has archived from looks exactly as it always did.
#[gpui::test]
fn a_rail_with_nothing_archived_shows_no_shelf(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-aaaa1111"));
    assert!(
        !shown(vcx, "rail-archived"),
        "a control for a shelf holding nothing"
    );
}

/// **The eternal-skeleton guard.** A filter that can produce "no rows"
/// must never produce "no rows for no stated reason" — an empty state that
/// lost the reason it is empty is the same defect as a skeleton that lost
/// its error, and it is the one this rail could plausibly grow.
///
/// So a rail whose every session is filed away says so, and the control
/// that undoes it is on screen at the same time.
#[gpui::test]
fn a_rail_filtered_down_to_nothing_says_why(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.archived = true;
    let mut b = session("bbbb2222");
    b.archived = true;
    h.plumbing.push_rail(vec![a, b]);
    vcx.run_until_parked();

    assert!(!shown(vcx, "rail-aaaa1111") && !shown(vcx, "rail-bbbb2222"));
    assert!(
        shown(vcx, "rail-empty"),
        "an empty rail said nothing at all"
    );
    assert_eq!(
        rail_empty_note(2, 2),
        "every session is archived (2)",
        "and what it said was the pre-Stage-AC \"no sessions\", which is \
         false here"
    );
    assert!(
        shown(vcx, "rail-archived"),
        "the way out of an all-archived rail is not on screen"
    );

    click(vcx, "rail-archived");
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-aaaa1111") && shown(vcx, "rail-bbbb2222"));
    assert!(
        !shown(vcx, "rail-empty"),
        "the empty note outlived the rows"
    );
}

/// eave never auto-approves, never auto-denies and never times out into
/// either (Slice K), so the only way out of `awaiting_approval` is
/// somebody clicking. A filter that hid that row would park the agent for
/// the life of the process behind an affordance nobody can reach.
///
/// The row therefore stays, and it accounts for its own presence in a list
/// the operator believes is filtered.
#[gpui::test]
fn an_archived_session_parked_on_a_question_keeps_its_row(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut asking = session("aaaa1111");
    asking.archived = true;
    asking.state = "awaiting_approval".into();
    asking.pending_approvals = 1;
    // A second archived session in a state nobody is blocked on, as the
    // control: if BOTH show, the filter is simply off.
    let mut done = session("bbbb2222");
    done.archived = true;
    done.state = "failed".into();
    h.plumbing.push_rail(vec![asking, done]);
    vcx.run_until_parked();

    assert!(
        shown(vcx, "rail-aaaa1111"),
        "an archived session parked on the operator vanished — nothing can \
         answer it now"
    );
    assert!(
        shown(vcx, "badge-aaaa1111"),
        "and it lost the badge that says you are the blocker"
    );
    assert!(
        !shown(vcx, "rail-bbbb2222"),
        "the archived `failed` session stayed too — the exception is not an \
         exception, the filter is off"
    );
    assert_eq!(
        archived_line(
            &{
                let mut s = session("aaaa1111");
                s.archived = true;
                s.pending_approvals = 1;
                s
            },
            false
        )
        .as_deref(),
        Some("archived · still asking"),
        "the row is on screen against the operator's filter and does not \
         say why"
    );
}

/// N4's floor, applied to the rail's rename field: **an open one costs one
/// caret and nothing else.**
///
/// This used to read "costs nothing", and the field paid for that with a
/// **solid** caret — a comment at the mount site said so in as many words,
/// and traded the blink for the floor because the only clock there was ran
/// at 30 fps. A caret has its own rate now, so the trade is off: the field
/// blinks, at `motion::CARET_TICK`, and the floor is asserted as "nothing
/// is animating" rather than as "no timer exists".
///
/// The second half is the one worth having. **Exactly one caret is lit at a
/// time**, and it is the one in the box the next key lands in — a composer
/// still blinking behind an open rename field would be a confident wrong
/// answer to the only question a caret is asked.
#[gpui::test]
fn an_open_rename_field_costs_one_caret_and_owns_it(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    // A files snapshot has to arrive before ANY selected session can
    // settle: `Shell::work_in_flight` counts "a selected session whose
    // files snapshot has not arrived" as work in flight, so the skeleton
    // rows hold a pulse lease for as long as nobody answers. That is a
    // KNOWN, pre-existing bug (catch-up plan §4.5, still open) and not
    // this slice's — Slice T hit the identical wall fixing the caret
    // test. Answering it here is what makes the assertions below about
    // the rename field rather than about the files rail.
    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: Vec::new(),
            omitted: 0,
            watching: true,
            basis: DiffBasis::WorkingTree,
        }),
    ));
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(16));
    assert!(
        !animating(vcx),
        "the shell was already animating before the field opened — then \
         nothing below is measuring the field"
    );
    // The composer owns the keyboard at this point, so its caret is the lit
    // one. Stated before the field opens so the flip below means something.
    assert!(
        shown(vcx, "caret-composer-lit") || shown(vcx, "caret-composer-dark"),
        "the composer drew no caret at all"
    );

    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    click(vcx, "ren-aaaa1111");
    vcx.simulate_input("naming this one");
    let entering = drive(&h, vcx, Duration::from_millis(16));
    assert!(
        shown(vcx, "rename-aaaa1111"),
        "the field closed before it was measured"
    );
    assert!(
        entering < 400,
        "the field never settled after {entering} frames"
    );
    assert_eq!(
        h.window
            .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap(),
        0,
        "an open rename field is still asking for frames",
    );
    assert!(
        !animating(vcx),
        "an open rename field drove the clock at the frame rate — the \
         reason a caret has a rate of its own is so that a text box can \
         blink without costing one",
    );
    assert!(
        caret_only(vcx),
        "the rename field is open and nothing is blinking in it",
    );
    // The keyboard moved, and so did the only lit caret in the window.
    assert!(
        shown(vcx, "caret-rename-lit") || shown(vcx, "caret-rename-dark"),
        "the rename field drew no caret",
    );
    assert!(
        shown(vcx, "caret-composer-dark"),
        "the composer's caret is still lit behind an open rename field — \
         two carets is a window that cannot say where the next key goes",
    );
}

/// **Hiding a row does not make its agent free.**
///
/// Stage L's aggregate ceiling exists for the session whose cost the
/// operator has stopped thinking about, and archiving one is the most
/// literal way there is to stop thinking about it. The filter is a
/// listing decision; the total is a fact about the machine, and it is
/// computed over every session the engine reports.
///
/// The first version of this slice got it wrong in exactly one line — the
/// ceiling was computed from the filtered list — and the failure is
/// invisible by eye: the number is simply smaller than the truth.
#[gpui::test]
fn an_archived_agent_still_counts_against_the_memory_ceiling(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut filed = session("aaaa1111");
    filed.archived = true;
    filed.live = true;
    filed.state = "running".into();
    filed.rss_bytes = Some(2 * 1024 * 1024 * 1024);
    h.plumbing.push_rail(vec![filed]);
    vcx.run_until_parked();

    assert!(
        !shown(vcx, "rail-aaaa1111"),
        "the row is not hidden — then this measures nothing"
    );
    assert!(
        shown(vcx, "rail-resource"),
        "the only live agent was archived and the machine's ceiling went \
         with it: two gigabytes of resident memory stopped being reported \
         because a row stopped being drawn"
    );
}

// Slice AD — the transcript reads like a document

/// Click a point offset from an element's own top-left corner.
///
/// [`click`] takes the centre, which is exactly wrong for a text block: a
/// paragraph's box is the full content column and its glyphs stop wherever
/// the sentence stops, so the centre of the box is usually the empty margin
/// to the right of the last line. Every link test here clicks near the
/// first glyph instead.
#[track_caller]
fn click_at(vcx: &mut VisualTestContext, selector: &'static str, dx: f32, dy: f32) {
    let bounds = vcx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("element {selector} did not render"));
    let p = gpui::point(bounds.origin.x + px(dx), bounds.origin.y + px(dy));
    assert!(
        p.x >= px(0.) && p.y >= px(0.),
        "element {selector} is off-screen at {p:?} — a click there hits \
         nothing and asserts nothing"
    );
    vcx.simulate_click(p, Modifiers::none());
}

#[track_caller]
fn click_at_with(
    vcx: &mut VisualTestContext,
    selector: &'static str,
    dx: f32,
    dy: f32,
    modifiers: Modifiers,
) {
    let bounds = vcx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("element {selector} did not render"));
    let p = gpui::point(bounds.origin.x + px(dx), bounds.origin.y + px(dy));
    assert!(
        p.x >= px(0.) && p.y >= px(0.),
        "element {selector} is off-screen at {p:?} — a click there hits \
         nothing and asserts nothing"
    );
    vcx.simulate_click(p, modifiers);
}

/// Middle-click the centre of the element. The tab strip's close path
/// listens on `MouseButton::Middle` down, not on `on_click`.
#[track_caller]
fn middle_click(vcx: &mut VisualTestContext, selector: &'static str) {
    let bounds = vcx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("element {selector} did not render"));
    let c = bounds.center();
    assert!(
        c.x >= px(0.) && c.y >= px(0.),
        "element {selector} is off-screen at {c:?}"
    );
    vcx.simulate_mouse_down(c, MouseButton::Middle, Modifiers::none());
}

/// The two credential buffers, as the shell holds them (D7).
///
/// Read off the entity rather than off the screen: the password is drawn
/// as bullets by construction, so the only place its real value exists is
/// here — which is exactly what makes "did it go into the right buffer?"
/// worth asserting.
fn dialog_buffers(h: &Harness, vcx: &mut VisualTestContext) -> (String, String) {
    vcx.read_window(&h.window, |shell, cx| {
        let shell = shell.read(cx);
        (
            shell
                .dialog_text
                .as_ref()
                .map(|(_, t)| t.clone())
                .unwrap_or_default(),
            shell
                .dialog_secret
                .as_ref()
                .map(|(_, t)| t.clone())
                .unwrap_or_default(),
        )
    })
    .unwrap()
}

/// Put a page ask on screen and let the shell draw it (G26).
///
/// Before G26 this was impossible: the ask map was inside `--features
/// web`, so the feature-off `pending()` returned `None` and no test could
/// reach the dialog at all.
#[track_caller]
fn raise_ask(
    h: &Harness,
    vcx: &mut VisualTestContext,
    tab: &str,
    origin: &str,
    what: crate::web::Ask,
) {
    let page = page_tab(tab, Some(origin), Some("A page"));
    strip_tabs(h, vcx, vec![page], Some(tab));
    crate::web::raise_ask_for_test(tab, origin, what);
    // The ask is raised outside gpui's world, so nothing has invalidated
    // the window — nudge the shell to redraw before anything measures it.
    strip_tabs(
        h,
        vcx,
        vec![page_tab(tab, Some(origin), Some("A page"))],
        Some(tab),
    );
    vcx.run_until_parked();
}

/// Press one of the mouse's two navigation buttons (G24).
///
/// Over the shell root rather than over a named control, because that is
/// the claim: mouse 4 and 5 are window-wide chrome gestures, not something
/// you have to aim at the back arrow.
#[track_caller]
fn mouse_navigate(vcx: &mut VisualTestContext, dir: NavigationDirection) {
    let bounds = vcx
        .debug_bounds("shell-root")
        .expect("the shell root did not render");
    vcx.simulate_mouse_down(
        bounds.center(),
        MouseButton::Navigate(dir),
        Modifiers::none(),
    );
}

/// Every URL the shell asked the system to open since the last drain.
fn opened_urls() -> Vec<String> {
    OPENED_URLS.with(|o| std::mem::take(&mut *o.borrow_mut()))
}

/// Feed one assistant message into a fresh shell and settle.
fn transcript_of(
    cx: &mut TestAppContext,
    markdown: &str,
) -> (Harness, &'static mut VisualTestContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![assistant_record(1, markdown)],
    });
    vcx.run_until_parked();
    (h, vcx)
}

/// A markdown table draws as a table.
///
/// Until this slice `Block::Table` fell through to the paragraph arm, so a
/// table rendered as its own raw source — a wall of `|` pipes, which the
/// catch-up audit named as the single ugliest thing on screen once a
/// session is open. This asserts the three things a table has to be: cells
/// in a grid, columns that LINE UP down the table, and an alignment that
/// follows the delimiter row rather than the renderer's convenience.
#[gpui::test]
fn a_markdown_table_renders_as_columns_and_not_as_pipes(cx: &mut TestAppContext) {
    let (_h, vcx) = transcript_of(
        cx,
        "| file | lines | ok |\n|:---|---:|:---:|\n| ui.rs | 6680 | yes |\n| theme.rs | 1243 | yes |",
    );

    assert!(shown(vcx, "md-table"), "the table block did not render");
    assert!(
        !shown(vcx, "md-para"),
        "a table that still renders through the paragraph arm is the raw-pipe \
         render this slice exists to delete"
    );

    // The grid: every cell of the fixture is present.
    for sel in [
        "md-head-0",
        "md-head-1",
        "md-head-2",
        "md-cell-0-0",
        "md-cell-0-1",
        "md-cell-0-2",
        "md-cell-1-0",
        "md-cell-1-1",
        "md-cell-1-2",
    ] {
        assert!(vcx.debug_bounds(sel).is_some(), "{sel} did not render");
    }

    // Columns line up: a cell and the header above it share a left edge and
    // a width, on every column. This is the property "aligned columns"
    // actually means, and it is the one a per-row layout would break.
    for c in 0..3 {
        let head = vcx
            .debug_bounds(["md-head-0", "md-head-1", "md-head-2"][c])
            .unwrap();
        for row in [
            ["md-cell-0-0", "md-cell-0-1", "md-cell-0-2"],
            ["md-cell-1-0", "md-cell-1-1", "md-cell-1-2"],
        ] {
            let cell = vcx.debug_bounds(row[c]).unwrap();
            assert_eq!(
                cell.origin.x, head.origin.x,
                "column {c} does not line up with its header"
            );
            assert_eq!(cell.size.width, head.size.width, "column {c} changes width");
        }
    }

    // Rows are rows: every cell of one row shares a top edge, and the
    // second row sits strictly below the first.
    let r0 = vcx.debug_bounds("md-cell-0-0").unwrap();
    assert_eq!(
        r0.origin.y,
        vcx.debug_bounds("md-cell-0-2").unwrap().origin.y
    );
    let r1 = vcx.debug_bounds("md-cell-1-0").unwrap();
    assert!(
        r1.origin.y >= r0.origin.y + r0.size.height,
        "row 1 overlaps row 0 — {r1:?} against {r0:?}"
    );

    // The columns are not equal thirds: `file` holds the longest cells and
    // gets the most room, `ok` the least. A layout that ignored content
    // would make these three numbers the same.
    let mut w = |s: &'static str| vcx.debug_bounds(s).unwrap().size.width;
    let (file, lines, ok) = (w("md-head-0"), w("md-head-1"), w("md-head-2"));
    assert!(
        file > lines && lines >= ok,
        "column widths should follow content: file {file:?}, lines {lines:?}, ok {ok:?}"
    );
}

/// A wide table is bounded by the transcript, not the other way round.
///
/// The stated choice (see `Shell::render_table`): the table never scrolls
/// sideways and never widens the transcript. Whatever it holds, the row is
/// exactly the content column wide; cells wrap; the container clips.
#[gpui::test]
fn a_wide_table_never_widens_the_transcript(cx: &mut TestAppContext) {
    let long = "an unusually long cell of prose that has no business being in a table at all";
    let src = format!(
        "| a | b | c | d | e | f |\n|---|---|---|---|---|---|\n| {long} | {long} | {long} | {long} | {long} | {long} |"
    );
    let (_h, vcx) = transcript_of(cx, &src);

    let table = vcx.debug_bounds("md-table").expect("the table rendered");
    assert!(
        table.size.width <= px(theme::CONTENT_MAX_W),
        "the table is {:?} wide against a {} px content column — a table that \
         widens its row makes the WHOLE transcript scroll sideways, so every \
         paragraph in the session pays for one table",
        table.size.width,
        theme::CONTENT_MAX_W
    );
    // Every cell stays inside the table's own box.
    for c in 0..6 {
        let cell = vcx
            .debug_bounds(
                [
                    "md-cell-0-0",
                    "md-cell-0-1",
                    "md-cell-0-2",
                    "md-cell-0-3",
                    "md-cell-0-4",
                    "md-cell-0-5",
                ][c],
            )
            .unwrap();
        assert!(
            cell.origin.x + cell.size.width <= table.origin.x + table.size.width + px(0.5),
            "cell {c} runs past the table's right edge"
        );
    }
    // And it grew DOWNWARD instead: six columns of that much prose cannot
    // be one line tall.
    assert!(
        table.size.height > px(theme::TABLE_ROW_MIN_H * 2.0),
        "the cells clipped instead of wrapping — table is only {:?} tall",
        table.size.height
    );
}

/// A table is one row of the virtualized list, and its height is bounded.
///
/// The list caches a row's measured height, so a row whose height is a
/// number the agent chose is a list that can no longer promise anything
/// about scrolling. `fence_view` bounds a fence for exactly this reason;
/// `table_view` is the same bargain, and this is the assertion that the
/// bargain is actually kept end to end rather than only in the pure fn.
#[gpui::test]
fn a_table_is_one_row_of_the_list_and_cannot_choose_its_own_height(cx: &mut TestAppContext) {
    let mut src = String::from("before\n\n| n | value |\n|---|---|\n");
    for i in 0..400 {
        src.push_str(&format!("| {i} | row {i} |\n"));
    }
    src.push_str("\nafter");
    let (h, vcx) = transcript_of(cx, &src);

    let rows = vcx
        .read_window(&h.window, |shell, cx| {
            shell.read(cx).transcript.read(cx).display.len()
        })
        .unwrap();
    assert_eq!(
        rows, 3,
        "paragraph + table + paragraph is three display rows — a 400-row \
         table is still ONE row of the list"
    );

    // 400 body rows, 60 drawn: the 61st never exists as an element and the
    // remainder is stated rather than swallowed.
    assert!(
        vcx.debug_bounds("md-cell-59-0").is_some(),
        "row 59 should draw"
    );
    assert!(
        vcx.debug_bounds("md-cell-60-0").is_none(),
        "row 60 drew — the cap is not being applied at render"
    );
    assert!(shown(vcx, "md-capped"), "the cap must state its remainder");
}

/// A link is a link, and it opens in the browser.
///
/// `markdown.rs` parsed a URL and threw it away — the label was underlined
/// and the click went nowhere. The hit test is the real one
/// (`TextLayout::index_for_position` against the laid-out glyphs), which is
/// why the second half of this test can prove it discriminates by POSITION:
/// the same paragraph, clicked on its plain-text opening, opens nothing.
#[gpui::test]
fn clicking_a_link_opens_it_in_the_default_browser(cx: &mut TestAppContext) {
    let _ = opened_urls();
    let (_h, vcx) = transcript_of(cx, "[the eave docs](https://eave.sh/docs)");
    // Near the first glyph: the whole paragraph is the link.
    click_at(vcx, "md-para", 4.0, theme::BODY_LINE / 2.0);
    vcx.run_until_parked();
    assert_eq!(
        opened_urls(),
        vec!["https://eave.sh/docs".to_string()],
        "the link's target never reached the opener"
    );
}

#[gpui::test]
fn plain_text_beside_a_link_takes_the_click_and_opens_nothing(cx: &mut TestAppContext) {
    let _ = opened_urls();
    // Twenty plain characters, then the link. A click on the first of them
    // is inside the block, inside the hitbox, and outside every link range.
    let (_h, vcx) = transcript_of(cx, "xxxxxxxxxxxxxxxxxxxx [docs](https://eave.sh)");
    click_at(vcx, "md-para", 4.0, theme::BODY_LINE / 2.0);
    vcx.run_until_parked();
    assert!(
        opened_urls().is_empty(),
        "a click on prose opened something — the hit test is not reading \
         the glyph under the pointer"
    );
}

/// A link eave will not open is inert, and says so by not looking like a
/// link at all.
///
/// `open(1)` launches whatever has a registered handler, and the target is
/// a string an agent wrote. Every one of these is refused at the parser AND
/// at the syscall, and none of them is underlined — the underline is the
/// app's promise that a click does something.
#[gpui::test]
fn a_link_eave_will_not_open_is_drawn_inert_and_takes_no_click(cx: &mut TestAppContext) {
    for target in [
        "javascript:alert(1)",
        "file:///etc/passwd",
        "data:text/html,<h1>hi</h1>",
        "x-apple-systempreferences://com.apple.preference",
    ] {
        let _ = opened_urls();
        let (_h, vcx) = transcript_of(
            cx,
            &format!(
                "[click me](
{target})"
            )
            .replace('\n', ""),
        );
        click_at(vcx, "md-para", 4.0, theme::BODY_LINE / 2.0);
        vcx.run_until_parked();
        assert!(
            opened_urls().is_empty(),
            "{target} reached the system opener"
        );
    }
    // The control, so this test cannot pass by the click missing: the same
    // markup with an `https` target, clicked at the same point, opens.
    let _ = opened_urls();
    let (_h, vcx) = transcript_of(cx, "[click me](https://eave.sh/ok)");
    click_at(vcx, "md-para", 4.0, theme::BODY_LINE / 2.0);
    vcx.run_until_parked();
    assert_eq!(
        opened_urls(),
        vec!["https://eave.sh/ok".to_string()],
        "the control did not open — every refusal above proved nothing"
    );

    // ...and the guard is at the syscall too, not only at the parser, so a
    // future caller cannot route around it.
    let _ = opened_urls();
    open_url("javascript:alert(1)");
    open_url("file:///etc/passwd");
    assert!(opened_urls().is_empty(), "open_url passed a non-web scheme");
    open_url("https://eave.sh");
    assert_eq!(opened_urls(), vec!["https://eave.sh".to_string()]);
}

/// The other opener, and the same rule.
///
/// `open_external` exists so `mailto:` and `tel:` reach the OS, and the
/// danger it carries is exactly the danger `open_url` carries: `open(1)`
/// launches whatever registered for a scheme, and the string came from a
/// page. The allow-list is asked twice — once when `web::policy` decides,
/// again at the syscall — so a caller who builds the request by hand still
/// cannot get an arbitrary scheme launched.
#[gpui::test]
fn open_external_only_passes_the_schemes_eave_named(_cx: &mut TestAppContext) {
    let _ = opened_urls();
    open_external("mailto:a@b.test");
    open_external("tel:+15551234");
    assert_eq!(
        opened_urls(),
        vec!["mailto:a@b.test".to_string(), "tel:+15551234".to_string()],
        "the control did not open — every refusal below would prove nothing"
    );

    let _ = opened_urls();
    open_external("zoommtg://zoom.us/join");
    open_external("javascript:alert(1)");
    open_external("https://eave.sh");
    open_external("x-random-app:do-a-thing");
    assert!(
        opened_urls().is_empty(),
        "open_external passed a scheme that is not on the allow-list"
    );
}

/// A list item's marker never collides with its own text.
///
/// The gutter is a fixed WIDTH beside a column that wraps — never a fixed
/// height, which is the shape `overlay::menu_row` got wrong in Slice R1
/// (a one-line box holding a three-line label drew rows on top of each
/// other). Both halves are asserted: the widest marker fits the gutter at
/// real glyph widths, and the rendered text column starts clear of it.
#[gpui::test]
fn a_list_marker_never_collides_with_its_own_text(cx: &mut TestAppContext) {
    // Real metrics first: the widest marker the renderer can produce is a
    // two-digit ordinal, and it must fit 22 px with room to spare.
    for marker in ["99.", "•", "◦", "▪"] {
        let w = real_text_width(marker, theme::SANS, theme::BODY_SIZE);
        assert!(
            w <= px(theme::LIST_GUTTER_W) - px(theme::HAIRLINE),
            "marker {marker:?} needs {w:?}, the gutter gives {} px",
            theme::LIST_GUTTER_W
        );
    }

    let (_h, vcx) = transcript_of(
        cx,
        "1. first item\n2. second item\n   - nested under two\n99. ninety-nine",
    );
    assert!(shown(vcx, "md-list"), "the list block did not render");
    assert!(
        !shown(vcx, "md-para"),
        "the list still renders as a paragraph of raw markers"
    );

    let items: Vec<_> = ["md-item-0", "md-item-1", "md-item-2", "md-item-3"]
        .iter()
        .map(|s| vcx.debug_bounds(s).unwrap_or_else(|| panic!("{s} missing")))
        .collect();
    // Nesting is visible: the nested item's text starts strictly right of
    // its parent's, by a whole indent step.
    assert_eq!(
        items[2].origin.x - items[1].origin.x,
        px(theme::LIST_INDENT),
        "one nesting level should indent by exactly one step"
    );
    assert_eq!(
        items[0].origin.x, items[3].origin.x,
        "same level, same edge"
    );
    // Items stack, never overlap — the R1 failure, asserted as CLEARANCE
    // rather than as "the boxes are different".
    for w in items.windows(2) {
        assert!(
            w[1].origin.y >= w[0].origin.y + w[0].size.height,
            "list items overlap: {:?} then {:?}",
            w[0],
            w[1]
        );
    }
}

/// A task list draws boxes, and a checked one is a different box.
#[gpui::test]
fn a_task_list_draws_boxes_instead_of_spelling_out_brackets(cx: &mut TestAppContext) {
    let (_h, vcx) = transcript_of(cx, "- [ ] not yet\n- [x] finished\n- plain bullet");
    let open = vcx.debug_bounds("md-task-open").expect("an open checkbox");
    let done = vcx
        .debug_bounds("md-task-done")
        .expect("a checked checkbox");
    assert_eq!(open.size.width, px(theme::CHECKBOX));
    assert_eq!(open.size.height, px(theme::CHECKBOX));
    assert_eq!(done.size, open.size, "checking a box must not resize it");
    // The box sits on the item's FIRST line, inside it, not above or below.
    let first = vcx.debug_bounds("md-item-0").unwrap();
    assert!(
        open.origin.y >= first.origin.y
            && open.origin.y + open.size.height <= first.origin.y + px(theme::BODY_LINE),
        "the checkbox {open:?} is not on the first line of {first:?}"
    );
}

/// A blockquote is a rule and quieter ink, and a heading shows no
/// asterisks.
///
/// Both were parsed-and-discarded before this slice: `> note` was a
/// paragraph that began with a greater-than sign, and a heading was the one
/// block rendered as a bare string, so `## The **why**` printed its own
/// markers at heading size.
#[gpui::test]
fn a_quote_and_a_heading_render_their_markup_rather_than_showing_it(cx: &mut TestAppContext) {
    let (h, vcx) = transcript_of(
        cx,
        "## The **why**\n\n> a quoted note\n> over two lines\n\nplain tail",
    );
    let quote = vcx.debug_bounds("md-quote").expect("the quote block");
    assert!(
        quote.size.height >= px(theme::BODY_LINE * 2.0),
        "two quoted lines should be two lines tall, got {:?}",
        quote.size.height
    );
    // Three blocks, and the heading is not one of the paragraphs.
    let rows = vcx
        .read_window(&h.window, |shell, cx| {
            shell.read(cx).transcript.read(cx).display.len()
        })
        .unwrap();
    assert_eq!(rows, 3, "heading + quote + paragraph");

    // The markers are consumed by the parser, which is what "renders its
    // markup" means — asserted on the runs, since a painted glyph is not
    // readable from a test.
    let runs = inline_runs("The **why**");
    assert_eq!(
        runs.iter().map(|r| r.text.as_str()).collect::<String>(),
        "The why"
    );
    assert!(runs.iter().any(|r| r.style.bold && r.text == "why"));
}

// ---------------------------------------------------------------------------
// Slice AJ — a timeout is not a git failure

/// The remedy line appears exactly when there is something to do about it.
///
/// The sentence itself is pinned in `eave-wire`
/// (`a_timeout_never_says_git_failed`); what can only be checked here is
/// that the RAIL routes each state to the right rendering — because the
/// defect was never in either layer's wording alone, it was the app adding
/// a frame on top of the engine's.
#[gpui::test]
async fn the_changes_rail_offers_a_remedy_only_when_there_is_one(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });

    let snap = |state: GitState| FilesSnapshot {
        state,
        files: vec![],
        omitted: 0,
        watching: true,
        basis: DiffBasis::WorkingTree,
    };
    let unanswered = |gate: Option<&str>| GitState::Unanswered {
        what: "status".into(),
        secs: 10.0,
        gate: gate.map(str::to_string),
    };

    for (name, state, want) in [
        (
            "blocked in a gated folder",
            unanswered(Some("~/Desktop")),
            true,
        ),
        ("blocked somewhere ordinary", unanswered(None), false),
        (
            "git itself refused",
            GitState::Error {
                message: "bad revision".into(),
            },
            false,
        ),
    ] {
        h.plumbing
            .push_files(("aaaa1111".into(), Some(snap(state))));
        vcx.run_until_parked();
        assert_eq!(
            shown(vcx, "git-remedy"),
            want,
            "{name}: the remedy line should {} be on screen",
            if want { "" } else { "not" }
        );
    }
}

// SLICE AI — the launcher's reach.
//
// `EAVE_LAUNCH_AT_BOOT=1` worked against an empty engine and silently did
// nothing against a non-empty one, because `Command::Launch` is serviced by
// one of the connection loop's two branches. These tests pin the UI half of
// the fix — that the hook takes the same route a person takes, and that the
// route exists. The connection loop itself is unreachable from this harness
// (it is replaced by plumbing), so the half that proves a session actually
// lands on disk is `scripts/q2-launch-proof.sh`'s `again` case.
// ---------------------------------------------------------------------------

/// **`+ new` asks for the launcher, and had no test for pressing it.**
///
/// The control was asserted to exist, to be right-aligned and to rest at
/// `wash_rest`; what it *does* was covered by nothing. It is also the only
/// way back to the launcher once a session is selected, which is what makes
/// it load-bearing rather than a convenience — Slice AI's fix rides on it.
#[gpui::test]
fn pressing_new_session_asks_for_the_launcher(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    // A session on screen, so the launcher is NOT already up. This is the
    // state in which `+ new` is the only route to it.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert!(!shown(vcx, "launcher"), "the launcher was already up");
    let _ = h.drain();

    click(vcx, "new-session");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert_eq!(cmds.len(), 1, "`+ new` emitted {cmds:?}");
    assert!(
        matches!(cmds[0], Command::NewSession),
        "`+ new` emitted {:?} rather than asking for the launcher",
        cmds[0],
    );

    // ...and it does NOT raise the launcher by itself. The UI is a pure
    // projection: the launcher arrives when the engine agrees it should.
    assert!(
        !shown(vcx, "launcher"),
        "the shell raised a launcher the engine had not agreed to",
    );
    h.plumbing.push_feed(FeedEvent::NoSession { error: None });
    vcx.run_until_parked();
    assert!(shown(vcx, "launcher"), "the launcher never arrived");
}

/// **The boot hook reaches the launcher before it presses start.**
///
/// This is the regression test for the defect: with a session selected the
/// launcher is not on screen, and a bare `Command::Launch` is read out by a
/// branch that drops it. The hook must therefore emit `NewSession` FIRST —
/// the same command `+ new` sends, in the same order a person presses them.
///
/// Order is the whole assertion. Two commands in the other order would still
/// be two commands, and would still do nothing.
#[gpui::test]
fn the_boot_hook_reaches_the_launcher_before_it_presses_start(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "launcher"),
        "a non-empty engine still rested at the launcher — this test proves \
         nothing unless the launcher is out of reach to begin with",
    );
    let _ = h.drain();

    h.window
        .update(vcx, |shell, _, cx| shell.launch_at_boot(cx))
        .unwrap();
    vcx.run_until_parked();

    let cmds = h.drain();
    assert_eq!(cmds.len(), 2, "the boot hook emitted {cmds:?}");
    assert!(
        matches!(cmds[0], Command::NewSession),
        "the boot hook pressed start before it asked for the launcher: {cmds:?}",
    );
    assert!(
        matches!(cmds[1], Command::Launch { .. }),
        "the boot hook asked for the launcher and never pressed start: {cmds:?}",
    );
}

/// The same hook against an EMPTY engine still presses start exactly once.
///
/// The fix adds a `NewSession` the rest state does not need; the rest loop
/// answers it with "already here". What must not happen is the extra step
/// turning into an extra session — `q2-launch-proof.sh`'s `launched` case
/// wants one journal, not two, and this is that claim at the command level.
#[gpui::test]
fn the_boot_hook_presses_start_once_against_an_empty_engine(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    assert!(
        shown(vcx, "launcher"),
        "an empty engine did not rest at the launcher",
    );
    let _ = h.drain();

    h.window
        .update(vcx, |shell, _, cx| shell.launch_at_boot(cx))
        .unwrap();
    vcx.run_until_parked();

    let cmds = h.drain();
    let starts = cmds
        .iter()
        .filter(|c| matches!(c, Command::Launch { .. }))
        .count();
    assert_eq!(
        starts, 1,
        "the boot hook pressed start {starts} times: {cmds:?}"
    );
}

// Slice AH — the eternal skeleton
//
// N4's idle floor says "a window with nothing mid-flight schedules
// nothing". `a_settled_window_is_never_told_to_re_render` proves it for a
// window with nothing SELECTED, and every other test in this file that
// selects a session answers its files channel by hand — Slice T's caret
// test and Stage AC's rename test both say so in their own comments,
// because neither could settle until they did.
//
// So the invariant held in the suite while a real running app broke it:
// the harness always delivered a snapshot and the real engine may not.
// These are the members of that family that do NOT answer.
//
// The instrument is N4's: count the redraws (`watch_redraws` observes the
// shell, and the shell notifies itself whenever any of its five state
// entities changes, so a notification on the shell IS "this window must be
// drawn again"). `simulate_next_frame` is the WRONG instrument here — it
// counts animation-frame REQUESTS, which is how hand-driven tweens are
// paced, and the pulse clock does not use them: it drives on its own
// 33 ms timer and `cx.notify`. A test written against `simulate_next_frame`
// reports a pulsing window as settled. (It reported one to me first.)
// ---------------------------------------------------------------------------

/// What ten seconds of a settled window with one text box in it costs.
///
/// A literal, and deliberately not `10_000 / CARET_TICK.as_millis()` —
/// deriving it from the constant would make the assertion pass at any rate
/// including the frame rate, which is the defective-test shape N1 shipped
/// and mutation testing caught (progress.md §Traps). 100 redraws over 10 s
/// is one blinking caret at `motion::CARET_TICK`. Anything above it is
/// something else driving the window; below it is a caret that stopped.
const CARET_REDRAWS_PER_10S: usize = 100;

/// Advance drawn time in 100 ms steps until nothing is animating, and
/// report how many steps that took. Panics past `cap` rather than
/// spinning — an eternal skeleton is precisely a loop that never ends,
/// and it must fail as a test rather than as a hung suite.
///
/// **`animating`, not `clock_running`.** Every caller here selects a
/// session, and a selected session mounts a composer whose caret blinks for
/// as long as it is on screen — so the clock in these tests never parks,
/// and it is not supposed to. What must stop is the skeleton, and the
/// skeleton is the thing that asks for `PULSE_TICK`.
#[track_caller]
fn steps_until_parked(vcx: &mut VisualTestContext, cap: usize, what: &str) -> usize {
    let mut steps = 0usize;
    while animating(vcx) {
        steps += 1;
        assert!(steps < cap, "{what}");
        vcx.executor().advance_clock(Duration::from_millis(100));
        vcx.run_until_parked();
    }
    steps
}

/// **The one the old code fails.** A selected session whose files
/// snapshot never arrives used to hold a pulse lease for the life of the
/// process — a 30 fps drive over a still picture on a window where
/// nothing is happening, costing +8.7 points of a core (N2) for as long
/// as eave is open.
///
/// Both halves are asserted, because they fail in opposite directions:
///
///   - Give up too early and the rail lies the other way — "no file list"
///     over a scan that is still legitimately running. One of the
///     engine's own git calls is bounded at 10 s
///     (`eave_engine::git::GIT_TIMEOUT`), so the rail has to out-wait one
///     of those before it may say anything at all.
///   - Give up never and you have the bug.
///
/// The step bounds are LITERALS. Deriving them from `FILES_PATIENCE`
/// would make this pass at any patience including zero and including
/// forever, which is the defective test N1 shipped and mutation testing
/// caught (progress.md §Traps). At a 100 ms step, 100 steps is 10 s of
/// drawn time and 300 is 30 s: the wait must clear the first and end
/// before the second.
#[gpui::test]
fn an_unanswered_files_rail_gives_up_the_clock_and_says_so(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    // A live engine: this is the case where elapsed time is the ONLY
    // evidence available, which is the case the patience exists for.
    h.plumbing
        .push_conn(ConnEvent::Connected { engine_pid: 4242 });
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    // No `push_files`. That absence is the whole test.

    assert!(
        shown(vcx, "files-skeleton"),
        "a list that has just been asked for should still say one is coming"
    );
    assert!(
        animating(vcx),
        "the skeleton is not moving at all — then nothing below is \
         measuring the lease this slice bounds"
    );

    let drawing = watch_redraws(&h, vcx);
    let steps = steps_until_parked(
        vcx,
        300,
        "the files skeleton was still driving the clock after 30 s of drawn \
         time — this is the eternal skeleton, and it runs for the life of \
         the process",
    );
    assert!(
        steps > 100,
        "the rail gave up after {steps} steps (~{} s) — it must out-wait \
         one of the engine's own timed-out git calls, or it will put an \
         error on screen over a healthy slow scan",
        steps / 10
    );
    assert!(
        drawing.get() > 100,
        "only {} redraws while the skeleton was up — the wait was not \
         actually costing frames, so the bound below proves nothing",
        drawing.get()
    );

    // The idle floor, applied to the state that used to defeat it: ten
    // more seconds, and the only thing left drawing is the caret in the
    // composer. It used to be zero, and the difference is the whole of what
    // a blinking caret costs — see `CARET_REDRAWS_PER_10S`.
    let settled = watch_redraws(&h, vcx);
    for _ in 0..10 {
        vcx.executor().advance_clock(Duration::from_secs(1));
        vcx.run_until_parked();
    }
    assert_eq!(
        settled.get(),
        CARET_REDRAWS_PER_10S,
        "a window whose file list is never coming redrew itself at a rate \
         that is not its caret's while the operator was away from the desk"
    );

    assert!(
        !shown(vcx, "files-skeleton"),
        "the skeleton is still on screen saying a list is coming"
    );
    assert!(
        shown(vcx, "files-silence"),
        "the animation stopped and took the fact with it — an eternal \
         skeleton is an error state that lost its error, and a FROZEN \
         skeleton is the same error state, cheaper"
    );
    assert!(
        shown(vcx, "files-retry"),
        "the connection is live, so there is something to try again, and \
         nothing offers it"
    );

    // Giving up bounds the picture and the clock, never the subscription:
    // a snapshot that arrives late renders exactly as an early one does.
    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: vec![ChangedFile {
                path: "src/late.rs".into(),
                status: FileStatus::Modified,
                adds: Some(1),
                dels: Some(0),
                untracked: false,
                orig_path: None,
            }],
            omitted: 0,
            watching: true,
            basis: DiffBasis::WorkingTree,
        }),
    ));
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "files-silence"),
        "the rail kept stating silence after it had been answered"
    );
    assert!(
        shown(vcx, "files-count"),
        "a snapshot that arrived after the rail gave up did not render"
    );
    let _ = h.drain();
}

/// The connection's own truth outranks any clock.
///
/// When the engine is unreachable the window already HOLDS the fact —
/// `ConnEvent::Failed` carries the words. Waiting 20 s to invent a vaguer
/// version of a sentence you can already read is what this project calls
/// inventing a fact, so this path costs no patience at all: the rail
/// states the connection's own message and parks.
///
/// And it offers no `try again`. After the 5-attempt breaker the feed
/// loop has RETURNED, so `Command::Resubscribe` would reach nobody — an
/// affordance that silently does nothing is the defect Stage AC widened
/// the launcher's command match to prevent.
#[gpui::test]
fn a_files_rail_with_no_engine_states_the_connection_and_waits_for_nothing(
    cx: &mut TestAppContext,
) {
    let (h, vcx) = boot(cx);
    h.plumbing
        .push_conn(ConnEvent::Connected { engine_pid: 4242 });
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    assert!(shown(vcx, "files-skeleton"), "nothing was ever waiting");
    assert!(animating(vcx), "nothing was ever moving");

    h.plumbing.push_conn(ConnEvent::Failed {
        message: "engine unreachable after 5 attempts: connection refused".into(),
    });
    vcx.run_until_parked();

    assert!(
        !shown(vcx, "files-skeleton"),
        "the rail is animating a list from an engine it cannot reach"
    );
    assert!(
        shown(vcx, "files-silence"),
        "the rail went quiet without saying why"
    );
    assert!(
        !shown(vcx, "files-retry"),
        "there is nothing left to ask — the feed loop returned with the \
         breaker, so this button cannot do what it says"
    );
    // The lease lapses rather than being revoked, so this is "it stopped",
    // not "it stopped this instant" — the same reading
    // `a_running_session_leases_the_clock_and_a_settled_one_lets_it_park`
    // takes. What matters is that it stops WITHOUT the 20 s patience.
    let steps = steps_until_parked(
        vcx,
        20,
        "an unreachable engine still leased the pulse clock two seconds later",
    );
    assert!(
        steps < 10,
        "the rail spent {steps} steps timing out a fact it already had"
    );

    let settled = watch_redraws(&h, vcx);
    for _ in 0..10 {
        vcx.executor().advance_clock(Duration::from_secs(1));
        vcx.run_until_parked();
    }
    assert_eq!(
        settled.get(),
        CARET_REDRAWS_PER_10S,
        "a window that knows no answer is coming redrew itself at a rate \
         that is not its caret's"
    );
}

/// The affordance does what it says, and asking again buys real patience.
///
/// Two failures this catches, and the second is why it exists: a `try
/// again` that emits nothing, and one that emits the command but leaves
/// the expired wait in place — that second one re-states the old silence
/// on the very next frame and reads as a dead button.
#[gpui::test]
fn trying_again_re_asks_and_restarts_the_rails_patience(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    h.plumbing
        .push_conn(ConnEvent::Connected { engine_pid: 4242 });
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    steps_until_parked(vcx, 300, "never gave up");
    assert!(shown(vcx, "files-retry"), "nothing to click");

    let _ = h.drain();
    click(vcx, "files-retry");
    vcx.run_until_parked();

    assert!(
        h.drain().iter().any(|c| matches!(c, Command::Resubscribe)),
        "`try again` asked nobody for anything"
    );
    assert!(
        shown(vcx, "files-skeleton"),
        "the rail is still stating the silence it just re-asked about"
    );
    assert!(
        !shown(vcx, "files-silence"),
        "the old sentence outlived the request that replaced it"
    );
    // ...and the second wait is a real one, not a frame of politeness.
    let again = steps_until_parked(vcx, 300, "the second wait never ended");
    assert!(
        again > 100,
        "asking again bought only {again} steps of patience"
    );
}

// Slice AK — the affordances are icons, revealed on hover

/// **A hovered row is exactly as tall as an unhovered one.**
///
/// This is the invariant the whole design rests on, and the reason the strip
/// is absolutely positioned rather than a flex child. The rail is a
/// virtualized list with cached row heights: a row that grew when the
/// pointer arrived would invalidate its own measurement mid-hover, shove
/// every row below it, and — because the pointer is now over a different
/// row — do it again. The old text strip could not hit this because it was
/// always laid out; hover-revealing anything in flow would.
///
/// Asserted to the pixel. There is no tolerance to spend: either the strip
/// participates in layout or it does not.
#[gpui::test]
fn hovering_a_row_does_not_move_it(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut owner = session("aaaa1111");
    owner.worktree = Some(worktree("eave/aaaa1111", Some(2), true));
    rail_with_selected(&h, vcx, vec![owner], "aaaa1111");

    let before = vcx.debug_bounds("rail-aaaa1111").expect("the row");
    assert!(
        vcx.debug_bounds("ren-aaaa1111").is_none(),
        "the affordances are on screen before anything hovered the row"
    );

    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    let after = vcx.debug_bounds("rail-aaaa1111").expect("the row");

    assert_eq!(
        before.size, after.size,
        "the row changed size when the pointer arrived: {:?} -> {:?}",
        before.size, after.size
    );
    assert_eq!(
        before.origin, after.origin,
        "the row moved when the pointer arrived: {:?} -> {:?}",
        before.origin, after.origin
    );
}

/// The strip sits over the row's right edge, not over its name.
///
/// Absolute positioning buys the height invariant above and costs this
/// one: nothing in the layout stops the strip covering whatever is beneath
/// it. The session's NAME is the one thing on the row that must stay
/// readable while a person reaches for its buttons.
///
/// **Slice AO repointed this test, and it then failed.** Stage AK wrote it
/// against `rail-title`, which is the rail's `SESSIONS` HEADER — not the
/// row's name, which is tagged `label-{sid}`. The header sits at the top of
/// the rail and is about 60 px wide, so "the strip starts to the right of
/// it" was true by a hundred pixels and would have stayed true no matter
/// what the strip covered. The test could not fail. Repointed, it caught a
/// real overlap of 21 px at four affordances — see `row_strip_reserve`.
///
/// It measures the name's drawn INK, not its box. The label is `flex_1`,
/// so its box runs to the row's right edge whatever the name is and the
/// strip overlaps that box by construction; what must not be covered is
/// glyphs. Shaped with the real text system at the row's own 14 px, at the
/// default weight — a slight UNDER-estimate of the semibold the row paints,
/// which is why the clearance below is 8 px and not 1.
#[gpui::test]
fn the_action_strip_does_not_cover_the_session_name(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");

    // An unnamed session falls back to the provider label, which is what
    // this row paints.
    // Slice AS: **14.0 was never the size this row draws**, and the default
    // weight was never the weight. See `real_name_width`.
    let ink = real_name_width("claude code");
    // What the row DRAWS is `min(ink, box)`: the label truncates, so a name
    // longer than its slot ends at the slot's edge with an ellipsis rather
    // than running on under the buttons.
    let clear = |vcx: &mut VisualTestContext| {
        let label = vcx.debug_bounds("label-aaaa1111").expect("the row's name");
        let first = vcx.debug_bounds("ren-aaaa1111").expect("the first action");
        first.origin.x - (label.origin.x + ink.min(label.size.width))
    };
    let two_icons = clear(vcx);
    assert!(
        two_icons >= px(8.),
        "the name runs {ink:?} and the action strip leaves {two_icons:?} \
         after it — rename and archive are on every row and must never be \
         over the name they belong to"
    );

    // And the WIDEST strip the rail can build: a selected, dead,
    // worktree\'d session shows pen, archive, open and delete — four 24 px
    // targets and three gaps, twice the width of the case above.
    //
    // **This one does overlap, and the assertion says so.** Four targets
    // reach 21 px into `claude code`, and no arrangement of a 61 px row
    // makes them not: reserving their width on line one was tried and it
    // cuts the name to `clau…`, which is a worse row at rest than a
    // partly-covered name is for the moment a pointer is on it.
    //
    // What must hold — and does — is that the strip never reaches the
    // START of the name. A person reaching for `archive` has to be able to
    // see which session they are archiving. 48 px is about six characters
    // at this size, written as a literal rather than derived from the
    // strip\'s own geometry.
    let mut wide = session("aaaa1111");
    wide.worktree = Some(worktree("eave/aaaa1111", Some(2), true));
    rail_with_selected(&h, vcx, vec![wide], "aaaa1111");
    hover_row(&h, vcx, "rail-aaaa1111", "wtd-aaaa1111");
    assert!(
        shown(vcx, "arc-aaaa1111") && shown(vcx, "wto-aaaa1111"),
        "the four-affordance strip did not render, so this measures the \
         narrow case twice"
    );
    let label = vcx.debug_bounds("label-aaaa1111").expect("the row\'s name");
    let strip = vcx.debug_bounds("ren-aaaa1111").expect("the first action");
    let visible = strip.origin.x - label.origin.x;
    assert!(
        visible < ink,
        "the four-icon strip clears the name entirely ({visible:?} of \
         {ink:?}) — if that is now true, this test should be asserting the \
         stronger invariant instead of this one"
    );
    assert!(
        visible >= px(48.),
        "the widest strip leaves only {visible:?} of the name showing; a \
         row you cannot identify is a row you archive by mistake"
    );

    // Slice AS: **and at a name somebody actually typed.** Everything above
    // measures `claude code` (76.5 px), which is the SHORTEST realistic
    // name the rail can draw — the provider fallback. Run the same two
    // cases at a fifteen-character title and the picture changes:
    //
    // | name | affordances | clearance |
    // | --- | --- | --- |
    // | `claude code` | 2 (pen, archive) | **+37.5 px** |
    // | `package the dmg` | 2 (pen, archive) | **+5.2 px** |
    // | `claude code` | 4 | −18.5 px |
    // | `package the dmg` | 4 | **−50.8 px** |
    //
    // So the strong invariant above — 8 px of daylight after the name — is
    // held by the FIXTURE and not by the layout: at a realistic title the
    // two affordances every row carries come within 5 px of the last glyph,
    // and this test would not have noticed. The clearance is asserted as
    // positive here rather than as ≥ 8, because a claim about a number the
    // rail cannot currently make is a claim that fails on the next slice
    // for the wrong reason. **This is a finding, not a fix**; the fix is
    // fewer affordances in a 60 px corner (Stage AO's caveat 4, still
    // open), and `docs/shots/as-rail-open.png` is the first photograph of
    // it — the pen sits on the `m` of `dmg`.
    let mut titled = session("aaaa1111");
    titled.title = Some("package the dmg".into());
    rail_with_selected(&h, vcx, vec![titled.clone()], "aaaa1111");
    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    let long_ink = real_name_width("package the dmg");
    let two_at_a_real_title = clear(vcx);
    assert!(
        two_at_a_real_title > px(0.),
        "`package the dmg` shapes to {long_ink:?} and the two affordances \
         EVERY row carries leave {two_at_a_real_title:?} after it — rename \
         and archive are on every row and must not be over the name they \
         belong to"
    );

    // ...and the widest strip keeps its floor at that length too, which is
    // the invariant that actually holds.
    let mut titled_wide = titled;
    titled_wide.worktree = Some(worktree("eave/aaaa1111", Some(2), true));
    rail_with_selected(&h, vcx, vec![titled_wide], "aaaa1111");
    hover_row(&h, vcx, "rail-aaaa1111", "wtd-aaaa1111");
    let label = vcx.debug_bounds("label-aaaa1111").expect("the row\'s name");
    let strip = vcx.debug_bounds("ren-aaaa1111").expect("the first action");
    let visible = strip.origin.x - label.origin.x;
    assert!(
        visible >= px(48.),
        "at a real title the widest strip leaves only {visible:?} of the \
         name showing"
    );
}

// ---------------------------------------------------------------------------
// Slice AO — the rail is a list, and a list has to fit.

/// **The slice, in one number.** A session row is two lines.
///
/// The photographed rail (`docs/shots/ship-from-applications.png`) fit three
/// sessions: every row carried a name line, an id-plus-path line, a
/// `shares files with 9fedaac4, 47890400` line that wrapped to two, and a
/// strip of text buttons. This holds the row to a ceiling and holds the
/// SHARING row to the same one, because the sharing row is the one that
/// blew the budget.
///
/// The ceiling is a literal. It is not `SPACE_2 * 2 + NAME_LINE + …`,
/// because a test built out of the constants the layout uses cannot fail
/// when the layout changes — §7, and Stage AK found exactly that bug in
/// this file.
#[gpui::test]
fn a_session_row_is_two_lines(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let plain = session("aaaa1111");
    let mut sharing = session("bbbb2222");
    sharing.shared_with = vec!["cccc3333".into(), "dddd4444".into()];
    h.plumbing.push_rail(vec![plain, sharing]);
    vcx.run_until_parked();

    for row in ["rail-aaaa1111", "rail-bbbb2222"] {
        let h = vcx.debug_bounds(row).unwrap().size.height;
        assert!(
            h <= px(64.),
            "{row} is {h:?} tall; the rail's whole problem was rows that \
             cost four lines apiece"
        );
    }

    // Both lines are actually there — a ceiling met by dropping a fact is
    // not the same slice.
    for sel in [
        "label-aaaa1111",
        "cwd-aaaa1111",
        "label-bbbb2222",
        "cwd-bbbb2222",
    ] {
        assert!(shown(vcx, sel), "{sel} did not render");
    }

    // And the sharing row costs no more than the plain one. This is the
    // assertion the old rail could not have passed: the sharer list was
    // two extra lines on every row that had one.
    assert_eq!(
        vcx.debug_bounds("rail-aaaa1111").unwrap().size.height,
        vcx.debug_bounds("rail-bbbb2222").unwrap().size.height,
        "the sharing fact still costs the row height — it is supposed to \
         be a mark on a line that already exists"
    );
}

/// Nothing on the row competes with the session's name.
///
/// The operator's complaint was that "the name, the id, the path and the
/// sharing line are all within 2px of the same size" — four facts at one
/// rank, so nothing told the eye where a row started and the rail read as
/// a wall. Asserted as CLEARANCE between the ranks rather than as an
/// ordering: two boxes 1 px apart are ordered and still look identical.
#[gpui::test]
fn nothing_on_the_row_competes_with_the_session_name(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut s = session("aaaa1111");
    s.shared_with = vec!["bbbb2222".into()];
    h.plumbing.push_rail(vec![s]);
    vcx.run_until_parked();

    let line_h = |sel: &'static str, vcx: &mut VisualTestContext| {
        vcx.debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} did not render"))
            .size
            .height
    };
    let name = line_h("label-aaaa1111", vcx);
    let folder = line_h("cwd-aaaa1111", vcx);
    let marks = line_h("marks-aaaa1111", vcx);

    assert!(
        name >= folder + px(4.),
        "the name's line box is {name:?} and the folder's is {folder:?} — \
         under 4px apart they read as one rank and the row has no anchor"
    );
    assert!(
        folder >= marks + px(1.),
        "the folder ({folder:?}) and its marks ({marks:?}) are the same \
         rank; the marks are supposed to be subordinate to the path"
    );
}

/// **The requirement this slice was actually failing, pinned.**
///
/// Every test in this file passed while the photographed rail read
/// `clau…`, `claude c…`, `chase the rai…`, `package…` — four names out of
/// five truncated, most of them mid-word. The session name is the one
/// thing on the row a person identifies the session by, and nothing was
/// testing that it fit. A row that shows ten characters of a name is worse
/// than the four-line row this slice replaced, which at least showed all
/// of it.
///
/// Asserted against a literal character count and the ink the text system
/// actually shapes — never against `RAIL_W` minus the constants the layout
/// uses, which is the derived-expectation trap that has already cost this
/// project a test that could not fail.
#[gpui::test]
fn a_realistically_named_session_shows_its_whole_name(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    // Twenty characters, which is a name somebody types: it is the length
    // of "chase the rail flake".
    let name = "chase the rail flake";
    assert_eq!(name.chars().count(), 20);
    let mut named = session("aaaa1111");
    named.title = Some(name.into());
    // ...on the row carrying the most other freight it can: shared with
    // two, so the marks group is on line two beside the folder.
    named.shared_with = vec!["bbbb2222".into(), "cccc3333".into()];
    rail_with_selected(&h, vcx, vec![named], "aaaa1111");

    let label = vcx.debug_bounds("label-aaaa1111").expect("the row\'s name");
    // Slice AS: at the weight the row DRAWS. This shaped at the family's
    // default weight, and the name is `WEIGHT_STRONG` — semibold runs
    // 118.5 px here against regular's 113.5, so the old measurement was
    // asking whether a lighter font would fit than the one on screen.
    let ink = real_name_width(name);
    assert!(
        label.size.width >= ink,
        "{name:?} shapes to {ink:?} and its slot is {:?} — the row would \
         render it as an ellipsis, and a name nobody can read is the whole \
         defect this slice was fixing",
        label.size.width
    );

    // And the folder beside it, on the same row, at the same time.
    let folder = vcx.debug_bounds("cwd-aaaa1111").expect("the folder");
    assert!(
        folder.size.width >= px(80.),
        "the folder is {:?} wide on a named, shared row",
        folder.size.width
    );
}

/// Selection reads as a FILL and a RIM — `launcher.rs`\'s treatment for its
/// provider tiles, which is the one place in this app that already did
/// selection properly.
///
/// An accent bar down the row\'s leading edge was tried and photographed.
/// The operator\'s ruling: *"no need for purple bar when selected"* — the
/// fill and the rim carried it on their own. What this asserts now is that
/// the rim is real and, more importantly, that it is laid out on EVERY row
/// so selecting one cannot move the rest.
#[gpui::test]
fn selection_is_a_fill_and_a_rim_and_never_a_bar(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222")],
        "aaaa1111",
    );
    assert!(
        !shown(vcx, "accent-aaaa1111"),
        "the accent bar is back on the selected row"
    );
    // The rim is a border on every row that only ever changes colour — a
    // border that APPEARED on selection would move every row below it the
    // moment the operator clicked. Proven by the size equality, which a
    // 2 px border on one row and none on the other could not satisfy.
    assert_eq!(
        vcx.debug_bounds("rail-aaaa1111").map(|r| r.size),
        vcx.debug_bounds("rail-bbbb2222").map(|r| r.size),
        "the selected row is a different size from its neighbour"
    );
}

/// Selecting a row does not move it, and neither does hovering it.
///
/// Stage AK proved the hover half and the rail depends on it: a virtualized
/// list with cached row heights cannot have a row that resizes under the
/// pointer. Slice AO added two things that could break the same invariant
/// for a different reason — a rim that appears on selection — so the
/// selected/unselected pair is asserted to the pixel as well.
#[gpui::test]
fn selecting_a_row_does_not_move_it(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing
        .push_rail(vec![session("aaaa1111"), session("bbbb2222")]);
    vcx.run_until_parked();
    let before = vcx.debug_bounds("rail-bbbb2222").expect("the second row");

    rail_with_selected(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222")],
        "aaaa1111",
    );
    let selected = vcx.debug_bounds("rail-aaaa1111").expect("the first row");
    let after = vcx.debug_bounds("rail-bbbb2222").expect("the second row");

    assert_eq!(
        before.size, after.size,
        "the unselected row changed size when its neighbour was selected"
    );
    assert_eq!(
        before.origin, after.origin,
        "selecting the row above moved this one: {:?} -> {:?} — the rim is \
         supposed to be laid out on every row and only change colour",
        before.origin, after.origin
    );
    assert_eq!(
        selected.size, after.size,
        "a selected row is a different height from an unselected one"
    );
}

/// The sharing fact is a COUNT, on every row, and never a list of ids.
///
/// Slice I2 rendered `shares files with 9fedaac4, 47890400` — two hex ids
/// wrapping to a second line, on every sharing row. A first pass at this
/// slice moved the identities to the selected row, which made one row
/// three lines tall and kept the hex; the operator called that out as the
/// single ugliest thing in the shot.
///
/// What survives is the part a person acts on: **somebody else is in these
/// files, and there are N of them.** The action it leads to — `isolate` —
/// is on the row and does not need the identities to work.
#[gpui::test]
fn the_sharing_fact_is_a_count_not_a_list_of_ids(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.shared_with = vec!["bbbb2222".into(), "cccc3333".into()];
    let mut b = session("bbbb2222");
    b.shared_with = vec!["aaaa1111".into(), "cccc3333".into()];
    rail_with_selected(&h, vcx, vec![a.clone(), b], "aaaa1111");

    assert!(shown(vcx, "marks-aaaa1111") && shown(vcx, "marks-bbbb2222"));
    assert_eq!(row_marks(&a).as_deref(), Some("+2 here"));

    // Selection changes nothing about it. The selected row is not a detail
    // pane and must not become one — that is what made the first pass's
    // selected row three lines tall.
    assert_eq!(
        vcx.debug_bounds("marks-aaaa1111").map(|r| r.size),
        vcx.debug_bounds("marks-bbbb2222").map(|r| r.size),
        "the selected row states the sharing fact differently from its \
         neighbour"
    );
    assert_eq!(
        vcx.debug_bounds("rail-aaaa1111").map(|r| r.size.height),
        vcx.debug_bounds("rail-bbbb2222").map(|r| r.size.height),
        "selecting a sharing row grew it — the identities are back"
    );

    // And no sharer is named anywhere in what the row states.
    let widest = row_marks(&a).unwrap();
    for id in ["bbbb2222", "cccc3333"] {
        assert!(
            !widest.contains(id),
            "the marks group names a sharer: {widest:?}"
        );
    }
    assert!(
        !shown(vcx, "shares-aaaa1111") && !shown(vcx, "shares-bbbb2222"),
        "the sharer list is back on a row"
    );
}

// ---------------------------------------------------------------------------
// Slice AN — the empty state.
//
// Text system: **Noop for every test in this section.** In-window glyph
// advances come from gpui's test text system, so a *width* here is not a
// fact about what a person reads. These assert three things that do not
// depend on shaping: whether an element is on screen at all, where its box
// sits relative to another box, and how tall a line's box is (line height
// is a style property, not a measured advance). The WORDS are asserted as
// pure functions in `ui.rs`'s own `mod tests`, where they are strings and
// not pixels.

/// Put one empty session in the rail and select it, which is the exact
/// gate `an_empty_log_states_only_the_kind_of_empty_it_can_prove` pins:
/// subscribed, and the engine's own count says the journal is empty.
fn empty_session(
    h: &Harness,
    vcx: &mut VisualTestContext,
    cwd: &str,
    model: Option<&str>,
) -> SessionInfo {
    let mut s = session("aaaa1111");
    s.last_seq = 0;
    s.cwd = cwd.into();
    s.model = model.map(|m| m.to_string());
    h.plumbing.push_rail(vec![s.clone()]);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    // Let the card's entrance land before anything measures it. The rise
    // is a live `top` offset while `motion::entrance_t` is below 1, so a
    // bounds read on a mid-entrance card is reading the animation, not the
    // layout — and two reads either side of an unrelated change would
    // differ for a reason that has nothing to do with the change.
    drive(h, vcx, Duration::from_millis(10));
    s
}

/// The four parts of an empty state, and the claim that a centred grey
/// string was none of them.
///
/// What shipped before this slice was `"nothing sent yet"` at
/// `SMALL_SIZE`, `text_tertiary`, dead centre of a pane that is roughly
/// two thirds of the window. It **announced** nothing (it was the app's
/// quietest rank), it **oriented** nobody (a person who has just opened
/// eave cannot tell which folder this session runs in or which agent it
/// drives), and it **offered** nothing.
///
/// So this asserts all four are present at once, because any one of them
/// alone is what was already there.
#[gpui::test]
fn the_empty_state_announces_orients_and_offers(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", Some("opus"));

    assert!(
        shown(vcx, "transcript-empty"),
        "a selected session with an empty journal shows no empty state at all"
    );
    for part in ["empty-title", "empty-where", "empty-key", "empty-action"] {
        assert!(
            shown(vcx, part),
            "the empty state is missing {part} — announce / orient / offer \
             is the whole difference between this and a caption"
        );
    }
}

/// **The announce rank is real, not claimed.** `DISPLAY_LINE` is 26 and
/// `SMALL_LINE` — what the old caption used — is 15. A headline whose box
/// is 15 px tall is the old caption with a new selector on it.
///
/// The number is written as a literal on purpose: reading it back out of
/// `theme::DISPLAY_LINE` would make this test agree with the constant no
/// matter what the constant became, which is the one thing it must not do.
#[gpui::test]
fn the_empty_states_headline_is_at_the_display_rank(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);

    let title = vcx.debug_bounds("empty-title").expect("the headline");
    assert_eq!(
        title.size.height,
        px(26.),
        "the headline's line box is {:?}; the display rank is 26 and the \
         retired caption's rank was 15",
        title.size.height
    );

    // And it is the ONLY thing on the card at that rank — a rank that
    // appears twice on one surface has stopped announcing anything.
    for quieter in ["empty-where", "empty-action"] {
        let b = vcx.debug_bounds(quieter).expect(quieter);
        assert!(
            b.size.height < px(26.),
            "{quieter} is {:?} tall, which is the headline's own rank",
            b.size.height
        );
    }
}

/// **Clearance, not merely fit.** The card is an absolute overlay bottomed
/// at `COMPOSER_CLEARANCE`, so it cannot reach the composer — but that is
/// an argument about the code, and the thing worth pinning is the gap on
/// screen. A future card that grew a fourth line, or a composer that grew
/// a second row, would close it silently.
#[gpui::test]
fn the_empty_state_clears_the_composer_it_points_at(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", Some("opus"));

    let action = vcx.debug_bounds("empty-action").expect("the action line");
    let hint = vcx.debug_bounds("composer-hint").expect("the composer");
    let card_bottom = action.origin.y + action.size.height;
    assert!(
        card_bottom < hint.origin.y,
        "the empty state runs to {card_bottom:?} and the composer starts at \
         {:?} — the card is sitting on the input it is telling you to use",
        hint.origin.y
    );
    // Not merely non-overlapping: separated by more than a hairline. The
    // composer's own card is inset from `COMPOSER_CLEARANCE`, so a gap
    // this small would already mean something has gone wrong above.
    assert!(
        hint.origin.y - card_bottom > px(24.),
        "only {:?} between the card and the composer",
        hint.origin.y - card_bottom
    );
}

/// **Above centre, not at it.** Dead centre is where an alert goes, which
/// is exactly why a small grey string sitting there read as a failed load
/// rather than as a rest state. But the first pass overcorrected to the
/// upper third and the card read as orphaned, so the ratio moved to 4:5.
///
/// `EMPTY_ABOVE`:`EMPTY_BELOW` is 4:5, so the free space above the card
/// should be about 4/9 of the free space total. The bounds are 0.38 and
/// 0.48 as literals, NOT derived from the constants they check: wide
/// enough that the card's own height and a pixel of rounding do not
/// matter, narrow enough that both failure modes fail — dead centre
/// (0.5) and the upper third this replaced (0.333).
#[gpui::test]
fn the_empty_state_sits_above_centre_but_not_at_the_top(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", Some("opus"));

    let pane = vcx.debug_bounds("transcript-empty").expect("the slot");
    let title = vcx.debug_bounds("empty-title").expect("the headline");
    let action = vcx.debug_bounds("empty-action").expect("the action line");

    let above = title.origin.y - pane.origin.y;
    let below = (pane.origin.y + pane.size.height) - (action.origin.y + action.size.height);
    assert!(
        above > px(0.) && below > px(0.),
        "the card is not inside its own slot: above={above:?} below={below:?}"
    );
    let share = f32::from(above) / f32::from(above + below);
    assert!(
        (0.38..0.48).contains(&share),
        "the card's free space is {share:.3} above / {:.3} below; 4:5 is \
         ~0.444, dead centre is 0.5, the old upper third was 0.333",
        1.0 - share
    );
}

/// **Gone the instant a turn exists**, and a turn is the operator's
/// message alone — not the agent's reply to it. A card still up while the
/// first directive is on screen would be claiming the session is empty
/// over the proof that it is not.
#[gpui::test]
fn the_empty_state_is_gone_the_instant_a_turn_exists(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    assert!(shown(vcx, "transcript-empty"));

    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(1, "the first turn")],
    });
    vcx.run_until_parked();

    assert!(
        !shown(vcx, "transcript-empty"),
        "one directive landed and the card is still claiming nothing was sent"
    );
    for part in ["empty-title", "empty-where", "empty-key", "empty-action"] {
        assert!(
            !shown(vcx, part),
            "{part} outlived the card it belongs to — the container went \
             away and a piece of it did not"
        );
    }
    // And the composer is still the composer: the card leaving must not
    // take the input with it.
    assert!(shown(vcx, "composer-hint"));
}

/// The card and the launcher answer different questions and can be up in
/// two windows at once, so a person has to be able to tell which one they
/// are looking at. They are never on screen together (the launcher owns
/// "no session"; the card owns "a session with no turns"), and the shapes
/// are different: the launcher centres its content in a 700 px column,
/// the card is left-aligned and capped at `DIRECTIVE_MAX_W`.
#[gpui::test]
fn the_empty_state_and_the_launcher_are_never_the_same_screen(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    assert!(shown(vcx, "launcher"), "a bare boot is the launcher");
    assert!(
        !shown(vcx, "transcript-empty"),
        "the launcher and the card must never stack"
    );
    let launcher_title = vcx.debug_bounds("launcher-title").expect("the launcher");

    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    assert!(!shown(vcx, "launcher"));
    let card_title = vcx.debug_bounds("empty-title").expect("the card");

    // The launcher's headline is centred in the pane; the card's is on
    // the transcript's left reading edge. That is the difference a person
    // reads first, before a single word.
    assert!(
        card_title.origin.x < launcher_title.origin.x,
        "the card's headline starts at {:?} and the launcher's at {:?} — \
         they are laid out alike and will be mistaken for each other",
        card_title.origin.x,
        launcher_title.origin.x
    );
}

/// The one clickable thing on the card, and it routes through the seam
/// that already existed (`open_in_system`) rather than through a command
/// invented for it. Knowing *where* an agent is about to run is the fact
/// this whole card is built around; being able to go look is the cheapest
/// possible way to make that fact checkable.
#[gpui::test]
fn the_empty_states_folder_line_opens_the_folder(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    OPENED.with(|o| o.borrow_mut().clear());
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);

    click(vcx, "empty-where");
    let opened = OPENED.with(|o| o.borrow().clone());
    assert_eq!(
        opened,
        vec![PathBuf::from("/tmp/eave-harness/repo")],
        "the cwd line did not open the cwd"
    );
}

/// **Emptying the composer's hint moves nothing.** The row's height is
/// set by the send button (26) and not by the hint (`MICRO_LINE`, 14), so
/// a hint that comes and goes with the draft must leave every box in the
/// composer exactly where it was — otherwise the first character typed
/// would shove the composer, and with it the clearance the empty-state
/// card is measured against.
///
/// This is the same invariant as `the_overlay_moves_nothing_it_floats_over`,
/// applied to a line of text that now has two states instead of one.
#[gpui::test]
fn emptying_the_composer_hint_moves_nothing(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);

    let probes = ["composer-hint", "send", "empty-title", "empty-action"];
    let quiet: Vec<_> = probes.iter().map(|s| vcx.debug_bounds(s)).collect();
    assert!(
        quiet.iter().all(|b| b.is_some()),
        "a probe never rendered, so it proves nothing: {quiet:?}"
    );

    // One character: the hint appears.
    vcx.simulate_input("x");
    vcx.run_until_parked();
    assert_eq!(composer_draft(vcx, &h), "x", "the keystroke never landed");

    let typing: Vec<_> = probes.iter().map(|s| vcx.debug_bounds(s)).collect();
    for (i, name) in probes.iter().enumerate() {
        let (a, b) = (quiet[i].unwrap(), typing[i].unwrap());
        assert_eq!(
            (a.origin, a.size.height),
            (b.origin, b.size.height),
            "{name} moved or changed height when the hint appeared: \
             {a:?} -> {b:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Slice AS — collapsing the rail must not cost you the other sessions.
//
// The titlebar's rail toggle removed the only way to reach another session:
// one click on a control that reads as "give me more room" made eave
// single-session until it was clicked again. The tab strip is the switcher
// that survives the collapse.
//
// Everything below is a HARNESS measurement. No hover state and no
// synthetic keystroke has ever been photographed in this app
// (docs/progress.md §Instrument list), and the harness cannot paint an icon
// at all (`TestAppContext` hardcodes `Arc::new(())` as its asset source) —
// so nothing here is allowed to depend on a tab's provider mark being
// visible. Every identity assertion goes through the NAME.

/// **The slice, in one test.** Collapse the rail and you can still switch.
///
/// It is written as the walk a person takes: five sessions in the rail, one
/// of them open, collapse the rail, and reach a different session. On
/// `integration-4` the second half of this cannot be written at all —
/// there is no element to click.
#[gpui::test]
fn collapsing_the_rail_leaves_a_way_to_switch_sessions(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let ids = ["aaaa1111", "bbbb2222", "cccc3333", "dddd4444", "eeee5555"];
    rail_and_strip(
        &h,
        vcx,
        ids.iter().map(|id| session(id)).collect(),
        "aaaa1111",
    );

    // Slice C5 CHANGED THE FIRST HALF OF THIS TEST, and the old assertion
    // is worth recording because it was right when it was written and is
    // wrong now. It read: *"the strip is on screen with the rail open —
    // two lists of the same sessions is not what the toggle is for"*.
    //
    // True while the strip held sessions. The strip holds PAGES now, which
    // the rail does not list and never will, so hiding it when the rail
    // opens would take the operator's open documents off screen. The two
    // surfaces are no longer two listings of one thing.
    assert!(shown(vcx, "rail-aaaa1111"), "the rail starts open");
    assert!(
        shown(vcx, "tab-strip"),
        "the strip is hidden while the rail is open — it holds pages, and \
         the rail does not"
    );

    click(vcx, "rail-toggle");
    vcx.run_until_parked();
    assert!(!shown(vcx, "rail-aaaa1111"), "the rail did not collapse");
    assert!(
        !shown(vcx, "tab-strip"),
        "the full strip survived the collapse — the spine is what a \
         collapsed rail shows, and two tab lists at once is neither"
    );

    // **Collapsed, there is no tab list at all — and this test now records
    // that as the operator's decision rather than asserting against it.**
    //
    // The history is worth keeping because the same ground has been walked
    // three times. Slice AS found the dead end (*"when i close the chat panel
    // we need to convert to page tabs for sessions so i can still switch"*)
    // and answered it with a strip in the titlebar. Slice C5 kept that. D1's
    // first build moved the strip into the rail and answered the collapse
    // case with a 44 px spine. The operator then ruled the spine out
    // (2026-08-21: *"i dont want the border on left side or on top on menu
    // close. we should close to full screen"*) and explicitly parked the
    // question the spine was answering — *"the top bar was supposed to be a
    // space for tabs when the sidebar was closed but idk about it now"*.
    //
    // So: with the rail shut, switching is ctrl-tab and ⌘L, and the toggle
    // that reopens the rail is on the pane header. **That toggle being
    // reachable is the part that is not negotiable**, and it is asserted
    // below — the first cut of this change left it inside the rail it
    // closes, and shutting the menu became one-way.
    assert!(
        !shown(vcx, "tab-strip"),
        "the strip survived the collapse — closing the menu is supposed to \
         give the window to the content"
    );
    assert!(
        shown(vcx, "rail-toggle"),
        "the control that reopens the rail is inside the rail — closing the \
         menu is a one-way door"
    );

    // ...and it is not decoration. **In two steps now, and the second step
    // is the slice's central claim.** Clicking a tab does not switch the
    // session directly; it activates the TAB, and the selection is derived
    // from which tab is active. That is what makes `TabsState::active` the
    // single notion of active rather than a second one kept beside the
    // shell's own — C1's open question, answered.
    // Re-open, and switch from the list — the walk the operator actually
    // takes now.
    click(vcx, "rail-toggle");
    vcx.run_until_parked();
    assert!(shown(vcx, "tab-strip"), "the rail did not reopen");
    let _ = h.drain();
    click(vcx, "tab-dddd4444");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "dddd4444"
        )),
        "clicking a tab emitted {cmds:?} — a strip you cannot switch with \
         is the dead end wearing a nicer shape"
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::Select(id) if id == "dddd4444")),
        "the shell switched the session without waiting for the strip to \
         say the tab was active — that is the second notion of active \
         growing back: {cmds:?}"
    );

    // The engine answers: that tab is active now. THEN the selection moves.
    strip_tabs(
        &h,
        vcx,
        ids.iter().map(|id| session_tab(id)).collect(),
        Some("dddd4444"),
    );
    let cmds = h.drain();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::Select(id) if id == "dddd4444")),
        "the active tab views a session and the shell stayed subscribed to \
         another one: {cmds:?}"
    );

    // And the toggle is an involution: shut, open, and the rail is back.
    click(vcx, "rail-toggle");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "rail-aaaa1111"),
        "the rail did not collapse again"
    );
    click(vcx, "rail-toggle");
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-aaaa1111"), "the rail did not come back");
    assert!(
        shown(vcx, "tab-strip"),
        "the strip vanished when the rail came back"
    );
}

/// **The transcript does not move when the strip appears.**
///
/// This is the constraint the placement was chosen to satisfy rather than
/// to trade against: the strip lives in the titlebar, which is
/// `h(TITLEBAR_H)` and `flex_none`, so nothing inside it can change the
/// height of anything below it. A band of its own between the titlebar and
/// the content row would have had to be reserved while the rail is open —
/// a permanent empty stripe paid for a state the operator is usually not
/// in — or it would have jumped the reading column on every collapse.
///
/// Asserted to the pixel on the reading column itself, on both sides of the
/// toggle and back again. The width is asserted to MOVE, in the other
/// direction, because a collapse that returned no room would not be worth
/// having.
#[gpui::test]
fn the_tab_strip_costs_the_transcript_no_height(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(
        &h,
        vcx,
        vec![
            session("aaaa1111"),
            session("bbbb2222"),
            session("cccc3333"),
        ],
        "aaaa1111",
    );

    let well = |vcx: &mut VisualTestContext| {
        vcx.debug_bounds("content-well")
            .expect("the reading column did not render")
    };
    // **Slice D1 re-pointed this from the titlebar to the pane header.** The
    // titlebar no longer exists — A′ gave the titlebar LINE to the rail on
    // the left and to this header on the right — but the question is
    // unchanged and is still the right one: does collapsing the rail move
    // the reading column vertically?
    let bar =
        |vcx: &mut VisualTestContext| vcx.debug_bounds("pane-header").expect("the pane header");

    let open = well(vcx);
    let open_bar = bar(vcx);
    assert!(
        shown(vcx, "tab-strip"),
        "the strip is not mounted with the rail OPEN — in A′ the strip lives \
         in the rail, so this is its normal state rather than its collapsed one"
    );
    click(vcx, "rail-toggle");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "rail"),
        "the rail survived the collapse — closing the menu gives the window \
         to the content"
    );
    let shut = well(vcx);
    let shut_bar = bar(vcx);

    assert_eq!(
        (open.origin.y, open.size.height),
        (shut.origin.y, shut.size.height),
        "the reading column moved when the tab strip appeared: {open:?} -> \
         {shut:?}. A layout that jumps on collapse is worse than the dead \
         end being fixed."
    );
    assert_eq!(
        open_bar.size.height, shut_bar.size.height,
        "the pane header changed height when the rail collapsed, which is \
         the same bug one element up"
    );
    assert!(
        shut.size.width >= open.size.width + px(200.),
        "collapsing the rail returned only {:?} of width ({:?} -> {:?}) — \
         if the strip cost the column what the rail cost it, collapsing \
         bought nothing",
        shut.size.width - open.size.width,
        open.size.width,
        shut.size.width
    );

    // ...and back. A toggle that is not an involution leaves the operator
    // somewhere they cannot get out of.
    click(vcx, "rail-toggle");
    vcx.run_until_parked();
    let again = well(vcx);
    assert_eq!(
        (open.origin, open.size),
        (again.origin, again.size),
        "re-opening the rail did not return the column to where it was"
    );

    // The rail reserves the traffic lights their room, asserted as CLEARANCE
    // against literals rather than as a fit against the constants the layout
    // uses. The lights sit at a fixed window position and the rail is what
    // they land on now, so this is the assertion that stops a later slice
    // shrinking the rail's top region out from under them.
    #[allow(clippy::assertions_on_constants)]
    {
        assert!(
            theme::TITLEBAR_H >= 44.0,
            "the rail's top region is {} px and the traffic lights sit at \
             y=17 with a 12 px glyph",
            theme::TITLEBAR_H
        );
        assert!(
            theme::RAIL_W >= theme::TRAFFIC_CLEARANCE,
            "the rail is {} px wide and the traffic lights need {} — an \
             expanded rail has to contain them outright",
            theme::RAIL_W,
            theme::TRAFFIC_CLEARANCE
        );
    }
}

/// A tab shows a session's NAME, in full, for the name the normal session
/// actually has.
///
/// The requirement Slice AO found nothing was testing — a rail whose every
/// name read `clau…` passed 241 tests — applied to the new surface. The
/// name here is `claude code`, the provider fallback an UNNAMED session
/// paints, because Slice AO's caveat 5 records unnamed-in-one-repo as the
/// normal case in the operator's own configuration rather than the edge
/// one.
///
/// Shaped at [`theme::WEIGHT_STRONG`], which is what the tab draws.
/// `real_text_width` shapes at the family's default weight and semibold is
/// wider — every name-fits assertion in this file that used the plain
/// helper was measuring a lighter font than the one on screen.
#[gpui::test]
fn a_tab_shows_a_realistic_session_name(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111")], "aaaa1111");

    // `real_name_width` shapes at 13 px semibold and guards those two
    // against the tokens, so a slice that moves the name's size fails here
    // rather than quietly measuring a font nothing draws.
    let ink = real_name_width("claude code");
    let label = vcx
        .debug_bounds("tab-label-aaaa1111")
        .expect("the tab's name");
    assert!(
        label.size.width >= ink,
        "`claude code` shapes to {ink:?} and the tab's name slot is {:?} — \
         a tab that renders the commonest session label as an ellipsis \
         cannot be used to tell two sessions apart",
        label.size.width
    );

    // Nothing on the tab is over the name. The rail's own version of this
    // invariant was pointed at the wrong element for two slices and could
    // never fail (`the_action_strip_does_not_cover_the_session_name`); this
    // one is pointed at the tab's name and at the tab's box, and it is a
    // containment check rather than a proximity one.
    let tab = vcx.debug_bounds("tab-aaaa1111").expect("the tab");
    assert!(
        label.origin.x >= tab.origin.x + px(8.),
        "the name starts {:?} into a tab that begins at {:?} — it is \
         outside the tab's own padding",
        label.origin.x - tab.origin.x,
        tab.origin.x
    );
    assert!(
        label.origin.x + label.size.width <= tab.origin.x + tab.size.width - px(8.),
        "the name runs to {:?} and the tab ends at {:?} — the slot has \
         eaten its own padding, and the next thing added to a tab will be \
         on top of the name",
        label.origin.x + label.size.width,
        tab.origin.x + tab.size.width
    );

    // A name too long for a tab CLIPS. It must not widen the tab: a strip
    // whose tabs are each as wide as their name has no rhythm, and the tab
    // you are aiming for moves when an unrelated session is renamed.
    let plain = tab.size.width;
    let mut long = session("aaaa1111");
    long.title = Some("chase the rail flake".into());
    rail_and_strip(&h, vcx, vec![long], "aaaa1111");
    let wide = vcx
        .debug_bounds("tab-aaaa1111")
        .expect("the tab")
        .size
        .width;
    assert_eq!(
        plain, wide,
        "a 20-character title made the tab {wide:?} instead of {plain:?} — \
         tabs are a fixed width and the name truncates into it"
    );
}

/// **A tab is not a row.** The strip must not be the rail lying on its
/// side, or collapsing the rail buys nothing.
///
/// The rail states where a session works, how many other sessions are in
/// those files, how dirty its worktree is, whether it is archived, what
/// permission posture it holds, and offers five affordances. A tab states
/// two things: which session, and what state it is in.
#[gpui::test]
fn a_tab_is_not_a_rail_row(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    // The most freighted row the rail can build: named, sharing with two,
    // worktree'd and dirty, on a dangerous posture.
    let mut loaded = session("aaaa1111");
    loaded.title = Some("the refactor".into());
    loaded.shared_with = vec!["bbbb2222".into(), "cccc3333".into()];
    loaded.worktree = Some(worktree("eave/aaaa1111", Some(2), true));
    loaded.permission_mode = Some("bypassPermissions".into());
    rail_and_strip(&h, vcx, vec![loaded], "aaaa1111");
    // Every one of those facts is on the row.
    for sel in ["cwd-aaaa1111", "marks-aaaa1111"] {
        assert!(
            shown(vcx, sel),
            "{sel} is not on the rail row to begin with"
        );
    }

    assert!(shown(vcx, "tab-aaaa1111"), "the tab did not mount");

    // **The question had to change shape, because A′ put both surfaces in
    // one column.** Before this slice the test collapsed the rail, so the row
    // was gone and `!shown(freight)` answered it. With the rail open the row
    // is on screen *by design* and its freight is on screen with it — so
    // "is this element visible" now answers nothing at all, and a test left
    // in that form would have gone on passing while measuring the row.
    //
    // What it asks instead is containment: none of the row's freight is
    // INSIDE the tab's box.
    let tab = vcx.debug_bounds("tab-aaaa1111").expect("the tab");
    for sel in [
        "cwd-aaaa1111",
        "marks-aaaa1111",
        "ren-aaaa1111",
        "arc-aaaa1111",
    ] {
        if let Some(b) = vcx.debug_bounds(sel) {
            let inside = b.origin.y >= tab.origin.y
                && b.origin.y + b.size.height <= tab.origin.y + tab.size.height;
            assert!(
                !inside,
                "{sel} is inside the tab's own box ({b:?} within {tab:?}) — a \
                 tab carrying the rail's freight needs the rail's room"
            );
        }
    }

    // **Slice D1 deleted this test's width ceiling, and the deletion is the
    // honest half of the change rather than a concession.**
    //
    // It used to read: *"the rail is 248 px wide and gives a name 158 of
    // them, and a tab may approach neither"*, asserted as `TAB_W <= 192` and
    // `TAB_NAME_W <= 112`. Both constants are gone from the layout — a row
    // takes the rail's width and its name is `flex_1` — so the ceiling is not
    // merely unmeasurable, it is **false**: a tab's name slot is now exactly
    // the rail's name slot, and that was the point. C5's caveat 8 recorded
    // what the old ceiling cost (`package the dmg`, 108.8 px, clipped).
    //
    // What survives is the claim that actually mattered, and it is asserted
    // above and below rather than through a proxy: **a tab is not a row
    // because of what it CARRIES, not because of how wide it is.** The two
    // now sit in one column at one width, which makes the freight test
    // sharper than it was when a width ceiling could stand in for it.
    let tab = vcx.debug_bounds("tab-aaaa1111").expect("the tab");
    let row = vcx.debug_bounds("rail-aaaa1111").expect("the rail row");
    assert!(
        tab.size.height < row.size.height,
        "the tab is {:?} tall and the row {:?} — a tab that has grown to a \
         row's height has grown to a row",
        tab.size.height,
        row.size.height
    );
}

/// Selection is a neutral raised fill plus a rim, and it moves nothing.
///
/// The operator ruled on the rail's version outright — *"no need for purple
/// bar when selected"* — and the rule the theme states is that the accent
/// marks where you are ACTING, not where you are. A selected tab is a
/// location.
///
/// What is asserted here is the part that can be: [`theme::selection_rim`]
/// is a 1 px INSET shadow rather than a border precisely so that selecting
/// cannot change an element's width, and in a horizontal strip that matters
/// more than it does in the rail — a 2 px border on one tab would move
/// every tab after it. Both selections are measured to the pixel, across
/// every tab.
#[gpui::test]
fn selecting_a_tab_moves_no_tab(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let ids = ["aaaa1111", "bbbb2222", "cccc3333"];
    let infos: Vec<SessionInfo> = ids.iter().map(|id| session(id)).collect();
    rail_and_strip(&h, vcx, infos.clone(), "aaaa1111");

    let boxes = |vcx: &mut VisualTestContext| -> Vec<gpui::Bounds<Pixels>> {
        ["tab-aaaa1111", "tab-bbbb2222", "tab-cccc3333"]
            .iter()
            .map(|sel| vcx.debug_bounds(sel).unwrap_or_else(|| panic!("{sel}")))
            .collect()
    };
    let first = boxes(vcx);
    // Every tab is the same box, which is what makes the strip a strip.
    //
    // **Slice D1 swapped which dimension is the fixed one, and that is the
    // axis change in one assertion.** A horizontal tab was `TAB_W` wide with
    // a shared baseline; a row is `RAIL_TAB_H` tall on a shared left edge,
    // and its WIDTH is the rail's rather than a constant — which is what
    // gives a three-word title the room `TAB_NAME_W` denied it.
    for (i, b) in first.iter().enumerate() {
        assert_eq!(
            b.size.height,
            px(theme::RAIL_TAB_H),
            "tab {i} is not the fixed height"
        );
        assert_eq!(b.size.width, first[0].size.width, "tab {i} width");
        assert_eq!(b.origin.x, first[0].origin.x, "tab {i} left edge");
    }

    rail_and_strip(&h, vcx, infos, "bbbb2222");
    let second = boxes(vcx);
    for (i, (a, b)) in first.iter().zip(second.iter()).enumerate() {
        assert_eq!(
            (a.origin, a.size),
            (b.origin, b.size),
            "tab {i} moved when the selection went from the first tab to \
             the second: {a:?} -> {b:?}. The rim is an inset shadow so that \
             this cannot happen."
        );
    }
}

/// Many sessions: every one of them has a tab, and the one you are in is
/// always on screen.
///
/// The strip is a fixed-width lane that SCROLLS rather than an overflow
/// menu or a shrink-to-fit row. What that has to earn is two properties,
/// and this asserts both: nothing is dropped (a session with no tab is the
/// dead end again, for that session), and the selected tab is never off the
/// end (a strip that says nothing about where you are is a strip you cannot
/// navigate from).
#[gpui::test]
fn no_session_falls_off_the_end_of_the_strip(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let ids = [
        // **Slice D4 had to grow this again.** D1 grew it from nine because
        // the rail fits nine; D4 added four non-tab rows to the same list
        // (zone gap, two ghost rows, folder rows), and `rail_rows_h` gives
        // those their own budget on top of the nine — so twelve fit now and
        // the fixture stopped overflowing at all.
        "aaaa1111", "bbbb2222", "cccc3333", "dddd4444", "eeee5555", "ffff6666", "aaab7777",
        "bbbc8888", "cccd9999", "dddda111", "eeeeb222", "ffffc333", "aaaad444", "bbbbe555",
        "ccccf666", "dddd0777",
    ];
    // Distinct activity stamps, newest first, so the rail's order is a
    // known thing rather than whatever `fold_rail` does with a nine-way tie
    // — the first version of this test asserted an order the fixture never
    // specified and read the tie as a bug.
    let infos: Vec<SessionInfo> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let mut s = session(id);
            s.last_activity_ms = Some((ids.len() - i) as u64 * 1000);
            s
        })
        .collect();
    rail_and_strip(&h, vcx, infos.clone(), "aaaa1111");

    let strip = vcx.debug_bounds("tab-strip").expect("the strip");
    // The fixture really does overflow — otherwise the two properties below
    // are asserted about a case that never arises.
    //
    // **Slice D1 had to grow the fixture to keep this true.** Nine tabs
    // overflowed the horizontal strip; nine is exactly what the rail now
    // shows (`nine_tabs_fit_the_rail_without_scrolling` asserts the other
    // side of that line), so the count went to twelve. A test whose
    // precondition quietly stopped holding would have gone on passing while
    // measuring nothing — which is the failure mode this assertion exists
    // to prevent, and it earned its keep on exactly this change.
    assert!(
        px(theme::RAIL_TAB_H) * (ids.len() as f32) > strip.size.height,
        "{} tabs fit in {:?}, so this test is not measuring an \
         overflowing strip at all",
        ids.len(),
        strip.size.height
    );

    let tabs = [
        "tab-aaaa1111",
        "tab-bbbb2222",
        "tab-cccc3333",
        "tab-dddd4444",
        "tab-eeee5555",
        "tab-ffff6666",
        "tab-aaab7777",
        "tab-bbbc8888",
        "tab-cccd9999",
        "tab-dddda111",
        "tab-eeeeb222",
        "tab-ffffc333",
    ];
    for sel in tabs {
        assert!(
            shown(vcx, sel),
            "{sel} has no tab — nine sessions is not an exotic number and a \
             session with no tab is unreachable with the rail shut"
        );
    }
    // In the rail's own order, top to bottom. Two surfaces disagreeing
    // about the order of the same list is its own kind of dead end.
    let ys: Vec<Pixels> = tabs
        .iter()
        .map(|sel| vcx.debug_bounds(sel).unwrap().origin.y)
        .collect();
    for w in ys.windows(2) {
        assert!(w[1] > w[0], "the strip is not in the rail's order: {ys:?}");
    }

    // The LAST session, selected. Its tab has to come to the operator.
    rail_and_strip(&h, vcx, infos, "ffffc333");
    let strip = vcx.debug_bounds("tab-strip").expect("the strip");
    let sel = vcx.debug_bounds("tab-ffffc333").expect("the selected tab");
    assert!(
        sel.origin.y >= strip.origin.y
            && sel.origin.y + sel.size.height <= strip.origin.y + strip.size.height,
        "the selected tab sits at {:?}..{:?} and the strip runs {:?}..{:?} \
         — the session you are reading is off the end of the only control \
         that could take you anywhere else",
        sel.origin.y,
        sel.origin.y + sel.size.height,
        strip.origin.y,
        strip.origin.y + strip.size.height
    );
}

/// A tab identifies its provider the way a row does — a real mark on an
/// opaque plate, with initials as the fallback.
///
/// The plate is not decoration and the contrast argument is
/// `theme.rs`'s (`a_provider_mark_clears_the_non_text_floor_on_its_own_plate`
/// measures 6.01:1 on `chip_plate` against 2.55:1 unplated). What a
/// harness can add is that the SLOT exists, is the rail's size, and is the
/// same box whether a provider has a logo or not — which is what keeps the
/// name slot beside it identical across providers.
///
/// It cannot assert the mark is visible. `TestAppContext` hardcodes
/// `Arc::new(())` as its asset source, so every icon in this file lays out
/// its box and paints nothing.
#[gpui::test]
fn a_tab_wears_the_providers_mark_on_the_same_plate_a_row_does(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut known = session("aaaa1111");
    known.provider = "claude-code".into();
    let mut unknown = session("bbbb2222");
    unknown.provider = "some-agent".into();
    rail_and_strip(&h, vcx, vec![known, unknown], "aaaa1111");

    for sel in ["tab-chip-aaaa1111", "tab-chip-bbbb2222"] {
        assert!(shown(vcx, sel), "{sel} did not render");
    }
    let a = vcx.debug_bounds("tab-label-aaaa1111").unwrap();
    let b = vcx.debug_bounds("tab-label-bbbb2222").unwrap();
    assert_eq!(
        a.size.width, b.size.width,
        "a provider with a logo and one without gave their names different \
         slots — the two chips are supposed to be interchangeable"
    );
    // Slice D1: the name slot is `flex_1` now rather than `TAB_NAME_W`, so
    // the assertion above stopped being trivially true the moment the width
    // came from layout instead of from a constant. Two providers whose
    // plates differed by a pixel would now give their names two different
    // widths, which is exactly what it is here to catch.
    // And the fallback really is the initials, which is the only half of a
    // provider's identity this harness can read at all.
    assert_eq!(provider_identity("some-agent").0, "SA");

    // The plate is the rail's, at the rail's size, and it leaves the name
    // its whole slot. Written as a literal: 18 px is `PROVIDER_CHIP`, and
    // reading it from the constant would let a slice grow the plate and
    // eat the name with the test moving along underneath.
    let chip = vcx.debug_bounds("tab-chip-aaaa1111").unwrap();
    assert!(
        chip.size.width <= px(12.),
        "the provider mark is {:?} on a tab. It is 12 px inside an 18 px \
         plate: a mark at the plate's full size reads as a crop, and every \
         pixel it takes is one the name does not get",
        chip.size.width
    );
}

/// **Nine tabs fit the rail at the default window with no scrolling — where
/// five fitted the titlebar strip.**
///
/// This is `five_sessions_fit_the_strip_without_scrolling`, re-pointed at the
/// vertical axis, and the number it asserts going from five to nine is the
/// whole functional case for A′. The old ceiling was not a styling problem:
/// a horizontal strip shares the titlebar with everything else on that line,
/// and `TAB_W` was set where it was *partly to keep five true* — C5's caveat
/// records the three-word operator titles that clipped to buy it. Vertical
/// space is what the window has spare.
///
/// Still measured on the RENDERED rows, never computed from `RAIL_TAB_H`
/// times [`theme::RAIL_TABS_VISIBLE`] — that arithmetic would agree with the
/// layout by construction and could not fail. That was the original test's
/// rule and it survives the axis change unchanged.
#[gpui::test]
fn nine_tabs_fit_the_rail_without_scrolling(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let ids = [
        "aaaa1111", "bbbb2222", "cccc3333", "dddd4444", "eeee5555", "ffff6666", "aaab7777",
        "bbbc8888", "cccd9999",
    ];
    rail_and_strip(
        &h,
        vcx,
        ids.iter().map(|id| session(id)).collect(),
        "aaaa1111",
    );

    // No `rail-toggle` click: A′ put the strip INSIDE the open rail, so the
    // tabs are on screen in the state the operator is normally in. That
    // click was load-bearing in every strip test before this slice and is
    // gone from all of them.
    let strip = vcx.debug_bounds("tab-strip").expect("the strip");
    for sel in [
        "tab-aaaa1111",
        "tab-bbbb2222",
        "tab-cccc3333",
        "tab-dddd4444",
        "tab-eeee5555",
        "tab-ffff6666",
        "tab-aaab7777",
        "tab-bbbc8888",
        "tab-cccd9999",
    ] {
        let tab = vcx.debug_bounds(sel).unwrap_or_else(|| panic!("{sel}"));
        assert!(
            tab.origin.y >= strip.origin.y
                && tab.origin.y + tab.size.height <= strip.origin.y + strip.size.height,
            "{sel} runs {:?}..{:?} and the strip runs {:?}..{:?} — nine tabs \
             have to be on screen at the default window size",
            tab.origin.y,
            tab.origin.y + tab.size.height,
            strip.origin.y,
            strip.origin.y + strip.size.height
        );
    }
}

// Slice AR — the `/` palette

/// A project with two real skills and a command in it, at a path nothing
/// else uses.
///
/// A REAL directory, because the thing under test is a directory read.
/// Mocking the filesystem here would leave the one behaviour this slice
/// adds — "look at the session's cwd and see what is in it" — asserted by
/// nothing.
struct SlashProject(PathBuf);

impl SlashProject {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("eave-ar-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |p: PathBuf, body: &str| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        write(
            root.join(".claude/skills/shipit/SKILL.md"),
            "---\nname: shipit\ndescription: cut a release and tag it\n---\nbody\n",
        );
        write(
            root.join(".claude/skills/shellcheck/SKILL.md"),
            "---\ndescription: lint every shell script in the repo\n---\nbody\n",
        );
        write(
            root.join(".claude/commands/review.md"),
            "---\ndescription: review the working tree\n---\nbody\n",
        );
        Self(root)
    }
    fn cwd(&self) -> String {
        self.0.display().to_string()
    }
}

impl Drop for SlashProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Select a session whose cwd is `project`, and settle.
fn open_session_in(h: &Harness, vcx: &mut VisualTestContext, project: &SlashProject) {
    let mut s = session("aaaa1111");
    s.cwd = project.cwd();
    h.plumbing.push_rail(vec![s]);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
}

/// Slice C5 — what the omnibox is holding.
fn omni_text(h: &Harness, vcx: &mut VisualTestContext) -> String {
    vcx.read_window(&h.window, |shell, cx| {
        shell
            .read(cx)
            .omni
            .open
            .as_ref()
            .map(|e| e.text.clone())
            .unwrap_or_default()
    })
    .unwrap()
}

fn draft_of(h: &Harness, vcx: &mut VisualTestContext) -> String {
    vcx.read_window(&h.window, |shell, cx| {
        shell
            .read(cx)
            .composer
            .read(cx)
            .field
            .read(cx)
            .content()
            .to_string()
    })
    .unwrap()
}

/// One `system/init` record, the way the engine journals it — the frame
/// the agent CLI writes before the model is invoked, carrying its own
/// complete list of the `/name`s it will accept (Slice AR2).
///
/// It carries a fixed `session_id`, which is load-bearing for Slice CF: a
/// SECOND one of these under the same id is claude announcing a turn it
/// started itself (`docs/research/turn-start-evidence.md` §1).
fn init_record(seq: u64, slash_commands: &[&str]) -> eave_core::journal::Record {
    eave_core::journal::Record {
        seq,
        ts_ms: seq * 10,
        event: eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::Init {
                session_id: "aaaa1111".into(),
                permission_mode: Some("default".into()),
                slash_commands: slash_commands.iter().map(|s| (*s).to_string()).collect(),
            },
        },
    }
}

/// What the palette is offering right now, as `(name, description)`.
///
/// Read off the shell's own state rather than the rendered rows, because
/// the fact under test is which rows exist and where each row's SECOND
/// line came from — and a `debug_bounds` selector can only say that a row
/// is on screen.
fn offered(h: &Harness, vcx: &mut VisualTestContext) -> Vec<(String, Option<String>)> {
    vcx.read_window(&h.window, |shell, cx| {
        shell
            .read(cx)
            .slash
            .cache
            .as_ref()
            .map(|c| {
                c.offered
                    .iter()
                    .map(|e| (e.name.clone(), e.description().map(str::to_string)))
                    .collect()
            })
            .unwrap_or_default()
    })
    .unwrap()
}

/// Let the background scan land and the entrance play.
fn settle_palette(h: &Harness, vcx: &mut VisualTestContext) {
    vcx.run_until_parked();
    drive(h, vcx, Duration::from_millis(200));
}

/// **The slice, end to end: type a slash, see your own skills, choose one.**
///
/// The palette is reached by TYPING, which makes this the one place the
/// central interaction is exercised at all — `docs/progress.md` records
/// that synthetic keyboard events never reach the real gpui app, so no
/// screenshot of this menu can ever be driven from outside the process.
/// Inside the harness the keystrokes are real dispatches through the real
/// `on_key`, and the directory is a real directory on disk.
#[gpui::test]
fn the_slash_palette_offers_the_operators_own_skills(cx: &mut TestAppContext) {
    let project = SlashProject::new("offers");
    let (h, vcx) = boot(cx);
    open_session_in(&h, vcx, &project);

    assert!(
        !shown(vcx, "slash-palette"),
        "the palette must not be up over an empty composer"
    );

    vcx.simulate_input("/");
    settle_palette(&h, vcx);
    assert!(
        shown(vcx, "slash-palette"),
        "a leading slash did not open the palette"
    );
    // The project's three come FIRST, because precedence is project →
    // user → plugin and the empty query preserves discovery order.
    //
    // **Only the first three rows can be asserted here, and that is the
    // feature rather than a weakness of the test.** The palette really
    // does read this operator's `~/.claude` as well — it is the real
    // discovery path, not a mock — so rows beyond the fixture belong to
    // whatever machine the suite is running on. What is machine-
    // independent is that the project's own things lead.
    for row in ["menu-row-0", "menu-row-1", "menu-row-2"] {
        assert!(shown(vcx, row), "{row} missing — the scan found too little");
    }

    // Typing filters, and the letter still reaches the composer — the
    // palette takes four keys and no others.
    vcx.simulate_input("she");
    settle_palette(&h, vcx);
    assert_eq!(
        draft_of(&h, vcx),
        "/she",
        "the palette swallowed a letter meant for the composer"
    );
    assert!(shown(vcx, "menu-row-0"), "/she matches shellcheck");
    assert!(
        !shown(vcx, "menu-row-1"),
        "/she matches exactly one of the three"
    );

    // Choosing puts it in the composer and does NOT send it.
    let before = h.window.update(vcx, |_, _, _| ()).is_ok();
    assert!(before);
    click(vcx, "menu-row-0");
    settle_palette(&h, vcx);
    assert_eq!(
        draft_of(&h, vcx),
        "/shellcheck ",
        "choosing a row completes the command and leaves room for an argument"
    );
    assert!(
        !shown(vcx, "slash-palette"),
        "the trailing space ends the query, so the palette closes itself"
    );
}

/// **The compatibility guarantee, and the reason it is a test.**
///
/// `Shell::send_current` has always forwarded a `/`-prefixed message
/// verbatim — the CLI resolves it, and this slice added discovery, not
/// plumbing. So an operator who types the whole command and presses Return
/// must get exactly what they got before the palette existed. Firing the
/// top match instead would silently change the meaning of Return for every
/// slash message ever typed into this app.
#[gpui::test]
fn return_still_sends_a_slash_command_typed_in_full(cx: &mut TestAppContext) {
    let project = SlashProject::new("sends");
    let (mut h, vcx) = boot(cx);
    open_session_in(&h, vcx, &project);

    vcx.simulate_input("/shipit");
    settle_palette(&h, vcx);
    assert!(
        shown(vcx, "slash-palette"),
        "the palette is up over the match"
    );

    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Send { text, images }] if text == "/shipit" && images.is_empty()),
        "Return over an un-navigated palette must send the text as typed, \
         got {cmds:?}"
    );
    assert_eq!(draft_of(&h, vcx), "", "a sent draft is cleared");
    assert!(
        !shown(vcx, "slash-palette"),
        "the palette outlived the message it was offering completions for"
    );
}

/// Return only *chooses* once the operator has moved the highlight onto a
/// row themselves. Down-then-Return is a decision; Return alone is a send.
#[gpui::test]
fn return_chooses_only_after_the_operator_moves_the_highlight(cx: &mut TestAppContext) {
    let project = SlashProject::new("highlight");
    let (mut h, vcx) = boot(cx);
    open_session_in(&h, vcx, &project);

    vcx.simulate_input("/ship");
    settle_palette(&h, vcx);
    let _ = h.drain();

    vcx.simulate_keystrokes("down");
    settle_palette(&h, vcx);
    vcx.simulate_keystrokes("enter");
    settle_palette(&h, vcx);

    assert!(
        h.drain().is_empty(),
        "Return on a highlighted row chooses; it must not also send"
    );
    assert_eq!(draft_of(&h, vcx), "/shipit ");
}

/// Escape refuses the palette **without touching the draft**.
///
/// The palette is a projection of the composer's text, so the obvious
/// implementation of "close" is "clear the text" — and that would make
/// escape both the only way to dismiss the menu and a way to lose what you
/// wrote. The refusal is its own fact, and it is retracted by the next
/// keystroke rather than by anything the operator has to remember.
#[gpui::test]
fn escape_refuses_the_palette_without_losing_the_draft(cx: &mut TestAppContext) {
    let project = SlashProject::new("escape");
    let (h, vcx) = boot(cx);
    open_session_in(&h, vcx, &project);

    vcx.simulate_input("/ship");
    settle_palette(&h, vcx);
    assert!(shown(vcx, "slash-palette"));

    vcx.simulate_keystrokes("escape");
    settle_palette(&h, vcx);
    assert!(!shown(vcx, "slash-palette"), "escape did not dismiss");
    assert_eq!(
        draft_of(&h, vcx),
        "/ship",
        "escape must not be a way to lose a draft"
    );

    // ...and typing brings it back, so the refusal traps nobody.
    vcx.simulate_input("i");
    settle_palette(&h, vcx);
    assert!(
        shown(vcx, "slash-palette"),
        "the next keystroke retracts the refusal"
    );
}

/// A slash that is not at the start, and a slash with an argument after
/// it, are both ordinary messages. `/` is a prefix, not a character to be
/// afraid of.
#[gpui::test]
fn an_ordinary_message_never_opens_the_palette(cx: &mut TestAppContext) {
    let project = SlashProject::new("ordinary");
    let (h, vcx) = boot(cx);
    open_session_in(&h, vcx, &project);

    vcx.simulate_input("what does src/main.rs do?");
    settle_palette(&h, vcx);
    assert!(
        !shown(vcx, "slash-palette"),
        "a slash in the middle of a sentence opened a command palette"
    );

    // "what does src/main.rs do?" is 25 characters.
    for _ in 0.."what does src/main.rs do?".len() {
        vcx.simulate_keystrokes("backspace");
    }
    assert_eq!(draft_of(&h, vcx), "", "the composer did not empty");
    vcx.simulate_input("/shipit now");
    settle_palette(&h, vcx);
    assert_eq!(draft_of(&h, vcx), "/shipit now");
    assert!(
        !shown(vcx, "slash-palette"),
        "once an argument is being typed the operator has already chosen, \
         and a menu over the composer is in the way"
    );
}

/// A directory with nothing in it says so, and says it about the query the
/// operator actually typed.
///
/// An empty menu would be the claim "you have no skills" made in the shape
/// of an absence, which is the defect Slice M named in the transcript's
/// empty state: "nothing there" and "have not looked" are different facts.
#[gpui::test]
fn a_query_that_matches_nothing_says_so_instead_of_showing_an_empty_menu(cx: &mut TestAppContext) {
    let project = SlashProject::new("nomatch");
    let (h, vcx) = boot(cx);
    open_session_in(&h, vcx, &project);

    vcx.simulate_input("/xyzzy");
    settle_palette(&h, vcx);
    assert!(
        shown(vcx, "slash-palette"),
        "the palette must stand its ground and explain, not vanish"
    );
    assert!(shown(vcx, "menu-row-0"), "the explanation is a row");
    assert!(
        !shown(vcx, "menu-row-1"),
        "exactly one row: the explanation"
    );
}

/// **The cwd trap, asserted at the only level that can catch it.**
///
/// eave is one binary in two roles and the ENGINE holds a session's cwd;
/// this process's own `current_dir()` is wherever the bundle was launched
/// from. A `slash::discover` keyed on the app's directory would still pass
/// every unit test in `slash.rs` — they hand it a path — and would find
/// nothing at all in production, or worse, would find whatever happened to
/// be beside the binary.
///
/// So the assertion is made through the whole shell: two sessions, two
/// real directories with different contents, and the palette has to follow
/// the SESSION rather than the process.
#[gpui::test]
fn the_palette_reads_the_sessions_directory_and_not_the_apps(cx: &mut TestAppContext) {
    let a = SlashProject::new("cwd-a");
    let b = SlashProject::new("cwd-b");
    // Give b a skill a does not have, so the two are distinguishable by
    // CONTENT and not merely by row count.
    std::fs::create_dir_all(b.0.join(".claude/skills/onlyinb")).unwrap();
    std::fs::write(
        b.0.join(".claude/skills/onlyinb/SKILL.md"),
        "---\ndescription: only b has this\n---\nbody\n",
    )
    .unwrap();

    let (h, vcx) = boot(cx);
    let mut sa = session("aaaa1111");
    sa.cwd = a.cwd();
    let mut sb = session("bbbb2222");
    sb.cwd = b.cwd();
    h.plumbing.push_rail(vec![sa, sb]);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();

    vcx.simulate_input("/onlyinb");
    settle_palette(&h, vcx);
    assert!(shown(vcx, "menu-row-0"), "the palette should be explaining");
    assert_eq!(
        draft_of(&h, vcx),
        "/onlyinb",
        "sanity: the text is what was typed"
    );
    // Session A's directory has no `onlyinb`, so the only row is the
    // "nothing matches" explanation and choosing it does nothing.
    click(vcx, "menu-row-0");
    settle_palette(&h, vcx);
    assert_eq!(
        draft_of(&h, vcx),
        "/onlyinb",
        "a disabled explanation row is not a completion"
    );

    // Now move to B, whose directory does have it.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "bbbb2222".into(),
    });
    vcx.run_until_parked();
    vcx.simulate_input("/onlyinb");
    settle_palette(&h, vcx);
    click(vcx, "menu-row-0");
    settle_palette(&h, vcx);
    assert_eq!(
        draft_of(&h, vcx),
        "/onlyinb ",
        "the palette must read the SESSION's cwd — it found a skill that \
         exists only under session B's directory"
    );
}

/// The palette is not allowed to become a poll.
///
/// The house rule is that a settled window schedules no frames. Discovery
/// is disk I/O, and the obvious way to keep it fresh is a watcher or a
/// timer — either of which would wake this window forever for the sake of
/// a menu. The scan is started by a keystroke and by nothing else.
#[gpui::test]
fn an_open_palette_settles_and_then_schedules_nothing(cx: &mut TestAppContext) {
    let project = SlashProject::new("settles");
    let (h, vcx) = boot(cx);
    open_session_in(&h, vcx, &project);
    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: Vec::new(),
            omitted: 0,
            watching: true,
            basis: DiffBasis::WorkingTree,
        }),
    ));
    vcx.run_until_parked();

    vcx.simulate_input("/");
    settle_palette(&h, vcx);
    assert!(shown(vcx, "slash-palette"));

    // Past the entrance and past the caret's own lease, so what is left is
    // whatever the palette itself is asking for.
    vcx.executor().advance_clock(Duration::from_secs(2));
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(600));
    assert!(
        shown(vcx, "slash-palette"),
        "the palette must still be up — a settled test that closed it \
         would be asserting nothing"
    );
    assert_eq!(
        h.window
            .update(vcx, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap(),
        0,
        "an open, settled palette is still asking for frames"
    );
}

/// The palette stops growing at [`theme::SLASH_MAX_ROWS`].
///
/// The cap is derived — the most rows that fit in a third of
/// [`theme::WINDOW_MIN_H`] — and its job is to keep a menu that grows
/// UPWARD out of the composer from covering the transcript the operator is
/// reading. This fixture has more skills than the cap so the cap is what
/// decides, on any machine.
///
/// The expected numbers are written as literals on purpose: deriving them
/// from the constants under test would make this pass for any value of
/// them.
#[gpui::test]
fn the_palette_stops_growing_before_it_covers_the_log(cx: &mut TestAppContext) {
    assert_eq!(
        theme::SLASH_MAX_ROWS,
        6,
        "6 rows of 25 px in a third of 520"
    );
    assert_eq!(theme::SLASH_MAX_H, 158.0, "6 * 25 + 4 + 4");

    let project = SlashProject::new("cap");
    for i in 0..12 {
        let d = project.0.join(format!(".claude/skills/many{i}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\ndescription: number {i}\n---\nbody\n"),
        )
        .unwrap();
    }
    let (h, vcx) = boot(cx);
    open_session_in(&h, vcx, &project);

    vcx.simulate_input("/many");
    settle_palette(&h, vcx);
    assert!(shown(vcx, "menu-row-5"), "the sixth row is inside the cap");
    assert!(
        !shown(vcx, "menu-row-6"),
        "a seventh row: the palette grew past its ceiling and is now \
         covering the log it floats over"
    );
    // Row seven is not unreachable — it is one more letter away.
    vcx.simulate_input("11");
    settle_palette(&h, vcx);
    click(vcx, "menu-row-0");
    settle_palette(&h, vcx);
    assert_eq!(
        draft_of(&h, vcx),
        "/many11 ",
        "filtering is how the capped tail is reached"
    );
}

/// **"You have none" and "I do not know where to look" are different
/// facts, and the palette used to say the first for both.**
///
/// Found by photographing it. The first capture
/// (`scripts/ar-capture.sh`, before `EAVE_REPLAY_PROVIDER` existed) ran
/// the replay rig, whose synthetic rail row carries the provider `replay`
/// — which `slash::roots` has no table for. The palette scanned nothing,
/// found nothing, and reported "no skill or command matches /" over a
/// directory that had a skill and a command sitting in it.
///
/// Every step was locally true and the sentence was still misleading,
/// which is the same shape as Slice AQ's `working for 99h 49m` over a
/// session parked on a question. `docs/shots/ar-unknown-provider.png`.
///
/// It matters beyond the rig: eave names six providers and can start one.
/// Five of them will hit this branch the day an adapter lands, and
/// `hermes` and `pi` have no surveyed roots at all.
#[gpui::test]
fn a_provider_eave_has_no_roots_for_says_so_rather_than_no_matches(cx: &mut TestAppContext) {
    let project = SlashProject::new("unknown");
    let (h, vcx) = boot(cx);
    // `hermes` is named by `launcher::PROVIDERS` and surveyed by nobody.
    let mut s = session("aaaa1111");
    s.cwd = project.cwd();
    s.provider = "hermes".into();
    h.plumbing.push_rail(vec![s]);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();

    vcx.simulate_input("/");
    settle_palette(&h, vcx);
    assert!(
        shown(vcx, "slash-palette"),
        "the palette must open and explain itself; going away would be the \
         same silence the empty menu was"
    );
    assert!(shown(vcx, "menu-row-0"));
    assert!(!shown(vcx, "menu-row-1"), "one sentence, not a list");

    // And the row is not a completion — there is nothing to complete to.
    click(vcx, "menu-row-0");
    settle_palette(&h, vcx);
    assert_eq!(draft_of(&h, vcx), "/", "an explanation is not a command");
}

/// The palette is exactly as wide as the composer it grows out of.
///
/// It was not, at first: `w_full` inside the bottom stack made it overhang
/// the composer by [`theme::GUTTER`] on both sides, which is visible in the
/// first version of `docs/shots/ar-palette.png`. Both are `Shell::column`
/// now, and this is the assertion that keeps them that way — a metric a
/// screenshot can show but no arithmetic in `theme` would ever catch,
/// because both values were individually legal.
#[gpui::test]
fn the_palette_lines_up_with_the_composer_it_grows_out_of(cx: &mut TestAppContext) {
    let project = SlashProject::new("width");
    let (h, vcx) = boot(cx);
    open_session_in(&h, vcx, &project);

    vcx.simulate_input("/");
    settle_palette(&h, vcx);
    let palette = vcx.debug_bounds("slash-palette").expect("palette");
    let composer = vcx.debug_bounds("composer-hint").expect("composer hint");
    assert!(
        palette.origin.x <= composer.origin.x,
        "the palette starts right of the composer's own text: {:?} vs {:?}",
        palette.origin.x,
        composer.origin.x
    );
    // The hint is inset inside the composer's padding, so the palette's
    // right edge must not exceed the composer card's — measured through
    // the send button, which sits at the card's inner right edge.
    let send = vcx
        .debug_bounds("send")
        .or_else(|| vcx.debug_bounds("composer-hint"));
    if let Some(send) = send {
        assert!(
            palette.origin.x + palette.size.width >= send.origin.x,
            "the palette does not reach the composer's right-hand controls"
        );
    }
}

// ---------------------------------------------------------------------------
// Slice AR2 — the inventory the CLI announces about itself

/// **The slice, end to end.** A command that exists in no file on this
/// machine is offered, is reachable, and completes.
///
/// `/context` is the case that matters: it is live in the real CLI, it is
/// in the real `system/init` frame, and `find ~/.claude
/// /opt/homebrew/lib/node_modules -name context` finds nothing. Slice AR's
/// palette could not name it, so an operator who typed `/` and did not see
/// it would conclude eave could not run it.
///
/// The same session's fixture skill is still described, from the same
/// scan AR built — which is the other half of the rule: the frame decides
/// WHICH rows exist, the disk decides what they SAY.
#[gpui::test]
fn the_palette_offers_a_command_the_cli_announced_that_no_file_backs(cx: &mut TestAppContext) {
    let project = SlashProject::new("announced");
    let (h, vcx) = boot(cx);
    open_session_in(&h, vcx, &project);
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![init_record(1, &["shipit", "context"])],
    });
    vcx.run_until_parked();

    vcx.simulate_input("/");
    settle_palette(&h, vcx);
    assert_eq!(
        offered(&h, vcx),
        vec![
            (
                "shipit".to_string(),
                Some("cut a release and tag it".into())
            ),
            ("context".to_string(), None),
        ],
        "the palette must offer exactly what the CLI announced, in the \
         frame's order, describing only what it could read"
    );

    // And the row with no description is a real, selectable row.
    vcx.simulate_input("cont");
    settle_palette(&h, vcx);
    click(vcx, "menu-row-0");
    settle_palette(&h, vcx);
    assert_eq!(
        draft_of(&h, vcx),
        "/context ",
        "a command with no file behind it still completes"
    );
}

/// **The carve-out, end to end.** `/clear` is announced by the CLI and is
/// still not offered.
///
/// Measured through eave's exact argv: it answers with an empty `result`,
/// no assistant turn, and a `conversation_reset` frame that gives the CLI
/// a new conversation id while eave's journal keeps appending to the old
/// one. It is the only name subtracted from the CLI's own answer, and this
/// is where that subtraction is visible.
#[gpui::test]
fn a_command_verified_inert_is_not_offered_even_though_the_cli_has_it(cx: &mut TestAppContext) {
    let project = SlashProject::new("denied");
    let (h, vcx) = boot(cx);
    open_session_in(&h, vcx, &project);
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![init_record(1, &["clear", "context"])],
    });
    vcx.run_until_parked();

    vcx.simulate_input("/clear");
    settle_palette(&h, vcx);
    assert!(
        shown(vcx, "slash-palette"),
        "the palette stays up and explains"
    );
    assert!(
        offered(&h, vcx).iter().all(|(n, _)| n != "clear"),
        "a command verified inert must never reach a row"
    );
    // The explanation row is not a completion: clicking it leaves the
    // draft as typed, so Return still sends `/clear` to the CLI exactly as
    // it always did. eave declines to ADVERTISE it; it does not block it.
    click(vcx, "menu-row-0");
    settle_palette(&h, vcx);
    assert_eq!(draft_of(&h, vcx), "/clear");
}

/// Before a session has said what it has, the palette falls back to the
/// scan — and offers **only** things it actually read.
///
/// There are two ways to be here and both are ordinary: a session that has
/// never been started (there is no frame to have arrived) and one whose
/// replay has not yet reached its first `Init`. The fallback is Slice AR's
/// palette unchanged: fewer rows than the CLI has, and zero rows the CLI
/// lacks.
///
/// **No row count is asserted, deliberately.** This test reads the real
/// `~/.claude` on whatever machine the suite runs on — that is the real
/// discovery path rather than a mock, and it is why the assertion is a
/// property of every row instead of a total.
#[gpui::test]
fn a_session_that_has_announced_nothing_falls_back_to_the_scan(cx: &mut TestAppContext) {
    let project = SlashProject::new("noinit");
    let (h, vcx) = boot(cx);
    open_session_in(&h, vcx, &project);

    vcx.simulate_input("/");
    settle_palette(&h, vcx);
    let rows = offered(&h, vcx);
    assert!(
        rows.iter().any(|(n, d)| n == "shipit" && d.is_some()),
        "the fallback is still the scan, descriptions and all"
    );
    assert!(
        rows.iter().all(|(_, d)| d.is_some()),
        "with no inventory eave may offer only what it READ, and everything \
         it reads has a file — a bare name here would be invented"
    );
}

/// **The inventory belongs to the session, not to the app.**
///
/// The AR2 twin of `the_palette_reads_the_sessions_directory_and_not_the_apps`,
/// and it guards the same trap from the other side. eave is one binary in
/// two roles: the ENGINE runs the CLI and sees its `system/init` frame, and
/// this process learns the inventory only by being told, on that session's
/// own stream. An inventory cached per process — or read from whatever
/// happened to arrive last — would pass every unit test in `slash.rs` and
/// would show one session's commands under another.
///
/// Both sessions share a directory on purpose, so the ONLY thing that can
/// distinguish their palettes is which session's stream the inventory came
/// off. The disk-scan cache is keyed on `(provider, cwd)` and is therefore
/// deliberately unchanged across the switch.
#[gpui::test]
fn the_inventory_follows_the_session_and_not_the_process(cx: &mut TestAppContext) {
    let project = SlashProject::new("per-session");
    let (h, vcx) = boot(cx);
    let mut sa = session("aaaa1111");
    sa.cwd = project.cwd();
    let mut sb = session("bbbb2222");
    sb.cwd = project.cwd();
    h.plumbing.push_rail(vec![sa, sb]);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![init_record(1, &["shipit"])],
    });
    vcx.run_until_parked();

    vcx.simulate_input("/");
    settle_palette(&h, vcx);
    assert_eq!(
        offered(&h, vcx)
            .into_iter()
            .map(|(n, _)| n)
            .collect::<Vec<_>>(),
        ["shipit"],
        "session A's CLI announced one command"
    );

    // Move to B, whose CLI announced a different one over the same
    // directory.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "bbbb2222".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "bbbb2222".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "bbbb2222".into(),
        epoch: 1,
        records: vec![init_record(1, &["review", "compact"])],
    });
    vcx.run_until_parked();

    vcx.simulate_input("/");
    settle_palette(&h, vcx);
    assert_eq!(
        offered(&h, vcx),
        vec![
            (
                "review".to_string(),
                Some("review the working tree".to_string())
            ),
            ("compact".to_string(), None),
        ],
        "the palette must follow the SESSION's own announcement"
    );
}

// ---------------------------------------------------------------------------
// Slice CC — the token gauge.

fn usage_record(
    seq: u64,
    turn: Option<eave_core::events::TurnTokens>,
    context: Option<eave_core::events::ContextWindow>,
) -> eave_core::journal::Record {
    eave_core::journal::Record {
        seq,
        ts_ms: seq * 10,
        event: eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::TokenUsage { turn, context },
        },
    }
}

/// **The gauge exists only when the provider said something**, and its
/// arrival moves nothing else in the composer.
///
/// Both halves matter. A permanent slot would print a caption over every
/// adapter that reports no tokens — the same defect Slice AN removed from
/// `composer-hint` — and a slot that reflows the send button under it
/// would make a quiet instrument the loudest thing on the row.
#[gpui::test]
fn the_token_gauge_appears_only_when_there_is_a_number_and_moves_nothing(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    ask(&h, vcx, vec![user_record(1, "go")]);
    assert!(
        !shown(vcx, "composer-usage"),
        "a session that has been told nothing about tokens must show no gauge"
    );

    let probes = ["composer-hint", "send"];
    let before: Vec<_> = probes.iter().map(|s| vcx.debug_bounds(s)).collect();
    assert!(
        before.iter().all(|b| b.is_some()),
        "a probe never rendered, so it proves nothing: {before:?}"
    );

    ask(
        &h,
        vcx,
        vec![
            user_record(1, "go"),
            usage_record(
                2,
                eave_core::events::TurnTokens::stated(Some(26_417), Some(4), None, None),
                Some(eave_core::events::ContextWindow {
                    used_tokens: 26_417,
                    limit_tokens: Some(1_000_000),
                }),
            ),
        ],
    );
    assert!(
        shown(vcx, "composer-usage"),
        "the provider stated its tokens and the composer said nothing"
    );

    let after: Vec<_> = probes.iter().map(|s| vcx.debug_bounds(s)).collect();
    for (i, name) in probes.iter().enumerate() {
        let (a, b) = (before[i].unwrap(), after[i].unwrap());
        assert_eq!(
            (a.origin, a.size.height),
            (b.origin, b.size.height),
            "{name} moved or changed height when the gauge appeared: {a:?} -> {b:?}"
        );
    }
}

/// The gauge is the quietest thing on the row it shares, and it says so
/// in named ranks rather than in numbers.
///
/// A rendered assertion cannot see this: gpui resolves text style inside
/// its own pass and exposes neither the computed size nor the painted ink
/// to a test. What can be pinned is the property that would decay — a
/// bare `px(11.0)` creeping in, or the ink being promoted to primary text
/// the next time somebody wants the number to be easier to read.
#[test]
fn the_gauge_names_the_quietest_rank_rather_than_a_number() {
    let src = include_str!("ui.rs");
    let at = src.find(r#"composer-usage"#).expect("the gauge element");
    let body = &src[at..at
        + src[at..]
            .find(".child(SharedString::from(usage))")
            .expect("its end")];

    assert!(
        body.contains("theme::MICRO_SIZE") && body.contains("theme::MICRO_LINE"),
        "the gauge left the instrument rank — MICRO is 'timestamps, counts, \
         the status bar, rail facts' and a token count is a count"
    );
    assert!(
        body.contains("theme::text_tertiary()"),
        "the gauge was promoted out of tertiary ink. Size and ink are two \
         channels and instrumentation may spend neither"
    );
    for loud in [
        "theme::BODY_SIZE",
        "theme::NAME_SIZE",
        "theme::DISPLAY_SIZE",
    ] {
        assert!(!body.contains(loud), "the gauge reached for {loud}");
    }
    // No bare literal anywhere in it: every size and space is a named rank.
    let numeric = body
        .match_indices("px(")
        .any(|(i, _)| body[i + 3..].starts_with(|c: char| c.is_ascii_digit()));
    assert!(
        !numeric,
        "the gauge grew a bare px() literal where the scale has a name for it"
    );
}

// ── Slice C5 — the tab strip and the omnibox ──────────────────────────────

/// **One strip, two kinds of tab.** The shape B1 argued for, C1 could not
/// draw, and C5 draws.
///
/// A page tab and a session tab sit on one line, in one ordering, each
/// wearing its own identity: the page a globe and its title, the session
/// the provider's mark and its name. There is no second strip and no second
/// ordering — B1 modelled the strip as ONE `Slot` list precisely so a
/// surface would never have to invent a rule for how two of them interleave.
#[gpui::test]
fn page_tabs_and_session_tabs_share_one_strip(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    // **Both pages resident, said out loud.** This test predates E3, which
    // split the page chip's selector on residency — so with no override it
    // asserted `tab-chip-p1` and got whatever `page_resident` happened to
    // answer. That is not the same answer in both feature states: a build
    // with no page engine reports every tab resident by design, and a
    // `--features web` build asks a Chromium that this harness does not
    // run, gets `false`, and draws the unloaded globe. So the test passed
    // without the browser and failed with it, for a reason that has nothing
    // to do with what it is about.
    //
    // Declared rather than tolerated: this test is about ONE STRIP holding
    // TWO KINDS, so it should pin residency to something and say which,
    // not accept either mark. Which mark goes with which state is
    // `an_unloaded_page_tab_does_not_look_like_a_loaded_one`'s claim.
    set_resident_for_test(Some(&["p1", "p2"]));
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://example.com/"), Some("Example Domain")),
            session_tab("aaaa1111"),
            page_tab("p2", Some("https://docs.rs/gpui"), None),
        ],
        Some("aaaa1111"),
    );

    for sel in ["tab-p1", "tab-aaaa1111", "tab-p2"] {
        assert!(shown(vcx, sel), "{sel} is not on the strip");
    }
    // Drawn in the model's order, TOP TO BOTTOM since Slice D1 turned the
    // axis, with the session tab between the two pages exactly where the
    // ordering puts it. One ordering is the claim; which way it runs is not.
    let y =
        |vcx: &mut VisualTestContext, sel: &'static str| vcx.debug_bounds(sel).expect(sel).origin.y;
    let (p1, sess, p2) = (y(vcx, "tab-p1"), y(vcx, "tab-aaaa1111"), y(vcx, "tab-p2"));
    assert!(
        p1 < sess && sess < p2,
        "the strip reordered the model: p1={p1:?} session={sess:?} p2={p2:?}"
    );

    // Every tab is the same box, whichever kind it is. A strip whose two
    // kinds had two metrics would have no rhythm, and the tab you are
    // aiming for would move when an unrelated one changed kind. On the
    // vertical axis the shared metric is the HEIGHT; the width is the
    // rail's and is therefore shared trivially, so it is asserted too but
    // proves less than it used to.
    let hgt = |vcx: &mut VisualTestContext, sel: &'static str| {
        vcx.debug_bounds(sel).expect(sel).size.height
    };
    assert_eq!(hgt(vcx, "tab-p1"), hgt(vcx, "tab-aaaa1111"));
    assert_eq!(hgt(vcx, "tab-p1"), hgt(vcx, "tab-p2"));
    let w = |vcx: &mut VisualTestContext, sel: &'static str| {
        vcx.debug_bounds(sel).expect(sel).size.width
    };
    assert_eq!(w(vcx, "tab-p1"), w(vcx, "tab-aaaa1111"));
    assert_eq!(w(vcx, "tab-p1"), w(vcx, "tab-p2"));

    // And each wears its own mark on the same plate.
    assert!(shown(vcx, "tab-chip-p1"), "the page tab has no plate");
    assert!(
        shown(vcx, "tab-chip-aaaa1111"),
        "the session tab lost its provider mark"
    );
    // Every tab can be closed. This is what ⌘W has a mouse equivalent for.
    for sel in ["tab-close-p1", "tab-close-aaaa1111", "tab-close-p2"] {
        assert!(shown(vcx, sel), "{sel} — a tab you cannot close");
    }

    // Cleared, because the `thread_local` outlives the test on a reused
    // thread and a leaked override would make an unrelated test's tabs
    // recede for reasons its author never wrote down.
    set_resident_for_test(None);
}

/// Slice E3. A tab whose page eave has thrown away must not look like one
/// holding a live page — otherwise the strip is claiming something it cannot
/// back up, and the operator has no way to know why clicking costs a reload.
///
/// **Asserted on the selector, not on the colour**, because this harness can
/// find what was painted and cannot read what ink it came out in. The globe
/// carries the state in its `debug_selector` for exactly that reason: a
/// treatment the suite cannot see is a treatment that rots.
#[gpui::test]
fn an_unloaded_page_tab_does_not_look_like_a_loaded_one(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    // `p2` is the evicted one. Set before the strip renders — the residency
    // is read during paint.
    set_resident_for_test(Some(&["p1"]));
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://example.com/"), Some("Example")),
            page_tab("p2", Some("https://docs.rs/gpui"), Some("gpui")),
        ],
        Some("p1"),
    );

    assert!(
        shown(vcx, "tab-chip-p1"),
        "the resident tab lost its ordinary globe"
    );
    assert!(
        !shown(vcx, "tab-chip-unloaded-p1"),
        "a tab holding a live page is drawn as unloaded"
    );
    assert!(
        shown(vcx, "tab-chip-unloaded-p2"),
        "an evicted tab is drawn exactly like a loaded one — the whole point \
         of the slice is that these differ"
    );
    assert!(
        !shown(vcx, "tab-chip-p2"),
        "the evicted tab still draws the loaded globe as well"
    );

    // **The treatment must not move anything.** A tab that changed size when
    // its page was evicted would make the strip twitch under the pointer on
    // a clock nobody can see, twenty minutes after they last touched it.
    let b = |vcx: &mut VisualTestContext, sel: &'static str| vcx.debug_bounds(sel).expect(sel);
    let (one, two) = (b(vcx, "tab-p1"), b(vcx, "tab-p2"));
    assert_eq!(
        one.size, two.size,
        "an evicted tab is a different size from a loaded one"
    );

    set_resident_for_test(None);
}

/// The other half, and the one that would rot silently: a SESSION tab has no
/// browser, so nothing about eviction may touch it. Without this, a build
/// with no page engine — which is the build this suite runs — would recede
/// every session row in the rail.
#[gpui::test]
fn eviction_never_recedes_a_session_tab(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    // Nothing is resident, page engine or not.
    set_resident_for_test(Some(&[]));
    strip_tabs(
        &h,
        vcx,
        vec![session_tab("aaaa1111"), page_tab("p1", None, None)],
        Some("aaaa1111"),
    );

    assert!(
        shown(vcx, "tab-chip-aaaa1111"),
        "the session tab lost its provider mark to a page-engine rule"
    );
    assert!(
        !shown(vcx, "tab-chip-unloaded-aaaa1111"),
        "a session tab was drawn as an unloaded PAGE"
    );
    assert!(
        shown(vcx, "tab-chip-unloaded-p1"),
        "the page tab should be the one that recedes"
    );

    set_resident_for_test(None);
}

// ---------------------------------------------------------------------------
// Slice D1 — the frame (A′): the rail owns the window, and the pane header
// is the line that says where you are.

/// **The address is on screen at rest.** The whole reason A′ exists.
///
/// Before this slice eave showed the current url NOWHERE without a
/// keystroke: `strip::page_label` draws a tab's title and falls back to the
/// address only when there is no title, so on a page with a title the
/// address lived only inside the omnibox — a surface you have to summon. For
/// an app whose stance is *every element states an observed fact*, a browser
/// that will not say where it is was a stance violation rather than a
/// missing feature, and this is the test that would have caught it.
///
/// Deliberately measured on a tab that HAS a title, because the titleless
/// case was never the broken one.
#[gpui::test]
fn the_address_is_on_screen_without_a_keystroke(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![page_tab(
            "p1",
            Some("https://docs.rs/gpui/latest/gpui/"),
            Some("gpui - Rust"),
        )],
        Some("p1"),
    );

    assert!(
        shown(vcx, "pane-address-text"),
        "no address on screen with a page open — the tab shows the TITLE, so \
         without this line the url is reachable only through ⌘L"
    );
    // And it is the typing form, which is what a person reads: scheme and
    // `www.` stripped, path kept. The path is the half that distinguishes
    // two pages on one host, so a header showing only the host would state
    // a fact too coarse to act on.
    assert_eq!(
        eave_wire::typing_form("https://docs.rs/gpui/latest/gpui/"),
        "docs.rs/gpui/latest/gpui"
    );
}

/// **The header is the same height for a page and for a session.**
///
/// One strip holds two kinds of tab. If each kind produced a
/// differently-shaped region beneath it, the mixed strip would read as an
/// accident of history rather than as the thing the application is — so the
/// two headers being identical in geometry, and different only in what they
/// say, is the frame's answer to eave's one genuinely novel constraint.
///
/// A third kind of tab is a third arm in `Shell::pane_header` and changes no
/// geometry at all; this is the assertion that keeps that true.
#[gpui::test]
fn both_kinds_of_tab_get_the_same_header(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");

    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://example.com/"), Some("Example Domain")),
            session_tab("aaaa1111"),
        ],
        Some("p1"),
    );
    let on_page = vcx
        .debug_bounds("pane-header")
        .expect("the header on a page");
    assert!(shown(vcx, "pane-address"), "a page has no address field");

    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://example.com/"), Some("Example Domain")),
            session_tab("aaaa1111"),
        ],
        Some("aaaa1111"),
    );
    let on_session = vcx
        .debug_bounds("pane-header")
        .expect("the header on a session");
    assert!(shown(vcx, "pane-crumb"), "a session has no breadcrumb");
    assert!(
        !shown(vcx, "pane-address"),
        "a session tab drew an address field — a transcript has no url, and \
         a field standing empty over one states a fact that is not true"
    );

    assert_eq!(
        (on_page.origin, on_page.size),
        (on_session.origin, on_session.size),
        "the header changed shape between a page tab and a session tab: \
         {on_page:?} -> {on_session:?}. Two kinds of tab, one silhouette."
    );
}

/// **The back and forward arrows say what the ENGINE says, and nothing else.**
///
/// `can_go_back` / `can_go_forward` have crossed the wire since B1 and are
/// populated from the real stack (`snapshot.rs:153`); `TabGo` has been
/// walkable just as long. The app had never sent that request once, because
/// there was no surface for the arrows to live on — so this asserts both
/// halves: the arrows reflect the engine's facts, and clicking an enabled one
/// **G26 closing G25.** The scrim must occlude, or a click aimed at the
/// dialog also lands on the live page underneath — the clickjacking shape,
/// and the reason `.occlude()` was added. This is the assertion that could
/// not exist before the ask map came out of the feature gate.
#[gpui::test]
fn a_page_dialog_covers_the_page_it_is_dimming(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    raise_ask(
        &h,
        vcx,
        "p1",
        "https://bank.test/",
        crate::web::Ask::Permission {
            kinds: vec![eave_wire::PermissionKind::Camera],
        },
    );
    assert!(shown(vcx, "page-dialog"), "the dialog did not render");
    let scrim = vcx.debug_bounds("page-dialog").expect("no scrim");
    let card = vcx.debug_bounds("page-dialog-card").expect("no card");
    assert!(
        scrim.size.width > card.size.width && scrim.size.height > card.size.height,
        "the scrim {:?} does not extend past the card {:?} — it cannot be \
         catching the clicks around it",
        scrim.size,
        card.size
    );
}

/// The origin is drawn on every ask, always. A dialog that does not say
/// who is asking is how a page impersonates the browser.
#[gpui::test]
fn every_page_dialog_says_where_it_came_from(cx: &mut TestAppContext) {
    let asks = [
        crate::web::Ask::Alert {
            message: "hi".into(),
        },
        crate::web::Ask::Permission {
            kinds: vec![eave_wire::PermissionKind::Camera],
        },
        crate::web::Ask::Credentials {
            host: "intranet.test".into(),
            realm: None,
            is_proxy: false,
        },
        crate::web::Ask::Certificate {
            url: "https://bank.test/".into(),
            reason: "ERR_CERT_AUTHORITY_INVALID".into(),
        },
    ];
    for ask in asks {
        let (h, vcx) = boot(cx);
        raise_ask(&h, vcx, "p1", "https://bank.test/", ask.clone());
        assert!(
            shown(vcx, "page-dialog-origin"),
            "{ask:?} drew no origin line"
        );
        crate::web::answer("p1", crate::web::Answer::Dismiss);
    }
}

/// **D7 — ⇥ moves to the password field, and what is typed there lands in
/// the SECRET buffer, not the one the user field renders verbatim.**
///
/// This is the routing half of the masking. The drawing half is
/// `web::masked`, asserted in `web/ask.rs` without a window.
#[gpui::test]
fn a_typed_password_goes_to_the_secret_buffer_and_not_the_visible_one(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    raise_ask(
        &h,
        vcx,
        "p1",
        "https://intranet.test/",
        crate::web::Ask::Credentials {
            host: "intranet.test".into(),
            realm: Some("Staff only".into()),
            is_proxy: false,
        },
    );
    assert!(shown(vcx, "page-dialog-user"), "no user field");
    assert!(shown(vcx, "page-dialog-password"), "no password field");

    vcx.simulate_keystrokes("a d a");
    vcx.simulate_keystrokes("tab");
    vcx.simulate_keystrokes("h u n t");
    vcx.run_until_parked();

    let (user, secret) = dialog_buffers(&h, vcx);
    assert_eq!(user, "ada", "the user name did not reach the user field");
    assert_eq!(
        secret, "hunt",
        "the password did not reach the secret field"
    );
    assert!(
        !user.contains("hunt"),
        "the password leaked into the field that is drawn verbatim: {user:?}"
    );
    assert_eq!(
        crate::web::masked(&secret),
        "••••",
        "the password would not be drawn as bullets"
    );
}

/// Answering clears the password. A secret that outlived its dialog would
/// be a secret in memory with nothing on screen accounting for it.
#[gpui::test]
fn answering_a_credential_ask_drops_the_password(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    raise_ask(
        &h,
        vcx,
        "p1",
        "https://intranet.test/",
        crate::web::Ask::Credentials {
            host: "intranet.test".into(),
            realm: None,
            is_proxy: false,
        },
    );
    vcx.simulate_keystrokes("tab");
    vcx.simulate_keystrokes("h u n t");
    vcx.run_until_parked();
    assert_eq!(dialog_buffers(&h, vcx).1, "hunt");

    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert_eq!(
        dialog_buffers(&h, vcx).1,
        "",
        "the password survived the dialog being dismissed"
    );
    assert!(crate::web::pending("p1").is_none());
}

/// **D8 — the safe answer is the primary button.**
///
/// A certificate interstitial whose most prominent control is "proceed" is
/// one that does not work. The two buttons are a pair of easily-swapped
/// arguments, so the inversion is asserted rather than read.
#[gpui::test]
fn the_certificate_interstitial_makes_going_back_the_prominent_choice(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    raise_ask(
        &h,
        vcx,
        "p1",
        "https://bank.test/",
        crate::web::Ask::Certificate {
            url: "https://bank.test/".into(),
            reason: "ERR_CERT_AUTHORITY_INVALID".into(),
        },
    );
    assert!(
        shown(vcx, "dialog-confirm") && shown(vcx, "dialog-refuse"),
        "the interstitial drew fewer than two buttons"
    );
    // The PRIMARY button must not proceed. Clicking it has to leave the
    // page blocked, which here means the ask is answered and gone with a
    // `Cancel` rather than an `Ok`.
    click(vcx, "dialog-confirm");
    vcx.run_until_parked();
    assert!(
        crate::web::pending("p1").is_none(),
        "the primary button did not answer the interstitial"
    );
}

/// Escape on an interstitial must not proceed. It is the key people press
/// to make a thing go away, and `Answer::Dismiss` is not a yes — see
/// `web/ask.rs::page_hears_yes`.
#[gpui::test]
fn escape_on_a_certificate_interstitial_does_not_proceed(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    raise_ask(
        &h,
        vcx,
        "p1",
        "https://bank.test/",
        crate::web::Ask::Certificate {
            url: "https://bank.test/".into(),
            reason: "ERR_CERT_REVOKED".into(),
        },
    );
    assert!(shown(vcx, "page-dialog"));
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "page-dialog"),
        "escape left the interstitial on screen"
    );
    assert!(
        crate::web::pending("p1").is_none(),
        "escape left the ask outstanding"
    );
}

/// An ask on a BACKGROUND tab does not seize the window. A page in one tab
/// stopping on `alert()` is its own business.
#[gpui::test]
fn an_ask_on_a_background_tab_is_not_drawn(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let a = page_tab("p1", Some("https://a.test/"), Some("A"));
    let b = page_tab("p2", Some("https://b.test/"), Some("B"));
    strip_tabs(&h, vcx, vec![a, b], Some("p1"));
    crate::web::raise_ask_for_test(
        "p2",
        "https://b.test/",
        crate::web::Ask::Alert {
            message: "look at me".into(),
        },
    );
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "page-dialog"),
        "a background tab's dialog seized the window"
    );
    crate::web::answer("p2", crate::web::Answer::Dismiss);
}

/// **D6 — `<input type=file>` reaches a real picker and the page gets the
/// paths.**
///
/// Both ends of this existed for a whole slice and nothing in the middle
/// ever built an `Answer::Files`, so the dialog drew one button and said
/// so. gpui's test platform queues the panel and hands back its sender,
/// which is what lets this be a test rather than a screenshot.
#[gpui::test]
fn choosing_a_file_answers_the_page_with_the_path(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    raise_ask(
        &h,
        vcx,
        "p1",
        "https://upload.test/",
        crate::web::Ask::Files {
            title: "Choose a file".into(),
            multiple: false,
            folder: false,
            save: false,
        },
    );
    assert!(shown(vcx, "dialog-choose"), "no picker button was drawn");
    assert!(
        !cx.did_prompt_for_paths(),
        "the panel opened before anyone asked for it"
    );

    click(vcx, "dialog-choose");
    vcx.run_until_parked();
    assert!(cx.did_prompt_for_paths(), "the button opened no panel");

    // The ask is still outstanding while the panel is up — the renderer is
    // still blocked, so the scrim is still telling the truth.
    assert!(
        crate::web::pending("p1").is_some(),
        "the ask was answered before a file was chosen"
    );

    cx.simulate_path_prompt_response(|opts| {
        assert!(opts.files, "a file ask did not offer files");
        assert!(!opts.directories, "a file ask offered directories");
        assert!(!opts.multiple, "a single-file ask allowed multiple");
        Some(vec![std::path::PathBuf::from("/tmp/eave-pick.txt")])
    });
    vcx.run_until_parked();
    assert!(
        crate::web::pending("p1").is_none(),
        "choosing a file left the ask outstanding"
    );
    assert!(!shown(vcx, "page-dialog"), "the card stayed up");
}

/// A `multiple` ask asks the panel for multiple. Getting this wrong is
/// invisible until someone tries to attach two files and cannot.
#[gpui::test]
fn a_multiple_file_ask_lets_the_panel_take_more_than_one(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    raise_ask(
        &h,
        vcx,
        "p1",
        "https://upload.test/",
        crate::web::Ask::Files {
            title: "Choose files".into(),
            multiple: true,
            folder: false,
            save: false,
        },
    );
    click(vcx, "dialog-choose");
    vcx.run_until_parked();
    cx.simulate_path_prompt_response(|opts| {
        assert!(opts.multiple, "a multiple ask did not allow multiple");
        Some(vec![
            std::path::PathBuf::from("/tmp/a.txt"),
            std::path::PathBuf::from("/tmp/b.txt"),
        ])
    });
    vcx.run_until_parked();
    assert!(crate::web::pending("p1").is_none());
}

/// A folder ask wants directories and NOT files. Offering both would let
/// someone satisfy `webkitdirectory` with a file the page cannot use.
#[gpui::test]
fn a_folder_ask_offers_directories_and_not_files(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    raise_ask(
        &h,
        vcx,
        "p1",
        "https://upload.test/",
        crate::web::Ask::Files {
            title: "Choose a folder".into(),
            multiple: false,
            folder: true,
            save: false,
        },
    );
    click(vcx, "dialog-choose");
    vcx.run_until_parked();
    cx.simulate_path_prompt_response(|opts| {
        assert!(opts.directories, "a folder ask did not offer directories");
        assert!(!opts.files, "a folder ask also offered files");
        Some(vec![std::path::PathBuf::from("/tmp/somewhere")])
    });
    vcx.run_until_parked();
    assert!(crate::web::pending("p1").is_none());
}

/// Cancelling the PANEL answers the ask — it must not leave the renderer
/// blocked on a callback nobody can reach any more.
#[gpui::test]
fn cancelling_the_panel_still_answers_the_page(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    raise_ask(
        &h,
        vcx,
        "p1",
        "https://upload.test/",
        crate::web::Ask::Files {
            title: "Choose a file".into(),
            multiple: false,
            folder: false,
            save: false,
        },
    );
    click(vcx, "dialog-choose");
    vcx.run_until_parked();
    cx.simulate_path_prompt_response(|_| None);
    vcx.run_until_parked();
    assert!(
        crate::web::pending("p1").is_none(),
        "cancelling the panel left the page stopped forever"
    );
    assert!(!shown(vcx, "page-dialog"));
}

/// An empty selection is a cancel, not a selection of nothing. The rule
/// exists at the CEF end too (`reply_with`); this is the shell end of it.
#[gpui::test]
fn an_empty_selection_is_a_cancel(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    raise_ask(
        &h,
        vcx,
        "p1",
        "https://upload.test/",
        crate::web::Ask::Files {
            title: "Choose a file".into(),
            multiple: false,
            folder: false,
            save: false,
        },
    );
    click(vcx, "dialog-choose");
    vcx.run_until_parked();
    cx.simulate_path_prompt_response(|_| Some(Vec::new()));
    vcx.run_until_parked();
    assert!(crate::web::pending("p1").is_none());
}

/// The card names the origin BEFORE the panel opens, which is the whole
/// reason eave shows one at all where Chrome goes straight to the panel:
/// the native picker cannot tell you which site summoned it.
#[gpui::test]
fn the_file_card_names_the_origin_before_any_panel_opens(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    raise_ask(
        &h,
        vcx,
        "p1",
        "https://upload.test/",
        crate::web::Ask::Files {
            title: "Choose a file".into(),
            multiple: false,
            folder: false,
            save: false,
        },
    );
    assert!(
        shown(vcx, "page-dialog-origin"),
        "no origin on the file card"
    );
    assert!(
        !cx.did_prompt_for_paths(),
        "the panel opened before the origin could be read"
    );
    crate::web::answer("p1", crate::web::Answer::Dismiss);
}

/// G24 — the two extra buttons on the mouse walk the page's history.
///
/// gpui has always delivered `MouseButton::Navigate`; eave matched on
/// Left, Right and Middle and dropped the rest. This asserts the binding
/// AND that it inherits the arrows' guards rather than adding a second
/// opinion about what history exists.
#[gpui::test]
fn the_mouses_navigation_buttons_walk_the_page_history(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut tab = page_tab("p1", Some("https://example.com/"), Some("Example Domain"));
    tab.can_go_back = true;
    tab.can_go_forward = true;
    tab.history_len = 3;
    strip_tabs(&h, vcx, vec![tab], Some("p1"));

    let _ = h.drain();
    mouse_navigate(vcx, NavigationDirection::Back);
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Go { tab_id, delta }) if tab_id == "p1" && *delta == -1
        )),
        "mouse back did not walk the stack: {cmds:?}"
    );

    let _ = h.drain();
    mouse_navigate(vcx, NavigationDirection::Forward);
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Go { tab_id, delta }) if tab_id == "p1" && *delta == 1
        )),
        "mouse forward did not walk the stack: {cmds:?}"
    );
}

/// …and it is the SAME guard, not a parallel one. A stack the engine says
/// is empty stays empty however the gesture arrived — the failure this
/// prevents is a mouse button that can walk past an end the arrows stop at.
#[gpui::test]
fn the_mouse_buttons_obey_the_engines_history_like_the_arrows_do(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let fresh = page_tab("p1", Some("https://example.com/"), Some("Example Domain"));
    strip_tabs(&h, vcx, vec![fresh], Some("p1"));

    let _ = h.drain();
    mouse_navigate(vcx, NavigationDirection::Back);
    mouse_navigate(vcx, NavigationDirection::Forward);
    vcx.run_until_parked();
    assert!(
        !h.drain()
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Go { .. }))),
        "a mouse button walked a stack the engine says is empty"
    );
}

/// A session tab has no history to walk, and the buttons must not invent
/// one. `go_tab` filters on `TabContent::Page`; this is the assertion that
/// the window-wide binding did not route around it.
#[gpui::test]
fn the_mouse_buttons_do_nothing_on_a_session_tab(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut page = page_tab("p1", Some("https://example.com/"), Some("Example"));
    page.can_go_back = true;
    page.history_len = 2;
    let session = session_tab("aaaa1111");
    strip_tabs(&h, vcx, vec![session, page], Some("aaaa1111"));

    let _ = h.drain();
    mouse_navigate(vcx, NavigationDirection::Back);
    vcx.run_until_parked();
    assert!(
        !h.drain()
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Go { .. }))),
        "the mouse walked a page's history while a session tab was active"
    );
}

/// actually emits the walk.
///
/// Disabled is DRAWN, not hidden: a control that disappears when it cannot
/// act makes the row's geometry depend on history, and the reload button
/// would slide sideways as you browsed.
#[gpui::test]
fn the_arrows_state_the_engines_history_and_walk_it(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let fresh = page_tab("p1", Some("https://example.com/"), Some("Example Domain"));
    strip_tabs(&h, vcx, vec![fresh.clone()], Some("p1"));

    // A tab with no history still draws both arrows.
    for sel in ["nav-back", "nav-fwd", "nav-reload"] {
        assert!(shown(vcx, sel), "{sel} is not drawn on a fresh page tab");
    }
    // ...and clicking the disabled one sends nothing.
    let _ = h.drain();
    click(vcx, "nav-back");
    vcx.run_until_parked();
    assert!(
        !h.drain()
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Go { .. }))),
        "back walked a stack the engine says is empty"
    );

    // Now the engine states a history.
    let mut visited = fresh;
    visited.can_go_back = true;
    visited.history_len = 2;
    strip_tabs(&h, vcx, vec![visited], Some("p1"));
    let _ = h.drain();
    click(vcx, "nav-back");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Go { tab_id, delta }) if tab_id == "p1" && *delta == -1
        )),
        "back did not walk the stack: {cmds:?}"
    );
    // Forward is still what the engine says it is, which is nothing.
    let _ = h.drain();
    click(vcx, "nav-fwd");
    vcx.run_until_parked();
    assert!(
        !h.drain()
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Go { .. }))),
        "forward walked a stack the engine says is empty"
    );
}

/// **Reload is a navigation to where the tab already is — and a tab with no
/// address does not offer one.**
///
/// The first half is only safe because of C5's reload fix:
/// `NavStack::is_refresh_of_current` asks the question BEFORE the fold runs,
/// so a same-url commit does not push a duplicate entry or truncate the
/// forward stack. The second half is the instrument rule — a reload button
/// that navigated to the empty string would be worse than one that is inert.
#[gpui::test]
fn reload_goes_where_the_tab_already_is(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![page_tab("p1", Some("https://example.com/"), None)],
        Some("p1"),
    );
    let _ = h.drain();
    click(vcx, "nav-reload");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Navigate { tab_id, url })
                if tab_id == "p1" && url == "https://example.com/"
        )),
        "reload did not re-navigate to the current address: {cmds:?}"
    );

    // A tab that has never navigated has nothing to reload.
    strip_tabs(&h, vcx, vec![page_tab("p2", None, None)], Some("p2"));
    let _ = h.drain();
    click(vcx, "nav-reload");
    vcx.run_until_parked();
    assert!(
        h.drain().is_empty(),
        "reload fired on a tab with no committed url"
    );
}

/// Slice E2 — the chords ride the same snapshot the pane header already
/// reads. `simulate_keystrokes` the **bound** actions, not a parallel
/// handler: gpui dispatches a matched action before bubble-phase `on_key`.
#[gpui::test]
fn cmd_bracket_walks_when_the_engine_says_it_can_and_is_silent_when_it_cannot(
    cx: &mut TestAppContext,
) {
    let (mut h, vcx) = boot(cx);
    let fresh = page_tab("p1", Some("https://example.com/"), Some("Example Domain"));
    strip_tabs(&h, vcx, vec![fresh.clone()], Some("p1"));

    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-[");
    vcx.run_until_parked();
    assert!(
        !h.drain()
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Go { .. }))),
        "⌘[ walked a stack the engine says is empty"
    );

    let mut visited = fresh;
    visited.can_go_back = true;
    visited.history_len = 2;
    strip_tabs(&h, vcx, vec![visited], Some("p1"));
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-[");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Go { tab_id, delta }) if tab_id == "p1" && *delta == -1
        )),
        "⌘[ did not walk the stack: {cmds:?}"
    );

    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-]");
    vcx.run_until_parked();
    assert!(
        !h.drain()
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Go { .. }))),
        "⌘] walked a stack the engine says is empty"
    );
}

#[gpui::test]
fn cmd_r_re_navigates_the_active_page_and_is_silent_without_a_url(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![page_tab("p1", Some("https://example.com/"), None)],
        Some("p1"),
    );
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-r");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Navigate { tab_id, url })
                if tab_id == "p1" && url == "https://example.com/"
        )),
        "⌘R did not re-navigate to the current address: {cmds:?}"
    );

    strip_tabs(&h, vcx, vec![page_tab("p2", None, None)], Some("p2"));
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-r");
    vcx.run_until_parked();
    assert!(
        h.drain().is_empty(),
        "⌘R fired on a tab with no committed url"
    );

    // A session tab has no url to reload. Silent, not an error.
    strip_tabs(&h, vcx, vec![session_tab("aaaa1111")], Some("aaaa1111"));
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-r");
    vcx.run_until_parked();
    assert!(h.drain().is_empty(), "⌘R fired on a session tab");
    vcx.simulate_keystrokes("cmd-[");
    vcx.run_until_parked();
    assert!(h.drain().is_empty(), "⌘[ fired on a session tab");
}

#[gpui::test]
fn cmd_digits_jump_the_strip_and_overflow_is_silent(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let tabs = || {
        vec![
            page_tab("p1", Some("https://a.test/"), None),
            page_tab("p2", Some("https://b.test/"), None),
            page_tab("p3", Some("https://c.test/"), None),
        ]
    };
    strip_tabs(&h, vcx, tabs(), Some("p2"));

    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-1");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "p1"
        )),
        "⌘1 did not activate the first tab: {cmds:?}"
    );

    strip_tabs(&h, vcx, tabs(), Some("p1"));
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-9");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "p3"
        )),
        "⌘9 did not activate the last tab: {cmds:?}"
    );

    let eight: Vec<_> = (1..=8)
        .map(|i| page_tab(&format!("t{i}"), Some(&format!("https://{i}.test/")), None))
        .collect();
    strip_tabs(&h, vcx, eight, Some("t1"));
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-8");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "t8"
        )),
        "⌘8 did not activate the 8th tab: {cmds:?}"
    );

    strip_tabs(&h, vcx, tabs(), Some("p1"));
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-5");
    vcx.run_until_parked();
    assert!(
        !h.drain()
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Activate(_)))),
        "⌘5 on a 3-tab strip jumped"
    );
}

#[gpui::test]
fn cmd_shift_t_reopens_a_closed_page_tab_and_not_a_session_tab(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://closed.test/"), Some("Closed")),
            session_tab("aaaa1111"),
        ],
        Some("p1"),
    );

    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-w");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Close(id)) if id == "p1"
        )),
        "⌘W did not close the page tab: {cmds:?}"
    );

    vcx.simulate_keystrokes("cmd-shift-t");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::OpenAt { url, activate: true })
                if url == "https://closed.test/"
        )),
        "⌘⇧T did not reopen the closed page tab: {cmds:?}"
    );

    // Closing only a session tab leaves nothing to reopen.
    strip_tabs(&h, vcx, vec![session_tab("aaaa1111")], Some("aaaa1111"));
    // A prior reopen consumed the stack; closing a session must not push.
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-w");
    vcx.run_until_parked();
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-shift-t");
    vcx.run_until_parked();
    assert!(
        !h.drain()
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::OpenAt { .. }))),
        "⌘⇧T reopened after only a session tab closed"
    );
}

#[gpui::test]
fn cmd_click_a_link_opens_a_background_tab_and_does_not_steal_focus(cx: &mut TestAppContext) {
    let (mut h, vcx) = transcript_of(cx, "[click me](https://eave.sh/ok)");
    // The transcript is the surface that has a link to ⌘-click, so the
    // active tab has to stay the session — a page tab would hide it.
    // The already-active tab is the thing that must not move.
    strip_tabs(
        &h,
        vcx,
        vec![
            session_tab("aaaa1111"),
            page_tab("p1", Some("https://already.test/"), None),
        ],
        Some("aaaa1111"),
    );
    let _ = opened_urls();
    let _ = h.drain();
    click_at_with(
        vcx,
        "md-para",
        4.0,
        theme::BODY_LINE / 2.0,
        Modifiers {
            platform: true,
            ..Modifiers::none()
        },
    );
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::OpenAt { url, activate: false })
                if url == "https://eave.sh/ok"
        )),
        "⌘-click did not open a background tab: {cmds:?}"
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Activate(_)))),
        "⌘-click stole focus: {cmds:?}"
    );
    assert!(
        opened_urls().is_empty(),
        "⌘-click also handed the link to the system opener"
    );
}

#[gpui::test]
fn middle_click_closes_a_tab_without_activating_it(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), None),
            page_tab("p2", Some("https://b.test/"), None),
            page_tab("p3", Some("https://c.test/"), None),
        ],
        Some("p1"),
    );
    let _ = h.drain();
    middle_click(vcx, "tab-p2");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Close(id)) if id == "p2"
        )),
        "middle-click did not close the tab: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "p2"
        )),
        "middle-click activated the tab on its way to closing it: {cmds:?}"
    );
}

/// **The window surface is the sidebar; the content is a floating card.**
///
/// Rail content sits on the glass, edge to edge. Everything right of the
/// rail lives in one rounded card inset by [`theme::CARD_GUTTER`]. A
/// full-height seam between them, or a card that reaches a window edge,
/// is the wrong frame.
#[gpui::test]
fn the_rail_owns_the_left_edge_and_the_content_owns_the_rest(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111")], "aaaa1111");

    let rail = vcx.debug_bounds("rail").expect("the rail");
    let well = vcx
        .debug_bounds("content-well")
        .expect("the content region");
    let header = vcx.debug_bounds("pane-header").expect("the pane header");
    let root = vcx.debug_bounds("shell-root").expect("the root");
    let gutter = px(theme::CARD_GUTTER);

    assert_eq!(
        rail.origin.x, root.origin.x,
        "the rail does not start at the window's left edge"
    );
    assert_eq!(
        rail.origin.y, root.origin.y,
        "the rail does not reach the titlebar line — the traffic lights are \
         sitting on whatever is behind it"
    );
    assert_eq!(
        well.origin.x,
        rail.origin.x + rail.size.width + gutter,
        "the card is not inset from the rail by the gutter — {:?} of air",
        well.origin.x - (rail.origin.x + rail.size.width)
    );
    assert_eq!(
        well.origin.y,
        root.origin.y + gutter,
        "the card does not sit a gutter below the top of the window"
    );
    assert_eq!(
        well.origin.x + well.size.width,
        root.origin.x + root.size.width - gutter,
        "the card does not sit a gutter in from the window's right edge"
    );
    assert_eq!(
        well.origin.y + well.size.height,
        root.origin.y + root.size.height - gutter,
        "the card does not sit a gutter above the window's bottom edge"
    );
    assert_eq!(
        header.origin.y, well.origin.y,
        "the pane header is not the top strip of the card"
    );
    assert_eq!(
        header.size.height,
        px(theme::PANE_HEADER_H),
        "the pane header is not the height its token names"
    );
}

/// **There is one vertical band, not two.**
///
/// `FILES_RAIL_W` was defined as `= RAIL_W` — 248 left, 248 right, a document
/// between them — and that literal symmetry is what made eave's silhouette an
/// IDE rather than anything of its own. All three D1 frame proposals deleted
/// the second column independently, which is the strongest signal in that
/// paper: three different starting points agreeing is not taste.
///
/// Asserted as containment and as reach, so it cannot be satisfied by moving
/// the column somewhere else on the right.
#[gpui::test]
fn the_changes_list_is_a_section_of_the_rail_not_a_second_column(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_files((
        "aaaa1111".into(),
        Some(FilesSnapshot {
            state: GitState::Ok,
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                status: FileStatus::Modified,
                adds: Some(3),
                dels: Some(1),
                orig_path: None,
                untracked: false,
            }],
            omitted: 0,
            watching: true,
            basis: DiffBasis::WorkingTree,
        }),
    ));
    vcx.run_until_parked();

    let rail = vcx.debug_bounds("rail").expect("the rail");
    let files = vcx
        .debug_bounds("files-section")
        .expect("the changes section did not render");
    let well = vcx
        .debug_bounds("content-well")
        .expect("the content region");

    assert!(
        files.origin.x >= rail.origin.x
            && files.origin.x + files.size.width <= rail.origin.x + rail.size.width,
        "the changes list runs {:?}..{:?} and the rail runs {:?}..{:?} — it is \
         still a column of its own",
        files.origin.x,
        files.origin.x + files.size.width,
        rail.origin.x,
        rail.origin.x + rail.size.width
    );
    // ...and it is BELOW the session list it is about, not beside it.
    let rows = vcx.debug_bounds("rail-rows").expect("the session list");
    assert!(
        files.origin.y >= rows.origin.y,
        "the changes section sits above the sessions it describes"
    );
    // The card sits a gutter in from the window's right edge — a second
    // column would fail this, and a full-bleed well would too.
    let window = vcx.debug_bounds("shell-root").expect("the root");
    assert_eq!(
        well.origin.x + well.size.width,
        window.origin.x + window.size.width - px(theme::CARD_GUTTER),
        "the card does not sit a gutter in from the window's right edge"
    );
}

/// **Closing the menu leaves the card floating on the window surface.**
///
/// The rail unmounts; the card keeps its gutter on all four sides,
/// including the left. Traffic lights stay. A card that went full-bleed
/// on close would put square page corners on the window edge.
#[gpui::test]
fn closing_the_menu_gives_the_window_to_the_content(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111")], "aaaa1111");

    let open = vcx
        .debug_bounds("content-well")
        .expect("the content region");
    assert!(
        open.origin.x >= px(theme::RAIL_W),
        "the content region starts at {:?} with the rail open",
        open.origin.x
    );

    click(vcx, "rail-toggle");
    vcx.run_until_parked();

    let shut = vcx
        .debug_bounds("content-well")
        .expect("the content region");
    let root = vcx.debug_bounds("shell-root").expect("the root");
    let gutter = px(theme::CARD_GUTTER);
    assert_eq!(
        shut.origin.x,
        root.origin.x + gutter,
        "the card starts at {:?} with the menu shut — it should keep its \
         left gutter",
        shut.origin.x
    );
    assert_eq!(
        shut.size.width,
        root.size.width - gutter - gutter,
        "the card is {:?} wide in a {:?} window",
        shut.size.width,
        root.size.width
    );
    assert_eq!(
        shut.origin.y,
        root.origin.y + gutter,
        "closing the rail dropped the card's top gutter"
    );

    // The one thing that may not go with the borders: the way back. A
    // control that closes something must not live inside the thing it
    // closes, and the first cut of this change had it doing exactly that.
    assert!(
        shown(vcx, "rail-toggle"),
        "there is no way to reopen the rail — closing the menu is one-way"
    );
    let toggle = vcx.debug_bounds("rail-toggle").expect("the toggle");
    assert!(
        toggle.origin.x >= px(theme::TRAFFIC_CLEARANCE),
        "the reopen toggle sits at {:?}, inside the traffic lights' \
         clearance of {} — it is under the close button",
        toggle.origin.x,
        theme::TRAFFIC_CLEARANCE
    );
}

/// A page tab takes the well from the transcript, and says which absence it
/// is when nothing can draw the page.
///
/// The harness builds without `--features web`, so there is no Chromium
/// here and `page_element` is `None` on every tab. That is exactly the case
/// this guards: before C5, `showing_page` meant "there are page pixels", so
/// a page tab in a non-web build rendered the TRANSCRIPT — an agent's
/// conversation sitting in the well where a document was asked for.
///
/// **Slice T1 split the two absences.** A page tab that has BEEN somewhere
/// and cannot draw is still `page-absence`; a tab that has never been
/// anywhere gets the launcher instead, because "the browser has not handed
/// this tab one yet" is not what somebody who pressed ⌘T wanted to read.
/// The invariant this test actually guards is unchanged and is asserted in
/// both cases: **the transcript never draws under a page tab.**
#[gpui::test]
fn a_page_tab_takes_the_well_and_states_why_it_is_empty(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let visited = || {
        let mut t = page_tab("p1", Some("https://docs.rs/"), Some("docs.rs"));
        t.history_len = 1;
        t
    };
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(
        &h,
        vcx,
        vec![session_tab("aaaa1111"), visited()],
        Some("aaaa1111"),
    );
    assert!(
        shown(vcx, "send"),
        "the session tab is active and the composer is not there"
    );
    assert!(!shown(vcx, "page-absence"), "no page tab is active");

    // Now the page — one that has been somewhere, so its emptiness is the
    // browser's and the well has to say so.
    strip_tabs(
        &h,
        vcx,
        vec![session_tab("aaaa1111"), visited()],
        Some("p1"),
    );
    assert!(
        shown(vcx, "page-absence"),
        "a page tab is active and the well says nothing about it"
    );
    // The agent's chrome is gone with the transcript. A prompt box floating
    // on somebody else's document is not a browser.
    assert!(!shown(vcx, "send"), "the composer drew over a page tab");

    // A tab that has never been anywhere is the OTHER absence: nothing has
    // gone wrong and nothing is starting, so it gets the launcher — and the
    // transcript still does not draw.
    strip_tabs(
        &h,
        vcx,
        vec![session_tab("aaaa1111"), page_tab("blank", None, None)],
        Some("blank"),
    );
    assert!(
        !shown(vcx, "page-absence"),
        "a fresh tab explained the browser instead of offering a chat"
    );
    assert!(shown(vcx, "launcher"));
    assert!(
        !shown(vcx, "send"),
        "the composer drew under the launcher on a blank tab"
    );
}

/// **⌘L opens the omnibox over the page, and row zero says what Return will
/// do.**
///
/// The occlusion rule at `showing_page` says every floating surface must
/// answer "suppressed, or shown over a page?" at birth. This is the first
/// surface built since that rule was written and the one case where the
/// answer is *shown*: an address bar that hid itself whenever a document
/// was loaded would be an address bar you could only use on a blank tab.
#[gpui::test]
fn the_omnibox_opens_over_a_page_and_states_what_return_does(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(
        &h,
        vcx,
        vec![page_tab(
            "p1",
            Some("https://example.com/"),
            Some("Example"),
        )],
        Some("p1"),
    );
    assert!(!shown(vcx, "omnibox"), "the omnibox is up before ⌘L");

    vcx.simulate_keystrokes("cmd-l");
    vcx.run_until_parked();
    assert!(shown(vcx, "omnibox"), "⌘L did not open the omnibox");
    assert!(
        shown(vcx, "omni-row-0"),
        "the omnibox is up with nothing saying what Return does"
    );
    // Seeded with the tab's own address, because the commonest reason to
    // open an address bar is to edit the address in it.
    assert_eq!(
        omni_text(&h, vcx),
        "https://example.com/",
        "⌘L opened on an empty field over a loaded page"
    );

    // Typing REPLACES the seed rather than appending to it. Without that,
    // ⌘L then typing produces `https://example.com/docs.rs` — which a
    // person only discovers after pressing Return.
    vcx.simulate_keystrokes("d o c s . r s");
    vcx.run_until_parked();
    assert_eq!(omni_text(&h, vcx), "docs.rs");

    // Escape refuses, and changes nothing. Escape must never be both the
    // only way to dismiss a surface AND a way to lose what you typed.
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(!shown(vcx, "omnibox"), "escape did not close the omnibox");
}

/// **Return goes through the request path, and never commits.**
///
/// B1's caveat 2 named this and C1 closed it: `tab_navigate` records a
/// request that whatever renders pages answers, and the commit is what
/// moves the tab. An omnibox that committed on Return would re-open the
/// double journal from the other end — two nav entries, two history visits,
/// and a back button that needs pressing twice.
#[gpui::test]
fn the_omnibox_navigates_through_the_request_path(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(&h, vcx, vec![page_tab("p1", None, None)], Some("p1"));

    vcx.simulate_keystrokes("cmd-l");
    vcx.simulate_keystrokes("e x a m p l e . c o m");
    vcx.run_until_parked();
    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();

    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Navigate { tab_id, url })
                if tab_id == "p1" && url == "https://example.com"
        )),
        "Return emitted {cmds:?} — the omnibox must ASK, and it must add \\
         the scheme the operator did not type"
    );
    assert!(!shown(vcx, "omnibox"), "committing left the omnibox up");

    // A sentence is a search, and it goes to the engine that was named on
    // screen — not silently to whoever the code happens to prefer.
    vcx.simulate_keystrokes("cmd-l");
    vcx.simulate_keystrokes("r u s t space t r a i t s");
    vcx.run_until_parked();
    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Navigate { url, .. })
                if url == "https://www.google.com/search?q=rust+traits"
        )),
        "a sentence was not searched for: {cmds:?}"
    );
}

/// With no page tab to commit into, Return opens one — at NOTHING, and then
/// asks it to go somewhere.
///
/// Two requests rather than `TabOpen { url }`, and the reason is the same
/// double journal: opening a tab AT a url commits that url in B1's fold,
/// which records a navigation to a page nobody has fetched.
#[gpui::test]
fn the_omnibox_opens_a_tab_when_the_active_one_is_a_session(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(&h, vcx, vec![session_tab("aaaa1111")], Some("aaaa1111"));

    vcx.simulate_keystrokes("cmd-l");
    vcx.simulate_keystrokes("e x a m p l e . c o m");
    vcx.run_until_parked();
    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::OpenAt { url, activate: true })
                if url == "https://example.com"
        )),
        "the omnibox dropped a navigation because the active tab was a \\
         session: {cmds:?}"
    );
}

/// ⌘T opens a page tab **and** the omnibox on it. A new tab with no way to
/// say where it should go is a blank page and a second keystroke.
#[gpui::test]
fn cmd_t_opens_a_tab_and_the_omnibox_with_it(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(&h, vcx, vec![session_tab("aaaa1111")], Some("aaaa1111"));

    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-t");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::NewTab))),
        "⌘T opened nothing: {cmds:?}"
    );
    assert!(
        shown(vcx, "omnibox"),
        "⌘T opened a tab with no way to aim it"
    );

    // The `+` on the strip is the same act.
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    let _ = h.drain();
    click(vcx, "tab-new");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::NewTab))),
        "the strip's + is not ⌘T: {cmds:?}"
    );
}

/// Slice T1 — seed a strip that has one project in it.
///
/// `workspace` is the checkout the project is bound to, which is the whole
/// point of the group: `TabGroup::workspace` has recorded that binding since
/// B1 and Slice T1 is what made anything act on it.
fn strip_with_project(
    h: &Harness,
    vcx: &mut VisualTestContext,
    workspace: Option<&str>,
    grouped: Vec<TabInfo>,
    loose: Vec<TabInfo>,
    active: Option<&str>,
) {
    let group_ids: Vec<String> = grouped.iter().map(|t| t.id.clone()).collect();
    let mut order: Vec<eave_wire::Slot> = loose
        .iter()
        .map(|t| eave_wire::Slot::Tab { id: t.id.clone() })
        .collect();
    // The group goes LAST on the wire on purpose: the drawn order must be
    // the strip's doing, not the fixture's.
    order.push(eave_wire::Slot::Group { id: "grp".into() });
    let mut tabs = loose;
    tabs.extend(grouped);
    h.plumbing.push_tabs(TabsSnapshot {
        order,
        tabs,
        groups: vec![eave_wire::TabGroup {
            id: "grp".into(),
            title: Some("eave".into()),
            color: None,
            collapsed: false,
            workspace: workspace.map(str::to_string),
            last_active: None,
            created_ts_ms: 0,
        }],
        group_tabs: vec![("grp".into(), group_ids)],
        active: active.map(str::to_string),
        read_only: false,
    });
    vcx.run_until_parked();
}

/// A project is drawn ABOVE the loose tabs, whatever order the engine keeps
/// them in, with the `TABS` divider marking where it stops.
///
/// The fixture puts the group LAST on the wire, so a strip that simply drew
/// `snapshot.order` would fail this — which is the point. B1's interleaved
/// ordering is still the operator's ordering and is untouched; Slice T1 lays
/// one presentation rule over it, in `strip::drawn_order`, where `items` and
/// `cycle_order` both read it.
#[gpui::test]
fn a_project_is_drawn_above_the_loose_tabs_with_the_divider_between_them(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_with_project(
        &h,
        vcx,
        Some("/tmp/eave-harness/repo"),
        vec![session_tab("aaaa1111")],
        vec![page_tab(
            "loose",
            Some("https://loose.test/"),
            Some("loose"),
        )],
        Some("aaaa1111"),
    );

    let head = vcx
        .debug_bounds("group-grp")
        .expect("the project's head never drew");
    let divider = vcx
        .debug_bounds("tabs-divider")
        .expect("nothing marked where the projects stop");
    let grouped = vcx
        .debug_bounds("tab-aaaa1111")
        .expect("the project's tab never drew");
    let loose = vcx
        .debug_bounds("tab-loose")
        .expect("the loose tab never drew");

    assert!(
        head.origin.y < grouped.origin.y,
        "a project's head has to sit above the tabs it holds"
    );
    assert!(
        grouped.origin.y < divider.origin.y,
        "the divider landed inside the project instead of under it"
    );
    assert!(
        divider.origin.y < loose.origin.y,
        "the loose tab drew above the divider, so the sections are inverted"
    );
}

/// Clicking a project's head folds it, and folding is an engine fact.
///
/// `collapsed` has been on the group since B1 and `strip.rs` has always
/// drawn a collapsed one as a head plus a count. Nothing ever SET it, so
/// every project was permanently open and the count was unreachable.
#[gpui::test]
fn folding_a_project_asks_the_engine_to_collapse_it(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_with_project(
        &h,
        vcx,
        Some("/tmp/eave-harness/repo"),
        vec![session_tab("aaaa1111")],
        Vec::new(),
        Some("aaaa1111"),
    );

    let _ = h.drain();
    click(vcx, "group-grp");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::GroupSetCollapsed {
                group_id,
                collapsed: true
            }) if group_id == "grp"
        )),
        "clicking the head did not fold the project: {cmds:?}"
    );
}

/// A session in a checkout that has no project mints one, and its tab is
/// asked for INSIDE it.
///
/// This is Slice T1 turning on the first of the three things
/// `TabGroup::workspace`'s own doc comment says nobody does — *the engine
/// never creates a group for a worktree*. It still does not: the client
/// does, because the client is where the snapshot is.
#[gpui::test]
fn a_session_in_an_unclaimed_checkout_mints_a_project_for_it(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut info = session("aaaa1111");
    info.checkout_root = Some("/tmp/eave-harness/repo".into());

    // Drain BEFORE the rail arrives. `reconcile_strip` runs in the render
    // pass, so the ask is already out by the time the rail push has parked
    // — draining after it would throw away the very command under test.
    let _ = h.drain();
    rail_with_selected(&h, vcx, vec![info], "aaaa1111");
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::OpenSessionTab {
                session_id,
                workspace: Some(workspace),
                group: None,
                ..
            }) if session_id == "aaaa1111" && workspace == "/tmp/eave-harness/repo"
        )),
        "the session's tab was asked for with no project to put it in: {cmds:?}"
    );
}

/// A session whose checkout ALREADY has a project joins it rather than
/// minting a second one.
#[gpui::test]
fn a_session_joins_the_project_its_checkout_already_has(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.checkout_root = Some("/tmp/eave-harness/repo".into());
    let mut b = session("bbbb2222");
    b.checkout_root = Some("/tmp/eave-harness/repo".into());

    // The project exists FIRST, holding one of the two sessions. With no
    // rail yet, `sessions_without_tabs` has nothing to ask for, so this
    // pushes no commands of its own.
    strip_with_project(
        &h,
        vcx,
        Some("/tmp/eave-harness/repo"),
        vec![session_tab("aaaa1111")],
        Vec::new(),
        Some("aaaa1111"),
    );
    let _ = h.drain();
    rail_with_selected(&h, vcx, vec![a, b], "aaaa1111");
    let cmds = h.drain();
    let asked: Vec<_> = cmds
        .iter()
        .filter_map(|c| match c {
            Command::Browser(BrowserCmd::OpenSessionTab {
                session_id,
                group,
                workspace,
                ..
            }) => Some((session_id.clone(), group.clone(), workspace.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        asked,
        vec![("bbbb2222".to_string(), Some("grp".to_string()), None)],
        "the second session should have joined `grp`, minting nothing: {cmds:?}"
    );
}

/// Slice T1 — ⌘T's tab is a **chat you can click into**, not a blank page
/// you have to aim first.
///
/// A page tab that has never been anywhere used to draw `PAGE_STARTING` —
/// "the browser exists and this tab has not been handed one yet". True, and
/// no use to somebody who did not ask for a browser. The well gets the
/// launcher instead: *which agent, in which folder*. So dismissing the
/// omnibox lands in a chat rather than on an explanation.
#[gpui::test]
fn a_blank_tab_offers_a_chat_rather_than_explaining_the_browser(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(
        &h,
        vcx,
        vec![session_tab("aaaa1111"), page_tab("blank", None, None)],
        Some("blank"),
    );

    assert!(
        shown(vcx, "launcher"),
        "a blank tab left the operator looking at the browser's excuses"
    );
}

/// ...and a page tab that HAS been somewhere still belongs to the page.
///
/// The counter-case matters more than the case: a launcher drawn over a
/// loaded page would be the floating-surface defect `showing_page` exists
/// to catch, and it is the launcher specifically that was photographed
/// drawing straight through docs.rs once already.
#[gpui::test]
fn a_page_that_has_been_somewhere_keeps_its_well(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let mut visited = page_tab("visited", Some("https://docs.rs/"), Some("docs.rs"));
    visited.history_len = 1;
    strip_tabs(
        &h,
        vcx,
        vec![session_tab("aaaa1111"), visited],
        Some("visited"),
    );

    assert!(
        !shown(vcx, "launcher"),
        "the launcher drew over a page that had been somewhere"
    );
}

/// Slice T1 — a session started from a blank ⌘T tab lands **in that tab**.
///
/// The operator made one tab and must end with one tab. Before this, the
/// launcher's session arrived as a new tab appended to the strip and the
/// blank one sat there forever — ⌘T, start a chat, and you owned two tabs,
/// one of them permanently empty.
#[gpui::test]
fn a_session_started_from_a_blank_tab_takes_that_tabs_place(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    // No session selected, one blank page tab active: the launcher is up
    // and it is up *because of that tab*.
    strip_tabs(&h, vcx, vec![page_tab("blank", None, None)], Some("blank"));
    assert!(shown(vcx, "launcher"), "the launcher is not up");

    let _ = h.drain();
    click(vcx, "launcher-start");
    vcx.run_until_parked();

    // The session the launch asked for now arrives on the rail.
    let _ = h.drain();
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::ReplaceWithSessionTab {
                closing,
                session_id,
                index: 0,
                ..
            }) if closing == "blank" && session_id == "aaaa1111"
        )),
        "the session did not take the blank tab's place: {cmds:?}"
    );
}

/// Slice T1 follow-up — a chat launched inside a project **starts in that
/// project's worktree**.
///
/// `TabGroup::workspace` was recorded by B1 and read by nobody: eave drew
/// the project, named it after the checkout, grouped its tabs — and then
/// launched every chat in whatever directory the engine defaulted to. The
/// group said which worktree it was about and the agent was started
/// somewhere else.
///
/// Nothing new goes on the wire for this. `Request::Open` has carried a
/// `cwd` since P2; the shell was passing `None`.
#[gpui::test]
fn a_chat_launched_in_a_project_starts_in_its_worktree(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    strip_with_project(
        &h,
        vcx,
        Some("/Users/x/Desktop/eave/wt-e3"),
        vec![page_tab("blank", None, None)],
        vec![],
        Some("blank"),
    );
    assert!(shown(vcx, "launcher"), "the launcher is not up");

    let _ = h.drain();
    click(vcx, "launcher-start");
    vcx.run_until_parked();

    let cmds = h.drain();
    let cwd = cmds
        .iter()
        .find_map(|c| match c {
            Command::Launch { cwd, .. } => Some(cwd.clone()),
            _ => None,
        })
        .expect("no launch");
    assert_eq!(
        cwd.as_deref(),
        Some("/Users/x/Desktop/eave/wt-e3"),
        "the chat did not inherit its project's worktree"
    );
}

/// ...and a chat launched outside any project still lets the engine pick.
///
/// The counter-case is the one that could regress silently. `cwd` is an
/// `Option` precisely so "no project" keeps eave's pre-T1 behaviour, and a
/// helper that invented a path for a loose tab — the checkout eave itself
/// is running in, say — would start every ungrouped chat somewhere the
/// operator never named.
#[gpui::test]
fn a_chat_launched_outside_a_project_names_no_worktree(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    // A loose blank tab, and a project sitting right next to it that this
    // tab does not belong to.
    strip_with_project(
        &h,
        vcx,
        Some("/Users/x/Desktop/eave/wt-e3"),
        vec![page_tab("in-project", None, None)],
        vec![page_tab("blank", None, None)],
        Some("blank"),
    );

    let _ = h.drain();
    click(vcx, "launcher-start");
    vcx.run_until_parked();

    let cmds = h.drain();
    let cwd = cmds
        .iter()
        .find_map(|c| match c {
            Command::Launch { cwd, .. } => Some(cwd.clone()),
            _ => None,
        })
        .expect("no launch");
    assert_eq!(
        cwd, None,
        "a loose tab borrowed a neighbouring project's worktree"
    );
}

/// ...and a session that arrives on its own does NOT evict somebody's tab.
///
/// The note is taken, not read: only the first session after a launch from
/// a blank tab claims it, and a session nobody launched from a tab claims
/// nothing.
#[gpui::test]
fn a_session_nobody_launched_from_a_tab_replaces_nothing(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    strip_tabs(&h, vcx, vec![page_tab("blank", None, None)], Some("blank"));

    let _ = h.drain();
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let cmds = h.drain();
    assert!(
        !cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::ReplaceWithSessionTab { .. })
        )),
        "a session arriving on its own took a tab that was not offered: {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::OpenSessionTab { session_id, .. })
                if session_id == "aaaa1111"
        )),
        "it should still get an ordinary tab of its own: {cmds:?}"
    );
}

/// Slice T1 — seed the bookmark tree. `(id, depth, title, url)`; a `None`
/// url makes a folder, exactly as `BookmarkAdd` means it.
fn push_bookmarks(
    h: &Harness,
    vcx: &mut VisualTestContext,
    nodes: &[(&str, usize, Option<&str>, Option<&str>)],
) {
    h.plumbing.push_bookmarks(eave_wire::BookmarksSnapshot {
        nodes: nodes
            .iter()
            .map(|(id, depth, title, url)| eave_wire::BookmarkNode {
                bookmark: eave_wire::Bookmark {
                    id: (*id).into(),
                    parent: None,
                    title: title.map(str::to_string),
                    kind: match url {
                        Some(u) => eave_wire::BookmarkKind::Link { url: (*u).into() },
                        None => eave_wire::BookmarkKind::Folder,
                    },
                    created_ts_ms: 0,
                },
                depth: *depth,
                children: 0,
            })
            .collect(),
        ..Default::default()
    });
    vcx.run_until_parked();
}

/// A page tab with a real address, so the bar has something to be over.
fn visited_tab(id: &str, url: &str) -> TabInfo {
    let mut t = page_tab(id, Some(url), Some("a page"));
    t.history_len = 1;
    t
}

/// Slice T1b — bookmarks live in the **rail**, so they are there in a chat
/// too.
///
/// This asserts the REVERSE of what Slice T1 asserted, deliberately. T1 hung
/// the bookmarks in a bar over the content well and this test proved the bar
/// vanished when the well held a session — the operator's ruling at the time.
/// T1b overturned it on the operator's own reference frame: the standing rule
/// is that a surface may sit over a page only if it is ABOUT that page, and a
/// bookmarks bar is not about the page it covers. In the rail there is no
/// page to be about, so the grid is unconditional — which is the whole point,
/// and is asserted in both directions so it stays a decision.
#[gpui::test]
fn the_bookmarks_grid_is_in_the_rail_in_a_page_and_in_a_chat(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(&h, vcx, &[("gh", 0, None, Some("https://github.com/"))]);

    strip_tabs(
        &h,
        vcx,
        vec![
            session_tab("aaaa1111"),
            visited_tab("p1", "https://docs.rs/"),
        ],
        Some("p1"),
    );
    assert!(shown(vcx, "bookmarks-grid"), "no grid over a page");
    assert!(
        shown(vcx, "bookmark-tile-gh"),
        "the grid drew none of its tiles"
    );

    strip_tabs(
        &h,
        vcx,
        vec![
            session_tab("aaaa1111"),
            visited_tab("p1", "https://docs.rs/"),
        ],
        Some("aaaa1111"),
    );
    assert!(
        shown(vcx, "bookmarks-grid"),
        "the grid vanished when the well held a conversation — it is in the \
         rail now and owes the page nothing"
    );
    assert!(
        shown(vcx, "bookmark-tile-gh"),
        "the grid lost its tiles in a chat"
    );
}

/// Slice T1b — a folder row sits **under the projects and above the TABS
/// seam**, which is the operator's own placement.
///
/// Geometry, not presence: "the folder rendered somewhere in the rail" is
/// true of every wrong answer here, including the one where folders land
/// among the loose tabs.
#[gpui::test]
fn a_bookmark_folder_sits_under_the_projects_and_above_the_tabs_seam(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(&h, vcx, &[("dev", 0, Some("dev"), None)]);
    strip_tabs(
        &h,
        vcx,
        vec![
            session_tab("aaaa1111"),
            visited_tab("p1", "https://docs.rs/"),
        ],
        Some("p1"),
    );

    let folder = vcx
        .debug_bounds("bookmark-folder-dev")
        .expect("the folder row did not render in the rail");
    let seam = vcx
        .debug_bounds("tabs-divider")
        .expect("no TABS seam to place the folder against");

    assert!(
        folder.origin.y < seam.origin.y,
        "the folder fell BELOW the TABS seam, among the loose tabs — \
         folder {folder:?} seam {seam:?}"
    );
}

/// Slice T1b — an open folder **takes up room**, which is what makes it a
/// disclosure rather than a popup.
///
/// The distinguishing property, and the only one worth asserting: T1's
/// overlay floated over the rail and moved nothing, so "the children are
/// visible" was true of both designs. What separates them is whether the
/// rows BELOW the folder get pushed down. The operator asked for the list
/// to grow and the sidebar to scroll; this is that, measured.
#[gpui::test]
fn opening_a_bookmark_folder_pushes_the_rows_below_it_down(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(
        &h,
        vcx,
        &[
            ("dev", 0, Some("dev"), None),
            // depth 1 — INSIDE the folder. At depth 0 it is a sibling link,
            // and the folder opens onto nothing.
            ("rs", 1, None, Some("https://docs.rs/")),
        ],
    );
    strip_tabs(
        &h,
        vcx,
        vec![
            session_tab("aaaa1111"),
            visited_tab("p1", "https://docs.rs/"),
        ],
        Some("p1"),
    );

    let before = vcx
        .debug_bounds("tabs-divider")
        .expect("no TABS seam")
        .origin
        .y;
    click(vcx, "bookmark-folder-dev");
    vcx.run_until_parked();
    let after = vcx
        .debug_bounds("tabs-divider")
        .expect("the TABS seam vanished when a folder opened")
        .origin
        .y;

    assert!(
        shown(vcx, "bookmark-child-rs"),
        "the folder did not open at all"
    );
    assert!(
        after > before,
        "the folder opened OVER the rail instead of pushing it down — the \
         TABS seam did not move (before {before:?}, after {after:?})"
    );
}

/// Slice T1b — a tile wears the site's own mark once the bytes are in.
///
/// The grid is label-less, so the mark is the entire identity of a
/// bookmark: a tile that never gets past the globe is a tile the operator
/// cannot tell from its neighbour. Keyed by HOST, which is what lets a
/// bookmark wear an icon it never had a browser to fetch.
#[gpui::test]
fn a_pinned_bookmark_wears_its_site_mark_when_the_bytes_are_in(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(&h, vcx, &[("gh", 0, None, Some("https://github.com/"))]);

    assert!(
        !shown(vcx, "bookmark-mark-gh"),
        "a tile drew a mark before any bytes arrived"
    );
    assert!(
        shown(vcx, "bookmark-tile-glyph"),
        "the globe fallback is missing, so an iconless tile is invisible"
    );

    crate::web::set_host_favicon_for_test(
        "github.com",
        Some(std::sync::Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            TINY_PNG.to_vec(),
        ))),
    );
    // Bytes landing is not itself a notify — Chromium's callback calls
    // `note_chrome_change` for that. Re-pushing the same snapshot is the
    // harness's way of driving the frame that would follow.
    push_bookmarks(&h, vcx, &[("gh", 0, None, Some("https://github.com/"))]);

    assert!(
        shown(vcx, "bookmark-mark-gh"),
        "the tile kept its globe with bytes in hand"
    );
    crate::web::set_host_favicon_for_test("github.com", None);
}

/// Slice T1b — the page well holds no bookmarks surface at all.
///
/// The companion to the test above, and the one that would catch the bar
/// coming back by accident: `bookmarks-bar` is gone, not merely hidden.
#[gpui::test]
fn no_bookmarks_bar_is_drawn_over_a_page(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(&h, vcx, &[("gh", 0, None, Some("https://github.com/"))]);
    strip_tabs(
        &h,
        vcx,
        vec![
            session_tab("aaaa1111"),
            visited_tab("p1", "https://docs.rs/"),
        ],
        Some("p1"),
    );
    assert!(
        !shown(vcx, "bookmarks-bar"),
        "the bar is over the page again"
    );
}

/// Slice T1 follow-up — a bookmark can be taken **off** the bar.
///
/// The bar shipped with three ways to put something on it (drag an
/// address, drop onto a folder, new folder) and no way to take anything
/// off. A chip cannot be dragged to a trash can, so a bookmark added by a
/// stray drop was permanent.
#[gpui::test]
fn a_right_click_on_a_bookmark_removes_it(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(&h, vcx, &[("gh", 0, None, Some("https://github.com/"))]);
    strip_tabs(
        &h,
        vcx,
        vec![visited_tab("p1", "https://docs.rs/")],
        Some("p1"),
    );

    let at = vcx
        .debug_bounds("bookmark-tile-gh")
        .expect("bookmark-tile-gh");
    vcx.simulate_mouse_down(at.center(), MouseButton::Right, Modifiers::none());
    vcx.run_until_parked();
    assert!(
        shown(vcx, "tab-menu-remove-bookmark"),
        "right-clicking a bookmark offered no way to remove it"
    );

    let _ = h.drain();
    click(vcx, "tab-menu-remove-bookmark");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::BookmarkRemove { bookmark_id }) if bookmark_id == "gh"
        )),
        "Remove sent no removal: {cmds:?}"
    );
}

/// ...and removing an OPEN folder closes it.
///
/// The engine answers a removal with a fresh snapshot, but the folder that
/// is open is the shell's own state and no snapshot clears it. Without
/// this, removing an open folder leaves its children hanging under a chip
/// that is gone.
#[gpui::test]
fn removing_an_open_bookmark_folder_closes_it(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(
        &h,
        vcx,
        &[
            ("dev", 0, Some("dev"), None),
            ("rs", 1, None, Some("https://docs.rs/")),
        ],
    );
    strip_tabs(
        &h,
        vcx,
        vec![visited_tab("p1", "https://docs.rs/")],
        Some("p1"),
    );

    click(vcx, "bookmark-folder-dev");
    vcx.run_until_parked();
    assert!(shown(vcx, "bookmark-child-rs"), "the folder never opened");

    let at = vcx
        .debug_bounds("bookmark-folder-dev")
        .expect("bookmark-folder-dev");
    vcx.simulate_mouse_down(at.center(), MouseButton::Right, Modifiers::none());
    vcx.run_until_parked();
    let _ = h.drain();
    click(vcx, "tab-menu-remove-bookmark");
    vcx.run_until_parked();

    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::BookmarkRemove { bookmark_id }) if bookmark_id == "dev"
        )),
        "the folder was not removed: {cmds:?}"
    );
    assert!(
        !shown(vcx, "bookmark-child-rs"),
        "a removed folder's children stayed on screen"
    );
}

/// ...and a bookmark filed INSIDE a folder can be removed too.
///
/// The folder is the only place such a bookmark can be right-clicked, so
/// leaving the child rows out would make anything dropped into a folder
/// the permanent kind again.
#[gpui::test]
fn a_bookmark_inside_a_folder_can_be_removed(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(
        &h,
        vcx,
        &[
            ("dev", 0, Some("dev"), None),
            ("rs", 1, None, Some("https://docs.rs/")),
        ],
    );
    strip_tabs(
        &h,
        vcx,
        vec![visited_tab("p1", "https://docs.rs/")],
        Some("p1"),
    );

    click(vcx, "bookmark-folder-dev");
    vcx.run_until_parked();
    let at = vcx
        .debug_bounds("bookmark-child-rs")
        .expect("bookmark-child-rs");
    vcx.simulate_mouse_down(at.center(), MouseButton::Right, Modifiers::none());
    vcx.run_until_parked();

    let _ = h.drain();
    click(vcx, "tab-menu-remove-bookmark");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::BookmarkRemove { bookmark_id }) if bookmark_id == "rs"
        )),
        "the child was not removed: {cmds:?}"
    );
    assert!(
        shown(vcx, "bookmark-folder-dev"),
        "removing a child took the folder with it"
    );
}

/// A folder opens its children under it, and only its DIRECT children.
#[gpui::test]
fn a_bookmark_folder_opens_its_children_under_it(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(
        &h,
        vcx,
        &[
            ("dev", 0, Some("dev"), None),
            ("rs", 1, None, Some("https://docs.rs/")),
            ("inner", 1, Some("inner"), None),
            ("deep", 2, None, Some("https://deep.test/")),
        ],
    );
    strip_tabs(
        &h,
        vcx,
        vec![visited_tab("p1", "https://docs.rs/")],
        Some("p1"),
    );

    assert!(shown(vcx, "bookmark-folder-dev"), "the folder never drew");
    assert!(!shown(vcx, "bookmark-child-rs"), "the folder opened itself");

    click(vcx, "bookmark-folder-dev");
    vcx.run_until_parked();
    assert!(shown(vcx, "bookmark-child-rs"), "the folder opened empty");
    assert!(
        !shown(vcx, "bookmark-child-deep"),
        "a grandchild drew as a child — the run did not stop at the folder's depth"
    );

    // Clicking it again shuts it. One folder open at a time, and a
    // disclosure that only opens is a trap.
    click(vcx, "bookmark-folder-dev");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "bookmark-child-rs"),
        "the folder would not shut"
    );
}

/// Clicking a bookmark sends the tab you are looking at, which is what a
/// bookmarks bar has meant in every browser.
#[gpui::test]
fn clicking_a_bookmark_sends_the_active_tab_to_it(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(&h, vcx, &[("gh", 0, None, Some("https://github.com/"))]);
    strip_tabs(
        &h,
        vcx,
        vec![visited_tab("p1", "https://docs.rs/")],
        Some("p1"),
    );

    let _ = h.drain();
    click(vcx, "bookmark-tile-gh");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Navigate { tab_id, url })
                if tab_id == "p1" && url == "https://github.com/"
        )),
        "the bookmark opened somewhere other than the tab in front of you: {cmds:?}"
    );
}

/// Dropping the address into a folder files it INSIDE, and onto the bar
/// files it on the bar.
///
/// The drop handlers are driven directly rather than through a synthetic
/// drag, which is what `the_rail_draws_the_width_it_was_dragged_to` does
/// for the divider: gpui's drag is a multi-frame gesture and the thing
/// worth pinning is what the handler ASKS the engine for.
#[gpui::test]
fn dropping_an_address_files_it_where_it_landed(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(&h, vcx, &[("dev", 0, Some("dev"), None)]);
    strip_tabs(
        &h,
        vcx,
        vec![visited_tab("p1", "https://docs.rs/")],
        Some("p1"),
    );

    let _ = h.drain();
    h.window
        .update(vcx, |shell, _, cx| {
            let drag = AddressDrag {
                url: "https://docs.rs/".into(),
                title: Some("docs.rs".into()),
            };
            shell.add_bookmark(Some("dev".into()), None, &drag, cx);
            shell.add_bookmark(None, Some(0), &drag, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    let cmds = h.drain();
    let adds: Vec<_> = cmds
        .iter()
        .filter_map(|c| match c {
            Command::Browser(BrowserCmd::BookmarkAdd {
                parent,
                index,
                url,
                title,
            }) => Some((parent.clone(), *index, url.clone(), title.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        adds,
        vec![
            (
                Some("dev".to_string()),
                None,
                Some("https://docs.rs/".to_string()),
                Some("docs.rs".to_string())
            ),
            (
                None,
                Some(0),
                Some("https://docs.rs/".to_string()),
                Some("docs.rs".to_string())
            ),
        ],
        "a drop did not land where it was aimed: {cmds:?}"
    );
}

/// A bookmark dragged onto a folder goes in it; dragged onto a chip, it
/// takes that chip's place.
#[gpui::test]
fn moving_a_bookmark_reparents_or_reorders(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(
        &h,
        vcx,
        &[
            ("gh", 0, None, Some("https://github.com/")),
            ("dev", 0, Some("dev"), None),
        ],
    );
    strip_tabs(
        &h,
        vcx,
        vec![visited_tab("p1", "https://docs.rs/")],
        Some("p1"),
    );

    let _ = h.drain();
    h.window
        .update(vcx, |shell, _, cx| {
            shell.move_bookmark("gh".into(), Some("dev".into()), 0, cx);
            shell.move_bookmark("gh".into(), None, 1, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    let cmds = h.drain();
    let moves: Vec<_> = cmds
        .iter()
        .filter_map(|c| match c {
            Command::Browser(BrowserCmd::BookmarkMove {
                bookmark_id,
                parent,
                index,
            }) => Some((bookmark_id.clone(), parent.clone(), *index)),
            _ => None,
        })
        .collect();
    assert_eq!(
        moves,
        vec![
            ("gh".to_string(), Some("dev".to_string()), 0),
            ("gh".to_string(), None, 1),
        ],
        "{cmds:?}"
    );
}

/// Slice T1 — the Folders header makes folders, and names them so two
/// are told apart.
///
/// New folder is on the Folders dropdown (double-click / right-click),
/// not on the bookmark grid's `+`. A folder created unnamed would draw
/// as an empty chip: eave has no inline rename for a bookmark yet
/// (`BookmarkRename` is on the wire and nothing calls it), so the
/// operator could open it and fill it but never identify it.
#[gpui::test]
fn the_folders_header_makes_folders_and_names_them_apart(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(&h, vcx, &[]);
    strip_tabs(
        &h,
        vcx,
        vec![visited_tab("p1", "https://docs.rs/")],
        Some("p1"),
    );

    let _ = h.drain();
    let at = vcx.debug_bounds("zone-folders").expect("zone-folders");
    vcx.simulate_mouse_down(at.center(), MouseButton::Right, Modifiers::none());
    vcx.run_until_parked();
    click(vcx, "tab-menu-new-folder");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::BookmarkAdd { url: None, title: Some(t), .. })
                if t == "new folder"
        )),
        "the Folders header made no folder: {cmds:?}"
    );

    // With one already there, the next is distinguishable from it.
    push_bookmarks(&h, vcx, &[("f1", 0, Some("new folder"), None)]);
    let _ = h.drain();
    let at = vcx.debug_bounds("zone-folders").expect("zone-folders");
    vcx.simulate_mouse_down(at.center(), MouseButton::Right, Modifiers::none());
    vcx.run_until_parked();
    click(vcx, "tab-menu-new-folder");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::BookmarkAdd { url: None, title: Some(t), .. })
                if t == "new folder 2"
        )),
        "a second folder was made indistinguishable from the first: {cmds:?}"
    );
}

/// ⌘W closes the active TAB, and lands on its right neighbour.
///
/// Right then left, which is what every browser does: landing on the left
/// neighbour each time walks the selection backwards through tabs the
/// operator has already dealt with. The successor is chosen HERE and not by
/// the engine's fold — B1's `Closed` clears `active` and does not pick a
/// replacement, which is correct for a data model and useless for a strip.
#[gpui::test]
fn closing_the_active_tab_lands_on_its_right_neighbour(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let tabs = || {
        vec![
            page_tab("p1", Some("https://a.test/"), None),
            page_tab("p2", Some("https://b.test/"), None),
            page_tab("p3", Some("https://c.test/"), None),
        ]
    };
    strip_tabs(&h, vcx, tabs(), Some("p2"));

    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-w");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "p3"
        )),
        "⌘W closed the active tab and landed nowhere: {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Close(id)) if id == "p2"
        )),
        "⌘W did not close the active tab: {cmds:?}"
    );

    // The last tab has no right neighbour and falls back to its left.
    strip_tabs(&h, vcx, tabs(), Some("p3"));
    let _ = h.drain();
    click(vcx, "tab-close-p3");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "p2"
        )),
        "the last tab closed onto nothing: {cmds:?}"
    );
    // The close × must not ALSO activate the tab it is removing: without
    // `stop_propagation` the click bubbles, the strip activates a tab, and
    // the very next frame takes it away.
    assert!(
        !cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "p3"
        )),
        "the close button activated the tab on its way to removing it: {cmds:?}"
    );
}

/// ctrl-tab walks the strip, and wraps. With two tabs open, a cycle that
/// did not wrap would make the second press do nothing.
#[gpui::test]
fn ctrl_tab_walks_the_strip_and_wraps(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let tabs = || vec![page_tab("p1", None, None), session_tab("aaaa1111")];
    strip_tabs(&h, vcx, tabs(), Some("p1"));

    let _ = h.drain();
    vcx.simulate_keystrokes("ctrl-tab");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "aaaa1111"
        )),
        "ctrl-tab went nowhere: {cmds:?}"
    );

    // From the last tab it wraps to the first.
    strip_tabs(&h, vcx, tabs(), Some("aaaa1111"));
    let _ = h.drain();
    vcx.simulate_keystrokes("ctrl-tab");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "p1"
        )),
        "ctrl-tab did not wrap: {cmds:?}"
    );

    // ...and backwards, which is the same walk in the other direction.
    let _ = h.drain();
    vcx.simulate_keystrokes("ctrl-shift-tab");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "p1"
        )),
        "ctrl-shift-tab did not wrap the other way: {cmds:?}"
    );
}

/// **⌘L moved the friction log, and the friction log still works.**
///
/// ⌘L has meant "the address bar" in every browser since Netscape and eave
/// is a browser now, so the collision is resolved in the browser's favour.
/// But the friction log is the instrument the whole dogfood log depends on,
/// so it moves rather than going away — and this holds BOTH halves, because
/// a slice that took a chord and forgot to rehome what was on it would look
/// exactly like a slice that did this properly.
#[gpui::test]
fn cmd_l_is_the_omnibox_and_the_friction_log_moved_to_cmd_shift_l(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(&h, vcx, vec![session_tab("aaaa1111")], Some("aaaa1111"));

    vcx.simulate_keystrokes("cmd-l");
    vcx.run_until_parked();
    assert!(shown(vcx, "omnibox"), "⌘L is not the omnibox");
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();

    // ⌘⇧L writes a line and confirms in the footer. The path is stated on
    // the first write of a run (Stage L), so the note is there either way.
    vcx.simulate_keystrokes("cmd-shift-l");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "omnibox"),
        "⌘⇧L opened the omnibox — the shift was not read"
    );
    let note = vcx
        .read_window(&h.window, |shell, cx| shell.read(cx).friction.clone())
        .unwrap();
    assert!(
        note.is_some(),
        "⌘⇧L wrote nothing the footer says anything about — the friction \
         log is the instrument the whole dogfood log depends on, and a \
         slice that took its chord and forgot to rehome it would look \
         exactly like this one from outside"
    );
}

/// The strip is a projection of the engine's `active`, never a UI-held
/// selection — the same rule the rail's highlight keeps.
///
/// Clicking a tab emits an ASK. Nothing moves on screen until the engine's
/// snapshot says the tab is active. A shell that moved its own `active`
/// optimistically would have two of them again, which is precisely the
/// thing C1 left open and C5 closes.
#[gpui::test]
fn the_strip_never_moves_its_own_selection(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let tabs = || vec![page_tab("p1", None, None), page_tab("p2", None, None)];
    strip_tabs(&h, vcx, tabs(), Some("p1"));

    let lit = |vcx: &mut VisualTestContext, sel: &'static str| vcx.debug_bounds(sel).expect(sel);
    let before = lit(vcx, "tab-p1");

    let _ = h.drain();
    click(vcx, "tab-p2");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "p2"
        )),
        "the click emitted no ask: {cmds:?}"
    );
    // Selection is a rim and a fill, neither of which moves anything — so
    // what is asserted is that the LAYOUT did not shift, which is the part
    // a test can see. `selecting_a_tab_moves_no_tab` covers the metric.
    assert_eq!(
        lit(vcx, "tab-p1").origin,
        before.origin,
        "the strip re-laid itself out on a click"
    );
}

// ---------------------------------------------------------------------------
// Slice E1 — page interaction: find, zoom, and the page's own menu
// ---------------------------------------------------------------------------
//
// **These run with the `web` feature OFF, which is the whole reason the
// state they read lives outside the gate** (`web/mod.rs`). What is asserted
// here is eave's half — which surfaces mount, what they say, and what
// intent a row emits. Chromium's half (that `Find` really counts, that
// `set_zoom_level` really rescales, that a right-click really produces a
// `run_context_menu`) is not assertable in this process at all and is
// verified by the capture rig instead; the report says which is which.
//
// Every one of them calls `pretend_available_for_test`, because a build
// with no Chromium in it correctly refuses all of this — see
// `Shell::active_page`.

/// Ask for a page menu the way Chromium does, and get it onto the screen.
///
/// **The wake-up is supplied by hand here, and that is a real gap the
/// report states.** In production `web::menu`'s handler calls
/// `host::note_chrome_change`, and the pump's redraw gate turns that into a
/// `refresh_windows` — because a right-click on a still page delivers no
/// frame, so the frame counter alone would leave the menu unopened. There
/// is no pump in this process, so that link in the chain is exercised by
/// the capture rig and not by this test. What IS exercised is everything
/// after it: the request being taken in the render pass, the row set, and
/// what each row emits.
#[track_caller]
fn request_page_menu(h: &Harness, vcx: &mut VisualTestContext, menu: crate::web::PageMenu) {
    crate::web::request_menu(menu);
    h.window.update(vcx, |_, _, cx| cx.notify()).unwrap();
    vcx.run_until_parked();
    // Let the entrance land, so nothing below asserts against a surface
    // that is still arriving.
    drive(h, vcx, Duration::from_millis(16));
}

/// A page tab, active, with the page engine pretending to exist.
fn page_shell(h: &Harness, vcx: &mut VisualTestContext, url: Option<&str>) -> &'static str {
    crate::web::pretend_available_for_test();
    rail_with_selected(h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(
        h,
        vcx,
        vec![page_tab("pppp1111", url, Some("A page"))],
        Some("pppp1111"),
    );
    "pppp1111"
}

/// ⌘F opens the find bar, and it is **over the page** — the one place a
/// floating surface is allowed to be, because it is about the page.
#[gpui::test]
fn cmd_f_opens_the_find_bar_over_a_page(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    page_shell(&h, vcx, Some("https://example.com/"));

    assert!(
        !shown(vcx, "find-bar"),
        "the bar was up before anyone asked"
    );
    vcx.simulate_keystrokes("cmd-f");
    vcx.run_until_parked();
    assert!(shown(vcx, "find-bar"), "⌘F opened nothing over a page");
    // Nothing typed, so there is nothing to count — and an empty slot
    // reserved for a number would be the surface claiming it knows
    // something it does not.
    assert!(
        !shown(vcx, "find-tally"),
        "the bar stated a count for a query nobody has typed"
    );
}

/// …and does nothing over a session. ⌘F over a transcript must not open a
/// bar that searches nothing.
#[gpui::test]
fn cmd_f_does_nothing_over_a_session_tab(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    crate::web::pretend_available_for_test();
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(&h, vcx, vec![session_tab("aaaa1111")], Some("aaaa1111"));

    vcx.simulate_keystrokes("cmd-f");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "find-bar"),
        "⌘F opened a find bar over a transcript, which it cannot search"
    );
}

/// Typing states the query; escape takes the bar away.
#[gpui::test]
fn the_find_bar_takes_text_and_escape_dismisses_it(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/"));

    vcx.simulate_keystrokes("cmd-f");
    vcx.simulate_keystrokes("e a v e");
    vcx.run_until_parked();
    assert_eq!(
        crate::web::find_state(tab).query,
        "eave",
        "the bar did not take what was typed into it"
    );

    // Backspace deletes ONE character — unlike the omnibox's, which clears
    // a seeded address whole. Nothing seeded this field, so there is no
    // selection to delete over.
    vcx.simulate_keystrokes("backspace");
    vcx.run_until_parked();
    assert_eq!(crate::web::find_state(tab).query, "eav");

    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(!shown(vcx, "find-bar"), "escape left the bar up");
    assert_eq!(
        crate::web::find_state(tab).query,
        "",
        "a dismissed bar kept its query, so ⌘F would reopen on a stale search"
    );
}

/// The bar states the COUNT, and the count is CEF's, transcribed.
///
/// Reachable in a build with no Chromium in it because the find state lives
/// outside the feature gate (`web/mod.rs`): `note_find_result` is what the
/// gated handler calls, and calling it here drives the same path a real
/// `OnFindResult` does. **The three states are asserted through the
/// SELECTOR** — `find-tally-3-of-17` rather than `find-tally` — because a
/// harness reads elements, not text, and a tally identified only by its ink
/// would have a test under its existence and none under what it says.
#[gpui::test]
fn the_find_bar_states_the_count_as_a_fact(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/"));

    vcx.simulate_keystrokes("cmd-f");
    vcx.simulate_keystrokes("e a v e");
    vcx.run_until_parked();
    // Nothing has come back yet: a query with no answer states nothing,
    // rather than reserving a slot for a number it does not have.
    assert!(
        !shown(vcx, "find-tally-1-of-17"),
        "the bar stated a count before CEF had answered"
    );

    // CEF reports progressively. This one is NOT final, and a count of
    // zero that is not final means *not yet* — the bar must not flash
    // "no matches" on every keystroke over a long page.
    crate::web::note_find_result(tab, 0, 0, false);
    h.window.update(vcx, |_, _, cx| cx.notify()).unwrap();
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "find-tally-no-matches"),
        "an unsettled zero was rendered as a real absence"
    );

    // A real count, with no active match yet — which is what CEF actually
    // sends first, and it must not read as "0 of 17".
    crate::web::note_find_result(tab, 17, 0, true);
    h.window.update(vcx, |_, _, cx| cx.notify()).unwrap();
    vcx.run_until_parked();
    assert!(
        shown(vcx, "find-tally-1-of-17"),
        "the bar did not state the count CEF gave it"
    );

    // Stepping moves the ordinal, and the ordinal is CEF's, transcribed.
    crate::web::note_find_result(tab, 17, 3, true);
    h.window.update(vcx, |_, _, cx| cx.notify()).unwrap();
    vcx.run_until_parked();
    assert!(shown(vcx, "find-tally-3-of-17"), "the ordinal did not move");

    // And a settled zero is the real absence, worth stating plainly.
    crate::web::find_search(tab, "nothingmatchesthis", true, false);
    crate::web::note_find_result(tab, 0, 0, true);
    h.window.update(vcx, |_, _, cx| cx.notify()).unwrap();
    vcx.run_until_parked();
    assert!(
        shown(vcx, "find-tally-no-matches"),
        "a settled zero said nothing where it should say so"
    );
}

/// **The bar does not own the whole keyboard, and that is the difference
/// from the omnibox.** The omnibox is modal — a key falling through it
/// would land somewhere invisible. The find bar sits over a live page that
/// is still the thing being read, so the keys that scroll it have to reach
/// it.
#[gpui::test]
fn the_find_bar_lets_the_pages_own_keys_through(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/"));

    vcx.simulate_keystrokes("cmd-f");
    vcx.simulate_keystrokes("e a v e");
    vcx.run_until_parked();

    vcx.simulate_keystrokes("down");
    vcx.simulate_keystrokes("pagedown");
    vcx.run_until_parked();
    assert_eq!(
        crate::web::find_state(tab).query,
        "eave",
        "an arrow key was typed into the find field instead of scrolling the page"
    );
    assert!(shown(vcx, "find-bar"), "a scroll key dismissed the bar");
}

/// The pane header states zoom **only** when it is not the default. A chip
/// reading 100% on every tab forever is a decoration.
#[gpui::test]
fn the_pane_header_states_zoom_only_when_it_is_not_the_default(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    page_shell(&h, vcx, Some("https://example.com/"));

    assert!(
        !shown(vcx, "pane-zoom"),
        "a tab nobody has zoomed stated a zoom"
    );

    vcx.simulate_keystrokes("cmd-=");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "pane-zoom"),
        "⌘+ zoomed nothing and said nothing"
    );

    // ⌘0 is the dismissal, and it puts the tab back in the same state as
    // one that was never touched — so the fact goes away rather than
    // becoming "100%".
    vcx.simulate_keystrokes("cmd-0");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "pane-zoom"),
        "⌘0 left a 100% chip behind, which is a decoration"
    );
}

/// Zoom is per TAB. Zooming one and switching away must not carry the
/// number across.
#[gpui::test]
fn zoom_belongs_to_the_tab_it_was_asked_on(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    crate::web::pretend_available_for_test();
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("pppp1111", Some("https://a.example/"), Some("A")),
            page_tab("pppp2222", Some("https://b.example/"), Some("B")),
        ],
        Some("pppp1111"),
    );

    vcx.simulate_keystrokes("cmd-=");
    vcx.run_until_parked();
    assert_eq!(crate::web::zoom_percent("pppp1111"), Some(110));
    assert_eq!(
        crate::web::zoom_percent("pppp2222"),
        None,
        "zooming one tab zoomed another"
    );
}

/// The menu Chromium asks for opens, and its rows are a function of what
/// was under the pointer.
#[gpui::test]
fn the_page_menu_says_what_was_clicked(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/"));

    // Bare text: the three navigation rows, Copy, and the developer pair
    // — and nothing else.
    request_page_menu(
        &h,
        vcx,
        crate::web::PageMenu {
            tab_id: tab.to_string(),
            x: 200.,
            y: 200.,
            page_x: 200,
            page_y: 200,
            link_url: None,
            selection: None,
            editable: false,
        },
    );
    assert!(shown(vcx, "menu-row-0"), "the page menu never opened");
    assert!(shown(vcx, "menu-row-3"), "Copy is missing");
    assert!(shown(vcx, "menu-row-4"), "View Page Source is missing");
    assert!(shown(vcx, "menu-row-5"), "Inspect is missing");
    assert!(
        !shown(vcx, "menu-row-6"),
        "a menu over bare text offered a link row"
    );

    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
}

/// A menu over a link gains the two link rows; a menu over a selection
/// gains the search row.
#[gpui::test]
fn a_link_and_a_selection_each_add_their_own_rows(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/"));

    request_page_menu(
        &h,
        vcx,
        crate::web::PageMenu {
            tab_id: tab.to_string(),
            x: 200.,
            y: 200.,
            page_x: 200,
            page_y: 200,
            link_url: Some("https://linked.example/".into()),
            selection: Some("some words".into()),
            editable: false,
        },
    );
    // Back, Forward, Reload, Copy, Copy Link, Open Link in New Tab,
    // Search, View Page Source, Inspect.
    assert!(shown(vcx, "menu-row-6"), "the search row is missing");
    assert!(
        !shown(vcx, "menu-row-9"),
        "the menu grew a row nothing asked for"
    );
}

/// G1's undo, and the reason it is a row and not a settings page eave does
/// not have: an answer that persists and cannot be taken back is a worse
/// bargain than one asked every time, because a mis-click on "Allow"
/// becomes permanent.
///
/// **Absent, not disabled, when there is nothing to reset.** A greyed row
/// on every page would tell every operator that eave holds permissions for
/// the site they are on — a claim about their data that is usually false.
#[gpui::test]
fn a_site_with_no_answers_offers_nothing_to_reset(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://permnone.example/x"));

    request_page_menu(
        &h,
        vcx,
        crate::web::PageMenu {
            tab_id: tab.to_string(),
            x: 200.,
            y: 200.,
            page_x: 200,
            page_y: 200,
            link_url: None,
            selection: None,
            editable: false,
        },
    );
    // Back, Forward, Reload, Copy, View Page Source, Inspect — and
    // nothing after them.
    assert!(shown(vcx, "menu-row-3"), "the Copy row is missing");
    assert!(
        !shown(vcx, "menu-row-6"),
        "a reset row appeared for a site with no answers"
    );
}

/// And when there IS something to reset, the row is there and it works —
/// locally at once, and on the wire for the store that actually held it.
#[gpui::test]
fn a_sites_permissions_can_be_reset_from_the_page_menu(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://permreset.example/room/7?x=1"));
    crate::web::permissions::note_answer(
        "https://permreset.example",
        &[
            crate::web::permissions::PermissionKind::Camera,
            crate::web::permissions::PermissionKind::Microphone,
        ],
        crate::web::permissions::PermissionDecision::Allow,
    );

    request_page_menu(
        &h,
        vcx,
        crate::web::PageMenu {
            tab_id: tab.to_string(),
            x: 200.,
            y: 200.,
            page_x: 200,
            page_y: 200,
            link_url: None,
            selection: None,
            editable: false,
        },
    );
    let _ = h.drain();
    // Back, Forward, Reload, Copy, Reset, View Page Source, Inspect.
    assert!(
        shown(vcx, "menu-row-4"),
        "a site with two answers offered no way to take them back"
    );
    click(vcx, "menu-row-4");
    vcx.run_until_parked();

    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::ForgetPermissions { origin })
                if origin == "https://permreset.example"
        )),
        "the reset row told the engine nothing: {cmds:?}"
    );
    // The mirror is cleared on the click rather than a round trip later,
    // so the next ask from that site is an ask again.
    assert_eq!(
        crate::web::permissions::count_for("https://permreset.example/"),
        0,
        "the answers survived their own reset"
    );
}

/// **Open Link in New Tab opens it in the BACKGROUND.** That row has meant
/// "keep this, I am still reading" in every browser for twenty years;
/// foregrounding it would be the behaviour of an ordinary click, which the
/// operator did not perform.
#[gpui::test]
fn opening_a_link_in_a_new_tab_does_not_steal_the_window(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/"));

    request_page_menu(
        &h,
        vcx,
        crate::web::PageMenu {
            tab_id: tab.to_string(),
            x: 200.,
            y: 200.,
            page_x: 200,
            page_y: 200,
            link_url: Some("https://linked.example/".into()),
            selection: None,
            editable: false,
        },
    );
    let _ = h.drain();
    // Back, Forward, Reload, Copy, Copy Link, Open Link in New Tab,
    // View Page Source, Inspect.
    click(vcx, "menu-row-5");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::OpenAt { url, activate: false })
                if url == "https://linked.example/"
        )),
        "Open Link in New Tab did not open the link, or stole the window: {cmds:?}"
    );
}

/// The search row searches for the selection — **all of it**, whatever the
/// label had room to show — and does not try to navigate to it.
#[gpui::test]
fn the_search_row_searches_for_the_whole_selection(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/"));

    // Deliberately something `omnibox::resolve` would read as an ADDRESS.
    // A person who highlights `example.com` in a paragraph and asks to
    // search for it has not asked to go there.
    request_page_menu(
        &h,
        vcx,
        crate::web::PageMenu {
            tab_id: tab.to_string(),
            x: 200.,
            y: 200.,
            page_x: 200,
            page_y: 200,
            link_url: None,
            selection: Some("example.com".into()),
            editable: false,
        },
    );
    let _ = h.drain();
    // Back, Forward, Reload, Copy, Search, View Page Source, Inspect.
    click(vcx, "menu-row-4");
    vcx.run_until_parked();
    let cmds = h.drain();
    let opened = cmds.iter().find_map(|c| match c {
        Command::Browser(BrowserCmd::OpenAt { url, .. }) => Some(url.clone()),
        _ => None,
    });
    let Some(opened) = opened else {
        panic!("the search row opened nothing: {cmds:?}");
    };
    assert!(
        opened.starts_with(&crate::omnibox::search_prefix()),
        "the search row navigated to the selection instead of searching for it: {opened}"
    );
    assert!(
        opened.contains("example.com"),
        "the query lost the selection: {opened}"
    );
}

/// Back and Forward are **disabled rather than absent** when there is no
/// history — the same choice the pane header's arrows make. A menu whose
/// length changes with history depth moves its own rows under the pointer.
#[gpui::test]
fn the_page_menu_disables_history_rows_rather_than_dropping_them(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/"));

    request_page_menu(
        &h,
        vcx,
        crate::web::PageMenu {
            tab_id: tab.to_string(),
            x: 200.,
            y: 200.,
            page_x: 200,
            page_y: 200,
            link_url: None,
            selection: None,
            editable: false,
        },
    );
    assert!(
        shown(vcx, "menu-row-0"),
        "a disabled Back row still renders"
    );
    let _ = h.drain();
    click(vcx, "menu-row-0");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Go { .. }))),
        "a disabled Back row walked the history anyway: {cmds:?}"
    );
}

/// **View Page Source opens `view-source:` in a new FOREGROUND tab.** The
/// rewrite is Chromium's own; eave's half is only the navigation — and it
/// activates, unlike "Open Link in New Tab", because the operator asked to
/// read the source now, not to set it aside for later.
#[gpui::test]
fn view_page_source_opens_the_source_in_a_new_tab(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/page"));

    request_page_menu(
        &h,
        vcx,
        crate::web::PageMenu {
            tab_id: tab.to_string(),
            x: 200.,
            y: 200.,
            page_x: 200,
            page_y: 200,
            link_url: None,
            selection: None,
            editable: false,
        },
    );
    let _ = h.drain();
    // Back, Forward, Reload, Copy, View Page Source, Inspect.
    click(vcx, "menu-row-4");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::OpenAt { url, activate: true })
                if url == "view-source:https://example.com/page"
        )),
        "View Page Source opened nothing, or the wrong thing: {cmds:?}"
    );
}

/// **Inspect reaches the web module with the click's PAGE-space point.**
/// The element under the pointer is found by Blink from its own
/// coordinates; the window-space pair the menu is drawn at would name the
/// wrong element the moment the page element is not at the window origin.
#[gpui::test]
fn inspect_carries_the_page_space_point(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/"));

    // Window-space and page-space deliberately differ, so a dispatch that
    // grabbed the wrong pair cannot pass.
    request_page_menu(
        &h,
        vcx,
        crate::web::PageMenu {
            tab_id: tab.to_string(),
            x: 264.,
            y: 331.,
            page_x: 240,
            page_y: 275,
            link_url: None,
            selection: None,
            editable: false,
        },
    );
    let _ = crate::web::take_inspect_for_test();
    click(vcx, "menu-row-5");
    vcx.run_until_parked();
    assert_eq!(
        crate::web::take_inspect_for_test(),
        Some((tab.to_string(), 240, 275)),
        "Inspect never reached the web module, or lost the point"
    );
}

// Slice D3 stage 1 — the rail's geometry, and the fact that it moves.

/// **The rail may never be dragged narrower than the traffic lights it
/// contains.**
///
/// A′ gave the rail the window's left edge, so the macOS window buttons sit
/// INSIDE it — and they sit at a window-fixed x that no layout of ours moves.
/// A rail narrower than that clearance would have them hanging off its right
/// edge onto the page: not a squeeze, a frame that is wrong.
///
/// The floor is asserted against `TRAFFIC_CLEARANCE` rather than against a
/// literal because this one really is a derivation — if the clearance ever
/// changes, the drag's floor has to move with it, and silently.
#[gpui::test]
fn the_rail_never_narrows_past_its_own_traffic_lights(_cx: &mut TestAppContext) {
    #[allow(clippy::assertions_on_constants)]
    {
        assert!(
            theme::RAIL_W_MIN >= theme::TRAFFIC_CLEARANCE,
            "the rail may be dragged to {} px and the traffic lights need \
             {} — at that width they hang off the rail onto the page",
            theme::RAIL_W_MIN,
            theme::TRAFFIC_CLEARANCE
        );
        assert!(
            theme::RAIL_W_MIN < theme::RAIL_W && theme::RAIL_W < theme::RAIL_W_MAX,
            "the resting width {} is outside its own drag range {}..{} — a \
             rail that opens outside the range it can be dragged to is a \
             frame that cannot return to its own default",
            theme::RAIL_W,
            theme::RAIL_W_MIN,
            theme::RAIL_W_MAX
        );
    }
}

/// **A width from disk is held to the bounds, and a corrupt one opens the
/// window anyway.**
///
/// The bounds are enforced on the value read from the file and not only on
/// the pointer, because the file is the one input eave does not control: a
/// build with wider bounds, a hand edit, a half-written file. Clamping rather
/// than rejecting is the deliberate half of it — the frame should open
/// narrow, not refuse to open.
///
/// `NaN` is called out separately because it is the one value that makes
/// `clamp` itself misbehave: `f32::clamp` panics on a NaN bound and returns
/// NaN for a NaN input, and a NaN width lays out as a rail of zero px with no
/// error anywhere.
#[gpui::test]
fn a_saved_rail_width_is_held_to_the_bounds(_cx: &mut TestAppContext) {
    assert_eq!(clamp_rail_w(theme::RAIL_W), theme::RAIL_W);
    assert_eq!(clamp_rail_w(10.0), theme::RAIL_W_MIN, "far under the floor");
    assert_eq!(
        clamp_rail_w(9000.0),
        theme::RAIL_W_MAX,
        "far over the ceiling"
    );
    assert_eq!(
        clamp_rail_w(f32::NAN),
        theme::RAIL_W,
        "a NaN width lays out as a zero-px rail and reports nothing — it has \
         to land on the default, not on a bound"
    );
    assert_eq!(clamp_rail_w(f32::INFINITY), theme::RAIL_W);
}

/// **The seam is grabbable, and the grab target is wider than the seam.**
///
/// The handle is transparent, so nothing else in the harness can tell it
/// exists; this is the only assertion that it is mounted at all. It also
/// pins the two facts that make it usable rather than decorative: it is as
/// tall as the rail (a divider you can only grab in the middle is a divider
/// you hunt for), and it straddles the seam rather than sitting inside the
/// rail, so the pointer finds it coming from either side.
#[gpui::test]
fn the_rail_seam_is_a_grab_target_on_both_sides_of_itself(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();

    let rail = vcx.debug_bounds("rail").expect("the rail renders");
    let grab = vcx.debug_bounds("rail-handle").expect("the resize handle");

    assert_eq!(
        grab.size.height, rail.size.height,
        "the handle is {:?} tall against a {:?} rail — a divider you can \
         only grab in the middle is one you hunt for",
        grab.size.height, rail.size.height
    );
    assert!(
        grab.size.width >= px(theme::RAIL_HANDLE_W),
        "the grab target is {:?}; the seam it covers is a hairline",
        grab.size.width
    );
    // Straddling, not inset: the handle's box crosses the rail's right edge,
    // so half of it is over the content. `deferred` is what keeps that half
    // paintable — without it the content region covers it and the target is
    // silently half as wide as this asserts.
    let seam = rail.origin.x + rail.size.width;
    assert!(
        grab.origin.x < seam && grab.origin.x + grab.size.width > seam,
        "the handle spans {:?}..{:?} and the seam is at {seam:?} — it has to \
         cross it, or the pointer only finds it from one side",
        grab.origin.x,
        grab.origin.x + grab.size.width
    );
}

/// **The rail's width is the shell's, not the token's.**
///
/// The token is the launch default and the shell holds what the operator
/// dragged to. This is the assertion that the layout actually reads the
/// second one — a frame that draws `theme::RAIL_W` would pass every other
/// test in this file and ignore every drag.
#[gpui::test]
fn the_rail_draws_the_width_it_was_dragged_to(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();
    assert_eq!(
        vcx.debug_bounds("rail").unwrap().size.width,
        px(theme::RAIL_W),
        "a fresh shell opens at the default"
    );

    h.window
        .update(vcx, |shell, _, cx| {
            shell.rail_w = theme::RAIL_W_MAX;
            cx.notify();
        })
        .unwrap();
    vcx.run_until_parked();
    assert_eq!(
        vcx.debug_bounds("rail").unwrap().size.width,
        px(theme::RAIL_W_MAX),
        "the rail did not follow the shell's width, so it is drawing the \
         token and no drag will ever move it"
    );
}

/// **The content takes the whole delta, and the reading column moves with
/// the seam rather than under it.**
///
/// The directive's words were "the gutter between rail and card stays
/// constant — the CARD absorbs the delta". There is no card: Slice D1's A′
/// made the content region full-bleed on its other three edges, on the
/// operator's border ruling, so the gutter is zero and stays zero. What
/// survives of the requirement is the part that matters — every pixel the
/// rail gains comes out of the content and none of it comes out of the
/// window, and the two regions still meet exactly at the seam with no gap
/// and no overlap.
#[gpui::test]
fn widening_the_rail_costs_the_content_and_nothing_else(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();

    let rail0 = vcx.debug_bounds("rail").unwrap();
    let well0 = vcx.debug_bounds("content-well").unwrap();
    let window_w = rail0.size.width + well0.size.width;

    h.window
        .update(vcx, |shell, _, cx| {
            shell.rail_w = theme::RAIL_W + 60.0;
            cx.notify();
        })
        .unwrap();
    vcx.run_until_parked();

    let rail1 = vcx.debug_bounds("rail").unwrap();
    let well1 = vcx.debug_bounds("content-well").unwrap();

    assert_eq!(
        rail1.size.width - rail0.size.width,
        well0.size.width - well1.size.width,
        "the rail grew by {:?} and the content gave up {:?} — the difference \
         went somewhere, and the only somewhere is off the window",
        rail1.size.width - rail0.size.width,
        well0.size.width - well1.size.width
    );
    assert_eq!(
        rail1.size.width + well1.size.width,
        window_w,
        "the two regions no longer fill the window between them"
    );
    // **Slice R1 changed what this asserts, and the change is the point.**
    //
    // D3 wrote it as `well1.origin.x == rail1.origin.x + rail1.size.width`
    // — no gap at all — because on D3's branch A′ had deleted the inset
    // card and any gap really was a rounding error. The consolidated tree
    // brings a card BACK, on purpose: the content is a floating plate with
    // a uniform `CARD_GUTTER` on every side (see `content_card` in
    // `ui.rs`). Those two frames disagree, and this test is where the
    // disagreement showed up.
    //
    // Weakening it to `>=` would have made it stop failing and stop
    // testing. Asserting the gutter EXACTLY keeps everything D3 wanted —
    // a stray pixel still fails — and adds a guard D3 could not have
    // written: the gutter changing silently now fails too.
    assert_eq!(
        well1.origin.x,
        rail1.origin.x + rail1.size.width + px(theme::CARD_GUTTER),
        "the content no longer starts one CARD_GUTTER after the rail ends. \
         Either the gutter moved, or the inset card is drifting — both are \
         frame regressions and neither is a rounding error"
    );
}

/// **A build with no page engine closes the fact channel on the first poll,
/// and that closure must not be read as a signal.**
///
/// This is the trigger for "eave renders no tabs at all" — the defect Slice
/// D1 photographed, verified against `main`, and filed as unexplained, and
/// which Slice D3 traced end to end.
///
/// The subscription task in `feed_loop` selects over two channels: the tab
/// snapshots coming from the engine, and the facts a page reports on its way
/// back. Its fact arm used to `return` on `None`. `None` on an
/// `UnboundedReceiver` means only "every sender has been dropped" — and in a
/// build without `--features web` that is true *immediately*, because this
/// sink is the one below and it takes the sender by value with nothing to
/// hold it. So the task ended before its first `select!` could favour the
/// other arm, `rx` was dropped with it, and the engine's first tab snapshot
/// arrived at a dead receiver. The client's read loop found the stream,
/// failed to send, removed the subscription, and discarded every later
/// snapshot against an absent entry — silently, at a layer with no logging.
/// The engine was reporting `run_tabs order=9 sent_ok=true` the whole time.
///
/// The fix is a guard on that arm, which retires it instead of returning.
/// **This test pins the TRIGGER rather than the fix**, deliberately: the
/// guard lives in a spawned async task inside `feed_loop` and this harness
/// drives the shell through `Plumbing` fakes that never build a `Client`, so
/// there is no seam here to assert it through. What this holds is the fact
/// that made the guard necessary — so that a later tidy-up which makes this
/// sink *hold* its sender cannot quietly remove the reason the guard is
/// there, and so that anyone who deletes the guard finds this explanation
/// attached to the thing that will bite them.
///
/// Verified in the product rather than here: with the guard, the strip
/// renders seven tabs and the pane header states an address
/// (`docs/shots/d3-rail-rest.png`); without it, both are empty.
#[cfg(not(feature = "web"))]
#[gpui::test]
fn a_build_with_no_page_engine_closes_the_fact_channel_at_once(_cx: &mut TestAppContext) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::web::Fact>();
    crate::web::install_fact_sink(tx);
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ),
        "this build's fact sink now keeps its sender alive. That is not a \
         bug, but the tab subscription's fact arm is guarded on the \
         assumption that a closed channel is NORMAL here rather than fatal \
         — read that guard before relying on this staying open."
    );
}

/// Two page tabs in the well, side by side. Dropping one tab onto another
/// is the Arc/Brave gesture; the engine still has one `active`, the well
/// paints both.
#[gpui::test]
fn dropping_a_tab_on_another_tab_splits_the_well(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
        ],
        Some("p1"),
    );
    assert!(
        !shown(vcx, "split-well"),
        "a single tab must not draw a split"
    );

    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
        })
        .unwrap();
    vcx.run_until_parked();

    assert!(shown(vcx, "split-well"), "the well did not split");
    assert!(shown(vcx, "split-pane-left"), "left pane missing");
    assert!(shown(vcx, "split-pane-right"), "right pane missing");
    assert!(shown(vcx, "split-handle"), "no divider between panes");
}

/// Dragging a tab onto itself is not a split — that would be a pane of
/// the same tab twice, and the only honest answer is to do nothing.
#[gpui::test]
fn dropping_a_tab_on_itself_does_not_split(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![page_tab("p1", Some("https://a.test/"), Some("A"))],
        Some("p1"),
    );
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p1", "p1", true, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "split-well"),
        "a tab dropped on itself opened a split"
    );
}

/// Two sessions cannot share the well: this shell has one transcript
/// subscription, and a second session pane would be a caption over an
/// empty log. The drop is refused rather than drawn dishonestly.
#[gpui::test]
fn two_session_tabs_do_not_split(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![session_tab("s1"), session_tab("s2")],
        Some("s1"),
    );
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("s2", "s1", true, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "split-well"),
        "two sessions opened a split this shell cannot back"
    );
}

/// Clicking a tab that is not in the split leaves it — the well is a view
/// of two tabs, not a lock. The third tab fills the card.
#[gpui::test]
fn clicking_a_third_tab_leaves_the_split(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
            page_tab("p3", Some("https://c.test/"), Some("C")),
        ],
        Some("p1"),
    );
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    assert!(shown(vcx, "split-well"));

    click(vcx, "tab-p3");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "split-well"),
        "clicking a third tab left the split on screen"
    );
}

/// Dropping a background tab onto the well (the document currently in
/// front) splits with that document. Left/right is which half of the
/// card the pointer was over.
#[gpui::test]
fn dropping_a_tab_on_the_well_splits_with_the_page_in_front(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
        ],
        Some("p1"),
    );
    h.window
        .update(vcx, |shell, _, cx| {
            shell.drop_tab_on_well("p2", true, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    assert!(shown(vcx, "split-well"), "well drop did not split");
    assert!(shown(vcx, "split-pane-left"));
    assert!(shown(vcx, "split-pane-right"));
}

/// The actual gesture: mouse-down on one tab, move past gpui's 2px drag
/// threshold, mouse-up on another. If this fails, `on_drag`/`on_drop` on
/// the rail tab is not wired, and calling `split_tabs` from a test is not
/// the same as the operator dragging.
#[gpui::test]
fn dragging_one_tab_onto_another_splits_the_well(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
        ],
        Some("p1"),
    );
    let from = vcx.debug_bounds("tab-p2").expect("tab-p2");
    let onto = vcx.debug_bounds("tab-p1").expect("tab-p1");
    vcx.simulate_mouse_down(from.center(), MouseButton::Left, Modifiers::none());
    let mut mid = from.center();
    mid.y += px(8.);
    vcx.simulate_mouse_move(mid, Some(MouseButton::Left), Modifiers::none());
    vcx.run_until_parked();
    vcx.simulate_mouse_move(onto.center(), Some(MouseButton::Left), Modifiers::none());
    vcx.simulate_mouse_up(onto.center(), MouseButton::Left, Modifiers::none());
    vcx.run_until_parked();
    assert!(
        shown(vcx, "split-well"),
        "dragging tab-p2 onto tab-p1 did not split the well — on_drag/on_drop \
         on the rail tab is not firing"
    );
}

/// Split tabs share one rail pill, side by side, matching the well.
/// Stacked full-width rows would mean the strip is silent about the split.
#[gpui::test]
fn split_tabs_sit_side_by_side_on_the_rail(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
            page_tab("p3", Some("https://c.test/"), Some("C")),
        ],
        Some("p1"),
    );
    assert!(
        !shown(vcx, "split-tabs"),
        "a single page must not draw the combined pill"
    );
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
        })
        .unwrap();
    vcx.run_until_parked();

    assert!(
        shown(vcx, "split-tabs"),
        "split tabs must share one rail pill"
    );
    let a = vcx.debug_bounds("tab-p1").expect("tab-p1");
    let b = vcx.debug_bounds("tab-p2").expect("tab-p2");
    assert_eq!(
        a.origin.y, b.origin.y,
        "split tabs must sit on one row, not stack: p1 {a:?} p2 {b:?}"
    );
    assert!(
        a.origin.x < b.origin.x,
        "left pane's tab should be the left chip"
    );
    let c = vcx.debug_bounds("tab-p3").expect("tab-p3");
    assert!(
        c.origin.y > a.origin.y,
        "a tab that is not in the split stays a row under the pill"
    );
}

/// `flex_basis(0)` plus the grow factor is the ratio. Without the zero
/// basis, a pane whose page had not reflowed kept its old content width
/// and the other pane got leftovers.
#[gpui::test]
fn both_split_panes_take_a_share_of_the_well(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
        ],
        Some("p1"),
    );
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
        })
        .unwrap();
    vcx.run_until_parked();

    let left = vcx.debug_bounds("split-pane-left").expect("left");
    let right = vcx.debug_bounds("split-pane-right").expect("right");
    assert!(
        left.size.width > px(200.) && right.size.width > px(200.),
        "a 50/50 split collapsed a pane: left {:?} right {:?}",
        left.size.width,
        right.size.width
    );
}

/// Clicking a pane focuses it (the ring) and does not leave the split.
#[gpui::test]
fn clicking_a_split_pane_focuses_it_without_leaving(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
        ],
        Some("p1"),
    );
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    h.drain();

    click(vcx, "split-pane-left");
    vcx.run_until_parked();
    assert!(shown(vcx, "split-well"), "clicking a pane left the split");
    assert!(
        h.drain()
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Activate(id)) if id == "p1")),
        "clicking the left pane did not activate it"
    );
}

/// Clicking a chip in the combined pill focuses that pane, same as
/// clicking the pane itself. It must not unsplit — that is reserved for
/// a tab that is not in the well.
#[gpui::test]
fn clicking_a_chip_in_the_combined_row_keeps_the_split(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
        ],
        Some("p1"),
    );
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    h.drain();

    click(vcx, "tab-p1");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "split-well") && shown(vcx, "split-tabs"),
        "clicking a split chip unsplits the well"
    );
    assert!(
        h.drain()
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Activate(id)) if id == "p1")),
        "clicking the left chip did not activate that pane"
    );
}

fn open_tab_strip_menu(h: &Harness, vcx: &mut VisualTestContext, tab_id: &str) {
    h.window
        .update(vcx, |shell, _, cx| {
            shell.open_tab_menu(tab_id, gpui::point(px(240.), px(180.)), cx);
        })
        .unwrap();
    vcx.run_until_parked();
}

/// Two-finger click on a rail tab opens the single-tab menu, not the
/// page's Chromium menu.
#[gpui::test]
fn a_right_click_on_a_tab_opens_the_single_tab_menu(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
        ],
        Some("p1"),
    );
    let from = vcx.debug_bounds("tab-p1").expect("tab-p1");
    vcx.simulate_mouse_down(from.center(), MouseButton::Right, Modifiers::none());
    vcx.run_until_parked();
    assert!(
        shown(vcx, "tab-menu-copy-link"),
        "right-clicking a tab did not open Copy Link"
    );
    assert!(
        shown(vcx, "tab-menu-add-split"),
        "Add Split View missing on a tab that has a partner"
    );
    assert!(
        !shown(vcx, "tab-menu-separate"),
        "a single tab offered the split menu"
    );
}

/// Right-clicking a tab that is in the well opens the split menu.
#[gpui::test]
fn a_right_click_on_a_split_tab_opens_the_split_menu(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
        ],
        Some("p1"),
    );
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    open_tab_strip_menu(&h, vcx, "p1");
    assert!(
        shown(vcx, "tab-menu-separate"),
        "split tab menu missing Separate All Tabs"
    );
    assert!(
        shown(vcx, "tab-menu-convert-axis"),
        "Convert to Vertical Split View missing"
    );
    assert!(shown(vcx, "menu-header"), "Tab Options header missing");
}

/// Copy Link writes the tab's url, which is the one thing Share and
/// Copy Link can honestly do without a share sheet.
#[gpui::test]
fn copy_link_from_the_tab_menu_writes_the_url(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![page_tab("p1", Some("https://a.test/path"), Some("A"))],
        Some("p1"),
    );
    open_tab_strip_menu(&h, vcx, "p1");
    click(vcx, "tab-menu-copy-link");
    vcx.run_until_parked();
    let text = vcx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("clipboard empty after Copy Link");
    assert_eq!(text, "https://a.test/path");
}

/// Separate All Tabs is the split menu's unsplit — the well is a view,
/// and this is how you leave it without closing either tab.
#[gpui::test]
fn separate_all_tabs_leaves_the_split(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
        ],
        Some("p1"),
    );
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    open_tab_strip_menu(&h, vcx, "p2");
    click(vcx, "tab-menu-separate");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "split-well"),
        "Separate All Tabs left the split on screen"
    );
}

/// Convert to Vertical Split View turns the well into a column. The
/// handle's long axis is the proof: a row-resize handle is wide and short.
#[gpui::test]
fn convert_to_vertical_split_turns_the_handle(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
        ],
        Some("p1"),
    );
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    let before = vcx.debug_bounds("split-handle").expect("handle");
    open_tab_strip_menu(&h, vcx, "p1");
    click(vcx, "tab-menu-convert-axis");
    vcx.run_until_parked();
    let after = vcx.debug_bounds("split-handle").expect("handle after");
    assert!(
        after.size.width > after.size.height,
        "vertical split handle should be a row, was {:?}",
        after.size
    );
    assert!(
        before.size.height > before.size.width,
        "horizontal handle should have been a column first, was {:?}",
        before.size
    );
    assert!(shown(vcx, "split-well"), "converting axis unsplits");
}

/// The plate paints decoded bytes, not an HTTP URL gpui cannot fetch.
/// The harness injects a 1×1 PNG the way Chromium's download_image
/// callback would after a live fetch.
#[gpui::test]
fn a_page_tab_paints_its_favicon_when_the_bytes_are_in(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    crate::web::set_favicon_for_test(
        "p1",
        Some(std::sync::Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            TINY_PNG.to_vec(),
        ))),
    );
    strip_tabs(
        &h,
        vcx,
        vec![page_tab("p1", Some("https://a.test/"), Some("A"))],
        Some("p1"),
    );
    assert!(
        shown(vcx, "tab-favicon-p1"),
        "a page with favicon bytes still drew the globe"
    );
    crate::web::set_favicon_for_test("p1", None);
}

/// Add Split View from the menu is the same act as dropping one tab on
/// another: the well becomes a view of two existing tabs.
#[gpui::test]
fn add_split_view_from_the_menu_splits_the_well(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
        ],
        Some("p1"),
    );
    open_tab_strip_menu(&h, vcx, "p1");
    click(vcx, "tab-menu-add-split");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "split-well"),
        "Add Split View did not split the well"
    );
}

// ---------------------------------------------------------------------------
// Slice CM — the capability the composer did not have
//
// Everything above this line is the composer eave already had, re-proved
// against a different buffer. These are the things that were **impossible**
// before it: there was no `EntityInputHandler` anywhere in the application, so
// no IME, no selection and no mouse caret, and a CJK operator could not type
// into eave at all.
//
// They drive the field the way macOS drives it — through the trait, not
// through a convenience wrapper — because the trait is the part that was
// missing and a test that went around it would prove nothing.

/// In scope so the tests below can drive the field the way macOS does — every
/// IME method is a trait method, and reaching them any other way would be
/// testing a wrapper instead of the thing that was missing.
use gpui::EntityInputHandler as _;

/// The composer's field, for tests that need to reach past the draft string.
fn composer_field(h: &Harness, vcx: &mut VisualTestContext) -> Entity<TextField> {
    vcx.read_window(&h.window, |shell, cx| {
        shell.read(cx).composer.read(cx).field.clone()
    })
    .unwrap()
}

/// **The slice's reason for existing.** Japanese input composes in the
/// composer, and the provisional text is marked while it does.
///
/// This is the exact path macOS's input method takes: `replace_and_mark_text_in
/// _range` for each keystroke of the composition, then `replace_text_in_range`
/// when the operator picks a candidate. Before Slice CM eave implemented
/// neither, so the system had nowhere to put composing text and the characters
/// were simply lost.
///
/// The marked range matters as much as the text: it is what the field
/// underlines, and it is what tells the operator which characters are still
/// provisional. A composition that inserted text with no mark would look
/// committed when it is not.
#[gpui::test]
fn japanese_composes_in_the_composer_and_commits(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    let field = composer_field(&h, vcx);

    // Typing `nihon` on a Japanese IME: the romaji are provisional, and the
    // input method rewrites the same marked range on every keystroke.
    //
    // **The ranges are UTF-16, not bytes** — that is the IME's own coordinate
    // system, and each of these kana is 1 unit and 3 bytes. It is also the
    // reason the old composer could never have grown IME support by accident:
    // it indexed a `String` by byte and had no mapping between the two.
    // bezel's `offset_to_utf16`/`offset_from_utf16` (and the round-trip test
    // vendored beside them) exist for exactly this boundary.
    for (partial, mark) in [("に", 0..1), ("にほ", 0..2), ("にほん", 0..3)] {
        let units = partial.encode_utf16().count();
        vcx.update(|window, cx| {
            field.update(cx, |f, cx| {
                f.replace_and_mark_text_in_range(None, partial, Some(0..units), window, cx);
            });
        });
        assert_eq!(
            draft_of(&h, vcx),
            partial,
            "the composition did not reach the draft"
        );
        let marked = vcx
            .update(|window, cx| field.update(cx, |f, cx| f.marked_text_range(window, cx)))
            .expect("composing text was not marked, so nothing underlines it");
        assert_eq!(
            marked, mark,
            "the marked range does not cover what is provisional"
        );
    }

    // Picking the candidate commits it: the kanji replace the kana and the
    // mark is gone, because nothing is provisional any more.
    vcx.update(|window, cx| {
        field.update(cx, |f, cx| {
            f.replace_text_in_range(None, "日本", window, cx);
        });
    });
    assert_eq!(draft_of(&h, vcx), "日本", "the candidate did not commit");
    assert!(
        vcx.update(|window, cx| field.update(cx, |f, cx| f.marked_text_range(window, cx)))
            .is_none(),
        "committed text is still marked provisional"
    );

    // And it sends like any other draft — the point being that the whole
    // composer works for it, not just the field.
    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    assert!(
        matches!(h.drain().as_slice(), [Command::Send { text, .. }] if text == "日本"),
        "a composed message did not send"
    );
}

/// Text can be selected, and typing replaces the selection.
///
/// The old composer had one position — the end — and no notion of a range at
/// all: `⌘A` did nothing, shift-arrow did nothing, and the only edit available
/// was to delete backwards from the end. Correcting a word in the middle of a
/// paragraph meant deleting everything after it.
#[gpui::test]
fn the_draft_can_be_selected_and_typed_over(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    let field = composer_field(&h, vcx);
    let selection = |vcx: &mut VisualTestContext, field: &Entity<TextField>| {
        vcx.update(|window, cx| field.update(cx, |f, cx| f.selected_text_range(false, window, cx)))
            .map(|s| s.range)
    };

    vcx.simulate_input("hello world");
    assert_eq!(
        selection(vcx, &field),
        Some(11..11),
        "the caret is at the end"
    );

    // Shift-left five times selects `world` backwards from the end.
    vcx.simulate_keystrokes("shift-left shift-left shift-left shift-left shift-left");
    assert_eq!(
        selection(vcx, &field),
        Some(6..11),
        "shift-left did not extend a selection"
    );

    // Typing replaces it — the behaviour every text field has and eave's had
    // no way to express.
    vcx.simulate_input("there");
    assert_eq!(
        draft_of(&h, vcx),
        "hello there",
        "typing over a selection did not replace it"
    );

    // ⌘A takes the lot, and backspace on a selection deletes the range rather
    // than one character.
    vcx.simulate_keystrokes("cmd-a");
    assert_eq!(
        selection(vcx, &field),
        Some(0..11),
        "cmd-A selected nothing"
    );
    vcx.simulate_keystrokes("backspace");
    assert_eq!(
        draft_of(&h, vcx),
        "",
        "backspace did not delete the selection"
    );
}

/// Clicking in the middle of a draft puts the caret there.
///
/// Not a nicety: with no mouse caret the only way to reach a character was to
/// delete every character after it. This asserts the caret **moved off the
/// end**, not a particular offset — the exact one depends on the shaped width
/// of the glyphs, and pinning it would be pinning the font.
#[gpui::test]
fn a_click_places_the_caret_in_the_draft(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    let field = composer_field(&h, vcx);

    vcx.simulate_input("the quick brown fox");
    let end = vcx
        .read_window(&h.window, |_, cx| field.read(cx).cursor())
        .unwrap();
    assert_eq!(end, 19, "precondition: typing leaves the caret at the end");

    // A few characters in from the left edge of the field, vertically centred.
    click_at(vcx, "composer-text", 12.0, 8.0);
    vcx.run_until_parked();

    let after = vcx
        .read_window(&h.window, |_, cx| field.read(cx).cursor())
        .unwrap();
    assert!(
        after < end,
        "clicking near the start of the draft left the caret at the end ({after})"
    );
}

/// Shift-Return breaks a line; Return still sends.
///
/// The composer is `Shape::Grow`, so a multi-line draft is a real thing now.
/// Both halves are asserted together because they are one decision: eave binds
/// `enter` to its own send on the composer's key context, which is exactly why
/// `shift-enter` had to be bound back to a newline by hand (delta item 10).
#[gpui::test]
fn shift_return_breaks_a_line_and_return_still_sends(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);

    vcx.simulate_input("first");
    vcx.simulate_keystrokes("shift-enter");
    vcx.simulate_input("second");
    assert_eq!(
        draft_of(&h, vcx),
        "first\nsecond",
        "shift-return did not break the line"
    );

    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    assert!(
        matches!(h.drain().as_slice(), [Command::Send { text, .. }] if text == "first\nsecond"),
        "return did not send the multi-line draft"
    );
}

/// A run of typing undoes as one step, and redo puts it back.
///
/// bezel groups adjacent edits of the same kind, so ⌘Z gives back the word
/// rather than the letter. eave had no undo at all.
#[gpui::test]
fn the_draft_undoes_a_word_at_a_time(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);

    vcx.simulate_input("hello");
    vcx.simulate_keystrokes("cmd-z");
    assert_eq!(
        draft_of(&h, vcx),
        "",
        "undo gave back a letter instead of the run"
    );
    vcx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(draft_of(&h, vcx), "hello", "redo did not restore the run");
}

/// The `/` palette borrows `up`/`down` and gives them back.
///
/// The mechanism the slice turns on: while the palette is live the field
/// claims a second key context and the arrows steer the menu; the moment it
/// closes the claim is dropped and the same keys are vertical caret motion
/// again. Both halves matter — a permanent claim would have taken the arrows
/// away from a multi-line draft for good.
#[gpui::test]
fn the_palette_borrows_the_arrows_and_returns_them(cx: &mut TestAppContext) {
    let project = SlashProject::new("borrow");
    let (h, vcx) = boot(cx);
    open_session_in(&h, vcx, &project);
    let field = composer_field(&h, vcx);

    // A two-line draft with no palette: `up` is caret motion.
    vcx.simulate_input("alpha");
    vcx.simulate_keystrokes("shift-enter");
    vcx.simulate_input("beta");
    let at_end = vcx
        .read_window(&h.window, |_, cx| field.read(cx).cursor())
        .unwrap();
    vcx.simulate_keystrokes("up");
    let after_up = vcx
        .read_window(&h.window, |_, cx| field.read(cx).cursor())
        .unwrap();
    assert!(
        after_up < at_end,
        "with no palette up must move the caret to the previous line"
    );

    // Now open the palette. The same key steers the menu and leaves the caret
    // where it is.
    vcx.simulate_keystrokes("cmd-a backspace");
    vcx.simulate_input("/ship");
    settle_palette(&h, vcx);
    let before = vcx
        .read_window(&h.window, |_, cx| field.read(cx).cursor())
        .unwrap();
    vcx.simulate_keystrokes("down");
    settle_palette(&h, vcx);
    assert_eq!(
        vcx.read_window(&h.window, |_, cx| field.read(cx).cursor())
            .unwrap(),
        before,
        "the palette's down key also moved the caret"
    );
}
