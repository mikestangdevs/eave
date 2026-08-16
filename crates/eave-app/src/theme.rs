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

use gpui::{hsla, px, rgb, BoxShadow, FontWeight, Hsla, Pixels};

// ---------------------------------------------------------------------------
// Surfaces (opaque — locked decision, no vibrancy/blur)

/// Window ground; the transcript sits directly on it.
pub fn canvas() -> Hsla {
    rgb(0x1a1a1a).into()
}
/// Raised blocks: user directive blocks, pills, cards.
pub fn raised() -> Hsla {
    rgb(0x232323).into()
}
/// L2 chrome plate: floating pills, cards above the log.
pub fn panel() -> Hsla {
    rgb(0x282828).into()
}
/// The composer card plate — L2 chrome, same rung as [`panel`].
pub fn composer() -> Hsla {
    panel()
}
/// Sunken panels: code blocks.
pub fn inset() -> Hsla {
    rgb(0x151515).into()
}

/// Hover/selection wash composited over any surface (waku's idiom: no
/// per-surface hover tokens, just a white wash).
pub fn overlay() -> Hsla {
    hsla(220.0 / 360.0, 0.10, 0.90, 0.05)
}
pub fn overlay_strong() -> Hsla {
    hsla(220.0 / 360.0, 0.10, 0.90, 0.09)
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
    rgb(0x2e2e2e).into()
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

/// Attention/connection panels sit at L2: their tint is pre-composited
/// over [`panel`] (the wash alpha blended into `#282828`) so the panel
/// is never darker than the chrome beneath it — a state surface must
/// read raised, not sunken. Keep in sync with `panel()` and the wash
/// alphas above.
pub fn warning_plate() -> Hsla {
    rgb(0x312f2b).into()
}
pub fn warning_plate_strong() -> Hsla {
    rgb(0x3e3930).into()
}
pub fn danger_plate() -> Hsla {
    rgb(0x332c2c).into()
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
