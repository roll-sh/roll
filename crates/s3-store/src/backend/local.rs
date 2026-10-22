//! A directory standing in for S3 (zero-setup development and tests). Writes are atomic and
//! durable: a temporary file, fsync, then rename (or a hard link for create-only writes), then
//! fsync of the directory.

use super::{Backend, BlobAttrs, ByteStream, Checked, Listed, Uploaded, UploadedPart};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Temporary files and multipart uploads, beside the keys (listings skip them).
const TMP: &str = ".s3-tmp";
const UPLOADS: &str = ".s3-uploads";
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

    fn tmp_path(&self) -> PathBuf {
        self.root.join(TMP).join(random_hex(24))
    }

    fn upload_dir(&self, upload_id: &str) -> Result<PathBuf> {
        if upload_id.is_empty() || !upload_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid upload id {upload_id:?}");
        }
        Ok(self.root.join(UPLOADS).join(upload_id))
    }

    /// Stream a body into a fresh temporary file (fsynced). Returns its path, size and CRC32C.
    async fn write_tmp(&self, body: ByteStream, len: u64) -> Result<(PathBuf, u64, u32)> {
        let tmp = self.tmp_path();
        let (mut body, state) = Checked::new(body, len);
        let result: Result<()> = async {
            let mut file = tokio::fs::File::create(&tmp).await?;
            use tokio::io::AsyncWriteExt;
            while let Some(chunk) = body.next().await {
                file.write_all(&chunk?).await?;
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
    let (start, end) = match range {
        Some(r) => (r.start.min(size), r.end.min(size).max(r.start.min(size))),
        None => (0, size),
    };
    f.seek(SeekFrom::Start(start))?;
    Ok(Some((f, end - start)))
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

#[async_trait]
impl Backend for LocalBackend {
    fn describe(&self) -> String {
        format!("file://{}", self.root.display())
    }

    async fn put(&self, key: &str, body: ByteStream, len: u64, _attrs: &BlobAttrs) -> Result<Uploaded> {
        let dst = self.path(key)?;
        let (tmp, size, crc32c) = self.write_tmp(body, len).await?;
        self.install(&tmp, &dst)?;
        Ok(Uploaded { size, crc32c })
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

    async fn get(&self, key: &str, range: Option<Range<u64>>) -> Result<Option<(u64, ByteStream)>> {
        let path = self.path(key)?;
        let opened = tokio::task::spawn_blocking(move || read_range(&path, range)).await??;
        Ok(opened.map(|(f, len)| (len, file_stream(f, len))))
    }

    async fn head(&self, key: &str) -> Result<Option<Listed>> {
        let path = self.path(key)?;
        match tokio::fs::metadata(&path).await {
            Ok(m) if m.is_file() => Ok(Some(Listed { key: key.to_string(), size: m.len(), modified_ms: modified_ms(&m) })),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.path(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
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
        Ok((all, more))
    }

    async fn create_multipart(&self, key: &str, _attrs: &BlobAttrs) -> Result<String> {
        self.path(key)?;
        let id = random_hex(32);
        let dir = self.upload_dir(&id)?;
        create_dirs(&dir)?;
        Self::write_part_file(&dir.join("key"), key.as_bytes())?;
        fsync_dir(&dir)?;
        Ok(id)
    }

    async fn upload_part(&self, key: &str, upload_id: &str, part: u32, body: ByteStream, len: u64) -> Result<UploadedPart> {
        let dir = self.upload_dir(upload_id)?;
        let owner = std::fs::read_to_string(dir.join("key")).with_context(|| format!("no multipart upload {upload_id}"))?;
        anyhow::ensure!(owner == key, "upload {upload_id} is for another key");
        let (tmp, size, crc32c) = self.write_tmp(body, len).await?;
        let dst = dir.join(format!("{part:05}"));
        if let Err(e) = std::fs::rename(&tmp, &dst) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        fsync_dir(&dir)?;
        Ok(UploadedPart { etag: format!("{crc32c:08x}-{size}"), size, crc32c })
    }

    async fn upload_part_copy(&self, key: &str, upload_id: &str, part: u32, src_key: &str, range: Option<Range<u64>>) -> Result<UploadedPart> {
        let Some((len, body)) = self.get(src_key, range).await? else { bail!("copy source {src_key} not found") };
        self.upload_part(key, upload_id, part, body, len).await
    }

    async fn complete_multipart(&self, key: &str, upload_id: &str, parts: &[(u32, String)]) -> Result<()> {
        let dir = self.upload_dir(upload_id)?;
        let owner = std::fs::read_to_string(dir.join("key")).with_context(|| format!("no multipart upload {upload_id}"))?;
        anyhow::ensure!(owner == key, "upload {upload_id} is for another key");
        let dst = self.path(key)?;
        let tmp = self.tmp_path();
        let parts = parts.to_vec();
        let dir2 = dir.clone();
        let tmp2 = tmp.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<()> {
            let mut out = std::fs::File::create(&tmp2)?;
            for (n, etag) in &parts {
                let path = dir2.join(format!("{n:05}"));
                let mut f = std::fs::File::open(&path).with_context(|| format!("part {n} missing"))?;
                let mut crc = 0u32;
                let mut size = 0u64;
                let mut buf = vec![0u8; CHUNK];
                loop {
                    let r = f.read(&mut buf)?;
                    if r == 0 {
                        break;
                    }
                    crc = crc32c::crc32c_append(crc, &buf[..r]);
                    size += r as u64;
                    out.write_all(&buf[..r])?;
                }
                anyhow::ensure!(*etag == format!("{crc:08x}-{size}"), "part {n} doesn't match its etag");
            }
            out.sync_all()?;
            Ok(())
        })
        .await?;
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
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
}
