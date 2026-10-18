//! Exchanges and their routing indexes.
//!
//! Each exchange keeps an index specialised for its type (hash map, topic trie, hash ring, …)
//! and updated incrementally, so binding changes stay cheap with millions of bindings and
//! routing never scans.

use crate::error::{AmqpError, Result};
use crate::message::Message;
use crate::value::{FieldTable, FieldValue};
use parking_lot::RwLock;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExchangeKind {
    Direct,
    Fanout,
    Topic,
    Headers,
    /// `x-consistent-hash`: binding keys are weights on a hash ring.
    ConsistentHash,
    /// `x-random`: one random bound queue per message.
    Random,
    /// `x-local-random` (RabbitMQ 4): picks a queue local to the publisher's node when possible.
    LocalRandom,
    /// `x-delayed-message`: holds messages for `x-delay` ms, then routes like the inner type.
    Delayed(Box<ExchangeKind>),
    /// `x-message-deduplication` (plugin-compatible): drops messages whose
    /// `x-deduplication-header` was seen recently, then routes like `direct`.
    Dedup,
}

impl ExchangeKind {
    pub fn parse(s: &str, args: &FieldTable) -> Result<Self> {
        Ok(match s {
            "direct" => Self::Direct,
            "fanout" => Self::Fanout,
            "topic" => Self::Topic,
            "headers" | "match" => Self::Headers,
            "x-consistent-hash" => Self::ConsistentHash,
            "x-random" => Self::Random,
            "x-local-random" => Self::LocalRandom,
            "x-message-deduplication" => Self::Dedup,
            "x-delayed-message" => {
                let inner = args
                    .get_str("x-delayed-type")
                    .ok_or_else(|| AmqpError::precondition("Invalid argument, 'x-delayed-type' must be an existing exchange type"))?;
                if inner == "x-delayed-message" {
                    return Err(AmqpError::precondition("x-delayed-type can't be x-delayed-message"));
                }
                Self::Delayed(Box::new(Self::parse(inner, args)?))
            }
            other => return Err(AmqpError::precondition(format!("unknown exchange type '{other}'"))),
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Fanout => "fanout",
            Self::Topic => "topic",
            Self::Headers => "headers",
            Self::ConsistentHash => "x-consistent-hash",
            Self::Random => "x-random",
            Self::LocalRandom => "x-local-random",
            Self::Delayed(_) => "x-delayed-message",
            Self::Dedup => "x-message-deduplication",
        }
    }

    fn routing(&self) -> &ExchangeKind {
        match self {
            Self::Delayed(inner) => inner.routing(),
            Self::Dedup => &Self::Direct,
            k => k,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DestKind {
    Queue,
    Exchange,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Dest {
    pub kind: DestKind,
    pub name: String,
}

impl Dest {
    pub fn queue(name: impl Into<String>) -> Self {
        Dest { kind: DestKind::Queue, name: name.into() }
    }
    pub fn exchange(name: impl Into<String>) -> Self {
        Dest { kind: DestKind::Exchange, name: name.into() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub source: String,
    pub dest: Dest,
    pub routing_key: String,
    pub args: FieldTable,
}

impl Binding {
    fn same(&self, dest: &Dest, key: &str, args: &FieldTable) -> bool {
        self.dest == *dest && self.routing_key == key && self.args.equivalent(args)
    }
}

pub struct Exchange {
    pub name: String,
    pub kind: ExchangeKind,
    pub durable: bool,
    pub auto_delete: bool,
    pub internal: bool,
    pub args: FieldTable,
    index: RwLock<Index>,
    pub stats: ExchangeStats,
}

#[derive(Default, Debug)]
pub struct ExchangeStats {
    pub publish_in: AtomicU64,
    pub publish_out: AtomicU64,
    pub unroutable: AtomicU64,
}

impl std::fmt::Debug for Exchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exchange").field("name", &self.name).field("kind", &self.kind).finish()
    }
}

#[derive(Default)]
struct Index {
    all: Vec<Binding>,
    direct: HashMap<String, Vec<Dest>>,
    topic: TopicTrie,
    /// Consistent-hash ring: point → destination.
    ring: BTreeMap<u32, Dest>,
}

impl Exchange {
    pub fn new(name: impl Into<String>, kind: ExchangeKind, durable: bool, auto_delete: bool, internal: bool, args: FieldTable) -> Self {
        Exchange { name: name.into(), kind, durable, auto_delete, internal, args, index: RwLock::default(), stats: Default::default() }
    }

    pub fn is_default(&self) -> bool {
        self.name.is_empty()
    }

    pub fn bindings(&self) -> Vec<Binding> {
        self.index.read().all.clone()
    }

    pub fn binding_count(&self) -> usize {
        self.index.read().all.len()
    }

    /// Add a binding; returns false if an identical one already exists.
    pub fn bind(&self, dest: Dest, key: &str, args: FieldTable) -> bool {
        let mut ix = self.index.write();
        if ix.all.iter().any(|b| b.same(&dest, key, &args)) {
            return false;
        }
        match self.kind.routing() {
            ExchangeKind::Direct => {
                let v = ix.direct.entry(key.to_string()).or_default();
                if !v.contains(&dest) {
                    v.push(dest.clone());
                }
            }
            ExchangeKind::Topic => ix.topic.insert(key, dest.clone()),
            ExchangeKind::ConsistentHash => {
                let weight = key.parse::<u32>().unwrap_or(1).clamp(1, 10_000);
                for i in 0..weight {
                    let point = hash32(format!("{}\0{}\0{i}", dest.name, key).as_bytes());
                    ix.ring.insert(point, dest.clone());
                }
            }
            _ => {}
        }
        ix.all.push(Binding { source: self.name.clone(), dest, routing_key: key.to_string(), args });
        true
    }

    /// Remove a binding; returns false if there was none.
    pub fn unbind(&self, dest: &Dest, key: &str, args: &FieldTable) -> bool {
        let mut ix = self.index.write();
        let Some(i) = ix.all.iter().position(|b| b.same(dest, key, args)) else {
            return false;
        };
        ix.all.remove(i);
        self.unindex(&mut ix, dest, key);
        true
    }

    /// Remove every binding to `dest`; returns how many.
    pub fn unbind_dest(&self, dest: &Dest) -> usize {
        let mut ix = self.index.write();
        let keys: Vec<String> = ix.all.iter().filter(|b| b.dest == *dest).map(|b| b.routing_key.clone()).collect();
        ix.all.retain(|b| b.dest != *dest);
        for k in &keys {
            self.unindex(&mut ix, dest, k);
        }
        keys.len()
    }

    fn unindex(&self, ix: &mut Index, dest: &Dest, key: &str) {
        // Another binding with the same dest and key (different args) keeps the index entry.
        if ix.all.iter().any(|b| b.dest == *dest && b.routing_key == key) {
            return;
        }
        match self.kind.routing() {
            ExchangeKind::Direct => {
                if let Some(v) = ix.direct.get_mut(key) {
                    v.retain(|d| d != dest);
                    if v.is_empty() {
                        ix.direct.remove(key);
                    }
                }
            }
            ExchangeKind::Topic => ix.topic.remove(key, dest),
            ExchangeKind::ConsistentHash => ix.ring.retain(|_, d| d != dest),
            _ => {}
        }
    }

    /// Destinations for a message (one hop; the vhost follows exchange-to-exchange bindings).
    pub fn route(&self, msg: &Message, routing_key: &str, out: &mut Vec<Dest>) {
        let ix = self.index.read();
        match self.kind.routing() {
            ExchangeKind::Direct => {
                if let Some(d) = ix.direct.get(routing_key) {
                    out.extend(d.iter().cloned());
                }
            }
            ExchangeKind::Fanout => {
                let mut seen = HashSet::new();
                out.extend(ix.all.iter().filter(|b| seen.insert(&b.dest)).map(|b| b.dest.clone()));
            }
            ExchangeKind::Topic => ix.topic.matches(routing_key, out),
            ExchangeKind::Headers => {
                let headers = msg.props.headers.as_ref();
                out.extend(ix.all.iter().filter(|b| headers_match(&b.args, headers)).map(|b| b.dest.clone()));
            }
            ExchangeKind::ConsistentHash => {
                if ix.ring.is_empty() {
                    return;
                }
                let key = self.hash_input(msg, routing_key);
                let Some(key) = key else { return };
                let h = hash32(key.as_bytes());
                let d = ix.ring.range(h..).next().or_else(|| ix.ring.iter().next()).map(|(_, d)| d.clone());
                out.extend(d);
            }
            ExchangeKind::Random | ExchangeKind::LocalRandom => {
                let dests: Vec<&Dest> = {
                    let mut seen = HashSet::new();
                    ix.all.iter().map(|b| &b.dest).filter(|d| seen.insert(*d)).collect()
                };
                if !dests.is_empty() {
                    out.push(dests[crate::rand_u64() as usize % dests.len()].clone());
                }
            }
            ExchangeKind::Delayed(_) | ExchangeKind::Dedup => unreachable!("routing() unwraps these"),
        }
    }

    /// What a consistent-hash exchange hashes: the routing key, a header or a property.
    fn hash_input(&self, msg: &Message, routing_key: &str) -> Option<String> {
        if let Some(h) = self.args.get_str("hash-header") {
            return msg.props.header(h).map(|v| match v.as_str() {
                Some(s) => s.to_string(),
                None => v.to_json().to_string(),
            });
        }
        if let Some(p) = self.args.get_str("hash-property") {
            return match p {
                "message_id" => msg.props.message_id.clone(),
                "correlation_id" => msg.props.correlation_id.clone(),
                "timestamp" => msg.props.timestamp.map(|t| t.to_string()),
                _ => None,
            };
        }
        Some(routing_key.to_string())
    }
}

/// Headers-exchange matching, following RabbitMQ: `x-match` is `all` (default), `any`,
/// `all-with-x` or `any-with-x`; a void binding value tests presence only.
pub fn headers_match(binding: &FieldTable, headers: Option<&FieldTable>) -> bool {
    let mode = binding.get_str("x-match").unwrap_or("all");
    let (any, with_x) = match mode {
        "any" => (true, false),
        "all-with-x" => (false, true),
        "any-with-x" => (true, true),
        _ => (false, false),
    };
    let empty = FieldTable::new();
    let headers = headers.unwrap_or(&empty);
    let mut checked = 0;
    for (k, v) in binding.iter() {
        if k == "x-match" || (!with_x && k.starts_with("x-")) {
            continue;
        }
        checked += 1;
        let hit = match headers.get(k) {
            None => false,
            Some(_) if matches!(v, FieldValue::Void) => true,
            Some(h) => h.equivalent(v),
        };
        if any && hit {
            return true;
        }
        if !any && !hit {
            return false;
        }
    }
    // `all` with nothing to check matches everything; `any` with nothing matches nothing.
    let _ = checked;
    !any
}

/// Topic routing trie with `*` (one word) and `#` (zero or more words).
#[derive(Default)]
pub struct TopicTrie {
    root: TrieNode,
}

#[derive(Default)]
struct TrieNode {
    children: HashMap<String, TrieNode>,
    dests: Vec<Dest>,
}

impl TopicTrie {
    pub fn insert(&mut self, pattern: &str, dest: Dest) {
        let mut node = &mut self.root;
        for w in pattern.split('.') {
            node = node.children.entry(w.to_string()).or_default();
        }
        if !node.dests.contains(&dest) {
            node.dests.push(dest);
        }
    }

    pub fn remove(&mut self, pattern: &str, dest: &Dest) {
        fn go(node: &mut TrieNode, words: &[&str], dest: &Dest) {
            match words.split_first() {
                None => node.dests.retain(|d| d != dest),
                Some((w, rest)) => {
                    if let Some(child) = node.children.get_mut(*w) {
                        go(child, rest, dest);
                        if child.dests.is_empty() && child.children.is_empty() {
                            node.children.remove(*w);
                        }
                    }
                }
            }
        }
        let words: Vec<&str> = pattern.split('.').collect();
        go(&mut self.root, &words, dest);
    }

    pub fn matches(&self, key: &str, out: &mut Vec<Dest>) {
        let words: Vec<&str> = if key.is_empty() { vec![""] } else { key.split('.').collect() };
        let mut found: Vec<&Dest> = Vec::new();
        Self::walk(&self.root, &words, &mut found);
        let mut seen = HashSet::new();
        out.extend(found.into_iter().filter(|d| seen.insert(*d)).cloned());
    }

    fn walk<'a>(node: &'a TrieNode, words: &[&str], out: &mut Vec<&'a Dest>) {
        if let Some(hash) = node.children.get("#") {
            // `#` swallows zero or more words.
            for i in 0..=words.len() {
                Self::walk(hash, &words[i..], out);
            }
        }
        match words.split_first() {
            None => out.extend(node.dests.iter()),
            Some((w, rest)) => {
                if let Some(c) = node.children.get(*w) {
                    Self::walk(c, rest, out);
                }
                if let Some(c) = node.children.get("*") {
                    Self::walk(c, rest, out);
                }
            }
        }
    }
}

/// A stable 32-bit hash (FNV-1a then a murmur finaliser) so every node agrees on placement.
pub fn hash32(data: &[u8]) -> u32 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33;
    h as u32
}

pub type ExchangeRef = Arc<Exchange>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Properties;
    use bytes::Bytes;

    fn msg(headers: Option<FieldTable>) -> Message {
        Message::new("x", "", Properties { headers, ..Default::default() }, Bytes::new())
    }

    fn route(ex: &Exchange, key: &str) -> Vec<String> {
        let mut out = vec![];
        ex.route(&msg(None), key, &mut out);
        let mut v: Vec<String> = out.into_iter().map(|d| d.name).collect();
        v.sort();
        v
    }

    #[test]
    fn topic() {
        let ex = Exchange::new("t", ExchangeKind::Topic, true, false, false, FieldTable::new());
        for (k, q) in [("a.*.c", "q1"), ("a.#", "q2"), ("#", "q3"), ("*.b.*", "q4"), ("a.b.c", "q5"), ("#.c", "q6"), ("a.#.c", "q7")] {
            ex.bind(Dest::queue(q), k, FieldTable::new());
        }
        assert_eq!(route(&ex, "a.b.c"), ["q1", "q2", "q3", "q4", "q5", "q6", "q7"]);
        assert_eq!(route(&ex, "a"), ["q2", "q3"]);
        assert_eq!(route(&ex, "a.c"), ["q2", "q3", "q6", "q7"]);
        assert_eq!(route(&ex, "x.y"), ["q3"]);
        assert_eq!(route(&ex, ""), ["q3"]);
        ex.unbind(&Dest::queue("q3"), "#", &FieldTable::new());
        assert_eq!(route(&ex, "x.y"), Vec::<String>::new());
    }

    #[test]
    fn headers() {
        let all: FieldTable = [("x-match", FieldValue::str("all")), ("a", 1i64.into()), ("b", "x".into())].into_iter().collect();
        let any: FieldTable = [("x-match", FieldValue::str("any")), ("a", 1i64.into()), ("b", "x".into())].into_iter().collect();
        let h1: FieldTable = [("a", FieldValue::I32(1)), ("b", "x".into())].into_iter().collect();
        let h2: FieldTable = [("a", FieldValue::I32(1))].into_iter().collect();
        assert!(headers_match(&all, Some(&h1)));
        assert!(!headers_match(&all, Some(&h2)));
        assert!(headers_match(&any, Some(&h2)));
        assert!(!headers_match(&any, None));
        let presence: FieldTable = [("a", FieldValue::Void)].into_iter().collect();
        assert!(headers_match(&presence, Some(&h2)));
    }

    #[test]
    fn consistent_hash_spreads() {
        let ex = Exchange::new("h", ExchangeKind::ConsistentHash, true, false, false, FieldTable::new());
        for q in ["a", "b", "c"] {
            ex.bind(Dest::queue(q), "10", FieldTable::new());
        }
        let mut counts: HashMap<String, usize> = HashMap::new();
        for i in 0..3000 {
            let r = route(&ex, &format!("key-{i}"));
            assert_eq!(r.len(), 1);
            *counts.entry(r[0].clone()).or_default() += 1;
        }
        assert!(counts.values().all(|&c| c > 600), "{counts:?}");
        // Same key, same queue.
        assert_eq!(route(&ex, "stable"), route(&ex, "stable"));
    }

    #[test]
    fn direct_and_duplicates() {
        let ex = Exchange::new("d", ExchangeKind::Direct, true, false, false, FieldTable::new());
        assert!(ex.bind(Dest::queue("q"), "k", FieldTable::new()));
        assert!(!ex.bind(Dest::queue("q"), "k", FieldTable::new()));
        let args: FieldTable = [("x", 1i64)].into_iter().collect();
        assert!(ex.bind(Dest::queue("q"), "k", args.clone()));
        assert_eq!(route(&ex, "k"), ["q"]);
        ex.unbind(&Dest::queue("q"), "k", &FieldTable::new());
        assert_eq!(route(&ex, "k"), ["q"]);
        ex.unbind(&Dest::queue("q"), "k", &args);
        assert!(route(&ex, "k").is_empty());
    }
}
