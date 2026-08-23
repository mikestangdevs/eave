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
    #[test]
    fn bezel_and_eave_share_one_gpui() {
        fn same(d: bezel::gpui::Div) -> gpui::Div {
            d
        }
        let _ = same as fn(gpui::Div) -> gpui::Div;
    }
}
