//! The modules Redis 8 bundles, as `MODULE LIST` and `INFO modules` show them: in the order of
//! the server's `modules` dict, the same dict here. They are added in load order (vector sets is
//! built in and loads first, then `redis:8-alpine` loads the `.so` files by name) with the
//! per-process hash seed, so the fifth one starts an incremental rehash nothing finishes at
//! startup; later lookups by module name (COMMAND LIST FILTERBY MODULE) take its steps.

use crate::sdict::ServerDict;

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

/// The `modules` dict (`modulesDictType`: case-insensitive names, incremental rehashing). Every
/// server starts from the same one, shared; a server copies it only when one of its lookups takes
/// a rehash step.
#[derive(Clone, Debug, Default)]
pub struct ModuleDict(Option<Box<ServerDict>>);

/// The dict as module loading leaves it.
static LOADED: std::sync::LazyLock<ServerDict> = std::sync::LazyLock::new(|| {
    let mut d = ServerDict::new(true);
    for m in LIST {
        // `RedisModule_Init` checks the name is free first (`RM_IsModuleNameBusy`).
        d.find(m.name.as_bytes());
        d.add(m.name.as_bytes());
    }
    d
});

fn by_name(name: &[u8]) -> Option<&'static Module> {
    LIST.iter().find(|m| m.name.as_bytes().eq_ignore_ascii_case(name))
}

impl ModuleDict {
    fn dict(&self) -> &ServerDict {
        self.0.as_deref().unwrap_or(&LOADED)
    }

    /// `dictFind` by module name, with the rehash step it takes.
    pub fn find(&mut self, name: &[u8]) -> Option<&'static Module> {
        if self.0.is_none() {
            if !LOADED.is_rehashing() {
                return LOADED.contains(name).then(|| by_name(name)).flatten();
            }
            self.0 = Some(Box::new(LOADED.clone()));
        }
        self.0.as_mut()?.find(name).and_then(by_name)
    }

    /// The modules in iteration order.
    pub fn modules(&self) -> impl Iterator<Item = &'static Module> + '_ {
        self.dict().iter().filter_map(by_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_leaves_the_last_module_in_the_new_table() {
        let d = ModuleDict::default();
        assert!(d.dict().is_rehashing());
        assert_eq!(d.modules().count(), 5);
        assert_eq!(d.modules().last().unwrap().name, "ReJSON");
    }

    #[test]
    fn lookups_finish_the_rehash() {
        let mut d = ModuleDict::default();
        for _ in 0..8 {
            assert_eq!(d.find(b"SEARCH").map(|m| m.name), Some("search"));
        }
        assert!(!d.dict().is_rehashing());
        assert_eq!(d.modules().count(), 5);
        assert!(d.find(b"nope").is_none());
    }
}
