//! Widevine's CDM, and when it arrives (Slice BR4).
//!
//! The CDM is not shipped in the CEF distribution; Chromium's component
//! updater downloads it into `<root_cache_path>/WidevineCdm/<version>/`
//! on its own. **That already happens in eave with no code at all** —
//! nine of this machine's eave data dirs held the 20 MB
//! `libwidevinecdm.dylib` on 2026-09-03, every one installed by Chromium
//! from inside an ordinary eave run. The codecs report's caveat 3 said the
//! opposite ("eave never calls `CefComponentUpdater::Update`" as the
//! reason `WidevineCdm/` was empty): what it measured was Chromium's own
//! start-up delay, which is ten to fifteen minutes, on probe sessions that
//! were minutes long. Nothing eave sets blocks the updater.
//!
//! What this module adds is the *first* ten minutes: a foreground update
//! check shortly after `CefInitialize`, so a page that asks for
//! `com.widevine.alpha` in the first session on a fresh data dir does not
//! get "No DRM" and try again tomorrow. When the CDM is present the check
//! answers up-to-date and costs one request. The result is traced
//! (`EAVE_WEB_TRACE=1`, `[web] cdm …`) because the next question — does a
//! CDM that is present also *load*, given Widevine's host verification —
//! is answered by a page, not by this call.
//!
//! `CefComponentUpdater` is `CEF_API_ADDED(14600)`, UI-thread only, and
//! documented to "return safe defaults" when the service is unavailable,
//! which is what a `None` here means.
//!
//! **Not at `CefInitialize`.** The instant it returns, the updater exists
//! and answers `0 registered` (measured 2026-09-03): Chromium registers
//! its components from a task posted during start-up. So [`arm`] records
//! the moment and [`poll`], from the pump, asks again on a short backoff
//! until Widevine is listed or the budget is spent.

use cef::*;

/// Chromium's component id for the Widevine CDM — the same on every
/// platform and in Chrome itself (`chrome://components`).
pub const WIDEVINE_COMPONENT_ID: &str = "oimompecagnajdejgnnjijobebaeigek";

fn trace() -> bool {
    std::env::var_os("EAVE_WEB_TRACE").is_some()
}

fn state_word(state: ComponentState) -> &'static str {
    match state {
        ComponentState::NEW => "new",
        ComponentState::CHECKING => "checking",
        ComponentState::CAN_UPDATE => "can-update",
        ComponentState::DOWNLOADING => "downloading",
        ComponentState::DECOMPRESSING => "decompressing",
        ComponentState::PATCHING => "patching",
        ComponentState::UPDATING => "updating",
        ComponentState::UPDATED => "updated",
        ComponentState::UP_TO_DATE => "up-to-date",
        ComponentState::UPDATE_ERROR => "update-error",
        ComponentState::RUN => "run",
        _ => "other",
    }
}

fn error_word(error: ComponentUpdateError) -> &'static str {
    match error {
        ComponentUpdateError::NONE => "none",
        ComponentUpdateError::UPDATE_IN_PROGRESS => "update-in-progress",
        ComponentUpdateError::UPDATE_CANCELED => "update-canceled",
        ComponentUpdateError::RETRY_LATER => "retry-later",
        ComponentUpdateError::SERVICE_ERROR => "service-error",
        ComponentUpdateError::UPDATE_CHECK_ERROR => "update-check-error",
        ComponentUpdateError::CRX_NOT_FOUND => "crx-not-found",
        ComponentUpdateError::INVALID_ARGUMENT => "invalid-argument",
        ComponentUpdateError::BAD_CRX_DATA_CALLBACK => "bad-crx-data-callback",
        _ => "other",
    }
}

wrap_component_update_callback! {
    struct TraceComplete {}

    impl ComponentUpdateCallback {
        fn on_complete(&self, component_id: Option<&CefString>, error: ComponentUpdateError) {
            if trace() {
                let id = component_id
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                eprintln!("[web] cdm update complete id={id} error={}", error_word(error));
            }
        }
    }
}

/// Tries are spaced this far apart, and there are this many. Twelve at
/// five seconds is a minute: long past the start-up task, short of
/// Chromium's own ten-minute first check.
const POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
const POLL_TRIES: u32 = 12;

thread_local! {
    /// `(next attempt, attempts left)`; `None` when armed-and-done or
    /// never armed. Main thread only, like the pump that reads it.
    static POLL: std::cell::Cell<Option<(std::time::Instant, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// Arm the first check. Call once, on the UI thread, after
/// `CefInitialize` succeeded; the check itself runs from [`poll`].
pub fn arm() {
    POLL.with(|p| {
        p.set(Some((
            std::time::Instant::now() + std::time::Duration::from_secs(2),
            POLL_TRIES,
        )))
    });
}

/// From the pump: one attempt when it is due, nothing otherwise. Silent
/// unless traced.
pub fn poll() {
    let Some((due, left)) = POLL.with(std::cell::Cell::get) else {
        return;
    };
    let now = std::time::Instant::now();
    if now < due {
        return;
    }
    let done = try_update();
    POLL.with(|p| {
        p.set(if done || left <= 1 {
            None
        } else {
            Some((now + POLL_EVERY, left - 1))
        })
    });
    if !done && left <= 1 && trace() {
        eprintln!("[web] cdm: gave up waiting for widevine to be registered");
    }
}

/// One foreground update check for Widevine. `true` when the request was
/// made (or cannot ever be); `false` when the component is not listed yet.
fn try_update() -> bool {
    let Some(updater) = component_updater_get() else {
        if trace() {
            eprintln!("[web] cdm: no component updater service");
        }
        return true;
    };
    let id = CefString::from(WIDEVINE_COMPONENT_ID);
    let Some(component) = updater.component_by_id(Some(&id)) else {
        if trace() {
            eprintln!(
                "[web] cdm: widevine is not a registered component yet ({} registered)",
                updater.component_count()
            );
        }
        return false;
    };
    if trace() {
        eprintln!(
            "[web] cdm: widevine version={} state={}",
            CefStringUtf16::from(&component.version()).to_string(),
            state_word(component.state())
        );
    }
    let mut callback = TraceComplete::new();
    updater.update(Some(&id), ComponentUpdatePriority::FOREGROUND, Some(&mut callback));
    true
}
