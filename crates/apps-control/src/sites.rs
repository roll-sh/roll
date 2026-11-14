//! Static sites without an instance (docs/apps/README.md, "Static sites"; Render's and Vercel's
//! static sites): a build whose plan serves files (busybox httpd on `/site`) has its files
//! exported once, as a tar of its image's `/site`, kept by the image's digest (so a rollback or a
//! promotion reuses it): on the node's disk under `sites/`, and in object storage for the node
//! that takes the service over. The owner's proxy answers from the tar directly: an index of its
//! regular files (name → offset and length) read once, and byte ranges read per request. Nothing
//! is extracted, so a tenant's archive can't place a symlink or a `..` path anywhere: entries that
//! aren't regular files with plain path components are left out of the index. No instance runs:
//! no cold start, nothing metered but requests and bytes.

use crate::{Apps, Error, Result};
use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::PathBuf;
use std::sync::Arc;

/// The biggest site kept (its tar).
pub const MAX_BYTES: u64 = 512 << 20;
/// The most files a site's index holds.
pub const MAX_FILES: usize = 100_000;
/// Where in the image a static plan's files are.
pub const ROOT: &str = "/site";

/// One site's files: the tar on this node's disk and where each file is in it.
pub struct Site {
    pub tar: PathBuf,
    pub files: HashMap<String, (u64, u64)>,
    /// Its own `_redirects` and `_headers` ([`crate::site_rules`]).
    pub rules: crate::site_rules::SiteRules,
    /// Its files gzipped once asked for (a site never changes), and how many bytes those hold.
    pub gzipped: parking_lot::Mutex<Gzipped>,
    /// When a request last read it (unix seconds): a site nobody asks for leaves memory.
    pub used: std::sync::atomic::AtomicU64,
}

/// A site's gzipped copies: how many bytes they hold, the copies, and the files that didn't fit
/// (sent as they are from then on, never gzipped again).
#[derive(Default)]
pub struct Gzipped {
    bytes: usize,
    files: HashMap<String, bytes::Bytes>,
    too_big: std::collections::HashSet<String>,
}

/// How many of an environment's earlier static deployments still answer for their fingerprinted
/// files ([`Apps::earlier_site_file`]).
pub const EARLIER_SITES: usize = 3;

/// How long after a deploy its environment's earlier deployments still answer for their
/// fingerprinted files.
pub const SKEW_WINDOW_MS: i64 = 24 * 3600 * 1000;

/// An environment's earlier static deployments, as worked out for its live site.
pub struct EarlierSites {
    live: String,
    /// Until when (unix ms) they answer.
    until: i64,
    keys: Vec<String>,
    /// Those that didn't load, and when.
    failed: HashMap<String, std::time::Instant>,
}

/// How long a site nobody asks for stays loaded (its index and gzipped copies).
const IDLE: u64 = 15 * 60;

/// Files being gzipped at once on this node, for every site: past it a file goes out as it is.
static GZIPPING: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The biggest file gzipped as it's asked for, and how much of a site's gzipped copies are kept.
pub const GZIP_MAX: u64 = 8 << 20;
const GZIP_KEPT: usize = 32 << 20;
/// Files smaller than this aren't worth it.
pub const GZIP_MIN: u64 = 1024;

/// Whether a file's name says it never changes: a build's own asset directory (Vite's
/// `assets/`, Astro's `_astro/`, CRA's `static/`) and a content hash in its name
/// (`index-BxR2a9Kd.js`, `main.8f3a2b1c.css`), or anything under Next's `_next/static/`. Such a
/// file is cached for a year (a site's `_headers` can say otherwise).
pub fn fingerprinted(name: &str) -> bool {
    if name.starts_with("_next/static/") {
        return true;
    }
    if !["assets/", "_astro/", "static/js/", "static/css/", "static/media/"].iter().any(|d| name.starts_with(d)) {
        return false;
    }
    let file = name.rsplit('/').next().unwrap_or(name);
    // What a bundler names by its content: code, styles, maps, fonts and images (never a
    // document or a download that happens to have digits in its name).
    let ext = file.rsplit('.').next().unwrap_or("");
    if !["js", "mjs", "cjs", "css", "map", "wasm", "json", "woff", "woff2", "ttf", "otf", "eot", "svg", "png", "jpg", "jpeg", "gif", "webp", "avif", "ico"].contains(&ext) {
        return false;
    }
    let stem = file.split_once('.').map_or(file, |(s, _)| s);
    // The hash is a segment of its own, after `-` or `.`: 8 to 64 letters and digits, both.
    let rest = &file[stem.len()..];
    stem.split('-').skip(1).chain(rest.split('.')).any(|seg| {
        (8..=64).contains(&seg.len()) && seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && seg.bytes().any(|b| b.is_ascii_digit()) && seg.bytes().any(|b| b.is_ascii_alphabetic())
    })
}

/// Whether a type shrinks when compressed (text, scripts, data, SVG, WebAssembly, older
/// fonts); images, video, woff2 and archives already are.
pub fn compressible(ctype: &str) -> bool {
    ctype.starts_with("text/") || ["application/json", "application/xml", "image/svg+xml", "application/wasm", "font/ttf", "font/otf", "image/x-icon"].iter().any(|t| ctype.starts_with(t))
}

impl Site {
    /// A file gzipped (level 6) and kept for the next asker, if it is already, or if the site's
    /// copies are under [`GZIP_KEPT`] and the node isn't gzipping too much else: None sends it as
    /// it is. A file is gzipped at most once (but by two requests racing), so asking costs nothing
    /// once a site's budget is spent.
    pub fn gzipped(&self, name: &str) -> Option<bytes::Bytes> {
        self.gzipped.lock().files.get(name).cloned()
    }

    pub async fn gzip(self: &std::sync::Arc<Self>, name: &str) -> Option<bytes::Bytes> {
        {
            let g = self.gzipped.lock();
            if let Some(b) = g.files.get(name) {
                return Some(b.clone());
            }
            if g.bytes >= GZIP_KEPT || g.too_big.contains(name) {
                return None;
            }
        }
        let _permit = GZIPPING.try_acquire().ok()?;
        self.compress(name).await.ok()
    }

    async fn compress(self: &std::sync::Arc<Self>, name: &str) -> std::io::Result<bytes::Bytes> {
        let &(off, len) = self.files.get(name).ok_or_else(|| std::io::Error::other("no such file"))?;
        let tar = self.tar.clone();
        let gz = tokio::task::spawn_blocking(move || {
            use std::io::{Read, Seek, Write};
            let mut f = std::fs::File::open(&tar)?;
            f.seek(std::io::SeekFrom::Start(off))?;
            let mut raw = Vec::with_capacity(len as usize);
            f.take(len).read_to_end(&mut raw)?;
            let mut e = flate2::write::GzEncoder::new(Vec::with_capacity(raw.len() / 3), flate2::Compression::new(6));
            e.write_all(&raw)?;
            e.finish()
        })
        .await
        .map_err(std::io::Error::other)??;
        let gz = bytes::Bytes::from(gz);
        let mut c = self.gzipped.lock();
        if !c.files.contains_key(name) {
            if c.bytes + gz.len() <= GZIP_KEPT {
                c.bytes += gz.len();
                c.files.insert(name.to_string(), gz.clone());
            } else {
                c.too_big.insert(name.to_string());
            }
        }
        Ok(gz)
    }
}

/// A site's rules files out of its tar (each at most [`crate::site_rules::MAX_FILE`]; a bigger
/// one is left out).
fn rules(tar: &std::path::Path, files: &HashMap<String, (u64, u64)>) -> std::io::Result<crate::site_rules::SiteRules> {
    use std::io::{Read, Seek};
    let read = |name: &str| -> std::io::Result<Option<String>> {
        let Some(&(off, len)) = files.get(name).filter(|(_, len)| *len <= crate::site_rules::MAX_FILE) else { return Ok(None) };
        let mut f = std::fs::File::open(tar)?;
        f.seek(std::io::SeekFrom::Start(off))?;
        let mut b = Vec::new();
        f.take(len).read_to_end(&mut b)?;
        // A line that isn't UTF-8 is skipped, not the site.
        Ok(Some(String::from_utf8_lossy(&b).into_owned()))
    };
    Ok(crate::site_rules::SiteRules::parse(read("_redirects")?.as_deref(), read("_headers")?.as_deref()))
}

/// Whether a file answers `path` as it is (the file, or its directory's `index.html`), before
/// any single-page or 404 fallback.
pub fn exists(site: &Site, path: &str) -> bool {
    let p = path.trim_matches('/');
    (!p.is_empty() && site.files.contains_key(p)) || site.files.contains_key(&if p.is_empty() { "index.html".to_string() } else { format!("{p}/index.html") }) || clean(site, path).is_some()
}

/// `/about`'s `about.html` (not `/about/`'s: its relative links would point inside it).
fn clean<'a>(site: &'a Site, path: &str) -> Option<&'a str> {
    let p = path.trim_start_matches('/');
    (!p.is_empty() && !p.ends_with('/')).then(|| site.files.get_key_value(&format!("{p}.html")).map(|(k, _)| k.as_str())).flatten()
}

/// A site's key: its image's digest (hex, no `sha256:`), or for an image built here and named
/// by its tag (`roll-apps-…:<deployment>`, never reused for other contents), the tag's SHA-256.
pub fn key(image: &str) -> Option<String> {
    match image.rsplit_once("@sha256:") {
        Some((_, d)) => (d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit())).then(|| d.to_ascii_lowercase()),
        None => (!image.trim().is_empty()).then(|| crate::public::token_hash(image)),
    }
}

/// The index of a site's tar: its regular files by path (no leading `/`), each with its data's
/// offset and length. Anything else (directories, links, devices, `..`, absolute paths) is left
/// out. Bounded by [`MAX_FILES`].
pub fn index(r: impl Read + Seek) -> anyhow::Result<HashMap<String, (u64, u64)>> {
    let mut a = tar::Archive::new(r);
    let mut files = HashMap::new();
    for e in a.entries_with_seek()? {
        let e = e?;
        if !matches!(e.header().entry_type(), tar::EntryType::Regular | tar::EntryType::Continuous) {
            continue;
        }
        let path = e.path()?;
        let mut parts = Vec::new();
        let mut plain = true;
        for c in path.components() {
            match c {
                std::path::Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
                std::path::Component::CurDir => {}
                _ => plain = false,
            }
        }
        if !plain || parts.is_empty() {
            continue;
        }
        anyhow::ensure!(files.len() < MAX_FILES, "the site has more than {MAX_FILES} files");
        files.insert(parts.join("/"), (e.raw_file_position(), e.size()));
    }
    Ok(files)
}

impl Apps {
    fn site_path(&self, key: &str) -> PathBuf {
        self.cfg.home.join("sites").join(format!("{key}.tar"))
    }

    /// After a build whose plan serves static files: its files out of the image, kept by digest.
    /// `None` when this runtime can't export them (the deployment runs a container as before).
    pub(crate) async fn export_site(self: &Arc<Self>, image: &str) -> anyhow::Result<Option<u64>> {
        let Some(key) = key(image) else { return Ok(None) };
        let k = key.clone();
        if let Some(size) = self.db(move |s| s.site(&k)).await? {
            return Ok(Some(size));
        }
        let Ok(mut tar) = self.deps.runtime.export_files(image, ROOT).await else { return Ok(None) };
        let path = self.site_path(&key);
        tokio::fs::create_dir_all(path.parent().expect("a parent")).await?;
        let part = path.with_extension(format!("part-{}", crate::model::random(6)));
        let written = async {
            let mut f = tokio::fs::File::create(&part).await?;
            let n = tokio::io::copy(&mut tokio::io::AsyncReadExt::take(&mut tar, MAX_BYTES + 1), &mut f).await?;
            anyhow::ensure!(n <= MAX_BYTES, "the site is over {} MiB", MAX_BYTES >> 20);
            tokio::io::AsyncWriteExt::flush(&mut f).await?;
            Ok(n)
        }
        .await;
        let n = match written {
            Ok(n) => n,
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(e);
            }
        };
        // It must index before anything relies on it.
        let p = part.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || index(std::fs::File::open(&p)?)).await? {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
        tokio::fs::rename(&part, &path).await?;
        if let Some(o) = &self.deps.objects {
            o.upload(&format!("apps/sites/{key}.tar"), tokio::fs::File::open(&path).await?).await?;
        }
        let k = key.clone();
        self.db(move |s| s.put_site(&k, n)).await?;
        Ok(Some(n))
    }

    /// `path`'s file from one of the environment's last [`EARLIER_SITES`] static deployments
    /// before the live one (`live`: its site), when it's fingerprinted ([`fingerprinted`]) and the
    /// live one went live less than [`SKEW_WINDOW_MS`] ago: a page loaded before a deploy still
    /// gets the chunks it asks for, for a day. The site, its key and the file.
    pub(crate) async fn earlier_site_file(self: &Arc<Self>, service: &str, env: &str, live: &str, path: &str) -> Option<(Arc<Site>, String, String)> {
        let name = path.trim_matches('/');
        if !fingerprinted(name) {
            return None;
        }
        let ek = crate::env_key(service, env);
        let cached = self.earlier_sites.lock().get(&ek).filter(|c| c.live == live).map(|c| (c.until, c.keys.clone()));
        let (until, keys) = match cached {
            Some(c) => c,
            None => {
                let (sid, e) = (service.to_string(), env.to_string());
                let deps = self.db(move |s| s.deployments(&sid, &e, 20, None)).await.ok()?;
                let active = deps.iter().find(|d| d.status == crate::model::DeployStatus::Active);
                let until = active.and_then(|a| a.finished_at).map_or(0, |at| at + SKEW_WINDOW_MS);
                let mut keys: Vec<String> = Vec::new();
                // Before the live one: never a deployment rolled back from.
                let before = active.map_or(i64::MIN, |a| a.created_at);
                for d in deps.iter().filter(|d| d.status == crate::model::DeployStatus::Inactive && d.created_at < before && d.plan.as_ref().is_some_and(|p| p["start"] == apps_build::STATIC_START)) {
                    if let Some(k) = d.image.as_deref().and_then(key).filter(|k| k != live && !keys.contains(k)) {
                        keys.push(k);
                    }
                    if keys.len() == EARLIER_SITES {
                        break;
                    }
                }
                self.earlier_sites.lock().insert(ek.clone(), EarlierSites { live: live.to_string(), until, keys: keys.clone(), failed: HashMap::new() });
                (until, keys)
            }
        };
        if code_sql::now_ms() > until {
            return None;
        }
        for k in keys {
            // One that didn't load is tried again after a minute, not on every request.
            let failed = self.earlier_sites.lock().get(&ek).and_then(|c| c.failed.get(&k).copied());
            if failed.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(60)) {
                continue;
            }
            match self.site(&k).await {
                Ok(site) if site.files.contains_key(name) => return Some((site, k, name.to_string())),
                Ok(_) => {}
                Err(_) => {
                    if let Some(c) = self.earlier_sites.lock().get_mut(&ek) {
                        c.failed.insert(k, std::time::Instant::now());
                    }
                }
            }
        }
        None
    }

    /// A site's files go with its image (the service's deletion): here, in object storage, and
    /// from the store.
    pub(crate) async fn drop_site(self: &Arc<Self>, key: &str) {
        self.sites.lock().remove(key);
        let _ = tokio::fs::remove_file(self.site_path(key)).await;
        if let Some(o) = &self.deps.objects {
            let _ = o.delete(&format!("apps/sites/{key}.tar")).await;
        }
        let k = key.to_string();
        let _ = self.db(move |s| s.delete_site(&k)).await;
    }

    /// A site's files, read once per node (fetched from object storage when this node didn't
    /// build it).
    pub(crate) async fn site(self: &Arc<Self>, key: &str) -> Result<Arc<Site>> {
        let now = now_s();
        if let Some(s) = self.sites.lock().get(key) {
            s.used.store(now, std::sync::atomic::Ordering::Relaxed);
            return Ok(s.clone());
        }
        // One load per site at a time: the others wait for it, then find it.
        let lock = self.site_loads.lock().entry(key.to_string()).or_default().clone();
        let _loading = lock.lock().await;
        if let Some(s) = self.sites.lock().get(key) {
            return Ok(s.clone());
        }
        // Loading another: those nobody asked for a while go (a request still reading one keeps it).
        self.sites.lock().retain(|_, s| now.saturating_sub(s.used.load(std::sync::atomic::Ordering::Relaxed)) < IDLE);
        let path = self.site_path(key);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            let Some(o) = &self.deps.objects else { return Err(Error::NotFound("this site's files aren't on this node".into())) };
            tokio::fs::create_dir_all(path.parent().expect("a parent")).await.map_err(|e| Error::Internal(e.to_string()))?;
            let part = path.with_extension(format!("part-{}", crate::model::random(6)));
            let f = tokio::fs::File::create(&part).await.map_err(|e| Error::Internal(e.to_string()))?;
            o.download(&format!("apps/sites/{key}.tar"), f).await.map_err(|e| Error::Internal(format!("fetching the site: {e:#}")))?;
            tokio::fs::rename(&part, &path).await.map_err(|e| Error::Internal(e.to_string()))?;
        }
        let p = path.clone();
        let (files, rules) = tokio::task::spawn_blocking(move || {
            let files = index(std::fs::File::open(&p)?)?;
            let rules = rules(&p, &files)?;
            Ok::<_, anyhow::Error>((files, rules))
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
        .map_err(|e| Error::Internal(e.to_string()))?;
        let site = Arc::new(Site { tar: path, files, rules, gzipped: Default::default(), used: now.into() });
        self.sites.lock().insert(key.to_string(), site.clone());
        self.site_loads.lock().remove(key);
        Ok(site)
    }
}

/// A file's content type, by its extension.
pub fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" | "webmanifest" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

/// What a request path is in a site: the file, the directory's `index.html`, else (`spa`: a
/// page path, no extension) the site's `index.html` as a 200, else its `404.html` as a 404.
pub fn resolve<'a>(site: &'a Site, path: &str, spa: bool) -> Option<(&'a str, u16)> {
    let p = path.trim_matches('/');
    let found = |k: &str| site.files.get_key_value(k).map(|(k, _)| k.as_str());
    if !p.is_empty()
        && let Some(k) = found(p)
    {
        return Some((k, 200));
    }
    let index = if p.is_empty() { "index.html".to_string() } else { format!("{p}/index.html") };
    if let Some(k) = found(&index) {
        return Some((k, 200));
    }
    // A page written as `about.html` (Next's static export, Astro's `build.format: "file"`,
    // Hugo's ugly URLs) answers at `/about`, as Netlify, Cloudflare Pages and Vercel serve it.
    if let Some(k) = clean(site, path) {
        return Some((k, 200));
    }
    let last = p.rsplit('/').next().unwrap_or("");
    if spa && !last.contains('.') {
        return found("index.html").map(|k| (k, 200));
    }
    found("404.html").map(|k| (k, 404))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprinted_files_are_the_builds_hashed_assets() {
        for yes in ["assets/index-BxR2a9Kd.js", "assets/logo-a1b2c3d4.svg", "_astro/index.DjX3kL9p.css", "static/js/main.8f3a2b1c.js", "static/media/font.5e6f7a8b9c.woff2", "_next/static/chunks/app.js", "_next/static/abc/_buildManifest.js"] {
            assert!(fingerprinted(yes), "{yes}");
        }
        for no in ["index.html", "assets/app.js", "assets/jquery-3.7.1.min.js", "assets/report-20240101.pdf", "img/photo-a1b2c3d4.png", "assets/polyfills-legacy.js", "static/robots.txt", "assets/customers-export2024q1.csv"] {
            assert!(!fingerprinted(no), "{no}");
        }
    }

    #[test]
    fn a_sites_index_leaves_out_what_isnt_a_plain_file() {
        let mut b = tar::Builder::new(Vec::new());
        let mut file = |name: &str, body: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            b.append_data(&mut h, name, body).unwrap();
        };
        file("./index.html", b"<h1>hi</h1>");
        file("./assets/app.js", b"go()");
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_cksum();
        b.append_link(&mut link, "./secret", "/etc/passwd").unwrap();
        // `..` can't be written through the builder's checks: set the raw name.
        let mut evil = tar::Header::new_old();
        evil.as_old_mut().name[..9].copy_from_slice(b"../escape");
        evil.set_size(1);
        evil.set_entry_type(tar::EntryType::Regular);
        evil.set_cksum();
        b.append(&evil, &b"x"[..]).unwrap();
        let bytes = b.into_inner().unwrap();
        let files = index(std::io::Cursor::new(&bytes)).unwrap();
        let mut names: Vec<&str> = files.keys().map(String::as_str).collect();
        names.sort();
        assert_eq!(names, ["assets/app.js", "index.html"]);
        let (off, len) = files["assets/app.js"];
        assert_eq!(&bytes[off as usize..(off + len) as usize], b"go()");
        let mut site = Site { tar: PathBuf::new(), files, rules: Default::default(), gzipped: Default::default(), used: 0.into() };
        assert_eq!(resolve(&site, "/", false), Some(("index.html", 200)));
        assert_eq!(resolve(&site, "/settings/profile", true), Some(("index.html", 200)), "a page path in a single-page app");
        assert_eq!(resolve(&site, "/settings/profile", false), None);
        assert_eq!(resolve(&site, "/logo.png", true), None, "a missing file stays missing");
        site.files.insert("about.html".into(), (0, 1));
        assert_eq!((resolve(&site, "/about", false), resolve(&site, "/about/", false)), (Some(("about.html", 200)), None), "a clean URL");
        assert!(exists(&site, "/about"), "so a rewrite never hides it");
        assert_eq!(key(&format!("nginx@sha256:{}", "A".repeat(64))), Some("a".repeat(64)));
    }
}
