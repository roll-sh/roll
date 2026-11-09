//! The bundle isolated work travels in (docs/cloud/README.md, "Isolated work"): one gzipped tar
//! with the run's environment (`env`) and the tree from `Code::checkout` (`tree/…`). rolld packs it
//! and uploads it to S3 under a per-run key; the task's `fetch` container downloads it through a
//! presigned URL and unpacks it here.
//!
//! The tree is a customer's, so unpacking follows `Store::materialize`'s rules for hostile trees:
//! every path is relative with plain components, nothing that exists already is written again (a
//! repeated name fails the run), symlinks are made but never followed (no later entry may go
//! through one), only files, directories and symlinks are allowed, and sizes are bounded by the
//! bytes actually read, never by what a header claims.
//!
//! The format is versioned by its first entry (`roll-bundle-1`): a fetcher of another version
//! refuses the bundle instead of guessing.

use anyhow::{Context, bail, ensure};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

/// The first entry of every bundle.
pub const VERSION: &str = "roll-bundle-1";
/// The env file inside the bundle, and the name `fetch` writes it under.
pub const ENV: &str = "env";
const TREE: &str = "tree/";
const MAX_ENV_BYTES: u64 = 1 << 20;

/// What a bundle may hold. The defaults are code-git's `Limits` (a pushed object at most
/// 100 MiB, a pack at most 2 GiB unpacked), so any tree roll stores fits.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_bytes: u64,
    pub max_file_bytes: u64,
    pub max_entries: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_bytes: 2 << 30, max_file_bytes: 100 << 20, max_entries: 1_000_000 }
    }
}

/// An environment variable's name: what a POSIX shell can export.
pub fn valid_name(k: &str) -> bool {
    let mut b = k.bytes();
    matches!(b.next(), Some(c) if c == b'_' || c.is_ascii_alphabetic()) && b.all(|c| c == b'_' || c.is_ascii_alphanumeric())
}

/// The env as a file `sh` sources: `export K='v'` lines, values single-quoted.
pub fn env_file(env: &BTreeMap<String, String>) -> anyhow::Result<String> {
    let mut out = String::new();
    for (k, v) in env {
        ensure!(valid_name(k), "{k:?} isn't an environment variable name");
        ensure!(!v.contains('\0'), "{k} holds a NUL byte");
        out.push_str(&format!("export {k}='{}'\n", v.replace('\'', "'\\''")));
    }
    Ok(out)
}

fn append(b: &mut tar::Builder<impl Write>, path: &str, kind: tar::EntryType, mode: u32, data: &[u8]) -> io::Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(kind);
    h.set_mode(mode);
    h.set_size(data.len() as u64);
    h.set_mtime(0);
    b.append_data(&mut h, path, data)
}

/// Pack `env` and the tree at `tree` (if any) into `out`. The tree is walked with
/// `symlink_metadata` and never followed: a symlink goes in as a symlink, whatever it points at.
pub fn pack(out: impl Write, env: &BTreeMap<String, String>, tree: Option<&Path>, limits: Limits) -> anyhow::Result<()> {
    let gz = flate2::write::GzEncoder::new(out, flate2::Compression::fast());
    let mut b = tar::Builder::new(gz);
    b.follow_symlinks(false);
    append(&mut b, VERSION, tar::EntryType::Regular, 0o644, b"")?;
    append(&mut b, ENV, tar::EntryType::Regular, 0o600, env_file(env)?.as_bytes())?;
    if let Some(root) = tree {
        let mut budget = Budget::new(limits);
        pack_dir(&mut b, root, Path::new(""), &mut budget)?;
    }
    b.into_inner()?.finish()?.flush()?;
    Ok(())
}

fn pack_dir(b: &mut tar::Builder<impl Write>, root: &Path, rel: &Path, budget: &mut Budget) -> anyhow::Result<()> {
    let mut names: Vec<_> = fs::read_dir(root.join(rel))?.map(|e| e.map(|e| e.file_name())).collect::<Result<_, _>>()?;
    names.sort();
    for name in names {
        let rel = rel.join(&name);
        let path = root.join(&rel);
        let meta = fs::symlink_metadata(&path)?;
        let tar_path = format!("{TREE}{}", rel.to_str().with_context(|| format!("{} isn't UTF-8", rel.display()))?);
        budget.entry()?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_mode(0o777);
            h.set_size(0);
            b.append_link(&mut h, &tar_path, &target)?;
        } else if meta.is_dir() {
            append(b, &tar_path, tar::EntryType::Directory, 0o755, b"")?;
            pack_dir(b, root, &rel, budget)?;
        } else if meta.is_file() {
            budget.bytes(meta.len())?;
            let mode = if meta.permissions().mode() & 0o111 != 0 { 0o755 } else { 0o644 };
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mode(mode);
            h.set_size(meta.len());
            h.set_mtime(0);
            // Read through what was opened: a file that grows meanwhile can't overrun its header.
            let f = fs::File::open(&path)?;
            b.append_data(&mut h, &tar_path, f.take(meta.len()))?;
        }
        // Anything else (a socket, a device) isn't part of a tree.
    }
    Ok(())
}

struct Budget {
    limits: Limits,
    bytes: u64,
    entries: u64,
}

impl Budget {
    fn new(limits: Limits) -> Budget {
        Budget { limits, bytes: 0, entries: 0 }
    }

    fn entry(&mut self) -> anyhow::Result<()> {
        self.entries += 1;
        ensure!(self.entries <= self.limits.max_entries, "the tree has more than {} entries", self.limits.max_entries);
        Ok(())
    }

    fn bytes(&mut self, n: u64) -> anyhow::Result<()> {
        ensure!(n <= self.limits.max_file_bytes, "a file is larger than {} MiB", self.limits.max_file_bytes >> 20);
        self.bytes += n;
        ensure!(self.bytes <= self.limits.max_bytes, "the tree is larger than {} MiB", self.limits.max_bytes >> 20);
        Ok(())
    }
}

/// What [`unpack`] wrote.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Unpacked {
    pub files: u64,
    pub bytes: u64,
}

/// Unpack a bundle: the tree into `tree` (which must exist and be empty) and the env file to
/// `env_out`. Fails on the first entry that breaks a rule, leaving what it wrote so far (the
/// task fails, and its storage goes with it).
pub fn unpack(input: impl Read, tree: &Path, env_out: &Path, limits: Limits) -> anyhow::Result<Unpacked> {
    ensure!(fs::read_dir(tree).with_context(|| format!("opening {}", tree.display()))?.next().is_none(), "{} isn't empty", tree.display());
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(input));
    let mut budget = Budget::new(limits);
    let mut out = Unpacked::default();
    let mut first = true;
    let mut env_seen = false;
    for entry in a.entries()? {
        let mut entry = entry?;
        let raw = entry.path_bytes().into_owned();
        let name = std::str::from_utf8(&raw).context("an entry's name isn't UTF-8")?.to_string();
        let kind = entry.header().entry_type();
        if first {
            ensure!(name == VERSION && kind == tar::EntryType::Regular, "not a {VERSION} bundle (first entry {name:?})");
            first = false;
            continue;
        }
        if name == ENV {
            ensure!(!env_seen && kind == tar::EntryType::Regular, "a second env");
            env_seen = true;
            let mut text = Vec::new();
            (&mut entry).take(MAX_ENV_BYTES + 1).read_to_end(&mut text)?;
            ensure!(text.len() as u64 <= MAX_ENV_BYTES, "the env is larger than 1 MiB");
            write_new(env_out, &mut text.as_slice(), 0o644, u64::MAX)?;
            continue;
        }
        let rel = name.strip_prefix(TREE).with_context(|| format!("{name:?} is outside the tree"))?;
        let rel = relative(rel)?;
        budget.entry()?;
        let dest = tree.join(&rel);
        parents(tree, &rel)?;
        match kind {
            tar::EntryType::Directory => fs::create_dir(&dest).with_context(|| format!("{}: already exists", rel.display()))?,
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let mode = if entry.header().mode()? & 0o111 != 0 { 0o755 } else { 0o644 };
                let room = limits.max_file_bytes.min(limits.max_bytes - budget.bytes);
                let n = write_new(&dest, &mut entry, mode, room).with_context(|| rel.display().to_string())?;
                budget.bytes(n)?;
                out.files += 1;
                out.bytes += n;
            }
            tar::EntryType::Symlink => {
                let target = entry.link_name()?.with_context(|| format!("{}: a symlink without a target", rel.display()))?;
                std::os::unix::fs::symlink(&target, &dest).with_context(|| format!("{}: already exists", rel.display()))?;
            }
            other => bail!("{}: {other:?} entries aren't allowed in a tree", rel.display()),
        }
    }
    ensure!(!first, "an empty bundle");
    ensure!(env_seen, "the bundle has no env");
    Ok(out)
}

/// A tree path made only of normal components.
fn relative(p: &str) -> anyhow::Result<PathBuf> {
    let p = p.trim_end_matches('/');
    ensure!(!p.is_empty() && !p.contains('\0'), "an empty or NUL path");
    let path = Path::new(p);
    for c in path.components() {
        ensure!(matches!(c, Component::Normal(_)), "{p:?}: only plain relative paths are allowed");
    }
    Ok(path.to_path_buf())
}

/// Every parent of `rel` under `root` is a real directory (made if missing): never a symlink, so
/// nothing is written through one, wherever it points.
fn parents(root: &Path, rel: &Path) -> anyhow::Result<()> {
    let mut at = root.to_path_buf();
    let n = rel.components().count();
    for c in rel.components().take(n.saturating_sub(1)) {
        at.push(c);
        match fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => bail!("{}: goes through something that isn't a directory", rel.display()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&at)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Write a new file (`O_EXCL`, so neither an existing file nor a symlink is ever opened), at most
/// `room` bytes.
fn write_new(path: &Path, r: &mut impl Read, mode: u32, room: u64) -> anyhow::Result<u64> {
    let mut f = OpenOptions::new().write(true).create_new(true).mode(mode).open(path).context("already exists")?;
    let n = io::copy(&mut r.take(room.saturating_add(1)), &mut f)?;
    ensure!(n <= room, "larger than allowed");
    f.set_permissions(fs::Permissions::from_mode(mode))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> BTreeMap<String, String> {
        [("A".to_string(), "it's \"quoted\" $HOME".to_string()), ("ROLL_REPO".into(), "acme/web".into())].into()
    }

    #[test]
    fn a_tree_round_trips_with_its_env_and_symlinks() {
        let src = tempfile::tempdir().unwrap();
        fs::create_dir_all(src.path().join("a/b")).unwrap();
        fs::write(src.path().join("a/b/c.txt"), "hello").unwrap();
        fs::write(src.path().join("run.sh"), "#!/bin/sh").unwrap();
        fs::set_permissions(src.path().join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", src.path().join("out")).unwrap();
        let mut buf = Vec::new();
        pack(&mut buf, &env(), Some(src.path()), Limits::default()).unwrap();
        let dst = tempfile::tempdir().unwrap();
        let tree = dst.path().join("w");
        fs::create_dir(&tree).unwrap();
        let got = unpack(buf.as_slice(), &tree, &dst.path().join("env"), Limits::default()).unwrap();
        assert_eq!(got, Unpacked { files: 2, bytes: 14 });
        assert_eq!(fs::read_to_string(tree.join("a/b/c.txt")).unwrap(), "hello");
        assert_eq!(fs::metadata(tree.join("run.sh")).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::read_link(tree.join("out")).unwrap(), Path::new("/etc/passwd"));
        let e = fs::read_to_string(dst.path().join("env")).unwrap();
        assert_eq!(e, "export A='it'\\''s \"quoted\" $HOME'\nexport ROLL_REPO='acme/web'\n");
        // sh reads back exactly what went in.
        let out = std::process::Command::new("sh").arg("-c").arg(format!(". {}; printf %s \"$A\"", dst.path().join("env").display())).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "it's \"quoted\" $HOME");
        assert!(env_file(&[("1X".to_string(), String::new())].into()).is_err());
    }
}
