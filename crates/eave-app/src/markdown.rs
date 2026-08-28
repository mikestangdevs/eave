//! Markdown block model for the transcript display tree.
//!
//! Two jobs (spec 03 §Rendering):
//! - split canonical message text into **top-level blocks** — the unit of
//!   virtualized rows ("row per markdown top-level block");
//! - **mend** the live tail for display: synthetic closers for unclosed
//!   emphasis/backticks/links plus a setext guard, applied to the display
//!   tree only; canonical text untouched.
//!
//! This is a deliberately small dialect: fences, headings, lists, tables,
//! rules, paragraphs; inline bold/italic/code/links. It parses what the
//! agent actually emits (see `fixtures/real-claude-*.tap.jsonl`), not
//! CommonMark.

/// One top-level block of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Fenced code. `closed` is false while the fence is still streaming —
    /// the display closes it synthetically; the flip to true on settle is a
    /// remeasure, never a splice (spec 03).
    Fence {
        lang: String,
        code: String,
        closed: bool,
    },
    Heading {
        level: u8,
        text: String,
    },
    /// Bullet/numbered list run (contiguous list lines, nesting kept as
    /// indentation inside).
    List {
        text: String,
    },
    /// Pipe table run. Held as raw source; [`table_view`] turns it into
    /// aligned columns at render time, the way [`crate::ui::fence_view`]
    /// bounds a fence — a pure, capped view function over stable source.
    Table {
        text: String,
    },
    /// Blockquote run, `>` markers already stripped.
    Quote {
        text: String,
    },
    Rule,
    Paragraph {
        text: String,
    },
}

impl Block {
    /// Canonical text content used for fingerprinting.
    pub fn source(&self) -> &str {
        match self {
            Block::Fence { code, .. } => code,
            Block::Heading { text, .. }
            | Block::List { text }
            | Block::Table { text }
            | Block::Quote { text }
            | Block::Paragraph { text } => text,
            Block::Rule => "",
        }
    }
}

fn is_fence_open(line: &str) -> Option<&str> {
    let t = line.trim_start();
    t.strip_prefix("```").map(|rest| rest.trim())
}

fn is_list_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("+ ")
        || t.split_once('.')
            .map(|(n, rest)| {
                !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && rest.starts_with(' ')
            })
            .unwrap_or(false)
}

fn is_table_line(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.len() > 1
}

/// A thematic break: three or more of `-`, `*` or `_`, optionally spaced.
///
/// The spaced forms (`- - -`) and the underscore form were silently
/// missing until Slice AD: `___` rendered as a literal row of underscores
/// in the middle of a paragraph, which is what a rule looks like when
/// nobody parsed it.
fn is_rule_line(line: &str) -> bool {
    // No allocation: this runs on every line of every message inside the
    // display fold, which holds a 4 ms thread-CPU budget over 10k rows.
    let mut seen = 0usize;
    let mut kind: Option<char> = None;
    for c in line.trim().chars() {
        if c.is_whitespace() {
            continue;
        }
        if !matches!(c, '-' | '*' | '_') {
            return false;
        }
        match kind {
            None => kind = Some(c),
            Some(k) if k == c => {}
            Some(_) => return false,
        }
        seen += 1;
    }
    seen >= 3
}

/// A blockquote line. Agents open with `> ` constantly — a callout, a
/// quoted error, a restatement of the instruction — and until Slice AD
/// every one of them rendered its own `>` as literal text inside a
/// paragraph.
fn is_quote_line(line: &str) -> bool {
    let t = line.trim_start();
    t == ">" || t.starts_with("> ") || t.starts_with(">>")
}

/// Strip one `>` marker (and the single space after it, if present).
fn strip_quote(line: &str) -> &str {
    let t = line.trim_start();
    let rest = t.strip_prefix('>').unwrap_or(t);
    rest.strip_prefix(' ').unwrap_or(rest)
}

/// Split message text into top-level blocks. Total function: any text,
/// including a half-streamed tail, yields a block list; the last block of a
/// live message is the one still growing.
pub fn split_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut lines = text.lines().peekable();
    // `text.lines()` drops the info of a trailing newline; that only
    // affects whether the tail paragraph is "complete", which mending
    // handles — block identity is unaffected.
    let mut para: Vec<&str> = Vec::new();

    macro_rules! flush_para {
        () => {
            if !para.is_empty() {
                blocks.push(Block::Paragraph {
                    text: para.join("\n"),
                });
                para.clear();
            }
        };
    }

    while let Some(line) = lines.next() {
        if let Some(lang) = is_fence_open(line) {
            flush_para!();
            let mut code = String::new();
            let mut closed = false;
            for l in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    closed = true;
                    break;
                }
                code.push_str(l);
                code.push('\n');
            }
            blocks.push(Block::Fence {
                lang: lang.to_string(),
                code,
                closed,
            });
        } else if line.trim().is_empty() {
            flush_para!();
        } else if let Some(stripped) = line.trim_start().strip_prefix('#') {
            // #..###### heading
            let mut level = 1u8;
            let mut rest = stripped;
            while let Some(r) = rest.strip_prefix('#') {
                level += 1;
                rest = r;
            }
            if rest.starts_with(' ') && level <= 6 {
                flush_para!();
                blocks.push(Block::Heading {
                    level,
                    text: rest.trim().to_string(),
                });
            } else {
                para.push(line);
            }
        } else if is_rule_line(line) {
            // Setext guard (spec 03 §Rendering): a dash run immediately
            // after paragraph text would flip that paragraph into a
            // heading in full CommonMark; while streaming we can't know
            // whether it's a rule or a half-typed table/whatever. We never
            // reinterpret the preceding paragraph: a rule is a rule, the
            // paragraph stays a paragraph. Identity is stable on settle.
            flush_para!();
            blocks.push(Block::Rule);
        } else if is_quote_line(line) {
            flush_para!();
            let mut lines_in = vec![strip_quote(line)];
            while let Some(next) = lines.peek() {
                if is_quote_line(next) {
                    lines_in.push(strip_quote(lines.next().unwrap()));
                } else {
                    break;
                }
            }
            blocks.push(Block::Quote {
                text: lines_in.join("\n"),
            });
        } else if is_list_line(line) {
            flush_para!();
            let mut items = vec![line];
            while let Some(next) = lines.peek() {
                // continuation: list lines or indented continuations
                if is_list_line(next) || (next.starts_with("  ") && !next.trim().is_empty()) {
                    items.push(lines.next().unwrap());
                } else {
                    break;
                }
            }
            blocks.push(Block::List {
                text: items.join("\n"),
            });
        } else if is_table_line(line) {
            flush_para!();
            let mut rows = vec![line];
            while let Some(next) = lines.peek() {
                if is_table_line(next) {
                    rows.push(lines.next().unwrap());
                } else {
                    break;
                }
            }
            blocks.push(Block::Table {
                text: rows.join("\n"),
            });
        } else {
            para.push(line);
        }
    }
    flush_para!();
    blocks
}

// ---------------------------------------------------------------------------
// Inline runs + mending

/// Style bits for one inline run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    /// Drawn as a link (underlined). NOT the same question as "can this be
    /// clicked" — see [`InlineRun::href`].
    pub link: bool,
    pub strike: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineRun {
    pub text: String,
    pub style: InlineStyle,
    /// Where this run goes when clicked, once it has passed [`web_url`].
    ///
    /// Separate from `style.link` on purpose. A link whose target eave will
    /// not open has **no href and no link styling** — the underline is the
    /// app's promise that a click does something, and a promise it cannot
    /// keep is worse than plain text. The one case that is underlined
    /// without an href is the mended tail of a link still streaming in
    /// (`[docs](https://exa`), where the target is not a target yet.
    pub href: Option<String>,
}

/// The only schemes eave will hand to the system opener.
///
/// `open(1)` will happily launch a `file://`, an `x-apple-…`, or anything
/// else with a registered handler, and the transcript is text an agent
/// wrote — so the allowlist is the whole security story here, and it is an
/// allowlist rather than a denylist for the usual reason. Whitespace and
/// control characters are refused too: nothing legal in a URL, and the
/// shape of a target trying to be two arguments.
pub fn web_url(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    // A scheme and nothing else is not a destination.
    let rest = &t[lower.find("//").expect("checked above") + 2..];
    if rest.is_empty() {
        return None;
    }
    Some(t.to_string())
}

/// Where a bare URL stops.
///
/// Trailing sentence punctuation belongs to the sentence, not the URL —
/// "see https://eave.sh." must not open `https://eave.sh.`. Closing
/// brackets are only trimmed when they are unbalanced, so a Wikipedia
/// `..._(disambiguation)` keeps its tail.
fn trim_url_tail(url: &str) -> &str {
    let mut end = url.len();
    loop {
        let s = &url[..end];
        let Some(last) = s.chars().last() else { break };
        let drop = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' => true,
            ')' => s.matches(')').count() > s.matches('(').count(),
            ']' => s.matches(']').count() > s.matches('[').count(),
            '}' => s.matches('}').count() > s.matches('{').count(),
            _ => false,
        };
        if drop {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    &url[..end]
}

/// The `)` that closes a link destination opened at `open`.
///
/// Balanced, not first-match: `[x](https://en.wikipedia.org/wiki/A_(b))`
/// and `[x](javascript:alert(1))` both carry a nested pair, and stopping
/// at the first `)` cut the target short and spat the remainder into the
/// prose as a stray bracket.
fn close_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (n, c) in chars[open..].iter().enumerate() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(open + n),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Does a bare `http(s)://…` autolink start at `i`?
fn autolink_at(chars: &[char], i: usize) -> Option<usize> {
    // Never in the middle of a word: `xhttps://x` is not a link, and
    // neither is the second half of an already-consumed one.
    if i > 0 && chars[i - 1].is_alphanumeric() {
        return None;
    }
    let head: String = chars[i..chars.len().min(i + 8)]
        .iter()
        .collect::<String>()
        .to_ascii_lowercase();
    if !(head.starts_with("http://") || head.starts_with("https://")) {
        return None;
    }
    let end = chars[i..]
        .iter()
        .position(|c| c.is_whitespace() || *c == '<' || *c == '>' || *c == '`')
        .map(|n| i + n)
        .unwrap_or(chars.len());
    let raw: String = chars[i..end].iter().collect();
    let kept = trim_url_tail(&raw);
    (!kept.is_empty()).then(|| kept.chars().count())
}

/// Parse inline markdown into styled runs, **mending** unclosed markers:
/// an unclosed `**`, `*`, `` ` ``, `~~` or `[label](target` at end of input
/// is treated as closed at the end — synthetic closers in the display tree
/// only (spec 03 §Rendering). Marker characters are consumed, not shown.
pub fn inline_runs(text: &str) -> Vec<InlineRun> {
    let mut runs: Vec<InlineRun> = Vec::new();
    let mut cur = String::new();
    let mut style = InlineStyle::default();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    macro_rules! flush {
        () => {
            if !cur.is_empty() {
                runs.push(InlineRun {
                    text: std::mem::take(&mut cur),
                    style,
                    href: None,
                });
            }
        };
    }

    while i < chars.len() {
        let c = chars[i];
        if style.code {
            // Inside inline code only a backtick has meaning.
            if c == '`' {
                flush!();
                style.code = false;
            } else {
                cur.push(c);
            }
            i += 1;
            continue;
        }
        match c {
            '`' => {
                flush!();
                style.code = true;
                i += 1;
            }
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                flush!();
                style.bold = !style.bold;
                i += 2;
            }
            '~' if i + 1 < chars.len() && chars[i + 1] == '~' => {
                flush!();
                style.strike = !style.strike;
                i += 2;
            }
            '*' => {
                flush!();
                style.italic = !style.italic;
                i += 1;
            }
            'h' | 'H' if autolink_at(&chars, i).is_some() => {
                let n = autolink_at(&chars, i).expect("just checked");
                let raw: String = chars[i..i + n].iter().collect();
                flush!();
                let href = web_url(&raw);
                runs.push(InlineRun {
                    text: raw,
                    style: InlineStyle {
                        link: href.is_some(),
                        ..style
                    },
                    href,
                });
                i += n;
            }
            '[' => {
                // `[label](target)`. Indices are CHAR indices throughout:
                // the byte-offset version of this shipped in Stage D and
                // ate one character of the following text for every
                // multi-byte character in the label.
                let close = chars[i + 1..]
                    .iter()
                    .position(|c| *c == ']')
                    .map(|n| i + 1 + n);
                let (label, target, consumed, mended) = match close {
                    Some(le) if chars.get(le + 1) == Some(&'(') => {
                        let label: String = chars[i + 1..le].iter().collect();
                        let open = le + 2;
                        match close_paren(&chars, open) {
                            Some(ce) => (
                                label,
                                chars[open..ce].iter().collect::<String>(),
                                ce + 1 - i,
                                false,
                            ),
                            // `[label](half-url` — mend: consume all.
                            None => (
                                label,
                                chars[open..].iter().collect::<String>(),
                                chars.len() - i,
                                true,
                            ),
                        }
                    }
                    // Bare `[text]` — not a link, keep it literally.
                    Some(_) => (String::new(), String::new(), 0, false),
                    // `[half-label` at end — mend: the label is the rest.
                    None => (
                        chars[i + 1..].iter().collect::<String>(),
                        String::new(),
                        chars.len() - i,
                        true,
                    ),
                };
                if consumed == 0 {
                    cur.push(c);
                    i += 1;
                } else {
                    let href = if mended { None } else { web_url(&target) };
                    // A settled link eave will not open is not drawn as a
                    // link; a still-streaming one keeps its underline.
                    let looks_like_link = mended || href.is_some();
                    flush!();
                    runs.push(InlineRun {
                        text: label,
                        style: InlineStyle {
                            link: looks_like_link,
                            ..style
                        },
                        href,
                    });
                    i += consumed;
                }
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
    // Synthetic closers: whatever style is still open simply ends here —
    // the accumulated run keeps its style; canonical text untouched.
    flush!();
    runs.retain(|r| !r.text.is_empty());
    runs
}

// ---------------------------------------------------------------------------
// Table view — the block's source, turned into columns
//
// A table is ONE row in a virtualized list, so the same rule the fence
// lives by applies here and for the same reason: a row's height may not be
// a number the agent chose. `table_view` is the fence's `fence_view`
// pattern applied to a second construct — a pure, capped projection of
// stable block source, unit-testable without a window, with the caps
// passed in so this module stays free of the theme.

/// Column alignment, from the delimiter row's colons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// A pipe table, ready to lay out.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TableView {
    /// Header cells; empty when the block has no delimiter row yet.
    pub head: Vec<String>,
    pub align: Vec<Align>,
    pub rows: Vec<Vec<String>>,
    /// Body rows the cap dropped. Stated, never silently swallowed.
    pub more_rows: usize,
    /// Per-column flex weights (see [`table_view`]).
    pub weights: Vec<f32>,
}

impl TableView {
    pub fn cols(&self) -> usize {
        self.weights.len()
    }
}

/// Split one table line into cells. `\|` is an escaped pipe, not a border.
fn split_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cur.push('|');
            }
            '|' => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out
}

/// Is this the `|:---|---:|` row, and what does it say about alignment?
fn delimiter_row(cells: &[String]) -> Option<Vec<Align>> {
    if cells.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(cells.len());
    for cell in cells {
        let c = cell.trim();
        let left = c.starts_with(':');
        let right = c.ends_with(':') && c.len() > 1;
        let core = c.trim_matches(':');
        if core.len() < 3 || !core.chars().all(|ch| ch == '-') {
            return None;
        }
        out.push(match (left, right) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        });
    }
    Some(out)
}

/// Narrowest and widest column weight. The clamp is the whole layout
/// policy: widths are proportional to each column's longest cell, so a
/// `#` column stays narrow and a prose column gets the room — but the
/// ratio between the two is capped at **10:1**, so one long free-text
/// column can dominate a table without erasing its neighbours.
pub const COL_WEIGHT_MIN: f32 = 4.0;
pub const COL_WEIGHT_MAX: f32 = 40.0;

/// Project a table block into columns.
///
/// Total, and deliberately forgiving, because it runs on a **half-arrived**
/// table too: the delimiter row lands one burst after the header, so for a
/// frame or two there is no delimiter to read. A block with no delimiter
/// row is not an error and must never fall back to raw pipes — one line
/// becomes a header, several become body rows, and the real table replaces
/// them the moment the `|---|` arrives.
///
/// `max_rows` caps the BODY (the header always draws); `cell_chars` caps
/// one cell's text. Both are the fence's bargain: the cap costs reading and
/// states its own remainder.
pub fn table_view(text: &str, max_rows: usize, cell_chars: usize) -> TableView {
    let raw: Vec<Vec<String>> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(split_cells)
        .collect();
    if raw.is_empty() {
        return TableView::default();
    }

    let (head, align, body): (Vec<String>, Vec<Align>, &[Vec<String>]) =
        match raw.get(1).and_then(|r| delimiter_row(r)) {
            Some(align) => (raw[0].clone(), align, &raw[2..]),
            // No delimiter yet. One line is a header on its own; more than one
            // is a body with no header, which is what a table missing its rule
            // actually is.
            None if raw.len() == 1 => (raw[0].clone(), Vec::new(), &[]),
            None => (Vec::new(), Vec::new(), &raw[..]),
        };

    let cols = raw.iter().map(|r| r.len()).max().unwrap_or(0);
    let clip = |s: &String| -> String {
        if s.chars().count() <= cell_chars {
            s.clone()
        } else {
            let mut out: String = s.chars().take(cell_chars).collect();
            out.push('…');
            out
        }
    };
    let pad = |r: &Vec<String>| -> Vec<String> {
        let mut v: Vec<String> = r.iter().map(clip).collect();
        v.resize(cols, String::new());
        v
    };

    let head = if head.is_empty() {
        Vec::new()
    } else {
        pad(&head)
    };
    let mut align = align;
    align.resize(cols, Align::Left);
    let more_rows = body.len().saturating_sub(max_rows);
    let rows: Vec<Vec<String>> = body.iter().take(max_rows).map(pad).collect();

    let weights = (0..cols)
        .map(|c| {
            let widest = head
                .get(c)
                .into_iter()
                .chain(rows.iter().filter_map(|r| r.get(c)))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0) as f32;
            widest.clamp(COL_WEIGHT_MIN, COL_WEIGHT_MAX)
        })
        .collect();

    TableView {
        head,
        align,
        rows,
        more_rows,
        weights,
    }
}

// ---------------------------------------------------------------------------
// List view

/// What draws in an item's marker gutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    Bullet,
    Number(u64),
    /// `- [ ]` / `- [x]`. Agents write plans as task lists and eave drew
    /// the brackets.
    Task(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    /// Nesting level, 0-based, derived from an indent STACK rather than
    /// from dividing the indent — two-space and four-space conventions
    /// both nest by one, which is what the writer meant either way.
    pub depth: usize,
    pub marker: Marker,
    pub text: String,
}

/// How deep the gutter is willing to indent. Past this the items still
/// draw, at the last indent — a runaway nesting must not push text off
/// the right edge of the column.
pub const LIST_MAX_DEPTH: usize = 5;

fn indent_of(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// Split a list line into (marker, rest), or `None` if it is not one.
fn parse_marker(t: &str) -> Option<(Marker, &str)> {
    for lead in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(lead) {
            for (open, done) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
                if let Some(after) = rest.strip_prefix(open) {
                    return Some((Marker::Task(done), after));
                }
            }
            // A checkbox with nothing after it yet, mid-stream.
            for (open, done) in [("[ ]", false), ("[x]", true), ("[X]", true)] {
                if rest.trim_end() == open {
                    return Some((Marker::Task(done), ""));
                }
            }
            return Some((Marker::Bullet, rest));
        }
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let rest = &t[digits.len()..];
        for lead in [". ", ") "] {
            if let Some(after) = rest.strip_prefix(lead) {
                return Some((Marker::Number(digits.parse().unwrap_or(1)), after));
            }
        }
    }
    None
}

/// Project a list block into items. Returns the items plus how many the
/// cap dropped, on the same terms as [`table_view`].
pub fn list_view(text: &str, max_items: usize) -> (Vec<ListItem>, usize) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut total = 0usize;
    for line in text.lines() {
        let indent = indent_of(line);
        let t = line.trim_start();
        match parse_marker(t) {
            Some((marker, rest)) => {
                while stack.last().is_some_and(|top| indent < *top) {
                    stack.pop();
                }
                if stack.last() != Some(&indent) {
                    stack.push(indent);
                }
                total += 1;
                if items.len() < max_items {
                    items.push(ListItem {
                        depth: (stack.len() - 1).min(LIST_MAX_DEPTH),
                        marker,
                        text: rest.trim_end().to_string(),
                    });
                }
            }
            // A lazy continuation line belongs to the item above it.
            None if !t.is_empty() => {
                if let Some(last) = items.last_mut() {
                    if !last.text.is_empty() {
                        last.text.push(' ');
                    }
                    last.text.push_str(t.trim_end());
                }
            }
            None => {}
        }
    }
    (items, total.saturating_sub(max_items))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_paragraphs_fences_lists_tables() {
        let text = "intro line\n\n```rust\nfn main() {}\n```\n\n- a\n- b\n\n| x | y |\n|---|---|\n| 1 | 2 |\n\ntail";
        let blocks = split_blocks(text);
        assert_eq!(blocks.len(), 5);
        assert!(matches!(&blocks[0], Block::Paragraph { text } if text == "intro line"));
        assert!(
            matches!(&blocks[1], Block::Fence { lang, code, closed: true } if lang == "rust" && code == "fn main() {}\n")
        );
        assert!(matches!(&blocks[2], Block::List { text } if text == "- a\n- b"));
        assert!(matches!(&blocks[3], Block::Table { .. }));
        assert!(matches!(&blocks[4], Block::Paragraph { text } if text == "tail"));
    }

    #[test]
    fn unclosed_fence_streams_as_open_block() {
        let blocks = split_blocks("before\n\n```rust\nlet x = 1;\nlet y");
        assert_eq!(blocks.len(), 2);
        assert!(
            matches!(&blocks[1], Block::Fence { closed: false, code, .. } if code == "let x = 1;\nlet y\n")
        );
        // Same prefix once the fence closes: block identity is stable.
        let closed = split_blocks("before\n\n```rust\nlet x = 1;\nlet y = 2;\n```\ndone");
        assert!(matches!(&closed[1], Block::Fence { closed: true, .. }));
    }

    #[test]
    fn mends_unclosed_inline_markers() {
        // Unclosed bold: style applies to the tail, no literal `**` shown.
        let runs = inline_runs("safe **loud tail");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "safe ");
        assert!(!runs[0].style.bold);
        assert_eq!(runs[1].text, "loud tail");
        assert!(runs[1].style.bold);

        // Unclosed inline code.
        let runs = inline_runs("call `half_ident");
        assert!(runs[1].style.code);
        assert_eq!(runs[1].text, "half_ident");

        // Unclosed link.
        let runs = inline_runs("see [the docs](https://exa");
        assert_eq!(runs[1].text, "the docs");
        assert!(runs[1].style.link);
    }

    #[test]
    fn closed_inline_round_trips() {
        let runs = inline_runs("a **b** `c` *d* [e](https://x) f");
        let texts: Vec<&str> = runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["a ", "b", " ", "c", " ", "d", " ", "e", " f"]);
        assert!(
            runs[1].style.bold && runs[3].style.code && runs[5].style.italic && runs[7].style.link
        );
    }

    #[test]
    fn setext_guard_keeps_paragraph_identity() {
        // Streaming pauses right after a dash run: the paragraph must stay
        // a paragraph and the dashes a rule — identical block identities
        // before and after more text arrives (no splice on settle).
        let mid = split_blocks("some text\n---");
        let done = split_blocks("some text\n---\nmore text");
        assert!(matches!(&mid[0], Block::Paragraph { .. }));
        assert!(matches!(&mid[1], Block::Rule));
        assert!(matches!(&done[0], Block::Paragraph { .. }));
        assert!(matches!(&done[1], Block::Rule));
        assert!(matches!(&done[2], Block::Paragraph { .. }));
    }

    // -----------------------------------------------------------------
    // Slice AD — what this file used to parse and throw away

    #[test]
    fn a_table_becomes_columns_with_its_stated_alignment() {
        let src = "| name | count | note |\n|:---|---:|:---:|\n| alpha | 12 | ok |\n| beta | 3 | also ok |";
        let t = table_view(src, 60, 200);
        assert_eq!(t.head, vec!["name", "count", "note"]);
        assert_eq!(t.align, vec![Align::Left, Align::Right, Align::Center]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[1], vec!["beta", "3", "also ok"]);
        assert_eq!(t.more_rows, 0);
        // Widths follow the longest cell, so `count` stays narrower than
        // `note` — floored at COL_WEIGHT_MIN so a `#` column never
        // collapses to nothing.
        assert_eq!(t.weights, vec![5.0, 5.0, 7.0]);

        // An escaped pipe is content, not a border.
        let esc = table_view("| a | b |\n|---|---|\n| x \\| y | z |", 60, 200);
        assert_eq!(esc.rows[0], vec!["x | y", "z"]);

        // A ragged row is padded, never dropped and never shifted left.
        let ragged = table_view("| a | b | c |\n|---|---|---|\n| only |", 60, 200);
        assert_eq!(ragged.rows[0], vec!["only", "", ""]);
    }

    #[test]
    fn a_half_arrived_table_never_shows_its_pipes() {
        // The delimiter row lands one burst after the header. For those
        // frames there is no `|---|` to read, and the fallback must still
        // be columns — the raw-pipe render is the thing this slice exists
        // to delete, so it may not come back as a streaming state.
        let head_only = table_view("| name | count |", 60, 200);
        assert_eq!(head_only.head, vec!["name", "count"]);
        assert_eq!(head_only.cols(), 2);
        assert!(head_only.rows.is_empty());

        // Two lines and no delimiter: a body with no header, not garbage.
        let no_rule = table_view("| a | b |\n| c | d |", 60, 200);
        assert!(no_rule.head.is_empty());
        assert_eq!(no_rule.rows, vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    #[test]
    fn a_table_is_bounded_in_both_directions() {
        // A table is ONE row of a virtualized list, so neither its row
        // count nor any single cell may be a number the agent chose.
        let mut src = String::from("| n |\n|---|\n");
        for i in 0..500 {
            src.push_str(&format!("| {i} |\n"));
        }
        let t = table_view(&src, 60, 200);
        assert_eq!(t.rows.len(), 60, "the cap draws exactly its own count");
        assert_eq!(t.more_rows, 440, "and states every row it is holding");

        let long = format!("| {} |\n|---|\n| x |", "z".repeat(4000));
        let t = table_view(&long, 60, 200);
        assert_eq!(t.head[0].chars().count(), 201, "200 kept plus the ellipsis");
        assert!(t.head[0].ends_with('…'), "a clipped cell says so");
        // ...and the weight clamp keeps one runaway column from erasing
        // its neighbours: 10:1 is the widest ratio a table can reach.
        let wide = table_view(
            &format!("| # | {} |\n|---|---|\n| 1 | y |", "z".repeat(300)),
            60,
            200,
        );
        assert_eq!(wide.weights, vec![4.0, 40.0]);
    }

    #[test]
    fn nesting_comes_from_the_indent_stack_not_from_dividing_it() {
        // Two-space and four-space conventions both mean "one level in",
        // which is why the depth comes from a stack of indent columns
        // rather than from `indent / 2`.
        let two = "- top\n  - under\n    - deeper\n- back";
        let (items, more) = list_view(two, 120);
        assert_eq!(more, 0);
        assert_eq!(
            items.iter().map(|i| i.depth).collect::<Vec<_>>(),
            vec![0, 1, 2, 0]
        );
        let four = "- top\n    - under\n        - deeper\n- back";
        let (items4, _) = list_view(four, 120);
        assert_eq!(
            items4.iter().map(|i| i.depth).collect::<Vec<_>>(),
            vec![0, 1, 2, 0],
            "four-space nesting is the same nesting, not twice as deep"
        );
        // Markers survive the trip, and a lazy continuation joins the item
        // above it rather than becoming an item of its own.
        let lazy = "1. first line\n   continues here\n2. second";
        let (items, _) = list_view(lazy, 120);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].marker, Marker::Number(1));
        assert_eq!(items[0].text, "first line continues here");
        assert_eq!(items[1].marker, Marker::Number(2));
    }

    #[test]
    fn a_task_list_is_boxes_and_not_brackets() {
        let (items, _) = list_view(
            "- [ ] open one\n- [x] done one\n- [X] done two\n- plain",
            120,
        );
        assert_eq!(items[0].marker, Marker::Task(false));
        assert_eq!(items[0].text, "open one", "the brackets are consumed");
        assert_eq!(items[1].marker, Marker::Task(true));
        assert_eq!(items[2].marker, Marker::Task(true));
        assert_eq!(items[3].marker, Marker::Bullet);
        // A checkbox that has arrived without its label yet.
        let (mid, _) = list_view("- [ ]", 120);
        assert_eq!(mid[0].marker, Marker::Task(false));
        assert_eq!(mid[0].text, "");
        // The cap states its remainder like every other cap here.
        let many: String = (0..200).map(|i| format!("- item {i}\n")).collect();
        let (items, more) = list_view(&many, 120);
        assert_eq!(items.len(), 120);
        assert_eq!(more, 80);
    }

    #[test]
    fn only_a_web_link_is_ever_handed_to_the_opener() {
        for good in [
            "https://eave.sh",
            "http://localhost:8080/x?y=1#z",
            "HTTPS://EAVE.SH/Path",
        ] {
            assert_eq!(web_url(good).as_deref(), Some(good), "{good} is a web link");
        }
        // Everything else is refused rather than handed to `open(1)`,
        // which would launch whatever has a registered handler.
        for bad in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,<script>alert(1)</script>",
            "mailto:someone@example.com",
            "x-apple-systempreferences://",
            "//evil.example.com",
            "/Users/someone/secrets",
            "ftp://example.com/f",
            "https://",
            "",
            "   ",
            "https://exa mple.com",
            "https://exa\nmple.com",
        ] {
            assert_eq!(web_url(bad), None, "{bad:?} must be refused");
        }
        // Leading/trailing whitespace is trimmed, not treated as a reason
        // to open something odd.
        assert_eq!(
            web_url("  https://eave.sh  ").as_deref(),
            Some("https://eave.sh")
        );
    }

    #[test]
    fn a_link_carries_its_target_and_a_refused_one_carries_nothing() {
        let runs = inline_runs("read [the spec](https://eave.sh/spec) first");
        let link = runs.iter().find(|r| r.style.link).expect("a link run");
        assert_eq!(link.text, "the spec");
        assert_eq!(link.href.as_deref(), Some("https://eave.sh/spec"));

        // A settled link eave will not open is NOT drawn as a link: the
        // underline is the app's promise that a click does something.
        let bad = inline_runs("run [this](javascript:alert(1)) now");
        assert!(
            bad.iter().all(|r| !r.style.link && r.href.is_none()),
            "a refused target must not be underlined: {bad:?}"
        );
        assert_eq!(
            bad.iter().map(|r| r.text.as_str()).collect::<String>(),
            "run this now",
            "the label survives as plain text"
        );

        // The still-streaming tail keeps its underline and gets no href —
        // the target is not a target yet.
        let mid = inline_runs("see [the docs](https://exa");
        assert!(mid[1].style.link);
        assert_eq!(mid[1].href, None);
    }

    #[test]
    fn a_bare_url_links_itself_and_leaves_the_sentence_its_punctuation() {
        let runs = inline_runs("docs live at https://eave.sh/docs. Read them.");
        let link = runs.iter().find(|r| r.style.link).expect("a bare url");
        assert_eq!(link.text, "https://eave.sh/docs");
        assert_eq!(link.href.as_deref(), Some("https://eave.sh/docs"));
        assert_eq!(
            runs.iter().map(|r| r.text.as_str()).collect::<String>(),
            "docs live at https://eave.sh/docs. Read them.",
            "no character is lost or duplicated"
        );
        // Balanced brackets belong to the URL; an unbalanced one closes
        // the sentence around it.
        let wiki = inline_runs("(see https://en.wikipedia.org/wiki/A_(b))");
        let link = wiki.iter().find(|r| r.style.link).unwrap();
        assert_eq!(link.text, "https://en.wikipedia.org/wiki/A_(b)");
        // Not in the middle of a word, and not inside a code span.
        assert!(inline_runs("xhttps://eave.sh")
            .iter()
            .all(|r| !r.style.link));
        assert!(inline_runs("`https://eave.sh`")
            .iter()
            .all(|r| !r.style.link));
    }

    #[test]
    fn a_multibyte_label_does_not_eat_the_text_after_it() {
        // The link branch mixed BYTE offsets into a CHAR cursor: every
        // multi-byte character in a label ate one character of whatever
        // followed the link.
        let runs = inline_runs("[café](https://eave.sh) TAIL");
        assert_eq!(
            runs.iter().map(|r| r.text.as_str()).collect::<String>(),
            "café TAIL"
        );
        let runs = inline_runs("[日本語のラベル](https://eave.sh)!!!");
        assert_eq!(
            runs.iter().map(|r| r.text.as_str()).collect::<String>(),
            "日本語のラベル!!!"
        );
    }

    #[test]
    fn strikethrough_is_a_style_and_not_four_tildes() {
        let runs = inline_runs("this ~~was wrong~~ now right");
        assert_eq!(
            runs.iter().map(|r| r.text.as_str()).collect::<String>(),
            "this was wrong now right",
            "the markers are consumed, not shown"
        );
        assert!(runs.iter().any(|r| r.style.strike && r.text == "was wrong"));
        // Unclosed, mid-stream: mended like every other marker here.
        let mid = inline_runs("this ~~is still");
        assert!(mid.last().unwrap().style.strike);
    }

    #[test]
    fn quotes_and_every_shape_of_rule_are_blocks_of_their_own() {
        let blocks = split_blocks("intro\n\n> a quoted line\n> and its second\n\nafter");
        assert_eq!(blocks.len(), 3);
        assert!(
            matches!(&blocks[1], Block::Quote { text } if text == "a quoted line\nand its second"),
            "the `>` markers are consumed: {:?}",
            blocks[1]
        );
        // Every thematic-break spelling, including the two that used to
        // render as literal punctuation in the middle of a paragraph.
        for rule in ["---", "***", "___", "- - -", "* * *", "_____"] {
            let b = split_blocks(&format!("before\n\n{rule}\n\nafter"));
            assert!(
                matches!(&b[1], Block::Rule),
                "{rule} should be a rule, got {:?}",
                b[1]
            );
        }
        // And a real list is still a list, not a rule.
        assert!(matches!(&split_blocks("- a\n- b")[0], Block::List { .. }));
    }
}
