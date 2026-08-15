//! Process-group signalling and kill verification (spec 01 §Kill & verify).
//!
//! Kill means proof of exit, not signal delivery. The group leader's exit is
//! reaped by the supervisor; group liveness is probed with `killpg(pgid, 0)`;
//! stragglers get exactly one bounded `ps` scan — synara's unthrottled scans
//! hit ~120 per teardown (research: synara.md pain points).

use std::io;
use std::time::Duration;

use tokio::process::Command;

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
