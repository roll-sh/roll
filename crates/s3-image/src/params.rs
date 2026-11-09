//! Transformation parameters: Supabase's options (`width`, `height`, `resize`, `quality`,
//! `format`), parsed from a query string or roll's `x-roll-transform` spec, then resolved against
//! the request's `Accept` header into a [`Plan`] whose canonical form keys the derived-image
//! cache.

use crate::ImageError;
use sha2::{Digest, Sha256};

/// Bumped whenever the same input and plan could produce different bytes (an encoder upgrade,
/// a changed default), so cached images made by an older engine are never served as this one's.
pub const ENGINE: &str = "img1";

/// Largest `width` or `height` a request may ask for (Supabase's limit), and the largest side of
/// any output.
pub const MAX_SIDE: u32 = 2500;
pub const MIN_QUALITY: u8 = 20;
pub const MAX_QUALITY: u8 = 100;
pub const DEFAULT_QUALITY: u8 = 80;

/// How the image fills a `width` × `height` box (only matters when both are given).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Resize {
    /// Scale to cover the box, then crop the overflow, centred (Supabase's default).
    #[default]
    Cover,
    /// Scale to fit inside the box, keeping the aspect ratio.
    Contain,
    /// Stretch to the box exactly.
    Fill,
}

impl Resize {
    pub fn as_str(self) -> &'static str {
        match self {
            Resize::Cover => "cover",
            Resize::Contain => "contain",
            Resize::Fill => "fill",
        }
    }
}

/// The output format a request asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Format {
    /// No `format`: WebP when the client accepts it, else the source's format (Supabase's
    /// behaviour).
    #[default]
    Auto,
    /// The source's format.
    Origin,
    Jpeg,
    Png,
    Webp,
    Avif,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Auto => "auto",
            Format::Origin => "origin",
            Format::Jpeg => "jpeg",
            Format::Png => "png",
            Format::Webp => "webp",
            Format::Avif => "avif",
        }
    }
}

/// What a request asked for, validated but not yet resolved against `Accept`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Params {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub resize: Resize,
    pub quality: Option<u8>,
    pub format: Format,
}

fn bad(msg: impl Into<String>) -> ImageError {
    ImageError::InvalidParams(msg.into())
}

impl Params {
    /// roll's `x-roll-transform` spec: comma-separated `name:value` pairs, e.g.
    /// `width:300,height:200,resize:contain,quality:70,format:webp`. Unknown names are errors.
    pub fn parse_spec(spec: &str) -> Result<Self, ImageError> {
        let mut p = Params::default();
        let mut seen = Vec::new();
        for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (k, v) = part.split_once(':').ok_or_else(|| bad(format!("{part:?} isn't name:value")))?;
            let k = k.trim();
            if seen.contains(&k) {
                return Err(bad(format!("{k} is given twice")));
            }
            seen.push(k);
            if !p.set(k, v.trim())? {
                return Err(bad(format!("unknown transformation {k:?} (width, height, resize, quality, format)")));
            }
        }
        Ok(p)
    }

    /// Supabase's query parameters (`?width=300&resize=contain`); other names are ignored (a
    /// signed URL's `token`, cache busters). A name given twice is an error.
    pub fn from_query<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<Self, ImageError> {
        let mut p = Params::default();
        let mut seen = Vec::new();
        for (k, v) in pairs {
            if matches!(k, "width" | "height" | "resize" | "quality" | "format") {
                if seen.contains(&k) {
                    return Err(bad(format!("{k} is given twice")));
                }
                seen.push(k);
                p.set(k, v)?;
            }
        }
        Ok(p)
    }

    /// Set one option; `false` if the name isn't one.
    fn set(&mut self, k: &str, v: &str) -> Result<bool, ImageError> {
        let dim = |v: &str| -> Result<Option<u32>, ImageError> {
            // Supabase treats 0 (and an empty value) as "not given".
            if v.is_empty() {
                return Ok(None);
            }
            let n: u32 = v.parse().map_err(|_| bad(format!("{k} must be a whole number of pixels, 1 to {MAX_SIDE}")))?;
            match n {
                0 => Ok(None),
                1..=MAX_SIDE => Ok(Some(n)),
                _ => Err(bad(format!("{k} must be at most {MAX_SIDE}"))),
            }
        };
        match k {
            "width" => self.width = dim(v)?,
            "height" => self.height = dim(v)?,
            "resize" => {
                self.resize = match v.to_ascii_lowercase().as_str() {
                    "cover" => Resize::Cover,
                    "contain" => Resize::Contain,
                    "fill" => Resize::Fill,
                    _ => return Err(bad("resize is cover, contain or fill")),
                }
            }
            "quality" => {
                let q: u8 = v.parse().map_err(|_| bad(format!("quality must be {MIN_QUALITY} to {MAX_QUALITY}")))?;
                if !(MIN_QUALITY..=MAX_QUALITY).contains(&q) {
                    return Err(bad(format!("quality must be {MIN_QUALITY} to {MAX_QUALITY}")));
                }
                self.quality = Some(q);
            }
            "format" => {
                self.format = match v.to_ascii_lowercase().as_str() {
                    "" | "auto" => Format::Auto,
                    "origin" => Format::Origin,
                    "jpeg" | "jpg" => Format::Jpeg,
                    "png" => Format::Png,
                    "webp" => Format::Webp,
                    "avif" => Format::Avif,
                    _ => return Err(bad("format is origin, webp, avif, jpeg, png or auto")),
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Resolve `Auto` against the request's `Accept` header and normalise everything that can't
    /// change the output, so equivalent requests share one cached image.
    pub fn resolve(&self, accept: Option<&str>) -> Plan {
        let (format, negotiated) = match self.format {
            Format::Auto => (if accepts_webp(accept) { Format::Webp } else { Format::Origin }, true),
            f => (f, false),
        };
        // With at most one side given, every mode keeps the aspect ratio: they're the same.
        let resize = if self.width.is_some() && self.height.is_some() { self.resize } else { Resize::Cover };
        // PNG is lossless: quality doesn't apply.
        let quality = if format == Format::Png { 100 } else { self.quality.unwrap_or(DEFAULT_QUALITY) };
        Plan { width: self.width, height: self.height, resize, quality, format, negotiated }
    }
}

/// Whether an `Accept` header lists `image/webp` (with a non-zero q).
pub fn accepts_webp(accept: Option<&str>) -> bool {
    let Some(a) = accept else { return false };
    a.split(',').any(|item| {
        let mut parts = item.split(';');
        let media = parts.next().unwrap_or_default().trim();
        if !media.eq_ignore_ascii_case("image/webp") {
            return false;
        }
        !parts.any(|p| {
            let p = p.trim();
            p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")).and_then(|q| q.trim().parse::<f32>().ok()).is_some_and(|q| q <= 0.0)
        })
    })
}

/// A resolved transformation: what the engine does, and the cache key of its output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Plan {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub resize: Resize,
    /// 20–100; 100 for PNG.
    pub quality: u8,
    /// Never `Auto`.
    pub format: Format,
    /// The format came from `Accept` (`format` wasn't given): responses must `Vary: Accept`.
    pub negotiated: bool,
}

impl Plan {
    /// The canonical text of the plan (with the engine version); equal plans, equal text.
    pub fn canonical(&self) -> String {
        let d = |v: Option<u32>| v.map(|n| n.to_string()).unwrap_or_default();
        format!("{ENGINE};w={};h={};r={};q={};f={}", d(self.width), d(self.height), self.resize.as_str(), self.quality, self.format.as_str())
    }

    /// The cache key: SHA-256 of [`Self::canonical`], hex.
    pub fn key(&self) -> String {
        hex::encode(Sha256::digest(self.canonical().as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_specs_and_queries() {
        let p = Params::parse_spec("width:300, height:200,resize:contain,quality:70,format:webp").unwrap();
        assert_eq!(p, Params { width: Some(300), height: Some(200), resize: Resize::Contain, quality: Some(70), format: Format::Webp });
        assert_eq!(Params::parse_spec("").unwrap(), Params::default());
        let q = Params::from_query([("width", "10"), ("token", "x"), ("format", "origin")]).unwrap();
        assert_eq!(q, Params { width: Some(10), format: Format::Origin, ..Default::default() });
        // 0 and empty mean "not given", as on Supabase.
        assert_eq!(Params::parse_spec("width:0").unwrap().width, None);
        assert_eq!(Params::from_query([("height", "")]).unwrap().height, None);
    }

    #[test]
    fn refuses_bad_values() {
        for bad in [
            "width:2501",
            "width:-1",
            "width:abc",
            "height:99999999999",
            "quality:19",
            "quality:101",
            "quality:x",
            "resize:crop",
            "format:gif",
            "format:heic",
            "width",
            "width:1,width:2",
            "colour:red",
        ] {
            assert!(matches!(Params::parse_spec(bad), Err(ImageError::InvalidParams(_))), "{bad}");
        }
        assert!(Params::from_query([("width", "1"), ("width", "2")]).is_err());
        assert!(Params::parse_spec("width:2500,height:1,quality:20").is_ok());
        assert!(Params::parse_spec("quality:100").is_ok());
    }

    #[test]
    fn negotiates_webp_from_accept() {
        let p = Params::default();
        assert_eq!(p.resolve(Some("image/avif,image/webp,image/apng,*/*;q=0.8")).format, Format::Webp);
        assert!(p.resolve(Some("image/webp")).negotiated);
        assert_eq!(p.resolve(Some("image/webp;q=0")).format, Format::Origin);
        assert_eq!(p.resolve(Some("*/*")).format, Format::Origin);
        assert_eq!(p.resolve(None).format, Format::Origin);
        let explicit = Params { format: Format::Avif, ..Default::default() }.resolve(Some("image/webp"));
        assert_eq!((explicit.format, explicit.negotiated), (Format::Avif, false));
        assert_eq!(Params { format: Format::Origin, ..Default::default() }.resolve(Some("image/webp")).format, Format::Origin);
    }

    #[test]
    fn equivalent_requests_share_a_key() {
        let a = Params::parse_spec("width:300,resize:fill").unwrap().resolve(None);
        let b = Params::parse_spec("width:300").unwrap().resolve(None);
        assert_eq!(a.key(), b.key(), "resize doesn't matter with one side");
        let c = Params::parse_spec("width:300,quality:80").unwrap().resolve(None);
        assert_eq!(b.key(), c.key(), "80 is the default quality");
        let png1 = Params::parse_spec("format:png,quality:30").unwrap().resolve(None);
        let png2 = Params::parse_spec("format:png").unwrap().resolve(None);
        assert_eq!(png1.key(), png2.key(), "quality doesn't apply to PNG");
        let d = Params::parse_spec("width:300,height:10,resize:fill").unwrap().resolve(None);
        let e = Params::parse_spec("width:300,height:10").unwrap().resolve(None);
        assert_ne!(d.key(), e.key());
        assert_eq!(a.key().len(), 64);
        assert!(a.canonical().starts_with(ENGINE));
        // Auto resolves first: a WebP-accepting client and format:webp share the image.
        let auto = Params::default().resolve(Some("image/webp"));
        let webp = Params { format: Format::Webp, ..Default::default() }.resolve(None);
        assert_eq!(auto.key(), webp.key());
    }
}
