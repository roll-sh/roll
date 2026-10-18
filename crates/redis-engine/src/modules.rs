//! The modules Redis 8 bundles, as `MODULE LIST` and `INFO modules` show them: in the order of
//! the server's `modules` dict, the same dict here. They are added in load order (vector sets is
//! built in and loads first, then `redis:8-alpine` loads the `.so` files by name) with the
//! per-process hash seed, so the fifth one starts an incremental rehash nothing finishes at
//! startup; later lookups by module name (COMMAND LIST FILTERBY MODULE) take its steps.

use std::hash::{BuildHasher, RandomState};
use std::sync::LazyLock;

pub struct Module {
    pub name: &'static str,
    pub ver: i64,
    pub path: &'static str,
    pub usedby: &'static [&'static str],
    pub using: &'static [&'static str],
    pub options: &'static [&'static str],
}

const IO: &[&str] = &["handle-io-errors"];

/// In load order.
pub static LIST: &[Module] = &[
    Module { name: "vectorset", ver: 1, path: "", usedby: &[], using: &[], options: &["handle-io-errors", "handle-repl-async-load"] },
    Module { name: "bf", ver: 81001, path: "/usr/local/lib/redis/modules//redisbloom.so", usedby: &[], using: &[], options: IO },
    Module {
        name: "search",
        ver: crate::search::VERSION,
        path: "/usr/local/lib/redis/modules//redisearch.so",
        usedby: &[],
        using: &["ReJSON"],
        options: IO,
    },
    Module { name: "timeseries", ver: 81001, path: "/usr/local/lib/redis/modules//redistimeseries.so", usedby: &[], using: &[], options: IO },
    Module { name: "ReJSON", ver: 81000, path: "/usr/local/lib/redis/modules//rejson.so", usedby: &["search"], using: &[], options: IO },
];

/// `dict_hash_function_seed`.
static KEY: LazyLock<RandomState> = LazyLock::new(RandomState::new);

/// `dictSdsCaseHash` of a module name.
fn hash(name: &[u8]) -> u64 {
    KEY.hash_one(name.to_ascii_lowercase())
}

/// The `modules` dict (`modulesDictType`: incremental rehashing) holding [`LIST`]'s indexes.
#[derive(Clone, Debug)]
pub struct ModuleDict {
    /// `ht_table[0]` and `[1]`: chains of indexes, head first.
    t: [Vec<Vec<usize>>; 2],
    used: [usize; 2],
    rehashidx: Option<usize>,
}

impl Default for ModuleDict {
    /// The dict as module loading leaves it.
    fn default() -> Self {
        let mut d = ModuleDict { t: [Vec::new(), Vec::new()], used: [0, 0], rehashidx: None };
        for (i, m) in LIST.iter().enumerate() {
            // `RedisModule_Init` checks the name is free first (`RM_IsModuleNameBusy`).
            d.find(m.name.as_bytes());
            d.add(i);
        }
        d
    }
}

impl ModuleDict {
    fn hash_of(i: usize) -> u64 {
        hash(LIST[i].name.as_bytes())
    }

    /// `dictAdd` (`dictFindLinkForInsert`: a rehash step, the expansion, then the head of the
    /// bucket).
    fn add(&mut self, i: usize) {
        let h = Self::hash_of(i);
        if !self.t[0].is_empty() {
            self.step(h as usize & (self.t[0].len() - 1));
        }
        self.expand_if_needed();
        let ti = usize::from(self.rehashidx.is_some());
        let mask = self.t[ti].len() - 1;
        self.t[ti][h as usize & mask].insert(0, i);
        self.used[ti] += 1;
    }

    /// `dictExpandIfNeeded`: four buckets, then doubled at a 1:1 load.
    fn expand_if_needed(&mut self) {
        if self.rehashidx.is_some() {
            return;
        }
        if self.t[0].is_empty() {
            self.t[0] = vec![Vec::new(); 4];
        } else if self.used[0] >= self.t[0].len() {
            self.t[1] = vec![Vec::new(); (self.used[0] + 1).next_power_of_two()];
            self.rehashidx = Some(0);
        }
    }

    /// `rehashEntriesInBucketAtIndex`.
    fn move_bucket(&mut self, idx: usize) {
        let mask = self.t[1].len() - 1;
        for i in std::mem::take(&mut self.t[0][idx]) {
            self.t[1][Self::hash_of(i) as usize & mask].insert(0, i);
            self.used[0] -= 1;
            self.used[1] += 1;
        }
    }

    /// `dictCheckRehashingCompleted`.
    fn check_completed(&mut self) {
        if self.rehashidx.is_some() && self.used[0] == 0 {
            self.t[0] = std::mem::take(&mut self.t[1]);
            self.used = [self.used[1], 0];
            self.rehashidx = None;
        }
    }

    /// `_dictRehashStepIfNeeded`: the visited bucket when it still has entries, else
    /// `dictRehash(d, 1)`.
    fn step(&mut self, visited: usize) {
        let Some(mut idx) = self.rehashidx else { return };
        if visited >= idx && !self.t[0][visited].is_empty() {
            self.move_bucket(visited);
        } else if self.used[0] != 0 {
            let mut empty_visits = 10;
            while self.t[0][idx].is_empty() {
                idx += 1;
                empty_visits -= 1;
                if empty_visits == 0 {
                    self.rehashidx = Some(idx);
                    return;
                }
            }
            self.move_bucket(idx);
            self.rehashidx = Some(idx + 1);
        }
        self.check_completed();
    }

    /// `dictFind` by module name, with the rehash step it takes.
    pub fn find(&mut self, name: &[u8]) -> Option<&'static Module> {
        if self.used[0] + self.used[1] == 0 {
            return None;
        }
        let h = hash(name);
        self.step(h as usize & (self.t[0].len() - 1));
        let tables = if self.rehashidx.is_some() { 2 } else { 1 };
        self.t[..tables]
            .iter()
            .flat_map(|t| &t[h as usize & (t.len() - 1)])
            .map(|&i| &LIST[i])
            .find(|m| m.name.as_bytes().eq_ignore_ascii_case(name))
    }

    /// The modules in iteration order (`ht_table[0]`, then `[1]` while rehashing).
    pub fn modules(&self) -> impl Iterator<Item = &'static Module> + '_ {
        let tables = if self.rehashidx.is_some() { 2 } else { 1 };
        self.t[..tables].iter().flatten().flatten().map(|&i| &LIST[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_leaves_the_last_module_in_the_new_table() {
        let d = ModuleDict::default();
        assert_eq!(d.rehashidx, Some(0));
        assert_eq!((d.used[0], d.used[1], d.t[1].len()), (4, 1, 8));
        assert_eq!(d.modules().last().unwrap().name, "ReJSON");
    }

    #[test]
    fn lookups_finish_the_rehash() {
        let mut d = ModuleDict::default();
        for _ in 0..8 {
            assert_eq!(d.find(b"SEARCH").map(|m| m.name), Some("search"));
        }
        assert_eq!(d.rehashidx, None);
        assert_eq!(d.modules().count(), 5);
        assert!(d.find(b"nope").is_none());
    }
}
