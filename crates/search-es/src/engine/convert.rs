//! Parsed documents to engine documents: every typed [`FieldValue`] of a [`ParsedDocument`]
//! becomes the terms, points, doc values, vectors, completion entries and stored fields its
//! field type writes in Elasticsearch, using the [`FieldTable`] for field numbers, analyzers
//! and normalizers.
//!
//! Encodings follow Lucene's field classes so queries, sorts and aggregations (which decode
//! them) agree: `LongPoint`/`IntPoint`/`DoublePoint`/`FloatPoint`/`HalfFloatPoint` packed
//! points with `SortedNumericDocValuesField` (sortable longs for floating types),
//! `InetAddressPoint` 16-byte addresses with sorted-set doc values, `LatLonPoint` plus
//! `LatLonDocValuesField`, range fields as 2×N-dimension points plus `BinaryRangeUtil` doc
//! values, `KeywordFieldMapper` terms (normalized) with sorted-set doc values.
//!
//! Text values of one field are passed to the engine together, so the analyzer's position
//! gap separates them; a field-level `position_increment_gap` is applied by analyzing here
//! into pre-analyzed tokens.

use super::fields::{FieldInfo, FieldTable, IGNORED_FIELD, Kind, NESTED_PATH_FIELD, ROUTING_FIELD, SOURCE_FIELD, DOC_COUNT_FIELD, FEATURE_FIELD, TSID_FIELD, TS_ROUTING_HASH_FIELD};
use crate::document::{DocField, FieldValue, ParsedDocument};
use crate::error::{EsError, EsResult};
use crate::json::Value;
use bytes::Bytes;
use search_codec::points::encode;
use search_codec::stored::OwnedStoredValue;
use search_index::doc::{DocValue, IndexableDoc, IndexableField, PreToken, StoredField};
use std::collections::HashMap;
use std::sync::Arc;

/// What a document carries besides its fields.
pub struct DocMeta<'a> {
    pub id: &'a str,
    pub routing: Option<&'a str>,
    /// Time series documents: `_tsid` and `_ts_routing_hash` (instead of `_routing`).
    pub tsid: Option<(&'a [u8], &'a str)>,
    /// The `_source` bytes to store (already filtered by the mapping's includes/excludes).
    pub source: Option<Bytes>,
}

/// Builds the engine document.
pub fn to_indexable(table: &FieldTable, parsed: &ParsedDocument, meta: &DocMeta<'_>) -> EsResult<IndexableDoc> {
    let mut root = IndexableDoc::new(Bytes::copy_from_slice(meta.id.as_bytes()));
    // Nested documents form a flat block, children before parents (as the parser lists them).
    // Fields of nested objects with `include_in_parent`/`include_in_root` are copied up: to the
    // root, or to the parent nested document, which comes later in the block.
    let mut extra: Vec<Vec<&DocField>> = vec![Vec::new(); parsed.nested.len()];
    let mut root_extra: Vec<&DocField> = Vec::new();
    for (i, n) in parsed.nested.iter().enumerate() {
        let Some(o) = table.objects.get(&n.path) else { continue };
        let parent_is_root = o.parent_nested.is_none();
        let mine: Vec<&DocField> = n.fields.iter().chain(extra[i].clone()).collect();
        if o.include_in_root || (o.include_in_parent && parent_is_root) {
            root_extra.extend(mine.iter().copied());
        } else if o.include_in_parent
            && let Some(pp) = &o.parent_nested
            && let Some(j) = parsed.nested[i + 1..].iter().position(|x| &x.path == pp)
        {
            extra[i + 1 + j].extend(mine);
        }
    }
    let mut nested_docs: Vec<IndexableDoc> = Vec::with_capacity(parsed.nested.len());
    for (i, n) in parsed.nested.iter().enumerate() {
        let mut d = IndexableDoc::default();
        d.fields.push(IndexableField::Term { field: NESTED_PATH_FIELD, term: Bytes::copy_from_slice(n.path.as_bytes()) });
        let mut w = Writer::new(table, &n.fields, meta.id);
        for f in n.fields.iter().chain(extra[i].iter().copied()) {
            w.field(&mut d, f)?;
        }
        w.finish(&mut d);
        nested_docs.push(d);
    }
    let mut w = Writer::new(table, &parsed.fields, meta.id);
    for f in parsed.fields.iter().chain(root_extra) {
        w.field(&mut root, f)?;
    }
    w.finish(&mut root);
    if let Some(src) = &meta.source {
        root.stored.push(StoredField { field: SOURCE_FIELD, value: OwnedStoredValue::Bytes(src.to_vec()) });
    }
    if let Some((tsid, hash)) = meta.tsid {
        root.fields.push(IndexableField::DocValue { field: TSID_FIELD, value: DocValue::Sorted(Bytes::from(super::tsdb::tsid_dv(tsid))) });
        root.fields.push(IndexableField::DocValue { field: TS_ROUTING_HASH_FIELD, value: DocValue::Sorted(Bytes::copy_from_slice(hash.as_bytes())) });
    } else if let Some(r) = meta.routing {
        root.fields.push(IndexableField::Term { field: ROUTING_FIELD, term: Bytes::copy_from_slice(r.as_bytes()) });
        root.stored.push(StoredField { field: ROUTING_FIELD, value: OwnedStoredValue::Str(r.to_string()) });
    }
    for name in &parsed.ignored {
        let b = Bytes::copy_from_slice(name.as_bytes());
        root.fields.push(IndexableField::Term { field: IGNORED_FIELD, term: b.clone() });
        root.fields.push(IndexableField::DocValue { field: IGNORED_FIELD, value: DocValue::SortedSet(b) });
        root.stored.push(StoredField { field: IGNORED_FIELD, value: OwnedStoredValue::Str(name.clone()) });
    }
    if let Some(c) = parsed.doc_count {
        root.fields.push(IndexableField::DocValue { field: DOC_COUNT_FIELD, value: DocValue::Numeric(c) });
    }
    root.nested = nested_docs;
    Ok(root)
}

#[allow(clippy::type_complexity)]
struct Writer<'t> {
    table: &'t FieldTable,
    /// The fields of the document being written (completion context paths read them).
    doc: &'t [DocField],
    /// Text values per engine field (analyzed together).
    texts: HashMap<u32, (Arc<FieldInfo>, Vec<String>)>,
    text_order: Vec<u32>,
    /// Range values per field: one binary doc value holds all of a doc's ranges.
    ranges: Vec<(u32, Kind, Vec<(super::ranges::Bound, super::ranges::Bound)>)>,
    /// `binary` doc values per field: one doc value holds all of a doc's values.
    binaries: Vec<(u32, Vec<Vec<u8>>)>,
    /// The document's `_id` (a `join` parent's own join value).
    id: &'t str,
}

fn long_point(v: i64) -> Vec<u8> {
    encode::i64_bytes(v).to_vec()
}

impl<'t> Writer<'t> {
    fn new(table: &'t FieldTable, doc: &'t [DocField], id: &'t str) -> Writer<'t> {
        Writer { table, doc, texts: HashMap::new(), text_order: Vec::new(), ranges: Vec::new(), binaries: Vec::new(), id }
    }

    fn text(&mut self, f: &Arc<FieldInfo>, number: u32, v: &str) {
        match self.texts.get_mut(&number) {
            Some((_, vals)) => vals.push(v.to_string()),
            None => {
                let info = if number == f.number { f.clone() } else { self.table.by_number.get(&number).cloned().unwrap_or_else(|| f.clone()) };
                self.texts.insert(number, (info, vec![v.to_string()]));
                self.text_order.push(number);
            }
        }
    }

    fn finish(&mut self, d: &mut IndexableDoc) {
        for (n, k, all) in std::mem::take(&mut self.ranges) {
            d.fields.push(IndexableField::DocValue { field: n, value: DocValue::Binary(super::ranges::encode_dv(k, &all).into()) });
        }
        for (n, values) in std::mem::take(&mut self.binaries) {
            d.fields.push(IndexableField::DocValue { field: n, value: DocValue::Binary(encode_binary_dv(values).into()) });
        }
        for n in std::mem::take(&mut self.text_order) {
            let Some((f, values)) = self.texts.remove(&n) else { continue };
            let Some(a) = f.analyzer.clone() else { continue };
            if !f.index {
                continue;
            }
            match f.position_gap {
                Some(gap) if values.len() > 1 => {
                    // A field-level gap: analyze value by value, then join with the field's gap.
                    let mut tokens: Vec<PreToken> = Vec::new();
                    let mut offset_base = 0u32;
                    let mut first = true;
                    for v in &values {
                        let mut at_start = true;
                        let mut last_end = 0;
                        a.analyze(v, &mut |t| {
                            let mut inc = t.position_increment;
                            if at_start && !first {
                                inc = inc.saturating_add(gap.max(0) as u32);
                            }
                            at_start = false;
                            tokens.push(PreToken::new(Bytes::copy_from_slice(t.term.as_bytes()), inc, offset_base + t.start_offset, offset_base + t.end_offset));
                            last_end = t.end_offset;
                        });
                        let len16 = v.encode_utf16().count() as u32;
                        offset_base += len16.max(last_end) + a.offset_gap();
                        first = false;
                    }
                    d.fields.push(IndexableField::Tokens { field: n, tokens });
                }
                _ => d.fields.push(IndexableField::Text { field: n, analyzer: a, values }),
            }
        }
    }

    fn field(&mut self, d: &mut IndexableDoc, df: &DocField) -> EsResult<()> {
        let Some(f) = self.table.get(&df.name).cloned() else {
            return Ok(());
        };
        let n = f.number;
        match (&df.value, f.kind) {
            (FieldValue::Text(s), Kind::Text | Kind::MatchOnlyText | Kind::SearchAsYouType) => {
                self.text(&f, n, s);
                if let Some((pf, _, _)) = f.prefix_field {
                    self.text(&f, pf, s);
                }
                if let Some(pf) = f.phrase_field {
                    self.text(&f, pf, s);
                }
                for (_, sf) in &f.shingle_fields {
                    self.text(&f, *sf, s);
                }
                if f.store {
                    d.stored.push(StoredField { field: n, value: OwnedStoredValue::Str(s.clone()) });
                }
                if f.doc_values {
                    d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedSet(s.as_bytes().to_vec().into()) });
                }
                if let Some(fdn) = f.subfield("fielddata")
                    && let Some(a) = &f.analyzer
                {
                    let mut terms: Vec<Vec<u8>> = Vec::new();
                    a.analyze(s, &mut |t| terms.push(t.term.as_bytes().to_vec()));
                    for t in terms {
                        d.fields.push(IndexableField::DocValue { field: fdn, value: DocValue::SortedSet(t.into()) });
                    }
                }
            }
            // `constant_keyword` indexes nothing: its value lives in the mapping.
            (FieldValue::Text(_) | FieldValue::Keyword(_), Kind::ConstantKeyword) => {}
            (FieldValue::Text(s) | FieldValue::Keyword(s), Kind::Keyword | Kind::Version | Kind::Wildcard | Kind::CountedKeyword) => {
                self.keyword(d, &f, s);
            }
            (FieldValue::Long(v), k) if k.is_integral() || matches!(k, Kind::ScaledFloat(_) | Kind::Date | Kind::DateNanos) => {
                let v = *v;
                if f.index {
                    let packed = match k {
                        Kind::Integer | Kind::Short | Kind::Byte | Kind::TokenCount => encode::i32_bytes(v as i32).to_vec(),
                        _ => long_point(v),
                    };
                    d.fields.push(IndexableField::Point { field: n, packed });
                }
                if f.doc_values {
                    d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedNumeric(v) });
                }
                if f.store {
                    let sv = match k {
                        Kind::Integer | Kind::Short | Kind::Byte | Kind::TokenCount => OwnedStoredValue::Int(v as i32),
                        Kind::ScaledFloat(sf) => OwnedStoredValue::Double(v as f64 / sf),
                        _ => OwnedStoredValue::Long(v),
                    };
                    d.stored.push(StoredField { field: n, value: sv });
                }
            }
            (FieldValue::Date(v), _) => {
                if f.index {
                    d.fields.push(IndexableField::Point { field: n, packed: long_point(*v) });
                }
                if f.doc_values {
                    d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedNumeric(*v) });
                }
                if f.store {
                    d.stored.push(StoredField { field: n, value: OwnedStoredValue::Long(*v) });
                }
            }
            (FieldValue::UnsignedLong(v), _) => {
                let s = (*v ^ (1u64 << 63)) as i64;
                if f.index {
                    d.fields.push(IndexableField::Point { field: n, packed: long_point(s) });
                }
                if f.doc_values {
                    d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedNumeric(s) });
                }
                if f.store {
                    d.stored.push(StoredField { field: n, value: OwnedStoredValue::Long(s) });
                }
            }
            (FieldValue::Double(v), _) => {
                if f.index {
                    d.fields.push(IndexableField::Point { field: n, packed: encode::f64_bytes(*v).to_vec() });
                }
                if f.doc_values {
                    d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedNumeric(encode::f64_sortable(*v)) });
                }
                if f.store {
                    d.stored.push(StoredField { field: n, value: OwnedStoredValue::Double(*v) });
                }
            }
            (FieldValue::Float(v), k) => {
                if k == Kind::HalfFloat {
                    if f.index {
                        d.fields.push(IndexableField::Point { field: n, packed: encode::f16_bytes(*v).to_vec() });
                    }
                    if f.doc_values {
                        d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedNumeric(encode::f16_sortable(*v) as i64) });
                    }
                } else {
                    if f.index {
                        d.fields.push(IndexableField::Point { field: n, packed: encode::f32_bytes(*v).to_vec() });
                    }
                    if f.doc_values {
                        d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedNumeric(encode::f32_sortable(*v) as i64) });
                    }
                }
                if f.store {
                    d.stored.push(StoredField { field: n, value: OwnedStoredValue::Float(*v) });
                }
            }
            (FieldValue::Boolean(b), _) => {
                if f.index {
                    d.fields.push(IndexableField::Term { field: n, term: Bytes::from_static(if *b { b"T" } else { b"F" }) });
                }
                if f.doc_values {
                    d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedNumeric(*b as i64) });
                }
                if f.store {
                    d.stored.push(StoredField { field: n, value: OwnedStoredValue::Str(if *b { "T" } else { "F" }.into()) });
                }
            }
            (FieldValue::Ip(b), _) => {
                if f.index {
                    d.fields.push(IndexableField::Point { field: n, packed: b.to_vec() });
                }
                if f.doc_values {
                    d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedSet(Bytes::copy_from_slice(b)) });
                }
                if f.store {
                    d.stored.push(StoredField { field: n, value: OwnedStoredValue::Bytes(b.to_vec()) });
                }
            }
            (FieldValue::Binary(b), _) => {
                if f.doc_values {
                    match self.binaries.iter_mut().find(|(x, _)| *x == n) {
                        Some((_, v)) => v.push(b.clone()),
                        None => self.binaries.push((n, vec![b.clone()])),
                    }
                }
                if f.store {
                    d.stored.push(StoredField { field: n, value: OwnedStoredValue::Bytes(b.clone()) });
                }
            }
            (FieldValue::GeoPoint(p), _) => {
                if f.index {
                    d.fields.push(IndexableField::Point { field: n, packed: encode::lat_lon_bytes(p.lat, p.lon).to_vec() });
                }
                if f.doc_values {
                    d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedNumeric(search_index::util::geo::encode_geo_dv(p.lat, p.lon)) });
                }
                if f.store {
                    d.stored.push(StoredField { field: n, value: OwnedStoredValue::Str(format!("{},{}", p.lat, p.lon)) });
                }
            }
            (FieldValue::Point { x, y }, _) => {
                let (x, y) = (*x as f32, *y as f32);
                if f.index
                    && let Ok(p) = search_index::query::xy::point_bytes(x, y)
                {
                    d.fields.push(IndexableField::Point { field: n, packed: p.to_vec() });
                }
                if f.doc_values
                    && let Ok(v) = search_index::query::xy::dv_value(x, y)
                {
                    d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedNumeric(v) });
                }
            }
            (FieldValue::Shape(g), k) => {
                let tris = super::shapes::triangles(g, k == Kind::Shape)?;
                if f.index {
                    for t in &tris {
                        d.fields.push(IndexableField::Point { field: n, packed: t.to_vec() });
                    }
                }
                if f.doc_values {
                    d.fields.push(IndexableField::DocValue { field: n, value: DocValue::Binary(search_index::query::shape::doc_value(&tris)) });
                }
            }
            (FieldValue::Range { from, to, include_from, include_to }, k) => {
                let (lo, hi) = super::ranges::bounds(&f, k, from, to, *include_from, *include_to)?;
                if f.index {
                    let mut packed = lo.packed.clone();
                    packed.extend_from_slice(&hi.packed);
                    d.fields.push(IndexableField::Point { field: n, packed });
                }
                if f.doc_values {
                    match self.ranges.iter_mut().find(|(x, _, _)| *x == n) {
                        Some((_, _, all)) => all.push((lo, hi)),
                        None => self.ranges.push((n, k, vec![(lo, hi)])),
                    }
                }
            }
            (FieldValue::DenseVector(v), _) => {
                let mut b = Vec::with_capacity(v.len() * 4);
                for x in v {
                    b.extend_from_slice(&x.to_le_bytes());
                }
                self.vector(d, &f, b);
            }
            (FieldValue::ByteVector(v), _) => {
                let b: Vec<u8> = v.iter().map(|x| *x as u8).collect();
                self.vector(d, &f, b);
            }
            (FieldValue::Feature(v), _) => {
                let mut v = *v;
                if f.mapper.raw("positive_score_impact").and_then(Value::as_bool) == Some(false) {
                    v = 1.0 / v;
                }
                if let Ok(t) = search_index::query::feature::feature_token("_feature", Bytes::copy_from_slice(df.name.as_bytes()), v) {
                    d.fields.push(IndexableField::Tokens { field: FEATURE_FIELD, tokens: vec![t] });
                }
            }
            (FieldValue::Features(fs), _) => {
                let negative = f.mapper.raw("positive_score_impact").and_then(Value::as_bool) == Some(false);
                let mut tokens = Vec::with_capacity(fs.len());
                for (k, v) in fs {
                    let v = if negative { 1.0 / *v } else { *v };
                    if let Ok(t) = search_index::query::feature::feature_token(&f.name, Bytes::copy_from_slice(k.as_bytes()), v) {
                        tokens.push(t);
                    }
                }
                if !tokens.is_empty() {
                    d.fields.push(IndexableField::Tokens { field: n, tokens });
                }
            }
            (FieldValue::Flattened(pairs), _) => {
                for (k, v) in pairs {
                    if f.ignore_above.is_some_and(|m| v.chars().count() > m) {
                        continue;
                    }
                    let vb = Bytes::copy_from_slice(v.as_bytes());
                    let mut kb = Vec::with_capacity(k.len() + 1 + v.len());
                    kb.extend_from_slice(k.as_bytes());
                    kb.push(0);
                    kb.extend_from_slice(v.as_bytes());
                    let kb = Bytes::from(kb);
                    if f.index {
                        d.fields.push(IndexableField::Term { field: n, term: vb.clone() });
                    }
                    if let Some(kf) = f.keyed_field {
                        if f.index {
                            d.fields.push(IndexableField::Term { field: kf, term: kb.clone() });
                        }
                        if f.doc_values {
                            d.fields.push(IndexableField::DocValue { field: kf, value: DocValue::SortedSet(kb) });
                        }
                    }
                    if f.doc_values {
                        d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedSet(vb) });
                    }
                }
            }
            (FieldValue::Completion { inputs, weight, contexts }, _) => {
                let w = weight.unwrap_or(1).max(0) as u64;
                let defs = super::completion::context_defs(&f);
                // Context-enabled fields key every input under each of its typed contexts.
                let typed = if defs.is_empty() { Vec::new() } else { super::completion::index_contexts(&f, &defs, contexts.as_ref(), self.doc)? };
                for input in inputs {
                    let key = super::completion::index_key(&f, input);
                    if key.is_empty() {
                        continue;
                    }
                    if typed.is_empty() {
                        d.fields.push(IndexableField::Suggest { field: n, key: key.into(), weight: w, payload: Bytes::copy_from_slice(input.as_bytes()) });
                        continue;
                    }
                    for ctx in &typed {
                        let mut k = Vec::with_capacity(ctx.len() + 1 + key.len());
                        k.extend_from_slice(ctx);
                        k.push(super::completion::CONTEXT_SEP);
                        k.extend_from_slice(&key);
                        d.fields.push(IndexableField::Suggest { field: n, key: k.into(), weight: w, payload: Bytes::copy_from_slice(input.as_bytes()) });
                    }
                }
            }
            (FieldValue::TokenCount(s), _) => {
                let mut count = 0i64;
                if let Some(a) = &f.analyzer {
                    let skip_inc = f.mapper.raw("enable_position_increments").and_then(Value::as_bool) == Some(false);
                    a.analyze(s, &mut |t| {
                        if skip_inc || t.position_increment > 0 || count == 0 {
                            count += 1;
                        }
                    });
                }
                if f.index {
                    d.fields.push(IndexableField::Point { field: n, packed: encode::i32_bytes(count as i32).to_vec() });
                }
                if f.doc_values {
                    d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedNumeric(count) });
                }
                if f.store {
                    d.stored.push(StoredField { field: n, value: OwnedStoredValue::Int(count as i32) });
                }
            }
            (FieldValue::Percolator(q, _), Kind::Percolator) => {
                d.fields.push(IndexableField::DocValue { field: n, value: DocValue::Binary(Bytes::copy_from_slice(q.as_bytes())) });
            }
            (FieldValue::Join { name, parent }, Kind::Join) => {
                // The relation (type filter), then the join values: a child's parent id under its
                // parent type, a parent's own id under its own type.
                let nb = Bytes::copy_from_slice(name.as_bytes());
                d.fields.push(IndexableField::Term { field: n, term: nb.clone() });
                d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedSet(nb) });
                let joiner = crate::join::Joiner::from_mapper(&f.mapper);
                let mut join_value = |ty: &str, v: &str| {
                    if let Some((_, jn)) = f.subfields.iter().find(|(p, _)| p == ty) {
                        let vb = Bytes::copy_from_slice(v.as_bytes());
                        d.fields.push(IndexableField::Term { field: *jn, term: vb.clone() });
                        d.fields.push(IndexableField::DocValue { field: *jn, value: DocValue::Sorted(vb) });
                    }
                };
                if let (Some(p), Some(pt)) = (parent, joiner.parent_of(name)) {
                    join_value(pt, p);
                }
                if joiner.is_parent(name) {
                    join_value(name, self.id);
                }
            }
            // A value whose kind doesn't fit the field's (dynamic templates, copy_to into
            // another type): index it by the field's own rules from its text form.
            (FieldValue::Keyword(s), k) if k.is_text() => self.text(&f, n, s),
            _ => {}
        }
        Ok(())
    }

    fn keyword(&mut self, d: &mut IndexableDoc, f: &Arc<FieldInfo>, s: &str) {
        let n = f.number;
        if f.ignore_above.is_some_and(|m| s.chars().count() > m) {
            return;
        }
        let normalized: std::borrow::Cow<str> = match &f.normalizer {
            Some(norm) => std::borrow::Cow::Owned(norm.normalize(s)),
            None => std::borrow::Cow::Borrowed(s),
        };
        let b = Bytes::copy_from_slice(normalized.as_bytes());
        if f.index {
            d.fields.push(IndexableField::Term { field: n, term: b.clone() });
        }
        if f.doc_values {
            d.fields.push(IndexableField::DocValue { field: n, value: DocValue::SortedSet(b) });
        }
        if f.store {
            d.stored.push(StoredField { field: n, value: OwnedStoredValue::Str(s.to_string()) });
        }
    }

    fn vector(&mut self, d: &mut IndexableDoc, f: &Arc<FieldInfo>, b: Vec<u8>) {
        let Some(spec) = f.vector else { return };
        if spec.dims == 0 {
            return;
        }
        if f.index {
            d.fields.push(IndexableField::Vector { field: f.number, value: b.into() });
        } else {
            d.fields.push(IndexableField::DocValue { field: f.number, value: DocValue::Binary(b.into()) });
        }
    }
}

/// An error a lower crate returned while indexing, as Elasticsearch reports it.
pub fn index_error(e: search_index::Error) -> EsError {
    let status = e.status();
    let kind = e.kind().to_string();
    let mut reason = e.to_string();
    if kind == "script_exception" {
        let (full, text) = crate::search::scripts::take_script_error(&reason);
        if let Some(full) = full {
            return full;
        }
        reason = text.to_string();
    }
    if search_analysis::is_java_exception(&kind) || kind == "i_o_exception" {
        EsError::java(status, kind, reason)
    } else {
        EsError::new(status, kind, reason)
    }
}

/// `BinaryFieldMapper.CustomBinaryDocValuesField`: a doc's values sorted and deduplicated,
/// as a vint count then each value as vint length and bytes.
pub fn encode_binary_dv(mut values: Vec<Vec<u8>>) -> Vec<u8> {
    values.sort();
    values.dedup();
    let mut out = Vec::with_capacity(values.iter().map(|v| v.len() + 2).sum::<usize>() + 2);
    let vint = |out: &mut Vec<u8>, mut x: usize| {
        while x >= 0x80 {
            out.push((x as u8 & 0x7f) | 0x80);
            x >>= 7;
        }
        out.push(x as u8);
    };
    vint(&mut out, values.len());
    for v in &values {
        vint(&mut out, v.len());
        out.extend_from_slice(v);
    }
    out
}

/// The values of a `binary` doc value written by [`encode_binary_dv`].
pub fn decode_binary_dv(mut b: &[u8]) -> Vec<&[u8]> {
    fn vint(b: &mut &[u8]) -> Option<usize> {
        let mut x = 0usize;
        for shift in (0..35).step_by(7) {
            let (&c, rest) = b.split_first()?;
            *b = rest;
            x |= ((c & 0x7f) as usize) << shift;
            if c & 0x80 == 0 {
                return Some(x);
            }
        }
        None
    }
    let mut out = Vec::new();
    let Some(n) = vint(&mut b) else { return out };
    for _ in 0..n {
        let Some(len) = vint(&mut b) else { break };
        if len > b.len() {
            break;
        }
        out.push(&b[..len]);
        b = &b[len..];
    }
    out
}

#[cfg(test)]
mod binary_dv_tests {
    use super::*;

    #[test]
    fn binary_doc_values_round_trip_sorted_unique() {
        let enc = encode_binary_dv(vec![vec![1, 2], vec![0, 0, 0], vec![1, 2], vec![0; 200]]);
        let dec = decode_binary_dv(&enc);
        assert_eq!(dec, vec![&[0u8, 0, 0][..], &[0u8; 200][..], &[1u8, 2][..]]);
        assert!(decode_binary_dv(&[]).is_empty());
        // A truncated value stops decoding instead of reading past the end.
        assert!(decode_binary_dv(&[1, 5, 1]).is_empty());
    }
}
