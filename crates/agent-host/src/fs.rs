//! Files for the canvas: list a directory, read a file (agent-proto `fs.list`, `fs.read`).
//!
//! A path inside a project is resolved against the project's directory and refused when it
//! leaves it, by `..` or through a symlink. Reads stop at 1 MiB; binary files and bigger ones
//! answer with `text: None` and their size.

use agent_proto::{FileContent, FsEntry};
use std::path::{Component, Path, PathBuf};
use tokio::io::AsyncReadExt;

/// Files bigger than this come back without text.
pub const READ_CAP: u64 = 1 << 20;
/// A listing stops after this many entries.
pub const LIST_CAP: usize = 5000;

#[derive(Debug, PartialEq, Eq)]
pub enum FsError {
    /// The path leaves the project.
    Escapes,
    NotFound,
    Other(String),
}

impl From<std::io::Error> for FsError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound { FsError::NotFound } else { FsError::Other(e.to_string()) }
    }
}

/// `rel` inside `root` (a leading `/` is the project's top). The answer is the real path, with
/// symlinks followed, and is always inside `root`'s real path.
pub async fn resolve(root: &Path, rel: &str) -> Result<PathBuf, FsError> {
    let mut lexical = PathBuf::new();
    for component in Path::new(rel.trim_start_matches('/')).components() {
        match component {
            Component::Normal(part) => lexical.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !lexical.pop() {
                    return Err(FsError::Escapes);
                }
            }
            Component::RootDir | Component::Prefix(_) => return Err(FsError::Escapes),
        }
    }
    let root = tokio::fs::canonicalize(root).await?;
    let real = tokio::fs::canonicalize(root.join(&lexical)).await?;
    if !real.starts_with(&root) {
        return Err(FsError::Escapes);
    }
    Ok(real)
}

/// A directory's entries: directories first, then by name; at most [`LIST_CAP`].
pub async fn list(dir: &Path) -> Result<Vec<FsEntry>, FsError> {
    let mut read = tokio::fs::read_dir(dir).await?;
    let mut entries = Vec::new();
    while let Some(entry) = read.next_entry().await? {
        if entries.len() >= LIST_CAP {
            break;
        }
        // Follow symlinks for the kind and size; a dangling one is listed as a file.
        let meta = match tokio::fs::metadata(entry.path()).await {
            Ok(meta) => Some(meta),
            Err(_) => entry.metadata().await.ok(),
        };
        let dir = meta.as_ref().is_some_and(|m| m.is_dir());
        entries.push(FsEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            dir,
            size: if dir { 0 } else { meta.map_or(0, |m| m.len()) },
        });
    }
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

/// A file's text (UTF-8, at most [`READ_CAP`]), or just its size.
pub async fn read(path: &Path, shown_as: String) -> Result<FileContent, FsError> {
    let meta = tokio::fs::metadata(path).await?;
    if meta.is_dir() {
        return Err(FsError::Other("that's a directory".into()));
    }
    let size = meta.len();
    let mut text = None;
    if size <= READ_CAP {
        let mut buf = Vec::new();
        tokio::fs::File::open(path).await?.take(READ_CAP + 1).read_to_end(&mut buf).await?;
        if buf.len() as u64 <= READ_CAP && !buf.contains(&0) {
            text = String::from_utf8(buf).ok();
        }
    }
    Ok(FileContent { path: shown_as, text, size })
}
