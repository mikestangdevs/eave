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
    // Must be Some: Chromium's network/renderer helpers will not know the
    // `eave:` scheme unless OnRegisterCustomSchemes runs in THIS process.
    // None here made `eave://pdf` die in NetworkContext after a correct
    // rewrite (blank PDF tab). See `web/pdf.rs` `scheme_app`.
    let mut app = eave_app::web::scheme_app();
    let ret = cef::execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    std::process::exit(ret.max(0));
}
