//! The theme editor — bezel's Create page, in eave.
//!
//! Knobs write [`crate::theme::set_brand`]. Specimens are the three
//! gallery patterns Slice BZ never put on the product: activity, grouped
//! tool calls, and a numbered diff. They read bezel's `Theme` global, which
//! [`crate::bezel_theme`] rebuilds from eave's tokens, so a slider move
//! repaints both this window and the shell.

use gpui::{
    div, point, prelude::*, px, size, App, Bounds, Context, Div, DragMoveEvent, Empty, Entity,
    Render, SharedString, TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowOptions,
};

use bezel::theme::Theme;
use bezel::ui::widgets::{self, ButtonStyle, Buttons as _, Controls as _};

use crate::theme::{self, Brand, BASE_COLORS};

/// One knob's drag. gpui delivers a move to every listener of the
/// payload *type*, so a unit `SliderDrag` (bezel's pin) moves every
/// slider at once. The id is how bezel's Create page keeps them apart.
#[derive(Clone)]
struct BrandKnobDrag(&'static str);

gpui::actions!(eave_theme, [OpenThemeEditor]);

struct EditorWindows(Option<WindowHandle<ThemeEditor>>);
impl gpui::Global for EditorWindows {}

pub fn init(cx: &mut App) {
    cx.on_action(|_: &OpenThemeEditor, cx| open(cx));
}

/// Open the editor, or raise it if it is already up.
pub fn open(cx: &mut App) {
    if let Some(handle) = cx.try_global::<EditorWindows>().and_then(|g| g.0.clone()) {
        if handle
            .update(cx, |_, window, _| {
                window.activate_window();
            })
            .is_ok()
        {
            return;
        }
    }
    let bounds = Bounds::centered(None, size(px(720.0), px(820.0)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(520.0), px(480.0))),
            window_background: WindowBackgroundAppearance::Blurred,
            titlebar: Some(TitlebarOptions {
                title: Some("Theme".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(theme::TRAFFIC_X), px(theme::TRAFFIC_Y))),
            }),
            ..Default::default()
        },
        |_, cx| cx.new(ThemeEditor::new),
    );
    if let Ok(handle) = opened {
        cx.set_global(EditorWindows(Some(handle)));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Specimen {
    Activity,
    Tools,
    Diff,
}

struct Knob {
    label: &'static str,
    id: &'static str,
    max: f32,
    decimals: usize,
    read: fn(&Brand) -> f32,
    write: fn(&mut Brand, f32),
}

const KNOBS: [Knob; 6] = [
    Knob {
        label: "Base hue",
        id: "tint-hue",
        max: 360.0,
        decimals: 0,
        read: |b| b.tint.hue,
        write: |b, v| b.tint.hue = v,
    },
    Knob {
        label: "Base chroma",
        id: "tint-chroma",
        max: 0.06,
        decimals: 3,
        read: |b| b.tint.chroma,
        write: |b, v| b.tint.chroma = v,
    },
    Knob {
        label: "Accent hue",
        id: "accent-hue",
        max: 360.0,
        decimals: 0,
        read: |b| b.accent.hue,
        write: |b, v| b.accent.hue = v,
    },
    Knob {
        label: "Accent chroma",
        id: "accent-chroma",
        max: 0.2,
        decimals: 3,
        read: |b| b.accent.chroma,
        write: |b, v| b.accent.chroma = v,
    },
    Knob {
        label: "Radius",
        id: "radius",
        max: 16.0,
        decimals: 1,
        read: |b| b.radius,
        write: |b, v| b.radius = v,
    },
    Knob {
        label: "Frost",
        id: "glass",
        max: 1.0,
        decimals: 2,
        read: |b| b.glass,
        write: |b, v| b.glass = v,
    },
];

struct ThemeEditor {
    specimen: Specimen,
    activity: Entity<crate::bezel_activity::ActivityDemo>,
    tools: Entity<crate::bezel_tools::ToolCallsDemo>,
    diff: Entity<crate::bezel_diff::DiffDemo>,
}

impl ThemeEditor {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            specimen: Specimen::Activity,
            activity: cx.new(|_| crate::bezel_activity::ActivityDemo::default()),
            tools: cx.new(|_| crate::bezel_tools::ToolCallsDemo::default()),
            diff: cx.new(|_| crate::bezel_diff::DiffDemo),
        }
    }
}

impl Render for ThemeEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme = Theme::of(cx).clone();
        let brand = theme::brand();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .font_family(theme.font_sans.clone())
            .child(
                div()
                    .flex_none()
                    .h(px(theme::TITLEBAR_H))
                    .px(px(theme::SPACE_4))
                    .flex()
                    .flex_row()
                    .items_center()
                    .window_control_area(gpui::WindowControlArea::Drag)
                    .child(
                        div()
                            .ml(px(72.0))
                            .text_size(px(theme::SMALL_SIZE))
                            .text_color(theme.text_muted)
                            .child("Theme"),
                    ),
            )
            .child(
                div()
                    .id("theme-editor-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(24.0))
                    .pb(px(32.0))
                    .flex()
                    .flex_col()
                    .gap(px(16.0))
                    .child(heading(&theme, "Base color"))
                    .child(presets(&theme, &brand, cx))
                    .children(
                        KNOBS
                            .iter()
                            .map(|knob| slider(&theme, &brand, knob, cx).into_any_element()),
                    )
                    .child(
                        div()
                            .id("theme-reset")
                            .self_start()
                            .on_click(cx.listener(|_, _, _, cx| {
                                theme::set_brand(Brand::default(), cx);
                                cx.notify();
                            }))
                            .child(theme.button("Reset", ButtonStyle::Ghost, Some("reset".into()))),
                    )
                    .child(heading(&theme, "Contrast"))
                    .child(contrast(&theme))
                    .child(heading(&theme, "Specimen"))
                    .child(specimen_tabs(self.specimen, &theme, cx))
                    .child(match self.specimen {
                        Specimen::Activity => self.activity.clone().into_any_element(),
                        Specimen::Tools => self.tools.clone().into_any_element(),
                        Specimen::Diff => self.diff.clone().into_any_element(),
                    })
                    .child(heading(&theme, "Copy"))
                    .child(snippet(&brand, &theme)),
            )
    }
}

fn heading(theme: &Theme, label: &'static str) -> Div {
    div()
        .pt(px(8.0))
        .text_size(px(11.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text_muted)
        .child(label)
}

fn presets(theme: &Theme, brand: &Brand, cx: &mut Context<ThemeEditor>) -> gpui::AnyElement {
    theme
        .toggle_group()
        .children(BASE_COLORS.iter().map(|(name, tint)| {
            let tint = *tint;
            let mut next = *brand;
            next.tint = tint;
            theme
                .toggle_group_item(*name, brand.tint == tint)
                .id(SharedString::from(*name))
                .cursor_pointer()
                .on_click(cx.listener(move |_, _, _, cx| {
                    theme::set_brand(next, cx);
                    cx.notify();
                }))
        }))
        .into_any_element()
}

fn slider(
    theme: &Theme,
    brand: &Brand,
    knob: &'static Knob,
    cx: &mut Context<ThemeEditor>,
) -> gpui::AnyElement {
    let value = (knob.read)(brand);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .child(
            div()
                .w(px(96.0))
                .flex_none()
                .text_size(px(12.5))
                .text_color(theme.text_muted)
                .child(knob.label),
        )
        .child(
            div()
                .w(px(240.0))
                .flex_none()
                .child(
                    theme
                        .slider(if knob.max > 0.0 { value / knob.max } else { 0.0 })
                        .id(knob.id)
                        .cursor_pointer()
                        .on_drag(BrandKnobDrag(knob.id), |_, _, _, cx| cx.new(|_| Empty))
                        .on_drag_move(cx.listener(
                            move |_, event: &DragMoveEvent<BrandKnobDrag>, _, cx| {
                                if event.drag(cx).0 != knob.id {
                                    return;
                                }
                                let fraction = widgets::axis_fraction(
                                    event.event.position,
                                    event.bounds,
                                    gpui::Axis::Horizontal,
                                    0.0,
                                );
                                let mut next = theme::brand();
                                (knob.write)(
                                    &mut next,
                                    (fraction * knob.max).clamp(0.0, knob.max),
                                );
                                theme::set_brand(next, cx);
                                cx.notify();
                            },
                        )),
                ),
        )
        .child(
            div()
                .w(px(56.0))
                .flex_none()
                .text_size(px(12.0))
                .font_family(theme.font_mono.clone())
                .text_color(theme.text_faint)
                .child(SharedString::from(format!("{value:.*}", knob.decimals))),
        )
        .into_any_element()
}

fn contrast(theme: &Theme) -> gpui::AnyElement {
    let pairs: [(&str, gpui::Hsla, gpui::Hsla); 4] = [
        ("text / bg", crate::theme::text(), crate::theme::canvas()),
        (
            "muted / surface",
            crate::theme::text_secondary(),
            crate::theme::raised(),
        ),
        ("accent / bg", crate::theme::accent(), crate::theme::canvas()),
        (
            "label / accent",
            crate::theme::on_accent(),
            crate::theme::accent(),
        ),
    ];
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .children(pairs.into_iter().map(|(label, fg, bg)| {
            let ratio = crate::theme::contrast_ratio(fg, bg);
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .w(px(150.0))
                        .flex_none()
                        .text_size(px(12.5))
                        .text_color(theme.text_muted)
                        .child(label),
                )
                .child(
                    div()
                        .w(px(46.0))
                        .flex_none()
                        .text_size(px(12.0))
                        .font_family(theme.font_mono.clone())
                        .text_color(theme.text)
                        .child(SharedString::from(format!("{ratio:.2}"))),
                )
                .child(widgets::status_dot(if ratio >= 4.5 {
                    theme.success
                } else {
                    theme.danger
                }))
                .into_any_element()
        }))
        .into_any_element()
}

fn specimen_tabs(
    current: Specimen,
    theme: &Theme,
    cx: &mut Context<ThemeEditor>,
) -> gpui::AnyElement {
    let items = [
        (Specimen::Activity, "Activity"),
        (Specimen::Tools, "Tool calls"),
        (Specimen::Diff, "Diff"),
    ];
    theme
        .toggle_group()
        .children(items.into_iter().map(|(id, label)| {
            theme
                .toggle_group_item(label, current == id)
                .id(SharedString::from(label))
                .cursor_pointer()
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.specimen = id;
                    cx.notify();
                }))
        }))
        .into_any_element()
}

fn snippet(brand: &Brand, theme: &Theme) -> Div {
    let text = format!(
        "theme::set_brand(\n    \
         Brand {{\n        \
         tint: Tint::new({:.3}, {:.3}),\n        \
         accent: Tint::new({:.3}, {:.3}),\n        \
         radius: {:.1},\n        \
         glass: {:.2},\n    \
         }},\n    \
         cx,\n\
         );",
        brand.tint.hue,
        brand.tint.chroma,
        brand.accent.hue,
        brand.accent.chroma,
        brand.radius,
        brand.glass,
    );
    div()
        .p(px(12.0))
        .rounded(px(bezel::theme::Theme::PANEL_RADIUS))
        .bg(theme.code_wash)
        .border_1()
        .border_color(theme.border)
        .font_family(theme.font_mono.clone())
        .text_size(px(11.5))
        .text_color(theme.code_text)
        .child(SharedString::from(text))
}
