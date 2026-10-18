//! `_FT.DEBUG` (`debug_commands.c`, `coord/debug_commands.c`): the module's introspection and
//! test hooks — inverted index, numeric tree, tag and suffix dumps, document info, the fork GC's
//! manual runs and schedule, TEMPORARY index timers, obfuscated FT.INFO, the vector index's debug
//! info, and the switches tests flip (thread pools, background scan controller, limits). Every
//! subcommand first checks that debug commands are enabled (`enable-debug-command`), then its
//! arity; HELP lists them all.

use super::ft::{info_reply, not_found, obfuscated_name, safe};
use super::*;
use crate::search::args::Ac;
use crate::search::gc;
use crate::search::index::Index;
use crate::search::numtree::{NodeId, NumericRange, NumericRangeTree};
use crate::search::spec;
use crate::search::vecsim::VecIndex;

const NODEBUG: &str = "Debug commands are disabled, please follow the redis configuration guide to enable them";

/// `commands[]` then `coordCommandsNames[]` (HELP's order).
const HELP: &[&str] = &[
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
    "SPEC_INVIDXES_INFO",
    "GC_FORCEINVOKE",
    "GC_FORCEBGINVOKE",
    "DISK_FLUSH",
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
    "DELETE_LOCAL_COORD_CURSORS",
    "DUMP_HNSW",
    "SET_MONITOR_EXPIRATION",
    "WORKERS",
    "COORD_THREADS",
    "BG_SCAN_CONTROLLER",
    "INDEXES",
    "INFO",
    "GET_HIDE_USER_DATA_FROM_LOGS",
    "YIELDS_COUNTER",
    "INDEXER_SLEEP_BEFORE_YIELD_MICROS",
    "QUERY_CONTROLLER",
    "DUMP_SCHEMA",
    "VECSIM_MOCK_TIMEOUT",
    "GET_MAX_DOC_ID",
    "DUMP_DELETED_IDS",
    "DISK_IO_CONTROL",
    "REGISTER_TEST_SCORERS",
    "SET_MAX_INDEXES",
    "FT.AGGREGATE",
    "_FT.AGGREGATE",
    "FT.SEARCH",
    "_FT.SEARCH",
    "FT.HYBRID",
    "_FT.HYBRID",
    "FT.PROFILE",
    "_FT.PROFILE",
    "SHARD_CONNECTION_STATES",
    "PAUSE_TOPOLOGY_UPDATER",
    "RESUME_TOPOLOGY_UPDATER",
    "CLEAR_PENDING_TOPOLOGY",
];

/// `INDEX_CURRENT_VERSION` (DUMP_SCHEMA's second element).
const INDEX_CURRENT_VERSION: i64 = 27;

/// `DEFAULT_MAX_INDEXES`.
const DEFAULT_MAX_INDEXES: i64 = 200_000;

fn err(msg: &str) -> Error {
    Error::msg(msg.to_string())
}

fn bulk(s: &str) -> Reply {
    Reply::bulk(s.as_bytes())
}

fn st(s: &str) -> Reply {
    Reply::status(s)
}

fn int(v: impl TryInto<i64>) -> Reply {
    Reply::Int(v.try_into().unwrap_or(i64::MAX))
}

/// `REPLY_WITH_LONG_LONG`: a bulk name and an integer, flat.
fn kv(out: &mut Vec<Reply>, name: &str, v: impl TryInto<i64>) {
    out.push(bulk(name));
    out.push(int(v));
}

/// The `_FT.DEBUG` subcommands: `args[1]` names the one registered.
pub(super) fn ft_debug(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let sub = String::from_utf8_lossy(&args[1]).to_ascii_uppercase();
    // `debugCommandsEnabled` (HELP doesn't ask).
    if sub != "HELP" && !super::server::protected_allowed(cx.cfg().enable_debug_command, cx.client) {
        return Err(err(NODEBUG));
    }
    let argc = args.len();
    let arity = || arity_error(&format!("_FT.DEBUG|{sub}"));
    match sub.as_str() {
        "HELP" => Ok(Reply::Array(HELP.iter().map(|n| bulk(n)).collect())),
        "DUMP_TERMS" => {
            if argc != 3 {
                return Err(arity());
            }
            let ix = sctx(cx, &args[2])?;
            let terms = ix.term_trie.iterate_all().collect_all();
            Ok(Reply::Array(terms.iter().filter_map(|e| crate::search::trie::runes_to_str(&e.0)).map(Reply::bulk).collect()))
        }
        "INVIDX_SUMMARY" | "DUMP_INVIDX" => {
            if argc != 4 {
                return Err(arity());
            }
            let ix = sctx(cx, &args[2])?;
            let Some(ii) = ix.terms.get(args[3].as_ref()) else { return Err(err("Can not find the inverted index")) };
            if sub == "DUMP_INVIDX" {
                return Ok(ids(ii));
            }
            let mut out = summary_header(ii);
            out.push(bulk("blocks"));
            for b in ii.blocks() {
                let mut blk = Vec::new();
                kv(&mut blk, "firstId", b.first_doc_id());
                kv(&mut blk, "lastId", b.last_doc_id());
                kv(&mut blk, "numEntries", b.num_entries());
                out.push(Reply::Array(blk));
            }
            Ok(Reply::Array(out))
        }
        "NUMIDX_SUMMARY" | "DUMP_NUMIDX" | "DUMP_NUMIDXTREE" => {
            let bad = match sub.as_str() {
                "NUMIDX_SUMMARY" => argc != 4,
                "DUMP_NUMIDX" => argc < 4,
                _ => !(4..=5).contains(&argc),
            };
            if bad {
                return Err(arity());
            }
            let ix = sctx(cx, &args[2])?;
            let fi = field_of(ix, &args[3], spec::T_NUMERIC | spec::T_GEO)?;
            let tree = ix.fields[fi].numeric.as_ref();
            Ok(match sub.as_str() {
                "NUMIDX_SUMMARY" => numidx_summary(tree),
                "DUMP_NUMIDX" => dump_numidx(tree, argc == 5),
                _ => dump_numidx_tree(tree, argc > 4 && args[4].eq_ignore_ascii_case(b"minimal")),
            })
        }
        "SPEC_INVIDXES_INFO" => {
            if argc != 3 {
                return Err(arity());
            }
            let ix = sctx(cx, &args[2])?;
            let mut out = Vec::new();
            kv(&mut out, "inverted_indexes_dict_size", ix.terms.len());
            kv(&mut out, "inverted_indexes_memory", ix.stats.inverted_size);
            Ok(Reply::Array(out))
        }
        "DUMP_TAGIDX" => {
            if argc != 4 {
                return Err(arity());
            }
            let ix = sctx(cx, &args[2])?;
            let fi = field_of(ix, &args[3], spec::T_TAG)?;
            let Some(tag) = &ix.fields[fi].tag else { return Ok(Reply::Array(Vec::new())) };
            let mut out = Vec::new();
            tag.values.for_each(|k, ii| {
                out.push(Reply::Array(vec![Reply::bulk(k), ids(ii)]));
                true
            });
            Ok(Reply::Array(out))
        }
        "INFO_TAGIDX" => info_tagidx(cx, args),
        "DUMP_SUFFIX_TRIE" => {
            if argc != 3 && argc != 4 {
                return Err(arity());
            }
            let ix = sctx(cx, &args[2])?;
            if argc == 3 {
                let Some(s) = &ix.suffix else { return Err(err("Index does not have suffix trie")) };
                return Ok(Reply::Array(s.keys().into_iter().map(Reply::bulk).collect()));
            }
            let fi = field_of(ix, &args[3], spec::T_TAG)?;
            let Some(tag) = &ix.fields[fi].tag else { return Ok(Reply::Array(Vec::new())) };
            let Some(s) = &tag.suffix else { return Err(err("tag field does have suffix trie")) };
            Ok(Reply::Array(s.keys().into_iter().map(Reply::bulk).collect()))
        }
        "DUMP_GEOMIDX" => {
            if argc != 4 {
                return Err(arity());
            }
            let ix = sctx(cx, &args[2])?;
            field_of(ix, &args[3], spec::T_GEOMETRY)?;
            Err(err("ERR _FT.DEBUG DUMP_GEOMIDX is not supported yet"))
        }
        "DUMP_PREFIX_TRIE" => {
            let p = &cx.eng.search.prefixes;
            let mut out = Vec::new();
            kv(&mut out, "prefixes_count", p.len());
            kv(&mut out, "prefixes_trie_nodes", p.n_nodes());
            Ok(Reply::Array(out))
        }
        "IDTODOCID" => {
            if argc != 4 {
                return Err(arity());
            }
            let ix = sctx(cx, &args[2])?;
            let Some(id) = crate::util::parse_i64(&args[3]) else { return Err(err("bad id given")) };
            match u64::try_from(id).ok().and_then(|id| ix.docs.get(id)) {
                Some(d) => Ok(Reply::bulk(crate::util::cstr(&d.key))),
                None => Err(err("document was removed")),
            }
        }
        "DOCIDTOID" => {
            if argc != 4 {
                return Err(arity());
            }
            let ix = sctx(cx, &args[2])?;
            Ok(int(ix.docs.get_id(&args[3])))
        }
        "DOCINFO" => {
            if argc < 5 {
                return Err(arity());
            }
            let ix = sctx(cx, &args[2])?;
            docinfo(ix, &args[3], &args[4])
        }
        "DUMP_PHONETIC_HASH" => {
            if argc != 3 {
                return Err(arity());
            }
            let (p, s) = crate::search::phonetic::double_metaphone(&args[2]);
            let pre = |v: Vec<u8>| {
                let mut out = vec![crate::search::tokenize::PHONETIC_PREFIX];
                out.extend_from_slice(&v);
                Reply::bulk(out)
            };
            Ok(Reply::Array(vec![pre(p), pre(s)]))
        }
        "GC_FORCEINVOKE" => {
            if !(3..=4).contains(&argc) {
                return Err(arity());
            }
            let set = gc::Settings::from_cfg(cx.cfg());
            let name = load(cx, &args[2])?;
            let ix = cx.eng.search.indexes.get_mut(&name).expect("loaded");
            if !gc::run_cycle(ix, true, set) {
                return Err(err("GC is not available for this index"));
            }
            Ok(st("DONE"))
        }
        "GC_FORCEBGINVOKE" => {
            if argc < 3 {
                return Err(arity());
            }
            let set = gc::Settings::from_cfg(cx.cfg());
            let name = load(cx, &args[2])?;
            gc::run_cycle(cx.eng.search.indexes.get_mut(&name).expect("loaded"), true, set);
            Ok(Reply::Ok)
        }
        "GC_STOP_SCHEDULE" => {
            if argc < 3 {
                return Err(arity());
            }
            let name = load(cx, &args[2])?;
            if let Some(g) = cx.eng.search.indexes.get_mut(&name).and_then(|ix| ix.gc.as_mut()) {
                g.next_run = 0;
            }
            Ok(Reply::Ok)
        }
        "GC_CONTINUE_SCHEDULE" => {
            if argc < 3 {
                return Err(arity());
            }
            let set = gc::Settings::from_cfg(cx.cfg());
            let interval = cx.cfg().num("search-fork-gc-run-interval") as i64;
            let name = load(cx, &args[2])?;
            let ix = cx.eng.search.indexes.get_mut(&name).expect("loaded");
            if ix.gc.as_ref().is_some_and(|g| g.next_run != 0) {
                return Err(err("GC is already running periodically"));
            }
            // `GCContext_StartNow`: a run now, then the schedule again.
            if gc::run_cycle(ix, false, set)
                && let Some(g) = ix.gc.as_mut()
            {
                g.next_run = crate::util::now_ms() + gc::next_period_ms(interval);
            }
            Ok(Reply::Ok)
        }
        "GC_WAIT_FOR_JOBS" => Ok(st("DONE")),
        "GC_CLEAN_NUMERIC" => {
            if argc != 4 {
                return Err(arity());
            }
            let ix = sctx_mut(cx, &args[2])?;
            let Some(fi) = ix.spec.field(&args[3]).filter(|f| f.is(spec::T_NUMERIC | spec::T_GEO)).map(|f| f.index) else {
                // the error, then the OK the command always ends with
                return Ok(Reply::Seq(vec![Reply::error("Could not find given field in index spec"), Reply::Ok]));
            };
            if let Some(tree) = ix.fields[fi].numeric.as_mut() {
                let r = tree.trim_empty_leaves();
                ix.stats.total_blocks += r.block_count_delta as i64;
            }
            Ok(Reply::Ok)
        }
        "DISK_FLUSH" => {
            if argc != 3 {
                return Err(arity());
            }
            load(cx, &args[2])?;
            Err(err("Index is not a disk index"))
        }
        "GIT_SHA" => Ok(Reply::bulk(b"")),
        "TTL" | "TTL_PAUSE" | "TTL_EXPIRE" => {
            if argc < 3 {
                return Err(arity());
            }
            let Some(name) = cx.eng.search.load_no_timer(crate::util::cstr(&args[2])) else { return Err(not_found(&args[2])) };
            let ix = cx.eng.search.indexes.get_mut(&name).expect("loaded");
            if ix.spec.flags & spec::I_TEMPORARY == 0 {
                return Err(err("Index is not temporary"));
            }
            match sub.as_str() {
                "TTL" => Ok(int(ix.expires_at.map_or(0, |t| (t - crate::util::now_ms()).max(0) / 1000))),
                "TTL_PAUSE" => {
                    if ix.expires_at.take().is_none() {
                        return Err(err("Index does not have a timer"));
                    }
                    Ok(Reply::Ok)
                }
                _ => {
                    // the timeout set to 1ms for one acquire (which counts a use)
                    ix.counter += 1;
                    ix.expires_at = Some(crate::util::now_ms() + 1);
                    Ok(Reply::Ok)
                }
            }
        }
        "VECSIM_INFO" => {
            if argc != 4 {
                return Err(arity());
            }
            let workers_limit = cx.cfg().num("search-tiered-hnsw-buffer-limit") as i64;
            let shared = cx.eng.search.vecsim_shared_memory(cx.cfg().num("search-workers") as usize);
            let ix = sctx_mut(cx, &args[2])?;
            let Some(fi) = ix.spec.field(&args[3]).filter(|f| f.is(spec::T_VECTOR)).map(|f| f.index) else {
                return Err(err("Vector index not found"));
            };
            let Some(p) = ix.spec.fields[fi].vector.clone() else { return Err(err("Can't open vector index")) };
            let vi = ix.fields[fi].vectors.get_or_insert_with(|| VecIndex::new(&p));
            vecsim_info(vi, &p, workers_limit, shared)
        }
        "DUMP_HNSW" => {
            if !(4..=5).contains(&argc) {
                return Err(arity());
            }
            let ix = sctx_mut(cx, &args[2])?;
            let Some(fi) = ix.spec.field(&args[3]).filter(|f| f.is(spec::T_VECTOR)).map(|f| f.index) else {
                return Err(err("Vector index not found"));
            };
            let Some(p) = ix.spec.fields[fi].vector.clone() else { return Err(err("Can't open vector index")) };
            let multi = p.multi;
            if p.algo != spec::Algo::Hnsw {
                ix.fields[fi].vectors.get_or_insert_with(|| VecIndex::new(&p));
                return Err(err("Vector index is not an HNSW index"));
            }
            ix.fields[fi].vectors.get_or_insert_with(|| VecIndex::new(&p));
            if multi {
                return Err(err("Command not supported for HNSW multi-value index"));
            }
            if argc == 5 && ix.docs.get_id(&args[4]) == 0 {
                return Err(err("The given key does not exist in index"));
            }
            // the graph's neighbour lists aren't modelled
            Err(err("ERR _FT.DEBUG DUMP_HNSW is not supported yet"))
        }
        "DELETE_LOCAL_CURSORS" => {
            if argc != 2 {
                return Err(arity());
            }
            cx.eng.search.cursors.clear();
            Ok(Reply::Ok)
        }
        "DELETE_LOCAL_COORD_CURSORS" => {
            if argc != 2 {
                return Err(arity());
            }
            Ok(Reply::Ok)
        }
        "SET_MONITOR_EXPIRATION" => set_monitor_expiration(cx, args),
        "WORKERS" => workers(cx, args),
        "COORD_THREADS" => coord_threads(cx, args),
        "BG_SCAN_CONTROLLER" => bg_scan_controller(cx, args),
        "INDEXES" => Ok(Reply::Set(cx.eng.search.indexes.keys().map(|n| safe(obfuscated_name(n).as_bytes())).collect())),
        "INFO" => {
            if argc < 3 {
                return Err(arity());
            }
            let all = args[2].eq_ignore_ascii_case(b"ALL");
            let wanted = String::from_utf8_lossy(crate::util::cstr(&args[2])).into_owned();
            let names: Vec<Bytes> = cx.eng.search.indexes.keys().filter(|n| all || obfuscated_name(n).eq_ignore_ascii_case(&wanted)).cloned().collect();
            let infos: Vec<Reply> = names.iter().map(|n| info_reply(cx, n, true, true)).collect();
            if infos.is_empty() {
                // the (empty) array, then the error
                return Ok(Reply::Seq(vec![Reply::Array(Vec::new()), Reply::error("Unknown obfuscated index name")]));
            }
            Ok(Reply::Array(infos))
        }
        "GET_HIDE_USER_DATA_FROM_LOGS" => Ok(int(cx.cfg().flag("hide-user-data-from-log") as i64)),
        "YIELDS_COUNTER" => {
            if argc != 3 {
                return Err(arity());
            }
            let a = &args[2];
            if a.eq_ignore_ascii_case(b"RESET") {
                Ok(Reply::Ok)
            } else if a.eq_ignore_ascii_case(b"BG_INDEX") || a.eq_ignore_ascii_case(b"LOAD") {
                Ok(Reply::Int(0))
            } else {
                Err(err("Unknown subcommand"))
            }
        }
        "INDEXER_SLEEP_BEFORE_YIELD_MICROS" => {
            if argc > 3 {
                return Err(arity());
            }
            if argc == 3 {
                let v = crate::util::parse_i64(&args[2]).filter(|&v| v >= 0);
                let Some(v) = v else { return Err(err("Invalid sleep time. Must be a non-negative integer.")) };
                cx.eng.search.debug.sleep_before_yield_us = v as u32;
                return Ok(Reply::Ok);
            }
            Err(arity())
        }
        "QUERY_CONTROLLER" => {
            if argc < 3 {
                return Err(arity());
            }
            match args[2].as_ref() {
                b"SET_PAUSE_RP_RESUME" => {
                    if argc != 3 {
                        return Err(arity());
                    }
                    Err(err("Query is not paused"))
                }
                b"GET_IS_RP_PAUSED" => {
                    if argc != 3 {
                        return Err(arity());
                    }
                    Ok(Reply::Int(0))
                }
                b"PRINT_RP_STREAM" => {
                    if argc != 3 {
                        return Err(arity());
                    }
                    Err(err("No debug RP is set"))
                }
                _ => Err(err("Invalid command for 'QUERY_CONTROLLER'")),
            }
        }
        "DUMP_SCHEMA" => {
            if argc != 3 {
                return Err(arity());
            }
            let ix = sctx(cx, &args[2])?;
            let mut items: Vec<crate::search::trie::RdbItem> = Vec::new();
            crate::search::rdb::save_spec(ix, &mut items);
            Ok(Reply::Array(vec![Reply::bulk(crate::rdb::module_items(&items)), Reply::Int(INDEX_CURRENT_VERSION)]))
        }
        "VECSIM_MOCK_TIMEOUT" => {
            if argc != 3 {
                return Err(arity());
            }
            match args[2].as_ref() {
                b"enable" => cx.eng.search.debug.vecsim_timeout = true,
                b"disable" => cx.eng.search.debug.vecsim_timeout = false,
                _ => return Err(err("Invalid command for 'VECSIM_MOCK_TIMEOUT'")),
            }
            Ok(Reply::Ok)
        }
        "GET_MAX_DOC_ID" => {
            if argc != 3 {
                return Err(arity());
            }
            let ix = sctx(cx, &args[2])?;
            Ok(int(ix.docs.max_doc_id))
        }
        "DUMP_DELETED_IDS" => {
            if argc != 3 {
                return Err(arity());
            }
            sctx(cx, &args[2])?;
            Ok(Reply::Array(Vec::new()))
        }
        "DISK_IO_CONTROL" => {
            if argc != 3 {
                return Err(arity());
            }
            let a = &args[2];
            if a.eq_ignore_ascii_case(b"enable") {
                Err(err("Async I/O is not supported (disk API not available or disk doesn't support async I/O)"))
            } else if a.eq_ignore_ascii_case(b"disable") {
                Ok(st("OK - Async I/O disabled"))
            } else if a.eq_ignore_ascii_case(b"status") {
                Ok(st("Async I/O: not supported by disk"))
            } else {
                Err(err("Invalid command for 'DISK_IO_CONTROL'. Use: enable, disable, or status"))
            }
        }
        "REGISTER_TEST_SCORERS" => {
            if argc != 2 {
                return Err(arity());
            }
            if std::mem::replace(&mut cx.eng.search.debug.test_scorers, true) {
                return Err(err("Scorer already registered or registration failed"));
            }
            Ok(Reply::Ok)
        }
        "SET_MAX_INDEXES" => {
            if argc != 3 {
                return Err(arity());
            }
            let v = crate::util::parse_i64(&args[2]).filter(|v| (1..=DEFAULT_MAX_INDEXES).contains(v));
            let Some(v) = v else { return Err(Error::msg(format!("Invalid value. Must be between 1 and {DEFAULT_MAX_INDEXES}."))) };
            let n = cx.eng.search.indexes.len();
            if (v as usize) < n {
                return Err(Error::msg(format!("Invalid value. Must be at least current number of indexes: {n}.")));
            }
            cx.eng.search.max_indexes = Some(v as usize);
            Ok(Reply::Ok)
        }
        "FT.SEARCH" | "FT.AGGREGATE" | "FT.PROFILE" => {
            // The query, at least one debug parameter and DEBUG_PARAMS_COUNT <count>.
            if argc < 7 {
                return Err(arity());
            }
            super::ft_aggregate::debug_query(cx, &args[1..])
        }
        "FT.HYBRID" | "_FT.HYBRID" => {
            // FT.HYBRID index SEARCH query VSIM field vector DEBUG_PARAMS_COUNT count.
            if argc < 9 {
                return Err(arity());
            }
            if sub == "FT.HYBRID" { super::ft_aggregate::debug_hybrid(cx, &args[1..]) } else { super::ft_aggregate::debug_internal_hybrid(cx, &args[1..]) }
        }
        "_FT.SEARCH" | "_FT.AGGREGATE" | "_FT.PROFILE" => {
            // `DEBUG_execCommandCommon` (the index), `RSProfileCommandImp` (SEARCH QUERY …).
            if argc < if sub == "_FT.PROFILE" { 6 } else { 3 } {
                return Err(arity());
            }
            super::ft_aggregate::debug_query(cx, &args[1..])
        }
        "SHARD_CONNECTION_STATES" => {
            if argc != 2 {
                return Err(arity());
            }
            Err(err("ERR _FT.DEBUG SHARD_CONNECTION_STATES is not supported yet"))
        }
        "PAUSE_TOPOLOGY_UPDATER" => {
            if argc != 2 {
                return Err(arity());
            }
            // no topology updater runs on a standalone server
            Err(err("Topology updater is already paused"))
        }
        "RESUME_TOPOLOGY_UPDATER" => {
            if argc != 2 {
                return Err(arity());
            }
            Err(err("ERR _FT.DEBUG RESUME_TOPOLOGY_UPDATER is not supported yet"))
        }
        "CLEAR_PENDING_TOPOLOGY" => {
            if argc != 2 {
                return Err(arity());
            }
            Ok(Reply::Ok)
        }
        _ => Err(Error::msg(format!("ERR unknown subcommand '{}'. Try _FT.DEBUG HELP.", String::from_utf8_lossy(&args[1])))),
    }
}

/// `Indexes_LoadIndexSpecUnsafe` (a use counted, the timer restarted), else the unknown index
/// error.
fn load(cx: &mut Cx, name: &[u8]) -> Result<Bytes, Error> {
    cx.eng.search.load(crate::util::cstr(name), false).ok_or_else(|| not_found(name))
}

/// `GET_SEARCH_CTX`: the index, or "Can not create a search ctx".
fn sctx<'c>(cx: &'c mut Cx, name: &[u8]) -> Result<&'c Index, Error> {
    let Some(real) = cx.eng.search.load(crate::util::cstr(name), false) else { return Err(err("Can not create a search ctx")) };
    Ok(&cx.eng.search.indexes[&real])
}

fn sctx_mut<'c>(cx: &'c mut Cx, name: &[u8]) -> Result<&'c mut Index, Error> {
    let Some(real) = cx.eng.search.load(crate::util::cstr(name), false) else { return Err(err("Can not create a search ctx")) };
    Ok(cx.eng.search.indexes.get_mut(&real).expect("loaded"))
}

/// `getFieldByNameAndType`.
fn field_of(ix: &Index, name: &[u8], types: u8) -> Result<usize, Error> {
    ix.spec.field(name).filter(|f| f.is(types)).map(|f| f.index).ok_or_else(|| err("Could not find given field in index spec"))
}

/// `ReplyReaderResultsIDs`: every record's doc id.
fn ids(ii: &crate::search::invidx::InvertedIndex) -> Reply {
    Reply::Array(ii.reader().map(|r| int(r.doc_id)).collect())
}

/// `InvertedIndexSummaryHeader`.
fn summary_header(ii: &crate::search::invidx::InvertedIndex) -> Vec<Reply> {
    let s = ii.summary();
    let mut out = Vec::new();
    kv(&mut out, "numDocs", s.number_of_docs);
    kv(&mut out, "numEntries", s.number_of_entries);
    kv(&mut out, "lastId", s.last_doc_id);
    kv(&mut out, "flags", s.flags);
    kv(&mut out, "numberOfBlocks", s.number_of_blocks);
    if s.has_efficiency {
        out.push(bulk("blocks_efficiency (numEntries/numberOfBlocks)"));
        out.push(Reply::Double(s.block_efficiency));
    }
    out
}

/// `debug_summary`.
fn numidx_summary(tree: Option<&NumericRangeTree>) -> Reply {
    let v = |f: fn(&NumericRangeTree) -> i64| int(tree.map_or(0, f));
    Reply::Array(vec![
        st("numRanges"),
        v(|t| t.num_ranges() as i64),
        st("numLeaves"),
        v(|t| t.num_leaves() as i64),
        st("numEntries"),
        v(|t| t.num_entries() as i64),
        st("lastDocId"),
        v(|t| t.last_doc_id() as i64),
        st("revisionId"),
        v(|t| t.revision_id() as i64),
        st("emptyLeaves"),
        v(|t| t.empty_leaves() as i64),
        st("RootMaxDepth"),
        v(|t| t.root().max_depth() as i64),
        st("MemoryUsage"),
        v(|t| t.mem_usage() as i64),
    ])
}

/// `debug_dump_index`: each range's doc ids, in the tree's reverse pre-order, with the inverted
/// index's header first when asked.
fn dump_numidx(tree: Option<&NumericRangeTree>, with_headers: bool) -> Reply {
    let Some(tree) = tree else { return Reply::Array(Vec::new()) };
    Reply::Array(
        tree.ranges()
            .map(|r| {
                let ids = ids(r.entries());
                if !with_headers {
                    return ids;
                }
                let s = r.entries().summary();
                let mut h = vec![
                    st("numDocs"),
                    int(s.number_of_docs),
                    st("numEntries"),
                    int(s.number_of_entries),
                    st("lastId"),
                    int(s.last_doc_id),
                    st("flags"),
                    int(s.flags),
                    st("numberOfBlocks"),
                    int(s.number_of_blocks),
                ];
                if s.has_efficiency {
                    h.push(st("blocks_efficiency (numEntries/numberOfBlocks)"));
                    h.push(Reply::Double(s.block_efficiency));
                }
                Reply::Array(vec![Reply::Array(h), ids])
            })
            .collect(),
    )
}

/// `debug_dump_tree`.
fn dump_numidx_tree(tree: Option<&NumericRangeTree>, minimal: bool) -> Reply {
    let v = |f: fn(&NumericRangeTree) -> i64| int(tree.map_or(0, f));
    let mut eff = 0.0;
    let root = match tree {
        Some(t) => node_debug(t, t.root_id(), minimal, &mut eff),
        None => Reply::Map(Vec::new()),
    };
    let ranges = tree.map_or(0, |t| t.num_ranges());
    let avg = if ranges > 0 { eff / ranges as f64 } else { 0.0 };
    Reply::Map(vec![
        (st("numRanges"), v(|t| t.num_ranges() as i64)),
        (st("numEntries"), v(|t| t.num_entries() as i64)),
        (st("lastDocId"), v(|t| t.last_doc_id() as i64)),
        (st("revisionId"), v(|t| t.revision_id() as i64)),
        (st("uniqueId"), v(|t| t.unique_id() as i64)),
        (st("emptyLeaves"), v(|t| t.empty_leaves() as i64)),
        (st("root"), root),
        (st("Tree stats"), Reply::Map(vec![(st("Average memory efficiency (numEntries/size)/numRanges"), Reply::Double(avg))])),
    ])
}

/// `reply_node_debug`: the node's range (unless `minimal`), and an internal node's split value,
/// depth and children.
fn node_debug(tree: &NumericRangeTree, id: NodeId, minimal: bool, eff: &mut f64) -> Reply {
    let node = tree.node(id);
    let mut m = Vec::new();
    if let Some(r) = node.range() {
        m.push((st("range"), if minimal { Reply::Array(Vec::new()) } else { range_debug(r, eff) }));
    }
    if let crate::search::numtree::Node::Internal(n) = node {
        m.push((st("value"), Reply::Double(n.value)));
        m.push((st("maxDepth"), int(n.max_depth)));
        m.push((st("left"), node_debug(tree, n.left, minimal, eff)));
        m.push((st("right"), node_debug(tree, n.right, minimal, eff)));
    }
    Reply::Map(m)
}

/// `reply_range_debug` with `reply_numeric_index_debug`.
fn range_debug(r: &NumericRange, eff: &mut f64) -> Reply {
    let s = r.entries().summary();
    *eff += s.block_efficiency;
    let mut values = Vec::new();
    for rec in r.entries().reader() {
        values.push(st("value"));
        values.push(Reply::Double(rec.numeric));
        values.push(st("docId"));
        values.push(int(rec.doc_id));
    }
    let entries = vec![
        st("numDocs"),
        int(s.number_of_docs),
        st("numEntries"),
        int(s.number_of_entries),
        st("lastId"),
        int(s.last_doc_id),
        st("size"),
        int(s.number_of_blocks),
        st("blocks_efficiency (numEntries/size)"),
        Reply::Double(s.block_efficiency),
        st("values"),
        Reply::Array(values),
    ];
    Reply::Array(vec![
        st("minVal"),
        Reply::Double(r.min_val()),
        st("maxVal"),
        Reply::Double(r.max_val()),
        st("invertedIndexSize [bytes]"),
        Reply::Double(r.memory_usage() as f64),
        st("card"),
        int(r.cardinality()),
        st("entries"),
        Reply::Array(entries),
    ])
}

/// INFO_TAGIDX index field [COUNT_VALUE_ENTRIES] [DUMP_ID_ENTRIES] [PREFIX p] [OFFSET n] [LIMIT n].
fn info_tagidx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 4 {
        return Err(arity_error("_FT.DEBUG|INFO_TAGIDX"));
    }
    let ix = sctx(cx, &args[2])?;
    let (mut count, mut dump, mut offset, mut limit) = (false, false, 0u32, 0u32);
    let mut ac = Ac::new(&args[4..]);
    while let Some(a) = ac.peek() {
        let name = String::from_utf8_lossy(a).to_ascii_lowercase();
        let ok = match name.as_str() {
            "count_value_entries" => {
                count = true;
                ac.advance().is_ok()
            }
            "dump_id_entries" => {
                dump = true;
                ac.advance().is_ok()
            }
            "prefix" => ac.advance().is_ok() && ac.get().is_ok(),
            "offset" => ac.advance().is_ok() && ac.get_u32(0).map(|v| offset = v).is_ok(),
            "limit" => ac.advance().is_ok() && ac.get_u32(0).map(|v| limit = v).is_ok(),
            _ => false,
        };
        if !ok {
            return Err(err("Could not parse argument (argspec fixme)"));
        }
    }
    let fi = field_of(ix, &args[3], spec::T_TAG)?;
    let Some(tag) = &ix.fields[fi].tag else { return Ok(Reply::Array(Vec::new())) };
    let mut out = vec![bulk("num_values"), int(tag.values.len())];
    if !(count || dump) {
        return Ok(Reply::Array(out));
    }
    let mut values = Vec::new();
    let mut n = 0usize;
    tag.values.for_each(|k, ii| {
        if n < offset as usize {
            n += 1;
            return true;
        }
        if values.len() >= limit as usize {
            return false;
        }
        let mut v = vec![bulk("value"), Reply::bulk(k), bulk("num_entries"), int(ii.unique_docs()), bulk("num_blocks"), int(ii.number_of_blocks())];
        if dump {
            v.push(bulk("entries"));
            v.push(ids(ii));
        }
        values.push(Reply::Array(v));
        true
    });
    out.push(bulk("values"));
    out.push(Reply::Array(values));
    Ok(Reply::Array(out))
}

/// DOCINFO index key REVEAL|OBFUSCATE.
fn docinfo(ix: &Index, key: &[u8], mode: &[u8]) -> CmdResult {
    let Some(d) = ix.docs.by_key(key) else { return Err(err("Document not found in index")) };
    let obfuscate = mode.eq_ignore_ascii_case(b"OBFUSCATE");
    if !obfuscate && !mode.eq_ignore_ascii_case(b"REVEAL") {
        return Err(err("Invalid argument. Expected REVEAL or OBFUSCATE as the last argument"));
    }
    use crate::search::doctable::SortVal;
    let mut flags = format!("(0x{:x}):", d.flags);
    for (bit, name) in [(0x01, "Deleted,"), (0x02, "HasPayload,"), (0x04, "HasSortVector,"), (0x08, "HasOffsetVector,")] {
        if d.flags & bit != 0 {
            flags.push_str(name);
        }
    }
    let mut m = vec![
        (st("internal_id"), int(d.id)),
        (bulk("flags"), bulk(&flags)),
        (st("score"), Reply::Double(d.score as f64)),
        (st("num_tokens"), int(d.doc_len)),
        (st("max_freq"), int(d.max_freq)),
        (st("refcount"), Reply::Int(1)),
    ];
    if !d.sort.is_empty() {
        fn value(v: &SortVal) -> Reply {
            match v {
                SortVal::Null => Reply::Nil,
                SortVal::Num(n) => Reply::bulk(crate::search::expr::num_to_str(*n)),
                SortVal::Str(s) => Reply::Bulk(s.clone()),
                SortVal::Multi(items) => Reply::Array(items.iter().map(value).collect()),
            }
        }
        let mut items = Vec::new();
        for (i, v) in d.sort.iter().enumerate() {
            if matches!(v, SortVal::Null) {
                continue;
            }
            let fs = ix.spec.fields.iter().find(|f| f.sort_idx == i as i32);
            let field = match fs {
                None => "!!! AS ???".to_string(),
                Some(f) if obfuscate => format!("FieldPath@{} AS Field@{}", f.index, f.index),
                Some(f) => format!("{} AS {}", spec::text(f.path()), spec::text(&f.name)),
            };
            items.push(Reply::Array(vec![st("index"), int(i), bulk("field"), bulk(&field), bulk("value"), value(v)]));
        }
        m.push((st("sortables"), Reply::Array(items)));
    }
    Ok(Reply::Map(m))
}

/// SET_MONITOR_EXPIRATION index [DOCUMENTS|NOT-DOCUMENTS] [FIELDS|NOT-FIELDS].
fn set_monitor_expiration(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 3 {
        return Err(arity_error("_FT.DEBUG|SET_MONITOR_EXPIRATION"));
    }
    let Some(name) = cx.eng.search.load_no_timer(crate::util::cstr(&args[2])) else { return Err(not_found(&args[2])) };
    let (mut docs, mut not_docs, mut fields, mut not_fields) = (false, false, false, false);
    for a in &args[3..] {
        match String::from_utf8_lossy(a).to_ascii_lowercase().as_str() {
            "not-documents" => not_docs = true,
            "documents" => docs = true,
            "fields" => fields = true,
            "not-fields" => not_fields = true,
            _ => return Err(err("Could not parse argument (argspec fixme)")),
        }
    }
    if docs && not_docs {
        return Err(err("Can't set both documents and not-documents"));
    }
    if fields && not_fields {
        return Err(err("Can't set both fields and not-fields"));
    }
    let ix = cx.eng.search.indexes.get_mut(&name).expect("loaded");
    if docs || not_docs {
        ix.monitor_expiration = docs;
    }
    if fields || not_fields {
        ix.monitor_field_expiration = fields;
    }
    Ok(Reply::Ok)
}

/// WORKERS PAUSE|RESUME|DRAIN|STATS|N_THREADS.
fn workers(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() != 3 {
        return Err(arity_error("_FT.DEBUG|WORKERS"));
    }
    let n = cx.cfg().num("search-workers") as i64;
    let d = &mut cx.eng.search.debug;
    let op = String::from_utf8_lossy(&args[2]).to_ascii_lowercase();
    match op.as_str() {
        "pause" => {
            if n == 0 || d.workers_paused {
                return Err(err("Operation failed: workers thread pool doesn't exists or is not running"));
            }
            d.workers_paused = true;
        }
        "resume" => {
            if n == 0 || !d.workers_paused {
                return Err(err("Operation failed: workers thread pool doesn't exists or is already running"));
            }
            d.workers_paused = false;
        }
        "drain" => {
            if d.workers_paused {
                return Err(err("Operation failed: workers thread pool is not running"));
            }
        }
        "stats" => {
            let mut out = Vec::new();
            kv(&mut out, "totalJobsDone", 0);
            kv(&mut out, "totalPendingJobs", 0);
            kv(&mut out, "highPriorityPendingJobs", 0);
            kv(&mut out, "lowPriorityPendingJobs", 0);
            kv(&mut out, "numThreadsAlive", n);
            kv(&mut out, "numJobsInProgress", 0);
            return Ok(Reply::Array(out));
        }
        "n_threads" => return Ok(Reply::Int(n)),
        _ => return Err(err("Invalid argument for 'WORKERS' subcommand")),
    }
    Ok(Reply::Ok)
}

/// COORD_THREADS PAUSE|RESUME|IS_PAUSED|STATS.
fn coord_threads(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() != 3 {
        return Err(arity_error("_FT.DEBUG|COORD_THREADS"));
    }
    let d = &mut cx.eng.search.debug;
    match String::from_utf8_lossy(&args[2]).to_ascii_lowercase().as_str() {
        "pause" => {
            if d.coord_paused {
                return Err(err("Operation failed: coordinator thread pool doesn't exists or is not running"));
            }
            d.coord_paused = true;
        }
        "resume" => {
            if !d.coord_paused {
                return Err(err("Operation failed: coordinator thread pool doesn't exists or is already running"));
            }
            d.coord_paused = false;
        }
        "is_paused" => return Ok(Reply::Int(d.coord_paused as i64)),
        "stats" => {
            let mut out = Vec::new();
            kv(&mut out, "totalJobsDone", 0);
            kv(&mut out, "totalPendingJobs", 0);
            return Ok(Reply::Array(out));
        }
        _ => return Err(err("Invalid argument for 'COORD_THREADS' subcommand")),
    }
    Ok(Reply::Ok)
}

/// BG_SCAN_CONTROLLER: the background scan's debug knobs (each subcommand sees `argv + 1`).
fn bg_scan_controller(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let arity = || arity_error("_FT.DEBUG|BG_SCAN_CONTROLLER");
    if args.len() < 3 {
        return Err(arity());
    }
    // the subcommand's own argc
    let argc = args.len() - 1;
    let op = String::from_utf8_lossy(&args[2]).into_owned();
    let invalid = || Error::msg(format!("Invalid argument for '{op}'"));
    match op.as_str() {
        "SET_MAX_SCANNED_DOCS" | "SET_PAUSE_ON_SCANNED_DOCS" => {
            if argc != 3 {
                return Err(arity());
            }
            crate::util::parse_i64(&args[3]).ok_or_else(invalid)?;
            Ok(Reply::Ok)
        }
        "SET_BG_INDEX_RESUME" | "TERMINATE_BG_POOL" => {
            if argc != 2 {
                return Err(arity());
            }
            Ok(Reply::Ok)
        }
        "GET_DEBUG_SCANNER_STATUS" | "DEBUG_SCANNER_UPDATE_CONFIG" => {
            if argc != 3 {
                return Err(arity());
            }
            if cx.eng.search.load_no_timer(crate::util::cstr(&args[3])).is_none() {
                return Err(not_found(&args[3]));
            }
            Err(err("Scanner is not initialized"))
        }
        "SET_PAUSE_BEFORE_SCAN" | "SET_PAUSE_ON_OOM" | "SET_PAUSE_BEFORE_OOM_RETRY" => {
            if argc != 3 {
                return Err(arity());
            }
            let v = &args[3];
            if !(v.eq_ignore_ascii_case(b"true") || v.eq_ignore_ascii_case(b"false")) {
                return Err(invalid());
            }
            Ok(Reply::Ok)
        }
        "SET_SIMULATE_ASYNC_OOM" => Err(err("SET_SIMULATE_ASYNC_OOM is only supported in disk mode")),
        _ => Err(err("Invalid command for 'BG_SCAN_CONTROLLER'")),
    }
}

/// `VecSim_Reply_Info_Iterator` over `VecSimIndex_DebugInfoIterator`: FLAT's fields, or the
/// tiered HNSW index's with its flat buffer's and graph's.
fn vecsim_info(vi: &VecIndex, p: &spec::VectorParams, buffer_limit: i64, shared: usize) -> CmdResult {
    let common = |out: &mut Vec<Reply>, algo: &str, size: usize, labels: usize, memory: usize, mode: &str| {
        out.extend([
            bulk("ALGORITHM"),
            bulk(algo),
            bulk("TYPE"),
            bulk(p.ty.name()),
            bulk("DIMENSION"),
            int(p.dim),
            bulk("METRIC"),
            bulk(p.metric.name()),
            bulk("IS_MULTI_VALUE"),
            int(p.multi as i64),
            bulk("IS_DISK"),
            Reply::Int(0),
            bulk("INDEX_SIZE"),
            int(size),
            bulk("INDEX_LABEL_COUNT"),
            int(labels),
            bulk("MEMORY"),
            int(memory),
            bulk("LAST_SEARCH_MODE"),
            bulk(mode),
        ]);
    };
    let mut out = Vec::new();
    match p.algo {
        spec::Algo::Flat => {
            common(&mut out, "FLAT", vi.size(), vi.label_count(), vi.memory(), vi.last_mode_name());
            out.extend([bulk("BLOCK_SIZE"), int(vi.block_size)]);
        }
        spec::Algo::Hnsw => {
            let (layer, buffer, graph) = vi.tiered_parts();
            common(&mut out, "TIERED", vi.size(), vi.label_count(), vi.memory(), vi.last_mode_name());
            out.extend([bulk("MANAGEMENT_LAYER_MEMORY"), int(layer), bulk("BACKGROUND_INDEXING"), Reply::Int(0), bulk("TIERED_BUFFER_LIMIT"), int(buffer_limit)]);
            let mut front = Vec::new();
            common(&mut front, "FLAT", 0, 0, buffer, "EMPTY_MODE");
            front.extend([bulk("BLOCK_SIZE"), int(vi.block_size)]);
            let mut back = Vec::new();
            common(&mut back, "HNSW", vi.size(), vi.label_count(), graph, vi.last_mode_name());
            let (level, entry) = match vi.max_level {
                Some((l, e)) => (l as i64, e as i64),
                None => (-1, -1),
            };
            back.extend([
                bulk("BLOCK_SIZE"),
                int(vi.block_size),
                bulk("M"),
                int(vi.m),
                bulk("EF_CONSTRUCTION"),
                int(if p.ef_construction == 0 { 200 } else { p.ef_construction }),
                bulk("EF_RUNTIME"),
                int(if p.ef_runtime == 0 { 10 } else { p.ef_runtime }),
                bulk("MAX_LEVEL"),
                Reply::Int(level),
                bulk("ENTRYPOINT"),
                Reply::Int(entry),
                bulk("EPSILON"),
                Reply::Double(if p.epsilon == 0.0 { 0.01 } else { p.epsilon }),
                bulk("NUMBER_OF_MARKED_DELETED"),
                int(vi.marked),
            ]);
            out.extend([bulk("FRONTEND_INDEX"), Reply::Array(front), bulk("BACKEND_INDEX"), Reply::Array(back), bulk("TIERED_HNSW_SWAP_JOBS_THRESHOLD"), Reply::Int(1024)]);
        }
        _ => return Err(err("ERR _FT.DEBUG VECSIM_INFO of an SVS-VAMANA index is not supported yet")),
    }
    out.extend([bulk("SHARED_MEMORY"), int(shared)]);
    Ok(Reply::Array(out))
}

impl crate::Engine {
    /// The module's timers, from the cron: TEMPORARY indexes past their timeout drop themselves
    /// (`FT.DROPINDEX name DD` through `RM_Call`, on a master), and each index's fork GC runs
    /// when its period comes (a cycle only collects past the clean threshold).
    pub fn search_cron(&mut self, now: i64) {
        if self.search.indexes.is_empty() {
            return;
        }
        let expired: Vec<Bytes> = self.search.indexes.iter().filter(|(_, ix)| ix.expires_at.is_some_and(|t| t <= now)).map(|(k, _)| k.clone()).collect();
        for name in expired {
            if let Some(ix) = self.search.indexes.get_mut(&name) {
                ix.expires_at = None;
            }
            if self.replica {
                continue;
            }
            let mut c = crate::client::Client::new(0);
            c.internal = true;
            let args = [Bytes::from_static(b"FT.DROPINDEX"), name, Bytes::from_static(b"DD")];
            let _ = self.dispatch(&mut c, &args, now, crate::engine::Mode::Top);
        }
        let set = gc::Settings::from_cfg(&self.cfg);
        let interval = self.cfg.num("search-fork-gc-run-interval") as i64;
        for ix in self.search.indexes.values_mut() {
            let Some(g) = ix.gc.as_ref() else { continue };
            if g.next_run == 0 || g.next_run > now {
                continue;
            }
            gc::run_cycle(ix, false, set);
            if let Some(g) = ix.gc.as_mut()
                && g.next_run != 0
            {
                g.next_run = now + gc::next_period_ms(interval);
            }
        }
    }
}
