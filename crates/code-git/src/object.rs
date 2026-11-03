//! git objects: SHA-1 ids, kinds, and the few parsers the protocol needs (commit headers, tree
//! entries, tag targets). Full object encoding belongs to the store; this is just enough to walk
//! history and serve it.

use bytes::Bytes;
use sha1::{Digest, Sha1};
use std::fmt;
use std::str::FromStr;

/// A git object id (SHA-1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ObjectId(pub [u8; 20]);

impl ObjectId {
    pub const ZERO: ObjectId = ObjectId([0; 20]);
    pub const HEX_LEN: usize = 40;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        Some(ObjectId(b.try_into().ok()?))
    }

    pub fn from_hex(s: &[u8]) -> Option<Self> {
        if s.len() != Self::HEX_LEN {
            return None;
        }
        let mut out = [0u8; 20];
        for (i, pair) in s.as_chunks::<2>().0.iter().enumerate() {
            out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(ObjectId(out))
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 20]
    }

    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(40);
        for b in self.0 {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 15) as usize] as char);
        }
        s
    }

    /// The id git gives `data` stored as `kind`: SHA-1 of `<kind> <len>\0<data>`.
    pub fn hash(kind: Kind, data: &[u8]) -> Self {
        let mut h = Sha1::new();
        h.update(kind.as_str().as_bytes());
        h.update(b" ");
        h.update(data.len().to_string().as_bytes());
        h.update([0]);
        h.update(data);
        ObjectId(h.finalize().into())
    }
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.to_hex())
    }
}

impl FromStr for ObjectId {
    type Err = InvalidObjectId;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s.as_bytes()).ok_or(InvalidObjectId)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid object id")]
pub struct InvalidObjectId;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
            Kind::Tag => "tag",
        }
    }

    pub fn parse(s: &[u8]) -> Option<Self> {
        Some(match s {
            b"commit" => Kind::Commit,
            b"tree" => Kind::Tree,
            b"blob" => Kind::Blob,
            b"tag" => Kind::Tag,
            _ => return None,
        })
    }

    /// The object type number in pack entry headers.
    pub fn pack_type(self) -> u8 {
        match self {
            Kind::Commit => 1,
            Kind::Tree => 2,
            Kind::Blob => 3,
            Kind::Tag => 4,
        }
    }

    pub fn from_pack_type(t: u8) -> Option<Self> {
        Some(match t {
            1 => Kind::Commit,
            2 => Kind::Tree,
            3 => Kind::Blob,
            4 => Kind::Tag,
            _ => return None,
        })
    }
}

/// A git object: its id, kind and content (without the `<kind> <len>\0` header).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Object {
    pub id: ObjectId,
    pub kind: Kind,
    pub data: Bytes,
}

impl Object {
    /// Hashes `data` to get the id.
    pub fn new(kind: Kind, data: impl Into<Bytes>) -> Self {
        let data = data.into();
        Object { id: ObjectId::hash(kind, &data), kind, data }
    }
}

/// Tree entry modes git writes.
pub mod mode {
    pub const TREE: u32 = 0o040000;
    pub const REGULAR: u32 = 0o100644;
    pub const EXECUTABLE: u32 = 0o100755;
    pub const SYMLINK: u32 = 0o120000;
    pub const GITLINK: u32 = 0o160000;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TreeEntry<'a> {
    pub mode: u32,
    pub name: &'a [u8],
    pub oid: ObjectId,
}

impl TreeEntry<'_> {
    pub fn is_tree(&self) -> bool {
        self.mode == mode::TREE
    }
    pub fn is_gitlink(&self) -> bool {
        self.mode == mode::GITLINK
    }
}

#[derive(Debug, thiserror::Error)]
#[error("malformed {0} object")]
pub struct Malformed(pub &'static str);

/// Iterates the entries of a tree object (`<octal mode> <name>\0<20-byte id>` repeated).
pub fn tree_entries(data: &[u8]) -> impl Iterator<Item = Result<TreeEntry<'_>, Malformed>> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let parsed = (|| {
            let sp = rest.iter().position(|&b| b == b' ')?;
            let mut mode = 0u32;
            if sp == 0 || sp > 7 {
                return None;
            }
            for &c in &rest[..sp] {
                if !(b'0'..=b'7').contains(&c) {
                    return None;
                }
                mode = (mode << 3) | (c - b'0') as u32;
            }
            let after = &rest[sp + 1..];
            let nul = after.iter().position(|&b| b == 0)?;
            let name = &after[..nul];
            let oid = ObjectId::from_bytes(after.get(nul + 1..nul + 21)?)?;
            Some((TreeEntry { mode, name, oid }, &after[nul + 21..]))
        })();
        match parsed {
            Some((entry, next)) => {
                rest = next;
                Some(Ok(entry))
            }
            None => {
                rest = &[];
                Some(Err(Malformed("tree")))
            }
        }
    })
}

/// The headers of a commit the protocol needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    /// Committer time, seconds since the epoch (0 if unparseable).
    pub time: i64,
}

pub fn parse_commit(data: &[u8]) -> Result<CommitInfo, Malformed> {
    let mut tree = None;
    let mut parents = Vec::new();
    let mut time = 0;
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            break; // end of headers
        }
        if let Some(v) = line.strip_prefix(b"tree ") {
            tree = ObjectId::from_hex(v);
        } else if let Some(v) = line.strip_prefix(b"parent ") {
            parents.push(ObjectId::from_hex(v).ok_or(Malformed("commit"))?);
        } else if let Some(v) = line.strip_prefix(b"committer ") {
            // `Name <email> 1700000000 +0000`
            let mut it = v.rsplit(|&b| b == b' ');
            let _tz = it.next();
            time = it.next().and_then(|t| std::str::from_utf8(t).ok()?.parse().ok()).unwrap_or(0);
        }
    }
    Ok(CommitInfo { tree: tree.ok_or(Malformed("commit"))?, parents, time })
}

/// What an annotated tag points at.
pub fn parse_tag(data: &[u8]) -> Result<(ObjectId, Kind), Malformed> {
    let mut object = None;
    let mut kind = None;
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix(b"object ") {
            object = ObjectId::from_hex(v);
        } else if let Some(v) = line.strip_prefix(b"type ") {
            kind = Kind::parse(v);
        }
    }
    Ok((object.ok_or(Malformed("tag"))?, kind.ok_or(Malformed("tag"))?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_like_git() {
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(ObjectId::hash(Kind::Blob, b"hello\n").to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
        // the empty tree
        assert_eq!(ObjectId::hash(Kind::Tree, b"").to_hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    }

    #[test]
    fn hex_round_trip() {
        let id: ObjectId = "ce013625030ba8dba906f756967f9e9ca394464a".parse().unwrap();
        assert_eq!(id.to_string(), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert!("xyz".parse::<ObjectId>().is_err());
        assert!(ObjectId::from_hex(b"ce013625030ba8dba906f756967f9e9ca394464g").is_none());
    }

    #[test]
    fn parses_trees_and_rejects_garbage() {
        let id = ObjectId::hash(Kind::Blob, b"x");
        let mut t = b"100644 a.txt\0".to_vec();
        t.extend_from_slice(&id.0);
        t.extend_from_slice(b"40000 dir\0");
        t.extend_from_slice(&id.0);
        let e: Vec<_> = tree_entries(&t).collect::<Result<_, _>>().unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].mode, mode::REGULAR);
        assert_eq!(e[1].name, b"dir");
        assert!(e[1].is_tree());
        assert!(tree_entries(&t[..t.len() - 3]).any(|r| r.is_err()));
        assert!(tree_entries(b"999 x\0").any(|r| r.is_err()));
    }

    #[test]
    fn parses_commits() {
        let c = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nparent ce013625030ba8dba906f756967f9e9ca394464a\nauthor A <a@x> 1 +0000\ncommitter A <a@x> 1700000000 +0100\n\nmsg\ntree nope\n";
        let i = parse_commit(c).unwrap();
        assert_eq!(i.parents.len(), 1);
        assert_eq!(i.time, 1_700_000_000);
    }
}
