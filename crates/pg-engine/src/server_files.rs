//! Server-side file access (`pg_read_file`, `pg_stat_file`, `pg_ls_dir`, server-side
//! `lo_import` / `lo_export`, `COPY ... FROM / TO '<file>'`), opt in and confined to one
//! directory.
//!
//! By default the engine refuses these for every role: reading or writing the host's files
//! would expose the machine to its tenants. A server started with `--allow-server-files <dir>`
//! (`EngineOptions::server_files`) lets the roles Postgres allows use them, inside `<dir>`
//! only: the directory stands for Postgres's data directory, relative paths are resolved
//! against it and every path goes through Postgres's check for a role without
//! `pg_read_server_files` (genfile.c `convert_and_check_filename`), whatever the role. On top
//! of that lexical check, a path whose real location (symbolic links resolved) is outside the
//! directory is refused the same way, which Postgres doesn't do. For development and testing
//! (the regression suite runs with it); `COPY ... PROGRAM` stays refused.

use crate::error::{Error, Result, code};
use std::path::{Path, PathBuf};

/// The predefined roles that may use server files (pg_authid.dat).
pub const PG_READ_SERVER_FILES: u32 = 4569;
pub const PG_WRITE_SERVER_FILES: u32 = 4570;
pub const PG_EXECUTE_SERVER_PROGRAM: u32 = 4571;

/// The directory server files are confined to.
#[derive(Debug)]
pub struct ServerFiles {
    /// As Postgres keeps DataDir: absolute, canonicalize_path'd.
    root: String,
    /// Its real location (symbolic links resolved).
    real: PathBuf,
}

impl ServerFiles {
    /// `dir` must be an existing directory (relative to the current one, if relative).
    pub fn new(dir: &Path) -> std::io::Result<ServerFiles> {
        let abs = std::path::absolute(dir)?;
        let real = std::fs::canonicalize(&abs)?;
        if !real.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("{} is not a directory", abs.display()),
            ));
        }
        let Some(root) = abs.to_str() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not valid UTF-8", abs.display()),
            ));
        };
        Ok(ServerFiles {
            root: canonicalize_path(root),
            real,
        })
    }

    /// The directory, as given (absolute).
    pub fn dir(&self) -> &str {
        &self.root
    }

    /// Where `name` is on the host: convert_and_check_filename with the directory as the
    /// data directory (and its current directory), then the real-path check. Errors are
    /// Postgres's for a disallowed path.
    pub fn resolve(&self, name: &str) -> Result<PathBuf> {
        let name = canonicalize_path(name);
        let absolute = name.starts_with('/');
        let refused = || {
            Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                if absolute {
                    "absolute path not allowed"
                } else {
                    "path must be in or below the data directory"
                },
            )
        };
        let path = if absolute {
            if !path_is_prefix_of_path(&self.root, &name) {
                return Err(refused());
            }
            PathBuf::from(&name)
        } else if name.is_empty() {
            // Opening "" fails as Postgres's open("") does.
            return Ok(PathBuf::new());
        } else if path_contains_parent_reference(&name) {
            return Err(refused());
        } else {
            Path::new(&self.root).join(&name)
        };
        // The deepest part of the path that exists, with its links resolved, must be inside.
        let mut probe = path.as_path();
        loop {
            if std::fs::symlink_metadata(probe).is_ok() {
                match std::fs::canonicalize(probe) {
                    Ok(real) if real.starts_with(&self.real) => break,
                    // Outside, or a dangling link (which a write would follow).
                    _ => return Err(refused()),
                }
            }
            match probe.parent() {
                Some(p) => probe = p,
                None => return Err(refused()),
            }
        }
        Ok(path)
    }
}

/// canonicalize_path (src/port/path.c): no trailing or doubled separators, no `.`
/// components, `..` folded where it can be (kept at the start of a relative path, dropped
/// at the root of an absolute one); an empty path stays empty, one that folds away is `.`.
pub fn canonicalize_path(p: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    let absolute = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    // Leading `..`s of a relative path.
    let mut ups = 0;
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." if !parts.is_empty() => {
                parts.pop();
            }
            ".." if !absolute => ups += 1,
            ".." => {}
            c => parts.push(c),
        }
    }
    let mut out: Vec<&str> = std::iter::repeat_n("..", ups).collect();
    out.extend(parts);
    let joined = out.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// path_is_prefix_of_path: `path2` is `path1` or below it.
fn path_is_prefix_of_path(path1: &str, path2: &str) -> bool {
    path2
        .strip_prefix(path1)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/') || path1 == "/")
}

/// path_contains_parent_reference, for a canonicalized path.
fn path_contains_parent_reference(p: &str) -> bool {
    p == ".." || p.starts_with("../")
}

/// An I/O error as Postgres reports it: errcode_for_file_access's SQLSTATE, and `%m` (the
/// reference server's strerror, musl's) after `msg`.
pub fn file_error(e: &std::io::Error, msg: &str) -> Error {
    let (state, text) = errno_text(e);
    Error::new(state, format!("{msg}: {text}"))
}

/// Whether the error is ENOENT.
pub fn is_enoent(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

/// Whether the error is ENOENT or EACCES (COPY's hint about psql's \copy).
pub fn is_enoent_or_eacces(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(e.kind(), NotFound | PermissionDenied) && e.raw_os_error() != Some(1)
}

fn errno_text(e: &std::io::Error) -> (&'static str, String) {
    use std::io::ErrorKind::*;
    // EPERM is PermissionDenied too.
    if e.raw_os_error() == Some(1) {
        return (code::INSUFFICIENT_PRIVILEGE, "Operation not permitted".into());
    }
    // ELOOP (its ErrorKind isn't stable).
    const ELOOP: i32 = if cfg!(target_os = "linux") { 40 } else { 62 };
    if e.raw_os_error() == Some(ELOOP) {
        return (code::INTERNAL_ERROR, "Symbolic link loop".into());
    }
    let (state, text) = match e.kind() {
        NotFound => (code::UNDEFINED_FILE, "No such file or directory"),
        PermissionDenied => (code::INSUFFICIENT_PRIVILEGE, "Permission denied"),
        ReadOnlyFilesystem => (code::INSUFFICIENT_PRIVILEGE, "Read-only file system"),
        AlreadyExists => ("58P02", "File exists"),
        NotADirectory => (code::WRONG_OBJECT_TYPE, "Not a directory"),
        IsADirectory => (code::WRONG_OBJECT_TYPE, "Is a directory"),
        DirectoryNotEmpty => (code::WRONG_OBJECT_TYPE, "Directory not empty"),
        StorageFull => ("53100", "No space left on device"),
        OutOfMemory => (code::OUT_OF_MEMORY, "Out of memory"),
        InvalidFilename => ("58P03", "Filename too long"),
        InvalidInput => (code::INTERNAL_ERROR, "Invalid argument"),
        _ => {
            let s = e.to_string();
            let s = match s.find(" (os error") {
                Some(i) => s[..i].to_string(),
                None => s,
            };
            return (code::INTERNAL_ERROR, s);
        }
    };
    (state, text.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize() {
        for (i, o) in [
            ("", ""),
            (".", "."),
            ("./", "."),
            ("a/./b//c/", "a/b/c"),
            ("a/../b", "b"),
            ("a/..", "."),
            ("../a", "../a"),
            ("../dir/..", ".."),
            ("a/../../b", "../b"),
            ("/", "/"),
            ("/..", "/"),
            ("/../a/./b/..", "/a"),
            ("//a//b/", "/a/b"),
        ] {
            assert_eq!(canonicalize_path(i), o, "{i:?}");
        }
    }

    #[test]
    fn prefix() {
        assert!(path_is_prefix_of_path("/d", "/d"));
        assert!(path_is_prefix_of_path("/d", "/d/x"));
        assert!(!path_is_prefix_of_path("/d", "/dx"));
        assert!(path_is_prefix_of_path("/", "/x"));
    }
}
