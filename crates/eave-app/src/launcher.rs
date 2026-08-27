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
//! [`Provider::models`] is therefore a list of well-known native ids, not
//! a claim about what the CLI will accept. There is no "CLI default" row:
//! a pick always names a model.
//!
//! # Slice CK: the aliases stopped being eave's to choose
//!
//! Everything above stays true and one thing under it changed. A list
//! typed by hand can only rot, and by 2026-08-21 all four of them had:
//! `PI_MODELS` named six models this machine's Pi cannot run, Claude's
//! named none of the two the CLI had gained, and Codex's and Grok's were
//! empty while both CLIs answer a list in under three seconds.
//!
//! So the launcher now asks. [`eave_core::capabilities`] holds one row per
//! provider saying how — a flag, a JSON-RPC method on the CLI's own
//! stdio wire, or **nothing, said out loud**. [`model_choices`] prefers
//! the CLI's answer over the arrays here whenever one has landed, and
//! [`model_source_note`] states which of the two is on screen, because a
//! probe is never allowed to hold a frame and so a frame has to be able
//! to admit it is showing yesterday's answer.

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
    /// Well-known `--model` values for the picker (native ids the CLI
    /// actually takes). First entry is the preselection.
    ///
    /// **Slice CK: this is what eave shows before the CLI answers, not
    /// what it shows instead.** Once
    /// [`eave_core::capabilities::reported`] has a list for this provider,
    /// [`model_choices`] ignores this field entirely — including when the
    /// CLI's answer is *no models at all*, which is a fact about the
    /// operator's credentials and beats anything typed here.
    ///
    /// **Empty is legal** — Hermes takes no `--model` (ACP), and a
    /// provider eave cannot drive must not offer a menu
    /// ([`tests::a_provider_eave_cannot_drive_offers_no_models`]).
    pub models: &'static [&'static str],
}

/// Every agent eave names, in strip order. Claude first because it is
/// the reference adapter — the one whose permission posture was proven
/// first and against which the others are written.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "claude-code",
        name: "Claude Code",
        mark: icons::CLAUDE_MARK,
        // Native `--model` values Claude Code actually takes, from
        // MonoCode's `claudeCatalog` (the same ids `list_models` returns
        // as `value`). Aliases `opus`/`sonnet`/`haiku` still work if the
        // CLI reports them; they are not duplicated here. First is
        // MonoCode's default (`claude:sonnet-5`).
        models: &[
            "claude-sonnet-5",
            "claude-opus-5",
            "claude-fable-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-opus-4-5",
            "claude-haiku-4-5",
        ],
    },
    Provider {
        id: "codex",
        name: "Codex",
        // OpenAI's mark stands in: the set carries no separate Codex mark,
        // which is also what comet does.
        mark: icons::OPENAI_MARK,
        // Native ids `codex app-server`'s `model/list` takes, from
        // MonoCode's `codexCatalog` / `codexProtocol` fixtures. First is
        // the row those fixtures mark `isDefault`. The live probe
        // outranks this the moment it answers (Slice CK).
        models: &[
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.4",
            "gpt-5.3-codex",
        ],
    },
    Provider {
        id: "cursor-agent",
        name: "Cursor",
        mark: icons::CURSOR_MARK,
        // Native ids MonoCode's `MODELS` lists for cursor (default
        // `cursor:composer-2.5`). `cursor-agent acp` takes no `--model`
        // on argv — the adapter sends `session/set_model` `{ modelId }`
        // the same way MonoCode's `applyModelSelection` does. The live
        // `--list-models` / ACP catalog still replaces this (Slice CK).
        models: &[
            "composer-2.5",
            "gpt-5.4",
            "claude-sonnet-4-6",
            "grok-4.6",
        ],
    },
    Provider {
        id: "grok",
        name: "Grok",
        mark: icons::GROK_MARK,
        // `grok models` on 2026-08-21: these two, default marked.
        models: &["grok-4.6", "grok-4.5"],
    },
    Provider {
        id: "hermes",
        name: "Hermes",
        mark: icons::HERMES_MARK,
        // Live, but `hermes acp` takes no `--model` — ACP carries the
        // model as protocol (`session/set_model`). Empty, not a fake
        // "let the CLI decide" row.
        models: &[],
    },
    Provider {
        id: "pi",
        name: "Pi",
        mark: icons::PI_MARK,
        // Slice AX shipped the model list tested, in
        // `eave_core::pi::MODELS`: Pi is multi-provider underneath and
        // `--model` takes a `provider/id` pair, so the provider axis is
        // carried IN the model string and eave's picker needs no second
        // dimension. `--provider` is never sent.
        models: eave_core::pi::MODELS,
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

/// The model choices to offer for a provider. Empty when the CLI has
/// none, or when eave cannot pass `--model` (Hermes).
///
/// # Slice CK: the installed CLI outranks [`Provider::models`]
///
/// If the CLI on this machine has said what it can run, **that is the
/// list**, and the hand-written one is not consulted at all. The static
/// arrays above stay because they are what eave shows before the answer
/// arrives, on a machine where the CLI cannot be asked, and in the app's
/// first frame — never as a second opinion once the CLI has spoken.
///
/// Asking is a side effect of being asked for the choices, and it costs
/// this frame nothing: [`eave_core::capabilities::begin`] starts a thread
/// and returns, and under `cargo test` it does not even do that (the
/// harness never calls `run`, so probes are never armed). That is the only
/// arrangement in which no render path can forget to ask — the defect
/// Slice CD paid for was a method every adapter implemented and nobody
/// called.
pub fn model_choices(p: &Provider) -> Vec<&'static str> {
    eave_core::capabilities::begin(p.id);
    if let Some(reported) = eave_core::capabilities::reported(p.id) {
        return reported.to_vec();
    }
    p.models.to_vec()
}

/// **Who said what the picker is showing** — the sentence that sits under
/// the model button so a stale list can never pass for a fresh one.
///
/// `None` means there is nothing to say: the CLI answered, so the list is
/// simply the CLI's. Every other state is a claim eave is making on its
/// own account, and says so.
///
/// This is the half of "non-blocking" that is not about threads. comet's
/// rule is that discovery never stalls a frame
/// (`docs/research/acp.md` §"Managed adapter installs": a cold `npx`
/// "could also stall a first chat for minutes"), and the price of that
/// rule is that a frame can be wrong. It is only an acceptable price if
/// the frame admits it.
pub fn model_source_note(p: &Provider) -> Option<String> {
    use eave_core::capabilities::Freshness;
    match eave_core::capabilities::freshness(p.id) {
        Freshness::Reported { named: 0 } => {
            Some(format!("{} reports no models for this account.", p.name))
        }
        Freshness::Reported { .. } => None,
        Freshness::Asking => Some(format!("eave's own list — asking {}…", p.name)),
        Freshness::Silent => Some(format!("eave's own list — {} did not answer.", p.name)),
        Freshness::Unaskable(why) => Some(format!("eave's own list — {why}")),
        Freshness::Refused => Some(format!(
            "eave's own list — eave will not spawn {} to ask.",
            p.name,
        )),
        Freshness::Unasked => Some("eave's own list.".to_string()),
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
    /// anybody knows that vendor's native ids, and [`model_choices`] is
    /// then empty until the CLI answers.
    #[test]
    fn a_provider_eave_cannot_drive_offers_no_models() {
        for p in PROVIDERS {
            if !eave_can_drive(p) {
                assert!(p.models.is_empty(), "{} is not startable", p.id);
            }
            assert!(
                !p.models.iter().any(|m| *m == "CLI default"),
                "{} still offers a send-no-flag row",
                p.id
            );
        }
    }

    #[test]
    fn a_pick_names_a_native_id() {
        let claude = &PROVIDERS[0];
        assert_eq!(claude.id, "claude-code");
        assert_eq!(claude.models[0], "claude-sonnet-5");
        let grok = PROVIDERS.iter().find(|p| p.id == "grok").expect("grok");
        assert_eq!(grok.models, &["grok-4.6", "grok-4.5"]);
        let codex = PROVIDERS.iter().find(|p| p.id == "codex").expect("codex");
        assert_eq!(codex.models[0], "gpt-5.6-terra");
        let cursor = PROVIDERS
            .iter()
            .find(|p| p.id == "cursor-agent")
            .expect("cursor");
        assert_eq!(cursor.models[0], "composer-2.5");
        let hermes = PROVIDERS.iter().find(|p| p.id == "hermes").expect("hermes");
        assert!(
            hermes.models.is_empty(),
            "hermes acp takes no --model: {:?}",
            hermes.models
        );
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
    ///
    /// **It asserted `NoAdapter` for every undrivable tile until the four
    /// adapters landed, and that assertion was too narrow rather than
    /// wrong.** There are now two distinct reasons eave will not drive a
    /// provider, and collapsing them would throw away the more useful one:
    ///
    /// - `NoAdapter` — nothing in `provider::ADAPTERS` can spawn it.
    /// - `PostureUnproven` — the adapter exists and works, but eave has not
    ///   established the CLI asks before it writes, so it refuses to run
    ///   it.
    ///
    /// Every catalogued provider is live today. The loop still holds the
    /// caption rule for any future undrivable tile; it no longer requires
    /// one to exist.
    #[test]
    fn a_tile_eave_cannot_drive_still_draws_and_says_why() {
        let dead: Vec<&Provider> = PROVIDERS.iter().filter(|p| !eave_can_drive(p)).collect();
        for p in dead {
            let a = availability(p);
            assert!(
                matches!(
                    a,
                    Availability::NoAdapter | Availability::PostureUnproven { .. }
                ),
                "{}: eave cannot drive it, so the tile must say that and not something about \
                 this machine — got {a:?}",
                p.id
            );
            assert!(
                !a.tile_status().is_empty(),
                "{}: an undrivable tile with no caption tells a person nothing",
                p.id
            );
            assert!(a.why_not(p.id).is_some());
        }
        for p in PROVIDERS {
            assert!(
                eave_can_drive(p),
                "{}: every named provider now has a verified adapter",
                p.id
            );
        }
    }

    /// The launcher's Pi list and the adapter's tested list must not drift.
    #[test]
    fn pi_offers_exactly_the_adapters_tested_models() {
        let pi = PROVIDERS.iter().find(|p| p.id == "pi").expect("pi tile");
        assert_eq!(
            pi.models, eave_core::pi::MODELS,
            "the launcher's Pi models drifted from the adapter's tested list"
        );
    }

    /// The two undrivable reasons do not read the same, because they do not
    /// have the same fix: one waits on eave, the other waits on evidence.
    #[test]
    fn a_missing_adapter_and_an_unproven_posture_do_not_say_the_same_thing() {
        let no_adapter = Availability::NoAdapter;
        let unproven = Availability::PostureUnproven {
            reason: "stand-in reason, not a real evidence string",
        };
        assert_ne!(
            no_adapter.tile_status(),
            unproven.tile_status(),
            "a tile eave cannot drive AT ALL and one it refuses to TRUST read identically, so \
             the caption cannot tell a person which of the two they are looking at"
        );
    }

    /// **The defect Slice CK exists to fix.** Measured on this machine
    /// 2026-08-21: `claude` 2.1.x answers its own `list_models` control
    /// request with `claude-fable-5[1m]` and `opus[1m]`, neither of which
    /// the hand-written array above had ever heard of. When the CLI has
    /// spoken, the picker shows what it said — not what somebody typed
    /// here months ago.
    ///
    /// Uses `record` rather than a live probe on purpose: this asserts the
    /// plumbing, and a test that needed the operator's auth would be a
    /// test that fails on a machine that has not logged in.
    #[test]
    fn the_picker_shows_what_the_installed_cli_reported() {
        let _turn = cache_turn();
        eave_core::capabilities::forget();
        let claude = &PROVIDERS[0];
        assert_eq!(claude.id, "claude-code");
        // Before: eave's own guess, and it does not contain Fable.
        assert!(
            !model_choices(claude).contains(&"claude-fable-5[1m]"),
            "the static list is not supposed to know about Fable",
        );
        eave_core::capabilities::record(
            "claude-code",
            vec!["claude-fable-5[1m]".into(), "sonnet".into()],
        );
        let offered = model_choices(claude);
        assert_eq!(
            offered,
            vec!["claude-fable-5[1m]", "sonnet"],
            "the CLI's answer, not eave's guess and not a send-no-flag row",
        );
        assert!(
            !offered.contains(&"opus"),
            "a hand-written alias survived the CLI's own answer",
        );
        eave_core::capabilities::forget();
    }

    /// A static list is what eave shows *before* the CLI answers. Once
    /// the CLI has spoken, that list is not consulted — even when eave's
    /// own guess was already populated (Slice CK).
    #[test]
    fn a_provider_with_no_static_list_still_gets_the_cli_s() {
        let _turn = cache_turn();
        eave_core::capabilities::forget();
        let codex = PROVIDERS
            .iter()
            .find(|p| p.id == "codex")
            .expect("codex tile");
        assert!(
            !codex.models.is_empty(),
            "codex has a MonoCode-backed static list"
        );
        assert_eq!(model_choices(codex), codex.models);
        eave_core::capabilities::record("codex", vec!["gpt-5.4".into(), "gpt-5.3-codex".into()]);
        assert_eq!(
            model_choices(codex),
            vec!["gpt-5.4", "gpt-5.3-codex"],
        );
        eave_core::capabilities::forget();
    }

    /// **A CLI reporting nothing outranks eave's guess.** Live on this
    /// machine 2026-08-21 `pi --list-models` answered *"No models
    /// available"* while `PI_MODELS` offered six `provider/id` pairs —
    /// every one of them a flag this operator's Pi would reject. The
    /// picker shows nothing and says who said so.
    #[test]
    fn a_cli_that_reports_no_models_retires_eaves_guess() {
        let _turn = cache_turn();
        eave_core::capabilities::forget();
        let pi = PROVIDERS.iter().find(|p| p.id == "pi").expect("pi tile");
        assert!(model_choices(pi).len() > 1, "pi has a hand-written list");
        eave_core::capabilities::record("pi", vec![]);
        assert_eq!(
            model_choices(pi),
            Vec::<&str>::new(),
            "eave kept offering models the CLI says this operator does not have",
        );
        let note = model_source_note(pi).expect("naming zero models is worth a sentence");
        assert!(note.contains("no models"), "{note:?}");
        eave_core::capabilities::forget();
    }

    /// **A stale list must announce itself.** The note is absent only when
    /// the CLI answered and named something; in every other state the
    /// picker is showing eave's own guess and says whose guess it is.
    #[test]
    fn a_list_eave_invented_says_so_and_one_the_cli_gave_does_not() {
        let _turn = cache_turn();
        eave_core::capabilities::forget();
        let claude = &PROVIDERS[0];
        let before = model_source_note(claude).expect("an unasked list is eave's own");
        assert!(
            before.contains("eave's own list"),
            "a guess must be labelled a guess: {before:?}",
        );
        eave_core::capabilities::record("claude-code", vec!["sonnet".into()]);
        assert_eq!(
            model_source_note(claude),
            None,
            "the CLI's own list needs no disclaimer",
        );
        // "The CLI has none" is an answer, and a different sentence from
        // "eave has not asked".
        eave_core::capabilities::forget();
        eave_core::capabilities::record("claude-code", vec![]);
        let none = model_source_note(claude).expect("no models is still worth stating");
        assert!(
            !none.contains("eave's own list"),
            "reporting zero models is the CLI's answer, not eave's guess: {none:?}",
        );
        eave_core::capabilities::forget();
    }

    /// Hermes is the provider that cannot be asked, and its note says why
    /// rather than leaving the operator to wonder whether eave is still
    /// loading. Silence is the honest answer, not a placeholder.
    #[test]
    fn a_provider_that_cannot_be_asked_says_so_instead_of_pending() {
        let _turn = cache_turn();
        eave_core::capabilities::forget();
        let hermes = PROVIDERS
            .iter()
            .find(|p| p.id == "hermes")
            .expect("hermes tile");
        let note = model_source_note(hermes).expect("an unaskable provider owes a reason");
        assert!(
            note.contains("picker"),
            "hermes' note must name the reason `hermes model` cannot be read: {note:?}",
        );
        assert!(!note.contains("asking"), "nothing is being asked: {note:?}");
        eave_core::capabilities::forget();
    }

    /// The capability cache is process-global and these tests clear it, so
    /// they take turns. Same reason `eave_core::capabilities::tests` has
    /// its own guard.
    /// The probe cache is process-global and `cargo test` runs this
    /// crate's tests in parallel threads of ONE process, so this lock has
    /// to be the SAME lock the `ui_harness` tests take — a second private
    /// mutex here would serialise these tests against each other and
    /// against nothing else. `the_model_menu_choice_reaches_the_launch`
    /// failed about every second run that way.
    fn cache_turn() -> std::sync::MutexGuard<'static, ()> {
        eave_core::capabilities::probe_test_lock()
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
