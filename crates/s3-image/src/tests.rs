use super::*;
use image::{ImageEncoder, RgbImage, RgbaImage};

/// A photo-like test image: smooth gradients with some texture.
fn photo(w: u32, h: u32) -> RgbImage {
    RgbImage::from_fn(w, h, |x, y| {
        let n = ((x.wrapping_mul(7919) ^ y.wrapping_mul(104_729)) % 23) as u8;
        image::Rgb([(x * 255 / w.max(1)) as u8 ^ (n & 3), (y * 255 / h.max(1)) as u8, ((x + y) * 127 / (w + h).max(1)) as u8 + n])
    })
}

fn jpeg_of(img: &RgbImage, q: u8) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, q).write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgb8).unwrap();
    out
}

fn png_rgba(img: &RgbaImage, icc: Option<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = image::codecs::png::PngEncoder::new(&mut out);
    if let Some(p) = icc {
        enc.set_icc_profile(p).unwrap();
    }
    enc.write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgba8).unwrap();
    out
}

fn png_rgb(img: &RgbImage, icc: Option<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = image::codecs::png::PngEncoder::new(&mut out);
    if let Some(p) = icc {
        enc.set_icc_profile(p).unwrap();
    }
    enc.write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgb8).unwrap();
    out
}

fn webp_lossless(img: &RgbImage) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out).write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgb8).unwrap();
    out
}

fn gif_frames(frames: &[RgbaImage]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = image::codecs::gif::GifEncoder::new_with_speed(&mut out, 30);
        enc.encode_frames(frames.iter().map(|f| image::Frame::new(f.clone()))).unwrap();
    }
    out
}

/// Insert an EXIF APP1 segment with this orientation (1–8) after a JPEG's SOI.
fn with_orientation(jpeg: &[u8], o: u16) -> Vec<u8> {
    let mut tiff = b"II*\0\x08\0\0\0".to_vec();
    tiff.extend(1u16.to_le_bytes()); // one entry
    tiff.extend(0x0112u16.to_le_bytes()); // Orientation
    tiff.extend(3u16.to_le_bytes()); // SHORT
    tiff.extend(1u32.to_le_bytes());
    tiff.extend(o.to_le_bytes());
    tiff.extend([0, 0]);
    tiff.extend(0u32.to_le_bytes()); // no next IFD
    let mut seg = b"Exif\0\0".to_vec();
    seg.extend(tiff);
    let mut out = jpeg[..2].to_vec();
    out.extend([0xFF, 0xE1]);
    out.extend(((seg.len() + 2) as u16).to_be_bytes());
    out.extend(seg);
    out.extend(&jpeg[2..]);
    out
}

fn plan(spec: &str) -> Plan {
    Params::parse_spec(spec).unwrap().resolve(None)
}

fn run(input: &[u8], spec: &str) -> (Vec<u8>, OutputFormat, u32, u32) {
    match transform(input, &plan(spec), &Limits::default()).unwrap() {
        Rendered::Image { bytes, format, width, height } => (bytes, format, width, height),
        Rendered::Original { .. } => panic!("expected a new image for {spec}"),
    }
}

/// Decode an output with `image` (AVIF: check the container only; nothing decodes it here).
fn decoded(bytes: &[u8], f: OutputFormat) -> Option<DynamicImage> {
    let fmt = match f {
        OutputFormat::Jpeg => image::ImageFormat::Jpeg,
        OutputFormat::Png => image::ImageFormat::Png,
        OutputFormat::Webp => image::ImageFormat::WebP,
        OutputFormat::Gif => image::ImageFormat::Gif,
        OutputFormat::Avif => {
            assert_eq!(&bytes[4..12], b"ftypavif", "an AVIF container");
            return None;
        }
    };
    Some(image::load_from_memory_with_format(bytes, fmt).unwrap())
}

#[test]
fn every_input_to_every_output() {
    let img = photo(64, 48);
    let rgba = RgbaImage::from_fn(64, 48, |x, y| image::Rgba([x as u8 * 4, y as u8 * 5, 90, if x < 8 { 0 } else { 255 }]));
    let inputs: Vec<(&str, Vec<u8>, OutputFormat)> = vec![
        ("jpeg", jpeg_of(&img, 90), OutputFormat::Jpeg),
        ("png", png_rgba(&rgba, None), OutputFormat::Png),
        ("webp", webp_lossless(&img), OutputFormat::Webp),
        ("gif", gif_frames(std::slice::from_ref(&rgba)), OutputFormat::Gif),
    ];
    for (name, input, origin) in &inputs {
        for (spec, want) in [("format:origin", *origin), ("format:jpeg", OutputFormat::Jpeg), ("format:png", OutputFormat::Png), ("format:webp", OutputFormat::Webp), ("format:avif", OutputFormat::Avif)] {
            let (bytes, f, w, h) = run(input, &format!("width:32,{spec}"));
            assert_eq!((f, w, h), (want, 32, 24), "{name} {spec}");
            if f != OutputFormat::Avif {
                assert_eq!(sniff(&bytes).map(|s| s.content_type()), Some(f.content_type()), "{name} {spec}: output bytes match the content type");
            }
            if let Some(d) = decoded(&bytes, f) {
                assert_eq!((d.width(), d.height()), (32, 24), "{name} {spec}");
            }
        }
    }
}

#[test]
fn resize_modes() {
    let src = jpeg_of(&photo(400, 200), 90);
    let dims = |spec: &str| {
        let (_, _, w, h) = run(&src, spec);
        (w, h)
    };
    // One side: the other follows the aspect ratio, whatever the mode.
    assert_eq!(dims("width:100"), (100, 50));
    assert_eq!(dims("height:50"), (100, 50));
    assert_eq!(dims("width:100,resize:fill"), (100, 50));
    // Both: cover crops to the box, contain fits inside it, fill stretches to it.
    assert_eq!(dims("width:100,height:100"), (100, 100));
    assert_eq!(dims("width:100,height:100,resize:cover"), (100, 100));
    assert_eq!(dims("width:100,height:100,resize:contain"), (100, 50));
    assert_eq!(dims("width:100,height:100,resize:fill"), (100, 100));
    assert_eq!(dims("width:300,height:50,resize:contain"), (100, 50));
    // Nothing given: the source's size.
    assert_eq!(dims("format:png"), (400, 200));
    assert_eq!(dims("width:1,height:1"), (1, 1));
    assert_eq!(dims("width:1"), (1, 1), "never 0 pixels");
}

#[test]
fn cover_crops_the_centre() {
    // Left third red, middle green, right third blue: a square cover keeps the middle.
    let img = RgbImage::from_fn(300, 100, |x, _| match x {
        0..100 => image::Rgb([255, 0, 0]),
        100..200 => image::Rgb([0, 255, 0]),
        _ => image::Rgb([0, 0, 255]),
    });
    let (bytes, f, w, h) = run(&png_rgb(&img, None), "width:50,height:50,format:png");
    assert_eq!((w, h), (50, 50));
    let d = decoded(&bytes, f).unwrap().to_rgb8();
    for p in [d.get_pixel(2, 25), d.get_pixel(25, 25), d.get_pixel(47, 25)] {
        assert!(p[1] > 200 && p[0] < 40 && p[2] < 40, "only the green middle remains: {p:?}");
    }
    // Contain keeps all three.
    let (bytes, f, ..) = run(&png_rgb(&img, None), "width:60,height:60,resize:contain,format:png");
    let d = decoded(&bytes, f).unwrap().to_rgb8();
    assert_eq!(d.dimensions(), (60, 20));
    assert!(d.get_pixel(2, 10)[0] > 200 && d.get_pixel(57, 10)[2] > 200);
}

#[test]
fn never_enlarges_and_caps_the_output() {
    let small = jpeg_of(&photo(100, 80), 90);
    let dims = |src: &[u8], spec: &str| {
        let (_, _, w, h) = run(src, spec);
        (w, h)
    };
    assert_eq!(dims(&small, "width:500"), (100, 80));
    assert_eq!(dims(&small, "height:2500"), (100, 80));
    assert_eq!(dims(&small, "width:500,height:500,resize:contain"), (100, 80));
    // An over-large box shrinks uniformly to fit the source, keeping its shape.
    assert_eq!(dims(&small, "width:400,height:200"), (100, 50));
    assert_eq!(dims(&small, "width:400,height:200,resize:fill"), (100, 50));
    // A long thin source: no side over 2500, even when only the other one was asked for.
    let long = png_rgb(&RgbImage::from_pixel(3000, 100, image::Rgb([9, 9, 9])), None);
    assert_eq!(dims(&long, "format:png"), (2500, 83));
    assert_eq!(dims(&long, "height:100"), (2500, 83));
    let tall = png_rgb(&RgbImage::from_pixel(10, 5000, image::Rgb([9, 9, 9])), None);
    assert_eq!(dims(&tall, "width:10"), (5, 2500));
}

#[test]
fn quality_changes_size() {
    let src = jpeg_of(&photo(300, 300), 95);
    let size = |spec: &str| run(&src, spec).0.len();
    for f in ["jpeg", "webp", "avif"] {
        let (lo, hi) = (size(&format!("format:{f},quality:20")), size(&format!("format:{f},quality:100")));
        assert!(lo < hi, "{f}: quality 20 ({lo} bytes) is smaller than 100 ({hi})");
    }
    assert_eq!(size("format:jpeg"), size("format:jpeg,quality:80"), "80 is the default");
    assert_eq!(size("format:png,quality:20"), size("format:png,quality:100"), "PNG ignores quality");
}

#[test]
fn applies_exif_orientation() {
    // Left half red, right half blue, 40×20 as stored.
    let img = RgbImage::from_fn(40, 20, |x, _| if x < 20 { image::Rgb([255, 0, 0]) } else { image::Rgb([0, 0, 255]) });
    let jpeg = jpeg_of(&img, 95);
    let red = |p: &image::Rgb<u8>| p[0] > 200 && p[2] < 60;
    let blue = |p: &image::Rgb<u8>| p[2] > 200 && p[0] < 60;
    // 6 = rotate 90° clockwise to display: the left half ends on top.
    let (bytes, f, w, h) = run(&with_orientation(&jpeg, 6), "format:png");
    assert_eq!((w, h), (20, 40));
    let d = decoded(&bytes, f).unwrap().to_rgb8();
    assert!(red(d.get_pixel(10, 3)) && blue(d.get_pixel(10, 36)), "rotated: red on top");
    // 8 = 90° counter-clockwise: red at the bottom.
    let d = decoded(&run(&with_orientation(&jpeg, 8), "format:png").0, OutputFormat::Png).unwrap().to_rgb8();
    assert!(blue(d.get_pixel(10, 3)) && red(d.get_pixel(10, 36)));
    // 3 = 180°; 2 = mirrored.
    for o in [2, 3] {
        let d = decoded(&run(&with_orientation(&jpeg, o), "format:png").0, OutputFormat::Png).unwrap().to_rgb8();
        assert_eq!(d.dimensions(), (40, 20));
        assert!(blue(d.get_pixel(3, 10)) && red(d.get_pixel(36, 10)), "orientation {o}");
    }
    // Requested sides apply to the upright image: width 10 of a 20×40 portrait is 10×20.
    let (_, _, w, h) = run(&with_orientation(&jpeg, 6), "width:10");
    assert_eq!((w, h), (10, 20));
    let (_, _, w, h) = run(&with_orientation(&jpeg, 6), "width:10,height:30");
    assert_eq!((w, h), (10, 30));
    // The output carries no EXIF (so viewers don't rotate it again).
    let out = run(&with_orientation(&jpeg, 6), "format:jpeg").0;
    assert!(!out.windows(6).any(|w| w == b"Exif\0\0"));
}

#[test]
fn converts_icc_profiles_to_srgb_and_strips_them() {
    let colour = image::Rgb([200u8, 100, 50]);
    let img = RgbImage::from_pixel(16, 16, colour);
    let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    let srgb = moxcms::ColorProfile::new_srgb().encode().unwrap();
    let px = |input: Vec<u8>| {
        let (bytes, f, ..) = run(&input, "format:png");
        assert!(!bytes.windows(4).any(|w| w == b"iCCP"), "no profile in the output");
        *decoded(&bytes, f).unwrap().to_rgb8().get_pixel(8, 8)
    };
    let untagged = px(png_rgb(&img, None));
    assert_eq!(untagged, colour);
    let tagged_srgb = px(png_rgb(&img, Some(srgb)));
    assert!(tagged_srgb.0.iter().zip(colour.0).all(|(a, b)| a.abs_diff(b) <= 2), "sRGB stays: {tagged_srgb:?}");
    let from_p3 = px(png_rgb(&img, Some(p3)));
    // P3's (200, 100, 50) is a more saturated orange: more red, less green and blue in sRGB.
    assert!(from_p3[0] > colour[0] + 5 && from_p3[1] < colour[1] && from_p3[2] < colour[2], "P3 converted: {from_p3:?}");
    // A profile that doesn't parse is ignored.
    assert_eq!(px(png_rgb(&img, Some(b"not a profile at all, really".to_vec()))), colour);
}

#[test]
fn alpha_is_kept_or_flattened() {
    let img = RgbaImage::from_fn(20, 20, |x, _| if x < 10 { image::Rgba([0, 0, 0, 0]) } else { image::Rgba([0, 0, 255, 255]) });
    let src = png_rgba(&img, None);
    for f in ["png", "webp"] {
        let (bytes, of, ..) = run(&src, &format!("format:{f}"));
        let d = decoded(&bytes, of).unwrap();
        assert!(d.color().has_alpha(), "{f} keeps alpha");
        assert_eq!(d.to_rgba8().get_pixel(2, 10)[3], 0, "{f}: transparent stays transparent");
    }
    // JPEG has no alpha: composited onto white.
    let (bytes, of, ..) = run(&src, "format:jpeg");
    let d = decoded(&bytes, of).unwrap().to_rgb8();
    assert!(d.get_pixel(2, 10).0.iter().all(|c| *c > 240), "white behind: {:?}", d.get_pixel(2, 10));
    // An alpha channel with nothing transparent is dropped.
    let opaque = png_rgba(&RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255])), None);
    let (bytes, of, ..) = run(&opaque, "format:png");
    assert!(!decoded(&bytes, of).unwrap().color().has_alpha());
}

#[test]
fn animations_pass_through_unless_converted() {
    let a = RgbaImage::from_pixel(30, 20, image::Rgba([255, 0, 0, 255]));
    let b = RgbaImage::from_pixel(30, 20, image::Rgba([0, 0, 255, 255]));
    let anim = gif_frames(&[a.clone(), b]);
    assert!(is_animated(&anim, InputFormat::Gif));
    assert!(!is_animated(&gif_frames(&[a]), InputFormat::Gif));
    let limits = Limits::default();
    for spec in ["width:10", "format:origin,width:10", "width:10,height:10,resize:fill"] {
        assert_eq!(transform(&anim, &plan(spec), &limits).unwrap(), Rendered::Original { format: InputFormat::Gif }, "{spec}");
    }
    // A format picked from Accept keeps the animation.
    let negotiated = Params::parse_spec("width:10").unwrap().resolve(Some("image/webp"));
    assert_eq!(negotiated.format, Format::Webp);
    assert_eq!(transform(&anim, &negotiated, &limits).unwrap(), Rendered::Original { format: InputFormat::Gif });
    // An explicit conversion makes a still of the first frame.
    let (bytes, f, w, h) = run(&anim, "format:png,width:15");
    assert_eq!((f, w, h), (OutputFormat::Png, 15, 10));
    let d = decoded(&bytes, f).unwrap().to_rgb8();
    assert!(d.get_pixel(7, 5)[0] > 200, "first frame (red)");
    // An animated WebP header (VP8X with the animation flag).
    let mut webp = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0".to_vec();
    webp.extend([0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert!(is_animated(&webp, InputFormat::Webp));
    assert_eq!(transform(&webp, &plan("width:5"), &limits).unwrap(), Rendered::Original { format: InputFormat::Webp });
}

#[test]
fn refuses_what_it_cannot_decode() {
    let limits = Limits::default();
    let p = plan("width:10");
    for (input, what) in [
        (b"\0\0\0\x1cftypavif\0\0\0\0avifmif1miaf".to_vec(), "AVIF"),
        (b"\0\0\0\x18ftypheic\0\0\0\0mif1heic".to_vec(), "HEIC"),
        (b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(), "SVG"),
        (b"BM\0\0\0\0".to_vec(), "BMP"),
        (Vec::new(), "nothing"),
    ] {
        assert!(matches!(transform(&input, &p, &limits), Err(ImageError::Unsupported(_))), "{what}");
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

/// Stored (uncompressed) zlib: enough for test PNGs without a deflate dependency.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = data.chunks(65_535).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        out.push((i + 1 == chunks.len()) as u8);
        out.extend((chunk.len() as u16).to_le_bytes());
        out.extend((!(chunk.len() as u16)).to_le_bytes());
        out.extend(*chunk);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    out.extend(((b << 16) | a).to_be_bytes());
    out
}

/// A PNG whose header declares `w` × `h` (8-bit RGBA) with `idat` as its compressed data.
fn png_claiming(w: u32, h: u32, idat: &[u8]) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend((data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend(data);
        out.extend(&body);
        out.extend(crc32(&body).to_be_bytes());
    };
    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", idat);
    chunk(b"IEND", &[]);
    out
}

#[test]
fn limits_are_checked_before_decoding() {
    let p = plan("width:10");
    let limits = Limits::default();
    let empty = zlib_stored(&[]);
    // Header dimensions over the pixel cap: refused before any pixel buffer exists.
    for (w, h) in [(100_000, 100_000), (u32::MAX >> 1, 2), (7072, 7072)] {
        assert!(matches!(transform(&png_claiming(w, h, &empty), &p, &limits), Err(ImageError::TooLarge(_))), "{w}×{h}");
    }
    // A GIF claiming a 65535×65535 screen.
    let mut gif = b"GIF89a".to_vec();
    gif.extend([0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0x3B]);
    assert!(matches!(transform(&gif, &p, &limits), Err(ImageError::TooLarge(_) | ImageError::Corrupt(_))));
    // A JPEG claiming 65500×65500 (SOF patched).
    let mut jpeg = jpeg_of(&photo(16, 16), 80);
    let sof = jpeg.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
    jpeg[sof + 5..sof + 9].copy_from_slice(&[0xFF, 0xDC, 0xFF, 0xDC]);
    assert!(matches!(transform(&jpeg, &p, &limits), Err(ImageError::TooLarge(_))));
    // Input bytes over the cap: refused unread.
    let small = Limits { max_input_bytes: 1000, ..Limits::default() };
    assert!(matches!(transform(&jpeg_of(&photo(200, 200), 95), &p, &small), Err(ImageError::TooLarge(_))));
    // A lower pixel cap.
    let tiny = Limits { max_pixels: 100, ..Limits::default() };
    assert!(matches!(transform(&jpeg_of(&photo(11, 10), 95), &p, &tiny), Err(ImageError::TooLarge(_))));
    assert!(transform(&jpeg_of(&photo(10, 10), 95), &p, &tiny).is_ok());
}

#[test]
fn decompression_bombs_are_refused_cheaply() {
    // 20000×20000 RGBA (1.6 GB decoded) from a small stream: refused by its header, fast.
    let rows: Vec<u8> = vec![0u8; (1 + 20_000 * 4) * 8];
    let bomb = png_claiming(20_000, 20_000, &zlib_stored(&rows));
    let started = Instant::now();
    assert!(matches!(transform(&bomb, &plan("width:10"), &Limits::default()), Err(ImageError::TooLarge(_))));
    assert!(started.elapsed() < Duration::from_secs(1));
    // Just under the pixel cap but a short stream: a truncation error, not a hang or a panic.
    let near = png_claiming(7000, 7000, &zlib_stored(&rows[..1000]));
    assert!(matches!(transform(&near, &plan("width:10"), &Limits::default()), Err(ImageError::Corrupt(_))));
}

/// xorshift: deterministic "random" corruption.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn truncated_and_corrupted_inputs_never_panic() {
    let img = photo(48, 32);
    let rgba = RgbaImage::from_fn(48, 32, |x, y| image::Rgba([x as u8, y as u8, 7, (x * 5) as u8]));
    let inputs = [
        jpeg_of(&img, 85),
        with_orientation(&jpeg_of(&img, 85), 6),
        png_rgba(&rgba, Some(moxcms::ColorProfile::new_display_p3().encode().unwrap())),
        webp_lossless(&img),
        gif_frames(&[rgba.clone(), rgba]),
        run(&jpeg_of(&img, 85), "format:webp").0,
    ];
    let p = plan("width:20,format:png");
    let limits = Limits::default();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut outcomes = [0usize; 2];
    for input in &inputs {
        // Every truncation point in the first 512 bytes, then a sample.
        let cuts: Vec<usize> = (0..input.len().min(512)).chain((512..input.len()).step_by(37)).collect();
        for n in cuts {
            outcomes[transform(&input[..n], &p, &limits).is_ok() as usize] += 1;
        }
        // Random byte changes and insertions (length fields included).
        for _ in 0..300 {
            let mut b = input.clone();
            for _ in 0..(1 + rng.next() % 8) {
                let i = (rng.next() as usize) % b.len();
                match rng.next() % 3 {
                    0 => b[i] = rng.next() as u8,
                    1 => b[i] ^= 0xFF,
                    _ => b.insert(i, 0xFF),
                }
            }
            outcomes[transform(&b, &p, &limits).is_ok() as usize] += 1;
        }
    }
    assert!(outcomes[0] > 0, "corrupt inputs fail: {outcomes:?}");
}

#[test]
fn output_is_deterministic() {
    let src = jpeg_of(&photo(200, 150), 90);
    let rgba = png_rgba(&RgbaImage::from_fn(60, 40, |x, y| image::Rgba([x as u8 * 3, y as u8 * 4, 10, (x * 4) as u8])), None);
    for input in [&src, &rgba] {
        for spec in ["width:120,format:jpeg", "width:120,format:png", "width:120,format:webp", "width:120,format:avif", "width:50,height:50", "quality:33,format:webp"] {
            let a = run(input, spec).0;
            let b = run(input, spec).0;
            assert_eq!(a, b, "{spec}");
        }
    }
}

#[test]
fn stops_at_the_deadline() {
    let src = jpeg_of(&photo(300, 300), 90);
    let limits = Limits { timeout: Duration::ZERO, ..Limits::default() };
    assert_eq!(transform(&src, &plan("width:100"), &limits), Err(ImageError::Timeout));
}

#[test]
fn geometry_edge_cases() {
    let g = |sw, sh, w, h, m| {
        let g = geometry(sw, sh, w, h, m, MAX_SIDE);
        (g.width, g.height)
    };
    assert_eq!(g(1, 1, Some(2500), Some(2500), Resize::Cover), (1, 1));
    assert_eq!(g(10_000, 1, None, None, Resize::Cover), (2500, 1));
    assert_eq!(g(1, 10_000, Some(1), None, Resize::Cover), (1, 2500), "a 1-pixel width can still be too tall");
    assert_eq!(g(5000, 5000, Some(2500), Some(10), Resize::Fill), (2500, 10));
    assert_eq!(g(5000, 5000, Some(2500), Some(10), Resize::Contain), (10, 10));
    let crop = geometry(400, 100, Some(50), Some(50), Resize::Cover, MAX_SIDE).crop.unwrap();
    assert_eq!(crop, (150.0, 0.0, 100.0, 100.0));
}
