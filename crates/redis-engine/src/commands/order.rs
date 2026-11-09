//! The order Redis walks its command table in. `server.commands` and each container's
//! subcommands are `dict`s hashed (case-insensitively) with the per-process hash seed, so
//! COMMAND, COMMAND LIST/DOCS, ACL CAT and INFO commandstats list commands in bucket order, which
//! differs between processes. The same dicts are built here: the same insertions in the same
//! order (`meta/order.txt`: the core table as `commands.def` declares it, then the modules as
//! `redis:8-alpine` loads them, each in its registration order, internal commands included), a
//! per-process hash key, and Redis 8's layout for command tables (four buckets at first, doubled
//! at a 1:1 load and rehashed at once, `force_full_rehash`, new entries at a bucket's head).

use super::{Command, lookup, lookup_sub};
use std::hash::{BuildHasher, RandomState};
use std::sync::LazyLock;

static ORDER_TXT: &str = include_str!("meta/order.txt");

/// A top-level command and its subcommands, both in iteration order.
pub struct Entry {
    pub cmd: &'static Command,
    pub subs: Vec<&'static Command>,
}

/// `dict_hash_function_seed`: one key for every command dict of the process.
static KEY: LazyLock<RandomState> = LazyLock::new(RandomState::new);

/// `dictSdsCaseHash`.
fn hash(name: &str) -> u64 {
    KEY.hash_one(name.to_ascii_lowercase().as_bytes())
}

/// The iteration order of a command dict the names were added to in order.
fn layout(names: &[&str]) -> Vec<usize> {
    let hashes: Vec<u64> = names.iter().map(|n| hash(n)).collect();
    let mut table: Vec<Vec<usize>> = Vec::new();
    for (i, &h) in hashes.iter().enumerate() {
        // `_dictExpandIfNeeded` before the insert, rehashed in full at once.
        if table.is_empty() {
            table = vec![Vec::new(); 4];
        } else if i >= table.len() {
            let mut grown = vec![Vec::new(); (i + 1).next_power_of_two()];
            let mask = grown.len() - 1;
            for chain in &table {
                for &e in chain {
                    grown[hashes[e] as usize & mask].insert(0, e);
                }
            }
            table = grown;
        }
        let mask = table.len() - 1;
        table[h as usize & mask].insert(0, i);
    }
    table.into_iter().flatten().collect()
}

/// Every command's full name in iteration order, containers followed by their subcommands.
static NAMES: LazyLock<Vec<String>> = LazyLock::new(|| {
    let rows: Vec<Vec<&str>> = ORDER_TXT.lines().map(|l| l.split(' ').collect()).collect();
    let tops: Vec<&str> = rows.iter().map(|r| r[0]).collect();
    let mut out = Vec::new();
    for i in layout(&tops) {
        let row = &rows[i];
        out.push(row[0].to_string());
        out.extend(layout(&row[1..]).into_iter().map(|j| format!("{}|{}", row[0], row[1 + j])));
    }
    out
});

static ORDER: LazyLock<Vec<Entry>> = LazyLock::new(|| {
    let mut out: Vec<Entry> = Vec::new();
    // Commands roll doesn't have still take their places in the table.
    for name in NAMES.iter() {
        match name.split_once('|') {
            None => {
                if let Some(cmd) = lookup(name.as_bytes()) {
                    out.push(Entry { cmd, subs: Vec::new() });
                }
            }
            Some((top, sub)) => {
                if let Some(e) = out.last_mut().filter(|e| e.cmd.name.eq_ignore_ascii_case(top))
                    && let Some(s) = lookup_sub(e.cmd, sub.as_bytes())
                {
                    e.subs.push(s);
                }
            }
        }
    }
    out
});

/// Every full name (`config|get`, as the commands declare them) in iteration order, the ones
/// roll doesn't have included.
pub fn names() -> &'static [String] {
    &NAMES
}

/// Each full name's place in [`names`], by lowercase name.
static POSITION: LazyLock<std::collections::HashMap<String, usize>> =
    LazyLock::new(|| NAMES.iter().enumerate().map(|(i, n)| (n.to_ascii_lowercase(), i)).collect());

/// Where a full name (any case) comes in the table's iteration order.
pub fn position(name: &str) -> Option<usize> {
    POSITION.get(&name.to_ascii_lowercase()).copied()
}

/// `items` (by full name) in the table's iteration order; names it doesn't know go last.
pub fn in_order<'a, T>(items: impl Iterator<Item = (&'a str, T)>) -> Vec<(&'a str, T)> {
    let mut v: Vec<(usize, &'a str, T)> = items.map(|(n, t)| (position(n).unwrap_or(usize::MAX), n, t)).collect();
    v.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    v.into_iter().map(|(_, n, t)| (n, t)).collect()
}

/// The commands in `server.commands`' iteration order.
pub fn commands() -> &'static [Entry] {
    &ORDER
}

/// A container's subcommands in its subcommand dict's iteration order.
pub fn subs_of(cmd: &Command) -> &'static [&'static Command] {
    static BY_CMD: LazyLock<std::collections::HashMap<usize, usize>> =
        LazyLock::new(|| ORDER.iter().enumerate().map(|(i, e)| (e.cmd as *const Command as usize, i)).collect());
    BY_CMD.get(&(cmd as *const Command as usize)).map_or(&[], |&i| ORDER[i].subs.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_once() {
        let t = super::super::table();
        assert_eq!(commands().len(), t.all.len());
        for c in &t.all {
            assert_eq!(subs_of(c).len(), c.subs.len(), "{}", c.name);
        }
    }

    #[test]
    fn layout_keeps_every_index() {
        let names: Vec<String> = (0..300).map(|i| format!("cmd{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut got = layout(&refs);
        got.sort();
        assert_eq!(got, (0..300).collect::<Vec<_>>());
    }
}
