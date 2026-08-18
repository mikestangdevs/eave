//! The eave visual language — every color, size, and metric the shell
//! paints. Nothing outside this module states a hex or a px design value.
//!
//! Stance: eave reads as an instrument over a delegated process, not a
//! chat app. Every element states an observed fact; color speaks only
//! when state deserves attention (live activity, warning, failure);
//! nothing performs or decorates. Palette and type scale derive from
//! waku's (solid dark surfaces, `.SystemUIFont` prose, white-alpha
//! hairlines) — the closest reference that works on our upstream gpui
//! pin and locked opaque window (spec 03 §No-fork consequences).

use std::sync::OnceLock;

use gpui::{hsla, px, rgb, rgba, BoxShadow, FontWeight, Hsla, Pixels};

// ---------------------------------------------------------------------------
// The glass ground (default since the 2026-08-17 ruling; `EAVE_VIBRANCY=0`
// keeps the opaque ladder as the tested fallback). The default flips the
// window to Blurred and every surface token below to its glass variant;
// nothing outside theme.rs branches on it except the window-background
// option itself.
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

/// True (the default): Blurred window + the glass ladder — the shipped
/// look per the 2026-08-17 ruling (comet's glass shell is the reference;
/// the opaque build read as flat gray). `EAVE_VIBRANCY=0` forces the old
/// opaque ladder — an escape hatch and the second tested mode, no longer
/// a pending spike. Read once; the window cannot change appearance
/// mid-run anyway.
pub fn vibrancy() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| std::env::var("EAVE_VIBRANCY").map(|v| v != "0").unwrap_or(true))
}

// The glass variants (private so both modes stay unit-testable in one
// process — the public fns branch on the env flag).

/// Glass ground: α 0.80 (0xCC), source `#0A0A0A` — near-black so the
/// blurred desktop's hue carries the surface (comet parity; see above).
fn glass_canvas() -> Hsla {
    rgba(0x0a0a0acc).into()
}
/// The content well's darkening wash (glass variant of
/// [`content_plate`]) — private so it stays unit-testable in both modes.
fn wash_content() -> Hsla {
    hsla(0.0, 0.0, 0.0, 0.25)
}
/// Rung washes share the hairline white (h 220, s .10, l .90 ≈ 229.5).
/// The β values were solved for the old `#212121` ground's steps; over
/// the darker comet-parity ground they give slightly tighter steps at
/// black, which is fine — the ordering (not the absolute step) is the
/// invariant, and the sweep test proves it on every backdrop.
fn wash_raised() -> Hsla {
    hsla(220.0 / 360.0, 0.10, 0.90, 0.044)
}
fn wash_panel() -> Hsla {
    hsla(220.0 / 360.0, 0.10, 0.90, 0.067)
}
fn wash_modal() -> Hsla {
    hsla(220.0 / 360.0, 0.10, 0.90, 0.097)
}
/// Sunken = black wash: 26.4·(1−0.205) = 21 = opaque inset over black.
fn wash_inset() -> Hsla {
    hsla(0.0, 0.0, 0.0, 0.205)
}
/// Attention plates: single-color equivalents of (L2 wash, then state
/// wash) stacked, so over a black backdrop they equal the opaque plates.
fn wash_warning_plate() -> Hsla {
    rgba(0xe3cfb11d).into()
}
fn wash_warning_plate_strong() -> Hsla {
    rgba(0xe1c4932e).into()
}
fn wash_danger_plate() -> Hsla {
    rgba(0xe3adab1f).into()
}

// ---------------------------------------------------------------------------
// Surfaces (glass by default — the 2026-08-17 ruling; `EAVE_VIBRANCY=0`
// is the opaque fallback)

/// Window ground; the transcript sits directly on it.
pub fn canvas() -> Hsla {
    if vibrancy() {
        glass_canvas()
    } else {
        rgb(0x1a1a1a).into()
    }
}
/// Raised blocks: user directive blocks, pills, cards.
pub fn raised() -> Hsla {
    if vibrancy() {
        wash_raised()
    } else {
        rgb(0x232323).into()
    }
}
/// L2 chrome plate: in-flow cards above the log (rail, composer, panels).
pub fn panel() -> Hsla {
    if vibrancy() {
        wash_panel()
    } else {
        rgb(0x282828).into()
    }
}
/// The composer card plate — L2 chrome, same rung as [`panel`].
pub fn composer() -> Hsla {
    panel()
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
// These tints are EAVE_VIBRANCY-independent: in-app blur samples the
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
    if vibrancy() {
        wash_content()
    } else {
        canvas()
    }
}

/// Sunken panels: code blocks.
pub fn inset() -> Hsla {
    if vibrancy() {
        wash_inset()
    } else {
        rgb(0x151515).into()
    }
}

/// Hover/selection wash composited over any surface (waku's idiom: no
/// per-surface hover tokens, just a white wash).
pub fn overlay() -> Hsla {
    hsla(220.0 / 360.0, 0.10, 0.90, 0.05)
}
pub fn overlay_strong() -> Hsla {
    hsla(220.0 / 360.0, 0.10, 0.90, 0.09)
}
/// One step above [`overlay_strong`], for an element whose RESTING fill
/// is already `overlay_strong` — the unarmed send button. A hover wash
/// that composites over a surface is the app's idiom, but an element
/// with its own `bg` replaces rather than composites, so the few filled
/// controls need their hovered fill named outright (Slice M).
pub fn overlay_strongest() -> Hsla {
    hsla(220.0 / 360.0, 0.10, 0.90, 0.14)
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
// `canvas`/`raised`/`composer`/`panel`/`inset` below are the semantic
// names for these rungs; `border`/`border_strong`/`edge_modal` are the
// hairline ladder.

/// L3 surface — reserved; no modal exists yet.
pub fn surface_modal() -> Hsla {
    if vibrancy() {
        wash_modal()
    } else {
        rgb(0x2e2e2e).into()
    }
}
/// L3 hairline — reserved with it.
pub fn edge_modal() -> Hsla {
    hsla(220.0 / 360.0, 0.10, 0.90, 0.20)
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
    hsla(220.0 / 360.0, 0.10, 0.90, 0.07)
}
pub fn border_strong() -> Hsla {
    hsla(220.0 / 360.0, 0.10, 0.90, 0.14)
}

// ---------------------------------------------------------------------------
// Text tiers

pub fn text() -> Hsla {
    rgb(0xe2e2e2).into()
}
pub fn text_secondary() -> Hsla {
    rgb(0xa3a3a3).into()
}
pub fn text_tertiary() -> Hsla {
    rgb(0x7d7d7d).into()
}
/// Placeholders, disabled, structural hints.
pub fn text_ghost() -> Hsla {
    rgb(0x575757).into()
}

/// Primary-action fill (the send button when armed) and the text on it.
pub fn inverse() -> Hsla {
    rgb(0xe7e9ec).into()
}
pub fn on_inverse() -> Hsla {
    rgb(0x17181c).into()
}
/// The armed send button under the pointer: the same fill, lifted. The
/// only direction available — [`inverse`] is already the brightest fill
/// in the app, so hover goes to white rather than adding a wash that
/// would darken it (Slice M).
pub fn inverse_hover() -> Hsla {
    rgb(0xf4f6f9).into()
}

// ---------------------------------------------------------------------------
// State colors — the only saturated pigment in the app

/// Live activity right now (running tool dot, streaming cursor).
pub fn live() -> Hsla {
    rgb(0x7c86ff).into()
}
pub fn success() -> Hsla {
    rgb(0x62c987).into()
}
pub fn warning() -> Hsla {
    rgb(0xe0b36a).into()
}
pub fn danger() -> Hsla {
    rgb(0xe2726a).into()
}

/// Panel tints for the attention surfaces (ring = border, wash = fill).
pub fn warning_ring() -> Hsla {
    warning().opacity(0.20)
}
pub fn warning_wash() -> Hsla {
    warning().opacity(0.05)
}
pub fn warning_wash_strong() -> Hsla {
    warning().opacity(0.12)
}
pub fn danger_ring() -> Hsla {
    danger().opacity(0.22)
}
pub fn danger_wash() -> Hsla {
    danger().opacity(0.06)
}
pub fn danger_wash_strong() -> Hsla {
    danger().opacity(0.14)
}
/// The kill button under the pointer. Its resting fill is already
/// `danger_wash_strong`, so the hover step is the same pigment again
/// rather than the neutral wash (Slice M).
pub fn danger_wash_hover() -> Hsla {
    danger().opacity(0.24)
}

/// Attention/connection panels sit at L2: their tint is pre-composited
/// over [`panel`] (the wash alpha blended into `#282828`) so the panel
/// is never darker than the chrome beneath it — a state surface must
/// read raised, not sunken. Keep in sync with `panel()` and the wash
/// alphas above.
pub fn warning_plate() -> Hsla {
    if vibrancy() {
        wash_warning_plate()
    } else {
        rgb(0x312f2b).into()
    }
}
pub fn warning_plate_strong() -> Hsla {
    if vibrancy() {
        wash_warning_plate_strong()
    } else {
        rgb(0x3e3930).into()
    }
}
pub fn danger_plate() -> Hsla {
    if vibrancy() {
        wash_danger_plate()
    } else {
        rgb(0x332c2c).into()
    }
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
    if vibrancy() {
        success().opacity(DIFF_WASH_ALPHA)
    } else {
        rgb(0x212c25).into()
    }
}
pub fn diff_del_bg() -> Hsla {
    if vibrancy() {
        danger().opacity(DIFF_WASH_ALPHA)
    } else {
        rgb(0x2e2322).into()
    }
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

/// Prose body (assistant text): waku `BODY`.
pub const BODY_SIZE: f32 = 13.5;
pub const BODY_LINE: f32 = 21.0;
/// User directive blocks: waku `USER_MESSAGE`.
pub const USER_SIZE: f32 = 14.0;
pub const USER_LINE: f32 = 20.0;
/// Fenced/inline code.
pub const CODE_SIZE: f32 = 11.5;
pub const CODE_LINE: f32 = 17.5;
/// Chip/label rank: tool rows, affordances, buttons.
pub const LABEL_SIZE: f32 = 12.5;
pub const LABEL_LINE: f32 = 16.0;
/// Panel detail rank: attention evidence, notice bodies.
pub const DETAIL_SIZE: f32 = 12.0;
pub const DETAIL_LINE: f32 = 16.0;
/// Footer / timestamps / markers.
pub const SMALL_SIZE: f32 = 11.0;
pub const SMALL_LINE: f32 = 15.0;
/// Code-block language strip, structural micro-labels.
pub const MICRO_SIZE: f32 = 10.0;
pub const MICRO_LINE: f32 = 14.0;
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
/// The send button glyph.
pub const SEND_GLYPH_SIZE: f32 = 13.0;
/// Composer input line (slightly airier than body so the caret row
/// doesn't feel cramped inside the card).
pub const COMPOSER_LINE: f32 = 22.0;
pub const COMPOSER_MIN_H: f32 = 24.0;

/// Weight roles — the only weights the shell uses. Prose is NORMAL;
/// labels/names are MEDIUM; STRONG is reserved for escalated notices
/// and the armed send glyph. (Markdown `**bold**` inside prose keeps
/// semantic BOLD — that's content, not chrome.)
pub const WEIGHT_BODY: FontWeight = FontWeight::NORMAL;
pub const WEIGHT_LABEL: FontWeight = FontWeight::MEDIUM;
pub const WEIGHT_STRONG: FontWeight = FontWeight::SEMIBOLD;

/// Heading ladder relative to body (waku's scale): size, line, weight.
pub fn heading(level: u8) -> (Pixels, Pixels, FontWeight) {
    match level {
        1 => (px(19.5), px(28.0), FontWeight::BOLD),
        2 => (px(17.5), px(25.0), FontWeight::BOLD),
        3 => (px(15.5), px(22.0), FontWeight::SEMIBOLD),
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
/// Provider identity chip: self-drawn monochrome initials on a bordered
/// rounded square. Deliberately never a third-party mark — one uniform,
/// self-drawn set for every provider (Slice A directive).
pub const PROVIDER_CHIP: f32 = 18.0;
pub const RADIUS_CHIP: f32 = 5.0;
/// The rail collapse toggle's self-drawn sidebar pictogram (titlebar).
pub const RAIL_TOGGLE_W: f32 = 16.0;
pub const RAIL_TOGGLE_H: f32 = 12.0;
pub const RADIUS_GLYPH: f32 = 3.0;

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
// and this is its curve. There is no per-frame drive anywhere in the app
// and nothing here schedules one.

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

    /// The glass ground is comet-parity near-black (2026-08-17 ruling):
    /// over a black backdrop it lands at `#080808`, adding almost no
    /// gray of its own, so the desktop's hue carries the surface instead
    /// of being washed toward gray (the retired `#212121` tint's failure
    /// — and with it the retired opaque-identity invariant: glass IS the
    /// look now, so it no longer needs to impersonate the opaque ladder
    /// on a black desktop). The content well darkens below the ground;
    /// attention plates stay above it.
    #[test]
    fn glass_ground_is_near_black_and_content_sinks() {
        let ground = over(glass_canvas(), (0.0, 0.0, 0.0));
        close(ground, rgb(0x080808).into());
        assert!(luma(over(wash_content(), ground)) < luma(ground));
        for b in (0..=255).step_by(15) {
            let v = b as f32 / 255.0;
            let g = over(glass_canvas(), (v, v, v));
            assert!(luma(over(wash_content(), g)) < luma(g), "backdrop {b}");
            assert!(luma(over(wash_warning_plate(), g)) > luma(g), "backdrop {b}");
            assert!(luma(over(wash_danger_plate(), g)) > luma(g), "backdrop {b}");
        }
    }

    /// The B2 diff washes obey the same law as every glass rung: on any
    /// backdrop they rise above the ground (state surfaces never read
    /// sunken) and keep their hues apart (add reads green, del reads
    /// red — the ground is achromatic, so channel order is hue). The
    /// opaque pre-blends must equal the same wash over the opaque
    /// canvas — the F.2 pre-composite discipline, checked not assumed.
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
        let opaque = (0x1a as f32 / 255.0, 0x1a as f32 / 255.0, 0x1a as f32 / 255.0);
        close(over(success().opacity(DIFF_WASH_ALPHA), opaque), rgb(0x212c25).into());
        close(over(danger().opacity(DIFF_WASH_ALPHA), opaque), rgb(0x2e2322).into());
    }

    /// The reason rungs are washes, not plates: over EVERY backdrop
    /// brightness the elevation ordering holds — steps compress toward
    /// white but never invert, and inset stays sunken. Independent
    /// plates (opaque or translucent) over a glass ground fail this.
    #[test]
    fn glass_ladder_never_inverts_on_any_backdrop() {
        for b in (0..=255).step_by(5) {
            let v = b as f32 / 255.0;
            let ground = over(glass_canvas(), (v, v, v));
            let g = luma(ground);
            let inset = luma(over(wash_inset(), ground));
            let raised = luma(over(wash_raised(), ground));
            let panel = luma(over(wash_panel(), ground));
            let modal = luma(over(wash_modal(), ground));
            assert!(
                inset < g && g < raised && raised < panel && panel < modal,
                "backdrop {b}: inset {inset} ground {g} raised {raised} panel {panel} modal {modal}"
            );
        }
    }
}
