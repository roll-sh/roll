//! FT.AGGREGATE (`AREQ_Execute` over the aggregation pipeline): the query iterator, then the
//! result processors the plan's steps build (`Pipeline_BuildQueryPart` and
//! `Pipeline_BuildAggregationPart`): scorer, loaders (implicit and LOAD), groupers with their
//! reducers, projectors and filters (APPLY/FILTER), sorters, pagers, counters and depleters.
//! Processors pull results from their upstream one at a time, sharing the pipeline's
//! counters (total, skipped, the chunk's result limit, the minimum score) and its error, so
//! the reply sees the same totals as the module's `sendChunk`: RESP2 reports the total after
//! the first result, RESP3 after the last.
//!
//! The module runs aggregations on its worker threads when the client can block, where the
//! loaders buffer their upstream before loading (`RPSafeLoader`); inside MULTI or a script
//! they load row by row.
//!
//! WITHCURSOR replies chunk by chunk (`runCursor`): the module keeps the pipeline between
//! FT.CURSOR READs; here each chunk rebuilds it from the request and restores what the
//! processors carried (the query iterator's place, the pagers', the lookups with the keys LOAD *
//! met). Once an accumulating processor (sorter, grouper, depleter, normalizer) drained the
//! query, what it still holds is replayed, with the metadata of the documents it references, as
//! the module's buffered results hold theirs. Rebuilt iterators read the index as it is then,
//! like the module's live readers (new entries show, deleted documents are skipped), with
//! wildcard ranges ending at the first chunk's max id; they open the readers the first chunk
//! opened, with its IDFs, and expand prefixes, fuzzy terms and tag patterns to its terms
//! (`Memo`), as the module's kept iterators do. FT.SEARCH WITHCURSOR reads the search pipeline
//! (scorer, sorter, pager, output loader, highlighter) the same way. A profiled cursor's
//! rebuilt iterators read their way back to where the earlier chunks stopped, read for read,
//! and count from there, so every iterator's counters add up as the kept ones' do.
//!
//! Timeouts (`search-timeout`, TIMEOUT): the query reader checks the clock every 100 reads, and
//! `_FT.DEBUG`'s `TIMEOUT_AFTER_N` times it out after that many results; NOT iterators check
//! every 5000 documents they skip, against the deadline they were built with (a cursor's later
//! reads keep it); under ON_TIMEOUT FAIL a query whose iterators took its time fails before its
//! pipeline is built, and on the workers the blocked client's timer fails a chunk that ran past
//! its timeout. Downstream, ON_TIMEOUT RETURN yields what the processors hold (sorters their
//! heap, loaders their buffer; groupers keep accumulating on a cursor's next read) with a
//! warning, FAIL replies with the timeout error (FT.PROFILE: no results, the warning), and a
//! timed-out cursor stays open under RETURN. The term expansions' own clock checks (the tries'
//! range, contains and wildcard walks) are not modelled: they only cut an expansion short once
//! real time ran out.
//!
//! FT.PROFILE runs the same pipeline with every iterator and processor counted
//! (`Profile_AddIters`, `Profile_AddRPs`). An FT.SEARCH's pipeline is built the way the module
//! builds it (scorer, arrange, output loader and highlighter) for the counters; its reply is
//! still FT.SEARCH's own.
//!
//! The memory guard (`oom`) stops queries before they run once used memory reaches
//! `maxmemory`. The debug parameters that pause a query on the workers (`PAUSE_BEFORE_RP_N`,
//! `PAUSE_AFTER_RP_N`, resumed by `_FT.DEBUG QUERY_CONTROLLER`) would hold the request while
//! other clients run; queries run to completion here, so those parameters are refused.
//!
//! Groups come out in the order they were first seen (the module's order follows its
//! per-process hash seed), and TOLIST keeps its values in insertion order for the same
//! reason.

use super::ft::verify_acl;
use super::ft_query::{self as fq, K_FORCE_LOAD, K_HIDDEN, K_NUMERIC, K_SCHEMA_SRC, K_SV_SRC, LKey, Lookup, Row};
use super::*;
use crate::engine::Engine;
use crate::search::areq::{
    self, Arrange, Command, F_HAS_DEPLETER, F_HAS_GROUPBY, F_HAS_SORTBY, F_HAS_WITHCOUNT, F_IS_AGGREGATE, F_IS_CURSOR, F_IS_HYBRID, F_IS_HYBRID_SEARCH, F_IS_SEARCH, F_NOROWS,
    F_REQUIRED_FIELDS, F_SEND_HIGHLIGHT, F_SEND_NOFIELDS, F_SEND_PAYLOADS, F_SEND_SCOREEXPLAIN, F_SEND_SCORES, F_SEND_SCORES_AS_FIELD, F_SEND_SORTKEYS, F_SENDRAWIDS,
    F_TYPED, FORMAT_EXPAND, OPTIMIZE, Step,
};
use crate::search::args::{Ac, c_eq};
use crate::search::cursor::Exec;
use crate::search::doctable::{DOC_DELETED, DOC_HAS_PAYLOAD, Dmd};
use crate::search::error::{Code, QErr};
use crate::search::eval::{self, EvalState, Mode, RsValue};
use crate::search::expr::{self, Expr};
use crate::search::hll::Hll;
use crate::search::index::Index;
use crate::search::iters::{self, BoxIt, Empty, IxResult, K_INTERSECT, K_TERM, K_UNION, PCtx, Pairs, Qcx, Skip};
use crate::search::mmheap::MmHeap;
use crate::search::optimizer::{self as qopt, OptType};
use crate::search::qast;
use crate::search::qeval::{EvalCx, Memo};
use crate::search::quantile::QuantStream;
use crate::search::scorers::{self, Expl, Scorer};
use crate::search::global::{Counted, QueryError, QueryWarning};
use crate::search::spec::Spec;
use crate::util::cstr;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Instant;

mod debug;
mod hybrid;
mod oom;
pub(super) use oom::{guard as oom_guard, query as oom_query};
pub(super) use hybrid::{debug_hybrid, debug_internal_hybrid, ft_hybrid, profile_hybrid};

/// `_FT.DEBUG FT.SEARCH|FT.AGGREGATE|FT.PROFILE|_FT.SEARCH|_FT.AGGREGATE|_FT.PROFILE …
/// DEBUG_PARAMS_COUNT <count>` (`argv` from the command's name on).
pub(super) fn debug_query(cx: &mut Cx, argv: &[Bytes]) -> CmdResult {
    let clock = Instant::now();
    let name = String::from_utf8_lossy(&argv[0]).to_ascii_uppercase();
    match name.as_str() {
        "FT.PROFILE" => {
            // `ProfileCommandHandlerImp`.
            if argv[3..].iter().any(|a| a.eq_ignore_ascii_case(b"WITHCURSOR")) {
                return Err(Error::msg("FT.PROFILE does not support cursor"));
            }
            verify_acl(cx, &argv[1])?;
            profile_with(cx, argv, true, clock)
        }
        "_FT.PROFILE" => profile_with(cx, argv, true, clock),
        _ => {
            let (c, p) = debug::split(argv)?;
            if c.len() < 2 {
                return Err(arity_error(&format!("_FT.DEBUG|{name}")));
            }
            if name.ends_with("SEARCH") { fq::search_with(cx, c, Some(p)) } else { aggregate_with(cx, c, Some(p)) }
        }
    }
}

/// `parseAndCompileDebug` for a request run with `background` workers.
pub(super) fn debug_parse(params: &[Bytes], req: &areq::Request, background: bool) -> Result<Option<u32>, QErr> {
    debug::parse(params, Policy::of(req.on_timeout), background)
}

/// FT.AGGREGATE index query [options].
pub(super) fn ft_aggregate(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 3 {
        return Err(arity_error("FT.AGGREGATE"));
    }
    // `DistAggregateCommandImp`'s memory guard.
    if let Some(o) = oom::guard(cx) {
        return oom::query(cx, o, args, None);
    }
    verify_acl(cx, &args[1])?;
    aggregate_with(cx, args, None)
}

/// FT.AGGREGATE's request, built and run; `_FT.DEBUG FT.AGGREGATE` gives its debug parameters
/// (`args` without them; `_FT.AGGREGATE` marks an internal request).
pub(super) fn aggregate_with(cx: &mut Cx, args: &[Bytes], dbg: Option<&[Bytes]>) -> CmdResult {
    // `execCommandCommon` counts an error building the request (a standalone server's as the
    // coordinator's); `DEBUG_execCommandCommon` does not.
    let pre = dbg.is_none().then_some(true);
    let mut req = areq::compile(Command::Aggregate, &cx.eng.cfg, &args[2..]).map_err(|e| count_error(cx.eng, e.into(), pre))?;
    req.internal = args[0].first() == Some(&b'_');
    let Some(name) = cx.eng.search.load(cstr(&args[1]), false) else {
        return Err(QErr::user(Code::NoIndex, "Index not found", format!(": {}", qast::lossy(&args[1]))).into());
    };
    let resp3 = cx.client.resp >= 3;
    {
        let eng = &mut *cx.eng;
        let ix = eng.search.indexes.get_mut(&name).expect("loaded");
        if let Err(e) = req.apply_context(ix, resp3) {
            return Err(count_error(eng, e.into(), pre));
        }
        // `prepareRequest`: the dialect counts once the request is built, whatever the run.
        ix.used_dialects |= 1 << (req.dialect.clamp(1, 8) - 1);
    }
    let is_cursor = req.flags & F_IS_CURSOR != 0;
    if is_cursor {
        req.cap_max_idle(&cx.eng.cfg);
    }
    // `IsNeededDepleter`: WITHCOUNT without a sort, a group or a count drains the query first.
    let f = req.flags;
    if f & F_IS_AGGREGATE != 0 && f & F_HAS_WITHCOUNT != 0 && f & (F_HAS_SORTBY | F_HAS_GROUPBY | F_NOROWS) == 0 {
        req.flags |= F_HAS_DEPLETER;
    }
    let background = cx.can_block() && cx.eng.cfg.num("search-workers") > 0;
    req.plan_errors = plan_side(req.internal, background, dbg.is_some());
    if let Some(p) = dbg {
        req.set_debug(debug_parse(p, &req, background));
    }
    let db = cx.client.db;
    if !is_cursor {
        let eng: &Engine = cx.eng;
        let ix: &Index = eng.search.indexes.get(&name).expect("loaded");
        let (reply, _) = run(eng, ix, &req, db, resp3, background, req.max_aggregate_results as u32, None, None, &mut false, None)?;
        count_dialect(cx, &name, &req);
        return Ok(reply);
    }

    start_cursor(cx, name, req, db, resp3, background, None)
}

/// FT.SEARCH WITHCURSOR: the search pipeline's first chunk, the cursor left for the rest.
pub(super) fn start_search_cursor(cx: &mut Cx, name: Bytes, req: areq::Request, db: usize, resp3: bool, background: bool) -> CmdResult {
    start_cursor(cx, name, req, db, resp3, background, None)
}

/// `AREQ_StartCursor`: the cursor is reserved once the pipeline is built, then its first
/// chunk runs (`runCursor`); `prof` profiles it (an internal FT.PROFILE).
#[allow(clippy::too_many_arguments)]
fn start_cursor(cx: &mut Cx, name: Bytes, req: areq::Request, db: usize, resp3: bool, background: bool, mut prof: Option<Box<ProfCur>>) -> CmdResult {
    let cap = cx.eng.cfg.num("search-index-cursor-limit") as i64;
    let spec_id = cx.eng.search.indexes.get(&name).expect("loaded").id;
    cx.eng.search.cursors.collect_expired();
    let gate = (cx.eng.search.cursors.index_total(spec_id) as i64 >= cap)
        .then(|| QErr::new(Code::Limit, format!("INDEX_CURSOR_LIMIT of {cap} has been reached for an index")));
    let chunk = if req.cursor.chunk_size != 0 { req.cursor.chunk_size } else { CURSOR_READ_SIZE };
    let mut st = CursorSt { chunk, db, ..CursorSt::default() };
    let mut started = false;
    let mut out = prof.as_ref().map(|p| p.chunk_out());
    let res = {
        let eng: &Engine = cx.eng;
        let ix: &Index = eng.search.indexes.get(&name).expect("loaded");
        run(eng, ix, &req, db, resp3, background, chunk, Some(&mut st), gate, &mut started, out.as_mut())
    };
    if !started {
        return res.map(|(r, _)| r);
    }
    let cursors = &mut cx.eng.search.cursors;
    let id = cursors.reserve(spec_id, name.clone(), req.cursor.max_idle, cap).expect("under the index's cursor limit");
    let mut c = cursors.take_running(id).expect("reserved");
    let (reply, done) = res?;
    count_dialect(cx, &name, &req);
    if let (Some(p), Some(o)) = (prof.as_mut(), out.as_ref()) {
        p.merge(o);
    }
    let reply = cursor_reply(reply, id, done, prof.as_deref(), resp3);
    if !done {
        st.prof = prof;
        c.exec = Some(Exec(Box::new(CursorExec { req, st })));
        cx.eng.search.cursors.pause(c);
    }
    Ok(reply)
}

/// A cursor's chunk reply: the results and the cursor's id (0 once done); a profiled
/// cursor's has the profile (RESP2: last, a null until done; RESP3: in a map with the
/// results, once done).
fn cursor_reply(reply: Reply, id: u64, done: bool, prof: Option<&ProfCur>, resp3: bool) -> Reply {
    let id = Reply::Int(if done { 0 } else { id as i64 });
    let Some(p) = prof else { return Reply::Array(vec![reply, id]) };
    let profile = done.then(|| p.reply());
    if resp3 {
        let mut m = vec![(Reply::Status("Results".into()), reply)];
        if let Some(pr) = profile {
            m.push((Reply::Status("Profile".into()), pr));
        }
        Reply::Array(vec![Reply::Map(m), id])
    } else {
        Reply::Array(vec![reply, id, profile.unwrap_or(Reply::Nil)])
    }
}

/// FT.PROFILE index SEARCH|AGGREGATE|HYBRID [LIMITED] QUERY query [options]
/// (`ProfileCommandHandler`, `RSProfileCommandImp`): the command's own reply, then the
/// profile of its single shard: the clocks, the warnings, the iterator tree's and the result
/// processors' counters.
pub(super) fn ft_profile(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let clock = Instant::now();
    if args.len() < 5 {
        return Err(arity_error("FT.PROFILE"));
    }
    if args[3..].iter().any(|a| a.eq_ignore_ascii_case(b"WITHCURSOR")) {
        return Err(Error::msg("FT.PROFILE does not support cursor"));
    }
    verify_acl(cx, &args[1])?;
    profile_with(cx, args, false, clock)
}

/// `RSProfileCommandImp`: `debug` (`_FT.DEBUG FT.PROFILE`) strips the debug parameters off a
/// SEARCH or AGGREGATE (HYBRID takes them as its own arguments); `_FT.PROFILE` is internal.
pub(super) fn profile_with(cx: &mut Cx, args: &[Bytes], debug: bool, clock: Instant) -> CmdResult {
    let cmd = if c_eq(&args[2], "SEARCH") {
        Command::Search
    } else if c_eq(&args[2], "AGGREGATE") {
        Command::Aggregate
    } else if c_eq(&args[2], "HYBRID") {
        let mut at = 3;
        let limited = c_eq(&args[at], "LIMITED");
        if limited {
            at += 1;
        }
        if !c_eq(&args[at], "QUERY") {
            return Err(Error::msg("The QUERY keyword is expected"));
        }
        return profile_hybrid(cx, &args[1], &args[at + 1..], limited, clock);
    } else {
        return Err(Error::msg("No `SEARCH`, `AGGREGATE`, or `HYBRID` provided"));
    };
    let mut at = 3;
    let limited = c_eq(&args[at], "LIMITED");
    if limited {
        at += 1;
    }
    if !c_eq(&args[at], "QUERY") {
        return Err(Error::msg("The QUERY keyword is expected"));
    }
    let internal = args[0].first() == Some(&b'_');
    // `execCommandCommon`'s memory guard (the debug wrappers' `DEBUG_execCommandCommon` has
    // none), over `argv[0], argv[1]` and the query's arguments.
    if !debug && let Some(o) = oom::guard(cx) {
        let mut v = vec![args[0].clone(), args[1].clone()];
        v.extend_from_slice(&args[at + 1..]);
        return oom::query(cx, o, &v, Some((limited, clock)));
    }
    // `DEBUG_execCommandCommon` over `argv[0], argv[1]` and the query's arguments.
    let (rest, params): (Vec<Bytes>, Option<Vec<Bytes>>) = if debug {
        let mut v = vec![args[0].clone(), args[1].clone()];
        v.extend_from_slice(&args[at + 1..]);
        let (c, p) = debug::split(&v)?;
        if c.len() < 2 {
            return Err(arity_error(&format!("_FT.DEBUG|{}", redis_proto::escape(&args[0]).to_ascii_uppercase())));
        }
        (c[2..].to_vec(), Some(p.to_vec()))
    } else {
        (args[at + 1..].to_vec(), None)
    };
    let search = cmd == Command::Search;
    // `execCommandCommon` counts an error building the request (a standalone server's as the
    // coordinator's); `DEBUG_execCommandCommon` does not.
    let pre = (!debug).then_some(true);
    let mut req = areq::compile(cmd, &cx.eng.cfg, &rest).map_err(|e| count_error(cx.eng, e.into(), pre))?;
    req.internal = internal;
    let Some(name) = cx.eng.search.load(cstr(&args[1]), false) else {
        return Err(QErr::user(Code::NoIndex, "Index not found", format!(": {}", qast::lossy(&args[1]))).into());
    };
    let resp3 = cx.client.resp >= 3;
    {
        let eng = &mut *cx.eng;
        let ix = eng.search.indexes.get_mut(&name).expect("loaded");
        if let Err(e) = req.apply_context(ix, resp3) {
            return Err(count_error(eng, e.into(), pre));
        }
        // `prepareRequest`: the dialect counts once the request is built, whatever the run.
        ix.used_dialects |= 1 << (req.dialect.clamp(1, 8) - 1);
    }
    let parse_ns = clock.elapsed().as_nanos() as u64;
    let f = req.flags;
    if !search && f & F_HAS_WITHCOUNT != 0 && f & (F_HAS_SORTBY | F_HAS_GROUPBY | F_NOROWS) == 0 {
        req.flags |= F_HAS_DEPLETER;
    }
    let background = cx.can_block() && cx.eng.cfg.num("search-workers") > 0;
    req.plan_errors = plan_side(internal, background, debug);
    if let Some(p) = &params {
        req.set_debug(debug_parse(p, &req, background));
    }
    let db = cx.client.db;
    // An internal profile may open a cursor (`_FT.CURSOR PROFILE` ends it with the profile).
    if req.flags & F_IS_CURSOR != 0 {
        req.cap_max_idle(&cx.eng.cfg);
        let pc = ProfCur { limited, background, internal, clock: Some(clock), parse_ns, ..ProfCur::default() };
        return start_cursor(cx, name, req, db, resp3, background, Some(Box::new(pc)));
    }
    let mut prof = ProfOut::new(limited, clock);
    let results = {
        let eng: &Engine = cx.eng;
        let ix: &Index = eng.search.indexes.get(&name).expect("loaded");
        if search {
            // FT.SEARCH's reply, then its pipeline run again for the counters.
            let reply = fq::run_search(eng, ix, &req, db, resp3, background, true)?;
            run(eng, ix, &req, db, resp3, background, u32::MAX, None, None, &mut false, Some(&mut prof))?;
            reply
        } else {
            run(eng, ix, &req, db, resp3, background, req.max_aggregate_results as u32, None, None, &mut false, Some(&mut prof))?.0
        }
    };
    count_dialect(cx, &name, &req);
    let mut pc = ProfCur { limited, background, internal, clock: Some(clock), parse_ns, ..ProfCur::default() };
    pc.merge(&prof);
    pc.reads = 0;
    let profile = pc.reply();
    let kv = |k: &str, v: Reply| (Reply::Status(k.into()), v);
    Ok(if resp3 { Reply::Map(vec![kv("Results", results), kv("Profile", profile)]) } else { Reply::Array(vec![results, profile]) })
}

/// A profile's counters (`ProfilePrinterCtx`, the processors' `RPProfile`s, the iterators'),
/// summed over a cursor's reads.
#[derive(Default)]
struct ProfCur {
    limited: bool,
    background: bool,
    internal: bool,
    /// When the command started.
    clock: Option<Instant>,
    parse_ns: u64,
    build_ns: u64,
    /// `cursor_reads` (0: not a cursor).
    reads: u64,
    /// The processors (as the first chunk built them) and their calls and time.
    names: Vec<(String, bool)>,
    counts: Vec<(u64, u64)>,
    iters: Option<Reply>,
    timed_out: bool,
    expansions: bool,
    /// The memory guard's empty reply (ON_OOM RETURN).
    oom: bool,
}

impl ProfCur {
    /// A chunk's profile collector.
    fn chunk_out(&self) -> ProfOut {
        ProfOut::new(self.limited, self.clock.unwrap_or_else(Instant::now))
    }

    /// A chunk's counters, added.
    fn merge(&mut self, o: &ProfOut) {
        if self.names.is_empty() {
            self.names = o.names.clone();
            self.counts = o.counts.clone();
            self.build_ns = o.build_ns.saturating_sub(self.parse_ns);
        } else {
            for (i, &(n, ns)) in o.counts.iter().enumerate() {
                if let Some(c) = self.counts.get_mut(o.base + i) {
                    c.0 += n;
                    c.1 += ns;
                }
            }
        }
        if let Some(it) = &o.iters {
            self.iters = Some(match self.iters.take() {
                Some(acc) => merge_iters(acc, it),
                None => it.clone(),
            });
        }
        self.timed_out |= o.timed_out;
        self.expansions |= o.expansions;
        self.reads += 1;
    }

    /// `Profile_PrintInFormat` with `Profile_PrintCommon`: the single shard, no coordinator.
    fn reply(&self) -> Reply {
        let ms = |ns: u64| Reply::Double(ns as f64 / 1_000_000.0);
        let kv = |k: &str, v: Reply| (Reply::Status(k.into()), v);
        let mut shard = vec![
            kv("Total profile time", ms(self.clock.map_or(0, |c| c.elapsed().as_nanos() as u64))),
            kv("Parsing time", ms(self.parse_ns)),
            kv("Workers queue time", ms(0)),
            kv("Pipeline creation time", ms(self.build_ns)),
        ];
        if self.background {
            shard.push(kv("Total GIL time", ms(0)));
        }
        if self.internal {
            shard.push(kv("Coordinator dispatch time [ms]", ms(0)));
        }
        let mut w = Vec::new();
        if self.oom {
            w.push(Reply::Status(oom::SHARD_WARNING.into()));
        }
        if self.timed_out {
            w.push(Reply::Status(TIMEOUT_WARNING.into()));
        }
        if self.expansions {
            w.push(Reply::Status("Max prefix expansions limit was reached".into()));
        }
        if w.is_empty() {
            w.push(Reply::Status("None".into()));
        }
        shard.push(kv("Warning", Reply::Array(w)));
        if self.reads > 0 {
            shard.push(kv("Internal cursor reads", Reply::Int(self.reads as i64)));
        }
        if let Some(it) = &self.iters {
            shard.push(kv("Iterators profile", it.clone()));
        }
        shard.push(kv("Result processors profile", Reply::Array(rps_reply(&self.names, &self.counts))));
        Reply::Map(vec![kv("Shards", Reply::Array(vec![Reply::Map(shard)])), kv("Coordinator", Reply::Map(Vec::new()))])
    }
}

/// `printProfileRP`: each processor's type, its own time (its upstream's taken off) and the
/// results it gave (its calls but the last).
fn rps_reply(names: &[(String, bool)], counts: &[(u64, u64)]) -> Vec<Reply> {
    let mut out = Vec::with_capacity(names.len());
    let mut upstream = 0.0;
    for (i, (name, gil)) in names.iter().enumerate() {
        let (n, ns) = counts.get(i).copied().unwrap_or((0, 0));
        let total = ns as f64 / 1_000_000.0;
        let mut m = vec![(Reply::Status("Type".into()), Reply::Status(name.clone()))];
        if *gil {
            m.push((Reply::Status("GIL-Time".into()), Reply::Double(0.0)));
        }
        m.push((Reply::Status("Time".into()), Reply::Double(total - upstream)));
        m.push((Reply::Status("Results processed".into()), Reply::Int(n as i64 - 1)));
        out.push(Reply::Map(m));
        upstream = total;
    }
    out
}

/// An iterator profile less an earlier one of the same tree: what was counted since.
fn sub_iters(now: Reply, base: &Reply) -> Reply {
    match (now, base) {
        (Reply::Map(a), Reply::Map(b)) if a.len() == b.len() => Reply::Map(
            a.into_iter()
                .zip(b)
                .map(|((k, v), (_, w))| {
                    let v = match (&k, v, w) {
                        (Reply::Status(n), Reply::Int(x), Reply::Int(y)) if n == "Number of reading operations" => Reply::Int(x - y),
                        (Reply::Status(n), Reply::Double(x), Reply::Double(y)) if n == "Time" => Reply::Double((x - y).max(0.0)),
                        (_, v, w) => sub_iters(v, w),
                    };
                    (k, v)
                })
                .collect(),
        ),
        (Reply::Array(a), Reply::Array(b)) if a.len() == b.len() => Reply::Array(a.into_iter().zip(b).map(|(v, w)| sub_iters(v, w)).collect()),
        (a, _) => a,
    }
}

/// Two reads' iterator profiles (the same tree) as one: their counters and times summed.
fn merge_iters(acc: Reply, new: &Reply) -> Reply {
    match (acc, new) {
        (Reply::Map(a), Reply::Map(b)) if a.len() == b.len() => Reply::Map(
            a.into_iter()
                .zip(b)
                .map(|((k, v), (_, w))| {
                    let v = match (&k, v, w) {
                        (Reply::Status(n), Reply::Int(x), Reply::Int(y)) if n == "Number of reading operations" => Reply::Int(x + y),
                        (Reply::Status(n), Reply::Double(x), Reply::Double(y)) if n == "Time" => Reply::Double(x + y),
                        (_, v, w) => merge_iters(v, w),
                    };
                    (k, v)
                })
                .collect(),
        ),
        (Reply::Array(a), Reply::Array(b)) if a.len() == b.len() => Reply::Array(a.into_iter().zip(b).map(|(v, w)| merge_iters(v, w)).collect()),
        (a, _) => a,
    }
}

/// `Document_EvalExpression`: FT.ADD's IF condition on the indexed document's schema fields.
pub(super) fn eval_if(eng: &Engine, db: usize, ix: &Index, key: &[u8], raw: &[u8]) -> Result<bool, QErr> {
    let id = ix.docs.get_id(key);
    let Some(dmd) = (id != 0).then(|| ix.docs.get(id)).flatten() else { return Err(QErr::new(Code::NoDoc, "")) };
    let e = expr::parse(cstr(raw)).map_err(|e| match e {
        Some(m) => QErr::new(Code::Expr, m),
        None => QErr::code(Code::Expr),
    })?;
    let spec = &ix.spec;
    // `RLOOKUP_OPT_ALLLOADED`: any schema field can be read.
    let mut lk = Lookup { all_loaded: true, ..Lookup::default() };
    let props = {
        let cell = RefCell::new((&mut lk, Vec::new()));
        eval::lookup_keys(&e, &|p: &[u8]| {
            let mut c = cell.borrow_mut();
            let (lk, out) = &mut *c;
            match lk.get_read(Some(spec), p, 0) {
                Some(i) => {
                    let k = KRef::of(&lk.keys[i]);
                    out.push((p.to_vec(), k));
                    true
                }
                None => false,
            }
        })?;
        cell.into_inner().1
    };
    let mut row = Row { dyn_vals: Vec::new(), sv: &dmd.sort };
    let all: Vec<usize> = (0..lk.keys.len()).collect();
    fq::load(eng, db, &mut lk, Some(spec), Some(&all), dmd, &mut row, false, 2);
    let mut st = EvalState { mode: Mode::Query, err: None };
    let view = View { props: &props, row: &row, ixr: None };
    let (rc, v) = st.run(&e, &view);
    if rc != eval::Rc::Ok {
        return Err(st.err.unwrap_or_else(|| QErr::code(Code::Generic)));
    }
    Ok(v.bool_test())
}

/// An error replied, counted by its code on the coordinator's side or the shard's (`None`:
/// not counted).
pub(super) fn count_error(eng: &Engine, e: Error, side: Option<bool>) -> Error {
    if let Some(coord) = side {
        eng.search.stats.error_reply(&e.0, coord);
    }
    e
}

/// Where a request's errors before its pipeline runs count ([`areq::Request::plan_errors`]):
/// on the workers by the request; in the foreground on the coordinator's side, but for a
/// debug wrapper's.
pub(super) fn plan_side(internal: bool, background: bool, debug: bool) -> Option<bool> {
    if background {
        Some(!internal)
    } else if debug {
        None
    } else {
        Some(true)
    }
}

/// An error building the request's plan or pipeline, counted where the request says.
pub(super) fn plan_err(eng: &Engine, req: &areq::Request, e: impl Into<Error>) -> Error {
    count_error(eng, e.into(), req.plan_errors)
}

/// A chunk replied: its warnings (`trackWarnings_Resp2`, `_replyWarnings`: the timeout's and
/// the prefix expansions', by the request), and the query (`finishSendChunk`; `None`: an error
/// after the first results left it uncounted).
pub(super) fn count_chunk(eng: &Engine, req: &areq::Request, f: Option<Counted>, elapsed: std::time::Duration, timed_out: bool, expansions: bool) {
    let stats = &eng.search.stats;
    if timed_out {
        stats.warning(QueryWarning::Timeout, !req.internal);
    }
    if expansions {
        stats.warning(QueryWarning::MaxPrefixExpansions, !req.internal);
    }
    if let Some(f) = f {
        stats.count_query(f, elapsed);
    }
}

/// ON_TIMEOUT FAIL's timeout replied as an error: by the chunk (`handleSendChunkError`, the
/// query counted too), or by the blocked client's timer on the workers
/// (`QueryTimeoutFailCallback`, `CursorReadTimeoutFailCallback`: only the error).
pub(super) fn count_timeout_error(eng: &Engine, req: &areq::Request, f: Counted, elapsed: std::time::Duration, timer: bool) {
    eng.search.stats.error(QueryError::Timeout, !req.internal);
    if !timer {
        eng.search.stats.count_query(f, elapsed);
    }
}

fn count_dialect(cx: &mut Cx, name: &[u8], req: &areq::Request) {
    if let Some(ix) = cx.eng.search.indexes.get_mut(name) {
        ix.used_dialects |= 1 << (req.dialect.clamp(1, 8) - 1);
    }
}

/// `RSCursorReadCommand`: FT.CURSOR READ index cursor_id [COUNT count].
pub(super) fn ft_cursor_read(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 4 {
        return Err(arity_error("FT.CURSOR|READ"));
    }
    verify_acl(cx, &args[2])?;
    let Some(cid) = crate::util::parse_i64(&args[3]) else {
        return Err(Error::msg("Bad cursor ID"));
    };
    let mut count = 0i64;
    if args.len() > 5 {
        if !args[4].eq_ignore_ascii_case(b"count") {
            return Err(Error::msg(format!("Unknown argument `{}`", qast::lossy(&args[4]))));
        }
        let Some(n) = crate::util::parse_i64(&args[5]) else {
            return Err(Error::msg(format!("Bad value for COUNT: `{}`", qast::lossy(&args[5]))));
        };
        count = n;
    }
    let Some(mut c) = cx.eng.search.cursors.take(cid as u64) else {
        return Err(Error::msg(format!("Cursor not found, id: {cid}")));
    };
    // `cursorRead`: the cursor's spec must still be there (a weak reference).
    let alive = cx.eng.search.indexes.get(&c.index).is_some_and(|ix| ix.id == c.spec_id);
    if !alive {
        return Err(QErr::new(Code::DroppedBackground, "The index was dropped while the cursor was idle").into());
    }
    let Some(any) = c.exec.take() else {
        return Err(Error::msg(format!("Cursor not found, id: {cid}")));
    };
    let mut exec = match any.0.downcast::<CursorExec>() {
        Ok(e) => e,
        Err(any) => match any.downcast::<hybrid::HybCursor>() {
            Ok(hc) => return hybrid::read_cursor(cx, c, hc, count as u64),
            Err(_) => return Err(Error::msg(format!("Cursor not found, id: {cid}"))),
        },
    };
    // `runCursor`: COUNT sticks for the reads after; the foreground cap applies again.
    let mut num = count as u64;
    if num == 0 {
        num = exec.st.chunk as u64;
        if num == 0 {
            num = CURSOR_READ_SIZE as u64;
        }
    }
    exec.st.chunk = num as u32;
    if exec.req.cap_timeout(&cx.eng.cfg) {
        exec.req.capped = true;
    }
    let resp3 = cx.client.resp >= 3;
    let background = cx.can_block() && cx.eng.cfg.num("search-workers") > 0;
    let mut prof = exec.st.prof.take();
    let mut out = prof.as_ref().map(|p| p.chunk_out());
    let (reply, done) = {
        let eng: &Engine = cx.eng;
        let ix: &Index = eng.search.indexes.get(&c.index).expect("alive");
        let CursorExec { req, st } = &mut *exec;
        // The documents load from the database the cursor was opened in.
        let db = st.db;
        run(eng, ix, req, db, resp3, background, num as u32, Some(st), None, &mut false, out.as_mut())?
    };
    if let (Some(p), Some(o)) = (prof.as_mut(), out.as_ref()) {
        p.merge(o);
    }
    let id = c.id;
    let reply = cursor_reply(reply, id, done, prof.as_deref(), resp3);
    if !done {
        exec.st.prof = prof;
        c.exec = Some(Exec(exec));
        cx.eng.search.cursors.pause(c);
    }
    Ok(reply)
}

/// `RSCursorProfileCommand`: _FT.CURSOR PROFILE index cursor_id — a profiled (internal)
/// cursor's profile so far, after an empty chunk; the cursor ends.
pub(super) fn ft_cursor_profile(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 4 {
        return Err(arity_error("_FT.CURSOR|PROFILE"));
    }
    let Some(cid) = crate::util::parse_i64(&args[3]) else {
        return Err(Error::msg("Bad cursor ID"));
    };
    // `%d` of the id.
    let shown = cid as i32;
    let Some(mut c) = cx.eng.search.cursors.take(cid as u64) else {
        return Err(Error::msg(format!("Cursor not found, id: {shown}")));
    };
    let Some(any) = c.exec.take() else {
        return Err(Error::msg(format!("Cursor not found, id: {shown}")));
    };
    let mut exec = match any.0.downcast::<CursorExec>() {
        Ok(e) => e,
        Err(any) => {
            c.exec = Some(Exec(any));
            cx.eng.search.cursors.pause(c);
            return Err(Error::msg(format!("cursor request is not profile, id: {shown}")));
        }
    };
    let Some(prof) = exec.st.prof.take() else {
        c.exec = Some(Exec(exec));
        cx.eng.search.cursors.pause(c);
        return Err(Error::msg(format!("cursor request is not profile, id: {shown}")));
    };
    let alive = cx.eng.search.indexes.get(&c.index).is_some_and(|ix| ix.id == c.spec_id);
    if !alive {
        return Err(Error::msg("The index was dropped while the cursor was idle"));
    }
    // `sendChunk_ReplyOnly_EmptyResults`.
    let resp3 = cx.client.resp >= 3;
    let req = &exec.req;
    let empty = if resp3 {
        let mut warnings = Vec::new();
        if req.capped {
            warnings.push(Reply::Status(CAPPED_WARNING.into()));
        }
        Reply::Map(vec![
            (Reply::Status("attributes".into()), Reply::Array(Vec::new())),
            (Reply::Status("format".into()), Reply::Status(if req.flags & FORMAT_EXPAND != 0 { "EXPAND" } else { "STRING" }.into())),
            (Reply::Status("results".into()), Reply::Array(Vec::new())),
            (Reply::Status("total_results".into()), Reply::Int(0)),
            (Reply::Status("warning".into()), Reply::Array(warnings)),
        ])
    } else {
        Reply::Array(vec![Reply::Int(0)])
    };
    Ok(cursor_reply(empty, 0, true, Some(&prof), resp3))
}

/// `RSCursorDelCommand`: FT.CURSOR DEL index cursor_id.
pub(super) fn ft_cursor_del(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 4 {
        return Err(arity_error("FT.CURSOR|DEL"));
    }
    verify_acl(cx, &args[2])?;
    let Some(cid) = crate::util::parse_i64(&args[3]) else {
        return Err(Error::msg("Bad cursor ID"));
    };
    if cx.eng.search.cursors.purge(cid as u64) { Ok(Reply::Ok) } else { Err(Error::msg("Cursor does not exist")) }
}

/// `RSCursorGCCommand`: FT.CURSOR GC index (the coordinator's handler wants one more argument).
pub(super) fn ft_cursor_gc(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 4 {
        return Err(arity_error("FT.CURSOR|GC"));
    }
    verify_acl(cx, &args[2])?;
    Ok(Reply::Int(cx.eng.search.cursors.gc()))
}

// ---------------------------------------------------------------------------------------------
// Results and keys

/// A key as a processor captured it when the pipeline was built (`const RLookupKey *`).
#[derive(Clone, Copy, Debug)]
struct KRef {
    dst: usize,
    flags: u32,
    svidx: usize,
}

impl KRef {
    fn of(k: &LKey) -> KRef {
        KRef { dst: k.dst, flags: k.flags, svidx: k.svidx }
    }
}

/// An APPLY output key: its values are `RSValue` references (`setReferenceValue`,
/// `RSValue_MakeOwnReference`), which `RSValue_IsArray` does not look through. Every array an
/// expression yields is such a reference.
const K_AGG_REF_ARR: u32 = 1 << 20;
/// An APPLY output key whose strings are references too (`RSValue_IsString` fails on them).
const K_AGG_REF_STR: u32 = 1 << 21;

/// Whether an expression's string results are references: properties, literals and the
/// branch `case` takes when both are.
fn yields_str_ref(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) | Expr::Property(_) => true,
        Expr::Func { func, args } => match expr::FUNCTIONS[*func].name {
            "split" | "matched_terms" => true,
            "case" => args.iter().skip(1).all(|a| a.as_ref().is_some_and(yields_str_ref)),
            _ => false,
        },
        _ => false,
    }
}

impl KRef {
    /// `RSValue_IsArray` on the key's value.
    fn array<'v>(&self, v: &'v RsValue) -> Option<&'v [RsValue]> {
        match v {
            RsValue::Array(a) if self.flags & K_AGG_REF_ARR == 0 => Some(a),
            _ => None,
        }
    }
}

fn copy_key(k: &LKey) -> LKey {
    LKey { name: k.name.clone(), path: k.path.clone(), flags: k.flags, svidx: k.svidx, dst: k.dst, dead: k.dead }
}

/// `RLookupRow_Get`: the row's own value, else the sorting vector's for sortable keys.
fn rget(row: &Row<'_>, k: KRef) -> Option<RsValue> {
    if let Some(Some(v)) = row.dyn_vals.get(k.dst) {
        return Some(v.clone());
    }
    if k.flags & K_SV_SRC != 0 {
        return row.sv.get(k.svidx).and_then(fq::sort_val);
    }
    None
}

fn rset(row: &mut Row<'_>, k: KRef, v: RsValue) {
    if row.dyn_vals.len() <= k.dst {
        row.dyn_vals.resize(k.dst + 1, None);
    }
    row.dyn_vals[k.dst] = Some(v);
}

/// A result moving down the pipeline (`SearchResult`).
struct Res<'a> {
    doc_id: u64,
    score: f64,
    expl: Option<Expl>,
    dmd: Option<&'a Dmd>,
    row: Row<'a>,
    ixr: Option<IxResult>,
}

impl<'a> Res<'a> {
    fn new() -> Res<'a> {
        Res { doc_id: 0, score: 0.0, expl: None, dmd: None, row: Row { dyn_vals: Vec::new(), sv: &[] }, ixr: None }
    }

    /// `SearchResult_Clear`: everything but the doc id.
    fn clear(&mut self) {
        self.score = 0.0;
        self.expl = None;
        self.ixr = None;
        self.row.dyn_vals.clear();
        self.row.sv = &[];
        self.dmd = None;
    }
}

/// `RS_RESULT_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum St {
    Ok,
    Eof,
    Error,
    TimedOut,
}

/// `RSTimeoutPolicy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) enum Policy {
    #[default]
    Return,
    Fail,
    Strict,
}

impl Policy {
    /// From `search-on-timeout`'s index.
    pub(super) fn of(v: i64) -> Policy {
        match v {
            1 => Policy::Fail,
            2 => Policy::Strict,
            _ => Policy::Return,
        }
    }
}

/// `TIMEOUT_COUNTER_LIMIT`: the query reader consults the clock once per this many reads.
const TIMEOUT_COUNTER_LIMIT: u32 = 100;

/// A request's clock (`SearchTime`) as its query reader checks it, and `_FT.DEBUG`'s
/// `RPTimeoutAfterCount` just downstream of the reader.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Tmo {
    /// `sctx->time.timeout`: now plus the query's timeout (0: `INT32_MAX` ms).
    deadline: Option<Instant>,
    /// `RPQueryIterator.timeoutLimiter`; `None` (`REDISEARCH_UNINITIALIZED`) skips the checks.
    limiter: Option<u32>,
    /// `!skipTimeoutChecks`: the check after a buffered run.
    checks: bool,
    /// `TIMEOUT_AFTER_N`'s count and what is left of it.
    after: Option<(u32, u32)>,
    /// `RPTimeoutAfterCount_SimulateTimeout`: the clock reads as timed out until the call
    /// returns.
    sim: bool,
}

impl Tmo {
    /// `SearchCtx_UpdateTime`, `shouldCheckInPipelineTimeout` (the limiter is set when the
    /// reader is built) and the debug timeout, which turns the checks on.
    pub(super) fn new(timeout_ms: i64, policy: Policy, background: bool, after: Option<u32>) -> Tmo {
        let skip = !(timeout_ms > 0 && (policy == Policy::Return || !background));
        let mut t = Tmo { deadline: None, limiter: (!skip).then_some(0), checks: !skip || after.is_some(), after: after.map(|n| (n, n)), sim: false };
        t.refresh(timeout_ms);
        t
    }

    /// `SearchCtx_UpdateTime` (a cursor's every read).
    pub(super) fn refresh(&mut self, timeout_ms: i64) {
        let ms = if timeout_ms == 0 { i32::MAX as i64 } else { timeout_ms as i32 as i64 };
        self.deadline = Instant::now().checked_add(std::time::Duration::from_millis(ms.max(0) as u64));
    }

    /// `AREQ_ShouldCheckTimeout` (the debug timeout aside) and `TimedOut`: the check once the
    /// query's iterators are built.
    pub(super) fn plan_timed_out(&self) -> bool {
        self.limiter.is_some() && self.passed()
    }

    /// The deadline the query's iterators check (`None`: `skipTimeoutChecks`).
    pub(super) fn iter_deadline(&self) -> Option<Instant> {
        self.limiter.and(self.deadline)
    }

    /// `!skipTimeoutChecks`.
    pub(super) fn checks(&self) -> bool {
        self.checks
    }

    /// `TimedOut`.
    pub(super) fn passed(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// `TimedOut_WithCounter`.
    pub(super) fn tick(&mut self) -> bool {
        let Some(c) = self.limiter.as_mut() else { return false };
        *c += 1;
        if *c != TIMEOUT_COUNTER_LIMIT {
            return false;
        }
        *c = 0;
        self.sim || self.passed()
    }

    /// `RPTimeoutAfterCount_Next`, before it calls its upstream (the root processor): past its
    /// count it makes the root's next check time out.
    pub(super) fn begin(&mut self) {
        let Some((_, rem)) = self.after.as_mut() else { return };
        if *rem == 0 {
            self.limiter = Some(TIMEOUT_COUNTER_LIMIT - 1);
            self.sim = true;
        } else {
            *rem -= 1;
        }
    }

    /// … and once it returned: the clock is back, and a timeout restarts the count (a
    /// cursor's next read).
    pub(super) fn end(&mut self, timed_out: bool) {
        if std::mem::take(&mut self.sim)
            && timed_out
            && let Some((count, rem)) = self.after.as_mut()
        {
            *rem = *count;
        }
    }
}

/// `QueryProcessingCtx` and what the processors read from the request.
struct Pcx<'a> {
    eng: &'a Engine,
    db: usize,
    spec: &'a Spec,
    lookups: Vec<Lookup>,
    total: i64,
    skipped: i64,
    result_limit: u32,
    min_score: f64,
    err: Option<QErr>,
    force: bool,
    dialect: u32,
    /// FT.PROFILE's per-processor counters (`RPProfile`): calls and their time, upstream's
    /// included, by chain position.
    prof: Option<Vec<(u64, u64)>>,
    /// `timeoutPolicy`.
    policy: Policy,
    /// The query's clock.
    tmo: Tmo,
    /// The query iterators' (NOT iterators' timeouts).
    iclock: Option<&'a iters::IterClock>,
}

impl Pcx<'_> {
    fn set_err(&mut self, e: QErr) {
        if self.err.is_none() {
            self.err = Some(e);
        }
    }
}

trait Proc<'a> {
    fn next(&mut self, up: &mut [Box<dyn Proc<'a> + 'a>], cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St;
    /// Its profile's type (`RPTypeToString`, `RPEvaluator_Reply`).
    fn name(&self) -> String;
    /// `RP_SAFE_LOADER`: its profile shows the time it held the GIL.
    fn gil(&self) -> bool {
        false
    }
    /// The query iterator's profile tree.
    fn profile_iters(&self, _pc: &PCtx) -> Option<Pairs> {
        None
    }
    /// Drained its upstream and now yields what it holds (sorters, groupers, depleters,
    /// normalizers): what it still holds is all a cursor's later chunks read from upstream.
    fn drained(&self) -> bool {
        false
    }
    /// What a cursor keeps between chunks.
    fn save(&mut self) -> Option<Saved> {
        None
    }
    fn restore(&mut self, _s: Saved) {}
}

/// A processor's state between a cursor's chunks.
enum Saved {
    /// The query iterator's last document.
    Query(u64),
    /// What a KNN query (read by distance, not by id) had not yet given.
    QueryRest(VecDeque<IxResult>),
    Pager { offset: u32, remaining: u32, skipping: bool },
    /// What the last accumulating processor had left, then its final status.
    Replay(VecDeque<Pend>, St),
    /// A grouper's groups so far (a timeout stopped it).
    Groups(Vec<Group>, HashMap<u64, usize>),
}

/// A result an accumulating processor still held when its cursor paused (the module's
/// buffered `SearchResult`, its document metadata referenced).
struct Pend {
    /// Into the cursor's held document metadata.
    dmd: Option<usize>,
    has_sv: bool,
    doc_id: u64,
    score: f64,
    expl: Option<Expl>,
    vals: Vec<Option<RsValue>>,
    ixr: Option<IxResult>,
}

impl Pend {
    fn of(r: Res<'_>, dmds: &mut Vec<Dmd>) -> Pend {
        let dmd = r.dmd.map(|d| {
            dmds.push(d.clone());
            dmds.len() - 1
        });
        Pend { dmd, has_sv: !r.row.sv.is_empty(), doc_id: r.doc_id, score: r.score, expl: r.expl, vals: r.row.dyn_vals, ixr: r.ixr }
    }
}

/// The source of a cursor's later chunks once an accumulating processor drained the query.
struct Replay<'a> {
    dmds: &'a [Dmd],
    items: VecDeque<Pend>,
    tail: St,
}

impl<'a> Proc<'a> for Replay<'a> {
    fn name(&self) -> String {
        "Index".into()
    }

    fn drained(&self) -> bool {
        true
    }

    fn next(&mut self, _up: &mut Chain<'a>, _cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        let Some(p) = self.items.pop_front() else {
            let st = self.tail;
            self.tail = St::Eof;
            return st;
        };
        let dmd = p.dmd.map(|i| &self.dmds[i]);
        let sv: &'a [_] = match dmd {
            Some(d) if p.has_sv => &d.sort,
            _ => &[],
        };
        *r = Res { doc_id: p.doc_id, score: p.score, expl: p.expl, dmd, row: Row { dyn_vals: p.vals, sv }, ixr: p.ixr };
        St::Ok
    }

    fn save(&mut self) -> Option<Saved> {
        Some(Saved::Replay(std::mem::take(&mut self.items), self.tail))
    }

    fn restore(&mut self, s: Saved) {
        if let Saved::Replay(items, tail) = s {
            (self.items, self.tail) = (items, tail);
        }
    }
}

type Chain<'a> = [Box<dyn Proc<'a> + 'a>];

/// The last processor's `Next`.
fn pull<'a>(chain: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
    let Some((p, up)) = chain.split_last_mut() else { return St::Eof };
    if cx.prof.is_none() {
        return p.next(up, cx, r);
    }
    let i = up.len();
    let t = Instant::now();
    let st = p.next(up, cx, r);
    let ns = t.elapsed().as_nanos() as u64;
    if let Some(pr) = &mut cx.prof {
        if pr.len() <= i {
            pr.resize(i + 1, (0, 0));
        }
        pr[i].0 += 1;
        pr[i].1 += ns;
    }
    st
}

// ---------------------------------------------------------------------------------------------
// Processors

/// `RPQueryIterator`: live documents the query matches.
struct QueryIt<'a> {
    it: BoxIt<'a>,
    ix: &'a Index,
    now: i64,
    /// The last document read.
    last: u64,
    /// A cursor's earlier chunks read up to this document.
    resume: Option<u64>,
    /// The query is a KNN one: its results come by distance, all known once it ran, so a
    /// cursor keeps those it has not given yet.
    knn: bool,
    /// Those results, when the cursor resumes.
    rest: Option<VecDeque<IxResult>>,
    /// A profiled cursor's resumed iterators, as their counters were once back where the
    /// earlier chunks left them: what this chunk counts starts there.
    base: Option<Pairs>,
}

impl<'a> Proc<'a> for QueryIt<'a> {
    fn name(&self) -> String {
        "Index".into()
    }

    fn profile_iters(&self, pc: &PCtx) -> Option<Pairs> {
        let now = self.it.print(pc, None);
        Some(match &self.base {
            Some(base) => match sub_iters(Reply::Map(now), &Reply::Map(base.clone())) {
                Reply::Map(m) => m,
                _ => unreachable!("a map"),
            },
            None => now,
        })
    }

    fn next(&mut self, _up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        // Behind `RPTimeoutAfterCount`.
        cx.tmo.begin();
        let st = self.read(cx, r);
        cx.tmo.end(st == St::TimedOut);
        st
    }

    fn save(&mut self) -> Option<Saved> {
        if !self.knn {
            return Some(Saved::Query(self.last));
        }
        if let Some(rest) = self.rest.take() {
            return Some(Saved::QueryRest(rest));
        }
        if self.last == 0 {
            // not read yet
            return None;
        }
        let mut rest = VecDeque::new();
        while self.it.read() {
            if let Some(cur) = self.it.current() {
                rest.push_back(cur.clone());
            }
        }
        Some(Saved::QueryRest(rest))
    }

    fn restore(&mut self, s: Saved) {
        match s {
            Saved::Query(last) if last > 0 => {
                self.last = last;
                self.resume = Some(last);
            }
            Saved::QueryRest(rest) => self.rest = Some(rest),
            _ => {}
        }
    }
}

impl<'a> QueryIt<'a> {
    /// `rpQueryItNext`: the clock checked every 100 reads.
    fn read(&mut self, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        loop {
            if cx.tmo.tick() {
                return St::TimedOut;
            }
            let ixr = if let Some(rest) = self.rest.as_mut() {
                let Some(ixr) = rest.pop_front() else { return St::Eof };
                ixr
            } else {
                let more = match self.resume.take() {
                    // Profiled, the rebuilt iterators read their way back to the last document,
                    // read for read as the earlier chunks did, which leaves every one of them
                    // where the module's kept iterators are; their counters start there.
                    Some(last) if cx.prof.is_some() => {
                        if let Some(c) = cx.iclock {
                            c.paused.set(true);
                        }
                        let mut more = self.it.read();
                        let at = |it: &mut BoxIt<'a>| it.current().map_or(u64::MAX, |c| c.doc_id);
                        while more && at(&mut self.it) < last {
                            more = self.it.read();
                        }
                        if let Some(c) = cx.iclock {
                            c.paused.set(false);
                        }
                        self.base = Some(self.it.print(&PCtx { limited: false, clock: true }, None));
                        if more && at(&mut self.it) == last { self.it.read() } else { more }
                    }
                    Some(last) => self.it.skip_to(last + 1) != Skip::Eof,
                    None => self.it.read(),
                };
                // `ITERATOR_TIMEOUT`: a cursor's next read goes on from where the iterators
                // stopped.
                if cx.iclock.is_some_and(|c| c.timed_out.take()) {
                    self.last = self.last.max(self.it.last_doc_id());
                    return St::TimedOut;
                }
                if !more {
                    return St::Eof;
                }
                let Some(cur) = self.it.current() else { return St::Eof };
                cur.clone()
            };
            self.last = ixr.doc_id;
            let ix: &'a Index = self.ix;
            let Some(dmd) = ix.docs.get(ixr.doc_id) else { continue };
            if dmd.flags & DOC_DELETED != 0 || (dmd.expire_at != 0 && dmd.expire_at <= self.now) {
                continue;
            }
            cx.total += 1;
            r.doc_id = dmd.id;
            r.ixr = Some(ixr);
            r.score = 0.0;
            r.dmd = Some(dmd);
            r.row.sv = &dmd.sort;
            return St::Ok;
        }
    }
}

/// `RPMetricsLoader`: the query's yielded distances into their keys.
struct MetricsRp {
    keys: Vec<KRef>,
}

impl<'a> Proc<'a> for MetricsRp {
    fn name(&self) -> String {
        "Metrics Applier".into()
    }

    fn next(&mut self, up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        let st = pull(up, cx, r);
        if st != St::Ok {
            return st;
        }
        let metrics = r.ixr.as_ref().map(|x| x.metrics.clone()).unwrap_or_default();
        for (m, v) in metrics {
            rset(&mut r.row, self.keys[m], RsValue::Number(v));
        }
        St::Ok
    }
}

/// `RPScorer`.
struct ScoreRp<'a> {
    scorer: Scorer,
    args: scorers::Args<'a>,
    explain: bool,
    key: Option<KRef>,
}

impl<'a> Proc<'a> for ScoreRp<'a> {
    fn name(&self) -> String {
        "Scorer".into()
    }

    fn next(&mut self, up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        loop {
            let st = pull(up, cx, r);
            if st != St::Ok {
                return st;
            }
            let default = IxResult::default();
            let (Some(dmd), ixr) = (r.dmd, r.ixr.as_ref().unwrap_or(&default)) else { return St::Ok };
            let (s, e) = self.scorer.score(&self.args, ixr, dmd, cx.min_score, self.explain);
            r.score = s;
            if self.explain {
                r.expl = e;
            }
            if s == scorers::FILTEROUT {
                cx.total -= 1;
                r.clear();
                continue;
            }
            if let Some(k) = self.key {
                rset(&mut r.row, k, RsValue::Number(s));
            }
            return St::Ok;
        }
    }
}

/// `RPMaxScoreNormalizer`: every result, then each (last first) divided by the top score.
struct MaxNorm<'a> {
    pool: Vec<Res<'a>>,
    max: f64,
    key: Option<KRef>,
    yielding: bool,
    /// The query timed out (ON_TIMEOUT RETURN): what the pool held, then the timeout.
    timed_out: bool,
}

impl<'a> Proc<'a> for MaxNorm<'a> {
    fn name(&self) -> String {
        "Score Max Normalizer".into()
    }

    fn drained(&self) -> bool {
        self.yielding
    }

    fn next(&mut self, up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        if !self.yielding {
            let saved = cx.result_limit;
            cx.result_limit = u32::MAX;
            loop {
                let mut p = Res::new();
                match pull(up, cx, &mut p) {
                    St::Ok => {}
                    St::Eof => break,
                    St::TimedOut if cx.policy == Policy::Return => {
                        self.timed_out = true;
                        break;
                    }
                    st => {
                        cx.result_limit = saved;
                        return st;
                    }
                }
                self.max = if self.max > p.score { self.max } else { p.score };
                self.pool.push(p);
            }
            cx.result_limit = saved;
            self.yielding = true;
        }
        let Some(p) = self.pool.pop() else { return if std::mem::take(&mut self.timed_out) { St::TimedOut } else { St::Eof } };
        *r = p;
        let old = r.score;
        if self.max != 0.0 {
            r.score /= self.max;
        }
        if let Some(k) = self.key {
            rset(&mut r.row, k, RsValue::Number(r.score));
        }
        if let Some(e) = &mut r.expl {
            e.s = format!("Final BM25STD.NORM: {:.2} = Original Score: {:.2} / Max Score: {:.2}", r.score, old, self.max);
        }
        St::Ok
    }
}

enum SortBy {
    Fields { keys: Vec<KRef>, asc: u64 },
    Score,
}

/// `SearchResult_CmpByFields`.
fn cmp_fields(keys: &[KRef], asc: u64, a: &Res<'_>, b: &Res<'_>, err: &mut Option<QErr>) -> i32 {
    for (i, &k) in keys.iter().enumerate() {
        let ascending = asc & (1u64 << i) != 0;
        match (rget(&a.row, k), rget(&b.row, k)) {
            (Some(x), Some(y)) => {
                let o = x.cmp_with(&y, Some(&mut *err));
                if o != Ordering::Equal {
                    let o = if ascending { o.reverse() } else { o };
                    return if o == Ordering::Less { -1 } else { 1 };
                }
            }
            (Some(_), None) => return 1,
            (None, Some(_)) => return -1,
            (None, None) => {}
        }
    }
    let ascending = !keys.is_empty() && asc & (1u64 << (keys.len() - 1)) != 0;
    let rc = if a.doc_id < b.doc_id { -1 } else { 1 };
    if ascending { -rc } else { rc }
}

/// `cmpByScore`.
fn cmp_score(a: &Res<'_>, b: &Res<'_>) -> i32 {
    if a.score < b.score {
        -1
    } else if a.score > b.score {
        1
    } else if a.doc_id > b.doc_id {
        -1
    } else {
        1
    }
}

/// `RPSorter`: the top results in a min-max heap, yielded best first.
struct Sorter<'a> {
    heap: MmHeap<Res<'a>>,
    by: SortBy,
    pooled: Res<'a>,
    yielding: bool,
    /// The query timed out (ON_TIMEOUT RETURN): the heap, then the timeout.
    timed_out: bool,
}

impl Sorter<'_> {
    fn new(size: usize, by: SortBy) -> Self {
        Sorter { heap: MmHeap::new(size), by, pooled: Res::new(), yielding: false, timed_out: false }
    }
}

impl<'a> Sorter<'a> {
    fn cmp(by: &SortBy, a: &Res<'_>, b: &Res<'_>, err: &mut Option<QErr>) -> i32 {
        match by {
            SortBy::Fields { keys, asc } => cmp_fields(keys, *asc, a, b, err),
            SortBy::Score => cmp_score(a, b),
        }
    }
}

impl<'a> Proc<'a> for Sorter<'a> {
    fn name(&self) -> String {
        "Sorter".into()
    }

    fn drained(&self) -> bool {
        self.yielding
    }

    fn next(&mut self, up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        if !self.yielding {
            let saved = cx.result_limit;
            cx.result_limit = u32::MAX;
            loop {
                match pull(up, cx, &mut self.pooled) {
                    St::Ok => {}
                    St::Eof => break,
                    // RETURN yields the sorted prefix, RETURN-STRICT leaves it to the drain.
                    St::TimedOut if cx.policy != Policy::Fail => {
                        cx.result_limit = saved;
                        self.yielding = true;
                        if cx.policy == Policy::Strict {
                            return St::TimedOut;
                        }
                        self.timed_out = true;
                        break;
                    }
                    st => {
                        cx.result_limit = saved;
                        return st;
                    }
                }
                let by = &self.by;
                let err = &mut cx.err;
                let mut c = |a: &Res<'a>, b: &Res<'a>| Sorter::cmp(by, a, b, err);
                if self.heap.count() < self.heap.size {
                    let p = std::mem::replace(&mut self.pooled, Res::new());
                    let score = p.score;
                    self.heap.insert(&mut c, p);
                    if score < cx.min_score {
                        cx.min_score = score;
                    }
                } else {
                    let min = self.heap.peek_min().expect("a full heap");
                    if min.score > cx.min_score {
                        cx.min_score = min.score;
                    }
                    let err = &mut cx.err;
                    let mut c = |a: &Res<'a>, b: &Res<'a>| Sorter::cmp(by, a, b, err);
                    if c(&self.pooled, self.heap.peek_min().expect("a full heap")) > 0 {
                        let p = std::mem::replace(&mut self.pooled, Res::new());
                        self.pooled = self.heap.exchange_min(&mut c, p);
                    }
                    self.pooled.clear();
                }
            }
            cx.result_limit = saved;
            self.yielding = true;
        }
        let by = &self.by;
        let err = &mut cx.err;
        let mut c = |a: &Res<'a>, b: &Res<'a>| Sorter::cmp(by, a, b, err);
        match self.heap.pop_max(&mut c) {
            Some(best) => {
                *r = best;
                St::Ok
            }
            None if std::mem::take(&mut self.timed_out) => St::TimedOut,
            None => St::Eof,
        }
    }
}

/// `RPPager`: skips `offset` results, then passes `remaining`.
struct Pager {
    offset: u32,
    remaining: u32,
    skipping: bool,
}

impl<'a> Proc<'a> for Pager {
    fn name(&self) -> String {
        "Pager/Limiter".into()
    }

    fn save(&mut self) -> Option<Saved> {
        Some(Saved::Pager { offset: self.offset, remaining: self.remaining, skipping: self.skipping })
    }

    fn restore(&mut self, s: Saved) {
        if let Saved::Pager { offset, remaining, skipping } = s {
            (self.offset, self.remaining, self.skipping) = (offset, remaining, skipping);
        }
    }

    fn next(&mut self, up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        if self.skipping {
            let limit = self.remaining.min(cx.result_limit);
            let downstream = cx.result_limit;
            cx.result_limit = self.offset.wrapping_add(limit);
            while self.offset > 0 {
                let st = pull(up, cx, r);
                if st != St::Ok {
                    return st;
                }
                cx.result_limit = cx.result_limit.wrapping_sub(1);
                self.offset -= 1;
                r.clear();
            }
            cx.result_limit = downstream;
            self.skipping = false;
        }
        if self.remaining == 0 {
            return St::Eof;
        }
        let st = pull(up, cx, r);
        if st == St::Ok {
            self.remaining -= 1;
        }
        st
    }
}

/// `RPCounter`: drains its upstream (the total is all LIMIT 0 0 wants).
struct Counter;

impl<'a> Proc<'a> for Counter {
    fn name(&self) -> String {
        "Counter".into()
    }

    fn next(&mut self, up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        loop {
            match pull(up, cx, r) {
                St::Ok => r.clear(),
                st @ (St::Error | St::TimedOut) => return st,
                St::Eof => {
                    // Profiled, the module's counter also counts a call of the last processor.
                    if let Some(last) = cx.prof.as_mut().and_then(|p| p.last_mut()) {
                        last.0 += 1;
                    }
                    return St::Eof;
                }
            }
        }
    }
}

/// `RPHighlighter`: HIGHLIGHT and SUMMARIZE over the loaded fields (FT.SEARCH).
struct Highlighter {
    /// The fields SUMMARIZE/HIGHLIGHT name, with their combined settings and text field ids;
    /// `None`: every visible key of the lookup, with the defaults.
    jobs: Option<Vec<(KRef, crate::search::highlight::Settings, Option<usize>)>>,
    def: crate::search::highlight::Settings,
    li: usize,
    options: u32,
}

impl Highlighter {
    /// `processField`: the field's value summarized in place.
    fn process(spec: &Spec, st: &crate::search::highlight::Settings, k: KRef, ft_id: Option<usize>, r: &mut Res<'_>, options: u32) {
        let Some(v) = rget(&r.row, k) else { return };
        if spec.is_json() && fq::json_skips(&v) {
            return;
        }
        let s = match &v {
            RsValue::String(s) | RsValue::RedisString(s) => s,
            RsValue::Trio(t) => match &t[0] {
                RsValue::String(s) | RsValue::RedisString(s) => s,
                _ => return,
            },
            _ => return,
        };
        let (Some(ir), Some(dmd)) = (r.ixr.as_ref(), r.dmd) else { return };
        if let Some(out) = crate::search::highlight::summarize_field(st, ft_id, s, ir, dmd.byte_offsets.as_ref(), options) {
            rset(&mut r.row, k, RsValue::String(out));
        }
    }
}

impl<'a> Proc<'a> for Highlighter {
    fn name(&self) -> String {
        "Highlighter".into()
    }

    fn next(&mut self, up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        let st = pull(up, cx, r);
        if st != St::Ok || r.ixr.is_none() || r.dmd.is_none() {
            return st;
        }
        let spec = cx.spec;
        match &self.jobs {
            Some(jobs) => {
                for (k, s, ft_id) in jobs {
                    Highlighter::process(spec, s, *k, *ft_id, r, self.options);
                }
            }
            None => {
                let keys: Vec<(KRef, Option<usize>)> = cx.lookups[self.li]
                    .keys
                    .iter()
                    .filter(|k| !k.dead && k.flags & K_HIDDEN == 0)
                    .map(|k| (KRef::of(k), spec.field(cstr(&k.name)).filter(|f| f.is(crate::search::spec::T_TEXT)).and_then(|f| f.ft_id)))
                    .collect();
                for (k, ft_id) in keys {
                    Highlighter::process(spec, &self.def, k, ft_id, r, self.options);
                }
            }
        }
        St::Ok
    }
}

/// `RPDepleter`: drains its upstream, then yields what it got.
struct Depleter<'a> {
    buf: VecDeque<Res<'a>>,
    last: St,
    done: bool,
}

impl<'a> Proc<'a> for Depleter<'a> {
    fn name(&self) -> String {
        "Depleter".into()
    }

    fn drained(&self) -> bool {
        self.done
    }

    fn next(&mut self, up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        if !self.done {
            loop {
                let mut p = Res::new();
                let st = pull(up, cx, &mut p);
                if st != St::Ok {
                    self.last = st;
                    break;
                }
                self.buf.push_back(p);
            }
            // Only RETURN yields what was depleted before a timeout (the next call depletes
            // on).
            if self.last == St::TimedOut && cx.policy != Policy::Return {
                self.last = St::Eof;
                return St::TimedOut;
            }
            self.done = true;
        }
        match self.buf.pop_front() {
            Some(p) => {
                *r = p;
                St::Ok
            }
            None => {
                let st = self.last;
                self.last = St::Eof;
                st
            }
        }
    }
}

/// What a loader loads: its keys (as they were when it was built), or the whole document
/// into its lookup.
enum Load {
    Keys(Lookup),
    All(usize),
}

/// `RPLoader` / `RPSafeLoader`.
struct Loader<'a> {
    what: Load,
    safe: bool,
    buf: Vec<Res<'a>>,
    pos: usize,
    last: St,
    yielding: bool,
}

impl<'a> Loader<'a> {
    fn new(what: Load, safe: bool) -> Self {
        Loader { what, safe, buf: Vec::new(), pos: 0, last: St::Ok, yielding: false }
    }

    /// `rpLoader_loadDocument`: whether the result can still be sent.
    fn load(&mut self, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> bool {
        let Some(dmd) = r.dmd else { return false };
        if dmd.flags & DOC_DELETED != 0 {
            return false;
        }
        r.row.sv = &dmd.sort;
        let (force, dialect) = (cx.force, cx.dialect);
        match &mut self.what {
            Load::Keys(lk) => {
                let idx: Vec<usize> = (0..lk.keys.len()).collect();
                fq::load(cx.eng, cx.db, lk, Some(cx.spec), Some(&idx), dmd, &mut r.row, force, dialect)
            }
            Load::All(li) => fq::load(cx.eng, cx.db, &mut cx.lookups[*li], Some(cx.spec), None, dmd, &mut r.row, force, dialect),
        }
    }
}

impl<'a> Proc<'a> for Loader<'a> {
    fn name(&self) -> String {
        if self.safe { "Threadsafe-Loader" } else { "Loader" }.into()
    }

    fn gil(&self) -> bool {
        self.safe
    }

    fn next(&mut self, up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        if !self.safe {
            loop {
                let st = pull(up, cx, r);
                if st != St::Ok {
                    return st;
                }
                if self.load(cx, r) {
                    return St::Ok;
                }
                cx.skipped += 1;
                *r = Res::new();
            }
        }
        loop {
            if self.yielding {
                while self.pos < self.buf.len() {
                    let p = std::mem::replace(&mut self.buf[self.pos], Res::new());
                    self.pos += 1;
                    if p.dmd.is_none() {
                        continue;
                    }
                    *r = p;
                    return St::Ok;
                }
                self.buf.clear();
                self.pos = 0;
                self.yielding = false;
                let st = self.last;
                self.last = St::Ok;
                if st != St::Ok {
                    return st;
                }
            }
            // Accumulate up to the chunk's limit, then load them all.
            let limit = cx.result_limit;
            let mut st = St::Ok;
            while cx.result_limit > 0 {
                let mut p = Res::new();
                st = pull(up, cx, &mut p);
                if st != St::Ok {
                    break;
                }
                cx.result_limit -= 1;
                self.buf.push(p);
            }
            cx.result_limit = limit;
            if (st != St::Eof && st != St::Ok && !(st == St::TimedOut && cx.policy == Policy::Return)) || self.buf.is_empty() {
                return st;
            }
            self.last = st;
            for i in 0..self.buf.len() {
                let mut p = std::mem::replace(&mut self.buf[i], Res::new());
                if self.load(cx, &mut p) {
                    self.buf[i] = p;
                } else {
                    cx.skipped += 1;
                }
            }
            self.pos = 0;
            self.yielding = true;
        }
    }
}

/// An expression's properties, resolved when the pipeline was built.
struct View<'v, 'a> {
    props: &'v [(Vec<u8>, KRef)],
    row: &'v Row<'a>,
    ixr: Option<&'v IxResult>,
}

fn matched_terms(r: &IxResult, max: usize, out: &mut Vec<Vec<u8>>) {
    if out.len() >= max {
        return;
    }
    if r.kind & (K_UNION | K_INTERSECT) != 0 {
        for c in &r.children {
            matched_terms(c, max, out);
        }
    } else if r.kind & K_TERM != 0
        && let Some(t) = &r.term
    {
        out.push(t.s.clone());
    }
}

impl eval::Lookup for View<'_, '_> {
    fn get(&self, name: &[u8]) -> Option<RsValue> {
        let (_, k) = self.props.iter().find(|(n, _)| n.as_slice() == name)?;
        rget(self.row, *k)
    }

    fn matched_terms(&self, max: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if let Some(r) = self.ixr {
            matched_terms(r, max, &mut out);
        }
        out
    }
}

/// `RPEvaluator`: a projector (APPLY) or a filter (FILTER).
struct Evaluator {
    expr: Expr,
    props: Vec<(Vec<u8>, KRef)>,
    out: Option<KRef>,
}

impl Evaluator {
    /// `rpevalCommon`'s evaluation, with the pipeline's error.
    fn eval(&self, cx: &mut Pcx<'_>, r: &Res<'_>) -> Option<RsValue> {
        let mut st = EvalState { mode: Mode::Query, err: cx.err.take() };
        let view = View { props: &self.props, row: &r.row, ixr: r.ixr.as_ref() };
        let (rc, v) = st.run(&self.expr, &view);
        cx.err = st.err;
        (rc == eval::Rc::Ok).then_some(v)
    }
}

impl<'a> Proc<'a> for Evaluator {
    fn name(&self) -> String {
        let ty = if self.out.is_some() { "Projector" } else { "Filter" };
        match &self.expr {
            Expr::Literal(expr::Literal::Str(v)) => format!("{ty} - Literal {}", String::from_utf8_lossy(cstr(v))),
            Expr::Literal(expr::Literal::Num(n)) => format!("{ty} - Literal {}", expr::num_to_str(*n)),
            Expr::Literal(expr::Literal::Null) => format!("{ty} - Literal "),
            Expr::Property(p) => format!("{ty} - Property {}", String::from_utf8_lossy(p)),
            Expr::Op { op, .. } => format!("{ty} - Operator {}", *op as char),
            Expr::Func { func, .. } => format!("{ty} - Function {}", expr::FUNCTIONS[*func].name),
            Expr::Pred { cond, .. } => format!("{ty} - Predicate {}", cond.as_str()),
            Expr::Inverted(_) => format!("{ty} - Inverted"),
        }
    }

    fn next(&mut self, up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        loop {
            let st = pull(up, cx, r);
            if st != St::Ok {
                return st;
            }
            let Some(v) = self.eval(cx, r) else { return St::Error };
            match self.out {
                Some(k) => {
                    rset(&mut r.row, k, v);
                    return St::Ok;
                }
                None => {
                    if v.bool_test() {
                        return St::Ok;
                    }
                    cx.total -= 1;
                    r.clear();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Grouping and reducers

/// The reducers' value hash (`RSValue_HashStable`): a tag, then the payload; trios hash as
/// their left value. The module seeds the grouping hash per process; this one is fixed.
#[derive(Hash)]
enum Tag {
    /// Never hashed here, but it keeps the other tags' discriminants.
    #[allow(dead_code)]
    Undefined,
    Null,
    Number,
    String,
    Array,
    Map,
}

fn hash_into<H: Hasher>(v: &RsValue, h: &mut H) {
    match v.leaf() {
        RsValue::Null => Tag::Null.hash(h),
        RsValue::Number(n) => {
            Tag::Number.hash(h);
            n.to_ne_bytes().hash(h);
        }
        RsValue::String(s) | RsValue::RedisString(s) => {
            Tag::String.hash(h);
            s.as_slice().hash(h);
        }
        RsValue::Array(a) => {
            Tag::Array.hash(h);
            for e in a {
                hash_into(e, h);
            }
        }
        RsValue::Map(m) => {
            Tag::Map.hash(h);
            for (k, v) in m {
                hash_into(k, h);
                hash_into(v, h);
            }
        }
        RsValue::Trio(_) => unreachable!("leaf"),
    }
}

fn hash_value(v: &RsValue, seed: u64) -> u64 {
    let mut h = DefaultHasher::new();
    h.write_u64(seed);
    hash_into(v, &mut h);
    h.finish()
}

/// `STABLE_SEED`.
const STABLE_SEED: u64 = 0x5f61767a;
const HLL_BITS: u8 = 8;

enum RKind {
    Count,
    Sum { avg: bool },
    Min,
    Max,
    CountDistinct,
    Distinctish,
    Hll,
    HllSum,
    Quantile { pct: f64, resolution: usize },
    StdDev,
    ToList,
    FirstValue { by: Option<KRef>, asc: bool },
    Sample { len: usize },
    Collect(Box<collect::Spec>),
}

struct Reducer {
    kind: RKind,
    src: Option<KRef>,
    dst: KRef,
}

enum Acc {
    Count(u64),
    Sum { n: u64, total: f64 },
    MinMax(f64),
    Distinct(HashSet<u64>),
    Hll(Hll),
    HllSum(Option<Hll>),
    Quantile(QuantStream),
    Dev { n: u64, m: f64, s: f64 },
    List { vals: Vec<RsValue>, idx: HashMap<u64, Vec<usize>> },
    First { value: Option<RsValue>, sortval: Option<RsValue> },
    Sample { seen: usize, vals: Vec<RsValue>, rng: crate::util::Rng },
    Collect(collect::Acc),
}

impl Reducer {
    fn instance(&self) -> Acc {
        match &self.kind {
            RKind::Count => Acc::Count(0),
            RKind::Sum { .. } => Acc::Sum { n: 0, total: 0.0 },
            RKind::Min => Acc::MinMax(f64::INFINITY),
            RKind::Max => Acc::MinMax(f64::NEG_INFINITY),
            RKind::CountDistinct => Acc::Distinct(HashSet::new()),
            RKind::Distinctish | RKind::Hll => Acc::Hll(Hll::new(HLL_BITS).expect("8 bits")),
            RKind::HllSum => Acc::HllSum(None),
            RKind::Quantile { resolution, .. } => Acc::Quantile(QuantStream::new(*resolution)),
            RKind::StdDev => Acc::Dev { n: 0, m: 0.0, s: 0.0 },
            RKind::ToList => Acc::List { vals: Vec::new(), idx: HashMap::new() },
            RKind::FirstValue { .. } => Acc::First { value: None, sortval: None },
            RKind::Sample { len } => Acc::Sample { seen: 0, vals: Vec::with_capacity((*len).min(1000)), rng: crate::util::Rng::new() },
            RKind::Collect(spec) => Acc::Collect(spec.instance()),
        }
    }

    fn add(&self, acc: &mut Acc, row: &Row<'_>, doc_id: u64, all: &[(Bytes, KRef)]) {
        let src = || self.src.and_then(|k| rget(row, k));
        match (&self.kind, acc) {
            (RKind::Count, Acc::Count(c)) => *c += 1,
            (RKind::Sum { .. }, Acc::Sum { n, total }) => {
                if let Some(d) = src().and_then(|v| v.to_number()) {
                    *total += d;
                    *n += 1;
                }
            }
            (RKind::Min, Acc::MinMax(m)) => {
                if let Some(d) = src().and_then(|v| v.to_number()) {
                    *m = if *m < d { *m } else { d };
                }
            }
            (RKind::Max, Acc::MinMax(m)) => {
                if let Some(d) = src().and_then(|v| v.to_number()) {
                    *m = if *m > d { *m } else { d };
                }
            }
            (RKind::CountDistinct, Acc::Distinct(set)) => {
                if let Some(v) = src()
                    && v != RsValue::Null
                {
                    set.insert(hash_value(&v, 0));
                }
            }
            (RKind::Distinctish | RKind::Hll, Acc::Hll(h)) => {
                if let Some(v) = src()
                    && v != RsValue::Null
                {
                    let hv = hash_value(&v, STABLE_SEED);
                    h.add_hash((hv as u32) ^ ((hv >> 32) as u32));
                }
            }
            (RKind::HllSum, Acc::HllSum(h)) => {
                let Some(v) = src() else { return };
                if self.src.is_some_and(|k| k.flags & K_AGG_REF_STR != 0) {
                    return;
                }
                let (RsValue::String(buf) | RsValue::RedisString(buf)) = &v else { return };
                if buf.len() < 5 {
                    return;
                }
                let bits = buf[4];
                let regs = &buf[5..];
                if bits > 64 || 1u128.checked_shl(bits as u32) != Some(regs.len() as u128) {
                    return;
                }
                match h {
                    Some(cur) => {
                        if bits == cur.bits {
                            cur.merge(regs);
                        }
                    }
                    None => *h = Hll::load(regs),
                }
            }
            (RKind::Quantile { .. }, Acc::Quantile(q)) => {
                let Some(v) = src() else { return };
                match self.src.and_then(|k| k.array(&v)) {
                    Some(a) => a.iter().filter_map(RsValue::to_number).for_each(|d| q.insert(d)),
                    None => {
                        if let Some(d) = v.to_number() {
                            q.insert(d);
                        }
                    }
                }
            }
            (RKind::StdDev, Acc::Dev { n, m, s }) => {
                let mut push = |d: f64| {
                    *n += 1;
                    if *n == 1 {
                        *m = d;
                        *s = 0.0;
                    } else {
                        let new_m = *m + (d - *m) / *n as f64;
                        // `S + (d - M) * (d - newM)`, contracted.
                        let new_s = (d - *m).mul_add(d - new_m, *s);
                        *m = new_m;
                        *s = new_s;
                    }
                };
                let Some(v) = src() else { return };
                match self.src.and_then(|k| k.array(&v)) {
                    Some(a) => a.iter().filter_map(RsValue::to_number).for_each(push),
                    None => {
                        if let Some(d) = v.to_number() {
                            push(d);
                        }
                    }
                }
            }
            (RKind::ToList, Acc::List { vals, idx }) => {
                let mut add = |v: RsValue| {
                    let h = hash_value(&v, 0);
                    let bucket = idx.entry(h).or_default();
                    if bucket.iter().any(|&i| vals[i].equal(&v)) {
                        return;
                    }
                    bucket.push(vals.len());
                    vals.push(v);
                };
                let Some(v) = src() else { return };
                match self.src.and_then(|k| k.array(&v)) {
                    Some(a) => a.iter().cloned().for_each(&mut add),
                    None => add(v),
                }
            }
            (RKind::FirstValue { by: None, .. }, Acc::First { value, .. }) => {
                if value.is_none() {
                    *value = Some(src().unwrap_or(RsValue::Null));
                }
            }
            (RKind::FirstValue { by: Some(by), asc }, Acc::First { value, sortval }) => {
                let val = src().unwrap_or(RsValue::Null);
                let cur = rget(row, *by).unwrap_or(RsValue::Null);
                match sortval {
                    None => {
                        *value = Some(val);
                        *sortval = Some(cur);
                    }
                    Some(_) if cur == RsValue::Null => {}
                    Some(best) if *best == RsValue::Null => *best = cur,
                    Some(best) => {
                        let rc = cur.cmp_with(best, None);
                        if if *asc { rc == Ordering::Less } else { rc == Ordering::Greater } {
                            *best = cur;
                            *value = Some(val);
                        }
                    }
                }
            }
            (RKind::Sample { len }, Acc::Sample { seen, vals, rng }) => {
                let Some(v) = src() else { return };
                if *seen < *len {
                    vals.push(v);
                } else {
                    let i = rng.below(*seen + 1);
                    if i < *len {
                        vals[i] = v;
                    }
                }
                *seen += 1;
            }
            (RKind::Collect(spec), Acc::Collect(c)) => spec.add(c, row, doc_id, all),
            _ => unreachable!("reducer state"),
        }
    }

    fn finalize(&self, acc: &mut Acc, all: &[(Bytes, KRef)]) -> RsValue {
        match (&self.kind, acc) {
            (RKind::Count, Acc::Count(c)) => RsValue::Number(*c as f64),
            (RKind::Sum { avg }, Acc::Sum { n, total }) => RsValue::Number(match (*n, avg) {
                (0, _) => f64::NAN,
                (n, true) => *total / n as f64,
                (_, false) => *total,
            }),
            (RKind::Min | RKind::Max, Acc::MinMax(m)) => RsValue::Number(*m),
            (RKind::CountDistinct, Acc::Distinct(set)) => RsValue::Number(set.len() as f64),
            (RKind::Distinctish, Acc::Hll(h)) => RsValue::Number(h.count() as f64),
            (RKind::Hll, Acc::Hll(h)) => {
                let mut out = vec![0, 0, 0, 0, h.bits];
                out.extend_from_slice(&h.registers);
                RsValue::String(out)
            }
            (RKind::HllSum, Acc::HllSum(h)) => RsValue::Number(h.as_ref().map_or(0, |h| h.count()) as f64),
            (RKind::Quantile { pct, .. }, Acc::Quantile(q)) => RsValue::Number(q.query(*pct)),
            (RKind::StdDev, Acc::Dev { n, s, .. }) => {
                let variance = if *n > 1 { *s / (*n - 1) as f64 } else { 0.0 };
                RsValue::Number(variance.sqrt())
            }
            (RKind::ToList, Acc::List { vals, .. }) => RsValue::Array(vals.clone()),
            (RKind::FirstValue { .. }, Acc::First { value, .. }) => value.clone().unwrap_or(RsValue::Null),
            (RKind::Sample { .. }, Acc::Sample { vals, .. }) => RsValue::Array(vals.clone()),
            (RKind::Collect(spec), Acc::Collect(c)) => spec.finalize(c, all),
            _ => unreachable!("reducer state"),
        }
    }
}

struct Group {
    vals: Vec<RsValue>,
    acc: Vec<Acc>,
}

/// `Grouper`: rows grouped by their values (arrays split into a group per element), the
/// reducers run per group, then a row per group.
struct Grouper {
    src: Vec<KRef>,
    dst: Vec<KRef>,
    reducers: Vec<Reducer>,
    groups: Vec<Group>,
    index: HashMap<u64, usize>,
    max_groups: usize,
    yielding: bool,
    pos: usize,
    /// The source lookup and, when a COLLECT takes `FIELDS *`, its visible keys (read per row,
    /// as `LOAD *` adds keys while rows load).
    src_li: usize,
    needs_all: bool,
    all: Vec<(Bytes, KRef)>,
}

/// The lookup's keys `FIELDS *` sees: live and not hidden, in order.
fn visible_keys(lk: &Lookup) -> Vec<(Bytes, KRef)> {
    lk.keys.iter().filter(|k| !k.dead && k.flags & K_HIDDEN == 0).map(|k| (k.name.clone(), KRef::of(k))).collect()
}

impl Grouper {
    fn limit_error(&self) -> QErr {
        QErr::new(Code::Limit, format!("Aggregate GROUPBY exceeded MAX_AGGREGATE_GROUPS limit of {} groups", self.max_groups))
    }

    #[allow(clippy::too_many_arguments)]
    fn extract(&mut self, xarr: &mut [RsValue], xpos: usize, hval: u64, row: &Row<'_>, expansion: usize, doc_id: u64) -> Result<(), QErr> {
        if xpos == xarr.len() {
            let gi = match self.index.get(&hval) {
                Some(&gi) => gi,
                None => {
                    if self.groups.len() >= self.max_groups {
                        return Err(self.limit_error());
                    }
                    let acc = self.reducers.iter().map(Reducer::instance).collect();
                    self.groups.push(Group { vals: xarr.to_vec(), acc });
                    self.index.insert(hval, self.groups.len() - 1);
                    self.groups.len() - 1
                }
            };
            let g = &mut self.groups[gi];
            for (r, acc) in self.reducers.iter().zip(g.acc.iter_mut()) {
                r.add(acc, row, doc_id, &self.all);
            }
            return Ok(());
        }
        match &xarr[xpos] {
            RsValue::Array(a) if a.is_empty() => {
                let h = hash_value(&RsValue::Null, hval);
                let saved = std::mem::replace(&mut xarr[xpos], RsValue::Null);
                let rc = self.extract(xarr, xpos + 1, h, row, expansion, doc_id);
                xarr[xpos] = saved;
                rc
            }
            RsValue::Array(a) => {
                let a = a.clone();
                let mut expansion = expansion;
                if a.len() > 1 {
                    if expansion > self.max_groups / a.len() {
                        return Err(self.limit_error());
                    }
                    expansion *= a.len();
                }
                let saved = std::mem::replace(&mut xarr[xpos], RsValue::Null);
                for e in a {
                    let h = hash_value(&e, hval);
                    xarr[xpos] = e;
                    if let Err(err) = self.extract(xarr, xpos + 1, h, row, expansion, doc_id) {
                        xarr[xpos] = saved;
                        return Err(err);
                    }
                }
                xarr[xpos] = saved;
                Ok(())
            }
            v => {
                let h = hash_value(v, hval);
                self.extract(xarr, xpos + 1, h, row, expansion, doc_id)
            }
        }
    }
}

impl<'a> Proc<'a> for Grouper {
    fn name(&self) -> String {
        "Grouper".into()
    }

    fn drained(&self) -> bool {
        self.yielding
    }

    /// A grouper a timeout stopped keeps its groups for the next chunk.
    fn save(&mut self) -> Option<Saved> {
        (!self.yielding).then(|| Saved::Groups(std::mem::take(&mut self.groups), std::mem::take(&mut self.index)))
    }

    fn restore(&mut self, s: Saved) {
        if let Saved::Groups(groups, index) = s {
            (self.groups, self.index) = (groups, index);
        }
    }

    fn next(&mut self, up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        if !self.yielding {
            let saved = cx.result_limit;
            cx.result_limit = u32::MAX;
            let mut st;
            loop {
                st = pull(up, cx, r);
                if st != St::Ok {
                    break;
                }
                if self.needs_all {
                    self.all = visible_keys(&cx.lookups[self.src_li]);
                }
                let mut vals: Vec<RsValue> = self.src.iter().map(|&k| rget(&r.row, k).unwrap_or(RsValue::Null)).collect();
                let rc = self.extract(&mut vals, 0, 0, &r.row, 1, r.doc_id);
                r.clear();
                if let Err(e) = rc {
                    cx.set_err(e);
                    st = St::Error;
                    break;
                }
            }
            cx.result_limit = saved;
            if st != St::Eof {
                return st;
            }
            self.yielding = true;
            if self.needs_all {
                self.all = visible_keys(&cx.lookups[self.src_li]);
            }
            cx.total = self.groups.len() as i64;
            cx.skipped = 0;
        }
        if self.pos >= self.groups.len() {
            return St::Eof;
        }
        let gi = self.pos;
        self.pos += 1;
        for (i, &k) in self.dst.iter().enumerate() {
            rset(&mut r.row, k, self.groups[gi].vals[i].clone());
        }
        let g = &mut self.groups[gi];
        for (red, acc) in self.reducers.iter().zip(g.acc.iter_mut()) {
            let v = red.finalize(acc, &self.all);
            rset(&mut r.row, red.dst, v);
        }
        St::Ok
    }
}

/// `ExtractKeyName` with `strictPrefix` (FT.HYBRID): a name needs its `@`, a path its `$`.
fn strict_key_name<'s>(s: &'s [u8], strict: bool, context: &str) -> Result<&'s [u8], QErr> {
    let s = cstr(s);
    match s.first() {
        Some(b'@') => Ok(&s[1..]),
        Some(b'$') => Ok(s),
        _ if strict => Err(QErr::user(
            Code::ParseArgs,
            "Missing prefix: name requires '@' prefix, JSON path require '$' prefix",
            format!(", got: {} in {context}", String::from_utf8_lossy(s)),
        )),
        _ => Ok(s),
    }
}

/// `ReducerOptions`.
struct RedOpts<'o, 'b> {
    name: &'o str,
    ac: Ac<'b>,
    lk: &'o mut Lookup,
    spec: Option<&'o Spec>,
    load_keys: Option<&'o mut Vec<LKey>>,
    /// `RSGlobalConfig.maxAggregateResults`.
    max_results: u64,
    /// `strictPrefix`.
    strict: bool,
}

impl RedOpts<'_, '_> {
    /// `ReducerOpts_GetKey`.
    fn get_key(&mut self) -> Result<KRef, QErr> {
        let Ok(s) = self.ac.get() else {
            return Err(QErr::user(Code::ParseArgs, "Missing arguments", format!(" for {}", self.name)));
        };
        let name = strict_key_name(s, self.strict, self.name)?.to_vec();
        self.resolve(&name)
    }

    /// `ReducerOpts_ResolveKey`.
    fn resolve(&mut self, name: &[u8]) -> Result<KRef, QErr> {
        let missing = || QErr::user(Code::NoPropKey, "Property not loaded nor in pipeline", format!(": `{}`", redis_proto::escape(name)));
        if let Some(i) = self.lk.get_read(self.spec, name, K_HIDDEN) {
            return Ok(KRef::of(&self.lk.keys[i]));
        }
        let Some(load_keys) = self.load_keys.as_deref_mut() else { return Err(missing()) };
        match self.lk.get_load(self.spec, name, name, K_HIDDEN) {
            Some(i) if self.lk.keys[i].flags & K_SCHEMA_SRC != 0 => {
                load_keys.push(copy_key(&self.lk.keys[i]));
                Ok(KRef::of(&self.lk.keys[i]))
            }
            _ => Err(missing()),
        }
    }

    /// `ReducerOpts_EnsureArgsConsumed`.
    fn consumed(&self) -> Result<(), QErr> {
        if self.ac.remaining() > 0 {
            return Err(QErr::unknown_arg(&self.ac, self.name));
        }
        Ok(())
    }
}

/// `RDCR_GetFactory` and the factories: the reducer, or `None` for an unknown name.
fn make_reducer(o: &mut RedOpts<'_, '_>) -> Option<Result<(RKind, Option<KRef>), QErr>> {
    let up = o.name.to_ascii_uppercase();
    let mut r = || -> Result<(RKind, Option<KRef>), QErr> {
        Ok(match up.as_str() {
            "COUNT" => {
                if o.ac.argc() != 0 {
                    return Err(QErr::new(Code::BadAttr, "Count accepts 0 values only"));
                }
                (RKind::Count, None)
            }
            "SUM" | "AVG" => (RKind::Sum { avg: up == "AVG" }, Some(o.get_key()?)),
            "MIN" => (RKind::Min, Some(o.get_key()?)),
            "MAX" => (RKind::Max, Some(o.get_key()?)),
            "TOLIST" => (RKind::ToList, Some(o.get_key()?)),
            "COUNT_DISTINCT" => (RKind::CountDistinct, Some(o.get_key()?)),
            "COUNT_DISTINCTISH" => (RKind::Distinctish, Some(o.get_key()?)),
            "HLL" => (RKind::Hll, Some(o.get_key()?)),
            "HLL_SUM" => (RKind::HllSum, Some(o.get_key()?)),
            "STDDEV" => (RKind::StdDev, Some(o.get_key()?)),
            "QUANTILE" => {
                let k = o.get_key()?;
                let pct = o.ac.get_double(0).map_err(|e| QErr::bad_args(o.name, e))?;
                if !(0.0..=1.0).contains(&pct) {
                    return Err(QErr::new(Code::ParseArgs, "Percentage must be between 0.0 and 1.0"));
                }
                let mut resolution = 500;
                if !o.ac.at_end() {
                    resolution = o.ac.get_u32(0).map_err(|e| QErr::bad_args("<resolution>", e))?;
                    if !(1..=1000).contains(&resolution) {
                        return Err(QErr::new(Code::ParseArgs, "Invalid resolution"));
                    }
                }
                o.consumed()?;
                (RKind::Quantile { pct, resolution: resolution as usize }, Some(k))
            }
            "FIRST_VALUE" => {
                let k = o.get_key()?;
                let mut by = None;
                let mut asc = true;
                if o.ac.advance_if("BY") {
                    by = Some(o.get_key()?);
                    if o.ac.advance_if("ASC") {
                        asc = true;
                    } else if o.ac.advance_if("DESC") {
                        asc = false;
                    }
                }
                o.consumed()?;
                (RKind::FirstValue { by, asc }, Some(k))
            }
            "RANDOM_SAMPLE" => {
                let k = o.get_key()?;
                let n = o.ac.get_u32(0).map_err(|e| QErr::bad_args("<sample size>", e))?;
                if n > 1000 {
                    return Err(QErr::new(Code::ParseArgs, "Sample size too large"));
                }
                (RKind::Sample { len: n as usize }, Some(k))
            }
            "COLLECT" => (RKind::Collect(Box::new(collect::parse(o)?)), None),
            _ => unreachable!(),
        })
    };
    const NAMES: [&str; 15] = [
        "COUNT",
        "SUM",
        "TOLIST",
        "MIN",
        "MAX",
        "AVG",
        "COUNT_DISTINCT",
        "COUNT_DISTINCTISH",
        "QUANTILE",
        "STDDEV",
        "FIRST_VALUE",
        "RANDOM_SAMPLE",
        "HLL",
        "HLL_SUM",
        "COLLECT",
    ];
    if !NAMES.contains(&up.as_str()) {
        return None;
    }
    Some(r())
}

// ---------------------------------------------------------------------------------------------
// The query optimizer's decisions (`QOptimizer_Parse` / `QOptimizer_QueryNodes`)

struct Opt {
    ty: OptType,
    /// `HasScorer`: the optimizer kept a scorer.
    scorer: bool,
}

fn optimizer(req: &areq::Request) -> Opt {
    match &req.optimizer {
        Some(p) if req.flags & OPTIMIZE != 0 => Opt { ty: p.ty, scorer: p.scorer },
        _ => Opt { ty: OptType::Undecided, scorer: false },
    }
}

/// `AGPLN_GetArrangeStep`: the last arrange step after the last group.
fn last_arrange(req: &areq::Request) -> Option<usize> {
    for (i, s) in req.steps.iter().enumerate().rev() {
        match s {
            Step::Group { .. } => return None,
            Step::Arrange(_) => return Some(i),
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Pipeline construction

struct Builder<'r, 'a> {
    req: &'r areq::Request,
    chain: Vec<Box<dyn Proc<'a> + 'a>>,
    opt: Opt,
    safe: bool,
    max_groups: usize,
    /// `cv.lastAstp->sortkeysLK[0]`.
    sortkey: Option<KRef>,
}

impl<'a> Builder<'_, 'a> {
    fn has(&self, f: u64) -> bool {
        self.req.flags & f != 0
    }

    fn push(&mut self, p: impl Proc<'a> + 'a) {
        self.chain.push(Box::new(p));
    }

    /// `getLoaderRP` for keys of lookup `li`.
    fn loader(&mut self, keys: Vec<LKey>) {
        let lk = Lookup { keys, ..Lookup::default() };
        let safe = self.safe;
        self.push(Loader::new(Load::Keys(lk), safe));
    }

    /// `getArrangeRP`: false when it added nothing.
    fn arrange(&mut self, cx: &mut Pcx<'a>, a: Option<&Arrange>, li: usize, is_last: bool) -> Result<bool, QErr> {
        let default = Arrange::default();
        let a = a.unwrap_or(&default);
        let mut max = a.offset.wrapping_add(a.limit);
        if max == 0 {
            max = 10;
        }
        let search = self.has(F_IS_SEARCH);
        let optimized = self.has(OPTIMIZE);
        max = max.min(if search { self.req.max_search_results } else { self.req.max_aggregate_results });
        if self.has(F_NOROWS) || max == 0 {
            self.push(Counter);
            return Ok(true);
        }
        let mut added = false;
        let mut depleter = false;
        let mut pager = false;
        if self.opt.ty != OptType::NoSorter {
            if let Some(keys) = a.sort_keys.as_ref().filter(|k| !k.is_empty()) {
                let spec = if li == 0 { Some(cx.spec) } else { None };
                let mut krefs = Vec::new();
                let mut load = Vec::new();
                for k in keys {
                    let k = cstr(k);
                    let lk = &mut cx.lookups[li];
                    let i = match lk.get_read(spec, k, 0) {
                        Some(i) => i,
                        None => match lk.get_load(spec, k, k, 0) {
                            Some(i) if lk.keys[i].flags & K_SCHEMA_SRC != 0 => {
                                load.push(copy_key(&lk.keys[i]));
                                i
                            }
                            _ => return Err(QErr::user(Code::NoPropKey, "Property", format!(" `{}` not loaded nor in schema", redis_proto::escape(k)))),
                        },
                    };
                    krefs.push(KRef::of(&cx.lookups[li].keys[i]));
                }
                if !load.is_empty() {
                    self.loader(load);
                }
                if is_last {
                    self.sortkey = krefs.first().copied();
                }
                self.push(Sorter::new(max as usize, SortBy::Fields { keys: krefs, asc: a.asc_map }));
                added = true;
            } else if self.has(F_IS_HYBRID) || (search && !optimized) || self.opt.scorer {
                self.push(Sorter::new(max as usize, SortBy::Score));
                added = true;
            }
        }
        if a.offset != 0 || (a.limit != 0 && !added) {
            if self.has(F_HAS_DEPLETER) {
                self.push(Depleter { buf: VecDeque::new(), last: St::Eof, done: false });
                depleter = true;
            }
            self.push(Pager { offset: a.offset as u32, remaining: a.limit as u32, skipping: true });
            added = true;
            pager = true;
        } else if search && optimized && !added {
            self.push(Pager { offset: 0, remaining: max as u32, skipping: true });
            added = true;
            pager = true;
        }
        if self.has(F_HAS_DEPLETER) {
            if !depleter {
                self.push(Depleter { buf: VecDeque::new(), last: St::Eof, done: false });
                added = true;
            }
            if a.is_limited && (!self.has(F_HAS_SORTBY) || a.offset != 0) && !pager {
                self.push(Pager { offset: a.offset as u32, remaining: a.limit as u32, skipping: true });
                added = true;
            }
        }
        Ok(added)
    }

    /// `getGroupRP` / `buildGroupRP`.
    fn group(&mut self, cx: &mut Pcx<'a>, props: &[Bytes], reducers: &[areq::Reducer], li: usize) -> Result<usize, QErr> {
        let gi = cx.lookups.len();
        cx.lookups.push(Lookup::default());
        let can_load = li == 0;
        let spec = if li == 0 { Some(cx.spec) } else { None };
        let mut load_keys: Vec<LKey> = Vec::new();
        let mut src = Vec::new();
        let mut dst = Vec::new();
        for p in props {
            let name = &cstr(p)[1.min(cstr(p).len())..];
            let lk = &mut cx.lookups[li];
            let i = match lk.get_read(spec, name, 0) {
                Some(i) => i,
                None => {
                    let i = if can_load { lk.get_load(spec, name, name, 0) } else { None };
                    match i {
                        Some(i) if lk.keys[i].flags & K_SCHEMA_SRC != 0 => {
                            load_keys.push(copy_key(&lk.keys[i]));
                            i
                        }
                        _ => return Err(QErr::user(Code::NoPropKey, "No such property", format!(" `{}`", redis_proto::escape(name)))),
                    }
                }
            };
            src.push(KRef::of(&cx.lookups[li].keys[i]));
            let Some(d) = cx.lookups[gi].get_write(name, 0) else {
                return Err(QErr::user(Code::DupField, "Property", format!(" `{}` specified more than once", redis_proto::escape(name))));
            };
            dst.push(KRef::of(&cx.lookups[gi].keys[d]));
        }
        let mut reds = Vec::new();
        for pr in reducers {
            let name = String::from_utf8_lossy(cstr(&pr.name)).into_owned();
            let (lks, rest) = cx.lookups.split_at_mut(gi);
            let _ = rest;
            let mut o = RedOpts {
                name: &name,
                ac: Ac::new_sds(&pr.args),
                lk: &mut lks[li],
                spec,
                load_keys: if can_load { Some(&mut load_keys) } else { None },
                max_results: self.req.max_aggregate_results,
                strict: self.req.strict_prefix,
            };
            let (kind, src) = match make_reducer(&mut o) {
                None => return Err(QErr::user(Code::NoReducer, "No such reducer", format!(": {name}"))),
                Some(r) => r?,
            };
            let Some(d) = cx.lookups[gi].get_write(cstr(&pr.alias), 0) else {
                return Err(QErr::user(Code::DupField, "Property", format!(" `{}` specified more than once", redis_proto::escape(cstr(&pr.alias)))));
            };
            reds.push(Reducer { kind, src, dst: KRef::of(&cx.lookups[gi].keys[d]) });
        }
        if !load_keys.is_empty() {
            self.loader(load_keys);
        }
        let max_groups = self.max_groups;
        let needs_all = reds.iter().any(|r| matches!(&r.kind, RKind::Collect(c) if matches!(c.fields, collect::Fields::All)));
        self.push(Grouper {
            src,
            dst,
            reducers: reds,
            groups: Vec::new(),
            index: HashMap::new(),
            max_groups,
            yielding: false,
            pos: 0,
            src_li: li,
            needs_all,
            all: Vec::new(),
        });
        Ok(gi)
    }

    /// APPLY / FILTER.
    fn evaluator(&mut self, cx: &mut Pcx<'a>, raw: &[u8], alias: Option<&[u8]>, li: usize) -> Result<(), QErr> {
        let e = expr::parse(raw).map_err(|e| match e {
            Some(m) => QErr::new(Code::Expr, m),
            None => QErr::code(Code::Expr),
        })?;
        let spec = if li == 0 { Some(cx.spec) } else { None };
        let props = {
            let cell = RefCell::new((&mut cx.lookups[li], Vec::new()));
            eval::lookup_keys(&e, &|p: &[u8]| {
                let mut c = cell.borrow_mut();
                let (lk, out) = &mut *c;
                match lk.get_read(spec, p, 0) {
                    Some(i) => {
                        let k = KRef::of(&lk.keys[i]);
                        out.push((p.to_vec(), k));
                        true
                    }
                    None => false,
                }
            })?;
            cell.into_inner().1
        };
        let out = match alias {
            Some(a) => {
                let a = cstr(a);
                let lk = &mut cx.lookups[li];
                let d = lk.get_write(a, fq::K_OVERRIDE).expect("an override");
                lk.keys[d].flags |= K_AGG_REF_ARR | if yields_str_ref(&e) { K_AGG_REF_STR } else { 0 };
                Some(KRef::of(&lk.keys[d]))
            }
            None => None,
        };
        self.push(Evaluator { expr: e, props, out });
        Ok(())
    }

    /// `buildOutputPipeline` (FT.SEARCH): RETURN's keys or the whole document loaded, then the
    /// highlighter.
    fn output(&mut self, cx: &mut Pcx<'a>) -> Result<(), QErr> {
        let spec = cx.spec;
        let explicit = self.req.out_fields.explicit_return;
        let mut keys = Vec::new();
        if explicit {
            let lf = fq::K_EXPLICIT_RETURN | if cx.force { K_FORCE_LOAD } else { 0 };
            for rf in &self.req.out_fields.fields {
                if let Some(i) = cx.lookups[0].get_load(Some(spec), cstr(&rf.name), cstr(&rf.path), lf) {
                    keys.push(copy_key(&cx.lookups[0].keys[i]));
                }
            }
        }
        if !keys.is_empty() {
            self.loader(keys);
        } else if !explicit {
            cx.lookups[0].all_loaded = !spec.is_json();
            let safe = self.safe;
            self.push(Loader::new(Load::All(0), safe));
        }
        if self.has(F_SEND_HIGHLIGHT) {
            use crate::search::highlight;
            let li = cx.lookups.len() - 1;
            let fl = &self.req.out_fields;
            let def = &fl.default_field;
            let mut jobs = Vec::new();
            for ff in &fl.fields {
                if def.mode == 0 && ff.mode == 0 {
                    continue;
                }
                let name = cstr(&ff.name);
                let lk = &mut cx.lookups[li];
                match lk.get_read(Some(spec), name, 0) {
                    None => return Err(QErr::user(Code::NoPropKey, "No such property", format!(" `{}`", redis_proto::escape(name)))),
                    Some(k) if lk.keys[k].flags & K_SCHEMA_SRC == 0 => {
                        return Err(QErr::user(Code::Inval, "Property", format!(" `{}` is not in schema", redis_proto::escape(name))));
                    }
                    Some(k) => {
                        let ft_id = spec.field(name).filter(|f| f.is(crate::search::spec::T_TEXT)).and_then(|f| f.ft_id);
                        jobs.push((KRef::of(&lk.keys[k]), highlight::normalize(Some(ff), def), ft_id));
                    }
                }
            }
            let chinese = matches!(self.req.opts.language, areq::Lang::Is(l) if crate::search::spec::LANGUAGES.get(l) == Some(&"chinese"));
            let options = if chinese { highlight::TOKLEN_EXACT } else { 0 };
            let jobs = (!fl.fields.is_empty() || def.mode == 0).then_some(jobs);
            self.push(Highlighter { jobs, def: highlight::normalize(None, def), li, options });
        }
        Ok(())
    }

    /// `processLoadStep`.
    fn load(&mut self, cx: &mut Pcx<'a>, args: &[Bytes], all: bool) -> Result<(), QErr> {
        let spec = cx.spec;
        let flags = if cx.force { K_FORCE_LOAD } else { 0 };
        let mut ac = Ac::new_sds(args);
        let mut keys = Vec::new();
        while let Some(s) = ac.take() {
            let path = strict_key_name(s, self.req.strict_prefix, "LOAD")?;
            let name = if ac.advance_if("AS") {
                match ac.get() {
                    Err(_) => return Err(QErr::new(Code::ParseArgs, "LOAD path AS name - must be accompanied with NAME")),
                    Ok(n) if c_eq(n, "AS") => return Err(QErr::new(Code::ParseArgs, "Alias for LOAD cannot be `AS`")),
                    Ok(n) => cstr(n),
                }
            } else {
                path
            };
            if let Some(i) = cx.lookups[0].get_load(Some(spec), name, path, flags)
                && keys.len() < args.len()
            {
                keys.push(copy_key(&cx.lookups[0].keys[i]));
            }
        }
        if !keys.is_empty() {
            self.loader(keys);
        } else if all {
            cx.lookups[0].all_loaded = !spec.is_json();
            let safe = self.safe;
            self.push(Loader::new(Load::All(0), safe));
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn build<'r, 'a>(req: &'r areq::Request, cx: &mut Pcx<'a>, it: BoxIt<'a>, ix: &'a Index, now: i64, safe: bool, args: scorers::Args<'a>, metrics: &[Vec<u8>]) -> Result<Builder<'r, 'a>, QErr> {
    let cfg = &cx.eng.cfg;
    let opt = optimizer(req);
    let max_groups = cfg.num("search-max-aggregate-groups").max(1) as usize;
    let mut b = Builder { req, chain: Vec::new(), opt, safe, max_groups, sortkey: None };
    cx.lookups.push(Lookup::default());
    let knn = matches!(req.root.as_ref().map(|n| &n.kind), Some(qast::Kind::Vector(vq)) if vq.knn);
    b.push(QueryIt { it, ix, now, last: 0, resume: None, knn, rest: None, base: None });
    // `getAdditionalMetricsRP`
    if !metrics.is_empty() {
        let lk = &mut cx.lookups[0];
        let keys = fq::metric_keys(lk, cx.spec, metrics)?;
        let keys = keys.into_iter().map(|i| KRef::of(&lk.keys[i])).collect();
        b.push(MetricsRp { keys });
    }

    // `Pipeline_BuildQueryPart`: a scorer when scores are asked for, or when a search returning
    // rows sorts by them.
    let search_rows = b.has(F_IS_SEARCH | F_IS_HYBRID_SEARCH) && !b.has(F_NOROWS);
    let scoring = if b.has(OPTIMIZE) { b.opt.scorer } else { !req.has_query_sortby() };
    if b.has(F_SEND_SCORES | F_SEND_SCORES_AS_FIELD) || (search_rows && scoring) {
        let key = if b.has(F_SEND_SCORES_AS_FIELD) {
            let lk = &mut cx.lookups[0];
            match &req.score_alias {
                // FT.HYBRID's YIELD_SCORE_AS.
                Some(a) => match lk.get_write(cstr(a), 0) {
                    Some(i) => Some(KRef::of(&lk.keys[i])),
                    None => return Err(QErr::user(Code::DupField, "Could not create score alias, name already exists in query", qast::lossy(a))),
                },
                None => lk.get_write(b"__score", fq::K_OVERRIDE).map(|i| KRef::of(&lk.keys[i])),
            }
        } else {
            None
        };
        let explicit = req.opts.scorer.as_deref().map(|s| cstr(s).to_vec());
        let name = explicit.clone().unwrap_or_else(|| {
            let s = cfg.text("search-default-scorer:value");
            if s.is_empty() { b"BM25STD".to_vec() } else { s.into_bytes() }
        });
        let scorer = Scorer::find(&name).unwrap_or(Scorer::Bm25Std);
        b.push(ScoreRp { scorer, args, explain: b.has(F_SEND_SCOREEXPLAIN), key });
        if explicit.as_deref() == Some(b"BM25STD.NORM") {
            b.push(MaxNorm { pool: Vec::new(), max: 0.0, key, yielding: false, timed_out: false });
        }
    }

    b.aggregation(cx)?;
    Ok(b)
}

impl<'a> Builder<'_, 'a> {
    /// `Pipeline_BuildAggregationPart`.
    fn aggregation(&mut self, cx: &mut Pcx<'a>) -> Result<(), QErr> {
    let b = self;
    let req = b.req;
    let last = last_arrange(req);
    let mut li = 0;
    let mut has_arrange = false;
    for (si, s) in req.steps.iter().enumerate() {
        match s {
            Step::Group { properties, reducers } => li = b.group(cx, properties, reducers, li)?,
            Step::Arrange(a) => {
                if !b.arrange(cx, Some(a), li, last == Some(si))? {
                    return Err(QErr::code(Code::Generic));
                }
                has_arrange = true;
            }
            Step::Apply { expr, alias } => b.evaluator(cx, expr, Some(alias), li)?,
            Step::Filter { expr } => b.evaluator(cx, expr, None, li)?,
            Step::Load { args, all } => {
                if li != 0 {
                    return Err(QErr::new(Code::Inval, "LOAD cannot be applied after projectors or reducers"));
                }
                b.load(cx, args, *all)?;
            }
            Step::Normalize { field, alias } => {
                let fs = cx.spec.field(cstr(field)).filter(|f| f.is(crate::search::spec::T_VECTOR));
                let Some(metric) = fs.and_then(|f| f.vector.as_ref()).map(|v| v.metric) else {
                    return Err(QErr::new(Code::Syntax, "Invalid vector field for normalization"));
                };
                let spec = if li == 0 { Some(cx.spec) } else { None };
                let lk = &mut cx.lookups[li];
                let key = lk.get_read(spec, cstr(alias), 0).map(|i| KRef::of(&lk.keys[i]));
                b.push(hybrid::VecNorm { metric, key });
            }
        }
    }
    if !has_arrange && (b.has(F_IS_SEARCH | F_IS_HYBRID_SEARCH) || b.has(F_HAS_DEPLETER)) && !b.arrange(cx, None, li, false)? {
        return Err(QErr::code(Code::Generic));
    }
    if b.has(F_IS_SEARCH) && !b.has(F_SEND_NOFIELDS) {
        b.output(cx)?;
    }
    Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Execution and the reply (`sendChunk`, `serializeResult`)

/// A cursor's request and what its pipeline keeps between chunks (the module keeps the
/// pipeline itself; here each chunk rebuilds it and restores this).
struct CursorExec {
    req: areq::Request,
    st: CursorSt,
}

#[derive(Default)]
struct CursorSt {
    /// The first chunk ran.
    started: bool,
    /// The database the cursor was opened in.
    db: usize,
    /// The lookups, with the keys loading added as documents came.
    lookups: Vec<Lookup>,
    /// Each processor's state, by position (in the chain the replay starts once `pending_at`).
    saved: Vec<Option<Saved>>,
    /// The last accumulating processor's position once it drained the query: later chunks
    /// replay what it held, and keep the metadata of the documents those results reference.
    pending_at: Option<usize>,
    dmds: Vec<Dmd>,
    min_score: f64,
    /// WITHCOUNT's total, kept across chunks.
    total: i64,
    /// The doc table's max id when the query's iterators were first built.
    top: u64,
    /// The scorer's average document length, as when the pipeline was built.
    avg_doc_len: f64,
    /// `cursorConfig.chunkSize`.
    chunk: u32,
    /// The query's metric requests (yielded distances).
    metrics: Vec<Vec<u8>>,
    /// The query reader's timeout checks and `TIMEOUT_AFTER_N`'s count.
    tmo: Tmo,
    /// A profiled cursor's counters.
    prof: Option<Box<ProfCur>>,
    /// The readers and expansions the first chunk's iterators were built from.
    memo: Memo,
    /// The deadline the first chunk's iterators were built with, and the NOT iterators a
    /// timeout ended (the module's kept iterators keep both).
    iter_deadline: Option<Instant>,
    not_eofs: Vec<bool>,
}

/// `RSGlobalConfig.cursorReadSize`.
const CURSOR_READ_SIZE: u32 = 1000;

/// What a profiled run reports (`Profile_PrintCommon`'s iterators and processors).
pub(super) struct ProfOut {
    /// LIMITED: big unions' children collapse into their count.
    pub limited: bool,
    /// Started when the command did.
    pub clock: Instant,
    /// When the pipeline was built.
    pub build_ns: u64,
    pub iters: Option<Reply>,
    pub rps: Vec<Reply>,
    pub expansions: bool,
    /// The query timed out.
    pub timed_out: bool,
    /// The processors run (their types, whether they hold the GIL) and their counters.
    names: Vec<(String, bool)>,
    counts: Vec<(u64, u64)>,
    /// The first processor's position in the chain as first built (a cursor's later reads
    /// replay what a drained processor held).
    base: usize,
}

impl ProfOut {
    pub(super) fn new(limited: bool, clock: Instant) -> ProfOut {
        ProfOut { limited, clock, build_ns: 0, iters: None, rps: Vec::new(), expansions: false, timed_out: false, names: Vec::new(), counts: Vec::new(), base: 0 }
    }
}

/// `QueryWarning_Strwarning(QUERY_WARNING_CODE_TIMED_OUT)`.
pub(super) const TIMEOUT_WARNING: &str = "Timeout limit was reached";

/// `QUERY_WARNING_CODE_MAX_TIMEOUT_CAPPED`.
pub(super) const CAPPED_WARNING: &str =
    "Query TIMEOUT exceeded the configured maximum (search-_max-foreground-timeout-limit) while search-workers is disabled; effective timeout was capped";

/// `QITR_ReportedTotal`: an unsigned 32-bit count.
fn reported(total: i64) -> i64 {
    total as u32 as i64
}

/// One chunk (`sendChunk`): its reply and whether the request is done (the cursor's id is 0).
/// `limit` is the chunk's result limit. A cursor's state carries the pipeline from chunk to
/// chunk; `gate` is the error a cursor's reservation fails with, once the pipeline is built,
/// and `started` tells whether the chunk began.
#[allow(clippy::too_many_arguments)]
fn run(
    eng: &Engine,
    ix: &Index,
    req: &areq::Request,
    db: usize,
    resp3: bool,
    background: bool,
    limit: u32,
    mut cur: Option<&mut CursorSt>,
    gate: Option<QErr>,
    started: &mut bool,
    mut prof: Option<&mut ProfOut>,
) -> Result<(Reply, bool), Error> {
    let started_at = Instant::now();
    let cfg = &eng.cfg;
    let spec = &ix.spec;
    let flags = req.flags;
    let now = crate::util::now_ms();
    let reading = cur.as_ref().is_some_and(|s| s.started);
    // FT.SEARCH's own run counts it (this one only profiles it), but for a cursor's.
    let count = flags & F_IS_SEARCH == 0 || cur.is_some();
    let plan_err = |e: Error| if count { count_error(eng, e, req.plan_errors) } else { e };
    // `TotalGlobalStats_CountQuery`'s flags: a cursor's reads are no FT.AGGREGATE's.
    let counted = Counted { internal: req.internal, cursor: flags & F_IS_CURSOR != 0, aggregate: flags & F_IS_AGGREGATE != 0 && !reading };
    let pending = cur.as_ref().and_then(|s| s.pending_at);
    let top = match cur.as_ref() {
        Some(s) if s.started => s.top,
        _ => ix.docs.max_doc_id,
    };
    let mut ft_to_field = Vec::new();
    for (i, f) in spec.fields.iter().enumerate() {
        if let Some(id) = f.ft_id {
            if ft_to_field.len() <= id {
                ft_to_field.resize(id + 1, 0);
            }
            ft_to_field[id] = i as u16;
        }
    }
    // The documents a replayed result references: gone from the table, they read as deleted.
    let mut dmds: Vec<Dmd> = cur.as_mut().map(|s| std::mem::take(&mut s.dmds)).unwrap_or_default();
    for d in &mut dmds {
        if ix.docs.get(d.id).is_none() {
            d.flags |= DOC_DELETED;
        }
    }
    let q = Qcx::new(&ix.docs, now, ft_to_field);
    let mut ecx = EvalCx {
        ix,
        q: &q,
        req_mask: req.opts.field_mask,
        slop: req.opts.slop as i64,
        in_order: req.opts.flags & qast::S_IN_ORDER != 0,
        dialect: req.dialect,
        max_expansions: cfg.num("search-max-prefix-expansions").max(0) as usize,
        min_prefix: cfg.num("search-min-prefix").max(0) as usize,
        min_heap: cfg.num("search-union-iterator-heap").max(0) as usize,
        prioritize: cfg.flag("search-_prioritize-intersect-union-children"),
        in_not: false,
        expansions_warning: false,
        top: reading.then_some(top),
        token_id: std::cell::Cell::new(0),
        can_skip_rich: req.opts.flags & qast::S_CAN_SKIP_RICH_RESULTS != 0,
        metric_requests: Vec::new(),
        hybrid: false,
        memo: None,
    };
    // A cursor's later chunks rebuild its iterators from what the first chunk opened.
    if let Some(s) = cur.as_ref() {
        ecx.memo = Some(RefCell::new(if s.started { s.memo.replaying() } else { Memo::default() }));
    }
    // The cursor's policy and clock are its request's (`SearchCtx_UpdateTime` before the
    // iterators are built); a read restarts the clock.
    let policy = Policy::of(req.on_timeout);
    let fresh = (!reading).then(|| Tmo::new(req.timeout, policy, background, req.timeout_after));
    match (fresh.as_ref(), cur.as_ref()) {
        (Some(t), _) => q.clock.deadline.set(t.iter_deadline()),
        (None, Some(s)) => {
            q.clock.deadline.set(s.iter_deadline);
            *q.clock.eofs.borrow_mut() = s.not_eofs.clone();
        }
        (None, None) => {}
    }
    let root = req.root.as_ref().expect("a parsed query");
    let mut it: BoxIt<'_> = if pending.is_some() { Box::new(Empty) } else { ecx.iterate(root).map_err(|e| plan_err(e.into()))? };
    if let Some(s) = cur.as_deref_mut()
        && !s.started
        && let Some(m) = ecx.memo.take()
    {
        s.memo = m.into_inner();
    }
    // `QOptimizer_Iterators`.
    if let Some(plan) = req.optimizer.as_ref().filter(|_| req.flags & OPTIMIZE != 0 && pending.is_none()) {
        it = qopt::apply(plan, root, it, ix, &q, ecx.min_heap);
    }
    // `prepareExecutionPlan`: under ON_TIMEOUT FAIL (FT.PROFILE aside) a query whose
    // iterators took up its time fails before its pipeline is built.
    if policy == Policy::Fail
        && prof.is_none()
        && fresh.as_ref().is_some_and(Tmo::plan_timed_out)
    {
        return Err(plan_err(QErr::code(Code::TimedOut).into()));
    }
    // `Profile_AddIters`.
    let it = if prof.is_some() { iters::profiled(it) } else { it };
    // The warning is the first chunk's (later ones start with a fresh error).
    let warn_expansions = ecx.expansions_warning && !reading;

    let avg = match cur.as_ref() {
        Some(s) if s.started => s.avg_doc_len,
        _ if ix.stats.num_documents != 0 => ix.stats.total_docs_len as f64 / ix.stats.num_documents as f64,
        _ => 0.0,
    };
    let sargs = scorers::Args { avg_doc_len: avg, tanh_factor: req.bm25std_tanh as i64, payload: req.payload.as_deref() };
    let tmo = match (cur.as_ref(), fresh) {
        (_, Some(t)) => t,
        (Some(s), None) => {
            let mut t = s.tmo;
            t.refresh(req.timeout);
            t
        }
        (None, None) => unreachable!("a fresh run"),
    };
    let mut cx = Pcx {
        eng,
        db,
        spec,
        lookups: Vec::new(),
        total: 0,
        skipped: 0,
        result_limit: 0,
        min_score: 0.0,
        err: None,
        force: spec.is_json() && req.dialect < 3,
        dialect: req.dialect,
        prof: None,
        policy,
        tmo,
        iclock: Some(&q.clock),
    };
    let mut metric_requests = std::mem::take(&mut ecx.metric_requests);
    // A replaying cursor builds no query iterator: its yielded distances keep their keys.
    if let Some(s) = cur.as_mut() {
        if pending.is_some() {
            metric_requests = s.metrics.clone();
        } else {
            s.metrics = metric_requests.clone();
        }
    }
    let Builder { mut chain, sortkey, .. } = build(req, &mut cx, it, ix, now, background, sargs, &metric_requests).map_err(|e| plan_err(e.into()))?;
    // `parseAndCompileDebug`, once the pipeline is built.
    if let Some(e) = &req.debug_err {
        return Err(plan_err(e.clone().into()));
    }
    if let Some(p) = prof.as_deref_mut() {
        cx.prof = Some(vec![(0, 0); chain.len()]);
        p.build_ns = p.clock.elapsed().as_nanos() as u64;
    }
    if let Some(e) = gate {
        return Err(plan_err(e.into()));
    }
    *started = true;
    let optimized = flags & OPTIMIZE != 0;
    if let Some(s) = cur.as_deref_mut()
        && s.started
    {
        cx.lookups = std::mem::take(&mut s.lookups);
        cx.min_score = s.min_score;
        if flags & F_HAS_WITHCOUNT != 0 {
            cx.total = s.total;
        }
        if let Some(at) = s.pending_at {
            chain.drain(..=at);
            chain.insert(0, Box::new(Replay { dmds: &dmds, items: VecDeque::new(), tail: St::Eof }));
        }
        for (p, saved) in chain.iter_mut().zip(std::mem::take(&mut s.saved)) {
            if let Some(saved) = saved {
                p.restore(saved);
            }
        }
    }

    cx.result_limit = limit;
    // `startPipelineCommon`: anything but ON_TIMEOUT RETURN (or ON_OOM FAIL) buffers the
    // chunk before replying.
    let collect = policy != Policy::Return || cfg.num("search-on-oom") == 1;
    let strict = policy == Policy::Strict;
    let mut rows: Vec<Res<'_>> = Vec::new();
    let mut r = Res::new();
    let mut rc;
    let mut collected = None;
    if collect {
        // `AggregateResults`.
        let mut v = Vec::new();
        rc = St::Eof;
        while cx.result_limit > 0 {
            let mut x = Res::new();
            rc = pull(&mut chain, &mut cx, &mut x);
            if rc != St::Ok {
                break;
            }
            cx.result_limit -= 1;
            v.push(x);
        }
        collected = Some(v);
        if cx.tmo.checks && cx.tmo.passed() {
            rc = St::TimedOut;
        }
    } else {
        rc = pull(&mut chain, &mut cx, &mut r);
    }
    if let Some(s) = cur.as_deref_mut() {
        s.tmo = cx.tmo;
    }
    // `handleSendChunkError`: an error, or a timeout under ON_TIMEOUT FAIL (FT.PROFILE warns
    // instead); either ends a cursor.
    if let Some(e) = &cx.err {
        let e: Error = e.clone().into();
        return Err(if count { count_error(eng, e, Some(!req.internal)) } else { e });
    }
    // `QueryTimeoutFailCallback` / `CursorReadTimeoutFailCallback`: on the workers, ON_TIMEOUT
    // FAIL leaves the clock to the blocked client's timer, which fires with the timeout
    // error once the timeout passed before the chunk was replied.
    if background && policy == Policy::Fail && req.timeout > 0 && prof.is_none() && cx.tmo.passed() {
        if count {
            count_timeout_error(eng, req, counted, started_at.elapsed(), true);
        }
        return Err(QErr::code(Code::TimedOut).into());
    }
    if rc == St::TimedOut && policy == Policy::Fail && prof.is_none() {
        if count {
            count_timeout_error(eng, req, counted, started_at.elapsed(), false);
        }
        return Err(QErr::code(Code::TimedOut).into());
    }
    if optimized {
        // `QOptimizer_UpdateTotalResults`.
        let (limit, offset) = match last_arrange(req).map(|i| &req.steps[i]) {
            Some(Step::Arrange(a)) if a.is_limited => (a.limit as i64, a.offset as i64),
            _ => (10, 0),
        };
        let reported = cx.total - cx.skipped;
        cx.skipped = 0;
        cx.total = (reported - offset).max(0).min(limit);
    }
    let first_total = cx.total - cx.skipped;
    let buffered_strict = collected.is_some() && strict;
    if flags & F_NOROWS == 0 && (buffered_strict || rc == St::Ok || rc == St::Eof) {
        if let Some(v) = collected.take() {
            rows = v;
        } else if cx.result_limit > 0 && rc == St::Ok {
            rows.push(std::mem::replace(&mut r, Res::new()));
            loop {
                cx.result_limit -= 1;
                if cx.result_limit == 0 {
                    break;
                }
                let mut x = Res::new();
                rc = pull(&mut chain, &mut cx, &mut x);
                if rc != St::Ok {
                    break;
                }
                rows.push(x);
            }
        }
    }
    // `shouldSetCursorDone`: a timeout under ON_TIMEOUT RETURN leaves the cursor for the next
    // read.
    let done = match rc {
        St::Ok => false,
        St::TimedOut => policy != Policy::Return,
        _ => true,
    };
    if let Some(s) = cur.as_deref_mut() {
        s.tmo = cx.tmo;
    }
    if count {
        // A pipeline's error after the chunk's first results (RESP3's warning) leaves the
        // query uncounted.
        count_chunk(eng, req, cx.err.is_none().then_some(counted), started_at.elapsed(), rc == St::TimedOut, warn_expansions);
    }
    if let Some(p) = prof {
        p.expansions = warn_expansions;
        p.timed_out = rc == St::TimedOut;
        let pc = PCtx { limited: p.limited, clock: true };
        p.iters = chain.first().and_then(|q| q.profile_iters(&pc)).map(Reply::Map);
        p.counts = cx.prof.take().unwrap_or_default();
        p.names = chain.iter().map(|rp| (rp.name(), rp.gil())).collect();
        p.base = pending.unwrap_or(0);
        p.rps = rps_reply(&p.names, &p.counts);
    }

    // The reply.
    let li = cx.lookups.len() - 1;
    let mut warnings = Vec::new();
    if resp3 {
        // `_replyWarnings`.
        if rc == St::TimedOut {
            warnings.push(Reply::Status(TIMEOUT_WARNING.into()));
        } else if rc == St::Error
            && let Some(e) = &cx.err
        {
            warnings.push(Reply::Status(e.text()));
        }
        if warn_expansions {
            warnings.push(Reply::Status("Max prefix expansions limit was reached".into()));
        }
        if req.capped {
            warnings.push(Reply::Status(CAPPED_WARNING.into()));
        }
    }
    let totals = (reported(first_total), reported(cx.total - cx.skipped));
    let reply = chunk_reply(req, &mut cx, li, sortkey, &rows, resp3, totals, warnings);
    drop(rows);

    // A cursor keeps the pipeline's state for its next chunk (`finishSendChunk` resets the
    // totals, WITHCOUNT's aside).
    let mut held = None;
    if let Some(s) = cur.as_deref_mut()
        && !done
    {
        s.total = if flags & F_HAS_WITHCOUNT != 0 { cx.total - cx.skipped } else { 0 };
        let mut saved = Vec::with_capacity(chain.len());
        // The last processor that drained its upstream (beyond a replay already in place).
        match chain.iter().rposition(|p| p.drained()).filter(|&at| s.pending_at.is_none() || at > 0) {
            Some(at) => {
                let mut kept = Vec::new();
                let mut items = VecDeque::new();
                let (up, rest) = chain.split_at_mut(at);
                let tail = loop {
                    let mut x = Res::new();
                    let st = rest[0].next(up, &mut cx, &mut x);
                    if st != St::Ok {
                        break st;
                    }
                    items.push_back(Pend::of(x, &mut kept));
                };
                saved.push(Some(Saved::Replay(items, tail)));
                saved.extend(rest[1..].iter_mut().map(|p| p.save()));
                s.pending_at = Some(s.pending_at.map_or(at, |pa| pa + at));
                held = Some(kept);
            }
            None => saved.extend(chain.iter_mut().map(|p| p.save())),
        }
        s.saved = saved;
        s.iter_deadline = q.clock.deadline.get();
        s.not_eofs = q.clock.eofs.borrow().clone();
        s.lookups = std::mem::take(&mut cx.lookups);
        s.min_score = cx.min_score;
        s.top = top;
        s.avg_doc_len = avg;
        s.started = true;
    }
    drop((chain, r, collected));
    if let Some(s) = cur {
        s.dmds = held.unwrap_or(dmds);
    }
    Ok((reply, done))
}

/// `sendChunk`'s reply: RESP2's total (`totals.0`) and rows, flat; RESP3's map with the
/// total (`totals.1`) and the warnings.
#[allow(clippy::too_many_arguments)]
fn chunk_reply(req: &areq::Request, cx: &mut Pcx<'_>, li: usize, sortkey: Option<KRef>, rows: &[Res<'_>], resp3: bool, totals: (i64, i64), warnings: Vec<Reply>) -> Reply {
    let mut out_rows = Vec::with_capacity(rows.len());
    for row in rows {
        out_rows.push(serialize(req, cx, li, sortkey, row, resp3));
    }
    if resp3 {
        let results: Vec<Reply> = out_rows
            .into_iter()
            .map(|parts| {
                let mut m: Vec<(Reply, Reply)> = parts.into_iter().map(|(k, v)| (Reply::Status(k.into()), v)).collect();
                m.push((Reply::Status("values".into()), Reply::Array(Vec::new())));
                Reply::Map(m)
            })
            .collect();
        return Reply::Map(vec![
            (Reply::Status("attributes".into()), Reply::Array(Vec::new())),
            (Reply::Status("format".into()), Reply::Status(if req.flags & FORMAT_EXPAND != 0 { "EXPAND" } else { "STRING" }.into())),
            (Reply::Status("results".into()), Reply::Array(results)),
            (Reply::Status("total_results".into()), Reply::Int(totals.1)),
            (Reply::Status("warning".into()), Reply::Array(warnings)),
        ]);
    }
    let mut out = vec![Reply::Int(totals.0)];
    for parts in out_rows {
        for (k, v) in parts {
            match (k, v) {
                ("extra_attributes", Reply::Map(m)) => out.push(Reply::Array(m.into_iter().flat_map(|(a, b)| [a, b]).collect())),
                ("required_fields", Reply::Map(m)) => out.extend(m.into_iter().map(|(_, b)| b)),
                (_, v) => out.push(v),
            }
        }
    }
    Reply::Array(out)
}

/// `getReplyKey`: the sorting vector's slot when the key has one, else the row.
fn reply_key(k: KRef, r: &Res<'_>) -> Option<RsValue> {
    if k.flags & K_SV_SRC != 0 && r.row.sv.len() > k.svidx { fq::sort_val(&r.row.sv[k.svidx]) } else { rget(&r.row, k) }
}

/// `serializeResult`: a row's parts by their RESP3 names.
fn serialize(req: &areq::Request, cx: &mut Pcx<'_>, li: usize, sortkey: Option<KRef>, r: &Res<'_>, resp3: bool) -> Vec<(&'static str, Reply)> {
    let flags = req.flags;
    let mut parts: Vec<(&'static str, Reply)> = Vec::new();
    if flags & F_IS_SEARCH != 0 {
        // FT.SEARCH's document key (a cursor's rows always reference their metadata).
        parts.push(("id", r.dmd.map_or(Reply::Nil, |d| Reply::Bulk(d.key.clone()))));
    }
    if flags & F_SEND_SCORES != 0 {
        let s = if flags & F_SEND_SCOREEXPLAIN != 0 {
            Reply::Array(vec![Reply::Double(r.score), r.expl.as_ref().map_or(Reply::Nil, fq::explain_reply)])
        } else {
            Reply::Double(r.score)
        };
        parts.push(("score", s));
    }
    if flags & F_SENDRAWIDS != 0 {
        parts.push(("id", Reply::Int(r.doc_id as i64)));
    }
    if flags & F_SEND_PAYLOADS != 0 {
        let p = match r.dmd {
            Some(d) if d.flags & DOC_HAS_PAYLOAD != 0 => d.payload.clone().map_or(Reply::Nil, Reply::Bulk),
            _ => Reply::Nil,
        };
        parts.push(("payload", p));
    }
    if flags & F_SEND_SORTKEYS != 0 {
        parts.push(("sortkey", fq::sortkey_reply(sortkey.and_then(|k| reply_key(k, r)))));
    }
    if flags & F_REQUIRED_FIELDS != 0
        && let Some(fields) = &req.required_fields
    {
        let start = if flags & F_SEND_SORTKEYS != 0 { 1 } else { 0 };
        let spec = if li == 0 { Some(cx.spec) } else { None };
        let mut m = Vec::new();
        for f in fields.iter().skip(start) {
            let name = cstr(f);
            let lk = &mut cx.lookups[li];
            let k = lk.get_read(spec, name, 0).map(|i| KRef::of(&lk.keys[i]));
            let mut v = k.and_then(|k| reply_key(k, r));
            if let Some(RsValue::Trio(t)) = &v {
                v = Some(t[0].clone());
            }
            if let (Some(k), Some(x)) = (k, &v)
                && k.flags & K_NUMERIC != 0
                && !matches!(x, RsValue::Number(_) | RsValue::Null)
            {
                v = Some(RsValue::Number(x.to_number().unwrap_or(0.0)));
            }
            m.push((Reply::bulk(name), fq::sortkey_reply(v)));
        }
        if !resp3 || !m.is_empty() {
            parts.push(("required_fields", Reply::Map(m)));
        }
    }
    if flags & F_SEND_NOFIELDS == 0 {
        let rule = &cx.spec.rule;
        let special = |n: &[u8]| [&rule.lang_field, &rule.score_field, &rule.payload_field].iter().any(|f| f.as_deref().is_some_and(|f| f.as_bytes() == n));
        let typed = flags & F_TYPED != 0;
        let expand = flags & FORMAT_EXPAND != 0;
        let explicit = req.out_fields.explicit_return;
        let mut fields = Vec::new();
        for k in &cx.lookups[li].keys {
            if k.dead || k.flags & K_HIDDEN != 0 || (explicit && k.flags & fq::K_EXPLICIT_RETURN == 0) || special(&k.name) {
                continue;
            }
            let Some(mut v) = rget(&r.row, KRef::of(k)) else { continue };
            if let RsValue::Trio(t) = &v {
                v = if expand {
                    t[2].clone()
                } else if req.dialect >= 3 {
                    t[1].clone()
                } else {
                    t[0].clone()
                };
            }
            fields.push((Reply::Bulk(k.name.clone()), v.reply(resp3, typed, expand)));
        }
        parts.push(("extra_attributes", Reply::Map(fields)));
    }
    parts
}

// ---------------------------------------------------------------------------------------------
// COLLECT

mod collect {
    //! The COLLECT reducer (`reducers/collect.c` parsing, the remote collect reducer): per
    //! group, the rows' chosen fields (or every visible one), optionally distinct, ranked by
    //! in-group sort keys and paged.

    use super::*;

    const MAX_SORT_KEYS: usize = 8;
    const MAX_SORT_TOKENS: u32 = (MAX_SORT_KEYS * 2) as u32;
    const MAX_FIELD_ARGS: i64 = 1024 + 1;
    /// `DEFAULT_LIMIT`: the ranked storage's count without LIMIT.
    const DEFAULT_LIMIT: usize = 10;

    pub(super) enum Fields {
        Specific(Vec<(Bytes, KRef)>),
        /// `FIELDS *`: the source lookup's visible keys, read as rows arrive.
        All,
    }

    pub(super) struct Spec {
        pub fields: Fields,
        sort: Vec<KRef>,
        asc_map: u64,
        offset: usize,
        count: usize,
        ranked: bool,
        distinct: bool,
    }

    /// A projected row: values by destination slot, trailing empties trimmed for equality.
    #[derive(Clone)]
    struct Proj(Vec<Option<RsValue>>);

    impl Proj {
        fn effective(&self) -> &[Option<RsValue>] {
            let end = self.0.iter().rposition(Option::is_some).map_or(0, |i| i + 1);
            &self.0[..end]
        }

        fn hash(&self) -> u64 {
            let mut h = DefaultHasher::new();
            for v in self.effective() {
                match v {
                    None => 0u8.hash(&mut h),
                    Some(v) => {
                        1u8.hash(&mut h);
                        hash_into(v, &mut h);
                    }
                }
            }
            h.finish()
        }

        fn same(&self, o: &Proj) -> bool {
            let (a, b) = (self.effective(), o.effective());
            a.len() == b.len()
                && a.iter().zip(b).all(|(x, y)| match (x, y) {
                    (None, None) => true,
                    (Some(p), Some(q)) => p.equal(q),
                    _ => false,
                })
        }
    }

    /// A ranked candidate: greater ranks better.
    struct Entry {
        vals: Vec<Option<RsValue>>,
        doc: u64,
        row: Proj,
    }

    /// `cmp_ranking`: the sort keys (`cmp_fields`, a missing value ranks worst either way),
    /// then the lower doc id.
    fn rank(a: &Entry, b_vals: &[Option<RsValue>], b_doc: u64, asc_map: u64) -> Ordering {
        for (i, (x, y)) in a.vals.iter().zip(b_vals).enumerate() {
            let o = match (x, y) {
                (Some(x), Some(y)) => match x.cmp_with(y, None) {
                    Ordering::Equal => continue,
                    o if asc_map & (1 << i) != 0 => o.reverse(),
                    o => o,
                },
                (Some(_), None) => Ordering::Greater,
                (None, Some(_)) => Ordering::Less,
                (None, None) => continue,
            };
            return o;
        }
        a.doc.cmp(&b_doc).reverse()
    }

    pub(super) struct Acc {
        entries: Vec<Entry>,
        buckets: HashMap<u64, Vec<usize>>,
    }

    /// `strtoll` over the whole argument, `errno` and an empty conversion rejected.
    fn strtoll_strict(s: &[u8]) -> Option<i64> {
        let t = s.trim_ascii_start();
        let (neg, digits) = match t.first() {
            Some(b'-') => (true, &t[1..]),
            Some(b'+') => (false, &t[1..]),
            _ => (false, t),
        };
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let mut v: i128 = 0;
        for d in digits {
            v = v * 10 + (d - b'0') as i128;
            if v > i64::MAX as i128 + 1 {
                return None;
            }
        }
        let v = if neg { -v } else { v };
        i64::try_from(v).ok()
    }

    fn lossy(b: &[u8]) -> String {
        redis_proto::escape(cstr(b))
    }

    /// `CollectArgs_Parse` (the `ArgParser` run with FIELDS at position 1, SORTBY and LIMIT
    /// sub-arguments, the DISTINCT flag; callback errors win over the parser's), then the
    /// key resolution.
    pub(super) fn parse(o: &mut RedOpts<'_, '_>) -> Result<Spec, QErr> {
        let mut status: Option<QErr> = None;
        let mut perr: Option<String> = None;
        let set = |st: &mut Option<QErr>, e: QErr| {
            if st.is_none() {
                *st = Some(e);
            }
        };
        let mut field_names: Vec<Vec<u8>> = Vec::new();
        let mut load_all = false;
        let mut sort_names: Vec<Vec<u8>> = Vec::new();
        let mut asc_map = u64::MAX;
        let mut limit: Option<(u64, u64)> = None;
        let mut distinct = false;
        let (mut sortby_done, mut limit_done) = (false, false);
        let max = o.max_results;
        let ac = &mut o.ac;

        // FIELDS ( * | <n> @field ... ), only as the first argument.
        let fields = |ac: &mut Ac<'_>, st: &mut Option<QErr>, names: &mut Vec<Vec<u8>>, all: &mut bool| {
            let Ok(first) = ac.get() else { return Err("FIELDS: Expected an argument, but none provided".to_string()) };
            let first = cstr(first);
            if first == b"*" {
                *all = true;
                return Ok(());
            }
            let Some(n) = strtoll_strict(first) else {
                set(st, QErr::new(Code::ParseArgs, "Expected number of fields or `*` after FIELDS"));
                return Ok(());
            };
            if !(1..=MAX_FIELD_ARGS).contains(&n) {
                set(st, QErr::new(Code::Limit, format!("FIELDS count must be in [1, {MAX_FIELD_ARGS}]")));
                return Ok(());
            }
            let Ok(mut sub) = ac.get_slice(n as usize) else {
                set(st, QErr::new(Code::ParseArgs, "Not enough arguments were provided based on argument count for FIELDS"));
                return Ok(());
            };
            while let Some(a) = sub.take() {
                let a = cstr(a);
                match a.first() {
                    Some(b'@') => names.push(a[1..].to_vec()),
                    Some(b'$') => names.push(a.to_vec()),
                    _ => {
                        set(
                            st,
                            QErr::user(
                                Code::ParseArgs,
                                "Missing prefix: name requires '@' prefix, JSON path require '$' prefix",
                                format!(", got: {} in FIELDS", lossy(a)),
                            ),
                        );
                        return Ok(());
                    }
                }
            }
            Ok(())
        };

        let mut parsed_first = false;
        if let Some(t) = ac.peek()
            && c_eq(t, "FIELDS")
        {
            let _ = ac.advance();
            parsed_first = true;
            if let Err(e) = fields(ac, &mut status, &mut field_names, &mut load_all) {
                perr = Some(e);
            }
        }
        if perr.is_none() && !parsed_first {
            perr = Some("FIELDS: Required positional argument missing or out of order".into());
        }
        while perr.is_none() && !ac.at_end() {
            let tok = ac.peek().expect("an argument");
            let word = cstr(tok).to_ascii_uppercase();
            match word.as_slice() {
                b"FIELDS" => {
                    // A parsed positional argument met again: only its name is skipped.
                    let _ = ac.advance();
                }
                b"SORTBY" if sortby_done => perr = Some("SORTBY: Argument specified multiple times".into()),
                b"LIMIT" if limit_done => perr = Some("LIMIT: Argument specified multiple times".into()),
                b"DISTINCT" if distinct => perr = Some("DISTINCT: Argument specified multiple times".into()),
                b"DISTINCT" => {
                    let _ = ac.advance();
                    distinct = true;
                }
                b"SORTBY" => {
                    let _ = ac.advance();
                    sortby_done = true;
                    let Ok(n) = ac.get_u32(0) else {
                        perr = Some("SORTBY: Failed to parse the argument count".into());
                        break;
                    };
                    if !(1..=MAX_SORT_TOKENS).contains(&n) {
                        perr = Some("SORTBY: Invalid argument count".into());
                        break;
                    }
                    let Ok(mut sub) = ac.get_slice(n as usize) else {
                        perr = Some("SORTBY: Not enough arguments were provided based on argument count".into());
                        break;
                    };
                    sort_names.clear();
                    asc_map = u64::MAX;
                    while let Some(a) = sub.take() {
                        let a = cstr(a);
                        if a.first() != Some(&b'@') {
                            set(&mut status, QErr::user(Code::ParseArgs, "Missing prefix: name requires '@' prefix", format!(" ({})", lossy(a))));
                            break;
                        }
                        if sort_names.len() >= MAX_SORT_KEYS {
                            set(&mut status, QErr::new(Code::Limit, format!("SORTBY exceeds maximum of {MAX_SORT_KEYS} fields")));
                            break;
                        }
                        sort_names.push(a[1..].to_vec());
                        let idx = sort_names.len() - 1;
                        if sub.advance_if("ASC") {
                        } else if sub.advance_if("DESC") {
                            asc_map &= !(1u64 << idx);
                        }
                    }
                }
                b"LIMIT" => {
                    let _ = ac.advance();
                    limit_done = true;
                    let Ok(mut sub) = ac.get_slice(2) else {
                        perr = Some("LIMIT: Not enough arguments were provided".into());
                        break;
                    };
                    let Ok(off) = sub.get_size(crate::search::args::GE0) else {
                        set(&mut status, QErr::new(Code::ParseArgs, "LIMIT offset must be a non-negative integer"));
                        continue;
                    };
                    let Ok(cnt) = sub.get_size(crate::search::args::GE1) else {
                        set(&mut status, QErr::new(Code::ParseArgs, "LIMIT count must be a positive integer"));
                        continue;
                    };
                    if off > max {
                        set(&mut status, QErr::new(Code::Limit, format!("LIMIT offset exceeds maximum of {max}")));
                        continue;
                    }
                    if cnt > max {
                        set(&mut status, QErr::new(Code::Limit, format!("LIMIT count exceeds maximum of {max}")));
                        continue;
                    }
                    limit = Some((off, cnt));
                }
                _ => perr = Some(format!("{}: Unknown argument", lossy(tok))),
            }
        }
        if let Some(e) = status {
            return Err(e);
        }
        if let Some(e) = perr {
            return Err(QErr::user(Code::ParseArgs, "Bad arguments for COLLECT", format!(": {e}")));
        }

        let fields = if load_all {
            Fields::All
        } else {
            let mut v = Vec::with_capacity(field_names.len());
            for n in &field_names {
                let k = o.resolve(n)?;
                v.push((Bytes::copy_from_slice(n), k));
            }
            Fields::Specific(v)
        };
        let mut sort = Vec::with_capacity(sort_names.len());
        for n in &sort_names {
            sort.push(o.resolve(n)?);
        }
        let ranked = !sort.is_empty();
        let (offset, count) = match (ranked, limit) {
            (_, Some((off, cnt))) => (off as usize, cnt as usize),
            (false, None) => (0, max as usize),
            (true, None) => (0, DEFAULT_LIMIT),
        };
        Ok(Spec { fields, sort, asc_map, offset, count, ranked, distinct })
    }

    impl Spec {
        pub fn instance(&self) -> Acc {
            Acc { entries: Vec::new(), buckets: HashMap::new() }
        }

        fn cap(&self) -> usize {
            self.offset.saturating_add(self.count)
        }

        fn project(&self, row: &Row<'_>, all: &[(Bytes, KRef)]) -> Proj {
            let keys: Vec<KRef> = match &self.fields {
                Fields::Specific(f) => f.iter().map(|(_, k)| *k).collect(),
                Fields::All => all.iter().map(|(_, k)| *k).collect(),
            };
            let mut out: Vec<Option<RsValue>> = Vec::new();
            for k in keys {
                if let Some(v) = rget(row, k) {
                    if out.len() <= k.dst {
                        out.resize(k.dst + 1, None);
                    }
                    out[k.dst] = Some(v);
                }
            }
            Proj(out)
        }

        fn find(acc: &Acc, p: &Proj, h: u64) -> Option<usize> {
            acc.buckets.get(&h)?.iter().copied().find(|&i| acc.entries[i].row.same(p))
        }

        fn worst(&self, acc: &Acc) -> Option<usize> {
            (0..acc.entries.len()).reduce(|w, i| {
                let (a, b) = (&acc.entries[i], &acc.entries[w]);
                if rank(a, &b.vals, b.doc, self.asc_map) == Ordering::Less { i } else { w }
            })
        }

        fn reindex(acc: &mut Acc) {
            acc.buckets.clear();
            for (i, e) in acc.entries.iter().enumerate() {
                acc.buckets.entry(e.row.hash()).or_default().push(i);
            }
        }

        pub fn add(&self, acc: &mut Acc, row: &Row<'_>, doc_id: u64, all: &[(Bytes, KRef)]) {
            let cap = self.cap();
            if !self.ranked {
                if acc.entries.len() >= cap {
                    return;
                }
                let p = self.project(row, all);
                if self.distinct {
                    let h = p.hash();
                    if Self::find(acc, &p, h).is_some() {
                        return;
                    }
                    acc.buckets.entry(h).or_default().push(acc.entries.len());
                }
                acc.entries.push(Entry { vals: Vec::new(), doc: doc_id, row: p });
                return;
            }
            let vals: Vec<Option<RsValue>> = self.sort.iter().map(|&k| rget(row, k)).collect();
            if !self.distinct {
                if acc.entries.len() < cap {
                    let p = self.project(row, all);
                    acc.entries.push(Entry { vals, doc: doc_id, row: p });
                } else if let Some(w) = self.worst(acc)
                    && rank(&acc.entries[w], &vals, doc_id, self.asc_map) == Ordering::Less
                {
                    let p = self.project(row, all);
                    acc.entries[w] = Entry { vals, doc: doc_id, row: p };
                }
                return;
            }
            // A priority queue keyed by the projected row: a repeat keeps its best rank; a
            // new row past the cap evicts the worst.
            let p = self.project(row, all);
            let h = p.hash();
            if let Some(i) = Self::find(acc, &p, h) {
                if rank(&acc.entries[i], &vals, doc_id, self.asc_map) == Ordering::Less {
                    acc.entries[i].vals = vals;
                    acc.entries[i].doc = doc_id;
                }
                return;
            }
            acc.buckets.entry(h).or_default().push(acc.entries.len());
            acc.entries.push(Entry { vals, doc: doc_id, row: p });
            if acc.entries.len() > cap
                && let Some(w) = self.worst(acc)
            {
                acc.entries.swap_remove(w);
                Self::reindex(acc);
            }
        }

        pub fn finalize(&self, acc: &mut Acc, all: &[(Bytes, KRef)]) -> RsValue {
            let template: Vec<(Bytes, usize)> = match &self.fields {
                Fields::Specific(f) => {
                    let mut seen = HashSet::new();
                    f.iter().filter(|(_, k)| seen.insert(k.dst)).map(|(n, k)| (n.clone(), k.dst)).collect()
                }
                Fields::All => all.iter().map(|(n, k)| (n.clone(), k.dst)).collect(),
            };
            let mut entries = std::mem::take(&mut acc.entries);
            acc.buckets.clear();
            if self.ranked {
                entries.sort_by(|a, b| rank(b, &a.vals, a.doc, self.asc_map));
            }
            RsValue::Array(
                entries
                    .into_iter()
                    .skip(self.offset)
                    .take(self.count)
                    .map(|e| {
                        RsValue::Map(
                            template
                                .iter()
                                .filter_map(|(n, d)| e.row.0.get(*d).and_then(|v| v.clone()).map(|v| (RsValue::String(n.to_vec()), v)))
                                .collect(),
                        )
                    })
                    .collect(),
            )
        }
    }
}
