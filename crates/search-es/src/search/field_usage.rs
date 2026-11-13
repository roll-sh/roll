//! Field usage statistics (`GET /{index}/_field_usage_stats`, Elasticsearch's
//! `FieldUsageStatsService`).
//!
//! Elasticsearch wraps every searcher in a `FieldUsageTrackingDirectoryReader` that notes, per
//! shard search request, which index structures of which fields Lucene touched (terms,
//! postings, frequencies, positions, offsets, payloads, proximity, stored fields, doc values,
//! points, norms, term vectors, vectors); when the request ends every noted (field, structure)
//! pair and every noted field (`any`) counts once.
//!
//! roll derives the same notes from the request instead of instrumenting the readers: the
//! query tree (what each query type reads for its field kind, scored or not), sorts, aggregation
//! fields, `docvalue_fields`, highlighting, `knn`, and the stored `_id` / `_source` of fetched
//! hits. Recording happens once per shard request, off the hot path (a handful of map updates
//! under the shard's lock).

use crate::engine::fields::{FieldInfo, FieldTable, Kind};
use crate::json::{Map, Value};
use crate::obj;
use search_codec::IndexOptions;
use std::collections::BTreeMap;

/// One field's counters, in the response's order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub any: u64,
    pub terms: u64,
    pub postings: u64,
    pub term_frequencies: u64,
    pub positions: u64,
    pub offsets: u64,
    pub payloads: u64,
    pub proximity: u64,
    pub stored_fields: u64,
    pub doc_values: u64,
    pub points: u64,
    pub norms: u64,
    pub term_vectors: u64,
    pub knn_vectors: u64,
}

/// The structures one request touched of one field (bit set of `U_*`).
pub type Used = u16;

pub const U_TERMS: Used = 1;
pub const U_POSTINGS: Used = 1 << 1;
pub const U_FREQS: Used = 1 << 2;
pub const U_POSITIONS: Used = 1 << 3;
pub const U_OFFSETS: Used = 1 << 4;
pub const U_PAYLOADS: Used = 1 << 5;
pub const U_PROXIMITY: Used = 1 << 6;
pub const U_STORED: Used = 1 << 7;
pub const U_DOC_VALUES: Used = 1 << 8;
pub const U_POINTS: Used = 1 << 9;
pub const U_NORMS: Used = 1 << 10;
pub const U_TERM_VECTORS: Used = 1 << 11;
pub const U_KNN: Used = 1 << 12;

impl Counts {
    fn add(&mut self, used: Used) {
        if used == 0 {
            return;
        }
        self.any += 1;
        let bit = |b: Used| u64::from(used & b != 0);
        self.terms += bit(U_TERMS);
        self.postings += bit(U_POSTINGS);
        self.term_frequencies += bit(U_FREQS);
        self.positions += bit(U_POSITIONS);
        self.offsets += bit(U_OFFSETS);
        self.payloads += bit(U_PAYLOADS);
        self.proximity += bit(U_PROXIMITY);
        self.stored_fields += bit(U_STORED);
        self.doc_values += bit(U_DOC_VALUES);
        self.points += bit(U_POINTS);
        self.norms += bit(U_NORMS);
        self.term_vectors += bit(U_TERM_VECTORS);
        self.knn_vectors += bit(U_KNN);
    }

    fn merge(&mut self, o: &Counts) {
        self.any += o.any;
        self.terms += o.terms;
        self.postings += o.postings;
        self.term_frequencies += o.term_frequencies;
        self.positions += o.positions;
        self.offsets += o.offsets;
        self.payloads += o.payloads;
        self.proximity += o.proximity;
        self.stored_fields += o.stored_fields;
        self.doc_values += o.doc_values;
        self.points += o.points;
        self.norms += o.norms;
        self.term_vectors += o.term_vectors;
        self.knn_vectors += o.knn_vectors;
    }

    /// `FieldUsageStats.PerFieldUsageStats.toXContent`.
    pub fn to_value(&self) -> Value {
        obj! {
            "any" => self.any as i64,
            "inverted_index" => obj! {
                "terms" => self.terms as i64,
                "postings" => self.postings as i64,
                "term_frequencies" => self.term_frequencies as i64,
                "positions" => self.positions as i64,
                "offsets" => self.offsets as i64,
                "payloads" => self.payloads as i64,
                "proximity" => self.proximity as i64,
            },
            "stored_fields" => self.stored_fields as i64,
            "doc_values" => self.doc_values as i64,
            "points" => self.points as i64,
            "norms" => self.norms as i64,
            "term_vectors" => self.term_vectors as i64,
            "knn_vectors" => self.knn_vectors as i64,
        }
    }
}

/// A shard's usage since it started tracking.
#[derive(Debug, Default)]
pub struct ShardUsage {
    pub fields: BTreeMap<String, Counts>,
    /// `tracking_id` and `tracking_started_at_millis`, set on first use.
    started: Option<(String, i64)>,
}

impl ShardUsage {
    /// The tracking session's id and start (begun now if it hasn't).
    pub fn tracking(&mut self) -> (String, i64) {
        self.started.get_or_insert_with(|| (crate::meta::names::random_uuid(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64))).clone()
    }

    /// The tracking session (`tracking_id`, start), once begun.
    pub fn session(&self) -> Option<&(String, i64)> {
        self.started.as_ref()
    }

    /// Continues the session of an earlier instance of the shard (search-server's wake), whose
    /// counts are `fields`.
    pub fn resume(&mut self, session: (String, i64), fields: BTreeMap<String, Counts>) {
        self.started = Some(session);
        self.fields = fields;
    }

    /// Adds one request's notes.
    pub fn record(&mut self, used: &BTreeMap<String, Used>) {
        self.tracking();
        for (f, u) in used {
            if *u != 0 {
                self.fields.entry(f.clone()).or_default().add(*u);
            }
        }
    }

    /// `stats` of the response: the fields matching `filter` (all when empty) and their sum.
    pub fn to_value(&self, filter: &[String]) -> Value {
        let mut all = Counts::default();
        let mut fields = Map::new();
        for (name, c) in &self.fields {
            if filter.is_empty() || filter.iter().any(|p| crate::xcontent::filter::glob_match(p, name)) {
                all.merge(c);
                fields.insert(name.clone(), c.to_value());
            }
        }
        obj! { "all_fields" => all.to_value(), "fields" => Value::Obj(fields) }
    }
}

/// What one shard request touched, by field.
#[derive(Debug, Default)]
pub struct Notes {
    pub used: BTreeMap<String, Used>,
}

impl Notes {
    pub fn note(&mut self, field: &str, used: Used) {
        *self.used.entry(field.to_string()).or_default() |= used;
    }

    /// What a search request's query, sorts, aggregations and fetch options touch.
    pub fn search(table: &FieldTable, src: &crate::search_source::SearchSource, fetched_hits: bool) -> Notes {
        let mut n = Notes::default();
        let field_sort = src.sort.iter().any(|s| matches!(s, crate::search_source::Sort::Field { field, .. } if field != "_doc" && field != "_score"))
            || src.sort.iter().any(|s| matches!(s, crate::search_source::Sort::Field { field, .. } if field == "_doc"));
        let sort_by_score = src.sort.is_empty() || src.sort.iter().any(|s| matches!(s, crate::search_source::Sort::Score { .. }));
        let scoring = sort_by_score || src.track_scores || !field_sort;
        if let Some(q) = &src.query {
            n.query(table, &q.to_value(), scoring);
        }
        if let Some(q) = &src.post_filter {
            n.query(table, &q.to_value(), false);
        }
        for k in &src.knn {
            if let Some(f) = k.get("field").and_then(Value::as_str) {
                n.note(f, U_KNN);
            }
            if let Some(fl) = k.get("filter") {
                n.query(table, fl, false);
            }
        }
        for s in &src.sort {
            if let crate::search_source::Sort::Field { field, .. } = s
                && let Some(f) = table.get(field)
            {
                // Numeric sorts skip non-competitive documents through the points index.
                let points = f.index && (f.kind.is_numeric() || f.kind.is_date());
                n.note(&f.name, U_DOC_VALUES | if points { U_POINTS } else { 0 });
            }
        }
        if let Some(aggs) = &src.aggs {
            n.aggs(table, aggs);
        }
        for d in &src.docvalue_fields {
            for name in table.matching(&d.field) {
                n.note(&name, U_DOC_VALUES);
            }
        }
        if fetched_hits {
            n.note("_id", U_STORED);
            let wants_source = src.source.as_ref().is_none_or(|s| s.fetch) || !src.fields.is_empty() || src.highlight.is_some();
            if wants_source {
                n.note("_source", U_STORED);
            }
            if let Some(h) = &src.highlight {
                n.highlight(table, h);
            }
        }
        n
    }

    fn highlight(&mut self, table: &FieldTable, h: &Map) {
        let Some(Value::Obj(fields)) = h.get("fields") else { return };
        let fvh_global = h.get("type").and_then(Value::as_str) == Some("fvh");
        for (pattern, opts) in fields {
            let fvh = match opts.get("type").and_then(Value::as_str) {
                Some(t) => t == "fvh",
                None => fvh_global,
            };
            for name in table.matching(pattern) {
                let Some(f) = table.get(&name) else { continue };
                if !f.kind.is_text() {
                    continue;
                }
                let tv = f.mapper.raw("term_vector").and_then(Value::as_str).is_some_and(|t| t.contains("offsets"));
                if fvh && tv {
                    self.note(&f.name, U_TERM_VECTORS);
                } else if f.index_options == IndexOptions::DocsAndFreqsAndPositionsAndOffsets {
                    self.note(&f.name, U_TERMS | U_POSTINGS | U_FREQS | U_POSITIONS | U_OFFSETS);
                } else if tv {
                    self.note(&f.name, U_TERM_VECTORS);
                }
            }
        }
    }

    fn aggs(&mut self, table: &FieldTable, aggs: &Map) {
        for body in aggs.values() {
            let Value::Obj(b) = body else { continue };
            for (k, v) in b {
                match (k.as_str(), v) {
                    ("aggs" | "aggregations", Value::Obj(sub)) => self.aggs(table, sub),
                    ("meta", _) => {}
                    (_, Value::Obj(def)) => {
                        let mut names: Vec<&str> = def.get("field").and_then(Value::as_str).into_iter().collect();
                        if let Some(Value::Arr(srcs)) = def.get("sources") {
                            for s in srcs.iter().filter_map(Value::as_object) {
                                for inner in s.values().filter_map(Value::as_object) {
                                    for t in inner.values().filter_map(Value::as_object) {
                                        names.extend(t.get("field").and_then(Value::as_str));
                                    }
                                }
                            }
                        }
                        for name in names {
                            if let Some(f) = table.get(name) {
                                self.note(&f.name.clone(), U_DOC_VALUES);
                            } else if let Some(k) = table.keyed(name) {
                                self.note(&keyed_name(&k), U_DOC_VALUES);
                            }
                        }
                        if let Some(Value::Obj(filter)) = def.get("filter").filter(|_| k == "filter") {
                            self.query(table, &Value::Obj(filter.clone()), false);
                        }
                    }
                    _ => {}
                }
            }
            if let Some(Value::Obj(q)) = b.get("filter") {
                self.query(table, &Value::Obj(q.clone()), false);
            }
        }
    }

    /// One query object (`{"<type>": {...}}`); `scoring`: whether its scores are needed.
    pub fn query(&mut self, table: &FieldTable, q: &Value, scoring: bool) {
        let Value::Obj(m) = q else { return };
        for (ty, body) in m {
            crate::stack::maybe_grow(|| self.one(table, ty, body, scoring));
        }
    }

    fn children(&mut self, table: &FieldTable, v: Option<&Value>, scoring: bool) {
        match v {
            Some(Value::Arr(a)) => a.iter().for_each(|x| self.query(table, x, scoring)),
            Some(x @ Value::Obj(_)) => self.query(table, x, scoring),
            _ => {}
        }
    }

    fn one(&mut self, table: &FieldTable, ty: &str, body: &Value, scoring: bool) {
        let b = body.as_object();
        let get = |k: &str| b.and_then(|m| m.get(k));
        match ty {
            "bool" => {
                self.children(table, get("must"), scoring);
                self.children(table, get("should"), scoring);
                self.children(table, get("filter"), false);
                self.children(table, get("must_not"), false);
            }
            "constant_score" => self.children(table, get("filter"), false),
            "boosting" => {
                self.children(table, get("positive"), scoring);
                self.children(table, get("negative"), scoring);
            }
            "dis_max" => self.children(table, get("queries"), scoring),
            "function_score" | "script_score" | "nested" | "has_child" | "has_parent" => self.children(table, get("query"), scoring),
            "ids" => self.note("_id", U_TERMS | U_POSTINGS),
            "exists" => {
                if let Some(name) = get("field").and_then(Value::as_str) {
                    for n in table.matching(name) {
                        if let Some(f) = table.get(&n) {
                            let used = if f.doc_values {
                                U_DOC_VALUES
                            } else if f.norms {
                                U_NORMS
                            } else {
                                U_TERMS | U_POSTINGS
                            };
                            self.note(&f.name.clone(), used);
                        }
                    }
                }
            }
            "match_all" | "match_none" | "wrapper" | "script" | "knn" => {
                if ty == "knn"
                    && let Some(f) = get("field").and_then(Value::as_str)
                {
                    self.note(f, U_KNN);
                }
            }
            "multi_match" | "query_string" | "simple_query_string" | "combined_fields" => {
                let fields: Vec<String> = match get("fields") {
                    Some(Value::Arr(a)) => a.iter().filter_map(Value::as_str).map(|s| s.split('^').next().unwrap_or(s).to_string()).collect(),
                    _ => match get("default_field").and_then(Value::as_str) {
                        Some(d) => vec![d.to_string()],
                        None => vec!["*".to_string()],
                    },
                };
                let phrase = get("type").and_then(Value::as_str).is_some_and(|t| t.starts_with("phrase"));
                for p in fields {
                    for name in table.matching(&p) {
                        if let Some(f) = table.get(&name) {
                            self.full_text(f, phrase, scoring);
                        }
                    }
                }
            }
            _ => {
                // Leaf queries name their field as the body's key (`{"term": {"f": ...}}`) or
                // with `field` (`terms_set`, `distance_feature`, `rank_feature`).
                let name = match get("field").and_then(Value::as_str) {
                    Some(f) => f.to_string(),
                    None => match b.and_then(|m| m.keys().find(|k| !matches!(k.as_str(), "boost" | "_name"))) {
                        Some(k) => k.clone(),
                        None => return,
                    },
                };
                let Some(f) = table.get(&name).cloned().or_else(|| table.keyed(&name)) else { return };
                let base = if f.flattened_key.is_some() { keyed_name(&f) } else { f.name.clone() };
                match ty {
                    "match" | "match_bool_prefix" => self.full_text(&f, false, scoring),
                    "match_phrase" | "match_phrase_prefix" => self.full_text(&f, true, scoring),
                    "span_term" | "span_near" | "span_first" | "span_or" | "span_multi" | "intervals" => {
                        self.note(&base, U_TERMS | U_POSTINGS | U_FREQS | U_POSITIONS | U_PROXIMITY)
                    }
                    "range" | "term" | "terms" if f.kind.is_numeric() || f.kind.is_date() || f.kind == Kind::Ip => {
                        let mut used = if f.index { U_POINTS } else { 0 };
                        if f.doc_values && (ty == "range" || !f.index) {
                            used |= U_DOC_VALUES;
                        }
                        self.note(&base, used);
                    }
                    "rank_feature" | "distance_feature" | "geo_distance" | "geo_bounding_box" | "geo_shape" | "geo_polygon" | "shape" => {
                        self.note(&base, U_POINTS)
                    }
                    _ => {
                        let mut used = U_TERMS | U_POSTINGS;
                        if scoring && ty == "term" {
                            used |= self.scoring_bits(&f);
                        }
                        self.note(&base, used);
                    }
                }
            }
        }
    }

    /// What scoring a term of `f` reads besides its postings: frequencies and norms.
    fn scoring_bits(&self, f: &FieldInfo) -> Used {
        let mut used = 0;
        if f.index_options as u8 >= IndexOptions::DocsAndFreqs as u8 {
            used |= U_FREQS;
        }
        if f.norms {
            used |= U_NORMS;
        }
        used
    }

    fn full_text(&mut self, f: &FieldInfo, phrase: bool, scoring: bool) {
        if f.kind.is_numeric() || f.kind.is_date() || f.kind == Kind::Ip {
            self.note(&f.name, if f.index { U_POINTS } else { U_DOC_VALUES });
            return;
        }
        let mut used = U_TERMS | U_POSTINGS;
        if phrase && f.kind.is_text() {
            used |= U_FREQS | U_POSITIONS | U_PROXIMITY;
        }
        if scoring {
            used |= self.scoring_bits(f);
        }
        self.note(&f.name, used);
    }
}

/// The Lucene field of a keyed `flattened` sub-field: `<root>._keyed`.
fn keyed_name(f: &FieldInfo) -> String {
    let key = f.flattened_key.as_deref().unwrap_or("");
    let root = f.name.strip_suffix(key).and_then(|r| r.strip_suffix('.')).unwrap_or(&f.name);
    format!("{root}._keyed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_once_per_request() {
        let mut s = ShardUsage::default();
        let mut used = BTreeMap::new();
        used.insert("a".to_string(), U_TERMS | U_POSTINGS);
        used.insert("b".to_string(), U_DOC_VALUES);
        s.record(&used);
        s.record(&used);
        let v = s.to_value(&[]);
        assert_eq!(v.get("all_fields").and_then(|a| a.get("any")).and_then(Value::as_i64), Some(4));
        assert_eq!(v.get("fields").and_then(|f| f.get("a")).and_then(|a| a.get("inverted_index")).and_then(|i| i.get("terms")).and_then(Value::as_i64), Some(2));
        let only_b = s.to_value(&["b".to_string()]);
        assert!(only_b.get("fields").and_then(|f| f.get("a")).is_none());
        // `all_fields` sums the fields shown.
        assert_eq!(only_b.get("all_fields").and_then(|a| a.get("any")).and_then(Value::as_i64), Some(2));
    }
}
