//! The activity pattern — a model working in public.
//!
//! Ported from `bezel/apps/gallery/src/patterns/agent.rs`. The two library
//! pieces are [`bezel::ui::widgets::Takeover`] (the section opens while the
//! run streams and yields the moment you press the header) and
//! [`bezel::ui::scroll::follow`] (the box pins to its newest line).
//! Everything else is a `div`. Copy the file.

use std::time::Duration;

use gpui::{
    div, linear_color_stop, linear_gradient, prelude::*, px, Context, IntoElement, Render,
    ScrollHandle, SharedString, Window,
};
use bezel::theme::Theme;
use bezel::ui::{
    loaders,
    scroll::{self, FollowState},
    widgets::{self, Layout as _},
};

/// How tall the reasoning box may get before it scrolls — Tailwind's
/// `max-h-40`, which is what the source caps it at.
pub const BOX_MAX: f32 = 160.0;
const FADE: f32 = 20.0;
const LINE: Duration = Duration::from_millis(260);

const PROMPT: &str = "Why does bezel have no bezel-agent crate yet?";
const REASONING: [&str; 8] = [
    "Reading ARCHITECTURE.md.",
    "\"extracted from working application code, never invented ahead of need\"",
    "Two pieces have landed: scroll::follow and widgets::Takeover.",
    "Both are in crates/ui, not in a crate of their own.",
    "scroll::follow — a terminal wants this. A build log wants this.",
    "Takeover — a detail pane following a selection wants this.",
    "Neither fails \"would a non-agent app want it\".",
    "There isn't any residue a new crate would hold.",
];
const ANSWER: &str = "Because nothing has failed the test yet. Both pieces \
    ported so far are general enough that a terminal or a build log would want \
    them, so they went to `ui` beside the components they compose with.";

fn duration() -> f32 {
    REASONING.len() as f32 * LINE.as_secs_f32()
}

/// Theme-editor specimen. Owns its own takeover and follow-scroll, the
/// way the gallery page does.
pub struct ActivityDemo {
    run: Option<std::time::Instant>,
    thought: widgets::Takeover,
    scroll: ScrollHandle,
    follow: FollowState,
}

impl Default for ActivityDemo {
    fn default() -> Self {
        Self {
            run: None,
            thought: widgets::Takeover::default(),
            scroll: ScrollHandle::new(),
            follow: FollowState::new(),
        }
    }
}

impl ActivityDemo {
    fn shown(&self) -> usize {
        match self.run {
            None => REASONING.len(),
            Some(started) => {
                let lines = (started.elapsed().as_secs_f32() / LINE.as_secs_f32()) as usize;
                lines.min(REASONING.len())
            }
        }
    }

    fn running(&self) -> bool {
        self.shown() < REASONING.len()
    }

    fn ask(&mut self, cx: &mut Context<Self>) {
        self.run = Some(std::time::Instant::now());
        self.thought = widgets::Takeover::default();
        self.follow.follow();
        cx.notify();
    }

    fn header(&self, theme: &Theme, open: bool, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let running = self.running();
        let view = cx.entity_id();
        let glyph = if running {
            loaders::orb(loaders::Orb::Cluster, "thinking", 14.0, theme, view, cx)
                .into_any_element()
        } else {
            theme.disclosure(open).into_any_element()
        };
        div()
            .id("thought-header")
            .self_start()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(4.0))
            .py(px(5.0))
            .rounded(px(bezel::theme::Theme::CONTROL_RADIUS))
            .cursor_pointer()
            .hover(|s| s.bg(theme.element_hover))
            .on_click(cx.listener(move |view, _, _, cx| {
                let running = view.running();
                view.thought.toggle(running);
                cx.notify();
            }))
            .child(div().flex().w(px(14.0)).justify_center().child(glyph))
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(if running {
                        "Thinking".to_string()
                    } else {
                        format!("Thought for {:.0}s", duration())
                    })),
            )
    }

    fn reasoning(&self, theme: &Theme) -> gpui::Div {
        div()
            .ml(px(10.0))
            .pl(px(12.0))
            .border_l_1()
            .border_color(theme.border)
            .child(
                div()
                    .relative()
                    .max_h(px(BOX_MAX))
                    .child(
                        div()
                            .id("reasoning")
                            .max_h(px(BOX_MAX))
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .child(div().flex().flex_col().gap(px(4.0)).pr(px(14.0)).children(
                                REASONING.iter().take(self.shown()).map(|line| {
                                    div()
                                        .text_size(px(12.5))
                                        .text_color(theme.text_muted.opacity(0.7))
                                        .child(SharedString::from(*line))
                                }),
                            )),
                    )
                    .child(
                        div().absolute().top_0().left_0().right_0().h(px(FADE)).bg(
                            linear_gradient(
                                180.0,
                                linear_color_stop(theme.bg, 0.0),
                                linear_color_stop(theme.bg.opacity(0.0), 1.0),
                            ),
                        ),
                    )
                    .child(scroll::follow(&self.scroll, &self.follow)),
            )
    }
}

impl Render for ActivityDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let running = self.running();
        let open = self.thought.get(running);
        if running {
            window.request_animation_frame();
        }
        div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div()
                    .self_end()
                    .max_w(px(440.0))
                    .px(px(14.0))
                    .py(px(9.0))
                    .rounded(px(bezel::theme::Theme::SURFACE_RADIUS))
                    .bg(theme.surface_raised)
                    .text_size(px(13.5))
                    .text_color(theme.text)
                    .child(PROMPT),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(self.header(&theme, open, cx))
                    .when(open, |zone| zone.child(self.reasoning(&theme))),
            )
            .when(!running, |page| {
                page.child(
                    div()
                        .text_size(px(13.5))
                        .text_color(theme.text)
                        .child(ANSWER),
                )
            })
            .child(
                div()
                    .id("ask")
                    .self_start()
                    .cursor_pointer()
                    .on_click(cx.listener(|view, _, _, cx| view.ask(cx)))
                    .child({
                        use bezel::ui::widgets::{ButtonStyle, Buttons as _};
                        theme.button("Ask again", ButtonStyle::Ghost, Some("agent-ask".into()))
                    }),
            )
    }
}

/// Product activity box: the turn's thought, follow-scrolled, Takeover
/// on the header. Words stay eave's ("working for 12s") — the register
/// is instrument; the *shape* is bezel's.
pub fn reasoning_box(
    text: &str,
    scroll: &ScrollHandle,
    follow: &FollowState,
    bg: gpui::Hsla,
    border: gpui::Hsla,
    ink: gpui::Hsla,
) -> gpui::Div {
    let lines: Vec<SharedString> = if text.is_empty() {
        Vec::new()
    } else {
        text.lines()
            .map(|l| SharedString::from(l.to_string()))
            .collect()
    };
    div()
        .ml(px(10.0))
        .pl(px(12.0))
        .border_l_1()
        .border_color(border)
        .child(
            div()
                .relative()
                .max_h(px(BOX_MAX))
                .child(
                    div()
                        .id("session-reasoning")
                        .max_h(px(BOX_MAX))
                        .overflow_y_scroll()
                        .track_scroll(scroll)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .pr(px(14.0))
                                .children(lines.into_iter().map(|line| {
                                    div()
                                        .text_size(px(theme::SMALL_SIZE))
                                        .line_height(px(theme::SMALL_LINE))
                                        .text_color(ink)
                                        .child(line)
                                })),
                        ),
                )
                .child(
                    div().absolute().top_0().left_0().right_0().h(px(FADE)).bg(
                        linear_gradient(
                            180.0,
                            linear_color_stop(bg, 0.0),
                            linear_color_stop(bg.opacity(0.0), 1.0),
                        ),
                    ),
                )
                .child(scroll::follow(scroll, follow)),
        )
}

use crate::theme;
