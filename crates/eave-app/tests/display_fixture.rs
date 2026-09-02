//! Headless Stage D acceptance against the REAL captured wire fixture
//! (build directive: "Stage D tuning must run against real pacing and real
//! markdown, not the fake").
//!
//! What renders is `DisplayList` output; these tests pin the properties the
//! GPUI layer relies on: minimal splices, fence-close = remeasure (the
//! headless form of "no visible reflow when a code fence closes"), stable
//! identity, and fold correctness vs a from-scratch rebuild.

use eave_app::display::{DisplayList, DisplayRow};
use eave_app::fixture;
use eave_app::markdown::Block;
use eave_wire::model::Transcript;
use eave_wire::SubscriptionItem;

fn fixture_steps() -> Vec<fixture::Step> {
    let path = fixture::default_path();
    let steps = fixture::load(&path).unwrap_or_else(|e| {
        panic!(
            "fixture {} unreadable: {e} — run tests from the workspace",
            path.display()
        )
    });
    assert!(!steps.is_empty(), "fixture is empty");
    steps
}

/// Replay the fixture burst-by-burst, calling `check(burst_ix, ops,
/// open_fences_before, open_fences_after, list)` after every applied burst.
fn replay(mut check: impl FnMut(usize, &eave_app::display::DiffOps, usize, usize, &DisplayList)) {
    let mut transcript = Transcript::new();
    let mut display = DisplayList::new();
    let open_fences = |d: &DisplayList| -> usize {
        d.rows()
            .iter()
            .filter(|r| matches!(d.block(r.id), Some(Block::Fence { closed: false, .. })))
            .count()
    };
    for (ix, step) in fixture_steps().into_iter().enumerate() {
        match step.item {
            SubscriptionItem::Reset { .. } => {
                transcript.reset();
                display.update(&transcript);
            }
            SubscriptionItem::Attention { .. }
            | SubscriptionItem::Sessions { .. }
            | SubscriptionItem::Files { .. } => {}
            // Slice B1: this fixture is a transcript capture. Browser
            // snapshots are not in it and would not be a transcript if
            // they were.
            SubscriptionItem::Tabs { .. }
            | SubscriptionItem::Bookmarks { .. }
            | SubscriptionItem::Permissions { .. }
            | SubscriptionItem::TermScreen { .. }
            | SubscriptionItem::TermOutput { .. }
            | SubscriptionItem::TermExited { .. } => {}
            SubscriptionItem::Events { epoch, records, .. } => {
                let before = open_fences(&display);
                transcript
                    .apply(epoch, &records)
                    .expect("fixture is gap-free");
                let ops = display.update(&transcript);
                let after = open_fences(&display);
                check(ix, &ops, before, after, &display);
            }
        }
    }
}

#[test]
fn fixture_applies_with_minimal_splices_and_no_resets() {
    let mut splices = 0usize;
    let mut spliced_rows = 0usize;
    let mut bursts = 0usize;
    let mut max_splice = 0usize;
    replay(|_, ops, _, _, list| {
        bursts += 1;
        assert!(
            !ops.reset,
            "mid-stream reset must never happen on the live path"
        );
        if let Some((range, count)) = &ops.splice {
            splices += 1;
            let touched = range.len().max(*count);
            spliced_rows += touched;
            max_splice = max_splice.max(touched);
            assert!(
                touched <= 4,
                "splice touched {touched} rows — not minimal (list len {})",
                list.len()
            );
        }
    });
    assert!(
        bursts > 50,
        "fixture should contain a real stream, got {bursts} bursts"
    );
    // The overwhelming majority of streaming bursts must be pure
    // remeasure/no-op; splices only when the block count changes.
    assert!(
        splices * 3 <= bursts,
        "{splices} splices across {bursts} bursts — virtualization would thrash"
    );
}

#[test]
fn fence_close_is_remeasure_never_splice() {
    let mut closes_seen = 0usize;
    replay(|ix, ops, before, after, _| {
        if after < before {
            closes_seen += 1;
            // Mending already displayed the fence as closed, so the close
            // itself must re-create nothing on screen: at most a pure
            // insertion (empty old range) after it, plus remeasure.
            if let Some((range, _)) = &ops.splice {
                assert!(
                    range.is_empty(),
                    "burst {ix}: fence close re-created on-screen rows {range:?} — visible reflow: {ops:?}"
                );
            }
        }
    });
    assert!(
        closes_seen >= 5,
        "fixture must exercise fence closes (prompt asked for ≥8 fences), saw {closes_seen}"
    );
}

#[test]
fn incremental_fold_equals_batch_rebuild() {
    let mut transcript = Transcript::new();
    let mut incremental = DisplayList::new();
    for step in fixture_steps() {
        match step.item {
            SubscriptionItem::Reset { .. } => {
                transcript.reset();
                incremental.update(&transcript);
            }
            SubscriptionItem::Events { epoch, records, .. } => {
                transcript.apply(epoch, &records).unwrap();
                incremental.update(&transcript);
            }
            SubscriptionItem::Attention { .. }
            | SubscriptionItem::Sessions { .. }
            | SubscriptionItem::Files { .. } => {}
            // Slice B1: this fixture is a transcript capture. Browser
            // snapshots are not in it and would not be a transcript if
            // they were.
            SubscriptionItem::Tabs { .. }
            | SubscriptionItem::Bookmarks { .. }
            | SubscriptionItem::Permissions { .. }
            | SubscriptionItem::TermScreen { .. }
            | SubscriptionItem::TermOutput { .. }
            | SubscriptionItem::TermExited { .. } => {}
        }
    }
    let mut batch = DisplayList::new();
    batch.update(&transcript);
    let a: &[DisplayRow] = incremental.rows();
    let b: &[DisplayRow] = batch.rows();
    assert_eq!(a, b, "incremental fold diverged from batch rebuild");
    assert!(
        transcript.closed.is_some(),
        "fixture ends with a Closed record"
    );
    // Sanity on content: the real stream produced tools and fences.
    assert!(
        a.len() > 30,
        "expected a rich display list, got {} rows",
        a.len()
    );
}

/// Per-thread CPU time — the fold is single-threaded pure compute, so
/// this IS its cost. Wall-clock here measured the scheduler, not the
/// fold (the documented trap: this machine idles at load >100, and the
/// wall version of this assert failed 6/6 solo on B1 code and B2 code
/// alike while the fold itself was unchanged).
fn thread_cpu() -> std::time::Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
    }
    std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

#[test]
fn update_cost_stays_flat_on_a_10k_row_transcript() {
    // CPU half of the 16 ms render budget: the per-burst fold on a
    // 10k-row transcript must be far below one frame. (The paint half is
    // measured in the app's perf mode — see ui::perf.)
    let mut transcript = Transcript::new();
    let steps = fixture_steps();
    // Build a long transcript by replaying the fixture's records many
    // times as settled history (fresh rows, same real markdown).
    let base_rows = {
        let mut t = Transcript::new();
        for step in &steps {
            if let SubscriptionItem::Events { epoch, records, .. } = &step.item {
                t.apply(*epoch, records).unwrap();
            }
        }
        t.rows
    };
    while transcript.rows.len() < 10_000 {
        for row in &base_rows {
            transcript.push_row(row.clone());
        }
    }
    while transcript.rows.len() > 10_000 {
        transcript.rows.pop();
        transcript.revs.pop();
    }

    let mut display = DisplayList::new();
    let t0 = std::time::Instant::now();
    display.update(&transcript);
    let cold = t0.elapsed();

    // Streaming burst: grow a live tail message.
    transcript.push_row(eave_wire::model::Row::Assistant {
        text: "live ".into(),
        live: true,
    });
    display.update(&transcript);
    let live_ix = transcript.rows.len() - 1;
    let mut worst = std::time::Duration::ZERO;
    for i in 0..200 {
        if let Some(eave_wire::model::Row::Assistant { text, .. }) = transcript.rows.last_mut() {
            text.push_str(if i % 17 == 0 {
                "\n\nnew para "
            } else {
                "more words "
            });
        }
        transcript.touch(live_ix);
        let t = thread_cpu();
        let ops = display.update(&transcript);
        worst = worst.max(thread_cpu() - t);
        assert!(!ops.reset);
    }
    eprintln!(
        "10k-row fold: cold build {cold:?} (wall), worst streaming burst {worst:?} (thread CPU)"
    );
    assert!(
        worst < std::time::Duration::from_millis(4),
        "per-burst fold cost {worst:?} eats the 16 ms frame budget"
    );
}
