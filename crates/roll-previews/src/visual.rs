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

#[cfg(test)]
mod tests {
    use super::changed;
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
    }
}
