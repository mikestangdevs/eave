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
/// Retired from the rail. The window surface *is* the sidebar: rail
/// content paints directly on the glass, with no contrasting fill and no
/// full-height seam. Kept because a later hover/selection wash may want
/// the same lift; nothing in the frame paints it now.
pub fn rail_plate() -> Hsla {
    ink(0.055)
}

/// The content card: a near-black well, darker than the window surface.
///
/// This inverts the elevation ladder on purpose. The card is content, not
/// chrome, and the well being the darkest thing on screen is what makes it
/// read as the hero. The 1 px rim ([`card_border`]) does the edge; the
/// luminance drop does the rest. Shadows don't read on dark (F.1).
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

/// Opaque plate for the front tile of the empty-bookmark stack.
///
/// [`overlay`] is α 0.08, so two overlapping ghosts show through each
/// other. This is the composite of [`overlay_strongest`] over black,
/// baked opaque, so the middle card reads as a filled tile.
pub fn bookmark_empty_front() -> Hsla {
    grey(0x2e)
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

/// A selected rail row's rim — **a 1 px INSET shadow, not a border**, ported
/// from comet's `card_selected_shadows`.
///
/// A border would be honest and would also move the row: the card is
/// `flex_1`, so adding one shrinks its content by 2 px, and adding one only
/// when selected makes a row change width the moment you click it —
/// `a_state_change_never_moves_a_row_under_the_pointer` exists because that
/// class of bug is real here. An inset shadow with a 1 px spread and no blur
/// draws the same line inside the element's own bounds and costs **zero
/// layout**, which is why comet reaches for it in the same situation.
///
/// It is [`border_strong`] rather than a pigment. Selection is a location,
/// not an action — see [`accent`].
pub fn selection_rim() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: border_strong(),
        offset: gpui::point(px(0.0), px(0.0)),
        blur_radius: px(0.0),
        spread_radius: px(1.0),
        inset: true,
    }]
}

// ---------------------------------------------------------------------------
// Edges

pub fn border() -> Hsla {
    ink(0.07)
}
pub fn border_strong() -> Hsla {
    ink(0.14)
}
/// The content card's outer edge. White at α 0.09 — inside the 0.08–0.10
/// band that reads as a rim on the dark well without becoming a second
/// window stroke. Dedicated so [`border`] can stay the interior hairline.
pub fn card_border() -> Hsla {
    ink(0.09)
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
/// Slice AL briefly moved the selected rail row onto a tinted selection wash
/// and was overruled; the surface this tier is solved for is unchanged.
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
/// **The plate a vendor mark sits on** (Slice AO).
///
/// This exists because of a measurement, not a preference. The rail's
/// provider chip carries `icons::claude_brand()` (`#D97757`), and inside a
/// washed session card that lands at **2.55:1** — under the 3:1 non-text
/// floor. On the bare frost outside the wash it is 3.59:1 and fine, which
/// is exactly the sort of "it depends where you paint it" failure Slice AG
/// spent a whole slice on.
///
/// An opaque plate takes the question away: the mark is no longer over a
/// wash over a glass ground over a desktop of unknown brightness, it is
/// over this, and the ratio is the same on every row in every state. At
/// `#121212` the brand orange reads **6.01:1** and the fallback initials
/// (`text_secondary_on_wash`) **11.9:1** — see
/// `a_provider_mark_clears_the_non_text_floor_on_its_own_plate`.
///
/// One step off black, so it sits BELOW every surface in the elevation
/// ladder rather than on it. That is deliberate: the chip is not a raised
/// card, it is a hole in the row with a logo in it, which is what an app
/// icon is everywhere else a person has seen one.
pub fn chip_plate() -> Hsla {
    rgb(0x121212).into()
}

/// The brightest fill in the app under the pointer: the same fill, lifted.
/// The only direction available — [`solid`] is already the brightest fill,
/// so hover goes to white rather than adding a wash that would darken it
/// (Slice M).
///
/// **Integration note.** Slice AL retired this, correctly, on its own
/// branch: the armed send button became [`accent`]'s and this lost its last
/// caller, which this project counts as a defect rather than as spare
/// capacity. It survives the merge because it did not lose its last caller
/// in the integrated tree — the launcher's "Start …" button hovers with it,
/// and that button is [`solid`]'s by AL's own rule (it is always there, so
/// it is a place, not a moment). Two branches were each right about a call
/// graph neither of them could see.
pub fn inverse_hover() -> Hsla {
    rgb(0xf4f6f9).into()
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
/// **The rule, and it is the operator's, not mine: the accent marks where
/// you are ACTING, not where you are.** A selected rail row is a *location*
/// — persistent, and it is where you already were. A caret, a focused
/// composer, an armed send button are *actions* — momentary, and they are
/// where your attention is going next.
///
/// This slice shipped it the other way round first: an accent bar plus an
/// accent-tinted wash on the selected row. It was overruled, and the
/// overrule is right — the operator had already rejected a *subtler*
/// version of the same thing on another branch ("no need for purple bar when
/// selected") an hour before this shipped. Selection went back to a neutral
/// fill and a rim; the accent moved onto the three momentary things below.
/// The report keeps the whole argument, because the arithmetic that got here
/// was sound and only its target was wrong.
///
/// **Where it is spent, and what each clears** (3:1, a mark's floor):
///
///  - the **composer caret** — 6.05:1 on the composer's glass, and the one
///    element on screen that is always eave's and never the agent's;
///  - the **composer's rim while it holds a draft** — the app's only focus
///    ring, 6.05:1 against the glass inside it and 4.88:1 against the well
///    outside;
///  - the **armed send button's fill**, with [`on_solid`] on the glyph.
///
/// **Where it is NOT spent, and the measurement that decided it**: the
/// rename field, which is otherwise the perfect focus ring — the one control
/// in eave that takes the keyboard. It lives *inside* the rail row's washed
/// card, where the accent reads **2.70:1**. That is the same wall the state
/// dot hit, and the reason `RAIL_STRIP_W` exists.
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

/// Ring around the focused split pane.
///
/// Same pigment and the same job as the composer's draft rim: this is
/// where input is going next, not a location. A selected tab is a
/// location and stays on [`selection_rim`]; clicking a pane is an
/// action, so the ring is the accent's. Always laid out (transparent
/// when the pane is not focused) so focusing cannot shift the well.
pub fn pane_focus_ring() -> Hsla {
    accent()
}
/// The armed send button under the pointer — **Tailwind violet-300**,
/// `oklch(0.811 0.111 293.571)`, one step up the same ramp.
///
/// A filled control REPLACES its bg rather than compositing a wash over it,
/// so a filled control has to name its hovered fill outright (Slice M). This
/// is the accent's, and it replaces `solid_hover` — which had exactly one
/// call site, the armed send button, and lost it when that button became the
/// accent's.
pub fn accent_hover() -> Hsla {
    oklch(0.811, 0.111, 293.571)
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

/// The selected row's leading-edge bar. Brighter than [`accent`] because
/// it is 3 px of pigment on a dark ground with a wash over it: at the
/// bar's width the eye integrates the stripe rather than reading its
/// colour, so the mark has to win on luminance or it is not a mark.
pub fn accent_bar() -> Hsla {
    rgb(0x9aa2ff).into()
}

/// Panel rings for the attention surfaces. The matching *fills* live in the
/// glass tints below — Slice AG retired `warning_wash`, `warning_wash_strong`
/// and `danger_wash` along with `warning_plate`/`warning_plate_strong`/
/// `danger_plate`, all six of which had zero call sites while
/// `glass_ground_is_near_black_and_content_sinks` measured two of them.
pub fn warning_ring() -> Hsla {
    warning().opacity(0.20)
}
pub fn danger_ring() -> Hsla {
    danger().opacity(0.22)
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

// ---------------------------------------------------------------------------
// Slice AQ — one contiguous block, deliberately. Slice AL owns this file and
// is re-valuing the tiers in parallel; everything below either DELEGATES to a
// token AL already owns (so AL's value flows through without a second edit
// here) or is a colour that did not exist in any form.

/// [`solid`] under the pointer. A filled control REPLACES its bg rather than
/// compositing a wash over it, so both ends of its fade have to be named
/// (Slice M).
pub fn solid_hover() -> Hsla {
    inverse_hover()
}

/// The QUIET ink on a [`solid`] plate — a keycap printed on a button, where
/// [`on_solid`] is the label beside it.
///
/// `#6a6d75` reads **4.26:1** on `solid` and 4.78:1 on `solid_hover`, against
/// the [`TEXT_QUIET_CONTRAST_FLOOR`] the same rank clears on the dark side.
/// Opaque rather than `on_solid().opacity(..)`: an alpha would have to be
/// re-solved for the hovered fill, and a keycap that changed contrast under
/// the pointer is a hierarchy that moves when you look at it.
pub fn on_solid_quiet() -> Hsla {
    rgb(0x6a6d75).into()
}

/// The danger pigment as an INK — a word rendered IN it, as opposed to a fill
/// or a rim rendered in it.
///
/// **[`danger`] cannot carry a word anywhere eave draws one.** Measured where
/// it is actually painted rather than against the bare ground:
/// **3.95:1** on the pending approval plate, **3.19:1** on that plate under a
/// hover wash, 3.67:1 on the kill button's resting fill, 3.14:1 on its
/// hovered one — and 4.54:1 on the bare well, a fit rather than a clearance.
/// Slice U's sweep only ever measured the NEUTRAL tiers, so the saturated
/// pigment has never been swept at all;
/// `saturated_pigment_is_measured_where_it_is_drawn` is the assertion, and
/// `docs/reports/stage-aq-approval-and-popups.md` §3 carries the census.
///
/// `#f2a49e` is [`danger`]'s hue lifted until it clears 4.5:1 with the 0.35
/// of margin the token→pixel skew costs at the worst site it lands on: 4.93:1
/// on the plate under a hover wash. The same move `text_secondary_on_wash`
/// made for the quiet tier, for the same reason and against the same sweep.
///
/// **The fill and the rim keep [`danger`].** A fill is measured as a step,
/// not as contrast, and desaturating it would flatten the one hue the app
/// uses to mean "this is destructive".
pub fn danger_ink() -> Hsla {
    rgb(0xf2a49e).into()
}
// --- end Slice AQ ----------------------------------------------------------

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
// Slice BZ — three roles bezel's Theme names and eave had no name for.
//
// Adopting bezel must not create a second token system, and the rule for
// closing a gap is to EXTEND this file and derive, never to write a literal
// into the bezel global. Each one below is a function of a token above it, so
// re-valuing the pigment re-values these with it — the property that makes
// them tokens rather than constants.
//
// They are painted by bezel's components. Nothing in `ui.rs` reads them yet,
// and that is not a reason to withhold them: the alternative is bezel
// inventing the value instead, somewhere this file cannot see it.

/// Text-selection highlight in an editable field.
///
/// eave genuinely had none — the composer never rendered a selection, and
/// [`selection_rim`] is a *shadow* on a selected row, a different thing
/// wearing a similar name. Derived from [`accent`], because a selection is
/// the app speaking about the operator's own act, which is the accent's job
/// everywhere else. At 0.30 it lands between [`overlay_strong`] and
/// [`overlay_strongest`] in weight, so a selection reads heavier than a hover
/// without becoming a fill.
pub fn selection() -> Hsla {
    accent().opacity(0.30)
}

/// Warning copy in the quiet tier — secondary text on a warning surface.
///
/// [`danger_ink`] is this role for danger, and Slice AQ sampled it against the
/// quiet-tier floor. There is no sampled amber sibling, so this derives the
/// relationship instead: the pigment carried toward the reading tone, which
/// lifts it without desaturating it, so the hue survives. The fill and the rim
/// keep [`warning`] at full strength — AQ's rule, and the reason the rings were
/// re-valued.
pub fn warning_muted() -> Hsla {
    crate::motion::mix(warning(), text(), 0.35)
}

/// Success copy in the quiet tier. Same derivation as [`warning_muted`].
pub fn success_muted() -> Hsla {
    crate::motion::mix(success(), text(), 0.35)
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

/// **The announce rank.** The one line on a screen that is allowed to be
/// big, and the only rank above a heading: the launcher's "Start a
/// session", the transcript's empty-state headline. Nothing that recurs
/// may use it — a rank that appears twice on one screen has stopped
/// announcing anything.
///
/// 20/26 are [`LAUNCHER_TITLE_SIZE`]/[`LAUNCHER_TITLE_LINE`]'s values,
/// promoted to a name. The launcher already had the app's only display
/// line and had to invent a private constant for it; this is that
/// constant given a role, so the second consumer inherits the first's
/// numbers rather than picking its own.
pub const DISPLAY_SIZE: f32 = 20.0;
pub const DISPLAY_LINE: f32 = 26.0;

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

/// Names and panel headings — **on a wide surface**: the titlebar, the
/// heading of an attention, connection or approval card, markdown's h2/h3.
/// 16/22.
///
/// The titlebar was `TITLE_SIZE` = 13.0, *smaller than the body text under
/// it*, which is not a taste question.
///
/// **It is deliberately NOT the rail's session name, and that was settled by
/// the operator against this slice's first attempt.** The rail row is the
/// one place the abstract argument and the real column disagree:
///
///  - The principle still holds and is written at [`DETAIL_SIZE`] — a rank
///    is a budget across size, weight and ink, and this slice spent exactly
///    one of the three where Slice AO spent all three and got "this text is
///    gigantic".
///  - But the rail is **248 px** wide and the row already carries a provider
///    mark, a state mark, a badge and a hover strip. At 16 the name truncates
///    mid-word, and the name is the only thing on that row a person reads.
///    Spending fewer channels does not buy back glyphs.
///
/// So the rank is right and the column is too narrow for it. The rail's name
/// is [`BODY_SIZE`]; this rank is for surfaces 560 px and wider, where it has
/// room to announce.
pub const NAME_SIZE: f32 = 16.0;
pub const NAME_LINE: f32 = 22.0;

/// Reading text: transcript prose, the composer, a user directive — and a
/// session's name in the rail, which is reading text on a 248 px column.
/// **The value is unchanged** at 13.5: this slice does not churn the one
/// rank that was already solved.
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
/// Chip/label rank: tool rows, affordances, buttons.
pub const LABEL_SIZE: f32 = 12.5;
pub const LABEL_LINE: f32 = 16.0;
/// Footer / timestamps / markers.
pub const SMALL_SIZE: f32 = 11.0;
pub const SMALL_LINE: f32 = 15.0;
/// Diff lines in the files rail (B2): smallest mono rank — the rail is
/// 248 px, so the shape of the change is the signal; long lines wrap.
pub const DIFF_SIZE: f32 = 10.0;
pub const DIFF_LINE: f32 = 15.0;
/// Titlebar title.
pub const TITLE_SIZE: f32 = 13.0;
/// Tool rows: the tool name is a machine identifier — mono, so tool
/// events read as instrument facts, not prose.
pub const TOOL_NAME_SIZE: f32 = 12.0;
pub const TOOL_NAME_LINE: f32 = 16.0;
/// Composer input line: [`BODY_SIZE`] text on a taller leading so the caret
/// row does not feel cramped inside the card. A line-height override on an
/// existing rank, not an eleventh size — it names no size of its own, which
/// is why the audit of screen-named ranks left it alone.
pub const COMPOSER_LINE: f32 = 22.0;
pub const COMPOSER_MIN_H: f32 = 24.0;

// ── Slice E1: the find bar ──────────────────────────────────────────────
//
// It floats over a live page, which is the one ground in this application
// eave does not draw — so it takes the composer's material
// (`chrome_glass` + `border_strong` + `shadow_panel`), the material this
// project measured against *scrolling content underneath*, rather than the
// pointer menus' `menu_glass`, which was measured against chrome.

/// The bar's height. [`RAIL_TAB_H`] rather than a new number: it is a
/// single row of controls, which is what that rank already means here.
pub const FIND_BAR_H: f32 = RAIL_TAB_H;
/// The query field never collapses to nothing while empty, so the bar does
/// not resize under the first keystroke.
pub const FIND_FIELD_MIN_W: f32 = 120.0;
/// …and never grows past this, so a pasted paragraph does not push the
/// tally off the window. The text truncates; the SEARCH still uses all of
/// it.
pub const FIND_FIELD_MAX_W: f32 = 240.0;

/// Weight roles — the only weights the shell uses. Prose is NORMAL;
/// labels/names are MEDIUM; STRONG is reserved for escalated notices
/// and the armed send glyph. (Markdown `**bold**` inside prose keeps
/// semantic BOLD — that's content, not chrome.)
pub const WEIGHT_BODY: FontWeight = FontWeight::NORMAL;
pub const WEIGHT_LABEL: FontWeight = FontWeight::MEDIUM;
pub const WEIGHT_STRONG: FontWeight = FontWeight::SEMIBOLD;

// --- Slice AL's type-scale contract -----------------------------------
//
// Written against by Slice AO before AL landed, so these names exist here
// with AL's stated values and AL resolves the overlap at integration. The
// three ranks below are the ones the type scale was missing: a display
// rank, a NAME rank between body and display, and a section rank for
// uppercase tracked headers. `DETAIL` and `MICRO` above moved to AL's
// values in the same pass.

/// Heading ladder relative to body (waku's scale): size, line, weight.
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

/// The macOS window button's diameter. Not ours to choose — measured.
pub const TRAFFIC_D: f32 = 12.0;

/// The air between the traffic lights and the first thing in the rail
/// (Slice D4.2, repair 6).
///
/// **Derived from the lights, not from [`TITLEBAR_H`].** The grid used to
/// begin wherever the titlebar spacer happened to end, which made its
/// position a coincidence of an unrelated constant: change `TITLEBAR_H` for
/// any reason and the grid moves with it for no reason. The lights are what
/// the eye actually measures the top of the rail against, so the pad is
/// stated against them — `TRAFFIC_Y + TRAFFIC_D` is where they end.
pub const RAIL_GRID_TOP: f32 = TRAFFIC_Y + TRAFFIC_D + SPACE_3;

/// Uniform air between the window surface and the content card: top,
/// right, bottom, and the gap between the rail and the card. 8 px is
/// [`SPACE_2`].
pub const CARD_GUTTER: f32 = SPACE_2;

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
/// T3 Code's `.chat-composer-glass-shell` outer rounding (`rounded-[22px]`).
///
/// **Superseded for the composer itself by [`RADIUS_BAR`] in Slice CM2** — the
/// composer is bezel's control bar now. Kept because the value is still the
/// right one for a T3-derived pill and removing it would be a second change
/// riding along with this one.
pub const RADIUS_COMPOSER: f32 = 22.0;

// ---------------------------------------------------------------------------
// The control bar — bezel's floating composer surface (Slice CM2).
//
// bezel's `ui::control_bar` module doc names its three jobs: "Apple Music's
// transport, a desktop agent app's composer, a floating toolbar." The composer
// is the second one, and these are that component's numbers, read off
// `crates/ui/src/control_bar.rs` at the pinned rev `f86dbbf`.
//
// **They live here rather than being read from bezel at the call site** for the
// reason Slice BZ's block already gives: there is exactly one place in eave
// where a pigment or a rank is decided, and `bezel_theme.rs` builds bezel's
// Theme FROM this file rather than the other way round. A composer that reached
// into `bezel::ui::control_bar::BAR_HEIGHT` directly would be a second token
// system with one member, and the next one would be free.

/// Bar corner. bezel's `Shape::Rounded` → `Theme::BUBBLE_RADIUS` (16).
///
/// bezel offers two shapes and argues for this one in the composer's case:
/// *"a stadium reads as a media control and a composer is not one."* The pill
/// (`Shape::Pill`, radius = half the height) is the transport, not this.
pub const RADIUS_BAR: f32 = 16.0;

/// bezel's `control_bar::BAR_HEIGHT`.
///
/// **eave takes it as a MINIMUM, not a fixed height, and that is the one place
/// this surface departs from bezel's.** bezel's bar is `.h(BAR_HEIGHT)` because
/// a transport holds one row of controls forever. eave's composer grows to
/// twelve rows (`text_field::Shape::Grow { min: 1, max: 12 }`) and carries an
/// attachment row above the text, so a fixed height would delete two features
/// to gain a number. At rest the two are identical; typing is what separates
/// them.
pub const BAR_HEIGHT: f32 = 56.0;

/// bezel's `control_bar::BAR_GAP` — gap between controls, and the bar's own end
/// inset. One number for both, so the first control sits as far from the bar's
/// edge as it does from its neighbour.
pub const BAR_GAP: f32 = 8.0;

/// The bar's rim: bezel paints `hairline(0.10)`, which on a dark appearance is
/// `hsla(0, 0, 1, 0.10)` — a flat white wash, not eave's [`border`].
///
/// It does NOT change with focus or with a draft. eave's composer pill used to
/// light its rim in [`accent`] when a draft existed; the bar's rim is uniform,
/// because on a floating glass surface the rim is what separates the card from
/// the backdrop rather than a state channel. Slice CM2 moves that signal to the
/// send button, which is where a "you have something to send" fact belongs.
pub fn bar_hairline() -> Hsla {
    hsla(0.0, 0.0, 1.0, 0.10)
}

/// The bar's fill: bezel's `Theme::glass_overlay()` on a dark appearance,
/// `oklch(0.33 0 0 / 34%)` — the reference `.glass-surface` menu tint verbatim.
///
/// bezel's own comment is worth carrying, because it is the trap: an earlier
/// 65% tint was tuned *before* there was a real blur under it, and kept over
/// one it "buried the backdrop's colour and menus read as flat grey slabs next
/// to the hue-inheriting chrome (user report)". 34% lets the blurred backdrop
/// carry the card. This is why the fill and [`BLUR_MENU`] are a pair and
/// neither is tuneable alone.
pub fn bar_glass() -> Hsla {
    // `neutral`, not `oklch`, and not a hand-converted `hsla`. This file
    // already carries the CSS Color 4 conversion bezel uses (same Ottosson
    // matrices), so the tint is the reference value rather than my arithmetic
    // about it — and `neutral` is the chroma-0 door through it, which skips the
    // hue math that would otherwise return float-noise saturation for a grey.
    // `the_bar_glass_is_bezels_glass_overlay` pins the two against each other.
    neutral(0.33).opacity(0.34)
}
pub const RADIUS_BUTTON: f32 = 7.0;

/// The SELECTED row's corner (Slice D4.2, repair 4).
///
/// Named `RADIUS_SELECT`, not `RADIUS_PILL`: that name is taken, by the
/// 13 px capsule further down this file. Two different shapes both called
/// "pill" is the collision this track has already paid for three times.
///
/// **8, not [`RADIUS_BUTTON`]'s 7.** Selection is now a filled pill and
/// nothing else — no rim, no outline — so the corner is the only thing
/// carrying its shape, and it is worth one shared token rather than three
/// call sites that can drift. Wake's sidebar reaches the same number for the
/// same reason ("圆角均 8px" — every corner 8).
pub const RADIUS_SELECT: f32 = 8.0;
/// T3's `--control-radius` (8 px): ghost chips in the composer toolbar.
pub const RADIUS_CONTROL: f32 = 8.0;

/// Controls.
/// T3 Code's send control: `sm:h-8` (32 px).
pub const SEND_BUTTON: f32 = 32.0;
/// T3 `ComposerControl` `h-7` (28 px). The model chip sits in the send row.
pub const CONTROL_H: f32 = 28.0;
/// T3 `ComposerControl` `gap-1.5` is 6 px; eave rounds to the 4 px grid.
pub const CONTROL_GAP: f32 = SPACE_2;
/// T3 `ComposerControl` `px-2.5` is 10 px; eave rounds to the 4 px grid.
pub const CONTROL_PAD_X: f32 = SPACE_3;
/// T3 compact picker `max-w-42` (168 px). Caps the model name.
pub const MODEL_CHIP_MAX_W: f32 = 168.0;
/// T3 composer image thumb `size-14` (56 px).
pub const CHIP_THUMB: f32 = 56.0;
pub const STATE_DOT: f32 = 6.0;
pub const TOOL_DOT: f32 = 5.0;
// The tool row's disclosure chevron was an eave token for one commit and is
// gone again: bezel's `step_row` draws it from `expanded`, so a token here
// would name a value nothing reads. See docs/reports/stage-bz-bezel.md §5.

/// The diff's `+`/`-` column.
///
/// Slice BZ, extracted from bezel's diff pattern. eave told add from remove by
/// a wash at DIFF_WASH_ALPHA (0.10) and nothing else — weak at that alpha, and
/// nothing at all to a reader who cannot separate the two hues. A glyph in its
/// own fixed column survives both, and being fixed is what keeps the text
/// left edges aligned down the pane whatever the sign is.
pub const DIFF_SIGN_W: f32 = 10.0;

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

/// Where the transcript's empty-state card sits in the free space above
/// the composer, as a flex-grow pair: `EMPTY_ABOVE` parts of spacer over
/// it, `EMPTY_BELOW` parts under it.
///
/// **Dead centre is the modal convention** — a box in the exact middle of
/// a pane is what an alert looks like, which is why "nothing sent yet"
/// centred in a black rectangle read as a failure rather than as a rest
/// state. So the card sits above centre, but only just.
///
/// 4:5 was 1:2 until the card was photographed. At a third of the free
/// height, and shrink-wrapped to its text, the card read as *orphaned* in
/// the upper-left of a very large empty pane — it drew the eye to how
/// much nothing surrounded it, which is the opposite of what an empty
/// state is for. Widening it to the reading column and dropping it to
/// 4/9 (~0.44) keeps it clear of the alert position while letting it
/// occupy the space rather than float in it. The gap below is still
/// visibly larger than the gap above, so it belongs to the pane rather
/// than hovering over the composer.
pub const EMPTY_ABOVE: f32 = 4.0;
pub const EMPTY_BELOW: f32 = 5.0;

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

/// Soft leading edge of the load wash, as a fraction of the pill.
pub const ADDRESS_LOAD_TIP: f32 = 0.22;

/// While the page is still loading, the wash crawls toward this fraction
/// of the pill and waits. Completing is Chromium's job, not the clock.
pub const ADDRESS_LOAD_HOLD: f32 = 0.88;

/// Time constant of that crawl, in ms. `1 - exp(-t/τ)` — at τ the wash
/// is at 63 % of [`ADDRESS_LOAD_HOLD`].
pub const ADDRESS_LOAD_TAU_MS: u64 = 520;

/// Once the page has landed, ease the remaining wash to the end, in ms.
/// Shorter than this read as a dart (attempt 5).
pub const ADDRESS_LOAD_FINISH_MS: u64 = 360;

/// Fade the wash out after that finish.
pub const ADDRESS_LOAD_FADE_MS: u64 = 280;

/// Wash over the pill during a load. 0.28 read as a solid plate;
/// 0.08 vanished when the overlay had no size. 0.12 is a tint.
pub const ADDRESS_LOAD_WASH: f32 = 0.12;

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
// Session rail: content on the window surface. There is no sidebar
// plate and no divider. The content card to the right is the
// differentiated object ([`content_plate`], inset by [`CARD_GUTTER`]).

/// **The rail's RESTING width (Slice D3): 220, down from 248.**
///
/// The verdict on 248 was "too thick". The replacement is bracketed by the
/// two reference rails that could actually be measured, rather than picked
/// by feel. Both shots are 144 dpi, so device pixels halve to points:
///
/// | reference | window | rail | share |
/// | --- | --- | --- | --- |
/// | Radius (browser) | 1723 pt | **184.5 pt** | 10.7% |
/// | Radius (agent)   | 1361 pt | **270.0 pt** | 19.8% |
///
/// Measured as *the vertical seam that persists down the full column*, which
/// matters: a single scanline finds the command palette's edge instead and
/// reports 481 pt for the first shot. The third rail reference (Dia) is a
/// 524 pt crop of a marketing image, so its absolute figures are not usable
/// and only its composition is.
///
/// **The references disagree by 85 px; what they agree on is a SHARE of the
/// window** — a rail is a tenth to a fifth of it. Mike's windows are ~1720 pt
/// (display 3456×2234 at 2×), where this is 13.3%: inside the bracket and
/// near the browser end, which is the right end for an app whose hero is a
/// page.
///
/// **228 and not the 220 the directive asked for, because 220 is a number
/// the session row cannot state its facts at.** The rail's width is not a
/// free choice — three tests hold three different facts to a floor, and each
/// one implies a minimum rail:
///
/// | fact | floor | needs |
/// | --- | --- | --- |
/// | the folder stays recognisable (`a_marked_row_still_shows_its_folder`) | 80 px of path | 211 |
/// | the memory ceiling is stated whole (`the_rail_states_what_every_live_agent_costs`) | 173.4 px of text | 223 |
/// | the action strip never covers the name's start (`the_action_strip_does_not_cover_the_session_name`) | 48 px of name | **227** |
///
/// The first two were 237 and 223 before this slice moved the row's lower
/// lines off their 40 px indent (see `meta_indent` in `ui.rs`); the third is
/// untouched by that, because it is a line-ONE constraint: four 24 px targets
/// and three gaps is 108 px of corner, and the name starts where the state
/// dot and the identity chip leave it. So 227 is the binding number and 228
/// is it rounded to the 4 px unit.
///
/// **Getting to 220 means offering fewer than four affordances in that
/// corner** — which is what `ui.rs` already recommends at the strip ("the fix
/// for that is to offer fewer of them in the corner"), and which stage 3's
/// context menu makes possible. That is a product call, not a geometry one,
/// so this slice measured it and left it.
///
/// **This is a default, not the width.** [`RAIL_W_MIN`] / [`RAIL_W_MAX`]
/// bound the drag and the shell holds the live value; this constant is only
/// what a data dir with no saved width starts at. **Anything that needs the
/// CURRENT width must read `Shell::rail_w`, not this** — and the tests that
/// measure name fits read the rendered row, which is why they kept working
/// when this moved.
pub const RAIL_W: f32 = 228.0;

/// The narrowest the rail may be dragged.
///
/// Not a taste bound. A′ gave the rail the window's left edge, so it
/// *contains the traffic lights*, and those sit at a window-fixed x — a rail
/// narrower than [`TRAFFIC_CLEARANCE`] would have them hanging off its right
/// edge onto the page. That is an 86 px floor; 180 is that floor plus a name
/// slot worth having (~130 px after the row's chrome).
/// `the_rail_never_narrows_past_its_own_traffic_lights` holds it.
pub const RAIL_W_MIN: f32 = 180.0;

/// The widest the rail may be dragged.
///
/// Past this the rail stops reading as navigation beside a document and
/// starts reading as a second column — the IDE look Slice D1 deleted the
/// files rail to escape. 320 is the old 248 plus the same 72 px the floor
/// sits below it, so the resting width is centred in its own range.
pub const RAIL_W_MAX: f32 = 320.0;

/// Narrowest fraction a split pane may occupy.
///
/// Derived, not chosen: the pane header's three icon buttons (`ICON_TOGGLE`
/// × 3) plus their gaps need ~100 px to stay usable, and the default well
/// is `WINDOW_W` − `RAIL_W` − 2×`CARD_GUTTER` = 1080 − 228 − 16 = 836.
/// 100/836 ≈ 0.12. 0.20 leaves ~167 px so the address still has a slot
/// after the arrows, matching the rail handle's "a hit target must be
/// bigger than the seam it marks" reasoning applied to a pane.
pub const SPLIT_RATIO_MIN: f32 = 0.20;

/// The resize handle straddling the rail's seam.
///
/// The seam is a hairline and a 1 px hit target is not a control. 8 px
/// centred on it gives 4 either side — [`RAIL_CARD_BLEED`]'s reasoning
/// (a mark must bleed past the thing it marks) applied to a hit box.
pub const RAIL_HANDLE_W: f32 = 8.0;

/// **The collapsed rail is a SPINE, never nothing (Slice D1).**
///
/// In the old frame the strip lived in the titlebar, so collapsing the rail
/// cost you the session list and kept the tabs. A′ moves the strip INTO the
/// rail, which means a rail that collapsed to zero would take every open tab
/// off screen — the exact dead end Slice AS's operator walked into
/// (*"when i close the chat panel we need to convert to page tabs for
/// sessions so i can still switch"*). So it collapses to a spine that still
/// holds every tab as a chip and a state dot.
///
/// 44 px is `PROVIDER_CHIP` 18 + `SPACE_2` either side + the two hairlines —
/// i.e. the width a tab's identity mark needs and nothing more.
pub const RAIL_SPINE_W: f32 = 44.0;

/// A tab's row height in the vertical strip (Slice D1; 28 → 24 in D3).
///
/// **The tallest thing on the row sets it, and that was never the name.**
/// At 28 the row was an 18 px provider plate with 5 px of air; the name's 19
/// px line box fitted inside that with room to spare. So shrinking the TYPE
/// could not have shortened the list by one pixel — only the mark could, and
/// [`RAIL_MARK`] takes it to 16. The row is 16 + `SPACE_1` above and below =
/// **24**, and [`RAIL_NAME_LINE`] 19 still sits inside it on 2.5 px of air.
///
/// This is the load-bearing half of "density lives in the rail", and it is
/// the half that is easy to get wrong: the first attempt moved the type and
/// the leading down as well, which shortened nothing extra and flattened the
/// row's two ranks into one. See [`RAIL_NAME_LINE`].
///
/// Still a multiple of the 4 px unit, for D1's reason: a vertical list of
/// these stacks, and an odd height accumulates a half-pixel seam every other
/// row where a horizontal strip only ever had one.
///
/// **The freed height is not a saving, it is the pinned zone's budget.** Nine
/// rows were 9×28 + 8×4 = 284 px; they are now 9×24 + 8×4 = **248**, and the
/// 36 px difference is roughly one row of stage 3's favicon grid.
pub const RAIL_TAB_H: f32 = 24.0;

/// **How many tabs the OPEN section shows before it scrolls (Slice D1).**
///
/// The section is `flex_none` and capped rather than `flex_1`, because the
/// rail holds two lists and an unbounded one would starve the other: nine open
/// tabs would push every session off the bottom of a 760 px window.
///
/// Nine is what the default window can give it without doing that: see
/// [`rail_tabs_max_h`] for the arithmetic, which is **`9 × 28 + 8 × 4 = 284`
/// px** of a 760 px rail — the gaps are eight, not nine, and leaving them out
/// is exactly the error `nine_tabs_fit_the_rail_without_scrolling` caught on
/// this slice's first run (the ninth row landed at 334 px inside a 252 px
/// box). That is the whole reason the test measures rendered rows instead of
/// re-deriving them from these constants: a computed assertion would have
/// made the same mistake and agreed with itself.
///
/// It is a **floor on what is visible, not a limit on what is open**: the
/// tenth tab scrolls.
///
/// Against the old horizontal strip's **five** — measured, and the ceiling
/// that made the axis a frame problem rather than a styling one.
pub const RAIL_TABS_VISIBLE: usize = 9;

/// The OPEN section's height cap: [`RAIL_TABS_VISIBLE`] rows and the gaps
/// **between** them, which is one fewer gap than rows.
pub fn rail_tabs_max_h() -> f32 {
    rail_tabs_h(RAIL_TABS_VISIBLE)
}

/// The OPEN section's height for `n` tabs — the exact height, capped at
/// [`RAIL_TABS_VISIBLE`].
///
/// **It has to be definite, and that is a layout fact rather than a
/// preference.** The section is a scroll container, and a scroll container's
/// automatic minimum size is zero: given only a `max_h` it collapses to
/// nothing and the strip renders as an empty header. Slice D1 shipped exactly
/// that for one commit, and the harness could not see it — with nine tabs the
/// content measures `rail_tabs_max_h()` precisely, so the box came out the
/// right size for the wrong reason and every containment assertion passed. It
/// took a screenshot with twelve tabs in the journal to catch it.
///
/// Zero rows gives zero height rather than one row of empty box, so a rail
/// with nothing open shows a header and no gap under it.
/// The OPEN section's height when the list also carries rows that are NOT
/// tabs (Slice D4).
///
/// [`RAIL_TABS_VISIBLE`] is a promise about **tabs** — nine of them on
/// screen at the default window size, which `nine_tabs_fit_the_rail_without_scrolling`
/// measures. D4 put other kinds of row into the same list: the zone
/// gap, the zone headers, and any bookmark folder rows. Capping the
/// total at nine would have spent the tabs' budget on them and pushed
/// the ninth tab off the bottom, which is exactly what it did on this
/// slice's first run.
///
/// So the cap applies to the tab rows and the rest are added on top.
///
/// One of the extras is the folders/tabs rule, which is [`rail_rule_h`]
/// rather than [`RAIL_TAB_H`]. Everything else extra (zone headers, folder
/// rows, the standing New Tab pill) is still one tab tall. `extra` always
/// includes that rule — `emit_folders_and_tabs_head` always pushes it.
pub fn rail_rows_h(rows: usize, extra: usize) -> f32 {
    let tabs = rows.saturating_sub(extra);
    let extra_tab_h = extra.saturating_sub(1);
    let n = tabs.min(RAIL_TABS_VISIBLE) + extra_tab_h;
    if extra == 0 {
        if n == 0 {
            return 0.0;
        }
        let n = n as f32;
        return RAIL_TAB_H * n + RAIL_ROW_GAP * (n - 1.0);
    }
    let total = n + 1; // + the rule
    RAIL_TAB_H * (n as f32) + rail_rule_h() + RAIL_ROW_GAP * (total.saturating_sub(1) as f32)
}

pub fn rail_tabs_h(n: usize) -> f32 {
    let n = n.min(RAIL_TABS_VISIBLE);
    if n == 0 {
        return 0.0;
    }
    let n = n as f32;
    // `RAIL_ROW_GAP`, not `TAB_GAP` (Slice D4). This is a DEFINITE height on
    // a scroll container, so it has to be computed from the gap the list
    // actually lays out with — D4 tightened rows to 2 px and leaving 4 px
    // here would have over-measured the box by `(n - 1) * 2`, silently
    // showing a strip of the tenth row.
    RAIL_TAB_H * n + RAIL_ROW_GAP * (n - 1.0)
}

/// **The changes section's height cap (Slice D1).**
///
/// It shares the rail with the tab list and the session list now, so it is
/// `flex_none` and bounded: a repo with two hundred changed files may not push
/// the sessions off the bottom of the window. Eight rows plus the header and
/// the footer fact, which is what a 760 px rail can give it once the OPEN
/// section has taken [`rail_tabs_max_h`].
///
/// `FILES_RAIL_W` is retired with the column it sized — it was defined as
/// `= RAIL_W`, and that symmetry is what made the window read as an IDE.
pub fn files_section_max_h() -> f32 {
    RAIL_LABEL_SLOT_H * 8.0 + SECTION_LINE + SMALL_LINE + SPACE_3 * 2.0
}

/// **The pane header: the content card's top strip.**
///
/// Equal to [`TITLEBAR_H`] so the card's toolbar is a full title-row, not
/// a 32 px band under a 48 px rail-top. It lives *inside* the card's
/// radius, not as a bar above it. Both kinds of tab still resolve to this
/// line.
///
/// It exists because A had a hole. The omnibox is the *edit* affordance, and a
/// palette that appears on ⌘L is not a display — so before this slice eave
/// showed the current address **nowhere at rest** (`strip::page_label` draws a
/// tab's title and falls back to the url only when there is no title), and
/// `can_go_back`/`can_go_forward` had nowhere to live at all. For an app whose
/// stance is *every element states an observed fact*, a browser that will not
/// say where you are is a stance violation, not a missing feature.
pub const PANE_HEADER_H: f32 = TITLEBAR_H;

/// The in-header address pill. [`SPACE_1`] of air above and below so the
/// hover wash is a pill in the strip, not a second bar — and not the 20 px
/// chip (`RAIL_TAB_H` − [`SPACE_1`]) that used to sit in a 48 px header.
pub const ADDRESS_H: f32 = PANE_HEADER_H - SPACE_1 * 2.0;

/// Ground margin around the floating content card (and the air the rails
/// keep from the window edges). Frame-level inset is [`CARD_GUTTER`]; this
/// survives as the rail's own left pad.
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

/// Air inside a rail hover/selection pill, between the wash's edge and
/// the content. 6 sits in the 4–6 px the operator asked for after the
/// gutter-inset pills sat flush on the text. [`SPACE_1`] is the floor of
/// that range and still read tight against [`RADIUS_SELECT`].
pub const RAIL_HOVER_PAD: f32 = 6.0;

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
///
/// **That identity is also the answer to "does the strip still pay for
/// itself now that the selection bar is gone?"** The rail is 248 px and
/// Slice AO is fighting for every one of them, so it is a fair question, and
/// the arithmetic is: the strip costs the session name **zero** pixels,
/// because the dot cost it the same 14 either way. Folding the strip back
/// into the card would return nothing to the name and would put the state
/// mark back on the selection wash at 2.51:1. It stays for the mark alone.
pub const RAIL_STRIP_W: f32 = STATE_DOT + SPACE_2;
/// A rail row's first line — the slot the session name, the rename field and
/// the state mark all align to. The rename field draws a visible 1 px border
/// and the resting label draws a transparent one of the same weight (Stage
/// AC), so the slot is the name's line plus both hairlines.
///
/// The name is [`BODY_SIZE`], not [`NAME_SIZE`] — see that constant.
pub const RAIL_LABEL_SLOT_H: f32 = BODY_LINE + HAIRLINE * 2.0;
/// Provider identity chip: self-drawn monochrome initials on a bordered
/// rounded square. Deliberately never a third-party mark — one uniform,
/// self-drawn set for every provider (Slice A directive).
pub const PROVIDER_CHIP: f32 = 18.0;
pub const RADIUS_CHIP: f32 = 5.0;

/// The size a session's NAME is drawn at, in the rail and on a tab.
///
/// Deliberately not [`BODY_SIZE`] (13.5) and emphatically not [`NAME_SIZE`]
/// (16). Slice AO shipped 16 and the operator's word for it was
/// "gigantic": in a 248 px column a 16 px name truncates mid-word, and the
/// name is the only thing on the row a person reads. Weight and ink carry
/// the hierarchy; the size step over the folder line (12.5) is half a
/// point.
///
/// It is a constant because it was three different numbers. Slice AO wrote
/// `px(13.0)` at the two call sites, Slice AL moved [`BODY_SIZE`] to 13.5
/// underneath it, and `the_action_strip_does_not_cover_the_session_name`
/// shaped its measurement at **14.0** — so the layout, the token and the
/// test each believed something different about the same glyphs. The tests
/// still write the value as a literal (§7: never derive an expectation from
/// the constant under test) and guard it against this value, so moving the
/// name's size fails loudly instead of sliding the measurement with it.
///
/// **Slice D3 moved it 13.0 → 12.5, and that is the whole "step down one
/// notch".** The rail is navigation and the transcript is reading, so they
/// should not share a scale — but before this they nearly did: 13.0 against
/// [`BODY_SIZE`] 13.5 is a *half a point*, which is not a step, it is a
/// rounding difference wearing a step's name. The old doc said so itself
/// ("the size step over the folder line (12.5) is half a point") and treated
/// it as a fact rather than a defect. 12.5 lands the rail's name ON the
/// folder line's size, so the rail has ONE size instead of two that differ
/// by less than a pixel, and the step over the reading column becomes a
/// visible 1.0.
pub const RAIL_NAME_SIZE: f32 = 12.5;
/// The line box [`RAIL_NAME_SIZE`] sits in.
///
/// **Slice D3 tried to take this to 17 with the size, and two tests refused
/// it — correctly.** The rail row's second line (the folder, the facts) is
/// already 17, so a 17 px name gives the row two ranks with the same line box
/// and no anchor: `a_rail_row_says_its_name_louder_than_its_facts` and
/// `nothing_on_the_row_competes_with_the_session_name` both fired.
///
/// The lesson is worth keeping, because "step the rail down one notch" reads
/// like a single knob and is not: a row's height is set by its TALLEST item,
/// and a *rank* inside the row is set by the DIFFERENCE between items. Moving
/// every number down by the same amount shortens nothing (the mark was
/// already the tallest thing) and flattens the hierarchy (the gap closes).
/// The density came from [`RAIL_MARK`] and [`RAIL_TAB_H`] instead, and this
/// stayed where it was.
pub const RAIL_NAME_LINE: f32 = 19.0;

/// **The rail row's leading mark box — one slot, whatever fills it.**
///
/// A session row fills it with the provider chip; from Slice D3's stage 2 a
/// page row fills it with a favicon. Making that ONE size is a compositional
/// decision, not a saving: the references put a page and a project on
/// adjacent rows and the eye reads them as one list because the marks line
/// up. Two sizes would make the mixed strip read as two lists interleaved —
/// which is the thing D1 built the single strip to stop doing.
///
/// 16 rather than [`PROVIDER_CHIP`]'s 18 because it is what a 24 px row can
/// hold with `SPACE_1` of air above and below, and because 16 is the size a
/// favicon is actually served at (`16×16` is the ICO default; anything else
/// is being rescaled either way).
pub const RAIL_MARK: f32 = 16.0;

/// The site mark inside a tile.
///
/// 24 logical × 2 = 48 device on a retina Mac. Favicon bytes are
/// rasterized to **this** size for the tile and to [`RAIL_MARK`]×2 (32)
/// for the tab plate — one 48×48 bitmap in both slots is why the plate
/// looked muddy (Metal bilinear 48→32). See `web/favicon_adapt.rs`.
pub const BOOKMARK_TILE_MARK: f32 = 24.0;

/// A tile's corner. Roughly a quarter of the tile, matching the reference
/// frame's squircle.
pub const BOOKMARK_TILE_RADIUS: f32 = 12.0;

/// How far a folder's children sit in from the folder row (Slice T1b).
///
/// One [`RAIL_MARK`] plus the gap that follows it, so a child's own glyph
/// starts where the folder's LABEL starts — the indent lines the two ranks
/// up rather than picking an arbitrary step.
pub const BOOKMARK_CHILD_INDENT: f32 = RAIL_MARK + SPACE_2;

// ---------------------------------------------------------------------------
// Slice D4 — the rail reads by SHAPE, not by labels
// ---------------------------------------------------------------------------
//
// D4 deleted every caps zone header (`BOOKMARKS`, `PROJECTS`, `TABS`,
// `SESSIONS`). Nothing names a zone any more, so two things have to carry
// what the labels used to: the silhouette of a zone's first row, and the
// space between zones. That makes the gap load-bearing rather than
// decorative, which is why it gets its own name and a much larger value
// than any gap inside a zone.

/// The space BETWEEN zones — the whole separator, now that no header names
/// one.
///
/// `UNIT * 6`. Twelve times [`RAIL_ROW_GAP`]: the ratio is the point, not
/// the absolute size. A reader group things by relative proximity, so the
/// between-zone gap has to be unmistakably larger than the within-zone one
/// or the rail reads as a single undifferentiated list.
///
/// Not a labelled-zone seam — those are [`RAIL_RULE_PAD`] plus a hairline
/// (folders/tabs inside the strip, tabs/sessions between the two
/// containers).
pub const RAIL_ZONE_GAP: f32 = UNIT * 6.0;

/// Air above and below a zone hairline, inside the divider row.
///
/// [`SPACE_2`] (8). Folders/tabs also picks up the strip's [`RAIL_ROW_GAP`]
/// (2), so that painted gap is 10. Tabs/sessions sit in different
/// containers with no list gap, so the painted gap is 8. Both land in
/// the 6–8 range that replaced the empty [`RAIL_ZONE_GAP`] box. A
/// `border_t` on a rounded header is what made the old line hug the
/// label and curve at the ends.
pub const RAIL_RULE_PAD: f32 = SPACE_2;

/// Height of a zone divider row: pad + hairline + pad.
pub fn rail_rule_h() -> f32 {
    RAIL_RULE_PAD * 2.0 + HAIRLINE
}

/// The space between rows INSIDE a zone.
///
/// **Deliberately off the 4 px unit.** Every other gap in the rail is a
/// multiple of [`UNIT`]; this one is half of it, because rows inside a zone
/// should read as one block and [`TAB_GAP`]'s 4 px was enough air to make
/// each row look separately addressable. Against [`RAIL_ZONE_GAP`] the
/// contrast is 1:12, which is what makes zone boundaries obvious with no
/// label to announce them.
pub const RAIL_ROW_GAP: f32 = 2.0;

/// One pinned-bookmark cell.
///
/// A **fixed square**. It does not grow with the rail: dragging the seam
/// changes how many sit on a row ([`bookmark_columns`]), never the cell.
/// D4's 60 filled three `flex_1` columns of the resting rail and became a
/// ~92 px rectangle at [`RAIL_W_MAX`] — that stretch is the thing this
/// number exists to stop.
///
/// 44 is `UNIT * 11`. Content width is `rail_w − 2 * RAIL_GUTTER`. Three
/// squares plus two [`BOOKMARK_GRID_GAP`]s need `3 * 44 + 20` = **152**.
/// At [`RAIL_W_MIN`] that is 156 ≥ 152, so three from the floor up. 48
/// needed 164 and wrapped the third tile (and the `+`) onto a second
/// row at the width the operator was actually using.
pub const BOOKMARK_TILE: f32 = 44.0;

/// Empty-grid illustration tile. Two units smaller than a real pin so
/// three overlapping squares fit the rail without looking like the
/// populated grid.
pub const BOOKMARK_EMPTY_TILE: f32 = BOOKMARK_TILE - SPACE_2;

/// Horizontal step between overlapping empty tiles. [`SPACE_4`] so they
/// overlap by half a tile (`40 − 16` = 24 of shared width).
pub const BOOKMARK_EMPTY_STEP_X: f32 = SPACE_4;

/// Vertical step between overlapping empty tiles.
pub const BOOKMARK_EMPTY_STEP_Y: f32 = SPACE_2;

/// The gap between grid cells (Slice D4 names 10 px).
pub const BOOKMARK_GRID_GAP: f32 = 10.0;

/// Fewest bookmark squares on a row. Kept as a floor for a rail that
/// somehow lands under [`RAIL_W_MIN`]; at every legal width three fit.
pub const BOOKMARK_COLS_MIN: usize = 2;

/// Most bookmark squares on a row. A fourth would fit at [`RAIL_W_MAX`]
/// (`4 * 44 + 30` = 206 ≤ 296) and is refused: leftover width stays empty
/// rather than minting a fourth column or stretching the square.
pub const BOOKMARK_COLS_MAX: usize = 3;

/// Empty-grid copy. 9/12 so "Save sites you open all the time" holds
/// one line at [`RAIL_W_MIN`] (156 px of content). 10 px still wrapped.
pub const BOOKMARK_EMPTY_SIZE: f32 = 9.0;
pub const BOOKMARK_EMPTY_LINE: f32 = 12.0;

/// How many bookmark squares one rail row holds at `rail_w`.
///
/// Three once three fixed squares fit in the content width, two only if
/// they do not (below the drag floor). Never four. Pass the shell's live
/// rail width, not [`RAIL_W`].
pub fn bookmark_columns(rail_w: f32) -> usize {
    let content = rail_w - 2.0 * RAIL_GUTTER;
    let three = (BOOKMARK_COLS_MAX as f32) * BOOKMARK_TILE
        + ((BOOKMARK_COLS_MAX - 1) as f32) * BOOKMARK_GRID_GAP;
    if content >= three {
        BOOKMARK_COLS_MAX
    } else {
        BOOKMARK_COLS_MIN
    }
}

/// A ghost row's height — the same as any other rail row, because it IS a
/// row. That is the whole mechanic: an action shaped like the thing it acts
/// on, rather than a button parked in a header.
pub const RAIL_GHOST_H: f32 = RAIL_TAB_H;

// ---------------------------------------------------------------------------
// Session tabs (Slice AS) — switching sessions with the rail collapsed.
//
// Collapsing the rail used to remove the only way to reach another
// session: the app became single-session until you opened the rail again,
// and the click that did it looks like "give me more room", not "give up
// navigation". The tab strip is the switcher that survives the collapse.
//
// **A tab is not a row.** The rail states where a session works, how many
// other sessions are in those files, how dirty its worktree is, and offers
// five affordances. A tab states two things — WHICH session and WHAT STATE
// it is in — because a strip that carried the rest would need the room the
// rail needed, and then collapsing the rail would have bought nothing.

/// A tab's height.
///
/// The tallest thing on a tab is the name's line box ([`RAIL_NAME_LINE`],
/// 19) — taller than the provider plate (18) and the state dot (6) — plus
/// `SPACE_1` of air above and below. It must clear [`TITLEBAR_H`], which is
/// where the strip lives; `the_tab_strip_costs_the_transcript_no_height`
/// asserts the clearance rather than the fit.
pub const TAB_H: f32 = RAIL_NAME_LINE + SPACE_1 * 2.0;

/// Everything on a tab that is not the name: the two side pads, the state
/// dot, the provider plate, and the two gaps between them.
///
/// Named so that [`TAB_W`] is stated as *chrome plus a name slot* rather
/// than as a round number somebody liked. The dot's gap is `SPACE_1` and
/// not the rail's `SPACE_2`: the dot and the plate are one identity group
/// here, and the 4 px goes to the name.
pub const TAB_CHROME_W: f32 = SPACE_2 * 2.0 + STATE_DOT + SPACE_1 + PROVIDER_CHIP + SPACE_2;

/// The name slot on a tab. Every width below is font-kit's, shaped at
/// [`RAIL_NAME_SIZE`] and [`WEIGHT_STRONG`] — the size and weight a name is
/// actually drawn at.
///
/// **The floor is the widest PROVIDER FALLBACK label**, which is what an
/// unnamed session paints and which Slice AO's caveat 5 records as the
/// normal case in the operator's own configuration rather than the edge
/// one. Of the six providers `launcher::PROVIDERS` registers: `cursor
/// agent` **80.2 px**, `claude code` 76.5, `hermes` 47.4, `codex` 38.0.
/// Nothing here may ever ellipsis.
///
/// **The target is a three-word operator title** — `package the dmg`
/// **108.8 px**, `fix the send race` 104.8. Those are what an operator
/// types, and a strip that clipped all of them would leave two sessions in
/// the same repo looking alike. 108.8 up to the 4 px unit is **112**.
///
/// **The ceiling is the rail.** The rail gives a name 158 px; a tab gives
/// it 92, which is 58 % of it. A four-word title (`chase the rail flake`,
/// 118.5 px) therefore reads whole in the rail and clipped on a tab, and
/// that is the intended difference rather than a shortfall — a strip that
/// showed as much as a row would need the room the rail needed, and then
/// collapsing the rail would have bought nothing.
///
/// **Slice C5 took 20 px off this, from 112, and it is a real loss stated
/// rather than absorbed.** The close affordance costs a tab
/// [`TAB_CLOSE_W`] = 22 px, and five tabs have to fit the strip at the
/// default window size (`five_sessions_fit_the_strip_without_scrolling`) —
/// which caps a tab at 168 px. Something had to give and this is what gave.
///
/// What it costs: the **three-word operator title** this slot was sized for
/// (`package the dmg` at 108.8 px, `fix the send race` at 104.8) now
/// clips. What it keeps: the widest **provider fallback**, which is what an
/// unnamed session paints and which Slice AO's caveat 5 records as the
/// normal case in this operator's own configuration — `cursor agent` at
/// **80.2 px**, `claude code` at 76.5. Those must never ellipsis and they
/// still do not, by 11.8 px.
///
/// The rail is where a full title reads. That was already the stated
/// difference between the two surfaces; C5 widened it.
pub const TAB_NAME_W: f32 = 92.0;

/// A tab's width — fixed, and the same for every tab.
///
/// Fixed rather than content-sized for the reason Slice AO's rows are a
/// uniform 60 px: a strip whose tabs each take the width their name
/// happens to want has no rhythm, and the tab you are aiming for moves
/// when an unrelated session is renamed. It is also what makes
/// [`TAB_NAME_W`] a slot a test can measure a real name against.
pub const TAB_W: f32 = TAB_CHROME_W + TAB_NAME_W + TAB_CLOSE_W;

/// The gap between tabs. One unit — enough that two selection rims do not
/// touch, small enough that the strip reads as one control.
pub const TAB_GAP: f32 = SPACE_1;
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

/// **Retired by Slice D1 with the column it sized.**
///
/// It was `= RAIL_W`, and that literal symmetry — 248 left, 248 right, a
/// document between them — is what made eave's silhouette an IDE rather than
/// anything of its own. The changes list is a section of the one rail now.
/// Kept as a definition and not a call site, so a slice that wants a second
/// column back has to say so out loud.
#[allow(dead_code)]
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
///
/// Slice AQ, `SPACE_2` → `SPACE_3`. The label sat 12 px from the surface
/// edge (`MENU_ROW_INSET` 4 + this 8) while the surface's own vertical pad
/// put the first glyph 8 px from the top — so the card was tighter
/// horizontally than vertically, which is backwards for a list of words and
/// is what made the popover read as text floating with no card around it
/// (`docs/shots/aq-before-menu.png`). 16 px puts the horizontal inset a
/// clear grid unit ahead of the vertical one.
pub const MENU_ROW_PAD_X: f32 = SPACE_3;
/// Minimum gap between a row's label and its trailing state, so the two
/// never close up on a long label.
pub const MENU_ROW_GAP: f32 = SPACE_3;

/// Gap between the anchor point and the menu's edge.
pub const MENU_ANCHOR_GAP: f32 = SPACE_1;
/// How close a menu may come to the window edge before it is pushed back
/// in. `anchored()` does the pushing; this is the margin it keeps.
pub const MENU_SCREEN_MARGIN: f32 = SPACE_2;

/// The largest share of the window the `/` palette may occupy (Slice AR).
///
/// A menu that opens at a pointer can be as tall as it likes — it is over
/// content the operator was not reading. The composer palette is different:
/// it grows UPWARD out of the composer, over the transcript, while the
/// operator is mid-sentence. A third is the point at which it still leaves
/// the majority of the log visible behind it.
pub const SLASH_PALETTE_WINDOW_SHARE: f32 = 1.0 / 3.0;

/// How many rows the `/` palette shows before it stops growing.
///
/// **Derived, not chosen.** It is the most rows that fit in
/// [`SLASH_PALETTE_WINDOW_SHARE`] of [`WINDOW_MIN_H`] — the SMALLEST window
/// eave allows, because a cap that only holds on a large window is not a
/// cap. At 520 px that is 173.3 px, less the surface's own
/// [`MENU_PAD_Y`] top and bottom, over a [`MENU_ROW_H`] of 25: **6**.
///
/// Six is not a limit on what is reachable. The list is ranked and the
/// query filters it, so row seven is reached by typing one more letter —
/// which is the gesture the palette is for.
pub const SLASH_MAX_ROWS: usize =
    ((WINDOW_MIN_H * SLASH_PALETTE_WINDOW_SHARE - MENU_PAD_Y * 2.0) / MENU_ROW_H) as usize;

/// The palette's height ceiling, in px — [`SLASH_MAX_ROWS`] rows plus the
/// surface's own vertical padding. Derived from the row metric so the two
/// cannot drift apart: a taller row makes a shorter menu, automatically.
pub const SLASH_MAX_H: f32 = MENU_ROW_H * SLASH_MAX_ROWS as f32 + MENU_PAD_Y * 2.0;

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

// ---------------------------------------------------------------------------
// Slice C5 — the tab strip's close affordance, and the omnibox.
//
// Appended rather than filed beside their neighbours on purpose: a peer
// branch is mid-rewrite of the middle of this file, and a new token in the
// contested region is a merge conflict for no design reason. Move them up
// when that lands.

/// The close × on a tab: the glyph's box, which is also its hit target.
///
/// [`ICON_SM`] would be a 14 px target, under every pointer-target floor
/// worth naming; this is the glyph at `ICON_SM` inside an 18 px box, which
/// is the same size the provider plate takes on the other end of the tab.
/// A tab is therefore symmetrical, which is the reason the number is this
/// one rather than the smallest thing that could be hit.
pub const TAB_CLOSE: f32 = PROVIDER_CHIP;

/// What the close costs a tab: the box plus the gap off the name.
///
/// Counted into [`TAB_W`] rather than taken out of [`TAB_NAME_W`], because
/// the name slot is a measured decision (see `TAB_NAME_W`'s note on
/// `cursor agent` at 80.2 px and a three-word title at 108.8) and spending
/// 22 of its 112 px on an affordance would have made the widest provider
/// fallback ellipsis.
pub const TAB_CLOSE_W: f32 = TAB_CLOSE + SPACE_1;

/// A group's head chip on the strip.
///
/// Not a tab: it carries a colour and a name, has no close of its own, and
/// is `flex_none` at its content width rather than at [`TAB_W`]. Groups are
/// few and named short; a fixed-width group head would either clip `eave`
/// or reserve a tab's worth of room for four characters.
pub const GROUP_HEAD_H: f32 = TAB_H;
/// The colour swatch on a group head.
pub const GROUP_DOT: f32 = STATE_DOT;

/// The omnibox's width.
///
/// Wider than [`CONTENT_MAX_W`] would give it and narrower than the window:
/// an address bar has to hold a real url legibly, and a surface that spans a
/// 1600 px window puts the caret a long way from the tab it belongs to.
/// Clamped by `max_w` against the window, so a narrow window shrinks it
/// rather than overflowing.
pub const OMNIBOX_W: f32 = 560.0;

/// The omnibox's own line box: the input, not the dropdown.
pub const OMNIBOX_H: f32 = 44.0;

/// One suggestion row.
pub const OMNIBOX_ROW_H: f32 = 40.0;

/// How far the **floating** omnibox hangs below the top of the content
/// well. Used only when the pane header cannot host the field — a session
/// tab or the launcher. Over a page, field and suggestions are one card
/// hung from the address bar.
pub const OMNIBOX_TOP: f32 = SPACE_3;

#[cfg(test)]
mod control_bar_tokens {
    //! Slice CM2 — eave's control-bar tokens against bezel's own.
    //!
    //! These exist because the numbers in the block above were **read off**
    //! bezel's `control_bar.rs` and then re-typed here. A copied constant with
    //! no link back to its source is exactly the shape that drifts silently
    //! when the pin moves: bezel changes `BUBBLE_RADIUS`, eave keeps painting
    //! 16, and nothing anywhere fails. Each test below closes that loop by
    //! asking bezel at run time.

    use super::*;

    /// The bar's fill IS bezel's `glass_overlay`, not an approximation of it.
    ///
    /// This is not circular even though `bezel_theme` builds bezel's `Theme`
    /// from eave's tokens: `glass_overlay()` on a dark appearance is a bezel
    /// *constant* (`oklch(0.33 0 0 / 34%)`), independent of every field eave
    /// hands over. So this compares eave's copy against bezel's original.
    #[test]
    fn the_bar_glass_is_bezels_glass_overlay() {
        let bezel = crate::bezel_theme::theme().glass_overlay();
        let ours = bar_glass();
        assert!(
            close(ours, bezel),
            "bar_glass() {ours:?} has drifted from bezel's glass_overlay() {bezel:?}"
        );
    }

    /// The rim is bezel's `hairline(0.10)` — the alpha `control_bar` passes.
    #[test]
    fn the_bar_rim_is_bezels_hairline_at_ten_percent() {
        let bezel = crate::bezel_theme::theme().hairline(0.10);
        let ours = bar_hairline();
        assert!(
            close(ours, bezel),
            "bar_hairline() {ours:?} has drifted from bezel's hairline(0.10) {bezel:?}"
        );
    }

    /// The corner is `Shape::Rounded`'s, which is `Theme::BUBBLE_RADIUS`.
    ///
    /// Pinned against the constant rather than the literal 16 so that a bezel
    /// bump moves both or fails here.
    #[test]
    fn the_bar_corner_is_bezels_rounded_shape() {
        assert_eq!(
            RADIUS_BAR,
            bezel::theme::Theme::BUBBLE_RADIUS,
            "RADIUS_BAR must be bezel's Shape::Rounded radius"
        );
    }

    /// The blur under the bar is the one bezel's `material` is given.
    ///
    /// `BLUR_MENU` predates this slice — P1 named 44 for "menus/modals" and
    /// marked it reserved. The point of the assert is that the two 44s are the
    /// same number and not a coincidence two files apart.
    #[test]
    fn the_bar_blur_is_bezels_menu_blur() {
        assert_eq!(
            BLUR_MENU,
            bezel::ui::material::MENU_BLUR,
            "the bar's backdrop blur must be the one bezel's material uses"
        );
    }

    /// The bar's own geometry, against `control_bar`'s constants.
    #[test]
    fn the_bar_height_and_gap_are_bezels() {
        assert_eq!(BAR_HEIGHT, bezel::ui::control_bar::BAR_HEIGHT);
        // BAR_GAP is private in bezel (it is the bar's own inset, not a knob),
        // so this pins the relationship the module doc states instead: the end
        // inset equals the gap between controls.
        assert_eq!(BAR_GAP, 8.0);
    }

    /// Two `Hsla` equal within float noise. The oklch path runs a matrix
    /// multiply and a gamut clip, so bit-equality is not the right question.
    fn close(a: Hsla, b: Hsla) -> bool {
        const EPS: f32 = 0.001;
        (a.h - b.h).abs() < EPS
            && (a.s - b.s).abs() < EPS
            && (a.l - b.l).abs() < EPS
            && (a.a - b.a).abs() < EPS
    }
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

    /// **A vendor mark is non-text pigment and has a floor too** (Slice AO).
    ///
    /// The rail used to draw a monochrome `CC` monogram, because Slice A's
    /// directive forbade a third-party mark; the operator withdrew that
    /// directive on 2026-08-20 and the rail now paints the real Claude
    /// asterisk. That put saturated brand pigment inside a washed session
    /// card for the first time, where it measures **2.55:1** — under
    /// [`TEXT_QUIET_CONTRAST_FLOOR`], which is also the WCAG non-text floor.
    ///
    /// [`chip_plate`] is the fix and this is what makes it a fix rather
    /// than a hope. Note what is NOT swept here: the glass grounds. That is
    /// the point of an opaque plate — the mark stops depending on the
    /// desktop behind the window, so there is one number instead of 52.
    #[test]
    fn a_provider_mark_clears_the_non_text_floor_on_its_own_plate() {
        let plate: Rgba = chip_plate().into();
        let ground = (plate.r, plate.g, plate.b);
        for (name, ink) in [
            ("claude_brand", crate::icons::claude_brand()),
            // The ink every OTHER provider's mark takes, and the fallback
            // initials chip with it.
            ("text_secondary_on_wash", text_secondary_on_wash()),
            // Slice E3: the ink a page tab's globe takes when eave is not
            // holding that page. It is one rung quieter ON PURPOSE — that is
            // the whole treatment — so it is the rung most likely to be
            // pushed under the floor by a later re-solve, and it belongs in
            // this list rather than in a comment claiming it is fine.
            ("text_tertiary (unloaded tab)", text_tertiary()),
            ("text", text()),
        ] {
            let c: Rgba = ink.into();
            let r = contrast_ratio((c.r, c.g, c.b), ground);
            assert!(
                r >= TEXT_QUIET_CONTRAST_FLOOR,
                "{name} is {r:.2}:1 on the chip plate, floor \
                 {TEXT_QUIET_CONTRAST_FLOOR}"
            );
        }

        // And the reason the plate exists, asserted rather than recounted:
        // the same pigment on the SELECTED row's wash does not clear it.
        //
        // Swept over every backdrop, because that is where this hides. On a
        // dark desktop the unplated mark reads 4.71:1 and looks fine — a
        // screenshot on this machine would have said so. The frost
        // lightens to `#3b3b3b` as the desktop behind it brightens, the
        // wash lightens it further, and the mark's own luminance is fixed,
        // so the ratio falls as the room gets brighter. The worst case is
        // the one that matters and it is under the floor.
        let brand: Rgba = crate::icons::claude_brand().into();
        let worst = grounds()
            .into_iter()
            .map(|g| contrast_ratio((brand.r, brand.g, brand.b), over(overlay_strong(), g)))
            .fold(f32::INFINITY, f32::min);
        assert!(
            worst < TEXT_QUIET_CONTRAST_FLOOR,
            "the brand mark now clears {worst:.2}:1 unplated on every \
             backdrop — if that is genuinely true, `chip_plate` has stopped \
             earning its place and this test should be the one that says so"
        );
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
    /// The card is the differentiated object, and its edge is the rim.
    ///
    /// The well sinks (`canvas → content_plate`); a directive block rises
    /// inside it (`content_plate → raised`). The fill's drop can be small
    /// on a near-black ground — that's why [`card_border`] exists, the same
    /// "read a plate from its rim" reasoning `border_inset` already
    /// documents. The rim is what this test holds to the floor. Measuring
    /// the fill unaided would be asking a black wash over `#0A0A0A` to do
    /// work it cannot.
    #[test]
    fn every_rung_is_a_step_you_can_see() {
        let mut worst_rim = f32::MAX;
        let mut worst_raised = f32::MAX;
        for g in grounds() {
            let well = over(content_plate(), g);
            let rim = over(card_border(), well);
            let rim_step = units(rim) - units(well);
            worst_rim = worst_rim.min(rim_step);
            assert!(
                rim_step >= SURFACE_STEP_FLOOR,
                "the card rim is {rim_step:.1}/255 above the well on ground \
                 {:.1}, floor {SURFACE_STEP_FLOOR}",
                units(g)
            );

            let raised = over(wash_raised(), well);
            let raised_step = units(raised) - units(well);
            worst_raised = worst_raised.min(raised_step);
            assert!(
                raised_step >= SURFACE_STEP_FLOOR,
                "well->raised is {raised_step:.1}/255 on ground {:.1}, floor \
                 {SURFACE_STEP_FLOOR}",
                units(g)
            );
        }
        assert!(
            worst_rim >= 7.0,
            "the worst backdrop leaves the card rim at only {worst_rim:.1}/255 \
             — that is the floor, not clearance"
        );
        assert!(
            worst_raised >= 6.0,
            "the worst backdrop leaves a raised block at only {worst_raised:.1}/255"
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
        // **Slice AQ: both grounds, and the WELL is the real one.** This
        // swept the bare frost only, and neither consumer sits on it — the
        // composer and the ↓-bottom pill are both inside the reading
        // column's content well, which is the frost under a 0.25 black wash.
        // The conclusion was right on the wrong surface (14.4/255 carried on
        // the bare frost, 14.2/255 in the well), so nothing moved on screen;
        // what changes is that the claim is now about where the chrome is.
        // Same correction Slice AG made to the elevation ladder and Slice AF
        // to the state washes, and the fourth instance of one question:
        // **above what, and does anything paint it?**
        let mut worst = (f32::MAX, 0.0f32, 0.0f32, 0.0f32);
        for bare in grounds() {
            for (where_, g) in [
                ("bare frost", bare),
                ("the well", over(wash_content(), bare)),
            ] {
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
                    "floating chrome is invisible on {where_} at backdrop {:.1}: \
                     fill {fill_d:.1}/255, rim {rim_d:.1}/255, floor \
                     {SURFACE_STEP_FLOOR}",
                    units(bare)
                );
            }
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
            ("accent_hover / violet-300", accent_hover(), [196, 180, 255]),
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
            ("glass_canvas", glass_canvas()),
            ("wash_content", wash_content()),
            ("overlay", overlay()),
            ("overlay_strong", overlay_strong()),
            ("overlay_strongest", overlay_strongest()),
            ("bookmark_empty_front", bookmark_empty_front()),
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
            ("accent_hover", accent_hover()),
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
    /// **The card sinks; a raised block inside it rises.** The rails sit
    /// on the bare frost beside the well, so `well < ground` is the
    /// ordering that must never invert — it is what makes the card the
    /// darkest thing on screen.
    ///
    /// **`ground < raised` is deliberately NOT asserted, and it is false.**
    /// A directive block inside the well is fractionally darker than the
    /// bare frost beside it from backdrop ≈140 up: the well's black wash
    /// takes a quarter of the ground away and `wash_raised` at α 0.044
    /// gives back less than that. The two surfaces never touch —
    /// [`CARD_GUTTER`] is always between them.
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
            let well = luma(over(content_plate(), ground));
            let raised = luma(over(wash_raised(), over(content_plate(), ground)));
            assert!(
                well < g,
                "backdrop {b}: well {well} did not sink below ground {g}"
            );
            assert!(
                well < raised,
                "backdrop {b}: well {well} did not sink below a raised block {raised}"
            );
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
            // --- the accent, on the three momentary things it is spent on.
            //     The composer's rim is measured against BOTH neighbours,
            //     the well outside it and its own glass inside; a rim that
            //     clears one and not the other has an invisible side.
            ("composer rim holding a draft — outside", accent(), well()),
            (
                "composer rim holding a draft — inside",
                accent(),
                vec![wash_content(), chrome_glass()],
            ),
            (
                "armed send fill",
                accent(),
                vec![wash_content(), chrome_glass()],
            ),
            (
                "armed send fill, hovered",
                accent_hover(),
                vec![wash_content(), chrome_glass()],
            ),
            (
                "composer caret",
                accent(),
                vec![wash_content(), chrome_glass()],
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
                vec![overlay_strong()],
            ),
            (
                "rail provider chip",
                text_secondary_on_wash(),
                TEXT_CONTRAST_FLOOR,
                vec![overlay_strong()],
            ),
            (
                "rail cwd/posture/shares/worktree lines",
                text_secondary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![overlay_strong()],
            ),
            (
                // Slice AL: the loud half of the posture line ("NEVER
                // ASKS") and of the worktree line ("worktree missing").
                // Both were drawn in `danger()`/`warning()` inside the
                // card, at 2.75:1 and 3.97:1 against a 4.5 floor.
                "rail posture and worktree lines, loud",
                text(),
                TEXT_CONTRAST_FLOOR,
                vec![overlay_strong()],
            ),
            (
                "rail worktree note",
                text_secondary_on_wash(),
                TEXT_CONTRAST_FLOOR,
                vec![overlay_strong()],
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
                vec![overlay_strong(), overlay()],
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
            // Slice AQ — the decision pair. Approve's fill is OPAQUE, so its
            // two inks are measured on the plate itself and the sweep only
            // proves the ground cannot reach them.
            (
                "approve label",
                on_solid(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), plate, solid()],
            ),
            (
                "approve label, hovered",
                on_solid(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), plate, solid_hover()],
            ),
            (
                "approve keycap",
                on_solid_quiet(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![wash_content(), plate, solid()],
            ),
            (
                "approve keycap, hovered",
                on_solid_quiet(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![wash_content(), plate, solid_hover()],
            ),
            (
                "deny label",
                danger_ink(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), plate],
            ),
            (
                "deny label, hovered",
                danger_ink(),
                TEXT_CONTRAST_FLOOR,
                vec![wash_content(), plate, danger_wash_strong()],
            ),
            (
                "deny keycap",
                text_secondary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![wash_content(), plate],
            ),
            (
                "deny keycap, hovered",
                text_secondary(),
                TEXT_QUIET_CONTRAST_FLOOR,
                vec![wash_content(), plate, danger_wash_strong()],
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
                "send glyph, armed",
                on_solid(),
                TEXT_CONTRAST_FLOOR,
                vec![accent()],
            ),
            (
                "send glyph, armed and hovered",
                on_solid(),
                TEXT_CONTRAST_FLOOR,
                vec![accent_hover()],
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

    /// **Slice AQ.** The table above enumerates the NEUTRAL tiers. The
    /// saturated pigment — `live`, `success`, `warning`, `danger` — has never
    /// been swept anywhere in this file, in any form, on any surface, and it
    /// carries words at twenty-one sites.
    ///
    /// This is the third instance of one defect and the first time the class
    /// has been named twice in a row: Slice U put floors on TOKENS rather
    /// than on SURFACES, Slice AF found it in the state ladder, Slice AD
    /// found it in quiet text under a wash — and all three were about the
    /// neutral ramp. **The question is still "above what, and does anything
    /// paint it?", and for the state colours the answer was "nothing has ever
    /// asked".**
    ///
    /// What the sweep found, measured at the site rather than against the
    /// bare ground: **sixteen of twenty-one drawn combinations are under
    /// their floor or fit it without clearance.** The worst is the rail's
    /// unverified-posture line at **2.10:1** on a hovered selected row. The
    /// two `danger` labels this slice owns were **3.95:1** and **3.19:1**;
    /// fixing those four leaves **twelve** sites of recorded debt below.
    ///
    /// # Why this test does not simply demand the floor everywhere
    ///
    /// Because sixteen of those sites are in three other slices' regions,
    /// and one of them — the rail — cannot be fixed by any ink. The rail row
    /// wash is the ladder's third rung, and solving a saturated ink against
    /// it lands outside the gamut, which is the same wall `text_ghost` hit
    /// from the other direction. That is a slice, not a line.
    ///
    /// So the shape is a **ratchet**, the same one a suite uses to carve out
    /// pre-existing failures by name. Every site is measured. A site marked
    /// [`Ink::Clears`] must clear its floor with the 0.35 the token→pixel
    /// skew costs. A site marked [`Ink::Under`] must be **no worse than the
    /// number recorded here** — so the debt is executable and named, a fix
    /// passes (and then moves its own row), and nothing can quietly get
    /// worse. The recorded numbers are literals; they are what this sweep
    /// measured on 2026-08-20, not expressions over the tokens they check.
    // The kill button's hovered label measures 3.14:1 and clippy reads that
    // as a fumbled π. It is a contrast ratio; rounding it to placate the lint
    // would falsify the one number the row exists to record.
    #[allow(clippy::approx_constant)]
    #[test]
    fn saturated_pigment_is_measured_where_it_is_drawn() {
        /// `Clears` = fixed, and held to the floor. `Under(worst)` = known
        /// debt, held to the number it was found at.
        enum Ink {
            Clears,
            Under(f32),
        }
        use Ink::*;
        let plate = warning_plate_strong_glass();
        let well = wash_content();
        // (site, region, ink, stack, status). The floor is TEXT_CONTRAST_FLOOR
        // for every row: each of these is a word, and a word that names a
        // failure is not structural chrome.
        let sites: Vec<(&str, &str, Hsla, Vec<Hsla>, Ink)> = vec![
            // --- the two this slice owns, fixed
            (
                "approve label",
                "AQ",
                on_solid(),
                vec![well, plate, solid()],
                Clears,
            ),
            ("deny label", "AQ", danger_ink(), vec![well, plate], Clears),
            (
                "deny label, hovered",
                "AQ",
                danger_ink(),
                vec![well, plate, danger_wash_strong()],
                Clears,
            ),
            // --- already clear before this slice, and pinned so they stay
            (
                "approval head 'needs your approval'",
                "AQ",
                warning(),
                vec![well, plate],
                Clears,
            ),
            (
                "composer hint while a question is open",
                "AQ",
                warning(),
                vec![well, chrome_glass()],
                Clears,
            ),
            (
                "attention label, quiet",
                "chrome",
                warning(),
                vec![well, warning_plate_glass()],
                Clears,
            ),
            (
                "attention label, prominent",
                "chrome",
                warning(),
                vec![well, plate],
                Clears,
            ),
            (
                "files rail resource ceiling, over budget",
                "files",
                warning(),
                vec![],
                Clears,
            ),
            // --- the debt, by region. Every one of these is a word rendered
            //     in `danger` or `success`, and `danger` as an INK is the
            //     single token behind eleven of the thirteen.
            (
                "rail posture line, unverified (selected)",
                "rail",
                danger(),
                vec![overlay_strong()],
                Under(2.52),
            ),
            (
                "rail posture line, unverified (selected + hover)",
                "rail",
                danger(),
                vec![overlay_strong(), overlay()],
                Under(2.10),
            ),
            (
                "rail worktree missing",
                "rail",
                warning(),
                vec![overlay_strong()],
                Under(3.98),
            ),
            (
                "diff stat adds",
                "files",
                success(),
                vec![overlay()],
                Under(4.41),
            ),
            (
                "diff stat dels",
                "files",
                danger(),
                vec![overlay()],
                Under(2.96),
            ),
            (
                "connection panel 'engine unavailable'",
                "chrome",
                danger(),
                vec![well, danger_plate_glass()],
                Under(4.48),
            ),
            (
                "attention label, unresponsive/failed",
                "chrome",
                danger(),
                vec![well, danger_plate_glass()],
                Under(4.48),
            ),
            (
                "kill button label",
                "chrome",
                danger(),
                vec![well, danger_plate_glass(), danger_wash_strong()],
                Under(3.67),
            ),
            (
                "kill button label, hovered",
                "chrome",
                danger(),
                vec![well, danger_plate_glass(), danger_wash_hover()],
                Under(3.14),
            ),
            (
                "undelivered send note",
                "transcript",
                danger(),
                vec![well],
                Under(4.54),
            ),
            (
                "tool status 'failed'/'interrupted'",
                "transcript",
                danger(),
                vec![well],
                Under(4.54),
            ),
            (
                "launcher error",
                "launcher",
                danger(),
                vec![well],
                Under(4.54),
            ),
        ];

        let mut debt = 0;
        for (site, region, ink, stack, status) in &sites {
            let c: Rgba = (*ink).into();
            let mut got = f32::MAX;
            for g in grounds() {
                let mut surface = g;
                for wash in stack {
                    surface = over(*wash, surface);
                }
                got = got.min(contrast_ratio((c.r, c.g, c.b), surface));
            }
            match status {
                Clears => assert!(
                    got >= TEXT_CONTRAST_FLOOR + 0.35,
                    "[{region}] {site} is {got:.2}:1 at its worst backdrop — a word \
                     needs {TEXT_CONTRAST_FLOOR} plus the 0.35 the token→pixel skew \
                     costs"
                ),
                Under(recorded) => {
                    debt += 1;
                    assert!(
                        got >= *recorded - 0.01,
                        "[{region}] {site} got WORSE: {got:.2}:1 against the \
                         {recorded:.2}:1 recorded on 2026-08-20. This site is \
                         already under the {TEXT_CONTRAST_FLOOR} floor; it may be \
                         fixed, it may not be deepened"
                    );
                    assert!(
                        got < TEXT_CONTRAST_FLOOR + 0.35,
                        "[{region}] {site} now reads {got:.2}:1 and clears the floor \
                         — somebody fixed it. Move this row to `Clears` and drop the \
                         recorded number, or the next regression here goes unnoticed"
                    );
                }
            }
        }
        // The size of the debt, as a literal. A new site drawn in a saturated
        // pigment lands in one of the two arms above; this is what makes
        // *adding* one a deliberate act rather than a quiet one.
        assert_eq!(
            debt, 12,
            "the saturated-pigment debt is 12 sites (2026-08-20). Changing this \
             number means a site was fixed or added — say which in the commit"
        );
    }

    /// **Slice AQ.** `a_popover_clears_the_chrome_it_floats_on` measures the
    /// menu against `chrome_glass`, which is the one ground a menu almost
    /// never opens over: chrome is the composer strip and the ↓-bottom pill.
    /// Menus anchor at the pointer, so the grounds they actually land on are
    /// the reading column's well and the bare frost the rails sit on.
    ///
    /// Measured on those two, the popover's FILL **inverts**: −3.9/255
    /// against the well and **−17.5/255 against the bare frost** at the
    /// bright end of the backdrop sweep. A popover that goes darker than
    /// what it floats on is an edgeless rectangle, which is exactly what
    /// `docs/shots/aq-before-menu.png` shows.
    ///
    /// # The ruling, which is the same ruling twice already
    ///
    /// **Leave the fill; let the rim carry it.** `menu_glass`'s tint
    /// (`#282828`, 40/255) sits INSIDE the range of grounds it must clear
    /// (8→59/255), and the step is `α · (tint − ground)` — so it passes
    /// through zero and no alpha closes it. Only a tint above 65/255 would,
    /// and that is `#444444`, the light-gray slab the 2026-08-17 retune
    /// already threw out. This is the third surface to reach that wall:
    /// `a_sunken_plate_is_read_from_its_rim` recorded it for `inset`, and
    /// Slice AF recorded it for `chrome_glass`.
    ///
    /// So this test asserts what is actually load-bearing — that the RIM
    /// clears every ground the popover can float over — and states the
    /// inversion rather than pretending the fill does the work. The
    /// composer is measured the same way, against the well it sits in
    /// rather than against the bare frost, for the same reason.
    #[test]
    fn a_popover_is_read_from_its_rim_on_every_ground_it_floats_over() {
        // Resolved eagerly rather than as three closures: a menu opens over
        // whatever happens to be under the pointer, so "the grounds a
        // popover can float over" is a list of surfaces, not a list of
        // functions.
        let under = [
            ("bare frost", grounds()),
            (
                "the well",
                grounds()
                    .into_iter()
                    .map(|g| over(wash_content(), g))
                    .collect(),
            ),
            (
                "the chrome",
                grounds()
                    .into_iter()
                    .map(|g| over(chrome_glass(), over(wash_content(), g)))
                    .collect(),
            ),
        ];

        let mut worst_fill = (f32::MAX, "");
        for (name, surfaces) in under {
            for (g, ground) in grounds().into_iter().zip(surfaces) {
                let menu = over(menu_glass(), ground);
                let rim = over(edge_modal(), menu);
                // The rim is the carrier: it has to be findable against what
                // is OUTSIDE the menu, which is the edge the eye looks for.
                let d = units(rim) - units(ground);
                assert!(
                    d >= SURFACE_STEP_FLOOR,
                    "the popover's rim is {d:+.1}/255 against {name} on backdrop \
                     {:.1}/255, floor {SURFACE_STEP_FLOOR} — the fill is ruled out \
                     as the carrier, so this is the whole edge",
                    units(g)
                );
                let fill = units(menu) - units(ground);
                if fill < worst_fill.0 {
                    worst_fill = (fill, name);
                }
            }
        }
        // The finding, pinned so that a later tint change has to confront it
        // rather than inherit it. If someone lifts the tint out of the
        // grounds' range this fails and the ruling above is what they should
        // read first.
        assert!(
            worst_fill.0 < 0.0,
            "the popover's fill now clears every ground it floats over \
             ({:+.1}/255 at worst, over {}) — the rim-carries-it ruling is \
             stale and `menu_glass`'s doc comment should say so",
            worst_fill.0,
            worst_fill.1
        );
    }
}
