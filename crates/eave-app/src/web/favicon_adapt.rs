//! Make a favicon readable on eave's dark rail, the way Arc tints a
//! GitHub mark white — and rasterize it so gpui paints 1:1.
//!
//! Favicons are authored for a light page. Dropped onto a dark tile, a
//! black-on-transparent glyph (x.com, many SVGs) vanishes. A colourful
//! mark (YouTube) and a mark that is already light-on-dark (GitHub's
//! white octocat in a black circle) already read, and inverting them
//! would wreck them.
//!
//! The test: the icon is a **dark grey glyph with a transparent
//! surround**. Then invert RGB and keep alpha. Anything else is left
//! alone — including GitHub, whose canvas is filled, so it has no
//! surround, and whose octocat is already light.
//!
//! gpui's `img` treats PNG pixels as 1× logical, and Metal bilinear-
//! filters any size miss. A 16 logical tab plate is 32 device on a 2×
//! Mac; the bookmark mark is the same 16 logical / 32 device. Each
//! slot still gets its own exact-size PNG so a later size split does
//! not share a bitmap — one 48×48 on both slots is why the plate
//! used to bilinear-filter 48→32.

use image::{
    codecs::png::PngEncoder, imageops, ExtendedColorType, ImageEncoder, ImageFormat, RgbaImage,
};

/// Device pixels for [`crate::theme::RAIL_MARK`] (16 logical) on a 2×
/// display. Must stay `RAIL_MARK * 2`.
pub const TAB_DEVICE_PX: u32 = 32;
/// Device pixels for [`crate::theme::BOOKMARK_TILE_MARK`] (16 logical)
/// on a 2× display. Must stay `BOOKMARK_TILE_MARK * 2`.
pub const TILE_DEVICE_PX: u32 = 32;

/// Opaque enough to count as "the mark" rather than an anti-aliased
/// fringe. 32/255 is ~12.5%.
const ALPHA_MIN: u8 = 32;
/// ` (max-min)/max ` below this is treated as grey. Chrome-ish
/// near-achromatic; a YouTube red square is ~0.9.
const SAT_GRAY: f32 = 0.18;
/// Fraction of pixels that are transparent. A filled GitHub circle is
/// ~0; an X on a clear canvas is well above 0.15.
const SURROUND: f32 = 0.15;
/// Mean Rec. 709 luma of opaque pixels, in 0..1. Below this the mark is
/// a dark glyph. GitHub's white cat plus black circle lands around
/// mid-grey and stays put.
const DARK_Y: f32 = 0.35;
/// Opaque pixels that must be grey for the invert to fire. Guards a
/// dark-but-coloured logo.
const GRAY_FRAC: f32 = 0.80;

/// Exact-size PNGs for the two paint slots, plus the native source
/// side so a later download can replace a 32 px ICO with a 120 px
/// apple-touch without the small one winning the race.
pub struct RailPngs {
    pub tab: Vec<u8>,
    pub tile: Vec<u8>,
    pub source_px: u32,
}

/// Invert a dark glyph if needed, then rasterize to [`TAB_DEVICE_PX`] and
/// [`TILE_DEVICE_PX`] so each slot paints 1:1 on a 2× display.
pub fn for_dark_rail(png: &[u8]) -> RailPngs {
    let Ok(img) = image::load_from_memory_with_format(png, ImageFormat::Png) else {
        return RailPngs {
            tab: png.to_vec(),
            tile: png.to_vec(),
            source_px: 0,
        };
    };
    let mut rgba = img.to_rgba8();
    if should_invert(&rgba) {
        invert_rgb(&mut rgba);
    }
    let square = to_square(&rgba);
    let source_px = square.width();
    RailPngs {
        tab: encode_png(&resize_to(&square, TAB_DEVICE_PX)).unwrap_or_else(|| png.to_vec()),
        tile: encode_png(&resize_to(&square, TILE_DEVICE_PX)).unwrap_or_else(|| png.to_vec()),
        source_px,
    }
}

/// If `png` is a dark-on-transparent glyph, return an inverted PNG.
/// Otherwise return the original bytes (including on decode failure).
/// Does not resample — the invert decision is independent of slot size.
#[cfg(test)]
pub fn adapt_for_dark(png: &[u8]) -> Vec<u8> {
    let Ok(img) = image::load_from_memory_with_format(png, ImageFormat::Png) else {
        return png.to_vec();
    };
    let mut rgba = img.to_rgba8();
    if !should_invert(&rgba) {
        return png.to_vec();
    }
    invert_rgb(&mut rgba);
    encode_png(&rgba).unwrap_or_else(|| png.to_vec())
}

fn invert_rgb(img: &mut RgbaImage) {
    for p in img.pixels_mut() {
        p.0[0] = 255 - p.0[0];
        p.0[1] = 255 - p.0[1];
        p.0[2] = 255 - p.0[2];
    }
}

fn to_square(img: &RgbaImage) -> RgbaImage {
    let w = img.width();
    let h = img.height();
    if w == h {
        return img.clone();
    }
    let side = w.max(h).max(1);
    let mut out = RgbaImage::new(side, side);
    let x = (side - w) / 2;
    let y = (side - h) / 2;
    imageops::replace(&mut out, img, i64::from(x), i64::from(y));
    out
}

fn resize_to(img: &RgbaImage, dest: u32) -> RgbaImage {
    let side = img.width().max(img.height()).max(1);
    if side == dest {
        return img.clone();
    }
    // Downscale: Lanczos3 keeps edges. Integer 2× upscale of a 16 px
    // ICO (the tab plate and the tile, both 32 device) is nearest so
    // each source pixel is a 2×2 block and the texture is 1:1. Any
    // other upscale is cubic — 1.5× nearest is the jagged "blurry
    // pixels" the 48-everywhere pass produced.
    let filter = if side > dest {
        imageops::FilterType::Lanczos3
    } else if dest % side == 0 && dest / side == 2 {
        imageops::FilterType::Nearest
    } else {
        imageops::FilterType::CatmullRom
    };
    imageops::resize(img, dest, dest, filter)
}

fn should_invert(img: &RgbaImage) -> bool {
    let total = img.width() as u32 * img.height() as u32;
    if total == 0 {
        return false;
    }
    let mut opaque = 0u32;
    let mut grey = 0u32;
    let mut luma_sum = 0.0f32;
    for p in img.pixels() {
        let [r, g, b, a] = p.0;
        if a < ALPHA_MIN {
            continue;
        }
        opaque += 1;
        let rf = r as f32 / 255.0;
        let gf = g as f32 / 255.0;
        let bf = b as f32 / 255.0;
        luma_sum += 0.2126 * rf + 0.7152 * gf + 0.0722 * bf;
        let max = rf.max(gf).max(bf);
        let min = rf.min(gf).min(bf);
        let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
        if sat < SAT_GRAY {
            grey += 1;
        }
    }
    if opaque == 0 {
        return false;
    }
    let surround = 1.0 - opaque as f32 / total as f32;
    let mean_y = luma_sum / opaque as f32;
    let grey_frac = grey as f32 / opaque as f32;
    surround > SURROUND && mean_y < DARK_Y && grey_frac > GRAY_FRAC
}

fn encode_png(img: &RgbaImage) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn png_of(img: &RgbaImage) -> Vec<u8> {
        encode_png(img).expect("encode")
    }

    fn png_size(png: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory_with_format(png, ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        (img.width(), img.height())
    }

    fn mean_luma(png: &[u8]) -> f32 {
        let img = image::load_from_memory_with_format(png, ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        let mut n = 0u32;
        let mut s = 0.0f32;
        for p in img.pixels() {
            if p.0[3] < ALPHA_MIN {
                continue;
            }
            n += 1;
            let y = 0.2126 * p.0[0] as f32 + 0.7152 * p.0[1] as f32 + 0.0722 * p.0[2] as f32;
            s += y / 255.0;
        }
        s / n as f32
    }

    /// Black X on clear canvas — the x.com case. Must become light.
    #[test]
    fn a_dark_glyph_on_clear_inverts() {
        let mut img = RgbaImage::new(16, 16);
        for x in 4..12 {
            for y in 4..12 {
                img.put_pixel(x, y, Rgba([20, 20, 20, 255]));
            }
        }
        let out = adapt_for_dark(&png_of(&img));
        assert!(
            mean_luma(&out) > 0.7,
            "a dark glyph stayed dark on a dark tile"
        );
    }

    /// Filled black circle with a white mark — GitHub's usual ICO.
    /// Inverting would turn the cat black. Leave it; the circle melts
    /// into the tile and the cat stays white.
    #[test]
    fn a_filled_light_on_dark_mark_is_left_alone() {
        let mut img = RgbaImage::new(16, 16);
        for p in img.pixels_mut() {
            *p = Rgba([20, 20, 20, 255]);
        }
        for x in 6..10 {
            for y in 6..10 {
                img.put_pixel(x, y, Rgba([230, 230, 230, 255]));
            }
        }
        let src = png_of(&img);
        let out = adapt_for_dark(&src);
        assert_eq!(out, src, "GitHub-shaped mark was inverted");
    }

    /// YouTube-red, even with a clear surround, must not invert.
    #[test]
    fn a_colour_mark_is_left_alone() {
        let mut img = RgbaImage::new(16, 16);
        for x in 2..14 {
            for y in 4..12 {
                img.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            }
        }
        let src = png_of(&img);
        let out = adapt_for_dark(&src);
        assert_eq!(out, src, "a red mark was inverted as if it were a glyph");
    }

    /// A 32×32 source (the usual 2× ICO frame) must land 1:1 on both
    /// slots at 32 device — stretching it is what Metal then filters.
    #[test]
    fn each_slot_is_its_device_size() {
        let mut img = RgbaImage::new(32, 32);
        for p in img.pixels_mut() {
            *p = Rgba([40, 80, 200, 255]);
        }
        let out = for_dark_rail(&png_of(&img));
        assert_eq!(png_size(&out.tab), (TAB_DEVICE_PX, TAB_DEVICE_PX));
        assert_eq!(png_size(&out.tile), (TILE_DEVICE_PX, TILE_DEVICE_PX));
        assert_eq!(out.source_px, 32);
    }
}
