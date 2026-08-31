//! Slice AG1 #13 — a session title PROPOSED from the transcript, in the
//! client, and nowhere else.
//!
//! # Why this file is in `eave-app` and may never move
//!
//! `watch` is eave's one cross-session broadcast, and spec 05 §Watch makes
//! it carry **session identity and state ONLY** — *"never journal records,
//! never provider output, never transcript content of any session."*
//! `SessionInfo::title` rides that channel to every watching client,
//! including clients that never subscribed to the session it names. So a
//! title derived from a transcript, sent on `watch`, would put one
//! session's content in front of a client that never asked for it. The
//! engine is forbidden from doing it by name:
//! `eave-engine/tests/watch.rs::the_engine_never_mines_a_title_out_of_the_transcript`
//! plants a canary in the first prompt and asserts, at the raw-byte level,
//! that no fragment of it ever reaches a `sessions` frame.
//!
//! **The line is AUTHORSHIP, not content.** The very same words are a
//! perfectly good title when the operator typed them
//! (`an_operator_title_is_a_durable_session_fact`), because then a human
//! chose to publish them. So eave may *propose*, and only a human may
//! *apply* — which is exactly what seeding the rail's rename field does:
//! the proposal is a local string until Enter turns it into a session fact
//! through the ordinary `Command::Rename` path.
//!
//! **Two structural consequences, both deliberate.**
//!
//! 1. This lives in `eave-app`, the one crate `eave-engine` does not
//!    depend on — it links `eave-core` and `eave-wire`, so a helper placed
//!    in either of those would sit inside the engine's own binary, one
//!    call away from the thing the invariant forbids. Pinned by
//!    [`tests::the_engine_cannot_even_link_this`].
//! 2. A proposal exists only for a session whose transcript this client
//!    already holds. eave does not go and fetch one to name a row: that
//!    would mean subscribing to every session, which is the fan-out
//!    `watch` exists to avoid. A row you have never opened opens its
//!    rename field empty, exactly as before.

use eave_wire::model::Row;

/// The cap the engine will apply anyway (`eave_core::store::TITLE_MAX_CHARS`).
///
/// Proposing something longer would mean showing the operator a string the
/// store would silently shorten — a field whose contents and whose result
/// disagree.
pub const PROPOSAL_MAX: usize = eave_core::store::TITLE_MAX_CHARS;

/// What to put in the rename field for a session that has no name.
///
/// The first thing the operator asked, folded and cut. **Their own words,
/// which is the point** — this is not a summary of the conversation and
/// deliberately not an attempt at one. A summary is a claim about what the
/// session turned out to be; the opening ask is a fact about what it was
/// for, it is already the sentence the operator would have typed, and it
/// costs no tokens and no model call.
///
/// Whitespace is folded exactly the way `eave_core::store::normalize_title`
/// folds it, so what the field shows and what the store keeps are the same
/// string. A prompt whose first line is a greeting therefore still reaches
/// the ask behind it, instead of proposing `Hi.`
pub fn propose(rows: &[Row]) -> Option<String> {
    let asked = rows.iter().find_map(|row| match row {
        Row::User { text, .. } => Some(text.as_str()),
        _ => None,
    })?;
    let folded = asked.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.is_empty() {
        return None;
    }
    Some(cut(&folded, PROPOSAL_MAX))
}

/// Cut to `max` characters, backing off to a word boundary.
///
/// **No ellipsis.** A snippet in a tooltip says it was cut because the
/// reader cannot get the rest; a title is a NAME, and `…` stored in one
/// reads ever after as part of the name. The operator is looking straight
/// at this string with a caret in it, so the honest move is to hand them a
/// clean phrase to accept or extend.
fn cut(folded: &str, max: usize) -> String {
    if folded.chars().count() <= max {
        return folded.to_string();
    }
    let head: String = folded.chars().take(max).collect();
    // A single word longer than the cap has no boundary to find, and the
    // hard cut is then the only answer there is.
    match head.rfind(' ') {
        Some(at) if at > 0 => head[..at].to_string(),
        _ => head,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eave_wire::model::Delivery;

    fn user(text: &str) -> Row {
        Row::User {
            text: text.into(),
            delivery: Delivery::Delivered,
        }
    }

    #[test]
    fn the_proposal_is_the_operators_own_opening_ask() {
        let rows = vec![
            user("rename the rail rows"),
            Row::Assistant {
                text: "sure".into(),
                live: false,
            },
            user("now do the other thing"),
        ];
        assert_eq!(propose(&rows).as_deref(), Some("rename the rail rows"));
    }

    /// Folded the way the store folds it, so the field and the stored
    /// title are the same string — and a greeting on line one does not
    /// become the name of the session.
    #[test]
    fn newlines_fold_to_spaces_rather_than_ending_the_title() {
        let rows = vec![user("  Hi.\n\nrename   the rail rows\n")];
        assert_eq!(propose(&rows).as_deref(), Some("Hi. rename the rail rows"));
        assert_eq!(
            eave_core::store::normalize_title(&propose(&rows).unwrap()).as_deref(),
            propose(&rows).as_deref(),
            "a proposal must survive the store's own normalisation unchanged"
        );
    }

    #[test]
    fn a_long_ask_is_cut_at_a_word_and_never_mid_word() {
        let long = "update the ACP decoder so that it handles the plan session \
                    update and puts the checklist on the row it belongs to";
        let out = propose(&[user(long)]).unwrap();
        assert!(out.chars().count() <= PROPOSAL_MAX, "{out:?}");
        assert!(
            long.starts_with(&out),
            "the cut must be a prefix of what was asked: {out:?}"
        );
        assert!(
            !out.ends_with(' ') && long[out.len()..].starts_with(' '),
            "cut mid-word: {out:?}"
        );
        assert!(!out.ends_with('…'), "a name is not a snippet: {out:?}");
    }

    /// One word longer than the cap has no boundary to back off to, and a
    /// hard cut beats proposing nothing.
    #[test]
    fn one_very_long_word_is_cut_hard() {
        let word = "a".repeat(PROPOSAL_MAX * 2);
        let out = propose(&[user(&word)]).unwrap();
        assert_eq!(out.chars().count(), PROPOSAL_MAX);
    }

    #[test]
    fn a_session_with_nothing_asked_yet_proposes_nothing() {
        assert_eq!(propose(&[]), None);
        assert_eq!(
            propose(&[Row::Assistant {
                text: "I have thoughts".into(),
                live: false
            }]),
            None,
            "the agent's words are never a proposal — only the operator's"
        );
        assert_eq!(propose(&[user("   \n  ")]), None);
    }

    /// The structural half of the invariant: the crate this lives in is
    /// one `eave-engine` cannot call. Moving `propose` into `eave-core` or
    /// `eave-wire` would compile, would pass every test above, and would
    /// put transcript-mining code inside the engine's own binary — so the
    /// guard is on the dependency, where the mistake actually is.
    #[test]
    fn the_engine_cannot_even_link_this() {
        let manifest = include_str!("../../eave-engine/Cargo.toml");
        assert!(
            !manifest.contains("eave-app"),
            "eave-engine now depends on eave-app: `propose` is reachable \
             from the process that spec 05 §Watch forbids to derive a title"
        );
    }
}
