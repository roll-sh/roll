//! The modules Redis 8 bundles, as `MODULE LIST` and `INFO modules` show them (in the order the
//! server's module dict yields them).

pub struct Module {
    pub name: &'static str,
    pub ver: i64,
    pub path: &'static str,
    pub usedby: &'static [&'static str],
    pub using: &'static [&'static str],
    pub options: &'static [&'static str],
}

const IO: &[&str] = &["handle-io-errors"];

pub static LIST: &[Module] = &[
    Module { name: "timeseries", ver: 81001, path: "/usr/local/lib/redis/modules//redistimeseries.so", usedby: &[], using: &[], options: IO },
    Module { name: "bf", ver: 81001, path: "/usr/local/lib/redis/modules//redisbloom.so", usedby: &[], using: &[], options: IO },
    Module {
        name: "search",
        ver: crate::search::VERSION,
        path: "/usr/local/lib/redis/modules//redisearch.so",
        usedby: &[],
        using: &["ReJSON"],
        options: IO,
    },
    Module { name: "vectorset", ver: 1, path: "", usedby: &[], using: &[], options: &["handle-io-errors", "handle-repl-async-load"] },
    Module { name: "ReJSON", ver: 81000, path: "/usr/local/lib/redis/modules//rejson.so", usedby: &["search"], using: &[], options: IO },
];
