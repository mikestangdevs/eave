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
    // **Slice CV1 — the fixture states the layout it measures.** The
    // product's default is the CANVAS (`LayoutMode::default`), and almost
    // every test in this file was written against the FOCUS well: it
    // measures a transcript, a composer, a tool row, a menu — things the
    // well holds and the plane hands to a card. Pinning the fixture to
    // Focus is one stated assumption in one place, instead of 166 silent
    // ones spread through the file, and it is the same move
    // `set_reduce_motion` above makes for the same reason. The tests that
    // are about a layout name theirs: `enter_columns` for the strip,
    // `enter_canvas` for the plane, and the default itself is pinned by
    // `a_fresh_window_opens_on_the_plane`.
    window
        .update(cx, |shell, _, _| {
            shell.layout = crate::columns::LayoutMode::Focus;
        })
        .unwrap();
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
        effort: None,
        fast: false,
        cwd: "/tmp/eave-harness".into(),
        created_ts_ms: 1,
        last_activity_ms: None,
        state: "idle".into(),
        live: false,
        epoch: 1,
        last_seq: 0,
        checkout_root: None,
        project_root: None,
        shared_with: Vec::new(),
        worktree: None,
        diff_stat: None,
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

/// Slice NC1 — the idle floor's window: nothing selected AND no field.
/// The launcher hosts a focused composer now, and a focused field costs
/// its caret (`a_focused_field_puts_the_caret_on_the_clock_at_the_launcher_too`),
/// so the window with NO timer at rest is one whose well a page has
/// taken. Drives the tab's entrance out first — a settling glide is a
/// timer, not a lease at rest.
fn take_the_well_with_a_page(h: &Harness, vcx: &mut VisualTestContext) {
    let mut visited = page_tab("visited", Some("https://docs.rs/"), Some("docs.rs"));
    visited.history_len = 1;
    strip_tabs(h, vcx, vec![visited], Some("visited"));
    vcx.run_until_parked();
    drive(h, vcx, Duration::from_millis(600));
    assert!(!shown(vcx, "send"), "the page's well has a composer in it");
}

/// Slice NC1 — what `Start` used to be: type into the launcher's composer
/// and press Return. The send IS the start.
fn start_chat(vcx: &mut VisualTestContext, text: &str) {
    vcx.simulate_input(text);
    vcx.run_until_parked();
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
}

/// Sessions are the default rail face. Tests that look at the files list
/// have to open it — the footer library icon is the door.
fn open_files_face(vcx: &mut VisualTestContext) {
    if shown(vcx, "files-section") {
        return;
    }
    click(vcx, "footer-library");
    vcx.run_until_parked();
}

/// Downloads / Memory / Settings still live in the library menu, now
/// opened from the files face header rather than the footer icon.
fn open_library_menu(vcx: &mut VisualTestContext) {
    open_files_face(vcx);
    click(vcx, "files-library");
    vcx.run_until_parked();
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

    // Now through the shell. The facts render as disabled rows in the
    // footer's library menu (operator directive 2026-08-25 replaced the
    // rail's resource lines with the icon footer) — the footer is standing
    // chrome, the Memory section appears only when something reports.
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(vec![dead.clone(), unprobed.clone()]);
    vcx.run_until_parked();
    assert!(
        shown(vcx, "rail-footer"),
        "the footer is standing chrome at the rail's foot"
    );
    open_library_menu(vcx);
    assert!(
        shown(vcx, "tab-menu-open-settings"),
        "the library menu opens, and Settings is in it"
    );
    assert!(
        !shown(vcx, "tab-menu-memory-fact"),
        "nothing live reports memory — the menu must state nothing rather \
         than a zero"
    );

    // The menu's entries are re-derived per render, so a fleet arriving
    // while it is open appears without a reopen.
    h.plumbing.push_rail(all.clone());
    vcx.run_until_parked();
    assert!(
        shown(vcx, "tab-menu-memory-fact"),
        "the aggregate fact renders once a live agent reports"
    );

    // The fact must fit the room it renders in, at REAL metrics (Stage J
    // amendment 1). The room is now a menu row's: the surface caps at
    // MENU_MAX_W and a row loses its inset and padding on both sides —
    // a ceiling that truncates against the menu edge is stated half way,
    // which is the same failure the rail check existed to catch.
    let room = px(theme::MENU_MAX_W
        - 2. * (theme::MENU_ROW_INSET + theme::MENU_ROW_PAD_X));
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
        let w = real_text_width(&line, ".SystemUIFont", theme::DETAIL_SIZE);
        assert!(
            w <= room,
            "the resource fact {line:?} needs {w:?} but a menu row gives \
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
    // Slice AG2 #11 gave every row a diffstat, and this row carries the
    // widest one. It is set HERE, on the test that measures line three,
    // precisely because the answer was to keep it OFF line three: a stat
    // that ever drifted back onto this line would take the folder from a
    // passing width to 25 px, and this assertion is what would say so.
    crowded.diff_stat = Some(DiffStat {
        adds: 999_999,
        dels: 999_999,
    });
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
                diff: None,
            },
        },
    }
}

/// Slice AG1 #14. An ACP agent's plan, naming the tool call it belongs to.
fn plan_update(
    seq: u64,
    id: &str,
    items: Vec<eave_core::tool::TodoItem>,
) -> eave_core::journal::Record {
    eave_core::journal::Record {
        seq,
        ts_ms: seq * 10,
        event: eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::PlanUpdate {
                call_id: id.into(),
                items,
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
/// Slice AG1 #4, end to end — an opened tool row draws the change, and
/// does NOT also draw the whole new file beneath it.
///
/// The duplication is the failure worth pinning: for ACP the result string
/// IS `newText`, so a row that drew both would show the change and then
/// repeat the entire file under it.
#[gpui::test]
fn an_opened_edit_row_draws_its_diff_instead_of_the_whole_new_file(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    let finished = eave_core::journal::Record {
        seq: 2,
        ts_ms: 20,
        event: eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::ToolUseFinished {
                id: "t1".into(),
                result: "one\nTWO\n".into(),
                is_error: false,
                diff: Some(eave_core::events::ToolDiff {
                    path: "a.txt".into(),
                    old_text: "one\ntwo\n".into(),
                    new_text: "one\nTWO\n".into(),
                }),
            },
        },
    };
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![
            tool_started_typed(
                1,
                "t1",
                "Write",
                r#"{"file_path":"a.txt"}"#,
                Some(eave_core::tool::ToolCall::Write {
                    path: "a.txt".into(),
                }),
            ),
            finished,
        ],
    });
    vcx.run_until_parked();

    // Collapsed rows stay cheap: no diff until it is opened.
    assert!(!shown(vcx, "tool-diff"), "a collapsed row drew a diff");
    click(vcx, "tool-head");
    vcx.run_until_parked();

    assert!(shown(vcx, "tool-diff"), "an opened edit row drew no diff");
    assert!(
        vcx.debug_bounds("tool-part-t1-input").is_some(),
        "the opened row lost its input, so this test is not measuring a real row"
    );
    assert!(
        vcx.debug_bounds("tool-part-t1-result").is_none(),
        "the row drew the diff AND repeated the whole new file under it"
    );
}

/// Slice AG1 #4, second half: a Claude `Edit` draws a diff on the surface,
/// from an event stream carrying NO provider diff block.
///
/// The ACP half shipped first and Claude Code — the provider eave was built
/// on — was the one left out, because it reports an edit through the tool's
/// arguments rather than through a diff block in the result. This is the
/// whole slice measured end to end: nothing but `old_string` / `new_string`
/// on the start, and a diff pane on the screen.
#[gpui::test]
fn a_claude_edit_row_draws_a_diff_with_no_provider_diff_block(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    let finished = eave_core::journal::Record {
        seq: 2,
        ts_ms: 20,
        event: eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::ToolUseFinished {
                id: "t1".into(),
                // Claude's own reply to an Edit: prose about the change,
                // not the file. There is no diff block anywhere here.
                result: "The file src/a.rs has been updated.".into(),
                is_error: false,
                diff: None,
            },
        },
    };
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![
            tool_started_typed(
                1,
                "t1",
                "Edit",
                r#"{"file_path":"src/a.rs","old_string":"one\ntwo\n","new_string":"one\nTWO\n"}"#,
                Some(eave_core::tool::ToolCall::Edit {
                    path: "src/a.rs".into(),
                }),
            ),
            finished,
        ],
    });
    vcx.run_until_parked();
    click(vcx, "tool-head");
    vcx.run_until_parked();

    assert!(
        shown(vcx, "tool-diff"),
        "a Claude edit drew no diff, so #4 is still ACP-only"
    );
    // The existing rule holds for a derived diff too: a drawn diff
    // replaces the result plate. For ACP that avoids repeating the whole
    // new file; here it drops a one-line confirmation that the diff
    // already states more precisely.
    assert!(
        vcx.debug_bounds("tool-part-t1-result").is_none(),
        "a derived diff left the result plate beside it"
    );
}

/// A FAILED edit keeps its error text and draws no diff — a picture of a
/// change that never landed would be a lie about the worktree.
#[gpui::test]
fn a_failed_edit_row_draws_its_error_not_a_diff(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    let finished = eave_core::journal::Record {
        seq: 2,
        ts_ms: 20,
        event: eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::ToolUseFinished {
                id: "t1".into(),
                result: "permission denied".into(),
                is_error: true,
                diff: Some(eave_core::events::ToolDiff {
                    path: "a.txt".into(),
                    old_text: "one\n".into(),
                    new_text: "two\n".into(),
                }),
            },
        },
    };
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![
            tool_started_typed(1, "t1", "Write", r#"{"file_path":"a.txt"}"#, None),
            finished,
        ],
    });
    vcx.run_until_parked();
    click(vcx, "tool-head");
    vcx.run_until_parked();

    assert!(
        !shown(vcx, "tool-diff"),
        "a failed edit drew a diff of a change that never landed"
    );
    assert!(
        vcx.debug_bounds("tool-part-t1-error").is_some(),
        "a failed edit lost its error text"
    );
}

/// Slice AG1 #4 — the diff logic itself, at the boundaries that matter.
#[test]
fn a_trimmed_hunk_shows_only_what_changed() {
    use eave_wire::frames::DiffOp;
    let old = "a\nb\nc\nd\ne\n";
    let new = "a\nb\nX\nd\ne\n";
    let (lines, more) = Shell::diff_lines(old, new, 1, 40);
    assert_eq!(more, 0);
    let ops: Vec<DiffOp> = lines.iter().map(|l| l.op).collect();
    // one context, the removal, the addition, one context — the shared
    // head and tail are dropped, which is the whole point.
    assert_eq!(
        ops,
        vec![DiffOp::Ctx, DiffOp::Del, DiffOp::Add, DiffOp::Ctx],
        "{lines:?}"
    );
    assert_eq!(lines[1].text, "c");
    assert_eq!(lines[2].text, "X");
}

/// An identical before and after is NO diff, not an empty one — the row
/// must fall back to its result rather than draw a hunk with no lines.
#[test]
fn an_unchanged_file_produces_no_hunk() {
    let (lines, _) = Shell::diff_lines("same\n", "same\n", 2, 40);
    assert!(lines.is_empty(), "{lines:?}");
}

/// A creation has no before-text, and that must read as a pure addition
/// rather than as "no diff" — this is the shape the real Grok capture has.
#[test]
fn a_creation_is_all_additions() {
    use eave_wire::frames::DiffOp;
    let (lines, _) = Shell::diff_lines("", "hi\nthere\n", 2, 40);
    assert!(!lines.is_empty(), "a creation produced no diff at all");
    assert!(
        lines.iter().all(|l| l.op == DiffOp::Add),
        "a creation drew something other than additions: {lines:?}"
    );
}

/// The cap states its remainder rather than trailing off.
#[test]
fn a_long_diff_states_how_much_it_did_not_draw() {
    let new: String = (0..100).map(|i| format!("line {i}\n")).collect();
    let (lines, more) = Shell::diff_lines("", &new, 2, 10);
    assert_eq!(lines.len(), 10);
    assert_eq!(more, 90, "the remainder was miscounted");
}

/// Slice AG1 #16 — a tool row that CHANGED a file offers a jump to it,
/// and the jump opens the files face rather than acting behind a shut rail.
#[gpui::test]
fn a_tool_row_that_changed_a_file_jumps_to_its_diff(cx: &mut TestAppContext) {
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
            "t1",
            "Write",
            r#"{"file_path":"src/lib.rs"}"#,
            Some(eave_core::tool::ToolCall::Write {
                path: "src/lib.rs".into(),
            }),
        )],
    });
    vcx.run_until_parked();

    // The files face starts shut, which is the state that made the naive
    // version of this feature look dead.
    assert!(!shown(vcx, "files-section"), "the files face started open");
    assert!(shown(vcx, "tool-jump-t1"), "a Write row offers no jump");

    click(vcx, "tool-jump-t1");
    vcx.run_until_parked();

    assert!(
        shown(vcx, "files-section"),
        "the jump left the files face shut, so the click did nothing visible"
    );
}

/// ...and a tool that only READ a file does not, because a jump lands on a
/// diff pane and an unchanged file has no diff to land on.
#[gpui::test]
fn a_read_only_tool_row_offers_no_jump(cx: &mut TestAppContext) {
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
            "t1",
            "Read",
            r#"{"file_path":"src/lib.rs"}"#,
            Some(eave_core::tool::ToolCall::Read {
                path: "src/lib.rs".into(),
            }),
        )],
    });
    vcx.run_until_parked();
    assert!(shown(vcx, "tool-row"), "no tool row at all");
    assert!(
        !shown(vcx, "tool-jump-t1"),
        "a Read row offered a jump to a diff that cannot exist"
    );
}

/// Slice AG1 #5 — the per-message copy takes the WHOLE message.
///
/// The distinction that matters: an assistant message renders as several
/// BLOCKS, and the control is mounted on block 0. A copy that handed back
/// only the block under the pointer would be the quiet failure the fence
/// copy already refuses, so this message is deliberately multi-paragraph
/// AND carries a fence, which is its own block.
#[gpui::test]
fn per_message_copy_takes_the_whole_message_not_one_block(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    let whole = "first paragraph\n\n```rust\nlet x = 1;\n```\n\nlast paragraph";
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![assistant_record(1, whole)],
    });
    vcx.run_until_parked();

    // The selector is per-message, so it names the ROW the message opens
    // at — row 0 here, because a reset session's first record is this
    // message. (`assistant_record(1, ..)`'s 1 is a seq, not a row index.)
    let sel = "msg-copy-0";
    assert!(shown(vcx, sel), "an assistant message has no copy control");
    click(vcx, sel);
    vcx.run_until_parked();

    let got = vcx
        .read_from_clipboard()
        .and_then(|i| i.text())
        .expect("the clipboard holds the message");
    assert_eq!(got, whole, "copy must be the whole message, blocks and all");
    assert!(
        got.contains("first paragraph") && got.contains("last paragraph"),
        "a block-scoped copy would have dropped one end: {got:?}"
    );
}

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

    // Slice D4 deleted the caps header this used to anchor on, and 0a945ef
    // then moved the add action off its ghost row and onto the zone header
    // as a right-aligned `+`. So `new-session-glyph` is no longer anything
    // to do with the left edge — it measured 177 against the footer's 12,
    // which is the `+` sitting where it belongs, not drift.
    //
    // The rule survives unchanged: everything at RAIL LEVEL starts on one
    // gutter. It is anchored on the zone header's ROW BOX, which is a direct
    // child of the rail exactly as the footer and a session row card are.
    // Deliberately not `zone-sessions-label` — the label is inset a further
    // RAIL_HOVER_PAD because the header is a hoverable card, and its chevron
    // is inset the same on the other side. That inset is the card's, not the
    // rail's, and measuring it here would assert the wrong column.
    //
    // This used to anchor against the resource lines (`rail-resource`),
    // the other text at rail level. They moved into the footer's library
    // menu (2026-08-25) and the footer is a PANEL, not a text line — it
    // wears its own SPACE_2 inset the way a session row's card wears its
    // overhang, so the one-gutter rule no longer has a second text column
    // at rail level to measure. The rule itself is still asserted below
    // against the files rail.
    // The zone header and a session row's card share their RIGHT edge — the
    // 4 px overhang, asserted. Both sit in the same scroller now, so this is
    // the same rule the header's control used to carry. The `+` cannot carry
    // it any more: the chevron is the header's trailing element and both are
    // inside the card's padding.
    assert_eq!(
        right(vcx, "zone-sessions"),
        right(vcx, "rail-aaaa1111"),
        "the row card and the zone header end on different edges"
    );

    open_files_face(vcx);

    // The files rail is the same rule, and had the same drift.
    assert_eq!(
        x(vcx, "files-title"),
        x(vcx, "files-mode"),
        "the files rail's header and mode line start on different edges"
    );
    // The files header's trailing slot is the library door. File rows
    // share that right edge.
    assert_eq!(
        right(vcx, "files-library"),
        right(vcx, "file-src/lib.rs"),
        "the file row and the files-face library door end on different edges"
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
            composer_hint(0, true, shell.read(cx).composer.read(cx).overflow, 0, false)
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

/// **The ↓-bottom pill needs a bottom to return to.**
///
/// Sending flips the scroll mode to `PinOnNextUserRow` and then to
/// `Pinned { msg }`, which asks the list to put the sent row at the top of
/// the viewport so the answer streams below it. On the first turn of a new
/// session there is nothing below to scroll into view: the list walks its
/// scroll top back to the first row and the pin does nothing. The mode is
/// still Pinned, though, and `scroll != Follow` used to be the whole test
/// for the affordance — so a two-row log wore a control offering to return
/// the reader to a bottom they had never left.
///
/// The state is set here rather than typed into the composer on purpose:
/// what is under test is the affordance's condition, not the send path. A
/// test that sent a message would pass through four other slices to reach
/// the same field, and would name one of them when it broke.
#[gpui::test]
fn the_bottom_pill_waits_for_a_log_taller_than_the_well(cx: &mut TestAppContext) {
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
        records: vec![user_record(1, "heyyy")],
    });
    vcx.run_until_parked();

    // The pill enters on `motion::DIALOG_IN`, so it paints at opacity 0 on
    // the frame it mounts. `run_until_parked` alone would therefore find no
    // pill in EITHER half of this test, and the absence assertion below
    // would pass without testing anything. Driving the clock is what makes
    // the negative honest.
    let pin = |vcx: &mut VisualTestContext, h: &Harness| {
        h.window
            .update(vcx, |shell, _, cx| {
                shell
                    .transcript
                    .update(cx, |t, _| t.scroll = ScrollMode::Pinned { msg: 0 });
            })
            .unwrap();
        vcx.run_until_parked();
        // TWO frames, and the second is the point. `render` reads the
        // scroll extent the LAST layout measured, so the frame that first
        // lays out taller content still asks the previous frame whether
        // there is anything to scroll. `refresh` forces the frame that
        // sees it. The pill also enters on `motion::DIALOG_IN`, so it
        // paints at opacity 0 on the frame it mounts and `drive` is what
        // brings it up — without both of these the absence assertion below
        // would pass while testing nothing.
        vcx.update(|window, _| window.refresh());
        vcx.run_until_parked();
        drive(h, vcx, Duration::from_millis(16));
    };

    pin(vcx, &h);
    assert!(
        !shown(vcx, "to-bottom"),
        "one row in an empty well, and the transcript is already offering \
         to scroll the reader back to a bottom they are sitting on"
    );

    // Now give it more transcript than well. The turn is deliberately left
    // OPEN — no `TurnCompleted` — because a settled turn folds (Slice AT1)
    // and a fold would take the height back out from under this assertion.
    let tall = (0..400)
        .map(|i| format!("line {i} of an answer that does not fit"))
        .collect::<Vec<_>>()
        .join("\n");
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![assistant_record(2, &tall)],
    });
    vcx.run_until_parked();

    pin(vcx, &h);
    assert!(
        shown(vcx, "to-bottom"),
        "the log is taller than the well and the pin has moved the view off \
         the bottom, and there is no way back"
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
fn the_launcher_is_a_composer_and_return_starts_the_chat(cx: &mut TestAppContext) {
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

    // Slice NC1: the launcher hosts the composer. Q2 unmounted it here
    // and gated the keyboard so nothing typed could surface later in the
    // first session opened; both are now the other way round, on
    // purpose — the words typed at the launcher are the chat's opening.
    assert!(shown(vcx, "launcher"));
    assert!(shown(vcx, "composer-hint"), "the launcher has no composer");
    assert!(
        shown(vcx, "composer-model"),
        "the launcher has no agent chip"
    );

    let _ = h.drain();
    vcx.simulate_input("typed at the launcher");
    vcx.run_until_parked();
    assert_eq!(
        draft(vcx, &h),
        "typed at the launcher",
        "typing did not reach the field"
    );
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    assert_eq!(draft(vcx, &h), "", "the send did not clear the field");
    let cmds = h.drain();
    assert_eq!(cmds.len(), 1, "return emitted {cmds:?}");
    match &cmds[0] {
        Command::Launch { prompt, .. } => {
            assert_eq!(
                prompt.as_ref().map(|(t, _)| t.as_str()),
                Some("typed at the launcher"),
                "the launch did not carry the words that started it"
            );
        }
        other => panic!("return emitted {other:?}, not a launch"),
    }
    assert!(
        !cmds.iter().any(|c| matches!(c, Command::Send { .. })),
        "a bare Send went out with no session to address — the rest loop drops it"
    );

    // An empty Return starts nothing: there is nothing to say.
    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    assert!(h.drain().is_empty(), "an empty Return launched a chat");
}

/// Behind a blank ⌘T tab the transcript may still be subscribed to the
/// chat you left. The send must deselect it first, because the session
/// loop drops `Launch` — and the words must never land in that chat.
#[gpui::test]
fn a_send_on_a_blank_tab_leaves_the_old_chat_before_it_launches(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(
        &h,
        vcx,
        vec![session_tab("aaaa1111"), page_tab("blank", None, None)],
        Some("blank"),
    );
    assert!(shown(vcx, "launcher"), "a blank tab is the launcher");
    assert!(shown(vcx, "composer-hint"), "the launcher has no composer");

    let _ = h.drain();
    start_chat(vcx, "a new chat");
    let cmds = h.drain();
    let launch = cmds
        .iter()
        .position(|c| matches!(c, Command::Launch { .. }))
        .expect("no launch went out");
    let leave = cmds
        .iter()
        .position(|c| matches!(c, Command::NewSession))
        .expect("the old chat was never deselected — the session loop drops Launch");
    assert!(
        leave < launch,
        "the launch went out before the deselect: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| matches!(c, Command::Send { .. })),
        "the words went to the chat you left: {cmds:?}"
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
    open_files_face(vcx);

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
    open_files_face(vcx);

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

/// **The send button is never pushed out of the composer.** Reported from
/// the running app, on the exact state that produced it: a question open,
/// so the composer's trailing slot is carrying Slice K's warning rather
/// than nothing.
///
/// The slot is a text div in a flex row, and a text div's default minimum
/// width is its own content — so it declined to shrink, the row
/// overflowed, and the card's `overflow_hidden` clipped the send and stop
/// buttons off the right edge. The affordance the row exists for stopped
/// being clickable at exactly the moment the operator needed it.
///
/// Asserted as CONTAINMENT rather than as "the hint truncates": what
/// matters is not how the words degrade, it is that the button is still
/// inside the card with its whole hit target. A future slice may reword
/// the warning freely; it may not put it in front of the button.
#[gpui::test]
fn the_warning_never_pushes_the_send_button_out_of_the_card(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    ask(
        &h,
        vcx,
        vec![
            user_record(1, "go"),
            approval_record(2, "req-1", "Write", "/repo/a.txt"),
        ],
    );
    // **At the narrowest window eave will open.** The defect is a width
    // defect, and a test that only ever asks it at the harness's default
    // size passes without asking anything — this one did, until it was
    // pointed at the size where the row actually runs out of room.
    vcx.simulate_resize(size(px(theme::WINDOW_MIN_W), px(theme::WINDOW_MIN_H)));
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(10));

    let card = vcx.debug_bounds("composer-card").expect("the composer card");
    let send = vcx.debug_bounds("send").expect("the send button");
    assert!(
        send.origin.x >= card.origin.x
            && send.origin.x + send.size.width <= card.origin.x + card.size.width,
        "the send button sits at {:?}..{:?} and the card ends at {:?} — the \
         warning pushed it through `overflow_hidden` and out of the app",
        send.origin.x,
        send.origin.x + send.size.width,
        card.origin.x + card.size.width
    );
    assert_eq!(
        send.size.width,
        px(composer::TOOL),
        "the send button was squeezed to {:?} instead of the row yielding \
         somewhere else — a shrunken hit target is the same defect wearing \
         a smaller hat",
        send.size.width
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
/// never started a session" and "the rail failed to load" the same picture.
/// A `rail-empty` note answered that.
///
/// **02c07d8 deleted the note deliberately** and moved the answer onto the
/// zone header: an empty rail is not bare glass because `Sessions` stands
/// there, labelled, with the `+` that fills it. `rail_empty_note` is kept in
/// `ui.rs` under `#[cfg(test)]`, and the comment above it says why — read
/// that before restoring a call site, which is what I did and had to undo.
///
/// What no longer holds is the ORIGINAL distinction: a first run and a
/// failed load both draw `Sessions` over nothing. That is a live gap in the
/// product, not something this test can assert, and it is written down in
/// the slice report rather than left implied by a red test.
#[gpui::test]
fn an_empty_rail_is_not_bare_glass(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(Vec::new());
    vcx.run_until_parked();

    assert!(
        shown(vcx, "zone-sessions-label"),
        "an empty rail is bare glass — the zone that names it is not drawn"
    );
    // The affordance that answers it, which is why nothing restates it.
    assert!(shown(vcx, "new-session"));

    // ...and it sits on the rail's one gutter. This was
    // `the_empty_state_starts_on_the_gutter`, whose subject — the note —
    // 02c07d8 deleted. The rule it carried is specifically an EMPTY rail's,
    // so it lives here rather than in `a_rail_has_one_left_edge`, whose
    // fixture has rows in it.
    let x = |vcx: &mut VisualTestContext, sel: &'static str| {
        vcx.debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} did not render"))
            .origin
            .x
    };
    assert_eq!(
        x(vcx, "zone-sessions"),
        x(vcx, "zone-folders"),
        "an empty rail's zone headers start on different edges"
    );

    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();
    assert!(
        shown(vcx, "rail-aaaa1111") && shown(vcx, "zone-sessions-label"),
        "a real session appears UNDER the zone header, it does not replace it"
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

/// Where a zone's add control is minted for `selector`, if it is one.
///
/// Slice D4 made the three zone actions ghost rows built by one `ghost_row`
/// helper, and this asked `ghost_row` about them. 0a945ef then moved those
/// actions onto the zone HEADER as a right-aligned `+` built by
/// `zone_toggle`, and `ghost_row` left `ui.rs` entirely — so the exemption
/// matched nothing, `new-session` fell through to the literal check it can
/// never satisfy, and the failure read "new-session is no longer tagged"
/// when it is tagged perfectly well, just generically.
///
/// Membership is proved the way it always was, by construction rather than
/// by proximity: the selector is a literal inside `zone_add_id`, which is
/// the only thing in the tree that mints one.
fn zone_add_call(src: &str, selector: &str) -> Option<usize> {
    let at = src.find("fn zone_add_id(")?;
    let body = &src[at..];
    let end = body.find("\n}\n")?;
    body[..end]
        .contains(&format!("\"{selector}\""))
        .then_some(at)
}

/// The body of a method on `Shell`, so a shared constructor can be checked
/// once for the shape every call site inherits.
fn shell_method_body<'a>(src: &'a str, sig: &str) -> &'a str {
    let at = src
        .find(sig)
        .unwrap_or_else(|| panic!("{sig} is gone from ui.rs"));
    let body = &src[at..];
    &body[..body
        .find("\n    }\n")
        .unwrap_or_else(|| panic!("{sig} has no end"))]
}

/// Every clickable element states that it is clickable, and answers the
/// pointer.
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
    // Slice AG1 #7: the follow-up queue's three per-row controls differ only
    // by glyph and handler, so they share a constructor exactly the way the
    // three above do, and are accounted for the same way.
    let queue_control_body = {
        let from = &src[src.find("fn queue_control(").unwrap()..];
        &from[..from.find("\n}\n").unwrap()]
    };
    let cursors_in_queue_control = queue_control_body.matches(".cursor_pointer()").count()
        + queue_control_body.matches(".cursor_text()").count();
    let via_queue_control = src.matches("queue_control(").count() - 1;
    assert_eq!(
        clicks,
        cursors
            - cursors_in_helper
            - cursors_in_model_chip
            - cursors_in_attach_button
            - cursors_in_queue_control
            + via_helper
            + via_model_chip
            + via_attach_button
            + via_queue_control,
        "every .on_click site needs a .cursor_pointer or .cursor_text, \
         directly or through row_action / model_chip / attach_button / \
         queue_control — eave had 84 fewer cursor calls than comet and every \
         one of them was the default arrow"
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
        // Slice D4 — the zone actions are built by ONE helper, so their
        // selector and their fade are both written generically
        // (`debug_selector(move || add_id.into())`, `hover_fade(add_id, ..)`).
        // A source-text search for the literal therefore cannot find them.
        //
        // The guarantee is not weakened, it moved: the helper is checked
        // once for the shape every zone action inherits, and the action is
        // checked for being minted by it. That is strictly stronger than
        // three literal matches, because it cannot drift per-zone.
        //
        // The wash is checked here rather than from the table above: a `+`
        // on a header is not a rail row, and it fades from transparent to
        // `overlay_strong` rather than from `wash_rest`. Naming its real
        // pair is the check; carrying the row's pair would be a lie that
        // happened to pass.
        if let Some(call) = zone_add_call(src, selector) {
            let helper = shell_method_body(src, "fn zone_toggle(");
            assert!(
                helper.contains(".debug_selector(move || add_id.into())"),
                "zone_toggle lost the debug_selector every zone action \
                 inherits (checked because {selector} is minted at {call})"
            );
            assert!(
                helper.contains(".cursor_pointer()"),
                "zone_toggle's add control stopped stating its cursor \
                 (checked because {selector} is minted at {call})"
            );
            // Whitespace-squeezed before matching, for the reason this
            // test already learned twice: rustfmt puts these three
            // arguments on one line or on four depending on how long the
            // surrounding chain is, and a fixed spelling of them together
            // is a match on formatting, not on behaviour.
            let squeezed: String = helper.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(
                squeezed.contains(
                    "hover_fade(add_id,gpui::transparent_black(),theme::overlay_strong(),)"
                ),
                "zone_toggle's add control lost the hover fade keyed on its \
                 own id (checked because {selector} is minted at {call})"
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
fn a_focused_field_puts_the_caret_on_the_clock_at_the_launcher_too(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    drive(&h, vcx, Duration::from_millis(10));
    vcx.executor().advance_clock(Duration::from_secs(1));
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(10));
    // Slice NC1: the launcher has a field now, and a focused field blinks.
    assert!(
        caret_only(vcx),
        "the launcher's composer is focused and its caret is not on the clock"
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
    // Slice NC1: the launcher hosts a focused composer now, whose caret
    // is on the clock like any field's — so the window with NO field is
    // one whose well a page has taken.
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    take_the_well_with_a_page(&h, vcx);
    assert!(
        !clock_running(vcx),
        "an idle shell leased the pulse clock (tick {:?})",
        vcx.update(|_, cx| motion::clock_tick(cx))
    );

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
    // Slice NC1: the floor is measured with no field on screen.
    take_the_well_with_a_page(&h, vcx);
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
    // Slice NC1: the floor is measured with no field on screen.
    take_the_well_with_a_page(&h, vcx);
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
    // Slice NC1: the floor is measured with no field on screen.
    take_the_well_with_a_page(&h, vcx);
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
    open_files_face(vcx);
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
    open_files_face(vcx);
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
    open_files_face(vcx);
    for (selector, expected) in [
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
    open_files_face(vcx);

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
        "bookmark-tile-rs",
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
    // Two pinned bookmarks, so the grid at the top of the rail holds a
    // tile in its SECOND column — the first sits left of the menu — that
    // SENDS when clicked. The empty zone's hint card does not (its close
    // control only flips a flag), and it was the `+` cell, gone since the
    // card, that used to sit under the menu.
    push_bookmarks(
        &h,
        vcx,
        &[
            ("gh", 0, None, Some("https://github.com/")),
            ("rs", 0, None, Some("https://docs.rs/")),
        ],
    );
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
    // Slice NC1: the launcher's composer would already hold the caret's
    // lease when the menu opens, and a lease taken before the menu is
    // not what this measures — measure on a well with no field.
    take_the_well_with_a_page(&h, vcx);
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
    // Slice NC1: the launcher is the new-chat cluster — the disc, the
    // composer with its agent chip, and where the chat will run. No
    // title, no tile grid, no Start button.
    assert!(shown(vcx, "empty-logo"));
    assert!(shown(vcx, "composer-hint"));
    assert!(shown(vcx, "composer-model"));
    assert!(shown(vcx, "send"));
    assert!(shown(vcx, "launcher-cwd"));
    for gone in [
        "launcher-title",
        "provider-strip",
        "launcher-start",
        "provider-tile-0",
    ] {
        assert!(!shown(vcx, gone), "{gone} is Q2's launcher, not a chat");
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
/// Start emits the command, carrying the chosen provider and — by default
/// — **no model at all**, which is not the same as an empty one.
#[gpui::test]
fn start_emits_the_launch_with_the_chosen_agent(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let _ = h.drain();
    start_chat(vcx, "hello");
    let cmds = h.drain();
    assert_eq!(cmds.len(), 1, "start emitted {} commands", cmds.len());
    match &cmds[0] {
        Command::Launch {
            provider,
            model,
            cwd,
            extra_args,
            prompt,
            tuning: _,
        } => {
            assert_eq!(prompt.as_ref().map(|(t, _)| t.as_str()), Some("hello"));
            assert_eq!(provider, "claude-code");
            assert_eq!(
                model.as_deref(),
                Some("claude-sonnet-5"),
                "start must send the model the launcher already shows"
            );
            assert_eq!(*cwd, None, "eave invented a worktree nobody chose");
            assert!(
                extra_args.is_empty(),
                "eave invented flags nobody configured: {extra_args:?}"
            );
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
    click(vcx, "composer-model");
    vcx.run_until_parked();
    assert!(shown(vcx, "model-picker"), "the model menu never opened");

    // Row 1 is Opus 5 — row 0 is the preselected Sonnet 5.
    click(vcx, "model-picker-row-1");
    vcx.run_until_parked();
    // The exit has to PLAY before the next click can land. P1's surface
    // stays mounted and occluding for the whole 100 ms exit, so a click on
    // "start" during it hits the catcher and does nothing — which is the
    // designed behaviour, and which cost this test a run to remember.
    drive(&h, vcx, Duration::from_millis(16));
    let chosen = vcx
        .read_window(&h.window, |shell, cx| shell.read(cx).launcher.model)
        .unwrap();
    assert_eq!(chosen, Some("claude-opus-5"));

    let _ = h.drain();
    start_chat(vcx, "hello");
    match &h.drain()[0] {
        Command::Launch { model, .. } => {
            assert_eq!(model.as_deref(), Some("claude-opus-5"))
        }
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
    click(vcx, "composer-model");
    vcx.run_until_parked();
    click(vcx, "model-picker-row-0");
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
/// Codex is tile 1 and is live, so a switch lands on its own preselection
/// rather than carrying Claude's id across — eave does not validate
/// models (P2 caveat 5), so a leftover flag would travel all the way to
/// a child process before anything complained.
#[gpui::test]
fn switching_provider_clears_a_model_it_would_not_understand(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    click(vcx, "composer-model");
    vcx.run_until_parked();
    click(vcx, "model-picker-row-1");
    vcx.run_until_parked();
    assert_eq!(
        vcx.read_window(&h.window, |shell, cx| shell.read(cx).launcher.model)
            .unwrap(),
        Some("claude-opus-5"),
    );

    h.window
        .update(vcx, |shell, _, cx| shell.pick_provider(1, cx))
        .unwrap();
    vcx.run_until_parked();
    assert_eq!(
        vcx.read_window(&h.window, |shell, cx| shell.read(cx).launcher.model)
            .unwrap(),
        Some("gpt-5.6-terra"),
        "Claude's model survived a switch onto Codex",
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
    // Slice NC1: the launcher has a focused composer, so what it costs at
    // rest is exactly what an empty session costs — the caret's tick,
    // and never the frame rate.
    vcx.executor().advance_clock(Duration::from_secs(1));
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(10));
    assert!(
        caret_only(vcx),
        "the launcher leased the pulse clock for something other than its caret",
    );
    assert!(
        !animating(vcx),
        "a settled launcher is driving the clock at the frame rate",
    );
}

/// Escape closes the model menu and leaves the launcher standing. The menu
/// owns the whole keyboard while it is open (P1), and the thing it must
/// not do is take the launcher down with it.
#[gpui::test]
fn escape_closes_the_model_menu_and_not_the_launcher(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    click(vcx, "composer-model");
    vcx.run_until_parked();
    assert!(shown(vcx, "model-picker"));
    vcx.simulate_keystrokes("escape");
    drive(&h, vcx, Duration::from_millis(16));
    assert!(
        !shown(vcx, "model-picker"),
        "escape did not dismiss the menu"
    );
    assert!(shown(vcx, "launcher"), "escape took the launcher with it");
}

/// MonoCode's picker: provider tabs, search, stars, two-line rows.
#[gpui::test]
fn the_model_picker_is_monocodes_card(cx: &mut TestAppContext) {
    let _turn = eave_core::capabilities::probe_test_lock();
    eave_core::capabilities::forget();
    let (h, vcx) = boot(cx);
    click(vcx, "composer-model");
    vcx.run_until_parked();
    for part in [
        "model-picker",
        "model-picker-tab-favorites",
        "model-picker-tab-claude-code",
        "model-picker-search",
        "model-picker-row-0",
        "model-picker-row-1",
        "model-picker-star-0",
    ] {
        assert!(shown(vcx, part), "{part} missing from the model picker");
    }
    click(vcx, "model-picker-star-0");
    vcx.run_until_parked();
    click(vcx, "model-picker-tab-favorites");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "model-picker-row-0"),
        "starring the first named model did not put it on the favorites tab"
    );
    vcx.read_window(&h.window, |shell, cx| {
        assert!(
            shell
                .read(cx)
                .model_picker
                .is_favorite("claude-code:claude-sonnet-5"),
            "star did not stick"
        );
    })
    .unwrap();
}

/// Typing in the open picker filters the list. Overlay owns the keyboard.
#[gpui::test]
fn the_model_picker_search_narrows_the_list(cx: &mut TestAppContext) {
    let _turn = eave_core::capabilities::probe_test_lock();
    eave_core::capabilities::forget();
    let (h, vcx) = boot(cx);
    click(vcx, "composer-model");
    vcx.run_until_parked();
    vcx.simulate_keystrokes("f a b l e");
    vcx.run_until_parked();
    vcx.read_window(&h.window, |shell, cx| {
        assert_eq!(
            shell.read(cx).model_picker.query,
            "fable",
            "typed search never reached the picker"
        );
    })
    .unwrap();
    assert!(shown(vcx, "model-picker-row-0"));
    assert!(
        !shown(vcx, "model-picker-row-1"),
        "search left extra rows on screen"
    );
}

/// **No session, no composer.** The rest loop drops a `Send` — holding it
/// to replay into whatever session opens next would be worse than losing
/// it — so an input box that silently eats what you type is the one thing
/// the launcher must not put on screen.
///
/// This is a defect the first build of Q2 shipped: the launcher rendered
/// over a live "Message eave…" field, and only a window capture showed it.
#[gpui::test]
fn the_launcher_shows_the_composer_to_type_into(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    assert!(shown(vcx, "launcher"));
    // Slice NC1 inverts Q2's rule: the composer is on the launcher, and
    // it is the SAME composer — one entity, one mount — so a draft typed
    // here is the draft the session opens with.
    assert!(
        shown(vcx, "composer-hint"),
        "the launcher offered no composer to type into",
    );
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert!(!shown(vcx, "launcher"));
    assert!(
        shown(vcx, "composer-hint"),
        "the composer did not stay with the session",
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
    open_files_face(vcx);
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
    open_files_face(vcx);
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
        session_name(&named_a, None).0,
        session_name(&named_b, None).0
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
    // What it draws instead of nothing (Slice ID2a: a stated placeholder,
    // no longer the provider's name).
    assert_eq!(
        session_name(&fresh, None),
        (NEW_SESSION_LABEL.to_string(), NameKind::Placeholder)
    );

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
        // The harness drives ONE window, so a snapshot with no `windows`
        // entry is exactly right: `strip_for_window` reads it as the
        // primary window's strip, which is the shape every assertion in
        // this file was written against (Slice W1).
        windows: Vec::new(),
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
        incognito: false,
        window: None,
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

/// A page tab Chromium has committed and is still fetching — B4's
/// precondition, and the state ⎋ is about.
fn loading_page_tab(id: &str) -> TabInfo {
    TabInfo {
        load: eave_wire::LoadState::Loading,
        ..page_tab(id, Some("https://slow.example/"), None)
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

// ── Slice AG1 #13 — a proposed title, applied by a human ──────────────

/// An unnamed session opens its rename field on the operator's own opening
/// ask, ready to accept with one key.
///
/// **The whole design is in what this test does NOT assert.** Nothing is
/// derived until the field opens, nothing leaves the app until Enter, and
/// the thing that leaves is the ordinary `Command::Rename` a typed name
/// sends — so the title reaching `watch` was published by a human, which
/// is the line `eave-engine/tests/watch.rs::the_engine_never_mines_a_title_out_of_the_transcript`
/// draws. eave proposes; only the operator applies.
#[gpui::test]
fn an_unnamed_session_proposes_its_opening_ask_and_enter_accepts(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![
            user_record(1, "rename the rail rows"),
            assistant_record(2, "on it"),
        ],
    });
    vcx.run_until_parked();

    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    click(vcx, "ren-aaaa1111");
    vcx.run_until_parked();
    assert_eq!(
        rename_buffer(vcx, &h).as_deref(),
        Some("rename the rail rows"),
        "an unnamed session opened its field empty"
    );

    let _ = h.drain();
    vcx.simulate_keystrokes("enter");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::Rename { id, title: Some(t) }]
            if id == "aaaa1111" && t == "rename the rail rows"),
        "accepting the proposal sent something else: {cmds:?}"
    );
}

/// A proposal is a suggestion, so the first keystroke replaces it whole —
/// the `seeded` behaviour the omnibox and a new bookmark folder already
/// use. And escape still means escape: a session eave proposed a name for
/// and the operator walked away from stays unnamed.
#[gpui::test]
fn a_proposal_is_typed_over_and_escaping_leaves_the_session_unnamed(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(1, "rename the rail rows")],
    });
    vcx.run_until_parked();

    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    click(vcx, "ren-aaaa1111");
    vcx.run_until_parked();
    vcx.simulate_input("x");
    assert_eq!(
        rename_buffer(vcx, &h).as_deref(),
        Some("x"),
        "typing appended to the proposal instead of replacing it"
    );

    let _ = h.drain();
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(
        h.drain().is_empty(),
        "escape published a name eave derived — the one thing it must \
         never do"
    );
}

/// A session that already has a name opens on THAT name, not on a
/// proposal.
///
/// The direction matters more than the convenience: a name the operator
/// chose is a decision, and overwriting it with one eave worked out from
/// the transcript would make the feature take something away.
#[gpui::test]
fn a_named_session_is_never_re_proposed(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut named = session("aaaa1111");
    named.title = Some("the old name".into());
    rail_with_selected(&h, vcx, vec![named], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(1, "rename the rail rows")],
    });
    vcx.run_until_parked();

    hover_row(&h, vcx, "rail-aaaa1111", "ren-aaaa1111");
    click(vcx, "ren-aaaa1111");
    vcx.run_until_parked();
    assert_eq!(
        rename_buffer(vcx, &h).as_deref(),
        Some("the old name"),
        "a proposal overwrote a name the operator had chosen"
    );
}

/// A session this client has never opened has nothing to propose, and
/// eave does not go and get one — while one it opened EARLIER still can be
/// named after you have moved on.
///
/// **Both halves are the boundary, not a gap.** Proposing for every row
/// would mean subscribing to every session's transcript, which is the
/// cross-session fan-out `watch` exists to avoid. Proposing for a session
/// this client legitimately had open is free, and it does not go stale:
/// the proposal is the FIRST thing that was asked, which no later turn
/// changes.
///
/// Driven through `begin_rename` directly because that is the tab-strip
/// menu's whole implementation of Rename (`TabMenuAction::Rename`), and
/// it is the only door to naming a session that is not the selected one —
/// the rail's pen appears on the selected row alone.
#[gpui::test]
fn a_proposal_outlives_the_switch_away_and_is_never_invented(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222")],
        "aaaa1111",
    );
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(1, "rename the rail rows")],
    });
    vcx.run_until_parked();

    // Move to the other session, which this client has never subscribed
    // to and holds not one row of.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "bbbb2222".into(),
    });
    vcx.run_until_parked();

    h.window
        .update(vcx, |shell, _, cx| {
            shell.begin_rename("bbbb2222".into(), cx);
        })
        .unwrap();
    vcx.run_until_parked();
    assert_eq!(
        rename_buffer(vcx, &h).as_deref(),
        Some(""),
        "a session this client never subscribed to came up with a name \
         from somewhere"
    );
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();

    h.window
        .update(vcx, |shell, _, cx| {
            shell.begin_rename("aaaa1111".into(), cx);
        })
        .unwrap();
    vcx.run_until_parked();
    assert_eq!(
        rename_buffer(vcx, &h).as_deref(),
        Some("rename the rail rows"),
        "the proposal did not survive switching away from the session it \
         came from"
    );
}

/// A proposal at the storage cap clips INSIDE the field and never grows
/// the row, and the caret stays visible past the clip.
///
/// **Measured here rather than walked, and that is a limit worth stating.**
/// The rail's pen is revealed by hover, and this machine's synthetic
/// pointer events do not drive gpui's hover — a `CGEventMouseMoved` at the
/// row leaves the affordance unrevealed, so a live window cannot reach this
/// field at all (`docs/reports/stage-ag1-13-title-proposal.md`). Geometry
/// is what `debug_bounds` genuinely can see, so geometry is what this
/// asserts, in the same shape as
/// `a_long_title_clips_itself_before_it_crowds_the_badge` above.
///
/// **Why the proposal is not cut to what the rail can DRAW.** The rail is
/// resizable, so there is no fixed character budget to cut to; a cap
/// derived from a width would change meaning when the operator drags the
/// seam. It is cut to a NAME's length (`session_title::DERIVED_MAX`, Slice
/// ID2a — under the store's cap, so accepting it changes nothing), which
/// is exactly what a typed title of the same length already does — the
/// field truncates, the row truncates, and the operator can trim it before
/// pressing Enter.
#[gpui::test]
fn a_proposal_at_the_cap_clips_inside_the_field(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    // Longer than the cap, in a font whose glyphs are wide, so the cut is
    // real and the clip is real.
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(
            1,
            &"M".repeat(eave_core::store::TITLE_MAX_CHARS * 2),
        )],
    });
    vcx.run_until_parked();

    h.window
        .update(vcx, |shell, _, cx| {
            shell.begin_rename("aaaa1111".into(), cx);
        })
        .unwrap();
    vcx.run_until_parked();

    let proposed = rename_buffer(vcx, &h).expect("the field is open");
    // Slice ID2a cut the proposal to `DERIVED_MAX` — a name's length,
    // under the store's cap — so the field, the row and the store all
    // hold the same string. The clip assertion below is what this test
    // is for, and a 48-character run of `M` still overflows the field.
    assert_eq!(
        proposed.chars().count(),
        crate::session_title::PROPOSAL_MAX,
        "a proposal must be cut to the derived length"
    );
    assert!(
        crate::session_title::PROPOSAL_MAX <= eave_core::store::TITLE_MAX_CHARS,
        "a proposal must never exceed what the store will keep"
    );

    let row = vcx.debug_bounds("rail-aaaa1111").expect("the row renders");
    let field = vcx
        .debug_bounds("rename-aaaa1111")
        .expect("the rename field renders");
    assert!(
        field.origin.x >= row.origin.x - px(1.)
            && field.origin.x + field.size.width <= row.origin.x + row.size.width + px(1.),
        "the field grew past its row: field {field:?} in row {row:?}"
    );

    // And it is really truncation being measured, not a happy fit.
    let wanted = real_text_width(&proposed, theme::SANS, theme::RAIL_NAME_SIZE);
    assert!(
        wanted > field.size.width,
        "a proposal at the cap ({wanted:?}) fits the field ({:?}) — then \
         nothing here is testing the clip",
        field.size.width
    );

    // The caret is the operator's place in the string, and the first thing
    // a truncating box must not clip. It is a SIBLING of the text for
    // exactly this reason.
    assert!(
        shown(vcx, "caret-rename-lit") || shown(vcx, "caret-rename-dark"),
        "a long proposal clipped the insertion point"
    );
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

/// **Slice AG2 #2 + #3, and the reason they are one test.**
///
/// Grouping earns its place by hiding rows; it is allowed to hide them only
/// because the header keeps their worst state. So the walk collapses a
/// project that is parked on a permission question and asserts the rows are
/// gone AND the question is still stated. waku ships the first half without
/// the second — a collapsed group there silently swallows a waiting session
/// — and eave's own `rail_shows` already refuses to let the archive filter
/// do that.
/// Slice AG2 #4 — a session that settled while the operator was elsewhere
/// says `finished` in the badge slot `state_badge` deliberately leaves
/// empty for `done`.
///
/// The gap in one sentence: *a session that ended an hour ago and one that
/// ended while you were reading it look identical.*
#[gpui::test]
fn a_session_that_settled_while_you_were_away_says_finished(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.state = "running".into();
    a.last_activity_ms = Some(100);
    h.plumbing.push_rail(vec![a.clone()]);
    vcx.run_until_parked();
    // First sight baselined it, and a running session has nothing to say.
    assert!(
        !shown(vcx, "badge-aaaa1111"),
        "a running session wore a word"
    );

    // It finishes, and nobody is looking: no session is selected.
    let mut done = a;
    done.state = "done".into();
    done.last_activity_ms = Some(200);
    h.plumbing.push_rail(vec![done]);
    vcx.run_until_parked();
    assert!(
        shown(vcx, "badge-aaaa1111"),
        "A SESSION FINISHED UNWATCHED AND THE RAIL SAID NOTHING — the gap"
    );
}

/// **A session you are looking at never says it finished**, because you
/// watched it finish. This is the half that keeps the mark from being noise.
#[gpui::test]
fn a_session_you_are_looking_at_never_says_finished(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.state = "running".into();
    a.last_activity_ms = Some(100);
    rail_with_selected(&h, vcx, vec![a.clone()], "aaaa1111");

    let mut done = a;
    done.state = "done".into();
    done.last_activity_ms = Some(200);
    h.plumbing.push_rail(vec![done]);
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "badge-aaaa1111"),
        "the session open on screen claimed the operator had missed it"
    );
}

/// A session the rail has never seen says NOTHING, however long ago it
/// settled.
///
/// Without this the feature opens with fifteen rows shouting about work the
/// operator already knows about, and the eye learns to stop reading the
/// slot — the failure `filter_note_text`'s own doc names.
#[gpui::test]
fn a_session_seen_for_the_first_time_says_nothing(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.state = "done".into();
    // Settled long ago, and this client has no record of ever looking.
    a.last_activity_ms = Some(9_999);
    h.plumbing.push_rail(vec![a]);
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "badge-aaaa1111"),
        "a session the rail had never seen before claimed to be news"
    );
}

/// **A collapsed project may not swallow a finish either** — #3's bargain
/// applied to the quieter fact: a header speaks for the children it hides.
#[gpui::test]
fn a_collapsed_project_still_says_a_child_finished(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.checkout_root = Some("/w/eave".into());
    a.state = "running".into();
    a.last_activity_ms = Some(100);
    let mut z = session("cccc3333");
    z.checkout_root = Some("/w/zed".into());
    h.plumbing.push_rail(vec![a.clone(), z.clone()]);
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "rail-group-/w/eave-badge"),
        "the header spoke before anything had happened"
    );

    let mut done = a;
    done.state = "done".into();
    done.last_activity_ms = Some(200);
    h.plumbing.push_rail(vec![done, z]);
    vcx.run_until_parked();
    assert!(
        shown(vcx, "rail-group-/w/eave-badge"),
        "the project said nothing about a child that finished"
    );

    click(vcx, "rail-group-/w/eave");
    vcx.run_until_parked();
    assert!(!shown(vcx, "rail-aaaa1111"), "the group would not collapse");
    assert!(
        shown(vcx, "rail-group-/w/eave-badge"),
        "COLLAPSING SWALLOWED THE FINISH"
    );
    // The other project is untouched — nothing under it moved.
    assert!(
        !shown(vcx, "rail-group-/w/zed-badge"),
        "a group with nothing to say wore a word anyway"
    );
}

/// Slice AG2 #7 — ⌘⇧↓ and ⌘⇧↑ move the selection one row along the rail.
///
/// The rail is the screen an operator lives in and it had no keyboard
/// selection at all: ⌘⇧A / ⌘⇧D could ACT on the oldest asking session, but
/// nothing could choose a row without the pointer.
#[gpui::test]
fn the_rail_walk_moves_the_selection_one_row_each_way(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(
        &h,
        vcx,
        vec![
            session("aaaa1111"),
            session("bbbb2222"),
            session("cccc3333"),
        ],
        "bbbb2222",
    );
    let _ = h.drain();

    vcx.simulate_keystrokes("cmd-shift-down");
    vcx.run_until_parked();
    assert!(
        matches!(h.drain().as_slice(), [Command::Select(id)] if id == "cccc3333"),
        "⌘⇧↓ did not walk to the next row"
    );

    // The selection only actually moves when the engine confirms it, so
    // the harness plays that part — the same thing a click relies on.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "cccc3333".into(),
    });
    vcx.run_until_parked();
    let _ = h.drain();

    vcx.simulate_keystrokes("cmd-shift-up");
    vcx.run_until_parked();
    assert!(
        matches!(h.drain().as_slice(), [Command::Select(id)] if id == "bbbb2222"),
        "⌘⇧↑ did not walk back"
    );
}

/// **The walk goes where the eye goes.** A row folded away under a
/// collapsed project is not somewhere the keyboard may land.
///
/// This is #3's bargain enforced on a second surface: a header speaks for
/// the children it hides, and nothing reaches past it in silence. A walk
/// that stepped into a collapsed group would move the transcript to a
/// session with no row on screen to say so.
#[gpui::test]
fn the_walk_will_not_step_into_a_collapsed_project(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.checkout_root = Some("/w/eave".into());
    let mut b = session("bbbb2222");
    b.checkout_root = Some("/w/zed".into());
    let mut c = session("cccc3333");
    c.checkout_root = Some("/w/zed".into());
    rail_with_selected(&h, vcx, vec![a, b, c], "aaaa1111");

    // Precondition: without the collapse, ⌘⇧↓ lands on the zed group's
    // first row. Establishing it here is what stops the assert below from
    // passing for the wrong reason.
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-shift-down");
    vcx.run_until_parked();
    assert!(
        matches!(h.drain().as_slice(), [Command::Select(id)] if id == "bbbb2222"),
        "the walk did not reach the second project at all, so this test \
         cannot show that collapsing is what stops it"
    );

    click(vcx, "rail-group-/w/zed");
    vcx.run_until_parked();
    assert!(!shown(vcx, "rail-bbbb2222"), "the group would not collapse");
    let _ = h.drain();

    vcx.simulate_keystrokes("cmd-shift-down");
    vcx.run_until_parked();
    assert!(
        h.drain().is_empty(),
        "THE WALK STEPPED INTO A COLLAPSED PROJECT — it selected a session \
         with no row on screen"
    );
}

/// The same rule through the filter's door: a row the text filter dropped
/// is not somewhere the walk may land either.
#[gpui::test]
fn the_walk_will_not_step_onto_a_row_the_filter_hid(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.title = Some("sweep the floor".into());
    // Sits BETWEEN the two matches, so a walk that ignored the filter
    // would land here and a walk that respects it must step over.
    let mut b = session("bbbb2222");
    b.title = Some("something else".into());
    let mut c = session("cccc3333");
    c.title = Some("sweep the stairs".into());
    rail_with_selected(&h, vcx, vec![a, b, c], "aaaa1111");

    click(vcx, "rail-filter");
    vcx.run_until_parked();
    vcx.simulate_input("sweep");
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-aaaa1111"), "the first match should show");
    assert!(shown(vcx, "rail-cccc3333"), "the second match should show");
    assert!(!shown(vcx, "rail-bbbb2222"), "the filter did not narrow");
    let _ = h.drain();

    // The chord is an ACTION, so it fires ahead of the filter field's own
    // key handling — which is deliberate, and is the interaction this
    // pins: narrow the list, then walk what is left without closing the
    // field.
    //
    // A POSITIVE landing, not an absence. Asserting only "nothing was
    // selected" would pass just as well if the chord were dead.
    vcx.simulate_keystrokes("cmd-shift-down");
    vcx.run_until_parked();
    assert!(
        matches!(h.drain().as_slice(), [Command::Select(id)] if id == "cccc3333"),
        "the walk landed on the row the filter had hidden, or nowhere at all"
    );
}

/// It clamps; it does not wrap.
///
/// The deliberate divergence from `strip::cycle`, which wraps and says
/// why. ctrl-tab is a CYCLER; these chords are DIRECTIONAL, and a ↓ that
/// jumped to the top would move the selection up the screen — in a rail
/// that is now a map of several checkouts, into a different project than
/// the one being read.
#[gpui::test]
fn the_walk_clamps_at_both_ends_rather_than_wrapping(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222")],
        "bbbb2222",
    );
    let _ = h.drain();

    vcx.simulate_keystrokes("cmd-shift-down");
    vcx.run_until_parked();
    assert!(
        h.drain().is_empty(),
        "⌘⇧↓ on the last row wrapped to the top"
    );

    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    let _ = h.drain();

    vcx.simulate_keystrokes("cmd-shift-up");
    vcx.run_until_parked();
    assert!(
        h.drain().is_empty(),
        "⌘⇧↑ on the first row wrapped to the bottom"
    );

    // The control. Both asserts above are absences, and an absence is
    // also what a dead chord produces — so prove the chord is live in
    // THIS fixture by walking the direction that has somewhere to go.
    vcx.simulate_keystrokes("cmd-shift-down");
    vcx.run_until_parked();
    assert!(
        matches!(h.drain().as_slice(), [Command::Select(id)] if id == "bbbb2222"),
        "the chord did nothing at all here, so the two clamps above prove \
         nothing"
    );
}

/// With nothing selected the walk ENTERS the list, from the end it is
/// travelling from. Refusing would make the chord dead at the launcher,
/// which is exactly where a keyboard-first operator starts.
#[gpui::test]
fn the_walk_enters_the_list_when_nothing_is_selected(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    h.plumbing
        .push_rail(vec![session("aaaa1111"), session("bbbb2222")]);
    vcx.run_until_parked();
    let _ = h.drain();

    vcx.simulate_keystrokes("cmd-shift-down");
    vcx.run_until_parked();
    assert!(
        matches!(h.drain().as_slice(), [Command::Select(id)] if id == "aaaa1111"),
        "⌘⇧↓ with no session open did not take the first row"
    );

    vcx.simulate_keystrokes("cmd-shift-up");
    vcx.run_until_parked();
    assert!(
        matches!(h.drain().as_slice(), [Command::Select(id)] if id == "bbbb2222"),
        "⌘⇧↑ with no session open did not take the last row"
    );
}

/// A rail showing its FILES face has no session rows on screen, so the
/// chord does nothing.
///
/// The refusal is the same one the collapsed-project test states: a chord
/// that moved the selection behind a list of files would be an invisible
/// act. It is also the test that found the defect — the first cut skipped
/// WRITING the walk order here rather than clearing it, which left the
/// previous list standing and the chord walking rows that were no longer
/// drawn.
#[gpui::test]
fn the_walk_is_inert_while_the_rail_shows_its_files_face(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222")],
        "aaaa1111",
    );
    // The control, taken BEFORE the face swaps: the chord works here.
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-shift-down");
    vcx.run_until_parked();
    assert!(
        matches!(h.drain().as_slice(), [Command::Select(id)] if id == "bbbb2222"),
        "the chord was already dead, so this test cannot show the face is \
         what stops it"
    );

    open_files_face(vcx);
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "rail-bbbb2222"),
        "the files face did not replace the session rows"
    );
    let _ = h.drain();

    vcx.simulate_keystrokes("cmd-shift-down");
    vcx.run_until_parked();
    assert!(
        h.drain().is_empty(),
        "the walk moved the selection behind a list of files"
    );
}

/// The walk's order is the PAINTED order, which is why it is recorded by
/// the paint rather than re-derived. `fold_rail` decides position once and
/// grouping preserves it, so the walk must follow the rail's own order and
/// not, say, the order the rows arrived in.
#[gpui::test]
fn the_walk_follows_the_rails_order_across_a_group_boundary(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    // A rootless session sorts to the top ungrouped, then the two
    // checkouts in first-member order.
    let loose = session("aaaa1111");
    let mut e = session("bbbb2222");
    e.checkout_root = Some("/w/eave".into());
    let mut z = session("cccc3333");
    z.checkout_root = Some("/w/zed".into());
    rail_with_selected(&h, vcx, vec![loose, e, z], "bbbb2222");
    let _ = h.drain();

    // From the eave group's only row, ⌘⇧↓ crosses INTO the zed group —
    // over its header, which is not a stop.
    vcx.simulate_keystrokes("cmd-shift-down");
    vcx.run_until_parked();
    assert!(
        matches!(h.drain().as_slice(), [Command::Select(id)] if id == "cccc3333"),
        "the walk did not cross the group boundary"
    );
}

#[gpui::test]
fn a_collapsed_project_still_says_a_session_is_waiting_on_you(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.checkout_root = Some("/w/eave".into());
    a.state = "awaiting_approval".into();
    a.pending_approvals = 1;
    let mut b = session("bbbb2222");
    b.checkout_root = Some("/w/eave".into());
    let mut z = session("cccc3333");
    z.checkout_root = Some("/w/zed".into());
    h.plumbing.push_rail(vec![a, b, z]);
    vcx.run_until_parked();

    // Two checkouts, so the rail divides. (One would not — see
    // `one_project_gets_no_header_at_all`.)
    assert!(
        shown(vcx, "rail-group-/w/eave-label"),
        "the rail held two checkouts and drew no project header"
    );
    assert!(shown(vcx, "rail-group-/w/zed-label"));
    assert!(shown(vcx, "rail-aaaa1111"), "the asking row");
    assert!(shown(vcx, "rail-bbbb2222"), "its neighbour");
    assert!(
        shown(vcx, "rail-group-/w/eave-badge"),
        "the header said nothing about a child waiting on the operator"
    );
    assert!(
        !shown(vcx, "rail-group-/w/zed-badge"),
        "a group with nothing to say wore a word anyway"
    );

    // Collapse the project that is asking — the dangerous direction.
    click(vcx, "rail-group-/w/eave");
    vcx.run_until_parked();
    assert!(!shown(vcx, "rail-aaaa1111"), "the group would not collapse");
    assert!(!shown(vcx, "rail-bbbb2222"));
    assert!(
        shown(vcx, "rail-group-/w/eave-badge"),
        "COLLAPSING SWALLOWED A SESSION PARKED ON A QUESTION — this is the \
         whole reason #3 ships with #2"
    );
    // The other project is untouched: collapse is keyed on the root, so one
    // header cannot fold another's rows.
    assert!(
        shown(vcx, "rail-cccc3333"),
        "collapsing eave folded zed too"
    );

    click(vcx, "rail-group-/w/eave");
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-aaaa1111"), "the group would not reopen");
}

/// **Slice AG2 #8 — a project's colour, in the slot the strip already
/// keeps for it.**
///
/// The rail's project header and the tab strip's project row are two
/// drawings of one thing, so the header takes the strip's own order —
/// chevron, swatch, title — rather than inventing a place for the mark.
/// The state dot stays on the far side of the header: identity left,
/// attention right, which is how the header was already split.
#[gpui::test]
fn a_project_header_wears_its_colour_in_the_strips_own_slot(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.checkout_root = Some("/w/eave".into());
    let mut z = session("bbbb2222");
    z.checkout_root = Some("/w/zed".into());
    h.plumbing.push_rail(vec![a, z]);
    vcx.run_until_parked();

    assert!(
        shown(vcx, "rail-group-/w/eave-swatch"),
        "the project header drew no colour at all"
    );
    assert!(shown(vcx, "rail-group-/w/zed-swatch"));

    let chevron = vcx.debug_bounds("rail-group-/w/eave-chevron").unwrap();
    let swatch = vcx.debug_bounds("rail-group-/w/eave-swatch").unwrap();
    let label = vcx.debug_bounds("rail-group-/w/eave-label").unwrap();
    assert!(
        chevron.origin.x < swatch.origin.x && swatch.origin.x < label.origin.x,
        "the swatch is not between the disclosure and the title: \
         chevron {:?} swatch {:?} label {:?}",
        chevron.origin.x,
        swatch.origin.x,
        label.origin.x
    );
    // The mark is the strip's 6 px dot, not a bar or a rule — a shape the
    // rail already uses for exactly one other thing, on the other side of
    // this same row.
    assert_eq!(swatch.size.width, px(theme::GROUP_DOT));
    assert_eq!(swatch.size.height, px(theme::GROUP_DOT));
}

/// A mark that distinguishes nothing is noise, and the rail already says
/// so about the header itself: `group_rail` emits no header unless the
/// sessions genuinely fall into more than one part. The swatch inherits
/// that rule by living on the header rather than on the row.
#[gpui::test]
fn one_project_gets_no_swatch_because_it_gets_no_header(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.checkout_root = Some("/w/eave".into());
    let mut b = session("bbbb2222");
    b.checkout_root = Some("/w/eave".into());
    h.plumbing.push_rail(vec![a, b]);
    vcx.run_until_parked();

    assert!(shown(vcx, "rail-aaaa1111"), "the rows are there");
    assert!(
        !shown(vcx, "rail-group-/w/eave-swatch"),
        "one project wore a colour telling it apart from nothing"
    );
}

/// **One function decides a project's colour, and both surfaces ask it.**
///
/// This is the structural half of the claim the doc comment on
/// `project_color` makes: the strip and the rail agree about a project
/// because they each COMPUTE the colour from the checkout root, not
/// because one of them stores it and the other reads it. A second
/// producer would be the two-favicon-cache defect again — two caches, one
/// fact, and a tab drawing a globe.
///
/// `group_color` is gone rather than left standing: the last slice deleted
/// two callerless twins in `composer.rs` that were still carrying the live
/// controls' selectors, and a pigment function nothing calls is the same
/// shape of trap.
#[gpui::test]
fn a_project_has_one_colour_and_one_place_that_decides_it(_cx: &mut TestAppContext) {
    let src = include_str!("ui.rs");
    let prod = &src[..src.rfind("#[cfg(test)]").expect("the tests module")];
    assert!(
        !prod.contains("group_color"),
        "the old pigment function is still here — with `project_color` \
         beside it that is a second answer to one question"
    );
    assert_eq!(
        prod.matches("fn project_color(").count(),
        1,
        "more than one decider"
    );
    // Slice ID2b: the strip's project row is gone (a project is a header
    // in the rail's tree and nowhere else), so ONE call site remains.
    assert_eq!(
        prod.matches("project_color(").count(),
        2,
        "the definition and exactly one call site — the rail's project \
         header. A second surface painting a project mark must come \
         through here too."
    );
    // The strip's call site passes the group's OWN checkout root. Passing
    // `None` there would compile, draw an accent dot, and give one project
    // two identities across two surfaces — the failure this whole
    // arrangement exists to make impossible, and the only one of these that
    // is invisible on screen until you have both surfaces open at once.
    assert!(
        prod.contains("Some(&g.root),\n                        chosen_colors.get(&g.root)"),
        "the rail's project swatch stopped asking about its own root"
    );
    // `named_color` without its parenthesis: `project_color` reaches it as
    // a function value (`chosen.and_then(named_color)`), not as a call.
    for (helper, uses) in [("named_color", 2), ("derived_project_ink(", 2)] {
        assert_eq!(
            prod.matches(helper).count(),
            uses,
            "{helper} is reached from somewhere other than project_color"
        );
    }
}

// ── Slice AG2 #11 — the diffstat on every row ──────────────────

/// The row this slice exists for is the one you are NOT looking at.
///
/// The `+N −N` used to be summed in the rail from the single `files`
/// subscription the shell holds, which is the selected session's and
/// nobody else's — fourteen other sessions could each be carrying a
/// hundred uncommitted lines and the rail said nothing about any of them.
/// It is a session fact now, so it arrives on the same watch frame as the
/// state word, for every row at once.
///
/// The third session is the other half of the claim: `None` is an honest
/// silence and must draw NOTHING, never a `+0 −0`.
#[gpui::test]
fn every_row_states_its_own_diffstat_not_only_the_selected_one(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.diff_stat = Some(DiffStat { adds: 12, dels: 3 });
    let mut b = session("bbbb2222");
    b.diff_stat = Some(DiffStat {
        adds: 400,
        dels: 91,
    });
    // Nothing to say: no scan yet, a failed scan, a truncated list, or a
    // clean tree — `FilesSnapshot::diff_stat` decides which, and every one
    // of them arrives here as the same `None`.
    let c = session("cccc3333");
    h.plumbing.push_rail(vec![a, b, c]);
    // The shell is subscribed to A, and holds no files snapshot for B at
    // all. Under the old rule that alone was enough to blank B's numbers.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();

    assert!(
        shown(vcx, "stat-aaaa1111"),
        "the selected row lost its stat"
    );
    assert!(
        shown(vcx, "stat-bbbb2222"),
        "a row nobody is subscribed to still has no numbers"
    );
    assert!(
        !shown(vcx, "stat-cccc3333"),
        "a session with no honest number drew one anyway"
    );
}

/// Which LINE the numbers are on, pinned — because the answer cost a
/// measurement and nothing else in the file records it.
///
/// The stat shipped on line three for one release, beside the path, back
/// when only the selected row had one. Once every row has one that line
/// cannot hold it: `a_marked_row_still_shows_its_folder` measured the
/// folder at 25 px of the 142 px line three shares, and `/Us…` is the exact
/// failure that test exists to catch. Line one had the room and the better
/// argument — the age is already there, and a magnitude and a span are the
/// same kind of glance.
///
/// Asserted as geometry rather than as a comment: same row as the age,
/// strictly above the path, and left of the age so the corner clock stays
/// the corner clock.
#[gpui::test]
fn the_diffstat_sits_with_the_age_and_not_with_the_path(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut s = session("aaaa1111");
    s.diff_stat = Some(DiffStat {
        adds: 124,
        dels: 18,
    });
    // An age the row will actually render: `coarse_age` says nothing under
    // a minute, and a fixture stamp is not an age.
    s.last_activity_ms = Some(eave_core::journal::now_ms().saturating_sub(2 * 60 * 60 * 1_000));
    h.plumbing.push_rail(vec![s]);
    vcx.run_until_parked();

    let mut at = |sel: &'static str| {
        vcx.debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} did not render"))
    };
    let stat = at("stat-aaaa1111");
    let age = at("age-aaaa1111");
    let cwd = at("cwd-aaaa1111");
    let label = at("label-aaaa1111");

    // Slice ID2a re-ranked the card: the TITLE is line one and the age
    // is its corner; the provider kicker is the metadata line under it
    // and the stat sits there, beside the thing that yields. Both stay
    // above the path, which is the fact AG2 #11 measured.
    // Same LINE, not same origin: the title's box is taller than the
    // age's (name metrics against micro metrics, plus the rename field's
    // invisible border), and `items_center` aligns their centres.
    assert!(
        same_line(age, label),
        "the age should share the title line: age {:?}, title {:?}",
        age.origin,
        label.origin
    );
    // Slice NC2: the row is two lines, and the stat is the title line's
    // corner beside the age.
    assert!(
        same_line(stat, label) && same_line(stat, age),
        "the stat should sit in the title line's corner beside the age"
    );
    assert!(
        stat.origin.y < cwd.origin.y,
        "the stat is back on line three with the path: stat {:?}, path {:?}",
        stat.origin,
        cwd.origin
    );
    assert!(
        label.origin.x + label.size.width <= age.origin.x,
        "the title ran under the corner the age holds"
    );
}

/// Two boxes on the same LINE of a row: their vertical centres agree to
/// within the rounding a centred flex row produces. Origins do not — a
/// name-size box and a micro-size box on one centred line start at
/// different y — so origin equality is the wrong question for anything
/// that shares a line with the title.
fn same_line(a: gpui::Bounds<Pixels>, b: gpui::Bounds<Pixels>) -> bool {
    let ca = a.origin.y + a.size.height / 2.;
    let cb = b.origin.y + b.size.height / 2.;
    (ca - cb).abs() <= px(2.)
}

/// **Slice ID2a — the card's anatomy, re-ranked.** The task is the row's
/// identity and the provider is its metadata: title line, then the state
/// dot + provider chip + kicker, then branch/cwd. Every fact Stage AB, AO
/// and AG2 put on the row is still on it — asserted by presence, on the
/// row carrying the most of them — and the row is no taller for the
/// re-ranking, so the glide and the window budgets are untouched.
#[gpui::test]
fn the_task_is_the_row_and_the_provider_is_its_metadata(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut s = session("aaaa1111");
    s.title = Some("Diagnose OMP chat blanking".into());
    s.model = Some("claude-opus-4-1".into());
    s.state = "quiet".into();
    s.shared_with = vec!["bbbb2222".into()];
    s.diff_stat = Some(DiffStat { adds: 12, dels: 3 });
    s.last_activity_ms = Some(eave_core::journal::now_ms().saturating_sub(19 * 60 * 1_000));
    let plain = session("bbbb2222");
    h.plumbing.push_rail(vec![s, plain]);
    vcx.run_until_parked();

    let mut at = |sel: &'static str| {
        vcx.debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} did not render"))
    };
    let label = at("label-aaaa1111");
    let badge = at("badge-aaaa1111");
    let age = at("age-aaaa1111");
    let chip = at("chip-aaaa1111");
    let stat = at("stat-aaaa1111");
    let cwd = at("cwd-aaaa1111");
    let marks = at("marks-aaaa1111");

    // Rank one: the title, with its attention word and its age.
    // Slice NC2: TWO ranks. The title line carries the mark, the name,
    // the badge, the stat and the age; the location line carries the
    // path and the marks. No metadata line, no model kicker.
    assert!(same_line(badge, label), "the badge left the title line");
    assert!(same_line(age, label), "the age left the title line");
    assert!(same_line(chip, label), "the provider's mark left the title line");
    assert!(
        chip.origin.x < label.origin.x,
        "the mark is not before the name: mark {:?}, name {:?}",
        chip.origin,
        label.origin
    );
    assert!(same_line(stat, label), "the stat left the title line");
    assert!(
        label.origin.y + label.size.height <= cwd.origin.y + px(1.),
        "the path is not under the title: title {:?}, path {:?}",
        label.origin,
        cwd.origin
    );
    assert!(same_line(marks, cwd), "the marks left the path line");

    let plain_label = at("label-bbbb2222");
    let plain_chip = at("chip-bbbb2222");
    assert!(same_line(plain_label, plain_chip));
    assert!(
        vcx.debug_bounds("prov-aaaa1111").is_none(),
        "the metadata line is back — the row is two lines"
    );
    assert_eq!(
        vcx.debug_bounds("rail-aaaa1111").unwrap().size.height,
        vcx.debug_bounds("rail-bbbb2222").unwrap().size.height,
        "a named row and a placeholder row must be the same height"
    );
}

/// **Slice ID2a — the derived name is on the ROW, not only in the rename
/// field, and it comes from the one resolver the paint uses.** A session
/// this client opened is called by its opening ask; one it never opened
/// stays a placeholder; and a title the operator applies outranks both.
/// Asserted through `Shell::row_name` because it is what the rail, the
/// strip and the breadcrumb all call — a test on the paint alone could
/// pass while the strip still said `claude code`.
#[gpui::test]
fn a_session_you_opened_is_called_by_its_opening_ask(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.last_seq = 1;
    let mut b = session("bbbb2222");
    b.last_seq = 1;
    rail_with_selected(&h, vcx, vec![a.clone(), b.clone()], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(1, "/goal diagnose the OMP chat blanking")],
    });
    vcx.run_until_parked();

    let names = h
        .window
        .update(vcx, |shell, _, _| (shell.row_name(&a), shell.row_name(&b)))
        .unwrap();
    assert_eq!(
        names.0,
        (
            "diagnose the OMP chat blanking".to_string(),
            NameKind::Derived
        ),
        "the opened session is not called by its ask"
    );
    assert_eq!(
        names.1,
        (UNTITLED_LABEL.to_string(), NameKind::Placeholder),
        "a session this client never opened came up with a name"
    );

    // The operator's title wins over the derived one, on the same row.
    let mut named = a.clone();
    named.title = Some("OMP blanking".into());
    let name = h
        .window
        .update(vcx, |shell, _, _| shell.row_name(&named))
        .unwrap();
    assert_eq!(name, ("OMP blanking".to_string(), NameKind::Named));

    // And the strip's session tab carries the same answer: the task, not
    // the provider, with a stand-in flagged as one.
    let chips = h
        .window
        .update(vcx, |shell, _, cx| {
            shell
                .sessions
                .read(cx)
                .sessions
                .iter()
                .map(|s| {
                    let (label, kind) = shell.row_name(s);
                    (s.session_id.clone(), label, kind == NameKind::Placeholder)
                })
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert!(chips.iter().any(|(id, label, ph)| id == "aaaa1111"
        && label == "diagnose the OMP chat blanking"
        && !ph));
    assert!(chips
        .iter()
        .any(|(id, label, ph)| id == "bbbb2222" && label == UNTITLED_LABEL && *ph));
}

/// Every row STATES its numbers; only the row you are in is a DOOR.
///
/// `toggle_files_face` swaps the well's face for the session the well is
/// showing. Fired from another row it would change a face you are not
/// looking at, on a session you did not name, and leave the number you
/// actually clicked unexplained — so off the selected row the stat carries
/// no handler at all and the click belongs to the row, which is the
/// ordinary thing a click on a row does.
#[gpui::test]
fn the_diffstat_is_a_door_only_on_the_row_you_are_in(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.diff_stat = Some(DiffStat { adds: 12, dels: 3 });
    let mut b = session("bbbb2222");
    b.diff_stat = Some(DiffStat {
        adds: 400,
        dels: 91,
    });
    h.plumbing.push_rail(vec![a, b]);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    let _ = h.drain();

    click(vcx, "stat-bbbb2222");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "rail-rows"),
        "another row's stat swapped the rail's face"
    );
    assert!(
        matches!(h.drain().as_slice(), [Command::Select(id)] if id == "bbbb2222"),
        "the click should have belonged to the row it landed on"
    );

    click(vcx, "stat-aaaa1111");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "rail-rows"),
        "the selected row's stat is still the door to the changes face"
    );
    assert!(
        h.drain().is_empty(),
        "the door stops the click; it must not also re-select the row"
    );
}

/// The floor line one now needs, for the reason line three has one.
///
/// Slice AG2 #11 put a permanent third occupant on this line, so the same
/// question applies to it: with everything at its widest, does the thing
/// that yields yield to NOTHING? It does not, and the reason is structural
/// rather than lucky — both of the flanking facts are hard-bounded to four
/// characters (`coarse_lines`, `coarse_age`), so what is left for the
/// kicker has a floor that no session's data can move.
///
/// A truncated provider name is the cheapest loss on the card: the chip
/// beside it carries the identity, and its own docs call the text "a label
/// for the name beside it". That is why this floor is 36 px where line
/// three's is 80.
///
/// **This is the extreme, not the ordinary row.** `+124 −18` beside a `2h`
/// leaves the kicker 71 px and `Claude Code` renders whole (67.4 px at
/// `MICRO_SIZE`). Reaching 39 px takes a session with six figures of
/// changed lines AND a span near the year rollover AND a model name — all
/// three at once.
#[gpui::test]
fn line_one_never_squeezes_the_title_to_nothing(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut s = session("aaaa1111");
    // Everything at its widest, at once: `+999k -999k` is the widest the
    // coarse ladder can shape to, `364d` the widest span before the year
    // rollover, and a long model name pushes the kicker past its box.
    s.diff_stat = Some(DiffStat {
        adds: 999_999,
        dels: 999_999,
    });
    s.last_activity_ms =
        Some(eave_core::journal::now_ms().saturating_sub(364 * 24 * 60 * 60 * 1_000));
    s.model = Some("claude-sonnet-4-5-20250929".into());
    h.plumbing.push_rail(vec![s]);
    vcx.run_until_parked();

    let _ = vcx
        .debug_bounds("age-aaaa1111")
        .expect("no age on the row — this is not the crowded line the test means");
    // Slice NC2: the stat and the age share the title line with the
    // name now, and the name is what yields. It must not yield to
    // nothing: `+999999 −999999` plus a year is the widest corner a row
    // can have, and the title still gets a word's width (45 px measured
    // at the harness's rail width; the floor is 40).
    let label = vcx
        .debug_bounds("label-aaaa1111")
        .expect("the title vanished");
    assert!(
        label.size.width >= px(40.),
        "the title was squeezed to {:?} by the numbers beside it",
        label.size.width
    );
}

/// One source for the number, pinned in the source itself.
///
/// The defect this forecloses is the two-favicon-cache one: a rail summing
/// its own total from the `files` subscription while the engine stamps
/// another onto the row would show two different numbers for one tree
/// depending on which one last arrived. `files_diffstat` was that second
/// summer and is gone; `FilesSnapshot::diff_stat` in `eave-wire` is the one
/// place that decides, and the rail only renders what it decided.
#[gpui::test]
fn the_rail_sums_nothing_and_renders_one_number(_cx: &mut TestAppContext) {
    let src = include_str!("ui.rs");
    let prod = &src[..src.rfind("#[cfg(test)]").expect("ui.rs has a test module")];
    assert_eq!(
        prod.matches("files_diffstat").count(),
        0,
        "the rail is summing a snapshot again"
    );
    assert_eq!(
        prod.matches("files_stat").count(),
        0,
        "the selected-session-only stat is back"
    );
    assert_eq!(
        prod.matches("let stat = info.diff_stat;").count(),
        1,
        "the row's number should come from the session fact, once"
    );
}

// ── Slice AG2 #12 — bulk operations ───────────────────────────────────

fn cmd() -> Modifiers {
    Modifiers {
        platform: true,
        ..Modifiers::none()
    }
}

fn shift() -> Modifiers {
    Modifiers {
        shift: true,
        ..Modifiers::none()
    }
}

/// **Let the resort glide finish before clicking the bar.**
///
/// The first mark makes the bar appear, which pushes every row down by its
/// height, and `RAIL_RESORT` animates that over 260 ms — during which a row
/// is PAINTED where it used to be and takes clicks there. Without this the
/// click on `rail-marked-clear` landed on the session row underneath and
/// the test failed with a `Select` it never asked for, which is how the
/// cost was found. `run_until_parked` does not help: the glide is a clock,
/// not a task.
fn settle(vcx: &mut VisualTestContext) {
    vcx.executor().advance_clock(Duration::from_millis(400));
    vcx.run_until_parked();
}

/// **A mark collects; the selection navigates. The point of #12 is that
/// they are not the same act.**
///
/// Today archiving a row you are not looking at is impossible — the
/// archive button is gated on `is_sel` — so tidying ten finished sessions
/// costs ten context switches, each of which swaps the transcript, the
/// composer, the files list and the page well. ⌘-click has to stage a row
/// without going anywhere near it, or the bulk act has not fixed anything.
#[gpui::test]
fn a_marked_row_is_staged_without_going_there(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222")],
        "aaaa1111",
    );
    let _ = h.drain();

    click_with(vcx, "rail-bbbb2222", cmd());
    vcx.run_until_parked();
    assert!(
        h.drain().is_empty(),
        "⌘-click navigated — staging a row moved the whole workspace onto it"
    );
    assert!(
        shown(vcx, "mark-bbbb2222"),
        "the row does not say it is staged"
    );
    assert!(
        !shown(vcx, "mark-aaaa1111"),
        "an unmarked row wore the rule"
    );
    assert!(shown(vcx, "rail-marked"), "nothing counted the staged rows");
    assert!(shown(vcx, "rail-marked-archive"));

    // And again unstages it — a toggle, so the gesture that made a mistake
    // is the gesture that undoes it.
    click_with(vcx, "rail-bbbb2222", cmd());
    vcx.run_until_parked();
    assert!(!shown(vcx, "mark-bbbb2222"));
    assert!(!shown(vcx, "rail-marked"), "the bar outlived the last mark");
}

/// Navigating is not unstaging. Looking at one of the rows you have
/// collected before acting on all of them is the whole reason the two
/// concepts are separate.
#[gpui::test]
fn a_plain_click_still_navigates_and_leaves_the_marks_standing(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222")],
        "aaaa1111",
    );
    let _ = h.drain();

    click_with(vcx, "rail-bbbb2222", cmd());
    vcx.run_until_parked();
    let _ = h.drain();

    click(vcx, "rail-bbbb2222");
    vcx.run_until_parked();
    assert!(
        matches!(h.drain().as_slice(), [Command::Select(id)] if id == "bbbb2222"),
        "a plain click stopped navigating"
    );
    assert!(
        shown(vcx, "mark-bbbb2222"),
        "going to look at a staged row threw the staging away"
    );
}

/// **A range walks what the rail DREW, not what it holds.**
///
/// The span comes from `rail_walk`, so it cannot reach into a collapsed
/// project, past the window, or through a row the text filter dropped —
/// the same argument AG2 #7 makes about the keyboard. Deriving it from the
/// session list instead would let one shift-click stage rows the operator
/// cannot see and is about to archive.
#[gpui::test]
fn a_range_cannot_reach_into_a_collapsed_project(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let loose = session("aaaa1111");
    let mut e = session("bbbb2222");
    e.checkout_root = Some("/w/eave".into());
    let mut z = session("cccc3333");
    z.checkout_root = Some("/w/zed".into());
    h.plumbing.push_rail(vec![loose, e, z]);
    vcx.run_until_parked();

    // Fold the middle project away. The rail now draws the loose row and
    // the zed row, with eave's between them in the list and on screen
    // nowhere.
    click(vcx, "rail-group-/w/eave");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "rail-bbbb2222"),
        "the project would not collapse"
    );

    click_with(vcx, "rail-aaaa1111", cmd());
    vcx.run_until_parked();
    click_with(vcx, "rail-cccc3333", shift());
    vcx.run_until_parked();

    assert!(shown(vcx, "mark-aaaa1111"), "the anchor lost its mark");
    assert!(
        shown(vcx, "mark-cccc3333"),
        "the range did not reach the far end"
    );

    // Re-open the project and look at what the range swept up.
    click(vcx, "rail-group-/w/eave");
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-bbbb2222"), "the project would not reopen");
    assert!(
        !shown(vcx, "mark-bbbb2222"),
        "A SHIFT-CLICK STAGED A ROW THAT WAS NOT ON SCREEN — this is the \
         whole reason the range reads `rail_walk`"
    );
}

/// A range only ever adds. A shift-click that unmarked a span would be the
/// one gesture in this rail that destroys work invisibly.
#[gpui::test]
fn a_range_only_ever_adds(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(vec![
        session("aaaa1111"),
        session("bbbb2222"),
        session("cccc3333"),
    ]);
    vcx.run_until_parked();

    click_with(vcx, "rail-aaaa1111", cmd());
    vcx.run_until_parked();
    click_with(vcx, "rail-cccc3333", shift());
    vcx.run_until_parked();
    for sel in ["mark-aaaa1111", "mark-bbbb2222", "mark-cccc3333"] {
        assert!(shown(vcx, sel), "{sel} was not swept up by the range");
    }

    // The same range again finds them all marked and leaves them that way.
    click_with(vcx, "rail-cccc3333", shift());
    vcx.run_until_parked();
    for sel in ["mark-aaaa1111", "mark-bbbb2222", "mark-cccc3333"] {
        assert!(shown(vcx, sel), "a second shift-click unstaged {sel}");
    }
}

/// The bar files them all away in one act and then stops counting them —
/// a staged set that survived its own action would leave the bar naming
/// rows the operator has already dealt with.
#[gpui::test]
fn archiving_the_marked_files_them_all_and_clears_the_staging(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    h.plumbing
        .push_rail(vec![session("aaaa1111"), session("bbbb2222")]);
    vcx.run_until_parked();

    click_with(vcx, "rail-aaaa1111", cmd());
    vcx.run_until_parked();
    click_with(vcx, "rail-bbbb2222", cmd());
    settle(vcx);
    let _ = h.drain();

    click(vcx, "rail-marked-archive");
    vcx.run_until_parked();
    let mut filed: Vec<String> = h
        .drain()
        .into_iter()
        .filter_map(|c| match c {
            Command::SetArchived { id, archived: true } => Some(id),
            _ => None,
        })
        .collect();
    filed.sort();
    assert_eq!(filed, vec!["aaaa1111".to_string(), "bbbb2222".to_string()]);
    assert!(
        !shown(vcx, "rail-marked"),
        "the staging outlived its own act"
    );
}

/// The direction mirrors the row button rather than inventing a second
/// rule: when every staged row is already filed, the same control brings
/// them back.
#[gpui::test]
fn a_set_that_is_already_filed_offers_to_bring_it_back(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();

    click_with(vcx, "rail-aaaa1111", cmd());
    vcx.run_until_parked();
    assert!(
        shown(vcx, "rail-marked-archive"),
        "a live row offered no filing"
    );

    // The engine reports it filed. The row leaves the list; the mark does
    // not, because the session still exists.
    let mut filed = session("aaaa1111");
    filed.archived = true;
    h.plumbing.push_rail(vec![filed]);
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "rail-aaaa1111"),
        "an archived row stayed on screen"
    );
    assert!(
        shown(vcx, "rail-marked-unarchive"),
        "the bar still offered to file away a set that is already filed"
    );
}

/// Clear unstages and does nothing else — the take-back for a range that
/// swept up more than was meant.
#[gpui::test]
fn clear_unstages_without_acting(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    // Seeded WITH a selection: an unselected rail emits its own
    // `Select` on a later frame, and this test's whole assertion is that
    // nothing was emitted.
    rail_with_selected(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222")],
        "bbbb2222",
    );

    click_with(vcx, "rail-aaaa1111", cmd());
    settle(vcx);
    let _ = h.drain();

    click(vcx, "rail-marked-clear");
    vcx.run_until_parked();
    let after = h.drain();
    assert!(
        after.is_empty(),
        "clearing the staging acted on it: {after:?}"
    );
    assert!(!shown(vcx, "mark-aaaa1111"));
    assert!(!shown(vcx, "rail-marked"));
}

/// A mark on a session the engine has forgotten is a number that cannot be
/// acted on. Pruned in the sessions observer, never the paint — the
/// precedent is `release_queued` and the reason is the same.
#[gpui::test]
fn a_mark_dies_with_the_session_it_was_on(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing
        .push_rail(vec![session("aaaa1111"), session("bbbb2222")]);
    vcx.run_until_parked();

    click_with(vcx, "rail-bbbb2222", cmd());
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-marked"));

    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "rail-marked"),
        "the bar went on counting a session that is gone"
    );
}

/// **Slice AG2 #5, and the clause that keeps it honest.**
///
/// A text filter is allowed to hide a session parked on a permission
/// question — it has to be, or typing a project name would surface
/// unrelated rows because they happened to be asking. What it may not do is
/// hide one silently, so the rail states the count. This walks both halves:
/// the narrowing, and the sentence that accounts for it.
#[gpui::test]
fn the_rail_filter_narrows_and_says_what_it_hid(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut keep = session("aaaa1111");
    keep.title = Some("eave sweep".into());
    let mut drop = session("bbbb2222");
    drop.title = Some("waku poll".into());
    let mut asking = session("cccc3333");
    asking.title = Some("waku approvals".into());
    asking.state = "awaiting_approval".into();
    asking.pending_approvals = 1;
    h.plumbing.push_rail(vec![keep, drop, asking]);
    vcx.run_until_parked();

    assert!(shown(vcx, "rail-filter"), "no filter control on a real list");
    assert!(
        shown(vcx, "rail-filter-placeholder"),
        "the control should read as closed before it is opened"
    );
    assert!(!shown(vcx, "rail-filter-note"), "nothing is filtered yet");
    assert!(shown(vcx, "rail-aaaa1111") && shown(vcx, "rail-cccc3333"));

    click(vcx, "rail-filter");
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-filter-field"), "the field would not open");

    // NOT "eave": the harness fixture's cwd is `/tmp/eave-harness`, so every
    // row matches that needle — correctly, since the path is on the card.
    // The needle has to be one only the intended row carries.
    vcx.simulate_input("sweep");
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-aaaa1111"), "the matching row went missing");
    assert!(!shown(vcx, "rail-bbbb2222"), "a non-matching row survived");
    assert!(
        !shown(vcx, "rail-cccc3333"),
        "an ASKING row survived a filter it does not match — the filter \
         would stop being a filter"
    );
    assert!(
        shown(vcx, "rail-filter-note"),
        "THE RAIL HID A SESSION WAITING ON THE OPERATOR AND SAID NOTHING"
    );

    // ⎋ closes the field and the whole list comes back — the filter is not
    // persisted, so there is no state left to strand the rows in.
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-cccc3333"), "the rows did not come back");
    assert!(!shown(vcx, "rail-filter-note"));
    assert!(shown(vcx, "rail-filter-placeholder"), "the field would not close");
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
    // 02c07d8 deleted the note and left the reason to the control that
    // undoes it. That is not a weakening: "2 archived · show" states the
    // count AND offers the way out on one line, where the note and the
    // toggle used to say it between them. The eternal-skeleton rule is
    // intact; what carries it moved.
    assert!(
        shown(vcx, "rail-archived"),
        "an all-archived rail said nothing at all — no note, and no toggle \
         either"
    );
    assert_eq!(
        archived_toggle_label(2, false),
        "2 archived · show",
        "and what it said did not name how many are filed away, which is \
         the reason the rail looks empty"
    );

    click(vcx, "rail-archived");
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-aaaa1111") && shown(vcx, "rail-bbbb2222"));
    assert_eq!(
        archived_toggle_label(2, true),
        "2 archived · hide",
        "the way back OUT is not offered once the archived rows are showing"
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
    // The ceiling lives in the footer's library menu now; the fact under
    // test is unchanged — `library_menu` computes over every session the
    // engine reports, never the filtered list.
    open_library_menu(vcx);
    assert!(
        shown(vcx, "tab-menu-memory-fact"),
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
    vcx.run_until_parked();
    open_files_face(vcx);

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
fn pressing_plus_on_projects_opens_a_new_tab(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert!(!shown(vcx, "launcher"), "the launcher was already up");
    let _ = h.drain();

    // Slice NC1: one door. `+ new` used to ask for the launcher alone
    // (Q2); a new tab IS the launcher now, so this is ⌘T's act — the tab
    // and the omnibox with it.
    click(vcx, "new-session");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::NewTab { .. }))),
        "`+ new` emitted {cmds:?} rather than opening a tab",
    );
    assert!(
        !cmds.iter().any(|c| matches!(c, Command::NewSession)),
        "`+ new` still asks for Q2's launcher: {cmds:?}"
    );
    assert!(
        shown(vcx, "omnibox"),
        "`+ new` opened a tab with no way to aim it"
    );
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
    open_files_face(vcx);

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
    open_files_face(vcx);
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
    open_files_face(vcx);
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
    // Six characters of THIS name's own ink — which is what "about six
    // characters" meant when it was first written down as the literal 48.
    //
    // Rows gained `RAIL_HOVER_PAD` after that literal was set: 6 px of card
    // padding on each side, so the hover wash is not edge-to-edge. The name
    // pays for the left one, and the strip now reaches 28 px into the ink
    // where the comment above still says 21. The literal went stale by 2 px;
    // the rule it stood for did not, and a row showing 46 px of a 74 px name
    // is still a row you can identify.
    //
    // Derived from the NAME and deliberately NOT from the strip: a floor
    // computed from the strip's own width would move whenever the strip did,
    // which is precisely the drift this assertion exists to catch. It is an
    // average advance rather than the first six glyphs shaped, which is the
    // same approximation the literal was.
    let six_chars = ink / "claude code".chars().count() as f32 * 6.;
    assert!(
        visible >= six_chars,
        "the widest strip leaves only {visible:?} of the name showing, under \
         the {six_chars:?} that six of its characters need; a row you cannot \
         identify is a row you archive by mistake"
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
    // Six characters of THIS name, same derivation and same reason as the
    // `claude code` case above — the literal 48 was set before rows carried
    // `RAIL_HOVER_PAD`, and a title is longer, so its six characters are a
    // different number of pixels from `claude code`'s. Two fixtures sharing
    // one literal was part of how the staleness hid.
    let six_chars = long_ink / "package the dmg".chars().count() as f32 * 6.;
    assert!(
        visible >= six_chars,
        "at a real title the widest strip leaves only {visible:?} of the \
         name showing, under the {six_chars:?} that six of its characters \
         need"
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
            h <= px(88.),
            "{row} is {h:?} tall; the card is two lines (title, \
             location), not the four-line disease"
        );
    }

    // Both ranks are actually there — a ceiling met by dropping a fact
    // is not the same slice. (Slice NC2: the mark is on the title line.)
    for sel in [
        "chip-aaaa1111",
        "label-aaaa1111",
        "cwd-aaaa1111",
        "chip-bbbb2222",
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

/// **Slice RS1 — the rail corner states an age, in ONE unit.**
///
/// The age itself is not this slice's — the rail has rendered
/// `session_age_ms` since the row was rebuilt, and RS1 arrived believing it
/// did not, because RS1 was written against a tree that had been rolled
/// back 15 commits. What RS1 changed is the FORMAT: the corner had been
/// drawing `fmt_elapsed`, which is the transcript's turn clock (`2m 05s`,
/// `1h 03m`) and is right where somebody watches it tick — not where
/// fifteen of them are glanced down a 228 px rail.
///
/// Both halves are asserted, because the seconds are the actual defect: a
/// corner whose last character changes every second, on every row, is
/// motion nobody asked for on a list people read at rest.
#[gpui::test]
fn the_rail_corner_states_an_age_in_one_unit(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut s = session("aaaa1111");
    s.last_activity_ms = Some(
        eave_core::journal::now_ms().saturating_sub(2 * 60 * 60 * 1_000 + 5 * 60 * 1_000),
    );
    h.plumbing.push_rail(vec![s]);
    vcx.run_until_parked();

    let age = vcx.debug_bounds("age-aaaa1111").expect("the row states no age");
    // Two hours and five minutes. `fmt_elapsed` renders that as `2h 05m`;
    // `coarse_age` renders `2h`. The widths are what a layout can see, so
    // that is what this measures — at real metrics, against the two
    // strings the two formatters actually produce.
    let one_unit = real_text_width(&"2h", theme::SANS, theme::MICRO_SIZE);
    let two_units = real_text_width(&"2h 05m", theme::SANS, theme::MICRO_SIZE);
    assert!(
        age.size.width < two_units,
        "the corner is {:?} wide — `2h 05m` needs {two_units:?} and `2h` \
         needs {one_unit:?}, so it is still drawing the turn clock",
        age.size.width
    );
}

/// **Slice RS1 — a row that moves is painted where it was, and the list
/// underneath it does not move at all.**
///
/// Both halves are the test. The first is the feature: rows used to jump
/// when a session arrived above them. The second is the constraint the
/// brief put on it and the reason the replay sweep is this stage's gate —
/// the glide is a PAINT offset, so no row's height, no row's box and no
/// distance between two rows may change while it runs. A glide implemented
/// with margins or with a spacer would pass the first assertion and fail
/// the third.
#[gpui::test]
fn a_reordered_row_glides_and_the_list_underneath_it_does_not_move(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(vec![session("aaaa1111"), session("bbbb2222")]);
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(16));

    let at = |sel: &'static str, vcx: &mut VisualTestContext| {
        vcx.debug_bounds(sel).unwrap_or_else(|| panic!("{sel} did not render"))
    };
    let a_before = at("rail-aaaa1111", vcx);
    let b_before = at("rail-bbbb2222", vcx);
    let pitch_before = b_before.origin.y - a_before.origin.y;

    // A third session arrives. `fold_rail` puts an arrival at the TOP and
    // leaves every surviving row in the order it already had, so both
    // existing rows are pushed down by exactly one row.
    h.plumbing.push_rail(vec![
        session("cccc3333"),
        session("aaaa1111"),
        session("bbbb2222"),
    ]);
    vcx.run_until_parked();

    let a_mid = at("rail-aaaa1111", vcx);
    let b_mid = at("rail-bbbb2222", vcx);
    assert!(
        (a_mid.origin.y - a_before.origin.y).abs() <= px(1.),
        "the row jumped from {:?} to {:?} the frame the arrival landed — \
         that IS the jump this slice exists to remove",
        a_before.origin.y,
        a_mid.origin.y
    );
    assert!(
        (b_mid.origin.y - b_before.origin.y).abs() <= px(1.),
        "the second surviving row jumped to {:?}",
        b_mid.origin.y
    );
    // ...and the list is rigid while it does it.
    assert_eq!(
        b_mid.origin.y - a_mid.origin.y,
        pitch_before,
        "the two rows are {:?} apart mid-glide and were {:?} apart before \
         — the glide is moving layout, not paint",
        b_mid.origin.y - a_mid.origin.y,
        pitch_before
    );
    assert_eq!(
        (a_mid.size, b_mid.size),
        (a_before.size, b_before.size),
        "a row changed size while gliding"
    );

    // It is an animation, not a hold: the window asks for frames, and when
    // it stops asking the rows are where the layout always had them.
    let frames = drive(&h, vcx, Duration::from_millis(16));
    assert!(
        frames > 1,
        "the reorder settled in {frames} frame(s) — nothing glided"
    );
    let a_after = at("rail-aaaa1111", vcx);
    let b_after = at("rail-bbbb2222", vcx);
    assert!(
        a_after.origin.y > a_before.origin.y,
        "the arrival above it never actually pushed the row down: {:?} then \
         {:?}",
        a_before.origin.y,
        a_after.origin.y
    );
    assert_eq!(
        b_after.origin.y - a_after.origin.y,
        pitch_before,
        "the rows landed at a different pitch than they set off at"
    );
    assert_eq!((a_after.size, b_after.size), (a_before.size, b_before.size));
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
    // Let the cluster's entrance land before anything measures it. The rise
    // is a live `top` offset while `motion::entrance_t` is below 1, so a
    // bounds read on a mid-entrance cluster is reading the animation, not
    // the layout — and two reads either side of an unrelated change would
    // differ for a reason that has nothing to do with the change.
    drive(h, vcx, Duration::from_millis(10));
    s
}

/// An empty selected session is a centered cluster: circular logo, then
/// the composer card, then the cwd line. The Slice AN card (`empty-title`,
/// `empty-key`, `empty-action`) is gone.
#[gpui::test]
fn the_empty_state_announces_orients_and_offers(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", Some("opus"));

    assert!(
        shown(vcx, "transcript-empty"),
        "a selected session with an empty journal shows no empty state at all"
    );
    assert!(
        shown(vcx, "empty-logo"),
        "the empty session has no logo placeholder"
    );
    assert!(
        shown(vcx, "empty-cluster"),
        "the logo+composer cluster never mounted"
    );
    for part in ["composer-hint", "send", "composer-attach", "composer-model"] {
        assert!(
            shown(vcx, part),
            "the composer card is missing {part} under the logo"
        );
    }
    assert!(
        shown(vcx, "empty-where"),
        "the cwd line is gone — work in this project is no longer reachable"
    );
    for retired in ["empty-title", "empty-key", "empty-action"] {
        assert!(
            !shown(vcx, retired),
            "{retired} is the Slice AN card this layout replaced"
        );
    }
}

/// The placeholder is a circle: equal width and height (the round fill is
/// `rounded_full` on that same box). A square that is not round, or a disc
/// that is a stretched oval, is not the mock.
#[gpui::test]
fn the_empty_states_headline_is_at_the_display_rank(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);

    let logo = vcx.debug_bounds("empty-logo").expect("the logo placeholder");
    assert_eq!(
        logo.size.width, logo.size.height,
        "the logo is not circular: {:?} × {:?}",
        logo.size.width, logo.size.height
    );
    // A 1 px speck or a pane-filling wash would still be square. The
    // disc is a control-sized mark, not a backdrop — 24 px is smaller
    // than any control in the composer, 120 px is larger than the
    // resting composer bar.
    assert!(
        logo.size.width > px(24.) && logo.size.width < px(120.),
        "the logo is {:?} across, which is not a placeholder disc",
        logo.size.width
    );
}

/// The composer sits *below* the logo, with a real gap. Stacking them on
/// top of each other, or putting the card above the disc, is not the mock.
#[gpui::test]
fn the_empty_state_clears_the_composer_it_points_at(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", Some("opus"));

    let logo = vcx.debug_bounds("empty-logo").expect("the logo");
    let card = vcx.debug_bounds("composer-card").expect("the composer card");
    let logo_bottom = logo.origin.y + logo.size.height;
    assert!(
        card.origin.y > logo_bottom,
        "the composer starts at {:?} and the logo ends at {logo_bottom:?} — \
         they overlap or the card is above the disc",
        card.origin.y
    );
    assert!(
        card.origin.y - logo_bottom > px(8.),
        "only {:?} between the logo and the composer",
        card.origin.y - logo_bottom
    );
}

/// The cluster sits near the middle of the well, not pinned to the top
/// third the way the Slice AN card was. Literals, not the flex-grow
/// constants: a leftover 4:5 spacer pair would land around 0.44 above.
#[gpui::test]
fn the_empty_state_sits_above_centre_but_not_at_the_top(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", Some("opus"));

    let pane = vcx.debug_bounds("transcript-empty").expect("the slot");
    let cluster = vcx.debug_bounds("empty-cluster").expect("the cluster");

    let pane_mid = f32::from(pane.origin.y) + f32::from(pane.size.height) / 2.0;
    let cluster_mid = f32::from(cluster.origin.y) + f32::from(cluster.size.height) / 2.0;
    let well_h = f32::from(pane.size.height);
    assert!(
        well_h > 0.0,
        "the empty overlay has no height, so centering is unmeasurable"
    );
    let offset = (cluster_mid - pane_mid).abs() / well_h;
    assert!(
        offset < 0.20,
        "the cluster mid is {offset:.3} of the well away from centre \
         (pane mid {pane_mid}, cluster mid {cluster_mid})"
    );
    // The retired card sat in the top third. A cluster whose mid is still
    // up there is that card with a logo glued on.
    let from_top = (cluster_mid - f32::from(pane.origin.y)) / well_h;
    assert!(
        from_top > 0.35,
        "the cluster mid is {from_top:.3} of the way down the well — \
         still the leftover upper-third card"
    );
}

/// **Gone the instant a turn exists**, and a turn is the operator's
/// message alone — not the agent's reply to it. The logo leaves; the
/// composer stays, now docked at the bottom of the well.
#[gpui::test]
fn the_empty_state_is_gone_the_instant_a_turn_exists(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    assert!(shown(vcx, "transcript-empty"));
    assert!(shown(vcx, "empty-logo"));

    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(1, "the first turn")],
    });
    vcx.run_until_parked();

    assert!(
        !shown(vcx, "transcript-empty"),
        "one directive landed and the cluster is still claiming nothing was sent"
    );
    for part in [
        "empty-logo",
        "empty-cluster",
        "empty-where",
        "empty-title",
        "empty-key",
        "empty-action",
    ] {
        assert!(
            !shown(vcx, part),
            "{part} outlived the cluster it belongs to"
        );
    }
    assert!(shown(vcx, "composer-hint"));
    assert!(shown(vcx, "send"));
    assert!(shown(vcx, "composer-attach"));

    let well = vcx.debug_bounds("content-well").expect("the well");
    let send = vcx.debug_bounds("send").expect("the send disc");
    let from_top = (f32::from(send.origin.y) - f32::from(well.origin.y))
        / f32::from(well.size.height);
    assert!(
        from_top > 0.55,
        "after the first turn the composer is still in the empty-cluster \
         slot (send is {from_top:.3} of the way down the well)"
    );
}

/// The cluster and the launcher answer different questions and can be up
/// in two windows at once. They are never on screen together (the launcher
/// owns "no session"; the cluster owns "a session with no turns").
#[gpui::test]
fn the_empty_state_and_the_launcher_are_never_the_same_screen(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    assert!(shown(vcx, "launcher"), "a bare boot is the launcher");
    // Slice NC1: the launcher draws the SAME cluster (disc, composer,
    // where-line) — but as its own overlay, never the empty-session view
    // stacked under it.
    assert!(shown(vcx, "empty-logo"), "the launcher lost the disc");
    assert!(
        !shown(vcx, "transcript-empty"),
        "the launcher and the empty-session cluster must never stack"
    );

    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    assert!(!shown(vcx, "launcher"));
    assert!(
        shown(vcx, "transcript-empty"),
        "selecting a session lost the cluster"
    );
    assert!(
        shown(vcx, "empty-logo"),
        "selecting a session lost the cluster"
    );
    let logo = vcx.debug_bounds("empty-logo").expect("the logo");
    assert_eq!(
        logo.size.width, logo.size.height,
        "the empty-session mark is not a disc — it will be mistaken for the launcher"
    );
}

/// The cwd line routes through the seam that already existed
/// (`open_in_system`) rather than through a command invented for it.
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
/// set by the send button and not by the hint, so a hint that comes and
/// goes with the draft must leave every box in the cluster exactly where
/// it was — otherwise the first character typed would shove the composer
/// and the logo with it.
#[gpui::test]
fn emptying_the_composer_hint_moves_nothing(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);

    let probes = ["composer-hint", "send", "empty-logo", "composer-attach"];
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

/// A draft typed into the empty cluster is still there after the first
/// record docks the composer, and the field still owns keys.
#[gpui::test]
fn a_typed_draft_survives_the_first_turn_docking(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    vcx.simulate_input("hello");
    vcx.run_until_parked();
    assert_eq!(composer_draft(vcx, &h), "hello");
    assert!(shown(vcx, "empty-logo"));

    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(1, "the first turn")],
    });
    vcx.run_until_parked();

    assert!(!shown(vcx, "empty-logo"));
    assert_eq!(
        composer_draft(vcx, &h),
        "hello",
        "docking the composer remounted the field and ate the draft"
    );
    vcx.simulate_input(" world");
    vcx.run_until_parked();
    assert_eq!(
        composer_draft(vcx, &h),
        "hello world",
        "the field lost the keyboard when it moved to the bottom"
    );
}

/// Shrinking the window must keep the cluster inside the well — not clip
/// it off the bottom — and still centred. The first headed walk showed an
/// empty well at a smaller size: the overlay had no definite height, so
/// the composer left the visible rect.
#[gpui::test]
fn the_empty_cluster_stays_in_the_well_when_the_window_shrinks(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", Some("opus"));
    assert!(shown(vcx, "empty-logo"));
    assert!(shown(vcx, "composer-card"));

    vcx.simulate_resize(size(px(theme::WINDOW_MIN_W), px(theme::WINDOW_MIN_H)));
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(10));

    let well = vcx.debug_bounds("transcript-empty").expect("the well");
    let card = vcx
        .debug_bounds("composer-card")
        .expect("composer after resize");
    let send = vcx.debug_bounds("send").expect("send after resize");
    assert!(
        well.size.height > px(0.) && well.size.width > px(0.),
        "the empty pane collapsed to {well:?}"
    );
    let well_bottom = well.origin.y + well.size.height;
    let well_right = well.origin.x + well.size.width;
    for (name, b) in [("composer-card", card), ("send", send)] {
        assert!(
            b.origin.y >= well.origin.y - px(1.)
                && b.origin.y + b.size.height <= well_bottom + px(1.)
                && b.origin.x >= well.origin.x - px(1.)
                && b.origin.x + b.size.width <= well_right + px(1.),
            "{name} left the well after resize: {b:?} vs well {well:?}"
        );
    }
    let card_mid = f32::from(card.origin.y) + f32::from(card.size.height) / 2.0;
    let well_mid = f32::from(well.origin.y) + f32::from(well.size.height) / 2.0;
    let offset = (card_mid - well_mid).abs() / f32::from(well.size.height);
    assert!(
        offset < 0.30,
        "after resize the composer is {offset:.3} of the well from centre"
    );
    assert!(
        shown(vcx, "empty-logo"),
        "the min window hid the logo — the disc fits the well leftover at WINDOW_MIN_H"
    );
    let logo = vcx.debug_bounds("empty-logo").expect("logo after resize");
    assert!(
        logo.origin.y >= well.origin.y - px(1.)
            && logo.origin.y + logo.size.height <= well_bottom + px(1.),
        "the logo left the well after resize: {logo:?} vs well {well:?}"
    );
}

/// Distance from the pane header's bottom to the cluster's vertical mid.
/// The well is the window leftover; growing or collapsing the rail must
/// not change this. A cluster that tracks the rail's min-content sits
/// lower as folders open and clips its logo when they shut.
fn cluster_mid_below_header(vcx: &mut VisualTestContext) -> f32 {
    let header = vcx
        .debug_bounds("pane-header")
        .expect("pane-header");
    let cluster = vcx
        .debug_bounds("empty-cluster")
        .expect("empty-cluster");
    let header_bottom = f32::from(header.origin.y) + f32::from(header.size.height);
    let cluster_mid = f32::from(cluster.origin.y) + f32::from(cluster.size.height) / 2.0;
    cluster_mid - header_bottom
}

/// Open folders on the rail must not move the empty cluster or hide the
/// logo. The headed walk: full-screen it sat in the middle; a smaller
/// window with Projects and Sessions open put it too low, then blank;
/// collapsing those zones recentred it and dropped the disc.
#[gpui::test]
fn the_empty_cluster_ignores_how_tall_the_rail_is(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let selected = empty_session(&h, vcx, "/tmp/eave-harness/repo", Some("opus"));
    assert!(shown(vcx, "empty-logo"));
    let before = cluster_mid_below_header(vcx);

    let mut rail = vec![selected];
    for i in 0..24 {
        let mut s = session(&format!("sess{i:04}"));
        s.last_seq = 1;
        rail.push(s);
    }
    h.plumbing.push_rail(rail);
    h.plumbing.push_bookmarks(eave_wire::BookmarksSnapshot {
        nodes: (0..20)
            .map(|i| eave_wire::BookmarkNode {
                bookmark: eave_wire::Bookmark {
                    id: format!("f{i}"),
                    parent: None,
                    title: Some(format!("folder {i}")),
                    kind: eave_wire::BookmarkKind::Folder,
                    created_ts_ms: 0,
                },
                depth: 0,
                children: 0,
            })
            .collect(),
        ..Default::default()
    });
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(10));

    assert!(
        shown(vcx, "empty-logo"),
        "filling the rail hid the logo"
    );
    let grown = cluster_mid_below_header(vcx);
    assert!(
        (grown - before).abs() < 8.0,
        "open Sessions + Folders moved the cluster {:+.1} px from the header \
         (the well is tracking the rail, not the window)",
        grown - before
    );

    // Folders first: they live in the rail-tabs strip whose definite
    // height grew, so collapsing them gives Sessions room to be hit.
    click(vcx, "zone-folders");
    vcx.run_until_parked();
    click(vcx, "zone-sessions");
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(10));

    assert!(
        shown(vcx, "empty-logo"),
        "collapsing the rail zones hid the logo"
    );
    let shut = cluster_mid_below_header(vcx);
    assert!(
        (shut - before).abs() < 8.0,
        "collapsing Sessions + Folders moved the cluster {:+.1} px from the header",
        shut - before
    );
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

/// Collapsing the rail takes PAGES with it — the tabs live in the rail
/// and nowhere else (ID2b′, back to Slice D1's claim) — and leaves the
/// toggle that reopens it; reopened, a page row still switches, and it
/// switches by asking, never by moving the shell's own selection.
#[gpui::test]
fn collapsing_the_rail_leaves_a_way_to_switch_sessions(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let ids = ["p1", "p2", "p3", "p4", "p5"];
    let tabs = || -> Vec<TabInfo> {
        ids.iter()
            .map(|id| page_tab(id, Some("https://a.test/"), Some(id)))
            .collect()
    };
    strip_tabs(&h, vcx, tabs(), Some("p1"));

    assert!(shown(vcx, "rail-aaaa1111"), "the rail starts open");
    assert!(shown(vcx, "tab-strip"), "PAGES is hidden while the rail is open");

    click(vcx, "rail-toggle");
    vcx.run_until_parked();
    assert!(!shown(vcx, "rail-aaaa1111"), "the rail did not collapse");
    assert!(
        !shown(vcx, "tab-strip"),
        "PAGES survived the collapse — the tabs live in the rail, and a \
         second tab list drawn somewhere else is ID2b's strip growing back"
    );
    assert!(
        shown(vcx, "rail-toggle"),
        "the control that reopens the rail is inside the rail — closing the \
         menu is a one-way door"
    );

    click(vcx, "rail-toggle");
    vcx.run_until_parked();
    assert!(shown(vcx, "tab-strip"), "the rail did not reopen");
    let _ = h.drain();
    click(vcx, "tab-p4");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "p4"
        )),
        "clicking a page row emitted {cmds:?} — a list you cannot switch \
         with is the dead end wearing a nicer shape"
    );

    click(vcx, "rail-toggle");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "rail-aaaa1111"),
        "the rail did not collapse again"
    );
    click(vcx, "rail-toggle");
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-aaaa1111"), "the rail did not come back");
    assert!(shown(vcx, "tab-strip"), "PAGES vanished when the rail came back");
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

/// A session tab's name sits inside its own padding and the tab keeps a
/// fixed width whatever the name's length (Slice C5, then ID2a for the
/// name). ID2b′: the one place a session TAB is still drawn is the split
/// pill, so that is where this is measured — at half a row's width, which
/// is why the C5 claim that the commonest label fits unclipped is not
/// made here: the pill truncates by design, and the row above it says the
/// whole name.
#[gpui::test]
fn a_tab_shows_a_realistic_session_name(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let pill = |h: &Harness, vcx: &mut VisualTestContext, info: SessionInfo| {
        rail_with_selected(h, vcx, vec![info], "aaaa1111");
        strip_tabs(
            h,
            vcx,
            vec![
                session_tab("aaaa1111"),
                page_tab("p1", Some("https://a.test/"), Some("A")),
            ],
            Some("aaaa1111"),
        );
        h.window
            .update(vcx, |shell, _, cx| shell.split_tabs("p1", "aaaa1111", true, cx))
            .unwrap();
        vcx.run_until_parked();
    };
    pill(&h, vcx, session("aaaa1111"));

    let label = vcx
        .debug_bounds("tab-label-aaaa1111")
        .expect("the tab's name");
    let tab = vcx.debug_bounds("tab-aaaa1111").expect("the tab");
    assert!(
        label.origin.x >= tab.origin.x + px(theme::SPACE_1),
        "the name starts {:?} into a tab that begins at {:?} — it is \
         outside the tab's own padding",
        label.origin.x - tab.origin.x,
        tab.origin.x
    );
    assert!(
        label.origin.x + label.size.width <= tab.origin.x + tab.size.width - px(theme::SPACE_1),
        "the name runs to {:?} and the tab ends at {:?} — the slot has \
         eaten its own padding, and the next thing added to a tab will be \
         on top of the name",
        label.origin.x + label.size.width,
        tab.origin.x + tab.size.width
    );

    let plain = tab.size.width;
    let mut long = session("aaaa1111");
    long.title = Some("chase the rail flake".into());
    pill(&h, vcx, long);
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

/// A page row is not a session row: it is `RAIL_TAB_H` tall and carries
/// none of the rail's freight — cwd, marks, the rename pen, the archive
/// control (Slice D1). ID2b′: measured on a loose page in PAGES against
/// the chat row in the tree, the two ranks the spine draws.
#[gpui::test]
fn a_tab_is_not_a_rail_row(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut loaded = session("aaaa1111");
    loaded.title = Some("the refactor".into());
    loaded.shared_with = vec!["bbbb2222".into(), "cccc3333".into()];
    loaded.worktree = Some(worktree("eave/aaaa1111", Some(2), true));
    loaded.permission_mode = Some("bypassPermissions".into());
    rail_with_selected(&h, vcx, vec![loaded], "aaaa1111");
    strip_tabs(
        &h,
        vcx,
        vec![page_tab("p1", Some("https://a.test/"), Some("A"))],
        Some("p1"),
    );
    for sel in ["cwd-aaaa1111", "marks-aaaa1111"] {
        assert!(
            shown(vcx, sel),
            "{sel} is not on the rail row to begin with"
        );
    }

    assert!(shown(vcx, "tab-p1"), "the page row did not mount");

    let tab = vcx.debug_bounds("tab-p1").expect("the page row");
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
                "{sel} is inside the page row's own box ({b:?} within {tab:?}) — \
                 a tab carrying the rail's freight needs the rail's room"
            );
        }
    }

    let row = vcx.debug_bounds("rail-aaaa1111").expect("the rail row");
    assert_eq!(tab.size.height, px(theme::RAIL_TAB_H), "a page row is one tab tall");
    assert!(
        tab.size.height < row.size.height,
        "the page row is {:?} tall and the chat row {:?} — a tab that has \
         grown to a row's height has grown to a row",
        tab.size.height,
        row.size.height
    );
}

/// Activating a page moves no page row: the rim is an inset shadow, so
/// the selection changing cannot re-lay the list out (Slice C5; ID2b′ on
/// PAGES).
#[gpui::test]
fn selecting_a_tab_moves_no_tab(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let tabs = || -> Vec<TabInfo> {
        ["p1", "p2", "p3"]
            .iter()
            .map(|id| page_tab(id, Some("https://a.test/"), Some(id)))
            .collect()
    };
    strip_tabs(&h, vcx, tabs(), Some("p1"));

    let boxes = |vcx: &mut VisualTestContext| -> Vec<gpui::Bounds<Pixels>> {
        ["tab-p1", "tab-p2", "tab-p3"]
            .iter()
            .map(|sel| vcx.debug_bounds(sel).unwrap_or_else(|| panic!("{sel}")))
            .collect()
    };
    let first = boxes(vcx);
    for (i, b) in first.iter().enumerate() {
        assert_eq!(
            b.size.height,
            px(theme::RAIL_TAB_H),
            "tab {i} is not the fixed height"
        );
        assert_eq!(b.size.width, first[0].size.width, "tab {i} width");
        assert_eq!(b.origin.x, first[0].origin.x, "tab {i} left edge");
    }

    strip_tabs(&h, vcx, tabs(), Some("p2"));
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

/// Many loose pages: every one of them has a row, and the one you are on
/// is always on screen.
///
/// PAGES is a fixed-height list that SCROLLS rather than an overflow
/// menu or a shrink-to-fit column. What that has to earn is two
/// properties, and this asserts both: nothing is dropped, and the active
/// page is never off the end (a list that says nothing about where you
/// are is a list you cannot navigate from).
#[gpui::test]
fn no_page_falls_off_the_end_of_pages(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let ids: Vec<String> = (0..16).map(|i| format!("p{i}")).collect();
    let tabs = || -> Vec<TabInfo> {
        ids.iter()
            .map(|id| page_tab(id, Some("https://a.test/"), Some(id)))
            .collect()
    };
    strip_tabs(&h, vcx, tabs(), Some("p0"));

    let strip = vcx.debug_bounds("tab-strip").expect("PAGES");
    // The fixture really does overflow — otherwise the two properties
    // below are asserted about a case that never arises.
    assert!(
        px(theme::RAIL_TAB_H) * (ids.len() as f32) > strip.size.height,
        "{} pages fit in {:?}, so this test is not measuring an \
         overflowing list at all",
        ids.len(),
        strip.size.height
    );
    const SELS: [&str; 16] = [
        "tab-p0", "tab-p1", "tab-p2", "tab-p3", "tab-p4", "tab-p5", "tab-p6", "tab-p7", "tab-p8",
        "tab-p9", "tab-p10", "tab-p11", "tab-p12", "tab-p13", "tab-p14", "tab-p15",
    ];
    let mut ys: Vec<Pixels> = Vec::new();
    for sel in SELS {
        let b = vcx
            .debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} has no row — a page with no row is unreachable"));
        ys.push(b.origin.y);
    }
    // In the engine's own order, top to bottom.
    for w in ys.windows(2) {
        assert!(w[1] > w[0], "PAGES is not in the engine's order: {ys:?}");
    }

    // The LAST page, active. Its row has to come to the operator.
    strip_tabs(&h, vcx, tabs(), Some("p15"));
    let strip = vcx.debug_bounds("tab-strip").expect("PAGES");
    let sel = vcx.debug_bounds("tab-p15").expect("the active page");
    assert!(
        sel.origin.y >= strip.origin.y
            && sel.origin.y + sel.size.height <= strip.origin.y + strip.size.height,
        "the active page sits at {:?}..{:?} and PAGES runs {:?}..{:?} — the \
         page you are reading is off the end of the only list that could \
         take you anywhere else",
        sel.origin.y,
        sel.origin.y + sel.size.height,
        strip.origin.y,
        strip.origin.y + strip.size.height
    );
}

/// A session tab identifies its provider the way a row does — a real
/// mark on an opaque plate, initials as the fallback — and the SLOT is
/// the same box whether a provider has a logo or not, which is what keeps
/// the name slot beside it identical across providers. ID2b′: measured in
/// the split pill, the one place a session tab is drawn; one pill per
/// provider, since two session tabs never share a split.
///
/// It cannot assert the mark is visible. `TestAppContext` hardcodes
/// `Arc::new(())` as its asset source, so every icon in this file lays out
/// its box and paints nothing.
#[gpui::test]
fn a_tab_wears_the_providers_mark_on_the_same_plate_a_row_does(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let pill = |h: &Harness, vcx: &mut VisualTestContext, provider: &str| {
        let mut info = session("aaaa1111");
        info.provider = provider.into();
        rail_with_selected(h, vcx, vec![info], "aaaa1111");
        strip_tabs(
            h,
            vcx,
            vec![
                session_tab("aaaa1111"),
                page_tab("p1", Some("https://a.test/"), Some("A")),
            ],
            Some("aaaa1111"),
        );
        h.window
            .update(vcx, |shell, _, cx| shell.split_tabs("p1", "aaaa1111", true, cx))
            .unwrap();
        vcx.run_until_parked();
        assert!(shown(vcx, "tab-chip-aaaa1111"), "tab-chip-aaaa1111 did not render");
        (
            vcx.debug_bounds("tab-label-aaaa1111").unwrap().size.width,
            vcx.debug_bounds("tab-chip-aaaa1111").unwrap().size.width,
        )
    };
    let (known_label, chip) = pill(&h, vcx, "claude-code");
    let (unknown_label, _) = pill(&h, vcx, "some-agent");
    assert_eq!(
        known_label, unknown_label,
        "a provider with a logo and one without gave their names different \
         slots — the two chips are supposed to be interchangeable"
    );
    assert_eq!(provider_identity("some-agent").0, "SA");
    assert!(
        chip <= px(12.),
        "the provider mark is {chip:?} on a tab. It is 12 px inside an 18 px \
         plate: a mark at the plate's full size reads as a crop, and every \
         pixel it takes is one the name does not get"
    );
}

/// **Nine pages fit PAGES without scrolling.**
///
/// This was `five_sessions_fit_the_strip_without_scrolling`, then
/// `nine_tabs_fit_the_rail_without_scrolling` when D1 turned the axis,
/// then ID2b's header-strip claim — and now ID2b′ puts the tabs back in
/// the rail as PAGES and the number is nine again, the whole functional
/// case for a vertical list. Measured on RENDERED elements, never
/// computed from a constant — the original test's rule, kept through
/// three axis changes.
#[gpui::test]
fn nine_pages_fit_the_rail_without_scrolling(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let ids = ["p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9"];
    strip_tabs(
        &h,
        vcx,
        ids.iter()
            .map(|id| page_tab(id, Some("https://a.test/"), Some(id)))
            .collect(),
        Some("p1"),
    );

    let strip = vcx.debug_bounds("tab-strip").expect("PAGES");
    for sel in [
        "tab-p1", "tab-p2", "tab-p3", "tab-p4", "tab-p5", "tab-p6", "tab-p7", "tab-p8", "tab-p9",
    ] {
        let tab = vcx.debug_bounds(sel).unwrap_or_else(|| panic!("{sel}"));
        assert!(
            tab.origin.y >= strip.origin.y
                && tab.origin.y + tab.size.height <= strip.origin.y + strip.size.height,
            "{sel} runs {:?}..{:?} and PAGES runs {:?}..{:?} — nine pages \
             have to be on screen at the default window size",
            tab.origin.y,
            tab.origin.y + tab.size.height,
            strip.origin.y,
            strip.origin.y + strip.size.height
        );
    }
    let rail = vcx.debug_bounds("rail").expect("the rail");
    assert!(
        strip.origin.x + strip.size.width <= rail.origin.x + rail.size.width,
        "PAGES is not inside the rail"
    );
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
                fast_lane: None,
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

/// MonoCode's composer chrome is on the empty session: project, access,
/// attach, model, send. The card is the same entity as the docked one.
#[gpui::test]
fn the_composer_carries_monocode_chrome(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    for part in [
        "composer-card",
        "composer-project",
        "composer-access",
        "composer-attach",
        "composer-model",
        "composer-hint",
        "send",
    ] {
        assert!(shown(vcx, part), "{part} missing from the composer");
    }
    assert!(
        !shown(vcx, "composer-stop"),
        "stop is a busy-turn control; an idle session must not show it"
    );
}

/// Stop is the composer's interrupt, and it does not kill the session.
#[gpui::test]
fn the_composer_stop_interrupts_a_running_turn(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut s = session("aaaa1111");
    s.state = "running".into();
    s.live = true;
    s.last_seq = 1;
    h.plumbing.push_rail(vec![s]);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert!(shown(vcx, "composer-stop"), "a running turn has no Stop");
    click(vcx, "composer-stop");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(c, Command::Stop)),
        "Stop must emit Command::Stop, not Kill: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| matches!(c, Command::Kill)),
        "composer Stop must not kill the session: {cmds:?}"
    );
}

/// Access picker lists MonoCode's four modes and choosing one is a click,
/// not a spawn-flag rewrite.
#[gpui::test]
fn the_access_picker_offers_the_four_modes(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    click(vcx, "composer-access");
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(200));
    assert!(shown(vcx, "overlay-menu") || shown(vcx, "menu-row-0"));
    for sel in ["menu-row-0", "menu-row-1", "menu-row-2", "menu-row-3"] {
        assert!(shown(vcx, sel), "{sel} is missing from the access picker");
    }
    click(vcx, "menu-row-3");
    vcx.run_until_parked();
    vcx.read_window(&h.window, |shell, cx| {
        assert_eq!(
            shell.read(cx).composer.read(cx).runtime_mode,
            crate::access::RuntimeMode::FullAccess
        );
    })
    .unwrap();
}

// ── Slice TN1 — the composer's effort and speed chips ─────────────────────

/// A chip is drawn only for a knob the provider's CLI takes (the
/// registry's own `TUNING`): Claude gets both, Grok the gauge alone,
/// Hermes neither. A control that cannot do anything is a lie.
#[gpui::test]
fn the_composer_wears_effort_and_speed_only_where_the_cli_takes_them(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    assert!(shown(vcx, "composer-effort"), "Claude takes --effort");
    assert!(shown(vcx, "composer-speed"), "Claude has a fast lane");
    assert!(shown(vcx, "composer-speed-off"), "the bolt states its side");
    assert!(shown(vcx, "composer-effort-gauge-0"), "the default draws a hollow gauge");

    for (provider, effort, speed) in [("grok", true, false), ("hermes", false, false)] {
        let mut s = session("bbbb2222");
        s.provider = provider.into();
        s.last_seq = 0;
        h.plumbing.push_rail(vec![s]);
        h.plumbing.push_feed(FeedEvent::Session {
            id: "bbbb2222".into(),
        });
        vcx.run_until_parked();
        assert_eq!(shown(vcx, "composer-effort"), effort, "{provider}: effort chip");
        assert_eq!(shown(vcx, "composer-speed"), speed, "{provider}: speed chip");
    }
}

/// The effort menu offers Default then the CLI's own words, and choosing
/// one is a `Command::Tune` on the selected session — never a spawn-flag
/// rewrite and never a new session.
#[gpui::test]
fn choosing_an_effort_level_tunes_the_selected_session(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    let _ = h.drain();
    // Slice NC3: a click STEPS the level, no menu. From the CLI's own
    // default the first step lands on Extra high.
    click(vcx, "composer-effort");
    vcx.run_until_parked();
    assert!(!shown(vcx, "menu-row-0"), "the effort chip opened a menu");
    let cmds = h.drain();
    match cmds.as_slice() {
        [Command::Tune { id, tuning }] => {
            assert_eq!(id, "aaaa1111");
            assert_eq!(tuning.effort.as_deref(), Some("xhigh"));
            assert!(!tuning.fast, "the speed knob must ride along unchanged");
        }
        other => panic!("choosing a level emitted {other:?}, not one Tune"),
    }
    assert!(
        !cmds.iter().any(|c| matches!(c, Command::NewSession | Command::Launch { .. })),
        "a tuning change must not start a new session"
    );

    // The chip reads what the engine reports, not what was clicked.
    let mut s = session("aaaa1111");
    s.effort = Some("xhigh".into());
    h.plumbing.push_rail(vec![s]);
    vcx.run_until_parked();
    assert!(shown(vcx, "composer-effort-gauge-3"), "xhigh lights three of four bars");
}

/// **Slice TN2.** Picking another model of the SAME provider on a live
/// chat switches THAT chat — one `Tune` carrying the model, the other two
/// knobs unchanged — and starts nothing. Before this the pick opened a
/// new session (the agent was "fixed at open"); the operator: *"i can
/// switch models mid conversation in claude cli terminal and others.
/// your telling me i cant in eave?"*
#[gpui::test]
fn choosing_a_model_on_a_live_chat_switches_that_chat(cx: &mut TestAppContext) {
    let _turn = eave_core::capabilities::probe_test_lock();
    eave_core::capabilities::forget();
    let (mut h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    let mut s = session("aaaa1111");
    s.effort = Some("xhigh".into());
    s.fast = true;
    h.plumbing.push_rail(vec![s]);
    vcx.run_until_parked();
    let _ = h.drain();
    click(vcx, "composer-model");
    vcx.run_until_parked();
    assert!(shown(vcx, "model-picker"), "the model menu never opened");
    // Row 1 is Opus 5 — row 0 is Sonnet 5 (the same list the launcher test reads).
    click(vcx, "model-picker-row-1");
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(16));
    let cmds = h.drain();
    match cmds.as_slice() {
        [Command::Tune { id, tuning }] => {
            assert_eq!(id, "aaaa1111");
            assert_eq!(tuning.model.as_deref(), Some("claude-opus-5"));
            assert_eq!(
                tuning.effort.as_deref(),
                Some("xhigh"),
                "effort must ride along"
            );
            assert!(tuning.fast, "the fast lane must ride along");
        }
        other => panic!("picking a model on a live chat emitted {other:?}, not one Tune"),
    }
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::NewSession | Command::Launch { .. })),
        "a model switch must not start a new session"
    );
}

/// Choosing a level must carry the OTHER knob along unchanged: a session
/// on the fast lane that picks Max stays on the fast lane. (The live walk
/// came up on `max` with the fast lane off, once, unexplained — this pins
/// the client's half of that.)
#[gpui::test]
fn choosing_a_level_keeps_the_fast_lane_as_it_was(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    let mut s = session("aaaa1111");
    s.effort = Some("xhigh".into());
    s.fast = true;
    h.plumbing.push_rail(vec![s]);
    vcx.run_until_parked();
    assert!(shown(vcx, "composer-speed-on"));
    let _ = h.drain();
    // Slice NC3: one click up from Extra high is Max.
    click(vcx, "composer-effort");
    vcx.run_until_parked();
    match h.drain().as_slice() {
        [Command::Tune { tuning, .. }] => {
            assert_eq!(tuning.effort.as_deref(), Some("max"));
            assert!(tuning.fast, "picking a level dropped the fast lane");
        }
        other => panic!("picking Max emitted {other:?}"),
    }
}

/// The bolt is a toggle: one click, one Tune with the other value; and it
/// states its side as a selector, because geometry is all a test can see.
#[gpui::test]
fn the_speed_bolt_toggles_and_states_its_side(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    let _ = h.drain();
    click(vcx, "composer-speed");
    vcx.run_until_parked();
    match h.drain().as_slice() {
        [Command::Tune { id, tuning }] => {
            assert_eq!(id, "aaaa1111");
            assert!(tuning.fast);
            assert_eq!(tuning.effort, None);
        }
        other => panic!("the bolt emitted {other:?}"),
    }
    let mut s = session("aaaa1111");
    s.fast = true;
    h.plumbing.push_rail(vec![s]);
    vcx.run_until_parked();
    assert!(shown(vcx, "composer-speed-on"));
    assert!(!shown(vcx, "composer-speed-off"));
    click(vcx, "composer-speed");
    vcx.run_until_parked();
    match h.drain().as_slice() {
        [Command::Tune { tuning, .. }] => assert!(!tuning.fast, "second click turns it off"),
        other => panic!("the bolt emitted {other:?}"),
    }
}

/// At the launcher there is no session to tune yet, so the choice rides
/// the launch itself and lands on the session it makes.
/// Slice NC3: a new chat starts at Extra high (the operator's default),
/// and the launch carries it without a click.
#[gpui::test]
fn a_new_chat_starts_at_extra_high_effort(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    assert!(shown(vcx, "launcher"));
    assert!(shown(vcx, "composer-effort-gauge-3"), "the launcher's gauge is not at Extra high");
    let _ = h.drain();
    start_chat(vcx, "hello");
    let launch = h
        .drain()
        .into_iter()
        .find_map(|c| match c {
            Command::Launch { tuning, .. } => Some(tuning),
            _ => None,
        })
        .expect("no launch went out");
    assert_eq!(launch.effort.as_deref(), Some("xhigh"));
    assert!(!launch.fast);
}

/// Slice NC3 — the operator's report, verbatim: *"when i open a new tab
/// the chat pops up then opens the search bar like it should but the
/// screen instantly goes blank."* The engine's ⌘T tab arrives as CEF
/// left it: at `about:blank`, one history entry. That tab is blank, the
/// launcher stays, and no page is drawn under it.
#[gpui::test]
fn a_tab_the_engine_reports_at_about_blank_is_still_the_launcher(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let mut fresh = page_tab("blank", Some("about:blank"), None);
    fresh.history_len = 1;
    strip_tabs(
        &h,
        vcx,
        vec![fresh, session_tab("aaaa1111")],
        Some("blank"),
    );
    assert!(shown(vcx, "launcher"), "CEF's about:blank commit hid the launcher");
    assert!(shown(vcx, "composer-hint"));
    assert!(!shown(vcx, "page-absence"));

    // Asked to go somewhere, it is a page again.
    let mut going = page_tab("blank", Some("about:blank"), None);
    going.history_len = 1;
    going.pending = Some(eave_core::browser::tabs::PendingNav {
        url: "https://docs.rs/".into(),
        seq: 1,
        delta: None,
    });
    strip_tabs(&h, vcx, vec![going, session_tab("aaaa1111")], Some("blank"));
    assert!(!shown(vcx, "launcher"), "a navigating tab kept the launcher");
}

#[gpui::test]
fn a_launch_carries_the_tuning_chosen_before_it(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    assert!(shown(vcx, "launcher"));
    h.window
        .update(vcx, |shell, _, cx| {
            shell.set_effort(Some("high".into()), cx);
            shell.toggle_fast(cx);
        })
        .unwrap();
    vcx.run_until_parked();
    assert!(
        h.drain().is_empty(),
        "with no session selected, a tuning is held, not sent"
    );
    // NC1: the launcher is a chat — the send IS the start.
    start_chat(vcx, "hello");
    let cmds = h.drain();
    let launch = cmds
        .iter()
        .find_map(|c| match c {
            Command::Launch { tuning, .. } => Some(tuning.clone()),
            _ => None,
        })
        .expect("no launch went out");
    assert_eq!(launch.effort.as_deref(), Some("high"));
    assert!(launch.fast);
}

// ── Slice C5 — the tab strip and the omnibox ──────────────────────────────

/// A page row and a chat row share one sidebar (ID2b′, the successor to
/// C5's "page tabs and session tabs share one strip"). The page is a row
/// in PAGES with its plate and its close control; the chat is its rail
/// row in the tree; both are inside the rail, PAGES above the tree, and
/// the chat draws no second row as a tab.
#[gpui::test]
fn page_rows_and_chat_rows_share_one_sidebar(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
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

    let rail = vcx.debug_bounds("rail").expect("the rail");
    let inside = |b: gpui::Bounds<Pixels>| {
        b.origin.x >= rail.origin.x
            && b.origin.x + b.size.width <= rail.origin.x + rail.size.width
            && b.origin.y >= rail.origin.y
            && b.origin.y + b.size.height <= rail.origin.y + rail.size.height
    };
    let p1 = vcx.debug_bounds("tab-p1").expect("tab-p1 is not on the rail");
    let p2 = vcx.debug_bounds("tab-p2").expect("tab-p2 is not on the rail");
    let chat = vcx.debug_bounds("rail-aaaa1111").expect("the chat row");
    assert!(inside(p1) && inside(p2) && inside(chat), "not every row is inside the rail");
    assert!(
        p1.origin.y < p2.origin.y && p2.origin.y < chat.origin.y,
        "PAGES is not above the tree: p1={:?} p2={:?} chat={:?}",
        p1.origin.y,
        p2.origin.y,
        chat.origin.y
    );
    assert!(
        !shown(vcx, "tab-aaaa1111"),
        "the chat drew a tab row as well as its own row — one session, two rows"
    );
    assert_eq!(p1.size.height, p2.size.height);
    assert_eq!(p1.size.width, p2.size.width);
    assert!(shown(vcx, "tab-chip-p1"), "the page row has no plate");
    for sel in ["tab-close-p1", "tab-close-p2"] {
        assert!(shown(vcx, sel), "{sel} — a page you cannot close");
    }

    set_resident_for_test(None);
}

/// A page tab that has not been anywhere wears no favicon, no unloaded
/// mark and no plate — and its label starts where New Tab's `+` does,
/// because nothing is reserved for a mark it does not wear (Slice T1).
/// ID2b′: back on the rail's PAGES rows, where the pill's `+` column is
/// the reference again.
#[gpui::test]
fn a_blank_page_tab_wears_no_mark(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    set_resident_for_test(Some(&["p1", "p2", "p3"]));
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("about:blank"), None),
            page_tab("p2", Some("https://example.com/"), Some("Example Domain")),
            page_tab("p3", None, None),
        ],
        Some("aaaa1111"),
    );
    for sel in [
        "tab-chip-p1",
        "tab-chip-unloaded-p1",
        "tab-favicon-p1",
        "tab-chip-p3",
        "tab-chip-unloaded-p3",
    ] {
        assert!(!shown(vcx, sel), "{sel} — a blank tab grew a mark back");
    }
    assert!(
        shown(vcx, "tab-chip-p2") || shown(vcx, "tab-favicon-p2"),
        "the rule leaked onto a real page"
    );
    let x = |vcx: &mut VisualTestContext, sel: &'static str| {
        vcx.debug_bounds(sel).expect(sel).origin.x
    };
    assert_eq!(
        x(vcx, "tab-label-p1"),
        x(vcx, "tab-new-glyph"),
        "the blank label does not start on New Tab's + column"
    );
    assert!(
        x(vcx, "tab-label-p2") > x(vcx, "tab-label-p1"),
        "the marked row's label lost its column"
    );
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

/// Eviction recedes a PAGE tab whose page eave has thrown away; a
/// session tab has no page to evict and keeps its provider mark (Slice
/// E3). ID2b′: the session tab is drawn in the split pill, beside the
/// page it is split with; the page tab is the receded one.
#[gpui::test]
fn eviction_never_recedes_a_session_tab(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    set_resident_for_test(Some(&[]));
    strip_tabs(
        &h,
        vcx,
        vec![
            session_tab("aaaa1111"),
            page_tab("p1", Some("https://example.com/"), None),
        ],
        Some("aaaa1111"),
    );
    h.window
        .update(vcx, |shell, _, cx| shell.split_tabs("p1", "aaaa1111", true, cx))
        .unwrap();
    vcx.run_until_parked();

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

/// B5 — ⇧⌘R reaches Chromium, and it does NOT go through the engine.
///
/// Both halves matter. A hard reload that sent `BrowserCmd::Navigate` would
/// look right on screen and be the wrong thing: it would push the engine's
/// nav stack and re-fetch through the cache, which is precisely what ⇧⌘R
/// exists not to do. So this asserts the CEF call happened AND that the
/// command channel stayed silent.
#[gpui::test]
fn cmd_shift_r_reloads_past_the_cache_without_touching_the_nav_stack(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    crate::web::pretend_available_for_test();
    strip_tabs(
        &h,
        vcx,
        vec![page_tab("p1", Some("https://example.com/"), None)],
        Some("p1"),
    );
    let _ = h.drain();
    let _ = crate::web::take_nav_calls_for_test();

    vcx.simulate_keystrokes("cmd-shift-r");
    vcx.run_until_parked();

    assert_eq!(
        crate::web::take_nav_calls_for_test(),
        vec![("hard-reload".to_string(), "p1".to_string())],
        "⇧⌘R did not reach Chromium"
    );
    assert!(
        h.drain().is_empty(),
        "⇧⌘R went through the engine — that is ⌘R's path, and it re-fetches \
         through the cache"
    );

    // …and plain ⌘R still does the OTHER thing, or the chord above has
    // simply stolen reload rather than added to it.
    vcx.simulate_keystrokes("cmd-r");
    vcx.run_until_parked();
    assert!(
        crate::web::take_nav_calls_for_test().is_empty(),
        "⌘R went to Chromium directly instead of through the engine"
    );
    assert!(!h.drain().is_empty(), "⌘R stopped re-navigating");
}

/// C4 — the speaker appears when a tab makes a noise, mutes on click, and
/// **stays while muted**.
///
/// That last clause is the whole design and the reason this test is worth
/// its length. If the icon were shown on `audible` alone, muting would
/// remove the only control that can unmute: the tab falls silent, the
/// speaker goes with it, and the state is unreachable from the strip. So
/// the interesting assertion is the one after the click, on a tab that is
/// no longer making any sound.
#[gpui::test]
fn a_tab_that_makes_a_noise_gets_a_speaker_that_survives_being_used(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    crate::web::pretend_available_for_test();
    // A session in the rail, because the rail is where the tab ROW is
    // drawn — without one the strip renders nothing and every assertion
    // below about an absent control passes for the wrong reason.
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");

    // Audibility arrives out of band (a CDP event in the running app, this
    // seam here), so nothing notifies gpui about it on its own. Re-pushing
    // the strip is what the running app gets from `note_chrome_change`, and
    // it is the same thing `set_favicon_for_test`'s caller relies on.
    let show = |vcx: &mut VisualTestContext, audible: bool| {
        crate::web::set_audible_for_test("p1", audible);
        strip_tabs(
            &h,
            vcx,
            vec![page_tab("p1", Some("https://noisy.example/"), Some("Noisy"))],
            Some("p1"),
        );
    };

    crate::web::set_tab_muted("p1", false);
    show(vcx, false);
    // The positive control for every `!shown` in this test.
    assert!(shown(vcx, "tab-p1"), "the tab row itself never rendered");
    assert!(
        !shown(vcx, "tab-audio-p1"),
        "a silent tab wears a speaker it has no use for"
    );

    // It starts playing.
    show(vcx, true);
    assert!(
        shown(vcx, "tab-audio-p1"),
        "a tab making a noise says nothing about it"
    );

    // Click mutes it, and does NOT activate the tab on the way through.
    click(vcx, "tab-audio-p1");
    vcx.run_until_parked();
    assert!(crate::web::tab_muted("p1"), "the speaker did not mute");

    // The page stops making sound — muted pages do. The control has to
    // outlive that, or unmuting is unreachable from here.
    show(vcx, false);
    assert!(
        shown(vcx, "tab-audio-p1"),
        "muting removed the only control that could unmute"
    );

    // And it toggles back off.
    click(vcx, "tab-audio-p1");
    vcx.run_until_parked();
    assert!(!crate::web::tab_muted("p1"), "the speaker did not unmute");
    show(vcx, false);
    assert!(
        !shown(vcx, "tab-audio-p1"),
        "an unmuted silent tab kept its speaker"
    );
}

/// A session tab never grows the audio control a page tab wears (Slice
/// B4). ID2b′: a session's TAB is drawn in exactly one place now — the
/// split pill, beside the page it is split with — and that is where the
/// absence is measured. Everywhere else the chat is a rail row.
#[gpui::test]
fn a_session_tab_never_grows_a_speaker(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    crate::web::pretend_available_for_test();
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(
        &h,
        vcx,
        vec![
            session_tab("aaaa1111"),
            page_tab("p1", Some("https://a.test/"), Some("A")),
        ],
        Some("aaaa1111"),
    );
    h.window
        .update(vcx, |shell, _, cx| shell.split_tabs("p1", "aaaa1111", true, cx))
        .unwrap();
    crate::web::set_audible_for_test("aaaa1111", true);
    crate::web::set_tab_muted("aaaa1111", true);
    vcx.run_until_parked();
    assert!(
        shown(vcx, "tab-aaaa1111"),
        "the session tab never rendered in the pill, so the absence below is vacuous"
    );
    assert!(
        !shown(vcx, "tab-audio-aaaa1111"),
        "a session tab grew an audio control"
    );
    crate::web::set_tab_muted("aaaa1111", false);
}

/// B4 — ⎋ stops a load, and only when there is one.
#[gpui::test]
fn escape_stops_a_load_in_flight_and_is_silent_otherwise(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    crate::web::pretend_available_for_test();
    // A session, because `Shell::on_key` returns early without one and the
    // escape arm lives at the END of that function — the same reason
    // `page_shell` seeds one.
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    // Settled: nothing to stop, so ⎋ must not fire. Without this half the
    // test would pass for an escape arm that stopped every tab every time.
    strip_tabs(
        &h,
        vcx,
        vec![page_tab("p1", Some("https://example.com/"), None)],
        Some("p1"),
    );
    let _ = crate::web::take_nav_calls_for_test();
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(
        crate::web::take_nav_calls_for_test().is_empty(),
        "⎋ stopped a page that was not loading"
    );

    strip_tabs(&h, vcx, vec![loading_page_tab("p2")], Some("p2"));
    let _ = crate::web::take_nav_calls_for_test();
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert_eq!(
        crate::web::take_nav_calls_for_test(),
        vec![("stop".to_string(), "p2".to_string())],
        "⎋ did not stop a load in flight"
    );
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

/// ⌘1–⌘9 are strip actions, so they fire before the picker's `on_key`.
/// While the picker is open they must pick a row, not a session/tab.
#[gpui::test]
fn cmd_digits_pick_a_model_while_the_picker_is_open(cx: &mut TestAppContext) {
    let _turn = eave_core::capabilities::probe_test_lock();
    eave_core::capabilities::forget();
    let (mut h, vcx) = boot(cx);
    empty_session(&h, vcx, "/tmp/eave-harness/repo", None);
    // A page sits at strip index 1 so a leaked ⌘2 would Activate it.
    strip_tabs(
        &h,
        vcx,
        vec![
            session_tab("aaaa1111"),
            page_tab("p1", Some("https://a.test/"), None),
            page_tab("p2", Some("https://b.test/"), None),
        ],
        Some("aaaa1111"),
    );
    click(vcx, "composer-model");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "model-picker"),
        "composer chip did not open the picker"
    );
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-2");
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(16));
    let cmds = h.drain();
    // Slice TN2: on a live chat the pick SWITCHES that chat's model (one
    // `Tune`), rather than choosing the next launch's model.
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Tune { id, tuning } if id == "aaaa1111" && tuning.model.as_deref() == Some("claude-opus-5")
        )),
        "⌘2 with the picker open did not pick the second row: {cmds:?}"
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Activate(_)))),
        "⌘2 with the picker open jumped the strip: {cmds:?}"
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
            Command::Browser(BrowserCmd::OpenAt { url, activate: true, .. })
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
            Command::Browser(BrowserCmd::OpenAt { url, activate: false, .. })
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
    open_files_face(vcx);

    let rail = vcx.debug_bounds("rail").expect("the rail");
    let files = vcx
        .debug_bounds("files-section")
        .expect("the changes face did not render");
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
    assert!(
        !shown(vcx, "rail-rows"),
        "the files face left the session list on screen — they are supposed \
         to be two faces of one slot"
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
    // Slice NC1: the launcher IS the composer — a blank tab is a chat
    // waiting for its first words, and `send` is how they go.
    assert!(
        shown(vcx, "send"),
        "the launcher on a blank tab has no composer"
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
            Command::Browser(BrowserCmd::OpenAt { url, activate: true, .. })
                if url == "https://example.com"
        )),
        "the omnibox dropped a navigation because the active tab was a \\
         session: {cmds:?}"
    );
}

/// ⌘T opens a page tab **and** the omnibox on it. A new tab with no way to
/// say where it should go is a blank page and a second keystroke.
#[gpui::test]
fn cmd_t_lands_on_a_chat_with_the_omnibox_lit(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(&h, vcx, vec![session_tab("aaaa1111")], Some("aaaa1111"));
    assert!(!shown(vcx, "launcher"));

    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-t");
    vcx.run_until_parked();
    // The tab is a round trip away; the engine answers with it active.
    strip_tabs(
        &h,
        vcx,
        vec![page_tab("blank", None, None), session_tab("aaaa1111")],
        Some("blank"),
    );
    let _ = h.drain();

    // Slice NC1: the blank tab is the launcher, the launcher is a chat,
    // and the address field is the one lit — so the first keystrokes
    // are the omnibox's, not the composer's.
    assert!(shown(vcx, "launcher"), "⌘T did not land on the launcher");
    assert!(shown(vcx, "composer-hint"), "the launcher has no composer");
    assert!(shown(vcx, "empty-logo"));
    let omnibox_up = vcx
        .read_window(&h.window, |shell, cx| shell.read(cx).omnibox_open())
        .unwrap();
    assert!(omnibox_up, "⌘T did not light the address field");
    vcx.simulate_input("rust docs");
    vcx.run_until_parked();
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
    assert_eq!(
        draft, "",
        "typing at the lit address field leaked into the composer"
    );

    // Escape closes the address field; the chat is what is left, and the
    // next words are its opening.
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    let _ = h.drain();
    start_chat(vcx, "start here");
    let cmds = h.drain();
    assert!(
        cmds.iter().any(
            |c| matches!(c, Command::Launch { prompt: Some((t, _)), .. } if t == "start here")
        ),
        "return on the chat did not launch it: {cmds:?}"
    );
}

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
            .any(|c| matches!(c, Command::Browser(BrowserCmd::NewTab { .. }))),
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
            .any(|c| matches!(c, Command::Browser(BrowserCmd::NewTab { .. }))),
        "the strip's + is not ⌘T: {cmds:?}"
    );
}

/// ⌘⇧N opens a private tab, not an ordinary one. The two must not
/// collapse: a chord that meant incognito and issued NewTab would write
/// the next navigation into history.
#[gpui::test]
fn cmd_shift_n_opens_an_incognito_tab(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(&h, vcx, vec![session_tab("aaaa1111")], Some("aaaa1111"));

    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-shift-n");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::NewIncognitoTab { .. }))),
        "⌘⇧N opened nothing private: {cmds:?}"
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::NewTab { .. }))),
        "⌘⇧N issued an ordinary NewTab: {cmds:?}"
    );
    assert!(
        shown(vcx, "omnibox"),
        "⌘⇧N opened a tab with no way to aim it"
    );
}

/// The rail footer (operator directive 2026-08-25): three icons, and each
/// one is a door that actually opens.
///
/// The `+` menu's rows are asserted against the SAME commands the acts
/// they name send elsewhere — `New Tab` is ⌘T's `BrowserCmd::NewTab`,
/// `New Session` is `+ new`'s `Command::NewSession` — because a menu row
/// that reaches its goal by a different route is a second code path
/// pretending to be a shortcut. The library and theme icons are asserted
/// as windows/menus that exist after the click.
#[gpui::test]
fn the_footer_panel_is_a_door_to_library_theme_and_new(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    for sel in ["footer-library", "footer-theme", "footer-plus"] {
        assert!(shown(vcx, sel), "{sel} is standing chrome and must render");
    }

    // The `+` menu: every row is an act eave has, and nothing else.
    click(vcx, "footer-plus");
    vcx.run_until_parked();
    for sel in [
        "tab-menu-new-project",
        "tab-menu-new-folder",
        "tab-menu-add-tab",
    ] {
        assert!(shown(vcx, sel), "{sel} missing from the + menu");
    }
    // Slice NC1: a new tab IS a new chat; a second row for the same
    // screen was the split the operator asked to close.
    assert!(
        !shown(vcx, "tab-menu-new-session"),
        "the + menu still offers Q2's New Session beside New Tab"
    );

    let _ = h.drain();
    click(vcx, "tab-menu-add-tab");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::NewTab { .. }))),
        "the + menu's New Tab is not ⌘T's act: {cmds:?}"
    );
    assert!(
        shown(vcx, "omnibox"),
        "New Tab from the + menu opened a tab with no way to aim it"
    );
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    // The closed menu's catcher stays mounted through its 100 ms exit and
    // occludes the window; a reopen click before the reap lands on the
    // catcher, not the button.
    vcx.executor().advance_clock(Duration::from_millis(200));
    vcx.run_until_parked();

    click(vcx, "footer-plus");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "tab-menu-new-project"),
        "the + menu did not reopen"
    );
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();

    // The library icon is ALWAYS present (operator ruling). It now opens
    // the files face; Downloads / Settings live on `files-library` in
    // that face's header.
    vcx.executor().advance_clock(Duration::from_millis(200));
    vcx.run_until_parked();
    let windows_before = cx.update(|cx| cx.windows().len());
    click(vcx, "footer-library");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "files-section"),
        "the footer library icon did not open the files face"
    );
    click(vcx, "files-library");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "tab-menu-download-inert"),
        "an empty download history is stated, not hidden"
    );
    click(vcx, "tab-menu-open-settings");
    vcx.run_until_parked();
    assert_eq!(
        cx.update(|cx| cx.windows().len()),
        windows_before + 1,
        "the library menu's Settings row did not open the settings window"
    );

    // The brush opens Customize Theme as a popup in THIS window — the
    // second window is exactly what the operator retired (2026-08-26),
    // and second windows eat synthetic clicks besides.
    vcx.executor().advance_clock(Duration::from_millis(200));
    vcx.run_until_parked();
    let windows_before = cx.update(|cx| cx.windows().len());
    click(vcx, "footer-theme");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "customize-field"),
        "the footer's brush did not open the Customize Theme popup"
    );
    assert_eq!(
        cx.update(|cx| cx.windows().len()),
        windows_before,
        "Customize Theme must be a popup in the shell, not a second window"
    );
}

/// The Customize Theme popup renders its whole grammar over the shell,
/// and the tabs switch what the field edits.
///
/// A POPUP, not a window (operator directive 2026-08-26): the brush and
/// ⌘⇧, mount the panel over the rail footer through the menus' own
/// overlay machinery — so the window count must not move, Escape must
/// take it down, and the panel view survives a close (it reopens on the
/// tab it was left on).
///
/// Deliberately MUTATION-FREE: a click on a swatch or the field writes
/// `theme::set_brand`, and the brand is process-global — a test that
/// installed a green accent would race `oklch_accents_match_reference`
/// and every other exact-pin token test running on a sibling thread.
/// Tab switching is view state, so it is the interactive surface this
/// test may drive; the write path is covered by the pure tests in
/// `theme_editor` and was proven live (seeded theme-brand.json, walked
/// 2026-08-25).
#[gpui::test]
fn the_customize_theme_panel_renders_and_switches_tabs(cx: &mut TestAppContext) {
    let (_h, vcx) = boot(cx);
    let windows_before = cx.update(|cx| cx.windows().len());
    click(vcx, "footer-theme");
    vcx.run_until_parked();
    assert_eq!(
        cx.update(|cx| cx.windows().len()),
        windows_before,
        "Customize Theme must mount as a popup, not a second window"
    );

    for sel in [
        "overlay-panel",
        "customize-tab-Background",
        "customize-tab-Accent",
        "customize-appearance-Light",
        "customize-appearance-Dark",
        "customize-field",
        "customize-swatch-Neutral",
        "customize-swatch-Slate",
        "theme-reset",
    ] {
        assert!(shown(vcx, sel), "{sel} missing from the Background tab");
    }

    click(vcx, "customize-tab-Accent");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "customize-swatch-Violet"),
        "the Accent tab did not bring the accent swatches"
    );
    assert!(
        !shown(vcx, "customize-swatch-Slate"),
        "the Background presets leaked onto the Accent tab"
    );
    assert_eq!(
        theme::brand(),
        crate::theme::Brand::default(),
        "switching tabs must not write the brand"
    );

    // Escape takes the popup down the way it takes every menu down, and
    // past the closed layer's occluding exit the brush reopens it — on
    // the Accent tab, because the panel's view outlives its popup.
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    vcx.executor().advance_clock(Duration::from_millis(200));
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "customize-field"),
        "escape did not close the Customize Theme popup"
    );
    click(vcx, "footer-theme");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "customize-swatch-Violet"),
        "the reopened popup forgot it was on the Accent tab"
    );
}

/// Import from your old browser is a popup in the shell, lists all four
/// sources, and Escape takes it down. Deliberately does not click Next:
/// Next parses the operator's real browser profiles on disk.
#[gpui::test]
fn the_import_dialog_lists_four_sources_and_is_a_popup(cx: &mut TestAppContext) {
    let (_h, vcx) = boot(cx);
    let windows_before = cx.update(|cx| cx.windows().len());
    vcx.dispatch_action(crate::import_dialog::OpenImportDialog);
    vcx.run_until_parked();
    assert_eq!(
        cx.update(|cx| cx.windows().len()),
        windows_before,
        "Import must be a popup in the shell, not a second window"
    );
    for sel in [
        "import-dialog",
        "import-source-Brave",
        "import-source-Chrome",
        "import-source-Safari",
        "import-source-Arc",
        "import-maybe-later",
        "import-next",
        "import-close",
    ] {
        assert!(shown(vcx, sel), "{sel} missing from the import picker");
    }
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    vcx.executor().advance_clock(Duration::from_millis(200));
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "import-dialog"),
        "escape did not close the import popup"
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
            base: None,
            last_active: None,
            created_ts_ms: 0,
        }],
        group_tabs: vec![("grp".into(), group_ids)],
        active: active.map(str::to_string),
        windows: Vec::new(),
        read_only: false,
    });
    vcx.run_until_parked();
}

/// **The session list is not starved, and no page row is cut off.**
///
/// The operator's report was "each section needs to be its own scrollable
/// section and have no cut offs", against a rail showing a tab row sliced
/// through the middle. Measured on a 520 px rail before the first fix: ONE
/// box held Projects, Folders and Tabs at a definite height, `flex_none`,
/// taking 277 px — and the SESSIONS list got 27 px. D4 split the sections;
/// ID2b′ has PAGES as one of them. This asserts the report's two halves on
/// that geometry: the session list keeps more than half the rail, and
/// PAGES holds every row it draws whole. A project group with no session
/// has no header to hang from and draws nowhere — its docked pages are
/// not loose, so they are not in PAGES either.
#[gpui::test]
fn the_session_list_is_not_starved_and_no_page_is_cut_off(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");

    let tab = |id: &str, title: &str| page_tab(id, Some("https://example.com"), Some(title));
    let group = |id: &str, title: &str| eave_wire::TabGroup {
        id: id.into(),
        title: Some(title.into()),
        color: None,
        collapsed: false,
        workspace: Some(format!("/w/{title}")),
        base: None,
        last_active: None,
        created_ts_ms: 0,
    };
    h.plumbing.push_tabs(TabsSnapshot {
        order: vec![
            eave_wire::Slot::Group { id: "g1".into() },
            eave_wire::Slot::Group { id: "g2".into() },
            eave_wire::Slot::Group { id: "g3".into() },
            eave_wire::Slot::Tab { id: "loose".into() },
            eave_wire::Slot::Tab { id: "loose2".into() },
        ],
        tabs: vec![
            tab("t1", "waku tab"),
            tab("t2", "wt-c5 tab"),
            tab("t3", "t3code tab"),
            tab("loose", "you - Google Search"),
            tab("loose2", "std - Rust"),
        ],
        groups: vec![
            group("g1", "waku"),
            group("g2", "wt-c5"),
            group("g3", "t3code"),
        ],
        group_tabs: vec![
            ("g1".into(), vec!["t1".into()]),
            ("g2".into(), vec!["t2".into()]),
            ("g3".into(), vec!["t3".into()]),
        ],
        active: Some("loose".into()),
        windows: Vec::new(),
        read_only: false,
    });
    vcx.run_until_parked();

    let rows = vcx
        .debug_bounds("rail-rows")
        .expect("the session list has no scroller of its own");
    let rail = vcx.debug_bounds("rail").expect("the rail");
    assert!(
        rows.size.height > rail.size.height / 2.,
        "the session list got {:?} of a {:?} rail — something above it is \
         taking the room the list needs",
        rows.size.height,
        rail.size.height
    );
    assert!(
        !shown(vcx, "projects-strip") && !shown(vcx, "rail-browse"),
        "the rail draws a zone that is gone"
    );
    let pages = vcx.debug_bounds("tab-strip").expect("PAGES");
    assert!(
        pages.origin.y + pages.size.height <= rows.origin.y,
        "PAGES and the tree overlap — they are still one box"
    );

    let inside = |vcx: &mut VisualTestContext, sel: &'static str, box_: gpui::Bounds<Pixels>| {
        let b = vcx
            .debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} never drew"));
        assert!(
            b.origin.y >= box_.origin.y
                && b.origin.y + b.size.height <= box_.origin.y + box_.size.height,
            "{sel} sits at {:?}..{:?} inside a box running {:?}..{:?} — the \
             row is CUT OFF by the section that is supposed to hold it",
            b.origin.y,
            b.origin.y + b.size.height,
            box_.origin.y,
            box_.origin.y + box_.size.height
        );
    };
    for sel in ["tabs-divider", "tab-loose", "tab-loose2", "tab-new"] {
        inside(vcx, sel, pages);
    }
    for sel in ["group-g1", "group-g2", "group-g3", "tab-t1", "tab-t2", "tab-t3"] {
        assert!(!shown(vcx, sel), "{sel} drew, with no session to hang from");
    }
}

/// PAGES with more rows than fit stops growing and scrolls, rather than
/// taking the room from the session list under it (Slice D4's rule, on
/// ID2b′'s zone). Same bargain the rail's window strikes (AG2 #10):
/// stated, never clipped in silence — and the active page is scrolled
/// into the box, so the page you are reading is never off its end.
#[gpui::test]
fn pages_with_more_rows_than_fit_cap_and_scroll(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");

    let n = 15;
    let ids: Vec<String> = (0..n).map(|i| format!("t{i}")).collect();
    let tabs = || -> Vec<TabInfo> {
        ids.iter()
            .map(|id| page_tab(id, Some("https://example.com"), Some(id)))
            .collect()
    };
    strip_tabs(&h, vcx, tabs(), Some("t0"));

    let pages = vcx.debug_bounds("tab-strip").expect("PAGES");
    let rail = vcx.debug_bounds("rail").expect("the rail");
    let uncapped = px(theme::rail_section_h(n + 2));
    assert!(
        uncapped > pages.size.height,
        "the fixture does not overflow the cap ({uncapped:?} vs {:?}), so \
         this test measures nothing",
        pages.size.height
    );
    assert!(
        pages.size.height < rail.size.height / 2.,
        "PAGES is {:?} of a {:?} rail — one zone is taking the space every \
         other zone needs",
        pages.size.height,
        rail.size.height
    );
    assert!(shown(vcx, "rail-aaaa1111"), "the session list was starved off screen");

    // The LAST page, activated. Its row has to come to the operator.
    strip_tabs(&h, vcx, tabs(), Some("t14"));
    let pages = vcx.debug_bounds("tab-strip").expect("PAGES");
    let sel = vcx.debug_bounds("tab-t14").expect("the active page");
    assert!(
        sel.origin.y >= pages.origin.y
            && sel.origin.y + sel.size.height <= pages.origin.y + pages.size.height,
        "the active page sits at {:?}..{:?} and PAGES runs {:?}..{:?} — the \
         page you are reading is off the end of its own list",
        sel.origin.y,
        sel.origin.y + sel.size.height,
        pages.origin.y,
        pages.origin.y + pages.size.height
    );
}

/// **ID2b′ in one test.** A page docked into a task is a row UNDER that
/// task in the tree; a loose page is a row in PAGES; the two never share
/// a zone, no strip is drawn in the pane header, and clicking either row
/// brings that page forward under A′'s page toolbar. ID2b's Browse row
/// and its header strip are gone.
#[gpui::test]
fn a_tasks_pages_sit_under_it_and_loose_pages_sit_in_pages(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let a = in_task("aaaa1111", "/tmp/eave-harness/repo", "/tmp/eave-harness/repo");
    rail_with_selected(&h, vcx, vec![a], "aaaa1111");
    let strip = |h: &Harness, vcx: &mut VisualTestContext, active: &str| {
        strip_with_project(
            h,
            vcx,
            Some("/tmp/eave-harness/repo"),
            vec![
                session_tab("aaaa1111"),
                page_tab("docked", Some("https://docs.rs/gpui"), Some("gpui - Rust")),
            ],
            vec![page_tab("loose", Some("https://loose.test/"), Some("loose"))],
            Some(active),
        );
    };
    strip(&h, vcx, "aaaa1111");

    let pages = vcx.debug_bounds("tab-strip").expect("no PAGES zone");
    let tree = vcx.debug_bounds("rail-rows").expect("no session tree");
    let inside = |b: gpui::Bounds<Pixels>, box_: gpui::Bounds<Pixels>| {
        b.origin.y >= box_.origin.y
            && b.origin.y + b.size.height <= box_.origin.y + box_.size.height
    };
    let loose = vcx.debug_bounds("tab-loose").expect("the loose page has no row");
    let docked = vcx.debug_bounds("tab-docked").expect("the docked page has no row");
    let chat = vcx.debug_bounds("rail-aaaa1111").expect("the chat row");
    assert!(inside(loose, pages), "the loose page is not in PAGES: {loose:?} vs {pages:?}");
    assert!(inside(docked, tree), "the docked page is not in the tree: {docked:?} vs {tree:?}");
    assert!(
        docked.origin.y > chat.origin.y,
        "the docked page is not under its task's chat: page {docked:?} chat {chat:?}"
    );
    assert!(
        docked.origin.x > loose.origin.x,
        "a docked page is not stepped in under its task ({:?} vs a loose page at {:?})",
        docked.origin.x,
        loose.origin.x
    );
    assert!(!shown(vcx, "rail-browse"), "ID2b's Browse row is back");
    let header = vcx.debug_bounds("pane-header").expect("the pane header");
    assert!(
        !inside(pages, header) && pages.origin.x < header.origin.x,
        "the tabs are drawn on the pane header again: {pages:?} vs {header:?}"
    );
    // A session's tab is the chat row, not a second row.
    assert!(!shown(vcx, "tab-aaaa1111"), "the chat drew a tab row under its own card");

    let _ = h.drain();
    click(vcx, "tab-loose");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "loose"
        )),
        "the loose page's row did not bring it forward: {cmds:?}"
    );
    strip(&h, vcx, "loose");
    assert!(shown(vcx, "pane-address"), "a loose page is not under A′'s page toolbar");
    let _ = h.drain();
    click(vcx, "tab-docked");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "docked"
        )),
        "the docked page's row did not bring it forward: {cmds:?}"
    );
    strip(&h, vcx, "docked");
    assert!(shown(vcx, "pane-address"), "a docked page is not under A′'s page toolbar");
}

/// Clicking a project's header folds it, and folding is the rail's own
/// fact, kept in `frame.json`.
///
/// This used to click the strip's group head and expect a `GroupUpdate {
/// collapsed }` on the wire. Slice ID2b deleted that head: a project is a
/// header in the rail's tree, and the rail has folded its headers
/// client-side since AG2 #2 — one collapsed set, keyed on the root. The
/// engine's `TabGroup::collapsed` is left where B1 put it, set by nobody.
#[gpui::test]
fn folding_a_project_hides_its_rows_and_asks_the_engine_nothing(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.checkout_root = Some("/tmp/eave-harness/repo".into());
    a.project_root = Some("/tmp/eave-harness/repo".into());
    let mut z = session("zzzz9999");
    z.checkout_root = Some("/tmp/eave-harness/other".into());
    z.project_root = Some("/tmp/eave-harness/other".into());
    rail_with_selected(&h, vcx, vec![a, z], "zzzz9999");
    strip_with_project(
        &h,
        vcx,
        Some("/tmp/eave-harness/repo"),
        vec![session_tab("aaaa1111")],
        vec![session_tab("zzzz9999")],
        Some("zzzz9999"),
    );
    assert!(shown(vcx, "rail-aaaa1111"), "the row is not drawn to begin with");

    let _ = h.drain();
    click(vcx, "rail-group-/tmp/eave-harness/repo");
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "rail-aaaa1111"),
        "clicking the header did not fold the project"
    );
    assert!(
        shown(vcx, "rail-group-/tmp/eave-harness/repo"),
        "the header folded itself away with its rows"
    );
    let cmds = h.drain();
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::Browser(_))),
        "folding a project put something on the wire: {cmds:?}"
    );
    click(vcx, "rail-group-/tmp/eave-harness/repo");
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-aaaa1111"), "the fold is not an involution");
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
    start_chat(vcx, "hello");

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
    start_chat(vcx, "hello");

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
    start_chat(vcx, "hello");

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

/// A bookmark folder row sits under the pinned grid and above PAGES
/// (Slice T1b; ID2b′ moved the anchor from ID2b's Browse row to the
/// divider that opens PAGES).
#[gpui::test]
fn a_bookmark_folder_sits_under_the_grid_and_above_the_pages(cx: &mut TestAppContext) {
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
    let divider = vcx
        .debug_bounds("tabs-divider")
        .expect("no PAGES divider to place the folder against");
    let grid = vcx.debug_bounds("bookmarks-grid").expect("the pinned grid");

    assert!(
        grid.origin.y < folder.origin.y,
        "the folder drew above the pinned grid — grid {grid:?} folder {folder:?}"
    );
    assert!(
        folder.origin.y < divider.origin.y,
        "the folder fell BELOW the PAGES divider, in with the loose pages — \
         folder {folder:?} divider {divider:?}"
    );
    assert!(!shown(vcx, "rail-browse"), "ID2b's Browse row is back");
}

/// Opening a bookmark folder pushes everything under it down — PAGES
/// included (Slice T1b; ID2b′ measures the divider that opens PAGES).
#[gpui::test]
fn opening_a_bookmark_folder_pushes_the_rows_below_it_down(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
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
        vec![
            session_tab("aaaa1111"),
            visited_tab("p1", "https://docs.rs/"),
        ],
        Some("p1"),
    );

    let before = vcx
        .debug_bounds("tabs-divider")
        .expect("no PAGES divider")
        .origin
        .y;
    click(vcx, "bookmark-folder-dev");
    vcx.run_until_parked();
    let after = vcx
        .debug_bounds("tabs-divider")
        .expect("the PAGES divider vanished when a folder opened")
        .origin
        .y;

    assert!(
        shown(vcx, "bookmark-child-rs"),
        "the folder did not open at all"
    );
    assert!(
        after > before,
        "the folder opened OVER the rail instead of pushing it down — \
         PAGES did not move (before {before:?}, after {after:?})"
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

/// A link inside a folder wears the same host mark the grid does.
///
/// Filing a tab into a folder used to swap the favicon for a globe
/// because the child row always painted `GLOBAL`. The bytes are keyed
/// by host, so a page you just had open still has them.
#[gpui::test]
fn a_bookmark_inside_a_folder_wears_its_site_mark_when_the_bytes_are_in(
    cx: &mut TestAppContext,
) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(
        &h,
        vcx,
        &[
            ("dev", 0, Some("dev"), None),
            ("gh", 1, None, Some("https://github.com/")),
        ],
    );
    click(vcx, "bookmark-folder-dev");
    vcx.run_until_parked();

    assert!(
        shown(vcx, "bookmark-child-gh"),
        "the folder did not open its child"
    );
    assert!(
        !shown(vcx, "bookmark-child-mark-gh"),
        "a folder child drew a mark before any bytes arrived"
    );
    assert!(
        shown(vcx, "bookmark-child-glyph"),
        "the globe fallback is missing, so an iconless child is invisible"
    );

    crate::web::set_host_favicon_for_test(
        "github.com",
        Some(std::sync::Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            TINY_PNG.to_vec(),
        ))),
    );
    push_bookmarks(
        &h,
        vcx,
        &[
            ("dev", 0, Some("dev"), None),
            ("gh", 1, None, Some("https://github.com/")),
        ],
    );

    assert!(
        shown(vcx, "bookmark-child-mark-gh"),
        "the folder child kept its globe with bytes in hand"
    );
    crate::web::set_host_favicon_for_test("github.com", None);
}

/// Hover the folder, wait out the 0.8s dwell, and force a Shell
/// redraw so the inventory mounts. `point_at_row` only drives the
/// hover wash; the dwell clock is Instant (executor in tests).
fn dwell_folder_popup(h: &Harness, vcx: &mut VisualTestContext, row: &'static str) {
    point_at_row(h, vcx, row);
    vcx.executor().advance_clock(Duration::from_millis(800));
    h.window.update(vcx, |_, _, cx| cx.notify()).unwrap();
    vcx.run_until_parked();
    drive(h, vcx, Duration::from_millis(20));
}

/// Hovering a folder opens a searchable, scrollable inventory of
/// everything in it — not an inline expand, and not a globe-only list.
#[gpui::test]
fn hovering_a_folder_opens_a_searchable_scrollable_popup(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(
        &h,
        vcx,
        &[
            ("dev", 0, Some("OpenModel"), None),
            ("rs", 1, Some("docs.rs"), Some("https://docs.rs/")),
            ("deep", 1, Some("deep"), Some("https://deep.test/")),
        ],
    );

    assert!(
        !shown(vcx, "folder-popup-dev"),
        "the popup was up before the pointer settled"
    );

    dwell_folder_popup(&h, vcx, "bookmark-folder-dev");

    assert!(
        shown(vcx, "folder-popup-dev"),
        "hovering the folder never opened its popup"
    );
    let folder = vcx
        .debug_bounds("bookmark-folder-dev")
        .expect("folder row");
    let pop = vcx
        .debug_bounds("folder-popup-dev")
        .expect("folder popup bounds");
    assert!(
        pop.size.width > px(40.0) && pop.size.height > px(20.0),
        "the popup mounted at zero size: {pop:?}"
    );
    assert!(
        pop.origin.x >= folder.origin.x + folder.size.width - px(8.0),
        "the popup is not on the outside of the sidebar: folder {folder:?} popup {pop:?}"
    );
    let folder_cy = folder.origin.y + folder.size.height / 2.0;
    let pop_cy = pop.origin.y + pop.size.height / 2.0;
    assert!(
        (pop_cy - folder_cy).abs() <= px(12.0),
        "the popup is not vertically centered on the folder: folder {folder:?} popup {pop:?}"
    );
    assert!(
        shown(vcx, "folder-popup-search-dev"),
        "the popup has no search field"
    );
    assert!(
        shown(vcx, "folder-popup-list"),
        "the popup has no scrolling list"
    );
    assert!(
        shown(vcx, "folder-popup-row-rs") && shown(vcx, "folder-popup-row-deep"),
        "the popup did not list the folder's pages"
    );
    assert!(
        shown(vcx, "folder-popup-search-placeholder"),
        "search was focused on hover, stealing keys from the page"
    );
}

/// Moving onto the inventory must not dismiss it — search and scroll
/// live on the popup, not on the folder row.
#[gpui::test]
fn the_folder_popup_survives_moving_onto_search(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(
        &h,
        vcx,
        &[
            ("dev", 0, Some("OpenModel"), None),
            ("rs", 1, Some("docs.rs"), Some("https://docs.rs/")),
        ],
    );

    dwell_folder_popup(&h, vcx, "bookmark-folder-dev");
    assert!(
        shown(vcx, "folder-popup-dev"),
        "the closed folder never offered its inventory"
    );

    let at = vcx
        .debug_bounds("folder-popup-search-dev")
        .expect("search field");
    vcx.simulate_mouse_move(at.center(), None, Modifiers::none());
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(20));
    assert!(
        shown(vcx, "folder-popup-dev"),
        "moving onto search dismissed the inventory"
    );
    assert!(
        shown(vcx, "folder-popup-list"),
        "the scrolling list went with it"
    );
}

/// The inventory is closed-folder hover only. An open folder already
/// lists its pages in the rail.
#[gpui::test]
fn an_open_folder_does_not_show_the_hover_popup(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(
        &h,
        vcx,
        &[
            ("dev", 0, Some("OpenModel"), None),
            ("rs", 1, Some("docs.rs"), Some("https://docs.rs/")),
        ],
    );

    dwell_folder_popup(&h, vcx, "bookmark-folder-dev");
    assert!(
        shown(vcx, "folder-popup-dev"),
        "the closed folder never offered its inventory"
    );

    click(vcx, "bookmark-folder-dev");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "bookmark-child-rs"),
        "the click did not expand the folder"
    );
    assert!(
        !shown(vcx, "folder-popup-dev"),
        "opening the folder left the popup standing"
    );

    point_at_row(&h, vcx, "bookmark-folder-dev");
    drive(&h, vcx, Duration::from_millis(20));
    assert!(
        !shown(vcx, "folder-popup-dev"),
        "hovering an open folder raised the inventory again"
    );
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

/// The empty bookmarks zone is a hint card on the dashed drop well — a
/// star badge, "Drag to add bookmarks", a close control — and NO `+`.
/// The gesture is the affordance; the card teaches it once.
#[gpui::test]
fn an_empty_bookmarks_zone_is_a_drag_hint_and_not_a_plus(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(&h, vcx, &[]);
    assert!(shown(vcx, "bookmarks-drop"), "no drop well in the empty zone");
    assert!(shown(vcx, "bookmarks-hint"), "the empty zone drew no hint card");
    assert!(
        shown(vcx, "bookmarks-hint-dismiss"),
        "the hint card cannot be closed"
    );
    assert!(
        !shown(vcx, "bookmark-new"),
        "a `+` is back on the empty zone — the drag is the affordance"
    );
    let well = vcx.debug_bounds("bookmarks-drop").expect("the well");
    let badge = vcx.debug_bounds("bookmarks-hint").expect("the badge");
    let close = vcx.debug_bounds("bookmarks-hint-dismiss").expect("the close");
    assert!(
        badge.origin.x > well.origin.x && badge.origin.x + badge.size.width < close.origin.x,
        "the badge is not centred left of the close: well {well:?} badge {badge:?} close {close:?}"
    );
}

/// Closing the hint card leaves the DROP WELL — a slim dashed slot — so a
/// drop still has somewhere to land; only the lesson goes.
#[gpui::test]
fn closing_the_bookmarks_hint_keeps_the_drop_well(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(&h, vcx, &[]);
    let tall = vcx.debug_bounds("bookmarks-drop").expect("the well").size.height;
    click(vcx, "bookmarks-hint-dismiss");
    assert!(!shown(vcx, "bookmarks-hint"), "the card did not close");
    assert!(shown(vcx, "bookmarks-drop"), "closing the card took the drop well with it");
    assert!(shown(vcx, "bookmarks-drop-slot"), "the closed well says nothing");
    let slim = vcx.debug_bounds("bookmarks-drop").expect("the well").size.height;
    assert!(slim < tall, "the closed well is no smaller: {slim:?} vs {tall:?}");
}

/// A tile's width is its share of the row: one pin spans the well, two
/// are halves, three are thirds, and a fourth splits the grid into two
/// even rows rather than trailing alone under three thirds. Height never
/// moves.
#[gpui::test]
fn bookmark_tiles_share_their_row_up_to_three_across(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let urls = [
        ("b1", "https://one.example/"),
        ("b2", "https://two.example/"),
        ("b3", "https://three.example/"),
        ("b4", "https://four.example/"),
    ];
    let mut heights = Vec::new();
    for n in 1..=4 {
        let pins: Vec<_> = urls[..n]
            .iter()
            .map(|(id, url)| (*id, 0usize, None, Some(*url)))
            .collect();
        push_bookmarks(&h, vcx, &pins);
        let well = vcx.debug_bounds("bookmarks-drop").expect("the well");
        let tiles: Vec<_> = urls[..n]
            .iter()
            .map(|(id, _)| {
                let sel: &'static str = Box::leak(format!("bookmark-tile-{id}").into_boxed_str());
                vcx.debug_bounds(sel).unwrap_or_else(|| panic!("no tile {id} with {n} pins"))
            })
            .collect();
        let across = match n {
            4 => 2,
            n => n,
        };
        // The well's content is its width less the 1 px rim each side.
        let content = f32::from(well.size.width) - 2.0;
        let share = (content - theme::BOOKMARK_GRID_GAP * (across as f32 - 1.0)) / across as f32;
        for t in &tiles {
            let w = f32::from(t.size.width);
            assert!(
                (w - share).abs() <= 3.0,
                "with {n} pins a tile is {w} wide, not a {across}-way share {share} of {content}"
            );
            heights.push(t.size.height);
        }
        let rows: std::collections::BTreeSet<i32> =
            tiles.iter().map(|t| f32::from(t.origin.y).round() as i32).collect();
        assert_eq!(
            rows.len(),
            if n == 4 { 2 } else { 1 },
            "{n} pins sat on {} rows",
            rows.len()
        );
    }
    assert!(
        heights.windows(2).all(|w| w[0] == w[1]),
        "a tile's height moved with the count: {heights:?}"
    );
}

/// One bookmark per site. A second drop of a host the grid already
/// holds sends nothing, and a dragged TAB of that host stays open — the
/// refusal happens before the close, or the page would just vanish.
#[gpui::test]
fn a_site_already_on_the_grid_is_not_bookmarked_twice(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(&h, vcx, &[("gh", 0, None, Some("https://github.com/"))]);
    strip_tabs(
        &h,
        vcx,
        vec![
            session_tab("aaaa1111"),
            visited_tab("p1", "https://www.github.com/eave/eave/pulls"),
            visited_tab("p2", "https://docs.rs/"),
        ],
        Some("p1"),
    );
    let _ = h.drain();
    h.window
        .update(vcx, |shell, _, cx| {
            let dup = AddressDrag {
                url: "https://github.com/eave/eave".into(),
                title: None,
            };
            assert!(!shell.add_bookmark(None, None, &dup, cx), "a second github pin was taken");
            let tab = TabDrag {
                id: "p1".into(),
                url: Some("https://www.github.com/eave/eave/pulls".into()),
                title: "pulls".into(),
            };
            shell.drop_tab_as_bookmark(None, None, &tab, cx);
            let fresh = AddressDrag {
                url: "https://docs.rs/".into(),
                title: None,
            };
            assert!(shell.add_bookmark(None, None, &fresh, cx), "a new site was refused");
        })
        .unwrap();
    vcx.run_until_parked();
    let cmds = h.drain();
    let adds: Vec<String> = cmds
        .iter()
        .filter_map(|c| match c {
            Command::Browser(BrowserCmd::BookmarkAdd { url, .. }) => url.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(adds, vec!["https://docs.rs/".to_string()], "sent: {cmds:?}");
    assert!(
        !cmds.iter().any(|c| matches!(c, Command::Browser(BrowserCmd::Close(_)))),
        "the refused tab was closed anyway: {cmds:?}"
    );
}

/// Once a bookmark is on the grid there is no `+` cell after it — one
/// drag taught the gesture. The well past the last tile still takes a drop.
#[gpui::test]
fn a_populated_grid_has_no_plus_cell(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    push_bookmarks(&h, vcx, &[("gh", 0, None, Some("https://github.com/"))]);
    assert!(shown(vcx, "bookmark-tile-gh"), "the tile is missing");
    assert!(!shown(vcx, "bookmark-new"), "a `+` cell trails the pins");
    assert!(!shown(vcx, "bookmarks-hint"), "the hint card is over a populated grid");
    assert!(shown(vcx, "bookmarks-drop"), "the populated grid lost its drop well");
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
    // D4.2 stage 3 renamed a LINK's removal from Delete to **Unpin** — a
    // pinned cell is unpinned, a folder is deleted. Same `BookmarkRemove`
    // underneath (`run_tab_menu` treats the two actions alike), so the rule
    // this test carries is unchanged and only the row's name moved.
    assert!(
        shown(vcx, "tab-menu-unpin"),
        "right-clicking a bookmark offered no way to take it off the bar"
    );

    let _ = h.drain();
    click(vcx, "tab-menu-unpin");
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
    // A child of a folder is still a LINK, so its row is Unpin — see
    // `a_right_click_on_a_bookmark_removes_it`.
    click(vcx, "tab-menu-unpin");
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

/// Clicking a bookmark opens the bookmark's OWN tab and leaves yours
/// alone. This test used to pin the opposite — "sends the tab you are
/// looking at, which is what a bookmarks bar has meant in every
/// browser" — and the first live walk overruled it: a bookmark that
/// eats the page in front of you reads as losing your place, and the
/// Arc model (the tile IS the tab) is what the operator asked for by
/// name. See `Shell::open_bookmark`.
#[gpui::test]
fn clicking_a_bookmark_opens_its_own_tab_not_yours(cx: &mut TestAppContext) {
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
            Command::Browser(BrowserCmd::OpenAt { url, activate: true, .. })
                if url == "https://github.com/"
        )),
        "the bookmark should open into its own tab: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Navigate { tab_id, .. }) if tab_id == "p1"
        )),
        "the operator's own tab must be left alone: {cmds:?}"
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

/// New folder is Untitled, selected, ready to type over. A second
/// folder is also Untitled — numbering was a stand-in for the rename
/// field this now opens.
///
/// New folder is on the Folders dropdown (right-click),
/// not on the bookmark grid's `+`.
#[gpui::test]
fn a_new_folder_is_untitled_and_ready_to_type(cx: &mut TestAppContext) {
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
                if t == crate::bookmarks::UNTITLED
        )),
        "the Folders header made no Untitled folder: {cmds:?}"
    );

    // The id is minted by the engine. The harness injects the snapshot
    // the watch would have delivered; that is what opens the field.
    push_bookmarks(
        &h,
        vcx,
        &[("f1", 0, Some(crate::bookmarks::UNTITLED), None)],
    );
    vcx.run_until_parked();
    assert!(
        vcx.debug_bounds("rename-bookmark-f1").is_some(),
        "the new folder did not open for naming"
    );
    assert!(
        shown(vcx, "rename-selection"),
        "Untitled was not selected, so the next key would append"
    );
    assert_eq!(
        rename_buffer(vcx, &h).as_deref(),
        Some(crate::bookmarks::UNTITLED),
        "the field opened on something other than Untitled"
    );

    vcx.simulate_input("work");
    assert_eq!(
        rename_buffer(vcx, &h).as_deref(),
        Some("work"),
        "typing appended to Untitled instead of replacing it: {:?}",
        rename_buffer(vcx, &h)
    );
    assert!(
        !shown(vcx, "rename-selection"),
        "the selection wash stayed after the first key"
    );

    // The menu OCCLUDES for its whole mounted life, exit included
    // (`overlay::menu_layer`), and `run_until_parked` does not advance the
    // motion clock — so without this the first menu is still on screen,
    // still painting its rows, and taking no clicks.
    drive(&h, vcx, Duration::from_millis(16));

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
                if t == crate::bookmarks::UNTITLED
        )),
        "a second folder was numbered rather than Untitled: {cmds:?}"
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

    // Bare text: the three navigation rows, Mute, Copy, and the developer
    // pair — and nothing else.
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
    assert!(shown(vcx, "menu-row-3"), "Mute Tab is missing");
    assert!(shown(vcx, "menu-row-4"), "Copy is missing");
    assert!(shown(vcx, "menu-row-5"), "View Page Source is missing");
    assert!(shown(vcx, "menu-row-6"), "Inspect is missing");
    assert!(
        !shown(vcx, "menu-row-7"),
        "a menu over bare text offered a link row"
    );

    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
}

/// A menu over a link gains the THREE link rows; a menu over a selection
/// gains the search row.
///
/// **This test was red on `main` and the product was right.** A commit
/// added `Open Link in Incognito Tab` beside `Copy Link` / `Open Link in
/// New Tab` and did not move the arithmetic here, so the row count below
/// was one short and the last assertion fired on a row somebody had in
/// fact asked for. Corrected rather than deleted: the assertion is the
/// only thing standing between this menu and rows nobody ordered.
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
    // 0 Back, 1 Forward, 2 Reload, 3 Mute Tab, 4 Copy, 5 Copy Link,
    // 6 Open Link in New Tab, 7 Open Link in Incognito Tab, 8 Search,
    // 9 View Page Source, 10 Inspect. Eleven rows, so the first index
    // that must NOT exist is 11.
    assert!(shown(vcx, "menu-row-8"), "the search row is missing");
    assert!(
        !shown(vcx, "menu-row-11"),
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
    // Back, Forward, Reload, Mute Tab, Copy, View Page Source, Inspect —
    // and nothing after them.
    assert!(shown(vcx, "menu-row-4"), "the Copy row is missing");
    assert!(
        !shown(vcx, "menu-row-7"),
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
    // Back, Forward, Reload, Mute Tab, Copy, Reset, View Page Source,
    // Inspect.
    assert!(
        shown(vcx, "menu-row-5"),
        "a site with two answers offered no way to take them back"
    );
    click(vcx, "menu-row-5");
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
    // Back, Forward, Reload, Mute Tab, Copy, Copy Link, Open Link in New
    // Tab, View Page Source, Inspect.
    click(vcx, "menu-row-6");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::OpenAt { url, activate: false, .. })
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
    // Back, Forward, Reload, Mute Tab, Copy, Search, View Page Source,
    // Inspect.
    click(vcx, "menu-row-5");
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
    // Back, Forward, Reload, Mute Tab, Copy, View Page Source, Inspect.
    click(vcx, "menu-row-5");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::OpenAt { url, activate: true, .. })
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
    // Back, Forward, Reload, Mute Tab, Copy, View Page Source, Inspect.
    click(vcx, "menu-row-6");
    vcx.run_until_parked();
    assert_eq!(
        crate::web::take_inspect_for_test(),
        Some((tab.to_string(), 240, 275)),
        "Inspect never reached the web module, or lost the point"
    );
    // And the dock is OPEN from the click itself, not from Chromium's
    // later `on_after_created` — the pane must stand on the next render.
    assert!(
        crate::web::devtools_open(tab),
        "Inspect did not open the DevTools dock"
    );
}

/// The dock's state machine, which is all a build without Chromium can
/// see of it: Inspect opens, a second Inspect leaves it open (CEF
/// retargets the existing frontend), and closing clears it for exactly
/// that tab. The pane itself is a `page_element` on the `devtools:` id
/// and only exists with the feature on.
#[gpui::test]
fn the_devtools_dock_opens_once_and_closes(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/"));
    assert!(!crate::web::devtools_open(tab), "a dock nobody asked for");

    crate::web::inspect_element(tab, 10, 10);
    crate::web::inspect_element(tab, 20, 20);
    assert!(crate::web::devtools_open(tab));
    assert!(
        !crate::web::devtools_open("someone-else"),
        "one tab's Inspect opened every tab's dock"
    );

    crate::web::close_devtools(tab);
    assert!(
        !crate::web::devtools_open(tab),
        "the dock survived its own close"
    );
    let _ = (h, vcx);
}

/// **A rect eave cannot trust is refused, not clamped.**
///
/// This is the one place the docking contract can be held by a test: the
/// rect arrives as text on the frontend's console, and everything after
/// it is pixels. A malformed report that got clamped into something
/// plausible would put the page somewhere quietly wrong — and the only
/// symptom is visual, so nothing downstream would catch it. Refusing
/// leaves the last good rect standing, which is the safe wrong answer.
#[gpui::test]
fn a_malformed_dock_rect_is_refused_rather_than_guessed(_cx: &mut TestAppContext) {
    use crate::web::parse_dock_bounds as p;

    assert_eq!(p("0,0,800,600"), Some((0., 0., 800., 600.)));
    assert_eq!(p(" 12 , 34 , 56 , 78 "), Some((12., 34., 56., 78.)));

    for bad in [
        "",              // nothing at all
        "0,0,800",       // short — three numbers is not a rect
        "0,0,800,600,7", // long — an extra field means a different format
        "0,0,800,abc",   // not a number
        "0,0,0,600",     // zero width is not a placement
        "0,0,800,0",     // zero height either
        "0,0,-800,600",  // negative extent
        "-1,0,800,600",  // outside the frontend's own viewport
        "0,0,NaN,600",   // non-finite
        "0,0,inf,600",
    ] {
        assert_eq!(p(bad), None, "{bad:?} was accepted as a page rect");
    }
}

/// **No rect means the old layout, not a lost page.**
///
/// `dock_page_rect` is `None` for every frame between Inspect and the
/// frontend's first report — and for ever if the shim never runs at all,
/// which a Chromium bump could cause without any eave change. The shell
/// must fall back to the side-by-side dock rather than render a page with
/// nowhere to be.
#[gpui::test]
fn a_dock_with_no_reported_rect_keeps_the_old_layout(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/"));

    assert_eq!(
        crate::web::dock_page_rect(tab),
        None,
        "a rect before any frontend said anything"
    );

    crate::web::set_dock_page_rect(tab, (100., 0., 700., 600.));
    assert_eq!(crate::web::dock_page_rect(tab), Some((100., 0., 700., 600.)));

    // Closing must drop it: the next dock would otherwise open with the
    // page already placed where the last one had it.
    crate::web::close_devtools(tab);
    assert_eq!(
        crate::web::dock_page_rect(tab),
        None,
        "the rect outlived the frontend that reported it"
    );
}

/// **A divider that is let go stops stretching.** While one of eave's
/// dividers is dragged, the page surfaces paint their last CEF frame
/// stretched to the new bounds — without that, the pane that is growing
/// shows a black band that tracks the pointer for the whole gesture,
/// because every move hands Chromium a size it cannot answer before the
/// next frame.
///
/// The direction that needs a test is the *release*. Leaving the flag set
/// paints every page stretched for ever, from then on, with nothing on
/// screen to say why — and because the flag is published from `render`
/// rather than from the drag handlers, the guarantee is "a grab that ends
/// any way at all clears on the next frame", which is exactly what this
/// walks.
#[gpui::test]
fn letting_go_of_a_divider_stops_the_stretch(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    assert!(
        !crate::web::live_divider_for_test(),
        "nothing is being dragged and the pages are already stretching"
    );

    for grab in ["devtools", "split"] {
        h.window
            .update(vcx, |shell, _, cx| {
                let drag = SplitDrag {
                    handle: 0,
                    grab_x: 400.,
                    start_ratio: 0.5,
                    well_w: 800.,
                };
                if grab == "devtools" {
                    shell.devtools_drag = Some(drag);
                } else {
                    shell.split_drag = Some(drag);
                }
                cx.notify();
            })
            .unwrap();
        vcx.run_until_parked();
        assert!(
            crate::web::live_divider_for_test(),
            "the {grab} divider was moving and the surfaces were still clipping"
        );

        // Dropped, not mouse-upped: the shape a lost grab takes.
        h.window
            .update(vcx, |shell, _, cx| {
                shell.devtools_drag = None;
                shell.split_drag = None;
                cx.notify();
            })
            .unwrap();
        vcx.run_until_parked();
        assert!(
            !crate::web::live_divider_for_test(),
            "the {grab} grab ended and every page is still painting stretched"
        );
    }
}

/// **The dock can be shut from the same row that opened it.** The
/// frontend's own ✕ cannot do it — that button asks a host window eave
/// does not provide — so if this row did not toggle, the dock would be a
/// one-way door.
#[gpui::test]
fn the_inspect_row_becomes_the_close_row_while_the_dock_is_open(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/"));

    let menu = crate::web::PageMenu {
        tab_id: tab.to_string(),
        x: 200.,
        y: 200.,
        page_x: 200,
        page_y: 200,
        link_url: None,
        selection: None,
        editable: false,
    };
    request_page_menu(&h, vcx, menu.clone());
    // Back, Forward, Reload, Mute Tab, Copy, View Page Source, Inspect.
    click(vcx, "menu-row-6");
    vcx.run_until_parked();
    assert!(crate::web::devtools_open(tab), "the dock never opened");

    request_page_menu(&h, vcx, menu);
    click(vcx, "menu-row-6");
    vcx.run_until_parked();
    assert!(
        !crate::web::devtools_open(tab),
        "the row that opened the dock did not close it"
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
    assert!(shown(vcx, "split-handle-0"), "no divider between panes");
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

/// **Slice SP1 — a split outlives the selection.** Choosing a tab outside
/// it shows that tab alone; the split keeps its pill on the rail, and
/// choosing a member brings the whole arrangement back. The first cut
/// dissolved the split on the next click elsewhere, which the operator
/// reported as "when I combine two tabs and click off of them they
/// separate".
///
/// The activation is answered by pushing the engine's snapshot back, the
/// way every strip test does — the shell never moves `active` itself.
#[gpui::test]
fn clicking_a_third_tab_shows_it_alone_and_keeps_the_split(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let tabs = || {
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
            page_tab("p3", Some("https://c.test/"), Some("C")),
        ]
    };
    strip_tabs(&h, vcx, tabs(), Some("p1"));
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
        })
        .unwrap();
    vcx.run_until_parked();
    assert!(shown(vcx, "split-well"));
    h.drain();

    click(vcx, "tab-p3");
    vcx.run_until_parked();
    assert!(
        h.drain()
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Activate(id)) if id == "p3")),
        "clicking the third tab did not ask for it"
    );
    strip_tabs(&h, vcx, tabs(), Some("p3"));
    assert!(
        !shown(vcx, "split-well"),
        "a tab outside the split must fill the card alone"
    );
    assert!(
        shown(vcx, "split-tabs"),
        "the split's pill left the rail when a third tab was chosen"
    );
    let kept = h
        .window
        .update(vcx, |shell, _, _| shell.splits.clone())
        .unwrap();
    assert_eq!(kept.len(), 1, "the split was dissolved: {kept:?}");
    assert_eq!(kept[0].panes, ["p1", "p2"]);

    strip_tabs(&h, vcx, tabs(), Some("p2"));
    assert!(
        shown(vcx, "split-well") && shown(vcx, "split-pane-left") && shown(vcx, "split-pane-right"),
        "choosing a member did not bring the split back"
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

/// Two tabs in a split share ONE rail pill, side by side, and a tab that
/// is not in the split stays a row under it (T3's combined row; ID2b′
/// back in PAGES).
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
    let before = vcx.debug_bounds("split-handle-0").expect("handle");
    open_tab_strip_menu(&h, vcx, "p1");
    click(vcx, "tab-menu-convert-axis");
    vcx.run_until_parked();
    let after = vcx.debug_bounds("split-handle-0").expect("handle after");
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

/// Slice SP1 — a split stacks to four panes and refuses a fifth. Each
/// drop lands beside the pane it was dropped on; the divider count is the
/// pane count less one; the fifth tab stays a row of its own under the
/// pill and the pill keeps four chips.
#[gpui::test]
fn a_split_stacks_to_four_panes_and_refuses_a_fifth(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tabs = || {
        (1..=5)
            .map(|i| {
                page_tab(
                    ["p1", "p2", "p3", "p4", "p5"][i - 1],
                    Some("https://a.test/"),
                    Some("A"),
                )
            })
            .collect::<Vec<_>>()
    };
    strip_tabs(&h, vcx, tabs(), Some("p1"));
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
            shell.split_tabs("p3", "p2", true, cx);
        })
        .unwrap();
    strip_tabs(&h, vcx, tabs(), Some("p3"));
    assert!(shown(vcx, "split-pane-left"), "three panes: no left");
    assert!(shown(vcx, "split-pane-mid-1"), "three panes: no middle");
    assert!(shown(vcx, "split-pane-right"), "three panes: no right");
    assert!(
        shown(vcx, "split-handle-1"),
        "three panes need two dividers"
    );
    assert!(
        !shown(vcx, "split-handle-2"),
        "three panes drew a third divider"
    );

    // Dropped on the FIRST pane's left: the new pane leads.
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p4", "p1", false, cx);
        })
        .unwrap();
    strip_tabs(&h, vcx, tabs(), Some("p4"));
    let panes = h
        .window
        .update(vcx, |shell, _, _| shell.splits[0].panes.clone())
        .unwrap();
    assert_eq!(panes, ["p4", "p1", "p2", "p3"]);
    assert!(
        shown(vcx, "split-pane-mid-2"),
        "four panes: no second middle"
    );
    assert!(
        shown(vcx, "split-handle-2"),
        "four panes need three dividers"
    );
    let l = vcx.debug_bounds("split-pane-left").expect("left");
    let r = vcx.debug_bounds("split-pane-right").expect("right");
    assert!(
        l.size.width > px(100.) && r.size.width > px(100.),
        "four equal panes collapsed one: left {:?} right {:?}",
        l.size.width,
        r.size.width
    );

    // The fifth is refused, on every door: the drop, the well, the menu.
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p5", "p2", true, cx);
            shell.drop_tab_on_well("p5", true, cx);
        })
        .unwrap();
    strip_tabs(&h, vcx, tabs(), Some("p4"));
    let panes = h
        .window
        .update(vcx, |shell, _, _| shell.splits[0].panes.clone())
        .unwrap();
    assert_eq!(panes, ["p4", "p1", "p2", "p3"], "a fifth pane got in");
    let pill = vcx.debug_bounds("split-tabs").expect("pill");
    let p5 = vcx.debug_bounds("tab-p5").expect("tab-p5");
    assert!(
        p5.origin.y > pill.origin.y + pill.size.height - px(1.),
        "the refused tab must stay a row under the pill: pill {pill:?} p5 {p5:?}"
    );
    open_tab_strip_menu(&h, vcx, "p2");
    click(vcx, "tab-menu-member");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "tab-menu-add-split"),
        "a member's own menu is missing Add Split View"
    );
    click(vcx, "tab-menu-add-split");
    vcx.run_until_parked();
    let panes = h
        .window
        .update(vcx, |shell, _, _| shell.splits[0].panes.clone())
        .unwrap();
    assert_eq!(panes.len(), 4, "the menu added a fifth pane: {panes:?}");
}

/// Slice SP1 — two splits coexist, each keeps its pill, and the well
/// shows whichever one the active tab belongs to. Moving a tab from one
/// split to the other leaves the first, which, down to one pane, is a
/// tab again.
#[gpui::test]
fn two_splits_keep_apart_and_a_tab_is_in_one_at_a_time(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tabs = || {
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
            page_tab("p3", Some("https://c.test/"), Some("C")),
            page_tab("p4", Some("https://d.test/"), Some("D")),
        ]
    };
    strip_tabs(&h, vcx, tabs(), Some("p1"));
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
            shell.split_tabs("p4", "p3", true, cx);
        })
        .unwrap();
    strip_tabs(&h, vcx, tabs(), Some("p4"));
    let splits = h
        .window
        .update(vcx, |shell, _, _| shell.splits.clone())
        .unwrap();
    assert_eq!(splits.len(), 2, "{splits:?}");
    assert_eq!(splits[0].panes, ["p1", "p2"]);
    assert_eq!(splits[1].panes, ["p3", "p4"]);
    // Both pills on the rail, one row each, the second under the first.
    let a = vcx.debug_bounds("tab-p1").expect("tab-p1");
    let b = vcx.debug_bounds("tab-p2").expect("tab-p2");
    let c = vcx.debug_bounds("tab-p3").expect("tab-p3");
    let d = vcx.debug_bounds("tab-p4").expect("tab-p4");
    assert_eq!(a.origin.y, b.origin.y, "first pill split across rows");
    assert_eq!(c.origin.y, d.origin.y, "second pill split across rows");
    assert!(c.origin.y > a.origin.y, "the second split is not under the first");
    // The well shows the split the active tab is in.
    let pane = vcx.debug_bounds("split-pane-left").expect("left pane");
    let header = vcx.debug_bounds("pane-header-left").expect("left header");
    assert!(
        header.origin.x >= pane.origin.x && shown(vcx, "split-well"),
        "the well did not draw p4's split"
    );

    // p2 moves to the second split: the first is a tab again.
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p4", true, cx);
        })
        .unwrap();
    strip_tabs(&h, vcx, tabs(), Some("p2"));
    let splits = h
        .window
        .update(vcx, |shell, _, _| shell.splits.clone())
        .unwrap();
    assert_eq!(splits.len(), 1, "{splits:?}");
    assert_eq!(splits[0].panes, ["p3", "p4", "p2"]);
    let a = vcx.debug_bounds("tab-p1").expect("tab-p1");
    let b = vcx.debug_bounds("tab-p2").expect("tab-p2");
    assert!(
        a.origin.y != b.origin.y,
        "p1 is still drawn in a pill with p2"
    );
}

/// Slice SP1 — closing a pane leaves the others; closing the focused
/// pane hands focus to its neighbour on the left so the successor walk
/// cannot land on a tab outside the split. A pair down to one pane is
/// a tab again, and a tab the strip lost (closed elsewhere) is pruned
/// by the observer, not left as a pane the well cannot draw.
#[gpui::test]
fn closing_or_losing_a_pane_shrinks_the_split(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let tabs = |ids: &[&str]| {
        ids.iter()
            .map(|id| page_tab(id, Some("https://a.test/"), Some("A")))
            .collect::<Vec<_>>()
    };
    strip_tabs(&h, vcx, tabs(&["p1", "p2", "p3", "p4"]), Some("p1"));
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
            shell.split_tabs("p3", "p2", true, cx);
        })
        .unwrap();
    strip_tabs(&h, vcx, tabs(&["p1", "p2", "p3", "p4"]), Some("p3"));
    h.drain();

    h.window
        .update(vcx, |shell, _, cx| shell.close_tab("p3", cx))
        .unwrap();
    vcx.run_until_parked();
    let sent = h.drain();
    assert!(
        sent.iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Activate(id)) if id == "p2")),
        "closing the focused right pane did not hand focus to its left neighbour: {sent:?}"
    );
    assert!(
        !sent
            .iter()
            .any(|c| matches!(c, Command::Browser(BrowserCmd::Activate(id)) if id == "p4")),
        "the successor walk reached a tab outside the split"
    );
    let panes = h
        .window
        .update(vcx, |shell, _, _| shell.splits[0].panes.clone())
        .unwrap();
    assert_eq!(panes, ["p1", "p2"]);

    // p2 goes away without this shell closing it (another window, the
    // engine): the next snapshot prunes it and the pair is a tab again.
    strip_tabs(&h, vcx, tabs(&["p1", "p4"]), Some("p1"));
    let splits = h
        .window
        .update(vcx, |shell, _, _| shell.splits.clone())
        .unwrap();
    assert!(
        splits.is_empty(),
        "a lost pane left a split behind: {splits:?}"
    );
    assert!(!shown(vcx, "split-well"));
    assert!(!shown(vcx, "split-tabs"));

    // But an EMPTY strip prunes nothing — that is the boot shape, before
    // the engine has answered, and the saved splits must survive it.
    strip_tabs(&h, vcx, tabs(&["p1", "p4"]), Some("p1"));
    h.window
        .update(vcx, |shell, _, cx| shell.split_tabs("p4", "p1", true, cx))
        .unwrap();
    strip_tabs(&h, vcx, Vec::new(), None);
    let splits = h
        .window
        .update(vcx, |shell, _, _| shell.splits.clone())
        .unwrap();
    assert_eq!(splits.len(), 1, "an empty snapshot erased the arrangement");
}

/// Slice SP1 — one session per split: a session joins a split of pages
/// and takes the transcript, a second session is refused, and the
/// partner "Add Split View" offers skips a session when the split
/// already has one.
#[gpui::test]
fn a_split_holds_one_session_and_refuses_a_second(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tabs = || {
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            page_tab("p2", Some("https://b.test/"), Some("B")),
            session_tab("s1"),
            session_tab("s2"),
        ]
    };
    strip_tabs(&h, vcx, tabs(), Some("p1"));
    h.window
        .update(vcx, |shell, _, cx| {
            shell.split_tabs("p2", "p1", true, cx);
            shell.split_tabs("s1", "p2", true, cx);
            shell.split_tabs("s2", "p1", true, cx);
        })
        .unwrap();
    strip_tabs(&h, vcx, tabs(), Some("s1"));
    let splits = h
        .window
        .update(vcx, |shell, _, _| shell.splits.clone())
        .unwrap();
    assert_eq!(splits.len(), 1, "{splits:?}");
    assert_eq!(
        splits[0].panes,
        ["p1", "p2", "s1"],
        "a second session got in"
    );
    assert!(shown(vcx, "split-well"));
    let partner = h
        .window
        .update(vcx, |shell, _, cx| shell.split_partner_for("p1", cx))
        .unwrap();
    assert_eq!(
        partner, None,
        "the only tab left is a session and the split has one: {partner:?}"
    );
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

// ---------------------------------------------------------------------------
// G2 — a notification that is actually on screen
// ---------------------------------------------------------------------------

/// Put a notification through the whole path a page uses, minus Chromium.
///
/// `web::drive_notifications` runs in the render pass, so pushing a
/// `Runtime.bindingCalled` event and rendering is exactly what a page
/// calling `new Notification(..)` does — `cdp_call`'s feature-off twin
/// issues the calls and nothing here needs a browser.
fn page_notifies(h: &Harness, vcx: &mut VisualTestContext, origin: &str, payload: &str) {
    let url = format!("{origin}/app");
    strip_tabs(h, vcx, vec![page_tab("t-note", Some(&url), None)], Some("t-note"));
    // First render installs the shim; the event can only follow it.
    crate::web::cdp::note_event(
        "t-note",
        "Runtime.bindingCalled",
        &serde_json::json!({
            "name": crate::web::NOTIFY_BINDING,
            "payload": payload,
        })
        .to_string(),
    );
    vcx.run_until_parked();
    drive(h, vcx, Duration::from_millis(16));
}

/// The row, as a thing you can see.
///
/// Every other G2 test asserts about decisions and CDP calls. This one
/// asserts the pixels exist, which is the claim `0f9fd22` could not make
/// and the one that matters to a person.
#[gpui::test]
fn a_granted_notification_appears_over_the_page(cx: &mut TestAppContext) {
    let (h, vcx) = &mut boot(cx);
    let origin = "https://harness-granted.example";
    crate::web::permissions::note_answer(
        origin,
        &[eave_core::browser::permissions::PermissionKind::Notifications],
        crate::web::permissions::PermissionDecision::Allow,
    );
    page_notifies(
        h,
        vcx,
        origin,
        r#"{"kind":"show","seq":1,"title":"Build done","body":"3 tests"}"#,
    );
    assert!(shown(vcx, "notification-stack"), "the stack never mounted");
    assert!(shown(vcx, "notification-toast"), "no toast on screen");
    assert!(
        shown(vcx, "notification-origin"),
        "the origin line is the one part of the card the page does not \
         control; a toast without it is unattributable"
    );
}

/// **The G2 defect itself, as a UI assertion.**
///
/// A screenshot of a working toast would not catch this: the failure was
/// never that notifications did not appear, it was that a page was told a
/// notification HAD appeared when nothing did. So the load-bearing test is
/// the negative one — no grant, no pixels, and the page hears `error`.
#[gpui::test]
fn a_notification_without_permission_never_reaches_the_screen(cx: &mut TestAppContext) {
    let (h, vcx) = &mut boot(cx);
    page_notifies(
        h,
        vcx,
        "https://harness-ungranted.example",
        r#"{"kind":"show","seq":1,"title":"trust me"}"#,
    );
    assert!(
        !shown(vcx, "notification-toast"),
        "a page with no grant put something on screen"
    );
    assert!(!shown(vcx, "notification-stack"));
}

/// S1: the Shortcuts pane is a viewer of the keymap the app actually
/// boots — asserted against the REAL `bootstrap`, not a hand-bound
/// replica. The chords the directive names must be present under the
/// labels the pane will print, ⌘, must now be Settings (not the Theme
/// Editor, which moved to ⌘⇧,), and no group may come out empty.
#[gpui::test]
fn the_shortcut_viewer_sees_the_booted_keymap(cx: &mut TestAppContext) {
    let (_h, _vcx) = boot(cx);
    cx.update(|cx| {
        let groups = crate::settings_window::shortcut_groups(cx, "");
        let find = |label: &str| -> Option<String> {
            groups.iter().flat_map(|g| g.rows.iter()).find_map(|r| {
                (r.label == label).then(|| r.chords.join(" · "))
            })
        };
        for (label, chord) in [
            ("New tab", "⌘T"),
            ("New incognito tab", "⇧⌘N"),
            ("Close the tab — the window when none is open", "⌘W"),
            ("Open the omnibox", "⌘L"),
            ("Find in page", "⌘F"),
            ("Back", "⌘["),
            ("Forward", "⌘]"),
            ("Reload the page", "⌘R"),
            ("Reopen the closed tab", "⇧⌘T"),
            ("Switch to the last tab", "⌘9"),
            ("Settings", "⌘,"),
            ("Customize Theme", "⇧⌘,"),
        ] {
            assert_eq!(
                find(label).as_deref(),
                Some(chord),
                "the viewer lost or mis-chords {label:?}"
            );
        }
        assert!(
            find("Switch to tab 1–8").is_some(),
            "the ⌘1–8 run collapsed wrong: {:?}",
            groups
                .iter()
                .flat_map(|g| g.rows.iter().map(|r| r.label.clone()))
                .collect::<Vec<_>>()
        );
        for g in &groups {
            assert!(!g.rows.is_empty(), "group {} rendered empty", g.name);
        }
    });
}

/// S1: every settings row round-trips — clicked in the real window,
/// applied live, written to disk, and the file parses back to exactly the
/// state on screen (which is what a relaunch reads; `set_settings_path`
/// is one `serde_json::from_str` over this same file). The rail width is
/// asserted live-side only: its persistence is `frame.json`, whose
/// `DATA_DIR` is deliberately unset under the harness.
#[gpui::test]
fn every_settings_row_round_trips(cx: &mut TestAppContext) {
    use crate::settings::{self, AppearanceSetting, SearchEngine};

    // S2 added sibling tests that also mutate the process-global store;
    // whoever writes settings holds this (see `settings_test_lock`).
    let _settings_turn = crate::settings::settings_test_lock();
    let (h, _vcx) = boot(cx);

    // A real file in the test temp dir. `set_settings_path` is a OnceLock:
    // this is the only test that sets it, and the tests that never do are
    // unaffected (their writes stay in the mirror).
    let dir = std::env::temp_dir().join(format!("eave-s1-roundtrip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("settings.json");
    let _ = std::fs::remove_file(&path);
    settings::set_settings_path(path.clone());

    let handle = cx
        .update(crate::settings_window::open)
        .expect("the settings window opens");
    let svcx = VisualTestContext::from_window(handle.into(), cx).into_mut();
    svcx.update(|window, _| window.refresh());
    svcx.run_until_parked();

    let on_disk = |path: &std::path::Path| -> crate::settings::Settings {
        serde_json::from_str(&std::fs::read_to_string(path).expect("settings.json written"))
            .expect("settings.json parses")
    };

    // Tabs pane: the eviction dial applies live — the thresholds the
    // eviction clock reads move with the click, no relaunch anywhere.
    click(svcx, "settings-pane-Tabs");
    click(svcx, "settings-seg-5 min");
    assert_eq!(settings::snapshot().evict_after_min, Some(5));
    assert_eq!(
        eave_core::thresholds::page_idle_evict_after(),
        std::time::Duration::from_secs(5 * 60),
        "the clicked threshold did not reach the eviction clock"
    );
    click(svcx, "settings-seg-Off");
    assert_eq!(settings::snapshot().evict_after_min, None);
    assert_eq!(
        eave_core::thresholds::page_idle_evict_after(),
        std::time::Duration::from_millis(u64::MAX),
        "Off must be an idle age no tab reaches"
    );
    assert_eq!(on_disk(&path).evict_after_min, None, "Off did not persist");

    click(svcx, "settings-seg-Is switched to");
    assert!(settings::snapshot().switch_to_background_tabs);
    assert!(settings::switch_to_background_tabs(), "the no-cx door lags");

    // General pane: appearance and search engine, with the live-apply
    // proof on the fact that actually moves — the prefix the omnibox
    // sends queries to.
    click(svcx, "settings-pane-General");
    click(svcx, "settings-seg-Dark");
    assert_eq!(settings::snapshot().appearance, AppearanceSetting::Dark);
    click(svcx, "settings-seg-DuckDuckGo");
    assert_eq!(settings::snapshot().search, SearchEngine::DuckDuckGo);
    assert_eq!(
        crate::omnibox::search_prefix(),
        crate::settings::DUCKDUCKGO_SEARCH,
        "the picked engine did not reach the omnibox"
    );
    // Custom with nothing typed falls back rather than searching "".
    click(svcx, "settings-seg-Custom");
    assert!(matches!(
        settings::snapshot().search,
        SearchEngine::Custom { ref prefix } if prefix.is_empty()
    ));
    assert_eq!(crate::omnibox::search_prefix(), crate::omnibox::DEFAULT_SEARCH);

    // The file now holds every change made above, and parses back to the
    // exact state on screen — the half of quit→relaunch that is code
    // rather than process lifecycle.
    let persisted = on_disk(&path);
    assert_eq!(persisted, settings::snapshot());
    assert_eq!(persisted.appearance, AppearanceSetting::Dark);
    assert!(persisted.switch_to_background_tabs);

    // Rail width: the settings store moves the live Shell — the frame
    // follows a write it did not make.
    cx.update(|cx| settings::set_rail_w(cx, 250.0));
    cx.run_until_parked();
    let shell_rail = h.window.update(cx, |shell, _, _| shell.rail_w).unwrap();
    assert_eq!(shell_rail, 250.0, "the Shell did not follow the store");

    // The Shortcuts and Info panes render from the state the other tests
    // pin; switching to them must not disturb the struct.
    click(svcx, "settings-pane-Shortcuts");
    click(svcx, "settings-pane-Info");
    assert_eq!(on_disk(&path), settings::snapshot());

    // Restore defaults — the store is process-global and one harness
    // assertion reads `search_prefix()` live.
    click(svcx, "settings-pane-General");
    click(svcx, "settings-seg-Google");
    click(svcx, "settings-seg-System");
    click(svcx, "settings-pane-Tabs");
    click(svcx, "settings-seg-20 min");
    click(svcx, "settings-seg-Stays in the background");
    cx.update(|cx| settings::set_rail_w(cx, theme::RAIL_W));
    assert_eq!(settings::snapshot(), crate::settings::Settings::default());
    assert_eq!(on_disk(&path), crate::settings::Settings::default());
}

/// S2: a stored default model pre-selects the launcher's picker —
/// visibly — and start carries exactly what the picker shows. The
/// default reaches the launch by PRESELECTION only: `launch()` sends
/// `launcher.model` and never consults the store for a model, so what
/// starts is always what was on screen.
#[gpui::test]
fn a_default_model_preselects_the_launcher_and_reaches_the_launch(cx: &mut TestAppContext) {
    // Lock order: probes first, then settings — every test taking both
    // must agree or two of them deadlock.
    let _probes = eave_core::capabilities::probe_test_lock();
    eave_core::capabilities::forget();
    let _settings = crate::settings::settings_test_lock();
    let (mut h, vcx) = boot(cx);
    cx.update(|cx| {
        crate::settings_agents::write_defaults(cx, |d| {
            d.set_model("claude-code", Some("opus".into()))
        })
    });
    // Slice NC1: no tiles — the picker's provider switch is
    // `pick_provider`, which re-reads the store on the way in.
    h.window
        .update(vcx, |shell, _, cx| shell.pick_provider(3, cx)) // grok
        .unwrap();
    vcx.run_until_parked();
    h.window
        .update(vcx, |shell, _, cx| shell.pick_provider(0, cx)) // claude again
        .unwrap();
    vcx.run_until_parked();
    let chosen = vcx
        .read_window(&h.window, |shell, cx| shell.read(cx).launcher.model)
        .unwrap();
    assert_eq!(
        chosen,
        Some("opus"),
        "the stored default did not pre-select"
    );
    let _ = h.drain();
    start_chat(vcx, "hello");
    match &h.drain()[0] {
        Command::Launch { model, .. } => assert_eq!(model.as_deref(), Some("opus")),
        other => panic!("start emitted {other:?}"),
    }
    cx.update(|cx| {
        crate::settings_agents::write_defaults(cx, |d| d.set_model("claude-code", None))
    });
}

/// S2: an explicit picker choice beats the stored default. The picker
/// moved off its preselection must not be refilled at start.
#[gpui::test]
fn an_explicit_picker_choice_beats_the_stored_default(cx: &mut TestAppContext) {
    let _probes = eave_core::capabilities::probe_test_lock();
    eave_core::capabilities::forget();
    let _settings = crate::settings::settings_test_lock();
    let (mut h, vcx) = boot(cx);
    cx.update(|cx| {
        crate::settings_agents::write_defaults(cx, |d| {
            d.set_model("claude-code", Some("claude-opus-5".into()))
        })
    });
    h.window
        .update(vcx, |shell, _, cx| shell.pick_provider(3, cx))
        .unwrap();
    vcx.run_until_parked();
    h.window
        .update(vcx, |shell, _, cx| shell.pick_provider(0, cx))
        .unwrap();
    vcx.run_until_parked();
    click(vcx, "composer-model");
    vcx.run_until_parked();
    assert!(shown(vcx, "model-picker"), "the model menu never opened");
    click(vcx, "model-picker-row-0"); // Sonnet 5, not the stored Opus 5
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(16));
    let chosen = vcx
        .read_window(&h.window, |shell, cx| shell.read(cx).launcher.model)
        .unwrap();
    assert_eq!(
        chosen,
        Some("claude-sonnet-5"),
        "the explicit pick did not stick"
    );
    let _ = h.drain();
    start_chat(vcx, "hello");
    match &h.drain()[0] {
        Command::Launch { model, .. } => assert_eq!(
            model.as_deref(),
            Some("claude-sonnet-5"),
            "the stored default resurrected behind an explicit choice"
        ),
        other => panic!("start emitted {other:?}"),
    }
    cx.update(|cx| {
        crate::settings_agents::write_defaults(cx, |d| d.set_model("claude-code", None))
    });
}

/// S2: default flags reach the launch — and the launcher SAYS so before
/// the click, because they apply with no per-session picker. Both halves
/// asserted: nothing configured → nothing claimed, configured → stated
/// and carried.
#[gpui::test]
fn default_flags_reach_the_launch_and_are_stated(cx: &mut TestAppContext) {
    let _settings = crate::settings::settings_test_lock();
    let (mut h, vcx) = boot(cx);
    assert!(
        !shown(vcx, "launcher-default-flags"),
        "the launcher claims default flags nobody configured"
    );
    cx.update(|cx| {
        crate::settings_agents::write_defaults(cx, |d| {
            d.set_extra_args("claude-code", vec!["--verbose".into()])
        })
    });
    vcx.run_until_parked();
    assert!(
        shown(vcx, "launcher-default-flags"),
        "flags will apply at start and nothing on the launcher says so"
    );
    let _ = h.drain();
    start_chat(vcx, "hello");
    match &h.drain()[0] {
        Command::Launch {
            model, extra_args, ..
        } => {
            assert_eq!(extra_args, &["--verbose".to_string()]);
            assert_eq!(
                model.as_deref(),
                Some("claude-sonnet-5"),
                "flags must not replace the model the launcher already shows"
            );
        }
        other => panic!("start emitted {other:?}"),
    }
    cx.update(|cx| {
        crate::settings_agents::write_defaults(cx, |d| d.set_extra_args("claude-code", vec![]))
    });
}

/// S2: the Agents pane writes a default with one click, refuses a
/// posture-defeating flag at entry, and keeps the last accepted flags
/// standing — driven through the real window, the real field entity and
/// the real observer, not through the module's internals.
#[gpui::test]
fn the_agents_pane_takes_a_default_and_refuses_a_posture_flag(cx: &mut TestAppContext) {
    let _probes = eave_core::capabilities::probe_test_lock();
    eave_core::capabilities::forget();
    let _settings = crate::settings::settings_test_lock();
    let (_h, _vcx) = boot(cx);
    let handle = cx
        .update(crate::settings_window::open)
        .expect("the settings window opens");
    let svcx = VisualTestContext::from_window(handle.into(), cx).into_mut();
    svcx.update(|window, _| window.refresh());
    svcx.run_until_parked();
    click(svcx, "settings-pane-Agents");
    svcx.run_until_parked();

    // Native id on claude's static list (the harness never arms probes,
    // so the static list is what the segments show).
    click(svcx, "agents-model-claude-code-claude-sonnet-5");
    svcx.run_until_parked();
    assert_eq!(
        crate::settings::snapshot().agents.model_for("claude-code"),
        Some("claude-sonnet-5")
    );
    click(svcx, "agents-model-claude-code-claude-opus-5");
    svcx.run_until_parked();
    assert_eq!(
        crate::settings::snapshot().agents.model_for("claude-code"),
        Some("claude-opus-5")
    );

    let field = svcx
        .update(|_, cx| crate::settings_agents::flags_field(cx, "claude-code"))
        .expect("the pane rendered claude's flags field");
    svcx.update(|_, cx| field.update(cx, |f, cx| f.set_content("--verbose", cx)));
    svcx.run_until_parked();
    assert_eq!(
        crate::settings::snapshot().agents.extra_args_for("claude-code"),
        ["--verbose"]
    );
    // A posture-defeating flag is refused at ENTRY: settings keep the
    // last accepted value, exactly what the caption tells the operator.
    svcx.update(|_, cx| {
        field.update(cx, |f, cx| {
            f.set_content("--verbose --permission-mode acceptEdits", cx)
        })
    });
    svcx.run_until_parked();
    assert_eq!(
        crate::settings::snapshot().agents.extra_args_for("claude-code"),
        ["--verbose"],
        "a flag that undoes the posture reached settings"
    );

    cx.update(|cx| {
        crate::settings_agents::write_defaults(cx, |d| d.set_model("claude-code", None))
    });
    svcx.update(|_, cx| field.update(cx, |f, cx| f.set_content("", cx)));
    svcx.run_until_parked();
    assert!(
        crate::settings::snapshot().agents.is_empty(),
        "cleared defaults left residue in the store"
    );
}

// ---------------------------------------------------------------------------
// Slice D4 — the downloads panel
// ---------------------------------------------------------------------------

/// A download the harness hands the panel. There is no Chromium in the
/// default feature state to produce one, which is the whole reason
/// `web::set_downloads_for_test` exists — the panel has to be drivable in
/// the build the suite actually runs.
fn download(
    id: u32,
    name: &str,
    percent: i32,
    done: bool,
    cancelled: bool,
) -> crate::web::Download {
    crate::web::Download {
        id,
        url: format!("https://example.com/{name}"),
        file_name: name.to_string(),
        path: format!("/tmp/{name}"),
        received: 0,
        total: 0,
        percent,
        done,
        cancelled,
    }
}

/// A page tab, with `list` already in the download map when it renders.
fn page_with_downloads(h: &Harness, vcx: &mut VisualTestContext, list: Vec<crate::web::Download>) {
    crate::web::set_downloads_for_test(list);
    strip_tabs(
        h,
        vcx,
        vec![page_tab("p1", Some("https://example.com/"), None)],
        Some("p1"),
    );
}

/// **The control arrives with the first download, and says whether one is
/// still running.**
///
/// Absent on an empty list on purpose: eave's downloads live in a
/// `thread_local!` map with no durable model behind it, so a standing
/// control would open onto `No downloads` for the whole of every fresh run
/// and promise a history eave does not keep. What it says while it is
/// there is exactly what eave knows.
///
/// The running/settled distinction is asserted through the SELECTOR
/// because the harness cannot read ink, and a lit state only the eye can
/// check is a lit state no test can defend.
#[gpui::test]
fn the_downloads_control_arrives_with_the_first_download(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    page_with_downloads(&h, vcx, Vec::new());
    assert!(
        !shown(vcx, "downloads-button"),
        "a session that has downloaded nothing offered a downloads control"
    );

    page_with_downloads(&h, vcx, vec![download(7, "report.pdf", 42, false, false)]);
    assert!(
        shown(vcx, "downloads-button"),
        "a running download put no control on the toolbar"
    );
    assert!(
        shown(vcx, "downloads-glyph-running"),
        "a transfer in flight is drawn as settled"
    );

    page_with_downloads(&h, vcx, vec![download(7, "report.pdf", 100, true, false)]);
    assert!(
        shown(vcx, "downloads-button"),
        "the control left with the transfer"
    );
    assert!(
        shown(vcx, "downloads-glyph"),
        "a finished transfer is still drawn as running"
    );
}

/// **The two things a download row can do, done from the panel.**
///
/// This is the assertion `574d4a3` could not make: cancel and reveal were
/// built and unit-tested, and there was no surface that could reach
/// either. A capability nothing can invoke is the shape E8 sat in for a
/// day.
///
/// **Reveal, never Open.** Chromium quarantines every download so that
/// Gatekeeper gets a say before anything executes (06-browser G4), and a
/// browser that launches its own downloads on one click hands that away.
/// The seam records the path rather than spawning, because a suite that
/// reveals is a suite that opens Finder on the machine running it.
#[gpui::test]
fn a_download_row_cancels_what_is_running_and_reveals_what_finished(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    page_with_downloads(
        &h,
        vcx,
        vec![
            download(7, "report.pdf", 42, false, false),
            download(8, "notes.md", 100, true, false),
        ],
    );

    click(vcx, "downloads-button");
    vcx.run_until_parked();
    // Row 0 is the `Downloads` header; the transfers follow it.
    assert!(shown(vcx, "menu-row-1"), "the panel drew no rows");
    assert!(shown(vcx, "menu-row-2"), "the second download is missing");
    assert!(
        !shown(vcx, "menu-row-3"),
        "the panel invented a row for a download that does not exist"
    );

    let _ = crate::web::take_cancelled_downloads_for_test();
    click(vcx, "tab-menu-download-cancel");
    vcx.run_until_parked();
    assert_eq!(
        crate::web::take_cancelled_downloads_for_test(),
        vec![7],
        "the running row stopped nothing"
    );

    // **Let the first menu finish leaving before opening the second.** A
    // closing popup is still painted and still occluding, so a click
    // aimed at the button lands on the corpse and the row that follows is
    // clicked on a layer that is no longer live — the exact
    // painted-but-dead shape that cost red main a day. `drive` runs until
    // the window stops asking for frames, which is when the exit is over.
    drive(&h, vcx, Duration::from_millis(16));
    click(vcx, "downloads-button");
    vcx.run_until_parked();
    let _ = crate::web::take_revealed_downloads_for_test();
    click(vcx, "tab-menu-download-reveal");
    vcx.run_until_parked();
    assert_eq!(
        crate::web::take_revealed_downloads_for_test(),
        vec!["/tmp/notes.md".to_string()],
        "the finished row showed nothing in Finder"
    );
}

/// **A transfer whose total Chromium does not know still offers to stop,
/// and a stopped one offers nothing.**
///
/// `percent_complete` is `-1` when the receive total is unknown
/// (`cef_download_item_capi.h:102`) — the ordinary case for a chunked
/// response, not a rare one. What that does to the LABEL is
/// `download_rows`' business and is asserted there; what it must not do is
/// cost the row its cancel, which is the only control a download with no
/// end in sight has.
///
/// The inert half carries a **positive control**, because "clicking it did
/// nothing" is what a test that missed the row entirely also reports. A
/// live cancel row sits beside the dead one and is clicked on the same
/// open menu: if that fires, the panel was reachable, and the silence
/// belongs to the disabled row rather than to the click.
#[gpui::test]
fn an_unknown_total_still_cancels_and_a_cancelled_row_is_inert(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    page_with_downloads(&h, vcx, vec![download(9, "stream.bin", -1, false, false)]);
    click(vcx, "downloads-button");
    vcx.run_until_parked();
    let _ = crate::web::take_cancelled_downloads_for_test();
    click(vcx, "tab-menu-download-cancel");
    vcx.run_until_parked();
    assert_eq!(
        crate::web::take_cancelled_downloads_for_test(),
        vec![9],
        "a download with no known total could not be stopped"
    );

    drive(&h, vcx, Duration::from_millis(16));
    page_with_downloads(
        &h,
        vcx,
        vec![
            download(9, "stream.bin", 12, false, true),
            download(10, "other.bin", 30, false, false),
        ],
    );
    click(vcx, "downloads-button");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "tab-menu-download-inert"),
        "a cancelled transfer vanished from the panel instead of saying so"
    );

    let _ = crate::web::take_cancelled_downloads_for_test();
    let _ = crate::web::take_revealed_downloads_for_test();
    click(vcx, "tab-menu-download-inert");
    vcx.run_until_parked();
    assert!(
        crate::web::take_cancelled_downloads_for_test().is_empty()
            && crate::web::take_revealed_downloads_for_test().is_empty(),
        "a disabled row acted"
    );
    // The control: same menu, same frame, a row that is supposed to work.
    click(vcx, "tab-menu-download-cancel");
    vcx.run_until_parked();
    assert_eq!(
        crate::web::take_cancelled_downloads_for_test(),
        vec![10],
        "the panel was not reachable at all, so the inert row proved nothing"
    );
}

/// A4 — the four edit chords reach Chromium when a page holds the keyboard.
///
/// The seam is the whole point: feature-OFF, `web::copy_selection` is an
/// empty function, so a chord that never fired and one that fired correctly
/// are the same observation. This is what would have caught the gap the
/// operator found by pressing ⌘C on a live page and getting nothing.
#[gpui::test]
fn the_edit_chords_reach_the_page_when_it_holds_the_keyboard(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    crate::web::pretend_available_for_test();
    strip_tabs(
        &h,
        vcx,
        vec![page_tab("p1", Some("https://example.com/"), None)],
        Some("p1"),
    );
    let _ = h.drain();
    let _ = crate::web::take_nav_calls_for_test();

    for (chord, want) in [
        ("cmd-c", "copy"),
        ("cmd-x", "cut"),
        ("cmd-v", "paste"),
        ("cmd-a", "select-all"),
    ] {
        vcx.simulate_keystrokes(chord);
        vcx.run_until_parked();
        assert_eq!(
            crate::web::take_nav_calls_for_test(),
            vec![(want.to_string(), "p1".to_string())],
            "{chord} did not reach Chromium as {want}"
        );
        assert!(
            h.drain().is_empty(),
            "{chord} went through the engine — an edit command is a browser \
             command, not a navigation"
        );
    }
}

/// …and they do NOT fire while a surface of eave's own holds the keyboard.
///
/// This is the half that protects the omnibox: `text_field.rs` binds ⌘A/⌘C/⌘X
/// in its own key context, and if the page path took them first, selecting an
/// address to retype it would instead select the whole document behind it.
#[gpui::test]
fn the_edit_chords_leave_the_omnibox_alone(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    crate::web::pretend_available_for_test();
    strip_tabs(
        &h,
        vcx,
        vec![page_tab("p1", Some("https://example.com/"), None)],
        Some("p1"),
    );
    let _ = h.drain();

    // The positive control FIRST: with no chrome up, ⌘C reaches the page.
    // Without this the assertion below also passes if the chord is simply
    // broken everywhere — which is exactly the vacuity that bit two of the
    // round-two tests.
    let _ = crate::web::take_nav_calls_for_test();
    vcx.simulate_keystrokes("cmd-c");
    vcx.run_until_parked();
    assert_eq!(
        crate::web::take_nav_calls_for_test().len(),
        1,
        "control failed: ⌘C did not reach the page even with no chrome up"
    );

    // Now open the omnibox and try again.
    vcx.simulate_keystrokes("cmd-l");
    vcx.run_until_parked();
    vcx.simulate_keystrokes("cmd-c");
    vcx.run_until_parked();
    assert!(
        crate::web::take_nav_calls_for_test().is_empty(),
        "⌘C reached the page while the omnibox had the keyboard — the \
         address bar's own copy is what that chord means there"
    );
}

/// A4 — an editable field grows Cut / Paste / Select All, and a page does not.
///
/// `editable` came off `ContextMenuParams` when the menu was first built and
/// nothing read it until this slice; these are the rows it was for. Asserted
/// as a COUNT DELTA rather than by row index: the round-two slice shifted
/// every index in this file by adding one row, and six tests failed for a
/// reason that had nothing to do with what they were checking.
#[gpui::test]
fn an_editable_field_gets_the_edit_group_and_a_page_does_not(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let tab = page_shell(&h, vcx, Some("https://example.com/"));

    let menu = |editable: bool| crate::web::PageMenu {
        tab_id: tab.to_string(),
        x: 200.,
        y: 200.,
        page_x: 200,
        page_y: 200,
        link_url: None,
        selection: Some("picked".to_string()),
        editable,
    };

    request_page_menu(&h, vcx, menu(false));
    assert!(shown(vcx, "menu-row-0"), "the page menu never opened");
    let plain = menu_row_count(vcx);

    request_page_menu(&h, vcx, menu(true));
    assert!(shown(vcx, "menu-row-0"), "the editable menu never opened");
    let editable = menu_row_count(vcx);

    assert_eq!(
        editable,
        plain + 3,
        "an editable field should add exactly Cut, Paste and Select All \
         (plain={plain}, editable={editable})"
    );
    assert!(
        plain > 0,
        "control failed: the plain menu had no rows, so the delta above \
         would pass against a menu that renders nothing"
    );
}

/// How many rows the open page menu has. Counts by probing successive
/// `debug_selector`s, because the harness resolves bounds and cannot read
/// text — see the module note.
fn menu_row_count(vcx: &mut VisualTestContext) -> usize {
    const ROWS: [&str; 16] = [
        "menu-row-0", "menu-row-1", "menu-row-2", "menu-row-3",
        "menu-row-4", "menu-row-5", "menu-row-6", "menu-row-7",
        "menu-row-8", "menu-row-9", "menu-row-10", "menu-row-11",
        "menu-row-12", "menu-row-13", "menu-row-14", "menu-row-15",
    ];
    ROWS.iter()
        .take_while(|s| vcx.debug_bounds(*s).is_some())
        .count()
}

// ---------------------------------------------------------------------------
// Slice W1 — ⌘N, a second window

/// **⌘N opens a second shell window, and the two strips do not see each
/// other.**
///
/// Driven through the real chord rather than by calling
/// `open_shell_window`, because the thing worth asserting is the one a
/// wrong binding would break: that ⌘N reaches `NewWindow` at all. The two
/// windows then read ONE engine snapshot carrying both strips and each
/// draws only its own — which is the whole mechanism, and the only defect
/// this slice can produce is a window drawing the other's tabs.
///
/// Every window read goes through `TestAppContext`, never through the
/// `VisualTestContext`: the latter is *inside* window one's update, and a
/// `WindowHandle::read` of the window you are already updating answers
/// `Err`. That cost a debugging round and is worth the sentence.
#[gpui::test]
fn cmd_n_opens_a_second_window_that_draws_its_own_strip(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    fn shells(cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|w| w.downcast::<Shell>())
                .filter_map(|handle| handle.read(cx).ok().map(|s| s.window_key.clone()))
                .collect()
        })
    }
    assert_eq!(
        shells(cx),
        vec![eave_wire::PRIMARY_WINDOW.to_string()],
        "eave booted with something other than one primary window"
    );

    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-n");
    vcx.run_until_parked();

    let open = shells(cx);
    assert_eq!(open.len(), 2, "⌘N did not open a second window: {open:?}");
    let second = open
        .iter()
        .find(|k| k.as_str() != eave_wire::PRIMARY_WINDOW)
        .expect("the second window took the primary's id")
        .clone();

    // A window opened by a chord opens a tab in ITSELF. A `NewTab` naming
    // the primary window here would be the bug that makes ⌘N look like a
    // very slow ⌘T.
    let opened: Vec<_> = h
        .drain()
        .into_iter()
        .filter_map(|c| match c {
            Command::Browser(BrowserCmd::NewTab { window, .. }) => Some(window),
            _ => None,
        })
        .collect();
    assert_eq!(
        opened,
        vec![Some(second.clone())],
        "⌘N asked for a tab in the wrong window"
    );

    // Now answer with one engine snapshot holding both strips, the way
    // the engine really does, and check each window draws only its own.
    h.plumbing.push_tabs(eave_wire::TabsSnapshot {
        tabs: vec![
            page_tab("pppp1111", Some("https://one.test/"), Some("One")),
            eave_wire::TabInfo {
                window: Some(second.clone()),
                ..page_tab("pppp2222", Some("https://two.test/"), Some("Two"))
            },
        ],
        groups: Vec::new(),
        group_tabs: Vec::new(),
        order: vec![eave_wire::Slot::Tab {
            id: "pppp1111".into(),
        }],
        active: Some("pppp1111".into()),
        windows: vec![
            eave_wire::WindowStrip {
                id: eave_wire::PRIMARY_WINDOW.into(),
                order: vec![eave_wire::Slot::Tab {
                    id: "pppp1111".into(),
                }],
                active: Some("pppp1111".into()),
            },
            eave_wire::WindowStrip {
                id: second.clone(),
                order: vec![eave_wire::Slot::Tab {
                    id: "pppp2222".into(),
                }],
                active: Some("pppp2222".into()),
            },
        ],
        read_only: false,
    });
    vcx.run_until_parked();

    fn drawn(cx: &mut TestAppContext, key: &str) -> (Vec<String>, Option<String>) {
        let key = key.to_string();
        cx.update(move |cx| {
            let handle = cx
                .windows()
                .into_iter()
                .filter_map(|w| w.downcast::<Shell>())
                .find(|h| h.read(cx).map(|s| s.window_key == key).unwrap_or(false))
                .expect("no such shell window");
            let shell = handle.read(cx).expect("shell");
            let snap = shell.strip_snapshot(cx);
            (
                snap.tabs.iter().map(|t| t.id.clone()).collect(),
                snap.active.clone(),
            )
        })
    }
    assert_eq!(
        drawn(cx, eave_wire::PRIMARY_WINDOW),
        (vec!["pppp1111".to_string()], Some("pppp1111".to_string())),
        "the primary window drew the second window's tab"
    );
    assert_eq!(
        drawn(cx, &second),
        (vec!["pppp2222".to_string()], Some("pppp2222".to_string())),
        "the second window drew the primary window's tab"
    );
}

/// **Closing a window closes its tabs, and a snapshot in flight across
/// that close does not bring the window back.**
///
/// The race is real and cheap to lose: the close sends a `TabClose` per
/// tab and removes the surface, and any snapshot already queued still
/// names the window. Adopting it would answer a gesture that asked for
/// one fewer window by producing an empty extra one. Driven here by
/// feeding exactly that stale snapshot after the close.
#[gpui::test]
fn closing_a_window_takes_its_tabs_and_a_stale_snapshot_does_not_revive_it(
    cx: &mut TestAppContext,
) {
    let (mut h, vcx) = boot(cx);
    fn shells(cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|w| w.downcast::<Shell>())
                .filter_map(|handle| handle.read(cx).ok().map(|s| s.window_key.clone()))
                .collect()
        })
    }
    vcx.simulate_keystrokes("cmd-n");
    vcx.run_until_parked();
    let second = shells(cx)
        .into_iter()
        .find(|k| k != eave_wire::PRIMARY_WINDOW)
        .expect("⌘N opened no second window");

    let stale = eave_wire::TabsSnapshot {
        tabs: vec![eave_wire::TabInfo {
            window: Some(second.clone()),
            ..page_tab("pppp2222", Some("https://two.test/"), Some("Two"))
        }],
        groups: Vec::new(),
        group_tabs: Vec::new(),
        order: Vec::new(),
        active: None,
        windows: vec![eave_wire::WindowStrip {
            id: second.clone(),
            order: vec![eave_wire::Slot::Tab {
                id: "pppp2222".into(),
            }],
            active: Some("pppp2222".into()),
        }],
        read_only: false,
    };
    h.plumbing.push_tabs(stale.clone());
    vcx.run_until_parked();

    // Close it the way the traffic light does.
    let _ = h.drain();
    cx.update(|cx| {
        let handle = cx
            .windows()
            .into_iter()
            .filter_map(|w| w.downcast::<Shell>())
            .find(|hd| {
                hd.read(cx)
                    .map(|s| s.window_key == second)
                    .unwrap_or(false)
            })
            .expect("no second window");
        // The `remove_window` path — ⌘W's no-tabs fallback and the
        // Window menu's Close row — NOT the traffic light's. It is the
        // one that does not raise `windowShouldClose:`, so it is the one
        // that silently orphaned a strip until both paths were routed
        // through `Shell::release_tabs_on_close`.
        handle
            .update(cx, |shell, window, cx| {
                shell.release_tabs_on_close(cx);
                window.remove_window();
            })
            .expect("close the second window");
    });
    cx.run_until_parked();

    // Its tab was asked to close...
    let closed: Vec<_> = h
        .drain()
        .into_iter()
        .filter_map(|c| match c {
            Command::Browser(BrowserCmd::Close(id)) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(
        closed,
        vec!["pppp2222".to_string()],
        "closing a window left its tabs alive in the engine"
    );

    // ...and the snapshot that was in flight when it closed must not
    // bring it back.
    h.plumbing.push_tabs(stale);
    vcx.run_until_parked();
    assert_eq!(
        shells(cx),
        vec![eave_wire::PRIMARY_WINDOW.to_string()],
        "a stale snapshot reopened a window the operator had closed"
    );
}

// ── Slice AG1 #7 — the follow-up queue ────────────────────────────────────
//
// Before this slice `send_current` wrote every message straight at the
// CLI's stdin whatever the session was doing. Claude's CLI holds one sent
// mid-turn and says nothing (Slice K, capture N), so the words were gone:
// not on screen, not editable, not retrievable. eave holds them now.
//
// Every test here is written against the OBSERVABLE fact — a `Command::Send`
// on the wire, or its absence — and not against the composer's private
// vector, because "the message did not reach the agent" is the thing the
// operator is promised.

/// Put a turn in flight, with the session in the rail so the composer is
/// looking at something. `state` stays `idle`: the turn ledger is what
/// makes this busy, exactly as it is for the Stop control and the working
/// trailer, and a rail row saying `running` would let a test pass over a
/// predicate that had quietly narrowed to the engine's session state.
fn in_flight(h: &Harness, vcx: &mut VisualTestContext) {
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    ask(h, vcx, vec![user_record(1, "go"), delivered_record(2)]);
}

fn draft_now(h: &Harness, vcx: &mut VisualTestContext) -> String {
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

fn queued_texts(h: &Harness, vcx: &mut VisualTestContext) -> Vec<String> {
    vcx.read_window(&h.window, |shell, cx| {
        shell
            .read(cx)
            .composer
            .read(cx)
            .queued
            .iter()
            .map(|q| q.text.clone())
            .collect::<Vec<_>>()
    })
    .unwrap()
}

fn sends(cmds: &[Command]) -> Vec<String> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::Send { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// **The whole slice in one test.** A follow-up typed during a turn does
/// not reach the agent, and it is on screen instead.
#[gpui::test]
fn a_follow_up_written_during_a_turn_is_held_and_shown(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    in_flight(&h, vcx);
    let _ = h.drain();

    vcx.simulate_input("also run the linter");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();

    assert!(
        sends(&h.drain()).is_empty(),
        "the follow-up went at the CLI mid-turn, which is the behaviour this slice replaces"
    );
    assert_eq!(draft_now(&h, vcx), "", "a queued draft is still spent");
    assert_eq!(queued_texts(&h, vcx), vec!["also run the linter"]);
    assert!(
        shown(vcx, "composer-queued-0"),
        "a held message with no row is the old invisible queue with extra steps"
    );
}

/// ...and it goes out on its own when the turn settles, without being
/// retyped.
#[gpui::test]
fn a_held_follow_up_goes_out_when_the_turn_ends(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    in_flight(&h, vcx);
    vcx.simulate_input("also run the linter");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    let _ = h.drain();

    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![turn_completed_record(3)],
    });
    vcx.run_until_parked();

    assert_eq!(
        sends(&h.drain()),
        vec!["also run the linter"],
        "the turn ended and the queue did not drain"
    );
    assert!(
        queued_texts(&h, vcx).is_empty(),
        "a released follow-up is still sitting in the queue and will go twice"
    );
    assert!(!shown(vcx, "composer-queued-0"));
}

/// **One at a time.** The queue exists so follow-ups become separate
/// turns; emptying it into one settle would be the opposite.
#[gpui::test]
fn only_the_head_of_the_queue_goes_out_per_turn(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    in_flight(&h, vcx);
    for text in ["first", "second", "third"] {
        vcx.simulate_input(text);
        vcx.simulate_keystrokes("enter");
    }
    vcx.run_until_parked();
    assert_eq!(queued_texts(&h, vcx), vec!["first", "second", "third"]);
    let _ = h.drain();

    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![turn_completed_record(3)],
    });
    vcx.run_until_parked();
    assert_eq!(
        sends(&h.drain()),
        vec!["first"],
        "the settle released more than the head"
    );
    assert_eq!(queued_texts(&h, vcx), vec!["second", "third"]);

    // The interlock, which is the part an edge-triggered release gets
    // wrong: nothing more goes out until the turn we just asked for has
    // actually opened and closed again. Every notify between now and then
    // re-runs the release rule.
    vcx.run_until_parked();
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    vcx.run_until_parked();
    assert!(
        sends(&h.drain()).is_empty(),
        "the second follow-up went out on top of the first"
    );

    // The released message's own turn arrives and settles; now the next.
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(4, "first"), delivered_record(5)],
    });
    vcx.run_until_parked();
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![turn_completed_record(6)],
    });
    vcx.run_until_parked();
    assert_eq!(sends(&h.drain()), vec!["second"]);
}

/// The case the composer has been describing in words since Slice K: a
/// message written while a permission question is open. The sentence was
/// true and unbacked — now there is a row behind it.
#[gpui::test]
fn a_follow_up_written_under_an_open_question_is_held(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    h.plumbing.push_rail(vec![session("aaaa1111")]);
    ask(
        &h,
        vcx,
        vec![approval_record(1, "req-1", "Write", "/repo/src/main.rs")],
    );
    let _ = h.drain();

    vcx.simulate_input("use the other path");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();

    assert!(
        sends(&h.drain()).is_empty(),
        "the CLI was handed a message it would swallow until the question was answered"
    );
    assert_eq!(queued_texts(&h, vcx), vec!["use the other path"]);
}

/// Return on an empty composer stays the no-op it has always been. A queue
/// that mints an empty row is worse than the send it replaced.
#[gpui::test]
fn return_on_an_empty_composer_queues_nothing(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    in_flight(&h, vcx);
    let _ = h.drain();

    vcx.simulate_keystrokes("enter");
    vcx.simulate_input("   ");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();

    assert!(queued_texts(&h, vcx).is_empty(), "whitespace became a row");
    assert!(sends(&h.drain()).is_empty());
    assert!(!shown(vcx, "composer-queue"));
}

/// The take-back. Once the words were in the pipe there was no way to
/// offer this at all.
#[gpui::test]
fn removing_a_queued_follow_up_takes_the_words_back(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    in_flight(&h, vcx);
    vcx.simulate_input("never mind this");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    let _ = h.drain();

    click(vcx, "composer-queued-0-remove");
    vcx.run_until_parked();
    assert!(queued_texts(&h, vcx).is_empty());

    // ...and the settle it was waiting for sends nothing.
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![turn_completed_record(3)],
    });
    vcx.run_until_parked();
    assert!(
        sends(&h.drain()).is_empty(),
        "a removed follow-up was sent anyway"
    );
}

/// Edit is a **swap**, and the assertion is that nothing is lost in either
/// direction: the queued words reach the field and the words that were in
/// the field take the row's place, keeping its position.
#[gpui::test]
fn editing_a_queued_follow_up_trades_places_with_the_draft(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    in_flight(&h, vcx);
    for text in ["first", "second"] {
        vcx.simulate_input(text);
        vcx.simulate_keystrokes("enter");
    }
    vcx.run_until_parked();
    vcx.simulate_input("still typing");
    let _ = h.drain();

    click(vcx, "composer-queued-1-edit");
    vcx.run_until_parked();
    assert_eq!(
        draft_now(&h, vcx),
        "second",
        "edit did not bring the queued words back to the field"
    );
    assert_eq!(
        queued_texts(&h, vcx),
        vec!["first", "still typing"],
        "the draft was spent, or the row changed position"
    );

    // Exactly reversible: the same click puts everything back.
    click(vcx, "composer-queued-1-edit");
    vcx.run_until_parked();
    assert_eq!(draft_now(&h, vcx), "still typing");
    assert_eq!(queued_texts(&h, vcx), vec!["first", "second"]);
}

/// The override. eave still lets an operator hand a CLI a message mid-turn
/// — what changed is that it is a button somebody pressed rather than what
/// happens to every message by default.
#[gpui::test]
fn send_now_puts_one_through_while_the_turn_is_still_running(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    in_flight(&h, vcx);
    for text in ["first", "second"] {
        vcx.simulate_input(text);
        vcx.simulate_keystrokes("enter");
    }
    vcx.run_until_parked();
    let _ = h.drain();

    click(vcx, "composer-queued-1-now");
    vcx.run_until_parked();
    assert_eq!(
        sends(&h.drain()),
        vec!["second"],
        "send now did not send, or sent the wrong row"
    );
    assert_eq!(queued_texts(&h, vcx), vec!["first"]);
}

/// **A queue belongs to one session**, for the reason a draft does and one
/// more: it is released by a turn ending, and the turn it is waiting for is
/// not the one the session you switched to is running.
#[gpui::test]
fn a_queue_travels_with_the_session_it_was_written_for(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    h.plumbing
        .push_rail(vec![session("aaaa1111"), session("bbbb2222")]);
    ask(&h, vcx, vec![user_record(1, "go"), delivered_record(2)]);
    vcx.simulate_input("for aaaa");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    assert_eq!(queued_texts(&h, vcx), vec!["for aaaa"]);
    let _ = h.drain();

    // Switch to B and settle B's turn. A's follow-up is not B's to send.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "bbbb2222".into(),
    });
    vcx.run_until_parked();
    assert!(
        queued_texts(&h, vcx).is_empty(),
        "B is wearing A's unsent follow-up"
    );
    h.plumbing.push_feed(FeedEvent::Records {
        id: "bbbb2222".into(),
        epoch: 1,
        records: vec![turn_completed_record(3)],
    });
    vcx.run_until_parked();
    assert!(
        sends(&h.drain()).is_empty(),
        "A's follow-up was sent to B, which is the failure `switch_to` exists to prevent"
    );

    // Come back and it is where it was left.
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    vcx.run_until_parked();
    assert_eq!(queued_texts(&h, vcx), vec!["for aaaa"]);
}

/// With nothing in flight the composer is unchanged: Return still sends,
/// immediately, and no row is drawn. The queue must not become a tax on
/// the ordinary case.
#[gpui::test]
fn an_idle_session_still_sends_the_moment_you_press_return(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    let _ = h.drain();

    vcx.simulate_input("run the tests");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();

    assert_eq!(sends(&h.drain()), vec!["run the tests"]);
    assert!(queued_texts(&h, vcx).is_empty());
    assert!(
        !shown(vcx, "composer-queue"),
        "the queue drew itself over a session with nothing queued"
    );
}

/// The guard on the release rule, tested against the window the pump does
/// not currently open.
///
/// `release_queued` reads the turn from the TRANSCRIPT and the queue from
/// the COMPOSER, and those are two entities that move at a session switch.
/// In the running app they move in one pump pass and gpui flushes the
/// observer after both, so the disagreement never lasts a frame — arming
/// the guard away breaks no other test in this file, which is exactly why
/// this one is written by hand rather than left to be discovered.
///
/// What it costs to be wrong is not a repaint: it is A's unsent follow-up
/// delivered to B, the failure `ComposerState::switch_to` was written to
/// prevent, arriving through a door that did not exist when it was written.
#[gpui::test]
fn a_release_is_refused_while_the_transcript_and_the_composer_disagree(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    h.plumbing
        .push_rail(vec![session("aaaa1111"), session("bbbb2222")]);
    ask(&h, vcx, vec![user_record(1, "go"), delivered_record(2)]);
    vcx.simulate_input("for aaaa");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    assert_eq!(queued_texts(&h, vcx), vec!["for aaaa"]);
    let _ = h.drain();

    // Move the transcript to B and leave the composer on A — half a switch,
    // which is the state the two-entity read is exposed to. Nothing is in
    // flight for B, so every other condition for a release is met.
    h.window
        .update(vcx, |shell, _, cx| {
            shell.transcript.update(cx, |t, cx| {
                t.reset();
                t.session_id = Some("bbbb2222".into());
                cx.notify();
            });
        })
        .unwrap();
    vcx.run_until_parked();

    assert!(
        sends(&h.drain()).is_empty(),
        "A's follow-up was released against B's transcript"
    );
    assert_eq!(
        queued_texts(&h, vcx),
        vec!["for aaaa"],
        "the queue was drained by a session it does not belong to"
    );
}

/// The other half of the predicate, and the other observer.
///
/// `send_is_held` is an OR: a turn open in the transcript, or the engine's
/// own session state saying `running`. A session that is busy by the second
/// witness only never touches the transcript when it stops being busy —
/// the fact arrives on the rail — so the release rule has to be reachable
/// from there too, or a follow-up queued against a spawning session waits
/// for an unrelated record to wake it.
#[gpui::test]
fn a_queue_held_by_the_rail_alone_drains_when_the_rail_says_so(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut running = session("aaaa1111");
    running.state = "running".into();
    running.live = true;
    rail_with_selected(&h, vcx, vec![running], "aaaa1111");
    let _ = h.drain();

    vcx.simulate_input("queue me");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    assert_eq!(
        queued_texts(&h, vcx),
        vec!["queue me"],
        "a session the rail calls running took the send anyway"
    );
    assert!(sends(&h.drain()).is_empty());

    // No record moves; only the rail's word for what the session is doing.
    let mut settled = session("aaaa1111");
    settled.state = "done".into();
    h.plumbing.push_rail(vec![settled]);
    vcx.run_until_parked();
    assert_eq!(
        sends(&h.drain()),
        vec!["queue me"],
        "the rail said the session had settled and the queue did not notice"
    );
}

// ── Slice AG2 #10 — scale windowing ───────────────────────────────────────
//
// The rail drew every session it had, forever. Fifteen is a column; sixty
// is a scrollbar with a list behind it, and the rows an operator actually
// works in are at the top of it.
//
// The probe for "which rows did the rail draw" is `Shell::rail_walk`, and
// deliberately so: it is written BY THE PAINT (AG2 #7) from exactly the
// rows that were rendered, so a test reading it cannot be told a different
// story than the keyboard is.

fn many_sessions(n: usize) -> Vec<SessionInfo> {
    (0..n)
        .map(|i| {
            let mut s = session(&format!("s{i:04}"));
            // Distinct roots, or `group_rail` folds them under one header
            // and the collapse logic starts deciding what the window is
            // being asked to decide.
            s.cwd = format!("/tmp/eave-harness/p{i:04}");
            s
        })
        .collect()
}

fn walk_now(h: &Harness, vcx: &mut VisualTestContext) -> Vec<String> {
    vcx.read_window(&h.window, |shell, cx| {
        shell.read(cx).rail_walk.borrow().clone()
    })
    .unwrap()
}

/// Press the rail's reveal control.
///
/// **Not `click`, and the reason is a harness limit rather than a
/// shortcut.** The control sits below thirty rows inside `rail_scroll`;
/// `debug_bounds` reports a position inside the scroll CONTENT and cannot
/// see clipping, so it answers `y = 2777` in a 760 px window (measured),
/// `click` takes that at face value and lands outside the window, and the
/// test goes green having exercised no handler. Scrolling first does not
/// help, because the coordinate `debug_bounds` returns does not move.
///
/// So the button's BEHAVIOUR is driven here by name and its WIRING is
/// pinned separately by
/// `the_reveal_control_is_wired_to_the_method_that_grows_the_window`.
/// Between them the two cover what one unreachable click would have.
fn press_reveal(h: &Harness, vcx: &mut VisualTestContext) {
    assert!(
        shown(vcx, "rail-reveal"),
        "nothing offered to reveal more sessions"
    );
    h.window
        .update(vcx, |shell, _, cx| shell.reveal_more_sessions(cx))
        .unwrap();
    vcx.run_until_parked();
}

/// The rail stops, and says so. A list that simply ended at thirty with no
/// account of the rest would read as a rail that had lost sessions — the
/// failure `archived_line` and the filter note both exist to prevent.
#[gpui::test]
fn a_long_rail_draws_a_window_and_accounts_for_the_rest(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(many_sessions(42));
    vcx.run_until_parked();

    let walk = walk_now(&h, vcx);
    assert_eq!(
        walk.len(),
        30,
        "the rail drew {} rows, not a window",
        walk.len()
    );
    assert_eq!(
        walk[0], "s0000",
        "the window took the wrong end of the list"
    );
    assert!(
        shown(vcx, "rail-reveal"),
        "twelve sessions vanished with nothing on screen to account for them"
    );
}

/// One reveal is a step, not a floodgate — and the count on it is the
/// number that is true.
#[gpui::test]
fn revealing_grows_the_window_by_one_step(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(many_sessions(80));
    vcx.run_until_parked();
    assert_eq!(walk_now(&h, vcx).len(), 30);

    press_reveal(&h, vcx);
    assert_eq!(
        walk_now(&h, vcx).len(),
        55,
        "a reveal did not add exactly one step"
    );
    assert!(
        shown(vcx, "rail-reveal"),
        "twenty-five are still held back and the control left"
    );

    press_reveal(&h, vcx);
    assert_eq!(
        walk_now(&h, vcx).len(),
        80,
        "the last step did not finish the list"
    );
    assert!(
        !shown(vcx, "rail-reveal"),
        "nothing is held back and the rail still offers to show more"
    );
}

/// **The binding rule.** A session parked on a permission question may not
/// be hidden by anything — that is `rail_shows`'s contract for the archive
/// filter, and the window is held to the same one because the harm is the
/// same.
#[gpui::test]
fn a_session_waiting_on_you_is_never_windowed_away(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut infos = many_sessions(42);
    infos[38].pending_approvals = 2;
    infos[38].state = "awaiting_approval".into();
    h.plumbing.push_rail(infos);
    vcx.run_until_parked();

    let walk = walk_now(&h, vcx);
    assert!(
        walk.contains(&"s0038".to_string()),
        "an agent stranded on a question was cut off by the window: {walk:?}"
    );
    // ...and it kept its place rather than being lifted to the top. The
    // rail's order is never re-decided — comet shipped attention bucketing
    // and reverted it with a user report attached.
    assert_eq!(walk[0], "s0000", "the window reordered the list");
    assert_eq!(
        walk.iter().position(|s| s == "s0038"),
        Some(walk.len() - 1),
        "the exempt row did not stay in recency order"
    );
}

/// The selected session is on screen whatever else is true — every other
/// filter in this rail already says so.
#[gpui::test]
fn the_selected_session_is_never_windowed_away(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, many_sessions(42), "s0040");
    let walk = walk_now(&h, vcx);
    assert!(
        walk.contains(&"s0040".to_string()),
        "the rail windowed away the session it is showing: {walk:?}"
    );
}

/// A short list is not a windowed list, and must not grow a control that
/// says otherwise. The composer's `+0 −0` rule: no affordance for an
/// occasion that does not exist.
#[gpui::test]
fn a_rail_that_fits_offers_nothing_to_reveal(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(many_sessions(30));
    vcx.run_until_parked();
    assert_eq!(walk_now(&h, vcx).len(), 30);
    assert!(
        !shown(vcx, "rail-reveal"),
        "a rail showing everything offered to show more"
    );
}

/// The window composes with the keyboard walk for free, and this pins that
/// it is free rather than accidental: `rail_walk` is written by the paint,
/// so a row the window held back is not somewhere the chord can go.
#[gpui::test]
fn the_walk_cannot_reach_a_row_the_window_held_back(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(many_sessions(42));
    vcx.run_until_parked();
    let walk = walk_now(&h, vcx);
    for held in ["s0030", "s0035", "s0041"] {
        assert!(
            !walk.contains(&held.to_string()),
            "{held} is off screen and the chord would still walk onto it"
        );
    }
}

/// A search still reaches every session the operator has: the window runs
/// AFTER the filter, so narrowing to an old row finds it.
#[gpui::test]
fn the_filter_still_reaches_past_the_window(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_rail(many_sessions(42));
    vcx.run_until_parked();
    assert!(!walk_now(&h, vcx).contains(&"s0038".to_string()));

    click(vcx, "rail-filter");
    vcx.run_until_parked();
    vcx.simulate_input("p0038");
    vcx.run_until_parked();
    assert_eq!(
        walk_now(&h, vcx),
        vec!["s0038".to_string()],
        "the filter could not see past the window"
    );
}

/// The reveal control is connected to the thing the tests above drive.
///
/// A structural check, and the pair to `press_reveal`'s doc: that helper
/// exercises the behaviour by name because the button cannot be clicked
/// from this harness, which leaves exactly one gap — a control that draws,
/// says the right number, and does nothing. This closes it.
#[gpui::test]
fn the_reveal_control_is_wired_to_the_method_that_grows_the_window(_cx: &mut TestAppContext) {
    let src = include_str!("ui.rs");
    let at = src
        .find(r#".debug_selector(|| "rail-reveal".into())"#)
        .expect("the rail's reveal control is gone");
    let element = &src[at..];
    let element = &element[..element
        .find(".into_any_element()")
        .expect("the reveal control has no end")];
    assert!(
        element.contains(".on_click("),
        "the reveal control draws and takes no click"
    );
    assert!(
        element.contains("reveal_more_sessions(cx)"),
        "the reveal control is wired to something other than the method the \
         tests drive, so both could be right and the button still dead"
    );
}

// ── Slice AG1 #14 — plan rendering ────────────────────────────────────

/// A `TodoWrite` row opens onto a checklist, and the JSON it was decoded
/// from is GONE rather than shown beside it.
///
/// The replacement is the point. `TodoWrite`'s input *is* the checklist,
/// so before this the one tool row in a turn that says what the agent
/// intends to do next made the operator read
/// `{"content":…,"status":"completed"}` to find out. Showing the card and
/// the blob together would be the duplication the diff row already
/// refuses two plates below.
#[gpui::test]
fn an_opened_plan_is_a_checklist_and_not_its_json(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    let items = vec![
        eave_core::tool::TodoItem {
            text: "read the adapter".into(),
            done: true,
        },
        eave_core::tool::TodoItem {
            text: "fix the parser".into(),
            done: false,
        },
    ];
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![
            tool_started_typed(
                1,
                "t1",
                "TodoWrite",
                r#"{"todos":[{"content":"read the adapter","status":"completed"},{"content":"fix the parser","status":"pending"}]}"#,
                Some(eave_core::tool::ToolCall::Todo { items }),
            ),
            tool_finished(2, "t1", "ok", false),
        ],
    });
    vcx.run_until_parked();

    // A collapsed plan is one row, which is what answers comet's
    // objection to cards: eight `TodoWrite` calls in a turn stay eight
    // lines until you ask for one.
    assert!(!shown(vcx, "tool-plan"), "a collapsed row drew its plan");
    click(vcx, "tool-head");
    vcx.run_until_parked();

    assert!(shown(vcx, "tool-plan"), "an opened plan drew no checklist");
    assert!(
        vcx.debug_bounds("tool-part-t1-input").is_none(),
        "the card is drawn and the JSON it was decoded from is still \
         under it — one fact rendered twice, and the worse one second"
    );
    assert!(
        vcx.debug_bounds("tool-part-t1-result").is_none(),
        "the card replaces the blob on BOTH plates: an ACP plan arrives \
         in the result, and a provider's own rendering of the list under \
         the card is the same duplication seen from the other side"
    );
}

/// The same card, reached the way an ACP agent actually reaches it.
///
/// Hermes announces its `todo` tool as `kind: "other"` with no `rawInput`,
/// so eave cannot type it and the row arrives `Unknown`; the checklist
/// arrives a frame later as ACP's own `plan` update. This walks that whole
/// path — untyped row, then plan — and asserts the card comes out the far
/// end, because the card was never the missing half of #14. **The producer
/// was.**
///
/// It also pins the thing a decoder-only test cannot see: the row is
/// already FINISHED when the plan lands, and the card still draws.
#[gpui::test]
fn an_acp_plan_draws_the_card_on_a_row_eave_could_not_type(cx: &mut TestAppContext) {
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
            tool_started_typed(
                1,
                "tc-caac208d6c1a",
                "other",
                r#"{"title":"todo (3 items)","kind":"other"}"#,
                Some(eave_core::tool::ToolCall::Unknown {
                    name: "other".into(),
                }),
            ),
            tool_finished(2, "tc-caac208d6c1a", "**Todo list**", false),
            plan_update(
                3,
                "tc-caac208d6c1a",
                vec![
                    eave_core::tool::TodoItem {
                        text: "Write the health-check handler".into(),
                        done: true,
                    },
                    eave_core::tool::TodoItem {
                        text: "Wire up the route".into(),
                        done: false,
                    },
                ],
            ),
        ],
    });
    vcx.run_until_parked();

    assert!(!shown(vcx, "tool-plan"), "a collapsed row drew its plan");
    click(vcx, "tool-head");
    vcx.run_until_parked();
    assert!(
        shown(vcx, "tool-plan"),
        "the plan never reached the row it named"
    );
    assert!(
        vcx.debug_bounds("tool-part-tc-caac208d6c1a-input")
            .is_none(),
        "the decoded card and the frame it was decoded from are both drawn"
    );
    // The half a window found and no test had looked at. An ACP plan
    // arrives in the tool's RESULT, and Hermes' own markdown rendering of
    // the same three items was drawing directly under the card.
    assert!(
        vcx.debug_bounds("tool-part-tc-caac208d6c1a-result")
            .is_none(),
        "the card and the provider's own rendering of it are both drawn"
    );
}

/// A plan whose call FAILED keeps its error text under the card.
///
/// Same exception the drawn diff makes, and for the same reason: the card
/// says what the agent intended, and only the error says what actually
/// happened. Suppressing it would leave a checklist standing where a
/// failure ought to be.
#[gpui::test]
fn a_failed_plan_call_still_states_what_went_wrong(cx: &mut TestAppContext) {
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
            tool_started_typed(
                1,
                "tc-fail",
                "other",
                "{}",
                Some(eave_core::tool::ToolCall::Unknown {
                    name: "other".into(),
                }),
            ),
            tool_finished(2, "tc-fail", "todo store is read-only", true),
            plan_update(
                3,
                "tc-fail",
                vec![eave_core::tool::TodoItem {
                    text: "Wire up the route".into(),
                    done: false,
                }],
            ),
        ],
    });
    vcx.run_until_parked();
    click(vcx, "tool-head");
    vcx.run_until_parked();

    assert!(shown(vcx, "tool-plan"), "the plan itself is still a fact");
    assert!(
        vcx.debug_bounds("tool-part-tc-fail-error").is_some(),
        "a checklist stands where the failure ought to be"
    );
}

// ── Slice AG1 #15 — quote into the composer ───────────────────────────

/// Quoting a message puts it in the composer as a blockquote, at the
/// caret, and a quote always starts on its own line.
///
/// The second half is the part with a wrong answer available: adding the
/// leading break unconditionally leaves a blank line at the top of an
/// empty composer, and never adding it welds the quote onto the end of
/// the sentence you were already writing.
#[gpui::test]
fn quoting_a_message_puts_it_in_the_composer_as_a_blockquote(cx: &mut TestAppContext) {
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
        records: vec![assistant_record(1, "first paragraph\n\nlast paragraph")],
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

    // A blank line inside the message is a bare `>`: trailing whitespace
    // on every empty line is what a hand-written quotation never has.
    let quoted = "> first paragraph\n>\n> last paragraph";

    assert!(shown(vcx, "msg-quote-0"), "a message has no quote control");
    click(vcx, "msg-quote-0");
    vcx.run_until_parked();
    assert_eq!(
        draft(vcx, &h),
        format!("{quoted}\n\n"),
        "an empty composer should take the quote with no leading blank line"
    );

    // Now the other branch: text behind the caret that is not a newline.
    vcx.simulate_input("hi");
    assert_eq!(draft(vcx, &h), format!("{quoted}\n\nhi"));
    click(vcx, "msg-quote-0");
    vcx.run_until_parked();
    assert_eq!(
        draft(vcx, &h),
        format!("{quoted}\n\nhi\n{quoted}\n\n"),
        "a quote welded onto the end of the sentence already being written"
    );
}

// ---------------------------------------------------------------------
// Slice AG1 #12 — the jump-to-turn rail
// ---------------------------------------------------------------------

/// Four turns, each with enough answer under it to push the well past a
/// scroll. Long paragraphs rather than many records: the rail's gate asks
/// whether the CONTENT is taller than the viewport, and a short transcript
/// deliberately gets no rail.
fn four_turns(h: &Harness, vcx: &mut VisualTestContext) {
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    let body = (0..14)
        .map(|n| format!("paragraph {n} of an answer long enough to scroll"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut records = Vec::new();
    let mut seq = 1u64;
    for turn in 0..4 {
        records.push(user_record(seq, &format!("prompt number {turn}")));
        seq += 1;
        records.push(assistant_record(seq, &body));
        seq += 1;
        records.push(turn_completed_record(seq));
        seq += 1;
    }
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records,
    });
    vcx.run_until_parked();
}

fn scroll_mode(vcx: &mut VisualTestContext, h: &Harness) -> ScrollMode {
    vcx.read_window(&h.window, |shell, cx| {
        shell.read(cx).transcript.read(cx).scroll
    })
    .unwrap()
}

/// **The whole slice, end to end.** Every turn is marked, and pressing a
/// mark puts that turn's prompt at the top of the well.
///
/// The assertion is on `ScrollMode::Pinned`, not on a pixel offset, and
/// that is deliberate: pinning is what the jump IS. `assert_scroll`
/// re-applies it on every update, which is why a jump taken mid-stream is
/// not scrolled away by the next batch — asserting a one-shot offset would
/// pass over an implementation that had lost exactly that property.
#[gpui::test]
fn the_turn_rail_marks_every_turn_and_a_press_jumps_to_one(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    four_turns(&h, vcx);

    assert!(shown(vcx, "turn-rail"), "four turns and no rail");
    // Four prompts at transcript rows 0, 2, 4, 6 — one tick each, and the
    // selector carries the row the tick will hand to the pin.
    for sel in ["turn-tick-0", "turn-tick-2", "turn-tick-4", "turn-tick-6"] {
        assert!(shown(vcx, sel), "no tick for {sel}");
    }
    assert!(
        !shown(vcx, "turn-tick-8"),
        "a tick appeared for a turn that does not exist"
    );

    click(vcx, "turn-tick-2");
    vcx.run_until_parked();
    assert_eq!(
        scroll_mode(vcx, &h),
        ScrollMode::Pinned { msg: 2 },
        "pressing a tick must pin that turn's prompt to the top of the well"
    );
}

/// waku's gate, and its reason: a rail over one turn is not a map of
/// anything, it is a dot beside the only thing on screen.
#[gpui::test]
fn one_turn_is_not_a_map(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    h.plumbing.push_feed(FeedEvent::Session {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    let body = (0..14)
        .map(|n| format!("paragraph {n} of an answer long enough to scroll"))
        .collect::<Vec<_>>()
        .join("\n\n");
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![
            user_record(1, "the only prompt"),
            assistant_record(2, &body),
            turn_completed_record(3),
        ],
    });
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "turn-rail"),
        "a single turn drew a rail to itself"
    );
}

/// ⌥⌘↑ / ⌥⌘↓, and the clamp.
///
/// **The chords are asserted from the keymap down**, not by calling
/// `walk_turn`: gpui dispatches a matched action ahead of bubble-phase
/// listeners, so a binding that collided with something already bound
/// would still leave the method perfectly correct and unreachable.
#[gpui::test]
fn the_turn_chords_walk_one_turn_each_way_and_clamp(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    four_turns(&h, vcx);

    // Parked at the tail, the newest turn is the one being read — so
    // there is nothing forward of it and the chord is a no-op rather
    // than a wrap back to the oldest.
    vcx.simulate_keystrokes("cmd-alt-down");
    vcx.run_until_parked();
    assert_eq!(
        scroll_mode(vcx, &h),
        ScrollMode::Follow,
        "⌥⌘↓ at the newest turn must clamp, not wrap to the oldest"
    );

    vcx.simulate_keystrokes("cmd-alt-up");
    vcx.run_until_parked();
    assert_eq!(
        scroll_mode(vcx, &h),
        ScrollMode::Pinned { msg: 4 },
        "⌥⌘↑ from the newest turn should land on the one before it"
    );

    vcx.simulate_keystrokes("cmd-alt-up");
    vcx.run_until_parked();
    assert_eq!(scroll_mode(vcx, &h), ScrollMode::Pinned { msg: 2 });

    vcx.simulate_keystrokes("cmd-alt-down");
    vcx.run_until_parked();
    assert_eq!(
        scroll_mode(vcx, &h),
        ScrollMode::Pinned { msg: 4 },
        "the walk must be symmetric"
    );

    // And the other end clamps too.
    vcx.simulate_keystrokes("cmd-alt-up");
    vcx.run_until_parked();
    vcx.simulate_keystrokes("cmd-alt-up");
    vcx.run_until_parked();
    assert_eq!(scroll_mode(vcx, &h), ScrollMode::Pinned { msg: 0 });
    vcx.simulate_keystrokes("cmd-alt-up");
    vcx.run_until_parked();
    assert_eq!(
        scroll_mode(vcx, &h),
        ScrollMode::Pinned { msg: 0 },
        "⌥⌘↑ at the oldest turn must clamp"
    );
}

/// **The walk reads the pin it asked for, not the scroll it got — and
/// this test exists because the suite could not see the bug.**
///
/// `ScrollMode::Pinned` is allowed to be a no-op: a bottom-aligned list
/// cannot put the last turn at the top, because nothing is below it to
/// scroll up past, and `prepaint_items` walks the anchor back. Walked
/// live, four ⌥⌘↑ presses produced four identical frames — the pin did
/// not take, so `is_scrolled_to_end` stayed true, so the newest turn
/// stayed active, so every press asked for the same unreachable turn.
///
/// The harness cannot reproduce the clamp (its list is laid out
/// differently), so this pins the RULE instead: with a pin in force, the
/// walk starts from the pinned turn, whatever the list did about it.
#[gpui::test]
fn the_walk_advances_from_the_pin_even_when_the_list_refused_it(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    four_turns(&h, vcx);

    // Pin a MIDDLE turn by hand and leave the list parked at the end.
    // That divergence is the whole test: `assert_scroll` is deliberately
    // NOT called, so the pin is in force and the list never moved — the
    // exact state a refused pin leaves behind.
    //
    // It has to be a middle turn. The first version of this test pinned
    // the NEWEST one and passed with the fix reverted, because at the
    // tail both rules name the same tick and the test could not tell
    // them apart. It went green having asserted nothing.
    h.window
        .update(vcx, |shell, _, cx| {
            shell
                .transcript
                .update(cx, |t, _| t.scroll = ScrollMode::Pinned { msg: 2 });
        })
        .unwrap();
    vcx.run_until_parked();

    // From turn 2, back one turn is turn 0. Reading the SCROLL instead
    // would see a list still at the end, call the newest turn active,
    // and walk to turn 4 — forward, from a press that means back.
    vcx.simulate_keystrokes("cmd-alt-up");
    vcx.run_until_parked();
    assert_eq!(
        scroll_mode(vcx, &h),
        ScrollMode::Pinned { msg: 0 },
        "the walk started from the list's position instead of the pin"
    );
}

/// The rail must never take layout from the reading column.
///
/// `Shell::column` centres every transcript row inside one shared rhythm,
/// so a rail that consumed width would shift the text of every message in
/// the app. This pins the overlay structurally — the geometry itself is
/// `turn_rail`'s to test, and it does, at every width.
#[gpui::test]
fn the_rail_is_an_overlay_and_never_takes_the_column_s_width(_cx: &mut TestAppContext) {
    let src = include_str!("ui.rs");
    let body = src
        .split("fn turn_rail_el(")
        .nth(1)
        .and_then(|s| s.split("\n    fn ").next())
        .expect("turn_rail_el exists");
    assert!(
        body.contains(".absolute()"),
        "the rail must be positioned out of flow"
    );
    // One mount, upstream of both render paths.
    assert_eq!(
        src.matches("self.turn_rail_el(cx)").count(),
        1,
        "the rail is wrapped onto `transcript_el` once, above the \
         split/docked branch — a second mount is a second thing to forget"
    );
}

/// A `hover_blend` key that no `hover_listener` writes never moves off
/// `rest` — and for these controls `rest` is `wash_rest`, which is
/// invisible against the plate.
///
/// **This is a source test because it is the only kind that can see this.**
/// The bug it guards shipped through the whole suite once: the quote
/// control laid out, hit-tested, took a click and filled the composer, and
/// drew nothing at all. `debug_bounds` reports geometry, never colour, so
/// every behavioural assertion passed on an invisible element. It was
/// found by looking at the window.
///
/// `motion::HoverFade` exists precisely so the blend and the listener
/// cannot be split — *"a blend with no listener never moves"*. These
/// controls predate it and are hand-split, so the pairing is checked here
/// instead. Adding another `hover_blend` to `ui.rs` should fail this test
/// until its key is paired too — and it has, twice now: AG1 #10's rewind
/// and AG1 #11's restore each tripped it on the way in, which is the only
/// evidence that it works.
#[gpui::test]
fn every_hover_blend_key_has_a_listener_that_writes_it(_cx: &mut TestAppContext) {
    let src = include_str!("ui.rs");
    let prod = &src[..src.rfind("#[cfg(test)]").expect("ui.rs has a test module")];
    assert_eq!(
        prod.matches("hover_blend(").count(),
        5,
        "a hover_blend was added or removed — pair its key with a listener \
         and account for it here"
    );
    // The message cluster: one listener per wrapper, one blend per control.
    assert_eq!(
        prod.matches("hover_listener(format!(\"msg-controls-{}\", id.msg))")
            .count(),
        2,
        "both message wrappers must register the cluster's hover key"
    );
    assert_eq!(
        prod.matches("msg-controls-{msg}").count(),
        4,
        "all four message controls must BLEND on the cluster key the wrapper \
         writes — copy, quote, AG1 #10's rewind, and AG1 #11's restore"
    );
    // Each control's own name is its id and debug_selector, and nothing
    // else. Blending on it is the defect: nothing registers it.
    for own in ["msg-copy-{msg}", "msg-quote-{msg}", "msg-rewind-{msg}"] {
        assert_eq!(
            prod.matches(own).count(),
            1,
            "{own} is an identity, not a hover key — blending on it makes an \
             invisible control that still takes clicks"
        );
    }
}

// ── Slice AG1 #10 — rewind to here ────────────────────────────────────

/// A journalled user message together with the provider's echo naming it,
/// which is the only way a row becomes addressable.
fn user_record_named(seq: u64, text: &str, msg_id: &str) -> Vec<eave_core::journal::Record> {
    vec![
        user_record(seq, text),
        eave_core::journal::Record {
            seq: seq + 1,
            ts_ms: (seq + 1) * 10,
            event: eave_core::journal::JournalEvent::Provider {
                event: eave_core::events::SessionEvent::UserEcho {
                    text: text.into(),
                    provider_msg_id: Some(msg_id.into()),
                },
            },
        },
    ]
}

fn rewound_reply(reply: eave_wire::RewindOk) -> FeedEvent {
    FeedEvent::Rewound {
        reply: Box::new(reply),
    }
}

/// The control is drawn for a Claude row the provider has named, and
/// clicking it asks for exactly that message.
#[gpui::test]
fn a_named_message_offers_a_rewind_and_asks_about_itself(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    let mut records = user_record_named(1, "first ask", "u-1");
    records.extend(user_record_named(3, "second ask", "u-2"));
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records,
    });
    vcx.run_until_parked();

    assert!(
        shown(vcx, "msg-rewind-0"),
        "a named message on a provider that can rewind gets the control"
    );
    let _ = h.drain();
    click(vcx, "msg-rewind-0");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        matches!(
            cmds.as_slice(),
            [Command::Rewind { msg_id, last_seen_msg_id, dropped }]
                if msg_id == "u-1"
                    // BOTH user turns go: the target is discarded too.
                    && *dropped == 2
                    // The newest message the client has rendered, which is
                    // the provider's interlock against discarding a turn
                    // the operator never saw.
                    && last_seen_msg_id.as_deref() == Some("u-2")
        ),
        "the click must carry the target, the count and the interlock: {cmds:?}"
    );
}

/// **The gate is the provider, not the row.** Same records, same ids, a
/// provider with no rewind on its wire — and no control at all.
///
/// waku answers this position the same way (`transcript_view.rs`: return
/// `None`). A control that explains itself only after it fails is worse
/// than one that was never offered.
#[gpui::test]
fn a_provider_that_cannot_rewind_is_offered_no_rewind(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut info = session("aaaa1111");
    info.provider = "cursor-agent".into();
    rail_with_selected(&h, vcx, vec![info], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: user_record_named(1, "first ask", "u-1"),
    });
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "msg-rewind-0"),
        "cursor-agent has no conversation rewind; the control must be absent"
    );
    assert!(
        shown(vcx, "msg-copy-0"),
        "the rest of the cluster is unaffected"
    );
}

/// A message the provider never named cannot be addressed, so it gets no
/// control even on Claude — a send whose echo has not arrived yet.
#[gpui::test]
fn an_unnamed_message_offers_no_rewind_even_on_claude(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(1, "in flight")],
    });
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "msg-rewind-0"),
        "no id means no address means no control"
    );
}

/// The point of the whole slice: the discarded message comes back as
/// something to edit and send again.
#[gpui::test]
fn a_rewind_hands_the_message_back_to_the_composer(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: user_record_named(1, "first ask", "u-1"),
    });
    vcx.run_until_parked();

    h.plumbing.push_feed(rewound_reply(eave_wire::RewindOk {
        rewound: true,
        prefill: Some("first ask".into()),
        reason: None,
        unsupported: false,
    }));
    vcx.run_until_parked();
    assert_eq!(composer_draft(vcx, &h), "first ask");
}

/// **A rewind must never clobber a draft.** AG1 #15's rule, and it bites
/// harder here: rewinding is something an operator does mid-thought, so
/// the composer is exactly where their replacement wording already is.
/// `set_content` would take it, and the undo history with it.
#[gpui::test]
fn a_prefill_is_inserted_and_never_replaces_what_was_being_typed(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: user_record_named(1, "first ask", "u-1"),
    });
    vcx.run_until_parked();
    vcx.simulate_input("keep");
    vcx.run_until_parked();
    assert_eq!(
        composer_draft(vcx, &h),
        "keep",
        "the draft never landed, so the assertion below would prove nothing"
    );

    h.plumbing.push_feed(rewound_reply(eave_wire::RewindOk {
        rewound: true,
        prefill: Some("back".into()),
        reason: None,
        unsupported: false,
    }));
    vcx.run_until_parked();
    let draft = composer_draft(vcx, &h);
    assert!(
        draft.contains("keep"),
        "the operator's own words were destroyed: {draft:?}"
    );
    assert!(
        draft.contains("back"),
        "the prefill never arrived: {draft:?}"
    );
}

/// A refusal is the provider's sentence, shown as the provider's — and
/// nothing is dropped from the transcript.
#[gpui::test]
fn a_refused_rewind_states_the_reason_and_keeps_every_row(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: user_record_named(1, "first ask", "u-1"),
    });
    vcx.run_until_parked();

    h.plumbing.push_feed(rewound_reply(eave_wire::RewindOk {
        rewound: false,
        prefill: None,
        reason: Some("turn running".into()),
        unsupported: false,
    }));
    vcx.run_until_parked();
    assert_eq!(
        composer_draft(vcx, &h),
        "",
        "a refused rewind must not seed anything"
    );
    assert!(
        shown(vcx, "msg-rewind-0"),
        "the row is still there and still offers the control"
    );
}

/// Applied ONCE. A reply left in the mailbox would paste the same message
/// into the composer again on every unrelated notify — and the transcript
/// notifies constantly.
#[gpui::test]
fn a_reply_is_applied_once_and_not_on_every_later_notify(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: user_record_named(1, "first ask", "u-1"),
    });
    vcx.run_until_parked();
    h.plumbing.push_feed(rewound_reply(eave_wire::RewindOk {
        rewound: true,
        prefill: Some("once".into()),
        reason: None,
        unsupported: false,
    }));
    vcx.run_until_parked();
    assert_eq!(composer_draft(vcx, &h), "once");
    // Anything at all that makes the transcript notify again. The seq must
    // CONTINUE the log — a gap is a resubscribe, which resets the state
    // this test is about and made an earlier version of it pass against
    // the very bug it was written for.
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![assistant_record(3, "later")],
    });
    vcx.run_until_parked();
    assert!(
        shown(vcx, "msg-rewind-0"),
        "the notify never reached the transcript, so nothing was proven"
    );
    assert_eq!(
        composer_draft(vcx, &h),
        "once",
        "the reply was applied a second time"
    );
}

// ── Slice AG1 #11 — turn checkpoints ──────────────────────────────────

/// A checkpoint record followed by the prompt it belongs to — the
/// adjacency the engine writes under the send lock and the projection
/// reads positionally.
fn checkpointed_user(seq: u64, text: &str) -> Vec<eave_core::journal::Record> {
    vec![
        eave_core::journal::Record {
            seq,
            ts_ms: seq * 10,
            event: eave_core::journal::JournalEvent::Checkpoint {
                tree: format!("tree{seq}"),
                commit: format!("commit{seq}"),
            },
        },
        user_record(seq + 1, text),
    ]
}

fn restored_reply(reply: eave_wire::RestoreOk) -> FeedEvent {
    FeedEvent::Restored {
        reply: Box::new(reply),
    }
}

/// The control appears for a checkpointed turn, and the first click is a
/// QUESTION: `confirmed` false, which writes nothing.
#[gpui::test]
fn a_checkpointed_turn_offers_an_undo_that_asks_first(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: checkpointed_user(1, "do the thing"),
    });
    vcx.run_until_parked();

    assert!(
        shown(vcx, "msg-restore-0"),
        "a turn with a checkpoint gets the undo control"
    );
    let _ = h.drain();
    click(vcx, "msg-restore-0");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        matches!(
            cmds.as_slice(),
            [Command::Restore { seq, confirmed }] if *seq == 1 && !*confirmed
        ),
        "the first click must ASK about the checkpoint's own seq: {cmds:?}"
    );
}

/// **The gate is git, not the provider.** This is the property that
/// separates #11 from #10: a checkpoint is eave's own, so the control
/// appears on a provider with no conversation rewind at all — where
/// `msg-rewind` is absent on the very same row.
#[gpui::test]
fn undo_is_offered_on_a_provider_that_cannot_rewind_a_word(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let mut s = session("aaaa1111");
    s.provider = "cursor-agent".into();
    rail_with_selected(&h, vcx, vec![s], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: checkpointed_user(1, "do the thing"),
    });
    vcx.run_until_parked();

    assert!(
        !shown(vcx, "msg-rewind-0"),
        "cursor-agent has no conversation rewind"
    );
    assert!(
        shown(vcx, "msg-restore-0"),
        "...but its files are still git's, so the undo stands"
    );
}

/// A turn with no checkpoint offers nothing. Covers a non-repo session, a
/// git that refused, and every turn journalled before this slice — which
/// are one fact to the reader and must be one branch here.
#[gpui::test]
fn a_turn_with_no_checkpoint_offers_no_undo(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![user_record(1, "do the thing")],
    });
    vcx.run_until_parked();

    assert!(
        !shown(vcx, "msg-restore-0"),
        "no checkpoint, no control — there is nothing to restore to"
    );
}

/// **The engine's count arms the control, and the second click carries
/// `confirmed`.** The whole two-step, end to end.
#[gpui::test]
fn the_engines_count_arms_the_undo_and_the_next_click_means_it(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: checkpointed_user(1, "do the thing"),
    });
    vcx.run_until_parked();
    let _ = h.drain();

    // The engine answers the question with a count.
    h.plumbing.push_feed(restored_reply(eave_wire::RestoreOk {
        seq: 1,
        restored: false,
        files: vec!["a.rs".into(), "b.rs".into()],
        reason: Some("undo 2 files?".into()),
    }));
    vcx.run_until_parked();

    click(vcx, "msg-restore-0");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        matches!(
            cmds.as_slice(),
            [Command::Restore { seq, confirmed }] if *seq == 1 && *confirmed
        ),
        "armed, the next click must MEAN it: {cmds:?}"
    );

    // And acting disarms: a third click is a question again, never a
    // second silent restore.
    h.plumbing.push_feed(restored_reply(eave_wire::RestoreOk {
        seq: 1,
        restored: true,
        files: vec!["a.rs".into(), "b.rs".into()],
        reason: None,
    }));
    vcx.run_until_parked();
    let _ = h.drain();
    click(vcx, "msg-restore-0");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        matches!(
            cmds.as_slice(),
            [Command::Restore { confirmed, .. }] if !*confirmed
        ),
        "a completed restore must disarm: {cmds:?}"
    );
}

/// A refusal — no count — states the engine's words and leaves the
/// control UNARMED. An armed control after a refusal would mean the next
/// click acted on a question that was never answered.
#[gpui::test]
fn a_refused_restore_states_the_reason_and_arms_nothing(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: checkpointed_user(1, "do the thing"),
    });
    vcx.run_until_parked();
    let _ = h.drain();

    h.plumbing.push_feed(restored_reply(eave_wire::RestoreOk {
        seq: 1,
        restored: false,
        files: Vec::new(),
        reason: Some("this session is not running".into()),
    }));
    vcx.run_until_parked();

    click(vcx, "msg-restore-0");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        matches!(
            cmds.as_slice(),
            [Command::Restore { confirmed, .. }] if !*confirmed
        ),
        "a refusal must not arm anything: {cmds:?}"
    );
}

/// **A checkpoint attaches to the turn it precedes, not to a later one.**
/// Two turns, only the first checkpointed — which is exactly what a
/// session that left its repository, or whose git refused once, produces.
#[gpui::test]
fn a_checkpoint_belongs_to_the_turn_it_preceded(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    h.plumbing.push_feed(FeedEvent::Reset {
        id: "aaaa1111".into(),
    });
    let mut records = checkpointed_user(1, "first ask");
    records.push(user_record(3, "second ask"));
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records,
    });
    vcx.run_until_parked();

    assert!(
        shown(vcx, "msg-restore-0"),
        "the checkpointed turn keeps it"
    );
    assert!(
        !shown(vcx, "msg-restore-1"),
        "an uncheckpointed turn must NOT inherit the previous turn's \
         checkpoint — restoring it would silently undo two turns"
    );
}


// ── Slice ID2b — the containment flip ──────────────────────────────────

/// A session in a stated task under a stated project (Slice ID2b).
fn in_task(id: &str, task: &str, project: &str) -> SessionInfo {
    let mut s = session(id);
    s.cwd = task.into();
    s.checkout_root = Some(task.into());
    s.project_root = Some(project.into());
    s
}

/// **The migration, on screen.** An isolated session's `checkout_root` is
/// its worktree directory, and before this slice the rail filed it under a
/// header titled `<sid8>` beside the `<repo>` header its siblings sat under
/// — one repository drawn as two projects. With `project_root` the
/// worktree is a TASK under its repository: a header one step in, marked
/// with a branch, its rows stepped in with it; the primary task starred
/// and first. Nothing moved in `tabs.jsonl` and nothing moved in the rail
/// except that one relocation, which is a correction.
#[gpui::test]
fn an_isolated_task_is_indented_under_its_repository(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let primary = in_task("aaaa1111", "/tmp/eave-harness/repo", "/tmp/eave-harness/repo");
    let mut wt = in_task(
        "bbbb2222",
        "/tmp/eave-harness/wt/repo/bbbb2222",
        "/tmp/eave-harness/repo",
    );
    wt.worktree = Some(eave_wire::WorktreeInfo {
        base: "/tmp/eave-harness/repo".into(),
        branch: "eave/fix-parser".into(),
        present: true,
        dirty_files: Some(2),
    });
    let other = in_task("cccc3333", "/tmp/eave-harness/zed", "/tmp/eave-harness/zed");
    rail_with_selected(&h, vcx, vec![primary, wt, other], "aaaa1111");

    // Two projects, keyed on the base checkouts — never on the worktree.
    let repo = vcx
        .debug_bounds("rail-group-/tmp/eave-harness/repo")
        .expect("no header for the repository");
    assert!(shown(vcx, "rail-group-/tmp/eave-harness/zed"));
    assert!(
        !shown(vcx, "rail-group-/tmp/eave-harness/wt/repo/bbbb2222"),
        "the worktree drew as a project of its own — the <sid8> header is back"
    );
    // Two tasks under the repository: the primary, starred and first; the
    // worktree, branch-marked, one step in.
    let star = vcx
        .debug_bounds("rail-task-/tmp/eave-harness/repo")
        .expect("no task header for the primary checkout");
    let branch = vcx
        .debug_bounds("rail-task-/tmp/eave-harness/wt/repo/bbbb2222")
        .expect("no task header for the worktree");
    assert!(shown(vcx, "rail-task-/tmp/eave-harness/repo-mark"));
    assert!(
        star.origin.y > repo.origin.y && branch.origin.y > star.origin.y,
        "header, then primary, then worktree — got repo {repo:?} star {star:?} branch {branch:?}"
    );
    let primary_row = vcx.debug_bounds("rail-aaaa1111").expect("the primary's row");
    let wt_row = vcx.debug_bounds("rail-bbbb2222").expect("the worktree's row");
    assert!(
        star.origin.y < primary_row.origin.y && primary_row.origin.y < branch.origin.y,
        "the primary's row is not between its header and the worktree's"
    );
    assert!(branch.origin.y < wt_row.origin.y, "the worktree's row is above its header");
    // Stepped in: the task headers and their rows sit right of the project
    // header's text, by the same step.
    let label = |vcx: &mut VisualTestContext, sel: &'static str| {
        vcx.debug_bounds(sel).expect(sel).origin.x
    };
    let project_x = label(vcx, "rail-group-/tmp/eave-harness/repo-label");
    let task_x = label(vcx, "rail-task-/tmp/eave-harness/repo-label");
    assert!(
        task_x > project_x,
        "the task header is not stepped in from its project ({task_x:?} vs {project_x:?})"
    );
    // A lone task draws no task header: `zed` is one task, and one task
    // IS the project.
    assert!(
        !shown(vcx, "rail-task-/tmp/eave-harness/zed"),
        "a project with one task repeated itself as a task header"
    );
}

/// RS1's binding rule, one level up (Slice ID2b): a collapsed PROJECT must
/// surface a child that is waiting, wherever under it that child sits — in
/// a worktree task, two headers down. Seeded on `pending_approvals` alone
/// with `state` left `running`, the case a state-name-only rollup drops.
#[gpui::test]
fn a_collapsed_project_surfaces_a_child_waiting_in_a_worktree_task(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let primary = in_task("aaaa1111", "/tmp/eave-harness/repo", "/tmp/eave-harness/repo");
    let mut wt = in_task(
        "bbbb2222",
        "/tmp/eave-harness/wt/repo/bbbb2222",
        "/tmp/eave-harness/repo",
    );
    wt.state = "running".into();
    wt.pending_approvals = 1;
    let other = in_task("cccc3333", "/tmp/eave-harness/zed", "/tmp/eave-harness/zed");
    rail_with_selected(&h, vcx, vec![primary, wt, other], "cccc3333");

    click(vcx, "rail-group-/tmp/eave-harness/repo");
    vcx.run_until_parked();
    assert!(!shown(vcx, "rail-bbbb2222"), "the project did not fold");
    assert!(
        !shown(vcx, "rail-task-/tmp/eave-harness/wt/repo/bbbb2222"),
        "a folded project left its task headers standing"
    );
    assert!(
        shown(vcx, "rail-group-/tmp/eave-harness/repo-badge"),
        "a collapsed project swallowed a session parked on a question — \
         the rollup did not reach the worktree task"
    );
    // The task header carries the same word once the project is open.
    click(vcx, "rail-group-/tmp/eave-harness/repo");
    vcx.run_until_parked();
    assert!(shown(vcx, "rail-task-/tmp/eave-harness/wt/repo/bbbb2222-badge"));
    assert!(
        !shown(vcx, "rail-task-/tmp/eave-harness/repo-badge"),
        "the primary task claimed the worktree's question"
    );
}

/// The `changes` row under a task takes the well the way a page does —
/// no transcript, no composer — and gives it back when a row is chosen
/// (Slice ID2b, ID2b′). It is a face of the SELECTION, keyed on the
/// task's checkout root, cleared by choosing a chat or a page, never a
/// tab of its own.
#[gpui::test]
fn the_changes_row_takes_the_well_and_a_row_click_gives_it_back(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = in_task("aaaa1111", "/tmp/eave-harness/repo", "/tmp/eave-harness/repo");
    a.diff_stat = Some(DiffStat { adds: 12, dels: 3 });
    a.last_seq = 3;
    rail_with_selected(&h, vcx, vec![a], "aaaa1111");
    strip_with_project(
        &h,
        vcx,
        Some("/tmp/eave-harness/repo"),
        vec![session_tab("aaaa1111")],
        Vec::new(),
        Some("aaaa1111"),
    );
    let row = "rail-diff-/tmp/eave-harness/repo";
    assert!(shown(vcx, row), "a task with a checkout has no changes row");
    assert!(
        shown(vcx, "rail-diff-/tmp/eave-harness/repo-stat"),
        "the changes row does not state the task's stat"
    );
    let diff = vcx.debug_bounds(row).unwrap();
    let chat = vcx.debug_bounds("rail-aaaa1111").expect("the chat row");
    assert!(diff.origin.y > chat.origin.y, "the changes row is not under the task's chat");
    assert!(shown(vcx, "composer-card"), "the chat frame has no composer");
    assert!(!shown(vcx, "diff-tool"), "the diff is in the well before anyone asked");

    click(vcx, row);
    vcx.run_until_parked();
    assert!(shown(vcx, "diff-tool"), "the changes row did not take the well");
    assert!(
        !shown(vcx, "composer-card"),
        "the composer stayed mounted under the diff"
    );
    assert!(shown(vcx, "pane-crumb"), "the header does not say what the well holds");

    click(vcx, "rail-aaaa1111");
    vcx.run_until_parked();
    assert!(!shown(vcx, "diff-tool"), "choosing the chat did not give the well back");
    assert!(shown(vcx, "composer-card"), "the composer did not come back");
}

/// Sign-off answer 1: under a TASK header a chat is two lines — the task
/// row already states the checkout and the branch, and a third line
/// repeating it is the redundancy AG2 #9's subline swap exists to
/// prevent. A chat under no task keeps its location line.
#[gpui::test]
fn a_chat_under_a_task_is_two_lines_and_a_loose_chat_keeps_its_location(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let primary = in_task("aaaa1111", "/tmp/eave-harness/repo", "/tmp/eave-harness/repo");
    let mut wt = in_task(
        "bbbb2222",
        "/tmp/eave-harness/wt/repo/bbbb2222",
        "/tmp/eave-harness/repo",
    );
    wt.worktree = Some(eave_wire::WorktreeInfo {
        base: "/tmp/eave-harness/repo".into(),
        branch: "eave/fix-parser".into(),
        present: true,
        dirty_files: Some(2),
    });
    let loose = session("cccc3333");
    rail_with_selected(&h, vcx, vec![loose, primary, wt], "aaaa1111");

    assert!(shown(vcx, "rail-task-/tmp/eave-harness/repo"), "no task headers — the fixture is wrong");
    assert!(
        !shown(vcx, "cwd-bbbb2222"),
        "a worktree chat under its task header repeated the branch the header states"
    );
    assert!(
        !shown(vcx, "cwd-aaaa1111"),
        "the primary's chat under its task header repeated the checkout"
    );
    assert!(
        shown(vcx, "cwd-cccc3333"),
        "a chat under no task lost its location line"
    );
    let two = vcx.debug_bounds("rail-bbbb2222").unwrap().size.height;
    let three = vcx.debug_bounds("rail-cccc3333").unwrap().size.height;
    assert!(
        two < three,
        "a two-line chat ({two:?}) is not shorter than a three-line one ({three:?})"
    );
}

/// Sign-off answer 2: the `changes` row is keyed on the task's checkout
/// root, not on whichever chat came first. Two chats share one tree;
/// when the first is gone the row stands, states the stat the remaining
/// chat carries, and opens on that chat's scan.
#[gpui::test]
fn the_changes_row_survives_its_first_chat_leaving(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let root = "/tmp/eave-harness/repo";
    let mut a = in_task("aaaa1111", root, root);
    a.diff_stat = Some(DiffStat { adds: 12, dels: 3 });
    let mut b = in_task("bbbb2222", root, root);
    b.diff_stat = Some(DiffStat { adds: 12, dels: 3 });
    let other = in_task("cccc3333", "/tmp/eave-harness/zed", "/tmp/eave-harness/zed");
    let strip = |h: &Harness, vcx: &mut VisualTestContext, chats: &[&str]| {
        strip_with_project(
            h,
            vcx,
            Some(root),
            chats.iter().map(|id| session_tab(id)).collect(),
            Vec::new(),
            Some("cccc3333"),
        );
    };
    rail_with_selected(&h, vcx, vec![a, b.clone(), other.clone()], "cccc3333");
    strip(&h, vcx, &["aaaa1111", "bbbb2222"]);
    let row = "rail-diff-/tmp/eave-harness/repo";
    assert!(shown(vcx, row), "no changes row for a task with two chats");

    // The first chat leaves. The row stays, the stat stays.
    rail_with_selected(&h, vcx, vec![b, other], "cccc3333");
    strip(&h, vcx, &["bbbb2222"]);
    assert!(shown(vcx, row), "the changes row went dark with its first chat");
    assert!(
        shown(vcx, "rail-diff-/tmp/eave-harness/repo-stat"),
        "the row lost its stat with its first chat"
    );
    // Opening it from another task's chat selects a chat IN this task
    // first — the well shows the transcript's session's scan, and that
    // has to be a session in this tree.
    let _ = h.drain();
    click(vcx, row);
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::Select(id) if id == "bbbb2222")),
        "the changes row opened on another task's scan: {cmds:?}"
    );
}

/// Sign-off answer 3: ctrl-tab and ⌘N walk the whole window in the
/// SIDEBAR's order — PAGES first, then each task's chats and pages down
/// the tree — never one task's tools alone.
#[gpui::test]
fn ctrl_tab_walks_the_spine_pages_first_then_the_tree(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let a = in_task("aaaa1111", "/tmp/eave-harness/repo", "/tmp/eave-harness/repo");
    rail_with_selected(&h, vcx, vec![a], "aaaa1111");
    let strip = |h: &Harness, vcx: &mut VisualTestContext, active: &str| {
        strip_with_project(
            h,
            vcx,
            Some("/tmp/eave-harness/repo"),
            vec![
                session_tab("aaaa1111"),
                page_tab("docked", Some("https://docs.rs/gpui"), Some("gpui - Rust")),
            ],
            vec![page_tab("loose", Some("https://loose.test/"), Some("loose"))],
            Some(active),
        );
    };
    let next = |h: &mut Harness, vcx: &mut VisualTestContext| -> Vec<String> {
        let _ = h.drain();
        vcx.simulate_keystrokes("ctrl-tab");
        vcx.run_until_parked();
        h.drain()
            .into_iter()
            .filter_map(|c| match c {
                Command::Browser(BrowserCmd::Activate(id)) => Some(id),
                _ => None,
            })
            .collect()
    };
    // The engine's own order hoists the group above the loose tab; the
    // sidebar's puts PAGES first. From the loose page, ctrl-tab enters
    // the task at its chat — not the docked page, and not itself.
    strip(&h, vcx, "loose");
    assert_eq!(next(&mut h, vcx), vec!["aaaa1111".to_string()], "from PAGES into the task's chat");
    strip(&h, vcx, "aaaa1111");
    assert_eq!(next(&mut h, vcx), vec!["docked".to_string()], "from the chat to its docked page");
    strip(&h, vcx, "docked");
    assert_eq!(next(&mut h, vcx), vec!["loose".to_string()], "from the last row back to PAGES");
    // ⌘1 is the first row of the spine — the loose page.
    strip(&h, vcx, "docked");
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-1");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "loose"
        )),
        "⌘1 did not land on the spine's first row: {cmds:?}"
    );
}

/// Sign-off answer 5: PAGES with nothing loose is the `+ New Tab` pill
/// alone. Hiding the zone would hide the browser half of eave at rest.
#[gpui::test]
fn pages_with_nothing_loose_is_the_new_tab_pill_alone(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_with_selected(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    strip_tabs(&h, vcx, vec![session_tab("aaaa1111")], Some("aaaa1111"));
    assert!(shown(vcx, "tab-strip"), "PAGES vanished with nothing loose");
    assert!(shown(vcx, "tabs-divider"), "PAGES lost its divider");
    assert!(shown(vcx, "tab-new"), "the New Tab pill is gone — ⌘T has no row");
    assert!(!shown(vcx, "rail-browse"), "ID2b's Browse row is back");
    let pages = vcx.debug_bounds("tab-strip").unwrap();
    let pill = vcx.debug_bounds("tab-new").unwrap();
    assert!(
        pill.origin.y + pill.size.height <= pages.origin.y + pages.size.height,
        "the pill is cut off by its own box: {pill:?} in {pages:?}"
    );
}

/// Slice SP1 — the `+ New Tab` pill is the head of PAGES, above every
/// page row, and stays there however many pages there are: the operator
/// found it "pushed down" one row per tab. ⌘T opens at index 0, so the
/// newest page is the row right under it (the plumbing's `NewTab` arm
/// sends `TabOpen { index: Some(0) }`; the harness sees the command, not
/// the request, and folds no engine — so the placement is the engine's
/// `place` test's to prove and this one pins the pill and the ask).
#[gpui::test]
fn the_new_tab_pill_heads_pages_and_a_new_page_lands_under_it(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
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
    let divider = vcx.debug_bounds("tabs-divider").expect("divider");
    let pill = vcx.debug_bounds("tab-new").expect("pill");
    let first = vcx.debug_bounds("tab-p1").expect("tab-p1");
    let last = vcx.debug_bounds("tab-p3").expect("tab-p3");
    assert!(
        divider.origin.y < pill.origin.y && pill.origin.y < first.origin.y,
        "the pill must sit between the divider and the first page: divider {divider:?} pill {pill:?} first {first:?}"
    );
    assert!(
        last.origin.y > pill.origin.y,
        "three pages pushed the pill below them: pill {pill:?} last {last:?}"
    );
    h.drain();
    click(vcx, "tab-new");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(c, Command::Browser(BrowserCmd::NewTab { .. }))),
        "the pill did not ask for a tab: {cmds:?}"
    );
}

/// ⌘T files a new page into the selected task's group, and loosely —
/// into PAGES — when a loose page is what is active (Slice ID2b, ID2b′).
/// Where a page lands follows the one Frame decision.
#[gpui::test]
fn a_new_page_lands_in_the_selected_task_or_loose(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let a = in_task("aaaa1111", "/tmp/eave-harness/repo", "/tmp/eave-harness/repo");
    rail_with_selected(&h, vcx, vec![a], "aaaa1111");
    strip_with_project(
        &h,
        vcx,
        Some("/tmp/eave-harness/repo"),
        vec![session_tab("aaaa1111")],
        vec![page_tab("loose", Some("https://loose.test/"), Some("loose"))],
        Some("aaaa1111"),
    );
    let _ = h.drain();
    click(vcx, "tab-new");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::NewTab { group: Some(g), .. }) if g == "grp"
        )),
        "⌘T on a task did not file the page into the task's group: {cmds:?}"
    );

    strip_with_project(
        &h,
        vcx,
        Some("/tmp/eave-harness/repo"),
        vec![session_tab("aaaa1111")],
        vec![page_tab("loose", Some("https://loose.test/"), Some("loose"))],
        Some("loose"),
    );
    h.window
        .update(vcx, |shell, _, cx| shell.close_omnibox(cx))
        .unwrap();
    vcx.run_until_parked();
    let _ = h.drain();
    click(vcx, "tab-new");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::NewTab { group: None, .. })
        )),
        "⌘T with a loose page active filed the page into a task: {cmds:?}"
    );
}

/// Selecting a rail row brings the task's chat forward: the row is the
/// task, and the frame swaps to it. Before Slice ID2b a rail click while a
/// page was active switched the transcript's subscription under a page
/// that stayed in the well.
#[gpui::test]
fn selecting_a_rail_row_brings_its_chat_forward(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let a = in_task("aaaa1111", "/tmp/eave-harness/repo", "/tmp/eave-harness/repo");
    rail_with_selected(&h, vcx, vec![a], "aaaa1111");
    strip_with_project(
        &h,
        vcx,
        Some("/tmp/eave-harness/repo"),
        vec![session_tab("aaaa1111")],
        vec![page_tab("loose", Some("https://loose.test/"), Some("loose"))],
        Some("loose"),
    );
    assert!(shown(vcx, "pane-address"), "Browse is not showing its page");
    let _ = h.drain();
    click(vcx, "rail-aaaa1111");
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::Select(id) if id == "aaaa1111")),
        "the row did not select its session: {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::Activate(id)) if id == "aaaa1111"
        )),
        "the row did not bring the task's chat forward — the page would \
         have stayed in the well: {cmds:?}"
    );
}

// ── Slice C2: the popup sheet ────────────────────────────────────────────
//
// The sheet is reachable feature-off for `raise_ask`'s reason: the popup
// registry is the ungated half of `web/popups.rs`, so the surface can be
// driven here without a framework. What CANNOT be driven here is the
// popup's PAGE — `page_element` needs a browser — so these pin the card,
// the header and the two ways out, and the walk is what proves a page
// arrives inside it.

/// File a popup on a page tab and let the shell draw it. Modelled on
/// `raise_ask`, including the second `strip_tabs`: the popup is filed
/// outside gpui's world, so nothing has invalidated the window yet.
#[track_caller]
fn open_popup(h: &Harness, vcx: &mut VisualTestContext, tab: &str, url: &str) {
    let page = || page_tab(tab, Some("https://opener.test/"), Some("The opener"));
    strip_tabs(h, vcx, vec![page()], Some(tab));
    crate::web::popups::note_opened(crate::web::popups::Popup {
        id: format!("popup:{tab}"),
        opener: tab.to_string(),
        url: url.to_string(),
    });
    strip_tabs(h, vcx, vec![page()], Some(tab));
    vcx.run_until_parked();
}

/// The handle-first shape, which is the whole reason this slice exists:
/// `window.open('')` has committed nowhere yet. It draws, it draws no
/// chrome of its own, and it offers neither way out — both of those open
/// its current address, and it has none.
#[gpui::test]
fn a_popup_that_has_been_nowhere_draws_a_page_and_no_chrome(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    open_popup(&h, vcx, "p1", "");
    assert!(shown(vcx, "popup-card"), "the overlay did not render");
    // **No address anywhere on it**, before or after it commits: the
    // overlay is all page. The url becomes visible the moment it is
    // expanded or split, because then it is an ordinary tab with an
    // ordinary pane header over it.
    assert!(
        !shown(vcx, "popup-origin") && !shown(vcx, "popup-header"),
        "the overlay grew chrome of its own"
    );
    assert!(
        !shown(vcx, "popup-expand") && !shown(vcx, "popup-split"),
        "a popup that has committed nowhere offered a promotion to nothing"
    );
    crate::web::close_popup("popup:p1");
}

/// **The overlay takes the well, and that is the point of it.** The card
/// is nearly the whole scrim — the dimmed page is an edge, not a frame —
/// and the gutter beside it is what the scrim's remaining width is spent
/// on. A regression to a dialog-sized card fails here.
#[gpui::test]
fn the_popup_takes_the_well_and_leaves_a_gutter(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    open_popup(&h, vcx, "p1", "https://accounts.test/signin");
    let scrim = vcx.debug_bounds("popup-scrim").expect("no scrim");
    let card = vcx.debug_bounds("popup-card").expect("no card");
    let controls = vcx.debug_bounds("popup-controls").expect("no controls");
    // The overlay takes what is to the RIGHT of where it starts — the
    // dim spans the window, the card spans the well — so the claim is
    // measured from the card's own left edge rather than from the
    // scrim's, which begins behind the rail.
    let room = f32::from(scrim.size.width) - f32::from(card.origin.x - scrim.origin.x);
    assert!(
        f32::from(card.size.width) > room * 0.7,
        "the card {:?} is not taking the {room}px it starts in — this is \
         the dialog-sized popup the operator refused",
        card.size,
    );
    // **The dim reaches the address row**, which is what mounting at the
    // root buys and what mounting in the well could not do.
    if let Some(header) = vcx.debug_bounds("pane-header") {
        assert!(
            scrim.origin.y <= header.origin.y,
            "the scrim starts at {:?}, below the address row at {:?} — it \
             is back inside the well",
            scrim.origin.y,
            header.origin.y
        );
    }
    assert!(
        scrim.size.width > card.size.width && scrim.size.height > card.size.height,
        "the scrim {:?} does not extend past the card {:?} — it cannot be \
         catching the clicks around it",
        scrim.size,
        card.size
    );
    // The controls stand OUTSIDE the card, never over the page.
    assert!(
        controls.origin.x >= card.origin.x + card.size.width,
        "the controls {:?} overlap the card {:?} — a button drawn on a \
         page is a button the page can hide something behind",
        controls.origin,
        card.origin
    );
    crate::web::close_popup("popup:p1");
}

/// A committed popup can be taken out of the overlay, both ways.
#[gpui::test]
fn a_committed_popup_offers_both_ways_out(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    open_popup(&h, vcx, "p1", "http://login.test/");
    assert!(
        shown(vcx, "popup-expand"),
        "a popup with somewhere to go offered no promotion"
    );
    assert!(
        shown(vcx, "popup-split"),
        "a popup with somewhere to go could not be put beside its opener"
    );
    crate::web::close_popup("popup:p1");
}

/// The ✕ takes the sheet down. The page behind it is untouched — a popup
/// is not a navigation.
#[gpui::test]
fn the_close_control_takes_the_sheet_down(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    open_popup(&h, vcx, "p1", "https://accounts.test/signin");
    assert!(shown(vcx, "popup-card"));
    click(vcx, "popup-close");
    vcx.run_until_parked();
    assert!(!shown(vcx, "popup-card"), "the sheet stayed up");
    assert_eq!(crate::web::popups::open_for("p1"), None);
}

/// Escape dismisses it, the way it dismisses every other modal surface in
/// the shell.
#[gpui::test]
fn escape_dismisses_a_popup(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    open_popup(&h, vcx, "p1", "https://accounts.test/signin");
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(!shown(vcx, "popup-card"), "escape did not close the sheet");
}

/// A popup belongs to ONE tab. Switching away must take the sheet with it
/// — a modal drawn over a page it is not about is the floating-surface
/// rule's own failure, and this one would be drawn over somebody else's
/// bank.
#[gpui::test]
fn a_popup_is_not_drawn_over_another_tab(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    open_popup(&h, vcx, "p1", "https://accounts.test/signin");
    assert!(shown(vcx, "popup-card"));
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://opener.test/"), Some("The opener")),
            page_tab("p2", Some("https://elsewhere.test/"), Some("Elsewhere")),
        ],
        Some("p2"),
    );
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "popup-card"),
        "the sheet followed the operator onto another tab"
    );
    crate::web::close_popup("popup:p1");
}

/// The gutter's split control finishes in two halves and the second one
/// lands in the tabs observer, so this drives both: ask, then let the tab
/// the engine minted appear.
///
/// Feature-off there is no engine to mint it, which is the point of
/// splitting the intent from the pairing — the pairing is ours and is
/// asserted here; the ask is one `PageRequest` and `web` owns it.
#[gpui::test]
fn the_split_control_puts_the_popup_beside_the_page_that_opened_it(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    open_popup(&h, vcx, "p1", "https://accounts.test/signin");
    // The ask, as `web::split_popup` files it. (Feature-off it cannot
    // read the snapshot for itself, so the known set is named here — the
    // property under test is the pairing, not where the list came from.)
    crate::web::popups::request_split(
        "p1",
        "https://accounts.test/signin",
        vec!["p1".to_string()],
    );
    crate::web::close_popup("popup:p1");
    // The engine answers with a tab, still on its pending navigation.
    let mut opened = page_tab("p2", None, None);
    opened.pending = Some(eave_wire::PendingNav {
        url: "https://accounts.test/signin".into(),
        seq: 0,
        delta: None,
    });
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://opener.test/"), Some("The opener")),
            opened,
        ],
        Some("p1"),
    );
    vcx.run_until_parked();
    let splits = h
        .window
        .update(vcx, |shell, _, _| shell.splits.clone())
        .unwrap();
    assert_eq!(splits.len(), 1, "the popup did not land in a split: {splits:?}");
    // The opener keeps its side: nothing the operator was reading moves.
    assert_eq!(splits[0].panes, ["p1", "p2"]);
}
// ── Slice CL1 — Columns mode ─────────────────────────────────────────

/// A selector built at runtime, for the `&'static str` the harness
/// helpers take. Leaked on purpose: a test process is short.
fn sel(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Let the strip's glide land (`columns::STRIP_GLIDE_MS`): the clock,
/// then a frame to draw the settled position. `run_until_parked` alone
/// does not help — the glide is a clock, not a task (the `settle` rule).
fn settle_strip(vcx: &mut VisualTestContext) {
    settle(vcx);
    vcx.update(|window, _| window.refresh());
    vcx.run_until_parked();
}

/// The strip's box, for the geometry assertions below.
fn strip_bounds(vcx: &mut VisualTestContext) -> Bounds<Pixels> {
    vcx.debug_bounds("column-strip").expect("column-strip")
}

/// Whether `inner` sits entirely inside `outer` on the x axis — "in view"
/// for a column, since the strip clips horizontally and `debug_bounds`
/// cannot see clipping (the AG2 #10 finding).
fn within_x(inner: Bounds<Pixels>, outer: Bounds<Pixels>) -> bool {
    inner.origin.x >= outer.origin.x
        && inner.origin.x + inner.size.width <= outer.origin.x + outer.size.width + px(0.5)
}

/// Six plain sessions, every one with a tab, the first selected.
fn six_sessions(h: &Harness, vcx: &mut VisualTestContext) -> Vec<String> {
    let ids: Vec<String> = ["aaaa1111", "bbbb2222", "cccc3333", "dddd4444", "eeee5555", "ffff6666"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let infos: Vec<SessionInfo> = ids.iter().map(|id| session(id)).collect();
    rail_and_strip(h, vcx, infos, &ids[0]);
    ids
}

/// Enter the STRIP deliberately. CL1's tests below are about the strip's
/// own arithmetic — its pitch, its scroll, its reorder — and since CV1
/// the minimize glyph opens the PLANE, so they name the mode rather than
/// going through a door that now leads somewhere else.
#[track_caller]
fn enter_columns(h: &Harness, vcx: &mut VisualTestContext) {
    h.window
        .update(vcx, |shell, _, cx| {
            shell.set_layout(crate::columns::LayoutMode::Columns, cx)
        })
        .unwrap();
    vcx.run_until_parked();
}

/// Enter the PLANE deliberately, for the tests below that are about it.
#[track_caller]
fn enter_canvas(h: &Harness, vcx: &mut VisualTestContext) {
    h.window
        .update(vcx, |shell, _, cx| {
            shell.set_layout(crate::columns::LayoutMode::Canvas, cx)
        })
        .unwrap();
    vcx.run_until_parked();
}

/// **Slice CV1 — the door.** The minimize glyph on Focus's header opens
/// the CANVAS, and a card's ⛶ comes back. The glyph is the whole mode
/// control in this direction; there is no bar and no picker.
#[gpui::test]
fn the_minimize_glyph_opens_the_canvas_and_a_cards_maximize_returns(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111"), session("bbbb2222")], "aaaa1111");
    assert!(shown(vcx, "layout-minimize"), "Focus's header carries the minimize glyph");
    assert!(shown(vcx, "content-well"), "Focus is the well as built");
    assert!(!shown(vcx, "column-strip"));

    click(vcx, "layout-minimize");
    vcx.run_until_parked();
    assert!(
        h.window.read_with(vcx, |shell, _| shell.layout.is_canvas()).unwrap(),
        "the glyph opens the plane, not the strip"
    );
    assert!(shown(vcx, "column-strip"), "the plane replaces the well");
    assert!(!shown(vcx, "content-well"));
    assert!(shown(vcx, "column-aaaa1111"), "every open row is a card");
    assert!(shown(vcx, "column-bbbb2222"));
    // The selected chat's column is the well's own chrome: the composer
    // lives in it, and only in it.
    let composer = vcx.debug_bounds("composer-card").expect("composer-card");
    let a = vcx.debug_bounds("column-aaaa1111").expect("column-aaaa1111");
    assert!(
        within_x(composer, a),
        "the composer must sit in the selected column: composer {composer:?}, column {a:?}"
    );
    // No mode control on the plane but a card's ⛶: the way back.
    assert!(!shown(vcx, "layout-minimize"), "the plane draws no minimize");
    click(vcx, "column-max-aaaa1111");
    vcx.run_until_parked();
    assert!(shown(vcx, "content-well"), "Focus is lossless: the well comes back");
    assert!(!shown(vcx, "column-strip"));
}

/// **Every OPEN row gets a column, in sidebar order, collapsed or not.**
/// PAGES first, then each task's chats, its `changes` row and its docked
/// pages; a collapsed task's rows are still open and keep their columns;
/// a closed tab loses its column.
#[gpui::test]
fn every_open_row_is_a_column_in_sidebar_order_even_under_a_collapsed_task(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut a = session("aaaa1111");
    a.checkout_root = Some("/w/eave".into());
    let mut b = session("bbbb2222");
    b.checkout_root = Some("/w/eave".into());
    let mut z = session("cccc3333");
    z.checkout_root = Some("/w/zed".into());
    rail_with_selected(&h, vcx, vec![a, b, z], "aaaa1111");
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            session_tab("aaaa1111"),
            session_tab("bbbb2222"),
            session_tab("cccc3333"),
        ],
        Some("aaaa1111"),
    );
    enter_columns(&h, vcx);

    let x = |vcx: &mut VisualTestContext, sel: &'static str| {
        vcx.debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} has no column"))
            .origin
            .x
    };
    let p1 = x(vcx, "column-p1");
    let ca = x(vcx, "column-aaaa1111");
    let cb = x(vcx, "column-bbbb2222");
    let de = x(vcx, "column-diff-/w/eave");
    let cz = x(vcx, "column-cccc3333");
    let dz = x(vcx, "column-diff-/w/zed");
    assert!(
        p1 < ca && ca < cb && cb < de && de < cz && cz < dz,
        "column order must be the sidebar's: p1={p1:?} a={ca:?} b={cb:?} diff-eave={de:?} z={cz:?} diff-zed={dz:?}"
    );
    // The selected session's task diff draws its scan once its column is
    // mounted (select b, the row beside it); the other task's states why
    // it cannot — the files subscription is the selection's.
    h.plumbing.push_feed(FeedEvent::Session { id: "bbbb2222".into() });
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            session_tab("aaaa1111"),
            session_tab("bbbb2222"),
            session_tab("cccc3333"),
        ],
        Some("bbbb2222"),
    );
    settle_strip(vcx);
    assert!(shown(vcx, "diff-tool"), "the selected task's changes column draws the files face");
    assert!(
        shown(vcx, "column-diff-/w/zed") && !shown(vcx, "column-body-diff-/w/zed")
            || shown(vcx, "column-note-diff-/w/zed"),
        "the other task's changes column is an empty slot or states it cannot scan"
    );

    // Collapse the /w/eave project. Its rows leave the rail and KEEP
    // their columns: open ≠ visible.
    click(vcx, "rail-group-/w/eave");
    vcx.run_until_parked();
    assert!(!shown(vcx, "rail-bbbb2222"), "the collapse must hide the row (precondition)");
    assert!(shown(vcx, "column-bbbb2222"), "a collapsed task's chat is still open and keeps its column");
    assert!(shown(vcx, "column-diff-/w/eave"), "a collapsed task's changes row keeps its column");

    // Close b: its column goes.
    strip_tabs(
        &h,
        vcx,
        vec![
            page_tab("p1", Some("https://a.test/"), Some("A")),
            session_tab("aaaa1111"),
            session_tab("cccc3333"),
        ],
        Some("aaaa1111"),
    );
    assert!(!shown(vcx, "column-bbbb2222"), "a closed tab has no column");
    assert!(shown(vcx, "column-cccc3333"));
}

/// Selecting a row scrolls its column into view — minimally — and an
/// operator scroll selects the column under the centre. One selection,
/// both directions.
#[gpui::test]
fn selection_and_scroll_are_one_fact_in_both_directions(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let ids = six_sessions(&h, vcx);
    enter_columns(&h, vcx);
    let strip = strip_bounds(vcx);
    let last = format!("column-{}", ids[5]);
    let last_col = vcx.debug_bounds(sel(last.clone())).expect("last column");
    assert!(
        !within_x(last_col, strip),
        "precondition: six columns of 560 px do not fit a {:?} px strip",
        strip.size.width
    );
    let _ = h.drain();

    // Select the last row, the way the rail does.
    let tabs: Vec<TabInfo> = ids.iter().map(|id| session_tab(id)).collect();
    h.plumbing.push_feed(FeedEvent::Session { id: ids[5].clone() });
    strip_tabs(&h, vcx, tabs.clone(), Some(&ids[5]));
    // Mid-glide the column is still on its way — and mounted already.
    assert!(
        shown(vcx, sel(format!("column-body-{}", ids[5]))),
        "the destination column is mounted before the glide lands"
    );
    settle_strip(vcx);
    let last_col = vcx.debug_bounds(sel(last.clone())).expect("last column");
    assert!(
        within_x(last_col, strip),
        "selecting the last row must scroll its column into view: {last_col:?} in {strip:?}"
    );
    let scroll_after_select = h
        .window
        .read_with(vcx, |shell, _| shell.columns_scroll.get())
        .unwrap();
    assert!(scroll_after_select > 0.0);
    // Minimal: the first column is now off the left, the last one's right
    // edge meets the strip's.
    let right_gap = (strip.origin.x + strip.size.width) - (last_col.origin.x + last_col.size.width);
    assert!(
        right_gap.abs() < px(1.0),
        "the move must be the least that shows the column whole (gap {right_gap:?})"
    );
    // A repaint does not move it again (the edge), and nothing got
    // selected by the move (no re-derivation from a selection-driven scroll).
    vcx.update(|window, _| window.refresh());
    vcx.run_until_parked();
    let again = h
        .window
        .read_with(vcx, |shell, _| shell.columns_scroll.get())
        .unwrap();
    assert_eq!(again, scroll_after_select, "the paint must not keep scrolling");
    // The one `Select` allowed is the strip reconcile's own ask for the
    // row that was just activated (C5's edge) — never another column's.
    let drained = h.drain();
    assert!(
        !drained
            .iter()
            .any(|c| matches!(c, Command::Select(id) if id != &ids[5])),
        "a selection-driven scroll must not itself select: {drained:?}"
    );

    // The other direction: a horizontal wheel over a chat column's HEAD
    // (never a page surface) walks the strip back to the start, and the
    // column under the centre becomes the selection.
    // Over the column's address row — no page surface there — though
    // the capture-phase wheel takes the gesture over a page too.
    let head = {
        let mut b = vcx
            .debug_bounds(sel(format!("column-{}", ids[5])))
            .expect("column slot");
        b.size.height = px(40.);
        b
    };
    vcx.simulate_event(gpui::ScrollWheelEvent {
        position: head.center(),
        delta: gpui::ScrollDelta::Pixels(point(px(4000.), px(3.))),
        modifiers: Modifiers::none(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    vcx.run_until_parked();
    let scroll = h
        .window
        .read_with(vcx, |shell, _| shell.columns_scroll.get())
        .unwrap();
    assert_eq!(scroll, 0.0, "a horizontal wheel must scroll the strip (clamped at 0)");
    let asked: Vec<String> = h
        .drain()
        .into_iter()
        .filter_map(|c| match c {
            Command::Select(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(
        asked,
        vec![ids[0].clone()],
        "scrolling to the start must select the column under the centre, once"
    );

    // A vertical-dominant wheel with a little drift is a transcript
    // scroll, not a strip scroll.
    vcx.simulate_event(gpui::ScrollWheelEvent {
        position: head.center(),
        delta: gpui::ScrollDelta::Pixels(point(px(-3.), px(-40.))),
        modifiers: Modifiers::none(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    vcx.run_until_parked();
    let scroll = h
        .window
        .read_with(vcx, |shell, _| shell.columns_scroll.get())
        .unwrap();
    assert_eq!(scroll, 0.0, "a vertical-dominant gesture must not walk the strip");
}

/// Only the columns in the viewport and one either side are laid out;
/// the rest are empty slots that keep their width, so every neighbour's
/// position is exact.
#[gpui::test]
fn only_the_viewport_and_its_neighbours_are_mounted(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let ids = six_sessions(&h, vcx);
    enter_columns(&h, vcx);
    let strip = strip_bounds(vcx);
    let view_w = f32::from(strip.size.width);
    let mounted = crate::columns::mounted_range(0.0, view_w, ids.len());
    assert!(
        mounted.end < ids.len(),
        "precondition: a {view_w} px strip must not mount all six columns"
    );
    for (ix, id) in ids.iter().enumerate() {
        let slot = format!("column-{id}");
        let body = format!("column-body-{id}");
        assert!(shown(vcx, sel(slot.clone())), "every column has a slot ({slot})");
        let has_body = shown(vcx, sel(body.clone()));
        assert_eq!(
            has_body,
            mounted.contains(&ix),
            "column {ix} mounted={has_body}, expected {}",
            mounted.contains(&ix)
        );
    }
    let first = vcx.debug_bounds(sel(format!("column-{}", ids[0]))).unwrap();
    let last = vcx.debug_bounds(sel(format!("column-{}", ids[5]))).unwrap();
    assert_eq!(
        f32::from(last.origin.x - first.origin.x),
        crate::columns::column_left(5),
        "an unmounted slot keeps its width"
    );
}

/// A mounted chat column that is not the selection shows ITS session's
/// transcript through a column feed, folded separately from the shell's
/// transcript; the feeds are asked for and released with the layout.
#[gpui::test]
fn a_column_feed_shows_another_sessions_transcript_and_never_the_shells(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111"), session("bbbb2222")], "aaaa1111");
    let _ = h.drain();
    enter_columns(&h, vcx);
    let subs: Vec<String> = h
        .drain()
        .into_iter()
        .filter_map(|c| match c {
            Command::ColumnSubscribe(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(
        subs,
        vec!["aaaa1111".to_string(), "bbbb2222".to_string()],
        "every mounted chat column asks for its feed (the selected one too)"
    );
    assert!(shown(vcx, "column-feed-bbbb2222"), "the unselected chat column draws its feed");
    assert!(
        shown(vcx, "column-note-bbbb2222"),
        "an empty feed says so"
    );
    h.plumbing.push_feed(FeedEvent::ColumnRecords {
        id: "bbbb2222".into(),
        epoch: 1,
        records: vec![user_record(1, "hello from b")],
    });
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "column-note-bbbb2222"),
        "a fed column has rows and drops its note"
    );
    let (shell_rows, feed_rows) = h
        .window
        .read_with(vcx, |shell, cx| {
            let shell_rows = shell.transcript.read(cx).display.len();
            let feed_rows = shell
                .columns
                .read(cx)
                .feed("bbbb2222")
                .map(|f| f.read(cx).display.len())
                .unwrap_or(0);
            (shell_rows, feed_rows)
        })
        .unwrap();
    assert_eq!(shell_rows, 0, "b's frames must never fold into the shell's transcript");
    assert!(feed_rows > 0, "b's frames fold into b's feed");

    // Back to Focus releases every feed.
    click(vcx, "column-max-aaaa1111");
    vcx.run_until_parked();
    let mut released: Vec<String> = h
        .drain()
        .into_iter()
        .filter_map(|c| match c {
            Command::ColumnUnsubscribe(id) => Some(id),
            _ => None,
        })
        .collect();
    released.sort();
    assert_eq!(released, vec!["aaaa1111".to_string(), "bbbb2222".to_string()]);
}

/// ⛶ enters Focus on that row; the toggle returns to Columns at the
/// same scroll. Lossless both ways.
#[gpui::test]
fn maximize_enters_focus_on_the_row_and_the_toggle_returns_to_the_same_scroll(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    let ids = six_sessions(&h, vcx);
    enter_columns(&h, vcx);
    let tabs: Vec<TabInfo> = ids.iter().map(|id| session_tab(id)).collect();
    h.plumbing.push_feed(FeedEvent::Session { id: ids[4].clone() });
    strip_tabs(&h, vcx, tabs, Some(&ids[4]));
    settle_strip(vcx);
    let scroll = h
        .window
        .read_with(vcx, |shell, _| shell.columns_scroll.get())
        .unwrap();
    assert!(scroll > 0.0, "precondition: the fifth column needed a scroll");
    let _ = h.drain();

    click(vcx, sel(format!("column-max-{}", ids[4])));
    vcx.run_until_parked();
    assert!(shown(vcx, "content-well"), "⛶ switches to Focus");
    assert!(
        h.drain().iter().any(|c| matches!(c, Command::Select(id) if id == &ids[4])),
        "⛶ selects that row"
    );

    enter_columns(&h, vcx);
    let back = h
        .window
        .read_with(vcx, |shell, _| shell.columns_scroll.get())
        .unwrap();
    assert_eq!(back, scroll, "returning to Columns lands at the same scroll");
}

/// ⌘⇧] / ⌘⇧[ step one column along the sidebar order and clamp at the ends.
#[gpui::test]
fn the_bracket_chords_step_one_column_and_clamp(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_and_strip(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222"), session("cccc3333")],
        "aaaa1111",
    );
    enter_columns(&h, vcx);
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-shift-]");
    vcx.run_until_parked();
    let asked: Vec<String> = h
        .drain()
        .into_iter()
        .filter_map(|c| match c {
            Command::Select(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(asked, vec!["bbbb2222".to_string()], "⌘⇧] asks for the next column");
    vcx.simulate_keystrokes("cmd-shift-[");
    vcx.run_until_parked();
    assert!(
        !h.drain().iter().any(|c| matches!(c, Command::Select(_))),
        "⌘⇧[ at the first column clamps"
    );
    // And in Focus the same chord walks the same order.
    click(vcx, "column-max-aaaa1111");
    vcx.run_until_parked();
    let _ = h.drain();
    vcx.simulate_keystrokes("cmd-shift-]");
    vcx.run_until_parked();
    assert!(
        h.drain().iter().any(|c| matches!(c, Command::Select(id) if id == "bbbb2222")),
        "the chord means the same thing in Focus"
    );
}

/// Dragging a PAGE row from the sidebar onto the strip reorders it — the
/// engine's `TabMove`, never a split — and a chat dropped there does
/// nothing, because a chat's place is the rail's order.
#[gpui::test]
fn dropping_a_page_on_the_strip_is_a_reorder_not_a_split(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
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
    enter_columns(&h, vcx);
    let _ = h.drain();
    let from = vcx.debug_bounds("tab-p3").expect("tab-p3");
    let c1 = vcx.debug_bounds("column-p1").expect("column-p1");
    let c2 = vcx.debug_bounds("column-p2").expect("column-p2");
    // The gap between the first and second columns.
    let gap = point((c1.origin.x + c1.size.width + c2.origin.x) / 2.0, c1.center().y);
    vcx.simulate_mouse_down(from.center(), MouseButton::Left, Modifiers::none());
    let mut mid = from.center();
    mid.y += px(8.);
    vcx.simulate_mouse_move(mid, Some(MouseButton::Left), Modifiers::none());
    vcx.run_until_parked();
    vcx.simulate_mouse_move(gap, Some(MouseButton::Left), Modifiers::none());
    vcx.simulate_mouse_up(gap, MouseButton::Left, Modifiers::none());
    vcx.run_until_parked();
    let cmds = h.drain();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Browser(BrowserCmd::MoveTab { tab_id, group: None, index: 1 }) if tab_id == "p3"
        )),
        "a page dropped in the first gap moves before p2: {cmds:?}"
    );
    assert!(!shown(vcx, "split-well"), "a strip drop is never a split");
}

// ── Slice CV1 — the canvas ───────────────────────────────────────────
//
// The plane is the product's layout as of 2026-09-07. These pin the four
// things the operator asked for — no window constraints, a grid that
// scrolls forever, free placement, and drag and resize — plus the
// continuity claim that makes the pivot cheap: an untouched plane is
// CL1's strip.
//
// Geometry note: the harness window is narrow, so `canvas::grid_cols` is
// 1 here and the grid runs DOWN. That is the wrap under test, not a
// limitation of it.

/// The card's rect the shell is actually laying out — the placement if
/// the operator has made one, the grid slot if not.
#[track_caller]
fn card_rect(h: &Harness, vcx: &mut VisualTestContext, stem: &str) -> crate::canvas::CardRect {
    h.window
        .read_with(vcx, |shell, _| {
            let rects = shell.canvas_rects.borrow();
            let ix = shell
                .column_walk
                .borrow()
                .iter()
                .position(|c| c.stem() == stem);
            ix.and_then(|i| rects.get(i).copied())
        })
        .unwrap()
        .unwrap_or_else(|| panic!("no card for {stem}"))
}

/// Put the plane at a zoom, the way the View menu does.
#[track_caller]
fn zoom_plane(h: &Harness, vcx: &mut VisualTestContext, to: f32) {
    h.window
        .update(vcx, |shell, _, cx| shell.zoom_canvas_to(to, None, cx))
        .unwrap();
    vcx.run_until_parked();
    let got = h.window.read_with(vcx, |s, _| s.canvas_zoom.get()).unwrap();
    assert!(
        (crate::canvas::clamp_zoom(got) - crate::canvas::clamp_zoom(to)).abs() < 1e-3,
        "asked for {to}, plane is at {got}"
    );
}

/// The point in WINDOW coordinates that lands on the centre of an element
/// inside a scaled card.
///
/// **`debug_bounds` reports an element's own bounds and the harness clicks
/// in window coordinates, and on a zoomed plane those are two spaces.** A
/// press aimed at a raw `debug_bounds` centre lands at `1/scale` of the way
/// there, misses the card entirely, and the test goes green having run no
/// handler — the same shape as AG2 #10's clipping trap, one space over.
#[track_caller]
fn at_drawn(h: &Harness, vcx: &mut VisualTestContext, selector: &str) -> Point<Pixels> {
    let b = vcx
        .debug_bounds(sel(selector.to_string()))
        .unwrap_or_else(|| panic!("no element for {selector}"));
    let s = crate::canvas::card_scale(h.window.read_with(vcx, |sh, _| sh.canvas_zoom.get()).unwrap());
    let c = b.center();
    point(px(f32::from(c.x) * s), px(f32::from(c.y) * s))
}

/// Carry an element from where it is DRAWN by a window-space delta.
#[track_caller]
fn carry_drawn(
    h: &Harness,
    vcx: &mut VisualTestContext,
    grab: &str,
    dx: f32,
    dy: f32,
) {
    let from = at_drawn(h, vcx, grab);
    let to = point(from.x + px(dx), from.y + px(dy));
    vcx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    let mut nudge = from;
    nudge.x += px(8.);
    nudge.y += px(8.);
    vcx.simulate_mouse_move(nudge, Some(MouseButton::Left), Modifiers::none());
    vcx.run_until_parked();
    vcx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
    vcx.run_until_parked();
    vcx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    vcx.run_until_parked();
}

/// Where a card is actually DRAWN, in window points.
///
/// `debug_bounds` reports an element's own bounds, and on the plane those
/// are in the card's coordinates — the scale is applied when the primitive
/// enters the scene, which is downstream of anything a test can read. So
/// the conversion is spelled out here rather than assumed: what lands on
/// screen is the bounds times the plane's scale.
#[track_caller]
fn drawn(
    h: &Harness,
    vcx: &mut VisualTestContext,
    stem: &str,
) -> (f32, f32, f32, f32) {
    let b = vcx
        .debug_bounds(sel(format!("column-{stem}")))
        .unwrap_or_else(|| panic!("no card element for {stem}"));
    let s = crate::canvas::card_scale(h.window.read_with(vcx, |sh, _| sh.canvas_zoom.get()).unwrap());
    (
        f32::from(b.origin.x) * s,
        f32::from(b.origin.y) * s,
        f32::from(b.size.width) * s,
        f32::from(b.size.height) * s,
    )
}

/// What the operator has placed by hand.
#[track_caller]
fn placed(h: &Harness, vcx: &mut VisualTestContext, stem: &str) -> Option<crate::canvas::CardRect> {
    h.window
        .read_with(vcx, |shell, _| shell.canvas_placed.borrow().get(stem).copied())
        .unwrap()
}

/// Carry an element by a grab point to a window position: press, a
/// nudge past the drag threshold, the move, the release. The same shape
/// `dropping_a_page_on_the_strip_is_a_reorder_not_a_split` uses, because
/// gpui starts a drag on the first move and not on the press.
#[track_caller]
fn carry(vcx: &mut VisualTestContext, grab: &'static str, to: Point<Pixels>) {
    let from = vcx.debug_bounds(grab).unwrap_or_else(|| panic!("{grab}")).center();
    vcx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    let mut nudge = from;
    nudge.x += px(8.);
    nudge.y += px(8.);
    vcx.simulate_mouse_move(nudge, Some(MouseButton::Left), Modifiers::none());
    vcx.run_until_parked();
    vcx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
    vcx.run_until_parked();
    vcx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    vcx.run_until_parked();
}

/// **The default is the plane.** A window with nothing saved opens on the
/// canvas — the pivot of 2026-09-07, stated where a reader will find it.
/// The harness fixture pins itself to Focus (see `boot`), so this asks
/// the mode's own default rather than a booted shell's.
#[gpui::test]
fn a_fresh_window_opens_on_the_plane(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    assert!(crate::columns::LayoutMode::default().is_canvas());
    // A frame.json with no entry for this window reads as the default.
    let prefs = crate::columns::layout_from(&serde_json::json!({}), "primary");
    assert!(prefs.mode.is_canvas(), "an unsaved window opens on the plane");
    // And the fixture's own override is what keeps the rest of the file
    // measuring the well — stated, not silent.
    assert!(h.window.read_with(vcx, |s, _| s.layout).unwrap() == crate::columns::LayoutMode::Focus);
}

/// **An untouched plane is the strip.** Every open row gets a card, in
/// the sidebar's order, one column wide — and where the window is too
/// narrow for two across, the grid WRAPS and runs down. That is the
/// "scrollable forever grid": no card count, no end stop.
#[gpui::test]
fn an_untouched_plane_lays_the_cards_out_in_a_grid_that_wraps(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222"), session("cccc3333")],
        "aaaa1111",
    );
    enter_canvas(&h, vcx);
    let a = card_rect(&h, vcx, "aaaa1111");
    let b = card_rect(&h, vcx, "bbbb2222");
    let c = card_rect(&h, vcx, "cccc3333");
    assert_eq!(a.w, crate::canvas::CARD_W, "a card is a column wide");
    assert_eq!(a.w, columns::COLUMN_W, "and that width is CL1's, unchanged");
    // This window fits one across, so the grid runs down and the pitch is
    // exact — nothing overlaps and nothing leaves a hole.
    assert_eq!(crate::canvas::grid_cols(a.w + 2.0 * crate::canvas::GUTTER), 1);
    assert_eq!(b.x, a.x, "the second card wrapped under the first");
    assert_eq!(b.y - a.y, a.h + crate::canvas::GUTTER, "one card and one gutter");
    assert_eq!(c.y - b.y, b.h + crate::canvas::GUTTER);
    // Nothing has been placed by hand, so nothing is saved.
    assert!(placed(&h, vcx, "aaaa1111").is_none());
}

/// **A card dragged by its header stays where it was dropped** — and
/// only that card moves. The landing is snapped to the lattice, which is
/// what keeps hand-placed cards lined up with each other.
#[gpui::test]
fn a_card_carried_by_its_header_stays_where_it_was_dropped(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111"), session("bbbb2222")], "aaaa1111");
    enter_canvas(&h, vcx);
    let before_a = card_rect(&h, vcx, "aaaa1111");
    let before_b = card_rect(&h, vcx, "bbbb2222");
    let grab = vcx.debug_bounds("card-grab-aaaa1111").expect("card-grab-aaaa1111");
    let start = grab.center();

    carry(vcx, "card-grab-aaaa1111", point(start.x + px(120.), start.y + px(60.)));

    let after = placed(&h, vcx, "aaaa1111").expect("the drag wrote a placement");
    assert_eq!(after.w, before_a.w, "a move is a move: the size is untouched");
    assert_eq!(after.h, before_a.h);
    // EXACTLY where it was let go — free form, not a lattice.
    assert_eq!(after.x, before_a.x + 120.0, "{after:?}");
    assert_eq!(after.y, before_a.y + 60.0);
    // The card that was NOT carried keeps its slot — but it closes up,
    // because the grid indexes only the unplaced cards.
    assert!(placed(&h, vcx, "bbbb2222").is_none(), "the neighbour was not placed");
    assert_eq!(
        card_rect(&h, vcx, "bbbb2222").y,
        before_a.y,
        "the grid closed up behind the card that left it"
    );
    assert_ne!(before_b.y, before_a.y, "precondition: b was below a");
}

/// **A press that does not move is still a click.** The threshold is what
/// keeps a card's header clickable, and nothing is written for it.
#[gpui::test]
fn a_press_on_a_card_header_that_does_not_move_places_nothing(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111"), session("bbbb2222")], "aaaa1111");
    enter_canvas(&h, vcx);
    let at = vcx.debug_bounds("card-grab-bbbb2222").expect("card-grab").center();
    vcx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    vcx.run_until_parked();
    assert!(placed(&h, vcx, "bbbb2222").is_none(), "a click is not a placement");
}

/// **The corner resizes and the origin does not move.** A card is
/// anchored at its top-left, which is what makes the gesture aimable; the
/// floor is a floor.
#[gpui::test]
fn a_cards_corner_resizes_it_without_moving_its_origin(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    enter_canvas(&h, vcx);
    let before = card_rect(&h, vcx, "aaaa1111");
    let grip = vcx.debug_bounds("card-grip-corner-aaaa1111").expect("corner grip");
    let from = grip.center();

    carry(vcx, "card-grip-corner-aaaa1111", point(from.x - px(180.), from.y - px(120.)));

    let after = placed(&h, vcx, "aaaa1111").expect("the resize wrote a placement");
    assert_eq!(after.x, before.x, "the origin never moves");
    assert_eq!(after.y, before.y);
    assert!(after.w < before.w, "the corner took the width: {after:?}");
    assert!(after.w >= crate::canvas::CARD_W_MIN, "and stopped at the floor");
    assert!(after.h <= before.h);
    assert!(after.h >= crate::canvas::CARD_H_MIN);
}

/// **`tidy` is the one way back.** Every card into the grid, the pan
/// home, and the placements CLEARED rather than frozen — so the next
/// window width reflows instead of keeping yesterday's columns.
#[gpui::test]
fn tidy_puts_every_card_back_in_the_grid(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111"), session("bbbb2222")], "aaaa1111");
    enter_canvas(&h, vcx);
    let home = card_rect(&h, vcx, "aaaa1111");
    let at = vcx.debug_bounds("card-grab-aaaa1111").expect("grab").center();
    carry(vcx, "card-grab-aaaa1111", point(at.x + px(96.), at.y + px(96.)));
    assert!(placed(&h, vcx, "aaaa1111").is_some(), "precondition: it was placed");

    h.window
        .update(vcx, |shell, _, cx| shell.tidy_canvas(cx))
        .unwrap();
    vcx.run_until_parked();
    assert!(placed(&h, vcx, "aaaa1111").is_none(), "tidy clears, it does not freeze");
    assert_eq!(card_rect(&h, vcx, "aaaa1111"), home, "and the card is back in its slot");
    assert_eq!(
        h.window.read_with(vcx, |s, _| s.canvas_pan.get()).unwrap(),
        (0.0, 0.0),
        "the pan came home too"
    );
}

/// **A placement survives the trip through Focus**, the way CL1's scroll
/// does: ⛶ into the well, the minimize glyph back, and the card is where
/// it was left. Lossless both ways.
#[gpui::test]
fn a_placement_survives_a_round_trip_through_focus(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111"), session("bbbb2222")], "aaaa1111");
    enter_canvas(&h, vcx);
    // The FIRST card, deliberately: this window fits one across, so the
    // second is below the fold, and `debug_bounds` reports content
    // coordinates and cannot see clipping (AG2 #10's documented limit) —
    // a press aimed at it would land outside the window and run no
    // handler, and the test would pass having driven nothing.
    let at = vcx.debug_bounds("card-grab-aaaa1111").expect("grab").center();
    carry(vcx, "card-grab-aaaa1111", point(at.x + px(48.), at.y + px(72.)));
    let left = placed(&h, vcx, "aaaa1111").expect("placed");

    click(vcx, "column-max-aaaa1111");
    vcx.run_until_parked();
    assert!(shown(vcx, "content-well"), "⛶ is the way into the well");
    click(vcx, "layout-minimize");
    vcx.run_until_parked();
    assert_eq!(placed(&h, vcx, "aaaa1111"), Some(left), "the card kept its place");
}

/// **A closed tab takes its placement with it**, so `frame.json` does not
/// collect an entry per tab that ever existed. Pruned in the tabs
/// observer against a NON-EMPTY snapshot — an empty one is the boot
/// shape, `prune_splits`'s rule and its reason.
#[gpui::test]
fn a_closed_cards_placement_is_pruned_but_an_empty_snapshot_prunes_nothing(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111"), session("bbbb2222")], "aaaa1111");
    enter_canvas(&h, vcx);
    // The first card, for `a_placement_survives_a_round_trip_through_focus`'s
    // reason: the second is below the fold here.
    let at = vcx.debug_bounds("card-grab-aaaa1111").expect("grab").center();
    carry(vcx, "card-grab-aaaa1111", point(at.x + px(48.), at.y + px(48.)));
    assert!(placed(&h, vcx, "aaaa1111").is_some());

    // The boot shape first: an empty snapshot must erase nothing.
    strip_tabs(&h, vcx, vec![], None);
    vcx.run_until_parked();
    assert!(
        placed(&h, vcx, "aaaa1111").is_some(),
        "an empty snapshot is the boot shape, not a close"
    );
    // Now a is really gone.
    strip_tabs(&h, vcx, vec![session_tab("bbbb2222")], Some("bbbb2222"));
    vcx.run_until_parked();
    assert!(placed(&h, vcx, "aaaa1111").is_none(), "the closed card's placement went with it");
}

/// **A click on open ground spawns a card THERE.** The ground is the only
/// thing under the pointer that owns no other gesture, so a plain click on
/// it can mean the obvious thing; the engine mints the id, so the
/// placement is written when the tab arrives.
#[gpui::test]
fn a_click_on_open_ground_opens_a_card_where_it_was_clicked(cx: &mut TestAppContext) {
    let (mut h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    enter_canvas(&h, vcx);
    let _ = h.drain();
    // A point on the plane that no card covers: to the right of the only
    // card, which is one column wide at the left gutter.
    let card = vcx.debug_bounds("column-aaaa1111").expect("column-aaaa1111");
    let strip = vcx.debug_bounds("column-strip").expect("column-strip");
    let ground = point(
        card.origin.x + card.size.width + px(40.),
        strip.origin.y + px(200.),
    );
    assert!(
        ground.x < strip.origin.x + strip.size.width,
        "precondition: the ground point is inside the plane"
    );
    vcx.simulate_mouse_down(ground, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_up(ground, MouseButton::Left, Modifiers::none());
    vcx.run_until_parked();
    assert!(
        h.drain().iter().any(|c| matches!(c, Command::Browser(BrowserCmd::NewTab { .. }))),
        "a click on the ground asks the engine for a tab"
    );

    // The engine answers, and the card lands where the click was — not at
    // the end of the grid.
    strip_tabs(
        &h,
        vcx,
        vec![session_tab("aaaa1111"), page_tab("p9", None, None)],
        Some("p9"),
    );
    vcx.run_until_parked();
    let placed = placed(&h, vcx, "p9").expect("the new card was placed at the click");
    assert!(
        placed.x > card.origin.x.into(),
        "it landed to the right of the card that was there: {placed:?}"
    );
}

/// **CV2 — the operator's overlap ruling, end to end.** *"when i zoom out
/// everything bundles together. everything should keep its place. not
/// change around and over lap."*
///
/// The first cut let the cards stop shrinking at a floor while their
/// POSITIONS kept scaling, and at 30% against a 50% floor every card was
/// drawn two thirds larger than the space left for it. There is one scale
/// now, and this is what says so: the gap between two neighbours, measured
/// where it lands on screen, is the gutter scaled by the same number the
/// cards are.
#[gpui::test]
fn cards_keep_their_gutter_at_every_zoom(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111"), session("bbbb2222")], "aaaa1111");
    enter_canvas(&h, vcx);
    for z in [crate::canvas::ZOOM_MIN, 0.6, 0.75, 0.9, crate::canvas::ZOOM_MAX] {
        zoom_plane(&h, vcx, z);
        let s = crate::canvas::card_scale(z);
        let a = drawn(&h, vcx, "aaaa1111");
        let b = drawn(&h, vcx, "bbbb2222");
        // This window fits one across, so b is under a.
        let gap = b.1 - (a.1 + a.3);
        assert!(
            (gap - crate::canvas::GUTTER * s).abs() < 1.0,
            "at zoom {z} the gap is {gap}, wanted {}",
            crate::canvas::GUTTER * s
        );
        assert!(gap > 0.0, "cards overlapped at zoom {z}");
        assert!(
            (a.2 - crate::canvas::CARD_W * s).abs() < 1.0,
            "a card is drawn at the plane's scale: {a:?} at zoom {z}"
        );
    }
}

/// **A card is LAID OUT at its own size however far the plane is zoomed.**
/// That is the whole of CV2: the contents are measured, wrapped and
/// line-broken as they would be at 100%, and only the painting of them is
/// smaller. CV1 scaled the box and left the contents to re-flow inside it,
/// which the operator called corruption.
#[gpui::test]
fn a_cards_layout_does_not_move_when_the_plane_zooms(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    enter_canvas(&h, vcx);
    let at_100 = vcx.debug_bounds("column-aaaa1111").expect("card").size;
    for z in [crate::canvas::ZOOM_MIN, 0.7, 0.9, crate::canvas::ZOOM_MAX] {
        zoom_plane(&h, vcx, z);
        let laid_out = vcx.debug_bounds("column-aaaa1111").expect("card").size;
        assert_eq!(laid_out, at_100, "the card was re-laid-out at zoom {z}");
    }
}

/// **A drag on a zoomed plane follows the pointer.** A card carried N
/// window points moves N points on screen, whatever the zoom — which
/// means the placement moves `N / scale` in canvas points. Get the divide
/// wrong and the card slides out from under the hand.
#[gpui::test]
fn a_card_carried_on_a_zoomed_plane_lands_under_the_pointer(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111"), session("bbbb2222")], "aaaa1111");
    enter_canvas(&h, vcx);
    zoom_plane(&h, vcx, crate::canvas::ZOOM_MIN);
    let s = crate::canvas::card_scale(crate::canvas::ZOOM_MIN);
    let before = card_rect(&h, vcx, "aaaa1111");

    // 120 x 60 WINDOW points, which is what the pointer travels; the
    // placement should move by `120 / s` canvas points, so the card stays
    // under the hand.
    carry_drawn(&h, vcx, "card-grab-aaaa1111", 120.0, 60.0);

    let after = placed(&h, vcx, "aaaa1111").expect("the drag wrote a placement");
    assert!((after.x - (before.x + 120.0 / s)).abs() < 1.0, "{after:?} from {before:?}");
    assert!((after.y - (before.y + 60.0 / s)).abs() < 1.0);
    assert_eq!(after.w, before.w, "a move is a move");
    assert_eq!(after.h, before.h);
}

/// The same for a resize: the corner tracks the pointer at any zoom, and
/// the origin still does not move.
#[gpui::test]
fn a_corner_dragged_on_a_zoomed_plane_tracks_the_pointer(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    enter_canvas(&h, vcx);
    zoom_plane(&h, vcx, crate::canvas::ZOOM_MIN);
    let s = crate::canvas::card_scale(crate::canvas::ZOOM_MIN);
    let before = card_rect(&h, vcx, "aaaa1111");

    carry_drawn(&h, vcx, "card-grip-corner-aaaa1111", -100.0, -60.0);

    let after = placed(&h, vcx, "aaaa1111").expect("the resize wrote a placement");
    assert_eq!(after.x, before.x, "the origin never moves");
    assert_eq!(after.y, before.y);
    assert!((after.w - (before.w - 100.0 / s)).abs() < 1.0, "{after:?} from {before:?}");
    assert!(after.w >= crate::canvas::CARD_W_MIN);
}

/// **A grip stays the same size on screen.** Everything else on a card is
/// content and belongs at the card's scale; a grip is a target, and one
/// scaled to three and a half points at the floor is not one the pointer
/// can find. *"Seamless dragging and resizing"* is the ask this protects.
#[gpui::test]
fn a_resize_grip_is_the_same_width_at_every_zoom(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(&h, vcx, vec![session("aaaa1111")], "aaaa1111");
    enter_canvas(&h, vcx);
    for z in [crate::canvas::ZOOM_MIN, 1.0, crate::canvas::ZOOM_MAX] {
        zoom_plane(&h, vcx, z);
        let s = crate::canvas::card_scale(z);
        let grip = vcx.debug_bounds("card-grip-right-aaaa1111").expect("right grip");
        let on_screen = f32::from(grip.size.width) * s;
        assert!(
            (on_screen - crate::canvas::GRIP_BAND).abs() < 0.5,
            "at zoom {z} the grip draws {on_screen} points wide"
        );
    }
}

/// **Every card is mounted at every zoom.** Nothing empties itself because
/// the plane moved: the 2026-09-07 ruling, *"everything should always be
/// rendered in view at all times."*
#[gpui::test]
fn every_card_stays_drawn_however_far_the_plane_is_panned(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    rail_and_strip(
        &h,
        vcx,
        vec![session("aaaa1111"), session("bbbb2222"), session("cccc3333")],
        "aaaa1111",
    );
    enter_canvas(&h, vcx);
    for stem in ["aaaa1111", "bbbb2222", "cccc3333"] {
        assert!(shown(vcx, sel(format!("column-body-{stem}"))), "{stem} at rest");
    }
    // Pan far away and zoom out: still drawn, all of them.
    h.window
        .update(vcx, |shell, _, cx| {
            shell.canvas_pan.set((4000.0, 4000.0));
            shell.canvas_shown_pan.set((4000.0, 4000.0));
            shell.canvas_zoom.set(crate::canvas::ZOOM_MIN);
            cx.notify();
        })
        .unwrap();
    vcx.run_until_parked();
    for stem in ["aaaa1111", "bbbb2222", "cccc3333"] {
        assert!(shown(vcx, sel(format!("column-body-{stem}"))), "{stem} zoomed out");
    }
}
