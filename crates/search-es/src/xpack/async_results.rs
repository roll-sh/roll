//! Results of asynchronous requests (async search, ES|QL / SQL / EQL async queries) and SQL
//! cursors, kept while the cluster is loaded.
//!
//! Elasticsearch stores them in the `.async-search` system index for `keep_alive` (5 days by
//! default); roll keeps them in memory (they are results of requests, not data), expiring them
//! on access past their `keep_alive`, and bounds their number and total size so a client that
//! never deletes them can't grow a cluster without limit. Every query runs to completion before
//! its submit request answers, so stored results are always complete.
//!
//! A durable cluster unloads when idle, which an Elasticsearch node never does, so there the
//! results also go to the `.async-search` system index, as on Elasticsearch: once
//! [`Results::track_changes`] is on, every id put, touched or removed is noted, and the server
//! writes the noted entries to the index before it acknowledges the request that changed them
//! (and before unloading); a wake reads them back (search-es `persist_hooks`).

use crate::json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

/// The kind of request a result belongs to (ids aren't valid across kinds, as on Elasticsearch).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Search,
    Esql,
    Sql,
    Eql,
    SqlCursor,
}

/// A stored result.
#[derive(Clone, Debug)]
pub struct Entry {
    pub kind: Kind,
    /// The response to return (without `id`/`is_running` decoration).
    pub response: Value,
    pub start_ms: i64,
    pub expiration_ms: i64,
    /// Bytes this entry accounts for (its JSON size).
    pub bytes: usize,
    /// Extra state a kind needs (SQL cursors: the remaining rows and columns).
    pub extra: Value,
}

/// At most this many stored results per cluster...
const MAX_ENTRIES: usize = 1024;
/// ...and this many bytes of them.
const MAX_BYTES: usize = 256 << 20;

#[derive(Default)]
pub struct Results {
    inner: parking_lot::Mutex<Inner>,
    /// Some change is noted ([`Results::has_changes`], checked after every request).
    changed: AtomicBool,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    bytes: usize,
    /// Note changed ids ([`Results::track_changes`]); in memory nobody would take them.
    track: bool,
    /// Ids put, touched or removed since the last [`Results::take_changes`].
    dirty: HashSet<String>,
}

impl Inner {
    fn expire(&mut self, now: i64) {
        let dead: Vec<String> = self.entries.iter().filter(|(_, e)| e.expiration_ms <= now).map(|(k, _)| k.clone()).collect();
        for k in dead {
            if let Some(e) = self.entries.remove(&k) {
                self.bytes -= e.bytes;
                self.note(&k);
            }
        }
    }

    fn note(&mut self, id: &str) {
        if self.track && !self.dirty.contains(id) {
            self.dirty.insert(id.to_string());
        }
    }

    /// Drops the entries that expire first until `need` more bytes and one more entry fit.
    fn make_room(&mut self, need: usize) {
        while !self.entries.is_empty() && (self.entries.len() >= MAX_ENTRIES || self.bytes + need > MAX_BYTES) {
            let Some(k) = self.entries.iter().min_by_key(|(_, e)| e.expiration_ms).map(|(k, _)| k.clone()) else { break };
            if let Some(e) = self.entries.remove(&k) {
                self.bytes -= e.bytes;
                self.note(&k);
            }
        }
    }
}

/// A new result id: Elasticsearch's are base64 of a document id and a task id; roll's are 30
/// random bytes in the same alphabet.
pub fn new_id() -> String {
    use base64::Engine;
    let bytes: [u8; 30] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// An id in Elasticsearch's `AsyncExecutionId` format: URL-safe base64 (padded) of the stored
/// document's id and the task id (`<node id>:<task number>`), each a vint-length-prefixed string.
pub fn async_id(node_id: &str) -> String {
    use base64::Engine;
    use std::sync::atomic::{AtomicU64, Ordering};
    static TASK: AtomicU64 = AtomicU64::new(1);
    let doc_bytes: [u8; 16] = rand::random();
    let doc = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(doc_bytes);
    let task = format!("{node_id}:{}", TASK.fetch_add(1, Ordering::Relaxed));
    let mut raw = Vec::with_capacity(doc.len() + task.len() + 4);
    for s in [&doc, &task] {
        write_vint(&mut raw, s.len() as u32);
        raw.extend_from_slice(s.as_bytes());
    }
    base64::engine::general_purpose::URL_SAFE.encode(raw)
}

fn write_vint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Checks that an id decodes as an `AsyncExecutionId`, failing like Elasticsearch
/// (`invalid id: [<id>]` caused by the stream error) when it doesn't.
pub fn check_id(id: &str) -> Result<(), crate::error::EsError> {
    task_of(id).map(|_| ())
}

/// The task id (`<node id>:<task number>`) an `AsyncExecutionId` holds, failing like
/// [`check_id`].
pub fn task_of(id: &str) -> Result<String, crate::error::EsError> {
    use base64::Engine;
    let invalid = |cause: crate::error::EsError| crate::error::EsError::illegal_argument(format!("invalid id: [{id}]")).caused_by(cause);
    let cfg = base64::engine::GeneralPurposeConfig::new().with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent).with_decode_allow_trailing_bits(true);
    let lenient = base64::engine::GeneralPurpose::new(&base64::alphabet::URL_SAFE, cfg);
    let raw = lenient.decode(id.trim_end_matches('=')).map_err(|e| invalid(crate::error::EsError::illegal_argument(e.to_string())))?;
    let mut pos = 0usize;
    let mut task = String::new();
    for _ in 0..2 {
        let mut len = 0u32;
        let mut shift = 0;
        loop {
            let Some(&b) = raw.get(pos) else {
                return Err(invalid(crate::error::EsError::new(400, "e_o_f_exception", "tried to read: 1 bytes but only 0 remaining")));
            };
            pos += 1;
            len |= ((b & 0x7f) as u32) << shift;
            shift += 7;
            if b & 0x80 == 0 || shift > 28 {
                break;
            }
        }
        let rest = raw.len() - pos;
        if (len as usize) > rest {
            return Err(invalid(crate::error::EsError::new(400, "e_o_f_exception", format!("tried to read: {len} bytes but only {rest} remaining"))));
        }
        task = String::from_utf8_lossy(&raw[pos..pos + len as usize]).into_owned();
        pos += len as usize;
    }
    if pos != raw.len() {
        return Err(crate::error::EsError::illegal_argument(format!("invalid id: [{id}]"))
            .caused_by(crate::error::EsError::illegal_argument(format!("invalid id: [{id}]"))));
    }
    Ok(task)
}

impl Results {
    pub fn put(&self, id: String, mut entry: Entry, now: i64) {
        entry.bytes = entry.response.to_json().len() + entry.extra.to_json().len();
        let mut g = self.inner.lock();
        g.expire(now);
        if let Some(old) = g.entries.remove(&id) {
            g.bytes -= old.bytes;
        }
        g.make_room(entry.bytes);
        g.bytes += entry.bytes;
        g.note(&id);
        g.entries.insert(id, entry);
        self.flag(&g);
    }

    /// A live result of the given kind.
    pub fn get(&self, id: &str, kind: Kind, now: i64) -> Option<Entry> {
        let mut g = self.inner.lock();
        g.expire(now);
        g.entries.get(id).filter(|e| e.kind == kind).cloned()
    }

    /// Extends a result's life (`keep_alive` on get).
    pub fn touch(&self, id: &str, expiration_ms: i64) {
        let mut g = self.inner.lock();
        if let Some(e) = g.entries.get_mut(id)
            && expiration_ms > e.expiration_ms
        {
            e.expiration_ms = expiration_ms;
            g.note(id);
            self.flag(&g);
        }
    }

    pub fn remove(&self, id: &str, kind: Kind) -> bool {
        let mut g = self.inner.lock();
        match g.entries.get(id) {
            Some(e) if e.kind == kind => {
                let e = g.entries.remove(id).expect("present");
                g.bytes -= e.bytes;
                g.note(id);
                self.flag(&g);
                true
            }
            _ => false,
        }
    }

    fn flag(&self, g: &Inner) {
        if !g.dirty.is_empty() {
            self.changed.store(true, Ordering::Release);
        }
    }

    /// Notes every change from now on, for [`Results::take_changes`] (a durable cluster).
    pub fn track_changes(&self) {
        self.inner.lock().track = true;
    }

    /// Whether changes are noted that [`Results::take_changes`] would return (one atomic load).
    pub fn has_changes(&self) -> bool {
        self.changed.load(Ordering::Acquire)
    }

    /// The ids changed since the last call, each with its live entry (`None`: removed or
    /// expired), sorted by id; the notes are cleared.
    pub fn take_changes(&self, now: i64) -> Vec<(String, Option<Entry>)> {
        let mut g = self.inner.lock();
        g.expire(now);
        self.changed.store(false, Ordering::Release);
        let dirty = std::mem::take(&mut g.dirty);
        let mut out: Vec<(String, Option<Entry>)> = dirty.into_iter().map(|id| { let e = g.entries.get(&id).cloned(); (id, e) }).collect();
        out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Notes `ids` as changed again (writing them failed: the next call retries).
    pub fn note_changed(&self, ids: impl IntoIterator<Item = String>) {
        let mut g = self.inner.lock();
        for id in ids {
            g.note(&id);
        }
        self.flag(&g);
    }

    /// Notes every live entry as changed (their copies were lost: rewrite them all).
    pub fn note_all(&self) {
        let mut g = self.inner.lock();
        if g.track {
            let ids: Vec<String> = g.entries.keys().cloned().collect();
            g.dirty.extend(ids);
        }
        self.flag(&g);
    }

    /// Puts an entry read back from storage, without noting a change (unless it doesn't fit
    /// or has expired: then its stored copy is noted for removal).
    pub fn restore(&self, id: String, mut entry: Entry, now: i64) {
        let mut g = self.inner.lock();
        if entry.expiration_ms <= now {
            g.note(&id);
            self.flag(&g);
            return;
        }
        entry.bytes = entry.response.to_json().len() + entry.extra.to_json().len();
        if let Some(old) = g.entries.remove(&id) {
            g.bytes -= old.bytes;
        }
        g.make_room(entry.bytes);
        g.bytes += entry.bytes;
        g.entries.insert(id, entry);
        self.flag(&g);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj;

    fn entry(kind: Kind, exp: i64) -> Entry {
        Entry { kind, response: obj! { "a" => 1 }, start_ms: 0, expiration_ms: exp, bytes: 0, extra: Value::Null }
    }

    #[test]
    fn put_get_expire_remove() {
        let r = Results::default();
        r.put("a".into(), entry(Kind::Search, 100), 0);
        assert!(r.get("a", Kind::Search, 50).is_some());
        assert!(r.get("a", Kind::Esql, 50).is_none());
        assert!(r.get("a", Kind::Search, 100).is_none());
        r.put("b".into(), entry(Kind::Sql, 100), 0);
        assert!(!r.remove("b", Kind::Search));
        assert!(r.remove("b", Kind::Sql));
        assert!(r.is_empty());
    }

    #[test]
    fn bounded() {
        let r = Results::default();
        for i in 0..(MAX_ENTRIES + 10) {
            r.put(format!("{i}"), entry(Kind::Search, 1000 + i as i64), 0);
        }
        assert_eq!(r.len(), MAX_ENTRIES);
        assert!(r.get("0", Kind::Search, 1).is_none());
        assert!(r.get(&format!("{}", MAX_ENTRIES + 9), Kind::Search, 1).is_some());
        assert_ne!(new_id(), new_id());
    }

    #[test]
    fn changes_are_noted_only_when_tracked() {
        let r = Results::default();
        r.put("a".into(), entry(Kind::Search, 100), 0);
        assert!(!r.has_changes(), "in memory nothing is noted");
        assert!(r.take_changes(0).is_empty());
        r.track_changes();
        r.put("b".into(), entry(Kind::Eql, 100), 0);
        r.touch("a", 50); // not later than its expiration: no change
        assert!(r.has_changes());
        let c = r.take_changes(0);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, "b");
        assert_eq!(c[0].1.as_ref().map(|e| e.kind), Some(Kind::Eql));
        assert!(!r.has_changes());
        r.touch("a", 200);
        assert!(r.remove("b", Kind::Eql));
        let c = r.take_changes(0);
        assert_eq!(c.iter().map(|(id, e)| (id.as_str(), e.as_ref().map(|e| e.expiration_ms))).collect::<Vec<_>>(), vec![("a", Some(200)), ("b", None)]);
        // Expired entries are noted as removed; a failed write is noted again.
        r.put("c".into(), entry(Kind::Sql, 10), 0);
        let _ = r.take_changes(0);
        let c = r.take_changes(20);
        assert_eq!(c.iter().map(|(id, e)| (id.as_str(), e.is_some())).collect::<Vec<_>>(), vec![("c", false)]);
        r.note_changed(["c".to_string()]);
        assert!(r.has_changes());
        assert_eq!(r.take_changes(20).len(), 1);
        // Restored entries aren't changes, unless they expired meanwhile.
        r.restore("d".into(), entry(Kind::SqlCursor, 100), 30);
        assert!(!r.has_changes());
        assert!(r.get("d", Kind::SqlCursor, 30).is_some());
        r.restore("e".into(), entry(Kind::Esql, 25), 30);
        assert!(r.get("e", Kind::Esql, 30).is_none());
        assert_eq!(r.take_changes(30).iter().map(|(id, e)| (id.as_str(), e.is_some())).collect::<Vec<_>>(), vec![("e", false)]);
        r.note_all();
        assert_eq!(r.take_changes(30).iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), vec!["a", "d"]);
    }

    #[test]
    fn async_ids() {
        let id = async_id("W6qZ2sOTRAub1GCZ-RsTIg");
        assert!(check_id(&id).is_ok());
        check_id("FklOZ0dOakZhUV9xQlBvRURvZ3B0WGccVzZxWjJzT1RSQXViMUdDWi1Sc1RJZzoxNTY2NA==").unwrap();
        let e = check_id("abc").unwrap_err();
        assert_eq!(e.reason(), "invalid id: [abc]");
        assert_eq!(e.to_value(false).get("caused_by").and_then(|c| c.get("reason")).and_then(Value::as_str), Some("tried to read: 105 bytes but only 1 remaining"));
    }
}
