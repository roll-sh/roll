//! The label index (`indexer.c`): `__index_<label>=<value>` and `__key_index_<label>` entries
//! naming the series that carry them, the inverse map from each series to its entries, the
//! filter expressions TS.MRANGE, TS.MGET, TS.QUERYINDEX and TS.QUERYLABELS take, and the share
//! of the index MEMORY USAGE attributes to a series.
//!
//! The index is global (across databases) and keyed by series name, as in the module.

use super::RStr;
use crate::types::rax::{Tree, dict_size};
use std::collections::{BTreeMap, BTreeSet};

/// The ACL check a query applies to each series it finds.
pub type CanRead<'a> = &'a mut dyn FnMut(&[u8]) -> bool;

const KV_PREFIX: &[u8] = b"__index_";
const K_PREFIX: &[u8] = b"__key_index_";

/// The bytes before the first NUL (what `%s` prints).
pub fn c_str(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |n| &b[..n])
}

/// A `RedisModuleDict` of names (NULL values) with the rax it allocates.
#[derive(Clone, Debug, Default)]
pub struct Leaf {
    pub set: BTreeSet<Vec<u8>>,
    tree: Option<Tree>,
}

impl Leaf {
    fn insert(&mut self, k: &[u8]) {
        if self.set.insert(k.to_vec()) {
            self.tree.get_or_insert_with(Tree::new).insert(k, false);
        }
    }

    fn remove(&mut self, k: &[u8]) {
        if self.set.remove(k)
            && let Some(t) = self.tree.as_mut()
        {
            t.remove(k);
        }
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// `RM_MallocSizeDict`.
    fn size(&self) -> usize {
        match &self.tree {
            Some(t) => dict_size(t),
            None => dict_size(&Tree::new()),
        }
    }

    pub fn contains(&self, k: &[u8]) -> bool {
        self.set.contains(k)
    }
}

fn kv_entry(k: &[u8], v: &[u8]) -> Vec<u8> {
    [KV_PREFIX, c_str(k), b"=", c_str(v)].concat()
}

fn k_entry(k: &[u8]) -> Vec<u8> {
    [K_PREFIX, c_str(k)].concat()
}

#[derive(Clone, Debug, Default)]
pub struct Index {
    /// `labelsIndex`: entry → the series carrying it.
    labels: BTreeMap<Vec<u8>, Leaf>,
    /// `tsLabelIndex`: series → its entries.
    series: BTreeMap<Vec<u8>, Leaf>,
}

/// `PredicateType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PType {
    Eq,
    Neq,
    ListMatch,
    ListNotMatch,
    Contains,
    NContains,
}

impl PType {
    fn inclusion(self) -> bool {
        matches!(self, PType::Eq | PType::Contains | PType::ListMatch)
    }
}

/// `QueryPredicate`.
#[derive(Clone, Debug)]
pub struct Pred {
    pub ty: PType,
    pub key: Vec<u8>,
    pub values: Vec<Vec<u8>>,
}

/// `strtok_r` over `s` from `*pos` with `delims` as a set of bytes.
fn strtok<'a>(s: &'a [u8], pos: &mut usize, delims: &[u8]) -> Option<&'a [u8]> {
    let mut i = *pos;
    while i < s.len() && delims.contains(&s[i]) {
        i += 1;
    }
    if i >= s.len() {
        *pos = s.len();
        return None;
    }
    let start = i;
    while i < s.len() && !delims.contains(&s[i]) {
        i += 1;
    }
    *pos = if i < s.len() { i + 1 } else { i };
    Some(&s[start..i])
}

fn find(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// `parseValueList`.
fn value_list(token: Option<&[u8]>) -> Option<Vec<Vec<u8>>> {
    let token = token?;
    let inner = token.strip_suffix(b")")?;
    if token.len() <= 1 {
        return Some(Vec::new());
    }
    let count = inner.iter().filter(|&&c| c == b',').count() + 1;
    let mut pos = 0;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(strtok(inner, &mut pos, b",")?.to_vec());
    }
    Some(out)
}

/// `parsePredicate` with the separator's bytes as delimiters.
fn predicate(s: &[u8], ty: PType, sep: &[u8]) -> Option<Pred> {
    let mut pos = 0;
    let key = strtok(s, &mut pos, sep)?.to_vec();
    let value = strtok(s, &mut pos, sep);
    let values = if sep.ends_with(b"(") {
        value_list(value)?
    } else {
        value.map(|v| vec![v.to_vec()]).unwrap_or_default()
    };
    Some(Pred { ty, key, values })
}

/// `parseLabelListFromArgs`: `None` when an expression doesn't parse.
pub fn parse_filters<B: AsRef<[u8]>>(args: &[B]) -> Option<Vec<Pred>> {
    args.iter()
        .map(|a| {
            let s = c_str(a.as_ref());
            if find(s, b"!=(") {
                predicate(s, PType::ListNotMatch, b"!=(")
            } else if find(s, b"!=") {
                predicate(s, PType::Neq, b"!=").map(|p| if p.values.is_empty() { Pred { ty: PType::Contains, ..p } } else { p })
            } else if find(s, b"=(") {
                predicate(s, PType::ListMatch, b"=(")
            } else if find(s, b"=") {
                predicate(s, PType::Eq, b"=").map(|p| if p.values.is_empty() { Pred { ty: PType::NContains, ..p } } else { p })
            } else {
                None
            }
        })
        .collect()
}

/// At least one `l=v` or `l=(…)` among the expressions.
pub fn has_matcher(preds: &[Pred]) -> bool {
    preds.iter().any(|p| matches!(p.ty, PType::Eq | PType::ListMatch))
}

impl Index {
    pub fn is_empty(&self) -> bool {
        self.series.is_empty() && self.labels.is_empty()
    }

    fn under_key(&mut self, add: bool, entry: &[u8], ts: &[u8]) {
        self.labels.entry(entry.to_vec()).or_default();
        self.series.entry(ts.to_vec()).or_default();
        if add {
            self.labels.get_mut(entry).expect("created").insert(ts);
            self.series.get_mut(ts).expect("created").insert(entry);
        } else {
            let leaf = self.labels.get_mut(entry).expect("created");
            leaf.remove(ts);
            if leaf.is_empty() {
                self.labels.remove(entry);
            }
        }
    }

    /// `IndexMetric`.
    pub fn add(&mut self, ts: &[u8], labels: &[(RStr, RStr)]) {
        for (k, v) in labels {
            self.under_key(true, &kv_entry(&k.b, &v.b), ts);
            self.under_key(true, &k_entry(&k.b), ts);
        }
    }

    /// `RemoveIndexedMetric`.
    pub fn remove(&mut self, ts: &[u8]) {
        let Some(leaf) = self.series.get(ts) else { return };
        let entries: Vec<Vec<u8>> = leaf.set.iter().cloned().collect();
        for e in entries {
            self.under_key(false, &e, ts);
        }
        self.series.remove(ts);
    }

    /// `RemoveAllIndexedMetrics` (FLUSHALL, FLUSHDB, a full resync).
    pub fn clear(&mut self) {
        let names: Vec<Vec<u8>> = self.series.keys().cloned().collect();
        for n in names {
            self.remove(&n);
        }
        self.series.clear();
    }

    pub fn is_indexed(&self, ts: &[u8]) -> bool {
        self.series.contains_key(ts)
    }

    /// `IndexMemUsage`: the series' own entry list and its share of each entry it is under.
    pub fn mem_usage(&self, ts: &[u8]) -> usize {
        let Some(leaf) = self.series.get(ts) else { return 0 };
        let mut total = leaf.size();
        for e in &leaf.set {
            if let Some(l) = self.labels.get(e)
                && !l.is_empty()
            {
                total += l.size() / l.len();
            }
        }
        total
    }

    /// `GetPredicateKeysDicts`.
    fn dicts(&self, p: &Pred) -> Vec<Option<&Leaf>> {
        if matches!(p.ty, PType::Contains | PType::NContains) {
            return vec![self.labels.get(&k_entry(&p.key))];
        }
        p.values.iter().map(|v| self.labels.get(&kv_entry(&p.key, v))).collect()
    }

    /// `QueryIndex`: the series matching every predicate, sorted. `can_read` is the ACL check
    /// (the flag says a series was skipped for it).
    pub fn query(&self, preds: &mut [Pred], mut can_read: Option<CanRead<'_>>) -> (BTreeSet<Vec<u8>>, bool) {
        let mut res = BTreeSet::new();
        let mut denied = false;
        if preds.is_empty() {
            return (res, denied);
        }
        if preds.len() > 1 {
            let mut min = (0, u64::MAX);
            for (i, p) in preds.iter().enumerate() {
                if !p.ty.inclusion() {
                    continue;
                }
                let size: u64 = self.dicts(p).iter().flatten().map(|l| l.len() as u64).sum();
                if size < min.1 {
                    min = (i, size);
                }
            }
            preds.swap(0, min.0);
        }
        if !preds[0].ty.inclusion() {
            return (res, denied);
        }
        for leaf in self.dicts(&preds[0]).into_iter().flatten() {
            for k in &leaf.set {
                if let Some(f) = can_read.as_mut()
                    && !f(k)
                {
                    denied = true;
                    continue;
                }
                if self.satisfies(k, &preds[1..]) {
                    res.insert(k.clone());
                }
            }
        }
        (res, denied)
    }

    fn satisfies(&self, k: &[u8], rest: &[Pred]) -> bool {
        rest.iter().all(|p| {
            let found = self.dicts(p).iter().flatten().any(|l| l.contains(k));
            found == p.ty.inclusion()
        })
    }

    /// `GetAllIndexedSeriesKeys`.
    pub fn all(&self) -> BTreeSet<Vec<u8>> {
        self.series.keys().cloned().collect()
    }

    /// `QueryLabelsFromIndex`: a series' label names, or its value for `label`.
    pub fn labels_of(&self, ts: &[u8], label: Option<&[u8]>) -> Vec<Vec<u8>> {
        let Some(leaf) = self.series.get(ts) else { return Vec::new() };
        let prefix = match label {
            None => K_PREFIX.to_vec(),
            Some(l) => [KV_PREFIX, c_str(l), b"="].concat(),
        };
        let candidate = if label.is_some() { prefix.len() - KV_PREFIX.len() - 1 } else { 0 };
        let mut out = Vec::new();
        for e in &leaf.set {
            if !e.starts_with(&prefix) {
                continue;
            }
            if label.is_some() && shadowed(leaf, &e[KV_PREFIX.len()..], candidate) {
                continue;
            }
            out.push(e[prefix.len()..].to_vec());
            if label.is_some() {
                break;
            }
        }
        out
    }
}

/// `ValuesMatchShadowedByLongerKey`.
fn shadowed(leaf: &Leaf, content: &[u8], candidate: usize) -> bool {
    leaf.set.iter().any(|e| {
        if e.len() <= K_PREFIX.len() || !e.starts_with(K_PREFIX) {
            return false;
        }
        let other = &e[K_PREFIX.len()..];
        other.len() > candidate && other.len() < content.len() && content.starts_with(other) && content[other.len()] == b'='
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(k: &str, v: &str) -> (RStr, RStr) {
        (RStr::arg(k.as_bytes()), RStr::arg(v.as_bytes()))
    }

    #[test]
    fn parses_and_queries() {
        let mut ix = Index::default();
        ix.add(b"a", &[l("x", "1"), l("y", "2")]);
        ix.add(b"b", &[l("x", "1")]);
        ix.add(b"c", &[l("x", "3")]);
        let q = |ix: &Index, f: &[&str]| {
            let mut p = parse_filters(f).unwrap();
            ix.query(&mut p, None).0.into_iter().map(|k| String::from_utf8(k).unwrap()).collect::<Vec<_>>()
        };
        assert_eq!(q(&ix, &["x=1"]), ["a", "b"]);
        assert_eq!(q(&ix, &["x=1", "y="]), ["b"]);
        assert_eq!(q(&ix, &["x=(1,3)", "y!=2"]), ["b", "c"]);
        assert_eq!(q(&ix, &["x!=(1)", "x!="]), ["c"]);
        assert!(parse_filters(&["x=(1,,2)"]).is_none());
        assert!(parse_filters(&["x"]).is_none());
        assert_eq!(ix.labels_of(b"a", None), [b"x".to_vec(), b"y".to_vec()]);
        assert_eq!(ix.labels_of(b"a", Some(b"y")), [b"2".to_vec()]);
        ix.remove(b"a");
        ix.remove(b"b");
        ix.remove(b"c");
        assert!(ix.is_empty());
    }
}
