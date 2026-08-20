//! The eave visual language — every color, size, and metric the shell
//! paints. Nothing outside this module states a hex or a px design value.
//!
//! Stance: eave reads as an instrument over a delegated process, not a
//! chat app. Every element states an observed fact; color speaks only
//! when state deserves attention (live activity, warning, failure);
//! nothing performs or decorates. Palette and type scale derive from
//! waku's (solid dark surfaces, `.SystemUIFont` prose, white-alpha
//! hairlines) — the closest reference that works on our upstream gpui
//! pin. (That "locked opaque window" premise is long gone: the fork pin
//! reversed 2026-08-17 and brought the blur with it, and the opaque build
//! was deleted 2026-08-19. The waku lineage of the TYPE and hairlines
//! stands; the surfaces are all washes now.)

use gpui::{hsla, px, rgb, BoxShadow, FontWeight, Hsla, Pixels};

// ---------------------------------------------------------------------------
// Colour math — **ported from comet** (`comet/crates/ui/src/theme.rs`
// 780–1050), not re-derived.
//
// This is the correction to how every eave design slice before this one
// worked. Nineteen of them invented `hsla(…)` values from first principles
// and then wrote a contrast floor to justify the value after the fact, and
// the result was 434 tests and a flat app. comet solved the same problem
// once, in a colour space where "one step lighter" and "same hue, less
// chroma" are operations rather than guesses, and named every accent after
// the Tailwind step it is. The values below are copied with their citations.
//
// **What is deliberately NOT ported**, because eave is dark-only and the
// rest would double the surface for nothing: `Appearance::Light` and every
// paired-light token, `INK_FILL_SCALE`, `INK_HAIRLINE_SCALE`, `band_for`.
// And one thing that could have been ported and should not be — comet's
// TEXT ramp (`neutral(0.922)`/`neutral(0.708)`/`neutral(0.556)`). Those are
// solved against an opaque `grey(6)` panel; eave's are solved against a
// translucent glass ground swept over 52 desktop brightnesses, and
// `neutral(0.556)` is *darker* than eave's quiet tier, which already sits at
// 3.42:1 against a 3.0 floor. Porting them would fail eave's own suite on
// the first run. Method ported, values kept.

/// An exact achromatic tone from an 8-bit channel value: `grey(0x0d)` is
/// `#0d0d0d`. For surfaces and inks matched against a sampled reference.
fn grey(value: u8) -> Hsla {
    hsla(0.0, 0.0, value as f32 / 255.0, 1.0)
}

/// A neutral (chroma 0) oklch tone. Chroma 0 means `r == g == b` exactly, so
/// this goes straight to an achromatic `Hsla` and skips the hue math, which
/// would otherwise return float-noise saturation.
fn neutral(lightness: f32) -> Hsla {
    let [v, _, _] = oklch_to_srgb(lightness, 0.0, 0.0);
    hsla(0.0, 0.0, v, 1.0)
}

/// Translucent neutral fill ink — every hover wash, selection wash, hairline
/// and rung in the app.
///
/// **Why this exists at all: eave was tinting its neutrals and hiding its
/// accents, which is exactly backwards.** Seven tokens were
/// `hsla(220/360, 0.10, 0.90, α)` — a blue-grey at 10 % saturation on every
/// neutral surface in the window. A cast on everything means nothing is
/// neutral, so nothing else can be coloured, and that is most of why the app
/// read as one flat material. comet's neutrals are literally achromatic and
/// its accents carry real chroma.
///
/// **Two deliberate divergences from comet's `ink`,** both stated rather
/// than absorbed:
///
///  - comet's dark branch is `hsla(0, 0, 1.0, α)`; eave's is `0.90`. eave's
///    alphas were each solved so their rung composites to a specific value
///    over the glass ground, and moving the white would move all five rungs
///    at once for no measured gain. The tint is what was wrong, not the
///    lightness.
///  - comet needs `ink(0.0)` because "fills must never rest on transparent
///    black in dark mode" — a straight interpolation from transparent black
///    flashes dark mid-fade. eave's `wash_rest` *is* transparent black and
///    does not flash, because `motion::mix` blends PREMULTIPLIED, so a
///    zero-alpha end contributes nothing but its transparency. Same bug, and
///    eave's fix is the more general one; it is already tested.
///
/// The cost of the de-tint, measured: at l .90 a saturation of .10 spreads
/// the channels by `(1 − |2l − 1|)·s` = 0.02, i.e. 5/255 between red and
/// blue at full strength and ≤ 0.5/255 through the densest wash. **No
/// surface or ink assertion moved.** The tint was never buying contrast.
fn ink(alpha: f32) -> Hsla {
    hsla(0.0, 0.0, 0.90, alpha)
}

/// An oklch colour (CSS Color 4 notation: L 0..1, C, H in degrees) as `Hsla`.
fn oklch(l: f32, c: f32, h_deg: f32) -> Hsla {
    let [r, g, b] = oklch_to_srgb(l, c, h_deg);
    let (h, s, l) = rgb_to_hsl(r, g, b);
    hsla(h, s, l, 1.0)
}

/// oklch → sRGB, each channel 0..1 and gamut-clipped.
/// Reference: Björn Ottosson's OKLab definition — the matrices CSS Color 4
/// uses. `oklch_accents_match_reference` pins the ones this file relies on.
fn oklch_to_srgb(l: f32, c: f32, h_deg: f32) -> [f32; 3] {
    let h = h_deg.to_radians();
    let a = c * h.cos();
    let b = c * h.sin();

    // OKLab → LMS (cube roots undone)
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);

    // LMS → linear sRGB
    let r = 4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_93 * s3;
    let g = -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_4 * s3;
    let b = -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3;

    [gamma_encode(r), gamma_encode(g), gamma_encode(b)]
}

fn gamma_encode(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB (0..1 components) → HSL, all components 0..1 (gpui's convention).
fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let delta = max - min;
    if delta < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };
    let h = if (max - r).abs() < f32::EPSILON {
        ((g - b) / delta).rem_euclid(6.0)
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    } / 6.0;
    (h, s, l)
}

// ---------------------------------------------------------------------------
// The glass ground — ruled by the user 2026-08-17, and since 2026-08-19
// the ONLY ground: the opaque build was deleted outright ("i never want
// it"), along with the `EAVE_VIBRANCY` switch that selected it. There is no
// fallback and no second render mode. The window opens Blurred
// unconditionally and every surface token below is a wash.
//
// The retune, not the flag, is the substance. Every element paints over
// the one root canvas, and in-window compositing is plain alpha (the blur
// samples only what is BEHIND the window — in-app scene-sampling blur
// stays never). That forces the architecture:
//
//   - The canvas is the single glass layer: near-black `#0A0A0A` at
//     α 0.80 (comet's frost is grey(8) @ 0.80 — the reference the
//     2026-08-17 ruling pointed at). The tint must add almost no gray
//     of its own: a mid-gray tint (the first cut used `#212121`) washes
//     the desktop's hue toward gray, and the whole window reads as flat
//     gray instead of dark colored glass. The desktop participates at
//     20% through the ground, everywhere.
//   - Elevation rungs CANNOT stay independent plates. A plate over the
//     glass gains less backdrop light than the ground as the wallpaper
//     brightens (an opaque plate gains none), so past a computable
//     brightness the ladder inverts — raised blocks would read sunken.
//     Instead each rung becomes a white-alpha wash over the shared
//     ground (the [`overlay`] idiom): its step is β·(white − ground),
//     which compresses on bright wallpapers but can never invert, on
//     any wallpaper. β is solved so the step over a black backdrop
//     equals the opaque ladder exactly.
//   - `inset` inverts the trick: a black wash, denser ink — darker than
//     the ground under any wallpaper.
//   - Chrome that floats over in-window content (the ↓-bottom pill)
//     keeps the pre-blended opaque plate: any alpha there ghosts the
//     log through it, and no blur will save it.

// The wash primitives. Private, and still worth keeping separate from the
// public tokens above: they are the values the elevation tests compose
// directly, so a rung can be checked without going through its semantic
// name. (They were private because the public fns used to branch on an env
// flag; that flag is gone, the separation earned its keep anyway.)

/// Glass ground: α 0.80 (0xCC), source `#0A0A0A` — near-black so the
/// blurred desktop's hue carries the surface (comet parity; see above).
fn glass_canvas() -> Hsla {
    // `grey(10)` is `oklch(0.145 0 0)` — Tailwind neutral-950, and comet's
    // own app background, which is where the 2026-08-17 "comet parity"
    // ruling actually came from. Written achromatically now rather than as
    // an rgba literal that happened to be neutral.
    grey(0x0a).opacity(0.80)
}
/// The content well's darkening wash, behind [`content_plate`].
fn wash_content() -> Hsla {
    hsla(0.0, 0.0, 0.0, 0.25)
}
/// The raised rung's wash. The β values were solved for the old `#212121`
/// ground's steps; over the darker comet-parity ground they give slightly
/// tighter steps at black, which is fine — the ordering (not the absolute
/// step) is the invariant, and the sweep test proves it on every backdrop.
fn wash_raised() -> Hsla {
    ink(0.044)
}

// ---------------------------------------------------------------------------
// Surfaces — all washes over the glass ground (see above)

/// Window ground; the transcript sits directly on it.
pub fn canvas() -> Hsla {
    glass_canvas()
}
/// Raised blocks: user directive blocks, pills, cards.
///
/// **The only rung of the old five-rung ladder that anything paints.**
/// `panel()`, `composer()` and `surface_modal()` were retired in Slice AG
/// with zero call sites each — `surface_modal`'s own doc had said so out
/// loud ("L3 surface — reserved; no modal exists yet") while
/// `every_rung_is_a_step_you_can_see` and
/// `glass_ladder_never_inverts_on_any_backdrop` spent a 52-backdrop sweep
/// constraining all three. Both tests now measure the two ladders eave
/// actually paints: `canvas → content_plate → raised` in the reading column
/// and `chrome_glass → menu_glass` in the floating chrome.
pub fn raised() -> Hsla {
    wash_raised()
}

// ---------------------------------------------------------------------------
// In-app glass (fork pin, Stage H) — chrome FLOATING OVER THE LOG paints
// a backdrop blur (the fork's BackdropBlur primitive) under a translucent
// tint. This supersedes the vibrancy spike's occlusion compromise: a
// floating surface no longer needs to be opaque to be legible, because
// the content beneath it is blurred before the tint composites.
//
// The retune logic, distinct from the vibrancy ladder's: here the
// backdrop is our OWN dark content (blurred log ≈ canvas 26/255 in empty
// regions, ~56/255 through text-dense regions), not an arbitrary
// desktop. A single tint alpha for the whole chrome rung keeps the rung
// coherent; the hex stays the rung's own value so hue identity holds.
// α 0.72 is the most-solid tint (stance: chrome is an instrument
// surface) at which the blurred content still registers: surface
// luminance breathes ~36→45/255 as content passes beneath (the depth
// cue), while body text keeps >8:1 and ghost text ~2:1 against it —
// the same ratios the opaque plates gave. At 0.80 the breath drops
// under ±6/255 (imperceptible); below ~0.6 ghost-tier text fogs out.
// These tints never depended on the window mode: in-app blur samples the
// scene, window vibrancy handles what's behind the window.

/// Backdrop-blur radius for L2 floating chrome (comet's composer class
/// runs 12–16; menus/modals take 44 — reserved with L3).
pub const BLUR_CHROME: f32 = 16.0;
const CHROME_GLASS_ALPHA: f32 = 0.72;

fn glass_tint(hex: u32) -> Hsla {
    let c: Hsla = rgb(hex).into();
    c.opacity(CHROME_GLASS_ALPHA)
}
/// L2 glass: floating chrome over the log (composer, ↓-bottom pill).
/// Retuned with the comet-parity ground (2026-08-17): the old `#282828`
/// tint read as a light-gray slab once the ground went near-black; this
/// lands the card a quiet step above the ground, like comet's input
/// pill, while α 0.72 keeps the occlusion that makes text legible over
/// scrolled log content.
pub fn chrome_glass() -> Hsla {
    glass_tint(0x141414)
}
/// The ↓-bottom pill under the pointer: the same glass, one step
/// lighter. Alpha is unchanged — the pill's whole job is to occlude the
/// log scrolling behind it, and a hover that thinned it would undo that
/// (Slice M).
pub fn chrome_glass_hover() -> Hsla {
    glass_tint(0x232323)
}
/// Attention/connection plates as glass — same hexes as the opaque
/// plates, same single chrome alpha.
pub fn warning_plate_glass() -> Hsla {
    glass_tint(0x312f2b)
}
pub fn warning_plate_strong_glass() -> Hsla {
    glass_tint(0x3e3930)
}
pub fn danger_plate_glass() -> Hsla {
    glass_tint(0x332c2c)
}
/// The content well (comet's shell model, adopted 2026-08-17): the
/// transcript + composer pane floats over the glass ground as an inset
/// rounded card, darkened by a black wash so the desktop's bleed dims
/// under the reading column (text keeps a solid floor) while the rails
/// sit on the bare frost around it. Opaque fallback: the plain canvas —
/// the hairline card outline alone carries the seam.
pub fn content_plate() -> Hsla {
    wash_content()
}

/// Sunken panels: code blocks.
/// A sunken plate's fill. Transparent on purpose.
///
/// This was a black wash (`wash_inset`, α 0.205) and it could not work: the
/// glass ground is already near-black (`#080808` over a black desktop), so
/// darkening it moves the pixel **2/255** — at or under the perceptual
/// threshold — and even doubling the alpha to 0.45 only buys 3.6/255 at the
/// worst backdrop. There is no room below the ground to sink into.
///
/// So a sunken plate is read from its **edge** instead, which is where every
/// consumer was already carrying a border. comet hit the same wall from the
/// other direction and reached the same conclusion — their `theme.rs:937-955`
/// records three rejected drop-shadow recipes and the reason ("drop shadows
/// are filled rects painted BEHIND the element, and behind a 5% fill they
/// showed straight through as an opaque dark plate").
///
/// A per-edge rim light — a brighter hairline on the top edge only — is the
/// next refinement and is deliberately not in this slice.
///
/// Slice AG: the rim in question is [`border`]. `border_inset` — the token
/// Slice U introduced for this job and `a_sunken_plate_is_read_from_its_rim`
/// measured — had **zero call sites**: all four sunken plates
/// (`ui.rs` fence body, tool payload, approval payload, quote block) draw
/// `.border_color(theme::border())`. It was retired and the test re-pointed
/// at what is painted; `border()` rises 11.8→15.3/255 across the sweep and
/// clears the floor everywhere, so nothing moved on screen.
pub fn inset() -> Hsla {
    gpui::transparent_black()
}

/// The resting end of a hover wash on an unfilled control (Slice N2) —
/// what such a control actually paints today, which is nothing: black at
/// zero alpha.
///
/// It is named rather than written as `transparent_black()` at thirteen
/// call sites because it is the *reason the blend is premultiplied*. This
/// colour carries black. Interpolate the components of `wash_rest → overlay`
/// straight and the midpoint is a half-alpha grey, so a light wash appearing
/// over a dark surface flashes dark on its way in; premultiplying weights
/// each end by its own alpha, so a zero-alpha end contributes nothing but
/// its transparency (`motion::mix`).
pub fn wash_rest() -> Hsla {
    gpui::transparent_black()
}

/// The three state rungs are ONE LADDER, and the floor between them is the
/// same [`STATE_DELTA_FLOOR`] that separates each from its ground. Slice U
/// pinned each wash against the ground and stopped there, so `overlay` and
/// `overlay_strong` were left **2/255 apart** — a hovered row and the
/// selected row were the same colour to the eye, and the test that was
/// supposed to catch exactly that could not see it, because it only ever
/// compared a wash to what was underneath it.
///
/// `adjacent_states_are_a_step_apart` is the missing assertion.
///
/// Hover wash on an unfilled control: the first rung off the ground.
pub fn overlay() -> Hsla {
    ink(0.08)
}
/// The standing fill of a SELECTED thing — one full step above [`overlay`],
/// so that "the row under my cursor" and "the row I am in" are never the
/// same colour. Was `0.09`.
pub fn overlay_strong() -> Hsla {
    ink(0.14)
}
/// One step above [`overlay_strong`], for an element whose RESTING fill
/// is already `overlay_strong` — the unarmed send button. A hover wash
/// that composites over a surface is the app's idiom, but an element
/// with its own `bg` replaces rather than composites, so the few filled
/// controls need their hovered fill named outright (Slice M). Was `0.14`,
/// which is now what the rung below it carries.
pub fn overlay_strongest() -> Hsla {
    ink(0.20)
}

// ---------------------------------------------------------------------------
// Elevation — separates layers, never decorates. On a dark canvas the
// mechanism is SURFACE LUMINANCE: each layer steps lighter, and its
// hairline steps up in alpha with it; a shadow is only a minor accent
// on L2/L3, never the carrier (a dark shadow on a dark ground is
// invisible). The ladder, below the canvas upward:
//
//   inset  #151515  sunken plates (code)          hairline α.07
//   L0     #1A1A1A  canvas — the log's ground
//   L1     #232323  raised-in-log (directives,     hairline α.07
//                   pills); never shadowed — per-
//                   row shadows decorate + cost
//   L2     #282828  chrome above the log           hairline α.14 + accent
//                   (composer, floating pills)     shadow
//   L3     #2E2E2E  modal layer (reserved)         hairline α.20 + accent
//
// **Those hexes are REFERENCE VALUES, not a code path** (2026-08-19). eave
// shipped an opaque build alongside the glass one until then, and the
// washes were tuned so that each rung composites to its hex over the glass
// ground — that is where these numbers come from and why each wash has the
// alpha it has. The opaque build is gone by ruling: glass is the only look,
// `EAVE_VIBRANCY` no longer exists, and there is no fallback. The hexes stay
// because deleting them would delete the derivation of every alpha below.
//
// **The L2 and L3 SURFACES are gone (Slice AG); the L3 hairline is not.**
// `panel`, `composer` and `surface_modal` had zero call sites between them
// while two flagship tests swept 52 backdrops over exactly those rungs. The
// ladders eave paints are `canvas → content_plate → raised` in the reading
// column and `chrome_glass → menu_glass` in the floating chrome; the tests
// measure those now. `edge_modal` stays because `overlay::menu_layer`
// genuinely draws it, which is the whole distinction being made here.

/// L3 hairline — the popover's rim (`overlay::menu_layer`).
pub fn edge_modal() -> Hsla {
    ink(0.20)
}

fn shadow_ink(alpha: f32) -> Hsla {
    hsla(0.0, 0.0, 0.0, alpha)
}
/// L2 accent shadow (minor — luminance carries the layer).
pub fn shadow_panel() -> Vec<BoxShadow> {
    vec![BoxShadow::new(px(0.0), px(2.0), shadow_ink(0.35)).blur_radius(px(8.0))]
}
/// L3 accent shadow — reserved.
pub fn shadow_modal() -> Vec<BoxShadow> {
    vec![BoxShadow::new(px(0.0), px(8.0), shadow_ink(0.50)).blur_radius(px(24.0))]
}

// ---------------------------------------------------------------------------
// Edges

pub fn border() -> Hsla {
    ink(0.07)
}
pub fn border_strong() -> Hsla {
    ink(0.14)
}

// ---------------------------------------------------------------------------
// Text tiers
//
// **An ink is chosen for the SURFACE it lands on, not for the tier it
// belongs to** (Slice AG). Slice U solved these two greys against the bare
// glass ground and stopped there, and it solved them to *fit*: at the bright
// end of the backdrop sweep `text_secondary` measured 4.71:1 against a 4.5
// floor and `text_tertiary` 3.04:1 against a 3.0 floor. `#a8a8a8` clears 4.5:1
// only up to a surface of **61/255** and the bare ground's bright end is
// **59.0/255** — two units of headroom. So *any* surface above the ground
// broke both tiers by construction, and eighteen drawn combinations were
// below their floor on `aac8cf4`.
//
// Two consequences are written into the values below:
//
//  1. **Clearance, not fit.** Each tier now clears its floor by ≥0.4 of ratio
//     at the worst backdrop. The margin is derived, not chosen: Slice AF
//     measured the token→pixel skew at 1.8/255 against a 4.0/255 tolerance
//     (`scripts/af-state-proof.sh`), and 4/255 of surface error costs up to
//     **0.33** of contrast ratio at these surfaces. An arithmetic claim that
//     clears by less than the instrument's own error is not a claim about
//     what a person sees. Both moves are 4/255 and 9/255 — smaller than that
//     same tolerance, so the guarantee moves and the picture does not.
//  2. **A wash needs its own rung.** See [`text_secondary_on_wash`].
//
// `every_ink_clears_its_floor_on_every_surface_it_lands_on` is the assertion;
// it enumerates real `.bg(…)`/`.text_color(…)` sites, not tokens.

pub fn text() -> Hsla {
    grey(0xe2)
}
/// The secondary tier on a BARE surface — the glass ground, the content
/// well, a plate. Was `#a8a8a8`, which cleared [`TEXT_CONTRAST_FLOOR`] by
/// 0.21 at the bright end of the sweep; `#acacac` clears it by 0.44.
///
/// It is also the tier the QUIET ink resolves to wherever a state wash can
/// land — see [`text_tertiary`].
pub fn text_secondary() -> Hsla {
    grey(0xac)
}
/// Placeholders, disabled, structural hints — the quietest tier that is
/// still allowed to carry a fact.
///
/// There used to be a fourth tier below this one, `text_ghost` (`#575757`),
/// and it was the single least readable thing eave drew: **1.55:1** against
/// the glass ground at the bright end of the backdrop sweep, on ~25 call
/// sites including diff context lines and the "no sessions" empty state. It
/// was retired rather than raised, because the value that would clear the
/// floor (`#858585`) is this tier — two names for one colour is not a
/// hierarchy. Three tiers, each of which clears [`TEXT_CONTRAST_FLOOR`].
///
/// Was `#858585`, at 3.04:1 against a 3.0 floor — fit, not clearance.
/// `#8e8e8e` reads 3.42:1.
///
/// **This tier may not be drawn where a state wash can land on it.** Solving
/// a quiet ink for `overlay_strong` at the bright end gives `#ababab`, which
/// is the secondary tier — the `text_ghost` retirement criterion, reached
/// from the other direction. So the rule is the demotion rather than a new
/// token: on a washable surface the quiet ink resolves to
/// [`text_secondary`] and the secondary ink to [`text_secondary_on_wash`].
/// The ladder shifts up one rung; it does not gain a rung at the bottom.
pub fn text_tertiary() -> Hsla {
    grey(0x8e)
}

/// The secondary tier where a STATE WASH can land on it — the rail's session
/// label and provider chip, `+ new`, a file row's basename, a diff line's
/// text. Solved against `overlay_strong` over the brightest ground in the
/// sweep (82.5/255), where it reads **4.91:1**.
///
/// Slice AL: the selected rail row is no longer `overlay_strong` — it is
/// [`accent_wash`], which composites *slightly darker* (relative luminance
/// 0.082 against 0.085 at the bright end), so every ink on it gained a
/// little. The surface this tier was solved for is still the worst one it
/// lands on; it is now the hovered file row rather than the selected session
/// row. `every_ink_clears_its_floor_on_every_surface_it_lands_on` enumerates
/// both.
///
/// It exists because the state ladder leaves no room for the plain tier.
/// At the bright end the ground is 59.0/255 and [`STATE_DELTA_FLOOR`] forces
/// three rungs above it, so the cheapest possible ladder still lands its
/// rungs at 67/75/83 — all above the 61/255 ceiling at which `#a8a8a8` last
/// cleared 4.5:1. **Capping the washes cannot fix this even in principle**;
/// it is not merely that capping them would undo Slice AF.
///
/// Sites that can reach the THIRD rung — a hover wash composited over a
/// selected row's `overlay_strong`, 94.1/255 — take [`text`] instead: this
/// rung would read 4.35:1 there. That is the rail's five row affordances
/// (rename, archive, isolate, open, delete), and they are the affordances of
/// the row you are managing, so primary is the right ink anyway (the same
/// call Slice AD made for the markdown table header).
pub fn text_secondary_on_wash() -> Hsla {
    grey(0xce)
}

// ---------------------------------------------------------------------------
// Solid paint — a bright plate and the dark label that goes on it.
//
// **This pair already existed and was called `inverse`/`on_inverse`, which
// named the mechanism instead of the role.** It is the launcher's "Start
// Claude Code" button and the armed send glyph: the most-contrast fill in
// the app, carrying a near-black label. comet has the same pair under the
// names below (`solid: neutral(0.922)`, `on_solid: grey(0x0e)`), and naming
// it for the role is what let [`on_solid`] be reused as the ink on the
// SATURATED fills too — see [`accent`]. Nothing moved on screen: the values
// are unchanged, the call sites are unchanged.

/// The maximum-contrast plate: the primary action's fill.
///
/// Was `#e7e9ec` — a blue-tinted white, 231/233/236. comet's is
/// `neutral(0.922)`, the same tone with the cast taken out, and this is now
/// that. The de-tint is 4/255 on the blue channel and nothing else.
pub fn solid() -> Hsla {
    neutral(0.922)
}
/// The label on any bright fill — [`solid`], [`accent`], or a state pigment
/// used as a chip.
///
/// **A bright fill takes a DARK label, and that is the whole trick this
/// slice turns on.** comet's `on_accent` is near-white because its accent is
/// indigo-500, a dark fill. eave's pigments all sit at l .65–.83, so the
/// legible ink on them is this one, and it clears by a lot: 6.68:1 on
/// [`danger`], 6.17:1 on [`live`], 6.77:1 on [`accent`], 9.96:1 on
/// [`success`], 11.21:1 on [`warning`]. One token rather than an `on_accent`
/// with the same value — two names for one colour is the criterion that
/// retired `text_ghost`.
pub fn on_solid() -> Hsla {
    grey(0x0e)
}
/// The armed send button under the pointer: the same fill, lifted. The
/// only direction available — [`solid`] is already the brightest fill
/// in the app, so hover goes to white rather than adding a wash that
/// would darken it (Slice M).
pub fn solid_hover() -> Hsla {
    grey(0xf7)
}

// ---------------------------------------------------------------------------
// Accent — the app's identity colour, and the only pigment that is not a
// state.

/// eave's accent — **Tailwind violet-400**, `oklch(0.702 0.183 293.541)`,
/// the step comet spends on `code_wash`.
///
/// **The finding that picked it.** comet's accent is `oklch(0.673 0.182
/// 276.935)`, indigo-400, which converts to **`#7c86ff`** — bit for bit the
/// value eave has been calling [`live`] since Stage F. eave already had
/// comet's identity colour and was spending it on the busy role, which is
/// why the app has never had an accent: the one saturated colour it owned
/// was already booked.
///
/// Two ways out, and the reason for the one taken: comet resolves the same
/// collision by putting *busy* on pink-400 and keeping indigo for accent.
/// That is a faithful port and it repaints every running session pink, which
/// is a taste call an operator should make rather than a slice. So eave
/// steps one family over instead — 293.5° against indigo's 277° — which is
/// far enough that "the row I chose" and "the session that is working" are
/// never the same colour, and they sit 10 px apart in the rail.
///
/// **Above its floor**, which is the constraint that fixed the *step*: as
/// the selection bar it lands on the rail's bare ground (see the leading
/// strip in `ui.rs`), where it measures **3.93:1** against a 3:1 floor. Over
/// the row's own wash it would have been **2.70:1** — under the floor
/// outright — and that is why the bar has a strip of its own.
///
/// Text on an accent fill is [`on_solid`], not a token of its own. comet's
/// `on_accent` is near-white because comet's accent fill is indigo-500, a
/// DARK plate; eave's accent is a light one, so its label goes the other
/// way. White on violet-400 is **2.85:1**; `on_solid` on it is **6.77:1**.
/// The PATTERN is the port — a saturated fill is usable without desaturating
/// it as long as its ink is named — not the token's value.
pub fn accent() -> Hsla {
    oklch(0.702, 0.183, 293.541)
}
/// Selection as a tinted wash rather than a grey one: the accent at α .24,
/// the standing fill of a selected rail row.
///
/// comet carries the same idea as `selection: hsla(0.66, 0.6, 0.55, 0.35)` —
/// a saturated colour at a third of an alpha, never a neutral wash. The α
/// here is solved rather than copied, because eave's ladder is stricter than
/// comet's: [`STATE_DELTA_FLOOR`] requires a selected row to sit a visible
/// step above a hovered one, and α .20 lands 1/255 above [`overlay`] at the
/// bright end. .24 clears it by 12/255 on the brightest backdrop and 20/255
/// on the darkest.
pub fn accent_wash() -> Hsla {
    accent().opacity(0.24)
}

// ---------------------------------------------------------------------------
// State colors — the only saturated pigment in the app

// The four state pigments, as **Tailwind 400 steps in oklch**, ported from
// comet's `dark()` constructor rather than invented here. Three of the four
// moved; the arithmetic is in `every_pigment_clears_the_surface_it_lands_on`
// and every one of them got *better*, because a Tailwind 400 is tuned to be
// legible on a dark ground and eave's hand-picked values were tuned to look
// nice in a swatch.
//
// `#e2726a` → red-400, `#e0b36a` → amber-400, `#62c987` → emerald-400.
// `live` did not move at all: `#7c86ff` IS `oklch(0.673 0.182 276.935)`.

/// Live activity right now (running tool dot, streaming cursor).
/// **indigo-400** — and see [`accent`] for why this value is the interesting
/// one in the file.
pub fn live() -> Hsla {
    oklch(0.673, 0.182, 276.935)
}
/// **emerald-400.** Was `#62c987`.
pub fn success() -> Hsla {
    oklch(0.765, 0.177, 163.223)
}
/// **amber-400.** Was `#e0b36a`, a tan that read as a dimmed neutral beside
/// the greys it sat in.
pub fn warning() -> Hsla {
    oklch(0.828, 0.189, 84.429)
}
/// **red-400.** Was `#e2726a`.
///
/// This is the token the whole slice turns on, so the numbers are worth
/// stating: as a *mark* on the rail's bare strip it goes 3.65:1 → **3.88:1**
/// here, and as a chip fill under [`on_solid`] 6.30:1 → **6.68:1**. It got
/// louder AND more legible, in the same move that was previously thought to
/// require desaturating it to `#f3c3bf`.
pub fn danger() -> Hsla {
    oklch(0.704, 0.191, 22.216)
}

// The panel rings used to be `warning().opacity(0.20)` and
// `danger().opacity(0.22)` and they are gone. At those alphas the ring
// composites to **1.5:1 and 1.4:1** against the plate it outlines — below
// even the quiet floor, and comfortably below the 3:1 a non-text mark owes
// its surface. That is the mechanism behind the operator's complaint that
// "the saturated pigments reach the screen as roughly one 6 px dot": the
// four pigments were on screen in three roles, and two of them were
// invisible by construction. The call sites now draw `warning()` and
// `danger()` at full strength — 7.04:1 and 4.76:1 on the plates they
// outline — and `every_pigment_clears_the_surface_it_lands_on` holds them
// there.
/// The kill button's resting fill (`attn-kill`).
pub fn danger_wash_strong() -> Hsla {
    danger().opacity(0.14)
}
/// The kill button under the pointer. Its resting fill is already
/// `danger_wash_strong`, so the hover step is the same pigment again
/// rather than the neutral wash (Slice M).
pub fn danger_wash_hover() -> Hsla {
    danger().opacity(0.24)
}

/// Diff line fills (Slice B2) — the only place state pigment tints a
/// surface inside the rails, because add/remove IS state (the stance:
/// color speaks only when state deserves attention). Glass: state-color
/// washes over the bare frost the rail rows sit on — on glass every
/// rung is a wash by architecture, and the sweep test proves these
/// always rise above the ground and keep their hue apart on any
/// backdrop. Opaque: the same washes PRE-BLENDED over the canvas (the
/// F.2 lesson — a translucent wash belongs to the surface it was solved
/// for; pre-composite instead of hoping).
const DIFF_WASH_ALPHA: f32 = 0.10;
pub fn diff_add_bg() -> Hsla {
    success().opacity(DIFF_WASH_ALPHA)
}
pub fn diff_del_bg() -> Hsla {
    danger().opacity(DIFF_WASH_ALPHA)
}

// ---------------------------------------------------------------------------
// Code

/// Inline code runs (warm, distinct from prose).
pub fn code_text() -> Hsla {
    rgb(0xe0a882).into()
}
/// Fenced code body — full-brightness mono on the inset plate.
pub fn code_block_text() -> Hsla {
    text()
}

// ---------------------------------------------------------------------------
// Rhythm — every gap and padding derives from a 4 px base unit.
// SPACE_HALF exists only for sub-grid baseline nudges (caret alignment),
// never for gaps between elements.

pub const UNIT: f32 = 4.0;
pub const SPACE_HALF: f32 = UNIT / 2.0;
pub const SPACE_1: f32 = UNIT;
pub const SPACE_2: f32 = UNIT * 2.0;
pub const SPACE_3: f32 = UNIT * 3.0;
pub const SPACE_4: f32 = UNIT * 4.0;
pub const SPACE_6: f32 = UNIT * 6.0;

/// 1 px rules and dividers (the markdown `---` rule).
pub const HAIRLINE: f32 = 1.0;

// ---------------------------------------------------------------------------
// Typography

pub const SANS: &str = ".SystemUIFont";
/// Menlo ships on every macOS; SF Mono and JetBrains Mono do not.
pub const MONO: &str = "Menlo";

// The five-rank scale, plus one label rank.
//
// **What was here before, and why it could not work.** Ten named sizes —
// 10, 10, 11, 11.5, 12, 12, 12.5, 13, 13.5, 14 — spanning **four pixels**.
// The largest text anywhere in the app was `USER_SIZE` at 14.0, and
// `TITLE_SIZE` (the titlebar) was **13.0, smaller than body text**. There
// was no display rank, so nothing in the app ever announced, and nineteen
// design slices had tuned only the one channel that was left: lightness.
//
// The scale below has a real top. Each rank is a ROLE, not a screen: the six
// retired names (`USER_SIZE`, `LABEL_SIZE`, `SMALL_SIZE`, `TOOL_NAME_SIZE`,
// `DIFF_SIZE`, `TITLE_SIZE`) were all sizes wearing a feature's name, and so
// was `LAUNCHER_TITLE_SIZE` — which turned out to be the interesting one.
// See [`DISPLAY_SIZE`].

/// The one rank that announces: 20/26.
///
/// **This value is not new — it was already in the file, quarantined.** It
/// was `LAUNCHER_TITLE_SIZE`, and the launcher is the only screen in eave
/// that reads as finished: a display headline, a quiet subtitle under it, a
/// tile grid, and a solid near-white primary button with a near-black label
/// ([`solid`]/[`on_solid`], which were also already there, misnamed as
/// `inverse`/`on_inverse`). eave did not lack a design language. It had two:
/// a good one used on one screen, and a flat one used everywhere else, and
/// the good one was locked behind a naming convention.
///
/// So this rank is a PROMOTION, not an invention, and it keeps the
/// launcher's own numbers rather than introducing a competing rank one step
/// away from them.
pub const DISPLAY_SIZE: f32 = 20.0;
pub const DISPLAY_LINE: f32 = 26.0;

/// Names and panel headings: the titlebar, a session's name in the rail, the
/// heading of an attention or approval card. 16/22.
///
/// The titlebar was `TITLE_SIZE` = 13.0 — *smaller than body text*, which is
/// not a taste question. A rail row's session name was `DETAIL_SIZE` = 12.0
/// against `SMALL_SIZE` = 11.0 metadata beneath it: one pixel of hierarchy
/// across the app's primary navigation surface.
pub const NAME_SIZE: f32 = 16.0;
pub const NAME_LINE: f32 = 22.0;

/// Reading text: transcript prose, the composer, a user directive.
/// **Unchanged** — this slice does not churn the one rank that was already
/// solved.
pub const BODY_SIZE: f32 = 13.5;
pub const BODY_LINE: f32 = 21.0;

/// Secondary metadata under a title, and every control label: buttons, menu
/// rows, chips, panel bodies, tool names. 12.5/17.
///
/// **A rank is a budget, not a licence.** Size, weight and ink are three
/// separate channels and a rank only buys you one of them at a time. This is
/// not a style preference; it is a field report. Slice AO painted a rail
/// session name at [`NAME_SIZE`] *and* [`WEIGHT_STRONG`] *and* [`text`] over
/// 12.5 dim metadata, and the operator's verdict was "this text is
/// gigantic". Nothing was wrong with any one of the three. Spending all
/// three at once is what overshot.
///
/// The rail in this file spends exactly one: the name went 12.0 → 16.0 and
/// kept the weight and the ink it already had.
pub const DETAIL_SIZE: f32 = 12.5;
pub const DETAIL_LINE: f32 = 17.0;

/// Timestamps, counts, the status bar, rail facts, diff lines. 11/15.
pub const MICRO_SIZE: f32 = 11.0;
pub const MICRO_LINE: f32 = 15.0;

/// Section headers — "SESSIONS", "CHANGES". Same size as [`MICRO_SIZE`] and
/// a different rank: it is set UPPERCASE, at [`WEIGHT_LABEL`], on a tighter
/// line, because a header is one line and does not need leading.
///
/// **It was supposed to carry letter-spacing and it cannot.** gpui's
/// `TextStyle` (fork pin `5d1f83d`) has `font_size`, `line_height`,
/// `font_weight`, `font_style`, `font_features`, `background_color`,
/// `underline`, `strikethrough`, `white_space`, `text_overflow`,
/// `text_align` — and no letter-spacing or tracking field of any kind; the
/// string `letter_spacing` does not occur anywhere in the crate. The only
/// way to fake it is to interleave thin spaces into the string, which breaks
/// truncation, measurement and copy-out. So the rank is carried by case and
/// weight, and this is stated rather than faked.
pub const SECTION_SIZE: f32 = 11.0;
pub const SECTION_LINE: f32 = 14.0;

/// Fenced and inline code, and every mono run in the rails.
///
/// **Mono keeps its own size, and that is a claim worth making rather than
/// an exception worth hiding.** Menlo's x-height is ~0.54 em against
/// `.SystemUIFont`'s ~0.52, and its advance is a fixed 0.60 em where SF's
/// lowercase averages ~0.50 — so a mono run set at the sans rank beside it
/// reads BIGGER and sets ~20% wider. 11.5 against [`BODY_SIZE`] 13.5 and
/// 12.5 against [`DETAIL_SIZE`] are the two pairings the app actually draws,
/// and both land mono within a few percent of the sans it sits next to.
/// One scale for two font families is a scale that is wrong for one of them.
pub const CODE_SIZE: f32 = 11.5;
pub const CODE_LINE: f32 = 17.5;

/// Composer input line: [`BODY_SIZE`] text on a taller leading so the caret
/// row does not feel cramped inside the card. A line-height override on an
/// existing rank, not an eleventh size — it names no size of its own, which
/// is why the audit of screen-named ranks left it alone.
pub const COMPOSER_LINE: f32 = 22.0;
pub const COMPOSER_MIN_H: f32 = 24.0;

/// Weight roles — the only weights the shell uses. Prose is NORMAL;
/// labels/names are MEDIUM; STRONG is reserved for escalated notices
/// and the armed send glyph. (Markdown `**bold**` inside prose keeps
/// semantic BOLD — that's content, not chrome.)
pub const WEIGHT_BODY: FontWeight = FontWeight::NORMAL;
pub const WEIGHT_LABEL: FontWeight = FontWeight::MEDIUM;
pub const WEIGHT_STRONG: FontWeight = FontWeight::SEMIBOLD;

/// Markdown's heading ladder, expressed in the app's ranks.
///
/// It used to carry three sizes of its own — 19.5, 17.5, 15.5 — which is
/// three MORE names in a scale that already had ten, and 19.5 sat half a
/// pixel from the launcher's 20.0 without either one knowing about the
/// other. A document's headings are content rather than chrome, but they are
/// still typography, and a second scale running half a pixel beside the
/// first is exactly the flatness this slice exists to remove.
///
/// h2 and h3 share [`NAME_SIZE`] and are separated by WEIGHT, which is a
/// rank axis that costs no new number. h4+ has always collapsed to body
/// semibold and still does.
pub fn heading(level: u8) -> (Pixels, Pixels, FontWeight) {
    match level {
        1 => (px(DISPLAY_SIZE), px(DISPLAY_LINE), FontWeight::BOLD),
        2 => (px(NAME_SIZE), px(NAME_LINE), FontWeight::BOLD),
        3 => (px(NAME_SIZE), px(NAME_LINE), FontWeight::SEMIBOLD),
        _ => (px(BODY_SIZE), px(BODY_LINE), FontWeight::SEMIBOLD),
    }
}

// ---------------------------------------------------------------------------
// Metrics

/// Default and minimum window sizes.
pub const WINDOW_W: f32 = 1080.0;
pub const WINDOW_H: f32 = 760.0;
pub const WINDOW_MIN_W: f32 = 720.0;
pub const WINDOW_MIN_H: f32 = 520.0;

/// Custom titlebar: bar height, traffic-light inset, and the clearance
/// the content leaves for the lights (waku's numbers).
pub const TITLEBAR_H: f32 = 48.0;
pub const TRAFFIC_X: f32 = 16.0;
pub const TRAFFIC_Y: f32 = 17.0;
pub const TRAFFIC_CLEARANCE: f32 = 86.0;

/// The reading column: outer gutter + centered cap (waku: 20 / 720).
pub const GUTTER: f32 = 20.0;
pub const CONTENT_MAX_W: f32 = 720.0;

/// Vertical rhythm in the log (all on the 4 px grid). TURN_GAP marks
/// an operator directive as a section break; MSG_TOP_GAP opens a
/// message; BLOCK_GAP separates blocks inside one message.
pub const TURN_GAP: f32 = SPACE_6;
pub const MSG_TOP_GAP: f32 = SPACE_4;
pub const BLOCK_GAP: f32 = SPACE_3;
pub const LOG_TOP_PAD: f32 = SPACE_6;
pub const LOG_BOTTOM_PAD: f32 = SPACE_6;
/// Tool rows indent under their message's prose — sub-events of the
/// turn, not peers of it.
pub const TOOL_INDENT: f32 = SPACE_4;
/// Code plate: language strip height and interior padding.
pub const CODE_STRIP_H: f32 = SPACE_6;

/// Directive block cap: don't let a one-liner span the whole column.
pub const DIRECTIVE_MAX_W: f32 = 560.0;

/// Radii.
pub const RADIUS_DIRECTIVE: f32 = 10.0;
pub const RADIUS_PANEL: f32 = 10.0;
pub const RADIUS_CODE: f32 = 8.0;
pub const RADIUS_COMPOSER: f32 = 13.0;
pub const RADIUS_BUTTON: f32 = 7.0;

/// Controls.
pub const SEND_BUTTON: f32 = 26.0;
pub const STATE_DOT: f32 = 6.0;
pub const TOOL_DOT: f32 = 5.0;

// ---------------------------------------------------------------------------
// Icons (Slices P3, Q1) — the box a glyph is drawn into.
//
// A gpui SVG is rendered INTO its box and tinted at paint time, so these
// are the only numbers that decide how heavy the set looks. They are not
// free-floating: each is paired with a `stroke-width` in the drawing, and
// `icons::tests` walks these very constants asserting the product lands in
// the family's 1.00-1.25 px band. Change one and that test names the class
// it broke.
//
// Q1 REPLACED P3's ten hand-drawn glyphs with Solar Icons (Linear), whose
// stroke is a fixed 1.5 on the 24-grid. That single number sets the whole
// ladder: 1.5/24 is legible only between 16 and 20 px, so the 12 px and
// 14 px slots P3 sized for its own heavier drawings could not survive the
// swap. They did not shrink the family; the family exposed them.

/// Glyphs beside a text label, and the composer's send arrow inside its
/// 26 px round button. The family's floor: 1.5/24 x 16 = a 1.00 px stroke.
///
/// **This is where `ICON_SEND` (14) and `ICON_INLINE` (12) went.** Both
/// were sized to sit at the cap height of the text beside them, which is
/// the correct instinct for a glyph you drew yourself and the wrong one
/// for a bought family: Solar Linear at 14 px is a 0.875 px stroke, a
/// hairline that dissolves against `surface()`.
pub const ICON_SM: f32 = 16.0;
/// Standalone controls that carry no label of their own — the size the
/// launcher's row affordances use. 1.5/24 x 18 = 1.125 px.
pub const ICON_MD: f32 = 18.0;
/// Both rail collapse toggles in the titlebar, and the family's ceiling:
/// 1.5/24 x 20 = 1.25 px, the top of the band. Unchanged from P3, where
/// the number was already chosen so the visible mark — not the box around
/// it — matched the hand-drawn pictogram it replaced.
pub const ICON_TOGGLE: f32 = 20.0;
/// The file status glyph. Exactly [`FILE_STATUS_W`], so the slot is
/// unchanged and paths stay aligned where the letter left them. The air
/// is inside the drawing: every status mark spans at most 12 of the 24
/// grid units, so it reads no larger than the letter it replaces.
///
/// These five are `icons::Class::Compact` — drawn at stroke 2.25 rather
/// than the family's 1.5, which is what lets them hold at 12 px
/// (2.25/24 x 12 = 1.125 px, mid-band) while the family cannot.
pub const ICON_STATUS: f32 = 12.0;

// ---------------------------------------------------------------------------
// The launcher (Slice Q2) — eave's rest state.
//
// A tile is deliberately WIDER than it is tall and deliberately large: it
// is the only thing on an otherwise empty screen, and the reference apps'
// rest states are confident rather than apologetic. The three stacked
// lines inside it (mark, name, availability) are what makes a tile read as
// an object rather than a button with a picture on it.

/// One provider tile. 112 x 96 fits six across `LAUNCHER_MAX_W` with
/// `SPACE_3` gutters and still leaves the strip centred in a 720 px
/// content column.
pub const TILE_W: f32 = 112.0;
pub const TILE_H: f32 = 96.0;
/// Tile corner. Larger than `RADIUS_CHIP` and smaller than
/// `RADIUS_COMPOSER`: a tile is a card, not a chip and not a panel.
pub const RADIUS_TILE: f32 = 10.0;
/// The gap between tiles, and between the launcher's stacked sections.
pub const TILE_GAP: f32 = SPACE_3;
/// The strip's cap: exactly three tiles and two gaps. Six providers
/// therefore wrap 3+3, which reads as a considered grid; the same six left
/// to fill the available width wrap 4+2, which reads as a bug.
pub const STRIP_MAX_W: f32 = TILE_W * 3.0 + TILE_GAP * 2.0;
pub const LAUNCHER_GAP: f32 = SPACE_6;
/// The launcher's column. Narrower than `CONTENT_MAX_W` so the strip does
/// not run the full width of a wide window — a rest state that spans
/// everything reads as a dashboard, not an invitation.
pub const LAUNCHER_MAX_W: f32 = 700.0;

// ---------------------------------------------------------------------------
// Provider marks (Slice Q1) — a vendor's own logo, not a glyph.
//
// Marks are fills, not strokes, so no weight band applies and these are
// pure fit decisions. They are separate constants from the glyph ladder
// because they will drift apart: a mark is sized so its SILHOUETTE reads
// beside a name, and every vendor draws to a different bounding box.

/// A mark beside a session's name, in a rail row or a tab.
pub const ICON_MARK: f32 = 18.0;
/// A mark as the subject of a launcher tile, where it is the thing being
/// chosen rather than a label on something else.
pub const ICON_MARK_LG: f32 = 28.0;

// ---------------------------------------------------------------------------
// Loaders (Slice N3) — the working indicator and the skeletons.
//
// These are the design values N1 deliberately left out of its catalog
// (`motion.rs` caveat 7: loader periods are a decision about how eave
// FEELS, not machinery), so they live here with every other design value.
// The curve they ride is `motion::EASE_IN_OUT` and the spec assembled
// from these numbers is `loaders::WORK_PULSE` — the shapes themselves are
// in `loaders.rs`, because an element is not a design value.

/// One period for every working indicator in the window. The pulse clock
/// runs them all off one epoch, so a rail dot and the transcript trailer
/// visibly share a clock rather than wobbling against each other; that
/// only reads as deliberate if they also share a period.
///
/// 1.2 s is a taste decision, and the taste is: slow enough that a 6 px
/// dot in peripheral vision reads as breathing rather than blinking, fast
/// enough that any glance longer than half a period catches movement. At
/// the clock's 33 ms tick that is ~36 frames a period.
pub const WORK_PULSE_MS: u64 = 1200;

/// What a cell fades DOWN to, as a fraction of its full color. Not zero:
/// a working indicator whose cells reach invisible flickers, and the dot
/// forms are replacing STATE dots — a state dot that disappears twice a
/// second is stating something false about the session.
pub const WORK_CELL_REST: f32 = 0.28;

/// Gap between the four cells of a dot-slot indicator. The cells are
/// sized from the slot around this, so the indicator is exactly as big as
/// the dot it replaces ([`STATE_DOT`] or [`TOOL_DOT`]) — the whole point
/// of a fixed slot is that a load-state flip moves nothing.
pub const WORK_DOT_GAP: f32 = HAIRLINE;

/// The in-flow bar form: cells and their geometry. Used where a caret
/// used to sit and in the transcript trailer, so the cell is sized to
/// read at body scale without becoming a progress bar.
pub const WORK_BAR_CELLS: usize = 4;
pub const WORK_BAR_CELL: f32 = 3.0;
pub const WORK_BAR_GAP: f32 = SPACE_HALF;

/// Skeleton rows for a load state. The slot height is [`DETAIL_LINE`] —
/// the line height of the stated-fact text a skeleton stands in for at
/// both of its sites — and the bar pulses INSIDE that slot, so the
/// skeleton occupies the same rhythm the arriving content will.
pub const SKELETON_ROW_H: f32 = DETAIL_LINE;
pub const SKELETON_BAR_H: f32 = SPACE_2;
pub const SKELETON_ROWS: usize = 5;
/// Bar widths as a fraction of the container, cycled down the rows.
/// Arbitrary but fixed: skeleton bars of equal width read as a progress
/// meter, and a meter would be a claim about how far along the load is —
/// which eave does not know.
pub const SKELETON_WIDTHS: [f32; 5] = [0.72, 0.46, 0.88, 0.34, 0.61];

// ---------------------------------------------------------------------------
// Session rail (Slice A) — comet's shell model (directive 2026-08-17):
// the rails paint NO surface of their own. Their rows sit directly on
// the glass ground, and the CONTENT WELL between them is the
// differentiated card ([`content_plate`], inset by [`PLATE_MARGIN`]).
// The old opaque L2 rail slabs read as gray cardboard next to the glass;
// a sidebar on frost has no plate to get wrong.

/// Rail width (62 × the 4 px unit).
pub const RAIL_W: f32 = 248.0;
/// Ground margin around the floating content card (and the air the rails
/// keep from the window edges).
pub const PLATE_MARGIN: f32 = SPACE_3;

/// The rail's one left edge, and the rule that goes with it.
///
/// **Every direct child of a rail — the header label, a row's TEXT, the
/// empty state, the footer line — starts on this edge. A row's CARD bleeds
/// [`RAIL_CARD_BLEED`] outside it on both sides, and the header's trailing
/// control shares the card's RIGHT edge.**
///
/// Written down because it was not, and a spacing scale cannot supply it: a
/// scale tells you the SIZE of a gap and says nothing about which edge
/// things line up on. Every value in the rails was already a legal
/// `SPACE_*` and the result still had four different left edges — header
/// label 24, row card 20, row text 28, footer 24 — with the row card
/// overhanging the header's `+ new` button by 4 px. Both rails had it, the
/// same way, because one was copied from the other.
pub const RAIL_GUTTER: f32 = SPACE_3;

/// How far a row's card extends outside [`RAIL_GUTTER`] on each side. The
/// card is a highlight around text, so it must bleed past the text it
/// highlights or the selection looks clipped.
pub const RAIL_CARD_BLEED: f32 = SPACE_3;

/// The rail row's **leading strip**: the bare column, outside the row's
/// washed card, that carries the selection bar and the state mark.
///
/// **It exists because a pigment cannot live on the selection wash.** Before
/// this slice the state dot sat inside the card, so on the row you were
/// looking at it composited over the selection wash — 82.9/255 at the bright
/// end of the backdrop sweep — where `danger()` measures **2.51:1** and
/// `live()` **2.46:1**, both under the 3:1 a mark owes its surface. The two
/// available answers were to lighten the pigments into the pale register the
/// operator had already rejected, or to move the mark off the wash. This is
/// the second one, and it is the [`STATE_DELTA_FLOOR`] lesson applied to
/// pigment: *above what, and does anything paint it?*
///
/// On the bare ground the same marks read **3.88:1** and **3.58:1**, and
/// neither pigment was touched to get there.
///
/// The width is [`STATE_DOT`] + `SPACE_2` — **exactly what the dot and its
/// gap cost inside the card** — so promoting the dot out of the card moves
/// no other pixel: the provider chip, the session name, the badge and every
/// meta line start on the same x they did before, and every rail width test
/// measures the same room it measured before.
pub const RAIL_STRIP_W: f32 = STATE_DOT + SPACE_2;
/// The selection bar itself, on the leading edge of the strip.
pub const SELECTION_BAR_W: f32 = 3.0;
/// Its height as a fraction of the row: a bar that ran the row's full height
/// would read as a border on the card it is deliberately outside of.
pub const SELECTION_BAR_INSET: f32 = SPACE_2;
pub const RADIUS_SELECTION_BAR: f32 = SELECTION_BAR_W / 2.0;
/// A rail row's first line — the slot the session name, the rename field and
/// the state mark all align to. The rename field draws a visible 1 px border
/// and the resting label draws a transparent one of the same weight (Stage
/// AC), so the slot is the name's line plus both hairlines.
pub const RAIL_LABEL_SLOT_H: f32 = NAME_LINE + HAIRLINE * 2.0;
/// Provider identity chip: self-drawn monochrome initials on a bordered
/// rounded square. Deliberately never a third-party mark — one uniform,
/// self-drawn set for every provider (Slice A directive).
pub const PROVIDER_CHIP: f32 = 18.0;
pub const RADIUS_CHIP: f32 = 5.0;
// Slice P3 deleted RAIL_TOGGLE_W/H and RADIUS_GLYPH with the hand-drawn
// sidebar pictogram they described. The toggles are Solar's
// `icons::SIDEBAR_MINIMALISTIC_LEFT` / `SIDEBAR_MINIMALISTIC` now, sized
// by `ICON_TOGGLE` above.

// ---------------------------------------------------------------------------
// Files rail (Slice B) — rows on the bare glass ground, mirroring the
// session rail (see above). Same width as the session rail: the two
// rails frame the content card symmetrically. B1 carries no new pigment
// — statuses use the text tiers, counts use the existing success/danger
// state colors; the add/remove diff washes are B2 tokens.

pub const FILES_RAIL_W: f32 = RAIL_W;
/// Fixed slot for the one-letter status glyph so paths align.
pub const FILE_STATUS_W: f32 = 12.0;
/// Render cap for the inline diff pane (B2). The rail diff is a
/// shape-of-change view, not a review surface — and the pane sits in a
/// non-virtualized scroll container, so every rendered line is laid out
/// every frame. At the engine's full 2000-line cap the pane alone cost
/// ~13 ms/frame (measured, EAVE_PERF); at 240 it holds the budget. The
/// remainder is a stated count, never silence.
pub const DIFF_PANE_MAX_LINES: usize = 240;

// ---------------------------------------------------------------------------
// Floating chrome geometry (Stage H glass)

/// The list's trailing spacer: keeps Follow's newest row above the
/// floating composer stack (resting composer card ≈ 86 px + stack gap +
/// breathing room, on the 4 px grid).
pub const COMPOSER_CLEARANCE: f32 = 116.0;
/// The ↓-bottom pill's rounding (content 26 px tall → full round = 13);
/// the pill's frost pass must state the radius explicitly.
pub const RADIUS_PILL: f32 = 13.0;

// ---------------------------------------------------------------------------
// The overlay layer (Slice P1) — popovers, menus, context menus. The L3
// rung, one step above the floating chrome the composer and the ↓-bottom
// pill sit on.
//
// **These tints carry no backdrop blur, and that is the slice's one
// deliberate visual gap.** Every other floating surface in eave frosts
// what is behind it (`BLUR_CHROME`, 16) and can therefore afford a
// see-through α 0.72. A menu cannot yet: its entrance fades opacity
// 0.3→1, and a backdrop blur does NOT fade with the element painting over
// it, so a frosted menu would flash a full-strength blur for the whole
// 140 ms while the tint was still arriving. Resolving that is a visual
// question for the first consumer, not machinery, so P1 ships the surface
// unfrosted and states the number the follow-up needs.
//
// Without the blur, alpha has to carry the occlusion on its own. The
// derivation, in the same terms as the chrome rung: body text behind an
// α 0.72 surface ghosts through at ~24% of its own contrast, which is
// plainly legible as a second layer of words under the menu's own. At
// α 0.92 it lands at ~7% — the alpha of `border()`, which is this
// design's own floor for "an edge you can just make out". So 0.92 is not
// a taste, it is the point where unblurred bleed drops to the faintest
// thing the palette already draws on purpose. **When the blur lands, this
// comes back down to `CHROME_GLASS_ALPHA`** — do not keep both.
const MENU_GLASS_ALPHA: f32 = 0.92;

/// Backdrop-blur radius for the L3 overlay rung — reserved, unused.
/// The value `BLUR_CHROME`'s comment has always named for "menus/modals";
/// see the alpha note above for why nothing paints it yet.
pub const BLUR_MENU: f32 = 44.0;

/// L3 glass: the popover surface. Must resolve LIGHTER than the chrome it
/// floats on, on every backdrop — asserted by
/// `a_popover_clears_the_chrome_it_floats_on`.
///
/// **Was `#1e1e1e`, and the reasoning that picked it compared hexes while
/// the two surfaces carry different alphas.** `#1e1e1e` is indeed a step
/// above `chrome_glass`'s `#141414` as a *tint*; it is not a step above it
/// as a *surface*, because chrome admits 28% of the backdrop and a menu
/// admits 8%. So chrome climbs with the desktop and the menu barely moves,
/// and the gap closes: **+12.3/255 over a black desktop, +2.0 at 200, and
/// −0.8 at white** — a popover that goes fractionally darker than the
/// surface carrying it, i.e. an edgeless rectangle.
///
/// The alpha is not the lever. [`MENU_GLASS_ALPHA`]'s 0.92 is derived from
/// unblurred text bleed and is load-bearing; and the gap scales as
/// `α · (tint − chrome)`, so alpha only multiplies a difference that was
/// already going to zero. The tint is the lever. `#282828` puts the worst
/// case — a white desktop — at **+8.4/255** and the best at +21.5.
pub fn menu_glass() -> Hsla {
    let c: Hsla = rgb(0x282828).into();
    c.opacity(MENU_GLASS_ALPHA)
}

/// The popover's rounding. Between `RADIUS_BUTTON` (7) and
/// `RADIUS_PANEL` (10): a menu is a small floating card, not a pane.
pub const RADIUS_MENU: f32 = 8.0;
/// A menu row's rounding — the wash is inset from the surface edge, so
/// it needs its own smaller radius (the standard concentric rule:
/// `RADIUS_MENU` − `MENU_ROW_INSET`, landing on the 4 px grid's half).
pub const RADIUS_MENU_ROW: f32 = 5.0;

/// Narrowest a menu goes. Under this a two-word label plus a trailing
/// state reads as two columns rather than one row.
pub const MENU_MIN_W: f32 = 176.0;
/// Widest. Past this a menu stops being a list of choices and starts
/// being a pane; a long label wraps inside the row instead.
pub const MENU_MAX_W: f32 = 320.0;
/// Vertical pad inside the surface, above the first row and below the
/// last.
pub const MENU_PAD_Y: f32 = SPACE_1;
/// Horizontal inset of a row's wash from the surface edge — what makes
/// the hover wash read as a pill inside the card rather than a full-bleed
/// band across it.
pub const MENU_ROW_INSET: f32 = SPACE_1;
/// A row's height. Matches neither the send button nor a transcript line:
/// it is [`DETAIL_LINE`] plus `SPACE_1` above and below, so the wash clears
/// the glyphs by one grid unit.
pub const MENU_ROW_H: f32 = DETAIL_LINE + SPACE_1 * 2.0;
/// Horizontal pad inside a row's wash.
pub const MENU_ROW_PAD_X: f32 = SPACE_2;
/// Minimum gap between a row's label and its trailing state, so the two
/// never close up on a long label.
pub const MENU_ROW_GAP: f32 = SPACE_3;

/// Gap between the anchor point and the menu's edge.
pub const MENU_ANCHOR_GAP: f32 = SPACE_1;
/// How close a menu may come to the window edge before it is pushed back
/// in. `anchored()` does the pushing; this is the margin it keeps.
pub const MENU_SCREEN_MARGIN: f32 = SPACE_2;

/// Disabled treatment (polish-inventory §2.6): comet's ported web recipe
/// is `disabled:opacity-35` plus no pointer events, and this is the 0.35.
/// It is deliberately NOT a text tier — a disabled control dims *whole*,
/// glyph and wash and trailing state together, which is what says "this
/// control is off" rather than "this text is less important".
/// How many lines of a FAILED tool's reply the transcript states before it
/// says how many more there are.
///
/// Chosen, not derived, and deliberately small: this block lives inside a
/// virtualized list, so its height is a row height, and a row whose height
/// grows with an agent's verbosity is how a transcript stops being able to
/// promise anything about scrolling. Six lines is enough for a compiler
/// error's first message or a failed assertion; anything longer is a thing
/// to open, not a thing to glance at. comet caps its equivalent at 24 and
/// pays for it with a fold and a measured height, which is the next slice.
pub const TOOL_ERROR_MAX_LINES: usize = 6;

/// How many lines of one code fence the transcript draws before it states
/// the remainder.
///
/// A fence is ONE row in a virtualized list, so an uncapped fence is a row
/// whose height is whatever the agent felt like emitting — and the list's
/// whole promise is that a row's height is knowable. 200 lines is well past
/// any fence in the capture corpus and still bounded. The copy button takes
/// the WHOLE fence, not the drawn part, so the cap costs reading and never
/// costs data.
pub const FENCE_MAX_LINES: usize = 200;

/// How many lines of an OPENED tool card's body the transcript draws.
///
/// Larger than [`TOOL_ERROR_MAX_LINES`] because opening a card is a
/// deliberate act — the reader asked — and smaller than
/// [`FENCE_MAX_LINES`] because the card is still a row in a virtualized
/// list and the payload behind it is already bounded at 8 KiB by
/// `eave_core::events::TOOL_PAYLOAD_CAP`. The remainder is stated, as
/// everywhere else that caps.
pub const TOOL_BODY_MAX_LINES: usize = 40;

/// How many BODY rows of one markdown table the transcript draws before it
/// states the remainder.
///
/// Same rule as [`FENCE_MAX_LINES`], same reason: a table is ONE row in a
/// virtualized list, so an uncapped table is a row whose height is whatever
/// the agent felt like emitting. **Chosen, not derived.** 60 rows at
/// [`TABLE_ROW_MIN_H`] (29 px) is 1,740 px — more than twice [`WINDOW_H`],
/// so the cap sits well past "more than a screenful", and the longest table
/// in this project's own 178 KB of docs is 23 rows, so nothing eave has
/// actually been asked to draw reaches it.
pub const TABLE_MAX_ROWS: usize = 60;

/// How many characters of ONE table cell are drawn.
///
/// The other half of the height bound: sixty rows of a two-thousand-
/// character cell is the same unbounded row by another route. **Chosen,
/// not derived.** 200 characters is ~3 wrapped lines in the widest column
/// a table can have at [`BODY_SIZE`]; a cell longer than that is prose that
/// wanted a paragraph.
pub const TABLE_CELL_MAX_CHARS: usize = 200;

/// How many items of one list block are drawn before the remainder is
/// stated. Larger than [`TABLE_MAX_ROWS`] because a list item is one line
/// where a table row is a full-width grid row.
pub const LIST_MAX_ITEMS: usize = 120;

/// Vertical padding inside a table cell, per edge.
pub const TABLE_CELL_PAD_Y: f32 = SPACE_1;
/// Horizontal padding inside a table cell, per edge.
pub const TABLE_CELL_PAD_X: f32 = SPACE_2;
/// The shortest a one-line table row can be: one body line plus both pads.
pub const TABLE_ROW_MIN_H: f32 = BODY_LINE + TABLE_CELL_PAD_Y * 2.0;

/// The marker gutter of a list item — bullet, number or checkbox — and the
/// step one nesting level indents by.
///
/// The gutter must fit the widest marker eave draws, which is a
/// two-digit ordinal ("99."). Asserted at real glyph widths in
/// `a_list_marker_never_collides_with_its_own_text`.
pub const LIST_GUTTER_W: f32 = 22.0;
pub const LIST_INDENT: f32 = SPACE_4;
/// Gap between the marker gutter and the item's text.
pub const LIST_MARKER_GAP: f32 = SPACE_1;
/// The task checkbox's box.
pub const CHECKBOX: f32 = 12.0;
pub const RADIUS_CHECKBOX: f32 = 3.0;

/// The quoted block's left rule.
pub const QUOTE_BAR_W: f32 = 2.0;

/// WCAG 2.1 AA for normal-size text. Cited, not chosen: SC 1.4.3.
pub const TEXT_CONTRAST_FLOOR: f32 = 4.5;
/// WCAG 2.1 AA for large text (SC 1.4.3), used here as the floor for the
/// quiet tier — a deliberate, stated relaxation for text that is structural
/// rather than primary, not a claim that the tier is WCAG-conforming at the
/// sizes eave draws it.
pub const TEXT_QUIET_CONTRAST_FLOOR: f32 = 3.0;
/// The smallest surface step this design is willing to call a step, in 8-bit
/// channel units over the darkest ground. **Chosen, not derived** — it is the
/// magnitude the elevation ladder had never constrained, and it is set at the
/// level the existing `raised` rung already clears so that pinning it is a
/// regression guard rather than a redesign. Revisit by eye, not by argument.
pub const SURFACE_STEP_FLOOR: f32 = 6.0;
/// A state change the eye can find: hover, selection, an armed edge. Same
/// units and the same standing as [`SURFACE_STEP_FLOOR`].
pub const STATE_DELTA_FLOOR: f32 = 8.0;

/// **A pigment only owes 4.5:1 when it is TEXT.** As a bar, a rim, a dot or
/// a fill it is a non-text element, and WCAG 2.1 §1.4.11 puts that at 3:1
/// against the adjacent surface.
///
/// That distinction is the entire unlock for the stuck question this slice
/// inherited. `danger()` measured 3.65:1 on the bare ground and 2.52:1 on a
/// selected rail row, and the previous slice's answer was to desaturate it
/// to `#f3c3bf`, a pale pink. The arithmetic behind that answer is real and
/// unavoidable *for text*: solving red for 4.5:1 on the selected row's
/// surface requires a relative luminance of **0.563**, and no colour with a
/// red hue reaches 0.563 without being a pink — WCAG's luminance is 72 %
/// green, so a red pays for its hue twice. `#f3c3bf`, the pale pink the
/// previous slice proposed, sits at 0.618; Tailwind red-400 sits at 0.314.
/// Every red that clears 4.5:1 there is a pink; this is not taste and no
/// amount of hue-hunting escapes it.
///
/// At 3:1 the requirement drops to **0.36**, and at 3:1 *against the bare
/// ground* (the surface the rail's leading strip actually paints) it drops
/// to **0.24** — which all four state pigments and the accent clear with
/// their saturation untouched. So the fix was never to the pigment. It was
/// to the SURFACE the pigment lands on, and to stopping the pigment from
/// carrying words it cannot carry.
///
/// `every_pigment_clears_the_surface_it_lands_on` is the assertion, and it
/// enumerates roles — "the rail's state mark", "the attention panel's
/// ring" — rather than tokens, for the reason Slice AG's ink test does.
pub const PIGMENT_CONTRAST_FLOOR: f32 = 3.0;

/// Retired. A container-wide `.opacity()` cannot express "disabled" on this
/// ground: composited over the glass canvas, **pure white at 0.35 lands at
/// `#5e5e5e`, 3.11:1** — so the mechanism's own ceiling is barely at the
/// quiet floor, and any real token multiplied through it is far below.
/// Disabled controls pick a dimmed token directly; see [`text_disabled`].
pub const DISABLED_OPACITY: f32 = 0.35;

/// A disabled control's label. A token, not an opacity — see the note on
/// [`DISABLED_OPACITY`] for why the multiply could never work.
pub fn text_disabled() -> Hsla {
    text_tertiary()
}

// ---------------------------------------------------------------------------
// Scroll-edge fades (Slice M) — the fork's per-primitive `EdgeFade`, the
// only mechanism that dissolves content over glass (a painted overlay
// cannot fade anything out when what is behind it is the desktop). See
// `crate::edge_fade` for the wrapper and comet's two production failure
// modes.
//
// The rule both bands below follow: **the band equals the resting pad on
// that edge.** Content at rest therefore sits exactly where the ramp
// reaches full opacity, so nothing fades until it actually scrolls into
// the chrome — the fade reads as occlusion, never as a dimmed first line.
// comet learned this the other way round (their sidebar needed the
// overflow gate because it has no resting top pad).

/// Both rail scrollers, both edges. The rails have no resting top pad, so
/// this band is gated on the scroll handle's overflow instead — a rail
/// whose sessions fit shows no ramp at all.
pub const RAIL_FADE_BAND: f32 = SPACE_6;

/// Transcript, top edge. Deliberately EQUAL to [`LOG_TOP_PAD`], which is
/// padding on the faded wrapper and therefore inside the faded region:
/// at scroll-top the first row's edge lands exactly at the end of the
/// ramp. Change one and change the other.
pub const TRANSCRIPT_FADE_BAND_TOP: f32 = LOG_TOP_PAD;

/// Transcript, bottom edge. Deliberately EQUAL to [`COMPOSER_CLEARANCE`],
/// the list's trailing spacer: in Follow the newest row's baseline sits
/// exactly one clearance above the viewport floor, i.e. exactly at the
/// end of this ramp, so the resting transcript is fully opaque and only
/// content scrolled UNDER the floating composer dissolves. Change one and
/// change the other.
///
/// The stack is taller than the clearance while an attention panel or a
/// connection panel stands; content behind that extra height is faded but
/// not to zero. That is the same limitation the fixed-height spacer
/// already has, not a new one.
pub const TRANSCRIPT_FADE_BAND_BOTTOM: f32 = COMPOSER_CLEARANCE;

// ---------------------------------------------------------------------------
// Motion (Slice M). eave has exactly one animation — the streaming veil —
// and this is its curve.
//
// Slice N1 built the frame drive, and it does NOT live here: the shared
// pulse clock, the cubic-bezier catalog and the render tail are in
// `motion.rs`. The split is deliberate — this file owns appearance
// (colour, spacing, radius), `motion` owns timing and curves. The veil is
// the one straggler on the wrong side of that line: it predates the
// catalog, and its hand-rolled power curve is a near-match for a real
// `cubic-bezier` that `motion` can express exactly. Migrating it is a
// visual change to a shipped animation, so it belongs to a consumer
// slice, not to the slice that built the machinery.

/// The veil dissolves as `1 − (1 − t)^EXP`: fast at the start, settling
/// at the end, so a chunk reaches legibility early and the last 20 % of
/// the fade is the part you do not notice. gpui's built-in set has no
/// ease-out below quintic (`ease_out_quint` at exponent 5 snaps), and the
/// exponent is comet's, chosen against the same problem on the same
/// mechanism (`markdown/veil.rs`).
pub const VEIL_EASE_EXPONENT: f32 = 1.6;

/// [`VEIL_EASE_EXPONENT`] as the easing function `Animation::with_easing`
/// wants. Before Slice M the veil ran on `Animation::new`'s default,
/// which is `linear`.
pub fn veil_ease(t: f32) -> f32 {
    1.0 - (1.0 - t).powf(VEIL_EASE_EXPONENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Rgba;

    fn over(top: Hsla, under: (f32, f32, f32)) -> (f32, f32, f32) {
        let t: Rgba = top.into();
        (
            t.a * t.r + (1.0 - t.a) * under.0,
            t.a * t.g + (1.0 - t.a) * under.1,
            t.a * t.b + (1.0 - t.a) * under.2,
        )
    }
    fn close(got: (f32, f32, f32), want: Hsla) {
        let want: Rgba = want.into();
        for (g, w) in [(got.0, want.r), (got.1, want.g), (got.2, want.b)] {
            assert!((g - w).abs() < 2.5 / 255.0, "channel {g} vs {w}");
        }
    }
    fn luma(c: (f32, f32, f32)) -> f32 {
        (c.0 + c.1 + c.2) / 3.0
    }

    /// sRGB relative luminance (WCAG 2.1 §relative luminance). NOT `luma`:
    /// that is a channel mean, which is why nothing below was expressible
    /// before — a mean cannot answer "can this be read".
    fn relative_luminance(c: (f32, f32, f32)) -> f32 {
        fn lin(u: f32) -> f32 {
            if u <= 0.04045 {
                u / 12.92
            } else {
                ((u + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * lin(c.0) + 0.7152 * lin(c.1) + 0.0722 * lin(c.2)
    }

    /// WCAG 2.1 contrast ratio, 1.0..=21.0.
    fn contrast_ratio(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
        let (la, lb) = (relative_luminance(a), relative_luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    /// Every glass ground the app can actually have: the canvas composited
    /// over each backdrop brightness. The same sweep the elevation ladder
    /// has always used — the point of this slice is that the OTHER systems
    /// were never swept.
    fn grounds() -> Vec<(f32, f32, f32)> {
        (0..=255)
            .step_by(5)
            .map(|b| {
                let v = b as f32 / 255.0;
                over(glass_canvas(), (v, v, v))
            })
            .collect()
    }

    /// 8-bit channel units, the unit every step floor is quoted in.
    fn units(c: (f32, f32, f32)) -> f32 {
        c.0 * 255.0
    }

    /// The scale has a top, and every rank is a step you can see.
    ///
    /// **Every number here is written as a literal**, never read back from
    /// the constant it is checking — this is a contract test as much as a
    /// design one. Two other slices are being written against these exact
    /// names and values, so a silent edit to one of them has to fail here
    /// rather than surface as a mismatch three merges later.
    ///
    /// What it is really guarding is the shape. Before this slice the app
    /// had TEN named sizes — 10, 10, 11, 11.5, 12, 12, 12.5, 13, 13.5, 14 —
    /// spanning **1.4×** from smallest to largest, which is not a scale, it
    /// is a rounding error with names. Nothing announced, so the only
    /// channel left to design with was lightness, and nineteen slices spent
    /// themselves on it.
    #[test]
    fn the_type_scale_has_a_top() {
        for (name, got, want) in [
            ("DISPLAY_SIZE", DISPLAY_SIZE, 20.0),
            ("DISPLAY_LINE", DISPLAY_LINE, 26.0),
            ("NAME_SIZE", NAME_SIZE, 16.0),
            ("NAME_LINE", NAME_LINE, 22.0),
            ("BODY_SIZE", BODY_SIZE, 13.5),
            ("BODY_LINE", BODY_LINE, 21.0),
            ("DETAIL_SIZE", DETAIL_SIZE, 12.5),
            ("DETAIL_LINE", DETAIL_LINE, 17.0),
            ("MICRO_SIZE", MICRO_SIZE, 11.0),
            ("MICRO_LINE", MICRO_LINE, 15.0),
            ("SECTION_SIZE", SECTION_SIZE, 11.0),
            ("SECTION_LINE", SECTION_LINE, 14.0),
        ] {
            assert_eq!(got, want, "{name} moved; two other slices depend on it");
        }

        // The three ratios that say the scale is a scale. Driven from a
        // `Vec` rather than written as three straight-line assertions
        // because every operand is a `const` and clippy folds such an
        // assertion to a literal `true` — `assertions_on_constants` is a
        // real finding here, not a lint to silence: an assertion the
        // compiler can evaluate is one that cannot fail at test time.
        let ratios: Vec<(&str, f32, f32, &str)> = vec![
            // 20/11 = 1.818x. The old scale's 14/10 was 1.4x.
            (
                "the whole scale",
                DISPLAY_SIZE,
                MICRO_SIZE,
                "a scale a person cannot see the ends of leaves lightness as \
                 the only channel",
            ),
            (
                "the display rank over body",
                DISPLAY_SIZE,
                BODY_SIZE,
                "the top does not announce",
            ),
            // The rail row: it was 12.0 over 11.0, a 1.09x "hierarchy".
            (
                "a name over its own metadata",
                NAME_SIZE,
                MICRO_SIZE,
                "a name that does not outrank the facts under it",
            ),
        ];
        for (what, hi, lo, why) in ratios {
            assert!(hi / lo >= 1.4, "{what} spans {:.2}x — {why}", hi / lo);
        }
        // Monotone, and no two ranks a rounding error apart.
        let ranks = [
            ("display", DISPLAY_SIZE),
            ("name", NAME_SIZE),
            ("body", BODY_SIZE),
            ("detail", DETAIL_SIZE),
            ("micro", MICRO_SIZE),
        ];
        for w in ranks.windows(2) {
            let (hi_name, hi) = w[0];
            let (lo_name, lo) = w[1];
            assert!(
                hi - lo >= 1.0,
                "{hi_name} and {lo_name} are {:.1} px apart — the old scale \
                 had four pairs closer than this and read as one size",
                hi - lo
            );
        }
        // Every rank leads. A line height under 1.15x its size sets solid.
        for (name, size, line) in [
            ("display", DISPLAY_SIZE, DISPLAY_LINE),
            ("name", NAME_SIZE, NAME_LINE),
            ("body", BODY_SIZE, BODY_LINE),
            ("detail", DETAIL_SIZE, DETAIL_LINE),
            ("micro", MICRO_SIZE, MICRO_LINE),
            ("section", SECTION_SIZE, SECTION_LINE),
            ("code", CODE_SIZE, CODE_LINE),
        ] {
            assert!(
                line / size >= 1.15,
                "{name} leads at {:.2}x and sets solid",
                line / size
            );
        }
    }

    /// Every tier that carries a fact clears its floor on EVERY backdrop.
    ///
    /// The bright end is the hard end: the glass ground lightens from
    /// `#080808` to `#3b3b3b` as the desktop behind it brightens, and light
    /// text loses contrast as it goes. Measuring on a black desktop only —
    /// which is what a screenshot on this machine shows — reports `text` at
    /// 15.46:1 when its real worst case is 8.65:1.
    ///
    /// **This is the BARE-GROUND case and only that.** It puts no surface
    /// between the tier and the ground, which is how eighteen drawn
    /// combinations sat under their floor for three slices —
    /// `every_ink_clears_its_floor_on_every_surface_it_lands_on` is the one
    /// that measures a tier where it is actually painted. This one is kept
    /// because it constrains the tokens themselves: it survives a call site
    /// being deleted, and it is what a new tier gets measured against before
    /// anything paints it.
    #[test]
    fn every_text_tier_is_readable_on_any_backdrop() {
        for g in grounds() {
            for (name, tier, floor) in [
                ("text", text(), TEXT_CONTRAST_FLOOR),
                ("text_secondary", text_secondary(), TEXT_CONTRAST_FLOOR),
                ("text_tertiary", text_tertiary(), TEXT_QUIET_CONTRAST_FLOOR),
                // Slice AD: the two code tiers carry facts on the bare
                // ground too (inline code in prose, a fence body on the
                // now-transparent `inset`), and the sweep had never been
                // pointed at either of them. Both clear; the assertion is
                // here so that moving one is a decision rather than an
                // accident.
                ("code_text", code_text(), TEXT_CONTRAST_FLOOR),
                ("code_block_text", code_block_text(), TEXT_CONTRAST_FLOOR),
            ] {
                let c: Rgba = tier.into();
                let r = contrast_ratio((c.r, c.g, c.b), g);
                assert!(
                    r >= floor,
                    "{name} is {r:.2}:1 on ground {:.3}, floor {floor}",
                    g.0
                );
            }
        }
    }

    /// A disabled control is still a control.
    ///
    /// This is the assertion that retired `DISABLED_OPACITY`. It does not
    /// test a value, it tests the MECHANISM: composited over the darkest
    /// ground, pure white at that opacity reaches 3.11:1 — so the multiply's
    /// own ceiling barely touches the quiet floor, and every real token put
    /// through it lands far below (`text_tertiary` did: 1.54:1). No choice of
    /// tier could have fixed it.
    /// Slice AD: the two surfaces the markdown renderer introduced.
    ///
    /// A table header is the first place in the app that uses `overlay()`
    /// as a RESTING fill rather than as a hover wash, and a task checkbox
    /// is the first filled state colour with a glyph on top of it. Both
    /// have to clear the same floors as everything else, and the header
    /// carries a second finding: the QUIET tier would not have cleared it.
    #[test]
    fn the_markdown_surfaces_clear_the_floors_they_stand_on() {
        for g in grounds() {
            let header = over(overlay(), g);
            // The header is a step you can see...
            assert!(
                units(header) - units(g) >= SURFACE_STEP_FLOOR,
                "a table header is only {:.1} units above its ground",
                units(header) - units(g)
            );
            // ...and the ink on it is readable.
            let ink: Rgba = text().into();
            let r = contrast_ratio((ink.r, ink.g, ink.b), header);
            assert!(
                r >= TEXT_CONTRAST_FLOOR,
                "table header ink is {r:.2}:1 on ground {:.3}",
                g.0
            );
        }
        // The reason the header takes the PRIMARY tier, and body cells with
        // it: the quiet tier does not survive its own wash. Measured across
        // the whole sweep, never on one desktop — over a black backdrop
        // `text_secondary` reads 7.35:1 on this header and the problem is
        // invisible.
        let quiet: Rgba = text_secondary().into();
        let worst = grounds()
            .into_iter()
            .map(|g| contrast_ratio((quiet.r, quiet.g, quiet.b), over(overlay(), g)))
            .fold(f32::INFINITY, f32::min);
        assert!(
            worst < TEXT_CONTRAST_FLOOR,
            "text_secondary now clears the floor over overlay() everywhere \
             ({worst:.2}:1 at worst) — the comment in `render_table` \
             explaining why the header cannot use it is stale"
        );
        // Slice AG made this a narrower claim than it looks. `text_secondary`
        // still cannot carry a header on its own wash (3.97:1 at the bright
        // end after the tier's clearance nudge), but
        // `text_secondary_on_wash` could — 5.30:1 there. AD's ruling that the
        // header and its body cells both take the PRIMARY tier stands on its
        // own reason ("a table is not a caption") and was not re-litigated;
        // what changed is that "the quiet tier does not survive its own wash"
        // is now a fact about `text_secondary`, not about every ink.
        // The checkmark inside a ticked task box, against its own fill.
        // Opaque, so no ground sweep applies.
        let tick: Rgba = on_solid().into();
        let fill: Rgba = success().into();
        let r = contrast_ratio((tick.r, tick.g, tick.b), (fill.r, fill.g, fill.b));
        assert!(
            r >= TEXT_CONTRAST_FLOOR,
            "the checkmark is {r:.2}:1 on its own box"
        );
        // And the box's own edge is findable when it is empty.
        for g in grounds() {
            let rim = over(border_strong(), g);
            assert!(
                units(rim) - units(g) >= SURFACE_STEP_FLOOR,
                "an unchecked task box has no visible edge on ground {:.3}",
                g.0
            );
        }
    }

    #[test]
    fn a_container_opacity_cannot_express_disabled() {
        let ground = grounds()[0];
        let ceiling = over(hsla(0.0, 0.0, 1.0, DISABLED_OPACITY), ground);
        let r = contrast_ratio(ceiling, ground);
        assert!(
            r < TEXT_CONTRAST_FLOOR,
            "white at DISABLED_OPACITY reaches {r:.2}:1 — if this ever clears \
             the normal floor the mechanism is viable again and text_disabled \
             can go back to being an opacity"
        );

        // ...and the token that replaced it does clear the quiet floor.
        let c: Rgba = text_disabled().into();
        for g in grounds() {
            let r = contrast_ratio((c.r, c.g, c.b), g);
            assert!(r >= TEXT_QUIET_CONTRAST_FLOOR, "text_disabled {r:.2}:1");
        }
    }

    /// Every rung is a step you can SEE, not merely a step in the right
    /// direction. `glass_ladder_never_inverts_on_any_backdrop` asserts the
    /// ordering and no magnitude, so a ladder whose rungs differed by 1/255
    /// passed it. `raised -> panel` was 3.9/255.
    ///
    /// **Slice AG re-pointed this at the ladder eave paints.** It used to
    /// sweep 52 backdrops over `wash_raised`/`wash_panel`/`wash_modal`, and
    /// `panel()`, `composer()` and `surface_modal()` had **zero call sites**
    /// between them — one rung of the five was painted. The reading column's
    /// real ladder is `canvas → content_plate → raised`: the well is an inset
    /// card that SINKS below the frost, and a directive block rises inside
    /// it.
    ///
    /// The first step is the same shape as `inset` and the composer, and gets
    /// the same ruling (2026-08-20, `floating_chrome_is_always_carried_by_
    /// something`): **the fill alone cannot carry it, so the rim does.** The
    /// well's black wash drops the ground by only 2.0/255 over a black
    /// desktop — there is no room below a near-black ground — while its
    /// `border()` hairline rises 11.8/255 there. At the bright end the fill
    /// is at its strongest (−14.8) and the rim at its weakest (+11.8). The
    /// assertion is on the surface, not on either part.
    ///
    /// The second step needs no ruling and no fix: `well → card` measures
    /// +8.0 to +9.7 across the whole sweep. It was measured in Slice AF and
    /// asserted by nothing until now.
    #[test]
    fn every_rung_is_a_step_you_can_see() {
        let mut worst_carried = f32::MAX;
        for g in grounds() {
            let well = over(wash_content(), g);
            let card = over(wash_raised(), well);
            let rim = over(border(), g);

            // canvas -> content_plate: fill sinks, rim rises, and at every
            // backdrop at least one of them clears the floor.
            let fill_d = (units(g) - units(well)).abs();
            let rim_d = (units(rim) - units(g)).abs();
            let carried = fill_d.max(rim_d);
            worst_carried = worst_carried.min(carried);
            assert!(
                carried >= SURFACE_STEP_FLOOR,
                "the content well is invisible on ground {:.1}: fill {fill_d:.1}/255, \
                 rim {rim_d:.1}/255, floor {SURFACE_STEP_FLOOR}",
                units(g)
            );

            // content_plate -> raised: a plain step, no rim needed.
            let step = units(card) - units(well);
            assert!(
                step >= SURFACE_STEP_FLOOR,
                "well->card is {step:.1}/255 on ground {:.1}, floor {SURFACE_STEP_FLOOR}",
                units(g)
            );
        }
        // Clearance, not fit. Quoted rather than derived from the floor, so
        // raising SURFACE_STEP_FLOOR cannot make this pass by moving the bar
        // it is measured against.
        assert!(
            worst_carried >= 8.0,
            "the worst backdrop leaves the content well at only {worst_carried:.1}/255 \
             — that is the floor, not clearance"
        );
    }

    /// A popover is lighter than the chrome it floats on. On every backdrop.
    ///
    /// `every_rung_is_a_step_you_can_see` and
    /// `glass_ladder_never_inverts_on_any_backdrop` both measure
    /// `wash_raised`/`wash_panel`/`wash_modal` — and **`wash_panel` and
    /// `wash_modal` have no call site in the app.** The elevation ladder
    /// those two tests guard is not the ladder eave paints. What it paints
    /// is `canvas -> content_plate -> raised` in the reading column and
    /// `chrome_glass -> menu_glass` in the floating chrome, and until this
    /// test the second pair was constrained by nothing at all.
    ///
    /// It failed on the shipped tint at the bright end of the sweep by
    /// -0.8/255 — an inversion, not a tight step.
    #[test]
    fn a_popover_clears_the_chrome_it_floats_on() {
        for g in grounds() {
            let chrome = over(chrome_glass(), g);
            let menu = over(menu_glass(), chrome);
            let d = units(menu) - units(chrome);
            assert!(
                d >= SURFACE_STEP_FLOOR,
                "a popover is {d:+.1}/255 against its chrome on ground \
                 {:.1}, floor {SURFACE_STEP_FLOOR}",
                units(g)
            );
        }
    }

    /// The wash a hovered menu row composites ON TOP of the popover has to
    /// clear the popover too — the surface moved, so the thing measured
    /// against it is re-measured rather than assumed to have come along.
    #[test]
    fn a_hovered_menu_row_clears_its_popover() {
        for g in grounds() {
            let menu = over(menu_glass(), over(chrome_glass(), g));
            let row = over(overlay(), menu);
            let d = units(row) - units(menu);
            assert!(
                d >= STATE_DELTA_FLOOR,
                "a hovered menu row moves {d:.1}/255 on ground {:.1}, floor \
                 {STATE_DELTA_FLOOR}",
                units(g)
            );
        }
    }

    /// Floating chrome is always readable — by its fill, or by its rim.
    ///
    /// `chrome_glass` is `#141414` at α 0.72, so 28% of the desktop bleeds
    /// through and the surface TRACKS the backdrop: +8.6/255 above the
    /// frost over a black desktop, **exactly 0.0/255 at desktop ≈60**, and
    /// 28/255 below it over white. The tint sits inside the ground's own
    /// range (8→59), so no alpha closes that crossing — only moving the
    /// tint out of the range would, and both directions were rejected:
    /// near-black reverses the polarity the reference app has, and
    /// `#444444` is the light-gray slab the 2026-08-17 retune already threw
    /// out. Ruling (2026-08-20): **leave the fill and let the rim carry it**
    /// — the same ruling `a_sunken_plate_is_read_from_its_rim` records for
    /// `inset`.
    ///
    /// The two are complementary, and that is the whole reason this holds.
    /// Neither part clears the floor everywhere on its own:
    ///
    /// | | dark desktop | crossover (~60) | white desktop |
    /// | --- | --- | --- | --- |
    /// | fill | +8.6 | **0.0** | -28.1 |
    /// | rim | +29 | +29 | **+5.4** |
    ///
    /// Where the fill vanishes the rim is at its strongest, and where the
    /// rim washes out the fill has opened to 22/255 of separation. So the
    /// assertion is on the surface, not on either part: at every backdrop
    /// SOMETHING clears the floor. Asserting the rim alone fails at ground
    /// 51.0, and asserting the fill alone fails at the crossover — this
    /// test was written both of those ways first.
    ///
    /// Blur helps as well: floating chrome frosts what is behind it, and a
    /// frosted region reads as a surface at any luminance. It is not
    /// measurable here, so it is a bonus and never the argument.
    #[test]
    fn floating_chrome_is_always_carried_by_something() {
        let mut worst = (f32::MAX, 0.0f32, 0.0f32, 0.0f32);
        for g in grounds() {
            let chrome = over(chrome_glass(), g);
            let rim = over(border_strong(), chrome);
            let fill_d = (units(chrome) - units(g)).abs();
            let rim_d = (units(rim) - units(g)).abs();
            let carried = fill_d.max(rim_d);
            if carried < worst.0 {
                worst = (carried, units(g), fill_d, rim_d);
            }
            assert!(
                carried >= SURFACE_STEP_FLOOR,
                "floating chrome is invisible on ground {:.1}: fill {fill_d:.1}/255, \
                 rim {rim_d:.1}/255, floor {SURFACE_STEP_FLOOR}",
                units(g)
            );
        }
        // Clearance, not fit: the worst backdrop in the sweep must not be
        // sitting ON the floor. Quoted rather than derived from the floor,
        // so raising SURFACE_STEP_FLOOR cannot make this pass by moving the
        // bar it is measured against.
        assert!(
            worst.0 >= 8.0,
            "the worst backdrop leaves chrome at only {:.1}/255 (ground {:.1}, \
             fill {:.1}, rim {:.1}) — that is the floor, not clearance",
            worst.0,
            worst.1,
            worst.2,
            worst.3
        );
    }

    /// A sunken plate carries no fill, so its rim must carry it alone.
    ///
    /// **The rim is `border()`, not `border_inset()`** (Slice AG). Slice U
    /// introduced `border_inset` for this job and this test measured it, and
    /// it had **zero call sites**: all four sunken plates — the fence body,
    /// the tool payload, the approval payload and the quote block — write
    /// `.border_color(theme::border())`. Same defect as `panel`/`composer`/
    /// `surface_modal`, one token smaller and easy to miss because the
    /// unpainted token was the *stronger* of the two, so the test was harder
    /// than reality rather than easier. `border_inset` is retired; the
    /// painted rim rises 11.8→15.3/255 and clears the floor everywhere, so
    /// nothing moved on screen.
    #[test]
    fn a_sunken_plate_is_read_from_its_rim() {
        let fill: Rgba = inset().into();
        assert_eq!(fill.a, 0.0, "inset regained a fill it cannot afford");
        let mut worst = f32::MAX;
        for g in grounds() {
            let rim = over(border(), g);
            let d = units(rim) - units(g);
            worst = worst.min(d);
            assert!(
                d >= STATE_DELTA_FLOOR,
                "the inset rim is {d:.1}/255 and it is the only thing drawing \
                 the plate"
            );
        }
        // Clearance, not fit — the number is written, not derived from the
        // floor it sits above.
        assert!(
            worst >= 10.0,
            "the sunken plate's rim falls to {worst:.1}/255 at its worst \
             backdrop — that is the floor, not clearance"
        );
    }

    /// A hovered or selected thing looks hovered or selected. Both were an
    /// 8.4/255 wash at the bright end of the sweep — the N2 hover machinery
    /// fading perfectly between two indistinguishable states.
    #[test]
    fn a_state_change_is_one_you_can_find() {
        for g in grounds() {
            for (name, w) in [
                ("overlay", overlay()),
                ("overlay_strong", overlay_strong()),
                ("border_strong", border_strong()),
            ] {
                let d = units(over(w, g)) - units(g);
                assert!(
                    d >= STATE_DELTA_FLOOR,
                    "{name} moves {d:.1}/255 on ground {:.1}, floor {STATE_DELTA_FLOOR}",
                    units(g)
                );
            }
        }
    }

    /// ...and it is one you can tell apart from the OTHER state.
    ///
    /// `a_state_change_is_one_you_can_find` measures every wash against the
    /// ground it sits on and never against its neighbour, so a ladder whose
    /// rungs all cleared the ground by 13/255 and each other by 2/255 passed
    /// it clean. That is what shipped: `overlay` at α 0.08 and
    /// `overlay_strong` at α 0.09 put the hovered row and the SELECTED row
    /// **2.0/255 apart** — the rail's two most important states, drawn the
    /// same colour, in the one place a person looks to answer "which session
    /// am I in?".
    ///
    /// The floor is the same one, because the question is the same one: a
    /// state change the eye can find. Measured on the bright end of the
    /// sweep, which is the hard end — a white wash over a light ground has
    /// the least room to move.
    #[test]
    fn adjacent_states_are_a_step_apart() {
        for g in grounds() {
            let ladder = [
                ("rest", units(g)),
                ("overlay", units(over(overlay(), g))),
                ("overlay_strong", units(over(overlay_strong(), g))),
                ("overlay_strongest", units(over(overlay_strongest(), g))),
            ];
            for w in ladder.windows(2) {
                let (lo_name, lo) = w[0];
                let (hi_name, hi) = w[1];
                assert!(
                    hi - lo >= STATE_DELTA_FLOOR,
                    "{lo_name}->{hi_name} is {:.1}/255 on ground {:.1}, floor \
                     {STATE_DELTA_FLOOR}",
                    hi - lo,
                    units(g)
                );
            }

            // Slice AL: the RAIL ROW's own two fills, which are no longer
            // two rungs of the neutral ladder above. A hovered row takes
            // `overlay`; a selected one takes `accent_wash`, a tinted
            // selection rather than a grey one (comet carries `selection` as
            // a saturated colour at a third of an alpha and never as a
            // wash of white).
            //
            // The alpha was SOLVED against this assertion rather than copied
            // from comet: at α .20 the accent wash lands 1/255 above
            // `overlay` at the bright end, which is the same defect Slice AF
            // found between `overlay` and `overlay_strong` — "the row under
            // my cursor" and "the row I am in" drawn the same colour. .24
            // clears the floor at every backdrop.
            let hovered = units(over(overlay(), g));
            let selected = units(over(accent_wash(), g));
            assert!(
                selected - hovered >= STATE_DELTA_FLOOR,
                "a selected rail row is {:.1}/255 above a hovered one on \
                 ground {:.1}, floor {STATE_DELTA_FLOOR}",
                selected - hovered,
                units(g)
            );
        }
    }

    /// The glass ground is comet-parity near-black (2026-08-17 ruling):
    /// over a black backdrop it lands at `#080808`, adding almost no
    /// gray of its own, so the desktop's hue carries the surface instead
    /// of being washed toward gray (the retired `#212121` tint's failure
    /// — and with it the retired opaque-identity invariant: glass IS the
    /// look now, so it no longer needs to impersonate the opaque ladder
    /// on a black desktop). The content well darkens below the ground;
    /// attention plates are found on every backdrop.
    ///
    /// **The plates measured here are the PAINTED ones** (Slice AG). This
    /// test used to assert `wash_warning_plate` and `wash_danger_plate` rose
    /// above the ground on every backdrop — and `warning_plate()`,
    /// `warning_plate_strong()` and `danger_plate()` had zero call sites.
    /// What the app paints is `warning_plate_glass`,
    /// `warning_plate_strong_glass` and `danger_plate_glass`: the same hexes
    /// at `CHROME_GLASS_ALPHA`, which admits 28% of the backdrop.
    ///
    /// Re-pointing it found the third instance of the `chrome_glass` shape:
    /// **the quiet warning plate is 7.2/255 BELOW the frost on a white
    /// desktop, and the danger plate 5.8/255 below.** Their tints (`#312f2b`
    /// = 49, `#332c2c` = 51) sit inside the ground's own 8→59 range, so the
    /// delta `α·(tint − ground)` must pass through zero — no alpha closes it,
    /// exactly as `a_popover_clears_the_chrome_it_floats_on` records for the
    /// composer. So the assertion takes the same 2026-08-20 ruling: **leave
    /// the fill, let the ring carry it.** These panels were always ringed
    /// and the ring is doing far more than the floor at the backdrop where
    /// the fill vanishes. What changed is that it is now asserted about the
    /// surface a person sees rather than about a token nothing draws.
    ///
    /// **Slice AL: the rings this leans on were not carrying what the doc
    /// claimed.** `warning_ring()` was `warning().opacity(0.20)` and
    /// `danger_ring()` `danger().opacity(0.22)`, so AF's "let the ring carry
    /// it" ruling was resting on a rim measuring 1.4–1.5:1 against its own
    /// plate — visible as a luminance STEP, which is what this test measures,
    /// and invisible as a colour, which is what a person sees. Both tokens
    /// were retired and the call sites draw the pigment at full strength;
    /// the numbers below got larger, and `every_pigment_clears_the_surface_it_lands_on`
    /// is the assertion that now holds them there.
    #[test]
    fn glass_ground_is_near_black_and_content_sinks() {
        let ground = over(glass_canvas(), (0.0, 0.0, 0.0));
        close(ground, rgb(0x080808).into());
        assert!(luma(over(wash_content(), ground)) < luma(ground));
        let mut worst = (f32::MAX, 0.0f32, 0.0f32, 0.0f32);
        for g in grounds() {
            assert!(
                luma(over(wash_content(), g)) < luma(g),
                "the content well stopped sinking on ground {:.1}",
                units(g)
            );
            for (name, plate, ring) in [
                ("warning_plate_glass", warning_plate_glass(), warning()),
                (
                    "warning_plate_strong_glass",
                    warning_plate_strong_glass(),
                    warning(),
                ),
                ("danger_plate_glass", danger_plate_glass(), danger()),
            ] {
                let surface = over(plate, g);
                let rim = over(ring, surface);
                let fill_d = (units(surface) - units(g)).abs();
                let rim_d = (units(rim) - units(g)).abs();
                let carried = fill_d.max(rim_d);
                if carried < worst.0 {
                    worst = (carried, units(g), fill_d, rim_d);
                }
                assert!(
                    carried >= SURFACE_STEP_FLOOR,
                    "{name} is invisible on ground {:.1}: fill {fill_d:.1}/255, \
                     ring {rim_d:.1}/255, floor {SURFACE_STEP_FLOOR}",
                    units(g)
                );
            }
        }
        // Clearance, not fit, and the number is written rather than derived.
        assert!(
            worst.0 >= 8.0,
            "an attention plate falls to {:.1}/255 on ground {:.1} (fill {:.1}, \
             ring {:.1}) — that is the floor, not clearance",
            worst.0,
            worst.1,
            worst.2,
            worst.3
        );
    }

    /// The B2 diff washes obey the same law as every glass rung: on any
    /// backdrop they rise above the ground (state surfaces never read
    /// sunken) and keep their hues apart (add reads green, del reads
    /// red — the ground is achromatic, so channel order is hue).
    ///
    /// **Slice AL removed two `close()` assertions from the end of this
    /// test**, and it is worth saying why rather than letting them look like
    /// a casualty of the palette port. They pinned the washes' *pre-blends
    /// over the opaque canvas* to `#212c25` and `#2e2322` — the F.2
    /// pre-composite discipline, for a build that was **deleted on
    /// 2026-08-19** ("i never want it"). They were checking that a colour
    /// nothing paints, over a ground nothing paints, equalled two hexes
    /// derived from the very constants under test. That is the third thing
    /// this project keeps finding: a floor on a token nobody draws.
    ///
    /// What replaces them is stronger and is not derived from the constants:
    /// `oklch_accents_match_reference` pins each pigment to an sRGB triple
    /// computed independently from the CSS Color 4 matrices.
    #[test]
    fn diff_washes_rise_and_keep_their_hue_on_any_backdrop() {
        for b in (0..=255).step_by(5) {
            let v = b as f32 / 255.0;
            let ground = over(glass_canvas(), (v, v, v));
            for (wash, hue_hi, hue_lo) in [
                (success().opacity(DIFF_WASH_ALPHA), 1, 0), // g over r
                (danger().opacity(DIFF_WASH_ALPHA), 0, 1),  // r over g
            ] {
                let lit = over(wash, ground);
                assert!(luma(lit) > luma(ground), "backdrop {b}: wash sank");
                let chans = [lit.0, lit.1, lit.2];
                assert!(
                    chans[hue_hi] > chans[hue_lo],
                    "backdrop {b}: hue collapsed ({chans:?})"
                );
            }
        }
    }

    /// The ported palette, pinned to values computed **outside this file**.
    ///
    /// Porting numbers without porting the test that pins them is how a port
    /// rots, so this is comet's `oklch_accents_match_reference` with eave's
    /// own five literals added. Every expectation here is a written 8-bit
    /// triple, never `oklch_to_srgb` compared against itself.
    ///
    /// The first row is the finding this slice is proudest of: comet's
    /// `accent` and eave's `live` are the same 24 bits, arrived at
    /// independently, three weeks and one colour space apart.
    #[test]
    fn oklch_accents_match_reference() {
        fn srgb_u8(c: Hsla) -> [u8; 3] {
            let c: Rgba = c.into();
            [
                (c.r * 255.0).round() as u8,
                (c.g * 255.0).round() as u8,
                (c.b * 255.0).round() as u8,
            ]
        }
        // Tailwind neutral-950 — the value comet samples its app background
        // from, and (independently) eave's glass tint since 2026-08-17.
        assert_eq!(srgb_u8(neutral(0.145)), [10, 10, 10]);
        assert_eq!(srgb_u8(neutral(0.922)), [229, 229, 229]); // solid
        for (name, got, want) in [
            ("live / indigo-400", live(), [124, 134, 255]),
            ("accent / violet-400", accent(), [166, 132, 255]),
            ("danger / red-400", danger(), [255, 100, 103]),
            ("warning / amber-400", warning(), [255, 185, 0]),
            ("success / emerald-400", success(), [0, 212, 146]),
        ] {
            assert_eq!(srgb_u8(got), want, "{name} drifted off its Tailwind step");
        }
    }

    /// Every neutral is **exactly** achromatic — `r == g == b`, no epsilon.
    ///
    /// eave tinted its neutrals and hid its accents; comet does the inverse,
    /// and this is the half of that finding a test can hold. Seven tokens
    /// were `hsla(220/360, 0.10, 0.90, α)` and four inks were hand-picked
    /// near-greys with a blue cast (`#e7e9ec`, `#17181c`, `#f4f6f9`). A cast
    /// on every neutral surface is why nothing else in the window could read
    /// as coloured.
    ///
    /// The chromatic tokens are listed too, from the other side: a pigment
    /// that quietly lost its chroma would otherwise pass this file silently.
    #[test]
    fn neutrals_are_achromatic_and_pigments_are_not() {
        for (name, c) in [
            ("text", text()),
            ("text_secondary", text_secondary()),
            ("text_tertiary", text_tertiary()),
            ("text_secondary_on_wash", text_secondary_on_wash()),
            ("solid", solid()),
            ("on_solid", on_solid()),
            ("solid_hover", solid_hover()),
            ("glass_canvas", glass_canvas()),
            ("wash_content", wash_content()),
            ("overlay", overlay()),
            ("overlay_strong", overlay_strong()),
            ("overlay_strongest", overlay_strongest()),
            ("wash_raised", wash_raised()),
            ("border", border()),
            ("border_strong", border_strong()),
            ("edge_modal", edge_modal()),
        ] {
            let c: Rgba = c.into();
            assert_eq!(
                (c.r, c.g),
                (c.g, c.b),
                "{name} is not achromatic: {} {} {}",
                c.r,
                c.g,
                c.b
            );
        }
        // ...and the pigments are, and stay, chromatic. 0.5 of HSL
        // saturation is the brief's own bar for "not laundered into a
        // pastel".
        for (name, c) in [
            ("live", live()),
            ("success", success()),
            ("warning", warning()),
            ("danger", danger()),
            ("accent", accent()),
        ] {
            assert!(
                c.s >= 0.5,
                "{name} is at {:.2} saturation — that is a pastel, not a pigment",
                c.s
            );
        }
        // And `danger` still reads as red: hue within 15 degrees of 0.
        let h = danger().h * 360.0;
        assert!(
            h <= 15.0 || h >= 345.0,
            "danger drifted to hue {h:.1} degrees — it has to read as red"
        );
    }

    /// The reason rungs are washes, not plates: over EVERY backdrop
    /// brightness the elevation ordering holds — steps compress toward
    /// white but never invert, and the well stays sunken. Independent
    /// plates (opaque or translucent) over a glass ground fail this.
    ///
    /// **Slice AG re-pointed this at the ladder eave paints**, for the same
    /// reason as `every_rung_is_a_step_you_can_see`: it used to order
    /// `raised < panel < modal` and two of those three had no call site. The
    /// reading column is `content_plate < canvas < raised` — the well sinks
    /// below the frost and a directive block rises inside the well — and it
    /// is the ordering that must never invert, because the rails sit on the
    /// bare frost beside the well and the two are edge to edge.
    ///
    /// The floating chrome's ordering (`chrome_glass < menu_glass`) is NOT
    /// duplicated here: `a_popover_clears_the_chrome_it_floats_on` already
    /// asserts a magnitude on that pair, which subsumes the direction.
    #[test]
    fn glass_ladder_never_inverts_on_any_backdrop() {
        for b in (0..=255).step_by(5) {
            let v = b as f32 / 255.0;
            let ground = over(glass_canvas(), (v, v, v));
            let g = luma(ground);
            let well = luma(over(wash_content(), ground));
            let card = luma(over(wash_raised(), over(wash_content(), ground)));
            // `inset` left this ladder when it stopped being a fill: a plate
            // that darkens a near-black ground moves 2/255 and reads as
            // nothing. It is asserted by `a_sunken_plate_is_read_from_its_rim`
            // instead. Ordering is still the invariant here; magnitude is
            // `every_rung_is_a_step_you_can_see`.
            //
            // **`ground < card` is deliberately NOT asserted, and it is
            // false.** A directive block inside the well is fractionally
            // DARKER than the bare frost beside it from backdrop ≈140 up
            // (−0.1/255 at 140, −7/255 at white): the well's black wash takes
            // a quarter of the ground away and `wash_raised` at α 0.044 gives
            // back less than that. Asserting it was the first way this test
            // was written and it failed at backdrop 140. It is not a defect —
            // the two surfaces never touch, the well's rim and its
            // `PLATE_MARGIN` are always between them — but the retired
            // five-rung ladder implied `canvas < raised` globally, and as
            // painted that is only true on a dark desktop. Recorded here so
            // it is not rediscovered as a bug.
            assert!(
                well < g,
                "backdrop {b}: well {well} did not sink below ground {g}"
            );
            assert!(well < card, "backdrop {b}: well {well} card {card}");
        }
    }

    /// **A pigment only owes 4.5:1 when it is TEXT.** Everywhere else it is a
    /// non-text element and the bar is [`PIGMENT_CONTRAST_FLOOR`], 3:1
    /// (WCAG 2.1 §1.4.11).
    ///
    /// This is the assertion the app never had, and its absence is the whole
    /// of the operator's complaint that the saturated pigments "reach the
    /// screen as roughly one 6 px dot". Two things were true at once on
    /// `639772a`:
    ///
    ///  - The pigments carried **words** at 2.5–4.5:1 in fourteen places, so
    ///    the arithmetic said they had to become pastels.
    ///  - The pigments carried **marks** at 1.4:1 (`warning_ring`,
    ///    `danger_ring`) and 2.5:1 (a state dot on the selected row), so
    ///    where they were legal they were invisible anyway.
    ///
    /// The fix is in both directions and neither of them is the pigment:
    /// words moved off the pigment (or the pigment became their plate), and
    /// marks moved onto surfaces where 3:1 is reachable — the rail's leading
    /// strip, and a ring drawn at full strength instead of α 0.20.
    ///
    /// **The carve-out, stated rather than omitted.** A `pigment_chip` — the
    /// rail's state badge, a failed tool's word, a file's `+12`/`−3` — is a
    /// fill with its own label on it, and the label clears 5.8:1 against the
    /// fill. Its edge against the row is not what identifies it, so its fill
    /// is not in this table. Every pigment that carries a fact *alone* is.
    #[test]
    fn every_pigment_clears_the_surface_it_lands_on() {
        let well = || vec![wash_content()];
        let bare = Vec::new;
        let quiet_plate = || vec![wash_content(), warning_plate_glass()];
        let loud_plate = || vec![wash_content(), warning_plate_strong_glass()];
        let danger_plate = || vec![wash_content(), danger_plate_glass()];
        // (role, pigment, the stack it lands on)
        let marks: Vec<(&str, Hsla, Vec<Hsla>)> = vec![
            // --- the rail's leading strip, on the BARE frost. This strip
            //     exists because of these five rows: inside the card, on a
            //     selected row's `overlay_strong`, `live` reads 2.47:1 and
            //     `danger` 2.75:1.
            ("rail state mark, running", live(), bare()),
            ("rail state mark, quiet or awaiting you", warning(), bare()),
            ("rail state mark, unresponsive or failed", danger(), bare()),
            ("rail state mark, settled", text_secondary(), bare()),
            ("rail selection bar", accent(), bare()),
            // --- the reading column, on the content well
            ("tool dot, running", live(), well()),
            ("tool dot, failed", danger(), well()),
            ("undelivered directive's rule", danger(), well()),
            ("launcher error's rule", danger(), well()),
            ("task checkbox rim, done", success(), well()),
            ("approve button rim", live(), well()),
            ("deny button rim", danger(), well()),
            // ...and the same two rims with the pointer on them
            (
                "approve button rim, hovered",
                live(),
                vec![wash_content(), overlay()],
            ),
            (
                "deny button rim, hovered",
                danger(),
                vec![wash_content(), overlay()],
            ),
            // --- the attention plates. The ring is measured against BOTH
            //     of its neighbours: the plate it encloses and the well it
            //     floats on. A rim that clears one and not the other is a
            //     rim with an invisible side.
            ("attention ring, quiet — outside", warning(), well()),
            ("attention ring, quiet — inside", warning(), quiet_plate()),
            ("attention ring, loud — inside", warning(), loud_plate()),
            ("attention dot, quiet", warning(), quiet_plate()),
            ("attention dot, loud", warning(), loud_plate()),
            ("attention ring, danger — outside", danger(), well()),
            ("attention ring, danger — inside", danger(), danger_plate()),
            ("attention dot, danger", danger(), danger_plate()),
            ("connection panel ring", danger(), danger_plate()),
            ("connection panel dot", danger(), danger_plate()),
            ("kill button rim", danger(), danger_plate()),
            ("approval card ring, pending", warning(), loud_plate()),
            // --- the launcher, inside the well
            (
                "launcher tile rim, chosen — outside",
                accent(),
                vec![wash_content()],
            ),
            (
                "launcher tile rim, chosen — inside",
                accent(),
                vec![wash_content(), overlay()],
            ),
        ];

        let mut tightest = (f32::MAX, "", 0.0f32);
        for (role, pigment, stack) in &marks {
            let c: Rgba = (*pigment).into();
            for g in grounds() {
                let mut surface = g;
                for wash in stack {
                    surface = over(*wash, surface);
                }
                let r = contrast_ratio((c.r, c.g, c.b), surface);
                assert!(
                    r >= PIGMENT_CONTRAST_FLOOR,
                    "{role} is {r:.2}:1 on a surface of {:.1}/255 (backdrop ground \
                     {:.1}/255), floor {PIGMENT_CONTRAST_FLOOR}",
                    units(surface),
                    units(g)
                );
                if r - PIGMENT_CONTRAST_FLOOR < tightest.0 {
                    tightest = (r - PIGMENT_CONTRAST_FLOOR, role, r);
                }
            }
        }

        // Clearance, not fit — and the margin is derived the same way Slice
        // AG's is, at the surfaces THIS table lands on rather than at the
        // ones the ink table lands on. AF measured the token→pixel skew at
        // 1.8/255 against a 4.0/255 tolerance; 4/255 of error at the bare
        // frost's bright end (231/255 of ground, where these marks are
        // tightest) costs up to 0.22 of contrast ratio. 0.25 is written
        // here rather than derived from the floor, so raising the floor
        // cannot make this pass by moving the bar it is measured against.
        assert!(
            tightest.0 >= 0.25,
            "the tightest pigment in the app is {} at {:.3}:1 against a floor of \
             {PIGMENT_CONTRAST_FLOOR} — {:.3} of clearance, less than the 0.25 the \
             token→pixel skew costs",
            tightest.1,
            tightest.2,
            tightest.0
        );
    }

    /// **Every ink clears its floor over every surface it can land on.**
    ///
    /// This is the third and largest instance of the session's unifying
    /// defect: *Slice U put floors on TOKENS rather than on SURFACES, and
    /// chose the ground as the reference every time.*
    /// `every_text_tier_is_readable_on_any_backdrop` sweeps 52 backdrops
    /// under each tier and never puts a surface between them, so every glyph
    /// drawn on a hover wash, a selection wash, a diff wash or a plate was
    /// unconstrained. On `aac8cf4` **eighteen drawn combinations were below
    /// their floor**, the worst of them `text_tertiary` on the selected rail
    /// row at 2.09:1 against a 3.0 floor — the row that answers "which
    /// session am I in", with its directory line on it.
    ///
    /// The table below is derived from real `.bg(…)` / `.text_color(…)` call
    /// sites, NOT from the token list, because a token list lies in both
    /// directions: `panel()`/`composer()`/`surface_modal()` were swept across
    /// 52 backdrops while nothing painted them, and `chrome_glass`,
    /// `menu_glass` and every state wash carried text that nothing measured.
    /// Each row is the stack from the glass ground UP, so a row that begins
    /// with `wash_content()` is inside the reading column and one that does
    /// not is on the bare frost the rails sit on.
    ///
    /// Two rules the table encodes, both of them forced by arithmetic rather
    /// than chosen:
    ///
    ///  - **The ink is chosen for the worst surface the site can REACH**, not
    ///    for the surface it is resting on. A rail row's directory line is on
    ///    the bare ground at rest, under `overlay` when hovered and under
    ///    `overlay_strong` when selected; hover is a background fade and text
    ///    cannot fade with it, so one ink has to survive all three.
    ///  - **Capping the washes could not have worked.** At the bright end the
    ///    ground is 59.0/255, `#a8a8a8` last cleared 4.5:1 at a surface of
    ///    61/255, and [`STATE_DELTA_FLOOR`] forces three rungs above the
    ///    ground — the cheapest legal ladder lands them at 67/75/83. The old
    ///    secondary tier fails on a ladder that does not exist yet.
    #[test]
    fn every_ink_clears_its_floor_on_every_surface_it_lands_on() {
        let well = || vec![wash_content()];
        let bare = Vec::new;
        let plate = warning_plate_strong_glass();
        let sites: Vec<(&str, Hsla, f32, Vec<Hsla>)> = vec![
            // --- the session rail: rows on the BARE frost, washed by state
            (
                "rail label, unselected",
                text_secondary_on_wash(),
                TEXT_CONTRAST_FLOOR,
                vec![overlay()],
            ),
            (
                "rail label, selected",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![accent_wash()],
            ),
            (
                "rail provider chip",
                text_secondary_on_wash(),
                TEXT_CONTRAST_FLOOR,
                vec![accent_wash()],
            ),
            (
                "rail cwd/posture/shares/worktree lines",
                text_secondary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![accent_wash()],
            ),
            (
                // Slice AL: the loud half of the posture line ("NEVER
                // ASKS") and of the worktree line ("worktree missing").
                // Both were drawn in `danger()`/`warning()` inside the
                // card, at 2.75:1 and 4.28:1 against a 4.5 floor.
                "rail posture and worktree lines, loud",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![accent_wash()],
            ),
            (
                "rail worktree note",
                text_secondary_on_wash(),
                TEXT_CONTRAST_FLOOR,
                vec![accent_wash()],
            ),
            (
                "rail resource ceiling, over budget",
                warning(),
                TEXT_CONTRAST_FLOOR,
                bare(),
            ),
            (
                "rail row affordance, hovered on a selected row",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![accent_wash(), overlay()],
            ),
            (
                "rail row affordance, hovered on an unselected row",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![overlay()],
            ),
            (
                "rail '+ new'",
                text_secondary_on_wash(),
                TEXT_CONTRAST_FLOOR,
                vec![overlay()],
            ),
            (
                "rail archived shelf toggle",
                text_secondary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![overlay()],
            ),
            (
                "rail 'sessions' title",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                bare(),
            ),
            (
                "rail resource ceiling",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                bare(),
            ),
            (
                "rail empty note",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                bare(),
            ),
            // --- Slice AL: the pigment CHIPS. A saturated fill carrying a
            //     dark label is what lets `danger` stay red: the word owes
            //     4.5:1 against the FILL, which it clears by 1.3, and the
            //     fill owes nothing to the row because the label identifies
            //     it. Solving the same word for 4.5:1 against the row would
            //     have required a relative luminance of 0.56, and every red
            //     at 0.56 is a pink.
            (
                "rail state badge, failed",
                on_solid(),
                TEXT_CONTRAST_FLOOR,
                vec![danger()],
            ),
            (
                "rail state badge, waiting on you",
                on_solid(),
                TEXT_CONTRAST_FLOOR,
                vec![warning()],
            ),
            (
                "rail state badge, running",
                on_solid(),
                TEXT_CONTRAST_FLOOR,
                vec![live()],
            ),
            (
                "file row added-count chip",
                on_solid(),
                TEXT_CONTRAST_FLOOR,
                vec![success()],
            ),
            (
                "file row removed-count chip",
                on_solid(),
                TEXT_CONTRAST_FLOOR,
                vec![danger()],
            ),
            // --- titlebar, also on the bare frost
            (
                "titlebar wordmark",
                text_secondary(),
                TEXT_CONTRAST_FLOOR,
                bare(),
            ),
            (
                "titlebar session id",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                bare(),
            ),
            (
                "titlebar rail/files toggle glyph",
                text_secondary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![overlay()],
            ),
            // --- the files rail and the diff, on the bare frost
            (
                "file row basename",
                text_secondary_on_wash(),
                TEXT_CONTRAST_FLOOR,
                vec![overlay()],
            ),
            (
                "file row directory prefix",
                text_secondary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![overlay()],
            ),
            (
                "file row status glyph",
                text_secondary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![overlay()],
            ),
            (
                "files rail header + facts",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                bare(),
            ),
            (
                "diff basis toggle, selected",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![overlay()],
            ),
            (
                "diff basis toggle, unselected",
                text_secondary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![overlay()],
            ),
            (
                "diff added line",
                text_secondary_on_wash(),
                TEXT_CONTRAST_FLOOR,
                vec![diff_add_bg()],
            ),
            (
                "diff removed line",
                text_secondary_on_wash(),
                TEXT_CONTRAST_FLOOR,
                vec![diff_del_bg()],
            ),
            (
                "diff context line",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                bare(),
            ),
            // --- the reading column: everything sits on the well
            ("transcript prose", text(), TEXT_CONTRAST_FLOOR, well()),
            ("inline code", code_text(), TEXT_CONTRAST_FLOOR, well()),
            (
                "blockquote body",
                text_secondary(),
                TEXT_CONTRAST_FLOOR,
                well(),
            ),
            (
                "capped-block trailer",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                well(),
            ),
            (
                "user directive block",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), wash_raised()],
            ),
            (
                "undelivered directive's note",
                text_secondary(),
                TEXT_CONTRAST_FLOOR,
                well(),
            ),
            (
                "failed tool's chip",
                on_solid(),
                TEXT_CONTRAST_FLOOR,
                vec![danger()],
            ),
            (
                "composer caret",
                accent(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), chrome_glass()],
            ),
            (
                "composer hint, queued behind a question",
                warning(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), chrome_glass()],
            ),
            (
                "marker pill",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![wash_content(), overlay()],
            ),
            (
                "markdown table header",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), overlay()],
            ),
            ("markdown table cell", text(), TEXT_CONTRAST_FLOOR, well()),
            (
                "tool row name and payload",
                text_secondary(),
                TEXT_CONTRAST_FLOOR,
                well(),
            ),
            (
                "fence body on the sunken plate",
                code_block_text(),
                TEXT_CONTRAST_FLOOR,
                well(),
            ),
            (
                "approval card heading, pending",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), plate],
            ),
            (
                "approve / deny button word",
                text(),
                TEXT_CONTRAST_FLOOR,
                well(),
            ),
            (
                "approve / deny button word, hovered",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), overlay()],
            ),
            (
                "approval card body, pending",
                text_secondary(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), plate],
            ),
            (
                "approval card quiet line, pending",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![wash_content(), plate],
            ),
            (
                "approval card payload, pending",
                code_block_text(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), plate],
            ),
            (
                "approval card body, decided",
                text_secondary(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), overlay()],
            ),
            (
                "approval card quiet line, decided",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![wash_content(), overlay()],
            ),
            // --- floating chrome inside the well
            (
                "composer draft",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), chrome_glass()],
            ),
            (
                "composer placeholder and hint",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![wash_content(), chrome_glass()],
            ),
            (
                "down-to-bottom pill",
                text_secondary(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), chrome_glass_hover()],
            ),
            (
                "send glyph, unarmed and hovered",
                text_secondary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![wash_content(), chrome_glass(), overlay_strongest()],
            ),
            (
                "attention panel body",
                text_secondary(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), warning_plate_glass()],
            ),
            (
                "attention panel heading",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), warning_plate_glass()],
            ),
            (
                "attention panel heading, loud",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), plate],
            ),
            (
                "kill button word, hovered",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), danger_plate_glass(), danger_wash_hover()],
            ),
            (
                "attention panel quiet note",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![wash_content(), plate],
            ),
            (
                "connection panel body",
                text_secondary(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), danger_plate_glass()],
            ),
            (
                "connection panel heading",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), danger_plate_glass()],
            ),
            // --- the launcher, inside the well
            (
                "launcher tile name, live and selected",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), overlay()],
            ),
            (
                "launcher tile name, dead and selected",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![wash_content(), overlay()],
            ),
            (
                "launcher tile caption, selected",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![wash_content(), overlay()],
            ),
            (
                "launcher cwd line",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                well(),
            ),
            (
                "launcher error",
                text_secondary(),
                TEXT_CONTRAST_FLOOR,
                well(),
            ),
            (
                "launcher model button",
                text_secondary_on_wash(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), overlay()],
            ),
            // --- the popover: root-mounted, so its worst ground is the bare
            //     frost the rails sit on, not the well
            (
                "menu row label",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![menu_glass(), overlay()],
            ),
            (
                "menu row trailing hint",
                text_tertiary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![menu_glass(), overlay()],
            ),
            (
                "menu row disabled label",
                text_disabled(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![menu_glass()],
            ),
            (
                "menu row over the composer chrome",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![chrome_glass(), menu_glass()],
            ),
        ];

        let mut tightest = (f32::MAX, "", 0.0f32, 0.0f32);
        for (site, ink, floor, stack) in &sites {
            let c: Rgba = (*ink).into();
            for g in grounds() {
                let mut surface = g;
                for wash in stack {
                    surface = over(*wash, surface);
                }
                let r = contrast_ratio((c.r, c.g, c.b), surface);
                assert!(
                    r >= *floor,
                    "{site} is {r:.2}:1 on a surface of {:.1}/255 (backdrop ground \
                     {:.1}/255), floor {floor}",
                    units(surface),
                    units(g)
                );
                if r - floor < tightest.0 {
                    tightest = (r - floor, site, r, *floor);
                }
            }
        }

        // Clearance, not fit — and the margin is DERIVED, not chosen.
        //
        // `theme.rs` reasons in straight alpha arithmetic on a synthetic
        // ground; the screen goes through a compositor, a glass surface
        // sampling a real desktop, a fade, and a colour space. Slice AF
        // measured that trip for the first time and found a skew of 1.8/255
        // against a 4.0/255 tolerance (`scripts/af-state-proof.sh`). At the
        // surfaces in this table, 4/255 of surface error costs up to 0.33 of
        // contrast ratio — so a floor cleared by less than that is an
        // arithmetic claim, not a claim about what a person sees. 0.35 is
        // written here rather than derived from either floor, so raising a
        // floor cannot make this pass by moving the bar it is measured
        // against.
        assert!(
            tightest.0 >= 0.35,
            "the tightest ink in the app is {} at {:.3}:1 against a floor of {:.1} \
             — {:.3} of clearance, less than the 0.35 the token→pixel skew costs",
            tightest.1,
            tightest.2,
            tightest.3,
            tightest.0
        );
    }
}
