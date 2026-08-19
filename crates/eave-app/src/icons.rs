//! Slice P3 — the icon pipeline.
//!
//! eave had no [`AssetSource`] and not one `svg()` call before this
//! module: every pictogram in the shell was either a text character
//! (`↑`, `+ new`, `↓ bottom`, the `M`/`A`/`D` status letters) or a small
//! tower of hand-positioned `div`s (both rail collapse toggles). This is
//! the machinery that lets a glyph be a glyph, plus the drawings.
//!
//! # The glyphs are drawn here, not imported
//!
//! Every file in `../assets/icons/` was drawn for this slice. Nothing is
//! sourced from Lucide, Feather, Heroicons or any other set, so there is
//! no upstream licence and no attribution to carry. The *convention* is
//! the widely-used one those sets share — a 24×24 viewBox, `fill="none"`,
//! round caps and joins, one open path per stroke — because it is what
//! makes a set look like a set. The drawings are ours.
//!
//! **One style throughout, enforced by a test.** Every icon is stroke-only
//! on a 24×24 grid with round caps and joins; none uses a fill. The stroke
//! *width* deliberately varies per file, and that is not an inconsistency:
//! gpui renders an SVG into its box, so a single 2.0-unit stroke would be
//! 1.7 px in the 20 px toggle and 1.0 px in the 12 px status slot — the
//! same number reading as two different weights. Each file's width is
//! chosen instead so its
//! *effective* stroke lands in 1.0–1.2 px at the size it is actually used
//! ([`WEIGHTS`] pins this, with the arithmetic).
//!
//! # How gpui draws these, and the two consequences
//!
//! `svg().path(P)` resolves `P` through the app's [`AssetSource`], hands
//! the bytes to `usvg`, rasterises at 2× ([`SMOOTH_SVG_SCALE_FACTOR`]) and
//! then **throws the colours away**: what survives is an alpha mask, tinted
//! at paint time with the element's OWN `text.color` — not an inherited
//! one. See [`icon`]: that distinction is the difference between a glyph
//! and a blank box, and it cost this slice a capture to find.
//!
//! 1. **The `stroke="#000"` in every file is arbitrary.** Only coverage
//!    survives. This is why the constraint "must work in both render modes
//!    and against every surface rung" is satisfied by construction rather
//!    than by testing each pairing: an icon is exactly as legible as the
//!    theme colour it is given, and those are already placed against every
//!    rung. What is NOT automatic is *stroke weight* — a hairline can
//!    dissolve on a busy ground where a text glyph would not — hence the
//!    weight discipline above.
//!
//! 2. **A missing asset is silent.** `render_alpha_mask` returns `Ok(None)`
//!    for a path the source does not know, and the caller `.log_err()`s a
//!    result that is not an error. No panic, no log line: the control keeps
//!    its box and its hitbox and simply draws nothing. That is the right
//!    robustness (the directive asks for `None`, not a panic) and a
//!    genuinely nasty failure mode — a typo'd path is an invisible button.
//!
//! The [`icons!`] macro is the answer to (2): the path constant and the
//! registry entry are generated from *one* literal, so a name that
//! compiles is a name that resolves. There is no way to typo one without
//! the other, because there is only one of them.

use std::borrow::Cow;

use gpui::{px, svg, AssetSource, Hsla, InteractiveElement as _, SharedString, Styled as _, Svg};

/// Defines the path constants and the embedded registry from one list.
///
/// Both halves come from the same literal on purpose — see the module
/// docs. `include_bytes!` resolves relative to THIS file, so the assets
/// live at `crates/eave-app/assets/icons/` and travel in the binary; there
/// is no install step and no runtime filesystem read.
macro_rules! icons {
    ($($(#[$m:meta])* $name:ident => $file:literal),+ $(,)?) => {
        $(
            $(#[$m])*
            pub const $name: &str = concat!("icons/", $file);
        )+

        /// Every registered icon, as (path, bytes).
        const REGISTRY: &[(&str, &[u8])] = &[
            $((
                concat!("icons/", $file),
                include_bytes!(concat!("../assets/icons/", $file)),
            ),)+
        ];
    };
}

icons! {
    /// Send. Replaces the `↑` character in the composer's send button.
    ARROW_UP => "arrow-up.svg",
    /// Return to the newest row. Replaces the `↓` in the `↓ bottom` pill.
    ARROW_DOWN => "arrow-down.svg",
    /// New session. Replaces the `+` in the rail's `+ new` button.
    PLUS => "plus.svg",
    /// Session rail collapse. Replaces the hand-drawn sidebar pictogram
    /// (bordered rect + a hairline `div` at the third).
    PANEL_LEFT => "panel-left.svg",
    /// Files rail collapse. The mirror of [`PANEL_LEFT`].
    PANEL_RIGHT => "panel-right.svg",
    /// `FileStatus::Added` — replaces the letter `A`.
    STATUS_ADDED => "status-added.svg",
    /// `FileStatus::Modified` — replaces the letter `M`. A RING, not a
    /// filled disc: eave already spends filled discs on session and tool
    /// state ([`crate::theme::STATE_DOT`], [`crate::theme::TOOL_DOT`]), and
    /// a second meaning for the same mark would be a vocabulary collision.
    STATUS_MODIFIED => "status-modified.svg",
    /// `FileStatus::Deleted` — replaces the letter `D`.
    STATUS_DELETED => "status-deleted.svg",
    /// `FileStatus::Renamed` — replaces the letter `R`.
    STATUS_RENAMED => "status-renamed.svg",
    /// `FileStatus::Conflicted` — replaces the letter `U`.
    STATUS_CONFLICTED => "status-conflicted.svg",
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
            .find(|(p, _)| *p == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(REGISTRY
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::from(*p))
            .collect())
    }
}

/// One glyph, in a fixed square box, in the colour you name.
///
/// **The colour is a required argument, and that is the whole point.**
/// gpui does NOT cascade text colour into an SVG: `compute_style_internal`
/// starts from `Style::default()` and refines it with the element's OWN
/// `base_style`, so `style.text.color` on a `svg()` is whatever that
/// element set and nothing else. `Svg::paint` then does
/// `self.path.zip(style.text.color)` — with no colour there is no tuple,
/// no `paint_svg` call, and the glyph is silently invisible even though
/// the bytes loaded fine and the box laid out correctly.
///
/// Setting `.text_color()` on the parent `div` looks like it should work
/// and does nothing. Making the colour a parameter is the only version of
/// this API that cannot be got wrong by accident.
///
/// `flex_none` because every one of these sits in a flex row beside text:
/// an icon that can be compressed by a long path or a long label is an
/// icon that changes shape, and the whole point of the fixed box is that
/// the surrounding layout cannot move it.
pub fn icon(path: &'static str, size: f32, color: Hsla) -> Svg {
    svg().path(path).size(px(size)).text_color(color).flex_none()
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
pub fn tagged_icon(
    path: &'static str,
    size: f32,
    color: Hsla,
    selector: impl Into<String>,
) -> Svg {
    let selector = selector.into();
    icon(path, size, color).debug_selector(move || selector)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The size each icon is actually rendered at, and the stroke width its
    /// file carries. Kept here rather than derived so that changing a
    /// `theme` box size or an SVG's `stroke-width` without re-checking the
    /// arithmetic fails this table rather than silently thinning a glyph.
    ///
    /// (path, rendered box px, stroke units on the 24-unit grid)
    const WEIGHTS: &[(&str, f32, f32)] = &[
        (ARROW_UP, crate::theme::ICON_SEND, 2.0),
        (ARROW_DOWN, crate::theme::ICON_INLINE, 2.25),
        (PLUS, crate::theme::ICON_INLINE, 2.25),
        (PANEL_LEFT, crate::theme::ICON_TOGGLE, 1.4),
        (PANEL_RIGHT, crate::theme::ICON_TOGGLE, 1.4),
        (STATUS_ADDED, crate::theme::ICON_STATUS, 2.25),
        (STATUS_MODIFIED, crate::theme::ICON_STATUS, 2.25),
        (STATUS_DELETED, crate::theme::ICON_STATUS, 2.25),
        (STATUS_RENAMED, crate::theme::ICON_STATUS, 2.25),
        (STATUS_CONFLICTED, crate::theme::ICON_STATUS, 2.25),
    ];

    fn body(path: &str) -> &'static str {
        REGISTRY
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, b)| std::str::from_utf8(b).expect("icons are UTF-8"))
            .expect("registered")
    }

    /// The directive's test, and the only one that can catch a broken
    /// drawing before it reaches a window: parse every icon with the SAME
    /// parser gpui will use at runtime (`usvg`, pinned to the version the
    /// fork's workspace pins), and require a viewBox.
    ///
    /// A substring check for `viewBox=` would pass on a file whose path
    /// data is malformed; `usvg::Tree::from_data` would not.
    #[test]
    fn every_icon_parses_with_the_renderers_own_parser() {
        for (path, bytes) in REGISTRY {
            let tree = usvg::Tree::from_data(bytes, &usvg::Options::default())
                .unwrap_or_else(|e| panic!("{path} does not parse: {e}"));
            let size = tree.size();
            assert_eq!(
                (size.width(), size.height()),
                (24.0, 24.0),
                "{path} does not carry the 24×24 viewBox the set is drawn on",
            );
            assert!(
                tree.root().has_children(),
                "{path} parses but draws nothing",
            );
        }
    }

    /// One style throughout — the directive's constraint, made checkable.
    /// Stroke-only on a 24-grid with round caps and joins; no fills.
    #[test]
    fn every_icon_is_drawn_in_the_same_stroked_style() {
        for (path, _) in REGISTRY {
            let src = body(path);
            assert!(src.contains(r#"viewBox="0 0 24 24""#), "{path}: grid");
            assert!(src.contains(r#"fill="none""#), "{path}: not stroke-only");
            assert!(src.contains(r#"stroke-linecap="round""#), "{path}: caps");
            assert!(src.contains(r#"stroke-linejoin="round""#), "{path}: joins");
            // NB: not a raw string — `r#"fill="#"#` closes at the `"#`
            // inside it, which is a genuinely confusing way to fail.
            assert!(
                !src.contains("fill=\"#"),
                "{path}: carries a fill, so it is not in the set's style",
            );
        }
    }

    /// Every glyph's stroke lands in 1.0–1.2 px at the size it is used.
    ///
    /// This is the constraint that keeps ten files reading as one set
    /// despite five different `stroke-width` values, and it is the one a
    /// future size change is most likely to break: bump `ICON_STATUS` from
    /// 10 to 14 and the status glyphs quietly become the heaviest things
    /// on screen.
    #[test]
    fn effective_stroke_weight_is_uniform_across_the_set() {
        for (path, box_px, stroke_units) in WEIGHTS {
            let effective = stroke_units / 24.0 * box_px;
            assert!(
                (1.0..=1.2).contains(&effective),
                "{path}: {stroke_units} units in a {box_px}px box = {effective}px stroke, \
                 outside the set's 1.0–1.2px band",
            );
            // ...and the table must describe the file, not a wish.
            assert!(
                body(path).contains(&format!(r#"stroke-width="{stroke_units}""#)),
                "{path}: file does not carry stroke-width {stroke_units}",
            );
        }
    }

    /// The table covers the registry. Without this, adding an icon and
    /// forgetting the table would leave it unweighed.
    #[test]
    fn the_weight_table_covers_every_registered_icon() {
        assert_eq!(WEIGHTS.len(), REGISTRY.len());
        for (path, _) in REGISTRY {
            assert!(
                WEIGHTS.iter().any(|(p, _, _)| p == path),
                "{path} is registered but has no weight entry",
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
        for (path, _) in REGISTRY {
            assert!(Icons.load(path).unwrap().is_some(), "{path}");
        }
        assert_eq!(REGISTRY.len(), 10);
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
    /// **This is not hypothetical: it shipped in this slice's first build,
    /// every glyph was invisible, and all 254 tests passed.** Only a window
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
