//! FT.CONFIG's variables (`RSGlobalConfigOptions`, then the coordinator's `clusterOptions_g`):
//! the legacy names over the same settings the `search-*` CONFIG parameters hold. A few have no
//! CONFIG parameter; they live in the config under `ft:` names.

use super::args::{Ac, AcErr, GE0, GE1};
use super::error::{Code, QErr, QResult};
use crate::config::{Config, Val};

type Get = fn(&Config) -> Option<String>;
type Set = fn(&mut Config, &mut Ac) -> QResult<()>;

pub struct Var {
    pub name: &'static str,
    pub help: &'static str,
    pub get: Get,
    /// `None`: not modifiable at runtime (`RSCONFIGVAR_F_IMMUTABLE`).
    pub set: Option<Set>,
}

/// `RETURN_PARSE_ERROR`.
fn parse_err(e: AcErr) -> QErr {
    QErr::new(Code::ParseArgs, e.strerror())
}

fn num(c: &Config, name: &str) -> i128 {
    c.num(name)
}

fn put(c: &mut Config, name: &'static str, v: i128) {
    c.put(name, Val::Num(v));
}

fn tf(b: bool) -> Option<String> {
    Some(if b { "true" } else { "false" }.into())
}

/// `CONFIG_BOOLEAN_SETTER`: `true` or `false`.
fn bool_arg(ac: &mut Ac) -> QResult<bool> {
    let a = ac.get().map_err(parse_err)?;
    if super::args::c_eq(a, "true") {
        Ok(true)
    } else if super::args::c_eq(a, "false") {
        Ok(false)
    } else {
        Err(parse_err(AcErr::Parse))
    }
}

macro_rules! numvar {
    ($name:literal, $help:expr, $param:literal) => {
        Var { name: $name, help: $help, get: |c| Some(num(c, $param).to_string()), set: None }
    };
    ($name:literal, $help:expr, $param:literal, $set:expr) => {
        Var { name: $name, help: $help, get: |c| Some(num(c, $param).to_string()), set: Some($set) }
    };
}

macro_rules! boolvar {
    ($name:literal, $help:expr, $param:literal) => {
        Var {
            name: $name,
            help: $help,
            get: |c| tf(c.flag($param)),
            set: Some(|c, ac| {
                let b = bool_arg(ac)?;
                c.put($param, Val::Bool(b));
                Ok(())
            }),
        }
    };
}

/// A size with `AC_F_GE*` flags stored as is.
fn set_size(c: &mut Config, ac: &mut Ac, name: &'static str, flags: u32) -> QResult<()> {
    let v = ac.get_size(flags).map_err(parse_err)?;
    put(c, name, v as i128);
    Ok(())
}

/// MINPREFIX, MAXPREFIXEXPANSIONS, UNION_ITERATOR_HEAP: a `long long` ≥ 1 clamped to 32 bits.
fn set_clamped(c: &mut Config, ac: &mut Ac, name: &'static str) -> QResult<()> {
    let v = ac.get_ll(GE1).map_err(parse_err)?;
    put(c, name, (v as i128).min(u32::MAX as i128));
    Ok(())
}

fn too_many_threads() -> QErr {
    QErr::new(Code::Limit, "Number of worker threads cannot exceed 16")
}

/// `translateOrClampMaxResults`.
fn max_results(v: i64) -> i128 {
    if v < 0 { 1 << 31 } else { (v as i128).min(1 << 31) }
}

fn unlimited(v: i128) -> String {
    if v == 1 << 31 { "unlimited".into() } else { v.to_string() }
}

fn text_or_null(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

pub const TIMEOUT_POLICIES: [&str; 3] = ["return", "fail", "return-strict"];
pub const OOM_POLICIES: [&str; 3] = ["return", "fail", "ignore"];

pub static VARS: &[Var] = &[
    Var { name: "EXTLOAD", help: "Load extension scoring/expansion module", get: |c| text_or_null(c.text("search-ext-load")), set: None },
    Var { name: "NOGC", help: "Disable garbage collection (for this process)", get: |c| tf(c.flag("search-no-gc")), set: None },
    Var {
        name: "MINPREFIX",
        help: "Set the minimum prefix for expansions (`*`)",
        get: |c| Some(num(c, "search-min-prefix").to_string()),
        set: Some(|c, ac| set_clamped(c, ac, "search-min-prefix")),
    },
    Var {
        name: "MINSTEMLEN",
        help: "Set the minimum word length to stem (default 4)",
        get: |c| Some(num(c, "search-min-stem-len").to_string()),
        set: Some(|c, ac| {
            let v = ac.get_u32(GE1).map_err(parse_err)?;
            if v < 2 {
                return Err(QErr::new(Code::Syntax, "Minimum stem length cannot be lower than 2"));
            }
            put(c, "search-min-stem-len", v as i128);
            Ok(())
        }),
    },
    numvar!(
        "FORKGC_SLEEP_BEFORE_EXIT",
        "set the amount of seconds for the fork GC to sleep before exists, should always be set to 0 (other then on tests).",
        "search-fork-gc-sleep-before-exit",
        |c, ac| set_size(c, ac, "search-fork-gc-sleep-before-exit", GE0)
    ),
    numvar!("MAXDOCTABLESIZE", "Maximum runtime document table size (for this process)", "search-max-doctablesize"),
    Var {
        name: "MAXSEARCHRESULTS",
        help: "Maximum number of results from ft.search command",
        get: |c| Some(unlimited(num(c, "search-max-search-results"))),
        set: Some(|c, ac| {
            let v = ac.get_ll(0).map_err(parse_err)?;
            put(c, "search-max-search-results", max_results(v));
            Ok(())
        }),
    },
    Var {
        name: "MAXAGGREGATERESULTS",
        help: "Maximum number of results from ft.aggregate command",
        get: |c| Some(unlimited(num(c, "search-max-aggregate-results"))),
        set: Some(|c, ac| {
            let v = ac.get_ll(0).map_err(parse_err)?;
            put(c, "search-max-aggregate-results", max_results(v));
            Ok(())
        }),
    },
    numvar!(
        "MAX_AGGREGATE_GROUPS",
        "Maximum number of GROUPBY groups materialized by ft.aggregate command",
        "search-max-aggregate-groups",
        |c, ac| {
            let v = ac.get_ll(GE1).map_err(parse_err)?;
            if v > 1 << 26 {
                return Err(QErr::new(Code::Limit, "Value exceeds maximum possible aggregate groups"));
            }
            put(c, "search-max-aggregate-groups", v as i128);
            Ok(())
        }
    ),
    numvar!("MAXEXPANSIONS", "Maximum prefix expansions to be used in a query", "search-max-prefix-expansions", |c, ac| set_clamped(
        c,
        ac,
        "search-max-prefix-expansions"
    )),
    numvar!("MAXPREFIXEXPANSIONS", "Maximum prefix expansions to be used in a query", "search-max-prefix-expansions", |c, ac| {
        set_clamped(c, ac, "search-max-prefix-expansions")
    }),
    numvar!("TIMEOUT", "Query (search) timeout", "search-timeout", |c, ac| {
        let v = ac.get_ll(GE0).map_err(parse_err)?;
        put(c, "search-timeout", v as i128);
        Ok(())
    }),
    numvar!(
        "_MAX_FOREGROUND_TIMEOUT_LIMIT",
        "Maximum allowed value (ms) for search-timeout and per-query TIMEOUT when workers are disabled (0 = unlimited)",
        "search-_max-foreground-timeout-limit",
        |c, ac| {
            let v = ac.get_ll(GE0).map_err(parse_err)?;
            put(c, "search-_max-foreground-timeout-limit", v as i128);
            Ok(())
        }
    ),
    numvar!(
        "WORKERS",
        "Number of worker threads to use for query processing and background tasks. Default is 0. This configuration also affects the number of connections per shard. See CONN_PER_SHARD.",
        "search-workers",
        |c, ac| {
            let v = ac.get_size(GE0).map_err(parse_err)?;
            if v > 16 {
                return Err(too_many_threads());
            }
            put(c, "search-workers", v as i128);
            Ok(())
        }
    ),
    numvar!(
        "MIN_OPERATION_WORKERS",
        "Number of worker threads to use for background tasks when the server is in an operation event. Default is 4",
        "search-min-operation-workers",
        |c, ac| {
            let v = ac.get_size(GE0).map_err(parse_err)?;
            if v > 16 {
                return Err(too_many_threads());
            }
            put(c, "search-min-operation-workers", v as i128);
            Ok(())
        }
    ),
    Var {
        name: "WORKER_THREADS",
        help: "Deprecated, see WORKERS and MIN_OPERATION_WORKERS",
        // MT_MODE is always MT_MODE_OFF here (it can only be set at load time).
        get: |_| Some("0".into()),
        set: None,
    },
    Var { name: "MT_MODE", help: "Deprecated, see WORKERS and MIN_OPERATION_WORKERS", get: |_| Some("MT_MODE_OFF".into()), set: None },
    numvar!(
        "TIERED_HNSW_BUFFER_LIMIT",
        "Use for setting the buffer limit threshold for vector similarity tiered HNSW index, so that if we are using WORKERS for indexing, and the number of vectors waiting in the buffer to be indexed exceeds this limit, we insert new vectors directly into HNSW",
        "search-tiered-hnsw-buffer-limit"
    ),
    numvar!("PRIVILEGED_THREADS_NUM", "Deprecated. See `WORKERS_PRIORITY_BIAS_THRESHOLD`", "search-workers-priority-bias-threshold"),
    numvar!(
        "WORKERS_PRIORITY_BIAS_THRESHOLD",
        "The number of high priority tasks to be executed at any given time by the worker thread pool, before executing low priority tasks. After this number of high priority tasks are being executed, the worker thread pool will execute high and low priority tasks alternately.",
        "search-workers-priority-bias-threshold"
    ),
    Var {
        name: "FRISOINI",
        help: "Path to Chinese dictionary configuration file (for Chinese tokenization)",
        get: |c| text_or_null(c.text("search-friso-ini")),
        set: None,
    },
    Var {
        name: "DEFAULT_SCORER",
        help: "Default scorer to use when no scorer is specified in queries",
        get: |c| text_or_null(c.text("search-default-scorer:value")),
        set: Some(|c, ac| {
            let a = ac.get().map_err(parse_err)?;
            let s = String::from_utf8_lossy(crate::util::cstr(a)).into_owned();
            if !super::scorer_exists(&s) {
                return Err(QErr::new(Code::BadVal, "Invalid default scorer value"));
            }
            c.extra.insert("search-default-scorer:value", Val::Str(Some(s)));
            Ok(())
        }),
    },
    Var {
        name: "ON_TIMEOUT",
        help: "Action to perform when search timeout is exceeded (choose RETURN or FAIL)",
        get: |c| Some(TIMEOUT_POLICIES[num(c, "search-on-timeout") as usize].into()),
        set: Some(|c, ac| {
            let a = ac.get().map_err(parse_err)?;
            let Some(i) = TIMEOUT_POLICIES.iter().position(|p| a.eq_ignore_ascii_case(p.as_bytes())) else {
                return Err(QErr::new(Code::BadVal, "Invalid ON_TIMEOUT value"));
            };
            put(c, "search-on-timeout", i as i128);
            Ok(())
        }),
    },
    numvar!("GCSCANSIZE", "Scan this many documents at a time during every GC iteration", "search-gc-scan-size", |c, ac| set_size(
        c,
        ac,
        "search-gc-scan-size",
        GE1
    )),
    numvar!(
        "MIN_PHONETIC_TERM_LEN",
        "Minimum length of term to be considered for phonetic matching",
        "search-min-phonetic-term-len",
        |c, ac| set_size(c, ac, "search-min-phonetic-term-len", GE1)
    ),
    Var { name: "GC_POLICY", help: "gc policy to use (DEFAULT/LEGACY)", get: |_| Some("fork".into()), set: None },
    numvar!(
        "FORK_GC_RUN_INTERVAL",
        "interval (in seconds) in which to run the fork gc (relevant only when fork gc is used)",
        "search-fork-gc-run-interval",
        |c, ac| set_size(c, ac, "search-fork-gc-run-interval", GE1)
    ),
    numvar!(
        "FORK_GC_CLEAN_THRESHOLD",
        "the fork gc will only start to clean when the number of not cleaned document will exceed this threshold",
        "search-fork-gc-clean-threshold",
        |c, ac| set_size(c, ac, "search-fork-gc-clean-threshold", 0)
    ),
    numvar!(
        "FORK_GC_RETRY_INTERVAL",
        "interval (in seconds) in which to retry running the forkgc after failure.",
        "search-fork-gc-retry-interval",
        |c, ac| set_size(c, ac, "search-fork-gc-retry-interval", GE1)
    ),
    Var {
        name: "FORK_GC_CLEAN_NUMERIC_EMPTY_NODES",
        help: "clean empty nodes from numeric tree",
        get: |c| tf(!c.flag("ft:no-clean-numeric-empty-nodes")),
        // A flag: takes no value.
        set: Some(|c, _| {
            c.extra.insert("ft:no-clean-numeric-empty-nodes", Val::Bool(false));
            Ok(())
        }),
    },
    Var {
        name: "_FORK_GC_CLEAN_NUMERIC_EMPTY_NODES",
        help: "clean empty nodes from numeric tree",
        get: |c| tf(!c.flag("ft:no-clean-numeric-empty-nodes")),
        set: Some(|c, ac| {
            let b = bool_arg(ac)?;
            c.extra.insert("ft:no-clean-numeric-empty-nodes", Val::Bool(!b));
            Ok(())
        }),
    },
    numvar!(
        "UNION_ITERATOR_HEAP",
        "minimum number of iterators in a union from which the iterator willswitch to heap based implementation.",
        "search-union-iterator-heap",
        |c, ac| set_clamped(c, ac, "search-union-iterator-heap")
    ),
    numvar!(
        "CURSOR_MAX_IDLE",
        "max idle time allowed to be set for cursor, setting it height might cause high memory consumption.",
        "search-cursor-max-idle",
        |c, ac| {
            let v = ac.get_ll(GE1).map_err(parse_err)?;
            put(c, "search-cursor-max-idle", v as i128);
            Ok(())
        }
    ),
    numvar!(
        "INDEX_CURSOR_LIMIT",
        "Max number of cursors for a given index that can be opened inside of a shard. Default is 128",
        "search-index-cursor-limit",
        |c, ac| {
            let v = ac.get_ll(GE0).map_err(parse_err)?;
            put(c, "search-index-cursor-limit", v as i128);
            Ok(())
        }
    ),
    Var { name: "NO_MEM_POOLS", help: "Set RediSearch to run without memory pools", get: |c| tf(c.flag("search-no-mem-pools")), set: None },
    Var {
        name: "PARTIAL_INDEXED_DOCS",
        help: "Enable commands filter which optimize indexing on partial hash updates",
        get: |c| tf(c.flag("search-partial-indexed-docs")),
        set: None,
    },
    Var {
        name: "UPGRADE_INDEX",
        help: "Relevant only when loading an v1.x rdb, specify argument for upgrading the index.",
        get: |_| Some("Upgrade config for upgrading".into()),
        set: None,
    },
    boolvar!("_NUMERIC_COMPRESS", "Enable legacy compression of double to float.", "search-_numeric-compress"),
    boolvar!(
        "_FREE_RESOURCE_ON_THREAD",
        "Determine whether some index resources are free on a second thread.",
        "search-_free-resource-on-thread"
    ),
    boolvar!("_PRINT_PROFILE_CLOCK", "Disable print of time for ft.profile. For testing only.", "search-_print-profile-clock"),
    Var {
        name: "RAW_DOCID_ENCODING",
        help: "Disable compression for DocID inverted index. Boost CPU performance.",
        get: |c| tf(c.flag("search-raw-docid-encoding")),
        set: None,
    },
    Var {
        name: "_NUMERIC_RANGES_PARENTS",
        help: "Keep numeric ranges in numeric tree parent nodes of leafs for `x` generations.",
        get: |c| Some(num(c, "search-_numeric-ranges-parents").to_string()),
        set: Some(|c, ac| {
            let v = ac.get_size(GE0).map_err(parse_err)?;
            if v > 2 {
                return Err(QErr::new(Code::ParseArgs, "Max depth for range cannot be higher than max depth for balance"));
            }
            put(c, "search-_numeric-ranges-parents", v as i128);
            Ok(())
        }),
    },
    numvar!(
        "DEFAULT_DIALECT",
        "Set RediSearch default dialect version through the lifetime of the server.",
        "search-default-dialect",
        |c, ac| {
            let v = ac.get_u32(GE1).map_err(parse_err)?;
            if v > 4 {
                return Err(QErr::user(Code::BadVal, "Default dialect version cannot be higher than ", "4"));
            }
            put(c, "search-default-dialect", v as i128);
            Ok(())
        }
    ),
    Var {
        name: "VSS_MAX_RESIZE",
        help: "Set RediSearch vector indexes max resize (in bytes).",
        get: |c| Some(num(c, "search-vss-max-resize").to_string()),
        set: Some(|c, ac| {
            let v = ac.get_size(GE0).map_err(parse_err)?;
            // Stored in an `unsigned int`.
            put(c, "search-vss-max-resize", (v as u32) as i128);
            Ok(())
        }),
    },
    numvar!(
        "MULTI_TEXT_SLOP",
        "Set RediSearch delta used to increase positional offsets between array slots for multi text values.Can control the level of separation between phrases in different array slots (related to the SLOP parameter of ft.search command)",
        "search-multi-text-slop"
    ),
    numvar!(
        "BG_INDEX_SLEEP_GAP",
        "The number of iterations to run while performing background indexing before we call usleep(1) (sleep for 1 micro-second) and make sure that we allow redis process other commands.",
        "search-bg-index-sleep-gap"
    ),
    boolvar!(
        "_PRIORITIZE_INTERSECT_UNION_CHILDREN",
        "Intersection iterator orders the children iterators by their relative estimated number of results in ascending order, so that if we see first iterators with a lower count of results we will skip a larger number of results, which translates into faster iteration. If this flag is set, we use this optimization in a way where union iterators are being factorize by the number of their own children, so that we sort by the number of children times the overall estimated number of results instead.",
        "search-_prioritize-intersect-union-children"
    ),
    boolvar!("ENABLE_UNSTABLE_FEATURES", "Enable unstable features.", "search-enable-unstable-features"),
    numvar!(
        "_BG_INDEX_MEM_PCT_THR",
        "Set the percentage of memory usage threshold (out of maxmemory) at which background indexing will stop. The default is 100 percent.",
        "search-_bg-index-mem-pct-thr",
        |c, ac| {
            let v = ac.get_u8(GE0).map_err(parse_err)?;
            if v > 100 {
                return Err(QErr::new(Code::Limit, "Memory limit for indexing cannot be greater then 100%"));
            }
            put(c, "search-_bg-index-mem-pct-thr", v as i128);
            Ok(())
        }
    ),
    numvar!(
        "BM25STD_TANH_FACTOR",
        "Set the BM25STD.TANH stretch factor. This is an integer value that divides the argument of the tanh function that is used to normalize the score computed by the BM25STD scorer.The default value is 4.",
        "search-bm25std-tanh-factor",
        |c, ac| {
            let v = ac.get_u32(GE1).map_err(parse_err)?;
            if v > 10_000 {
                return Err(QErr::new(Code::Limit, "BM25STD_TANH_FACTOR must be between 1 and 10000 inclusive"));
            }
            put(c, "search-bm25std-tanh-factor", v as i128);
            Ok(())
        }
    ),
    numvar!(
        "_BG_INDEX_OOM_PAUSE_TIME",
        "Set the time (in seconds) given to the background indexing thread to sleep when it reaches the memory limit, giving time to reallocate memory.The default value is 5 seconds in Redis Enterprise, 0 in Redis OS.",
        "search-_bg-index-oom-pause-time",
        |c, ac| {
            let v = ac.get_u32(GE0).map_err(parse_err)?;
            put(c, "search-_bg-index-oom-pause-time", v as i128);
            Ok(())
        }
    ),
    numvar!(
        "INDEXER_YIELD_EVERY_OPS",
        "The number of operations to perform before yielding to Redis during indexing while loading",
        "search-indexer-yield-every-ops",
        |c, ac| {
            let v = ac.get_u32(GE1).map_err(parse_err)?;
            put(c, "search-indexer-yield-every-ops", v as i128);
            Ok(())
        }
    ),
    numvar!(
        "BG_INDEX_SLEEP_DURATION_US",
        "Sleep duration in microseconds during background indexing periodic sleep (max 999999, usleep POSIX limit)",
        "search-bg-index-sleep-duration-us",
        |c, ac| {
            let v = ac.get_u32(GE1).map_err(parse_err)?;
            if v > 999_999 {
                return Err(QErr::new(Code::Limit, "BG_INDEX_SLEEP_DURATION_US must be between 1 and 999999 (usleep POSIX limit)"));
            }
            put(c, "search-bg-index-sleep-duration-us", v as i128);
            Ok(())
        }
    ),
    Var {
        name: "ON_OOM",
        help: "Action to perform when search OOM is exceeded (choose RETURN, FAIL or IGNORE)",
        get: |c| Some(OOM_POLICIES[num(c, "search-on-oom") as usize].into()),
        set: Some(|c, ac| {
            let a = ac.get().map_err(parse_err)?;
            let Some(i) = OOM_POLICIES.iter().position(|p| a.eq_ignore_ascii_case(p.as_bytes())) else {
                return Err(QErr::new(Code::BadVal, "Invalid ON_OOM value"));
            };
            put(c, "search-on-oom", i as i128);
            Ok(())
        }),
    },
    numvar!(
        "_MIN_TRIM_DELAY_MS",
        "Minimum delay before checking trimming state after slot migration (in milliseconds)",
        "search-_min-trim-delay-ms",
        |c, ac| {
            let v = ac.get_u32(GE1).map_err(parse_err)? as i128;
            let max = num(c, "search-_max-trim-delay-ms");
            if v >= max {
                return Err(QErr::new(Code::ParseArgs, format!("_MIN_TRIM_DELAY_MS ({v}) must be less than _MAX_TRIM_DELAY_MS ({max})")));
            }
            put(c, "search-_min-trim-delay-ms", v);
            Ok(())
        }
    ),
    numvar!(
        "_MAX_TRIM_DELAY_MS",
        "Maximum delay before enabling trimming after slot migration (in milliseconds)",
        "search-_max-trim-delay-ms",
        |c, ac| {
            let v = ac.get_u32(GE1).map_err(parse_err)? as i128;
            let min = num(c, "search-_min-trim-delay-ms");
            if v <= min {
                return Err(QErr::new(Code::ParseArgs, format!("_MAX_TRIM_DELAY_MS ({v}) must be greater than _MIN_TRIM_DELAY_MS ({min})")));
            }
            put(c, "search-_max-trim-delay-ms", v);
            Ok(())
        }
    ),
    numvar!(
        "_TRIMMING_STATE_CHECK_DELAY_MS",
        "Delay between trimming state checks (in milliseconds)",
        "search-_trimming-state-check-delay-ms",
        |c, ac| {
            let v = ac.get_u32(GE1).map_err(parse_err)?;
            put(c, "search-_trimming-state-check-delay-ms", v as i128);
            Ok(())
        }
    ),
    Var {
        name: "_SIMULATE_IN_FLEX",
        help: "Simulate working under Flex conditions. This is used for testing only.",
        get: |c| tf(c.flag("search-_simulate-in-flex")),
        set: None,
    },
    Var {
        name: "search-disk-drop-read-cache",
        help: "Drop OS read cache after each SpeedB read (yes/no, default no)",
        get: |c| tf(c.flag("search-disk-drop-read-cache")),
        set: None,
    },
    Var {
        name: "search-disk-use-direct-reads",
        help: "Use O_DIRECT for SpeedB reads (yes/no, default no)",
        get: |c| tf(c.flag("search-disk-use-direct-reads")),
        set: None,
    },
    // The coordinator's options.
    Var { name: "PARTITIONS", help: "Number of RediSearch partitions to use", get: |_| Some("AUTO".into()), set: None },
    Var {
        name: "CLUSTER_TIMEOUT",
        help: "Cluster synchronization timeout",
        get: |c| Some(num(c, "ft:cluster-timeout").to_string()),
        set: Some(|c, ac| {
            let v = ac.get_int(GE1).map_err(parse_err)?;
            c.extra.insert("ft:cluster-timeout", Val::Num(v as i128));
            Ok(())
        }),
    },
    Var {
        name: "OSS_GLOBAL_PASSWORD",
        help: "Deprecated, Global oss cluster password that will be used to connect to other shards",
        get: |_| Some("Password: *******".into()),
        // Consumes its value if there is one; never fails.
        set: Some(|_, ac| {
            let _ = ac.advance();
            Ok(())
        }),
    },
    Var {
        name: "CONN_PER_SHARD",
        help: "Number of connections to each shard in the cluster. Default to 0. If 0, the number of connections is set to `WORKERS` + 1.",
        get: |c| Some(num(c, "ft:conn-per-shard").to_string()),
        set: Some(|c, ac| {
            let v = ac.get_size(GE0).map_err(parse_err)?;
            c.extra.insert("ft:conn-per-shard", Val::Num(v as i128));
            Ok(())
        }),
    },
    Var {
        name: "CURSOR_REPLY_THRESHOLD",
        help: "Maximum number of replies to accumulate before triggering `_FT.CURSOR READ` on the shards",
        get: |c| Some(c.extra.get("ft:cursor-reply-threshold").map_or(1, Val::num).to_string()),
        set: Some(|c, ac| {
            let v = ac.get_size(GE1).map_err(parse_err)?;
            c.extra.insert("ft:cursor-reply-threshold", Val::Num(v as i128));
            Ok(())
        }),
    },
    Var { name: "SEARCH_THREADS", help: "Sets the number of search threads in the coordinator thread pool", get: |_| Some("20".into()), set: None },
    Var { name: "SEARCH_IO_THREADS", help: "Sets the number of I/O threads in the coordinator", get: |_| Some("1".into()), set: None },
    Var {
        name: "TOPOLOGY_VALIDATION_TIMEOUT",
        help: "Sets the timeout for topology validation (in milliseconds). After this timeout, any pending requests will be processed, even if the topology is not fully connected. Default is 30000 (30 seconds). 0 means no timeout.",
        get: |c| Some(c.extra.get("ft:topology-validation-timeout").map_or(30_000, Val::num).to_string()),
        set: Some(|c, ac| {
            let v = ac.get_size(GE0).map_err(parse_err)?;
            c.extra.insert("ft:topology-validation-timeout", Val::Num(v as i128));
            Ok(())
        }),
    },
    Var {
        name: "CONNECT_TIMEOUT",
        help: "Sets the per-attempt timeout for inter-shard connection setup (in milliseconds). Bounds the TCP+TLS handshake so a blackholed SYN does not stall a connection indefinitely. Default is 10000 (10 seconds). 0 disables the timeout.",
        get: |c| Some(c.extra.get("ft:connect-timeout").map_or(10_000, Val::num).to_string()),
        set: Some(|c, ac| {
            let v = ac.get_int(GE0).map_err(parse_err)?;
            c.extra.insert("ft:connect-timeout", Val::Num(v as i128));
            Ok(())
        }),
    },
];

/// `findConfigVar`: case-insensitive.
pub fn find(name: &[u8]) -> Option<&'static Var> {
    let name = crate::util::cstr(name);
    VARS.iter().find(|v| v.name.as_bytes().eq_ignore_ascii_case(name))
}
