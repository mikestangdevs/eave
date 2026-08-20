# Stage AB — the rail says which session

Two sessions in the same repo, under the same provider, in the same state
rendered as one row drawn twice. `SessionInfo` carried no title, no
preview, and no activity time — only `created_ts_ms`. This slice gives the
rail something to say.

It does **not** give it the thing the build directive asked for, and the
reason is the first caveat.

---

## Caveats

### 1. The title is not derived from the transcript, and cannot be

The directive asked for a title derived from the session's first
`UserSent`, capped at ~64 characters, stamped on `SessionInfo`. The gate
was to check that against the raw-byte watch canary first. It fails, and
not marginally.

`watch` is the protocol's only cross-session broadcast. The invariant on it
(spec 05 §Watch) is that it may carry facts ABOUT a session and never a
byte of any session's transcript, and
`eave-engine/tests/watch.rs::watch_channel_never_carries_transcript_content`
proves it on the actual socket bytes rather than on decoded types. That
test drives a session through its whole life while a raw watcher records
everything, and it plants its marker like this:

```rust
c.send("s1", &format!("{CANARY} do the thing"))
```

The canary is the **first token of the first prompt**. So a title made of
the first prompt's leading words does not merely violate the invariant in
principle — it puts `LEAK_CANARY_7f3a` on the watch channel and fails that
test by name. There is no cap short enough to help: every non-empty prefix
of that prompt contains the marker. The directive's own fallback suggestion
of "leading words only" is dead for exactly the same reason, and that is
worth stating plainly, because it reads like a safe middle ground and is
not one.

The canary was not weakened. The feature was.

**What shipped instead:** `SessionInfo.title: Option<String>`, set only by
an explicit `rename` request and persisted in `meta.json`. The engine reads
no journal to fill it. `None` — nobody named it — is the default and the
honest one.

This is the same line the codebase already drew for approvals, which is
what makes it a precedent rather than an improvisation:
`pending_approvals` is a count on `watch`, while the tool name and the path
that count refers to are transcript and travel only on that session's own
`subscribe`. Here: a label the operator chose may cross; the words the
agent was given may not.

The cost is real. A session is unnamed until someone names it, so the
automatic version of this feature does not exist and cannot without
retiring the invariant. See caveat 3 for what makes an unnamed row
distinguishable anyway.

### 2. `last_activity_ms` stamps state changes only, and that is load-bearing

The obvious implementation — timestamp the last journal record — is a
quieter version of the same leak. A timestamp that advanced per record
would let any watcher read the *rate* of any session's transcript off the
watch channel: not content sent, but content inferred, which the invariant
is equally about.

So the stamp moves only where the state name a watcher already reads
actually changes. `SessionHandle::settle_state` takes the reported state
before a mutation and compares it after; equal means no stamp. The
equality check is the design, not an optimization — the attention monitor
re-publishes on evidence updates as well as on transitions, so an
unconditional stamp would tick on the monitor's cadence. As built, the
field is a strictly redundant encoding of something `state` already told
every watcher, which is what puts it inside the invariant rather than
beside it.

`activity_is_stamped_by_state_changes_and_nothing_else` asserts the
negative half: the stamp holds still across a stretch of heavy streaming
because the state name held still.

### 3. There is no rename affordance in the app yet

`rename` exists end to end — wire method, engine, `Client::rename`,
persistence, tests — and nothing in the GUI calls it. eave has exactly one
text field (the composer, which Slice K already doubles as the deny-reason
field), and giving the rail an inline editor is a text-input slice, not a
line of this one.

That leaves the feature honestly half-delivered rather than dishonestly
whole, so the rail was made to solve its stated problem without it:
**the row's meta line now carries the session's 8-char id.** That is pure
identity, the one class of fact `watch` has always been allowed to carry,
and it makes two untitled sessions in one repo distinguishable today with
no operator action at all. It is dropped where the cwd already ends in the
same sid8 — every isolated session — so the line never says the same thing
twice.

The next slice for this is a rename box. Until it lands, titles are
reachable only through the client API.

### 4. Row order is decided once, on arrival

`last_activity_ms` sorts the rail, but only when a row arrives. After that
the row keeps its place for as long as the session exists, whatever its
state does.

This is deliberate and it is a trade. Re-sorting live is the bug comet
shipped: it ordered its rail by attention, so merely opening a session
changed that session's state, which re-sorted the rail, which slid the row
out from under the pointer that had just clicked it. The alternative —
freezing the order only while the pointer is over the rail — makes the
layout depend on hover state and is much harder to assert.

The consequence to know about: a long-lived rail drifts out of activity
order, and only a reconnect or a restart re-sorts it. That was judged the
better failure. The thing an operator tracks a session by is where they
last saw it, and a list you cannot reliably click is worse than a list in
an order that is merely old.

### 5. `docs/` did not exist

None of the documents the build directive named — `docs/spec/05-wire.md`,
`docs/spec/03-ui.md`, `docs/spec/04-slice.md`, `docs/progress.md`,
`docs/reports/stage-g-sidebar.md` — exist in this repository or anywhere in
its git history. The tree has never contained a `docs/` directory. This
report is the first file in one.

Everything above was therefore established from the code, which carries the
specs' content in its doc comments in considerable detail —
`frames.rs`'s §Watch commentary and `watch.rs` itself were more than
sufficient to settle the gate. The §Traps entry about "doesn't overflow"
versus "doesn't touch" was taken at face value from the directive and
honoured directly: the fit test asserts clearance.

---

## What shipped

**`eave-wire`.** `SessionInfo.title: Option<String>` and
`SessionInfo.last_activity_ms: Option<u64>`, both `#[serde(default,
skip_serializing_if)]` so a pre-Stage-AB engine's frame still parses and an
unnamed session puts no `title` key on the wire at all. A new `rename`
method carrying `Option<String>` — clearing a name is the same verb as
setting one, which is what stops the two from ever disagreeing — and
`RenameOk`, which returns the whole `SessionInfo` so a caller reads back
the *normalized* title rather than assuming its own string landed.

**`eave-core`.** `Meta.title`, plus `normalize_title` and
`TITLE_MAX_CHARS`. Titles fold to a single trimmed line (the rail draws one
line; a stored newline would be an invisible difference between two
identical-looking titles) and cap at 64 **characters**, never bytes, so the
cap cannot split a multi-byte character.

Also `Meta::save_keeping_title`, which fixes a bug the feature would
otherwise have shipped with. `meta.json` is rewritten wholesale by two
writers with different concerns: the state machine, on every transition,
from a snapshot taken at spawn; and `rename`, on one field. Either erases
the other unless one rule holds — **`rename` is the only writer of `title`,
and every other writer preserves it.** Without it, naming a session and
then sending it a message loses the name, silently.

**`eave-engine`.** `Shared::reported_state` (extracted from `info()`, which
had the rule inline), `SessionHandle::settle_state`, and the three
transition sites that call it: spawn, terminal settle, and attention.
`Engine::rename` is the sole writer of the title and deliberately does
*not* stamp activity — naming a session is not the session doing anything,
and sorting a rail by "most recently renamed" would be a lie about which
agent is working. On load, `last_state_change_ms` seeds the stamp from the
last `Closed` record, which is durable and therefore identical across a
restart; only `Closed` counts, because reading any other record's timestamp
would reintroduce caveat 2.

**`eave-app`.** The reconnect fallback — what eave selects when the held
selection has vanished — now picks the most recently *active* session
rather than the most recently created one, which after a restart is rarely
the one you were working in. Sessions that never ran fall back to creation
order, so the old behaviour is the floor rather than a special case.

`fold_rail` replaces the in-render sort: facts still fold by
replacement from every snapshot, and only row *position* carries across,
because position is not an engine fact. `row_label` puts the title in the
provider label's slot rather than on a line of its own — which is what makes
"an unnamed session renders no empty line" true by construction rather than
by a conditional. The provider identity is not lost with it; the chip beside
the label is drawn from the same `provider_identity` call.

## Acceptance

| Claim | Where |
| --- | --- |
| Two sessions, same cwd, same provider → distinguishable rows | `ui_harness::two_sessions_in_one_repo_do_not_render_as_the_same_row` |
| Title truncates rather than pushing the badge out — asserted as CLEARANCE | `ui_harness::a_long_title_clips_itself_before_it_crowds_the_badge` |
| Empty journal → no title, no empty line | `ui_harness::an_unnamed_session_states_no_title_and_draws_no_empty_line` |
| Order stable across a state transition, and the click still lands | `ui_harness::a_state_change_never_moves_a_row_under_the_pointer` |
| Canary content crosses `subscribe`, never `watch`, with the new fields present | `watch::watch_channel_never_carries_transcript_content` (unchanged) |
| The engine never derives a title from the journal | `watch::the_engine_never_mines_a_title_out_of_the_transcript` |
| An operator title survives a turn and a restart | `watch::an_operator_title_is_a_durable_session_fact` |
| Activity stamps state changes and nothing else | `watch::activity_is_stamped_by_state_changes_and_nothing_else` |

The fit test asserts clearance in the sense §Traps asks for: the badge is
measured at real glyph widths and proven to have kept its whole word, the
title is proven to have genuinely needed more room than it got, and the
space between the two is required to be at least the row's own gap — not
merely non-negative. The row is then checked to end inside the rail.

## Verification

`cargo test --workspace`, unpiped, exit code read from cargo itself rather
than from the end of a pipe: **353 passed, 0 failed, 3 ignored, exit 0.**
337 before this slice, so 16 tests are new — 3 in `store.rs`, 2 in
`frames.rs`, 3 in `watch.rs`, 4 pure-function tests in `ui.rs`, and 4
through the hermetic UI harness.

`cargo fmt --all --check` and `cargo clippy --workspace --all-targets --
-D warnings` both exit 0.

`files_channel_never_crosses_sessions` — the known pre-existing flake —
passed on both full runs. It was not chased.

The suite was run twice: once before and once after a rename and two
comment fixes, because both landed while the first run was still
compiling and a result that describes a tree you did not commit is not a
result. The numbers above are the second run, on the committed tree.

## The number

`TITLE_MAX_CHARS = 64` is justified rather than asserted:
the test measures a title at the cap against the label slot's actual width
and requires the cap to be *past* what the row can draw. Bounding the wire
and bounding the display are different jobs, and a cap tight enough to be
the visual limit would silently destroy the operator's text.
