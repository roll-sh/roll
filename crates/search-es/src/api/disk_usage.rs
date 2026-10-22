//! `POST /{index}/_disk_usage` (`TransportAnalyzeIndexDiskUsageAction`): the bytes each field
//! takes in an index's segments, by index structure.
//!
//! Segments keep their structures in sections keyed by engine field
//! (`search_codec::container::SectionKind`), so most figures are exact section lengths:
//! the term dictionary, postings, positions and payloads make the inverted index, plus doc
//! values, points, norms, vectors (data, graph, quantized) and bloom filters. Stored fields share
//! one compressed section per segment: as Elasticsearch measures them by reading documents,
//! that section is split among the fields in proportion to the bytes of their stored values.
//! Engine fields map back to the mapping's field names (the layouts of a vector field and the
//! subfields of a join field keep their own names, as Elasticsearch's do).

use crate::api::Reply;
use crate::engine::fields::{FieldTable, IGNORED_FIELD, NESTED_PATH_FIELD, ROUTING_FIELD, SOURCE_FIELD};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::Ctx;
use crate::units::ByteSize;
use search_codec::container::SectionKind;
use std::collections::BTreeMap;

/// One field's bytes per structure.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Usage {
    inverted_index: u64,
    stored_fields: u64,
    doc_values: u64,
    points: u64,
    norms: u64,
    term_vectors: u64,
    knn_vectors: u64,
    bloom_filter: u64,
}

impl Usage {
    fn total(&self) -> u64 {
        self.inverted_index + self.stored_fields + self.doc_values + self.points + self.norms + self.term_vectors + self.knn_vectors + self.bloom_filter
    }

    fn add(&mut self, o: &Usage) {
        self.inverted_index += o.inverted_index;
        self.stored_fields += o.stored_fields;
        self.doc_values += o.doc_values;
        self.points += o.points;
        self.norms += o.norms;
        self.term_vectors += o.term_vectors;
        self.knn_vectors += o.knn_vectors;
        self.bloom_filter += o.bloom_filter;
    }

    /// `IndexDiskUsageStats.PerFieldDiskUsage.toXContent`: sizes with their readable forms.
    fn to_value(self) -> Value {
        let mut m = Map::new();
        let put = |m: &mut Map, k: &str, v: u64| {
            m.insert(k.into(), Value::from(ByteSize(v as i64).human()));
            m.insert(format!("{k}_in_bytes"), Value::from(v as i64));
        };
        put(&mut m, "total", self.total());
        let mut inv = Map::new();
        put(&mut inv, "total", self.inverted_index);
        m.insert("inverted_index".into(), Value::Obj(inv));
        put(&mut m, "stored_fields", self.stored_fields);
        put(&mut m, "doc_values", self.doc_values);
        put(&mut m, "points", self.points);
        put(&mut m, "norms", self.norms);
        put(&mut m, "term_vectors", self.term_vectors);
        put(&mut m, "knn_vectors", self.knn_vectors);
        put(&mut m, "bloom_filter", self.bloom_filter);
        Value::Obj(m)
    }
}

/// The field name an engine field reports under. With synthetic `_source` the stored document
/// is the synthesized one (Elasticsearch rebuilds it from doc values and keeps what they can't
/// hold in `_ignored_source`): it reports as `_ignored_source`, not as a stored `_source`.
fn field_name(table: &FieldTable, n: u32, synthetic: bool) -> String {
    use search_index::schema::{ID_FIELD, PRIMARY_TERM_FIELD, SEQ_NO_FIELD, VERSION_FIELD};
    match n {
        ID_FIELD => "_id".into(),
        SEQ_NO_FIELD => "_seq_no".into(),
        VERSION_FIELD => "_version".into(),
        PRIMARY_TERM_FIELD => "_primary_term".into(),
        SOURCE_FIELD if synthetic => "_ignored_source".into(),
        SOURCE_FIELD => "_source".into(),
        ROUTING_FIELD => "_routing".into(),
        IGNORED_FIELD => "_ignored".into(),
        NESTED_PATH_FIELD => "_nested_path".into(),
        _ => match table.by_number.get(&n) {
            Some(f) => f.name.clone(),
            None => {
                // An earlier layout of a vector field (`path\0layout`): the path.
                let name = table.schema.name(n);
                name.split('\u{0}').next().unwrap_or(&name).to_string()
            }
        },
    }
}

/// Adds one shard's usage per field to `out`.
fn shard_usage(table: &FieldTable, synthetic: bool, searcher: &search_index::Searcher, out: &mut BTreeMap<String, Usage>) -> EsResult<()> {
    let ie = crate::engine::convert::index_error;
    for seg in searcher.segments() {
        let mut stored_section = 0u64;
        for (kind, field, len) in seg.reader.sections() {
            match kind {
                SectionKind::Meta => continue,
                SectionKind::Stored | SectionKind::StoredDict => {
                    stored_section += len;
                    continue;
                }
                _ => {}
            }
            let u = out.entry(field_name(table, field, synthetic)).or_default();
            match kind {
                SectionKind::TermsIndex | SectionKind::TermsMeta | SectionKind::Docs | SectionKind::Positions | SectionKind::Payloads | SectionKind::Suggest => {
                    u.inverted_index += len
                }
                SectionKind::Norms => u.norms += len,
                SectionKind::DocValues => u.doc_values += len,
                SectionKind::Points => u.points += len,
                SectionKind::VectorData | SectionKind::VectorGraph | SectionKind::VectorQuantized => u.knn_vectors += len,
                SectionKind::TermsBloom => u.bloom_filter += len,
                SectionKind::Meta | SectionKind::Stored | SectionKind::StoredDict => {}
            }
        }
        if stored_section == 0 {
            continue;
        }
        // The stored section split by the raw bytes each field's values take.
        seg.ensure_stored().map_err(ie)?;
        let mut raw: BTreeMap<u32, u64> = BTreeMap::new();
        let mut reader = seg.reader.stored();
        for d in 0..seg.reader.max_doc() {
            if !seg.is_live(d) {
                continue;
            }
            for (f, v) in reader.document(d).map_err(|e| EsError::internal(e.to_string()))? {
                *raw.entry(f).or_insert(0) += stored_len(&v);
            }
        }
        let sum: u64 = raw.values().sum();
        if sum == 0 {
            continue;
        }
        let mut given = 0u64;
        let n = raw.len();
        for (i, (f, bytes)) in raw.into_iter().enumerate() {
            let share = if i + 1 == n { stored_section - given } else { (stored_section as u128 * bytes as u128 / sum as u128) as u64 };
            given += share;
            out.entry(field_name(table, f, synthetic)).or_default().stored_fields += share;
        }
    }
    Ok(())
}

fn stored_len(v: &search_codec::stored::OwnedStoredValue) -> u64 {
    use search_codec::stored::OwnedStoredValue as V;
    match v {
        V::Bytes(b) => b.len() as u64,
        V::Str(s) => s.len() as u64,
        V::Int(_) | V::Float(_) => 4,
        _ => 8,
    }
}

/// `POST /{index}/_disk_usage`.
pub fn disk_usage(ctx: &Ctx) -> EsResult<Reply> {
    if !ctx.bool_param("run_expensive_tasks", false)? {
        return Err(EsError::illegal_argument(
            "analyzing the disk usage of an index is expensive and resource-intensive, the parameter [run_expensive_tasks] must be set to [true] in order for the task to be performed.",
        ));
    }
    let flush = ctx.bool_param("flush", true)?;
    let expr: Vec<String> = ctx.list_param("index").unwrap_or_default();
    let st = ctx.state();
    let opts = ctx.indices_options(crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN)?;
    let names = crate::meta::resolve::resolve(&st, &expr, &opts, ctx.now_ms)?;
    let mut shards = 0u64;
    let mut out = Map::new();
    let mut per: Vec<(String, Value)> = Vec::new();
    for name in &names {
        let Some(idx) = st.metadata.indices.get(name).cloned() else { continue };
        if idx.is_closed() {
            continue;
        }
        let engine = ctx.cluster.engine.index(ctx.cluster, &st, &idx)?;
        if flush {
            engine.refresh_all()?;
        }
        let table = engine.table();
        let synthetic = crate::engine::synthetic::source_mode(&idx) == crate::engine::synthetic::SourceMode::Synthetic;
        let mut fields: BTreeMap<String, Usage> = BTreeMap::new();
        let mut store = 0u64;
        for s in &engine.shards {
            shards += 1;
            let searcher = s.shard.searcher();
            store += searcher.segments().iter().map(|g| g.reader.size_in_bytes()).sum::<u64>();
            shard_usage(&table, synthetic, &searcher, &mut fields)?;
        }
        let mut all = Usage::default();
        for u in fields.values() {
            all.add(u);
        }
        let mut fm = Map::new();
        for (k, u) in fields {
            fm.insert(k, u.to_value());
        }
        per.push((
            idx.name.clone(),
            obj! {
                "store_size" => ByteSize(store as i64).human(),
                "store_size_in_bytes" => store as i64,
                "all_fields" => all.to_value(),
                "fields" => Value::Obj(fm),
            },
        ));
    }
    out.insert("_shards".into(), obj! { "total" => shards as i64, "successful" => shards as i64, "failed" => 0 });
    for (k, v) in per {
        out.insert(k, v);
    }
    Reply::ok(Value::Obj(out))
}
