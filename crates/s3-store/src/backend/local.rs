//! A directory standing in for S3 (zero-setup development and tests). Writes are atomic and
//! durable: a temporary file, fsync, then rename (or a hard link for create-only writes), then
//! fsync of the directory.
//!
//! SSE-C blobs are encrypted at rest ([`super::local_sse`]); SSE-KMS needs AWS and is refused.

use super::local_sse::{self, Encryptor, Segment, Sidecar};
use super::{Backend, BlobAttrs, BlobSse, ByteStream, Checked, CopySource, CustomerKey, Listed, SseUnsupported, Uploaded, UploadedPart};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Temporary files and multipart uploads, beside the keys (listings skip them).
const TMP: &str = ".s3-tmp";
const UPLOADS: &str = ".s3-uploads";
/// Encrypted blobs' sidecars (`<key>.json`).
const SSE: &str = ".s3-sse";
const CHUNK: usize = 256 * 1024;

pub struct LocalBackend {
    root: PathBuf,
}

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n.div_ceil(2)];
    getrandom::fill(&mut b).expect("os randomness");
    b.iter().map(|x| format!("{x:02x}")).collect::<String>()[..n].to_string()
}

fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// `create_dir_all`, durably: every directory it creates is fsynced into its parent, so a power
/// loss can't take a new directory (and the files later written into it) away.
fn create_dirs(dir: &Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.exists() {
            break;
        }
        missing.push(d.to_path_buf());
        cur = d.parent();
    }
    if missing.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    // Outermost first: each parent then holds its new child.
    for d in missing.iter().rev() {
        if let Some(parent) = d.parent() {
            fsync_dir(parent)?;
        }
    }
    Ok(())
}

fn kms_unsupported() -> anyhow::Error {
    SseUnsupported("SSE-KMS needs an AWS S3 backend; the local directory backend can't do it".into()).into()
}

/// The SSE-C key of `sse` (`None` without one; SSE-KMS is refused).
fn customer(sse: Option<&BlobSse>) -> Result<Option<&CustomerKey>> {
    match sse {
        None => Ok(None),
        Some(BlobSse::Customer(k)) => Ok(Some(k)),
        Some(BlobSse::Kms(_)) => Err(kms_unsupported()),
    }
}

/// An encrypted part's backend etag: plaintext CRC32C and size, and its segment's salt.
fn part_token(crc: u32, size: u64, salt: Option<&str>) -> String {
    match salt {
        Some(s) => format!("{crc:08x}-{size}-{s}"),
        None => format!("{crc:08x}-{size}"),
    }
}

impl LocalBackend {
    pub fn new(root: &Path) -> Result<Self> {
        create_dirs(&root.join(TMP))?;
        create_dirs(&root.join(UPLOADS))?;
        Ok(Self { root: std::path::absolute(root)? })
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        if key.is_empty() || key.starts_with('/') || key.split('/').any(|p| p.is_empty() || p == "." || p == ".." || p.starts_with(".s3-")) {
            bail!("invalid backend key {key:?}");
        }
        Ok(self.root.join(key))
    }

    fn sidecar_path(&self, key: &str) -> Result<PathBuf> {
        self.path(key)?;
        Ok(self.root.join(SSE).join(format!("{key}.json")))
    }

    /// The sidecar of an encrypted blob (`None`: the blob isn't encrypted).
    fn sidecar(&self, key: &str) -> Result<Option<Sidecar>> {
        match std::fs::read(self.sidecar_path(key)?) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b).with_context(|| format!("sidecar of {key}"))?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Write a blob's sidecar durably (before the blob itself).
    fn write_sidecar(&self, key: &str, side: &Sidecar) -> Result<()> {
        let dst = self.sidecar_path(key)?;
        let tmp = self.tmp_path();
        Self::write_part_file(&tmp, &serde_json::to_vec(side)?)?;
        self.install(&tmp, &dst)
    }

    fn remove_sidecar(&self, key: &str) -> Result<()> {
        match std::fs::remove_file(self.sidecar_path(key)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn tmp_path(&self) -> PathBuf {
        self.root.join(TMP).join(random_hex(24))
    }

    fn upload_dir(&self, upload_id: &str) -> Result<PathBuf> {
        if upload_id.is_empty() || !upload_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid upload id {upload_id:?}");
        }
        Ok(self.root.join(UPLOADS).join(upload_id))
    }

    /// Stream a body into a fresh temporary file (fsynced), encrypted as one segment when `enc`
    /// is given. Returns its path, plaintext size and plaintext CRC32C.
    async fn write_tmp(&self, body: ByteStream, len: u64, enc: Option<Encryptor>) -> Result<(PathBuf, u64, u32)> {
        let tmp = self.tmp_path();
        let (mut body, state) = Checked::new(body, len);
        let result: Result<()> = async {
            let mut file = tokio::fs::File::create(&tmp).await?;
            use tokio::io::AsyncWriteExt;
            let mut enc = enc;
            let mut sealed = Vec::new();
            while let Some(chunk) = body.next().await {
                let chunk = chunk?;
                match enc.as_mut() {
                    Some(e) => {
                        e.update(&chunk, &mut sealed)?;
                        file.write_all(&sealed).await?;
                        sealed.clear();
                    }
                    None => file.write_all(&chunk).await?,
                }
            }
            if let Some(e) = enc {
                e.finish(&mut sealed)?;
                file.write_all(&sealed).await?;
            }
            file.flush().await?;
            file.sync_all().await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        let Some((size, crc)) = state.result() else {
            let _ = tokio::fs::remove_file(&tmp).await;
            bail!("body incomplete");
        };
        Ok((tmp, size, crc))
    }

    /// Move a finished temporary file into place (replacing what's there).
    fn install(&self, tmp: &Path, dst: &Path) -> Result<()> {
        let dir = dst.parent().context("key without a directory")?;
        create_dirs(dir)?;
        if let Err(e) = std::fs::rename(tmp, dst) {
            let _ = std::fs::remove_file(tmp);
            return Err(e.into());
        }
        fsync_dir(dir)?;
        Ok(())
    }

    fn write_part_file(path: &Path, data: &[u8]) -> Result<()> {
        let mut f = std::fs::File::create(path)?;
        f.write_all(data)?;
        f.sync_all()?;
        Ok(())
    }

    /// The key check of an encrypted upload (`None`: not encrypted), and its owner key.
    fn upload_info(&self, upload_id: &str, key: &str) -> Result<(PathBuf, Option<String>)> {
        let dir = self.upload_dir(upload_id)?;
        let owner = std::fs::read_to_string(dir.join("key")).with_context(|| format!("no multipart upload {upload_id}"))?;
        anyhow::ensure!(owner == key, "upload {upload_id} is for another key");
        let check = match std::fs::read_to_string(dir.join("sse")) {
            Ok(c) => Some(c),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        Ok((dir, check))
    }
}

fn modified_ms(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Listed>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".s3-") {
            continue;
        }
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            // Deleted while listing.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if meta.is_dir() {
            walk(root, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            let key = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
            out.push(Listed { key, size: meta.len(), modified_ms: modified_ms(&meta) });
        }
    }
    Ok(())
}

fn read_range(path: &Path, range: Option<Range<u64>>) -> std::io::Result<Option<(std::fs::File, u64)>> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let size = f.metadata()?.len();
    let (start, end) = clamp(range, size);
    f.seek(SeekFrom::Start(start))?;
    Ok(Some((f, end - start)))
}

/// `range` clamped to `size` (an empty range when it starts past the end).
fn clamp(range: Option<Range<u64>>, size: u64) -> (u64, u64) {
    match range {
        Some(r) => (r.start.min(size), r.end.min(size).max(r.start.min(size))),
        None => (0, size),
    }
}

fn file_stream(file: std::fs::File, len: u64) -> ByteStream {
    futures::stream::unfold((Some(file), len), |(file, left)| async move {
        let mut file = file?;
        if left == 0 {
            return None;
        }
        let n = (left as usize).min(CHUNK);
        let read = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; n];
            file.read_exact(&mut buf).map(|_| (file, buf))
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(|r| r);
        match read {
            Ok((file, buf)) => Some((Ok(Bytes::from(buf)), (Some(file), left - n as u64))),
            Err(e) => Some((Err(e), (None, 0))),
        }
    })
    .boxed()
}

/// The plaintext of `plan`'s chunks, decrypted one at a time as the stream is read.
fn decrypting_stream(file: std::fs::File, key: CustomerKey, side: Sidecar, plan: Vec<local_sse::ChunkRead>) -> ByteStream {
    let state = (Some(file), VecDeque::from(plan));
    let ctx = std::sync::Arc::new((key, side));
    futures::stream::unfold((state, ctx), |((file, mut plan), ctx)| async move {
        let mut file = file?;
        let c = plan.pop_front()?;
        let cx = ctx.clone();
        let read = tokio::task::spawn_blocking(move || -> std::io::Result<(std::fs::File, Vec<u8>)> {
            let mut stored = vec![0u8; c.stored];
            file.seek(SeekFrom::Start(c.offset))?;
            file.read_exact(&mut stored)?;
            let plain = local_sse::open(&cx.0, &cx.1, &c, &stored).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            Ok((file, plain[c.skip..c.skip + c.keep].to_vec()))
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(|r| r);
        match read {
            Ok((file, data)) => Some((Ok(Bytes::from(data)), ((Some(file), plan), ctx))),
            Err(e) => Some((Err(e), ((None, VecDeque::new()), ctx))),
        }
    })
    .boxed()
}

#[async_trait]
impl Backend for LocalBackend {
    fn describe(&self) -> String {
        format!("file://{}", self.root.display())
    }

    async fn put(&self, key: &str, body: ByteStream, len: u64, attrs: &BlobAttrs) -> Result<Uploaded> {
        let dst = self.path(key)?;
        match customer(attrs.sse.as_ref())? {
            None => {
                let (tmp, size, crc32c) = self.write_tmp(body, len, None).await?;
                // Never leave a sidecar that would make plaintext read as ciphertext.
                self.remove_sidecar(key)?;
                self.install(&tmp, &dst)?;
                Ok(Uploaded { size, crc32c })
            }
            Some(k) => {
                let salt = local_sse::new_salt();
                let (tmp, size, crc32c) = self.write_tmp(body, len, Some(Encryptor::new(k, &salt, len)?)).await?;
                let side = Sidecar { v: 1, check: local_sse::check_value(k), segments: vec![Segment { salt, len: size }] };
                // The sidecar first: an encrypted blob is never there without it.
                if let Err(e) = self.write_sidecar(key, &side) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                self.install(&tmp, &dst)?;
                Ok(Uploaded { size, crc32c })
            }
        }
    }

    async fn put_small(&self, key: &str, data: Bytes) -> Result<()> {
        let dst = self.path(key)?;
        let tmp = self.tmp_path();
        Self::write_part_file(&tmp, &data)?;
        self.install(&tmp, &dst)
    }

    async fn put_create_only(&self, key: &str, data: Bytes) -> Result<bool> {
        let dst = self.path(key)?;
        let tmp = self.tmp_path();
        Self::write_part_file(&tmp, &data)?;
        let dir = dst.parent().context("key without a directory")?;
        create_dirs(dir)?;
        let linked = std::fs::hard_link(&tmp, &dst);
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => {
                fsync_dir(dir)?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn get_blob(&self, key: &str, range: Option<Range<u64>>, sse: Option<&BlobSse>) -> Result<Option<(u64, ByteStream)>> {
        let path = self.path(key)?;
        let wanted = customer(sse)?.cloned();
        let side = self.sidecar(key)?;
        match (side, wanted) {
            (None, None) => {
                let opened = tokio::task::spawn_blocking(move || read_range(&path, range)).await??;
                Ok(opened.map(|(f, len)| (len, file_stream(f, len))))
            }
            (None, Some(_)) => bail!("{key} isn't encrypted with a customer key"),
            (Some(_), None) => bail!("{key} is encrypted with a customer key (SSE-C); reading it needs the key"),
            (Some(side), Some(k)) => {
                side.verify_key(&k)?;
                let file = match std::fs::File::open(&path) {
                    Ok(f) => f,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e.into()),
                };
                let stored = file.metadata()?.len();
                anyhow::ensure!(stored == side.encrypted_len(), "{key} has {stored} bytes, its sidecar says {}", side.encrypted_len());
                let (start, end) = clamp(range, side.plain_len());
                let plan = local_sse::plan(&side, start, end);
                Ok(Some((end - start, decrypting_stream(file, k, side, plan))))
            }
        }
    }

    async fn head(&self, key: &str) -> Result<Option<Listed>> {
        let path = self.path(key)?;
        match tokio::fs::metadata(&path).await {
            Ok(m) if m.is_file() => {
                let size = match self.sidecar(key)? {
                    Some(side) => side.plain_len(),
                    None => m.len(),
                };
                Ok(Some(Listed { key: key.to_string(), size, modified_ms: modified_ms(&m) }))
            }
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.path(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        // After the blob: a sidecar without its blob is harmless.
        self.remove_sidecar(key)
    }

    async fn list_page(&self, prefix: &str, start_after: Option<&str>, max: usize) -> Result<(Vec<Listed>, bool)> {
        let root = self.root.clone();
        let base = match prefix.rfind('/') {
            Some(i) => root.join(&prefix[..i]),
            None => root.clone(),
        };
        let mut all = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            walk(&root, &base, &mut out).map(|_| out)
        })
        .await??;
        all.retain(|l| l.key.starts_with(prefix) && start_after.is_none_or(|a| l.key.as_str() > a));
        all.sort_by(|a, b| a.key.cmp(&b.key));
        let more = all.len() > max;
        all.truncate(max);
        // Encrypted blobs list their plaintext size, as on S3.
        for l in &mut all {
            if let Some(side) = self.sidecar(&l.key)? {
                l.size = side.plain_len();
            }
        }
        Ok((all, more))
    }

    async fn create_multipart(&self, key: &str, attrs: &BlobAttrs) -> Result<String> {
        self.path(key)?;
        let key_check = customer(attrs.sse.as_ref())?.map(local_sse::check_value);
        let id = random_hex(32);
        let dir = self.upload_dir(&id)?;
        create_dirs(&dir)?;
        if let Some(c) = &key_check {
            Self::write_part_file(&dir.join("sse"), c.as_bytes())?;
        }
        Self::write_part_file(&dir.join("key"), key.as_bytes())?;
        fsync_dir(&dir)?;
        Ok(id)
    }

    async fn upload_part(&self, key: &str, upload_id: &str, part: u32, body: ByteStream, len: u64, sse: Option<&BlobSse>) -> Result<UploadedPart> {
        let (dir, check) = self.upload_info(upload_id, key)?;
        let k = customer(sse)?;
        let enc = match (&check, k) {
            (None, None) => None,
            (Some(c), Some(k)) if *c == local_sse::check_value(k) => Some((k, local_sse::new_salt())),
            (Some(_), Some(_)) => bail!("upload {upload_id} is encrypted with another customer key"),
            (Some(_), None) => bail!("upload {upload_id} is encrypted with a customer key; its parts need it"),
            (None, Some(_)) => bail!("upload {upload_id} isn't encrypted with a customer key"),
        };
        let encryptor = match &enc {
            Some((k, salt)) => Some(Encryptor::new(k, salt, len)?),
            None => None,
        };
        let (tmp, size, crc32c) = self.write_tmp(body, len, encryptor).await?;
        if let Some((_, salt)) = &enc {
            // The part's segment first, as for a blob's sidecar.
            let seg = Segment { salt: salt.clone(), len: size };
            if let Err(e) = Self::write_part_file(&dir.join(format!("{part:05}.sse")), &serde_json::to_vec(&seg)?) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
        let dst = dir.join(format!("{part:05}"));
        if let Err(e) = std::fs::rename(&tmp, &dst) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        fsync_dir(&dir)?;
        Ok(UploadedPart { etag: part_token(crc32c, size, enc.as_ref().map(|(_, s)| s.as_str())), size, crc32c })
    }

    async fn upload_part_copy(&self, key: &str, upload_id: &str, part: u32, src: &CopySource<'_>, sse: Option<&BlobSse>) -> Result<UploadedPart> {
        let Some((len, body)) = self.get_blob(src.key, src.range.clone(), src.sse).await? else { bail!("copy source {} not found", src.key) };
        self.upload_part(key, upload_id, part, body, len, sse).await
    }

    async fn copy(&self, src: &CopySource<'_>, key: &str, attrs: &BlobAttrs) -> Result<Uploaded> {
        let Some((len, body)) = self.get_blob(src.key, src.range.clone(), src.sse).await? else { bail!("copy source {} not found", src.key) };
        self.put(key, body, len, attrs).await
    }

    async fn complete_multipart(&self, key: &str, upload_id: &str, parts: &[(u32, String)], sse: Option<&BlobSse>) -> Result<()> {
        let (dir, check) = self.upload_info(upload_id, key)?;
        let k = customer(sse)?.cloned();
        if let (Some(c), Some(k)) = (&check, &k) {
            anyhow::ensure!(*c == local_sse::check_value(k), "upload {upload_id} is encrypted with another customer key");
        }
        anyhow::ensure!(check.is_some() || k.is_none(), "upload {upload_id} isn't encrypted with a customer key");
        let dst = self.path(key)?;
        let tmp = self.tmp_path();
        let parts_owned = parts.to_vec();
        let dir2 = dir.clone();
        let tmp2 = tmp.clone();
        let encrypted = check.is_some();
        let result = tokio::task::spawn_blocking(move || -> Result<Vec<Segment>> {
            let mut out = std::fs::File::create(&tmp2)?;
            let mut segments = Vec::new();
            for (n, etag) in &parts_owned {
                let path = dir2.join(format!("{n:05}"));
                let mut f = std::fs::File::open(&path).with_context(|| format!("part {n} missing"))?;
                let seg = if encrypted {
                    let seg: Segment = serde_json::from_slice(&std::fs::read(dir2.join(format!("{n:05}.sse"))).with_context(|| format!("part {n}'s segment missing"))?)?;
                    // `<crc>-<size>-<salt>`: the size and salt must be this file's (the CRC is
                    // checked against the plaintext below, when the key is given).
                    anyhow::ensure!(etag.split_once('-').map(|(_, rest)| rest) == Some(format!("{}-{}", seg.len, seg.salt).as_str()), "part {n} doesn't match its etag");
                    anyhow::ensure!(f.metadata()?.len() == local_sse::encrypted_len(seg.len), "part {n} has the wrong size");
                    Some(seg)
                } else {
                    None
                };
                let mut crc = 0u32;
                let mut size = 0u64;
                let mut buf = vec![0u8; CHUNK];
                loop {
                    let r = f.read(&mut buf)?;
                    if r == 0 {
                        break;
                    }
                    if seg.is_none() {
                        crc = crc32c::crc32c_append(crc, &buf[..r]);
                    }
                    size += r as u64;
                    out.write_all(&buf[..r])?;
                }
                match seg {
                    Some(s) => segments.push(s),
                    None => anyhow::ensure!(*etag == part_token(crc, size, None), "part {n} doesn't match its etag"),
                }
            }
            out.sync_all()?;
            Ok(segments)
        })
        .await?;
        let segments = match result {
            Ok(s) => s,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        };
        match &check {
            Some(c) => {
                let side = Sidecar { v: 1, check: c.clone(), segments };
                // With the key at hand, prove every chunk decrypts and the plaintext matches the
                // parts' CRC32Cs before the object exists.
                if let Some(k) = &k
                    && let Err(e) = verify_encrypted(&tmp, k, &side, parts)
                {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                if let Err(e) = self.write_sidecar(key, &side) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
            }
            None => self.remove_sidecar(key)?,
        }
        self.install(&tmp, &dst)?;
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    async fn abort_multipart(&self, _key: &str, upload_id: &str) -> Result<()> {
        let dir = self.upload_dir(upload_id)?;
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Decrypt an assembled encrypted upload and check each part's plaintext CRC32C (the first
/// eight characters of its token).
fn verify_encrypted(path: &Path, key: &CustomerKey, side: &Sidecar, parts: &[(u32, String)]) -> Result<()> {
    let mut f = std::fs::File::open(path)?;
    for (si, (n, token)) in parts.iter().enumerate() {
        let want = u32::from_str_radix(token.get(..8).unwrap_or(""), 16).with_context(|| format!("part {n}'s etag"))?;
        let (start, len): (u64, u64) = (side.segments[..si].iter().map(|s| s.len).sum(), side.segments[si].len);
        let mut crc = 0u32;
        for c in local_sse::plan(side, start, start + len) {
            let mut stored = vec![0u8; c.stored];
            f.seek(SeekFrom::Start(c.offset))?;
            f.read_exact(&mut stored)?;
            crc = crc32c::crc32c_append(crc, &local_sse::open(key, side, &c, &stored)?[c.skip..c.skip + c.keep]);
        }
        anyhow::ensure!(crc == want, "part {n} doesn't match its checksum");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_dirs_makes_nested_directories_and_tolerates_existing_ones() {
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("a/b/c");
        create_dirs(&deep).unwrap();
        assert!(deep.is_dir());
        // Again, and a sibling sharing the existing part.
        create_dirs(&deep).unwrap();
        create_dirs(&root.path().join("a/b/d")).unwrap();
        assert!(root.path().join("a/b/d").is_dir());
        // Racing callers both succeed.
        let shared = root.path().join("x/y/z");
        std::thread::scope(|s| {
            let h: Vec<_> = (0..4).map(|_| s.spawn(|| create_dirs(&shared))).collect();
            for t in h {
                t.join().unwrap().unwrap();
            }
        });
        assert!(shared.is_dir());
    }

    #[tokio::test]
    async fn writes_into_new_directories_survive_a_reopen() {
        let root = tempfile::tempdir().unwrap();
        let b = LocalBackend::new(&root.path().join("fresh/root")).unwrap();
        assert!(b.put_create_only("s3/meta/wal/00000000000000000001", Bytes::from_static(b"one")).await.unwrap());
        assert!(!b.put_create_only("s3/meta/wal/00000000000000000001", Bytes::from_static(b"two")).await.unwrap());
        b.put_small("s3/blobs/ab/abcdef", Bytes::from_static(b"blob")).await.unwrap();
        let id = b.create_multipart("s3/blobs/cd/cdef", &BlobAttrs::default()).await.unwrap();
        let again = LocalBackend::new(&root.path().join("fresh/root")).unwrap();
        assert_eq!(again.get_bytes("s3/meta/wal/00000000000000000001").await.unwrap().unwrap(), "one");
        assert_eq!(again.get_bytes("s3/blobs/ab/abcdef").await.unwrap().unwrap(), "blob");
        again.abort_multipart("s3/blobs/cd/cdef", &id).await.unwrap();
    }

    #[test]
    fn part_tokens() {
        assert_eq!(part_token(0xdead, 5, None), "0000dead-5");
        assert_eq!(part_token(0xdead, 5, Some("ab")), "0000dead-5-ab");
    }
}
