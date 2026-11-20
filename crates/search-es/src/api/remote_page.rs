//! A remote's page of hits read without a tree of it (reindex from remote, [`super::remote`]).
//!
//! A page is up to 16 MiB a hostile remote chose. Parsed into a [`Value`] tree it would cost
//! many times that (a node and a vector slot per value: `[0,0,…]` grows forty-fold). So the page
//! is scanned once, byte by byte, without recursion: only the envelope is decoded (`_scroll_id`,
//! `pit_id`, `hits.total`, each hit's `_index`, `_id`, `_routing`, `_version` and `sort`, and
//! `_shards`), each part bounded, and each hit's `_source` stays a slice of the page's bytes
//! (sharing its buffer, no copy) that goes as it is to indexing, which reads one document at a
//! time as it does a bulk request's. Anything else is skipped, not built.

use crate::error::{EsError, EsResult};
use crate::json::Value;
use bytes::Bytes;

/// Nesting the scanner follows, as the JSON parser allows.
const MAX_DEPTH: usize = 1000;
/// The longest `_index`, `_id`, `_routing` or field name decoded.
const MAX_NAME: usize = 4096;
/// The most bytes of a small part parsed into a tree (`hits.total`, `_shards`, a hit's `fields`).
const MAX_SMALL: usize = 64 * 1024;
/// The most bytes of a hit's `sort` values (`search_after`'s next page).
const MAX_SORT: usize = 4096;

/// One hit of a page.
pub struct ScannedHit {
    pub index: String,
    pub id: String,
    pub routing: Option<String>,
    pub version: i64,
    /// The hit's `_source` as the remote sent it (a slice of the page), `None` without one.
    pub source: Option<Bytes>,
}

/// What a page holds.
#[derive(Default)]
pub struct ScannedPage {
    pub scroll_id: Option<String>,
    pub pit_id: Option<String>,
    /// `hits.total`: a number, or `{"value": n}`.
    pub total: Option<u64>,
    pub hits: Vec<ScannedHit>,
    /// The last hit's `sort` values.
    pub last_sort: Option<Value>,
    /// `hits.hits` was there (a page without it isn't a search answer).
    pub has_hits: bool,
    /// Shards that failed, and the first one's reason.
    pub shard_failures: Option<(usize, String)>,
}

fn bad(what: &str) -> EsError {
    EsError::java(500, "x_content_parse_exception", format!("the remote's page isn't JSON a search answers with: {what}"))
}

struct Scan<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> Scan<'a> {
    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\r' | b'\n') = self.s.get(self.pos) {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> EsResult<()> {
        if self.peek() != Some(b) {
            return Err(bad(&format!("expected [{}] at byte {}", b as char, self.pos)));
        }
        self.pos += 1;
        Ok(())
    }

    /// Steps over a string whose opening quote is at `pos`.
    fn skip_string(&mut self) -> EsResult<()> {
        self.pos += 1;
        while let Some(&c) = self.s.get(self.pos) {
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(());
                }
                b'\\' => self.pos += 2,
                _ => self.pos += 1,
            }
        }
        Err(bad("a string never ends"))
    }

    /// A string's text, at most `max` bytes of it.
    fn string(&mut self, max: usize) -> EsResult<String> {
        if self.peek() != Some(b'"') {
            return Err(bad(&format!("expected a string at byte {}", self.pos)));
        }
        let start = self.pos;
        self.skip_string()?;
        let raw = &self.s[start..self.pos];
        if raw.len() > max + 2 + max / 2 {
            return Err(bad(&format!("a string of [{}] bytes, more than the [{max}] roll reads there", raw.len())));
        }
        match crate::json::parse(raw) {
            Ok(Some(Value::Str(s))) if s.len() <= max => Ok(s),
            Ok(Some(Value::Str(s))) => Err(bad(&format!("a string of [{}] bytes, more than the [{max}] roll reads there", s.len()))),
            _ => Err(bad("an invalid string")),
        }
    }

    /// Steps over one value, without recursion; its byte span.
    fn skip(&mut self) -> EsResult<(usize, usize)> {
        self.ws();
        let start = self.pos;
        match self.s.get(self.pos) {
            None => return Err(bad("the page ends early")),
            Some(b'"') => self.skip_string()?,
            Some(b'{' | b'[') => {
                let mut depth = 0usize;
                loop {
                    match self.s.get(self.pos) {
                        None => return Err(bad("an object or array never ends")),
                        Some(b'"') => self.skip_string()?,
                        Some(b'{' | b'[') => {
                            depth += 1;
                            if depth > MAX_DEPTH {
                                return Err(bad("nested too deep"));
                            }
                            self.pos += 1;
                        }
                        Some(b'}' | b']') => {
                            depth -= 1;
                            self.pos += 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        Some(_) => self.pos += 1,
                    }
                }
            }
            Some(_) => {
                while let Some(&c) = self.s.get(self.pos) {
                    if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
                        break;
                    }
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(bad(&format!("a value expected at byte {start}")));
                }
            }
        }
        Ok((start, self.pos))
    }

    /// One value parsed into a tree, if it's at most `max` bytes.
    fn small(&mut self, max: usize, what: &str) -> EsResult<Value> {
        let (a, b) = self.skip()?;
        if b - a > max {
            return Err(bad(&format!("[{what}] is [{}] bytes, more than the [{max}] roll reads", b - a)));
        }
        crate::json::parse(&self.s[a..b]).ok().flatten().ok_or_else(|| bad(&format!("[{what}] isn't valid JSON")))
    }

    /// The next member's name of an object whose `{` was read (`None` at its `}`).
    fn key(&mut self, first: &mut bool) -> EsResult<Option<String>> {
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(None);
        }
        if !std::mem::take(first) {
            self.expect(b',')?;
        }
        let k = self.string(MAX_NAME)?;
        self.expect(b':')?;
        Ok(Some(k))
    }

    /// Whether another element of an array whose `[` was read follows.
    fn element(&mut self, first: &mut bool) -> EsResult<bool> {
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(false);
        }
        if !std::mem::take(first) {
            self.expect(b',')?;
        }
        Ok(true)
    }
}

/// Reads one page of a remote search (`_search`, `_search/scroll`, a point-in-time search).
pub fn scan(page: &Bytes, max_id: usize) -> EsResult<ScannedPage> {
    let mut sc = Scan { s: page, pos: 0 };
    let mut out = ScannedPage::default();
    sc.expect(b'{')?;
    let mut first = true;
    while let Some(k) = sc.key(&mut first)? {
        match k.as_str() {
            "_scroll_id" => out.scroll_id = Some(id(&mut sc, max_id, "scroll")?),
            "pit_id" => out.pit_id = Some(id(&mut sc, max_id, "point in time")?),
            "_shards" => {
                let v = sc.small(MAX_SMALL, "_shards")?;
                if let Some(f) = v.get("failures").and_then(Value::as_array).filter(|f| !f.is_empty()) {
                    let reason = f.first().and_then(|x| x.pointer(&["reason", "reason"]).or_else(|| x.get("reason"))).and_then(Value::as_str).unwrap_or("shard failure");
                    out.shard_failures = Some((f.len(), reason.chars().take(1000).collect()));
                }
            }
            "hits" if sc.peek() == Some(b'{') => {
                sc.pos += 1;
                let mut first = true;
                while let Some(k) = sc.key(&mut first)? {
                    match k.as_str() {
                        "total" => {
                            out.total = match sc.small(MAX_SMALL, "hits.total")? {
                                Value::Obj(m) => m.get("value").and_then(Value::as_i64),
                                t => t.as_i64(),
                            }
                            .map(|n| n.max(0) as u64);
                        }
                        "hits" => {
                            out.has_hits = true;
                            sc.expect(b'[')?;
                            let mut first = true;
                            while sc.element(&mut first)? {
                                let (hit, sort) = scan_hit(&mut sc, page)?;
                                out.hits.push(hit);
                                if sort.is_some() {
                                    out.last_sort = sort;
                                }
                            }
                        }
                        _ => {
                            sc.skip()?;
                        }
                    }
                }
            }
            _ => {
                sc.skip()?;
            }
        }
    }
    Ok(out)
}

fn id(sc: &mut Scan<'_>, max: usize, what: &str) -> EsResult<String> {
    sc.string(max).map_err(|_| EsError::illegal_argument(format!("the remote's {what} id is longer than the [{max}] bytes roll keeps, or isn't a string")))
}

fn scan_hit(sc: &mut Scan<'_>, page: &Bytes) -> EsResult<(ScannedHit, Option<Value>)> {
    sc.expect(b'{')?;
    let (mut index, mut id, mut routing, mut version, mut source, mut sort) = (None, None, None, -1i64, None, None);
    let mut first = true;
    while let Some(k) = sc.key(&mut first)? {
        match k.as_str() {
            "_index" => index = Some(sc.string(MAX_NAME)?),
            "_id" => id = Some(sc.string(MAX_NAME)?),
            "_routing" => routing = Some(sc.string(MAX_NAME)?),
            "_version" => version = sc.small(64, "_version")?.as_i64().unwrap_or(-1),
            "_source" => {
                let (a, b) = sc.skip()?;
                source = match page[a] {
                    b'{' => Some(page.slice(a..b)),
                    b'n' if &page[a..b] == b"null" => None,
                    _ => return Err(EsError::java(500, "x_content_parse_exception", "[hit] failed to parse [_source]")),
                };
            }
            "sort" => sort = Some(sc.small(MAX_SORT, "sort")?),
            "fields" => {
                let f = sc.small(MAX_SMALL, "fields")?;
                if routing.is_none() {
                    routing = match f.get("_routing") {
                        Some(Value::Arr(a)) => a.first().and_then(Value::as_str).map(str::to_string),
                        Some(Value::Str(x)) => Some(x.clone()),
                        _ => None,
                    };
                }
            }
            _ => {
                sc.skip()?;
            }
        }
    }
    let (Some(index), Some(id)) = (index, id) else { return Err(bad("a hit without [_index] or [_id]")) };
    Ok((ScannedHit { index, id, routing, version, source }, sort))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_is_read_without_building_its_sources() {
        let page = Bytes::from_static(
            br#"{"pit_id":"p2","_shards":{"total":1,"failed":0},"hits":{"total":{"value":2,"relation":"eq"},"max_score":null,"hits":[
              {"_index":"src","_id":"1","_version":3,"_source":{"a":[1,2,{"b":"}]\"x"}]},"sort":[7]},
              {"_index":"src","_id":"2","_routing":"r","_source":null,"fields":{"_routing":["ignored"]},"sort":[9,"z"]}]}}"#,
        );
        let p = scan(&page, 4096).unwrap();
        assert_eq!((p.pit_id.as_deref(), p.total, p.hits.len(), p.has_hits), (Some("p2"), Some(2), 2, true));
        assert_eq!(p.hits[0].source.as_deref(), Some(&br#"{"a":[1,2,{"b":"}]\"x"}]}"#[..]));
        assert_eq!((p.hits[0].version, p.hits[1].routing.as_deref(), p.hits[1].source.is_none()), (3, Some("r"), true));
        assert_eq!(p.last_sort.unwrap().to_json(), r#"[9,"z"]"#);
    }

    #[test]
    fn bounds_and_broken_pages() {
        let long = format!(r#"{{"_scroll_id":"{}","hits":{{"hits":[]}}}}"#, "a".repeat(5000));
        assert!(scan(&Bytes::from(long), 4096).err().expect("refused").reason().contains("longer than the [4096] bytes"));
        for broken in [r#"{"hits":{"hits":[{"_index":"a","_id":"1","_source":{"a":1}"#, r#"{"hits":{"hits":[{"_id":"1"}]}}"#, r#"[1,2]"#] {
            assert!(scan(&Bytes::from(broken), 4096).is_err(), "{broken}");
        }
        let failed = Bytes::from_static(br#"{"_shards":{"failures":[{"reason":{"reason":"boom"}}]},"hits":{"hits":[]}}"#);
        assert_eq!(scan(&failed, 4096).unwrap().shard_failures, Some((1, "boom".to_string())));
        // Deep nesting is stepped over without recursion, up to the parser's limit.
        let deep = format!(r#"{{"hits":{{"hits":[{{"_index":"a","_id":"1","_source":{{"x":{}1{}}}}}]}}}}"#, "[".repeat(900), "]".repeat(900));
        assert_eq!(scan(&Bytes::from(deep), 4096).unwrap().hits.len(), 1);
        let deeper = format!(r#"{{"hits":{{"hits":[{{"_index":"a","_id":"1","_source":{{"x":{}1{}}}}}]}}}}"#, "[".repeat(1100), "]".repeat(1100));
        assert!(scan(&Bytes::from(deeper), 4096).is_err());
    }
}
