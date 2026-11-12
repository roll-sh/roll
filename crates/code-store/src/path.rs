//! Repository paths: `/`-separated, relative, UTF-8.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub const MAX_PATH: usize = 4096;

/// A validated file path inside a repository: no leading or trailing `/`, no empty, `.` or `..`
/// components, no `.git` component (any case), no NUL.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath(String);

impl RepoPath {
    pub fn new(s: impl Into<String>) -> anyhow::Result<RepoPath> {
        let s = s.into();
        if s.is_empty() || s.len() > MAX_PATH {
            anyhow::bail!("path must be 1-{MAX_PATH} bytes");
        }
        for c in s.split('/') {
            validate_component(c).map_err(|e| anyhow::anyhow!("invalid path {s:?}: {e}"))?;
        }
        Ok(RepoPath(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Final component.
    pub fn name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    pub fn join(&self, name: &str) -> anyhow::Result<RepoPath> {
        validate_component(name)?;
        Ok(RepoPath(format!("{}/{}", self.0, name)))
    }

    /// True if `self` is `dir` or lies under it (`dir` empty matches everything).
    pub fn starts_with_dir(&self, dir: &str) -> bool {
        let dir = dir.trim_matches('/');
        dir.is_empty() || self.0 == dir || (self.0.starts_with(dir) && self.0.as_bytes().get(dir.len()) == Some(&b'/'))
    }
}

/// One path component (a git tree entry name).
pub fn validate_component(c: &str) -> anyhow::Result<()> {
    if c.is_empty() {
        anyhow::bail!("empty path component");
    }
    if c == "." || c == ".." {
        anyhow::bail!("`.` and `..` are not allowed");
    }
    if c.eq_ignore_ascii_case(".git") {
        anyhow::bail!("`.git` is reserved");
    }
    if c.bytes().any(|b| b == 0 || b == b'/') {
        anyhow::bail!("NUL and `/` are not allowed in a component");
    }
    Ok(())
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl Serialize for RepoPath {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        RepoPath::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation() {
        for ok in ["a", "a/b", "src/main.rs", ".github/workflows/x.yml", "a..b", ".gitignore"] {
            RepoPath::new(ok).unwrap();
        }
        for bad in ["", "/a", "a/", "a//b", "../a", "a/./b", ".git/config", "x/.GIT/y", "a\0b"] {
            assert!(RepoPath::new(bad).is_err(), "{bad:?}");
        }
        let p = RepoPath::new("src/lib/x.rs").unwrap();
        assert!(p.starts_with_dir("src"));
        assert!(p.starts_with_dir("src/lib/"));
        assert!(!p.starts_with_dir("sr"));
        assert!(p.starts_with_dir(""));
        assert_eq!(p.name(), "x.rs");
    }
}
