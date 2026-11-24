//! A copy from Algolia, Typesense or Meilisearch into one index: the source's documents page by
//! page, each page written with `_bulk` before the next is read, its progress on the task's
//! [`Live`] counters ([`super`] registers the task and runs [`Copy::run`] in the background).

use super::{algolia, typesense, LABEL, MAX_ID_BYTES};
use crate::control::Control;
use crate::store::SearchCluster;
use search_es::api::by_query::{Live, Status};
use search_es::api::remote::{self, Client, Lines};
use search_es::EsRequest;
use serde_json::{json, Value};
use std::sync::Arc;

/// The most bytes of documents one `_bulk` carries (the Typesense export's batches).
const BATCH_BYTES: usize = 8 * 1024 * 1024;

/// The task's stored response, from its counters (`BulkByScrollResponse`).
pub(super) fn response(live: &Live, started: std::time::Instant) -> search_es::json::Value {
    let mut out = None;
    live.update(|s: &mut Status| out = Some(s.response(search_es::api::by_query::Kind::Reindex, started.elapsed().as_millis() as u64, true)));
    out.expect("set")
}

/// Where a copy reads its documents.
pub(super) enum Reader {
    /// `POST /1/indexes/{index}/browse`, then its `cursor` until there's none.
    Algolia { index: String, cursor: Option<String>, first: bool, done: bool },
    /// `GET /collections/{c}/documents/export`, line by line.
    Typesense { collection: String, lines: Option<Box<Lines>>, geo: Vec<String> },
    /// `GET /indexes/{uid}/documents?offset&limit` until a page comes back short.
    Meili { uid: String, offset: u64, done: bool },
}

/// One copy: the source's documents into `dest`, page by page.
pub(super) struct Copy {
    pub reader: Reader,
    pub client: Client,
    pub id_field: String,
    pub dest: String,
    pub size: u32,
    pub total: Option<u64>,
}

/// A page made into a `_bulk` body, charged while held.
struct Batch {
    body: String,
    docs: u64,
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
    /// counters on `live` say how far it got.
    pub(super) async fn run(mut self, me: &Control, c: &SearchCluster, live: &Arc<Live>) -> std::result::Result<(), search_es::EsError> {
        if let Some(t) = self.total {
            live.update(|s| s.total = t);
        }
        let node = me.node.reindex().clone();
        loop {
            if live.cancelled() {
                let why = live.cancel_reason();
                live.update(|s| s.canceled = Some(why));
                break;
            }
            let started = std::time::Instant::now();
            let Some(batch) = self.next(&node, live).await? else { break };
            let docs = batch.docs;
            if !batch.failures.is_empty() {
                live.update(|s| {
                    s.failures.extend(batch.failures.iter().cloned());
                    s.failure_status = 400;
                });
                break;
            }
            if docs == 0 {
                continue;
            }
            let body: bytes::Bytes = batch.body.into_bytes().into();
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
                break;
            }
            let rps = live.rate();
            if rps > 0.0 && rps.is_finite() {
                let want = std::time::Duration::from_secs_f64((docs as f64 / rps).min(3600.0));
                if let Some(wait) = want.checked_sub(started.elapsed()) {
                    live.pause(wait).await;
                }
            }
        }
        let _ = me.call(c, EsRequest::new("POST", &format!("/{}/_refresh", urlencoding::encode(&self.dest)))).await;
        Ok(())
    }

    /// The next batch, or `None` at the end.
    async fn next(&mut self, node: &remote::RemoteNode, live: &Arc<Live>) -> std::result::Result<Option<Batch>, search_es::EsError> {
        let mut held = search_es::breaker::Reservation::new(node.budget.clone());
        let mut body = String::new();
        let mut failures = Vec::new();
        let mut docs = 0u64;
        match &mut self.reader {
            Reader::Algolia { index, cursor, first, done } => {
                if *done {
                    return Ok(None);
                }
                let req = match cursor.take() {
                    Some(c) => json!({ "cursor": c }),
                    None if *first => json!({ "hitsPerPage": self.size }),
                    None => return Ok(None),
                };
                *first = false;
                let page = self.client.post(&format!("/1/indexes/{}/browse", algolia::enc(index)), &req.to_string()).await?;
                if let Some(n) = page.value.get("nbHits").and_then(search_es::json::Value::as_number).and_then(|n| n.as_i64()).and_then(|n| u64::try_from(n).ok()) {
                    live.update(|s| s.total = s.total.max(n));
                }
                let sent = req["cursor"].as_str();
                match page.value.get("cursor").and_then(search_es::json::Value::as_str) {
                    Some(c) if sent == Some(c) => return Err(search_es::EsError::illegal_state("Algolia's browse cursor didn't move")),
                    Some(c) if c.len() <= MAX_CURSOR => *cursor = Some(c.to_string()),
                    Some(_) => return Err(search_es::EsError::illegal_state("Algolia's browse cursor is longer than roll keeps")),
                    None => *done = true,
                }
                for hit in page.value.get("hits").and_then(search_es::json::Value::as_array).map(Vec::as_slice).unwrap_or_default() {
                    push_doc(hit, &self.id_field, &self.dest, DECORATIONS, &mut body, &mut held, &mut failures, &mut docs)?;
                }
            }
            Reader::Meili { uid, offset, done } => {
                if *done {
                    return Ok(None);
                }
                let page = self.client.get(&format!("/indexes/{}/documents?offset={}&limit={}", urlencoding::encode(uid), offset, self.size)).await?;
                let hits = page.value.get("results").and_then(search_es::json::Value::as_array).map(Vec::as_slice).unwrap_or_default();
                if (hits.len() as u64) < u64::from(self.size) {
                    *done = true;
                }
                *offset += hits.len() as u64;
                for hit in hits {
                    push_doc(hit, &self.id_field, &self.dest, &[], &mut body, &mut held, &mut failures, &mut docs)?;
                }
                if hits.is_empty() {
                    return Ok(None);
                }
            }
            Reader::Typesense { collection, lines, geo } => {
                if lines.is_none() {
                    let path = format!("/collections/{}/documents/export", urlencoding::encode(collection));
                    *lines = Some(Box::new(self.client.lines(&path, std::time::Duration::from_secs(24 * 3600)).await?));
                }
                let l = lines.as_mut().expect("set");
                while docs < u64::from(self.size) && body.len() < BATCH_BYTES {
                    let Some(line) = l.next().await? else { break };
                    held.add(3 * line.len() as u64, LABEL)?;
                    let Some(mut doc) = search_es::json::parse(&line).ok().flatten() else {
                        failures.push(failure(&self.dest, "", "a line of the export isn't a JSON document"));
                        continue;
                    };
                    drop(line);
                    for g in geo.iter() {
                        typesense::geo_point(&mut doc, g);
                    }
                    push_doc(&doc, &self.id_field, &self.dest, &[], &mut body, &mut held, &mut failures, &mut docs)?;
                }
                if docs == 0 && failures.is_empty() {
                    return Ok(None);
                }
            }
        }
        Ok(Some(Batch { body, docs, failures, _held: held }))
    }
}

/// Adds one document's `_bulk` lines to `body` (its `_id` from `id_field`, keys in `skip`
/// left out), charged as it grows; a document without a usable id is a failure.
#[allow(clippy::too_many_arguments)]
fn push_doc(doc: &search_es::json::Value, id_field: &str, dest: &str, skip: &[&str], body: &mut String, held: &mut search_es::breaker::Reservation, failures: &mut Vec<search_es::json::Value>, docs: &mut u64) -> search_es::EsResult<()> {
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
    let start = body.len();
    body.push_str(r#"{"index":{"_id":"#);
    body.push_str(&serde_json::to_string(&id).expect("a string renders"));
    body.push_str("}}\n{");
    let mut first = true;
    for (k, v) in m.iter().filter(|(k, _)| !skip.contains(&k.as_str())) {
        if !std::mem::take(&mut first) {
            body.push(',');
        }
        body.push_str(&serde_json::to_string(k).expect("a string renders"));
        body.push(':');
        body.push_str(&v.to_json());
    }
    body.push_str("}\n");
    held.add((body.len() - start) as u64, LABEL)?;
    *docs += 1;
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

