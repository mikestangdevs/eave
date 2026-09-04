//! Slice Q1 — the icon family.
//!
//! P3 built the pipeline (an [`AssetSource`], the [`icons!`] macro, the
//! colour-is-a-required-argument rule) and hand-drew ten glyphs to prove
//! it. This slice keeps every mechanism and **replaces the drawings with a
//! bought family**, on the user's instruction: the reference apps he wants
//! eave to look like carry a coherent icon set, and ten in-house glyphs
//! were never going to become one.
//!
//! # Where these come from
//!
//! The 55 line glyphs are the **Solar Icons** set (Linear weight) by 480
//! Design, **CC BY 4.0** — vendored out of `comet/crates/ui/assets/icons/`
//! in this same workspace, which is where the user pointed. **Attribution
//! is a licence condition, not a courtesy**; eave had no notices file at
//! all before this slice, so `THIRD_PARTY_NOTICES.md` is new and is part
//! of the deliverable rather than paperwork after it.
//!
//! The six provider marks (`claude-mark`, `openai-mark`, `cursor-mark`,
//! `grok-mark`, `hermes-mark`, `pi-mark`) are the vendors' own logos, used
//! nominatively — to name which agent a session runs. This **reverses
//! Stage K's no-third-party-marks ruling**, on the user's explicit
//! instruction ("we will definitely be using comet logos").
//!
//! The five `status-*` glyphs are the only P3 drawings that survive, for a
//! reason that is not sentiment: **the family has no VCS status set**, and
//! nothing in 53 Solar glyphs means "modified" or "conflicted".
//!
//! # Three classes, because one weight rule cannot cover all three
//!
//! P3's real idea was that a glyph's *file* stroke-width is meaningless on
//! its own — what the eye reads is `stroke_units / 24 * box_px`, and a
//! single 2.0-unit stroke is 1.7 px in a 20 px box and 1.0 px in a 12 px
//! one. That idea is kept and generalised: each class states its own
//! stroke and the size band that keeps its *effective* stroke inside
//! **1.00–1.25 px**, and a test walks the theme's own size tokens.
//!
//! | class | grid | stroke | legible boxes | eave's sizes |
//! | --- | --- | --- | --- | --- |
//! | [`Class::Line`] | 24 | 1.5 | 16–20 px | `ICON_SM` 16, `ICON_MD` 18, `ICON_TOGGLE` 20 |
//! | [`Class::Compact`] | 24 | 2.25 | 10.7–13.3 px | `ICON_STATUS` 12 |
//! | [`Class::Mark`] | any | none — fills | any | `ICON_MARK` 18, `ICON_MARK_LG` 28 |
//!
//! **This is why `ICON_SEND` and `ICON_INLINE` grew from 14 and 12 to 16.**
//! Solar Linear at 14 px is a 0.875 px stroke — a hairline that dissolves
//! against `surface()`. The family did not fit eave's slots; the slots
//! were wrong, and they are the sparse-and-flat problem in miniature.
//!
//! # Two gpui facts this module exists to survive (both P3's, both still true)
//!
//! `svg().path(P)` rasterises through `usvg` and **throws the colours
//! away**: what survives is an alpha mask, tinted at paint time with the
//! element's OWN `text.color`, not an inherited one. So:
//!
//! 1. **`fill="currentColor"` and `stroke="#000"` in these files are both
//!    arbitrary.** Only coverage survives. A brand mark therefore renders
//!    as a one-colour silhouette — which is what the marks are drawn as,
//!    and why [`claude_brand`] applies Claude's orange at the call site
//!    rather than the file carrying it.
//! 2. **A missing asset is silent.** `render_alpha_mask` returns
//!    `Ok(None)` for an unknown path and the caller `.log_err()`s a result
//!    that is not an error: no panic, no log line, a control that keeps its
//!    box and its hitbox and draws nothing. The [`icons!`] macro is the
//!    answer — path constant and registry entry come from ONE literal, so
//!    a name that compiles is a name that resolves.
//!
//! And the one that cost P3 a screen capture to find: **gpui does not
//! cascade text colour into an SVG.** `Svg::paint` guards on
//! `self.path.as_ref().zip(style.text.color)` — its own computed style,
//! `Style::default()` refined only by its own base style. Setting
//! `.text_color()` on the parent `div` looks like it should work and does
//! nothing. That is why [`icon`] takes the colour as a required argument.

use std::borrow::Cow;

use gpui::{px, svg, AssetSource, Hsla, InteractiveElement as _, SharedString, Styled as _, Svg};

/// Which drawing convention a glyph follows. See the module docs — the
/// classes exist because their stroke weights differ, so the size band
/// that keeps them reading as one set differs too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Solar Icons (Linear), CC BY 4.0 — 480 Design. 24-grid, stroke 1.5.
    Line,
    /// eave's own VCS status glyphs. 24-grid, stroke 2.25, drawn heavier
    /// so they hold at the 12 px the file rail gives them.
    Compact,
    /// A provider's own logo. Fills, not strokes, and no fixed grid.
    Mark,
}

/// Defines the path constants and the embedded registry from one list.
///
/// Both halves come from the same literal on purpose — see the module
/// docs. `include_bytes!` resolves relative to THIS file, so the assets
/// live at `crates/eave-app/assets/icons/` and travel in the binary; there
/// is no install step and no runtime filesystem read.
macro_rules! icons {
    ($($class:ident { $($(#[$m:meta])* $name:ident => $file:literal),+ $(,)? })+) => {
        $($(
            $(#[$m])*
            // The family is vendored WHOLE, so most of these constants have
            // no call site today and that is the intended state, not an
            // oversight: a bought set is only coherent if the next screen
            // can reach for a glyph without a second licensing pass. An
            // unused constant here is inventory. The registry is still
            // fully covered — every one of them is `include_bytes!`d, walked
            // by `every_icon_parses_with_the_renderers_own_parser`, and
            // checked against the assets directory, so a glyph that is
            // broken or missing fails a test whether or not it is drawn.
            #[allow(dead_code)]
            pub const $name: &str = concat!("icons/", $file);
        )+)+

        /// Every registered icon, as (path, bytes, class).
        const REGISTRY: &[(&str, &[u8], Class)] = &[
            $($((
                concat!("icons/", $file),
                include_bytes!(concat!("../assets/icons/", $file)),
                Class::$class,
            ),)+)+
        ];
    };
}

icons! {

// Solar Icons (Linear) by 480 Design, CC BY 4.0. Attribution in
// `THIRD_PARTY_NOTICES.md`. The whole family is vendored rather than the
// handful today's screens use: a set is only coherent if the next screen
// can reach for a glyph without a second licensing pass, and 55 files is
// ~40 KB in the binary.
Line {
    ADD => "add.svg",
    ADD_CIRCLE => "add-circle.svg",
    ALT_ARROW_DOWN => "alt-arrow-down.svg",
    ALT_ARROW_LEFT => "alt-arrow-left.svg",
    ALT_ARROW_RIGHT => "alt-arrow-right.svg",
    ARCHIVE_MINIMALISTIC => "archive-minimalistic.svg",
    ARCHIVE_UP_MINIMALISTIC => "archive-up-minimalistic.svg",
    ARROW_DOWN => "arrow-down.svg",
    ARROW_LEFT => "arrow-left.svg",
    ARROW_RIGHT => "arrow-right.svg",
    ARROW_UP => "arrow-up.svg",
    BELL => "bell.svg",
    CHAT_ROUND_LINE => "chat-round-line.svg",
    CHECKLIST => "checklist.svg",
    CLOSE => "close.svg",
    CLOSE_CIRCLE => "close-circle.svg",
    CLOUD => "cloud.svg",
    COMMAND => "command.svg",
    COPY => "copy.svg",
    DANGER_TRIANGLE => "danger-triangle.svg",
    DOCUMENT_ADD => "document-add.svg",
    DOCUMENT => "document.svg",
    EXPAND_ARROWS => "expand-arrows.svg",
    FOLD_VERTICAL => "fold-vertical.svg",
    FOLDER_WITH_FILES => "folder-with-files.svg",
    FOLDER => "folder.svg",
    FOLDER_OPEN => "folder-open.svg",
    GIT_BRANCH => "git-branch.svg",
    GLOBAL => "global.svg",
    INFO_CIRCLE => "info-circle.svg",
    KEYBOARD => "keyboard.svg",
    LAPTOP => "laptop.svg",
    LIST => "list.svg",
    LOGOUT_2 => "logout-2.svg",
    MAGNIFER => "magnifer.svg",
    MONITOR => "monitor.svg",
    /// Not from the vendored set: drawn in the family's own stroke
    /// grammar (24-box, 1.5 stroke, no fill) for the rail footer's theme
    /// control, because Solar's brush was not among the 55 vendored files
    /// and the registry test holds it to the same parser either way.
    PAINT_BRUSH => "paint-brush.svg",
    /// Drawn in the family's stroke grammar for the Customize Theme
    /// Light/Dark toggle — Solar's sun/moon were not among the 55
    /// vendored files. Held to the same registry parser.
    SUN => "sun.svg",
    MOON => "moon.svg",
    /// Hat + glasses. Drawn in the family's stroke grammar for incognito
    /// tabs, because Solar's set has no private-browsing mark.
    INCOGNITO => "incognito.svg",
    PAPERCLIP => "paperclip.svg",
    PEN_NEW_SQUARE => "pen-new-square.svg",
    PEN => "pen.svg",
    REFRESH => "refresh.svg",
    RESTART => "restart.svg",
    RETURN => "return.svg",
    SETTINGS_MINIMALISTIC => "settings-minimalistic.svg",
    SIDEBAR_MINIMALISTIC_LEFT => "sidebar-minimalistic-left.svg",
    SIDEBAR_MINIMALISTIC => "sidebar-minimalistic.svg",
    SMARTPHONE => "smartphone.svg",
    SORT_VERTICAL => "sort-vertical.svg",
    STAR_BOLD => "star-bold.svg",
    STAR => "star.svg",
    TAG => "tag.svg",
    TRASH_BIN_MINIMALISTIC => "trash-bin-minimalistic.svg",
    TUNING => "tuning.svg",
    VOLUME_CROSS => "volume-cross.svg",
    VOLUME_LOUD => "volume-loud.svg",
    WIDGET => "widget.svg",
    WIFI_OFF => "wifi-off.svg",
    // Slice TN1 — the composer's fast-lane toggle. Not a Solar file: drawn
    // to the same 24-grid / 1.5 stroke so it sits beside them.
    BOLT => "bolt.svg",
    /// Slice C2 — "put this beside the page it came from". Not a Solar
    /// file either: it is `sidebar-minimalistic`'s own outline with the
    /// divider moved to the CENTRE, drawn on the same grid at the same
    /// stroke. A moved line rather than a new drawing because the two
    /// glyphs mean neighbouring things, and because reusing the sidebar's
    /// own glyph for a split would be the vocabulary collision
    /// `STATUS_MODIFIED` names.
    SPLIT_PANES => "split-panes.svg",
}

// eave's own. The family has no VCS status set, and these are the only P3
// drawings that survive it.
Compact {
    /// `FileStatus::Added` — replaces the letter `A`.
    STATUS_ADDED => "status-added.svg",
    /// `FileStatus::Modified` — replaces the letter `M`. A RING, not a
    /// filled disc: eave already spends filled discs on session and tool
    /// state (`theme::STATE_DOT`, `theme::TOOL_DOT`), and a second meaning
    /// for the same mark would be a vocabulary collision.
    STATUS_MODIFIED => "status-modified.svg",
    /// `FileStatus::Deleted` — replaces the letter `D`.
    STATUS_DELETED => "status-deleted.svg",
    /// `FileStatus::Renamed` — replaces the letter `R`.
    STATUS_RENAMED => "status-renamed.svg",
    /// `FileStatus::Conflicted` — replaces the letter `U`.
    STATUS_CONFLICTED => "status-conflicted.svg",
}

// The providers' own logos, used nominatively to name which agent a
// session runs. gpui keeps only coverage, so each renders as a one-colour
// silhouette in whatever colour the call site names.
Mark {
    /// Anthropic's Claude. Tinted with [`claude_brand`] at the call site.
    CLAUDE_MARK => "claude-mark.svg",
    /// OpenAI — the Codex CLI's vendor.
    OPENAI_MARK => "openai-mark.svg",
    /// Cursor.
    CURSOR_MARK => "cursor-mark.svg",
    /// xAI's Grok.
    GROK_MARK => "grok-mark.svg",
    /// Hermes.
    HERMES_MARK => "hermes-mark.svg",
    /// Inflection's Pi.
    PI_MARK => "pi-mark.svg",
}

}

/// The app's asset source. Registered once, with `with_assets`, at the
/// only place an `Application` is built.
///
/// Unknown paths return `Ok(None)` rather than panicking or erroring: a
/// shell that dies because a decoration is missing is worse than one that
/// draws a blank box. See the module docs for why that is also a hazard.
pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(REGISTRY
            .iter()
            .find(|(p, _, _)| *p == path)
            .map(|(_, bytes, _)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(REGISTRY
            .iter()
            .filter(|(p, _, _)| p.starts_with(path))
            .map(|(p, _, _)| SharedString::from(*p))
            .collect())
    }
}

/// Claude's brand orange, `#D97757`.
///
/// The mark's file says `fill="currentColor"`, which gpui discards — the
/// colour has to arrive at the call site or the mark is whatever grey the
/// surrounding text is. Kept as a function rather than a `theme` token
/// because it is not eave's palette: it is Anthropic's, and it does not
/// move when eave's does.
// Unused until the launcher (Q2) draws a Claude tile; kept here rather
// than introduced there because it is a fact about the mark, and the mark
// arrives in this slice.
#[allow(dead_code)]
pub fn claude_brand() -> Hsla {
    gpui::rgb(0xD97757).into()
}

/// One glyph, in a fixed square box, in the colour you name.
///
/// **The colour is a required argument, and that is the whole point** —
/// see the module docs. Setting `.text_color()` on the parent `div` looks
/// like it should work and silently draws nothing.
///
/// `flex_none` because every one of these sits in a flex row beside text:
/// an icon that can be compressed by a long path or a long label is an
/// icon that changes shape, and the whole point of the fixed box is that
/// the surrounding layout cannot move it.
pub fn icon(path: &'static str, size: f32, color: Hsla) -> Svg {
    svg()
        .path(path)
        .size(px(size))
        .text_color(color)
        .flex_none()
}

/// Same, with a `debug_selector` so the harness can find its box.
///
/// The harness cannot prove an icon *paints* — see the tests below — so
/// what it can do is measure the slot, which is what this is for.
///
/// Takes an owned selector rather than a `&'static str` because the file
/// rail draws one of these PER ROW: a shared tag there would make
/// `debug_bounds` ambiguous, and an alignment test that measures whichever
/// row the lookup happened to reach first proves nothing.
pub fn tagged_icon(path: &'static str, size: f32, color: Hsla, selector: impl Into<String>) -> Svg {
    let selector = selector.into();
    icon(path, size, color).debug_selector(move || selector)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;

    /// The effective-stroke band every class has to land in, in device-
    /// independent pixels: `stroke_units / grid * box_px`.
    ///
    /// P3 used 1.0–1.2 for ten hand-drawn glyphs it could redraw at will.
    /// This is 1.0–**1.25**, and the extra 0.05 px is not a fudge to make
    /// a test pass: Solar Linear is one fixed 1.5-unit stroke, so its
    /// legible range is 16–20 px, and 20 px is `ICON_TOGGLE` — the rail
    /// collapse controls, the largest line glyph eave draws. Narrowing the
    /// band back to 1.2 would mean either shrinking those controls or
    /// redrawing 52 bought files, and the second is not a thing you do to
    /// a licensed set.
    const BAND: std::ops::RangeInclusive<f32> = 1.0..=1.25;

    /// (class, stroke units on the 24-grid, the theme sizes it is drawn at).
    ///
    /// This is the P3 table generalised: it walks the theme's OWN size
    /// tokens, so bumping `ICON_STATUS` from 12 to 16 without re-checking
    /// the arithmetic fails here rather than silently thinning a glyph.
    /// [`Class::Mark`] is absent on purpose — marks are fills and have no
    /// stroke to be thinned.
    const CLASS_SIZES: &[(Class, f32, &[f32])] = &[
        (
            Class::Line,
            1.5,
            &[theme::ICON_SM, theme::ICON_MD, theme::ICON_TOGGLE],
        ),
        (Class::Compact, 2.25, &[theme::ICON_STATUS]),
    ];

    fn body(path: &str) -> &'static str {
        REGISTRY
            .iter()
            .find(|(p, _, _)| *p == path)
            .map(|(_, b, _)| std::str::from_utf8(b).expect("icons are UTF-8"))
            .expect("registered")
    }

    fn of(class: Class) -> impl Iterator<Item = &'static str> {
        REGISTRY
            .iter()
            .filter(move |(_, _, c)| *c == class)
            .map(|(p, _, _)| *p)
    }

    /// Parse every icon with the SAME parser gpui will use at runtime
    /// (`usvg`, pinned to the version the fork's workspace pins). A
    /// substring check for `viewBox=` would pass on a file whose path data
    /// is malformed; `usvg::Tree::from_data` would not.
    #[test]
    fn every_icon_parses_with_the_renderers_own_parser() {
        for (path, bytes, _) in REGISTRY {
            let tree = usvg::Tree::from_data(bytes, &usvg::Options::default())
                .unwrap_or_else(|e| panic!("{path} does not parse: {e}"));
            assert!(
                tree.root().has_children(),
                "{path} parses but draws nothing"
            );
        }
    }

    /// The line family is one bought set at one weight. If a glyph is ever
    /// added from a different Solar weight — Bold, Broken, Outline — this
    /// is what catches it, and it is worth catching: the whole reason a set
    /// reads as a set is that every stroke is the same number.
    #[test]
    fn every_line_glyph_is_solar_linear_on_the_same_grid() {
        for path in of(Class::Line) {
            let src = body(path);
            assert!(
                src.contains(r#"viewBox="0 0 24 24""#),
                "{path}: not 24-grid"
            );
            assert!(
                src.contains(r#"stroke-width="1.5""#),
                "{path}: not the Linear weight",
            );
        }
    }

    /// eave's five, drawn to P3's rules. They are heavier than the family
    /// on purpose (see the module docs); everything else about them —
    /// grid, stroke-only, round caps and joins — matches it.
    #[test]
    fn every_compact_glyph_keeps_p3s_drawing_rules() {
        for path in of(Class::Compact) {
            let src = body(path);
            assert!(src.contains(r#"viewBox="0 0 24 24""#), "{path}: grid");
            assert!(src.contains(r#"stroke-width="2.25""#), "{path}: weight");
            assert!(src.contains(r#"fill="none""#), "{path}: not stroke-only");
            assert!(src.contains(r#"stroke-linecap="round""#), "{path}: caps");
            assert!(src.contains(r#"stroke-linejoin="round""#), "{path}: joins");
            // NB: not a raw string — `r#"fill="#"#` closes at the `"#`
            // inside it, which is a genuinely confusing way to fail.
            assert!(!src.contains("fill=\"#"), "{path}: carries a fill");
        }
    }

    /// A mark is a silhouette, and must not be mistakable for a line
    /// glyph. If one ever arrives carrying a `stroke-width` it would sit
    /// outside the band test entirely — unweighed and unnoticed — so the
    /// absence is asserted rather than assumed.
    #[test]
    fn every_mark_is_a_fill_with_no_stroke_to_weigh() {
        for path in of(Class::Mark) {
            let src = body(path);
            assert!(
                src.contains("fill="),
                "{path}: a mark with no fill draws nothing"
            );
            assert!(
                !src.contains("stroke-width="),
                "{path}: carries a stroke, so it belongs in a weighed class",
            );
        }
    }

    /// Every class's effective stroke lands in [`BAND`] at every theme size
    /// it is actually drawn at. This is the test that keeps three drawing
    /// conventions reading as one set on screen.
    #[test]
    fn effective_stroke_weight_is_uniform_across_every_class_and_size() {
        for (class, stroke_units, sizes) in CLASS_SIZES {
            for box_px in *sizes {
                let effective = stroke_units / 24.0 * box_px;
                assert!(
                    BAND.contains(&effective),
                    "{class:?}: {stroke_units} units in a {box_px}px box = {effective}px, \
                     outside the set's {:?}–{:?}px band",
                    BAND.start(),
                    BAND.end(),
                );
            }
        }
    }

    /// The size table covers every weighed class. Without this, adding a
    /// fourth class and forgetting the table would leave it unweighed —
    /// the same hole the P3 table's coverage test closed.
    #[test]
    fn the_size_table_covers_every_class_that_has_a_stroke() {
        for (_, _, class) in REGISTRY {
            if *class == Class::Mark {
                continue;
            }
            assert!(
                CLASS_SIZES.iter().any(|(c, _, _)| c == class),
                "{class:?} is registered but has no size entry",
            );
        }
    }

    /// A path the source does not know resolves to `None` — it does not
    /// panic, and it does not error. The shell's robustness depends on
    /// this, and so does the harness (which has no asset source at all).
    #[test]
    fn an_unknown_path_is_none_rather_than_a_panic_or_an_error() {
        assert!(Icons.load("icons/does-not-exist.svg").unwrap().is_none());
        assert!(Icons.load("").unwrap().is_none());
        assert!(Icons.load("../../etc/passwd").unwrap().is_none());
    }

    /// Every constant the module exports actually resolves. This is the
    /// test that would catch the macro being bypassed — someone writing a
    /// bare `svg().path("icons/typo.svg")` is not covered by anything
    /// here, which is exactly why the macro exists.
    #[test]
    fn every_exported_path_resolves_to_bytes() {
        for (path, _, _) in REGISTRY {
            assert!(Icons.load(path).unwrap().is_some(), "{path}");
        }
        // 55 vendored Solar files plus PAINT_BRUSH, SUN, MOON, INCOGNITO and
        // VOLUME_CROSS — eave's own drawings in the family's stroke
        // grammar. VOLUME_CROSS is C4's muted speaker: Solar ships
        // `volume-loud` and neither it nor comet's vendored set has the
        // crossed variant, so it is `volume-loud`'s cone with the two
        // arcs replaced by an X at the same 1.5 stroke and round caps.
        // BOLT (Slice TN1) is the composer's fast-lane toggle, drawn the
        // same way — Solar's own bolt was not among the vendored files.
        // SPLIT_PANES (Slice C2) is `sidebar-minimalistic` with its
        // divider moved to the centre, which is the same drawing on the
        // same grid rather than a new one.
        assert_eq!(of(Class::Line).count(), 62, "the Line family");
        assert_eq!(of(Class::Compact).count(), 5, "eave's status glyphs");
        assert_eq!(of(Class::Mark).count(), 6, "the provider marks");
        assert_eq!(REGISTRY.len(), 73);
    }

    /// Every registered path is a file that exists on disk under exactly
    /// the name the constant claims, and every file on disk is registered.
    ///
    /// `include_bytes!` already proves the first half at compile time. The
    /// second half is the one that matters here: an unregistered leftover
    /// is dead weight in the repo that no test would otherwise mention.
    #[test]
    fn the_assets_directory_and_the_registry_hold_the_same_names() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons");
        let mut on_disk: Vec<String> = std::fs::read_dir(dir)
            .expect("assets/icons")
            .map(|e| format!("icons/{}", e.unwrap().file_name().to_string_lossy()))
            .collect();
        on_disk.sort();
        let mut registered: Vec<String> = REGISTRY.iter().map(|(p, _, _)| p.to_string()).collect();
        registered.sort();
        assert_eq!(on_disk, registered);
    }

    /// Pins the gpui behaviour that makes [`icon`]'s colour parameter
    /// necessary, because it is the least obvious thing in this module and
    /// the fork could change it under us.
    ///
    /// A default `Style` carries NO text colour. Since
    /// `Interactivity::compute_style_internal` starts from
    /// `Style::default()` and refines it only with the element's own
    /// `base_style`, an `svg()` that names no colour reaches `Svg::paint`
    /// with `style.text.color == None` — and paint is guarded on
    /// `self.path.as_ref().zip(style.text.color)`, so it silently draws
    /// nothing while laying out a perfectly correct box.
    ///
    /// **This is not hypothetical: it shipped in P3's first build, every
    /// glyph was invisible, and all 254 tests passed.** Only a window
    /// capture found it. If a fork bump ever makes SVGs inherit colour,
    /// this fails and someone gets to re-read that decision.
    #[test]
    fn a_default_style_carries_no_text_colour_which_is_why_icon_takes_one() {
        assert!(
            gpui::Style::default().text.color.is_none(),
            "gpui now supplies a default text colour — re-read `icon`'s docs \
             before relying on it, and check whether Svg::paint still guards \
             on style.text.color",
        );
    }

    #[test]
    fn list_enumerates_the_set_by_prefix() {
        assert_eq!(Icons.list("icons/").unwrap().len(), REGISTRY.len());
        assert!(Icons.list("nope/").unwrap().is_empty());
    }
}
