//! Tool-call presentation from bezel's gallery
//! (`bezel/apps/gallery/src/patterns/agent.rs`).
//!
//! bezel has no ToolCall type: `step_row` takes strings. What looked like
//! a component is `slice::chunk_by` over consecutive calls of the same
//! verb, plus a bordered box. A lone call is a card; a group of them
//! shares one box and the group's row owns the disclosure.
//!
//! The live transcript is virtualized per tool, so a group header that
//! swallowed three tools would be a display-list change. Consecutive
//! same-name tools still *look* grouped: they share one box by joining
//! their corners, which does not change row count or fingerprints.

use std::collections::HashSet;

use gpui::{div, prelude::*, px, Div, SharedString};
use eave_wire::model::Row;
use bezel::ui::icons;
use bezel::ui::widgets::{Status as _, step_row_hover};

use crate::theme;

/// Where this tool sits in a run of the same name.
#[derive(Clone, Copy)]
pub struct Run {
    pub first: bool,
    pub last: bool,
}

/// Consecutive `Row::Tool` of the same `name`, with nothing else between.
pub fn run_at(rows: &[Row], index: usize) -> Run {
    let Some(Row::Tool { name, .. }) = rows.get(index) else {
        return Run {
            first: true,
            last: true,
        };
    };
    let mut start = index;
    while start > 0 {
        match &rows[start - 1] {
            Row::Tool { name: prev, .. } if prev == name => start -= 1,
            _ => break,
        }
    }
    let mut end = index;
    while end + 1 < rows.len() {
        match &rows[end + 1] {
            Row::Tool { name: next, .. } if next == name => end += 1,
            _ => break,
        }
    }
    Run {
        first: start == index,
        last: end == index,
    }
}

/// The box a card and a group share: rounded, bordered, clipping.
pub fn box_(first: bool, last: bool) -> Div {
    let r = px(theme::scaled_radius(theme::RADIUS_PANEL));
    let bz = crate::bezel_theme::theme();
    div()
        .when(first && last, |d| d.rounded(r))
        .when(first && !last, |d| d.rounded_t(r))
        .when(last && !first, |d| d.rounded_b(r))
        .when(first, |d| d.border_t_1())
        .border_l_1()
        .border_r_1()
        .when(last, |d| d.border_b_1())
        .when(!first, |d| d.border_t_1())
        .border_color(bz.border)
        .overflow_hidden()
}

/// Theme-editor specimen: grouped tool calls, copied from the gallery.
pub struct ToolCallsDemo {
    open_groups: HashSet<usize>,
    open_output: HashSet<usize>,
}

impl Default for ToolCallsDemo {
    fn default() -> Self {
        let mut start = 0;
        let mut open_groups = HashSet::new();
        for run in CALLS.chunk_by(|a, b| a.verb == b.verb) {
            if run.len() > 1 {
                open_groups.insert(start);
                break;
            }
            start += run.len();
        }
        Self {
            open_groups,
            open_output: HashSet::new(),
        }
    }
}

struct Call {
    icon: &'static str,
    verb: &'static str,
    detail: &'static str,
    ms: u32,
    failed: bool,
    output: Option<&'static str>,
}

const CALLS: [Call; 8] = [
    Call {
        icon: icons::COMPASS,
        verb: "Discover",
        detail: "sources for \"bezel-agent\"",
        ms: 40,
        failed: false,
        output: Some("ARCHITECTURE.md\nTODO.md"),
    },
    Call {
        icon: icons::BOOK,
        verb: "Read",
        detail: "crates/ui/src/scroll.rs",
        ms: 3,
        failed: false,
        output: None,
    },
    Call {
        icon: icons::BOOK,
        verb: "Read",
        detail: "crates/ui/src/widgets.rs",
        ms: 2,
        failed: false,
        output: None,
    },
    Call {
        icon: icons::BOOK,
        verb: "Read",
        detail: "ARCHITECTURE.md",
        ms: 2,
        failed: false,
        output: Some("extracted from working application code"),
    },
    Call {
        icon: icons::TERMINAL,
        verb: "Run",
        detail: "cargo test -p ui",
        ms: 1_412,
        failed: false,
        output: Some("running 87 tests\n\ntest result: ok. 87 passed; 0 failed"),
    },
    Call {
        icon: icons::BOOK,
        verb: "Read",
        detail: "crates/agent/src/lib.rs",
        ms: 1,
        failed: true,
        output: Some("error: no such file or directory (os error 2)"),
    },
    Call {
        icon: icons::PEN,
        verb: "Edit",
        detail: "todos/agent.md",
        ms: 7,
        failed: false,
        output: None,
    },
    Call {
        icon: icons::GIT_BRANCH,
        verb: "Diff",
        detail: "crates/ui",
        ms: 31,
        failed: false,
        output: None,
    },
];

fn took(ms: u32) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f32 / 1000.0)
    }
}

impl ToolCallsDemo {
    fn card(theme: &bezel::theme::Theme) -> Div {
        div()
            .rounded(px(bezel::theme::Theme::PANEL_RADIUS))
            .border_1()
            .border_color(theme.border)
            .overflow_hidden()
    }

    fn call(
        &self,
        theme: &bezel::theme::Theme,
        index: usize,
        first: bool,
        cx: &mut gpui::Context<Self>,
    ) -> Div {
        let call = &CALLS[index];
        let open = self.open_output.contains(&index);
        div()
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .child(
                theme
                    .step_row(
                        call.icon,
                        call.verb,
                        Some(SharedString::from(call.detail)),
                        Some(SharedString::from(took(call.ms))),
                        call.failed,
                        call.output.map(|_| open),
                    )
                    .hover(step_row_hover)
                    .id(SharedString::from(format!("demo-call-{index}")))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if !view.open_output.insert(index) {
                            view.open_output.remove(&index);
                        }
                        cx.notify();
                    })),
            )
            .when_some(call.output.filter(|_| open), |el, output| {
                el.child(
                    theme.step_output(SharedString::from(format!("demo-out-{index}")), output),
                )
            })
    }
}

impl gpui::Render for ToolCallsDemo {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let theme = bezel::theme::Theme::of(cx).clone();
        let mut offset = 0;
        let mut groups = Vec::new();
        for run in CALLS.chunk_by(|a, b| a.verb == b.verb) {
            groups.push((offset, run.len()));
            offset += run.len();
        }
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(groups.into_iter().map(|(start, len)| {
                if len == 1 {
                    return Self::card(&theme)
                        .child(self.call(&theme, start, true, cx))
                        .into_any_element();
                }
                let open = self.open_groups.contains(&start);
                let failed = CALLS[start..start + len].iter().any(|c| c.failed);
                Self::card(&theme)
                    .child(
                        theme
                            .step_row(
                                CALLS[start].icon,
                                CALLS[start].verb,
                                Some(SharedString::from(format!("· {len}"))),
                                None,
                                failed,
                                Some(open),
                            )
                            .hover(step_row_hover)
                            .id(SharedString::from(format!("demo-group-{start}")))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if !view.open_groups.insert(start) {
                                    view.open_groups.remove(&start);
                                }
                                cx.notify();
                            })),
                    )
                    .when(open, |group| {
                        group.child(
                            div()
                                .border_t_1()
                                .border_color(theme.border)
                                .pl(px(16.0))
                                .children((start..start + len).map(|index| {
                                    self.call(&theme, index, index == start, cx)
                                        .into_any_element()
                                })),
                        )
                    })
                    .into_any_element()
            }))
    }
}
