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
    (!p.is_empty() && site.files.contains_key(p)) || site.files.contains_key(&if p.is_empty() { "index.html".to_string() } else { format!("{p}/index.html") })
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
        if let Some(s) = self.sites.lock().get(key) {
            return Ok(s.clone());
        }
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
        let site = Arc::new(Site { tar: path, files, rules });
        self.sites.lock().insert(key.to_string(), site.clone());
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
        let site = Site { tar: PathBuf::new(), files, rules: Default::default() };
        assert_eq!(resolve(&site, "/", false), Some(("index.html", 200)));
        assert_eq!(resolve(&site, "/settings/profile", true), Some(("index.html", 200)), "a page path in a single-page app");
        assert_eq!(resolve(&site, "/settings/profile", false), None);
        assert_eq!(resolve(&site, "/logo.png", true), None, "a missing file stays missing");
        assert_eq!(key(&format!("nginx@sha256:{}", "A".repeat(64))), Some("a".repeat(64)));
    }
}
