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
    compare(before, after).map(|(share, _)| share)
}

/// [`changed`], and what the change is (Percy's grouped diffs): the area that differs, as it
/// was and as it is, each reduced to an 8 × 8 average hash and its brightness, digested to 12
/// hex characters. Two pages changed the same way (a header's new logo on every page, at every
/// width and in every browser where it lands alike) have the same signature, so a reviewer
/// accepts them once. `None` for the signature when nothing differs.
pub fn compare(before: &[u8], after: &[u8]) -> Option<(f64, Option<String>)> {
    let (a, b) = (decode(before)?, decode(after)?);
    let (w, h) = (a.width().max(b.width()), a.height().max(b.height()));
    if w == 0 || h == 0 {
        return Some((0.0, None));
    }
    let (cw, ch) = (a.width().min(b.width()), a.height().min(b.height()));
    let mut differ: u64 = w as u64 * h as u64 - cw as u64 * ch as u64;
    // The changed area's box: what only one of them has is in it.
    let mut bbox = if (w, h) != (cw, ch) { Some((if w > cw { cw } else { 0 }, if h > ch { ch } else { 0 }, w, h)) } else { None };
    let mut grow = |x: u32, y: u32| {
        bbox = Some(match bbox {
            None => (x, y, x + 1, y + 1),
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1)),
        });
    };
    for y in 0..ch {
        for x in 0..cw {
            let (p, q) = (a.get_pixel(x, y).0, b.get_pixel(x, y).0);
            if p.iter().zip(q.iter()).any(|(m, n)| m.abs_diff(*n) > TOLERANCE) {
                differ += 1;
                grow(x, y);
            }
        }
    }
    let share = differ as f64 / (w as u64 * h as u64) as f64;
    let signature = bbox.map(|r| {
        use sha2::Digest as _;
        let ((was, was_mean), (is, is_mean)) = (average_hash(&a, r), average_hash(&b, r));
        // The shapes, and how bright each side is (a uniform area's hash is all zeros: a button
        // gone red and one gone blue aren't the same change).
        let raw = format!("{was:016x}{is:016x}{was_mean:02x}{is_mean:02x}");
        let digest = sha2::Sha256::digest(raw.as_bytes());
        digest.iter().take(6).map(|b| format!("{b:02x}")).collect::<String>()
    });
    Some((share, signature))
}

/// An 8 × 8 average hash of `img` inside `(x0, y0, x1, y1)` (clipped to it): each cell's
/// brightness against the mean; and that mean, in 16 steps.
fn average_hash(img: &RgbaImage, (x0, y0, x1, y1): (u32, u32, u32, u32)) -> (u64, u8) {
    let (x1, y1) = (x1.min(img.width()), y1.min(img.height()));
    if x0 >= x1 || y0 >= y1 {
        return (0, 0);
    }
    let mut cells = [0f64; 64];
    for (i, cell) in cells.iter_mut().enumerate() {
        let (cx, cy) = ((i % 8) as u32, (i / 8) as u32);
        let (ax, bx) = (x0 + (x1 - x0) * cx / 8, x0 + ((x1 - x0) * (cx + 1) / 8).max((x1 - x0) * cx / 8 + 1));
        let (ay, by) = (y0 + (y1 - y0) * cy / 8, y0 + ((y1 - y0) * (cy + 1) / 8).max((y1 - y0) * cy / 8 + 1));
        let (mut sum, mut n) = (0f64, 0f64);
        // At most 32 × 32 samples a cell: a whole-page change costs no more than a small one.
        let (sx, sy) = (((bx - ax) / 32).max(1), ((by - ay) / 32).max(1));
        let mut y = ay;
        while y < by.min(y1) {
            let mut x = ax;
            while x < bx.min(x1) {
                let p = img.get_pixel(x, y).0;
                sum += 0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64;
                n += 1.0;
                x += sx;
            }
            y += sy;
        }
        *cell = if n > 0.0 { sum / n } else { 0.0 };
    }
    let mean = cells.iter().sum::<f64>() / 64.0;
    // Above the mean by a whole step of brightness: a uniform area's cells differ from its mean
    // only by rounding, which mustn't make bits.
    (cells.iter().enumerate().fold(0u64, |h, (i, c)| if *c > mean + 1.0 { h | (1 << i) } else { h }), (mean / 16.0) as u8)
}

/// The diff's largest picture: a screenshot is a viewport (1280 × 800, a phone's 780 × 1688), and
/// past this a diff would cost hundreds of MiB for what nobody reads.
const DIFF_MAX: (u32, u32) = (2048, 8192);

/// One diff at a time: each holds two decoded screenshots (up to `decode`'s 256 MiB) and its
/// own picture, and it's made on request, so callers queue rather than add up.
static DIFFING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The last few diffs made, by what they were made of: a link in a pull request comment is
/// fetched again and again (and by anyone who has it), and it's the same picture each time.
static DIFFS: std::sync::Mutex<Vec<([u8; 32], Vec<u8>)>> = std::sync::Mutex::new(Vec::new());
const DIFFS_KEPT: usize = 8;

/// What changed, as a picture (Percy's and Chromatic's diff view): `after` faded to a third
/// where it's the same as `before`, and every pixel that differs (or that only one of them has)
/// marked in a solid red, up to [`DIFF_MAX`]. `None` when either isn't a PNG it can read.
pub fn diff(before: &[u8], after: &[u8]) -> Option<Vec<u8>> {
    use sha2::Digest as _;
    let key: [u8; 32] = sha2::Sha256::new().chain_update((before.len() as u64).to_be_bytes()).chain_update(before).chain_update(after).finalize().into();
    let kept = || DIFFS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _)| *k == key).map(|(_, png)| png.clone());
    if let Some(png) = kept() {
        return Some(png);
    }
    let _one = DIFFING.lock().unwrap_or_else(|e| e.into_inner());
    // Made while this one waited its turn.
    if let Some(png) = kept() {
        return Some(png);
    }
    let png = draw(before, after)?;
    let mut diffs = DIFFS.lock().unwrap_or_else(|e| e.into_inner());
    if diffs.len() >= DIFFS_KEPT {
        diffs.remove(0);
    }
    diffs.push((key, png.clone()));
    Some(png)
}

fn draw(before: &[u8], after: &[u8]) -> Option<Vec<u8>> {
    const MARK: [u8; 4] = [235, 52, 82, 255];
    let (a, b) = (decode(before)?, decode(after)?);
    let (w, h) = (a.width().max(b.width()).min(DIFF_MAX.0), a.height().max(b.height()).min(DIFF_MAX.1));
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

/// Lines a text comparison names, each way.
const TEXT_LINES: usize = 20;

/// A page's visible text against the baseline's (Chromatic's DOM snapshots, in words): the lines
/// it has that the baseline's doesn't, and the other way, in page order, at most 20 each with
/// how many in all. Lines are compared whole, as many times as they appear.
pub fn text_diff(then: &str, now: &str) -> crate::TextDiff {
    use std::collections::HashMap;
    let lines = |t: &str| t.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect::<Vec<_>>();
    let (then, now) = (lines(then), lines(now));
    let only = |a: &[String], b: &[String]| -> Vec<String> {
        let mut left: HashMap<&str, usize> = HashMap::new();
        for l in b {
            *left.entry(l.as_str()).or_default() += 1;
        }
        a.iter()
            .filter(|l| match left.get_mut(l.as_str()) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    false
                }
                _ => true,
            })
            .cloned()
            .collect()
    };
    let (added, removed) = (only(&now, &then), only(&then, &now));
    crate::TextDiff { added_n: added.len(), removed_n: removed.len(), added: added.into_iter().take(TEXT_LINES).collect(), removed: removed.into_iter().take(TEXT_LINES).collect() }
}

#[cfg(test)]
mod tests {
    /// Pages changed the same way have one signature wherever the change sits (Percy's grouped
    /// diffs); another change, a color included, has another; an unchanged page none.
    #[test]
    fn the_same_change_on_two_pages_has_one_signature() {
        // A 6 × 6 square at (x, y) on a white 40 × 40 page, in `color`.
        let page = |x: u32, y: u32, color: [u8; 4]| png(40, 40, move |px, py| if (x..x + 6).contains(&px) && (y..y + 6).contains(&py) { color } else { [255, 255, 255, 255] });
        let (black, red, blue) = ([0, 0, 0, 255], [220, 30, 30, 255], [30, 30, 220, 255]);
        let sig = |before: &[u8], after: &[u8]| super::compare(before, after).unwrap().1;
        let here = sig(&page(2, 2, black), &page(2, 2, red));
        assert!(here.as_ref().is_some_and(|s| s.len() == 12));
        assert_eq!(sig(&page(20, 30, black), &page(20, 30, red)), here, "the same change elsewhere");
        assert_ne!(sig(&page(2, 2, black), &page(2, 2, blue)), here, "a button gone blue isn't one gone red");
        assert_eq!(sig(&page(2, 2, black), &page(2, 2, black)), None, "nothing changed");
    }

    #[test]
    fn a_page_s_words_against_main_s() {
        let d = super::text_diff("Shop\nCheckout\nHelp\nHelp", "Shop\nPay now\n\n  Help  ");
        assert_eq!((d.added, d.removed, d.added_n, d.removed_n), (vec!["Pay now".to_string()], vec!["Checkout".to_string(), "Help".to_string()], 1, 2));
        let many: String = (0..30).map(|i| format!("line {i}\n")).collect();
        let d = super::text_diff("", &many);
        assert_eq!((d.added.len(), d.added_n), (20, 30), "bounded, with the count");
    }
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
