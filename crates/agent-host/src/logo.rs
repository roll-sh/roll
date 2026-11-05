//! A project's own logo (docs/code/agents.md, "Identity"): found in the project the way T3 Code
//! finds a project's favicon, and carried in the project record as a small `data:` URL.
//!
//! Ported from T3 Code's `apps/server/src/project/ProjectFaviconResolver.ts` (MIT): the picture a
//! `File` icon names (T3's `faviconPath`), else the first of the well-known favicon and logo
//! paths, else the icon an `index.html` or a root route links to. Differences:
//!
//! - `.icns` is left out of the well-known paths and AVIF isn't read: browsers can't show the
//!   one, and decoding the other needs native libraries. Either falls through to the next path.
//! - Every candidate is canonicalized and must stay inside the canonical project root and be a
//!   regular file, so a symlink out of the repo is never read. Reads are bounded
//!   ([`MAX_SOURCE`], [`MAX_PICTURE`]) whatever the file's metadata says.
//! - An SVG goes as it is (clients show it only as an `<img>`, where scripts never run); a raster
//!   is decoded under [`image::Limits`], shrunk to fit [`ProjectLogo::MAX_PX`] and sent as PNG. A
//!   picture that still doesn't fit [`ProjectLogo::MAX_URL`] is skipped for the next candidate.
//! - Instead of T3's TTL cache, results are cached by fingerprint (path, length, modification
//!   time), so a refresh only stats files and re-decodes nothing that didn't change.

use agent_proto::{ProjectIcon, ProjectLogo};
use base64::Engine;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// T3's `FAVICON_CANDIDATES`, without `.icns`.
pub const CANDIDATES: [&str; 21] = [
    "favicon.svg",
    "favicon.ico",
    "favicon.png",
    "public/favicon.svg",
    "public/favicon.ico",
    "public/favicon.png",
    "app/favicon.ico",
    "app/favicon.png",
    "app/icon.svg",
    "app/icon.png",
    "app/icon.ico",
    "src/favicon.ico",
    "src/favicon.svg",
    "src/app/favicon.ico",
    "src/app/icon.svg",
    "src/app/icon.png",
    "assets/icon.svg",
    "assets/icon.png",
    "assets/logo.svg",
    "assets/logo.png",
    ".idea/icon.svg",
];

/// T3's `ICON_SOURCE_FILES`: pages and root routes that link the site's icon.
pub const SOURCES: [&str; 7] = [
    "index.html",
    "public/index.html",
    "app/routes/__root.tsx",
    "src/routes/__root.tsx",
    "app/root.tsx",
    "src/root.tsx",
    "src/index.html",
];

/// Bytes read of a source file at most.
pub const MAX_SOURCE: u64 = 1 << 20;
/// Bytes read of a picture at most.
pub const MAX_PICTURE: u64 = 2 << 20;
/// What a raster may decode to.
const MAX_SIDE: u32 = 4096;
const MAX_ALLOC: u64 = 64 << 20;
/// Fingerprints remembered at most (cleared when full).
const MAX_CACHED: usize = 1024;

/// A file as it is now: a change in any part means reading it again.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Fingerprint {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Debug)]
enum Cached {
    /// A picture: its logo, or `None` when it can't be one (too big, undecodable).
    Picture(Option<ProjectLogo>),
    /// A source file: the icon href it links, if any.
    Href(Option<String>),
}

/// Finds projects' logos, remembering what it read by fingerprint.
#[derive(Default)]
pub struct Logos {
    cache: std::sync::Mutex<HashMap<Fingerprint, Cached>>,
}

impl Logos {
    /// The project's logo by T3's order, or `None`. Blocking (file I/O and decoding): call it
    /// from `spawn_blocking`.
    pub fn resolve(&self, project: &Path, icon: &ProjectIcon) -> Option<ProjectLogo> {
        let root = std::fs::canonicalize(project).ok()?;
        if let ProjectIcon::File { path } = icon
            && let Some(logo) = self.picture(&root, path)
        {
            return Some(logo);
        }
        if let Some(logo) = CANDIDATES.iter().find_map(|c| self.picture(&root, c)) {
            return Some(logo);
        }
        SOURCES.iter().find_map(|source| {
            let href = self.href(&root, source)?;
            let clean = href.trim_start_matches('/');
            [format!("public/{clean}"), clean.to_string()].iter().find_map(|c| self.picture(&root, c))
        })
    }

    /// The logo a file inside the project makes, if it exists and can be one.
    fn picture(&self, root: &Path, rel: &str) -> Option<ProjectLogo> {
        let (path, print) = inside(root, rel)?;
        if let Some(Cached::Picture(logo)) = self.cached(&print) {
            return logo;
        }
        let logo = read_bounded(&path, MAX_PICTURE).and_then(|bytes| encode(rel, &bytes));
        self.remember(print, Cached::Picture(logo.clone()));
        logo
    }

    /// The icon href a source file links, if it exists and links one.
    fn href(&self, root: &Path, rel: &str) -> Option<String> {
        let (path, print) = inside(root, rel)?;
        if let Some(Cached::Href(href)) = self.cached(&print) {
            return href;
        }
        let href = read_bounded(&path, MAX_SOURCE).and_then(|bytes| icon_href(&String::from_utf8_lossy(&bytes)));
        self.remember(print, Cached::Href(href.clone()));
        href
    }

    fn cached(&self, print: &Fingerprint) -> Option<Cached> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(print).cloned()
    }

    fn remember(&self, print: Fingerprint, value: Cached) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= MAX_CACHED {
            cache.clear();
        }
        cache.insert(print, value);
    }
}

/// `rel` under the canonical `root`, if it resolves (through any symlinks) to a regular file that
/// is still inside it.
fn inside(root: &Path, rel: &str) -> Option<(PathBuf, Fingerprint)> {
    if rel.is_empty() || Path::new(rel).is_absolute() {
        return None;
    }
    let path = std::fs::canonicalize(root.join(rel)).ok()?;
    if !path.starts_with(root) {
        return None;
    }
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let print = Fingerprint { path: path.clone(), len: meta.len(), modified: meta.modified().ok() };
    Some((path, print))
}

/// At most `max` bytes of a file, or `None` if it holds more (or can't be read).
fn read_bounded(path: &Path, max: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.take(max + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= max).then_some(bytes)
}

/// A picture's bytes as a logo: an SVG as it is, a raster as a PNG shrunk to fit. The kind comes
/// from the name the project uses (`rel`), not the file a symlink points at.
fn encode(rel: &str, bytes: &[u8]) -> Option<ProjectLogo> {
    let ext = Path::new(rel).extension()?.to_str()?.to_ascii_lowercase();
    let url = match ext.as_str() {
        "svg" => data_url("image/svg+xml", bytes),
        "png" | "ico" | "gif" | "webp" | "jpg" | "jpeg" => data_url("image/png", &shrink(bytes)?),
        _ => return None,
    };
    (url.len() <= ProjectLogo::MAX_URL).then(|| ProjectLogo { path: rel.to_string(), url })
}

/// A raster decoded under limits, shrunk to fit [`ProjectLogo::MAX_PX`] (never grown), as PNG.
fn shrink(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let mut img = reader.decode().ok()?;
    let px = ProjectLogo::MAX_PX;
    if img.width() > px || img.height() > px {
        img = img.resize(px, px, image::imageops::FilterType::Lanczos3);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// The icon a page or a root route links (T3's `extractIconHref`): an HTML
/// `<link rel="icon" href="…">`, else an object literal with `rel: "icon"` and `href: "…"`
/// (TanStack Router and Remix root routes). Remote and inline hrefs don't count.
pub fn icon_href(source: &str) -> Option<String> {
    let href = html_link(source).or_else(|| source.split('}').find_map(|run| if has_icon_rel(run) { quoted_after(run, "href", ':') } else { None }))?;
    let href = href.split(['?', '#']).next().unwrap_or_default().trim().to_string();
    let remote = href.starts_with("//") || href.contains("://") || href.starts_with("data:");
    (!href.is_empty() && !remote).then_some(href)
}

/// The href of the first `<link>` whose `rel` is `icon` or `shortcut icon`.
fn html_link(source: &str) -> Option<String> {
    let lower = source.to_ascii_lowercase();
    let mut at = 0;
    while let Some(start) = lower[at..].find("<link").map(|i| at + i) {
        let end = lower[start..].find('>').map_or(lower.len(), |i| start + i);
        let tag = &source[start..end];
        let rel = quoted_after(tag, "rel", '=').map(|r| r.to_ascii_lowercase());
        if matches!(rel.as_deref(), Some("icon" | "shortcut icon"))
            && let Some(href) = quoted_after(tag, "href", '=')
        {
            return Some(href);
        }
        at = end.max(start + 1);
    }
    None
}

/// Whether a run of an object literal says `rel: "icon"` (or `"shortcut icon"`).
fn has_icon_rel(run: &str) -> bool {
    matches!(quoted_after(run, "rel", ':').map(|r| r.to_ascii_lowercase()).as_deref(), Some("icon" | "shortcut icon"))
}

/// The quoted value after the first `name` (a whole word) followed by `sep`, like `href="x"` or
/// `href: 'x'`.
fn quoted_after(text: &str, name: &str, sep: char) -> Option<String> {
    // ASCII lowercasing keeps byte offsets, so indexes into `lower` index `text` too.
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut at = 0;
    while let Some(i) = lower[at..].find(name).map(|i| at + i) {
        at = i + name.len();
        if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'-' || bytes[i - 1] == b'_') {
            continue;
        }
        let Some(rest) = text[at..].trim_start().strip_prefix(sep) else { continue };
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else { continue };
        let value = &rest[1..];
        let Some(end) = value.find(quote) else { continue };
        return Some(value[..end].to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn decode_png(logo: &ProjectLogo) -> image::DynamicImage {
        let b64 = logo.url.strip_prefix("data:image/png;base64,").expect("a PNG data URL");
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).unwrap()
    }

    #[test]
    fn finds_the_link_in_html_and_root_routes() {
        let html = r#"<head><link rel="stylesheet" href="/a.css"><LINK href="/brand/icon.svg?v=2" rel="icon" type="image/svg+xml"></head>"#;
        assert_eq!(icon_href(html).as_deref(), Some("/brand/icon.svg"));
        let shortcut = r#"<link rel='shortcut icon' href='favicon.ico'>"#;
        assert_eq!(icon_href(shortcut).as_deref(), Some("favicon.ico"));
        let tsx = r#"export const Route = createRootRoute({ head: () => ({ links: [
            { rel: "stylesheet", href: appCss },
            { rel: "icon", type: "image/png", href: "/logo192.png" },
        ] }) })"#;
        assert_eq!(icon_href(tsx).as_deref(), Some("/logo192.png"));
        assert_eq!(icon_href(r#"<link rel="icon" href="https://cdn.example.com/x.png">"#), None);
        assert_eq!(icon_href(r#"<link rel="icon" href="data:image/png;base64,AAAA">"#), None);
        assert_eq!(icon_href(r#"<link rel="preload" href="/x.png">"#), None);
        assert_eq!(icon_href(r#"<link rel="apple-touch-icon" href="/x.png">"#), None);
    }

    #[test]
    fn takes_candidates_in_order_and_the_file_icon_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "public/favicon.svg", b"<svg xmlns='http://www.w3.org/2000/svg'/>");
        write(root, "assets/logo.png", &png(16, 16));
        let logos = Logos::default();
        let auto = logos.resolve(root, &ProjectIcon::Auto).unwrap();
        assert_eq!(auto.path, "public/favicon.svg");
        assert!(auto.url.starts_with("data:image/svg+xml;base64,"));
        write(root, "favicon.png", &png(8, 8));
        assert_eq!(logos.resolve(root, &ProjectIcon::Auto).unwrap().path, "favicon.png");
        let file = ProjectIcon::File { path: "assets/logo.png".into() };
        assert_eq!(logos.resolve(root, &file).unwrap().path, "assets/logo.png");
        // A file icon that isn't there falls back to the search (T3's order).
        let missing = ProjectIcon::File { path: "nope.png".into() };
        assert_eq!(logos.resolve(root, &missing).unwrap().path, "favicon.png");
    }

    #[test]
    fn follows_an_index_html_link() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "index.html", br#"<link rel="icon" href="/brand.svg">"#);
        write(root, "public/brand.svg", b"<svg/>");
        assert_eq!(Logos::default().resolve(root, &ProjectIcon::Auto).unwrap().path, "public/brand.svg");
        assert_eq!(Logos::default().resolve(&root.join("missing"), &ProjectIcon::Auto), None);
    }

    #[cfg(unix)]
    #[test]
    fn never_reads_through_a_symlink_out_of_the_project() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "secret.svg", b"<svg/>");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        symlink(outside.path().join("secret.svg"), root.join("favicon.svg")).unwrap();
        symlink(outside.path(), root.join("public")).unwrap();
        let logos = Logos::default();
        assert_eq!(logos.resolve(root, &ProjectIcon::Auto), None);
        assert_eq!(logos.resolve(root, &ProjectIcon::File { path: "public/secret.svg".into() }), None);
        // A symlink that stays inside the project is followed.
        write(root, "brand/mark.svg", b"<svg/>");
        std::fs::create_dir_all(root.join("app")).unwrap();
        symlink(root.join("brand/mark.svg"), root.join("app/icon.svg")).unwrap();
        assert_eq!(logos.resolve(root, &ProjectIcon::Auto).unwrap().path, "app/icon.svg");
    }

    #[test]
    fn skips_an_oversized_svg_for_the_next_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let big = format!("<svg>{}</svg>", "x".repeat(ProjectLogo::MAX_URL));
        write(root, "favicon.svg", big.as_bytes());
        write(root, "favicon.png", &png(4, 4));
        assert_eq!(Logos::default().resolve(root, &ProjectIcon::Auto).unwrap().path, "favicon.png");
    }

    #[test]
    fn shrinks_a_large_raster_to_a_small_png() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "app/icon.png", &png(512, 256));
        let logo = Logos::default().resolve(root, &ProjectIcon::Auto).unwrap();
        assert!(logo.url.len() <= ProjectLogo::MAX_URL);
        let img = decode_png(&logo);
        assert_eq!((img.width(), img.height()), (64, 32));
        // Small ones keep their size.
        write(root, "favicon.png", &png(16, 16));
        let img = decode_png(&Logos::default().resolve(root, &ProjectIcon::Auto).unwrap());
        assert_eq!((img.width(), img.height()), (16, 16));
    }

    #[test]
    fn refuses_garbage_and_reads_a_changed_file_again() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "favicon.png", b"not a png");
        let logos = Logos::default();
        assert_eq!(logos.resolve(root, &ProjectIcon::Auto), None);
        // Rewritten (a new length): read again.
        write(root, "favicon.png", &png(8, 8));
        assert_eq!(logos.resolve(root, &ProjectIcon::Auto).unwrap().path, "favicon.png");
    }
}
