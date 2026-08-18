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
    let vcx = VisualTestContext::from_window(window.into(), cx).into_mut();
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

    // Hover coverage. Five controls across four sites had a handler and no
    // feedback of any kind; each site is checked by its own selector so a
    // regression names the control it broke rather than a number.
    for (selector, kind) in [
        ("attn-kill", "danger_wash_hover"),
        ("send", "inverse_hover"),
        ("to-bottom", "chrome_glass_hover"),
    ] {
        let anchor = format!(r#"debug_selector(|| "{selector}".into())"#);
        let at = src
            .find(&anchor)
            .unwrap_or_else(|| panic!("{selector} is no longer tagged"));
        let window = &src[at..src.len().min(at + 900)];
        assert!(
            window.contains(".hover("),
            "{selector} lost its hover feedback"
        );
        assert!(
            window.contains(kind),
            "{selector} should hover to {kind} — a filled control replaces \
             its bg rather than compositing the neutral wash over it"
        );
    }
    // approve / deny share one builder and one selector expression.
    let at = src.find("fn decision_button(").expect("decision_button");
    let window = &src[at..src.len().min(at + 2400)];
    assert!(
        window.contains(".hover("),
        "approve and deny lost their hover feedback"
    );
}
