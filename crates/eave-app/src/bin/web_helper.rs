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
    // The framework is `dlopen`ed, not linked, and every CEF entry point is
    // a null pointer until it is. `helper: true` because a helper sits three
    // directories deeper in the bundle than the main app does, so it
    // resolves the framework by a different relative path.
    let exe = std::env::current_exe().expect("current_exe");
    let loader = cef::library_loader::LibraryLoader::new(&exe, true);
    if !loader.load() {
        std::process::exit(1);
    }
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);

    let args = cef::args::Args::new();
    let ret = cef::execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
    std::process::exit(ret.max(0));
}
