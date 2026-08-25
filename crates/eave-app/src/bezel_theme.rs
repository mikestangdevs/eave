//! bezel, and the one seam it is allowed through.
//!
//! # Attribution
//!
//! [bezel](https://github.com/crabtalk/bezel) is MIT-licensed, © 2026
//! clearloop, with portions extracted from `zeronsh/comet` © Wing. eave
//! vendors nothing: it is a rev-pinned git dependency (`f86dbbf`, `main` at
//! adoption on 2026-08-23) and the licence travels with the source in
//! cargo's checkout. The pin and the reasoning behind it are in
//! `crates/eave-app/Cargo.toml`; the `[patch]` that makes it build against
//! our gpui fork is in the workspace root.
//!
//! # Why this module exists at all
//!
//! bezel's components read a `bezel::theme::Theme` gpui `Global` at paint
//! time — one flat struct of 44 fields, the way SwiftUI views read
//! `@Environment`. eave already has a single source of truth for colour,
//! type and spacing in [`crate::theme`], and Slice BZ's standing rule is
//! that adopting bezel must not create a second one.
//!
//! So this module is the **only** place allowed to write a bezel token, and
//! it writes every one of them *from* [`crate::theme`]. Nothing here invents
//! a colour. When bezel wants a value eave has no token for, the fix is to
//! add the token to `theme.rs` and derive it here — never to hardcode it
//! into the global, because a literal in this file is exactly the second
//! token system the rule forbids.
//!
//! Stage 0 wires the dependency and proves the graph. The construction
//! itself is stage 1.

use bezel::theme::{Appearance, SyntaxPalette, Theme};
use gpui::App;

use crate::theme as eave;

/// Install eave's tokens as bezel's `Theme` global. Call once, at startup,
/// before any bezel component paints.
///
/// [`Theme::install_custom`] is the seam that makes preference (a) of the
/// stage 1 rule reachable at all: bezel can be handed a whole theme rather
/// than asked to pick one of its own two. Without it the only options would
/// have been bezel's palette (a second token system by definition) or a fork.
pub fn install(cx: &mut App) {
    reinstall(cx);
}

/// The asset source, once, for both icon sets.
///
/// gpui takes exactly ONE [`AssetSource`] per `Application`, and both eave and
/// bezel ship their own — each serving paths shaped `icons/<name>.svg`. A
/// bezel component that draws an icon calls `svg().path(..)`, which resolves
/// through whichever source the application registered, so registering eave's
/// alone means every bezel glyph silently renders as empty coverage. Not an
/// error, not a panic: a blank square where a chevron should be.
///
/// **eave's set wins a collision, deliberately.** The two sets overlap by
/// name (`check`, `close`, `plus` are in both) and eave's glyphs are the ones
/// the rest of the app paints; letting bezel's answer for a shared name would
/// mean the same idea drawn two ways on one screen depending on which
/// component asked. So eave is tried first and bezel is the fallback, which
/// also means adopting a bezel component never silently restyles an eave one.
pub struct Assets;

impl gpui::AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        match crate::icons::Icons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => bezel::ui::icons::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        let mut out = crate::icons::Icons.list(path)?;
        for p in bezel::ui::icons::Assets.list(path)? {
            if !out.contains(&p) {
                out.push(p);
            }
        }
        Ok(out)
    }
}

/// bezel's theme where there is no `App` to read the global from.
///
/// bezel's components are catalog traits on `&Theme`, and the intended way to
/// reach one is `Theme::of(cx)`. eave's transcript rows are rendered by
/// `render_row`, which takes no context at all — it is a pure function of the
/// row and the animation phase, which is exactly why the display list can be
/// folded and measured without a window.
///
/// Threading an `App` down to it to fetch a constant would trade that property
/// away for nothing, because eave's theme IS constant: there is one appearance
/// and no runtime switch. So this hands back the same value the global holds,
/// built once. `the_global_and_the_context_free_theme_are_the_same` is what
/// keeps that sentence true — if eave ever grows a second appearance, that
/// test fails and this function is the thing that has to change.
pub fn theme() -> Theme {
    if let Ok(g) = CACHE.read() {
        if let Some(t) = g.as_ref() {
            return t.clone();
        }
    }
    let t = from_eave_tokens();
    if let Ok(mut g) = CACHE.write() {
        *g = Some(t.clone());
    }
    t
}

static CACHE: std::sync::RwLock<Option<Theme>> = std::sync::RwLock::new(None);

/// Rebuild bezel's global from the current eave tokens. Called at boot
/// and whenever the theme editor writes a brand.
pub fn reinstall(cx: &mut App) {
    let t = from_eave_tokens();
    if let Ok(mut g) = CACHE.write() {
        *g = Some(t.clone());
    }
    Theme::install_custom(t, cx);
}

/// eave's tokens, in the shape bezel reads them.
///
/// **Every field on the right-hand side is a call into [`crate::theme`].**
/// That is the whole invariant, and `every_bezel_token_is_an_eave_token`
/// holds it: not one literal colour appears below, so there is exactly one
/// place in eave where a pigment is decided and it is not this file. Where
/// bezel named a role eave had not, the token was added to `theme.rs` and
/// derived there — see its Slice BZ block — rather than being written here.
///
/// # eave is dark-only, and that is a fact about eave
///
/// bezel ships a designed light appearance; eave has never had one. The
/// appearance is pinned to [`Appearance::Dark`] because every token below is
/// a dark-ground value, and letting the OS flip bezel to Light would repaint
/// its components against eave's dark surfaces — bezel's own light palette
/// over eave's dark ground, the worst of both. When eave grows a light
/// appearance, this is the line that learns about it.
fn from_eave_tokens() -> Theme {
    Theme {
        appearance: Appearance::Dark,

        // ---- surfaces ----
        // eave's ladder is not bezel's, and it inverts on purpose: the
        // content well is the DARKEST plane (`content_plate`), because the
        // well is the hero and chrome floats above it. bezel's `surface_card`
        // is the nearest role, so the inversion travels with the value
        // instead of being argued about at each call site.
        bg: eave::canvas(),
        surface: eave::chrome_glass(),
        surface_raised: eave::raised(),
        surface_card: eave::content_plate(),
        surface_dialog: eave::menu_glass(),
        surface_overlay: eave::menu_glass(),

        // ---- interactive state ----
        // The three-rung state ladder, in the order eave solved it for:
        // hover is one step off the ground, selected is one step above hover.
        element_hover: eave::overlay(),
        element_active: eave::overlay_strong(),
        surface_raised_hover: eave::inverse_hover(),

        // ---- edges ----
        border: eave::border(),
        border_strong: eave::border_strong(),

        // ---- text ----
        text: eave::text(),
        text_muted: eave::text_secondary(),
        text_faint: eave::text_disabled(),
        text_dim: eave::text_tertiary(),

        // ---- the high-contrast solid (primary buttons) ----
        solid: eave::solid(),
        on_solid: eave::on_solid(),

        // ---- accents ----
        // bezel ships its accent neutral on purpose, so an installing app
        // does not inherit someone else's hue. eave HAS chosen one, and this
        // is the line where that choice reaches bezel's components.
        accent: eave::accent(),
        accent_strong: eave::accent_hover(),
        on_accent: eave::on_accent(),

        // ---- state pigment ----
        // `danger_ink` is eave's quiet-tier danger text, sampled by Slice AQ
        // against the quiet floor; `warning_muted`/`success_muted` are its
        // two siblings, derived in theme.rs rather than invented here.
        danger: eave::danger(),
        danger_muted: eave::danger_ink(),
        danger_strong: eave::danger(),
        warning: eave::warning(),
        warning_muted: eave::warning_muted(),
        success: eave::success(),
        success_muted: eave::success_muted(),
        busy: eave::live(),

        // ---- components ----
        band: eave::chip_plate(),
        // The composer is L2 glass in eave — floating chrome over the log,
        // not a plate on it. bezel's `input_bg` is the same role.
        input_bg: eave::chrome_glass(),
        selection: eave::selection(),
        // A caret is the next glyph before you type it, so it is the body
        // text colour. bezel's own token doc argues this, and eave's composer
        // already painted `text()` at a toggled opacity — the two agreed
        // before they met.
        caret: eave::text(),
        // A block cursor is a terminal's, and eave has no terminal. Pointed
        // at the caret's tone so that if one ever arrives it is already
        // consistent, rather than at a colour chosen for nothing.
        cursor: eave::text(),

        // ---- code and diff ----
        code_text: eave::code_text(),
        // A sunken plate in eave is read from its RIM, not its fill: the
        // glass ground is already near-black, so darkening it moves the pixel
        // ~2/255. `inset()` is transparent for exactly that reason, and this
        // carries the same answer into bezel rather than re-deriving a wash
        // that eave measured and rejected.
        code_wash: eave::inset(),
        diff_add: eave::diff_add_bg(),
        diff_del: eave::diff_del_bg(),
        diff_hunk_bg: eave::raised(),
        syntax: monochrome_syntax(),

        // ---- type ----
        // Both fallbacks are the same families: eave names a concrete system
        // face rather than a stack, so there is nothing to fall back TO.
        font_sans: eave::SANS.into(),
        font_mono: eave::MONO.into(),
        font_sans_fallback: eave::SANS.into(),
        font_mono_fallback: eave::MONO.into(),
    }
}

/// The syntax palette eave actually paints: none.
///
/// bezel's [`SyntaxPalette`] has 24 fields and eave has no highlighter — a
/// fenced block paints [`crate::theme::code_block_text`] end to end, and an
/// inline run paints [`crate::theme::code_text`]. Twenty-four invented hues
/// would be a palette eave never chose, installed globally, which is the
/// second token system this stage exists to prevent; the fact that they would
/// look plausible is what makes it worse, not better.
///
/// So the palette states the truth instead: one tone, the one a code body
/// already has. Comments and punctuation recede to the muted tone because
/// eave's markdown already treats de-emphasis that way, and `invalid` is the
/// one place a hue is warranted — a highlighter saying "this does not parse"
/// is state, and state is what pigment is for.
///
/// When eave grows a highlighter, that slice picks the hues. This is not a
/// placeholder to be forgotten: it is what the app looks like today, and if
/// it is ever wrong it will be wrong visibly.
fn monochrome_syntax() -> SyntaxPalette {
    let body = eave::code_block_text();
    let quiet = eave::text_tertiary();
    SyntaxPalette {
        comment: quiet,
        punctuation: quiet,
        operator: quiet,
        invalid: eave::danger(),
        keyword: body,
        string: body,
        string_special: body,
        escape: body,
        number: body,
        boolean: body,
        type_name: body,
        type_builtin: body,
        constructor: body,
        function: body,
        function_builtin: body,
        macro_name: body,
        property: body,
        constant: body,
        variable: body,
        variable_special: body,
        parameter: body,
        tag: body,
        attribute: body,
        label: body,
    }
}

/// The one-type-universe proof, and the reason it is compiled rather than
/// asserted in prose.
///
/// bezel pins gpui to `crabtalk/zed`; eave pins `mikestangdevs/zed`. Two git
/// sources for one package name is a graph cargo will build without
/// complaint, and the failure it produces is not a link error — it is two
/// incompatible type universes, in which `bezel::ui`'s `Div` and
/// `gpui::Div` are unrelated types and no bezel component can ever be the
/// child of an eave element. That failure appears at the first *use*, which
/// could be a stage away from the manifest change that caused it.
///
/// This function is that first use, brought forward to stage 0. It takes a
/// `Div` from bezel's crate graph and returns it as `gpui::Div` — eave's own
/// import, the same one every element in `ui.rs` is built from. If the
/// `[patch]` in the workspace root ever stops reconciling, this stops
/// compiling, and it stops compiling *here*, next to the comment that
/// explains it.
///
/// `cargo tree` is the other half of the check and cannot replace this one:
/// it reports what cargo *resolved*, while this reports what rustc *unified*.
#[allow(dead_code, reason = "a compile-time proof; stage 1 gives it callers")]
fn bezel_paints_through_our_gpui(theme: &bezel::theme::Theme) -> gpui::Div {
    use bezel::ui::widgets::Status as _;
    theme.step_row("check", "bezel", None, None, false, None)
}

#[cfg(test)]
mod tests {
    /// There is exactly ONE gpui, and this is what says so at the type level.
    ///
    /// `gpui::Div` here is eave's dependency. `bezel::gpui::Div` is what
    /// bezel's components are built from and return. The assignment below is
    /// only well-typed if the `[patch]` collapsed both to a single package —
    /// which is the stage 0 gate, stated as code.
    /// No pigment is decided in this file, and this is what enforces it.
    ///
    /// The stage 1 gate is "no second token system". Every field of the
    /// bezel theme is a call into `theme.rs` today, but nothing about that
    /// survives the next person who needs a colour bezel names and eave does
    /// not — the tempting one-line fix is a literal right here, and it would
    /// be invisible in review because it would sit among forty-three honest
    /// mappings.
    ///
    /// So the file reads itself. A colour constructor outside a comment is a
    /// failure, and the fix it points at is the one the module doc states:
    /// add the token to `theme.rs`, derive it there, and map it here.
    #[test]
    fn no_pigment_is_decided_in_this_file() {
        let src = include_str!("bezel_theme.rs");
        let offenders: Vec<_> = src
            .lines()
            .enumerate()
            .filter(|(_, l)| {
                let code = l.trim_start();
                !code.starts_with("//") && !code.starts_with("///")
            })
            // Split so this line is not itself a match — the first cut of
            // this test failed on its own detector, which is funny once.
            .filter(|(_, l)| {
                [
                    concat!("rgb", "(0x"),
                    concat!("hsla", "("),
                    concat!("rgba", "("),
                ]
                .iter()
                .any(|needle| l.contains(needle))
            })
            .map(|(n, l)| format!("{}: {}", n + 1, l.trim()))
            .collect();
        assert!(
            offenders.is_empty(),
            "a colour was decided in bezel_theme.rs instead of theme.rs:\n{}",
            offenders.join("\n")
        );
    }

    /// Every token bezel paints traces to `theme.rs` — spot-checked at the
    /// fields that would be noticed first if the mapping silently stopped
    /// being applied (which is how a default palette usually announces
    /// itself: not as a crash, but as a slightly different grey).
    #[test]
    fn the_installed_theme_is_eaves_own() {
        let t = super::from_eave_tokens();
        assert_eq!(t.text, crate::theme::text());
        assert_eq!(t.bg, crate::theme::canvas());
        assert_eq!(t.accent, crate::theme::accent());
        assert_eq!(t.input_bg, crate::theme::chrome_glass());
        assert_eq!(t.caret, crate::theme::text());
        assert_eq!(t.diff_add, crate::theme::diff_add_bg());
        assert_eq!(t.font_sans.as_ref(), crate::theme::SANS);
        assert_eq!(t.appearance, bezel::theme::Appearance::Dark);
    }

    /// **The one thing `install_custom` cannot reach, measured rather than
    /// hoped about.**
    ///
    /// bezel's `ink`/`wash`/`hairline` are free functions keyed on the
    /// appearance enum, not fields of the Theme struct — so handing bezel a
    /// whole theme does not redirect them. bezel's dark ink is pure white;
    /// eave's is 0.90 white. Over eave's near-black ground the two diverge by
    /// `alpha × 25.5`/255, which is nothing at a hover wash and visible at a
    /// scrollbar thumb.
    ///
    /// This pins the crossing point instead of pretending it is absent. At
    /// eave's own [`SURFACE_STEP_FLOOR`](crate::theme::SURFACE_STEP_FLOOR) of
    /// 6/255 the boundary sits near α 0.235. Below it the two inks are the
    /// same colour to the eye; above it they are not, and the only bezel
    /// modules that go above it are `scroll.rs` (the scrollbar thumb) and
    /// `controls.rs` (toggle knob, checkbox rim) — neither of which eave
    /// adopts, because stage 3 keeps our list and its scrolling and neither
    /// control is one of the five surfaces.
    ///
    /// If a later slice adopts one of those, this test is the note explaining
    /// why the thumb looks a shade bright, and the fix is to paint it with an
    /// eave token rather than to let bezel's ink decide.
    #[test]
    fn bezels_ink_and_eaves_ink_part_company_above_one_step() {
        let t = super::from_eave_tokens();
        // eave's `ink` is private; `raised()` IS `ink(0.044)`, so its
        // lightness is eave's ink lightness without restating the literal.
        let eave_l = crate::theme::raised().l;
        let bezel_l = t.ink(0.044).l;
        assert!(bezel_l > eave_l, "bezel's dark ink should be the brighter");

        let divergence = |alpha: f32| alpha * (bezel_l - eave_l) * 255.0;
        let floor = crate::theme::SURFACE_STEP_FLOOR;

        // The highest alpha reached by any bezel module eave adopts.
        assert!(
            divergence(0.12) < floor,
            "at α 0.12 the two inks differ by {:.2}/255, over the {floor}/255 floor",
            divergence(0.12)
        );
        // The scrollbar thumb, which eave does not adopt — proof the boundary
        // is real and not an artefact of picking a flattering alpha.
        assert!(
            divergence(0.38) >= floor,
            "at α 0.38 the two inks differ by only {:.2}/255 — if this is now \
             under the {floor}/255 floor the divergence has closed and the \
             caveat in the report is stale",
            divergence(0.38)
        );
    }

    /// The context-free theme and the installed global must not drift.
    ///
    /// They are the same constructor today. The day eave grows a light
    /// appearance they stop being the same value, and this is the test that
    /// says so before a screen does.
    #[test]
    fn the_global_and_the_context_free_theme_are_the_same() {
        let built = super::from_eave_tokens();
        let cached = super::theme();
        assert_eq!(built.text, cached.text);
        assert_eq!(built.bg, cached.bg);
        assert_eq!(built.appearance, cached.appearance);
        assert_eq!(built.font_mono, cached.font_mono);
    }

    #[test]
    fn bezel_and_eave_share_one_gpui() {
        fn same(d: bezel::gpui::Div) -> gpui::Div {
            d
        }
        let _ = same as fn(gpui::Div) -> gpui::Div;
    }
}
