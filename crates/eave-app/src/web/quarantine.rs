//! `com.apple.quarantine` on a finished download (G4).
//!
//! # RETRACTION: the gap this was built for did not exist
//! **G4's premise was wrong, and a live download proved it.** The spec row
//! said "nothing sets `com.apple.quarantine`", and the report built on it
//! said eave was handing over unsigned binaries with less friction than
//! every other browser on the machine. Both were arrived at by **grepping
//! eave's own source**, which answers a different question.
//!
//! Measured on 2026-08-23, downloading through the built bundle: the
//! finished file carries
//!
//! ```text
//! 0281;6a8b2bc0;;8BF670FC-7A2C-45C7-A3EB-FAD2FE160F0B
//! ```
//!
//! eave hard-codes `0083`, the agent `eave`, and an **empty** UUID, so that
//! value cannot have come from this module. **Chromium quarantines its own
//! completed downloads**, and it writes without `XATTR_CREATE`, so it
//! overwrites whatever was there. This is the same mistake §4 G of
//! `06-browser.md` was written to name — an inventory of *eave's* code
//! reporting an absence that Chromium was quietly filling — repeated one
//! section later, in a row marked verified.
//!
//! # What this module is still for, stated honestly
//! Two things, both smaller than the claim it was built on:
//!
//! 1. **Chromium marks at completion; this marks as soon as the partial
//!    file exists.** The trace shows this module's `setxattr` succeeding on
//!    the in-progress file, which means at that instant Chromium had not
//!    marked it — so there is a real window, and eave now closes it. It is
//!    seconds on a large download, not the "no protection at all" the
//!    original justification claimed.
//! 2. It costs one refused syscall per download update thereafter.
//!
//! # The gap that IS real, and is not fixed here
//! **The agent field in Chromium's value is EMPTY.** Chrome writes
//! `Chrome`, Safari writes `Safari`; a download through eave names nobody,
//! so the Finder's "Where did this come from?" sheet cannot say eave.
//!
//! Fixing that means writing *after* Chromium — i.e. on `done`, and
//! **without** `XATTR_CREATE`, which is precisely the overwrite this
//! module refuses to be able to do. That trade is real and unresolved: the
//! rule "nothing here can ever clear a quarantine somebody else set" is
//! worth more than eave's name in a sheet, so it stands until somebody
//! finds a way to have both.
//!
//! # The value, read off this machine rather than guessed
//! `<flags>;<hex unix seconds>;<agent>;<uuid>` — four fields, semicolon
//! separated. Real values from `~/Downloads` on the machine this was
//! written on:
//!
//! ```text
//! 0083;61f17ed9;Chrome;983D0344-B6B1-4790-B3A6-FF0FE542DA40
//! 0083;613f6c16;Safari;056DA32E-0B45-413B-8D9F-EF443DD846B1
//! 0081;678df797;Arc;
//! ```
//!
//! eave writes Chrome's and Safari's flags, `0083`.
//!
//! **The UUID field is left empty, deliberately.** It is a key into
//! `~/Library/Preferences/com.apple.LaunchServices.QuarantineEventsV2`,
//! where the origin URL of the download is recorded — and eave does not
//! write to that database. A UUID pointing at a row that does not exist is
//! worse than none: it promises provenance the "Where did this come from?"
//! sheet cannot then produce. Arc, a shipping Chromium browser, leaves it
//! empty for the same reason and Gatekeeper is unbothered.
//!
//! # When it is set: as early as the file exists, not at completion
//! The attribute goes on as soon as the partial file is on disk. That is
//! the one thing this module adds over Chromium's own marking, which
//! happens at completion — see the retraction above.
//!
//! `XATTR_CREATE` means the first write wins and later ones fail
//! harmlessly with `EEXIST`. That is the direction to fail in: this code
//! must never be able to *remove* or overwrite a quarantine somebody or
//! something else already set.

use std::path::Path;

/// What eave calls itself in the attribute. Shows up in the Finder's
/// "downloaded from" sheet, so it is a name a person should recognise.
pub const AGENT: &str = "Eave";

/// Flags: what Chrome and Safari both write on this platform.
///
/// Note that Chromium's own marking of the finished file uses `0281`, not
/// this — so a completed eave download ends up carrying Chromium's value,
/// not the one below. See the retraction in the module docs.
const FLAGS: &str = "0083";

/// The attribute's value for a download taken at `secs` (Unix seconds).
///
/// Split out from the syscall so the format is a pure function that the
/// suite can assert on — the syscall is trivially right or wrong, and the
/// string is where a browser gets this quietly wrong and finds out months
/// later.
pub fn value(secs: u64) -> String {
    format!("{FLAGS};{secs:x};{AGENT};")
}

/// Seconds since the epoch, or 0 if the clock is before it.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Put the file under Gatekeeper's eye.
///
/// Returns whether this call is what set it. `false` covers both "already
/// quarantined" and "could not" — neither is worth interrupting a download
/// over, and the caller only uses it to avoid tracing the same file twice.
///
/// **Never clears and never overwrites**, by construction: `XATTR_CREATE`
/// refuses when the attribute is already there.
pub fn mark(path: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let name = c"com.apple.quarantine";
    let value = value(now_secs());
    // SAFETY: both pointers are valid for the length passed and outlive the
    // call. `position` is 0 and meaningless for anything but resource
    // forks; `XATTR_CREATE` makes an existing attribute an error rather
    // than an overwrite.
    let rc = unsafe {
        libc::setxattr(
            c_path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            libc::XATTR_CREATE,
        )
    };
    rc == 0
}

/// What the attribute says, if it is there. For the suite, and for anyone
/// diagnosing a download that Gatekeeper let through.
pub fn read(path: &Path) -> Option<String> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let name = c"com.apple.quarantine";
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for `buf.len()` bytes and outlives the
    // call; a negative return means "no such attribute" and is handled.
    let len = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            0,
        )
    };
    if len <= 0 {
        return None;
    }
    String::from_utf8(buf[..len as usize].to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("eave-quarantine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"#!/bin/sh\necho hi\n").expect("write");
        path
    }

    /// The shape, field by field, against the real values in this module's
    /// docs. A browser gets this wrong quietly and finds out months later,
    /// which is exactly what a test is for.
    #[test]
    fn the_value_has_the_four_fields_gatekeeper_expects() {
        let v = value(0x61f1_7ed9);
        assert_eq!(v, "0083;61f17ed9;Eave;");
        let fields: Vec<&str> = v.split(';').collect();
        assert_eq!(fields.len(), 4, "{v}");
        assert_eq!(fields[0], "0083", "the flags Chrome and Safari both write");
        assert_eq!(
            u64::from_str_radix(fields[1], 16).unwrap(),
            0x61f1_7ed9,
            "the timestamp is hex seconds, not decimal and not millis"
        );
        assert_eq!(fields[2], "Eave", "the name a person reads in the sheet");
        assert!(
            fields[3].is_empty(),
            "a uuid here promises a database row eave does not write"
        );
    }

    /// The real syscall against a real file. Not a mock: the whole claim of
    /// this module is about what is on the disk afterwards, and a fake
    /// would assert only that eave agrees with itself.
    #[test]
    fn marking_a_file_puts_the_attribute_where_gatekeeper_looks() {
        let path = scratch("payload.sh");
        assert!(
            read(&path).is_none(),
            "a fresh file was already quarantined"
        );
        assert!(mark(&path), "setxattr refused");
        let got = read(&path).expect("no quarantine attribute after marking");
        assert!(got.starts_with("0083;"), "{got}");
        assert!(got.contains(";Eave;"), "{got}");
        let _ = std::fs::remove_file(&path);
    }

    /// The direction this must fail in: a second call never overwrites and
    /// never clears. A partial file is marked while it downloads and the
    /// completion pass must not disturb it — nor must anything here be able
    /// to lift a quarantine somebody else set.
    #[test]
    fn marking_twice_leaves_the_first_attribute_alone() {
        let path = scratch("twice.sh");
        assert!(mark(&path));
        let first = read(&path).expect("first mark");
        assert!(!mark(&path), "the second mark reported that it wrote");
        assert_eq!(read(&path).as_deref(), Some(first.as_str()));
        let _ = std::fs::remove_file(&path);
    }

    /// A path that is not there is a `false`, not a panic. A download can
    /// be cancelled between the update and this call.
    #[test]
    fn a_missing_file_is_refused_quietly() {
        let path = std::env::temp_dir().join("eave-quarantine-nope/never.bin");
        assert!(!mark(&path));
        assert!(read(&path).is_none());
    }
}
