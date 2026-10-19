//! Search commands (FT.*), as the RediSearch module bundled with Redis 8.10 (v8.10.1)
//! registers them on a standalone server: the coordinator's FT.* commands run locally (one
//! shard), the write commands through their `_FT.*` internal twins, which are what replicates.

use super::*;
use crate::cmd;
use crate::search::args::Ac;
use crate::search::config as ftcfg;
use crate::search::error::{Code, QErr};

/// Commands RediSearch registers as internal: unknown to ordinary connections (a master's link
/// and the log replayer may run them).
const INTERNAL_FLAG: u64 = INTERNAL | NOSCRIPT;

pub static COMMANDS: &[Command] = &[
    cmd!("FT.ADD", -1, WRITE | DENYOOM | MODULE, 2, 2, 1, super::ft_doc::ft_add, Module),
    cmd!("FT.AGGREGATE", -3, READONLY | MODULE, 0, 0, 0, ft_aggregate, Module),
    cmd!("FT.ALIASADD", 3, WRITE | DENYOOM | MODULE, 0, 0, 0, super::ft_admin::ft_aliasadd, Module),
    cmd!("FT.ALIASDEL", 2, WRITE | MODULE, 0, 0, 0, super::ft_admin::ft_aliasdel, Module),
    cmd!("FT.ALIASLIST", 2, READONLY | MODULE, 0, 0, 0, super::ft_admin::ft_aliaslist, Module),
    cmd!("FT.ALIASUPDATE", 3, WRITE | DENYOOM | MODULE, 0, 0, 0, super::ft_admin::ft_aliasupdate, Module),
    cmd!("FT.ALTER", -6, WRITE | DENYOOM | MODULE, 0, 0, 0, super::ft_admin::ft_alter, Module),
    crate::container!("FT.CONFIG", READONLY | MODULE, Module, FT_CONFIG),
    cmd!("FT.CREATE", -5, WRITE | DENYOOM | MODULE, 0, 0, 0, super::ft::ft_create, Module),
    crate::container!("FT.CURSOR", READONLY | MODULE, Module, FT_CURSOR),
    cmd!("FT.DEL", -1, WRITE | MODULE, 2, 2, 1, super::ft_doc::ft_del, Module),
    cmd!("FT.DICTADD", -3, WRITE | DENYOOM | MODULE, 0, 0, 0, super::ft_admin::ft_dictadd, Module),
    cmd!("FT.DICTDEL", -3, WRITE | MODULE, 0, 0, 0, super::ft_admin::ft_dictdel, Module),
    cmd!("FT.DICTDUMP", 2, READONLY | MODULE, 0, 0, 0, super::ft_admin::ft_dictdump, Module),
    cmd!("FT.DROP", -1, WRITE | MODULE, 0, 0, 0, super::ft::ft_drop, Module),
    cmd!("FT.DROPINDEX", -2, WRITE | MODULE, 0, 0, 0, super::ft::ft_dropindex, Module),
    cmd!("FT.EXPLAIN", -3, READONLY | MODULE, 0, 0, 0, super::ft_query::ft_explain, Module),
    cmd!("FT.EXPLAINCLI", -3, READONLY | MODULE, 0, 0, 0, super::ft_query::ft_explaincli, Module),
    cmd!("FT.GET", -1, READONLY | MODULE, 2, 2, 1, super::ft_doc::ft_get, Module),
    cmd!("FT.HYBRID", -7, READONLY | MODULE, 0, 0, 0, ft_hybrid, Module),
    cmd!("FT.INFO", 2, READONLY | MODULE, 0, 0, 0, super::ft::ft_info, Module),
    cmd!("FT.MGET", -1, READONLY | MODULE, 0, 0, 0, super::ft_doc::ft_mget, Module),
    cmd!("FT.PROFILE", -5, READONLY | MODULE, 0, 0, 0, ft_profile, Module),
    cmd!("FT.SAFEADD", -1, WRITE | DENYOOM | MODULE, 2, 2, 1, super::ft_doc::ft_add, Module),
    cmd!("FT.SEARCH", -3, READONLY | MODULE, 0, 0, 0, ft_search, Module),
    cmd!("FT.SPELLCHECK", -3, READONLY | MODULE, 0, 0, 0, super::ft_spell::ft_spellcheck, Module),
    cmd!("FT.SUGADD", -4, WRITE | DENYOOM | MODULE, 1, 1, 1, super::ft_sug::sugadd, Module),
    cmd!("FT.SUGDEL", 3, WRITE | MODULE, 1, 1, 1, super::ft_sug::sugdel, Module),
    cmd!("FT.SUGGET", -3, READONLY | MODULE, 1, 1, 1, super::ft_sug::sugget, Module),
    cmd!("FT.SUGLEN", 2, READONLY | MODULE, 1, 1, 1, super::ft_sug::suglen, Module),
    cmd!("FT.SYNADD", -1, WRITE | DENYOOM | MODULE, 0, 0, 0, super::ft_admin::ft_synadd, Module),
    cmd!("FT.SYNDUMP", 2, READONLY | MODULE, 0, 0, 0, super::ft_admin::ft_syndump, Module),
    cmd!("FT.SYNUPDATE", -4, WRITE | DENYOOM | MODULE, 0, 0, 0, super::ft_admin::ft_synupdate, Module),
    cmd!("FT.TAGVALS", 3, READONLY | MODULE, 0, 0, 0, super::ft_admin::ft_tagvals, Module),
    cmd!("FT._ALIASADDIFNX", 3, WRITE | DENYOOM | MODULE, 0, 0, 0, super::ft_admin::ft_aliasaddifnx, Module),
    cmd!("FT._ALIASDELIFX", 2, WRITE | MODULE, 0, 0, 0, super::ft_admin::ft_aliasdelifx, Module),
    cmd!("FT._ALTERIFNX", -6, WRITE | DENYOOM | MODULE, 0, 0, 0, super::ft_admin::ft_alterifnx, Module),
    cmd!("FT._CREATEIFNX", -5, WRITE | DENYOOM | MODULE, 0, 0, 0, super::ft::ft_createifnx, Module),
    cmd!("FT._DROPIFX", -1, WRITE | MODULE, 0, 0, 0, super::ft::ft_dropifx, Module),
    cmd!("FT._DROPINDEXIFX", -2, WRITE | MODULE, 0, 0, 0, super::ft::ft_dropindexifx, Module),
    cmd!("FT._LIST", -1, READONLY | MODULE, 0, 0, 0, super::ft::ft_list, Module),
    crate::container!("_FT.CONFIG", READONLY | MODULE, Module, &FT_CONFIG_LOCAL),
    crate::container!("_FT.DEBUG", READONLY | MODULE, Module, FT_DEBUG),
    cmd!("search.CLUSTERINFO", -1, READONLY | MODULE | NOSCRIPT | LOADING, 0, 0, 0, cluster_info, Module),
    cmd!("search.CLUSTERREFRESH", -1, READONLY | MODULE | NOSCRIPT, 0, 0, 0, cluster_refresh, Module),
    cmd!("search.CLUSTERSET", -1, READONLY | MODULE | NOSCRIPT | LOADING, 0, 0, 0, cluster_set, Module),
    // Internal (`_FT.*`): the shard-local commands the FT.* ones run.
    crate::container!("_FT.CURSOR", READONLY | MODULE | INTERNAL_FLAG, Module, FT_CURSOR_LOCAL),
    cmd!("_FT.CREATE", -1, WRITE | DENYOOM | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft::int_create, Module),
    cmd!("_FT._CREATEIFNX", -1, WRITE | DENYOOM | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft::int_createifnx, Module),
    cmd!("_FT.ALTER", -1, WRITE | DENYOOM | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft_admin::int_alter, Module),
    cmd!("_FT._ALTERIFNX", -1, WRITE | DENYOOM | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft_admin::int_alterifnx, Module),
    cmd!("_FT.DROPINDEX", -1, WRITE | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft::int_dropindex, Module),
    cmd!("_FT._DROPINDEXIFX", -1, WRITE | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft::int_dropindexifx, Module),
    cmd!("_FT.DROP", -1, WRITE | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft::int_drop, Module),
    cmd!("_FT._DROPIFX", -1, WRITE | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft::int_dropifx, Module),
    cmd!("_FT.DICTADD", -1, WRITE | DENYOOM | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft_admin::int_dictadd, Module),
    cmd!("_FT.DICTDEL", -1, WRITE | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft_admin::int_dictdel, Module),
    cmd!("_FT.ALIASADD", -1, WRITE | DENYOOM | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft_admin::int_aliasadd, Module),
    cmd!("_FT._ALIASADDIFNX", -1, WRITE | DENYOOM | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft_admin::int_aliasaddifnx, Module),
    cmd!("_FT.ALIASDEL", -1, WRITE | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft_admin::int_aliasdel, Module),
    cmd!("_FT._ALIASDELIFX", -1, WRITE | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft_admin::int_aliasdelifx, Module),
    cmd!("_FT.ALIASUPDATE", -1, WRITE | DENYOOM | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft_admin::int_aliasupdate, Module),
    cmd!("_FT.SYNUPDATE", -1, WRITE | DENYOOM | MODULE | INTERNAL_FLAG, 0, 0, 0, super::ft_admin::int_synupdate, Module),
];

static FT_CONFIG: &[Command] = &[
    cmd!("GET", -3, READONLY | MODULE, 0, 0, 0, ft_config, Module),
    cmd!("SET", -3, WRITE | MODULE, 0, 0, 0, ft_config, Module),
    cmd!("HELP", -3, READONLY | MODULE, 0, 0, 0, ft_config, Module),
];

/// A static of its own (not a reference to a constant like FT.CONFIG's): identical constants
/// may share an address, and commands are told apart by address.
static FT_CONFIG_LOCAL: [Command; 3] = [
    cmd!("GET", -3, READONLY | MODULE, 0, 0, 0, ft_config, Module),
    cmd!("SET", -3, WRITE | MODULE, 0, 0, 0, ft_config, Module),
    cmd!("HELP", -3, READONLY | MODULE, 0, 0, 0, ft_config, Module),
];

static FT_CURSOR: &[Command] = &[
    cmd!("GC", -2, READONLY | MODULE, 0, 0, 0, super::ft_aggregate::ft_cursor_gc, Module),
    cmd!("DEL", -4, READONLY | MODULE, 0, 0, 0, super::ft_aggregate::ft_cursor_del, Module),
    cmd!("READ", -4, READONLY | MODULE, 0, 0, 0, ft_cursor_read, Module),
];

static FT_CURSOR_LOCAL: &[Command] = &[
    cmd!("GC", -2, READONLY | MODULE, 0, 0, 0, super::ft_aggregate::ft_cursor_gc, Module),
    cmd!("PROFILE", -2, READONLY | MODULE, 0, 0, 0, super::ft_aggregate::ft_cursor_profile, Module),
    cmd!("DEL", -4, READONLY | MODULE, 0, 0, 0, super::ft_aggregate::ft_cursor_del, Module),
    cmd!("READ", -4, READONLY | MODULE, 0, 0, 0, super::ft_aggregate::ft_cursor_read, Module),
];

macro_rules! debug_subs {
    ($($n:literal),* $(,)?) => {
        &[$(cmd!($n, -2, READONLY | MODULE, 0, 0, 0, super::ft_debug::ft_debug, Module)),*]
    };
}

static FT_DEBUG: &[Command] = debug_subs![
    "DUMP_INVIDX",
    "DUMP_NUMIDX",
    "DUMP_NUMIDXTREE",
    "DUMP_TAGIDX",
    "INFO_TAGIDX",
    "DUMP_GEOMIDX",
    "DUMP_PREFIX_TRIE",
    "IDTODOCID",
    "DOCIDTOID",
    "DOCINFO",
    "DUMP_PHONETIC_HASH",
    "DUMP_SUFFIX_TRIE",
    "DUMP_TERMS",
    "INVIDX_SUMMARY",
    "NUMIDX_SUMMARY",
    "GC_FORCEINVOKE",
    "GC_FORCEBGINVOKE",
    "GC_CLEAN_NUMERIC",
    "GC_STOP_SCHEDULE",
    "GC_CONTINUE_SCHEDULE",
    "GC_WAIT_FOR_JOBS",
    "GIT_SHA",
    "TTL",
    "TTL_PAUSE",
    "TTL_EXPIRE",
    "VECSIM_INFO",
    "DELETE_LOCAL_CURSORS",
    "DUMP_HNSW",
    "SET_MONITOR_EXPIRATION",
    "WORKERS",
    "INDEXES",
    "INFO",
    "GET_HIDE_USER_DATA_FROM_LOGS",
    "YIELDS_COUNTER",
    "BG_SCAN_CONTROLLER",
    "INDEXER_SLEEP_BEFORE_YIELD_MICROS",
    "QUERY_CONTROLLER",
    "DUMP_SCHEMA",
    "GET_MAX_DOC_ID",
    "DUMP_DELETED_IDS",
    "SPEC_INVIDXES_INFO",
    "VECSIM_MOCK_TIMEOUT",
    "REGISTER_TEST_SCORERS",
    "SET_MAX_INDEXES",
    "DISK_IO_CONTROL",
    "DISK_FLUSH",
    "FT.SEARCH",
    "FT.AGGREGATE",
    "FT.PROFILE",
    "FT.HYBRID",
    "_FT.SEARCH",
    "_FT.AGGREGATE",
    "_FT.PROFILE",
    "_FT.HYBRID",
    "SHARD_CONNECTION_STATES",
    "PAUSE_TOPOLOGY_UPDATER",
    "RESUME_TOPOLOGY_UPDATER",
    "CLEAR_PENDING_TOPOLOGY",
    "COORD_THREADS",
    "DELETE_LOCAL_COORD_CURSORS",
    "HELP",
];

/// ACL categories RediSearch declares for its commands (`CreateCommandWithAcl`): `search` plus
/// the listed ones; internal commands and subcommands have none.
pub fn acl_categories(c: &Command) -> Option<&'static [&'static str]> {
    // `_FT.DEBUG`'s subcommands share names with the commands but declare no categories.
    if FT_DEBUG.iter().any(|s| std::ptr::eq(s, c)) {
        return Some(&[]);
    }
    let n = c.name.to_ascii_lowercase();
    Some(match n.as_str() {
        "ft.add" | "ft.del" | "ft.safeadd" | "ft._dropifx" | "ft.sugadd" | "ft.sugdel" => &["write", "search"],
        "ft.aggregate" | "ft.cursor" | "ft.get" | "ft.hybrid" | "ft.mget" | "ft.profile" | "ft.search" | "ft.sugget" | "ft.suglen" => {
            &["read", "search"]
        }
        "ft.config" | "_ft.config" => &["admin", "search"],
        "ft.drop" | "ft.dropindex" | "ft._dropindexifx" => &["write", "slow", "dangerous", "search"],
        "ft.tagvals" => &["read", "slow", "dangerous", "search"],
        "ft._list" => &["admin", "slow", "search"],
        "_ft.debug" => &["admin", "slow", "dangerous", "search"],
        _ if n.starts_with("ft.") || n.starts_with("search.") => &["search"],
        _ => return None,
    })
}

// The query commands' INFO counters (`RSGlobalStats.totalStats.queries`) are counted where the
// module counts them, in the queries' execution (`QueryStats`).

fn ft_search(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    super::ft_query::ft_search(cx, args)
}

fn ft_aggregate(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    super::ft_aggregate::ft_aggregate(cx, args)
}

fn ft_profile(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    super::ft_aggregate::ft_profile(cx, args)
}

fn ft_hybrid(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    super::ft_aggregate::ft_hybrid(cx, args)
}

fn ft_cursor_read(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    super::ft_aggregate::ft_cursor_read(cx, args)
}

/// FT.CONFIG GET|SET|HELP (`ConfigCommand`).
fn ft_config(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let resp3 = cx.resp() == 3;
    let action = &args[1];
    let name = &args[2];
    let dump = |cx: &Cx, help: bool| -> Reply {
        let one = |v: &ftcfg::Var| -> (Reply, Reply) {
            let val = match (v.get)(cx.cfg()) {
                Some(s) => Reply::bulk(s),
                None => Reply::Nil,
            };
            let body = if help {
                let kv = vec![
                    (Reply::status("Description"), Reply::status(v.help)),
                    (Reply::status("Value"), val),
                ];
                if resp3 { Reply::Map(kv) } else { Reply::Array(kv.into_iter().flat_map(|(k, v)| [k, v]).collect()) }
            } else {
                val
            };
            (Reply::status(v.name), body)
        };
        let vars: Vec<&ftcfg::Var> = if crate::util::cstr(name) == b"*" {
            ftcfg::VARS.iter().collect()
        } else {
            ftcfg::find(name).into_iter().collect()
        };
        let pairs: Vec<(Reply, Reply)> = vars.into_iter().map(one).collect();
        if resp3 {
            Reply::Map(pairs)
        } else {
            Reply::Array(
                pairs
                    .into_iter()
                    .map(|(k, v)| match v {
                        Reply::Array(mut items) => {
                            items.insert(0, k);
                            Reply::Array(items)
                        }
                        v => Reply::Array(vec![k, v]),
                    })
                    .collect(),
            )
        }
    };
    if crate::search::args::c_eq(action, "GET") {
        Ok(dump(cx, false))
    } else if crate::search::args::c_eq(action, "HELP") {
        Ok(dump(cx, true))
    } else if crate::search::args::c_eq(action, "SET") {
        let Some(var) = ftcfg::find(name) else {
            return Err(QErr::code(Code::NoOption).into());
        };
        let Some(set) = var.set else {
            return Err(QErr::new(Code::BadOption, "Not modifiable at runtime").into());
        };
        let mut ac = Ac { objs: args, offset: 3, sds: false };
        let mut cfg = cx.eng.cfg.clone();
        set(&mut cfg, &mut ac).map_err(Error::from)?;
        let workers = cfg.num("search-workers");
        cx.eng.cfg = cfg;
        cx.eng.search.set_vec_mode(workers);
        if ac.offset != args.len() {
            Ok(Reply::status("EXCESSARGS"))
        } else {
            Ok(Reply::Ok)
        }
    } else {
        Ok(Reply::status("No such configuration action"))
    }
}

/// `search.CLUSTERINFO`: the coordinator answers from its I/O thread, which a standalone server
/// never starts: the client stays blocked.
fn cluster_info(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    if !cx.can_block() {
        return Err(Error::new("ERR Blocking module command called from transaction"));
    }
    Ok(cx.block_on(Vec::new(), 0, Reply::Nil))
}

fn cluster_refresh(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Ok)
}

/// `search.CLUSTERSET` (`RedisEnterprise_ParseTopology`): the topology's syntax errors; a valid
/// topology is accepted (a standalone server has nothing to fan out to).
fn cluster_set(_cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    crate::search::cluster::parse_topology(args).map(|_| Reply::Ok).map_err(Error::msg)
}

