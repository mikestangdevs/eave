//! IME composition: the state, and the decisions about it (06-browser A3).
//!
//! # What an IME is, and why a browser cannot skip it
//! An Input Method Editor is the machinery between a keypress and a
//! character for every language whose characters are not on the keyboard.
//! Typing `ka` in Japanese produces a provisional `か`, then a candidate
//! list, then a commit to `歌`; typing `⌥e e` in French produces `é`. The
//! provisional stage is the whole difference: text that is **on screen and
//! not yet in the document**, which the application must display, replace
//! as it changes, and finally commit or throw away.
//!
//! Without it eave is not merely awkward for those users — Japanese,
//! Chinese and Korean cannot be typed into a page **at all**, and a French
//! or German layout types the wrong letters, because
//! [`super::input::key`]'s table maps a key's POSITION to a US character.
//!
//! # Why this file has no CEF in it
//! Everything here is a decision about ranges and rectangles, and none of
//! it needs Chromium — so the default feature-OFF suite runs all of it, the
//! way `nav_policy` and `download_rows` already do. The gated half is four
//! calls in [`super::input`] and one callback in [`super::osr`].
//!
//! # The state is a MIRROR, and that is deliberate
//! Chromium owns the real composition; it lives in the renderer. What is
//! here is only what macOS asks eave for and Chromium will not volunteer:
//! is there a composition, how long is it, and **where is it on screen** so
//! the candidate window lands under the text instead of at the origin.
//! Chromium answers the last one through `OnImeCompositionRangeChanged`,
//! which is the only reason [`note_range`] exists.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;

/// One composing character's box, in the page's own logical pixels.
///
/// Page space and element space are the same space (`osr::screen_point`
/// answers identity for exactly this reason), so turning one of these into
/// a window rectangle is an addition and not a projection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// What eave believes about one tab's composition.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Composition {
    /// The marked (provisional) range in **UTF-16 units**, as eave last
    /// told Chromium. `None` means nothing is composing.
    ///
    /// UTF-16 because that is what `NSTextInputClient` counts in, and
    /// converting to bytes here would mean converting back at every
    /// question macOS asks.
    pub marked: Option<Range<usize>>,
    /// Where the caret sits inside the composition, when Chromium has said.
    pub selected: Option<Range<usize>>,
    /// One box per composing character, newest report wins.
    pub bounds: Vec<CharRect>,
}

thread_local! {
    static COMPOSITIONS: RefCell<HashMap<String, Composition>> =
        RefCell::new(HashMap::new());
}

/// eave has told Chromium to compose `len` UTF-16 units.
///
/// `len == 0` is not a composition — macOS sets an empty marked range to
/// mean "I am done", and treating that as live would leave every tab
/// permanently composing and swallow every subsequent keystroke.
pub fn set_marked(tab_id: &str, len: usize, selected: Option<Range<usize>>) {
    COMPOSITIONS.with(|c| {
        let mut map = c.borrow_mut();
        if len == 0 {
            map.remove(tab_id);
            return;
        }
        let entry = map.entry(tab_id.to_string()).or_default();
        entry.marked = Some(0..len);
        entry.selected = selected;
    });
}

/// The composition ended — committed, cancelled, or finished.
pub fn clear(tab_id: &str) {
    COMPOSITIONS.with(|c| {
        c.borrow_mut().remove(tab_id);
    });
}

/// **Is a composition live?** The question the key path asks.
///
/// While this is true the raw key path must stay out of the way: the
/// keystrokes belong to the IME, and sending them to Chromium as well
/// would type the romaji *and* compose the kana. Same shape as the find
/// bar's guard one line above it in `page.rs`, and the same defect if it
/// is missing.
pub fn is_composing(tab_id: &str) -> bool {
    COMPOSITIONS.with(|c| {
        c.borrow()
            .get(tab_id)
            .and_then(|k| k.marked.as_ref())
            .is_some_and(|m| !m.is_empty())
    })
}

/// Which CEF call actually inserts text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Insert {
    /// `ImeCommitText` — **completes an OPEN composition**, and does
    /// nothing at all without one.
    Commit,
    /// A synthesised `CHAR` key event — ordinary typing.
    Char,
}

/// **The decision that cost a live walk to find.**
///
/// `ImeCommitText` completes an existing composition. With none open,
/// Chromium has nothing to complete and the call is a **silent no-op** —
/// no error, no `beforeinput`, no text. The first cut of A3 routed every
/// character through it on the reasoning that one insertion path beats
/// two; it compiled, 798 tests passed, the input handler registered with
/// `focused=true`, and `replace_text_in_range` fired 72 times with the
/// right text while **nothing whatsoever reached the page**.
///
/// It is a pure function of one bool so that the default suite can hold
/// that finding, which no test could have produced on its own: every
/// layer the suite can see was green while the feature did nothing.
pub fn insert_kind(tab_id: &str) -> Insert {
    if is_composing(tab_id) {
        Insert::Commit
    } else {
        Insert::Char
    }
}

/// The marked range macOS asks for, in UTF-16 units.
pub fn marked_range(tab_id: &str) -> Option<Range<usize>> {
    COMPOSITIONS.with(|c| c.borrow().get(tab_id).and_then(|k| k.marked.clone()))
}

/// Where Chromium says the composition is — its answer to
/// `OnImeCompositionRangeChanged`.
pub fn note_range(tab_id: &str, selected: Option<Range<usize>>, bounds: Vec<CharRect>) {
    COMPOSITIONS.with(|c| {
        let mut map = c.borrow_mut();
        let entry = map.entry(tab_id.to_string()).or_default();
        entry.selected = selected;
        entry.bounds = bounds;
    });
}

/// Chromium's view of the selection, when it has offered one.
pub fn selected_range(tab_id: &str) -> Option<Range<usize>> {
    COMPOSITIONS.with(|c| c.borrow().get(tab_id).and_then(|k| k.selected.clone()))
}

/// The boxes Chromium last reported for this tab.
pub fn bounds(tab_id: &str) -> Vec<CharRect> {
    COMPOSITIONS.with(|c| {
        c.borrow()
            .get(tab_id)
            .map(|k| k.bounds.clone())
            .unwrap_or_default()
    })
}

/// A tab is gone.
pub fn forget_tab(tab_id: &str) {
    clear(tab_id);
}

/// **Where the candidate window goes**, for the UTF-16 range macOS names.
///
/// Pure, and the whole reason this file is testable: it is arithmetic over
/// a list of boxes.
///
/// Three cases, and the third is the one that matters. macOS asks about a
/// range **past the end** of the composition whenever it wants the caret
/// rather than the text — at the moment composition starts, and again
/// after every commit. Answering `None` there puts the candidate window at
/// the window origin, which is the classic "IME panel in the top-left
/// corner" bug. So a range past the end collapses to a caret at the
/// trailing edge of the last box eave knows about.
pub fn pick_rect(boxes: &[CharRect], range: &Range<usize>) -> Option<CharRect> {
    let last = boxes.last()?;
    if range.start >= boxes.len() {
        // The caret after the text, not the text.
        return Some(CharRect {
            x: last.x + last.width,
            width: 0.0,
            ..*last
        });
    }
    let end = range.end.min(boxes.len()).max(range.start + 1);
    let run = &boxes[range.start..end];
    // The union, clamped to the first line: `firstRectForCharacterRange`
    // wants one rectangle, and a composition that wrapped would otherwise
    // report a box spanning both lines with the panel floating between
    // them.
    let top = run[0].y;
    let same_line: Vec<&CharRect> = run.iter().take_while(|r| r.y == top).collect();
    let first = same_line.first()?;
    let x = first.x;
    let right = same_line
        .iter()
        .map(|r| r.x + r.width)
        .fold(f32::MIN, f32::max);
    let height = same_line.iter().map(|r| r.height).fold(0.0, f32::max);
    Some(CharRect {
        x,
        y: top,
        width: right - x,
        height,
    })
}

/// Page space to window space. An addition, because they are the same
/// space translated by where the element was laid out.
pub fn to_window(r: &CharRect, origin_x: f32, origin_y: f32) -> CharRect {
    CharRect {
        x: r.x + origin_x,
        y: r.y + origin_y,
        ..*r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxes() -> Vec<CharRect> {
        vec![
            CharRect { x: 10.0, y: 100.0, width: 8.0, height: 16.0 },
            CharRect { x: 18.0, y: 100.0, width: 8.0, height: 16.0 },
            CharRect { x: 26.0, y: 100.0, width: 8.0, height: 16.0 },
        ]
    }

    /// An empty marked range is macOS saying "done", not "composing".
    ///
    /// Getting this wrong leaves a tab permanently composing, and the key
    /// guard then swallows every keystroke for the life of the tab — a
    /// dead keyboard with nothing in any log.
    #[test]
    fn an_empty_marked_range_ends_the_composition() {
        set_marked("t", 3, None);
        assert!(is_composing("t"));
        set_marked("t", 0, None);
        assert!(!is_composing("t"), "an empty mark left the tab composing");
        assert_eq!(marked_range("t"), None);
    }

    /// **Ordinary typing must NOT go through `ImeCommitText`.**
    ///
    /// The regression guard for the defect a live walk found and the suite
    /// could not: with no composition open that call inserts nothing and
    /// reports nothing. If this assertion ever flips, typing into a page
    /// goes silently dead again.
    #[test]
    fn typing_with_no_composition_inserts_by_char_not_by_commit() {
        assert_eq!(insert_kind("fresh-tab"), Insert::Char);
    }

    /// …and a real commit, completing a real composition, does.
    #[test]
    fn committing_an_open_composition_goes_through_ime_commit() {
        set_marked("t-commit", 3, None);
        assert_eq!(insert_kind("t-commit"), Insert::Commit);
        clear("t-commit");
        assert_eq!(
            insert_kind("t-commit"),
            Insert::Char,
            "a finished composition still claimed the commit path"
        );
    }

    /// A tab that never composed is not composing.
    #[test]
    fn a_tab_with_no_composition_is_not_composing() {
        assert!(!is_composing("never-touched"));
        assert_eq!(marked_range("never-touched"), None);
    }

    /// Committing clears, so the next keystroke is a fresh start.
    #[test]
    fn clearing_ends_the_composition() {
        set_marked("t2", 2, Some(0..2));
        note_range("t2", Some(0..2), boxes());
        clear("t2");
        assert!(!is_composing("t2"));
        assert!(bounds("t2").is_empty(), "a cleared tab kept its boxes");
    }

    /// One character asks for that character's box.
    #[test]
    fn a_single_character_gets_its_own_box() {
        let r = pick_rect(&boxes(), &(1..2)).expect("a box");
        assert_eq!(r.x, 18.0);
        assert_eq!(r.width, 8.0);
    }

    /// A run gets the union of the run.
    #[test]
    fn a_run_gets_the_union_of_its_boxes() {
        let r = pick_rect(&boxes(), &(0..3)).expect("a box");
        assert_eq!(r.x, 10.0);
        assert_eq!(r.width, 24.0, "the union did not span all three");
    }

    /// **The case that puts the panel in the corner if it is missed.**
    ///
    /// macOS asks about the range AFTER the composition to place the caret
    /// — at the start of composing, and again after every commit. `None`
    /// here is what draws the candidate window at the window origin.
    #[test]
    fn a_range_past_the_end_collapses_to_the_caret() {
        let r = pick_rect(&boxes(), &(3..3)).expect("a caret, not None");
        assert_eq!(r.x, 34.0, "the caret is not at the trailing edge");
        assert_eq!(r.width, 0.0);
        assert_eq!(r.y, 100.0, "the caret left the text's line");
    }

    /// With nothing reported there is nothing to answer, and `None` is
    /// honest: the caller falls back to the element's own bounds rather
    /// than to a fabricated point.
    #[test]
    fn no_boxes_answers_nothing() {
        assert_eq!(pick_rect(&[], &(0..1)), None);
    }

    /// A composition that wrapped reports the FIRST line only.
    #[test]
    fn a_wrapped_composition_reports_only_its_first_line() {
        let mut wrapped = boxes();
        wrapped.push(CharRect { x: 10.0, y: 120.0, width: 8.0, height: 16.0 });
        let r = pick_rect(&wrapped, &(0..4)).expect("a box");
        assert_eq!(r.y, 100.0);
        assert_eq!(r.width, 24.0, "the box swallowed the wrapped line");
    }

    /// Page space plus the element's origin is window space.
    #[test]
    fn a_page_box_becomes_a_window_box_by_addition() {
        let r = to_window(&boxes()[0], 40.0, 200.0);
        assert_eq!((r.x, r.y), (50.0, 300.0));
        assert_eq!((r.width, r.height), (8.0, 16.0));
    }

    /// Closing a tab takes its composition with it.
    #[test]
    fn forgetting_a_tab_forgets_its_composition() {
        set_marked("doomed", 2, None);
        forget_tab("doomed");
        assert!(!is_composing("doomed"));
    }
}
