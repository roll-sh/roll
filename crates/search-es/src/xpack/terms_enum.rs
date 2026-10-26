//! `_terms_enum`: the indexed terms of a keyword-family field that start with a prefix, for
//! autocompletion.
//!
//! `keyword`, `version` and `flattened` subfields read every segment's term dictionary, and
//! `ip` its sorted-set doc values dictionary (as Elasticsearch's `getTerms` do): each segment
//! seeks past `search_after` (and to the prefix when it is a byte range), yields at most
//! `size` matching terms, and the per-segment lists merge in term-byte order — so paging
//! reaches any term and costs O(size) per segment, whatever the dictionary size. Version
//! prefixes apply to the decoded text and IP prefixes to the address text, so those scan
//! from `search_after`. Other enumerable fields (`constant_keyword`, `wildcard`) run a `terms`
//! aggregation in key order through the core search API. Like Elasticsearch 9.5.5, `boolean`
//! fields, `flattened` roots and non-keyword fields (text, numbers) answer an empty list.

use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal};

const FIELDS: [&str; 7] = ["field", "string", "size", "timeout", "case_insensitive", "index_filter", "search_after"];
const ENUMERABLE: [&str; 5] = ["keyword", "constant_keyword", "wildcard", "version", "ip"];

/// How a field's terms are stored and printed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TermKind {
    /// UTF-8 bytes (`keyword`, flattened values).
    Raw,
    /// `VersionEncoder` bytes, printed decoded.
    Version,
    /// 16-byte IPv6 (IPv4-mapped for IPv4), printed as the address.
    Ip,
}

/// One index's dictionary for the requested field.
#[derive(Clone)]
struct Dict {
    number: u32,
    doc_values: bool,
    kind: TermKind,
    /// Flattened subfields: `key\0` before every value.
    key_prefix: Vec<u8>,
}

/// The dictionary of `field` in an index (`Err(())`: not enumerated directly; `Ok(None)`:
/// nothing to enumerate, as Elasticsearch answers for booleans and flattened roots).
fn dict_of(table: &crate::engine::fields::FieldTable, field: &str) -> Result<Option<Dict>, ()> {
    let fi = match table.get(field) {
        Some(f) => f.clone(),
        None => match table.keyed(field) {
            Some(f) => f,
            None => return Ok(None),
        },
    };
    if let Some(k) = &fi.flattened_key {
        let mut key_prefix = k.as_bytes().to_vec();
        key_prefix.push(0);
        return Ok(Some(Dict { number: fi.number, doc_values: false, kind: TermKind::Raw, key_prefix }));
    }
    let terms = |kind| Ok(Some(Dict { number: fi.number, doc_values: !fi.index, kind, key_prefix: Vec::new() }));
    match fi.ty.as_str() {
        "keyword" if fi.index || fi.doc_values => terms(TermKind::Raw),
        "version" => terms(TermKind::Version),
        // `terms_enum` for ip only works with doc values.
        "ip" if fi.doc_values => Ok(Some(Dict { number: fi.number, doc_values: true, kind: TermKind::Ip, key_prefix: Vec::new() })),
        "boolean" | "flattened" | "ip" | "keyword" => Ok(None),
        _ => Err(()),
    }
}

/// Whether `text` starts with `prefix` (case-insensitively: per character, either case).
fn has_prefix(text: &str, prefix: &str, ci: bool) -> bool {
    if !ci {
        return text.starts_with(prefix);
    }
    let mut t = text.chars();
    prefix.chars().all(|p| t.next().is_some_and(|c| c == p || c.to_lowercase().eq(p.to_lowercase())))
}

/// The address an `ip` term stands for.
fn ip_text(b: &[u8]) -> String {
    let Ok(a) = <[u8; 16]>::try_from(b) else { return String::from_utf8_lossy(b).into_owned() };
    let v6 = std::net::Ipv6Addr::from(a);
    match v6.to_ipv4_mapped() {
        Some(v4) => v4.to_string(),
        None => v6.to_string(),
    }
}

fn ip_bytes(s: &str) -> Option<Vec<u8>> {
    Some(match s.parse::<std::net::IpAddr>().ok()? {
        std::net::IpAddr::V4(a) => a.to_ipv6_mapped().octets().to_vec(),
        std::net::IpAddr::V6(a) => a.octets().to_vec(),
    })
}

/// A request's matching rules over one dictionary.
struct Matcher<'a> {
    prefix: &'a str,
    ci: bool,
    /// The encoded `search_after` (exclusive), without the key prefix.
    after: Option<Vec<u8>>,
    size: usize,
}

impl Matcher<'_> {
    /// The term's text if it matches (the bytes after the key prefix).
    fn text(&self, kind: TermKind, rest: &[u8]) -> Option<String> {
        let text = match kind {
            TermKind::Raw => String::from_utf8_lossy(rest).into_owned(),
            TermKind::Version => search_aggs::version_string::decode(rest),
            TermKind::Ip => ip_text(rest),
        };
        let ok = match kind {
            // `VersionEncoder.prefixAutomaton` lowercases only the prefix when case-insensitive.
            TermKind::Version if self.ci => text.starts_with(&self.prefix.to_lowercase()),
            _ => has_prefix(&text, self.prefix, self.ci),
        };
        ok.then_some(text)
    }

    /// Whether a raw term past `rest` can no longer match: byte-ordered prefixes end.
    fn past(&self, kind: TermKind, rest: &[u8]) -> bool {
        kind == TermKind::Raw && !self.ci && !self.prefix.is_empty() && rest > self.prefix.as_bytes() && !rest.starts_with(self.prefix.as_bytes())
    }

    /// Up to `size` matching terms of one segment, as (term bytes, text).
    fn segment(&self, reader: &search_codec::reader::SegmentReader, d: &Dict, out: &mut Vec<(Vec<u8>, String)>) {
        use std::ops::Bound;
        let mut found = 0;
        if d.doc_values {
            let Some(dv) = reader.sorted_set(d.number) else { return };
            let start = match &self.after {
                Some(a) => match dv.lookup_term(a) {
                    Ok(o) => o + 1,
                    Err(o) => o,
                },
                None => 0,
            };
            let mut buf = Vec::new();
            for ord in start..dv.value_count() {
                buf.clear();
                dv.lookup_ord(ord, &mut buf);
                if self.past(d.kind, &buf) {
                    break;
                }
                if let Some(t) = self.text(d.kind, &buf) {
                    out.push((buf.clone(), t));
                    found += 1;
                    if found >= self.size {
                        break;
                    }
                }
            }
            return;
        }
        let Some(terms) = reader.terms(d.number) else { return };
        let mut lower = d.key_prefix.clone();
        let mut inclusive = true;
        if d.kind == TermKind::Raw && !self.ci && !self.prefix.is_empty() {
            lower.extend_from_slice(self.prefix.as_bytes());
        }
        if let Some(a) = &self.after {
            let mut after = d.key_prefix.clone();
            after.extend_from_slice(a);
            if after >= lower {
                lower = after;
                inclusive = false;
            }
        }
        let mut it = terms.range(if inclusive { Bound::Included(&lower[..]) } else { Bound::Excluded(&lower[..]) }, Bound::Unbounded);
        while let Some(t) = it.next() {
            let Some(rest) = t.strip_prefix(&d.key_prefix[..]) else { break };
            if self.past(d.kind, rest) {
                break;
            }
            if let Some(text) = self.text(d.kind, rest) {
                out.push((rest.to_vec(), text));
                found += 1;
                if found >= self.size {
                    break;
                }
            }
        }
    }
}

/// The terms of `field` over the shards of `targets` (`None`: some index's field isn't
/// enumerated directly, so the aggregation path answers).
fn enumerate(cluster: &crate::cluster::Cluster, targets: &str, field: &str, prefix: &str, ci: bool, after: Option<&str>, size: usize) -> EsResult<Option<Vec<String>>> {
    let st = cluster.state();
    let req = crate::search::SearchRequest {
        indices: crate::meta::resolve::split_expressions(targets),
        opts: crate::meta::resolve::IndicesOptions { ignore_unavailable: true, ..crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED },
        source: crate::search_source::SearchSource::default(),
        routing: None,
        preference: None,
        dfs: false,
        request_cache: None,
        total_as_int: false,
        typed_keys: false,
        allow_partial: true,
        scroll: None,
        include_named_queries_score: false,
        force_synthetic_source: false,
        now_ms: 0,
        fused: None,
        rank_only: false,
    };
    let shards = crate::search::shard_targets(cluster, &st, &req)?;
    drop(st);
    let mut out: Vec<(Vec<u8>, String)> = Vec::new();
    for t in shards {
        let Ok(dict) = dict_of(&t.engine.table(), field) else { return Ok(None) };
        let Some(dict) = dict else { continue };
        let after = after.map(|a| match dict.kind {
            TermKind::Raw => a.as_bytes().to_vec(),
            TermKind::Version => search_aggs::version_string::encode(a).unwrap_or_else(|_| a.as_bytes().to_vec()),
            // An unparseable address fails the shard in Elasticsearch; nothing follows it here.
            TermKind::Ip => ip_bytes(a).unwrap_or_else(|| vec![0xff; 17]),
        });
        let m = Matcher { prefix, ci, after, size };
        let searcher = t.searcher()?;
        for seg in searcher.segments() {
            m.segment(&seg.reader, &dict, &mut out);
        }
    }
    out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    out.truncate(size);
    Ok(Some(out.into_iter().map(|(_, t)| t).collect()))
}

/// A Lucene regexp matching strings that start with `prefix` (case-insensitively if asked).
pub fn prefix_regex(prefix: &str, case_insensitive: bool) -> String {
    let mut out = String::with_capacity(prefix.len() * 4 + 2);
    for c in prefix.chars() {
        let lower: Vec<char> = c.to_lowercase().collect();
        let upper: Vec<char> = c.to_uppercase().collect();
        if case_insensitive && (lower.len() == 1 && upper.len() == 1) && lower[0] != upper[0] {
            out.push('[');
            out.push(lower[0]);
            out.push(upper[0]);
            out.push(']');
        } else if c.is_alphanumeric() {
            out.push(c);
        } else {
            out.push('\\');
            out.push(c);
        }
    }
    out.push_str(".*");
    out
}

/// The number of shards of the indices `index` resolves to.
async fn shard_count(ctx: &Ctx<'_>, index: &str) -> EsResult<i64> {
    let settings = internal::call(ctx.cluster, "GET", &format!("/{}/_settings/index.number_of_shards?flat_settings=true", enc(index)), None).await?;
    Ok(settings
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .filter_map(|i| i.pointer(&["settings", "index.number_of_shards"]))
        .filter_map(|n| n.as_i64().or_else(|| n.to_es_string().parse().ok()))
        .sum())
}

pub async fn terms_enum(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if !FIELDS.contains(&k.as_str()) {
            return Err(crate::xpack::unknown_field(ctx, "terms_enum_request", k));
        }
    }
    let field = match body.get("field").or_else(|| ctx.query("field").map(|_| &Value::Null)) {
        Some(Value::Str(f)) => f.clone(),
        _ => match ctx.query("field") {
            Some(f) => f.to_string(),
            None => return Err(EsError::action_request_validation(&["field cannot be null".into()])),
        },
    };
    let prefix = body.get("string").map(Value::to_es_string).unwrap_or_default();
    // The shards collect until they hold `size` terms, checked after adding one: a size of 0
    // or less returns the first term.
    let size = body.get("size").and_then(Value::as_i64).unwrap_or(10).max(1);
    let ci = body.get("case_insensitive").and_then(Value::as_bool).unwrap_or(false);
    let after = body.get("search_after").map(Value::to_es_string);
    let index = ctx.path_param("index").unwrap_or("_all").to_string();
    let shards = |total: i64| obj! { "total" => total, "successful" => total, "failed" => 0 };
    // Which indices take part (`index_filter` selects indices, not documents) and the field's type.
    let caps_uri = format!("/{}/_field_caps?fields={}&include_unmapped=false", enc(&index), enc(&field));
    let caps = internal::call(ctx.cluster, "GET", &caps_uri, None).await?;
    // `_shards` counts the shards of every target index, mapped or not.
    let mut total_shards = shard_count(ctx, &index).await?;
    let ty = caps.pointer(&["fields", field.as_str()]).and_then(Value::as_object).and_then(|m| m.keys().next().cloned());
    // Flattened subfields (and fields field_caps doesn't list) are resolved per index below.
    if ty.as_deref().is_some_and(|t| t != "flattened" && !ENUMERABLE.contains(&t)) {
        return Reply::ok(obj! { "_shards" => shards(total_shards), "terms" => Value::Arr(vec![]), "complete" => true });
    }
    let mut targets = index.clone();
    if let Some(f) = body.get("index_filter") {
        // `canMatchShard`: the shards whose `can_match` rewrites the filter to `match_none`
        // drop out (as field_caps decides it), whether or not any document matches.
        let names: Vec<String> = {
            let resolved = crate::api::resolve_indices(ctx, crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN)?;
            let st = ctx.state();
            resolved
                .into_iter()
                .filter(|n| {
                    st.metadata.indices.get(n.as_str()).is_none_or(|idx| crate::api::field_caps::can_match(f, idx) && crate::api::field_caps::shards_can_match(ctx, &st, f, idx))
                })
                .collect()
        };
        if names.is_empty() {
            return Reply::ok(obj! { "_shards" => shards(0), "terms" => Value::Arr(vec![]), "complete" => true });
        }
        targets = names.join(",");
        total_shards = shard_count(ctx, &targets).await?;
    }
    let started = std::time::Instant::now();
    let timeout_ms = body.get("timeout").and_then(|t| crate::units::TimeValue::parse(&t.to_es_string(), "timeout").ok()).map(|t| t.as_millis()).unwrap_or(1000);
    if let Some(terms) = enumerate(ctx.cluster, &targets, &field, &prefix, ci, after.as_deref(), size as usize)? {
        let complete = (started.elapsed().as_millis() as i64) < timeout_ms;
        return Reply::ok(obj! { "_shards" => shards(total_shards), "terms" => Value::Arr(terms.into_iter().map(Value::from).collect()), "complete" => complete });
    }
    // `constant_keyword` and `wildcard`: a terms aggregation in key order (a constant keyword
    // has one term; wildcard values come from doc values).
    let fetch = size + i64::from(after.is_some()) * 1000;
    let mut terms_agg = obj! { "field" => field.clone(), "size" => fetch, "order" => obj! { "_key" => "asc" }, "min_doc_count" => 1 };
    if !prefix.is_empty()
        && let Value::Obj(m) = &mut terms_agg
    {
        m.insert("include".into(), prefix_regex(&prefix, ci).into());
    }
    let q = obj! { "size" => 0, "aggs" => obj! { "t" => obj! { "terms" => terms_agg } } };
    let r = internal::call(ctx.cluster, "POST", &format!("/{}/_search?ignore_unavailable=true", enc(&targets)), Some(&q)).await?;
    let mut terms: Vec<Value> = r
        .pointer(&["aggregations", "t", "buckets"])
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|b| b.get("key_as_string").or_else(|| b.get("key")).map(Value::to_es_string))
        .filter(|t| after.as_ref().is_none_or(|a| t.as_str() > a.as_str()))
        .map(Value::from)
        .collect();
    terms.truncate(size as usize);
    let complete = (started.elapsed().as_millis() as i64) < timeout_ms;
    Reply::ok(obj! { "_shards" => shards(total_shards), "terms" => Value::Arr(terms), "complete" => complete })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes() {
        assert_eq!(prefix_regex("gar", false), "gar.*");
        assert_eq!(prefix_regex("Ga", true), "[gG][aA].*");
        assert_eq!(prefix_regex("a.b", false), "a\\.b.*");
    }

    #[test]
    fn matching_rules() {
        assert!(has_prefix("Kxy", "kX", true));
        assert!(!has_prefix("Kxy", "kX", false));
        assert!(has_prefix("ÉTÉ", "été", true));
        assert!(!has_prefix("k", "kk", true));
        assert_eq!(ip_text(&ip_bytes("10.0.5.1").unwrap()), "10.0.5.1");
        assert_eq!(ip_text(&ip_bytes("2001:db8::1").unwrap()), "2001:db8::1");
        let m = Matcher { prefix: "k01", ci: false, after: None, size: 3 };
        assert!(m.past(TermKind::Raw, b"k02"));
        assert!(!m.past(TermKind::Raw, b"k011"));
        assert!(!m.past(TermKind::Raw, b"a"));
        // Version prefixes apply to the decoded text, so they never end a scan.
        assert!(!m.past(TermKind::Version, b"zzz"));
        let enc = search_aggs::version_string::encode("1.11000.0").unwrap();
        let v = Matcher { prefix: "1.1100", ci: false, after: None, size: 3 };
        assert_eq!(v.text(TermKind::Version, &enc).as_deref(), Some("1.11000.0"));
    }
}
