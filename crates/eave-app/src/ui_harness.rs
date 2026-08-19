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
    vcx.simulate_click(bounds.center(), modifiers);
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

    let all = vec![live_a.clone(), live_b.clone(), dead.clone(), unprobed.clone()];
    let (line, over) = resource_line(&all, Some(32 * GB)).expect("two live agents report");
    assert_eq!(line, "2 agents · 0.5 GB of 32.0 GB");
    assert!(!over, "half a gig of thirty-two is not a warning");

    let (one, _) = resource_line(&[live_a.clone()], Some(32 * GB)).unwrap();
    assert_eq!(one, "1 agent · 0.2 GB of 32.0 GB", "singular reads as singular");

    let mut hog = session("eeee5555");
    hog.live = true;
    hog.rss_bytes = Some(24 * GB);
    let (_, over) = resource_line(&[hog.clone()], Some(32 * GB)).unwrap();
    assert!(over, "three quarters of the machine is the warning case");

    // No denominator is stated as absence, never invented.
    let (bare, over) = resource_line(&all, None).unwrap();
    assert_eq!(bare, "2 agents · 0.5 GB resident");
    assert!(!over, "without a machine size there is no ratio to warn about");

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
    h.plumbing.push_wt((
        "aaaa1111".into(),
        Some(WtNote::Error("refused".into())),
    ));
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
    assert!(!offered(alone, &h, vcx), "no collision, nothing to opt out of");

    let mut started = ready.clone();
    started.state = "idle".into();
    assert!(!offered(started, &h, vcx), "cwd is fixed after the first send");

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
            shell.read(cx).sessions.read(cx).wt_notes.get("aaaa1111").cloned()
        })
        .unwrap();
    assert!(
        matches!(
            stated,
            Some(WtNote::ConfirmDelete { dirty_files: Some(5), unmerged_commits: Some(1) })
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
    assert!(!shown(vcx, "wt-note-aaaa1111"), "a settled action states nothing");

    let _ = h.drain();
    click(vcx, "wtd-aaaa1111");
    let cmds = h.drain();
    assert!(
        matches!(cmds.as_slice(), [Command::RemoveWorktree { force: false, .. }]),
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
    h.plumbing.push_feed(FeedEvent::Session { id: "aaaa1111".into() });
    h.plumbing.push_feed(FeedEvent::Reset { id: "aaaa1111".into() });
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
    h.plumbing.push_feed(FeedEvent::Session { id: "bbbb2222".into() });
    h.plumbing.push_feed(FeedEvent::Reset { id: "bbbb2222".into() });
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
    h.plumbing.push_feed(FeedEvent::Session { id: "aaaa1111".into() });
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
    assert!(h.drain().is_empty(), "return on an empty draft is not a send");
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
    h.plumbing.push_feed(FeedEvent::Session { id: "aaaa1111".into() });
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
    h.plumbing.push_feed(FeedEvent::Session { id: "aaaa1111".into() });
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
            shell.read(cx).files.read(cx).diff.as_ref().map(|d| d.path.clone())
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
    h.plumbing.push_feed(FeedEvent::Session { id: "aaaa1111".into() });
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
    assert_eq!(opened, vec![PathBuf::from("/tmp/eave-harness/repo/src/lib.rs")]);
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
fn approval_record(seq: u64, request_id: &str, tool: &str, path: &str) -> eave_core::journal::Record {
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
    h.plumbing.push_feed(FeedEvent::Session { id: "aaaa1111".into() });
    h.plumbing.push_feed(FeedEvent::Reset { id: "aaaa1111".into() });
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
    ask(&h, vcx, vec![approval_record(1, "req-1", "Write", "/repo/src/main.rs")]);
    let _ = h.drain();

    assert!(shown(vcx, "approval-req-1"), "the question must be on screen");
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
    ask(&h, vcx, vec![approval_record(1, "req-1", "Bash", "/repo/out.txt")]);
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
    ask(&h, vcx, vec![approval_record(1, "req-2", "Bash", "/repo/out.txt")]);
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
    ask(&h, vcx, vec![approval_record(1, "req-1", "Write", "/repo/a.txt")]);
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

    for mode in ["default", "manual", "bypassPermissions", "acceptEdits", "dontAsk"] {
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
    assert!(hint_before.is_some(), "the composer always states its own key");

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
    ask(&h, vcx, vec![approval_record(1, "req-1", "Bash", "/repo/target")]);
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
    h.plumbing.push_feed(FeedEvent::Reset { id: "aaaa1111".into() });
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

    // Nothing subscribed at all.
    assert!(
        shown(vcx, "transcript-empty"),
        "with no session selected the well must say so"
    );

    // Subscribed, and the engine's own count says the journal is empty.
    let mut fresh = session("aaaa1111");
    fresh.last_seq = 0;
    h.plumbing.push_rail(vec![fresh]);
    h.plumbing.push_feed(FeedEvent::Session { id: "aaaa1111".into() });
    h.plumbing.push_feed(FeedEvent::Reset { id: "aaaa1111".into() });
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
    h.plumbing.push_feed(FeedEvent::Session { id: "bbbb2222".into() });
    h.plumbing.push_feed(FeedEvent::Reset { id: "bbbb2222".into() });
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
    h.plumbing.push_feed(FeedEvent::Session { id: "aaaa1111".into() });
    h.plumbing.push_feed(FeedEvent::Reset { id: "aaaa1111".into() });
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
    h.window
        .update(vcx, |_, _, cx| cx.notify())
        .unwrap();
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
    h.window
        .update(vcx, |_, _, cx| cx.notify())
        .unwrap();
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
    assert!(
        shown(vcx, "transcript-empty"),
        "no session is selected, so the note should be on screen"
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
    assert_eq!(h.window.update(vcx, |_, window, cx| window.simulate_next_frame(cx)).unwrap(), 0);
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

    h.plumbing.push_feed(FeedEvent::Session { id: "aaaa1111".into() });
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
fn a_glyph_swap_would_move_the_text_which_is_why_the_blink_recolours(
    _cx: &mut TestAppContext,
) {
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
        ("send-glyph", theme::ICON_SEND),
        ("new-session-glyph", theme::ICON_INLINE),
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
    assert!(theme::ICON_STATUS <= theme::FILE_STATUS_W);
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
