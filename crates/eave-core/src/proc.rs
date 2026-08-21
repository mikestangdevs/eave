//! Process identity and OS-fact inspection (macOS).
//!
//! Every signal to a stored pid is gated on the **(pid, start_time)** pair
//! recorded at spawn — table stakes across the reference corpus
//! (research: 00-divergences.md). start_time comes from libproc's
//! `proc_bsdinfo`, which also yields the OS facts spec 01 classifies
//! `failed` from (zombie status, parent link).

use std::mem;

/// `proc_bsdinfo.pbi_status` value for a zombie (SZOMB in sys/proc.h).
const SZOMB: u32 = 5;

/// `PROC_ALL_PIDS` from `<sys/proc_info.h>`. Declared here because the
/// `libc` crate binds `proc_listpids` but not its selector constants.
const PROC_ALL_PIDS: u32 = 1;

/// The identity recorded at spawn. Signals are only ever sent to a pid
/// whose live start_time still matches this record. Serialized into
/// `meta.json` as the orphan-detection breadcrumb (spec 02 §Layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProcIdentity {
    pub pid: i32,
    pub start_tvsec: u64,
    pub start_tvusec: u64,
}

/// A snapshot of OS facts about a live pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcInfo {
    pub start_tvsec: u64,
    pub start_tvusec: u64,
    pub ppid: i32,
    pub is_zombie: bool,
}

/// Inspect a pid via `proc_pidinfo(PROC_PIDTBSDINFO)`. `None` means the
/// process does not exist or is not inspectable — callers must treat that
/// as "identity cannot be confirmed", never as "safe to signal".
pub fn inspect(pid: i32) -> Option<ProcInfo> {
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let filled = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if filled != size {
        return None;
    }
    Some(ProcInfo {
        start_tvsec: info.pbi_start_tvsec,
        start_tvusec: info.pbi_start_tvusec,
        ppid: info.pbi_ppid as i32,
        is_zombie: info.pbi_status == SZOMB,
    })
}

/// Record the identity of a freshly spawned child.
pub fn identity_of(pid: i32) -> Option<ProcIdentity> {
    inspect(pid).map(|i| ProcIdentity {
        pid,
        start_tvsec: i.start_tvsec,
        start_tvusec: i.start_tvusec,
    })
}

/// The pid-reuse guard: true only if the pid exists *and* its start_time
/// matches the recorded identity.
pub fn identity_matches(id: &ProcIdentity) -> bool {
    match inspect(id.pid) {
        Some(info) => info.start_tvsec == id.start_tvsec && info.start_tvusec == id.start_tvusec,
        None => false,
    }
}

/// Three-way liveness verdict for the `failed` classification (spec 01).
///
/// `proc_pidinfo` cannot see zombies at all (returns 0 bytes — verified on
/// this OS 2026-08-16), so `inspect` alone cannot distinguish "exited
/// unreaped" from "gone". `sysctl KERN_PROC_PID` still returns a
/// `kinfo_proc` for a zombie with `p_stat == SZOMB`, and returns nothing
/// once the pid is reaped/gone — that is the tiebreaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidState {
    /// No such process (or reaped): `sysctl` has nothing for the pid.
    Gone,
    /// Exited but unreaped.
    Zombie,
    /// Alive and inspectable.
    Live(ProcInfo),
    /// Exists per `sysctl` yet uninspectable via `proc_pidinfo` — a
    /// transient window (exec, teardown). Never provable death.
    Uninspectable,
}

/// Byte offset of `extern_proc.p_stat` inside a `kinfo_proc` buffer on
/// LP64 macOS: `p_un` union (2 ptrs, 16) + `p_vmspace` (8) + `p_sigacts`
/// (8) + `p_flag` (4). libc does not export `kinfo_proc` for this target;
/// the layout is pinned by `pid_state_sees_zombies_and_gone_pids`
/// (verified against a real zombie on this OS, 2026-08-16).
const KINFO_P_STAT_OFFSET: usize = 36;

pub fn pid_state(pid: i32) -> PidState {
    if let Some(info) = inspect(pid) {
        return PidState::Live(info);
    }
    let mut mib: [libc::c_int; 4] = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut buf = [0u8; 1024];
    let mut size = buf.len() as libc::size_t;
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r == 0 && size > KINFO_P_STAT_OFFSET {
        if u32::from(buf[KINFO_P_STAT_OFFSET]) == SZOMB {
            PidState::Zombie
        } else {
            PidState::Uninspectable
        }
    } else {
        PidState::Gone
    }
}

/// Cumulative CPU time (user + system) consumed by the process, in opaque
/// units — only deltas are meaningful (zero delta across a sample interval
/// = "no CPU", the `unresponsive` evidence input, spec 01). `None` when the
/// process is gone or uninspectable.
pub fn cpu_time(pid: i32) -> Option<u64> {
    let mut info: libc::proc_taskinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let filled = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if filled != size {
        return None;
    }
    Some(info.pti_total_user + info.pti_total_system)
}

/// Resident memory held by the process right now, in bytes (Stage L).
///
/// The aggregate-resource fact the rail states is built from these: at N
/// concurrent sessions the ceiling is not eave's own footprint (engine ~8
/// MB at N=10) but the agents', and a real `claude` runs 200–310 MB each.
///
/// Two honest limits, stated wherever this is rendered rather than
/// papered over:
/// - This is the **leader** only. Tool children (a `cargo build`, a test
///   run) are the agent's memory in every sense that matters to the
///   machine, and are not counted here — a bounded descendant walk per
///   snapshot is the price of counting them, and v0 does not pay it.
/// - Resident size double-counts shared pages across processes, so a sum
///   over sessions is an upper bound on physical pressure, not an exact
///   figure.
///
/// `None` when the process is gone or uninspectable — never a fake zero,
/// which would read as "this agent costs nothing".
pub fn resident_bytes(pid: i32) -> Option<u64> {
    let mut info: libc::proc_taskinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let filled = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if filled != size {
        return None;
    }
    Some(info.pti_resident_size)
}

/// The direct children of a pid.
///
/// Added for Slice C1, where the thing being measured is a *tree*: CEF
/// spawns five or six helper processes and the page's real cost is spread
/// across them, so the leader-only rule [`resident_bytes`] states for agents
/// would report a browser as nearly free.
///
/// Direct children only, one level deep. Chromium's helpers are all direct
/// children of the process that initialised CEF, so one level is the whole
/// tree *for this caller* — it is not a general descendant walk, and the
/// note on [`resident_bytes`] about unbounded walks still stands for the
/// agent case.
///
/// `None` if the kernel would not say; an empty vector means it said, and
/// the answer was none.
pub fn child_pids(of: u32) -> Option<Vec<i32>> {
    // Ask for the size first: the process table changes between calls, so a
    // fixed buffer would silently truncate on a busy machine.
    let needed = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return None;
    }
    // Headroom for processes started between the two calls.
    let capacity = (needed as usize / mem::size_of::<i32>()) + 64;
    let mut pids = vec![0i32; capacity];
    let bytes = unsafe {
        libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr() as *mut libc::c_void,
            (capacity * mem::size_of::<i32>()) as libc::c_int,
        )
    };
    if bytes <= 0 {
        return None;
    }
    pids.truncate(bytes as usize / mem::size_of::<i32>());
    Some(
        pids.into_iter()
            .filter(|pid| *pid > 0)
            .filter(|pid| inspect(*pid).is_some_and(|i| i.ppid == of as i32))
            .collect(),
    )
}

/// Physical memory installed in the machine, in bytes — the denominator
/// of the aggregate resource fact (Stage L). `None` if the sysctl fails,
/// in which case the fact states the sum alone and no ratio: a made-up
/// denominator would be worse than a missing one.
pub fn physical_memory() -> Option<u64> {
    let mut out: u64 = 0;
    let mut len = mem::size_of::<u64>();
    let name = c"hw.memsize";
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut out as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && out > 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_state_sees_zombies_and_gone_pids() {
        // A child we deliberately do not reap is a zombie; after reaping,
        // the pid is gone. This is the OS-facts input to `failed`.
        let child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn true");
        let pid = child.id() as i32;
        // Wait for it to exit without reaping it (no wait() call).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match pid_state(pid) {
                PidState::Zombie => break,
                _ if std::time::Instant::now() > deadline => {
                    panic!("child never became a zombie")
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        assert!(inspect(pid).is_none(), "zombies are proc_pidinfo-invisible");
        let mut child = child;
        child.wait().expect("reap");
        assert_eq!(pid_state(pid), PidState::Gone);
    }

    /// The aggregate resource fact is only worth rendering if the number
    /// under it is real. Our own process holds a non-trivial resident set
    /// and a reaped pid holds none — `None`, never a fake zero.
    #[test]
    fn resident_bytes_reads_a_live_process_and_refuses_a_dead_one() {
        let pid = std::process::id() as i32;
        let rss = resident_bytes(pid).expect("own resident size");
        assert!(
            rss > 1_000_000,
            "a running test binary holds more than a megabyte; got {rss}"
        );
        let mut child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn true");
        let dead = child.id() as i32;
        child.wait().expect("reap");
        assert_eq!(
            resident_bytes(dead),
            None,
            "a reaped pid must report nothing — a zero would render as \
             'this agent costs nothing'"
        );
    }

    /// The denominator. If this ever returns `None` on a real Mac the
    /// fact degrades to a bare sum, which is the intended fallback.
    #[test]
    fn physical_memory_is_a_plausible_machine_size() {
        let bytes = physical_memory().expect("hw.memsize on macOS");
        assert!(
            bytes >= 1 << 30,
            "at least a gigabyte installed; got {bytes}"
        );
    }

    #[test]
    fn cpu_time_reads_own_process_and_advances() {
        let pid = std::process::id() as i32;
        let a = cpu_time(pid).expect("own cpu time");
        // Burn a little CPU so the counter provably moves.
        let mut x: u64 = 0;
        for i in 0..20_000_000u64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(i);
        }
        std::hint::black_box(x);
        let b = cpu_time(pid).expect("own cpu time again");
        assert!(b > a, "cpu counter advanced: {a} -> {b}");
    }
}
