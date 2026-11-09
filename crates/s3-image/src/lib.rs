//! roll storage image transformations, with no I/O: decode, orient, colour-manage, resize and
//! encode an image held in memory. `s3-server` reads the source, runs [`transform`] on a bounded
//! blocking pool and caches the output as a derived blob (`s3-store`'s `derived`).
//!
//! - **Inputs**: JPEG (`zune-jpeg`), PNG, GIF (the first frame) and WebP (`image-webp`), all pure
//!   Rust through `image`. Recognised but refused: AVIF (the only pure-Rust AV1 decoder, `rav1d`,
//!   needs `nasm` on x86 through `avif-decode`, which also can't bound the frame size an AV1
//!   sequence header declares before allocating) and HEIC (HEVC: no pure-Rust decoder exists).
//!   Anything else (SVG, TIFF, BMP, …) is refused too. The format comes from the bytes, never
//!   from the object's `Content-Type`.
//! - **Animated** GIF, WebP and PNG (APNG) pass through untouched ([`Rendered::Original`]) unless
//!   another format is asked for, which converts the first frame.
//! - **Operations**: Supabase's `width`, `height`, `resize` (cover, contain, fill), `quality` and
//!   `format` ([`Params`], resolved into a [`Plan`]). Images are never enlarged, and no output
//!   side exceeds [`MAX_SIDE`].
//! - **Orientation**: EXIF orientation is applied (the output is upright and carries none).
//! - **Colour**: an embedded RGB ICC profile (Display P3, Adobe RGB, …) is converted to sRGB
//!   (`moxcms`); outputs are sRGB without a profile. All metadata (EXIF, XMP, ICC) is dropped.
//! - **Outputs**: JPEG (`jpeg-encoder`, pure Rust, SIMD), PNG (`image`), WebP (libwebp, lossy),
//!   AVIF (`ravif`/rav1e, pure Rust), GIF (for static GIF sources kept as GIF). Deterministic:
//!   every encoder runs single-threaded with fixed settings, so the same input and plan give the
//!   same bytes ([`ENGINE`] versions that promise).
//! - **Safety**: inputs over [`Limits::max_input_bytes`] are refused unread; dimensions come
//!   from the header and are checked against [`Limits::max_pixels`] before any pixel buffer
//!   exists, and decoders get an allocation cap; work stops at the deadline between stages.

mod params;
mod sniff;
mod webp;

pub use params::{DEFAULT_QUALITY, ENGINE, Format, MAX_QUALITY, MAX_SIDE, MIN_QUALITY, Params, Plan, Resize, accepts_webp};
pub use sniff::{InputFormat, is_animated, sniff};

use fast_image_resize as fr;
use image::{DynamicImage, ImageDecoder, metadata::Orientation};
use std::fmt;
use std::io::Cursor;
use std::time::{Duration, Instant};

/// Why a transformation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// The request's parameters are invalid (a client error).
    InvalidParams(String),
    /// The input is over a limit (bytes or pixels).
    TooLarge(String),
    /// Not an image roll can transform (unknown format, AVIF, HEIC).
    Unsupported(String),
    /// The image is corrupt or truncated.
    Corrupt(String),
    /// The work ran past its deadline.
    Timeout,
    /// An encoder failed.
    Internal(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::InvalidParams(m) | ImageError::TooLarge(m) | ImageError::Unsupported(m) | ImageError::Corrupt(m) | ImageError::Internal(m) => f.write_str(m),
            ImageError::Timeout => f.write_str("the image took too long to transform"),
        }
    }
}

impl std::error::Error for ImageError {}

/// Resource limits of one transformation.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Inputs larger than this are refused (25 MiB, Supabase's limit).
    pub max_input_bytes: usize,
    /// Inputs with more pixels than this (by their header) are refused before decoding.
    pub max_pixels: u64,
    /// Largest side of an output (2500, Supabase's).
    pub max_side: u32,
    /// Cap on any one allocation a decoder makes.
    pub max_alloc: u64,
    /// The work stops (between stages) after this long.
    pub timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_input_bytes: 25 << 20, max_pixels: 50_000_000, max_side: MAX_SIDE, max_alloc: 512 << 20, timeout: Duration::from_secs(20) }
    }
}

/// An output format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OutputFormat {
    Jpeg,
    Png,
    Webp,
    Avif,
    Gif,
}

impl OutputFormat {
    pub fn content_type(self) -> &'static str {
        match self {
            OutputFormat::Jpeg => "image/jpeg",
            OutputFormat::Png => "image/png",
            OutputFormat::Webp => "image/webp",
            OutputFormat::Avif => "image/avif",
            OutputFormat::Gif => "image/gif",
        }
    }
}

/// What [`transform`] made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rendered {
    /// A new image.
    Image { bytes: Vec<u8>, format: OutputFormat, width: u32, height: u32 },
    /// Serve the input unchanged (an animated image without a format conversion).
    Original { format: InputFormat },
}

/// Transform `input` as `plan` says, within `limits`.
pub fn transform(input: &[u8], plan: &Plan, limits: &Limits) -> Result<Rendered, ImageError> {
    let deadline = Instant::now() + limits.timeout;
    let check = || if Instant::now() > deadline { Err(ImageError::Timeout) } else { Ok(()) };
    if input.len() > limits.max_input_bytes {
        return Err(ImageError::TooLarge(format!("images over {} MiB can't be transformed", limits.max_input_bytes >> 20)));
    }
    let format = sniff(input).ok_or_else(|| ImageError::Unsupported("not a JPEG, PNG, WebP or GIF image".into()))?;
    match format {
        InputFormat::Avif => return Err(ImageError::Unsupported("AVIF images can't be transformed yet (they can be produced: format=avif)".into())),
        InputFormat::Heic => return Err(ImageError::Unsupported("HEIC/HEIF images can't be transformed".into())),
        _ => {}
    }
    // Animations stay animations unless a conversion was asked for explicitly (a format picked
    // from `Accept` doesn't count: a browser that takes WebP still wants the GIF to move).
    let keep = plan.negotiated || matches!((plan.format, format), (Format::Origin, _) | (Format::Webp, InputFormat::Webp) | (Format::Png, InputFormat::Png));
    if keep && is_animated(input, format) {
        return Ok(Rendered::Original { format });
    }
    let decoded = decode(input, format, limits)?;
    check()?;
    let mut px = Pixels::from(decoded.image);
    if let Some(icc) = &decoded.icc {
        px.convert_to_srgb(icc);
    }
    px.drop_opaque_alpha();
    check()?;

    let transposed = matches!(decoded.orientation, Orientation::Rotate90 | Orientation::Rotate270 | Orientation::Rotate90FlipH | Orientation::Rotate270FlipH);
    // Geometry is symmetric under the orientation's transpose (crops are centred), so it's worked
    // out on the stored pixels with the requested sides swapped, and the orientation applied to
    // the (small) result.
    let (rw, rh) = if transposed { (plan.height, plan.width) } else { (plan.width, plan.height) };
    let g = geometry(px.width, px.height, rw, rh, plan.resize, limits.max_side);
    let mut px = px.resize(&g)?;
    check()?;
    px.orient(decoded.orientation);

    let out = match plan.format {
        Format::Jpeg => OutputFormat::Jpeg,
        Format::Png => OutputFormat::Png,
        Format::Webp => OutputFormat::Webp,
        Format::Avif => OutputFormat::Avif,
        Format::Origin | Format::Auto => match format {
            InputFormat::Png => OutputFormat::Png,
            InputFormat::Gif => OutputFormat::Gif,
            InputFormat::Webp => OutputFormat::Webp,
            _ => OutputFormat::Jpeg,
        },
    };
    let bytes = encode(&mut px, out, plan.quality)?;
    Ok(Rendered::Image { bytes, format: out, width: px.width, height: px.height })
}

struct Decoded {
    image: DynamicImage,
    orientation: Orientation,
    icc: Option<Vec<u8>>,
}

fn corrupt(e: image::ImageError) -> ImageError {
    match e {
        image::ImageError::Limits(l) => ImageError::TooLarge(format!("the image is too large to transform: {l}")),
        image::ImageError::Unsupported(u) => ImageError::Unsupported(format!("unsupported image: {u}")),
        e => ImageError::Corrupt(format!("the image is corrupt or truncated: {e}")),
    }
}

fn decode(input: &[u8], format: InputFormat, limits: &Limits) -> Result<Decoded, ImageError> {
    use image::codecs::{gif::GifDecoder, jpeg::JpegDecoder, png::PngDecoder, webp::WebPDecoder};
    let r = Cursor::new(input);
    match format {
        InputFormat::Jpeg => run(JpegDecoder::new(r).map_err(corrupt)?, limits),
        InputFormat::Png => run(PngDecoder::with_limits(r, decoder_limits(limits)).map_err(corrupt)?, limits),
        InputFormat::Gif => run(GifDecoder::new(r).map_err(corrupt)?, limits),
        InputFormat::Webp => run(WebPDecoder::new(r).map_err(corrupt)?, limits),
        InputFormat::Avif | InputFormat::Heic => Err(ImageError::Unsupported("unsupported image format".into())),
    }
}

fn decoder_limits(limits: &Limits) -> image::Limits {
    let mut l = image::Limits::default();
    l.max_alloc = Some(limits.max_alloc);
    l
}

/// Check the header's dimensions, then decode (the decoder allocates from the same header).
fn run(mut d: impl ImageDecoder, limits: &Limits) -> Result<Decoded, ImageError> {
    let (w, h) = d.dimensions();
    if w == 0 || h == 0 {
        return Err(ImageError::Corrupt("the image has no pixels".into()));
    }
    let pixels = w as u64 * h as u64;
    if pixels > limits.max_pixels {
        return Err(ImageError::TooLarge(format!("images over {} megapixels can't be transformed ({w}×{h})", limits.max_pixels / 1_000_000)));
    }
    let mut l = decoder_limits(limits);
    l.max_image_width = Some(w);
    l.max_image_height = Some(h);
    d.set_limits(l).map_err(corrupt)?;
    // Metadata is best effort: a broken EXIF or ICC block doesn't fail the image.
    let orientation = d.orientation().unwrap_or(Orientation::NoTransforms);
    let icc = d.icc_profile().ok().flatten();
    let image = DynamicImage::from_decoder(d).map_err(corrupt)?;
    Ok(Decoded { image, orientation, icc })
}

/// 8-bit RGB or RGBA pixels.
struct Pixels {
    data: Vec<u8>,
    width: u32,
    height: u32,
    /// 3 or 4.
    channels: usize,
}

impl From<DynamicImage> for Pixels {
    fn from(img: DynamicImage) -> Self {
        let (width, height) = (img.width(), img.height());
        match img {
            DynamicImage::ImageRgb8(b) => Pixels { data: b.into_raw(), width, height, channels: 3 },
            DynamicImage::ImageRgba8(b) => Pixels { data: b.into_raw(), width, height, channels: 4 },
            img if img.color().has_alpha() => Pixels { data: img.into_rgba8().into_raw(), width, height, channels: 4 },
            img => Pixels { data: img.into_rgb8().into_raw(), width, height, channels: 3 },
        }
    }
}

/// How to get from the source to the output: an optional centred crop of the source, scaled to
/// `width` × `height`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Geometry {
    width: u32,
    height: u32,
    /// left, top, width, height in source pixels.
    crop: Option<(f64, f64, f64, f64)>,
}

/// The output size for a `sw` × `sh` source: never larger than the source (no upscaling: an
/// over-large box shrinks uniformly to fit it), and no side over `max_side`.
fn geometry(sw: u32, sh: u32, rw: Option<u32>, rh: Option<u32>, mode: Resize, max_side: u32) -> Geometry {
    let (sw_f, sh_f) = (sw as f64, sh as f64);
    let (mut w, mut h, crop) = match (rw, rh) {
        (None, None) => (sw_f, sh_f, None),
        (Some(w), None) => {
            let w = (w as f64).min(sw_f);
            (w, sh_f * w / sw_f, None)
        }
        (None, Some(h)) => {
            let h = (h as f64).min(sh_f);
            (sw_f * h / sh_f, h, None)
        }
        (Some(w), Some(h)) => {
            let (w, h) = (w as f64, h as f64);
            match mode {
                Resize::Contain => {
                    let s = (w / sw_f).min(h / sh_f).min(1.0);
                    (sw_f * s, sh_f * s, None)
                }
                Resize::Fill | Resize::Cover => {
                    let f = (w / sw_f).max(h / sh_f);
                    let (w, h) = if f > 1.0 { (w / f, h / f) } else { (w, h) };
                    if mode == Resize::Fill {
                        (w, h, None)
                    } else {
                        let (cw, ch) = if sw_f / sh_f > w / h { (sh_f * w / h, sh_f) } else { (sw_f, sw_f * h / w) };
                        (w, h, Some(((sw_f - cw) / 2.0, (sh_f - ch) / 2.0, cw, ch)))
                    }
                }
            }
        }
    };
    let m = w.max(h);
    if m > max_side as f64 {
        let s = max_side as f64 / m;
        w *= s;
        h *= s;
    }
    let side = |v: f64| (v.round() as u32).clamp(1, max_side.max(1));
    Geometry { width: side(w), height: side(h), crop }
}

impl Pixels {
    /// Convert from the embedded ICC profile to sRGB. A profile that doesn't parse, isn't RGB,
    /// or can't be applied leaves the pixels as they are (what browsers do with broken ones).
    fn convert_to_srgb(&mut self, icc: &[u8]) {
        use moxcms::{ColorProfile, DataColorSpace, Layout, TransformOptions};
        let Ok(src) = ColorProfile::new_from_slice(icc) else { return };
        if src.color_space != DataColorSpace::Rgb {
            return;
        }
        let layout = if self.channels == 4 { Layout::Rgba } else { Layout::Rgb };
        let Ok(t) = src.create_transform_8bit(layout, &ColorProfile::new_srgb(), layout, TransformOptions::default()) else { return };
        let mut out = vec![0u8; self.data.len()];
        if t.transform(&self.data, &mut out).is_ok() {
            self.data = out;
        }
    }

    /// RGBA whose every pixel is opaque becomes RGB (smaller to encode, and JPEG-able).
    fn drop_opaque_alpha(&mut self) {
        if self.channels == 4 && self.data.as_chunks::<4>().0.iter().all(|p| p[3] == 255) {
            self.data = self.data.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
            self.channels = 3;
        }
    }

    /// Composite onto white and drop alpha (for JPEG).
    fn flatten(&mut self) {
        if self.channels == 4 {
            self.data = self
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| {
                    let a = p[3] as u32;
                    let mix = |c: u8| ((c as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
                    [mix(p[0]), mix(p[1]), mix(p[2])]
                })
                .collect();
            self.channels = 3;
        }
    }

    fn resize(self, g: &Geometry) -> Result<Pixels, ImageError> {
        if g.crop.is_none() && g.width == self.width && g.height == self.height {
            return Ok(self);
        }
        let pt = if self.channels == 4 { fr::PixelType::U8x4 } else { fr::PixelType::U8x3 };
        let src = fr::images::ImageRef::new(self.width, self.height, &self.data, pt).map_err(|e| ImageError::Internal(e.to_string()))?;
        let mut dst = fr::images::Image::new(g.width, g.height, pt);
        let mut opts = fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3));
        if let Some((l, t, w, h)) = g.crop {
            opts = opts.crop(l, t, w, h);
        }
        fr::Resizer::new().resize(&src, &mut dst, &opts).map_err(|e| ImageError::Internal(e.to_string()))?;
        Ok(Pixels { data: dst.into_vec(), width: g.width, height: g.height, channels: self.channels })
    }

    fn orient(&mut self, o: Orientation) {
        if o == Orientation::NoTransforms {
            return;
        }
        let data = std::mem::take(&mut self.data);
        let mut img = if self.channels == 4 {
            DynamicImage::ImageRgba8(image::RgbaImage::from_raw(self.width, self.height, data).expect("sized buffer"))
        } else {
            DynamicImage::ImageRgb8(image::RgbImage::from_raw(self.width, self.height, data).expect("sized buffer"))
        };
        img.apply_orientation(o);
        self.width = img.width();
        self.height = img.height();
        self.data = img.into_bytes();
    }
}

/// AVIF encoder speed (rav1e, 1 slowest … 10 fastest). On-the-fly encoding favours speed: AV1 is
/// still the slowest output by far (about 1–3 CPU-seconds for a 1200-pixel-wide image at 10,
/// roughly a third more at 9, for files a few percent smaller; see `docs/storage/pricing.md`).
const AVIF_SPEED: u8 = 10;

fn encode(px: &mut Pixels, out: OutputFormat, quality: u8) -> Result<Vec<u8>, ImageError> {
    let (w, h) = (px.width, px.height);
    let internal = |what: &str, e: &dyn fmt::Display| ImageError::Internal(format!("{what} encoding failed: {e}"));
    match out {
        OutputFormat::Jpeg => {
            px.flatten();
            let mut buf = Vec::new();
            let mut enc = jpeg_encoder::Encoder::new(&mut buf, quality);
            // Full chroma at high qualities (text and sharp edges), 4:2:0 below (photos).
            enc.set_sampling_factor(if quality >= 90 { jpeg_encoder::SamplingFactor::R_4_4_4 } else { jpeg_encoder::SamplingFactor::R_4_2_0 });
            enc.set_progressive(true);
            enc.encode(&px.data, w as u16, h as u16, jpeg_encoder::ColorType::Rgb).map_err(|e| internal("JPEG", &e))?;
            Ok(buf)
        }
        OutputFormat::Png => {
            use image::ImageEncoder;
            use image::codecs::png::{CompressionType, FilterType, PngEncoder};
            let mut buf = Vec::new();
            let color = if px.channels == 4 { image::ExtendedColorType::Rgba8 } else { image::ExtendedColorType::Rgb8 };
            PngEncoder::new_with_quality(&mut buf, CompressionType::Fast, FilterType::Adaptive).write_image(&px.data, w, h, color).map_err(|e| internal("PNG", &e))?;
            Ok(buf)
        }
        OutputFormat::Gif => {
            let mut buf = Vec::new();
            {
                let mut enc = image::codecs::gif::GifEncoder::new_with_speed(&mut buf, 10);
                let color = if px.channels == 4 { image::ExtendedColorType::Rgba8 } else { image::ExtendedColorType::Rgb8 };
                enc.encode(&px.data, w, h, color).map_err(|e| internal("GIF", &e))?;
            }
            Ok(buf)
        }
        OutputFormat::Webp => webp::encode(&px.data, w, h, px.channels, quality),
        OutputFormat::Avif => {
            use rgb::FromSlice;
            let enc = ravif::Encoder::new().with_quality(quality as f32).with_alpha_quality(quality as f32).with_speed(AVIF_SPEED).with_num_threads(Some(1));
            let r = if px.channels == 4 {
                enc.encode_rgba(imgref::Img::new(px.data.as_rgba(), w as usize, h as usize))
            } else {
                enc.encode_rgb(imgref::Img::new(px.data.as_rgb(), w as usize, h as usize))
            };
            Ok(r.map_err(|e| internal("AVIF", &e))?.avif_file)
        }
    }
}

#[cfg(test)]
mod tests;
