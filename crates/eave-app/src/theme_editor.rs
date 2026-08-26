//! The theme editor — a compact Customize Theme panel.
//!
//! Rebuilt 2026-08-25 on the operator's reference (an Arc-style customize
//! popover): target tabs, a 2-D hue×chroma field, preset swatches, and the
//! sliders. The bezel Create-page shape it replaces — knob list, contrast
//! table, specimen gallery, copyable snippet — is gone with its Demo
//! entities; the contrast guard survives as a compact strip because it is
//! a guard, not a gallery.
//!
//! **A popup, not a window** (operator directive 2026-08-26 — the first
//! cut opened a second window). The [`ThemeEditor`] view is the popup's
//! CONTENT; the shell owns the [`crate::overlay::Popup`] that mounts it
//! over the rail footer through `overlay::panel_layer`, which is also why
//! there is no titlebar and no ⌘W handler here — the catcher and Escape
//! close it the way they close every menu. A welcome side effect: the
//! main window takes synthetic clicks, so the panel is back inside the
//! harness's reach (second windows eat them — the standing trap).
//!
//! Every control writes [`crate::theme::set_brand`], which persists and
//! repaints every window — so there is no Save button: a Save that did
//! anything beyond what already happened would be a lie. The Light/Dark
//! toggle pins [`crate::settings::AppearanceSetting`]; the THEME is
//! shared across the switch and only the frost is per-appearance
//! (operator, 2026-08-26 — see [`crate::theme::brand`]), which is why
//! there is no Copy-from-Dark row: with one shared color there is
//! nothing to copy. The reference's Highlight tab is still absent:
//! the brand holds two tints, not three.

use gpui::{
    div, prelude::*, px, App, Context, Div, DragMoveEvent, Empty, Hsla, MouseButton, Render,
    SharedString, Window,
};

use bezel::theme::Theme;
use bezel::ui::widgets::{self, ButtonStyle, Buttons as _, Controls as _};

use crate::icons;
use crate::settings::{self, AppearanceSetting};
use crate::theme::{self, Brand, Tint, BASE_COLORS};

/// One knob's drag. gpui delivers a move to every listener of the
/// payload *type*, so a unit `SliderDrag` (bezel's pin) moves every
/// slider at once. The id is how bezel's Create page keeps them apart —
/// and the 2-D field rides the same channel under its own id.
#[derive(Clone)]
struct BrandKnobDrag(&'static str);

gpui::actions!(eave_theme, [OpenThemeEditor]);

/// Which of the brand's two tints the field is editing. These are the
/// panel's tabs, and there are exactly two because the brand holds exactly
/// two tints — the reference's third tab has nothing here to edit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Background,
    Accent,
}

impl Target {
    fn label(self) -> &'static str {
        match self {
            Target::Background => "Background",
            Target::Accent => "Accent",
        }
    }

    /// The ceiling the intensity axis runs to — the same maxima the old
    /// knob list used: surface tints stay quiet, accents may saturate.
    fn max_chroma(self) -> f32 {
        match self {
            Target::Background => 0.06,
            Target::Accent => 0.2,
        }
    }

    fn read(self, b: &Brand) -> Tint {
        match self {
            Target::Background => b.tint,
            Target::Accent => b.accent,
        }
    }

    fn write(self, b: &mut Brand, t: Tint) {
        match self {
            Target::Background => b.tint = t,
            Target::Accent => b.accent = t,
        }
    }
}

/// The 2-D field's raster. Fixed pixel sizes because the selection ring is
/// absolutely positioned over it and a fraction needs a denominator.
const FIELD_W: f32 = 336.0;
const FIELD_H: f32 = 160.0;
const FIELD_COLS: usize = 24;
const FIELD_ROWS: usize = 10;

/// Visual chroma the field paints at — the sample's candy rainbow, not the
/// quiet wash a click actually installs. Y still maps to
/// [`Target::max_chroma`]: a click on the top row of Background is 0.06,
/// even though the cell is painted at 0.22. Without this the Background
/// grid is 24×10 cells of near-grey, which is a colour picker that cannot
/// be read as one.
const FIELD_PREVIEW_CHROMA: f32 = 0.22;
/// Lightness at the top of the field (saturated) and the bottom (near
/// black). The sample fades candy → black; a fixed mid-grey L made every
/// hue look like mud.
const FIELD_PREVIEW_L_TOP: f32 = 0.74;
const FIELD_PREVIEW_L_BOT: f32 = 0.16;

/// Paint a field cell. `chroma` is the brand value (0..=max); the sample
/// conversion still goes through [`theme::field_sample`] so a click and
/// the palette share one oklch path — only the *display* chroma and L
/// are scaled up so the grid can be read.
fn field_paint(chroma: f32, max: f32, hue: f32) -> Hsla {
    let frac = if max <= f32::EPSILON {
        0.0
    } else {
        (chroma / max).clamp(0.0, 1.0)
    };
    let l = FIELD_PREVIEW_L_BOT + frac * (FIELD_PREVIEW_L_TOP - FIELD_PREVIEW_L_BOT);
    theme::field_sample(l, frac * FIELD_PREVIEW_CHROMA, hue)
}

struct Knob {
    label: &'static str,
    id: &'static str,
    max: f32,
    decimals: usize,
    read: fn(&Brand) -> f32,
    write: fn(&mut Brand, f32),
}

/// Intensity is the field's vertical axis said as a number — two controls,
/// one value, the same pairing the old editor had between a slider and its
/// readout. One knob per target because their ceilings differ.
const INTENSITY_BG: Knob = Knob {
    label: "Intensity",
    id: "tint-chroma",
    max: 0.06,
    decimals: 3,
    read: |b| b.tint.chroma,
    write: |b, v| b.tint.chroma = v,
};
const INTENSITY_ACCENT: Knob = Knob {
    label: "Intensity",
    id: "accent-chroma",
    max: 0.2,
    decimals: 3,
    read: |b| b.accent.chroma,
    write: |b, v| b.accent.chroma = v,
};
/// The window frost. "Alpha" in the reference's words; it is the one alpha
/// eave's ground actually has, and it belongs to the window rather than to
/// either tint, so it renders on the Background tab only.
const ALPHA: Knob = Knob {
    label: "Alpha",
    id: "glass",
    max: 1.0,
    decimals: 2,
    read: |b| b.glass,
    write: |b, v| b.glass = v,
};
const RADIUS: Knob = Knob {
    label: "Radius",
    id: "radius",
    max: 16.0,
    decimals: 1,
    read: |b| b.radius,
    write: |b, v| b.radius = v,
};

/// The accent swatch row: the shipped violet first (it is the default the
/// Reset button restores), then the wheel at the accent ramp's own chroma.
const ACCENT_SWATCHES: [(&str, Tint); 7] = [
    ("Violet", Tint::new(293.541, 0.183)),
    ("Blue", Tint::new(259.0, 0.18)),
    ("Cyan", Tint::new(221.0, 0.14)),
    ("Green", Tint::new(150.0, 0.16)),
    ("Amber", Tint::new(75.0, 0.15)),
    ("Red", Tint::new(25.0, 0.19)),
    ("Pink", Tint::new(350.0, 0.17)),
];

pub struct ThemeEditor {
    target: Target,
}

impl ThemeEditor {
    pub(crate) fn new(_: &mut Context<Self>) -> Self {
        Self {
            target: Target::Background,
        }
    }

    /// Apply a picked `(hue, chroma)` to the active target.
    fn pick(&self, hue: f32, chroma: f32, cx: &mut App) {
        let target = self.target;
        let mut next = theme::brand();
        let max = target.max_chroma();
        target.write(
            &mut next,
            Tint::new(hue.clamp(0.0, 360.0), chroma.clamp(0.0, max)),
        );
        theme::set_brand(next, cx);
    }
}

impl Render for ThemeEditor {
    /// The popup's content: the panel body and nothing else. Material,
    /// motion and dismissal belong to `overlay::panel_layer` in the shell
    /// — this element brings only its own size, which is the one thing a
    /// panel keeps that a menu delegates to its rows.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme = Theme::of(cx).clone();
        let brand = theme::brand();
        let target = self.target;
        div()
            .id("theme-editor-body")
            .w(px(FIELD_W + 32.0))
            .max_h(px(600.0))
            .overflow_y_scroll()
            .p(px(16.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .text_color(theme.text)
            .font_family(theme.font_sans.clone())
            .child(
                div()
                    .text_size(px(theme::SMALL_SIZE))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Customize Theme"),
            )
            .child(self.appearance_toggle(&theme, cx))
            .child(self.tabs(&theme, cx))
            .child(self.field(target, &brand, cx))
            .child(self.swatches(target, &brand, cx))
            .child(slider(
                &theme,
                &brand,
                match target {
                    Target::Background => &INTENSITY_BG,
                    Target::Accent => &INTENSITY_ACCENT,
                },
                cx,
            ))
            .children(matches!(target, Target::Background).then(|| slider(&theme, &brand, &ALPHA, cx)))
            .child(slider(&theme, &brand, &RADIUS, cx))
            .child(
                div()
                    .id("theme-reset")
                    .debug_selector(|| "theme-reset".into())
                    .self_start()
                    .on_click(cx.listener(|_, _, _, cx| {
                        theme::set_brand(Brand::for_appearance(theme::appearance()), cx);
                        cx.notify();
                    }))
                    .child(theme.button("Reset", ButtonStyle::Ghost, Some("reset".into()))),
            )
            .child(heading(&theme, "Contrast"))
            .child(contrast(&theme))
    }
}

impl ThemeEditor {
    /// Light / Dark, as two equal pills. Pins the appearance setting
    /// (not System) so a click is a choice, and writes through
    /// [`settings::update`] so Settings and this panel stay one value.
    fn appearance_toggle(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let current = theme::appearance();
        div()
            .flex()
            .flex_row()
            .gap(px(8.0))
            .children(
                [
                    (theme::Appearance::Light, "Light", icons::SUN),
                    (theme::Appearance::Dark, "Dark", icons::MOON),
                ]
                .into_iter()
                .map(|(mode, label, glyph)| {
                    let selected = current == mode;
                    let ink = if selected { theme.text } else { theme.text_muted };
                    div()
                        .id(SharedString::from(label))
                        .debug_selector(move || format!("customize-appearance-{label}"))
                        .flex_1()
                        .h(px(36.0))
                        .rounded(px(8.0))
                        .cursor_pointer()
                        .bg(if selected {
                            crate::theme::overlay_strong()
                        } else {
                            crate::theme::overlay()
                        })
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .gap(px(6.0))
                        .text_size(px(12.5))
                        .font_weight(if selected {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::NORMAL
                        })
                        .text_color(ink)
                        .child(icons::icon(glyph, crate::theme::ICON_SM, ink))
                        .child(label)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            settings::update(cx, |s| {
                                s.appearance = match mode {
                                    theme::Appearance::Light => AppearanceSetting::Light,
                                    theme::Appearance::Dark => AppearanceSetting::Dark,
                                };
                            });
                            cx.notify();
                        }))
                }),
            )
    }

    /// The target tabs — the reference's underline grammar: quiet labels,
    /// a 2 px rule under the active one in the palette's own accent.
    fn tabs(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_row()
            .gap(px(18.0))
            .children([Target::Background, Target::Accent].into_iter().map(|t| {
                let active = self.target == t;
                div()
                    .id(SharedString::from(t.label()))
                    .debug_selector(move || format!("customize-tab-{}", t.label()))
                    .cursor_pointer()
                    .pb(px(4.0))
                    .border_b_2()
                    .border_color(if active {
                        crate::theme::accent()
                    } else {
                        gpui::transparent_black()
                    })
                    .text_size(px(12.5))
                    .font_weight(if active {
                        gpui::FontWeight::SEMIBOLD
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .text_color(if active { theme.text } else { theme.text_muted })
                    .child(t.label())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.target = t;
                        cx.notify();
                    }))
            }))
    }

    /// The 2-D field: hue runs left→right, chroma runs bottom→top. Cells
    /// paint through [`field_paint`] (vivid chroma, candy→black lightness)
    /// so the grid can be read as a colour picker; a click still writes
    /// `0..=max_chroma` through the same oklch conversion. A cell click
    /// lands on that cell's pair; a drag tracks the pointer continuously.
    fn field(
        &self,
        target: Target,
        brand: &Brand,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let max = target.max_chroma();
        let current = target.read(brand);
        // The ring sits at the current pair. Chroma above the tab's own
        // ceiling (never writable from here) clamps to the top edge.
        let ring_x = (current.hue / 360.0).clamp(0.0, 1.0) * FIELD_W - 7.0;
        let ring_y = (1.0 - (current.chroma / max).clamp(0.0, 1.0)) * FIELD_H - 7.0;
        div()
            .id("customize-field")
            .debug_selector(|| "customize-field".into())
            .w(px(FIELD_W))
            .h(px(FIELD_H))
            .flex_none()
            .relative()
            .rounded(px(6.0))
            .overflow_hidden()
            .cursor_pointer()
            .flex()
            .flex_col()
            .on_drag(BrandKnobDrag("field"), |_, _, _, cx| cx.new(|_| Empty))
            .on_drag_move(cx.listener(
                move |view, event: &DragMoveEvent<BrandKnobDrag>, _, cx| {
                    if event.drag(cx).0 != "field" {
                        return;
                    }
                    let b = event.bounds;
                    let fx = (f32::from(event.event.position.x - b.origin.x)
                        / f32::from(b.size.width))
                    .clamp(0.0, 1.0);
                    let fy = (f32::from(event.event.position.y - b.origin.y)
                        / f32::from(b.size.height))
                    .clamp(0.0, 1.0);
                    view.pick(fx * 360.0, (1.0 - fy) * view.target.max_chroma(), cx);
                    cx.notify();
                },
            ))
            .children((0..FIELD_ROWS).map(|row| {
                div()
                    .flex_1()
                    .flex()
                    .flex_row()
                    .children((0..FIELD_COLS).map(|col| {
                        let hue = (col as f32 + 0.5) / FIELD_COLS as f32 * 360.0;
                        let chroma = (1.0 - (row as f32 + 0.5) / FIELD_ROWS as f32) * max;
                        div()
                            .flex_1()
                            .bg(field_paint(chroma, max, hue))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |view, _, _, cx| {
                                    view.pick(hue, chroma, cx);
                                    cx.notify();
                                }),
                            )
                    }))
            }))
            .child(
                div()
                    .absolute()
                    .left(px(ring_x.clamp(-2.0, FIELD_W - 12.0)))
                    .top(px(ring_y.clamp(-2.0, FIELD_H - 12.0)))
                    .w(px(14.0))
                    .h(px(14.0))
                    .rounded_full()
                    .border_2()
                    .border_color(gpui::white()),
            )
    }

    /// The preset row. Background offers the five neutral families the
    /// palette shipped with (bezel's own list, quoted); Accent offers the
    /// wheel at the accent ramp's chroma, violet first because violet is
    /// the default. A ring marks the swatch the brand currently matches —
    /// absent when the field picked something off-list, which is a fact,
    /// not a bug.
    fn swatches(&self, target: Target, brand: &Brand, cx: &mut Context<Self>) -> Div {
        let max = target.max_chroma();
        let current = target.read(brand);
        let items: Vec<(&'static str, Tint)> = match target {
            Target::Background => BASE_COLORS.to_vec(),
            Target::Accent => ACCENT_SWATCHES.to_vec(),
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .children(items.into_iter().map(|(name, tint)| {
                let selected = (current.hue - tint.hue).abs() < 0.01
                    && (current.chroma - tint.chroma).abs() < 1e-4;
                let fill: Hsla = if tint.chroma <= f32::EPSILON {
                    // The Neutral swatch: chroma 0 has no hue to preview,
                    // and sampling it through the field's lightness would
                    // paint five identical grey dots. One grey dot.
                    crate::theme::field_sample(if crate::theme::is_light() { 0.62 } else { 0.45 }, 0.0, 0.0)
                } else {
                    field_paint(tint.chroma, max, tint.hue)
                };
                div()
                    .id(SharedString::from(name))
                    .debug_selector(move || format!("customize-swatch-{name}"))
                    .cursor_pointer()
                    .w(px(20.0))
                    .h(px(20.0))
                    .rounded_full()
                    .bg(fill)
                    .border_2()
                    .border_color(if selected {
                        gpui::white()
                    } else {
                        gpui::transparent_black()
                    })
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.pick(tint.hue, tint.chroma, cx);
                        cx.notify();
                    }))
            }))
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
                .w(px(72.0))
                .flex_none()
                .text_size(px(12.5))
                .text_color(theme.text_muted)
                .child(knob.label),
        )
        .child(
            div()
                .flex_1()
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
                .w(px(48.0))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every preset swatch must sit inside its tab's own chroma ceiling —
    /// a swatch above it would set a value the field can neither show nor
    /// give back, so the ring would sit clamped at the top edge telling a
    /// quiet lie about what is installed.
    #[test]
    fn every_swatch_fits_inside_its_tabs_ceiling() {
        for (name, tint) in ACCENT_SWATCHES {
            assert!(
                tint.chroma <= Target::Accent.max_chroma(),
                "accent swatch {name} carries chroma {} over the tab's {}",
                tint.chroma,
                Target::Accent.max_chroma()
            );
        }
        for (name, tint) in BASE_COLORS {
            assert!(
                tint.chroma <= Target::Background.max_chroma(),
                "background swatch {name} carries chroma {} over the tab's {}",
                tint.chroma,
                Target::Background.max_chroma()
            );
        }
    }

    /// The two tabs write the two DIFFERENT tints, and each read gives
    /// back what its write installed — the wiring a copy-paste error
    /// between the match arms would break silently.
    #[test]
    fn each_tab_reads_and_writes_its_own_tint() {
        let mut b = Brand::default();
        Target::Background.write(&mut b, Tint::new(100.0, 0.05));
        Target::Accent.write(&mut b, Tint::new(200.0, 0.15));
        assert_eq!(Target::Background.read(&b), Tint::new(100.0, 0.05));
        assert_eq!(Target::Accent.read(&b), Tint::new(200.0, 0.15));
        assert_eq!(b.tint, Tint::new(100.0, 0.05));
        assert_eq!(b.accent, Tint::new(200.0, 0.15));
    }

    /// The default the Reset button restores is the first accent swatch —
    /// the ring must find it, or Reset lands the brand on a value no
    /// swatch claims.
    #[test]
    fn the_first_accent_swatch_is_the_shipped_default() {
        let (name, tint) = ACCENT_SWATCHES[0];
        assert_eq!(name, "Violet");
        assert!((tint.hue - 293.541).abs() < 1e-3);
        assert!((tint.chroma - 0.183).abs() < 1e-4);
    }

    /// The Background field must paint louder than it writes. A grid whose
    /// cells use the 0.06 apply ceiling is a grey rectangle; the sample is
    /// a rainbow. This pins the display ceiling above the apply one so a
    /// later "make the field honest" cleanup cannot quietly grey it out.
    #[test]
    fn the_background_field_paints_louder_than_it_writes() {
        assert!(
            FIELD_PREVIEW_CHROMA > Target::Background.max_chroma(),
            "preview chroma {} must exceed the Background apply ceiling {}",
            FIELD_PREVIEW_CHROMA,
            Target::Background.max_chroma()
        );
        assert!(
            FIELD_PREVIEW_L_TOP > FIELD_PREVIEW_L_BOT,
            "the field must fade bright→dark, not the other way"
        );
    }
}
