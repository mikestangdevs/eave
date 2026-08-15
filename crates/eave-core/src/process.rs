//! Process-group signalling and kill verification (spec 01 §Kill & verify).
//!
//! Kill means proof of exit, not signal delivery. The group leader's exit is
//! reaped by the supervisor; group liveness is probed with `killpg(pgid, 0)`;
//! stragglers get exactly one bounded `ps` scan — synara's unthrottled scans
//! hit ~120 per teardown (research: synara.md pain points).

use std::io;
use std::time::Duration;

use tokio::process::Command;

use crate::proc::{self, ProcIdentity};
use crate::thresholds::{KILL_GRACE, KILL_VERIFY_WINDOW};

pub fn killpg(pgid: i32, signal: i32) -> io::Result<()> {
    if unsafe { libc::killpg(pgid, signal) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Does the process group still have members? EPERM counts as alive: a
/// member we cannot signal is still a member.
pub fn group_alive(pgid: i32) -> bool {
    if unsafe { libc::killpg(pgid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Poll until the group is empty or the window elapses. Returns true if the
/// group emptied.
pub async fn await_group_empty(pgid: i32, window: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        if !group_alive(pgid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanOutcome {
    /// The breadcrumb no longer matches a live process — nothing to do.
    AlreadyGone,
    Reaped { escalated: bool },
    /// Exit could not be proven; never reported reaped on hope.
    Unproven,
}

/// v0 orphan policy (spec 02 §Resume contract step 2): a `meta.json`
/// breadcrumb that still matches a live process is killed and verified,
/// never re-adopted. The orphan is not our child — no `waitpid` — so proof
/// of exit is the (pid, start_time) guard going false and the group
/// emptying. The child was spawned via `setsid`, so its pgid is its pid.
pub async fn reap_orphan(id: &ProcIdentity) -> OrphanOutcome {
    if !proc::identity_matches(id) {
        return OrphanOutcome::AlreadyGone;
    }
    let _ = killpg(id.pid, libc::SIGTERM);
    if await_orphan_gone(id, KILL_GRACE).await {
        return OrphanOutcome::Reaped { escalated: false };
    }
    let _ = killpg(id.pid, libc::SIGKILL);
    if await_orphan_gone(id, KILL_VERIFY_WINDOW).await {
        OrphanOutcome::Reaped { escalated: true }
    } else {
        OrphanOutcome::Unproven
    }
}

async fn await_orphan_gone(id: &ProcIdentity, window: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        if !proc::identity_matches(id) && !group_alive(id.pid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The single bounded descendant scan: one `ps` invocation listing every
/// pid in `pgid`. Used only after group SIGKILL still left members.
pub async fn scan_group_pids(pgid: i32) -> io::Result<Vec<i32>> {
    let output = Command::new("ps")
        .args(["-eo", "pid=,pgid="])
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut pids = Vec::new();
    for line in stdout.lines() {
        let mut cols = line.split_whitespace();
        let (Some(pid), Some(line_pgid)) = (cols.next(), cols.next()) else {
            continue;
        };
        if line_pgid.parse::<i32>() == Ok(pgid) {
            if let Ok(pid) = pid.parse::<i32>() {
                pids.push(pid);
            }
        }
    }
    Ok(pids)
}
