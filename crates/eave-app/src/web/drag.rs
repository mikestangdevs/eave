//! Drag & drop as the chrome and the page share it (06-browser A5).
//!
//! # Why this file has no CEF in it
//! Same bargain as [`super::ime`] and [`super::selection`]: what is here is
//! a map and the rules for updating it, so the suite that runs by default
//! exercises all of it. The gated half is the four `drag_target_*` calls
//! and `RenderHandler::start_dragging`, which only mean anything against a
//! live browser.
//!
//! # Two directions, one file
//! **In** is a file (or a set of files) arriving from the OS. gpui has
//! already translated Finder's drop into an [`gpui::ExternalPaths`] drag;
//! this module only remembers which page the pointer is over so we send
//! `drag_enter` once, `drag_over` while it stays, and `drag_leave` when it
//! goes, rather than enter-on-every-move.
//!
//! **Out** is Chromium telling us a drag began inside a page — a link, an
//! image, a file, a selected fragment. `start_dragging` is the callback
//! and it fires *after* the gesture is underway, which is why eave cannot
//! use gpui's `on_drag` for this: that API starts a drag from a mouse-down
//! *on the element*, and putting it on the page would steal text selection.
//! We take the drag over (`start_dragging` returns 1) and drive it with
//! the mouse events the page already forwards.
//!
//! # What this is NOT
//! Dragging *out of the window onto the desktop* is a native
//! `NSDraggingSession`. This gpui fork receives file drops (`draggingEntered:`)
//! and does not start them; `cx.active_drag` is `pub(crate)`, so there is
//! no imperative "start a drag now" either. Page↔page and OS→page are
//! wired. Page→OS is the recorded limit, the same shape as A4's
//! `preventDefault()` on ⌘C.
//!
//! Returning 1 from `start_dragging` and then never calling
//! `drag_source_ended_at` / `drag_source_system_drag_ended` hangs the
//! renderer. [`end_out`] and [`flush_out_cancel`] exist so a forgotten
//! tab, a mouse-up on chrome, or a closed page cannot take that path.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

/// One step of a pointer moving across a drop target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InEvent {
    /// First frame the pointer is over this page.
    Enter,
    /// Still over, and it moved.
    Over,
    /// Was over, now is not.
    Leave,
    /// Not over, and was not over. Silence.
    Quiet,
}

/// What Chromium said the page is dragging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutKind {
    Link {
        url: String,
        title: String,
    },
    Files {
        paths: Vec<String>,
    },
    Fragment {
        text: String,
        html: String,
    },
    /// An image-only drag, or anything we do not project. The gated half
    /// still holds the original `DragData` and can feed it to a target.
    Other,
}

#[derive(Debug, Clone)]
struct OutState {
    source: String,
    over: Option<String>,
    kind: OutKind,
}

thread_local! {
    static IN_OVER: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
    static OUT: RefCell<Option<OutState>> = const { RefCell::new(None) };
    static PENDING_OUT_CANCEL: Cell<bool> = const { Cell::new(false) };
    static EAT_MOUSE_UP: Cell<bool> = const { Cell::new(false) };
}

fn step(was: bool, now: bool) -> InEvent {
    match (was, now) {
        (false, true) => InEvent::Enter,
        (true, true) => InEvent::Over,
        (true, false) => InEvent::Leave,
        (false, false) => InEvent::Quiet,
    }
}

/// Classify what `start_dragging` handed us, from the flags and strings
/// already copied out of CEF. Priority is file, then link, then fragment:
/// a downloadable image is a file even when it also has a URL.
pub fn out_kind(
    is_file: bool,
    paths: Vec<String>,
    is_link: bool,
    url: Option<String>,
    title: Option<String>,
    is_fragment: bool,
    text: Option<String>,
    html: Option<String>,
) -> OutKind {
    if is_file && !paths.is_empty() {
        return OutKind::Files { paths };
    }
    if is_link {
        if let Some(url) = url.filter(|u| !u.is_empty()) {
            return OutKind::Link {
                title: title.unwrap_or_default(),
                url,
            };
        }
    }
    if is_fragment
        && (text.as_ref().is_some_and(|s| !s.is_empty())
            || html.as_ref().is_some_and(|s| !s.is_empty()))
    {
        return OutKind::Fragment {
            text: text.unwrap_or_default(),
            html: html.unwrap_or_default(),
        };
    }
    OutKind::Other
}

/// The pointer moved over or off this page during an OS file drag.
pub fn note_in_over(tab_id: &str, is_over: bool) -> InEvent {
    IN_OVER.with(|m| {
        let mut m = m.borrow_mut();
        let was = m.get(tab_id).copied().unwrap_or(false);
        if is_over {
            m.insert(tab_id.to_string(), true);
        } else {
            m.remove(tab_id);
        }
        step(was, is_over)
    })
}

/// Is this page currently the in-drag target?
pub fn in_is_over(tab_id: &str) -> bool {
    IN_OVER.with(|m| m.borrow().get(tab_id).copied().unwrap_or(false))
}

/// The OS drag ended. Returns every tab that still thought it was over, so
/// the caller can send `drag_leave` to each.
pub fn end_in() -> Vec<String> {
    IN_OVER.with(|m| {
        m.borrow_mut()
            .drain()
            .filter_map(|(id, over)| over.then_some(id))
            .collect()
    })
}

/// Chromium has started a drag in this tab. Replaces any in-flight out-drag
/// rather than stacking: CEF will not start a second one without ending the
/// first, and a leftover would hang the previous renderer.
pub fn begin_out(source: &str, kind: OutKind) {
    OUT.with(|o| {
        *o.borrow_mut() = Some(OutState {
            source: source.to_string(),
            over: None,
            kind,
        });
    });
    PENDING_OUT_CANCEL.set(false);
}

pub fn out_is_live() -> bool {
    OUT.with(|o| o.borrow().is_some())
}

pub fn out_source() -> Option<String> {
    OUT.with(|o| o.borrow().as_ref().map(|s| s.source.clone()))
}

pub fn out_kind_in_flight() -> Option<OutKind> {
    OUT.with(|o| o.borrow().as_ref().map(|s| s.kind.clone()))
}

/// The pointer moved over or off `tab_id` during an out-drag.
///
/// Switching pages yields a Leave on the previous target *and* an Enter on
/// the new one, in that order — the caller sends both, and sending Enter
/// without Leave would leave Chromium thinking two pages were targets.
pub fn note_out_over(tab_id: &str, is_over: bool) -> Vec<(String, InEvent)> {
    OUT.with(|o| {
        let mut o = o.borrow_mut();
        let Some(st) = o.as_mut() else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if is_over {
            match st.over.as_deref() {
                Some(prev) if prev == tab_id => {
                    events.push((tab_id.to_string(), InEvent::Over));
                }
                Some(_) => {
                    let prev = st.over.take().expect("matched Some");
                    events.push((prev, InEvent::Leave));
                    st.over = Some(tab_id.to_string());
                    events.push((tab_id.to_string(), InEvent::Enter));
                }
                None => {
                    st.over = Some(tab_id.to_string());
                    events.push((tab_id.to_string(), InEvent::Enter));
                }
            }
        } else if st.over.as_deref() == Some(tab_id) {
            st.over = None;
            events.push((tab_id.to_string(), InEvent::Leave));
        }
        events
    })
}

pub fn out_over() -> Option<String> {
    OUT.with(|o| o.borrow().as_ref().and_then(|s| s.over.clone()))
}

/// The out-drag is finished (dropped or cancelled). Clears the cell.
pub fn end_out() {
    OUT.with(|o| *o.borrow_mut() = None);
    PENDING_OUT_CANCEL.set(false);
}

/// Mouse-up landed off every page. Do not cancel yet: capture-phase
/// `mouse_up_out` on one pane fires *before* bubble-phase drop on the
/// other, and cancelling first would drop onto an ended drag. The next
/// paint flushes this if the drag is still live.
pub fn request_out_cancel() {
    if out_is_live() {
        PENDING_OUT_CANCEL.set(true);
    }
}

/// True when a cancel requested last event is still owed. Consumes the
/// flag. The caller must then [`end_out`] after telling the source.
pub fn flush_out_cancel() -> bool {
    PENDING_OUT_CANCEL.replace(false) && out_is_live()
}

/// A file drop's `MouseUp` must not also be forwarded as a page click.
pub fn eat_next_mouse_up() {
    EAT_MOUSE_UP.set(true);
}

pub fn take_eat_mouse_up() -> bool {
    EAT_MOUSE_UP.replace(false)
}

/// A tab is gone. An in-flight out-drag *from* it is the hang path —
/// the caller still has to tell Chromium, then this forgets the chrome
/// half.
pub fn forget_tab(tab_id: &str) {
    IN_OVER.with(|m| {
        m.borrow_mut().remove(tab_id);
    });
    let source_died = OUT.with(|o| o.borrow().as_ref().is_some_and(|s| s.source == tab_id));
    if source_died {
        end_out();
    } else {
        let _ = note_out_over(tab_id, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset() {
        end_in();
        end_out();
        let _ = take_eat_mouse_up();
        PENDING_OUT_CANCEL.set(false);
        forget_tab("a");
        forget_tab("b");
        forget_tab("c");
    }

    #[test]
    fn a_page_the_pointer_has_not_entered_is_quiet() {
        reset();
        assert_eq!(note_in_over("a", false), InEvent::Quiet);
        assert!(!in_is_over("a"));
    }

    #[test]
    fn crossing_onto_a_page_is_enter_then_over() {
        reset();
        assert_eq!(note_in_over("a", true), InEvent::Enter);
        assert!(in_is_over("a"));
        assert_eq!(note_in_over("a", true), InEvent::Over);
        assert_eq!(note_in_over("a", true), InEvent::Over);
    }

    #[test]
    fn leaving_a_page_is_leave_and_then_quiet() {
        reset();
        assert_eq!(note_in_over("a", true), InEvent::Enter);
        assert_eq!(note_in_over("a", false), InEvent::Leave);
        assert!(!in_is_over("a"));
        assert_eq!(note_in_over("a", false), InEvent::Quiet);
    }

    #[test]
    fn two_pages_track_independently() {
        reset();
        assert_eq!(note_in_over("a", true), InEvent::Enter);
        assert_eq!(note_in_over("b", true), InEvent::Enter);
        assert!(in_is_over("a") && in_is_over("b"));
        assert_eq!(note_in_over("a", false), InEvent::Leave);
        assert!(!in_is_over("a") && in_is_over("b"));
    }

    #[test]
    fn ending_an_in_drag_reports_every_page_still_over() {
        reset();
        note_in_over("a", true);
        note_in_over("b", true);
        note_in_over("c", false);
        let mut left = end_in();
        left.sort();
        assert_eq!(left, vec!["a", "b"]);
        assert!(!in_is_over("a") && !in_is_over("b"));
        assert!(end_in().is_empty(), "a second end must not re-leave");
    }

    #[test]
    fn a_file_beats_a_link_when_both_are_present() {
        // An image on a page is often a file *and* a URL. The file is what
        // an upload zone and the desktop both want.
        assert_eq!(
            out_kind(
                true,
                vec!["/tmp/pic.png".into()],
                true,
                Some("https://ex/pic.png".into()),
                Some("pic".into()),
                false,
                None,
                None,
            ),
            OutKind::Files {
                paths: vec!["/tmp/pic.png".into()]
            }
        );
    }

    #[test]
    fn a_link_without_a_file_is_a_link() {
        assert_eq!(
            out_kind(
                false,
                vec![],
                true,
                Some("https://ex/".into()),
                Some("Example".into()),
                false,
                None,
                None,
            ),
            OutKind::Link {
                url: "https://ex/".into(),
                title: "Example".into(),
            }
        );
    }

    #[test]
    fn an_empty_link_url_is_not_a_link() {
        assert_eq!(
            out_kind(
                false,
                vec![],
                true,
                Some(String::new()),
                None,
                false,
                None,
                None
            ),
            OutKind::Other
        );
    }

    #[test]
    fn a_fragment_is_text_or_html() {
        assert_eq!(
            out_kind(
                false,
                vec![],
                false,
                None,
                None,
                true,
                Some("hello".into()),
                Some("<b>hello</b>".into()),
            ),
            OutKind::Fragment {
                text: "hello".into(),
                html: "<b>hello</b>".into(),
            }
        );
    }

    #[test]
    fn a_file_flag_with_no_paths_is_not_a_file() {
        // Chromium can say is_file without having given us a path yet
        // (contents-only). We still hold the DragData gated; the kind
        // is Other so we do not invent a path.
        assert_eq!(
            out_kind(true, vec![], false, None, None, false, None, None),
            OutKind::Other
        );
    }

    #[test]
    fn begin_out_replaces_rather_than_stacking() {
        reset();
        begin_out("a", OutKind::Other);
        begin_out(
            "b",
            OutKind::Link {
                url: "https://ex/".into(),
                title: String::new(),
            },
        );
        assert_eq!(out_source().as_deref(), Some("b"));
        assert!(!matches!(out_kind_in_flight(), Some(OutKind::Other)));
    }

    #[test]
    fn crossing_from_the_source_page_to_another_leaves_then_enters() {
        reset();
        begin_out("a", OutKind::Other);
        assert_eq!(note_out_over("a", true), vec![("a".into(), InEvent::Enter)]);
        assert_eq!(
            note_out_over("b", true),
            vec![("a".into(), InEvent::Leave), ("b".into(), InEvent::Enter),]
        );
        assert_eq!(out_over().as_deref(), Some("b"));
        assert_eq!(note_out_over("b", true), vec![("b".into(), InEvent::Over)]);
        assert_eq!(
            note_out_over("b", false),
            vec![("b".into(), InEvent::Leave)]
        );
        assert_eq!(out_over(), None);
    }

    #[test]
    fn leaving_a_page_we_were_not_over_is_silence() {
        reset();
        begin_out("a", OutKind::Other);
        assert!(note_out_over("b", false).is_empty());
    }

    #[test]
    fn note_out_over_with_no_drag_is_silence() {
        reset();
        assert!(note_out_over("a", true).is_empty());
    }

    #[test]
    fn a_cancel_requested_on_mouse_up_out_flushes_only_if_still_live() {
        reset();
        begin_out("a", OutKind::Other);
        request_out_cancel();
        // A drop on another pane ended it before paint.
        end_out();
        assert!(
            !flush_out_cancel(),
            "flushing after a drop would cancel a drag that already landed"
        );

        begin_out("a", OutKind::Other);
        request_out_cancel();
        assert!(flush_out_cancel());
        assert!(
            !flush_out_cancel(),
            "the flag is one-shot; a second paint must not re-cancel"
        );
    }

    #[test]
    fn request_out_cancel_is_a_no_op_when_nothing_is_dragging() {
        reset();
        request_out_cancel();
        assert!(!flush_out_cancel());
    }

    #[test]
    fn eat_mouse_up_is_one_shot() {
        reset();
        assert!(!take_eat_mouse_up());
        eat_next_mouse_up();
        assert!(take_eat_mouse_up());
        assert!(!take_eat_mouse_up());
    }

    #[test]
    fn forgetting_the_source_tab_ends_the_out_drag() {
        reset();
        begin_out("a", OutKind::Other);
        note_out_over("b", true);
        forget_tab("a");
        assert!(!out_is_live());
        assert_eq!(out_over(), None);
    }

    #[test]
    fn forgetting_a_target_tab_leaves_it_but_keeps_the_drag() {
        reset();
        begin_out("a", OutKind::Other);
        note_out_over("b", true);
        forget_tab("b");
        assert!(out_is_live());
        assert_eq!(out_source().as_deref(), Some("a"));
        assert_eq!(out_over(), None);
    }

    #[test]
    fn forgetting_an_in_over_tab_does_not_leave_the_others() {
        reset();
        note_in_over("a", true);
        note_in_over("b", true);
        forget_tab("a");
        assert!(!in_is_over("a"));
        assert!(in_is_over("b"));
    }
}
