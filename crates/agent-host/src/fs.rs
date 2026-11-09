//! Files for the canvas: list a directory, read a file, write one (agent-proto `fs.list`,
//! `fs.read`, `fs.write`).
//!
//! A path inside a project is resolved against the project's directory and refused when it
//! leaves it, by `..` or through a symlink. Reads stop at 1 MiB; binary files and bigger ones
//! answer with `text: None` and their size. A write (T3's `projects.writeFile`, for "Save to
//! workspace") makes the folders it needs inside the root and replaces a regular file, never
//! a symlink or a folder.

use agent_proto::{FileContent, FsEntry};
use std::path::{Component, Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

/// `rel` as a path relative to its root: `.` dropped, `..` resolved, refused when it climbs out.
fn lexical(rel: &str) -> Result<PathBuf, FsError> {
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
    Ok(lexical)
}

/// `rel` inside `root` (a leading `/` is the project's top). The answer is the real path, with
/// symlinks followed, and is always inside `root`'s real path.
pub async fn resolve(root: &Path, rel: &str) -> Result<PathBuf, FsError> {
    let lexical = lexical(rel)?;
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

/// Writes `text` to `rel` inside `root` and answers the path written, relative to `root`. The
/// folders on the way are made when missing; the nearest one that exists must really be inside
/// `root`, and so must the folder once made (a symlinked folder can't lead out). The file itself
/// is opened without following a symlink.
pub async fn write(root: &Path, rel: &str, text: &str) -> Result<String, FsError> {
    let lexical = lexical(rel)?;
    let (Some(parent), Some(name)) = (lexical.parent(), lexical.file_name()) else {
        return Err(FsError::Other("name a file to write".into()));
    };
    let root = tokio::fs::canonicalize(root).await?;
    let mut existing = root.join(parent);
    loop {
        match tokio::fs::canonicalize(&existing).await {
            Ok(real) if real.starts_with(&root) => break,
            Ok(_) => return Err(FsError::Escapes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink exists: refuse it rather than make what it points at.
                if tokio::fs::symlink_metadata(&existing).await.is_ok() {
                    return Err(FsError::Escapes);
                }
                existing = existing.parent().map(Path::to_path_buf).ok_or(FsError::Escapes)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let dir = root.join(parent);
    tokio::fs::create_dir_all(&dir).await?;
    let dir = tokio::fs::canonicalize(&dir).await?;
    if !dir.starts_with(&root) {
        return Err(FsError::Escapes);
    }
    let path = dir.join(name);
    match tokio::fs::symlink_metadata(&path).await {
        Ok(meta) if meta.is_file() => {}
        Ok(meta) if meta.is_dir() => return Err(FsError::Other("that's a directory".into())),
        Ok(_) => return Err(FsError::Other("that's a link or a special file, not a file roll replaces".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut file = tokio::fs::OpenOptions::new().write(true).create(true).truncate(true).custom_flags(libc::O_NOFOLLOW).open(&path).await?;
    file.write_all(text.as_bytes()).await?;
    file.flush().await?;
    Ok(lexical.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_stay_inside_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        assert_eq!(write(&root, "docs/plans/fix.md", "# Fix\n").await.unwrap(), "docs/plans/fix.md");
        assert_eq!(std::fs::read_to_string(root.join("docs/plans/fix.md")).unwrap(), "# Fix\n");
        // Replacing a file is fine; `./` and a leading `/` are the root.
        assert_eq!(write(&root, "/./docs/plans/fix.md", "again\n").await.unwrap(), "docs/plans/fix.md");
        assert_eq!(std::fs::read_to_string(root.join("docs/plans/fix.md")).unwrap(), "again\n");

        assert_eq!(write(&root, "../out.md", "x").await, Err(FsError::Escapes));
        assert_eq!(write(&root, "docs/../../out.md", "x").await, Err(FsError::Escapes));
        assert!(write(&root, "", "x").await.is_err());
        assert!(write(&root, "docs", "x").await.is_err(), "a folder isn't replaced");

        // A symlinked folder that leads out, a dangling link, and a link as the file itself.
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        assert_eq!(write(&root, "out/plan.md", "x").await, Err(FsError::Escapes));
        assert_eq!(write(&root, "out/new/plan.md", "x").await, Err(FsError::Escapes));
        std::os::unix::fs::symlink(tmp.path().join("missing"), root.join("dangling")).unwrap();
        assert_eq!(write(&root, "dangling/plan.md", "x").await, Err(FsError::Escapes));
        std::os::unix::fs::symlink(outside.join("target.md"), root.join("link.md")).unwrap();
        assert!(write(&root, "link.md", "x").await.is_err());
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none(), "nothing was written outside");
    }
}
