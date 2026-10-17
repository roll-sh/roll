//! The module's INFO sections (`RS_moduleInfoFunc`, `info/info_redis/info_redis.c`): the
//! version, index totals and runtime configuration, all prefixed `search_`.

use crate::config::Config;
use std::fmt::Write;

/// `(section name, body)` pairs in the order RediSearch adds them; every field already carries
/// the `search_` prefix and a CRLF.
pub fn sections(cfg: &Config) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    out.push(("search_version", format!("search_version:{}\r\nsearch_redis_version:{} - oss\r\n", super::VERSION_TEXT, crate::commands::server::REDIS_VERSION)));
    // Without indexes (and `search-_info-on-zero-indexes` off) only the counts and the runtime
    // configuration are given.
    let mut s = String::new();
    for f in [
        "number_of_indexes",
        "number_of_active_indexes",
        "number_of_active_indexes_running_queries",
        "number_of_active_indexes_indexing",
        "total_active_write_threads",
        "total_indexing_time",
        "total_num_docs_in_indexes",
        "total_inverted_index_blocks",
    ] {
        let _ = write!(s, "search_{f}:0\r\n");
    }
    out.push(("search_indexes", s));
    out.push(("search_runtime_configurations", runtime(cfg)));
    out
}

fn runtime(c: &Config) -> String {
    let mut s = String::new();
    let mut f = |k: &str, v: String| {
        let _ = write!(s, "search_{k}:{v}\r\n");
    };
    let on = |b: bool| if b { "ON" } else { "OFF" }.to_string();
    f("extension_load", c.text("search-ext-load"));
    f("friso_ini", c.text("search-friso-ini"));
    f("default_scorer", c.text("search-default-scorer:value"));
    f("enableGC", on(!c.flag("search-no-gc")));
    f("minimal_term_prefix", c.num("search-min-prefix").to_string());
    f("minimal_stem_length", c.num("search-min-stem-len").to_string());
    f("maximal_prefix_expansions", c.num("search-max-prefix-expansions").to_string());
    f("query_timeout_ms", c.num("search-timeout").to_string());
    f("timeout_policy", super::config::TIMEOUT_POLICIES[c.num("search-on-timeout") as usize].into());
    f("oom_policy", super::config::OOM_POLICIES[c.num("search-on-oom") as usize].into());
    // `cursorReadSize` has no setting.
    f("cursor_read_size", "1000".into());
    f("cursor_max_idle_time", c.num("search-cursor-max-idle").to_string());
    f("max_doc_table_size", c.num("search-max-doctablesize").to_string());
    f("max_search_results", c.num("search-max-search-results").to_string());
    f("max_aggregate_results", c.num("search-max-aggregate-results").to_string());
    f("max_aggregate_groups", c.num("search-max-aggregate-groups").to_string());
    f("gc_scan_size", c.num("search-gc-scan-size").to_string());
    f("min_phonetic_term_length", c.num("search-min-phonetic-term-len").to_string());
    f("bm25std_tanh_factor", c.num("search-bm25std-tanh-factor").to_string());
    f("info_on_zero_indexes", on(c.flag("search-_info-on-zero-indexes")));
    s
}
