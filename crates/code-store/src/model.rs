//! The canonical data model: changesets (Bonsai). A changeset lists the files it changes relative
//! to its first parent; it never points at a full tree, so creating one costs O(change).

use crate::ids::{ChangesetId, Oid};
use crate::path::RepoPath;
use bytes::Bytes;
use serde::Serialize;
use sha2::Digest;
use std::collections::BTreeMap;

/// File mode, as git knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Regular,
    Executable,
    Symlink,
    /// A submodule: the oid is a commit in another repository, not a blob here.
    Gitlink,
}

impl Mode {
    pub fn git(&self) -> &'static [u8] {
        match self {
            Mode::Regular => b"100644",
            Mode::Executable => b"100755",
            Mode::Symlink => b"120000",
            Mode::Gitlink => b"160000",
        }
    }

    /// Parse a git tree entry mode for a non-tree entry. Legacy modes (`100664`, …) map to
    /// the closest canonical one; the tree then doesn't round-trip by derivation and the import
    /// keeps the original objects.
    pub fn from_git(m: &[u8]) -> Option<Mode> {
        Some(match m {
            b"100644" | b"100664" | b"100640" | b"100600" | b"100444" => Mode::Regular,
            b"100755" | b"100775" | b"100700" => Mode::Executable,
            b"120000" => Mode::Symlink,
            b"160000" => Mode::Gitlink,
            _ if m.starts_with(b"100") => Mode::Regular,
            _ => return None,
        })
    }

    fn code(&self) -> u8 {
        match self {
            Mode::Regular => 0,
            Mode::Executable => 1,
            Mode::Symlink => 2,
            Mode::Gitlink => 3,
        }
    }

    fn from_code(c: u8) -> Option<Mode> {
        Some(match c {
            0 => Mode::Regular,
            1 => Mode::Executable,
            2 => Mode::Symlink,
            3 => Mode::Gitlink,
            _ => return None,
        })
    }

    /// Content can be 3-way merged as text (regular and executable files).
    pub fn is_file(&self) -> bool {
        matches!(self, Mode::Regular | Mode::Executable)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum FileChange {
    Modify { blob: Oid, mode: Mode },
    Delete,
}

/// A git identity line, kept as raw bytes (`Name <email> 1700000000 +0100`) so imports round-trip.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Signature(pub Bytes);

impl Signature {
    pub fn new(name: &str, email: &str, unix_secs: i64, tz_offset_minutes: i32) -> Signature {
        let clean = |s: &str| s.replace(['<', '>', '\n', '\0'], "");
        let sign = if tz_offset_minutes < 0 { '-' } else { '+' };
        let tz = tz_offset_minutes.unsigned_abs();
        Signature(Bytes::from(format!(
            "{} <{}> {} {}{:02}{:02}",
            clean(name).trim(),
            clean(email).trim(),
            unix_secs,
            sign,
            tz / 60,
            tz % 60
        )))
    }

    pub fn now(name: &str, email: &str) -> Signature {
        Signature::new(name, email, crate::now_ms() / 1000, 0)
    }

    pub fn name(&self) -> String {
        let s = String::from_utf8_lossy(&self.0);
        s.split('<').next().unwrap_or("").trim().to_string()
    }

    pub fn email(&self) -> String {
        let s = String::from_utf8_lossy(&self.0);
        match (s.find('<'), s.rfind('>')) {
            (Some(a), Some(b)) if a < b => s[a + 1..b].to_string(),
            _ => String::new(),
        }
    }

    /// Seconds since the epoch.
    pub fn time(&self) -> i64 {
        let s = String::from_utf8_lossy(&self.0);
        let tail = s.rsplit('>').next().unwrap_or("");
        tail.split_whitespace().next().and_then(|t| t.parse().ok()).unwrap_or(0)
    }
}

impl std::fmt::Debug for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(&self.0))
    }
}

impl Serialize for Signature {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Signature", 3)?;
        st.serialize_field("name", &self.name())?;
        st.serialize_field("email", &self.email())?;
        st.serialize_field("time", &self.time())?;
        st.end()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changeset {
    /// The first parent is the mainline; `files` are relative to it.
    pub parents: Vec<ChangesetId>,
    pub author: Signature,
    pub committer: Signature,
    /// Exact message bytes.
    pub message: Bytes,
    /// Extra git commit headers after `committer` (`encoding`, `mergetag`, `gpgsig`, …), in
    /// order, with continuation lines joined by `\n`.
    pub extra: Vec<(String, Bytes)>,
    pub files: BTreeMap<RepoPath, FileChange>,
    /// The verbatim git commit for imports whose commit doesn't round-trip by derivation
    /// (unusual header order, legacy tree modes, …). Derivation then uses these bytes.
    pub git_raw: Option<Bytes>,
}

impl Changeset {
    pub fn id(&self) -> ChangesetId {
        ChangesetId(sha2::Sha256::digest(self.encode()).into())
    }

    /// First line of the message.
    pub fn summary(&self) -> String {
        String::from_utf8_lossy(&self.message).lines().next().unwrap_or("").to_string()
    }

    /// Canonical binary encoding (the id is its SHA-256).
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(256 + self.message.len() + self.files.len() * 64);
        b.push(1); // version
        varint(&mut b, self.parents.len() as u64);
        for p in &self.parents {
            b.extend_from_slice(&p.0);
        }
        bytes(&mut b, &self.author.0);
        bytes(&mut b, &self.committer.0);
        bytes(&mut b, &self.message);
        varint(&mut b, self.extra.len() as u64);
        for (k, v) in &self.extra {
            bytes(&mut b, k.as_bytes());
            bytes(&mut b, v);
        }
        varint(&mut b, self.files.len() as u64);
        for (path, change) in &self.files {
            bytes(&mut b, path.as_str().as_bytes());
            match change {
                FileChange::Delete => b.push(0),
                FileChange::Modify { blob, mode } => {
                    b.push(1 + mode.code());
                    b.extend_from_slice(&blob.0);
                }
            }
        }
        match &self.git_raw {
            None => b.push(0),
            Some(raw) => {
                b.push(1);
                bytes(&mut b, raw);
            }
        }
        b
    }

    pub fn decode(data: &[u8]) -> anyhow::Result<Changeset> {
        let mut r = Reader { data, pos: 0 };
        anyhow::ensure!(r.u8()? == 1, "unknown changeset encoding");
        let n = r.count(32)?;
        let mut parents = Vec::with_capacity(n);
        for _ in 0..n {
            parents.push(ChangesetId(r.take(32)?.try_into()?));
        }
        let author = Signature(Bytes::copy_from_slice(r.bytes()?));
        let committer = Signature(Bytes::copy_from_slice(r.bytes()?));
        let message = Bytes::copy_from_slice(r.bytes()?);
        let n = r.count(2)?;
        let mut extra = Vec::with_capacity(n);
        for _ in 0..n {
            let k = String::from_utf8(r.bytes()?.to_vec())?;
            extra.push((k, Bytes::copy_from_slice(r.bytes()?)));
        }
        let n = r.count(2)?;
        let mut files = BTreeMap::new();
        for _ in 0..n {
            let path = RepoPath::new(String::from_utf8(r.bytes()?.to_vec())?)?;
            let change = match r.u8()? {
                0 => FileChange::Delete,
                c => FileChange::Modify {
                    mode: Mode::from_code(c - 1).ok_or_else(|| anyhow::anyhow!("bad mode"))?,
                    blob: Oid(r.take(20)?.try_into()?),
                },
            };
            files.insert(path, change);
        }
        let git_raw = match r.u8()? {
            0 => None,
            _ => Some(Bytes::copy_from_slice(r.bytes()?)),
        };
        anyhow::ensure!(r.pos == data.len(), "trailing bytes in changeset");
        Ok(Changeset { parents, author, committer, message, extra, files, git_raw })
    }
}

fn varint(b: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        b.push((v as u8) | 0x80);
        v >>= 7;
    }
    b.push(v as u8);
}

fn bytes(b: &mut Vec<u8>, data: &[u8]) {
    varint(b, data.len() as u64);
    b.extend_from_slice(data);
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> anyhow::Result<u8> {
        let v = *self.data.get(self.pos).ok_or_else(|| anyhow::anyhow!("truncated changeset"))?;
        self.pos += 1;
        Ok(v)
    }

    fn varint(&mut self) -> anyhow::Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        anyhow::bail!("varint overflow")
    }

    /// A count of items that take at least `min_size` bytes each, bounded by the bytes left.
    fn count(&mut self, min_size: usize) -> anyhow::Result<usize> {
        let n = self.varint()? as usize;
        anyhow::ensure!(n.saturating_mul(min_size) <= self.data.len() - self.pos, "truncated changeset");
        Ok(n)
    }

    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        anyhow::ensure!(n <= self.data.len() - self.pos, "truncated changeset");
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn bytes(&mut self) -> anyhow::Result<&'a [u8]> {
        let n = self.varint()? as usize;
        self.take(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_roundtrip() {
        let mut files = BTreeMap::new();
        files.insert(RepoPath::new("a/b.txt").unwrap(), FileChange::Modify { blob: Oid([7; 20]), mode: Mode::Executable });
        files.insert(RepoPath::new("gone").unwrap(), FileChange::Delete);
        let cs = Changeset {
            parents: vec![ChangesetId([1; 32]), ChangesetId([2; 32])],
            author: Signature::new("Ada", "ada@example.com", 1_700_000_000, -90),
            committer: Signature::new("Bot", "bot@roll.sh", 1_700_000_001, 60),
            message: Bytes::from_static(b"hello\n\nbody\n"),
            extra: vec![("encoding".into(), Bytes::from_static(b"ISO-8859-1"))],
            files,
            git_raw: Some(Bytes::from_static(b"raw")),
        };
        let back = Changeset::decode(&cs.encode()).unwrap();
        assert_eq!(back, cs);
        assert_eq!(back.id(), cs.id());
        assert_eq!(cs.author.0.as_ref(), b"Ada <ada@example.com> 1700000000 -0130");
        assert_eq!(cs.author.name(), "Ada");
        assert_eq!(cs.author.email(), "ada@example.com");
        assert_eq!(cs.author.time(), 1_700_000_000);
        assert!(Changeset::decode(&cs.encode()[..10]).is_err());
    }
}
