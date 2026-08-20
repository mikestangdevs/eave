//! Slice Q2 — the launcher: eave's rest state.
//!
//! # There was no rest state before this slice
//!
//! That is the thing to understand first. `ui.rs`'s connection loop
//! **opened a session by itself** the moment it found an empty engine, and
//! otherwise selected the most recent one — so the string "no session
//! selected" that Slice M wrote was, in practice, a frame or two at boot
//! and nothing else. eave had no moment of rest to design, because it
//! never rested.
//!
//! The launcher is that moment made real: nothing is running, the shell
//! states where it would run and under which agent, and **a session exists
//! only because you asked for one**. Auto-open is gone; `+ new` comes here
//! instead of silently spawning.
//!
//! # Why the strip is honest rather than aspirational
//!
//! Six marks, and exactly **one** of them can be started today. That is
//! not a placeholder — it is
//! [`eave_core::session::POSTURE_VERIFIED_PROVIDERS`], the fail-closed
//! gate Slice P2 put in front of per-session config, and it currently
//! reads `["claude-code"]` because Slice K's posture proof is
//! Claude-specific and transfers to no other CLI.
//!
//! So the tiles read their liveness from **the same constant the engine
//! refuses on** ([`is_live`]) rather than from a bool in this file. A tile
//! cannot claim to work when the engine would refuse it, and an adapter
//! cannot land without its tile lighting up — [`tests::every_verified_provider_has_a_tile`]
//! fails if the two ever disagree.
//!
//! The five that are not live are drawn, dimmed, and inert. Hiding them
//! would be tidier and less true: the shape of the product is that eave
//! drives several agents, and the strip is where that promise is either
//! kept or visibly outstanding.
//!
//! # The model list is a convenience, not a validation
//!
//! eave passes `--model` **verbatim** and does not check it (P2 caveat 5:
//! an unknown model is the CLI's error to state, not eave's to guess).
//! [`Provider::models`] is therefore a list of well-known aliases to save
//! typing, not a claim about what the CLI will accept, and picking
//! [`MODEL_DEFAULT`] sends **no `--model` flag at all** — which is not the
//! same as sending an empty one.

use gpui::{
    div, px, AnyElement, App, ClickEvent, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window,
};

use crate::{icons, theme};

/// One agent eave knows how to name. Not one it can necessarily run —
/// see [`is_live`].
pub struct Provider {
    /// The value that goes in `AgentConfig::provider`, and the string the
    /// engine's gate is checked against.
    ///
    /// **Only `claude-code` is meaningful today.** The other five are the
    /// ids we expect their adapters to register under; nothing depends on
    /// them being right, because nothing can start them. When an adapter
    /// lands, its id here has to match the gate or
    /// [`tests::every_verified_provider_has_a_tile`] fails.
    ///
    /// **Read this before adding the second live provider.** The launcher
    /// sends `AgentConfig { provider, model, program: None, .. }`, and
    /// `program: None` means *the engine's own `EAVE_AGENT` binary*
    /// (`Engine::resolve_agent`). The id is a posture assertion, not a
    /// binary selector. Today that is harmless — the only startable tile
    /// is the one `EAVE_AGENT` already points at — but the moment two
    /// tiles are live, a tile that does not also carry a `program` would
    /// spawn the *other* vendor's CLI while claiming to be itself. That is
    /// the single worst bug this file could grow, and there is no test
    /// that would catch it, because no second provider exists to catch it
    /// with.
    pub id: &'static str,
    pub name: &'static str,
    /// A [`icons::Class::Mark`] path — the vendor's own logo.
    pub mark: &'static str,
    /// Well-known `--model` aliases, first entry always [`MODEL_DEFAULT`].
    /// Empty for a provider eave cannot start: offering model choices for
    /// an agent that will not spawn is the kind of detail that makes a UI
    /// feel fake.
    pub models: &'static [&'static str],
}

/// The "let the CLI decide" entry. Selecting it means `AgentConfig::model`
/// is `None` and **no `--model` flag is passed**.
pub const MODEL_DEFAULT: &str = "CLI default";

/// Every agent eave names, in strip order. Claude first because it is the
/// only one that starts.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "claude-code",
        name: "Claude Code",
        mark: icons::CLAUDE_MARK,
        models: &[MODEL_DEFAULT, "opus", "sonnet", "haiku"],
    },
    Provider {
        id: "codex",
        name: "Codex",
        // OpenAI's mark stands in: the set carries no separate Codex mark,
        // which is also what comet does.
        mark: icons::OPENAI_MARK,
        models: &[],
    },
    Provider {
        id: "cursor-agent",
        name: "Cursor",
        mark: icons::CURSOR_MARK,
        models: &[],
    },
    Provider {
        id: "grok",
        name: "Grok",
        mark: icons::GROK_MARK,
        models: &[],
    },
    Provider {
        id: "hermes",
        name: "Hermes",
        mark: icons::HERMES_MARK,
        // Empty until the tile is live, which
        // `tests::only_a_startable_provider_offers_models` enforces and
        // which this slice deliberately did not break.
        //
        // Slice AW established what belongs here when it IS live, and it
        // is exactly one entry: `hermes acp` takes no `--model` flag at
        // all — ACP carries the model as protocol — and the list a live
        // Hermes offers is whatever providers that machine holds
        // credentials for (177 entries on the development machine, every
        // one of them machine-specific). So `&[MODEL_DEFAULT]`, the "send
        // no model, let the CLI decide" entry, is the only selection eave
        // could honour, and it goes in with the line that makes the tile
        // live rather than before it.
        models: &[],
    },
    Provider {
        id: "pi",
        name: "Pi",
        mark: icons::PI_MARK,
        models: &[],
    },
];

/// Can eave actually start this one?
///
/// Delegates to `eave-core` — the **same** constant the engine refuses
/// on — rather than answering from a field here. Two sources of truth for
/// "is this provider supported" is exactly how a UI ends up offering a
/// button that produces an error.
pub fn is_live(p: &Provider) -> bool {
    eave_core::session::posture_is_verified(p.id)
}

/// The tile the launcher opens on: the first provider eave can start.
///
/// Returns an index rather than a reference so it can be held in state.
/// Falls back to 0 only if nothing is live, which would mean the gate list
/// is empty — a state `every_verified_provider_has_a_tile` would already
/// have failed on, but the render path still has to produce something.
pub fn default_provider() -> usize {
    PROVIDERS.iter().position(is_live).unwrap_or(0)
}

/// One provider tile.
///
/// **A dead tile still occludes and still has a hitbox.** It takes no
/// handler, so a click on it does nothing at all — which is the point:
/// "does nothing" is a clearer answer than "falls through to whatever is
/// behind me". Same treatment P1 gave a disabled menu row, same
/// [`theme::DISABLED_OPACITY`].
pub fn tile(
    ix: usize,
    p: &Provider,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let live = is_live(p);
    // The mark's colour is a required argument (`icons::icon`) — gpui does
    // not cascade text colour into an SVG, so a tile that forgets this
    // lays out a perfectly correct 28px box and paints nothing.
    let mark_color = if !live {
        theme::text_tertiary()
    } else if p.id == "claude-code" {
        // The one mark with a sourced brand colour. The other five render
        // in eave's own text colour rather than a hex I invented.
        icons::claude_brand()
    } else {
        theme::text()
    };
    let body = div()
        .id(("provider-tile", ix))
        .debug_selector(move || format!("provider-tile-{ix}"))
        .w(px(theme::TILE_W))
        .h(px(theme::TILE_H))
        .rounded(px(theme::RADIUS_TILE))
        .border_1()
        // Slice AL tried the accent here and put it back. A chosen tile is
        // a SELECTION — where you are, not where you are acting — and the
        // accent's rule (see `theme::accent`) does not care that this
        // particular selection happens to be short-lived. The rim stays
        // neutral and the tile's own wash carries the state.
        .border_color(if selected {
            theme::border_strong()
        } else {
            theme::border()
        })
        .bg(if selected {
            theme::overlay()
        } else {
            theme::wash_rest()
        })
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(theme::SPACE_2))
        .child(icons::icon(p.mark, theme::ICON_MARK_LG, mark_color))
        .child(
            div()
                .text_size(px(theme::DETAIL_SIZE))
                .line_height(px(theme::DETAIL_LINE))
                .text_color(if live {
                    theme::text()
                } else {
                    theme::text_tertiary()
                })
                .child(p.name),
        )
        .child(
            div()
                .text_size(px(theme::MICRO_SIZE))
                .line_height(px(theme::MICRO_LINE))
                .text_color(theme::text_tertiary())
                .child(if live { "ready" } else { "no adapter yet" }),
        );

    if live {
        body.cursor_pointer().on_click(on_click).into_any_element()
    } else {
        // No handler, and dimmed by TOKEN rather than by opacity. The
        // container multiply used to live here and it could not work: over
        // the glass ground it took the tile's name from 2.72:1 to **1.54:1**
        // and its caption to 1.28:1, on the six tiles that are the first
        // thing anyone sees. It was not a bad constant — pure white through
        // the same multiply only reaches 3.11:1, so no tier could have
        // survived it (`a_container_opacity_cannot_express_disabled`).
        //
        // The relationship the old comment wanted is kept by the tokens
        // themselves: a dead tile already picks `text_tertiary` for its
        // name and its mark where a live one picks `text`, which is the
        // whole distinction, and it still has no pointer and still says
        // "no adapter yet".
        body.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant that keeps the strip honest: **every provider the
    /// engine would accept has a tile**, and its id matches. If an adapter
    /// lands in `eave-core`'s gate and nobody adds it here, the launcher
    /// would silently keep refusing to offer something that works.
    #[test]
    fn every_verified_provider_has_a_tile() {
        for id in eave_core::session::POSTURE_VERIFIED_PROVIDERS {
            let tile = PROVIDERS.iter().find(|p| p.id == id);
            assert!(
                tile.is_some(),
                "{id} is posture-verified but the launcher has no tile for it",
            );
        }
    }

    /// The mirror: a tile is live *because* the gate says so, never
    /// because this file says so. Stated as a count so that adding an
    /// adapter is a deliberate two-line change (gate + this number) rather
    /// than something that drifts.
    #[test]
    fn exactly_one_provider_can_be_started_today() {
        assert_eq!(PROVIDERS.iter().filter(|p| is_live(p)).count(), 1);
        assert!(is_live(&PROVIDERS[0]), "Claude is the strip's first tile");
    }

    /// A dead provider offers no models. Model choices for an agent that
    /// cannot spawn is the exact texture of a mocked-up UI.
    #[test]
    fn only_a_startable_provider_offers_models() {
        for p in PROVIDERS {
            if is_live(p) {
                assert!(!p.models.is_empty(), "{} is live with no models", p.id);
                assert_eq!(
                    p.models[0], MODEL_DEFAULT,
                    "{}: the first choice must be the CLI's own default",
                    p.id,
                );
            } else {
                assert!(p.models.is_empty(), "{} is not startable", p.id);
            }
        }
    }

    /// The launcher opens on something you can actually start.
    #[test]
    fn the_default_selection_is_a_live_tile() {
        assert!(is_live(&PROVIDERS[default_provider()]));
    }

    /// Every mark resolves. `icons` already proves each registered path
    /// loads; this proves the launcher spells them the same way, which is
    /// the failure `icons`'s macro cannot catch on its own — a tile could
    /// name a real constant for the wrong vendor.
    #[test]
    fn every_tile_names_a_distinct_registered_mark() {
        use gpui::AssetSource as _;
        let mut seen = Vec::new();
        for p in PROVIDERS {
            assert!(
                icons::Icons.load(p.mark).unwrap().is_some(),
                "{}: {} does not resolve",
                p.id,
                p.mark,
            );
            assert!(
                !seen.contains(&p.mark),
                "{}: {} is used twice",
                p.id,
                p.mark
            );
            seen.push(p.mark);
        }
    }
}
