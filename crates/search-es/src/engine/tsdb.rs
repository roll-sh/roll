//! Time series indices (`index.mode: time_series`): routing by dimension values, `_tsid` and
//! the generated `_id`, byte for byte as Elasticsearch 9.5 builds them.
//!
//! - **Routing** (`IndexRouting.ExtractFromSource`): every scalar of the source whose path
//!   matches `index.routing_path` contributes `(murmur3(path), murmur3(value text))`; sorted by
//!   path (then order seen), they fold into `hash = 31·hash + (name_hash ^ value_hash)`. The
//!   shard comes from that hash, and the hash travels as the request's routing, base64url-encoded
//!   (`_ts_routing_hash`).
//! - **`_tsid`** (`RoutingPathFields.buildHash`): the dimension values, typed and serialized as
//!   `StreamOutput` writes them (`s` + vint length + UTF-8, `l` + big-endian long, `b` + `t|f`),
//!   sorted by field name, become `vint(len) · murmur3_128(names) · murmur3_32(first value)… ·
//!   murmur3_128(values)`. Elasticsearch hashes the *backing arrays* of the names
//!   (`new BytesRef(String)`: three bytes per char, zero padded) and of the serialized values
//!   (`BytesStreamOutput` buffers, grown by Lucene's `ArrayUtil.oversize`), so the padding is
//!   reproduced here: it is part of every `_tsid` and `_id` Elasticsearch has ever written.
//! - **`_id`** (`TsidExtractingIdFieldMapper.createId`): base64url of
//!   `routing hash (LE i32) · murmur3_128(_tsid).h1 (LE i64) · @timestamp (BE i64)`; reads and
//!   deletes route by its first four bytes.
//! - **`_tsid` doc values** hold the `_tsid` as lowercase hex ([`tsid_dv`]): hex keeps the byte
//!   order (sorting, `_key` order, the `time_series` aggregation's series order) and lets
//!   keyword-typed consumers (search-aggs' terms and composite sources) carry the value as
//!   text; responses print it base64url-encoded as Elasticsearch's `TIME_SERIES_ID` format
//!   does ([`tsid_display`]).

use crate::document::{FieldValue, ParsedDocument};
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::meta::IndexMeta;
use base64::Engine as _;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The `_tsid` as its doc value (lowercase hex; byte order preserved).
pub fn tsid_dv(tsid: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(tsid.len() * 2);
    for b in tsid {
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 15) as usize]);
    }
    out
}

/// The `_tsid` bytes of a doc value written by [`tsid_dv`]; `None` when it isn't hex.
pub fn tsid_from_dv(dv: &[u8]) -> Option<Vec<u8>> {
    if !dv.len().is_multiple_of(2) {
        return None;
    }
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    };
    dv.as_chunks::<2>().0.iter().map(|p| Some((nib(p[0])? << 4) | nib(p[1])?)).collect()
}

/// A `_tsid` doc value as responses print it (`DocValueFormat.TIME_SERIES_ID`: base64url
/// without padding).
pub fn tsid_display(dv: &[u8]) -> String {
    match tsid_from_dv(dv) {
        Some(b) => B64.encode(b),
        None => String::from_utf8_lossy(dv).into_owned(),
    }
}

/// A printed `_tsid` (base64url) back to its doc value (`search_after`, composite `after`).
pub fn tsid_parse(s: &str) -> EsResult<Vec<u8>> {
    B64.decode(s.trim_end_matches('='))
        .map(|b| tsid_dv(&b))
        .map_err(|_| EsError::illegal_argument(format!("Illegal base64 character in [{s}]")))
}

/// Whether `idx` is a time series index.
pub fn is_time_series(idx: &IndexMeta) -> bool {
    idx.settings.get("index.mode") == Some("time_series")
}

/// `specifying routing is not supported because the destination index [x] is in time_series mode`
/// (and the same for other operations).
pub fn unsupported(idx: &IndexMeta, operation: &str) -> EsError {
    EsError::illegal_argument(format!("{operation} is not supported because the destination index [{}] is in time_series mode", idx.name))
}

/// `ExtractFromSource.idToHash` on an id that isn't one (`resource_not_found_exception`).
pub fn invalid_id(idx: &IndexMeta, id: &str) -> EsError {
    EsError::resource_not_found(format!("invalid id [{id}] for index [{}] in time_series mode", idx.name))
}

/// The routing hash of a document's source (`ExtractFromSource.hashSource`).
pub fn routing_hash(idx: &IndexMeta, source: &Value) -> EsResult<i32> {
    let patterns = idx.settings.get_list("index.routing_path").filter(|p| !p.is_empty()).or_else(|| idx.settings.get_list("index.dimensions")).unwrap_or_default();
    let mut hashes: Vec<(Vec<u8>, i32, usize)> = Vec::new();
    if let Value::Obj(m) = source {
        for (k, v) in m {
            extract(&patterns, k, v, &mut hashes);
        }
    }
    if hashes.is_empty() {
        return Err(EsError::illegal_argument("Error extracting routing: source didn't contain any routing fields"));
    }
    hashes.sort();
    let mut h: i32 = 0;
    for (name, vh, _) in &hashes {
        h = h.wrapping_mul(31).wrapping_add(murmur32(name) ^ vh);
    }
    Ok(h)
}

/// `Builder.extractItem`: objects descend, arrays repeat the path, scalars hash their text.
fn extract(patterns: &[String], path: &str, v: &Value, out: &mut Vec<(Vec<u8>, i32, usize)>) {
    match v {
        Value::Obj(m) => {
            for (k, c) in m {
                crate::stack::maybe_grow(|| extract(patterns, &format!("{path}.{k}"), c, out));
            }
        }
        Value::Arr(a) => {
            for x in a {
                crate::stack::maybe_grow(|| extract(patterns, path, x, out));
            }
        }
        other => add(patterns, path, other, out),
    }
}

fn add(patterns: &[String], path: &str, v: &Value, out: &mut Vec<(Vec<u8>, i32, usize)>) {
    let text = match v {
        Value::Null => return,
        Value::Str(s) => s.clone(),
        other => other.to_es_string(),
    };
    if !patterns.iter().any(|p| crate::xcontent::filter::glob_match(p, path)) {
        return;
    }
    let n = out.len();
    out.push((path.as_bytes().to_vec(), murmur32(text.as_bytes()), n));
}

fn murmur32(b: &[u8]) -> i32 {
    crate::meta::routing::murmur3_x86_32(b, 0)
}

/// `TimeSeriesRoutingHashFieldMapper.encode`.
pub fn encode_hash(h: i32) -> String {
    B64.encode(h.to_le_bytes())
}

/// The routing hash a `_ts_routing_hash` routing value or a time series `_id` starts with.
pub fn decode_hash(s: &str) -> Option<i32> {
    let b = B64.decode(s.as_bytes()).ok()?;
    (b.len() >= 4).then(|| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// A dimension value, serialized as `RoutingPathFields` does.
fn encode_dimension(v: &FieldValue) -> Option<Vec<Vec<u8>>> {
    // Each inner vec is one `write` call on the `BytesStreamOutput` (buffer growth follows).
    Some(match v {
        FieldValue::Keyword(s) | FieldValue::Text(s) => {
            let mut len = Vec::new();
            vint(&mut len, s.len() as u32);
            vec![b"s".to_vec(), len, s.as_bytes().to_vec()]
        }
        FieldValue::Ip(b) => {
            let s = search_index::query::range::inet_to_string(b);
            let mut len = Vec::new();
            vint(&mut len, s.len() as u32);
            vec![b"s".to_vec(), len, s.into_bytes()]
        }
        FieldValue::Long(l) => vec![b"l".to_vec(), l.to_be_bytes().to_vec()],
        FieldValue::UnsignedLong(u) => {
            if *u <= i64::MAX as u64 {
                vec![b"l".to_vec(), (*u as i64).to_be_bytes().to_vec()]
            } else {
                // `UNSIGNED_LONG_SHIFTED`: the doc value is the value minus 2^63.
                vec![b"u".to_vec(), ((*u as i64) ^ i64::MIN).to_be_bytes().to_vec()]
            }
        }
        FieldValue::Boolean(b) => vec![b"b".to_vec(), vec![if *b { b't' } else { b'f' }]],
        _ => return None,
    })
}

fn vint(out: &mut Vec<u8>, mut v: u32) {
    while v & !0x7f != 0 {
        out.push(((v & 0x7f) | 0x80) as u8);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Lucene's `ArrayUtil.oversize(n, 1)` (64-bit JVM).
fn oversize(n: usize) -> usize {
    let extra = (n >> 3).max(3);
    (n + extra + 7) & !7
}

/// The bytes Elasticsearch feeds the hasher for one serialized value: the value and the zero
/// tail of its `BytesStreamOutput` buffer.
fn padded(writes: &[Vec<u8>]) -> (Vec<u8>, usize) {
    let mut cap = 0usize;
    let mut data = Vec::new();
    for w in writes {
        let need = data.len() + w.len();
        if need > cap {
            cap = oversize(need);
        }
        data.extend_from_slice(w);
    }
    let len = data.len();
    data.resize(cap, 0);
    (data, len)
}

/// The dimension values of a parsed document, by field name.
pub fn dimensions(idx: &IndexMeta, doc: &ParsedDocument) -> Vec<(String, Vec<Vec<Vec<u8>>>)> {
    let mut out: Vec<(String, Vec<Vec<Vec<u8>>>)> = Vec::new();
    for f in &doc.fields {
        let Some(crate::mapping::MapperRef::Field(fm)) = idx.mapping.lookup(&f.name) else { continue };
        // `flattened` fields name their dimension keys (`time_series_dimensions`): each is a
        // keyword dimension `<field>.<key>`.
        if let (FieldValue::Flattened(pairs), Some(Value::Arr(keys))) = (&f.value, fm.raw("time_series_dimensions")) {
            for (k, v) in pairs {
                if !keys.iter().any(|x| x.as_str() == Some(k.as_str())) {
                    continue;
                }
                let name = format!("{}.{k}", f.name);
                let Some(enc) = encode_dimension(&FieldValue::Keyword(v.clone())) else { continue };
                match out.iter_mut().find(|(n, _)| *n == name) {
                    Some((_, vals)) => vals.push(enc),
                    None => out.push((name, vec![enc])),
                }
            }
            continue;
        }
        if fm.raw("time_series_dimension").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let Some(enc) = encode_dimension(&f.value) else { continue };
        match out.iter_mut().find(|(n, _)| n.as_str() == &*f.name) {
            Some((_, vals)) => vals.push(enc),
            None => out.push((f.name.to_string(), vec![enc])),
        }
    }
    out.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    out
}

/// `RoutingPathFields.buildHash`: the `_tsid` of sorted dimensions (at most 512 fields count).
pub fn build_tsid(dims: &[(String, Vec<Vec<Vec<u8>>>)]) -> Vec<u8> {
    let n = dims.len().min(512);
    let len = 16 + 16 + 4 * n;
    let mut out = Vec::with_capacity(len + 2);
    vint(&mut out, len as u32);
    let mut names = Vec::new();
    for (name, _) in dims {
        names.extend_from_slice(name.as_bytes());
        // `new BytesRef(String).bytes`: `UnicodeUtil.maxUTF8Length` = 3 bytes per UTF-16 unit.
        let units = name.encode_utf16().count();
        names.resize(names.len() + units * 3 - name.len(), 0);
    }
    let (h1, h2) = search_aggs::util::murmur3_x64_128(&names, 0);
    out.extend_from_slice(&h1.to_le_bytes());
    out.extend_from_slice(&h2.to_le_bytes());
    let start = out.len();
    for (_, vals) in dims {
        if out.len() - start >= 4 * n {
            break;
        }
        let (v, l) = padded(&vals[0]);
        out.extend_from_slice(&murmur32(&v[..l]).to_le_bytes());
    }
    let mut all = Vec::new();
    for (_, vals) in dims {
        for w in vals {
            all.extend_from_slice(&padded(w).0);
        }
    }
    let (h1, h2) = search_aggs::util::murmur3_x64_128(&all, 0);
    out.extend_from_slice(&h1.to_le_bytes());
    out.extend_from_slice(&h2.to_le_bytes());
    out
}

/// `TsidExtractingIdFieldMapper.createId`.
pub fn create_id(routing_hash: i32, tsid: &[u8], timestamp: i64) -> String {
    let (h1, _) = search_aggs::util::murmur3_x64_128(tsid, 0);
    let mut b = [0u8; 20];
    b[..4].copy_from_slice(&routing_hash.to_le_bytes());
    b[4..12].copy_from_slice(&h1.to_le_bytes());
    b[12..].copy_from_slice(&timestamp.to_be_bytes());
    B64.encode(b)
}

/// `index.mapping.synthetic_id` (default `true` in Elasticsearch 9.5): ids that hold the whole `_tsid`.
pub fn synthetic_ids(idx: &IndexMeta) -> bool {
    idx.settings.get("index.mapping.synthetic_id") != Some("false")
}

/// A synthetic time series id: `_tsid · (Long.MAX_VALUE − @timestamp) (BE) · routing hash (BE)`,
/// so ids sort by series, then newest first.
pub fn create_synthetic_id(routing_hash: i32, tsid: &[u8], timestamp: i64) -> String {
    let mut b = Vec::with_capacity(tsid.len() + 12);
    b.extend_from_slice(tsid);
    b.extend_from_slice(&(i64::MAX - timestamp).to_be_bytes());
    b.extend_from_slice(&routing_hash.to_be_bytes());
    B64.encode(b)
}

/// The routing hash a time series `_id` carries (first four bytes little-endian, or the last
/// four big-endian for synthetic ids).
pub fn id_routing_hash(idx: &IndexMeta, id: &str) -> Option<i32> {
    let b = B64.decode(id.as_bytes()).ok()?;
    if b.len() < 4 {
        return None;
    }
    if synthetic_ids(idx) {
        let n = b.len();
        return Some(i32::from_be_bytes([b[n - 4], b[n - 3], b[n - 2], b[n - 1]]));
    }
    Some(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// How a time series document names itself in version conflicts: `[tsid@timestamp]`.
pub fn describe(tsid: &[u8], timestamp: &str) -> String {
    format!("{}@{timestamp}", B64.encode(tsid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kw(s: &str) -> Vec<Vec<Vec<u8>>> {
        vec![encode_dimension(&FieldValue::Keyword(s.into())).unwrap()]
    }

    /// Ids from Elasticsearch 9.5's `tsdb/26_id_generation` suite.
    #[test]
    fn ids_match_elasticsearch() {
        let dims = vec![("k8s.pod.uid".to_string(), kw("947e4ced-1786-4e53-9e0c-5c447e959507")), ("metricset".to_string(), kw("pod"))];
        let tsid = build_tsid(&dims);
        let mut h: i32 = 0;
        let mut parts = vec![("k8s.pod.uid", "947e4ced-1786-4e53-9e0c-5c447e959507"), ("metricset", "pod")];
        parts.sort();
        for (n, v) in parts {
            h = h.wrapping_mul(31).wrapping_add(murmur32(n.as_bytes()) ^ murmur32(v.as_bytes()));
        }
        // 2021-04-28T18:52:04.467Z
        assert_eq!(create_id(h, &tsid, 1_619_635_924_467), "cZZNs7B9sSWsyrL5AAABeRnS7fM");
        let dims = vec![("k8s.pod.uid".to_string(), kw("df3145b3-0563-4d3b-a0f7-897eb2876ea9")), ("metricset".to_string(), kw("pod"))];
        let tsid = build_tsid(&dims);
        assert_eq!(B64.encode(&tsid), "KCjEJ9R_BgO8TRX2QOd6dpQ5ihHD--qoyLTiOy0pmP6_RAIE-e0-dKQ");
        assert_eq!(encode_hash(decode_hash("cn4exQ").unwrap()), "cn4exQ");
    }

    #[test]
    fn buffers_grow_like_bytes_stream_output() {
        assert_eq!(oversize(1), 8);
        assert_eq!(oversize(38), 48);
        assert_eq!(padded(&[b"s".to_vec(), vec![3], b"pod".to_vec()]).0.len(), 8);
        assert_eq!(padded(&[b"l".to_vec(), 7i64.to_be_bytes().to_vec()]).0.len(), 16);
    }
}

#[cfg(test)]
mod tsid_dv_tests {
    use super::*;

    #[test]
    fn hex_round_trip_keeps_byte_order() {
        let a = [0x00u8, 0xff, 0x10];
        let b = [0x01u8, 0x00];
        let c = [0x01u8, 0x00, 0x00];
        let (da, db, dc) = (tsid_dv(&a), tsid_dv(&b), tsid_dv(&c));
        assert!(da < db && db < dc);
        assert_eq!(tsid_from_dv(&da).unwrap(), a);
        assert_eq!(tsid_from_dv(b"0g"), None);
        assert_eq!(tsid_from_dv(b"abc"), None);
        let shown = tsid_display(&dc);
        assert_eq!(shown, "AQAA");
        assert_eq!(tsid_parse(&shown).unwrap(), dc);
        assert!(tsid_parse("!!").is_err());
    }
}
