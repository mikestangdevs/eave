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
    /// Pipe table run.
    Table {
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
            .map(|(n, rest)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && rest.starts_with(' '))
            .unwrap_or(false)
}

fn is_table_line(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.len() > 1
}

fn is_rule_line(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*'))
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
    pub link: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineRun {
    pub text: String,
    pub style: InlineStyle,
}

/// Parse inline markdown into styled runs, **mending** unclosed markers:
/// an unclosed `**`, `*`, `` ` `` or `[label](target` at end of input is
/// treated as closed at the end — synthetic closers in the display tree
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
            '*' => {
                flush!();
                style.italic = !style.italic;
                i += 1;
            }
            '[' => {
                // Try to parse [label](target). Unclosed at end-of-input →
                // mended: render the label styled as a link.
                let rest: String = chars[i + 1..].iter().collect();
                let label_end = rest.find(']');
                let (label, consumed) = match label_end {
                    Some(le) => {
                        let after = &rest[le + 1..];
                        if let Some(stripped) = after.strip_prefix('(') {
                            let close = stripped.find(')');
                            match close {
                                Some(ce) => (rest[..le].to_string(), 1 + le + 1 + 1 + ce + 1),
                                // `[label](half-url` — mend: consume all
                                None => (rest[..le].to_string(), chars.len() - i),
                            }
                        } else {
                            // bare [text] — not a link, keep literally
                            (String::new(), 0)
                        }
                    }
                    // `[half-label` at end — mend: label is the rest
                    None => (rest.clone(), chars.len() - i),
                };
                if consumed == 0 {
                    cur.push(c);
                    i += 1;
                } else {
                    flush!();
                    runs.push(InlineRun {
                        text: label,
                        style: InlineStyle {
                            link: true,
                            ..style
                        },
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
    runs
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
        assert!(runs[1].style.bold && runs[3].style.code && runs[5].style.italic && runs[7].style.link);
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
}
