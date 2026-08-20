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
use gpui::{Modifiers, TestAppContext, VisualTestContext};

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
        provider: "claude-code".into(),
        model: None,
        cwd: "/tmp/eave-harness".into(),
        created_ts_ms: 1,
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
    let wts = gpui::WindowTextSystem::new(real_text_system());
    let run = TextRun {
        len: text.len(),
        font: gpui::font(family),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    wts.shape_line(SharedString::from(text.to_string()), px(size), &[run], None)
        .width
}

// ---------------------------------------------------------------------------
// Stated facts — REAL text system.
//
// Amendment 1: these tests assert that a fact the operator must read
// actually fits the room it renders in, so they shape it with real
// font-kit metrics. Synthetic glyph advances would make every one of
// them pass vacuously — which is the exact failure this amendment
// exists to prevent.

/// Stage L: ⌘L's destination. It used to be
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
    let (line, over) = resource_line(&all, Some(32 * GB)).expect("two live agents report");
    assert_eq!(line, "2 agents · 0.5 GB of 32.0 GB");
    assert!(!over, "half a gig of thirty-two is not a warning");

    let (one, _) = resource_line(&[live_a.clone()], Some(32 * GB)).unwrap();
    assert_eq!(
        one, "1 agent · 0.2 GB of 32.0 GB",
        "singular reads as singular"
    );

    let mut hog = session("eeee5555");
    hog.live = true;
    hog.rss_bytes = Some(24 * GB);
    let (_, over) = resource_line(&[hog.clone()], Some(32 * GB)).unwrap();
    assert!(over, "three quarters of the machine is the warning case");

    // No denominator is stated as absence, never invented.
    let (bare, over) = resource_line(&all, None).unwrap();
    assert_eq!(bare, "2 agents · 0.5 GB resident");
    assert!(
        !over,
        "without a machine size there is no ratio to warn about"
    );

    // Nothing live that reports memory ⇒ no line at all. A "0.0 GB" over
    // a running agent would be a lie; silence is the honest shape.
    assert!(resource_line(&[dead.clone(), unprobed.clone()], Some(32 * GB)).is_none());
    assert!(resource_line(&[], Some(32 * GB)).is_none());

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
    let (widest, _) = resource_line(&fleet, Some(512 * GB)).unwrap();
    assert_eq!(widest, "30 agents · 120.0 GB of 512.0 GB");
    for line in [widest, "2 agents · 0.5 GB of 32.0 GB".to_string()] {
        let w = real_text_width(&line, ".SystemUIFont", theme::SMALL_SIZE);
        assert!(
            w <= room,
            "the resource fact {line:?} needs {w:?} but the rail gives \
             {room:?} — a ceiling stated half way is not stated"
        );
    }
}

/// The rail's fact lines are full-width, so the room a fact gets is set
/// by the layout, not by the glyphs in it. Proven here (two facts of very
/// different length get identical width) and then relied on: the room is
/// read from the rendered layout, the fact is measured with real metrics.
#[gpui::test]
fn every_worktree_fact_fits_the_rail_at_real_glyph_widths(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut short = session("aaaa1111");
    short.worktree = Some(worktree("eave/aaaa1111", Some(2), true));
    let mut long = session("bbbb2222");
    long.cwd = "/Users/somebody/very/deeply/nested/checkout/of/a/project".into();
    long.worktree = Some(worktree("eave/bbbb2222", None, true));
    h.plumbing.push_rail(vec![short, long]);
    vcx.run_until_parked();

    let room = vcx.debug_bounds("wt-line-aaaa1111").unwrap().size.width;
    assert_eq!(
        room,
        vcx.debug_bounds("wt-line-bbbb2222").unwrap().size.width,
        "fact lines must be full-width: the room a fact gets cannot depend \
         on the fact's own length, or this measurement means nothing"
    );

    // Every shape `worktree_line` can produce, plus the cwd line and every
    // badge word — all of them must survive rail width.
    let shapes = [
        worktree_line(&worktree("eave/aaaa1111", Some(0), true)),
        worktree_line(&worktree("eave/aaaa1111", Some(2), true)),
        worktree_line(&worktree("eave/aaaa1111", Some(999_999), true)),
        worktree_line(&worktree("eave/aaaa1111", None, true)),
        worktree_line(&worktree("eave/aaaa1111", Some(2), false)),
    ];
    for line in shapes {
        let w = real_text_width(&line, theme::MONO, theme::MICRO_SIZE);
        assert!(
            w <= room,
            "worktree fact {line:?} needs {w:?} but the rail gives {room:?} \
             — the dirty count is the fact decision 2 protects; it must \
             never lose to truncation"
        );
    }
    for state in ["quiet", "unresponsive", "failed"] {
        let word = state_badge(state).unwrap();
        let w = real_text_width(word, theme::SANS, theme::SMALL_SIZE);
        assert!(w <= room, "badge {word:?} needs {w:?}, room is {room:?}");
    }
}

/// The two facts whose length is unbounded — the sharer list and the
/// engine's stated note — must WRAP, never clip. A clipped sharer list
/// names the wrong set of sessions; a clipped confirmation hides "delete
/// again to mean it", which is the whole explanation of an armed button.
///
/// Real metrics prove the wrap is required (three sharers already
/// overflow); the rendered layout proves the element actually grows
/// instead of holding one line and swallowing the tail.
#[gpui::test]
fn facts_that_grow_wrap_instead_of_losing_their_tail(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let mut one = session("aaaa1111");
    one.shared_with = vec!["bbbb2222".into()];
    one.worktree = Some(worktree("eave/aaaa1111", Some(1), true));
    h.plumbing.push_rail(vec![one]);
    h.plumbing
        .push_wt(("aaaa1111".into(), Some(WtNote::Error("refused".into()))));
    vcx.run_until_parked();
    let room = vcx.debug_bounds("shares-aaaa1111").unwrap().size.width;
    let short_shares = vcx.debug_bounds("shares-aaaa1111").unwrap().size.height;
    let short_note = vcx.debug_bounds("wt-note-aaaa1111").unwrap().size.height;

    // Real metrics: these strings genuinely do not fit on one line.
    let three = shares_line(&[
        "bbbb2222".to_string(),
        "cccc3333".to_string(),
        "dddd4444".to_string(),
    ]);
    assert!(
        real_text_width(&three, theme::SANS, theme::SMALL_SIZE) > room,
        "three sharers should overflow one line — if this ever stops being \
         true the wrap below is untested, not unnecessary"
    );
    let confirm = "2 uncommitted files, 0 unmerged commits \u{2014} delete again to mean it";
    assert!(
        real_text_width(confirm, theme::SANS, theme::SMALL_SIZE) > room,
        "the armed confirmation does not fit on one line"
    );

    // Rendered layout: the elements grow to hold the overflow.
    let mut many = session("aaaa1111");
    many.shared_with = (0..6).map(|i| format!("share{i}00000")).collect();
    many.worktree = Some(worktree("eave/aaaa1111", Some(1), true));
    h.plumbing.push_rail(vec![many]);
    h.plumbing.push_wt((
        "aaaa1111".into(),
        Some(WtNote::ConfirmDelete {
            dirty_files: Some(2),
            unmerged_commits: Some(0),
        }),
    ));
    vcx.run_until_parked();
    assert!(
        vcx.debug_bounds("shares-aaaa1111").unwrap().size.height > short_shares,
        "the sharer list clipped instead of wrapping — the operator would \
         read a truncated set of session ids as the whole set"
    );
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
    click(vcx, "wtd-aaaa1111");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::RemoveWorktree { id, force: false }] if id == "aaaa1111"),
        "the first click must not force, got {cmds:?}"
    );
    assert!(!shown(vcx, "wt-note-aaaa1111"), "nothing to state yet");

    // The engine refuses with facts; the row states them and arms.
    h.plumbing.push_wt((
        "aaaa1111".into(),
        Some(WtNote::ConfirmDelete {
            dirty_files: Some(2),
            unmerged_commits: Some(0),
        }),
    ));
    vcx.run_until_parked();
    assert!(shown(vcx, "wt-note-aaaa1111"), "the refusal must be stated");

    // A second refusal with different counts replaces the note — the
    // operator always reads what is true now, not what was true then.
    h.plumbing.push_wt((
        "aaaa1111".into(),
        Some(WtNote::ConfirmDelete {
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
                .wt_notes
                .get("aaaa1111")
                .cloned()
        })
        .unwrap();
    assert!(
        matches!(
            stated,
            Some(WtNote::ConfirmDelete {
                dirty_files: Some(5),
                unmerged_commits: Some(1)
            })
        ),
        "the note must carry the freshest counts, got {stated:?}"
    );

    // Now the click means it.
    let _ = h.drain();
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
    h.plumbing.push_wt((
        "aaaa1111".into(),
        Some(WtNote::ConfirmDelete {
            dirty_files: Some(2),
            unmerged_commits: Some(0),
        }),
    ));
    vcx.run_until_parked();

    h.plumbing.push_wt(("aaaa1111".into(), None)); // settled
    vcx.run_until_parked();
    assert!(
        !shown(vcx, "wt-note-aaaa1111"),
        "a settled action states nothing"
    );

    let _ = h.drain();
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

fn tool_started(seq: u64, id: &str, name: &str, input: &str) -> eave_core::journal::Record {
    eave_core::journal::Record {
        seq,
        ts_ms: seq * 10,
        event: eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::ToolUseStarted {
                id: id.into(),
                name: name.into(),
                input: input.into(),
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
    assert!(
        vcx.debug_bounds("tool-detail").is_some(),
        "a running tool row states no command"
    );

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
    assert!(
        vcx.debug_bounds("tool-detail").is_some(),
        "the command stopped being stated once the tool finished"
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
    assert_eq!(
        x(vcx, "rail-title"),
        x(vcx, "rail-resource"),
        "the sessions rail's header and footer start on different edges"
    );
    // The header's trailing control and a row's card share their RIGHT
    // edge — this is the 4 px overhang, asserted.
    assert_eq!(
        right(vcx, "new-session"),
        right(vcx, "rail-aaaa1111"),
        "the row card and the + new button end on different edges"
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
        x(vcx, "rail-title"),
        "the empty state is indented past the header it sits under"
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
            shell.read(cx).composer.read(cx).text.clone()
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
        matches!(cmds.as_slice(), [Command::Send(t)] if t == "run the tests"),
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
            shell.read(cx).composer.read(cx).text.clone()
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
        matches!(cmds.as_slice(), [Command::Send(t)] if t == "for aaaa"),
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
            shell.read(cx).composer.read(cx).text.clone()
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
            shell.read(cx).composer.read(cx).text.clone()
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
    h.plumbing.push_rail(vec![asks, never]);
    vcx.run_until_parked();

    // Same full-width room for both, so the room is layout-derived and
    // text-independent (the composition this measurement relies on).
    let room = vcx.debug_bounds("posture-aaaa1111").unwrap().size.width;
    assert_eq!(
        room,
        vcx.debug_bounds("posture-bbbb2222").unwrap().size.width,
        "posture lines must be full-width like every other rail fact"
    );

    for mode in [
        "default",
        "manual",
        "bypassPermissions",
        "acceptEdits",
        "dontAsk",
    ] {
        let text = posture_line(mode);
        let w = real_text_width(&text, theme::SANS, theme::SMALL_SIZE);
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
        "the composer always states its own key"
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
    let cursors = src.matches(".cursor_pointer()").count();
    assert_eq!(
        clicks, cursors,
        "every .on_click site needs a .cursor_pointer — eave had 84 fewer \
         cursor calls than comet and every one of them was the default arrow"
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
        ("to-bottom", "chrome_glass_hover"),
    ] {
        let anchor = format!(r#"debug_selector(|| "{selector}".into())"#);
        let at = src
            .find(&anchor)
            .unwrap_or_else(|| panic!("{selector} is no longer tagged"));
        // Wide enough to clear the rationale comments that sit between a
        // selector and its wash; a fade that genuinely moved elsewhere in
        // the tree would take its selector with it.
        let window = &src[at..src.len().min(at + 1600)];
        assert!(
            window.contains(".hover_fade("),
            "{selector} lost its hover feedback (or went back to a snapping .hover)"
        );
        assert!(
            window.contains(wash),
            "{selector} should hover to {wash} — a filled control replaces \
             its bg rather than compositing the neutral wash over it"
        );
    }
    // The five built from a formatted selector, checked at their builder.
    for (builder, what) in [
        (r#"format!("rail-{}", info.session_id)"#, "the session row"),
        (r#"format!("iso-{}", info.session_id)"#, "isolate"),
        (r#"format!("wto-{}", info.session_id)"#, "worktree open"),
        (r#"format!("wtd-{}", info.session_id)"#, "worktree delete"),
        (r#"format!("file-{}", f.path)"#, "the file row"),
    ] {
        let at = src
            .find(&format!("debug_selector(|| {builder})"))
            .unwrap_or_else(|| panic!("{what} is no longer tagged"));
        let window = &src[at..src.len().min(at + 1400)];
        assert!(
            window.contains(".hover_fade("),
            "{what} lost its hover feedback"
        );
    }
    // approve / deny share one builder and one selector expression.
    let at = src.find("fn decision_button(").expect("decision_button");
    let window = &src[at..src.len().min(at + 2400)];
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

/// The caret's drive, which is the one tween in the slice that never
/// settles — so it is the one that could quietly cost forever.
///
/// **An empty composer takes no lease.** That is a deliberate gate, not an
/// oversight: eave's caret is on screen always, and a caret that blinked
/// always would mean a window redrawing 30 times a second for the life of
/// the process, which is precisely the invariant N1 was built to hold — *a
/// window with nothing mid-flight schedules nothing at all*. See
/// `Shell::caret_lit`.
#[gpui::test]
fn only_a_draft_puts_the_caret_on_the_clock(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
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
        }),
    ));
    vcx.run_until_parked();
    // Past every entrance: a session that just mounted is animating. The
    // assertion is about a SETTLED shell, which is what the invariant is
    // about — a window with nothing mid-flight schedules nothing at all.
    vcx.executor().advance_clock(Duration::from_secs(1));
    vcx.run_until_parked();
    drive(&h, vcx, Duration::from_millis(10));
    assert!(
        !vcx.update(|_, cx| motion::clock_running(cx)),
        "an idle shell with an empty composer had the pulse clock running"
    );

    vcx.simulate_keystrokes("h i");
    vcx.run_until_parked();
    assert!(
        vcx.update(|_, cx| motion::clock_running(cx)),
        "a draft did not put the caret on the clock"
    );

    // Sending empties the composer; the lease lapses and the clock parks.
    // PULSE_LEASE is 300 ms, so a second of clock is comfortably past it.
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    vcx.executor().advance_clock(Duration::from_secs(1));
    vcx.run_until_parked();
    assert!(
        !vcx.update(|_, cx| motion::clock_running(cx)),
        "the clock kept ticking after the draft was sent"
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

/// **The structural assertion the trailer exists to survive.** It sits
/// near the tail of a bottom-anchored virtualized list, which is exactly
/// where a new row would shove every other row up by its own height. It
/// is not a row: it lives in the floating chrome stack, a sibling of the
/// list. So the trailer appearing and disappearing must move nothing.
///
/// The instrument is an approval card's button, because it is the one
/// thing rendered INSIDE a list row that carries a selector — its bounds
/// are a direct readout of the list's layout.
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
        ],
    );
    assert!(shown(vcx, "work-trailer"), "the turn is open");
    let before = vcx
        .debug_bounds("approve-req-1")
        .expect("the approval row must be on screen to measure it");

    // End the turn: the trailer goes, and `TurnCompleted` adds no row of
    // its own when it is not an error, so the list content is unchanged.
    h.plumbing.push_feed(FeedEvent::Records {
        id: "aaaa1111".into(),
        epoch: 1,
        records: vec![turn_completed_record(4)],
    });
    vcx.run_until_parked();
    assert!(!shown(vcx, "work-trailer"));
    let after = vcx
        .debug_bounds("approve-req-1")
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
    let rail = vcx
        .debug_bounds("rail-aaaa1111")
        .expect("the rail row did not render");
    assert!(
        overlaps(menu, rail),
        "the fixture proves nothing unless the menu actually covers a live \
         affordance — menu {menu:?} rail {rail:?}"
    );

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
    assert!(h.drain().is_empty(), "the click fell through to the shell");
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
            shell.read(cx).composer.read(cx).text.clone()
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
    let rail = vcx.debug_bounds("rail-aaaa1111").expect("no rail row");
    assert!(
        overlaps(menu, rail),
        "the fixture must cover a live affordance"
    );

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
/// property: `launcher::is_live` reads
/// `eave_core::session::POSTURE_VERIFIED_PROVIDERS`, the same constant the
/// engine refuses on, so a tile that looks clickable is one that starts.
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
        .filter(|(_, p)| crate::launcher::is_live(p))
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
        Command::Launch { provider, model } => {
            assert_eq!(provider, "claude-code");
            assert_eq!(*model, None, "eave invented a model nobody chose");
        }
        other => panic!("start emitted {other:?}"),
    }
}

/// The launcher is the overlay layer's first product consumer: the model
/// picker is a real P1 popup, and what it chooses reaches the command.
#[gpui::test]
fn the_model_menu_choice_reaches_the_launch(cx: &mut TestAppContext) {
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
