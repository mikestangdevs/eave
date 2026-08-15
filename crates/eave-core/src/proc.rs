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
#[derive(Debug, Clone, Copy)]
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
        Some(info) => {
            info.start_tvsec == id.start_tvsec && info.start_tvusec == id.start_tvusec
        }
        None => false,
    }
}
