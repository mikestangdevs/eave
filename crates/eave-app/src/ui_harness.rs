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
        .push_row_note(("aaaa1111".into(), Some(RowNote::Error("refused".into()))));
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
    h.plumbing.push_row_note((
        "aaaa1111".into(),
        Some(RowNote::ConfirmDelete {
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
    // Both cursors count (Stage AC). The property is that a click handler
    // always states what the pointer becomes — not that every one of them
    // becomes a hand. The rail's rename field is typed into rather than
    // pressed and takes an I-beam; `cursor_pointer` there would say
    // "button" over a text box.
    let cursors = src.matches(".cursor_pointer()").count() + src.matches(".cursor_text()").count();
    assert_eq!(
        clicks, cursors,
        "every .on_click site needs a .cursor_pointer or .cursor_text — \
         eave had 84 fewer cursor calls than comet and every one of them \
         was the default arrow"
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
            basis: DiffBasis::WorkingTree,
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
    let need = real_text_width(word, theme::SANS, theme::SMALL_SIZE);
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

/// The slice's whole point. Two sessions in the same repo, under the same
/// provider, in the same state used to be one row drawn twice.
#[gpui::test]
fn two_sessions_in_one_repo_do_not_render_as_the_same_row(cx: &mut TestAppContext) {
    let (h, vcx) = boot(cx);
    let a = session("aaaa1111");
    let b = session("bbbb2222");
    // Identical in every fact the rail stated before this slice.
    assert_eq!(a.provider, b.provider);
    assert_eq!(a.cwd, b.cwd);
    assert_eq!(a.state, b.state);
    h.plumbing.push_rail(vec![a.clone(), b.clone()]);
    vcx.run_until_parked();

    // Unnamed — and still told apart, without anyone having to do
    // anything, because the meta line carries the session's identity.
    assert!(shown(vcx, "cwd-aaaa1111") && shown(vcx, "cwd-bbbb2222"));
    assert_ne!(rail_meta_line(&a, None), rail_meta_line(&b, None));

    // Named — the label carries it, which is the version an operator
    // actually reads.
    let mut named_a = a.clone();
    named_a.title = Some("review the parser".into());
    let mut named_b = b.clone();
    named_b.title = Some("chase the flake".into());
    h.plumbing.push_rail(vec![named_a.clone(), named_b.clone()]);
    vcx.run_until_parked();
    assert!(shown(vcx, "label-aaaa1111") && shown(vcx, "label-bbbb2222"));
    assert_ne!(
        row_label(&named_a, "claude code"),
        row_label(&named_b, "claude code")
    );
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
        shell.read(cx).composer.read(cx).text.clone()
    })
    .unwrap()
}

/// Put one session in the rail and make it the selected one.
fn rail_with_selected(h: &Harness, vcx: &mut VisualTestContext, infos: Vec<SessionInfo>, id: &str) {
    h.plumbing.push_rail(infos);
    h.plumbing.push_feed(FeedEvent::Session { id: id.into() });
    vcx.run_until_parked();
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
    let buttons = [
        ("ren-aaaa1111", "rename"),
        ("arc-aaaa1111", "archive"),
        ("wto-aaaa1111", "open"),
        ("wtd-aaaa1111", "delete"),
    ];
    let mut boxes = Vec::new();
    for (selector, word) in buttons {
        let b = vcx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} did not render"));
        // It kept its whole word. A button clipped to "arch…" states a
        // different action from the one it performs.
        let need = real_text_width(word, theme::SANS, theme::SMALL_SIZE);
        assert!(
            b.size.width >= need + px(theme::SPACE_2 * 2.0) - px(1.),
            "{selector} got {:?} for \"{word}\" needing {need:?} plus its \
             own padding — it is clipping its label",
            b.size.width
        );
        // Inside the rail, on both edges.
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
            assert!(
                dx >= px(theme::SPACE_2 - 1.0) || dy >= px(0.),
                "{sa} and {sb} are {dx:?} apart horizontally and {dy:?} \
                 vertically — the row lays out {:?} of gap and they are \
                 drawing over each other",
                px(theme::SPACE_2)
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

/// N4's floor, applied to the one surface in this slice that could break
/// it: **a rename field sitting open costs nothing.**
///
/// The composer's caret blinks, and a blink never settles — its consumer
/// holds a pulse lease for as long as it is on screen (`caret_lit`, and
/// the divergence is documented there). This field draws a SOLID caret
/// precisely so that the idle floor stays reachable with a text box open.
#[gpui::test]
fn an_open_rename_field_asks_for_no_frames(cx: &mut TestAppContext) {
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
        !clock_running(vcx),
        "the shell was already leasing the clock before the field opened — \
         then nothing below is measuring the field"
    );

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
        !clock_running(vcx),
        "the rename caret leased the pulse clock — the whole reason it is \
         drawn solid is that a blink here would pin the window at the \
         frame rate for as long as somebody is naming something",
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

/// Advance drawn time in 100 ms steps until the pulse clock parks, and
/// report how many steps that took. Panics past `cap` rather than
/// spinning — an eternal skeleton is precisely a loop that never ends,
/// and it must fail as a test rather than as a hung suite.
#[track_caller]
fn steps_until_parked(vcx: &mut VisualTestContext, cap: usize, what: &str) -> usize {
    let mut steps = 0usize;
    while clock_running(vcx) {
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
        clock_running(vcx),
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
    // more seconds, and the window is told to draw itself zero times.
    let settled = watch_redraws(&h, vcx);
    for _ in 0..10 {
        vcx.executor().advance_clock(Duration::from_secs(1));
        vcx.run_until_parked();
    }
    assert_eq!(
        settled.get(),
        0,
        "a window whose file list is never coming still redrew itself while \
         the operator was away from the desk"
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
    assert!(clock_running(vcx), "nothing was ever moving");

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
        0,
        "a window that knows no answer is coming still redrew itself"
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
