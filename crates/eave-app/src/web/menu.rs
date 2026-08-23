//! The page's context menu: Chromium decides what is under the pointer,
//! eave decides what the menu says and draws it.
//!
//! # Why there is anything to do here at all
//! A windowless browser has no window, so **CEF cannot show a menu**. In
//! windowed mode `CefContextMenuHandler` is an optional customisation hook
//! over a menu the framework puts up itself; in OSR it is the whole
//! implementation, and a client that does not supply one gets a right-click
//! that does nothing at all. That was eave until this slice — and not only
//! because the handler was missing: `page.rs` forwarded LEFT mouse buttons
//! and no others, so a right-click never reached Chromium in the first
//! place. Two absences, one symptom, and the second one hid the first.
//!
//! # The decision: take the params, decline the model
//! `RunContextMenu` is answered with `1` — *we are handling it* — and the
//! callback is **cancelled on the same stack**, before returning. The menu
//! that then appears is eave's own, built from
//! [`ContextMenuParams`](cef::ContextMenuParams) and driven by ordinary
//! browser calls.
//!
//! The alternative — hold CEF's callback across frames and answer it with a
//! command id when a row is clicked — is the shape cefclient uses, and it
//! was rejected for two reasons. It puts a live Chromium callback in the
//! shell's state for as long as a menu is open, which is a lifetime this
//! process has already been bitten by (`with_browser`'s note). And it buys
//! nothing: of the six rows eave shows, `Open Link in New Tab` and
//! `Search for…` have no CEF command id, so the custom path is needed
//! anyway and holding the callback would mean maintaining both.
//!
//! What is NOT rebuilt is anything Chromium knows and eave does not:
//! *what* was clicked. Whether there is a link, whether there is a
//! selection and what it says, whether the field is editable — all of it
//! comes from the params, because all of it is a fact about a DOM this
//! process cannot see.
//!
//! # The anchor is in window space by the time it leaves here
//! `params.xcoord/ycoord` are page-space, and the surface that draws the
//! menu is mounted at the window root (`overlay::menu_layer`). The origin
//! the page element published at paint (`osr::View`) is the whole
//! conversion — the same one `page.rs::to_page` does in the other
//! direction, and converting it here rather than at the draw site means
//! there is one place that knows and not two that can disagree.

// `wrap_context_menu_handler!` expands to trait names that do not appear
// in this source (`WrapContextMenuHandler`, `ImplContextMenuHandler`) and
// names `ContextMenuHandler` as its return type. The glob is what the macro
// requires — the same reason `tabs.rs` and `osr.rs` carry one.
use cef::*;

use super::PageMenu;

fn string_of(s: cef::CefStringUserfree) -> Option<String> {
    let text = cef::CefString::from(&s).to_string();
    (!text.is_empty()).then_some(text)
}

cef::wrap_context_menu_handler! {
    pub struct PageContextMenuHandler {
        tab_id: String,
    }

    impl ContextMenuHandler {
        /// Chromium wants a menu shown. There is no window to show one in,
        /// so this records what was clicked and answers `1`.
        fn run_context_menu(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            params: Option<&mut ContextMenuParams>,
            _model: Option<&mut MenuModel>,
            callback: Option<&mut RunContextMenuCallback>,
        ) -> i32 {
            let Some(params) = params else {
                // No params is no menu. Declining outright is better than
                // an empty one at a point we would have to invent.
                return 0;
            };

            // Page space -> window space, using the origin the element
            // published at its last paint. See the module note.
            let view = super::osr::view(&self.tab_id);
            let menu = PageMenu {
                tab_id: self.tab_id.clone(),
                x: params.xcoord() as f32 + view.origin_x,
                y: params.ycoord() as f32 + view.origin_y,
                link_url: string_of(params.link_url()),
                selection: string_of(params.selection_text()),
                editable: params.is_editable() == 1,
            };

            if super::host::traced() {
                eprintln!(
                    "[web] context menu at ({}, {}) link={:?} selection={:?}",
                    menu.x,
                    menu.y,
                    menu.link_url.is_some(),
                    menu.selection.is_some()
                );
            }

            super::request_menu(menu);

            // **Cancelled here, on this stack, and that is deliberate.**
            // Answering `1` means eave has taken responsibility for the
            // menu; the callback is CEF's way of being told which of ITS
            // command ids was chosen, and eave chooses none of them. Not
            // cancelling would leave Chromium believing a menu is up.
            if let Some(callback) = callback {
                callback.cancel();
            }

            // The menu is chrome, and no page frame is coming to carry it
            // onto the screen — a right-click on a still page repaints
            // nothing. See `host::note_chrome_change`.
            super::host::note_chrome_change();
            1
        }
    }
}
