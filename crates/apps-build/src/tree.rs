//! Source trees: what the providers read.
//!
//! Paths are relative and `/`-separated, with no leading slash, `.` or `..`. A [`DirTree`] is a
//! tree a user pushed, so it's hostile: it never follows a symlink (a path through one reads as
//! absent), never leaves its root, and bounds what one call reads.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;

/// A source tree.
pub trait Tree {
    /// The bytes of the regular file at `path`, or `None`.
    fn read(&self, path: &str) -> Option<Vec<u8>>;
    /// Whether `path` is a file or a directory.
    fn exists(&self, path: &str) -> bool;
    /// The names directly under `dir` (`""` is the root), sorted. Directories end in `/`.
    fn list(&self, dir: &str) -> Vec<String>;
}

/// The most [`DirTree::read`] returns: the first MiB of a bigger file.
pub const MAX_READ: u64 = 1 << 20;
/// The most entries [`DirTree::list`] looks at in one directory.
pub const MAX_LIST: usize = 1000;

/// Whether `path` is a relative path that stays inside the tree.
pub(crate) fn valid(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains(['\\', '\0']))
}

/// A directory on disk.
pub struct DirTree {
    root: PathBuf,
}

impl DirTree {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `path` under the root with its metadata, when no part of it is a symlink.
    fn resolve(&self, path: &str) -> Option<(PathBuf, fs::Metadata)> {
        if path.is_empty() {
            return Some((self.root.clone(), fs::metadata(&self.root).ok()?));
        }
        if !valid(path) {
            return None;
        }
        let mut full = self.root.clone();
        let mut meta = None;
        for part in path.split('/') {
            full.push(part);
            let m = fs::symlink_metadata(&full).ok()?;
            if m.file_type().is_symlink() {
                return None;
            }
            meta = Some(m);
        }
        Some((full, meta?))
    }
}

impl Tree for DirTree {
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        let (full, meta) = self.resolve(path)?;
        if !meta.is_file() {
            return None;
        }
        let file = fs::File::open(full).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut buf = Vec::new();
        file.take(MAX_READ).read_to_end(&mut buf).ok()?;
        Some(buf)
    }

    fn exists(&self, path: &str) -> bool {
        self.resolve(path)
            .is_some_and(|(_, m)| m.is_file() || m.is_dir())
    }

    fn list(&self, dir: &str) -> Vec<String> {
        let Some((full, meta)) = self.resolve(dir) else {
            return Vec::new();
        };
        if !meta.is_dir() {
            return Vec::new();
        }
        let Ok(entries) = fs::read_dir(full) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .take(MAX_LIST)
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let kind = entry.file_type().ok()?;
                let name = entry.file_name().into_string().ok()?;
                if kind.is_dir() {
                    Some(format!("{name}/"))
                } else if kind.is_file() {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        names.sort();
        names
    }
}

/// Files in memory, path to content: `POST /v1/apps/plan {files}` and tests.
#[derive(Clone, Debug, Default)]
pub struct MapTree(pub BTreeMap<String, String>);

impl Tree for MapTree {
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        valid(path)
            .then(|| self.0.get(path))
            .flatten()
            .map(|s| s.as_bytes().to_vec())
    }

    fn exists(&self, path: &str) -> bool {
        valid(path)
            && (self.0.contains_key(path)
                || self
                    .0
                    .range(format!("{path}/")..)
                    .next()
                    .is_some_and(|(k, _)| k.starts_with(&format!("{path}/"))))
    }

    fn list(&self, dir: &str) -> Vec<String> {
        if !dir.is_empty() && !valid(dir) {
            return Vec::new();
        }
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let mut names: Vec<String> = self
            .0
            .range(prefix.clone()..)
            .map(|(k, _)| k)
            .take_while(|k| k.starts_with(&prefix))
            .filter_map(|k| {
                let rest = &k[prefix.len()..];
                let name = match rest.split_once('/') {
                    Some((d, _)) => format!("{d}/"),
                    None => rest.to_string(),
                };
                (!name.is_empty() && name != "/").then_some(name)
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }
}
