//! Build-time stamps for the Info pane (Slice S1) — two facts the source
//! tree knows and the running binary otherwise could not state.
//!
//! - `EAVE_BUILD_SHA` — exported by `scripts/bundle-app.sh` before it runs
//!   cargo. Compiled into the Mach-O rather than written to the plist
//!   because this machine's bundle workflow RESTAMPS binaries into
//!   existing `.app`s (`EAVE_CURRENT` documents why); a plist stamp would
//!   survive a restamp as yesterday's sha. Absent (a plain `cargo build`,
//!   the test suite) the Info pane says so instead of guessing.
//! - `EAVE_GPUI_REV` — parsed from the workspace `Cargo.lock`, so the row
//!   can never drift from the pin actually built against. The manifests
//!   name the same rev twice ([dependencies] and the root `[patch]`); the
//!   lockfile is what cargo resolved, which is the fact worth showing.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=EAVE_BUILD_SHA");
    if let Ok(sha) = std::env::var("EAVE_BUILD_SHA") {
        if !sha.trim().is_empty() {
            println!("cargo:rustc-env=EAVE_BUILD_SHA={}", sha.trim());
        }
    }

    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let lock = Path::new(&manifest).join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    if let Some(rev) = std::fs::read_to_string(&lock)
        .ok()
        .and_then(|text| gpui_rev(&text))
    {
        println!("cargo:rustc-env=EAVE_GPUI_REV={rev}");
    }
}

/// The commit in gpui's `source = "git+…#<sha>"` lockfile line. Returns
/// `None` on anything unexpected — a missing stamp renders as "unknown",
/// which beats a build that fails over a display row.
fn gpui_rev(lock: &str) -> Option<String> {
    let mut in_gpui = false;
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            in_gpui = false;
        } else if line == "name = \"gpui\"" {
            in_gpui = true;
        } else if in_gpui {
            if let Some(rest) = line.strip_prefix("source = ") {
                let sha = rest.trim_matches('"').rsplit('#').next()?;
                if sha.len() >= 7 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Some(sha.to_string());
                }
                return None;
            }
        }
    }
    None
}
