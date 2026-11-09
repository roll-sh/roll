//! Git object formats: trees and commits are encoded exactly as git does, so their SHA-1s match.

use crate::ids::Oid;
use crate::model::Mode;
use crate::path::validate_component;
use bytes::Bytes;
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
            Kind::Tag => "tag",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "commit" => Kind::Commit,
            "tree" => Kind::Tree,
            "blob" => Kind::Blob,
            "tag" => Kind::Tag,
            _ => return None,
        })
    }

    /// Pack object type number.
    pub fn pack_type(&self) -> u8 {
        match self {
            Kind::Commit => 1,
            Kind::Tree => 2,
            Kind::Blob => 3,
            Kind::Tag => 4,
        }
    }

    fn code(&self) -> i64 {
        self.pack_type() as i64
    }

    pub(crate) fn from_code(c: i64) -> Option<Kind> {
        Some(match c {
            1 => Kind::Commit,
            2 => Kind::Tree,
            3 => Kind::Blob,
            4 => Kind::Tag,
            _ => return None,
        })
    }

    pub(crate) fn db_code(&self) -> i64 {
        self.code()
    }
}

pub const TREE_MODE: &[u8] = b"40000";

/// One entry of a git tree. `name` and `mode` are slices of the tree's bytes, so unchanged
/// entries re-encode byte-for-byte (legacy modes included).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: Bytes,
    pub name: Bytes,
    pub oid: Oid,
}

impl TreeEntry {
    pub fn tree(name: &str, oid: Oid) -> TreeEntry {
        TreeEntry { mode: Bytes::from_static(TREE_MODE), name: Bytes::copy_from_slice(name.as_bytes()), oid }
    }

    pub fn file(name: &str, mode: Mode, oid: Oid) -> TreeEntry {
        TreeEntry { mode: Bytes::from_static(mode.git()), name: Bytes::copy_from_slice(name.as_bytes()), oid }
    }

    pub fn is_tree(&self) -> bool {
        is_tree_mode(&self.mode)
    }

    pub fn file_mode(&self) -> Option<Mode> {
        if self.is_tree() { None } else { Mode::from_git(&self.mode) }
    }

    /// Validated UTF-8 at parse time.
    pub fn name(&self) -> &str {
        std::str::from_utf8(&self.name).unwrap_or("")
    }
}

pub fn is_tree_mode(m: &[u8]) -> bool {
    m == b"40000" || m == b"040000"
}

/// Git's tree order: byte order of names, with tree names compared as if they ended in `/`.
pub fn entry_cmp(a: &[u8], a_tree: bool, b: &[u8], b_tree: bool) -> Ordering {
    let n = a.len().min(b.len());
    match a[..n].cmp(&b[..n]) {
        Ordering::Equal => {}
        o => return o,
    }
    let ca = a.get(n).copied().or(if a_tree { Some(b'/') } else { None });
    let cb = b.get(n).copied().or(if b_tree { Some(b'/') } else { None });
    ca.cmp(&cb)
}

pub fn parse_tree(data: &Bytes) -> anyhow::Result<Vec<TreeEntry>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let sp = memchr(b' ', &data[pos..]).ok_or_else(|| anyhow::anyhow!("corrupt tree"))? + pos;
        let nul = memchr(0, &data[sp..]).ok_or_else(|| anyhow::anyhow!("corrupt tree"))? + sp;
        anyhow::ensure!(nul + 21 <= data.len(), "corrupt tree");
        let name = data.slice(sp + 1..nul);
        let s = std::str::from_utf8(&name).map_err(|_| anyhow::anyhow!("non-UTF-8 file name in tree"))?;
        validate_component(s)?;
        out.push(TreeEntry { mode: data.slice(pos..sp), name, oid: Oid(data[nul + 1..nul + 21].try_into()?) });
        pos = nul + 21;
    }
    Ok(out)
}

/// Encode entries (sorted with [`sort_entries`]).
pub fn encode_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut b = Vec::with_capacity(entries.len() * 40);
    for e in entries {
        b.extend_from_slice(&e.mode);
        b.push(b' ');
        b.extend_from_slice(&e.name);
        b.push(0);
        b.extend_from_slice(&e.oid.0);
    }
    b
}

pub fn sort_entries(entries: &mut [TreeEntry]) {
    entries.sort_by(|a, b| entry_cmp(&a.name, a.is_tree(), &b.name, b.is_tree()));
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitParts {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Bytes,
    pub committer: Bytes,
    pub extra: Vec<(String, Bytes)>,
    pub message: Bytes,
}

pub fn parse_commit(data: &Bytes) -> anyhow::Result<CommitParts> {
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = None;
    let mut committer = None;
    let mut extra: Vec<(String, Vec<u8>)> = Vec::new();
    let mut pos = 0;
    let message;
    loop {
        let end = memchr(b'\n', &data[pos..]).map(|i| i + pos);
        let line_end = end.unwrap_or(data.len());
        let line = &data[pos..line_end];
        if line.is_empty() {
            message = data.slice((line_end + 1).min(data.len())..);
            break;
        }
        if line[0] == b' ' {
            let last = extra.last_mut().ok_or_else(|| anyhow::anyhow!("corrupt commit: continuation line"))?;
            last.1.push(b'\n');
            last.1.extend_from_slice(&line[1..]);
        } else {
            let sp = memchr(b' ', line).ok_or_else(|| anyhow::anyhow!("corrupt commit header"))?;
            let (k, v) = (&line[..sp], &line[sp + 1..]);
            match k {
                b"tree" if tree.is_none() && parents.is_empty() => tree = Some(parse_hex_oid(v)?),
                b"parent" if author.is_none() => parents.push(parse_hex_oid(v)?),
                b"author" if author.is_none() => author = Some(data.slice(pos + sp + 1..line_end)),
                b"committer" if committer.is_none() => committer = Some(data.slice(pos + sp + 1..line_end)),
                _ => extra.push((String::from_utf8_lossy(k).into_owned(), v.to_vec())),
            }
        }
        match end {
            Some(e) => pos = e + 1,
            None => {
                message = Bytes::new();
                break;
            }
        }
    }
    Ok(CommitParts {
        tree: tree.ok_or_else(|| anyhow::anyhow!("commit without tree"))?,
        parents,
        author: author.ok_or_else(|| anyhow::anyhow!("commit without author"))?,
        committer: committer.ok_or_else(|| anyhow::anyhow!("commit without committer"))?,
        extra: extra.into_iter().map(|(k, v)| (k, Bytes::from(v))).collect(),
        message,
    })
}

pub fn encode_commit(c: &CommitParts) -> Vec<u8> {
    let mut b = Vec::with_capacity(256 + c.message.len());
    b.extend_from_slice(b"tree ");
    b.extend_from_slice(c.tree.hex().as_bytes());
    b.push(b'\n');
    for p in &c.parents {
        b.extend_from_slice(b"parent ");
        b.extend_from_slice(p.hex().as_bytes());
        b.push(b'\n');
    }
    b.extend_from_slice(b"author ");
    b.extend_from_slice(&c.author);
    b.push(b'\n');
    b.extend_from_slice(b"committer ");
    b.extend_from_slice(&c.committer);
    b.push(b'\n');
    for (k, v) in &c.extra {
        b.extend_from_slice(k.as_bytes());
        b.push(b' ');
        for (i, line) in v.split(|&c| c == b'\n').enumerate() {
            if i > 0 {
                b.extend_from_slice(b"\n ");
            }
            b.extend_from_slice(line);
        }
        b.push(b'\n');
    }
    b.push(b'\n');
    b.extend_from_slice(&c.message);
    b
}

fn parse_hex_oid(v: &[u8]) -> anyhow::Result<Oid> {
    std::str::from_utf8(v)?.trim().parse()
}

pub(crate) fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

/// Text-ish: no NUL in the first 8 KiB (git's heuristic).
pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_order() {
        let mut v = vec![
            TreeEntry::file("a.txt", Mode::Regular, Oid::ZERO),
            TreeEntry::tree("a", Oid::ZERO),
            TreeEntry::file("a-b", Mode::Regular, Oid::ZERO),
            TreeEntry::file("a0", Mode::Regular, Oid::ZERO),
        ];
        sort_entries(&mut v);
        let names: Vec<_> = v.iter().map(|e| e.name()).collect();
        // `a/` sorts after `a.txt` and `a-b` ('/' = 0x2f > '.' and '-') and before `a0`.
        assert_eq!(names, ["a-b", "a.txt", "a", "a0"]);
        let data = Bytes::from(encode_tree(&v));
        assert_eq!(parse_tree(&data).unwrap(), v);
    }

    #[test]
    fn commit_roundtrip() {
        let raw = Bytes::from_static(
            b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nparent 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a@b> 1 +0000\ncommitter C <c@d> 2 -0100\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\nmsg\n",
        );
        let c = parse_commit(&raw).unwrap();
        assert_eq!(c.extra.len(), 1);
        assert_eq!(encode_commit(&c), raw.to_vec());
    }
}
