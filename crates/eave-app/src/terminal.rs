//! The emulator half of a handover (Slice AG1 #16).
//!
//! The engine owns the pty and sends bytes; this parses them, keeps the
//! grid, and turns it into something the shell can paint. It also encodes
//! keystrokes back the other way.
//!
//! # Why the split is here
//!
//! An emulator is a state machine over a byte stream, and the state it
//! builds — the screen — is what gets painted. Running it in the engine
//! and shipping screens would mean re-inventing a diff protocol for a grid
//! that changes every frame; running it here costs one parse of bytes eave
//! already has in hand. It is also how tmux and every other terminal
//! multiplexer splits the same problem: the server holds the pty, the
//! client renders.
//!
//! # What this module deliberately does not do
//!
//! **Selection and scrollback.** `alacritty_terminal` has both, and both
//! need surface work this slice does not have: a selection that survives
//! rows unmounting is the same problem AG1 #15 left open for the
//! transcript, and there is no reason to solve it twice in different ways.
//! The grid keeps its scrollback; nothing reaches it yet.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor, Rgb};

/// A terminal's size in cells.
///
/// `total_lines` is `screen_lines` plus the scrollback alacritty is asked
/// to keep. The scrollback is real and the grid maintains it; this slice
/// paints only the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermSize {
    pub cols: usize,
    pub rows: usize,
}

/// How many lines of scrollback the grid keeps behind the screen.
const SCROLLBACK: usize = 5_000;

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows + SCROLLBACK
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Alacritty pushes events at its embedder — bell, title changes,
/// clipboard writes, PTY writes for terminal queries.
///
/// **eave answers none of them, and that is a decision with a cost.** A
/// TUI that asks the terminal a question (cursor position, colour support)
/// gets no reply, and the well-behaved ones time out and carry on. The one
/// that matters is `Event::PtyWrite`, which is how a query gets answered;
/// wiring it needs a channel back to the engine's pty and is the first
/// thing to add if a CLI is seen hanging on a query.
#[derive(Clone)]
struct Silent;

impl EventListener for Silent {
    fn send_event(&self, _: Event) {}
}

/// One run of cells sharing a style — what actually gets painted.
///
/// Runs rather than cells because a cell is a `div` and an 80×24 screen is
/// 1,920 of them, re-laid-out every frame. A real screen collapses to a
/// couple of dozen runs a line, and the transcript's own rule (row height
/// may depend only on identity and content) applies here too: a run is
/// content, a cell is a coordinate.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub text: String,
    pub fg: Rgb,
    /// `None` = the terminal's default background, which the surface paints
    /// as its own — a run that stated the theme's background as an explicit
    /// colour would stop following the theme.
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// One screen, ready to paint.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Screen {
    pub lines: Vec<Vec<Run>>,
    /// Cursor cell, `None` when the TUI has hidden it.
    pub cursor: Option<(usize, usize)>,
    pub cols: usize,
    pub rows: usize,
}

pub struct Emulator {
    term: Term<Silent>,
    parser: Processor,
    size: TermSize,
}

impl Emulator {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = TermSize {
            cols: (cols as usize).max(1),
            rows: (rows as usize).max(1),
        };
        Self {
            term: Term::new(Config::default(), &size, Silent),
            parser: Processor::new(),
            size,
        }
    }

    pub fn size(&self) -> TermSize {
        self.size
    }

    /// Feed bytes from the pty.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Resize the grid. Returns whether anything changed, so the caller can
    /// avoid telling the engine about a resize that was not one — every
    /// forwarded resize raises SIGWINCH and every TUI repaints on one.
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let next = TermSize {
            cols: (cols as usize).max(1),
            rows: (rows as usize).max(1),
        };
        if next == self.size {
            return false;
        }
        self.size = next;
        self.term.resize(next);
        true
    }

    /// Whether the TUI has asked for application cursor keys (DECCKM).
    ///
    /// This is not a detail. In application mode an arrow key is `ESC O A`
    /// and in normal mode it is `ESC [ A`, and a TUI in application mode
    /// that receives the normal form does not move the cursor — it inserts
    /// the letter. Sending one form always is how arrow keys "type letters"
    /// in a hand-rolled terminal.
    pub fn app_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// The current screen.
    pub fn screen(&self) -> Screen {
        let content = self.term.renderable_content();
        let cols = self.size.cols;
        let mut lines: Vec<Vec<Run>> = vec![Vec::new(); self.size.rows];
        let colors = content.colors;

        let cursor_point = content.cursor.point;
        // A TUI hides the cursor while it repaints (DECTCEM), and a block
        // left blinking over a redrawing screen is the most obvious way a
        // hand-rolled terminal looks wrong.
        let cursor_shown = self.term.mode().contains(TermMode::SHOW_CURSOR);

        for indexed in content.display_iter {
            let line = indexed.point.line.0 + content.display_offset as i32;
            if line < 0 || line as usize >= lines.len() {
                continue;
            }
            let cell = indexed.cell;
            // A wide character occupies two cells; the second is a spacer
            // holding no glyph. Painting it would double every CJK
            // character and every emoji.
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let hidden = cell.flags.contains(Flags::HIDDEN);
            let (mut fg, mut bg) = (cell.fg, cell.bg);
            // INVERSE is a swap, not a colour: it is how a TUI draws a
            // selected row, and resolving it after the swap means the theme
            // still supplies both sides.
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            let fg_rgb = resolve_fg(fg, colors);
            let bg_rgb = resolve(bg, colors, false);
            let run = Run {
                text: if hidden { ' ' } else { cell.c }.to_string(),
                fg: fg_rgb,
                bg: bg_rgb,
                bold: cell.flags.contains(Flags::BOLD),
                italic: cell.flags.contains(Flags::ITALIC),
                underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
            };
            push_run(&mut lines[line as usize], run);
        }

        let cursor = {
            let l = cursor_point.line.0 + content.display_offset as i32;
            if l >= 0 && (l as usize) < lines.len() && cursor_shown {
                Some((
                    l as usize,
                    cursor_point.column.0.min(cols.saturating_sub(1)),
                ))
            } else {
                None
            }
        };

        Screen {
            lines,
            cursor,
            cols,
            rows: self.size.rows,
        }
    }
}

/// Append one cell to a line, merging into the previous run when the style
/// matches. This is where 1,920 cells become a couple of dozen runs.
fn push_run(line: &mut Vec<Run>, cell: Run) {
    if let Some(last) = line.last_mut() {
        if last.fg == cell.fg
            && last.bg == cell.bg
            && last.bold == cell.bold
            && last.italic == cell.italic
            && last.underline == cell.underline
        {
            last.text.push_str(&cell.text);
            return;
        }
    }
    line.push(cell);
}

/// The xterm 256-colour palette, resolved arithmetically rather than from a
/// table: 0–15 are the named colours the terminal supplies, 16–231 are a
/// 6×6×6 cube, 232–255 are a 24-step grey ramp.
fn indexed_rgb(i: u8) -> Rgb {
    match i {
        0..=15 => ANSI_16[i as usize],
        16..=231 => {
            let i = i - 16;
            let step = |v: u8| if v == 0 { 0u8 } else { 55 + v * 40 };
            Rgb {
                r: step(i / 36),
                g: step((i % 36) / 6),
                b: step(i % 6),
            }
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            Rgb { r: v, g: v, b: v }
        }
    }
}

/// A conventional 16-colour palette.
///
/// eave supplies these rather than reading the theme, and the reason is the
/// same one `project_color` states from the other side: **a colour a
/// program CHOSE is rendered verbatim.** When a CLI says "red" it is
/// marking an error, and remapping red into eave's palette would quietly
/// restyle somebody else's product — the operator would be looking at a
/// screenshot of a thing that does not exist.
const ANSI_16: [Rgb; 16] = [
    Rgb {
        r: 0x1c,
        g: 0x1c,
        b: 0x1c,
    }, // black
    Rgb {
        r: 0xd7,
        g: 0x4e,
        b: 0x4e,
    }, // red
    Rgb {
        r: 0x5c,
        g: 0xb8,
        b: 0x5c,
    }, // green
    Rgb {
        r: 0xd0,
        g: 0xa2,
        b: 0x4e,
    }, // yellow
    Rgb {
        r: 0x5a,
        g: 0x8f,
        b: 0xd6,
    }, // blue
    Rgb {
        r: 0xa9,
        g: 0x74,
        b: 0xd0,
    }, // magenta
    Rgb {
        r: 0x4e,
        g: 0xb0,
        b: 0xb8,
    }, // cyan
    Rgb {
        r: 0xc8,
        g: 0xc8,
        b: 0xc8,
    }, // white
    Rgb {
        r: 0x5a,
        g: 0x5a,
        b: 0x5a,
    }, // bright black
    Rgb {
        r: 0xf0,
        g: 0x6a,
        b: 0x6a,
    }, // bright red
    Rgb {
        r: 0x79,
        g: 0xd6,
        b: 0x79,
    }, // bright green
    Rgb {
        r: 0xf0,
        g: 0xc0,
        b: 0x6a,
    }, // bright yellow
    Rgb {
        r: 0x79,
        g: 0xac,
        b: 0xf0,
    }, // bright blue
    Rgb {
        r: 0xc4,
        g: 0x92,
        b: 0xf0,
    }, // bright magenta
    Rgb {
        r: 0x6a,
        g: 0xcd,
        b: 0xd6,
    }, // bright cyan
    Rgb {
        r: 0xff,
        g: 0xff,
        b: 0xff,
    }, // bright white
];

/// The terminal's own default foreground, used when a cell names no colour.
pub const DEFAULT_FG: Rgb = Rgb {
    r: 0xd4,
    g: 0xd4,
    b: 0xd4,
};

/// The terminal's own default background.
///
/// **The panel paints this, not eave's surface colour**, and the reason is
/// the one `ANSI_16` states: a TUI's palette was chosen against a dark
/// terminal background, and re-hosting it on eave's own surface would
/// change contrast the CLI's authors picked. A terminal is a window onto
/// somebody else's program, not a region of eave's theme.
///
/// It is also what an INVERSE cell's foreground resolves to, which is the
/// other half of the same fact: reversed text is drawn in the background
/// colour, so the two must agree or a selected row draws invisibly.
pub const DEFAULT_BG: Rgb = Rgb {
    r: 0x14,
    g: 0x14,
    b: 0x16,
};

/// Resolve one cell colour.
///
/// `is_fg` decides what a *default* means: a default foreground is a real
/// colour to paint with, and a default background is **absence** — the
/// surface paints its own, so the terminal follows eave's theme everywhere
/// the TUI did not have an opinion.
fn resolve(
    c: AnsiColor,
    colors: &alacritty_terminal::term::color::Colors,
    is_fg: bool,
) -> Option<Rgb> {
    match c {
        AnsiColor::Spec(rgb) => Some(rgb),
        AnsiColor::Indexed(i) => Some(colors[i as usize].unwrap_or_else(|| indexed_rgb(i))),
        // A DEFAULT BACKGROUND IS ABSENCE, so the panel paints its own and
        // a run never states a colour it did not choose.
        AnsiColor::Named(NamedColor::Background) if !is_fg => None,
        AnsiColor::Named(NamedColor::Foreground) if is_fg => Some(DEFAULT_FG),
        // But a TUI that names the BACKGROUND for a foreground means it —
        // that is an INVERSE cell after the swap, and it must resolve to a
        // real colour or reversed text draws in the default foreground and
        // vanishes into its own highlight.
        AnsiColor::Named(NamedColor::Background) => Some(DEFAULT_BG),
        // Likewise the foreground named as a background.
        AnsiColor::Named(NamedColor::Foreground) => Some(DEFAULT_FG),
        AnsiColor::Named(n) => {
            let i = n as usize;
            // The terminal's own table wins when the TUI has set one (OSC
            // 4); the arithmetic palette is the fallback.
            Some(colors[i].unwrap_or_else(|| indexed_rgb((i & 0xff) as u8)))
        }
    }
}

/// Resolve a foreground, which always has one.
fn resolve_fg(c: AnsiColor, colors: &alacritty_terminal::term::color::Colors) -> Rgb {
    resolve(c, colors, true).unwrap_or(DEFAULT_FG)
}

// ---------------------------------------------------------------------------
// Keys

/// Encode a keystroke as the bytes a terminal would send.
///
/// `app_cursor` is [`Emulator::app_cursor`] — see its doc for why passing
/// the wrong one makes arrow keys type letters.
///
/// `None` means eave has nothing to send, which is not the same as an empty
/// slice: a bare modifier press produces no bytes and must not be forwarded
/// as one.
pub fn encode_key(ks: &gpui::Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    // Cmd is the one modifier a terminal does not have. macOS spends it on
    // the application, and eave keeps it: ⌘V, ⌘W and the handover toggle
    // itself all have to survive a focused terminal.
    if m.platform {
        return None;
    }
    let key = ks.key.as_str();

    let csi = |c: &str| -> Option<Vec<u8>> {
        // The cursor keys are the ones that change shape with DECCKM; the
        // others (Home/End as `ESC [ H`) do not.
        let prefix = if app_cursor { "\x1bO" } else { "\x1b[" };
        Some(format!("{prefix}{c}").into_bytes())
    };

    let bytes: Option<Vec<u8>> = match key {
        "up" => csi("A"),
        "down" => csi("B"),
        "right" => csi("C"),
        "left" => csi("D"),
        "home" => csi("H"),
        "end" => csi("F"),
        "pageup" => Some(b"\x1b[5~".to_vec()),
        "pagedown" => Some(b"\x1b[6~".to_vec()),
        "delete" => Some(b"\x1b[3~".to_vec()),
        "insert" => Some(b"\x1b[2~".to_vec()),
        // A pty in canonical mode translates \r; a TUI in raw mode expects
        // it. Sending \n instead is why hand-rolled terminals need Enter
        // pressed twice in some CLIs.
        "enter" => Some(b"\r".to_vec()),
        "tab" if m.shift => Some(b"\x1b[Z".to_vec()),
        "tab" => Some(b"\t".to_vec()),
        // DEL, not BS. Unix terminals send 0x7f for the backspace key and
        // the CLIs are configured for it; 0x08 leaves the character.
        "backspace" => Some(vec![0x7f]),
        "escape" => Some(vec![0x1b]),
        "space" if m.control => Some(vec![0x00]),
        "space" => Some(b" ".to_vec()),
        "f1" => Some(b"\x1bOP".to_vec()),
        "f2" => Some(b"\x1bOQ".to_vec()),
        "f3" => Some(b"\x1bOR".to_vec()),
        "f4" => Some(b"\x1bOS".to_vec()),
        "f5" => Some(b"\x1b[15~".to_vec()),
        "f6" => Some(b"\x1b[17~".to_vec()),
        "f7" => Some(b"\x1b[18~".to_vec()),
        "f8" => Some(b"\x1b[19~".to_vec()),
        "f9" => Some(b"\x1b[20~".to_vec()),
        "f10" => Some(b"\x1b[21~".to_vec()),
        "f11" => Some(b"\x1b[23~".to_vec()),
        "f12" => Some(b"\x1b[24~".to_vec()),
        _ => None,
    };
    if let Some(b) = bytes {
        // Alt on a special key is the ESC prefix, same as on a character.
        return Some(if m.alt && !b.starts_with(&[0x1b]) {
            let mut out = vec![0x1b];
            out.extend_from_slice(&b);
            out
        } else {
            b
        });
    }

    // Control characters: ctrl-a..ctrl-z are 0x01..0x1a, and the handful of
    // punctuation controls that CLIs actually use.
    if m.control {
        let c = key.chars().next()?;
        let ctl = match c {
            'a'..='z' => Some((c as u8) - b'a' + 1),
            '@' => Some(0x00),
            '[' => Some(0x1b),
            '\\' => Some(0x1c),
            ']' => Some(0x1d),
            '^' => Some(0x1e),
            '_' => Some(0x1f),
            '?' => Some(0x7f),
            _ => None,
        }?;
        return Some(vec![ctl]);
    }

    // An ordinary character. `key_char` is what the layout actually
    // produces — using `key` would send `s` for a Thai keyboard's ๆ and
    // would drop every accented character.
    let text = ks
        .key_char
        .clone()
        .or_else(|| (key.chars().count() == 1).then(|| key.to_string()))?;
    if text.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    if m.alt {
        out.push(0x1b);
    }
    out.extend_from_slice(text.as_bytes());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Keystroke;

    fn ks(s: &str) -> Keystroke {
        Keystroke::parse(s).expect("keystroke parses")
    }

    fn feed(bytes: &[u8]) -> Screen {
        let mut e = Emulator::new(20, 4);
        e.feed(bytes);
        e.screen()
    }

    fn text_of(screen: &Screen, line: usize) -> String {
        screen.lines[line]
            .iter()
            .map(|r| r.text.as_str())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn plain_text_lands_on_the_screen() {
        let s = feed(b"hello");
        assert_eq!(text_of(&s, 0), "hello");
    }

    /// The runs are the whole reason this is not a grid of divs: a line of
    /// one colour must be ONE run, not eighty.
    #[test]
    fn cells_of_one_style_collapse_into_a_single_run() {
        let s = feed(b"hello world");
        assert_eq!(s.lines[0].len(), 1, "{:?}", s.lines[0]);
        assert_eq!(s.lines[0][0].text.trim_end(), "hello world");
    }

    #[test]
    fn a_colour_change_starts_a_new_run() {
        let s = feed(b"ab\x1b[31mcd");
        let runs = &s.lines[0];
        assert!(runs.len() >= 2, "{runs:?}");
        assert_eq!(runs[0].text, "ab");
        assert_eq!(runs[1].text.trim_end(), "cd");
        assert_ne!(runs[0].fg, runs[1].fg);
        assert_eq!(runs[1].fg, ANSI_16[1], "SGR 31 is red");
    }

    /// `INVERSE` is how every TUI draws a selected row. Resolved as a swap
    /// rather than as a colour, so the theme still supplies both sides.
    #[test]
    fn inverse_swaps_the_two_colours_rather_than_naming_new_ones() {
        let normal = feed(b"x");
        let inverted = feed(b"\x1b[7mx");
        let n = &normal.lines[0][0];
        let i = &inverted.lines[0][0];
        assert_eq!(
            i.bg,
            Some(n.fg),
            "an inverted cell's background is the normal foreground"
        );
        assert_eq!(
            i.fg, DEFAULT_BG,
            "and reversed text draws in the background colour — resolving it \
             to the default FOREGROUND makes a selected row invisible"
        );
    }

    /// A default background is ABSENCE, so the surface paints its own and
    /// the terminal follows eave's theme where the TUI had no opinion.
    #[test]
    fn a_default_background_is_absent_rather_than_a_colour() {
        let s = feed(b"plain");
        assert_eq!(s.lines[0][0].bg, None);
    }

    #[test]
    fn the_256_colour_cube_and_grey_ramp_resolve_arithmetically() {
        assert_eq!(indexed_rgb(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(
            indexed_rgb(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        // The grey ramp starts at 8 and steps by 10.
        assert_eq!(indexed_rgb(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            indexed_rgb(255),
            Rgb {
                r: 238,
                g: 238,
                b: 238
            }
        );
        // And the low 16 come from the named palette, not the cube.
        assert_eq!(indexed_rgb(1), ANSI_16[1]);
    }

    /// **The bug that makes arrow keys type letters.** A TUI in application
    /// cursor mode expects `ESC O A`; sending `ESC [ A` inserts the letter.
    #[test]
    fn arrow_keys_follow_the_application_cursor_mode() {
        assert_eq!(encode_key(&ks("up"), false).unwrap(), b"\x1b[A");
        assert_eq!(encode_key(&ks("up"), true).unwrap(), b"\x1bOA");
        assert_eq!(encode_key(&ks("left"), false).unwrap(), b"\x1b[D");
    }

    /// And the mode is read from the stream, not assumed: DECCKM is `ESC [
    /// ? 1 h`, which is what every full-screen CLI sends on startup.
    #[test]
    fn the_application_cursor_mode_is_read_from_the_stream() {
        let mut e = Emulator::new(20, 4);
        assert!(!e.app_cursor());
        e.feed(b"\x1b[?1h");
        assert!(e.app_cursor(), "DECCKM was not picked up");
        e.feed(b"\x1b[?1l");
        assert!(!e.app_cursor());
    }

    #[test]
    fn control_characters_are_the_ones_a_cli_expects() {
        assert_eq!(encode_key(&ks("ctrl-c"), false).unwrap(), vec![0x03]);
        assert_eq!(encode_key(&ks("ctrl-d"), false).unwrap(), vec![0x04]);
        assert_eq!(encode_key(&ks("ctrl-a"), false).unwrap(), vec![0x01]);
        assert_eq!(encode_key(&ks("escape"), false).unwrap(), vec![0x1b]);
    }

    /// Unix terminals send DEL for the backspace key. 0x08 leaves the
    /// character on screen in every one of these CLIs.
    #[test]
    fn backspace_is_del_and_enter_is_carriage_return() {
        assert_eq!(encode_key(&ks("backspace"), false).unwrap(), vec![0x7f]);
        assert_eq!(encode_key(&ks("enter"), false).unwrap(), b"\r");
    }

    #[test]
    fn shift_tab_is_its_own_sequence() {
        assert_eq!(encode_key(&ks("tab"), false).unwrap(), b"\t");
        assert_eq!(encode_key(&ks("shift-tab"), false).unwrap(), b"\x1b[Z");
    }

    /// Alt is the ESC prefix — the convention every readline-based CLI uses
    /// for word-wise motion.
    #[test]
    fn alt_prefixes_with_escape() {
        assert_eq!(encode_key(&ks("alt-b"), false).unwrap(), b"\x1bb");
    }

    /// **⌘ never reaches the TUI.** macOS spends it on the application, and
    /// eave needs it: the handover toggle itself is a ⌘ chord, and a
    /// terminal that swallowed ⌘ would trap the operator inside itself.
    #[test]
    fn command_chords_are_left_for_the_application() {
        assert_eq!(encode_key(&ks("cmd-w"), false), None);
        assert_eq!(encode_key(&ks("cmd-shift-t"), false), None);
    }

    /// A resize that changes nothing must not be forwarded: every resize
    /// raises SIGWINCH and every TUI repaints on one.
    #[test]
    fn a_resize_to_the_same_size_is_not_a_resize() {
        let mut e = Emulator::new(80, 24);
        assert!(!e.resize(80, 24));
        assert!(e.resize(100, 24));
        assert_eq!(
            e.size(),
            TermSize {
                cols: 100,
                rows: 24
            }
        );
    }

    /// The screen is what the pty says it is, and a replay must land on it.
    /// This is the reattach path in miniature.
    #[test]
    fn a_replayed_stream_arrives_at_the_screen_it_describes() {
        let mut e = Emulator::new(20, 4);
        // Clear, home, then draw two lines — a repaint, as a TUI does it.
        e.feed(b"\x1b[2J\x1b[H");
        e.feed(b"first\r\n");
        e.feed(b"second");
        assert_eq!(text_of(&e.screen(), 0), "first");
        assert_eq!(text_of(&e.screen(), 1), "second");
    }

    /// A stream split at an arbitrary byte — which is what a pty read
    /// boundary is — must parse identically to the whole.
    #[test]
    fn a_stream_split_mid_sequence_parses_the_same() {
        let whole = b"\x1b[31mred\x1b[0m plain";
        let mut a = Emulator::new(20, 4);
        a.feed(whole);
        let mut b = Emulator::new(20, 4);
        for i in 0..whole.len() {
            b.feed(&whole[i..i + 1]);
        }
        assert_eq!(a.screen(), b.screen());
    }
}
