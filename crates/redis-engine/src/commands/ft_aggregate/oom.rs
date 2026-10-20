//! The query memory guard (`QueryMemoryGuard`) and its empty replies (`reply_empty.c`): before
//! a query is parsed, used memory at or over `maxmemory` (`RedisModule_GetUsedMemoryRatio`,
//! no limit: never) stops it unless `search-on-oom` is IGNORE. FAIL replies with the OOM
//! error; RETURN with no results and the out-of-memory warning, shaped from a shallow look at
//! the arguments (WITHCURSOR anywhere, FORMAT's value). FT.SEARCH, FT.AGGREGATE and FT.HYBRID
//! check first thing (the coordinator's handlers); FT.PROFILE once its SEARCH/AGGREGATE/HYBRID
//! and QUERY words are read; `_FT.HYBRID` once its debug parameters are. The other debug
//! wrappers and cursor reads never check.

use super::*;

/// `QUERY_WOOM_SHARD`.
pub(super) const SHARD_WARNING: &str = "One or more shards failed to execute the query due to insufficient memory";
/// `QUERY_WOOM_COORD`.
const COORD_WARNING: &str = "Coordinator failed to execute the query due to insufficient memory";
/// `QueryWarning_Strwarning(QUERY_WARNING_CODE_OUT_OF_MEMORY_SHARD)`.
const SHARD_CODE_WARNING: &str = "Shard failed to execute the query due to insufficient memory";

/// What the guard stopped a query with (`search-on-oom`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::commands) enum Oom {
    Fail,
    Return,
}

/// `QueryMemoryGuard`: whether (and how) the query is stopped.
pub(in crate::commands) fn guard(cx: &Cx) -> Option<Oom> {
    let policy = cx.eng.cfg.num("search-on-oom");
    // OomPolicy_Ignore
    if policy == 2 {
        return None;
    }
    let max = cx.eng.cfg.maxmemory;
    if max == 0 || (cx.eng.used_memory() as f32) / (max as f32) < 1.0 {
        return None;
    }
    Some(if policy == 1 { Oom::Fail } else { Oom::Return })
}

/// `QueryMemoryGuardFailure_WithReply`, the error counted (on the coordinator's side or the
/// shard's).
fn fail(cx: &Cx, coord: bool) -> Error {
    cx.eng.search.stats.error(QueryError::Oom, coord);
    QErr::code(Code::OutOfMemory).into()
}

/// FT.SEARCH and FT.AGGREGATE (`profile`: FT.PROFILE's, with its LIMITED and clock) stopped by
/// the guard: the error, or `single_shard_common_query_reply_empty` over `argv` (the command,
/// the index, the query's arguments).
pub(in crate::commands) fn query(cx: &mut Cx, o: Oom, argv: &[Bytes], profile: Option<(bool, Instant)>) -> CmdResult {
    let internal = argv[0].first() == Some(&b'_');
    // A standalone server's guards count the error as the coordinator's (`COORD_ERR_WARN`,
    // `GetNumShards_UnSafe() == 1`), whatever the command.
    if o == Oom::Fail {
        return Err(fail(cx, true));
    }
    // `shallow_parse_query_args`.
    let cursor = argv.iter().any(|a| a.eq_ignore_ascii_case(b"WITHCURSOR"));
    let mut expand = false;
    if let Some(i) = argv.iter().skip(1).position(|a| a.eq_ignore_ascii_case(b"FORMAT")) {
        match argv.get(i + 2).map(|f| cstr(f)) {
            None => return Err(QErr::new(Code::BadVal, "Need an argument for FORMAT").into()),
            Some(f) if f.eq_ignore_ascii_case(b"EXPAND") => expand = true,
            Some(f) if f.eq_ignore_ascii_case(b"STRING") => {}
            Some(f) => return Err(QErr::user(Code::ParseArgs, "FORMAT", format!(" {} is not supported", redis_proto::escape(f))).into()),
        }
    }
    // `sendChunk_ReplyOnly_EmptyResults`: the warning counted (no query is).
    cx.eng.search.stats.warning(QueryWarning::Oom, !internal);
    let resp3 = cx.client.resp >= 3;
    let results = if resp3 {
        let warning = if internal { SHARD_WARNING } else { COORD_WARNING };
        Reply::Map(vec![
            (Reply::Status("attributes".into()), Reply::Array(Vec::new())),
            (Reply::Status("format".into()), Reply::Status(if expand { "EXPAND" } else { "STRING" }.into())),
            (Reply::Status("results".into()), Reply::Array(Vec::new())),
            (Reply::Status("total_results".into()), Reply::Int(0)),
            (Reply::Status("warning".into()), Reply::Array(vec![Reply::Status(warning.into())])),
        ])
    } else {
        Reply::Array(vec![Reply::Int(0)])
    };
    let prof = profile.map(|(limited, clock)| ProfCur { limited, internal, clock: Some(clock), oom: true, ..ProfCur::default() });
    if cursor {
        return Ok(cursor_reply(results, 0, true, prof.as_ref(), resp3));
    }
    let Some(p) = prof else { return Ok(results) };
    let kv = |k: &str, v: Reply| (Reply::Status(k.into()), v);
    Ok(if resp3 { Reply::Map(vec![kv("Results", results), kv("Profile", p.reply())]) } else { Reply::Array(vec![results, p.reply()]) })
}

/// FT.HYBRID stopped by the guard: the error, or `common_hybrid_query_reply_empty` (the
/// internal `_FT.HYBRID`: its subqueries' cursor ids, none).
pub(in crate::commands) fn hybrid(cx: &mut Cx, o: Oom, internal: bool, profile: bool) -> CmdResult {
    if o == Oom::Fail {
        return Err(fail(cx, !internal));
    }
    // `common_hybrid_query_reply_empty`: the shard's warning for `_FT.HYBRID`'s cursor reply,
    // `sendChunk_ReplyOnly_HybridEmptyResults`'s coordinator's otherwise.
    cx.eng.search.stats.warning(QueryWarning::Oom, !internal);
    let kv = |k: &str, v: Reply| (Reply::Status(k.into()), v);
    if internal {
        return Ok(Reply::Map(vec![kv("SEARCH", Reply::Int(0)), kv("VSIM", Reply::Int(0)), kv("warnings", Reply::Array(vec![Reply::Status(SHARD_CODE_WARNING.into())]))]));
    }
    // `sendChunk_ReplyOnly_HybridEmptyResults`.
    let results = Reply::Map(vec![
        kv("total_results", Reply::Int(0)),
        kv("results", Reply::Array(Vec::new())),
        kv("warnings", Reply::Array(vec![Reply::Status(COORD_WARNING.into())])),
        kv("execution_time", Reply::Double(0.0)),
    ]);
    if !profile {
        return Ok(results);
    }
    // `Profile_PrintInFormat` without shards or coordinator.
    let p = Reply::Map(vec![kv("Shards", Reply::Array(Vec::new())), kv("Coordinator", Reply::Map(Vec::new()))]);
    Ok(if cx.client.resp >= 3 { Reply::Map(vec![kv("Results", results), kv("Profile", p)]) } else { Reply::Array(vec![results, p]) })
}
