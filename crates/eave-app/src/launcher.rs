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
//! The tiles read their state from **the same table the engine spawns
//! from** — [`eave_core::provider`] — rather than from a bool in this
//! file. A tile cannot claim to work when the engine would refuse it, and
//! an adapter cannot land without its tile lighting up, because there is
//! no second list to forget to update
//! ([`tests::the_strip_names_exactly_the_providers_eave_catalogues`]).
//!
//! # "No adapter yet" was one word for two different problems
//!
//! Until Slice AT every tile eave could not start said *"no adapter
//! yet"*, and five of six said it. That sentence was true of Cursor and
//! **false of Codex, Grok, Hermes and Pi the moment their adapters
//! landed** — and it would have gone on being the only thing a tile could
//! say to somebody who simply had not installed the CLI. The two states
//! have different fixes: one is a release eave owes you, the other is an
//! install you can do in ten seconds. A grey tile that says the first
//! when it means the second tells you to wait for something you do not
//! need.
//!
//! So a tile states one of four things, from
//! [`eave_core::provider::Availability`]:
//!
//! | tile says | means | the fix |
//! | --- | --- | --- |
//! | `ready` | adapter, verified posture, CLI on this machine | — |
//! | `not installed` | eave can drive it; the CLI is not here | install it |
//! | `posture unproven` | the adapter declines to run unsupervised | a proof |
//! | `no adapter yet` | eave names it and cannot drive it | eave's work |
//!
//! **A tile eave would accept is selectable whether or not the CLI is
//! installed**, and its caption is the reason. Installed-ness is a
//! fact about the machine that can change between two frames; the
//! adapter's existence cannot. Making the click depend on the first would
//! make the strip's behaviour — and every test of it — depend on what
//! happens to be in `/opt/homebrew/bin` on the machine running it.
//! Pressing start on an uninstalled provider gets the engine's own
//! refusal on `launcher-error`, which is the authoritative one, because
//! **the engine is the process that spawns**.
//!
//! The tiles eave cannot drive are still drawn, dimmed, and inert. Hiding
//! them would be tidier and less true: the shape of the product is that
//! eave drives several agents, and the strip is where that promise is
//! either kept or visibly outstanding.
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

/// One agent eave knows how to name, and the things only a UI needs to
/// know about it. Whether eave can *run* it is not a field here — see
/// [`availability`].
pub struct Provider {
    /// The value that goes in `AgentConfig::provider`. It selects the
    /// adapter, the binary and the posture proof — one id, one row in
    /// [`eave_core::provider::CATALOG`], enforced by
    /// [`tests::the_strip_names_exactly_the_providers_eave_catalogues`].
    ///
    /// # The bug this used to warn about is fixed
    ///
    /// The warning that stood here said: the launcher sends
    /// `AgentConfig { provider, model, program: None }`, `program: None`
    /// meant *the engine's own `EAVE_AGENT` binary*, so the moment a
    /// second tile went live it would spawn **the other vendor's CLI
    /// while claiming to be itself** — "the single worst bug this file
    /// could grow", with no test able to catch it because no second
    /// provider existed to catch it with.
    ///
    /// Slice AT closed it in the engine rather than here, which is where
    /// it belonged: `program: None` with a named provider now resolves
    /// **that provider's own CLI** (`Engine::program_plan`). The launcher
    /// still sends no program, and that is now the correct thing to send
    /// — the id is the selector.
    pub id: &'static str,
    pub name: &'static str,
    /// A [`icons::Class::Mark`] path — the vendor's own logo.
    pub mark: &'static str,
    /// Well-known `--model` aliases for the picker; when non-empty the
    /// first entry is always [`MODEL_DEFAULT`].
    ///
    /// **Empty is legal and means "this CLI's own default only"** — see
    /// [`model_choices`]. An adapter can land without anybody knowing
    /// that vendor's aliases, and eave does not invent model names (it
    /// passes `--model` verbatim and does not validate it, P2 caveat 5).
    /// What stays forbidden is the reverse: a provider eave cannot drive
    /// offering a menu of models, which is the exact texture of a
    /// mocked-up UI ([`tests::a_provider_eave_cannot_drive_offers_no_models`]).
    pub models: &'static [&'static str],
}

/// The "let the CLI decide" entry. Selecting it means `AgentConfig::model`
/// is `None` and **no `--model` flag is passed**.
pub const MODEL_DEFAULT: &str = "CLI default";

/// Every agent eave names, in strip order. Claude first because it is
/// the reference adapter — the one whose permission posture was proven
/// first and against which the others are written.
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
        models: &[],
    },
    Provider {
        id: "pi",
        name: "Pi",
        mark: icons::PI_MARK,
        models: &[],
    },
];

/// What eave can do about this provider on this machine right now.
///
/// Delegates to `eave-core` — the **same** table the engine spawns from —
/// rather than answering from a field here. Two sources of truth for "is
/// this provider supported" is exactly how a UI ends up offering a button
/// that produces an error.
pub fn availability(p: &Provider) -> eave_core::provider::Availability {
    eave_core::provider::availability(p.id)
}

/// Would the engine accept a session naming this provider at all — is
/// there a registered adapter that has declared a verified posture?
///
/// **This, not installed-ness, is what makes a tile clickable.** It is a
/// fact about eave's own code, so it is the same on every machine and at
/// every frame; whether the CLI is on disk is neither. See the module
/// doc.
///
/// The same call the engine's gate makes, so the two cannot drift
/// ([`tests::a_live_tile_is_one_the_engine_would_accept`]).
pub fn eave_can_drive(p: &Provider) -> bool {
    eave_core::session::posture_is_verified(p.id)
}

/// The tile the launcher opens on: the first provider eave has an adapter
/// for.
///
/// **Deliberately not the first *ready* one.** Opening on a different
/// tile depending on which CLIs happen to be installed would make the
/// rest state of the app a property of the machine, and would move under
/// an operator who installed something. It opens where eave's own
/// capability says it should and lets the caption carry the machine's
/// side of it.
///
/// Returns an index rather than a reference so it can be held in state.
/// Falls back to 0 only if eave has no adapters at all — a state
/// [`tests::the_reference_adapter_is_registered`] refuses, but the render
/// path still has to produce something.
pub fn default_provider() -> usize {
    PROVIDERS.iter().position(eave_can_drive).unwrap_or(0)
}

/// The model choices to offer for a provider: always at least
/// [`MODEL_DEFAULT`], which sends no `--model` flag at all.
///
/// An adapter can land before anybody has catalogued that vendor's model
/// aliases, and "let the CLI decide" is a real, correct choice for every
/// agent — so the picker is never empty and never has to be special-cased
/// by the caller.
pub fn model_choices(p: &Provider) -> &'static [&'static str] {
    if p.models.is_empty() {
        const JUST_THE_DEFAULT: &[&str] = &[MODEL_DEFAULT];
        JUST_THE_DEFAULT
    } else {
        p.models
    }
}

/// One provider tile.
///
/// **A dead tile still occludes and still has a hitbox.** It takes no
/// handler, so a click on it does nothing at all — which is the point:
/// "does nothing" is a clearer answer than "falls through to whatever is
/// behind me". Same treatment P1 gave a disabled menu row, same
/// [`theme::DISABLED_OPACITY`].
///
/// The caption is [`eave_core::provider::Availability::tile_status`] —
/// the state read off the registry at draw time, not a string chosen
/// here. A provider eave can drive but whose CLI is absent reads *not
/// installed* and stays live to the click; see the module doc for why
/// that split falls where it does.
pub fn tile(
    ix: usize,
    p: &Provider,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    // Two different questions, and the tile answers both. `live` is
    // eave's capability and decides the click; `status` is this machine's
    // and decides the caption.
    let live = eave_can_drive(p);
    let status = availability(p).tile_status();
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
                .child(status),
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
    use eave_core::provider::{self, Availability};

    /// The invariant that keeps the strip honest: **the strip and the
    /// registry name the same providers**. An adapter that lands in
    /// `eave-core` with no tile here is a capability nobody can reach; a
    /// tile whose id is in no catalog row is one the engine would refuse
    /// with "eave does not know a provider".
    ///
    /// Both directions, because before Slice AT only one of them was
    /// checked and the unchecked one was where the drift would have gone.
    #[test]
    fn the_strip_names_exactly_the_providers_eave_catalogues() {
        for e in provider::CATALOG {
            assert!(
                PROVIDERS.iter().any(|p| p.id == e.id),
                "{} is catalogued in eave-core but the launcher has no tile for it",
                e.id,
            );
        }
        for p in PROVIDERS {
            assert!(
                provider::catalog(p.id).is_some(),
                "the launcher offers a tile for {}, which eave-core does not name — the engine \
                 would refuse it",
                p.id,
            );
        }
        assert_eq!(PROVIDERS.len(), provider::CATALOG.len());
    }

    /// Every adapter eave registers can be reached from the UI, and every
    /// tile the UI treats as live has an adapter behind it. Stated as the
    /// rule rather than as a count: four adapters are landing in parallel
    /// and a number here would have to be edited by each of them.
    #[test]
    fn a_live_tile_is_one_the_engine_would_accept() {
        for p in PROVIDERS {
            assert_eq!(
                eave_can_drive(p),
                eave_core::session::posture_is_verified(p.id),
                "{}: the launcher and the engine's gate disagree about whether eave can drive it",
                p.id,
            );
        }
        for id in eave_core::session::posture_verified_providers() {
            assert!(
                PROVIDERS.iter().any(|p| p.id == id),
                "{id} is posture-verified but the launcher has no tile for it",
            );
        }
    }

    /// Claude is the reference adapter and the strip's first tile. The
    /// one thing here that is allowed to be a specific claim, because it
    /// is the thing every other adapter is written against.
    #[test]
    fn the_reference_adapter_is_registered() {
        assert_eq!(PROVIDERS[0].id, "claude-code", "Claude leads the strip");
        assert!(
            eave_can_drive(&PROVIDERS[0]),
            "the reference adapter is gone"
        );
        assert!(
            PROVIDERS.iter().any(eave_can_drive),
            "no provider has an adapter, so the launcher can start nothing"
        );
    }

    /// A provider eave cannot drive offers no models. Model choices for
    /// an agent that cannot spawn is the exact texture of a mocked-up UI.
    ///
    /// The converse is **not** asserted: an adapter may land before
    /// anybody knows that vendor's aliases, and [`model_choices`] gives
    /// it the CLI's own default rather than an empty menu.
    #[test]
    fn a_provider_eave_cannot_drive_offers_no_models() {
        for p in PROVIDERS {
            if !eave_can_drive(p) {
                assert!(p.models.is_empty(), "{} is not startable", p.id);
            }
            if !p.models.is_empty() {
                assert_eq!(
                    p.models[0], MODEL_DEFAULT,
                    "{}: the first choice must be the CLI's own default",
                    p.id,
                );
            }
            let offered = model_choices(p);
            assert_eq!(
                offered[0], MODEL_DEFAULT,
                "{}: every picker opens on the CLI's own default",
                p.id,
            );
            assert!(!offered.is_empty(), "{}: an empty model picker", p.id);
        }
    }

    /// The launcher opens on something eave has an adapter for — and on
    /// the same tile regardless of what is installed, which is what makes
    /// this assertion mean anything on a machine other than mine.
    #[test]
    fn the_default_selection_is_a_tile_eave_can_drive() {
        assert!(eave_can_drive(&PROVIDERS[default_provider()]));
    }

    /// **The user-visible half of Slice AT.** A tile says which kind of
    /// "not now" it is, and the four sentences are distinct — a caption
    /// that read the same for "eave cannot do this yet" and "you have not
    /// installed it" is the defect this slice exists to fix.
    #[test]
    fn a_tile_distinguishes_a_missing_adapter_from_a_missing_cli() {
        let sentences = [
            Availability::Ready {
                program: std::path::PathBuf::from("/somewhere/claude"),
            },
            Availability::NotInstalled {
                bin: "cursor-agent",
                searched: vec![std::path::PathBuf::from("/usr/bin/cursor-agent")],
            },
            Availability::PostureUnproven {
                reason: "nobody has shown it asks",
            },
            Availability::NoAdapter,
        ]
        .map(|a| a.tile_status());
        for (i, s) in sentences.iter().enumerate() {
            assert!(!s.is_empty(), "state {i} says nothing");
            assert!(
                sentences.iter().skip(i + 1).all(|o| o != s),
                "two different states both say {s:?}, which is the bug this test exists for",
            );
        }
        // And the one every tile in the strip actually shows resolves.
        for p in PROVIDERS {
            let a = availability(p);
            assert!(
                !a.tile_status().is_empty(),
                "{} draws a tile with no caption",
                p.id
            );
            assert_eq!(
                a.is_ready(),
                a.why_not(p.id).is_none(),
                "{}: a tile that is not ready must state why",
                p.id
            );
        }
    }

    /// A tile eave cannot drive still **draws, says something true, and
    /// takes no click** — the promise the strip makes by showing six
    /// marks when it can start one.
    ///
    /// Over every such provider rather than a named one: this was written
    /// against `cursor-agent` while its CLI was absent from the machine,
    /// and the CLI appeared three hours later.
    #[test]
    fn a_tile_eave_cannot_drive_still_draws_and_says_why() {
        let dead: Vec<&Provider> = PROVIDERS.iter().filter(|p| !eave_can_drive(p)).collect();
        assert!(!dead.is_empty(), "nothing to prove if every tile is live");
        for p in dead {
            assert_eq!(
                availability(p),
                Availability::NoAdapter,
                "{}: eave cannot drive it, so the tile must say that and not something about \
                 this machine",
                p.id
            );
            assert_eq!(availability(p).tile_status(), "no adapter yet");
            assert!(availability(p).why_not(p.id).is_some());
        }
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
