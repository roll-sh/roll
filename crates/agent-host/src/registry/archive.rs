//! Downloading and unpacking an agent's archive, in Rust (no system `tar` or `unzip`): T3 Code's
//! `installBinary` (`AcpRegistrySupport.ts`), stricter about what an archive may hold.
//!
//! - A download is streamed to disk with its SHA-256, at most [`Limits::download`] bytes; a hash the
//!   registry lists must match, or nothing is kept.
//! - An archive (`.tar.gz`/`.tgz`, `.tar.bz2`/`.tbz2`, `.zip` stored or deflated) may hold only
//!   regular files and directories: a symlink, hardlink, device or FIFO, an absolute path, a `..`,
//!   or a path listed twice refuses the whole archive. Entries and unpacked bytes are capped.
//! - Anything else is the executable itself.
//! - Files are created new (never through something that already exists) under the extraction
//!   root, and the command must be a regular file inside it.

use super::index::command_path;
use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Bytes downloaded (T3's 1 GiB).
    pub download: u64,
    /// Entries in an archive.
    pub entries: usize,
    /// Bytes unpacked from an archive.
    pub unpacked: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { download: 1 << 30, entries: 20_000, unpacked: 2 << 30 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    TarGz,
    TarBz2,
    Zip,
    Raw,
}

/// T3's `archiveKind`: by the URL path's suffix.
pub fn kind(url: &str) -> Kind {
    let path = reqwest::Url::parse(url).map(|u| u.path().to_ascii_lowercase()).unwrap_or_default();
    if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
        Kind::TarGz
    } else if path.ends_with(".tar.bz2") || path.ends_with(".tbz2") {
        Kind::TarBz2
    } else if path.ends_with(".zip") {
        Kind::Zip
    } else {
        Kind::Raw
    }
}

/// Downloads `url` into `dest` (a new file), checking `sha256` (lowercase hex) when given.
pub async fn download(client: &reqwest::Client, url: &str, dest: &Path, sha256: Option<&str>, limit: u64) -> anyhow::Result<()> {
    let mut res = client.get(url).send().await.with_context(|| format!("downloading {url}"))?;
    if !res.status().is_success() {
        bail!("downloading {url}: {}", res.status());
    }
    if res.content_length().is_some_and(|n| n > limit) {
        bail!("{url} is over {limit} bytes");
    }
    let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(dest).await?;
    let mut hash = Sha256::new();
    let mut got = 0u64;
    while let Some(chunk) = res.chunk().await.with_context(|| format!("downloading {url}"))? {
        got += chunk.len() as u64;
        if got > limit {
            bail!("{url} is over {limit} bytes");
        }
        hash.update(&chunk);
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    if let Some(want) = sha256 {
        let have = hex::encode(hash.finalize());
        if have != want {
            bail!("the download doesn't match the registry's SHA-256 (expected {want}, got {have})");
        }
    }
    Ok(())
}

/// Unpacks `archive` into `root` (an empty directory) and returns the command's path in it,
/// executable. Blocking.
pub fn unpack(kind: Kind, archive: &Path, root: &Path, cmd: &[String], limits: Limits) -> anyhow::Result<PathBuf> {
    match kind {
        Kind::Raw => {
            let target = root.join(cmd.iter().collect::<PathBuf>());
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(archive, &target)?;
        }
        Kind::TarGz => untar(flate2::read::GzDecoder::new(std::fs::File::open(archive)?), root, limits)?,
        Kind::TarBz2 => untar(bzip2::read::BzDecoder::new(std::fs::File::open(archive)?), root, limits)?,
        Kind::Zip => unzip(std::fs::File::open(archive)?, root, limits)?,
    }
    executable(root, cmd)
}

/// The command inside `root`: a regular file there (not through a link), made executable.
fn executable(root: &Path, cmd: &[String]) -> anyhow::Result<PathBuf> {
    let path = root.join(cmd.iter().collect::<PathBuf>());
    let meta = std::fs::symlink_metadata(&path).map_err(|_| anyhow::anyhow!("the archive has no {}", cmd.join("/")))?;
    if !meta.is_file() {
        bail!("{} in the archive isn't a regular file", cmd.join("/"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    let real = std::fs::canonicalize(&path)?;
    if !real.starts_with(std::fs::canonicalize(root)?) {
        bail!("{} resolves outside the agent's folder", cmd.join("/"));
    }
    Ok(path)
}

/// What one archive entry is.
enum Entry {
    Dir,
    File { executable: bool },
}

/// Writes entries under `root`, holding the archive to the rules in the module doc.
struct Writer<'a> {
    root: &'a Path,
    limits: Limits,
    seen: HashSet<Vec<String>>,
    entries: usize,
    unpacked: u64,
}

impl<'a> Writer<'a> {
    fn new(root: &'a Path, limits: Limits) -> Self {
        Writer { root, limits, seen: HashSet::new(), entries: 0, unpacked: 0 }
    }

    fn put(&mut self, name: &str, entry: Entry, body: &mut dyn Read) -> anyhow::Result<()> {
        self.entries += 1;
        if self.entries > self.limits.entries {
            bail!("the archive has more than {} entries", self.limits.entries);
        }
        // The archive's own root (`./`) is the extraction root.
        if matches!(entry, Entry::Dir) && matches!(name.trim_end_matches('/'), "" | ".") {
            return Ok(());
        }
        let segments = command_path(name).ok_or_else(|| anyhow::anyhow!("the archive holds an unsafe path {name:?}"))?;
        if !self.seen.insert(segments.clone()) {
            bail!("the archive lists {name:?} twice");
        }
        let path = self.root.join(segments.iter().collect::<PathBuf>());
        match entry {
            Entry::Dir => std::fs::create_dir_all(&path)?,
            Entry::File { executable } => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.mode(if executable { 0o755 } else { 0o644 });
                }
                #[cfg(not(unix))]
                let _ = executable;
                let mut file = opts.open(&path).with_context(|| format!("unpacking {name:?}"))?;
                let left = self.limits.unpacked - self.unpacked;
                let copied = std::io::copy(&mut (&mut *body).take(left + 1), &mut file)?;
                if copied > left {
                    bail!("the archive unpacks to more than {} bytes", self.limits.unpacked);
                }
                self.unpacked += copied;
                file.flush()?;
            }
        }
        Ok(())
    }
}

fn untar(reader: impl Read, root: &Path, limits: Limits) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(reader);
    let mut w = Writer::new(root, limits);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        let name = String::from_utf8(entry.path_bytes().into_owned()).map_err(|_| anyhow::anyhow!("the archive holds a path that isn't UTF-8"))?;
        let what = match kind {
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                Entry::File { executable: entry.header().mode().is_ok_and(|m| m & 0o111 != 0) }
            }
            tar::EntryType::Directory => Entry::Dir,
            // PAX metadata (the tar crate applies what it needs).
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader => continue,
            other => bail!("the archive holds {name:?}, which is a {other:?}, not a file or folder"),
        };
        w.put(&name, what, &mut entry)?;
    }
    Ok(())
}

fn unzip(file: std::fs::File, root: &Path, limits: Limits) -> anyhow::Result<()> {
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).context("reading the zip")?;
    if archive.len() > limits.entries {
        bail!("the archive has more than {} entries", limits.entries);
    }
    let mut w = Writer::new(root, limits);
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().map_err(|e| anyhow::anyhow!("the zip holds a name that isn't UTF-8: {e}"))?.into_owned();
        let mode = entry.unix_mode();
        // Only regular files and folders: a symlink (or a device) is refused.
        let file_type = mode.map(|m| m & 0o170000);
        let what = if entry.is_dir() && file_type.is_none_or(|t| t == 0o040000) {
            Entry::Dir
        } else if entry.is_file() && file_type.is_none_or(|t| t == 0o100000) && !entry.is_symlink() {
            Entry::File { executable: mode.is_some_and(|m| m & 0o111 != 0) }
        } else {
            bail!("the archive holds {name:?}, which isn't a file or folder");
        };
        w.put(&name, what, &mut entry)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> Vec<String> {
        command_path(s).unwrap()
    }

    /// A tar with whatever headers the test wants (the tar crate's builder refuses `..` itself).
    fn tar_of(entries: &[(&str, tar::EntryType, &[u8], Option<&str>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, kind, body, link) in entries {
            let mut h = tar::Header::new_gnu();
            {
                let raw = h.as_old_mut();
                raw.name[..name.len()].copy_from_slice(name.as_bytes());
            }
            h.set_entry_type(*kind);
            h.set_size(body.len() as u64);
            h.set_mode(0o755);
            if let Some(l) = link {
                h.set_link_name(l).unwrap();
            }
            h.set_cksum();
            out.extend_from_slice(h.as_bytes());
            out.extend_from_slice(body);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        out.extend_from_slice(&[0; 1024]);
        out
    }

    fn gz(bytes: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn unpack_bytes(kind: Kind, bytes: &[u8], command: &str, limits: Limits) -> (tempfile::TempDir, anyhow::Result<PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        std::fs::write(&archive, bytes).unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let got = unpack(kind, &archive, &root, &cmd(command), limits);
        (dir, got)
    }

    use tar::EntryType::{Directory as D, Regular as F};

    #[test]
    fn unpacks_a_tar_gz_and_finds_the_command() {
        let t = tar_of(&[("./", D, b"", None), ("bin/", D, b"", None), ("bin/agent", F, b"#!/bin/sh\n", None), ("README", F, b"hi", None)]);
        let (_dir, got) = unpack_bytes(Kind::TarGz, &gz(&t), "./bin/agent", Limits::default());
        let path = got.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"#!/bin/sh\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o755);
        }
    }

    #[test]
    fn refuses_unsafe_tar_entries() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("traversal", tar_of(&[("../evil", F, b"x", None), ("agent", F, b"x", None)])),
            ("nested traversal", tar_of(&[("a/../../evil", F, b"x", None)])),
            ("absolute", tar_of(&[("/tmp/evil", F, b"x", None)])),
            ("symlink", tar_of(&[("agent", tar::EntryType::Symlink, b"", Some("/bin/sh"))])),
            ("hardlink", tar_of(&[("x", F, b"x", None), ("agent", tar::EntryType::Link, b"", Some("x"))])),
            ("device", tar_of(&[("agent", tar::EntryType::Char, b"", None)])),
            ("fifo", tar_of(&[("agent", tar::EntryType::Fifo, b"", None)])),
            ("duplicate", tar_of(&[("agent", F, b"one", None), ("agent", F, b"two", None)])),
            ("missing command", tar_of(&[("other", F, b"x", None)])),
            ("command is a folder", tar_of(&[("agent/", D, b"", None)])),
        ];
        for (what, t) in cases {
            let (dir, got) = unpack_bytes(Kind::TarGz, &gz(&t), "agent", Limits::default());
            assert!(got.is_err(), "{what}: {got:?}");
            assert!(!dir.path().parent().unwrap().join("evil").exists() && !Path::new("/tmp/evil").exists(), "{what} escaped");
        }
    }

    #[test]
    fn caps_entries_and_unpacked_bytes() {
        let many: Vec<(String, tar::EntryType, &[u8], Option<&str>)> = (0..5).map(|i| (format!("f{i}"), F, &b"x"[..], None)).collect();
        let refs: Vec<_> = many.iter().map(|(n, k, b, l)| (n.as_str(), *k, *b, *l)).collect();
        let t = tar_of(&refs);
        let (_d, got) = unpack_bytes(Kind::TarGz, &gz(&t), "f0", Limits { entries: 4, ..Limits::default() });
        assert!(got.unwrap_err().to_string().contains("entries"));
        let (_d, got) = unpack_bytes(Kind::TarGz, &gz(&t), "f0", Limits { entries: 5, ..Limits::default() });
        assert!(got.is_ok());
        let big = tar_of(&[("agent", F, &[7u8; 4096], None)]);
        let (_d, got) = unpack_bytes(Kind::TarGz, &gz(&big), "agent", Limits { unpacked: 4095, ..Limits::default() });
        assert!(got.unwrap_err().to_string().contains("more than"));
    }

    #[test]
    fn unpacks_a_tar_bz2() {
        let t = tar_of(&[("agent", F, b"bz", None)]);
        let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
        e.write_all(&t).unwrap();
        let (_d, got) = unpack_bytes(Kind::TarBz2, &e.finish().unwrap(), "agent", Limits::default());
        assert_eq!(std::fs::read(got.unwrap()).unwrap(), b"bz");
    }

    fn zip_of(f: impl FnOnce(&mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>)) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        f(&mut w);
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn unpacks_a_zip_and_refuses_unsafe_entries() {
        let opts = || zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).unix_permissions(0o755);
        let good = zip_of(|w| {
            w.add_directory("dist/", opts()).unwrap();
            w.start_file("dist/agent", opts()).unwrap();
            w.write_all(b"zipped").unwrap();
        });
        let (_d, got) = unpack_bytes(Kind::Zip, &good, "dist/agent", Limits::default());
        assert_eq!(std::fs::read(got.unwrap()).unwrap(), b"zipped");

        let symlink = zip_of(|w| w.add_symlink("agent", "/bin/sh", opts()).unwrap());
        let traversal = zip_of(|w| {
            w.start_file("../agent", opts()).unwrap();
            w.write_all(b"x").unwrap();
        });
        let absolute = zip_of(|w| {
            w.start_file("/agent", opts()).unwrap();
            w.write_all(b"x").unwrap();
        });
        for (what, bytes) in [("symlink", symlink), ("traversal", traversal), ("absolute", absolute)] {
            let (_d, got) = unpack_bytes(Kind::Zip, &bytes, "agent", Limits::default());
            assert!(got.is_err(), "{what}");
        }
    }

    #[test]
    fn a_raw_download_is_the_command() {
        let (_d, got) = unpack_bytes(Kind::Raw, b"\x7fELF", "bin/agent", Limits::default());
        assert_eq!(std::fs::read(got.unwrap()).unwrap(), b"\x7fELF");
    }

    #[test]
    fn kinds_by_suffix() {
        assert_eq!(kind("https://x/a.tar.gz"), Kind::TarGz);
        assert_eq!(kind("https://x/a.TGZ?x=1"), Kind::TarGz);
        assert_eq!(kind("https://x/a.tar.bz2"), Kind::TarBz2);
        assert_eq!(kind("https://x/a.zip"), Kind::Zip);
        assert_eq!(kind("https://x/sigit-linux-amd64"), Kind::Raw);
    }
}
