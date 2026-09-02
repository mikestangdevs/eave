//! Slice TN1 — the composer's two tuning chips, as data.
//!
//! The elements live in `ui.rs` (they are a projection of the selected
//! session, or of the launcher's choice-in-progress). This file is what
//! can be named without a `Shell`: the person-facing word for a CLI's
//! level, how full the gauge draws for it, and the rows the effort menu
//! offers.
//!
//! # The shape
//!
//! Two chips after the model chip, drawn only for what the provider can
//! actually do (`eave_core::provider::tuning_support`) — a control that
//! cannot do anything is a lie, so Hermes gets neither and Grok gets one:
//!
//! - **effort** — a four-bar gauge that fills with the level, then the
//!   word. Click opens a menu of the CLI's own levels with *Default* at
//!   the top; the current one wears the check. `Default` draws the gauge
//!   hollow: eave does not know which level the provider's default is
//!   and will not draw a guess.
//! - **speed** — a bolt. Click toggles. Lit in the accent with the word
//!   `Fast` when on; a quiet bolt alone when off.
//!
//! The levels are the CLI's words (`xhigh`), shown as a person reads
//! them (`Extra high`); [`level_label`] is the ONE table and the menu
//! row's detail line keeps the CLI's own spelling beside it so the flag
//! that will be passed is never hidden.

use eave_core::tuning::Tuning;

/// Bars in the gauge. Four reads at 12 px; five levels still map onto
/// it without two adjacent levels drawing the same.
pub const GAUGE_BARS: usize = 4;

/// The person-facing word for a CLI's own level word. Unknown words are
/// shown as they are — title-cased, never renamed.
pub fn level_label(level: &str) -> String {
    match level {
        "off" => "Off".into(),
        "none" => "None".into(),
        "minimal" => "Minimal".into(),
        "low" => "Low".into(),
        "medium" => "Medium".into(),
        "high" => "High".into(),
        "xhigh" => "Extra high".into(),
        "max" => "Max".into(),
        "ultra" => "Ultra".into(),
        other => {
            let mut c = other.chars();
            match c.next() {
                Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}

/// What the effort chip says. `None` = the provider's own default: the
/// chip names the KNOB (`Effort`, beside a hollow gauge) rather than a
/// level eave made up — and not the word "Default", which the model chip
/// beside it already wears when a CLI lists `default` as a model (Claude
/// does), and two Defaults in a row read as one fact twice.
pub fn effort_chip_label(effort: Option<&str>) -> String {
    match effort {
        Some(l) => level_label(l),
        None => "Effort".into(),
    }
}

/// How many of [`GAUGE_BARS`] are lit for `level` among `levels`
/// (lowest first). With four or fewer levels each one is a bar. With
/// more, they spread as `ceil((ix+1) · BARS / n)` clamped so that ONLY
/// the top level owns the full gauge — five levels read 1,2,3,3,4 and
/// never draw two adjacent levels as the same picture at the top, which
/// is where the difference matters. An unknown or absent level lights
/// nothing: a hollow gauge is the default's picture, deliberately.
pub fn gauge_fill(levels: &[&str], level: Option<&str>) -> usize {
    let Some(level) = level else {
        return 0;
    };
    let n = levels.len();
    let Some(ix) = levels.iter().position(|l| *l == level) else {
        return 0;
    };
    if n == 0 {
        return 0;
    }
    if ix + 1 == n {
        return GAUGE_BARS;
    }
    if n <= GAUGE_BARS {
        return ix + 1;
    }
    // More levels than bars: spread, but keep the full gauge for the top
    // level only.
    let spread = ((ix + 1) * GAUGE_BARS).div_ceil(n);
    spread.clamp(1, GAUGE_BARS - 1)
}

/// One row of the effort menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortRow {
    /// `None` is the *Default* row.
    pub level: Option<&'static str>,
    pub label: String,
    /// The CLI's own word, shown beside the label so the flag is never
    /// hidden; empty for the Default row.
    pub detail: String,
    pub current: bool,
}

/// The rows the effort menu offers for a provider: Default first, then
/// the CLI's levels lowest first. Exactly one row is `current`.
pub fn effort_rows(levels: &[&'static str], current: Option<&str>) -> Vec<EffortRow> {
    let mut rows = vec![EffortRow {
        level: None,
        label: "Default".into(),
        detail: "the CLI's own".into(),
        current: current.is_none(),
    }];
    for l in levels {
        rows.push(EffortRow {
            level: Some(l),
            label: level_label(l),
            detail: (*l).to_string(),
            current: current == Some(*l),
        });
    }
    rows
}

/// The tuning the composer shows for a session: what the engine reports.
pub fn of_session(effort: Option<&str>, fast: bool) -> Tuning {
    Tuning {
        effort: effort.map(str::to_string),
        fast,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_read_as_words_and_unknown_ones_are_not_renamed() {
        assert_eq!(level_label("xhigh"), "Extra high");
        assert_eq!(level_label("ultra"), "Ultra");
        assert_eq!(level_label("turbo"), "Turbo");
        assert_eq!(effort_chip_label(None), "Effort");
    }

    #[test]
    fn the_gauge_fills_lowest_to_highest_and_the_default_is_hollow() {
        let five = ["low", "medium", "high", "xhigh", "max"];
        let fills: Vec<usize> = five.iter().map(|l| gauge_fill(&five, Some(l))).collect();
        assert_eq!(
            fills,
            vec![1, 2, 3, 3, 4],
            "five levels onto four bars: {fills:?}"
        );
        let four = ["low", "medium", "high", "xhigh"];
        let fills: Vec<usize> = four.iter().map(|l| gauge_fill(&four, Some(l))).collect();
        assert_eq!(fills, vec![1, 2, 3, 4]);
        let seven = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
        let fills: Vec<usize> = seven.iter().map(|l| gauge_fill(&seven, Some(l))).collect();
        assert_eq!(*fills.last().unwrap(), GAUGE_BARS);
        assert!(
            fills.windows(2).all(|w| w[0] <= w[1]),
            "not monotonic: {fills:?}"
        );
        assert_eq!(gauge_fill(&five, None), 0);
        assert_eq!(gauge_fill(&five, Some("turbo")), 0);
    }

    #[test]
    fn the_menu_leads_with_default_and_marks_exactly_one_row() {
        let rows = effort_rows(&["low", "high"], Some("high"));
        assert_eq!(rows[0].label, "Default");
        assert_eq!(rows.iter().filter(|r| r.current).count(), 1);
        assert!(rows[2].current);
        assert_eq!(rows[2].detail, "high");
        let none = effort_rows(&["low"], None);
        assert!(none[0].current);
    }
}
