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

static META: OnceLock<HashMap<String, Meta>> = OnceLock::new();

/// Metadata of a command or subcommand by full name (`get`, `config|get`, `ft.search`),
/// case-insensitive.
pub fn get(full: &str) -> Option<&'static Meta> {
    let mut full = full.to_ascii_lowercase();
    // The internal `_FT.CURSOR` (hidden from COMMAND, so not captured) has FT.CURSOR's
    // subcommands, plus PROFILE, which is declared like GC.
    if let Some(sub) = full.strip_prefix("_ft.cursor|") {
        full = format!("ft.cursor|{}", if sub == "profile" { "gc" } else { sub });
    }
    META.get_or_init(build).get(&full)
}

/// Every full name Redis has, lowercase.
pub fn names_all() -> impl Iterator<Item = &'static String> {
    META.get_or_init(build).keys()
}

fn build() -> HashMap<String, Meta> {
    let mut out: HashMap<String, Meta> = HashMap::new();
    let (info, _) = redis_proto::decode(INFO_RESP).expect("command.resp").expect("complete");
    fn add_info(out: &mut HashMap<String, Meta>, entry: &Reply) {
        let Reply::Array(v) = entry else { return };
        let Some(name) = v.first().and_then(text) else { return };
        if let Some(Reply::Array(subs)) = v.get(9) {
            for s in subs {
                add_info(out, s);
            }
        }
        out.entry(name.to_ascii_lowercase()).or_default().info = Some(entry.clone());
    }
    if let Reply::Array(all) = &info {
        for e in all {
            add_info(&mut out, e);
        }
    }
    let (docs, _) = redis_proto::decode(DOCS_RESP).expect("docs.resp").expect("complete");
    fn add_docs(out: &mut HashMap<String, Meta>, docs: &Reply) {
        let Reply::Map(pairs) = docs else { return };
        for (k, v) in pairs {
            let Some(name) = text(k) else { continue };
            if let Reply::Map(fields) = v {
                for (fk, fv) in fields {
                    if text(fk).as_deref() == Some("subcommands") {
                        add_docs(out, fv);
                    }
                }
            }
            out.entry(name.to_ascii_lowercase()).or_default().docs = Some(v.clone());
        }
    }
    add_docs(&mut out, &docs);
    out
}

#[cfg(test)]
mod tests;
