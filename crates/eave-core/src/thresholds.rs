//! Every attention/kill timing constant lives here and nowhere else
//! (spec 01 §Attention clocks: "defined in one place").

use std::time::Duration;

/// Quiet clock: runs only while nothing is in flight (no outstanding
/// `tool_use`, no pending approval hold). Elapsing triggers classification
/// into `quiet` vs `unresponsive` — never a kill (spec 01).
pub const QUIET_AFTER: Duration = Duration::from_secs(120);

/// Separate threshold for an outstanding tool call; while a tool is in
/// flight, `QUIET_AFTER` does not run (spec 01).
pub const TOOL_EXEC_AFTER: Duration = Duration::from_secs(10 * 60);

/// `unresponsive` requires zero CPU across this many consecutive samples
/// with nothing in flight (spec 01).
pub const CPU_SAMPLES: u32 = 6;
pub const CPU_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Kill escalation: group SIGTERM → grace → group SIGKILL (spec 01 §Kill).
pub const KILL_GRACE: Duration = Duration::from_secs(3);

/// After SIGKILL the group is probed for this long before the kill is
/// declared unproven.
pub const KILL_VERIFY_WINDOW: Duration = Duration::from_secs(5);

/// Descendants holding inherited pipes may emit stale lines after the group
/// leader dies; protocol input is dropped from kill initiation (spec 01,
/// research: bb.md pain points — the spec's 1 s window is subsumed by
/// dropping everything after initiation, which is strictly safer).
pub const STALE_PIPE_DROP: Duration = Duration::from_secs(1);

/// Journal write-behind: appends buffer in memory and reach the OS on this
/// tick — the same ~120 ms coalesce cadence as the wire (spec 02
/// §Durability; research: deepseek-harness.md §3). Terminal records are
/// additionally fsynced at append time.
pub const JOURNAL_FLUSH_INTERVAL: Duration = Duration::from_millis(120);

/// The attention-clock values the classifier actually runs with, in ms.
/// Defaults are the constants above; each is overridable via `EAVE_THRESH_*`
/// env vars — **dev/test-loop only** (the acceptance tests run the real
/// classifier at scaled-down clocks; nothing else may set these).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttentionThresholds {
    pub quiet_after_ms: u64,
    pub tool_exec_after_ms: u64,
    pub cpu_samples: u32,
    pub cpu_sample_interval_ms: u64,
}

impl Default for AttentionThresholds {
    fn default() -> Self {
        Self {
            quiet_after_ms: QUIET_AFTER.as_millis() as u64,
            tool_exec_after_ms: TOOL_EXEC_AFTER.as_millis() as u64,
            cpu_samples: CPU_SAMPLES,
            cpu_sample_interval_ms: CPU_SAMPLE_INTERVAL.as_millis() as u64,
        }
    }
}

impl AttentionThresholds {
    pub fn from_env() -> Self {
        fn get<T: std::str::FromStr>(key: &str, default: T) -> T {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        }
        let d = Self::default();
        Self {
            quiet_after_ms: get("EAVE_THRESH_QUIET_MS", d.quiet_after_ms),
            tool_exec_after_ms: get("EAVE_THRESH_TOOL_MS", d.tool_exec_after_ms),
            cpu_samples: get("EAVE_THRESH_CPU_SAMPLES", d.cpu_samples),
            cpu_sample_interval_ms: get("EAVE_THRESH_CPU_SAMPLE_MS", d.cpu_sample_interval_ms),
        }
    }

    /// Monitor tick granularity: fine enough to hit scaled-down test clocks
    /// promptly, capped at 1 s for the real ones.
    pub fn tick_ms(&self) -> u64 {
        (self.quiet_after_ms.min(self.tool_exec_after_ms) / 8).clamp(25, 1000)
    }
}

// ---------------------------------------------------------------------------
// The page engine's clocks (Slice E3)
// ---------------------------------------------------------------------------
//
// These are here rather than next to the CEF code for the reason at the top
// of this file: every attention/kill timing constant lives in one place. A
// browser eviction clock is one of those even though the thing it stops is
// Chromium rather than an agent — and `eave-core` is the crate both the
// engine and the app already depend on.

/// How long a page tab may go un-looked-at before eave takes its browser
/// back (Slice E3).
///
/// **The cost this buys back is measured, and it is not the cost that was
/// written down.** C5's caveat said ~322 MB per live browser and therefore
/// ~3.2 GB at ten tabs. E3 measured the shape of it instead of inheriting
/// the number (`scripts/e3-evict-proof.py`, ten loopback origins each
/// holding a live 32 MB array, this machine, 2026-08-22):
///
/// | state | helper processes | resident |
/// | --- | --- | --- |
/// | CEF up, no page resident | 5 | 0.32 GB |
/// | ten page tabs | 11 | 0.90 GB |
/// | after eviction | 5 | 0.32 GB |
///
/// So ~322 MB is very close to eave's **floor** — what Chromium's GPU,
/// network, storage and utility helpers cost before a single page exists —
/// and the marginal cost of a tab is ~58 MB of which 32 MB is the page's
/// own array. Ten tabs cost 0.90 GB here, not 3.2 GB. The floor is paid
/// once and eviction cannot touch it; what eviction reclaims is everything
/// above it, which on this run was **all** of it.
///
/// The conditions matter and travel with the number: these are trivial
/// local pages. A real page's renderer is larger and ten heavy ones would
/// cost much more — but they would cost more *as pages*, not as tabs.
///
/// Twenty minutes is the value this slice was told to start at, and it is a
/// **decision rather than a derivation** — there is no measurement behind
/// the number and it should not be quoted as though there were. What IS
/// arguable is the shape of the cost on either side of it: too short and
/// the operator pays a reload for a tab they were about to return to; too
/// long and the memory is never actually given back. Twenty minutes sits
/// past any plausible "I looked at something else for a moment" and inside
/// a normal working session.
///
/// The thing that would move it is evidence eave does not have yet: how
/// long a real operator's un-looked-at tabs actually stay un-looked-at.
/// `docs/dogfood-log.md` is where that would come from.
pub const PAGE_IDLE_EVICT_AFTER: Duration = Duration::from_secs(20 * 60);

/// How long after a page starts an audio stream eave keeps treating that tab
/// as playing media (Slice E3).
///
/// **This constant exists because of a hole in what CEF will tell us, and
/// it should disappear if that hole ever closes.** CEF 151 offers exactly
/// one way to observe page audio — `CefAudioHandler` — and taking it means
/// the audio stream is routed to the handler as PCM *instead of* to the
/// speakers. eave declines the capture (`get_audio_parameters` returns 0),
/// which leaves playback alone but means the only edge we ever see is the
/// START of a stream. There is no stop edge to wait for.
///
/// So "is this tab playing audio?" is answered by a hold with a timeout,
/// and both failure modes are real and worth naming:
///
/// - a page playing ONE continuous four-hour stream is evicted at the hour
///   mark, and the audio stops;
/// - a page that played a single notification sound is held resident for an
///   hour after it.
///
/// An hour is 3× [`PAGE_IDLE_EVICT_AFTER`], which is the actual argument
/// for it: long enough that ordinary music and video are not cut off
/// mid-play, short enough that a beep does not pin a tab for a session.
pub const PAGE_AUDIO_HOLD: Duration = Duration::from_secs(60 * 60);

/// Read a duration from the environment in milliseconds, falling back to a
/// constant. **Dev/test-loop only**, exactly like the `EAVE_THRESH_*` knobs
/// on [`AttentionThresholds`]: a twenty-minute clock cannot be exercised by
/// a test that has to finish, so the test scales it down and nothing else
/// may.
fn env_duration(key: &str, default: Duration) -> Duration {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(default)
}

/// [`PAGE_IDLE_EVICT_AFTER`], overridable via `EAVE_THRESH_PAGE_EVICT_MS`.
pub fn page_idle_evict_after() -> Duration {
    env_duration("EAVE_THRESH_PAGE_EVICT_MS", PAGE_IDLE_EVICT_AFTER)
}

/// [`PAGE_AUDIO_HOLD`], overridable via `EAVE_THRESH_PAGE_AUDIO_MS`.
pub fn page_audio_hold() -> Duration {
    env_duration("EAVE_THRESH_PAGE_AUDIO_MS", PAGE_AUDIO_HOLD)
}

/// How many page tabs may hold a browser at once.
///
/// # What this is for, and why it is not the same knob as eviction
/// [`PAGE_IDLE_EVICT_AFTER`] answers "when does a tab eave built a browser
/// for give it back". This answers the other question C5 and E3 both left
/// open: **which tabs get a browser before anybody asks.** Until now the
/// answer was "none" — creation required activation — so every restored
/// tab booted the first time it was opened or pulled into a split, and the
/// well showed "starting the page…" over an empty pane while Chromium came
/// up. That is the defect this bounds.
///
/// # Where eight comes from
/// A decision, like the twenty minutes, and it should not be quoted as a
/// derivation. What IS measured is the cost either side of it: E3 found ten
/// page tabs at 0.90 GB against a 0.32 GB floor, so a resident tab is worth
/// roughly **60 MB**, not the ~322 MB the create-on-activation rule was
/// written to avoid. Eight is therefore about half a gigabyte over the
/// floor — inside what the measurement says is affordable, and past any
/// plausible "the tabs I am actually working in".
///
/// The thing that would move it is the same evidence that would move the
/// eviction clock: how many tabs a real operator actually cycles between.
/// `docs/dogfood-log.md` is where that comes from.
pub const PAGE_PRELOAD_MAX: usize = 8;

/// [`PAGE_PRELOAD_MAX`], overridable via `EAVE_THRESH_PAGE_PRELOAD_MAX`.
///
/// Zero is a legitimate value and restores the old behaviour exactly: no
/// tab is warmed, and every page boots when it is first shown.
pub fn page_preload_max() -> usize {
    std::env::var("EAVE_THRESH_PAGE_PRELOAD_MAX")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(PAGE_PRELOAD_MAX)
}
