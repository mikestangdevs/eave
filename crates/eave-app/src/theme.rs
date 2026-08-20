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


use gpui::{hsla, px, rgb, rgba, BoxShadow, FontWeight, Hsla, Pixels};

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
    rgba(0x0a0a0acc).into()
}
/// The content well's darkening wash, behind [`content_plate`].
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
// Surfaces — all washes over the glass ground (see above)

/// Window ground; the transcript sits directly on it.
pub fn canvas() -> Hsla {
    glass_canvas()
}
/// Raised blocks: user directive blocks, pills, cards.
pub fn raised() -> Hsla {
    wash_raised()
}
/// L2 chrome plate: in-flow cards above the log (rail, composer, panels).
pub fn panel() -> Hsla {
    wash_panel()
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
pub fn inset() -> Hsla {
    wash_inset()
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
//
// **Those hexes are REFERENCE VALUES, not a code path** (2026-08-19). eave
// shipped an opaque build alongside the glass one until then, and the
// washes were tuned so that each rung composites to its hex over the glass
// ground — that is where these numbers come from and why each wash has the
// alpha it has. The opaque build is gone by ruling: glass is the only look,
// `EAVE_VIBRANCY` no longer exists, and there is no fallback. The hexes stay
// because deleting them would delete the derivation of every alpha below.

/// L3 surface — reserved; no modal exists yet.
pub fn surface_modal() -> Hsla {
    wash_modal()
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
    wash_warning_plate()
}
pub fn warning_plate_strong() -> Hsla {
    wash_warning_plate_strong()
}
pub fn danger_plate() -> Hsla {
    wash_danger_plate()
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
/// The launcher's headline. The largest type in the app, and the only
/// place eave says its own name.
pub const LAUNCHER_TITLE_SIZE: f32 = 20.0;
pub const LAUNCHER_TITLE_LINE: f32 = 26.0;

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

/// L3 glass: the popover surface. One luminance step above
/// [`chrome_glass`]'s `#141414` and below its hovered `#232323`, so a
/// hovered menu row — which composites [`overlay`] ON TOP of this — still
/// resolves as lighter than the surface carrying it.
pub fn menu_glass() -> Hsla {
    let c: Hsla = rgb(0x1e1e1e).into();
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
/// A row's height. Matches neither the send button (26 is coincidence)
/// nor a transcript line: it is `LABEL_LINE` (16) plus `SPACE_1` above
/// and below, so the wash clears the glyphs by one grid unit.
pub const MENU_ROW_H: f32 = LABEL_LINE + SPACE_1 * 2.0;
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
pub const DISABLED_OPACITY: f32 = 0.35;

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
