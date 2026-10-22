//! Scroll and point-in-time contexts: the shard searchers a search started with, kept for
//! `keep_alive` so later pages (`_search/scroll`) and PIT searches see the same snapshot.
//! Expiry is checked on access (no timer runs), bounded by `search.max_keep_alive` (24h) and
//! `search.max_open_scroll_context` (500).

use super::{SearchOutcome, SearchRequest, Target};
use crate::rest::Reply;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::Ctx;
use crate::units::TimeValue;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A kept search.
pub struct SearchContext {
    pub targets: Vec<Target>,
    /// The request of a scroll (pages advance `from`).
    pub request: Option<SearchRequest>,
    pub expires: Instant,
    pub pit: bool,
    /// Scroll: hits returned so far.
    pub offset: usize,
    pub indices: Vec<String>,
}

#[derive(Default)]
pub struct Contexts {
    next: std::sync::atomic::AtomicU64,
    map: Mutex<HashMap<String, SearchContext>>,
}

impl std::fmt::Debug for Contexts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Contexts").field("open", &self.map.lock().len()).finish()
    }
}

fn new_id(n: u64, pit: bool) -> String {
    use base64::Engine as _;
    let kind: &[u8] = if pit { b"\x01pit" } else { b"\x01scroll" };
    let mut b = Vec::with_capacity(32);
    b.extend_from_slice(kind);
    b.extend_from_slice(&n.to_be_bytes());
    let r: u64 = rand::random();
    b.extend_from_slice(&r.to_be_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn max_keep_alive(ctx: &Ctx) -> TimeValue {
    let st = ctx.state();
    st.metadata.cluster_setting("search.max_keep_alive").and_then(|v| TimeValue::parse(v, "search.max_keep_alive").ok()).unwrap_or(TimeValue::hours(24))
}

/// `SearchService.checkKeepAliveLimit`.
pub fn check_keep_alive(ctx: &Ctx, ka: TimeValue) -> EsResult<Duration> {
    let max = max_keep_alive(ctx);
    if ka.as_millis() > max.as_millis() {
        return Err(EsError::illegal_argument(format!(
            "Keep alive for request ({}) is too large. It must be less than ({}). This limit can be set by changing the [search.max_keep_alive] cluster level setting.",
            ka.string_rep(),
            max.string_rep()
        )));
    }
    Ok(Duration::from_millis(ka.as_millis().max(0) as u64))
}

impl Contexts {
    fn purge(&self) {
        let now = Instant::now();
        self.map.lock().retain(|_, c| c.expires > now);
    }

    pub fn open(&self, c: SearchContext) -> String {
        self.purge();
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let id = new_id(n, c.pit);
        self.map.lock().insert(id.clone(), c);
        id
    }

    pub fn count_scrolls(&self) -> usize {
        self.map.lock().values().filter(|c| !c.pit).count()
    }

    /// Runs `f` on a live context (extending its expiry by `keep_alive`).
    pub fn with<R>(&self, id: &str, keep_alive: Option<Duration>, f: impl FnOnce(&mut SearchContext) -> R) -> Option<R> {
        self.purge();
        let mut g = self.map.lock();
        let c = g.get_mut(id)?;
        if let Some(k) = keep_alive {
            c.expires = Instant::now() + k;
        }
        Some(f(c))
    }

    pub fn close(&self, id: &str) -> bool {
        self.map.lock().remove(id).is_some()
    }

    pub fn close_all(&self, pit: bool) -> usize {
        let mut g = self.map.lock();
        let before = g.len();
        g.retain(|_, c| c.pit != pit);
        before - g.len()
    }
}

fn missing(id: &str) -> EsError {
    let inner = EsError::new(404, "search_context_missing_exception", format!("No search context found for id [{id}]"));
    EsError::new(404, "search_phase_execution_exception", "all shards failed")
        .with_meta("phase", "query")
        .with_meta("grouped", true)
        .with_meta("failed_shards", Value::Arr(vec![obj! { "shard" => -1, "index" => Value::Null, "reason" => inner.to_value(false) }]))
        .caused_by(inner.clone())
        .with_root_causes(vec![inner])
}

/// Starts a scroll after its first page.
pub fn start_scroll(ctx: &Ctx, req: &SearchRequest, targets: Vec<Target>, out: &mut SearchOutcome, returned: usize) -> EsResult<()> {
    let ka = check_keep_alive(ctx, req.scroll.unwrap_or(TimeValue::minutes(1)))?;
    let max_open = ctx.state().metadata.cluster_setting("search.max_open_scroll_context").and_then(|v| v.parse::<usize>().ok()).unwrap_or(500);
    if ctx.cluster.contexts.count_scrolls() >= max_open {
        return Err(EsError::new(
            500,
            "exception",
            format!("Trying to create too many scroll contexts. Must be less than or equal to: [{max_open}]. This limit can be set by changing the [search.max_open_scroll_context] setting."),
        ));
    }
    for t in &targets {
        t.shard.counters.scroll_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let id = ctx.cluster.contexts.open(SearchContext { targets, request: Some(req.clone()), expires: Instant::now() + ka, pit: false, offset: returned, indices: req.indices.clone() });
    out.scroll_id = Some(id);
    Ok(())
}

/// `GET/POST /_search/scroll`.
pub fn scroll(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let mut id = ctx.param("scroll_id").map(str::to_string);
    let mut ka = ctx.query("scroll").map(str::to_string);
    for (k, v) in &body {
        match k.as_str() {
            "scroll_id" => id = Some(v.to_es_string()),
            "scroll" => ka = Some(v.to_es_string()),
            other => return Err(EsError::illegal_argument(format!("Unknown parameter [{other}] in request body or parameter is of the wrong type[{}] ", token(v)))),
        }
    }
    let Some(id) = id else {
        return Err(EsError::action_request_validation(&["scrollId is missing".into()]));
    };
    check_scroll_id(&id)?;
    let keep = match &ka {
        Some(k) => Some(check_keep_alive(ctx, TimeValue::parse(k, "scroll")?)?),
        None => None,
    };
    let state = ctx.cluster.contexts.with(&id, keep, |c| (c.targets.clone(), c.request.clone(), c.offset));
    let Some((targets, Some(req), offset)) = state else { return Err(missing(&id)) };
    let size = req.source.size.unwrap_or(10).max(0) as usize;
    let mut page = req.clone();
    page.source.from = Some(offset as i32);
    page.source.track_total_hits = Some(crate::search_source::TrackTotalHits::Accurate);
    page.scroll = None;
    for t in &targets {
        t.shard.counters.scroll_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let mut out = super::execute_on(ctx.cluster, &page, &targets)?;
    let n = out.hits.len();
    let _ = size;
    ctx.cluster.contexts.with(&id, None, |c| c.offset += n);
    out.scroll_id = Some(id);
    Reply::ok(super::render(&out, &req))
}

fn token(v: &Value) -> &'static str {
    match v {
        Value::Arr(_) => "START_ARRAY",
        Value::Obj(_) => "START_OBJECT",
        _ => "VALUE_STRING",
    }
}

/// `DELETE /_search/scroll`.
pub fn clear_scroll(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let mut ids: Vec<String> = ctx.param("scroll_id").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    // Ids in the body replace the URL's (`RestClearScrollAction`).
    if body.contains_key("scroll_id") {
        ids.clear();
    }
    for (k, v) in &body {
        match (k.as_str(), v) {
            ("scroll_id", Value::Arr(a)) => ids.extend(a.iter().map(|x| x.to_es_string())),
            ("scroll_id", other) => ids.push(other.to_es_string()),
            (other, _) => return Err(EsError::illegal_argument(format!("Unknown parameter [{other}] in request body or parameter is of the wrong type[{}] ", token(v)))),
        }
    }
    if ids.is_empty() {
        return Err(EsError::action_request_validation(&["no scroll ids specified".into()]));
    }
    let mut freed = 0;
    if ids.iter().any(|i| i == "_all") {
        freed = ctx.cluster.contexts.close_all(false);
    } else {
        for id in &ids {
            check_scroll_id(id)?;
        }
        for id in &ids {
            if ctx.cluster.contexts.close(id) {
                freed += 1;
            }
        }
    }
    let status = if freed == 0 && !ids.iter().any(|i| i == "_all") { 404 } else { 200 };
    Ok(Reply::Body(status, obj! { "succeeded" => true, "num_freed" => freed as i64 }))
}

/// `POST /{index}/_pit`.
pub fn open_pit(ctx: &Ctx) -> EsResult<Reply> {
    let Some(ka) = ctx.query("keep_alive") else {
        return Err(EsError::action_request_validation(&["[keep_alive] is not specified".into()]));
    };
    let ka = check_keep_alive(ctx, TimeValue::parse(ka, "keep_alive")?)?;
    let body = ctx.body_map()?.unwrap_or_default();
    let mut req = super::parse_request(ctx, None, None)?;
    // `index_filter`: shards it can't match are left out of the point in time (can_match),
    // reported as skipped.
    let filter = match body.get("index_filter") {
        Some(f) => Some(crate::query::parse_query(f, ctx.raw_json(), &[crate::json::locate::Seg::Key("index_filter")]).map_err(|e| {
            let get = |k: &str| e.meta().iter().find(|(n, _)| n == k).and_then(|(_, v)| v.as_i64());
            let (l, c) = get("line").zip(get("col")).map(|(l, c)| (l as usize, c as usize)).unwrap_or((1, 1));
            EsError::x_content_parse(l, c, "[open_point_in_time_request] failed to parse field [index_filter]").caused_by(e)
        })?.0),
        None => None,
    };
    req.source = Default::default();
    let st = ctx.state();
    let mut targets = super::shard_targets(ctx.cluster, &st, &req)?;
    for t in targets.iter_mut() {
        t.searcher = Some(t.searcher()?);
    }
    let n = targets.len();
    if let Some(f) = &filter {
        let mut kept = Vec::with_capacity(targets.len());
        for t in targets {
            let searcher = t.searcher()?;
            let table = t.engine.table();
            let qctx = super::query::QueryCtx::new(ctx.cluster, &st, &t.idx, &table, &searcher, ctx.now_ms);
            let q = qctx.to_filter(f).map_err(|e| super::query::shard_exception(&t.idx, e))?;
            if searcher.count(&q).map_err(crate::engine::convert::index_error)? > 0 {
                kept.push(t);
            }
        }
        targets = kept;
    }
    let skipped = n - targets.len();
    let id = ctx.cluster.contexts.open(SearchContext { targets, request: None, expires: Instant::now() + ka, pit: true, offset: 0, indices: req.indices.clone() });
    Reply::ok(obj! { "id" => id, "_shards" => obj! { "total" => n as i64, "successful" => n as i64, "skipped" => skipped as i64, "failed" => 0 } })
}

/// `DELETE /_pit`.
pub fn close_pit(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let Some(id) = body.get("id").map(|v| v.to_es_string()) else {
        return Err(EsError::action_request_validation(&["point in time id is missing".into()]));
    };
    check_pit_id(&id)?;
    let ok = ctx.cluster.contexts.close(&id);
    Ok(Reply::Body(if ok { 200 } else { 404 }, obj! { "succeeded" => true, "num_freed" => ok as i64 }))
}

/// `TransportSearchHelper.parseScrollId` on an id this node never minted: the decoded bytes are
/// read as Elasticsearch's scroll id layout (an optional context-uuid marker, the search type,
/// then `(uuid?, long id, node)` per shard), and any failure is "Cannot parse scroll id" with
/// the stream's exception as its cause. Ids that parse are well formed but unknown here.
fn check_scroll_id(id: &str) -> EsResult<()> {
    use base64::Engine as _;
    let wrap = |cause: EsError| EsError::illegal_argument("Cannot parse scroll id").caused_by(cause);
    if let Some(msg) = crate::units::java_base64_url_error(id) {
        return Err(wrap(EsError::illegal_argument(msg)));
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(id.trim_end_matches('=')).map_err(|e| wrap(EsError::illegal_argument(e.to_string())))?;
    if bytes.starts_with(b"\x01scroll") {
        return Ok(());
    }
    let mut r = StreamIn { b: &bytes, pos: 0 };
    let parsed = (|| -> EsResult<()> {
        let first = r.string()?;
        let with_uuid = first == "include_context_uuid";
        if with_uuid {
            r.string()?;
        }
        let n = r.array_size()?;
        for _ in 0..n {
            if with_uuid {
                r.string()?;
            }
            r.long()?;
            r.string()?;
        }
        if r.pos != bytes.len() {
            return Err(EsError::illegal_argument("Not all bytes were read"));
        }
        Ok(())
    })();
    parsed.map_err(wrap)
}

/// Elasticsearch's `StreamInput` over a byte array, with its exceptions.
struct StreamIn<'a> {
    b: &'a [u8],
    pos: usize,
}

impl StreamIn<'_> {
    fn remaining(&self) -> usize {
        self.b.len() - self.pos
    }

    fn eof(&self, wanted: usize) -> EsError {
        EsError::java(400, "e_o_f_exception", format!("tried to read: {wanted} bytes but only {} remaining", self.remaining()))
    }

    fn byte(&mut self) -> EsResult<u8> {
        let Some(&b) = self.b.get(self.pos) else {
            // `ByteBuffer.get` past the end, rethrown as an `EOFException`.
            return Err(EsError::java(400, "e_o_f_exception", "").with_null_reason().caused_by(EsError::java(400, "buffer_underflow_exception", "").with_null_reason()));
        };
        self.pos += 1;
        Ok(b)
    }

    fn vint(&mut self) -> EsResult<i32> {
        let mut v: u32 = 0;
        for i in 0..5 {
            let b = self.byte()?;
            if i == 4 && b & 0xf0 != 0 {
                return Err(EsError::java(400, "i_o_exception", format!("Invalid vInt (({:x} & 0x7f) << 28) | {:x}", b, v)));
            }
            v |= u32::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                break;
            }
        }
        Ok(v as i32)
    }

    fn array_size(&mut self) -> EsResult<usize> {
        let n = self.vint()?;
        if n < 0 {
            return Err(EsError::java(400, "negative_array_size_exception", format!("array size must be positive but was: {n}")));
        }
        let n = n as usize;
        // Each element takes at least a byte (`ensureCanReadBytes`).
        if n > self.remaining() {
            return Err(self.eof(n));
        }
        Ok(n)
    }

    fn long(&mut self) -> EsResult<i64> {
        if self.remaining() < 8 {
            return Err(self.eof(8));
        }
        let v = i64::from_be_bytes(self.b[self.pos..self.pos + 8].try_into().unwrap_or_default());
        self.pos += 8;
        Ok(v)
    }

    /// `readString`: a char count, then that many UTF-16 units as (modified) UTF-8.
    fn string(&mut self) -> EsResult<String> {
        let chars = self.array_size()?;
        let mut out = String::new();
        for _ in 0..chars {
            let b = self.byte()?;
            let c = match b >> 4 {
                0..=7 => u32::from(b),
                12 | 13 => (u32::from(b & 0x1f) << 6) | u32::from(self.byte()? & 0x3f),
                14 => (u32::from(b & 0x0f) << 12) | (u32::from(self.byte()? & 0x3f) << 6) | u32::from(self.byte()? & 0x3f),
                _ => return Err(EsError::java(400, "i_o_exception", format!("Invalid string; unexpected character: {b} hex: {b:x}"))),
            };
            out.push(char::from_u32(c).unwrap_or('\u{FFFD}'));
        }
        Ok(out)
    }
}

/// Ids this node never minted fail as `SearchContextId.decode` does on them: their first
/// bytes read as a transport version (a vint) no node knows.
fn check_pit_id(id: &str) -> EsResult<()> {
    use base64::Engine as _;
    if let Some(msg) = crate::units::java_base64_url_error(id) {
        return Err(EsError::illegal_argument(msg));
    }
    let trimmed = id.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(trimmed).map_err(|e| EsError::illegal_argument(e.to_string()))?;
    if bytes.starts_with(b"\x01pit") {
        return Ok(());
    }
    let mut version: u64 = 0;
    for (i, b) in bytes.iter().take(5).enumerate() {
        version |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            break;
        }
    }
    Err(EsError::illegal_argument(format!("unknown transport version [{}] reading search context id", version as i32)))
}

/// The targets of a PIT search (extending its keep alive).
pub fn pit_targets(ctx: &Ctx, pit: &crate::search_source::PointInTime) -> EsResult<Vec<Target>> {
    check_pit_id(&pit.id)?;
    let keep = match &pit.keep_alive {
        Some(k) => Some(check_keep_alive(ctx, *k)?),
        None => None,
    };
    match ctx.cluster.contexts.with(&pit.id, keep, |c| c.targets.clone()) {
        Some(t) => Ok(t),
        None => Err(missing(&pit.id)),
    }
}

#[allow(dead_code)]
fn unused(_: Map) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn cause(id: &str) -> Value {
        let e = check_scroll_id(id).unwrap_err();
        assert_eq!(e.reason(), "Cannot parse scroll id");
        e.to_value(false).get("caused_by").cloned().unwrap()
    }

    // Elasticsearch 9.5.5's causes for the same ids.
    #[test]
    fn foreign_scroll_ids_fail_to_parse() {
        assert_eq!(cause("abc").to_json(), r#"{"type":"e_o_f_exception","reason":"tried to read: 105 bytes but only 1 remaining"}"#);
        assert_eq!(cause("DXF1ZXJ5QW5kRmV0Y2gBAAAAAAAAAAEWdGVzdA==").to_json(), r#"{"type":"e_o_f_exception","reason":"tried to read: 22 bytes but only 4 remaining"}"#);
        assert_eq!(cause("").to_json(), r#"{"type":"e_o_f_exception","reason":null,"caused_by":{"type":"buffer_underflow_exception","reason":null}}"#);
        assert_eq!(cause("a").to_json(), r#"{"type":"illegal_argument_exception","reason":"Input byte[] should at least have 2 bytes for base64 bytes"}"#);
        // Well formed: one shard context on node "test".
        assert!(check_scroll_id("DXF1ZXJ5QW5kRmV0Y2gBAAAAAAAAAAEEdGVzdA==").is_ok());
        assert!(check_scroll_id(&new_id(7, false)).is_ok());
        assert!(check_pit_id(&new_id(7, true)).is_ok());
        assert_eq!(check_pit_id("abc").unwrap_err().reason(), "unknown transport version [105] reading search context id");
    }
}
