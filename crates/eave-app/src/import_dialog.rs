//! Import from your old browser — the Arc-style first-run picker.
//!
//! A popup (not a second window): the shell mounts it through
//! `overlay::panel_layer`, same as Customize Theme. Second windows eat
//! synthetic clicks; this one has to be walkable.

use std::sync::Arc;

use eave_core::browser::import::{
    self, Browser, ImportedPayload, SourceStatus,
};

use gpui::{
    div, img, prelude::*, px, App, ClickEvent, Context, Div, EventEmitter, Image, ImageFormat,
    IntoElement, ObjectFit, Render, SharedString, Window,
};

use crate::icons;
use crate::theme;

gpui::actions!(eave_import, [OpenImportDialog]);

pub enum ImportEvent {
    Close,
    Apply {
        bookmarks: Vec<eave_wire::ImportedBookmark>,
        history: Vec<eave_wire::ImportedVisit>,
    },
}

pub struct ImportDialog {
    sources: Vec<(Browser, SourceStatus)>,
    selected: Option<Browser>,
    step: Step,
    include_bookmarks: bool,
    include_history: bool,
}

enum Step {
    Pick,
    Confirm(ImportedPayload),
    Failed(String),
    Done { bookmarks: u32, history: u32 },
}

impl EventEmitter<ImportEvent> for ImportDialog {}

impl ImportDialog {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            sources: import::detect_from_home(),
            selected: None,
            step: Step::Pick,
            include_bookmarks: true,
            include_history: true,
        }
    }

    pub fn reset(&mut self) {
        self.sources = import::detect_from_home();
        self.selected = None;
        self.step = Step::Pick;
        self.include_bookmarks = true;
        self.include_history = true;
    }

}

pub fn any_source_ready() -> bool {
    import::detect_from_home()
        .iter()
        .any(|(_, s)| *s == SourceStatus::Ready)
}

impl Render for ImportDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
            .id("import-dialog")
            .debug_selector(|| "import-dialog".into())
            .w(px(560.0))
            .flex_none()
            .p(px(28.0))
            .flex()
            .flex_col()
            .gap(px(18.0))
            .text_color(theme::text())
            .child(self.header(cx))
            .child(match &self.step {
                Step::Pick => self.pick_body(cx).into_any_element(),
                Step::Confirm(payload) => self.confirm(payload, cx).into_any_element(),
                Step::Failed(msg) => self.failed(msg, cx).into_any_element(),
                Step::Done {
                    bookmarks,
                    history,
                } => self.done(*bookmarks, *history, cx).into_any_element(),
            })
    }
}

impl ImportDialog {
    fn header(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_row()
            .items_start()
            .child(
                div()
                    .size(px(44.0))
                    .rounded(px(12.0))
                    .bg(theme::solid())
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(22.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme::on_solid())
                    .child("e"),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("import-close")
                    .debug_selector(|| "import-close".into())
                    .size(px(28.0))
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx)))
                    .child(icons::icon(icons::CLOSE, theme::ICON_SM, theme::text_secondary())),
            )
    }

    fn pick_body(&self, cx: &mut Context<Self>) -> Div {
        let selected = self.selected;
        let can_next = selected.is_some_and(|b| {
            self.sources
                .iter()
                .any(|(src, st)| *src == b && *st == SourceStatus::Ready)
        });
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(24.0))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .gap(px(28.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .pt(px(8.0))
                            .child(
                                div()
                                    .text_size(px(22.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Import from your old browser"),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .min_w(px(0.))
                                    .text_size(px(14.0))
                                    .text_color(theme::text_secondary())
                                    .child(
                                        "Import everything you need from your old browser to hit the ground running with Eave.",
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .w(px(PILL_W))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .children(self.sources.iter().copied().map(|(browser, status)| {
                                source_pill(browser, status, selected == Some(browser), cx)
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(10.0))
                    .child(footer_btn(
                        "import-maybe-later",
                        "Maybe Later",
                        false,
                        true,
                        cx.listener(|this, _, _, cx| this.dismiss(cx)),
                    ))
                    .child(footer_btn(
                        "import-next",
                        "Next",
                        true,
                        can_next,
                        cx.listener(|this, _, _, cx| this.go_next(cx)),
                    )),
            )
    }

    fn confirm(&self, payload: &ImportedPayload, cx: &mut Context<Self>) -> Div {
        let bm = payload.bookmark_count();
        let hist = payload.history_count();
        let source = payload.source.map(|b| b.label()).unwrap_or("that browser");
        let can_import = (self.include_bookmarks && bm > 0) || (self.include_history && hist > 0);
        div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div()
                    .text_size(px(22.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(format!("Import from {source}")),
            )
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(theme::text_secondary())
                    .child("Choose what to bring across. Passwords stay where they are — Eave has nowhere to put them yet."),
            )
            .child(checkbox_row(
                "import-bookmarks",
                "Bookmarks",
                bm,
                bm > 0,
                self.include_bookmarks && bm > 0,
                cx.listener(|this, _, _, cx| {
                    this.include_bookmarks = !this.include_bookmarks;
                    cx.notify();
                }),
            ))
            .child(checkbox_row(
                "import-history",
                "History",
                hist,
                hist > 0,
                self.include_history && hist > 0,
                cx.listener(|this, _, _, cx| {
                    this.include_history = !this.include_history;
                    cx.notify();
                }),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(10.0))
                    .child(footer_btn(
                        "import-back",
                        "Back",
                        false,
                        true,
                        cx.listener(|this, _, _, cx| {
                            this.step = Step::Pick;
                            cx.notify();
                        }),
                    ))
                    .child(footer_btn(
                        "import-go",
                        "Import",
                        true,
                        can_import,
                        cx.listener(|this, _, _, cx| this.run_import(cx)),
                    )),
            )
    }

    fn failed(&self, msg: &str, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div()
                    .text_size(px(22.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Could not import"),
            )
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(theme::text_secondary())
                    .child(SharedString::from(msg.to_string())),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .child(footer_btn(
                        "import-back",
                        "Back",
                        false,
                        true,
                        cx.listener(|this, _, _, cx| {
                            this.step = Step::Pick;
                            cx.notify();
                        }),
                    )),
            )
    }

    fn done(&self, bookmarks: u32, history: u32, cx: &mut Context<Self>) -> Div {
        let summary = match (bookmarks, history) {
            (0, 0) => "Nothing to import.".into(),
            (b, 0) => format!("Imported {b} bookmarks."),
            (0, h) => format!("Imported {h} history entries."),
            (b, h) => format!("Imported {b} bookmarks and {h} history entries."),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div()
                    .text_size(px(22.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("You're in"),
            )
            .child(
                div()
                    .id("import-summary")
                    .debug_selector(|| "import-summary".into())
                    .text_size(px(14.0))
                    .text_color(theme::text_secondary())
                    .child(summary),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .child(footer_btn(
                        "import-done",
                        "Done",
                        true,
                        true,
                        cx.listener(|this, _, _, cx| this.dismiss(cx)),
                    )),
            )
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(ImportEvent::Close);
    }

    fn go_next(&mut self, cx: &mut Context<Self>) {
        let Some(browser) = self.selected else {
            return;
        };
        match import::load_from_home(browser, true, true) {
            Ok(payload) => {
                self.include_bookmarks = payload.bookmark_count() > 0;
                self.include_history = payload.history_count() > 0;
                self.step = Step::Confirm(payload);
            }
            Err(e) => self.step = Step::Failed(e.to_string()),
        }
        cx.notify();
    }

    fn run_import(&mut self, cx: &mut Context<Self>) {
        let Step::Confirm(payload) = &self.step else {
            return;
        };
        let bookmarks = if self.include_bookmarks {
            payload.bookmarks.iter().cloned().map(to_wire_node).collect()
        } else {
            Vec::new()
        };
        let history = if self.include_history {
            payload
                .history
                .iter()
                .cloned()
                .map(|v| eave_wire::ImportedVisit {
                    url: v.url,
                    title: v.title,
                    visit_count: v.visit_count,
                    last_visit_ms: v.last_visit_ms,
                })
                .collect()
        } else {
            Vec::new()
        };
        let bm = if self.include_bookmarks {
            payload.bookmark_count() as u32
        } else {
            0
        };
        let hist = if self.include_history {
            payload.history_count() as u32
        } else {
            0
        };
        cx.emit(ImportEvent::Apply {
            bookmarks,
            history,
        });
        self.step = Step::Done {
            bookmarks: bm,
            history: hist,
        };
        cx.notify();
    }
}

/// Shared width for every source pill so Chrome (the longest name) and
/// Arc (the shortest) occupy the same slot, icons on one vertical edge.
const PILL_W: f32 = 168.0;

fn to_wire_node(n: import::ImportedNode) -> eave_wire::ImportedBookmark {
    eave_wire::ImportedBookmark {
        title: n.title,
        url: n.url,
        children: n.children.into_iter().map(to_wire_node).collect(),
    }
}

fn source_pill(
    browser: Browser,
    status: SourceStatus,
    selected: bool,
    cx: &mut Context<ImportDialog>,
) -> impl IntoElement {
    let ready = status == SourceStatus::Ready;
    div()
        .id(SharedString::from(format!("import-source-{}", browser.label())))
        .debug_selector(move || format!("import-source-{}", browser.label()))
        .w(px(PILL_W))
        .h(px(44.0))
        .rounded(px(12.0))
        .overflow_hidden()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .pl(px(12.0))
        .bg(if selected {
            theme::overlay_strong()
        } else {
            theme::overlay()
        })
        .when(ready, |d| d.cursor_pointer())
        .opacity(if ready { 1.0 } else { 0.4 })
        .when(ready, |d| {
            d.on_click(cx.listener(move |this, _, _, cx| {
                this.selected = Some(browser);
                cx.notify();
            }))
        })
        .child(radio(selected && ready))
        .child(
            div()
                .ml(px(8.0))
                .flex_1()
                .min_w(px(0.))
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(browser.label()),
        )
        .child(
            div()
                .size(px(44.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .overflow_hidden()
                .child(source_logo(browser, 44.0)),
        )
}

/// The vendor's own macOS app icon. PNG, not an SVG reconstruction:
/// `svg().path()` is an alpha mask and would grey them out, and a
/// drawn-from-memory Chrome circle is not Chrome.
fn source_logo(browser: Browser, size: f32) -> impl IntoElement {
    let bytes: &'static [u8] = match browser {
        Browser::Brave => include_bytes!("../assets/logos/brave.png"),
        Browser::Chrome => include_bytes!("../assets/logos/chrome.png"),
        Browser::Safari => include_bytes!("../assets/logos/safari.png"),
        Browser::Arc => include_bytes!("../assets/logos/arc.png"),
    };
    img(Arc::new(Image::from_bytes(
        ImageFormat::Png,
        bytes.to_vec(),
    )))
    .size(px(size))
    .flex_none()
    .object_fit(ObjectFit::Contain)
}

fn radio(on: bool) -> Div {
    div()
        .size(px(16.0))
        .rounded_full()
        .border_1()
        .border_color(if on {
            theme::text()
        } else {
            theme::text_tertiary()
        })
        .flex()
        .items_center()
        .justify_center()
        .when(on, |d| {
            d.child(div().size(px(8.0)).rounded_full().bg(theme::text()))
        })
}

fn checkbox_row(
    id: &'static str,
    label: &'static str,
    count: usize,
    enabled: bool,
    on: bool,
    handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let caption = if enabled {
        format!("{count}")
    } else {
        "none".into()
    };
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .h(px(40.0))
        .px(px(12.0))
        .rounded(px(10.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .bg(theme::overlay())
        .when(enabled, |d| d.cursor_pointer().on_click(handler))
        .opacity(if enabled { 1.0 } else { 0.45 })
        .child(
            div()
                .size(px(16.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(theme::text_secondary())
                .bg(if on {
                    theme::solid()
                } else {
                    gpui::transparent_black()
                }),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(13.0))
                .child(label),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(theme::text_secondary())
                .child(caption),
        )
}

fn footer_btn(
    id: &'static str,
    label: &'static str,
    primary: bool,
    enabled: bool,
    handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .h(px(36.0))
        .px(px(16.0))
        .rounded(px(10.0))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(13.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .bg(if primary {
            theme::solid()
        } else {
            theme::overlay_strong()
        })
        .text_color(if primary {
            theme::on_solid()
        } else {
            theme::text()
        })
        .opacity(if enabled { 1.0 } else { 0.4 })
        .when(enabled, |d| d.cursor_pointer().on_click(handler))
        .child(label)
}
