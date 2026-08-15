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
