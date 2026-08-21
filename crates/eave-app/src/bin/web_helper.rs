//! The CEF helper process.
//!
//! Chromium's macOS layout requires a separate executable for the renderer,
//! GPU, network and utility children. It cannot be the main binary: those
//! children must not be a gpui app, must not open a window, and must not
//! install an `NSApp` subclass. Making `eave` re-exec itself would mean
//! every one of those "must not"s became a branch in `main` that a future
//! edit could get wrong — so it is a different program instead.
//!
//! It does exactly one thing.

fn main() {
    let args = cef::args::Args::new();
    let ret = cef::execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
    std::process::exit(ret.max(0));
}
