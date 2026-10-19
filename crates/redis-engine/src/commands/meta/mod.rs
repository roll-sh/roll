//! Command metadata exactly as Redis 8.10.2 reports it: its `COMMAND` and `COMMAND DOCS` replies
//! (RESP3, captured from `redis:8-alpine` with its bundled modules), decoded once. `COMMAND INFO`
//! takes the tips and key specs from here and `COMMAND DOCS` everything; arity, flags and ACL
//! categories come from roll's own table, which a test keeps identical to these.

use crate::Reply;
use std::collections::HashMap;
use std::sync::OnceLock;

static INFO_RESP: &[u8] = include_bytes!("command.resp");
static DOCS_RESP: &[u8] = include_bytes!("docs.resp");

#[derive(Default)]
pub struct Meta {
    /// The `COMMAND INFO` entry: `[name, arity, flags, first, last, step, categories, tips,
    /// key specs, subcommands]`.
    pub info: Option<Reply>,
    /// The `COMMAND DOCS` map (summary, since, group, complexity, arguments…), subcommands
    /// included.
    pub docs: Option<Reply>,
}

impl Meta {
    fn field(&self, i: usize) -> Option<&Reply> {
        match &self.info {
            Some(Reply::Array(v)) => v.get(i),
            _ => None,
        }
    }

    pub fn arity(&self) -> Option<i64> {
        match self.field(1) {
            Some(Reply::Int(n)) => Some(*n),
            _ => None,
        }
    }

    /// Flag names (`write`, `movablekeys`…).
    pub fn flags(&self) -> Vec<String> {
        names(self.field(2))
    }

    /// `(first, last, step)`.
    pub fn keys(&self) -> Option<(i64, i64, i64)> {
        match (self.field(3), self.field(4), self.field(5)) {
            (Some(Reply::Int(a)), Some(Reply::Int(b)), Some(Reply::Int(c))) => Some((*a, *b, *c)),
            _ => None,
        }
    }

    pub fn categories(&self) -> Vec<String> {
        names(self.field(6))
    }

    pub fn tips(&self) -> Reply {
        self.field(7).cloned().unwrap_or(Reply::Array(vec![]))
    }

    pub fn key_specs(&self) -> Reply {
        self.field(8).cloned().unwrap_or(Reply::Array(vec![]))
    }
}

fn names(r: Option<&Reply>) -> Vec<String> {
    match r {
        Some(Reply::Array(v) | Reply::Set(v)) => v
            .iter()
            .filter_map(|x| match x {
                Reply::Status(s) => Some(s.clone()),
                Reply::Bulk(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn text(r: &Reply) -> Option<String> {
    match r {
        Reply::Bulk(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Reply::Status(s) => Some(s.clone()),
        _ => None,
    }
}

/// Where a command's metadata is in the captured replies: kept encoded (decoding all of it
/// would hold megabytes for the life of the process), each entry decoded when asked for.
#[derive(Clone, Default)]
struct Loc {
    info: Option<Place>,
    docs: Option<Place>,
}

/// A value's bytes in a capture, or a subcommand's place in its container's value (its index
/// in the COMMAND entry's subcommand list, its name in the docs' subcommand map).
#[derive(Clone)]
enum Place {
    At(usize, usize),
    Sub(usize, usize, SubKey),
}

#[derive(Clone)]
enum SubKey {
    Index(usize),
    Name(String),
}

static INDEX: OnceLock<HashMap<String, Loc>> = OnceLock::new();

fn norm(full: &str) -> String {
    let mut full = full.to_ascii_lowercase();
    // The internal `_FT.CURSOR` (hidden from COMMAND, so not captured) has FT.CURSOR's
    // subcommands, plus PROFILE, which is declared like GC.
    if let Some(sub) = full.strip_prefix("_ft.cursor|") {
        full = format!("ft.cursor|{}", if sub == "profile" { "gc" } else { sub });
    }
    full
}

/// Metadata of a command or subcommand by full name (`get`, `config|get`, `ft.search`),
/// case-insensitive, decoded now.
pub fn get(full: &str) -> Option<Meta> {
    let loc = INDEX.get_or_init(build).get(&norm(full))?;
    Some(Meta { info: loc.info.as_ref().and_then(|p| decode_at(INFO_RESP, p)), docs: loc.docs.as_ref().and_then(|p| decode_at(DOCS_RESP, p)) })
}

/// Whether Redis has this command (no decoding).
pub fn exists(full: &str) -> bool {
    INDEX.get_or_init(build).contains_key(&norm(full))
}

/// Every full name Redis has, lowercase.
pub fn names_all() -> impl Iterator<Item = &'static String> {
    INDEX.get_or_init(build).keys()
}

fn decode_at(buf: &[u8], p: &Place) -> Option<Reply> {
    match p {
        Place::At(from, to) => redis_proto::decode(&buf[*from..*to]).ok().flatten().map(|(r, _)| r),
        Place::Sub(from, to, key) => {
            let parent = redis_proto::decode(&buf[*from..*to]).ok().flatten()?.0;
            match (key, parent) {
                (SubKey::Index(i), Reply::Array(mut v)) => match v.get_mut(9) {
                    Some(Reply::Array(subs)) if *i < subs.len() => Some(subs.swap_remove(*i)),
                    _ => None,
                },
                (SubKey::Name(n), Reply::Map(fields)) => fields.into_iter().find_map(|(k, v)| match (text(&k).as_deref(), v) {
                    (Some("subcommands"), Reply::Map(subs)) => subs.into_iter().find(|(k, _)| text(k).is_some_and(|t| t.eq_ignore_ascii_case(n))).map(|(_, v)| v),
                    _ => None,
                }),
                _ => None,
            }
        }
    }
}

/// An aggregate's header (`*N` / `%N`): the element count and where the elements start.
fn header(buf: &[u8]) -> (usize, usize) {
    let end = buf.iter().position(|&b| b == b'\r').expect("header");
    (std::str::from_utf8(&buf[1..end]).ok().and_then(|n| n.parse().ok()).expect("count"), end + 2)
}

/// One value at `pos`: it, decoded, and where the next one starts (the decoded value is only
/// looked at while indexing).
fn next(buf: &[u8], pos: usize) -> (Reply, usize) {
    let (r, n) = redis_proto::decode(&buf[pos..]).expect("capture").expect("complete");
    (r, pos + n)
}

fn build() -> HashMap<String, Loc> {
    let mut out: HashMap<String, Loc> = HashMap::new();
    // COMMAND: an array of entries, subcommands at [9].
    let (n, mut pos) = header(INFO_RESP);
    for _ in 0..n {
        let (entry, end) = next(INFO_RESP, pos);
        if let Reply::Array(v) = &entry
            && let Some(name) = v.first().and_then(text)
        {
            if let Some(Reply::Array(subs)) = v.get(9) {
                for (i, s) in subs.iter().enumerate() {
                    if let Reply::Array(sv) = s
                        && let Some(sn) = sv.first().and_then(text)
                    {
                        out.entry(sn.to_ascii_lowercase()).or_default().info = Some(Place::Sub(pos, end, SubKey::Index(i)));
                    }
                }
            }
            out.entry(name.to_ascii_lowercase()).or_default().info = Some(Place::At(pos, end));
        }
        pos = end;
    }
    // COMMAND DOCS: a map of name to fields, subcommands under `subcommands`.
    let (n, mut pos) = header(DOCS_RESP);
    for _ in 0..n {
        let (k, at) = next(DOCS_RESP, pos);
        let (v, end) = next(DOCS_RESP, at);
        if let Some(name) = text(&k) {
            if let Reply::Map(fields) = &v {
                for (fk, fv) in fields {
                    if text(fk).as_deref() == Some("subcommands")
                        && let Reply::Map(subs) = fv
                    {
                        for (sk, _) in subs {
                            if let Some(sn) = text(sk) {
                                let key = sn.to_ascii_lowercase();
                                out.entry(key).or_default().docs = Some(Place::Sub(at, end, SubKey::Name(sn)));
                            }
                        }
                    }
                }
            }
            out.entry(name.to_ascii_lowercase()).or_default().docs = Some(Place::At(at, end));
        }
        pos = end;
    }
    out
}

#[cfg(test)]
mod tests;
