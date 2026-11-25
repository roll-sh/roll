//! A copy from Algolia, Typesense or Meilisearch into one index: the source's documents page by
//! page, each page written with `_bulk` before the next is read, its progress on the task's
//! [`Live`] counters ([`super`] registers the task and runs [`Copy::run`] in the background).
//! A copy saves its place in the index's import record ([`super::later::Imported`]); a catch-up
//! ([`Mode::CatchUp`]) compares each page with what roll holds and writes only what differs.

use super::later::{now_ms, Imported, Position, SAVE_EVERY};
use super::{algolia, typesense, LABEL, MAX_ID_BYTES, META};
use crate::migrate::Kind;
use crate::control::Control;
use crate::store::SearchCluster;
use search_es::api::by_query::{Live, Status};
use search_es::api::remote::{self, Client, Lines};
use search_es::EsRequest;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

/// The most bytes of documents one `_bulk` carries (the Typesense export's batches).
const BATCH_BYTES: usize = 8 * 1024 * 1024;

/// The task's stored response, from its counters (`BulkByScrollResponse`).
pub(super) fn response(live: &Live, started: std::time::Instant) -> search_es::json::Value {
    let mut out = None;
    live.update(|s: &mut Status| out = Some(s.response(search_es::api::by_query::Kind::Reindex, started.elapsed().as_millis() as u64, true)));
    out.expect("set")
}

/// Where a copy reads its documents (`filter`: a catch-up's records changed since).
pub(super) enum Reader {
    /// `POST /1/indexes/{index}/browse`, then its `cursor` until there's none.
    Algolia { index: String, cursor: Option<String>, first: bool, done: bool, seen: HashSet<u64>, filter: Option<String> },
    /// `GET /collections/{c}/documents/export`, line by line; the first `skip` lines (a resumed
    /// copy's, already written) are read and passed over.
    Typesense { collection: String, lines: Option<Box<Lines>>, geo: Vec<String>, filter: Option<String>, skip: u64, consumed: u64 },
    /// `GET /indexes/{uid}/documents?offset&limit` until a page comes back short.
    Meili { uid: String, offset: u64, done: bool, filter: Option<String> },
}

impl Reader {
    /// A reader of `index` from its start or from `at` (a stopped copy's place).
    pub(super) fn new(kind: Kind, index: &str, at: Option<Position>, filter: Option<String>, geo: Vec<String>) -> Reader {
        let index = index.to_string();
        match kind {
            Kind::Algolia => {
                let cursor = match at {
                    Some(Position::Cursor(c)) => Some(c),
                    _ => None,
                };
                Reader::Algolia { first: cursor.is_none(), index, cursor, done: false, seen: HashSet::new(), filter }
            }
            Kind::Typesense => {
                let skip = match at {
                    Some(Position::Lines(n)) => n,
                    _ => 0,
                };
                Reader::Typesense { collection: index, lines: None, geo, filter, skip, consumed: 0 }
            }
            _ => {
                let offset = match at {
                    Some(Position::Offset(n)) => n,
                    _ => 0,
                };
                Reader::Meili { uid: index, offset, done: false, filter }
            }
        }
    }

    /// The next page's place: where a copy stopped now would resume.
    fn position(&self) -> Option<Position> {
        match self {
            Reader::Algolia { cursor: Some(c), .. } => Some(Position::Cursor(c.clone())),
            Reader::Algolia { first: true, .. } => None,
            Reader::Algolia { .. } => None,
            Reader::Typesense { consumed, skip, .. } => Some(Position::Lines((*consumed).max(*skip))),
            Reader::Meili { offset, .. } => Some(Position::Offset(*offset)),
        }
    }
}

/// What a run does.
pub(super) enum Mode {
    /// The first copy (or a resumed one): writes every document, saves its place.
    Copy,
    /// A catch-up started at `started_ms`: writes only documents that differ from roll's
    /// (top-level `excludes` left out of the comparison: roll doesn't keep them).
    CatchUp { started_ms: i64, excludes: Vec<String> },
}

/// One copy: the source's documents into `dest`, page by page.
pub(super) struct Copy {
    pub reader: Reader,
    pub client: Client,
    pub id_field: String,
    pub dest: String,
    pub size: u32,
    pub total: Option<u64>,
    /// Documents read so far, and how many the source first said it holds: one that sends
    /// far more is stopped.
    pub read: u64,
    pub stated: Option<u64>,
    pub mode: Mode,
    /// The index's import record, saved as the copy goes.
    pub meta: Imported,
    /// The place after the last page written: where the copy resumes if it stops.
    pub resume_at: Option<Position>,
}

/// A page's documents (`_id`, source JSON), charged while held.
struct Batch {
    docs: Vec<(String, String)>,
    /// Documents that couldn't be written (no usable id), as task failures.
    failures: Vec<search_es::json::Value>,
    _held: search_es::breaker::Reservation,
}

/// `_bulk` requests sent again after a 429 (Elasticsearch's reindex: 11).
const BULK_RETRIES: u32 = 11;
/// An Algolia cursor longer than this is refused (one is a few hundred bytes).
const MAX_CURSOR: usize = 16 * 1024;
/// Keys Algolia adds to a hit that aren't the record.
const DECORATIONS: &[&str] = &["_highlightResult", "_snippetResult", "_rankingInfo", "_distinctSeqID"];

impl Copy {
    /// Pages until the source has no more, the task is cancelled, or a page fails; the
    /// counters on `live` say how far it got, and the index's import record where it stands.
    pub(super) async fn run(mut self, me: &Control, c: &SearchCluster, live: &Arc<Live>) -> std::result::Result<(), search_es::EsError> {
        if let Some(t) = self.total {
            live.update(|s| s.total = t);
        }
        if matches!(self.mode, Mode::Copy) {
            self.resume_at = self.resume_at.clone().or_else(|| self.reader.position());
            self.meta.copy.state = "copying".into();
            me.save_import(c, &self.dest, &self.meta).await;
        }
        let result = self.pages(me, c, live).await;
        let finished = matches!(result, Ok(true));
        match &self.mode {
            Mode::Copy => {
                self.meta.copy.state = if finished { "done" } else { "stopped" }.into();
                self.meta.copy.position = if finished { None } else { self.resume_at.clone() };
                if finished {
                    self.meta.copy.done_ms = Some(now_ms());
                }
            }
            Mode::CatchUp { started_ms, .. } => {
                if finished {
                    self.meta.caught_up_ms = Some(*started_ms);
                }
            }
        }
        me.save_import(c, &self.dest, &self.meta).await;
        let _ = me.call(c, EsRequest::new("POST", &format!("/{}/_refresh", urlencoding::encode(&self.dest)))).await;
        result.map(|_| ())
    }

    /// The pages: `true` once the source has no more, `false` when the run stopped early
    /// (cancelled, a page that failed).
    async fn pages(&mut self, me: &Control, c: &SearchCluster, live: &Arc<Live>) -> std::result::Result<bool, search_es::EsError> {
        let node = me.node.reindex().clone();
        let mut saved = std::time::Instant::now();
        loop {
            if live.cancelled() {
                let why = live.cancel_reason();
                live.update(|s| s.canceled = Some(why));
                return Ok(false);
            }
            let started = std::time::Instant::now();
            let Some(batch) = self.next(&node, live).await? else { return Ok(true) };
            if !batch.failures.is_empty() {
                live.update(|s| {
                    s.failures.extend(batch.failures.iter().cloned());
                    s.failure_status = 400;
                });
                return Ok(false);
            }
            if batch.docs.is_empty() {
                self.written(me, c, &mut saved).await;
                continue;
            }
            // A cancel that came while the page was read: nothing more is written.
            if live.cancelled() {
                let why = live.cancel_reason();
                live.update(|s| s.canceled = Some(why));
                return Ok(false);
            }
            let (docs, unchanged) = self.changed(me, c, batch.docs).await;
            if unchanged > 0 {
                live.update(|s| s.noops += unchanged);
            }
            if docs.is_empty() {
                self.written(me, c, &mut saved).await;
                continue;
            }
            let n_docs = docs.len() as u64;
            let mut text = String::new();
            for (id, src) in &docs {
                text.push_str(r#"{"index":{"_id":"#);
                text.push_str(&serde_json::to_string(id).expect("a string renders"));
                text.push_str("}}\n");
                text.push_str(src);
                text.push('\n');
            }
            drop(docs);
            let body: bytes::Bytes = text.into_bytes().into();
            let path = format!("/{}/_bulk", urlencoding::encode(&self.dest));
            // A busy cluster (its in-flight limit, its write pool) answers 429: sent again with
            // Elasticsearch's backoff, as reindex retries a rejected bulk.
            let mut attempt = 0u32;
            let resp = loop {
                let mut req = EsRequest::new("POST", &path);
                req.headers.push(("Content-Type".into(), "application/x-ndjson".into()));
                req.body = body.clone();
                let resp = me.call(c, req).await;
                if resp.status != 429 || attempt >= BULK_RETRIES || live.cancelled() {
                    break resp;
                }
                live.pause(std::time::Duration::from_millis(500 + 10 * (((0.8 * f64::from(attempt)).exp() as u64).saturating_sub(1)))).await;
                attempt += 1;
            };
            let (created, updated, failures) = bulk_outcome(resp.status, &resp.body, &self.dest);
            let failed = !failures.is_empty();
            live.update(|s| {
                s.batches += 1;
                s.created += created;
                s.updated += updated;
                if failed {
                    s.failures.extend(failures);
                    s.failure_status = 400;
                }
                if s.total < s.created + s.updated {
                    s.total = s.created + s.updated;
                }
            });
            // Elasticsearch's reindex stops at a failed batch: so does the copy.
            if failed {
                return Ok(false);
            }
            self.written(me, c, &mut saved).await;
            let rps = live.rate();
            if rps > 0.0 && rps.is_finite() {
                let want = std::time::Duration::from_secs_f64((n_docs as f64 / rps).min(3600.0));
                if let Some(wait) = want.checked_sub(started.elapsed()) {
                    live.pause(wait).await;
                }
            }
        }
    }

    /// A page is written: the copy's place moves past it, and is saved when it's been a while.
    async fn written(&mut self, me: &Control, c: &SearchCluster, saved: &mut std::time::Instant) {
        if !matches!(self.mode, Mode::Copy) {
            return;
        }
        self.resume_at = self.reader.position();
        if saved.elapsed() >= SAVE_EVERY {
            self.meta.copy.position = self.resume_at.clone();
            me.save_import(c, &self.dest, &self.meta).await;
            *saved = std::time::Instant::now();
        }
    }

    /// A catch-up's page less the documents roll already holds as they are (`_mget`), and how
    /// many those were; a copy's page as it is.
    async fn changed(&self, me: &Control, c: &SearchCluster, docs: Vec<(String, String)>) -> (Vec<(String, String)>, u64) {
        let Mode::CatchUp { excludes, .. } = &self.mode else { return (docs, 0) };
        let ids: Vec<&str> = docs.iter().map(|(id, _)| id.as_str()).collect();
        let (status, v) = me.call_json(c, "POST", &format!("/{}/_mget", urlencoding::encode(&self.dest)), Some(&json!({ "ids": ids }))).await;
        if status != 200 {
            return (docs, 0);
        }
        let mut held: std::collections::HashMap<&str, &Value> = std::collections::HashMap::new();
        for d in v["docs"].as_array().into_iter().flatten() {
            if let (Some(id), Some(src)) = (d["_id"].as_str(), d.get("_source").filter(|_| d["found"] == true)) {
                held.insert(id, src);
            }
        }
        let mut out = Vec::new();
        let mut same = 0;
        for (id, src) in docs.iter() {
            let unchanged = held.get(id.as_str()).is_some_and(|roll| {
                let Ok(Value::Object(mut ours)) = serde_json::from_str::<Value>(src) else { return false };
                for e in excludes {
                    ours.remove(e);
                }
                Value::Object(ours) == **roll
            });
            if unchanged {
                same += 1;
            } else {
                out.push((id.clone(), src.clone()));
            }
        }
        (out, same)
    }

    /// The next batch, or `None` at the end.
    async fn next(&mut self, node: &remote::RemoteNode, live: &Arc<Live>) -> std::result::Result<Option<Batch>, search_es::EsError> {
        let mut held = search_es::breaker::Reservation::new(node.budget.clone());
        let mut body: Vec<(String, String)> = Vec::new();
        let mut failures = Vec::new();
        match &mut self.reader {
            Reader::Algolia { index, cursor, first, done, seen, filter } => {
                if *done {
                    return Ok(None);
                }
                let req = match cursor.take() {
                    Some(c) => json!({ "cursor": c }),
                    None if *first => match filter {
                        Some(f) => json!({ "hitsPerPage": self.size, "numericFilters": [f] }),
                        None => json!({ "hitsPerPage": self.size }),
                    },
                    None => return Ok(None),
                };
                *first = false;
                let page = self.client.post(&format!("/1/indexes/{}/browse", algolia::enc(index)), &req.to_string()).await?;
                if let Some(n) = page.value.get("nbHits").and_then(search_es::json::Value::as_number).and_then(|n| n.as_i64()).and_then(|n| u64::try_from(n).ok()) {
                    live.update(|s| s.total = s.total.max(n));
                    self.stated.get_or_insert(n);
                }
                match page.value.get("cursor").and_then(search_es::json::Value::as_str) {
                    // A cursor seen before would read the same pages again, forever.
                    Some(c) if !seen.insert(hash(c)) => return Err(search_es::EsError::illegal_state("Algolia's browse cursor came back to a page it already read")),
                    Some(c) if c.len() <= MAX_CURSOR => *cursor = Some(c.to_string()),
                    Some(_) => return Err(search_es::EsError::illegal_state("Algolia's browse cursor is longer than roll keeps")),
                    None => *done = true,
                }
                let hits = page.value.get("hits").and_then(search_es::json::Value::as_array).map(Vec::as_slice).unwrap_or_default();
                self.read += hits.len() as u64;
                past_total(self.read, self.stated)?;
                for hit in hits {
                    push_doc(hit, &self.id_field, &self.dest, DECORATIONS, &mut body, &mut held, &mut failures)?;
                }
            }
            Reader::Meili { uid, offset, done, filter } => {
                if *done {
                    return Ok(None);
                }
                let filter = filter.as_ref().map(|f| format!("&filter={}", urlencoding::encode(f))).unwrap_or_default();
                let page = self.client.get(&format!("/indexes/{}/documents?offset={}&limit={}{filter}", urlencoding::encode(uid), offset, self.size)).await?;
                let hits = page.value.get("results").and_then(search_es::json::Value::as_array).map(Vec::as_slice).unwrap_or_default();
                if (hits.len() as u64) < u64::from(self.size) {
                    *done = true;
                }
                *offset += hits.len() as u64;
                self.read += hits.len() as u64;
                if let Some(n) = page.value.get("total").and_then(search_es::json::Value::as_number).and_then(|n| n.as_i64()).and_then(|n| u64::try_from(n).ok()) {
                    live.update(|s| s.total = s.total.max(n));
                    self.stated.get_or_insert(n);
                }
                past_total(self.read, self.stated)?;
                for hit in hits {
                    push_doc(hit, &self.id_field, &self.dest, &[], &mut body, &mut held, &mut failures)?;
                }
                if hits.is_empty() {
                    return Ok(None);
                }
            }
            Reader::Typesense { collection, lines, geo, filter, skip, consumed } => {
                if lines.is_none() {
                    let filter = filter.as_ref().map(|f| format!("?filter_by={}", urlencoding::encode(f))).unwrap_or_default();
                    let path = format!("/collections/{}/documents/export{filter}", urlencoding::encode(collection));
                    *lines = Some(Box::new(self.client.lines(&path, std::time::Duration::from_secs(24 * 3600)).await?));
                }
                let l = lines.as_mut().expect("set");
                let mut bytes = 0usize;
                let mut ended = false;
                while (body.len() as u64) < u64::from(self.size) && bytes < BATCH_BYTES {
                    let Some(line) = l.next().await? else {
                        ended = true;
                        break;
                    };
                    self.read += 1;
                    *consumed += 1;
                    past_total(self.read, self.stated)?;
                    // A resumed copy passes over the lines it already wrote.
                    if *consumed <= *skip {
                        continue;
                    }
                    bytes += line.len();
                    held.add(3 * line.len() as u64, LABEL)?;
                    let Some(mut doc) = search_es::json::parse(&line).ok().flatten() else {
                        failures.push(failure(&self.dest, "", "a line of the export isn't a JSON document"));
                        continue;
                    };
                    drop(line);
                    for g in geo.iter() {
                        typesense::geo_point(&mut doc, g);
                    }
                    push_doc(&doc, &self.id_field, &self.dest, &[], &mut body, &mut held, &mut failures)?;
                }
                if ended && body.is_empty() && failures.is_empty() {
                    return Ok(None);
                }
            }
        }
        Ok(Some(Batch { docs: body, failures, _held: held }))
    }
}

/// Adds one document (its `_id` from `id_field`, its source without the keys in `skip` and
/// metadata-field names) to `out`, charged as it's added; a document without a usable id is a
/// failure.
fn push_doc(doc: &search_es::json::Value, id_field: &str, dest: &str, skip: &[&str], out: &mut Vec<(String, String)>, held: &mut search_es::breaker::Reservation, failures: &mut Vec<search_es::json::Value>) -> search_es::EsResult<()> {
    let Some(m) = doc.as_object() else {
        failures.push(failure(dest, "", "a document that isn't an object"));
        return Ok(());
    };
    let id = match m.get(id_field) {
        Some(search_es::json::Value::Str(s)) => s.clone(),
        Some(v @ search_es::json::Value::Num(_)) => v.to_es_string(),
        _ => String::new(),
    };
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        failures.push(failure(dest, "", &format!("a document without a usable [{id_field}] (its _id: 1 to {MAX_ID_BYTES} bytes)")));
        return Ok(());
    }
    let mut body = String::from("{");
    let mut first = true;
    for (k, v) in m.iter().filter(|(k, _)| !skip.contains(&k.as_str()) && !META.contains(&k.as_str())) {
        if !std::mem::take(&mut first) {
            body.push(',');
        }
        body.push_str(&serde_json::to_string(k).expect("a string renders"));
        body.push(':');
        body.push_str(&v.to_json());
    }
    body.push('}');
    // The source and its `_bulk` line (made from it later), and a catch-up's comparison.
    held.add(3 * (body.len() + id.len()) as u64 + 64, LABEL)?;
    out.push((id, body));
    Ok(())
}

/// A short hash of a cursor, to tell one already seen.
fn hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Refuses to read on once a source has sent more than twice the documents it said it holds
/// (and a page's worth more): its pages would never end.
fn past_total(read: u64, stated: Option<u64>) -> search_es::EsResult<()> {
    let Some(total) = stated else { return Ok(()) };
    if read > 2 * total + u64::from(super::PAGE_DOCS) {
        return Err(search_es::EsError::illegal_state(format!("the source sent {read} documents after saying it holds {total}: stopped")));
    }
    Ok(())
}

fn failure(index: &str, id: &str, reason: &str) -> search_es::json::Value {
    let v = json!({ "index": index, "id": id, "cause": { "type": "illegal_argument_exception", "reason": reason }, "status": 400 });
    search_es::json::parse(v.to_string().as_bytes()).ok().flatten().unwrap_or_default()
}

/// Created and updated documents of a `_bulk` answer, and its failed items (at most 100).
fn bulk_outcome(status: u16, body: &[u8], dest: &str) -> (u64, u64, Vec<search_es::json::Value>) {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return (0, 0, vec![failure(dest, "", &format!("writing the page failed ({status})"))]);
    };
    if status != 200 {
        let reason = v.pointer("/error/reason").and_then(Value::as_str).unwrap_or("writing the page failed");
        return (0, 0, vec![failure(dest, "", reason)]);
    }
    let (mut created, mut updated, mut failures) = (0, 0, Vec::new());
    for item in v["items"].as_array().into_iter().flatten() {
        let r = &item["index"];
        match r["result"].as_str() {
            Some("created") => created += 1,
            Some("updated") | Some("noop") => updated += 1,
            _ if failures.len() < 100 => {
                let f = json!({ "index": dest, "id": r["_id"], "cause": r["error"], "status": r["status"] });
                failures.extend(search_es::json::parse(f.to_string().as_bytes()).ok().flatten());
            }
            _ => {}
        }
    }
    (created, updated, failures)
}

