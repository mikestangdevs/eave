//! The turn model against a real background-subagent capture.
//!
//! `fixtures/claude-background-subagent-2.1.228.jsonl` is comet's live
//! capture of Claude CLI 2.1.228 (session `a8fb1e09-…`), copied in because
//! eave has none of its own and cannot cheaply produce one: it needs a
//! background subagent to finish and wake the CLI.
//!
//! Slice CG's survey ends with this as unknown #9 — *"nothing here is
//! verified against eave's own adapters at runtime… replaying both fixtures
//! through `ClaudeStream::parse_line` is the cheapest possible confirmation
//! and should be the consuming slice's first act."* This is that act. Every
//! claim below is read off eave's own parser, not off the JSON by eye.

use eave_core::events::SessionEvent;
use eave_core::journal::{JournalEvent, Record};
use eave_core::stream::ClaudeStream;
use eave_core::thresholds::AttentionThresholds;
use eave_core::turn::{TurnLedger, TurnStart};

fn fixture(name: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn rec(seq: u64, event: JournalEvent) -> Record {
    Record {
        seq,
        ts_ms: seq,
        event,
    }
}

/// Fold the whole capture, recording which turn shape was open as each
/// wire line was consumed.
fn replay(name: &str) -> (TurnLedger, Vec<(usize, Option<TurnStart>)>) {
    let src = fixture(name);
    let mut stream = ClaudeStream::new();
    let mut ledger = TurnLedger::new(0);
    let mut seq = 0u64;
    // The capture begins with the operator's prompt already sent: the
    // journal records that pair, the CLI's stdout does not.
    for ev in [
        JournalEvent::UserSent {
            text: "launch a background subagent".into(),
        },
        JournalEvent::UserDelivered {},
    ] {
        seq += 1;
        ledger.on_record(&rec(seq, ev), seq);
    }
    let mut trail = Vec::new();
    for (ix, line) in src.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        for event in stream.parse_line(line) {
            seq += 1;
            ledger.on_record(&rec(seq, JournalEvent::Provider { event }), seq);
        }
        trail.push((ix + 1, ledger.open()));
    }
    (ledger, trail)
}

fn shape_at(trail: &[(usize, Option<TurnStart>)], line: usize) -> Option<TurnStart> {
    trail
        .iter()
        .find(|(l, _)| *l == line)
        .unwrap_or_else(|| panic!("line {line} produced no fold"))
        .1
}

/// **The bug, and the arithmetic that proves it fixed.**
///
/// One user prompt; eave's parser yields TWO `TurnCompleted` frames, because
/// the background-subagent wake is a whole second turn on the same session
/// (`docs/research/turn-start-evidence.md` §1). Under the old model that ran
/// +1, −1, −1 → floored at −1, so the operator's NEXT delivery landed at 0
/// and `in_turn()` — `> 0` — was **false**: no working trailer, and the
/// attention clocks disarmed, on a turn that was genuinely running.
///
/// Recording the second `init` as a turn of its own nets it: +1, −1, +1, −1.
#[test]
fn a_background_subagent_wake_leaves_the_next_turn_countable() {
    let (mut ledger, _) = replay("claude-background-subagent-2.1.228.jsonl");
    assert_eq!(
        ledger.open(),
        None,
        "the capture ends with both turns closed"
    );
    // The whole point: the next thing the operator asks is visible.
    let n = 10_000;
    ledger.on_record(
        &rec(
            n,
            JournalEvent::UserSent {
                text: "now what?".into(),
            },
        ),
        n,
    );
    ledger.on_record(&rec(n + 1, JournalEvent::UserDelivered {}), n + 1);
    assert_eq!(
        ledger.open(),
        Some(TurnStart::Prompted),
        "an unpaired wake `result` swallowed the operator's next turn"
    );
}

/// All three turn shapes, in one capture, read off eave's own parser.
///
/// This is the evidence that `Unannounced` is not an invented occupant: the
/// subagent's `Bash`, its permission question and its prose all land after
/// the operator's `result` (line 60) and before the wake turn is announced
/// (line 78), and **no terminal frame ever covers them**.
#[test]
fn the_capture_contains_all_three_turn_shapes_in_order() {
    let (_, trail) = replay("claude-background-subagent-2.1.228.jsonl");
    for (line, want, what) in [
        (59, Some(TurnStart::Prompted), "the operator's turn"),
        (60, None, "its `result` closes it"),
        (
            63,
            Some(TurnStart::Unannounced),
            "the subagent's Bash, with nothing announcing a turn",
        ),
        (
            66,
            Some(TurnStart::Unannounced),
            "its permission question, still unannounced",
        ),
        (
            77,
            Some(TurnStart::Unannounced),
            "still unannounced at the state change",
        ),
        (
            78,
            Some(TurnStart::Announced),
            "the second `system/init` announces the wake turn",
        ),
        (
            113,
            Some(TurnStart::Announced),
            "parent-level output nobody asked for",
        ),
        (119, None, "the wake turn's own `result` closes it"),
    ] {
        assert_eq!(shape_at(&trail, line), want, "line {line}: {what}");
    }
}

/// The unannounced stretch settles on eave's existing quiet clock and on
/// nothing else — no watchdog, no new constant, and never while the
/// operator's own permission question is standing.
#[test]
fn the_unannounced_stretch_settles_on_quiet_but_not_over_an_open_question() {
    let src = fixture("claude-background-subagent-2.1.228.jsonl");
    let quiet = AttentionThresholds::default().quiet_after_ms;
    let mut stream = ClaudeStream::new();
    let mut ledger = TurnLedger::new(0);
    let mut seq = 0u64;
    for ev in [
        JournalEvent::UserSent { text: "go".into() },
        JournalEvent::UserDelivered {},
    ] {
        seq += 1;
        ledger.on_record(&rec(seq, ev), seq);
    }
    let mut answered: Option<String> = None;
    for (ix, line) in src.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        for event in stream.parse_line(line) {
            let question = match &event {
                SessionEvent::ApprovalRequested { request_id, .. } => Some(request_id.clone()),
                _ => None,
            };
            seq += 1;
            ledger.on_record(&rec(seq, JournalEvent::Provider { event }), seq);
            if let Some(id) = question {
                answered = Some(id);
                // Parked on a human: silence here is the operator's, not the
                // agent's, and no length of it may settle the turn.
                assert!(
                    !ledger.settle(seq + quiet * 100, quiet),
                    "an open question was quiesced away at line {}",
                    ix + 1
                );
            }
        }
        // Stop just before the wake turn is announced.
        if ix + 1 == 76 {
            break;
        }
    }
    // The operator answers. Until they do, the turn is held on THEM — the
    // assertion inside the loop is what proves that.
    let id = answered.expect("the capture's permission question never arrived");
    seq += 1;
    ledger.on_record(
        &rec(
            seq,
            JournalEvent::ApprovalDecided {
                request_id: id,
                allowed: true,
                message: None,
            },
        ),
        seq,
    );
    // A capture that stopped there — the subagent finished, the CLI never
    // woke — leaves unannounced work open. It settles, once, on quiet.
    assert_eq!(ledger.open(), Some(TurnStart::Unannounced));
    assert!(!ledger.settle(seq + quiet - 1, quiet), "settled early");
    assert!(ledger.settle(seq + quiet, quiet));
    assert_eq!(ledger.open(), None);
}

/// The negative control, from eave's OWN capture: an ordinary turn — with a
/// foreground `Bash` whose `task_started`/`task_notification` pair is
/// field-for-field identical to the background one (CG §(b)) — must produce
/// exactly one turn and leave nothing open behind it.
///
/// This is what stops the model from captioning routine tool traffic as the
/// agent working on its own.
#[test]
fn an_ordinary_tool_turn_opens_exactly_one_turn_and_leaves_nothing_open() {
    // eave's own capture, taken 2026-08-15 and written up in
    // `docs/research/claude-stream-notes.md`.
    let src = fixture("claude-tool-turn-2026-08-15.jsonl");
    let mut stream = ClaudeStream::new();
    let mut ledger = TurnLedger::new(0);
    let mut seq = 0u64;
    for ev in [
        JournalEvent::UserSent {
            text: "sleep 8 then echo".into(),
        },
        JournalEvent::UserDelivered {},
    ] {
        seq += 1;
        ledger.on_record(&rec(seq, ev), seq);
    }
    let mut shapes = Vec::new();
    for line in src.lines() {
        // Each capture line is `<seconds> <json>`.
        let Some(ix) = line.find('{') else { continue };
        for event in stream.parse_line(&line[ix..]) {
            seq += 1;
            ledger.on_record(&rec(seq, JournalEvent::Provider { event }), seq);
            shapes.push(ledger.open());
        }
    }
    assert!(
        shapes
            .iter()
            .all(|s| matches!(s, Some(TurnStart::Prompted) | None)),
        "an ordinary turn produced an agent-started turn: {:?}",
        shapes
            .iter()
            .filter(|s| !matches!(s, Some(TurnStart::Prompted) | None))
            .collect::<Vec<_>>()
    );
    assert_eq!(ledger.open(), None, "the turn never closed");
    let quiet = AttentionThresholds::default().quiet_after_ms;
    assert!(
        !ledger.settle(seq + quiet * 100, quiet),
        "nothing to settle"
    );
}
