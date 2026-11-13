//! How much a preview's page changed between two pushes (Percy's and Chromatic's visual review,
//! from the screenshots roll takes at each head): the share of pixels that differ.

use image::{ImageFormat, ImageReader, Limits, RgbaImage};
use std::io::Cursor;

/// A channel may differ this much before its pixel counts as changed (anti-aliasing, a caret).
const TOLERANCE: u8 = 16;

/// A screenshot as pixels, refused past what a page's screenshot can be.
fn decode(png: &[u8]) -> Option<RgbaImage> {
    let mut r = ImageReader::with_format(Cursor::new(png), ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 << 20);
    r.limits(limits);
    r.decode().ok().map(|i| i.to_rgba8())
}

/// The share of pixels (0.0–1.0) that differ between `before` and `after`; the area one has and
/// the other doesn't counts as changed. `None` when either isn't a PNG it can read.
pub fn changed(before: &[u8], after: &[u8]) -> Option<f64> {
    let (a, b) = (decode(before)?, decode(after)?);
    let (w, h) = (a.width().max(b.width()) as u64, a.height().max(b.height()) as u64);
    if w == 0 || h == 0 {
        return Some(0.0);
    }
    let (cw, ch) = (a.width().min(b.width()), a.height().min(b.height()));
    let mut differ: u64 = w * h - cw as u64 * ch as u64;
    for y in 0..ch {
        for x in 0..cw {
            let (p, q) = (a.get_pixel(x, y).0, b.get_pixel(x, y).0);
            if p.iter().zip(q.iter()).any(|(m, n)| m.abs_diff(*n) > TOLERANCE) {
                differ += 1;
            }
        }
    }
    Some(differ as f64 / (w * h) as f64)
}

/// What changed, as a picture (Percy's and Chromatic's diff view): `after` faded to a third
/// where it's the same as `before`, and every pixel that differs (or that only one of them has)
/// marked in a solid red. `None` when either isn't a PNG it can read.
pub fn diff(before: &[u8], after: &[u8]) -> Option<Vec<u8>> {
    const MARK: [u8; 4] = [235, 52, 82, 255];
    let (a, b) = (decode(before)?, decode(after)?);
    let (w, h) = (a.width().max(b.width()), a.height().max(b.height()));
    let out = RgbaImage::from_fn(w, h, |x, y| {
        let (p, q) = (a.get_pixel_checked(x, y).map(|p| p.0), b.get_pixel_checked(x, y).map(|p| p.0));
        match (p, q) {
            (Some(p), Some(q)) if !p.iter().zip(q.iter()).any(|(m, n)| m.abs_diff(*n) > TOLERANCE) => {
                // Faded toward white, so the marks stand out on dark pages and light ones.
                let fade = |c: u8| (c as u16 / 3 + 170) as u8;
                image::Rgba([fade(q[0]), fade(q[1]), fade(q[2]), 255])
            }
            _ => image::Rgba(MARK),
        }
    });
    let mut png = Cursor::new(Vec::new());
    out.write_to(&mut png, ImageFormat::Png).ok()?;
    Some(png.into_inner())
}

#[cfg(test)]
mod tests {
    use super::{changed, diff};
    use image::{ImageFormat, Rgba, RgbaImage};

    fn png(w: u32, h: u32, paint: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let img = RgbaImage::from_fn(w, h, |x, y| Rgba(paint(x, y)));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn the_share_of_pixels_that_changed() {
        let white = png(100, 100, |_, _| [255, 255, 255, 255]);
        assert_eq!(changed(&white, &white), Some(0.0));
        // A quarter repainted; a near-identical shade isn't a change.
        let quarter = png(100, 100, |x, y| if x < 50 && y < 50 { [0, 0, 0, 255] } else { [250, 250, 250, 255] });
        assert_eq!(changed(&white, &quarter), Some(0.25));
        // Taller: the new area counts.
        let taller = png(100, 200, |_, _| [255, 255, 255, 255]);
        assert_eq!(changed(&white, &taller), Some(0.5));
        assert_eq!(changed(b"not a png", &white), None);
        // The diff marks what changed, and the area only one of them has.
        let d = image::load_from_memory(&diff(&white, &quarter).unwrap()).unwrap().to_rgba8();
        assert_eq!((d.get_pixel(10, 10).0, d.get_pixel(90, 90).0), ([235, 52, 82, 255], [253, 253, 253, 255]));
        let d = image::load_from_memory(&diff(&white, &taller).unwrap()).unwrap().to_rgba8();
        assert_eq!((d.height(), d.get_pixel(0, 150).0), (200, [235, 52, 82, 255]));
    }
}
