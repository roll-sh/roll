//! Per-command ACL metadata, built once from the command table: a dense id per command and
//! subcommand (the bit in a selector's command bitmap), the full name (`config|get`), ACL
//! categories and key-spec permissions (from Redis' own command table, see `spec.rs`), and which
//! arguments are Pub/Sub channels.

use super::spec::SPECS;
use crate::commands::{self, Command, Group};
use foldhash::fast::RandomState;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Category names in Redis' order (`ACL CAT`, `COMMAND INFO`). The trailing ones are the module
/// categories `redis:8` ships with (they exist but contain no command here).
pub const CATEGORIES: &[&str] = &[
    "keyspace",
    "read",
    "write",
    "set",
    "sortedset",
    "list",
    "hash",
    "string",
    "array",
    "bitmap",
    "hyperloglog",
    "geo",
    "stream",
    "pubsub",
    "admin",
    "fast",
    "slow",
    "blocking",
    "dangerous",
    "connection",
    "transaction",
    "scripting",
    "bloom",
    "cuckoo",
    "cms",
    "topk",
    "tdigest",
    "search",
    "timeseries",
    "json",
];

const fn cat(name: &str) -> u32 {
    let mut i = 0;
    while i < CATEGORIES.len() {
        if const_eq(CATEGORIES[i], name) {
            return 1 << i;
        }
        i += 1;
    }
    panic!("unknown category");
}

const fn const_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

pub const C_WRITE: u32 = cat("write");
pub const C_READ: u32 = cat("read");
pub const C_SLOW: u32 = cat("slow");
pub const C_FAST: u32 = cat("fast");

/// The bit of a category name (case-insensitive), as Redis' `ACLGetCommandCategoryFlagByName`.
pub fn category_bit(name: &[u8]) -> Option<u32> {
    CATEGORIES.iter().position(|c| c.as_bytes().eq_ignore_ascii_case(name)).map(|i| 1 << i)
}

/// Category names of a bitmask, in canonical order.
pub fn category_names(mask: u32) -> Vec<&'static str> {
    CATEGORIES.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, c)| *c).collect()
}

/// Size of a selector's command bitmap (like Redis' `USER_COMMAND_BITS_COUNT`). The last bit is
/// never assigned to a command: `+@all` sets it, so it records "may run future commands".
pub const BITS: usize = 1024;
pub const FUTURE_BIT: u16 = (BITS - 1) as u16;

/// Key permission bits of `%R~` / `%W~` patterns.
pub const PERM_R: u8 = 1;
pub const PERM_W: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channels {
    None,
    /// SUBSCRIBE / SSUBSCRIBE: every argument is a channel.
    Subscribe,
    /// PSUBSCRIBE: every argument is a pattern (matched literally against ACL patterns).
    Pattern,
    /// PUBLISH / SPUBLISH: the first argument.
    Publish,
}

pub struct Meta {
    pub cmd: &'static Command,
    pub id: u16,
    /// `get`, `config|get`.
    pub full: String,
    pub cats: u32,
    /// `doesCommandHaveKeys`.
    pub has_keys: bool,
    pub channels: Channels,
    pub parent: Option<u16>,
    pub subs: Vec<u16>,
}

pub struct Index {
    pub cmds: Vec<Meta>,
    /// Top-level command ids, in table order.
    pub top: Vec<u16>,
    by_ptr: HashMap<usize, u16, RandomState>,
}

impl Index {
    #[inline]
    pub fn id(&self, cmd: &Command) -> Option<u16> {
        self.by_ptr.get(&(cmd as *const Command as usize)).copied()
    }

    #[inline]
    pub fn meta(&self, cmd: &Command) -> Option<&Meta> {
        self.id(cmd).map(|i| &self.cmds[i as usize])
    }
}

static INDEX: OnceLock<Index> = OnceLock::new();

pub fn index() -> &'static Index {
    INDEX.get_or_init(build)
}

fn spec_of(full: &str) -> Option<(u32, &'static [u8])> {
    SPECS.binary_search_by(|e| e.0.cmp(full)).ok().map(|i| (SPECS[i].1, SPECS[i].2))
}

fn build() -> Index {
    let mut idx = Index { cmds: Vec::new(), top: Vec::new(), by_ptr: HashMap::default() };
    for &c in &commands::table().all {
        let id = push(&mut idx, c, c.name.to_string(), None);
        idx.top.push(id);
        for s in c.subs {
            let sid = push(&mut idx, s, format!("{}|{}", c.name, s.name), Some(id));
            idx.cmds[id as usize].subs.push(sid);
        }
    }
    assert!(idx.cmds.len() < BITS - 1, "too many commands for the ACL bitmap");
    idx
}

fn push(idx: &mut Index, c: &'static Command, full: String, parent: Option<u16>) -> u16 {
    let id = idx.cmds.len() as u16;
    let cats = spec_of(&full).map_or_else(|| fallback_categories(c), |(cats, _)| cats);
    let has_keys = commands::keyspec::has_keys(c, &full);
    let channels = if parent.is_some() {
        Channels::None
    } else {
        match c.name {
            "subscribe" | "ssubscribe" => Channels::Subscribe,
            "psubscribe" => Channels::Pattern,
            "publish" | "spublish" => Channels::Publish,
            _ => Channels::None,
        }
    };
    idx.by_ptr.insert(c as *const Command as usize, id);
    idx.cmds.push(Meta { cmd: c, id, full, cats, has_keys, channels, parent, subs: Vec::new() });
    id
}

/// Categories for a command Redis doesn't have, derived from its flags and group the way
/// Redis' implicit categories work.
fn fallback_categories(c: &Command) -> u32 {
    if c.group == Group::Module {
        return module_categories(c);
    }
    let mut m = match c.group {
        Group::Generic => cat("keyspace"),
        Group::String => cat("string"),
        Group::List => cat("list"),
        Group::Set => cat("set"),
        Group::SortedSet => cat("sortedset"),
        Group::Hash => cat("hash"),
        Group::PubSub => cat("pubsub"),
        Group::Transactions => cat("transaction"),
        Group::Connection => cat("connection"),
        Group::Scripting => cat("scripting"),
        Group::HyperLogLog => cat("hyperloglog"),
        Group::Geo => cat("geo"),
        Group::Stream => cat("stream"),
        Group::Bitmap => cat("bitmap"),
        Group::Array => cat("array"),
        Group::Server | Group::Cluster | Group::Module => 0,
    };
    if c.has(commands::WRITE) {
        m |= C_WRITE;
    }
    if c.has(commands::READONLY) && m & cat("scripting") == 0 {
        m |= C_READ;
    }
    if c.has(commands::ADMIN) {
        m |= cat("admin") | cat("dangerous");
    }
    if c.has(commands::DANGEROUS) {
        m |= cat("dangerous");
    }
    if c.has(commands::PUBSUB) {
        m |= cat("pubsub");
    }
    if c.has(commands::FAST) {
        m |= C_FAST;
    }
    if c.has(commands::BLOCKING) {
        m |= cat("blocking");
    }
    if m & C_FAST == 0 {
        m |= C_SLOW;
    }
    m
}

/// A module command has exactly the categories its module declares (no implicit @slow):
/// read or write, and the module's own.
fn module_categories(c: &Command) -> u32 {
    let module = [("json.", "json"), ("bf.", "bloom"), ("cf.", "cuckoo"), ("cms.", "cms"), ("topk.", "topk"), ("tdigest.", "tdigest"), ("ts.", "timeseries")];
    // A module that declares no categories (vector sets) has none.
    let Some(&(_, name)) = module.iter().find(|(p, _)| c.name.starts_with(p)) else { return 0 };
    let mut m = cat(name);
    if c.has(commands::WRITE) {
        m |= C_WRITE;
    }
    if c.has(commands::READONLY) {
        m |= C_READ;
    }
    if c.has(commands::ACL_FAST) {
        m |= C_FAST;
    }
    m
}

/// ACL categories of a command (`COMMAND INFO`), in Redis' order.
pub fn categories_of(c: &Command) -> Vec<&'static str> {
    let mask = match index().meta(c) {
        Some(m) => m.cats,
        None => fallback_categories(c),
    };
    category_names(mask)
}

/// Resolve `name` or `container|sub` like Redis' `ACLLookupCommand` (one level of
/// subcommands; `get|x` is not a command).
pub fn lookup_full(name: &[u8]) -> Option<&'static Command> {
    if name.is_empty() {
        return None;
    }
    let mut parts = name.split(|&c| c == b'|');
    let base = commands::lookup(parts.next()?)?;
    let Some(sub) = parts.next() else { return Some(base) };
    if parts.next().is_some() || base.subs.is_empty() {
        return None;
    }
    commands::lookup_sub(base, sub)
}
