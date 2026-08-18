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
