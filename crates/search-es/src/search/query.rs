//! The Query DSL against an index (`QueryBuilder.toQuery`): each parsed [`Query`] becomes an
//! engine query over the shard's field numbers, with the field type's rules (`MappedFieldType`
//! `termQuery` / `rangeQuery` / `prefixQuery` …), Elasticsearch's boosts (`BoostQuery` around
//! anything but match-none), named queries, unmapped-field handling and error texts.
//!
//! Full-text analysis (match family, multi_match, query_string) lives in [`super::text`].

use super::text;
use crate::cluster::Cluster;
use crate::engine::fields::{FieldInfo, FieldTable, IGNORED_FIELD, Kind, NESTED_PATH_FIELD, ROUTING_FIELD};
use crate::error::{EsError, EsResult};
use crate::json::{Number, Value};
use crate::meta::{ClusterState, IndexMeta};
use crate::query::Query;
use crate::query::term_level::Fuzziness;
use search_codec::points::encode;
use search_index::query::{
    BooleanQuery, BoostQuery, ConstantScoreQuery, DisMaxQuery, DocValuesRangeQuery, DocValuesTermsQuery, DvRange, FieldExistsQuery,
    MatchAllQuery, MatchNoneQuery, MultiTermQuery, Occur, PointInSetQuery, PointType, RewriteMethod,
    TermQuery, ToParentQuery,
};
use search_index::schema::ID_FIELD;
use search_index::{QueryRef, Searcher};
use std::cell::RefCell;
use std::sync::Arc;

/// What converting a query needs: the index's fields, its shard's searcher, the cluster.
pub struct QueryCtx<'a> {
    pub cluster: &'a Cluster,
    pub st: &'a ClusterState,
    pub idx: &'a IndexMeta,
    pub table: &'a Arc<FieldTable>,
    pub searcher: &'a Searcher,
    pub now_ms: i64,
    /// The nested scope a query runs in (`nested` queries set it for their inner query).
    pub nested: RefCell<Option<String>>,
    /// Named queries (`_name`), for `matched_queries`.
    pub named: RefCell<Vec<(String, QueryRef)>>,
    /// `search.allow_expensive_queries`.
    pub allow_expensive: bool,
    /// Inner hits definitions met while converting (`nested`, `has_child`, `has_parent`).
    pub inner_hits: RefCell<Vec<InnerHitsDef>>,
    /// The shard id within the index (random_score salting).
    pub shard: u32,
    /// Deprecation warnings raised while converting.
    pub warnings: RefCell<Vec<String>>,
    /// The request's `runtime_mappings` (the mapping's `runtime` section applies too).
    pub runtime_mappings: Option<crate::json::Map>,
    /// Constant-scoring join queries built so far (see [`super::join`]).
    pub constant_joins: std::cell::Cell<u32>,
    /// The searched alias's filter: `knn` queries take it as one more pre-filter.
    pub alias_filter: Option<crate::query::Query>,
    /// Converting the query of nested inner hits: `knn` queries become exact searches over
    /// every vector (`InnerHitsRewriteContext` → `ExactKnnQueryBuilder`).
    pub exact_knn: std::cell::Cell<bool>,
    /// Unmapped fields are errors (percolator queries, `allow_unmapped_fields: false`): the
    /// first one met is recorded in `unmapped`.
    pub strict_unmapped: std::cell::Cell<bool>,
    pub unmapped: RefCell<Option<String>>,
    /// `percolate` queries met (matched slots, the percolated documents' index).
    pub percolate: RefCell<Vec<Arc<super::percolate::Percolated>>>,
}

/// An `inner_hits` request found in the query tree.
#[derive(Clone)]
pub struct InnerHitsDef {
    pub name: String,
    pub spec: crate::query::joining::InnerHits,
    /// The nested path (nested inner hits).
    pub nested_path: Option<String>,
    /// The child query (filter of docs eligible as inner hits, scored).
    pub query: QueryRef,
    /// Parent/child inner hits (`has_child`, `has_parent`).
    pub join: Option<super::join::JoinInner>,
}

pub fn arc<Q: search_index::Query + 'static>(q: Q) -> QueryRef {
    Arc::new(q)
}

pub fn match_none(reason: &str) -> QueryRef {
    arc(MatchNoneQuery::new(reason))
}

/// `QueryShardException` ("failed to create query: …") wrapping a conversion error.
pub fn shard_exception(idx: &IndexMeta, e: EsError) -> EsError {
    if e.kind == "query_shard_exception" {
        return e;
    }
    let reason = format!("failed to create query: {}", e.reason());
    EsError::new(400, "query_shard_exception", reason).with_meta("index_uuid", idx.uuid.as_str()).with_meta("index", idx.name.as_str()).caused_by(e)
}

/// A `query_shard_exception` of its own (no cause).
pub fn qse(ctx: &QueryCtx, reason: impl Into<String>) -> EsError {
    EsError::new(400, "query_shard_exception", reason).with_meta("index_uuid", ctx.idx.uuid.as_str()).with_meta("index", ctx.idx.name.as_str())
}

impl<'a> QueryCtx<'a> {
    pub fn new(cluster: &'a Cluster, st: &'a ClusterState, idx: &'a IndexMeta, table: &'a Arc<FieldTable>, searcher: &'a Searcher, now_ms: i64) -> QueryCtx<'a> {
        let allow = st.metadata.cluster_setting("search.allow_expensive_queries").is_none_or(|v| v != "false");
        QueryCtx {
            cluster,
            st,
            idx,
            table,
            searcher,
            now_ms,
            nested: RefCell::new(None),
            named: RefCell::new(Vec::new()),
            allow_expensive: allow,
            inner_hits: RefCell::new(Vec::new()),
            shard: 0,
            warnings: RefCell::new(Vec::new()),
            runtime_mappings: None,
            constant_joins: std::cell::Cell::new(0),
            alias_filter: None,
            exact_knn: std::cell::Cell::new(false),
            strict_unmapped: std::cell::Cell::new(false),
            unmapped: RefCell::new(None),
            percolate: RefCell::new(Vec::new()),
        }
    }

    /// The runtime field `name` (concrete fields win over mapping-level runtime fields; a
    /// request's `runtime_mappings` shadow both).
    pub fn runtime_field(&self, name: &str) -> Option<Arc<super::runtime::RtField>> {
        let requested = self.runtime_mappings.as_ref().is_some_and(|r| r.contains_key(name) || name.rsplit_once('.').is_some_and(|(p, _)| r.contains_key(p)));
        if !requested && (self.idx.mapping.runtime.is_empty() || self.table.get(name).is_some()) {
            return None;
        }
        super::runtime::RuntimeFields::of(&self.idx.mapping, self.runtime_mappings.as_ref()).ok()?.get(name).cloned()
    }

    /// A query over runtime field values (`AbstractScriptFieldQuery`, constant scoring).
    fn runtime_query(&self, f: Arc<super::runtime::RtField>, pred: super::runtime::Predicate) -> EsResult<QueryRef> {
        if !self.allow_expensive {
            return Err(EsError::new(400, "elasticsearch_exception", "queries cannot be executed against runtime fields while [search.allow_expensive_queries] is set to [false]."));
        }
        let ev = super::runtime::Evaluator::new(&self.cluster.scripts, self.st, self.table, self.shard as i32, &self.idx.name, f)?;
        let q = search_index::query::ScriptQuery { script: Arc::new(super::runtime::RuntimeQuery { ev, pred: Arc::new(pred) }) };
        Ok(arc(ConstantScoreQuery::new(arc(q))))
    }

    /// Term-level and simple full-text queries on runtime fields.
    fn runtime_dispatch(&self, q: &Query) -> EsResult<Option<QueryRef>> {
        use super::runtime::{Predicate, query_value};
        use Query as Q;
        let field = match q {
            Q::Term(t) => &t.field,
            Q::Terms(t) if t.values.is_some() => &t.field,
            Q::Range(r) => &r.field,
            Q::Exists(e) => &e.field,
            Q::Prefix(p) => &p.field,
            Q::Match(m) => &m.field,
            _ => return Ok(None),
        };
        // `lookup` runtime fields only fetch (`LookupRuntimeFieldType` refuses queries).
        if self.table.get(field).is_none()
            && let Some(Value::Obj(def)) = self.runtime_mappings.as_ref().and_then(|r| r.get(field))
            && def.get("type").and_then(Value::as_str) == Some("lookup")
        {
            return Err(EsError::illegal_argument(format!("Field [{field}] of type [lookup] does not support {} queries", q.name())));
        }
        let Some(f) = self.runtime_field(field) else { return Ok(None) };
        let pred = match q {
            Q::Term(t) => Predicate::Terms(vec![query_value(&f, &t.value)?]),
            Q::Match(m) => Predicate::Terms(vec![query_value(&f, &m.query)?]),
            Q::Terms(t) => Predicate::Terms(t.values.iter().flatten().map(|v| query_value(&f, v)).collect::<EsResult<_>>()?),
            Q::Exists(_) => Predicate::Exists,
            Q::Prefix(p) => Predicate::Prefix(p.value.clone()),
            Q::Range(r) => Predicate::Range {
                lo: r.from.as_ref().filter(|v| !v.is_null()).map(|v| query_value(&f, v)).transpose()?,
                hi: r.to.as_ref().filter(|v| !v.is_null()).map(|v| query_value(&f, v)).transpose()?,
                include_lower: r.include_lower,
                include_upper: r.include_upper,
            },
            _ => return Ok(None),
        };
        self.runtime_query(f, pred).map(Some)
    }

    pub fn cluster_warn(&self, msg: &str) {
        self.warnings.borrow_mut().push(msg.to_string());
    }

    pub fn field(&self, name: &str) -> Option<Arc<FieldInfo>> {
        let f = self.table.get(name).cloned().or_else(|| self.table.keyed(name));
        if f.is_none() && self.strict_unmapped.get() && self.unmapped.borrow().is_none() {
            *self.unmapped.borrow_mut() = Some(name.to_string());
        }
        f
    }

    /// `KeyedFlattenedFieldType` refuses automaton queries.
    fn check_keyed(f: &FieldInfo, query: &str) -> EsResult<()> {
        if f.flattened_key.is_some() {
            return Err(EsError::java(400, "unsupported_operation_exception", format!("[{query}] queries are not currently supported on keyed [flattened] fields.")));
        }
        Ok(())
    }

    /// `SeqNoFieldMapper`: indices without sequence numbers refuse `_seq_no` queries and sorts.
    pub fn check_seq_no_queryable(&self) -> EsResult<()> {
        if crate::write::seq_no_disabled(self.idx) {
            return Err(EsError::illegal_argument("Cannot query field [_seq_no]: _seq_no cannot be queried when [index.disable_sequence_numbers] is [true]"));
        }
        Ok(())
    }

    /// Converts a query (boost and `_name` applied).
    pub fn to_query(&self, q: &Query) -> EsResult<QueryRef> {
        let inner = crate::stack::maybe_grow(|| self.do_to_query(q))?;
        let c = q.common();
        let out = if c.boost != 1.0 && !inner.as_any().is::<MatchNoneQuery>() { arc(BoostQuery::new(inner, c.boost)) } else { inner };
        if let Some(n) = &c.name {
            self.named.borrow_mut().push((n.clone(), out.clone()));
        }
        Ok(out)
    }

    /// Converts a query used as a filter (no scores needed; same structure).
    pub fn to_filter(&self, q: &Query) -> EsResult<QueryRef> {
        self.to_query(q)
    }

    fn expensive(&self, what: &str) -> EsResult<()> {
        if !self.allow_expensive {
            return Err(EsError::new(
                400,
                "exception",
                format!("[{what}] queries cannot be executed when 'search.allow_expensive_queries' is set to false."),
            ));
        }
        Ok(())
    }

    fn do_to_query(&self, q: &Query) -> EsResult<QueryRef> {
        use Query as Q;
        if (self.runtime_mappings.is_some() || !self.idx.mapping.runtime.is_empty())
            && let Some(rq) = self.runtime_dispatch(q)?
        {
            return Ok(rq);
        }
        match q {
            Q::MatchAll(_) => Ok(match_all(self)),
            Q::MatchNone(_) => Ok(match_none("User requested \"match_none\" query.")),
            Q::Bool(b) => self.bool_query(b),
            Q::Boosting(b) => {
                let pos = self.to_query(&b.positive)?;
                let neg = self.to_query(&b.negative)?;
                Ok(arc(search_index::query::BoostingQuery::new(pos, neg, b.negative_boost).map_err(index_err)?))
            }
            Q::ConstantScore(c) => {
                let inner = self.to_filter(&c.filter)?;
                Ok(arc(ConstantScoreQuery::new(inner)))
            }
            Q::DisMax(d) => {
                let mut subs = Vec::with_capacity(d.queries.len());
                for x in &d.queries {
                    subs.push(self.to_query(x)?);
                }
                if subs.is_empty() {
                    return Ok(match_none("No query left after rewrite."));
                }
                Ok(arc(DisMaxQuery::new(subs, d.tie_breaker)))
            }
            Q::Term(t) => {
                let Some(f) = self.field(&t.field) else { return self.unmapped_or_meta_term(&t.field, std::slice::from_ref(&t.value), t.case_insensitive) };
                self.term_query(&f, &t.value, t.case_insensitive)
            }
            Q::Terms(t) => self.terms_query(t),
            Q::Range(r) => super::range::range_query(self, r),
            Q::Exists(e) => self.exists_query(&e.field),
            Q::Prefix(p) => {
                self.check_regex_length(&p.value, "Prefix")?;
                self.prefix_query(&p.field, &p.value, p.case_insensitive, p.rewrite.as_deref())
            }
            Q::Wildcard(w) => self.wildcard_query(&w.field, &w.value, w.case_insensitive, w.rewrite.as_deref()),
            Q::Regexp(r) => self.regexp_query(r),
            Q::Fuzzy(fz) => self.fuzzy_query(fz),
            Q::Ids(ids) => {
                // `IdsQueryBuilder.doToQuery`: at most `index.max_result_window` ids.
                let max = self.idx.settings.get("index.max_result_window").and_then(|v| v.parse::<usize>().ok()).unwrap_or(10_000);
                if ids.ids.len() > max {
                    return Err(EsError::illegal_argument(format!("Too many ids specified, allowed max result window is [{max}]")));
                }
                Ok(ids_query(&ids.ids))
            }
            Q::Match(m) => text::match_query(self, m),
            Q::MatchPhrase(m) => text::match_phrase(self, m),
            Q::MatchPhrasePrefix(m) => text::match_phrase_prefix(self, m),
            Q::MatchBoolPrefix(m) => text::match_bool_prefix(self, m),
            Q::MultiMatch(m) => text::multi_match(self, m),
            Q::CombinedFields(c) => text::combined_fields(self, c),
            Q::QueryString(qs) => super::query_string::query_string(self, qs),
            Q::SimpleQueryString(qs) => super::query_string::simple_query_string(self, qs),
            Q::Nested(n) => self.nested_query(n),
            Q::Wrapper(w) => {
                let v = crate::json::parse(&w.source)?.unwrap_or(Value::Null);
                let (inner, _) = crate::query::parse_query(&v, None, &[])?;
                self.to_query(&inner)
            }
            other => super::extra::to_query(self, other),
        }
    }

    fn bool_query(&self, b: &crate::query::compound::Bool) -> EsResult<QueryRef> {
        if b.must.is_empty() && b.filter.is_empty() && b.must_not.is_empty() && b.should.is_empty() {
            return Ok(match_all(self));
        }
        let mut q = BooleanQuery::new();
        // Elasticsearch adds clauses in this order: must, must_not, should, filter.
        for x in &b.must {
            q = q.add(Occur::Must, self.to_query(x)?);
        }
        for x in &b.must_not {
            q = q.add(Occur::MustNot, self.to_filter(x)?);
        }
        for x in &b.should {
            q = q.add(Occur::Should, self.to_query(x)?);
        }
        for x in &b.filter {
            q = q.add(Occur::Filter, self.to_filter(x)?);
        }
        let required = b.must.len() + b.filter.len();
        let msm = match &b.minimum_should_match {
            Some(m) => calculate_msm(b.should.len() as i32, m)?,
            None => 0,
        };
        let msm = if b.minimum_should_match.is_none() && required == 0 && !b.should.is_empty() { 0 } else { msm };
        if msm > 0 {
            q = q.with_msm(msm as usize);
        }
        // A pure negative query matches everything else (`adjust_pure_negative`).
        if b.must.is_empty() && b.should.is_empty() && b.filter.is_empty() && !b.must_not.is_empty() && b.adjust_pure_negative {
            q = q.add(Occur::Filter, match_all(self));
        }
        Ok(arc(q))
    }

    /// A multi-term query on `f`: over postings when the field is indexed, over its doc values
    /// otherwise (`MultiTermQuery.DOC_VALUES_REWRITE`).
    pub fn multi_term(&self, f: &FieldInfo, q: MultiTermQuery) -> QueryRef {
        if !f.index && f.doc_values { super::dv_terms::DocValuesMultiTermQuery::query(q) } else { arc(q) }
    }

    /// The term query of a field type (`MappedFieldType.termQuery`).
    pub fn term_query(&self, f: &FieldInfo, value: &Value, case_insensitive: bool) -> EsResult<QueryRef> {
        let n = f.number;
        match f.kind {
            Kind::Keyword | Kind::Version | Kind::CountedKeyword | Kind::Flattened => {
                if !f.index && !f.doc_values {
                    return Err(not_searchable(f));
                }
                let s = value_text(value);
                if case_insensitive {
                    let s = match &f.flattened_key {
                        Some(k) => format!("{k}\0{s}"),
                        None => s,
                    };
                    return Ok(self.multi_term(f, MultiTermQuery::wildcard(n, &escape_wildcard(&s), true).map_err(index_err)?));
                }
                let t = normalize(f, &s);
                if f.index {
                    Ok(arc(TermQuery::new(n, t.into_bytes())))
                } else {
                    Ok(arc(ConstantScoreQuery::new(arc(DocValuesTermsQuery::new(n, [t.into_bytes()])))))
                }
            }
            Kind::Text | Kind::MatchOnlyText | Kind::SearchAsYouType => {
                if !f.index && !f.doc_values {
                    return Err(not_searchable(f));
                }
                let s = value_text(value);
                if !f.index {
                    // Doc values hold the whole value: an exact (slow) match on it.
                    if case_insensitive {
                        return Ok(self.multi_term(f, MultiTermQuery::wildcard(n, &escape_wildcard(&s), true).map_err(index_err)?));
                    }
                    return Ok(arc(ConstantScoreQuery::new(arc(DocValuesTermsQuery::new(n, [s.into_bytes()])))));
                }
                if case_insensitive {
                    return Ok(arc(MultiTermQuery::wildcard(n, &escape_wildcard(&s), true).map_err(index_err)?));
                }
                let q = arc(TermQuery::new(n, s.into_bytes()));
                Ok(if f.kind == Kind::MatchOnlyText { arc(ConstantScoreQuery::new(q)) } else { q })
            }
            Kind::Wildcard => {
                let s = value_text(value);
                super::extra::wildcard_field_query(self, f, &escape_wildcard(&s), case_insensitive)
            }
            Kind::ConstantKeyword => {
                let s = value_text(value);
                let matches = match &f.constant_value {
                    Some(c) => {
                        if case_insensitive {
                            c.to_lowercase() == s.to_lowercase()
                        } else {
                            *c == s
                        }
                    }
                    None => false,
                };
                Ok(if matches { arc(MatchAllQuery) } else { match_none("The \"term\" query doesn't match the constant value") })
            }
            Kind::Boolean => {
                if !f.index && !f.doc_values {
                    return Err(not_searchable(f));
                }
                let b = match value {
                    Value::Bool(b) => *b,
                    Value::Str(s) => match s.as_str() {
                        "true" => true,
                        "false" | "" => false,
                        _ => return Err(EsError::illegal_argument(format!("Can't parse boolean value [{s}], expected [true] or [false]"))),
                    },
                    other => return Err(EsError::illegal_argument(format!("Can't parse boolean value [{}], expected [true] or [false]", other.to_es_string()))),
                };
                if f.index {
                    Ok(arc(TermQuery::new(n, if b { b"T".to_vec() } else { b"F".to_vec() })))
                } else {
                    Ok(arc(ConstantScoreQuery::new(arc(DocValuesRangeQuery { field: n, range: DvRange::Long { lower: b as i64, upper: b as i64 } }))))
                }
            }
            k if k.is_numeric() => {
                let q = super::range::numeric_term(self, f, value)?;
                Ok(q)
            }
            Kind::Date | Kind::DateNanos => {
                let q = super::range::date_term(self, f, value)?;
                Ok(q)
            }
            Kind::Ip => super::range::ip_term(self, f, value),
            Kind::GeoPoint | Kind::GeoShape | Kind::Point | Kind::Shape => Err(EsError::illegal_argument(format!(
                "Geometry fields do not support exact searching, use dedicated geometry queries instead: [{}]",
                f.name
            ))),
            k if k.is_range() => super::range::range_field_term(self, f, value),
            Kind::Binary => Err(EsError::illegal_argument("Binary fields do not support searching".to_string())),
            Kind::DenseVector => Err(EsError::illegal_argument("[dense_vector] fields do not support term queries".to_string())),
            Kind::SparseVector | Kind::RankFeatures => {
                Err(EsError::illegal_argument(format!("[{}] fields do not support [term] queries", f.ty)))
            }
            Kind::RankFeature => Err(EsError::illegal_argument("[rank_feature] fields do not support [term] queries".to_string())),
            Kind::Completion => {
                let s = value_text(value);
                Ok(arc(TermQuery::new(n, s.into_bytes())))
            }
            Kind::Join => {
                let s = value_text(value);
                Ok(arc(TermQuery::new(n, s.into_bytes())))
            }
            _ => Err(EsError::illegal_argument(format!("Field [{}] of type [{}] does not support term queries", f.name, f.ty))),
        }
    }

    pub(crate) fn unmapped_or_meta_term(&self, field: &str, values: &[Value], case_insensitive: bool) -> EsResult<QueryRef> {
        match field {
            "_id" => {
                let ids: Vec<String> = values.iter().map(value_text).collect();
                Ok(ids_query(&ids))
            }
            "_index" => {
                let any = values.iter().any(|v| {
                    let s = value_text(v);
                    if case_insensitive { crate::xcontent::filter::glob_match(&s.to_lowercase(), &self.idx.name.to_lowercase()) } else { crate::xcontent::filter::glob_match(&s, &self.idx.name) }
                });
                Ok(if any { arc(MatchAllQuery) } else { match_none(&format!("The index [{}] doesn't match", self.idx.name)) })
            }
            "_tsid" if crate::engine::tsdb::is_time_series(self.idx) => Err(EsError::illegal_argument("[_tsid] is not searchable")),
            "_routing" => Ok(terms_on(ROUTING_FIELD, values.iter().map(value_text).collect())),
            "_ignored" => Ok(terms_on(IGNORED_FIELD, values.iter().map(value_text).collect())),
            "_nested_path" => Ok(terms_on(NESTED_PATH_FIELD, values.iter().map(value_text).collect())),
            "_tier" => {
                let tier = self.idx.settings.get("index.routing.allocation.include._tier_preference").unwrap_or("data_content");
                let first = tier.split(',').next().unwrap_or("");
                let any = values.iter().any(|v| crate::xcontent::filter::glob_match(&value_text(v), first));
                Ok(if any { arc(MatchAllQuery) } else { match_none("tier doesn't match") })
            }
            "_seq_no" => {
                self.check_seq_no_queryable()?;
                let mut pts = Vec::new();
                for v in values {
                    pts.push(encode::i64_bytes(super::range::parse_long_value(v)?).to_vec());
                }
                Ok(arc(ConstantScoreQuery::new(arc(PointInSetQuery::new(search_index::schema::SEQ_NO_FIELD, pts, PointType::Long)))))
            }
            _ => Ok(match_none(&format!("unmapped field [{field}]"))),
        }
    }

    fn terms_query(&self, t: &crate::query::term_level::Terms) -> EsResult<QueryRef> {
        let values: Vec<Value> = match (&t.values, &t.lookup) {
            (Some(v), _) => v.clone(),
            (None, Some(l)) => super::extra::terms_lookup(self, l)?,
            (None, None) => Vec::new(),
        };
        if values.is_empty() {
            return Ok(match_none("No terms supplied for \"terms\" query."));
        }
        let max = self.idx.settings.get("index.max_terms_count").and_then(|v| v.parse::<usize>().ok()).unwrap_or(65536);
        if values.len() > max {
            return Err(EsError::illegal_argument(format!(
                "The number of terms [{}] used in the Terms Query request has exceeded the allowed maximum of [{max}]. This maximum can be set by changing the [index.max_terms_count] index level setting.",
                values.len()
            )));
        }
        let Some(f) = self.field(&t.field) else { return self.unmapped_or_meta_term(&t.field, &values, false) };
        match f.kind {
            Kind::Keyword | Kind::Version | Kind::Text | Kind::MatchOnlyText | Kind::SearchAsYouType | Kind::Flattened | Kind::CountedKeyword => {
                if !f.index && !f.doc_values {
                    return Err(not_searchable(&f));
                }
                let terms: Vec<Vec<u8>> = values
                    .iter()
                    .map(|v| {
                        let s = value_text(v);
                        if f.kind.is_text() { s.into_bytes() } else { normalize(&f, &s).into_bytes() }
                    })
                    .collect();
                if f.index {
                    Ok(arc(ConstantScoreQuery::new(arc(MultiTermQuery::set(f.number, terms)))))
                } else {
                    Ok(arc(ConstantScoreQuery::new(arc(DocValuesTermsQuery::new(f.number, terms)))))
                }
            }
            Kind::Boolean | Kind::Ip | Kind::ConstantKeyword | Kind::Wildcard => {
                let mut b = BooleanQuery::new();
                for v in &values {
                    b = b.add(Occur::Should, self.term_query(&f, v, false)?);
                }
                Ok(arc(ConstantScoreQuery::new(arc(b))))
            }
            k if k.is_numeric() || k.is_date() => super::range::numeric_terms(self, &f, &values),
            _ => {
                let mut b = BooleanQuery::new();
                for v in &values {
                    b = b.add(Occur::Should, self.term_query(&f, v, false)?);
                }
                Ok(arc(ConstantScoreQuery::new(arc(b))))
            }
        }
    }

    /// `exists` (`ExistsQueryBuilder.newFilter`): one field, or every field under an object path.
    pub fn exists_query(&self, field: &str) -> EsResult<QueryRef> {
        let mut fields: Vec<Arc<FieldInfo>> = Vec::new();
        if field == "_seq_no" && crate::write::seq_no_disabled(self.idx) {
            return Ok(match_none("sequence numbers disabled"));
        }
        let shard_error = |msg: String| EsError::new(400, "query_shard_exception", msg).with_meta("index_uuid", self.idx.uuid.as_str()).with_meta("index", self.idx.name.as_str());
        match field {
            "_id" | "_index" | "_seq_no" | "_version" | "_tier" => return Ok(arc(ConstantScoreQuery::new(match_all(self)))),
            "_primary_term" | "_ignored_source" => return Ok(match_none("no such field")),
            "_source" => return Err(shard_error("The _source field is not searchable".into())),
            "_doc_count" => return Err(shard_error("Field [_doc_count] of type [_doc_count] does not support exists queries".into())),
            "_field_names" => return Err(EsError::java(500, "unsupported_operation_exception", "Cannot run exists query on _field_names")),
            "_nested_path" => return Err(EsError::java(500, "unsupported_operation_exception", "Cannot run exists() query against the nested field path")),
            _ => {}
        }
        if field == "_routing" {
            return Ok(arc(ConstantScoreQuery::new(arc(FieldExistsQuery { field: ROUTING_FIELD }))));
        }
        if field == "_ignored" {
            return Ok(arc(ConstantScoreQuery::new(arc(FieldExistsQuery { field: IGNORED_FIELD }))));
        }
        if field == "_tsid" && crate::engine::tsdb::is_time_series(self.idx) {
            return Ok(arc(ConstantScoreQuery::new(arc(FieldExistsQuery { field: crate::engine::fields::TSID_FIELD }))));
        }
        if let Some(f) = self.field(field) {
            fields.push(f);
        } else {
            // An object path or pattern: every leaf below it.
            let pattern = if field.contains('*') { field.to_string() } else { format!("{field}.*") };
            for name in self.table.matching(&pattern) {
                if let Some(f) = self.field(&name)
                    && f.parent.is_none()
                {
                    fields.push(f);
                }
            }
            if field.contains('*') {
                for name in self.table.matching(field) {
                    if let Some(f) = self.field(&name)
                        && !fields.iter().any(|x| x.number == f.number)
                    {
                        fields.push(f);
                    }
                }
            }
        }
        if fields.is_empty() {
            return Ok(match_none(&format!("No mappings yet for field [{field}]")));
        }
        let one = |f: &FieldInfo| -> QueryRef {
            match f.kind {
                Kind::ConstantKeyword => {
                    if f.constant_value.is_some() {
                        arc(MatchAllQuery)
                    } else {
                        match_none("constant_keyword without value")
                    }
                }
                Kind::RankFeature => arc(TermQuery::new(crate::engine::fields::FEATURE_FIELD, f.name.as_bytes().to_vec())),
                // A keyed flattened field exists where some term starts with its key.
                Kind::Flattened if f.flattened_key.is_some() => arc(MultiTermQuery::prefix(f.number, normalize(f, "").into_bytes())),
                _ => arc(FieldExistsQuery { field: f.number }),
            }
        };
        if fields.len() == 1 {
            return Ok(arc(ConstantScoreQuery::new(one(&fields[0]))));
        }
        let mut b = BooleanQuery::new();
        for f in &fields {
            b = b.add(Occur::Should, one(f));
        }
        Ok(arc(ConstantScoreQuery::new(arc(b))))
    }

    fn rewrite_of(&self, r: Option<&str>) -> EsResult<Option<RewriteMethod>> {
        match r {
            None => Ok(None),
            Some(s) => RewriteMethod::parse(s).map(Some).ok_or_else(|| EsError::illegal_argument(format!("[rewrite] unknown rewrite method [{s}]"))),
        }
    }

    /// `index.max_regex_length` for regexp patterns and prefix values (UTF-16 length, like
    /// Java's `String.length()`).
    pub fn check_regex_length(&self, value: &str, what: &str) -> EsResult<()> {
        let max_len = self.idx.settings.get("index.max_regex_length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1000);
        let len = value.encode_utf16().count();
        if len > max_len {
            let noun = if what == "Prefix" { "prefix" } else { "regex" };
            return Err(EsError::illegal_argument(format!(
                "The length of {noun} [{len}] used in the {what} Query request has exceeded the allowed maximum of [{max_len}]. This maximum can be set by changing the [index.max_regex_length] index level setting."
            )));
        }
        Ok(())
    }

    pub fn prefix_query(&self, field: &str, value: &str, case_insensitive: bool, rewrite: Option<&str>) -> EsResult<QueryRef> {
        let Some(f) = self.field(field) else {
            if field == "_id" {
                self.expensive("prefix")?;
                return Ok(arc(MultiTermQuery::prefix(ID_FIELD, value.as_bytes().to_vec())));
            }
            if field == "_index" {
                let ok = self.idx.name.starts_with(value);
                return Ok(if ok { arc(MatchAllQuery) } else { match_none("prefix doesn't match index") });
            }
            return Ok(match_none(&format!("unmapped field [{field}]")));
        };
        let rw = self.rewrite_of(rewrite)?;
        let wrap = |q: MultiTermQuery| -> QueryRef { self.multi_term(&f, match rw { Some(r) => q.with_rewrite(r), None => q }) };
        match f.kind {
            Kind::Keyword | Kind::Version | Kind::Text | Kind::SearchAsYouType | Kind::MatchOnlyText | Kind::Flattened | Kind::CountedKeyword => {
                if !f.index && !f.doc_values {
                    return Err(not_searchable(&f));
                }
                let walks_terms = || {
                    if self.allow_expensive {
                        return Ok(());
                    }
                    Err(EsError::new(
                        400,
                        "exception",
                        "[prefix] queries cannot be executed when 'search.allow_expensive_queries' is set to false. For optimised prefix queries on text fields please enable [index_prefixes].",
                    ))
                };
                if case_insensitive {
                    walks_terms()?;
                    let pattern = format!("{}*", escape_wildcard(value));
                    return Ok(wrap(MultiTermQuery::wildcard(f.number, &pattern, true).map_err(index_err)?));
                }
                // `index_prefixes`: a term query on the prefix field when the prefix fits.
                if let Some((pf, min, max)) = f.prefix_field.filter(|_| f.index) {
                    let len = value.chars().count();
                    if len >= min && len <= max && rw.is_none() {
                        return Ok(arc(ConstantScoreQuery::new(arc(TermQuery::new(pf, value.as_bytes().to_vec())))));
                    }
                }
                // `StringFieldType.prefixQuery`: a term-dictionary walk.
                walks_terms()?;
                let v = if f.kind.is_text() { value.to_string() } else { normalize(&f, value) };
                Ok(wrap(MultiTermQuery::prefix(f.number, v.into_bytes())))
            }
            Kind::Wildcard => super::extra::wildcard_field_query(self, &f, &format!("{}*", escape_wildcard(value)), case_insensitive),
            Kind::ConstantKeyword => {
                let ok = f.constant_value.as_deref().is_some_and(|c| if case_insensitive { c.to_lowercase().starts_with(&value.to_lowercase()) } else { c.starts_with(value) });
                Ok(if ok { arc(MatchAllQuery) } else { match_none("constant value doesn't match") })
            }
            _ => Err(EsError::new(
                400,
                "query_shard_exception",
                format!("Can only use prefix queries on keyword, text and wildcard fields - not on [{}] which is of type [{}]", f.name, f.ty),
            )
            .with_meta("index_uuid", self.idx.uuid.as_str())
            .with_meta("index", self.idx.name.as_str())),
        }
    }

    pub fn wildcard_query(&self, field: &str, value: &str, case_insensitive: bool, rewrite: Option<&str>) -> EsResult<QueryRef> {
        let Some(f) = self.field(field) else {
            if field == "_id" {
                return Ok(arc(MultiTermQuery::wildcard(ID_FIELD, value, case_insensitive).map_err(index_err)?));
            }
            if field == "_index" {
                let ok = crate::xcontent::filter::glob_match(value, &self.idx.name);
                return Ok(if ok { arc(MatchAllQuery) } else { match_none("wildcard doesn't match index") });
            }
            return Ok(match_none(&format!("unmapped field [{field}]")));
        };
        self.expensive("wildcard")?;
        Self::check_keyed(&f, "wildcard")?;
        let rw = self.rewrite_of(rewrite)?;
        match f.kind {
            Kind::Keyword | Kind::Version | Kind::Text | Kind::SearchAsYouType | Kind::MatchOnlyText | Kind::Flattened | Kind::CountedKeyword => {
                if !f.index && !f.doc_values {
                    return Err(not_searchable(&f));
                }
                let pattern = if f.kind.is_text() || f.normalizer.is_none() { value.to_string() } else { normalize_wildcard(&f, value) };
                let q = MultiTermQuery::wildcard(f.number, &pattern, case_insensitive).map_err(index_err)?;
                Ok(self.multi_term(&f, match rw {
                    Some(r) => q.with_rewrite(r),
                    None => q,
                }))
            }
            Kind::Wildcard => super::extra::wildcard_field_query(self, &f, value, case_insensitive),
            Kind::ConstantKeyword => {
                let ok = f.constant_value.as_deref().is_some_and(|c| {
                    if case_insensitive { crate::xcontent::filter::glob_match(&value.to_lowercase(), &c.to_lowercase()) } else { crate::xcontent::filter::glob_match(value, c) }
                });
                Ok(if ok { arc(MatchAllQuery) } else { match_none("constant value doesn't match") })
            }
            _ => Err(qse(self, format!("Can only use wildcard queries on keyword, text and wildcard fields - not on [{}] which is of type [{}]", f.name, f.ty))),
        }
    }

    fn regexp_query(&self, r: &crate::query::term_level::Regexp) -> EsResult<QueryRef> {
        let Some(f) = self.field(&r.field) else {
            if r.field == "_id" {
                return Ok(arc(MultiTermQuery::regexp(ID_FIELD, &r.value, crate::query::term_level::regexp_flags::to_codec(r.flags), r.case_insensitive, r.max_determinized_states as u32).map_err(index_err)?));
            }
            return Ok(match_none(&format!("unmapped field [{}]", r.field)));
        };
        self.expensive("regexp")?;
        Self::check_keyed(&f, "regexp")?;
        self.check_regex_length(&r.value, "Regexp")?;
        let rw = self.rewrite_of(r.rewrite.as_deref())?;
        match f.kind {
            Kind::Keyword | Kind::Version | Kind::Text | Kind::SearchAsYouType | Kind::MatchOnlyText | Kind::Flattened | Kind::CountedKeyword => {
                if !f.index && !f.doc_values {
                    return Err(not_searchable(&f));
                }
                let q = MultiTermQuery::regexp(f.number, &r.value, crate::query::term_level::regexp_flags::to_codec(r.flags), r.case_insensitive, r.max_determinized_states as u32).map_err(index_err)?;
                Ok(self.multi_term(&f, match rw {
                    Some(x) => q.with_rewrite(x),
                    None => q,
                }))
            }
            Kind::Wildcard => super::extra::wildcard_regexp(self, &f, r),
            Kind::ConstantKeyword => Ok(arc(MatchAllQuery)),
            _ => Err(qse(self, format!("Can only use regexp queries on keyword and text fields - not on [{}] which is of type [{}]", f.name, f.ty))),
        }
    }

    fn fuzzy_query(&self, fz: &crate::query::term_level::Fuzzy) -> EsResult<QueryRef> {
        let Some(f) = self.field(&fz.field) else { return Ok(match_none(&format!("unmapped field [{}]", fz.field))) };
        self.expensive("fuzzy")?;
        Self::check_keyed(&f, "fuzzy")?;
        let value = value_text(&fz.value);
        match f.kind {
            Kind::Keyword | Kind::Version | Kind::Text | Kind::SearchAsYouType | Kind::MatchOnlyText | Kind::Flattened | Kind::CountedKeyword => {
                let v = if f.kind.is_text() { value } else { normalize(&f, &value) };
                let edits = fuzzy_edits(&fz.fuzziness, &v);
                let mut q = MultiTermQuery::fuzzy(f.number, &v, edits, fz.prefix_length.max(0) as usize, fz.max_expansions.max(1) as usize, fz.transpositions);
                if let Some(r) = self.rewrite_of(fz.rewrite.as_deref())? {
                    q = q.with_rewrite(r);
                }
                if !f.index && !f.doc_values {
                    return Err(not_searchable(&f));
                }
                Ok(self.multi_term(&f, q))
            }
            k if k.is_numeric() || k.is_date() => super::range::numeric_fuzzy(self, &f, &fz.value, &fz.fuzziness),
            _ => Err(qse(self, format!("Can only use fuzzy queries on keyword and text fields - not on [{}] which is of type [{}]", f.name, f.ty))),
        }
    }

    fn nested_query(&self, n: &crate::query::joining::Nested) -> EsResult<QueryRef> {
        // `NestedQueryBuilder.doToQuery`: block joins are expensive.
        if !self.allow_expensive {
            return Err(EsError::new(400, "exception", "[joining] queries cannot be executed when 'search.allow_expensive_queries' is set to false."));
        }
        let Some(o) = self.table.objects.get(&n.path).cloned() else {
            if n.ignore_unmapped {
                return Ok(match_none(&format!("[nested] unmapped path [{}]", n.path)));
            }
            return Err(qse(self, format!("[nested] failed to find nested object under path [{}]", n.path)));
        };
        if !o.nested {
            return Err(qse(self, format!("[nested] nested object under path [{}] is not of nested type", n.path)));
        }
        let parent_filter: QueryRef = match &*self.nested.borrow() {
            None => search_index::query::nested::root_filter(),
            Some(p) => arc(TermQuery::new(NESTED_PATH_FIELD, p.as_bytes().to_vec())),
        };
        let prev = self.nested.replace(Some(n.path.clone()));
        let inner = self.to_query(&n.query);
        self.nested.replace(prev);
        let inner = inner?;
        let child_filter: QueryRef = arc(TermQuery::new(NESTED_PATH_FIELD, n.path.as_bytes().to_vec()));
        let child = arc(BooleanQuery::new().add(Occur::Must, inner.clone()).add(Occur::Filter, child_filter));
        let mode = match n.score_mode {
            crate::query::joining::JoinScoreMode::None => search_index::query::JoinScoreMode::None,
            crate::query::joining::JoinScoreMode::Avg => search_index::query::JoinScoreMode::Avg,
            crate::query::joining::JoinScoreMode::Max => search_index::query::JoinScoreMode::Max,
            crate::query::joining::JoinScoreMode::Min => search_index::query::JoinScoreMode::Min,
            crate::query::joining::JoinScoreMode::Total => search_index::query::JoinScoreMode::Total,
        };
        if let Some(ih) = &n.inner_hits {
            let name = ih.name.clone().unwrap_or_else(|| n.path.clone());
            // Inner hits rewrite `knn` queries to exact ones: every matching child counts.
            let ih_child = if crate::query::contains_query(&n.query, "knn") {
                let prev = self.nested.replace(Some(n.path.clone()));
                let was = self.exact_knn.replace(true);
                let named = self.named.borrow().len();
                let inner = self.to_query(&n.query);
                self.named.borrow_mut().truncate(named);
                self.exact_knn.set(was);
                self.nested.replace(prev);
                let filter: QueryRef = arc(TermQuery::new(NESTED_PATH_FIELD, n.path.as_bytes().to_vec()));
                arc(BooleanQuery::new().add(Occur::Must, inner?).add(Occur::Filter, filter))
            } else {
                child.clone()
            };
            self.inner_hits.borrow_mut().push(InnerHitsDef { name, spec: ih.clone(), nested_path: Some(n.path.clone()), query: ih_child, join: None });
        }
        Ok(arc(ToParentQuery { child, parent_filter, score_mode: mode }))
    }
}

/// `match_all` in the current scope: inside a nested query it is all docs of the scope.
pub fn match_all(_ctx: &QueryCtx) -> QueryRef {
    arc(MatchAllQuery)
}

fn terms_on(field: u32, values: Vec<String>) -> QueryRef {
    if values.len() == 1 {
        return arc(TermQuery::new(field, values[0].clone().into_bytes()));
    }
    arc(ConstantScoreQuery::new(arc(MultiTermQuery::set(field, values.into_iter().map(String::into_bytes)))))
}

/// The `ids` query (`TermInSetQuery` on `_id`).
pub fn ids_query(ids: &[String]) -> QueryRef {
    if ids.is_empty() {
        return match_none("Missing ids in \"ids\" query.");
    }
    arc(ConstantScoreQuery::new(arc(MultiTermQuery::set(ID_FIELD, ids.iter().map(|s| s.as_bytes().to_vec())))))
}

pub fn not_searchable(f: &FieldInfo) -> EsError {
    EsError::illegal_argument(format!("Cannot search on field [{}] since it is not indexed.", f.name))
}

pub fn index_err(e: search_index::Error) -> EsError {
    crate::engine::convert::index_error(e)
}

/// A query value as the text a term query looks for (`BytesRefs.toBytesRef`).
pub fn value_text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Num(Number::Float(f)) => crate::json::write::java_double(*f),
        other => other.to_es_string(),
    }
}

/// A keyword value through the field's normalizer (`normalize` for multi-term queries).
pub fn normalize(f: &FieldInfo, s: &str) -> String {
    let v = match &f.normalizer {
        Some(n) => n.normalize(s),
        None => s.to_string(),
    };
    // Keyed flattened terms carry their key.
    match &f.flattened_key {
        Some(k) => format!("{k}\0{v}"),
        None => v,
    }
}

/// Normalizes the literal parts of a wildcard pattern, keeping `*`, `?` and escapes.
fn normalize_wildcard(f: &FieldInfo, pattern: &str) -> String {
    let mut out = String::new();
    let mut lit = String::new();
    let mut chars = pattern.chars().peekable();
    let flush = |lit: &mut String, out: &mut String| {
        if !lit.is_empty() {
            out.push_str(&escape_wildcard(&normalize(f, lit)));
            lit.clear();
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '*' | '?' => {
                flush(&mut lit, &mut out);
                out.push(c);
            }
            '\\' => {
                if let Some(n) = chars.next() {
                    lit.push(n);
                }
            }
            c => lit.push(c),
        }
    }
    flush(&mut lit, &mut out);
    out
}

pub fn escape_wildcard(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `Fuzziness.asDistance(term)`.
pub fn fuzzy_edits(f: &Fuzziness, term: &str) -> u32 {
    match f {
        Fuzziness::Edits(n) => *n as u32,
        Fuzziness::Auto { low, high } => {
            let len = term.chars().count() as i32;
            if len < *low {
                0
            } else if len < *high {
                1
            } else {
                2
            }
        }
    }
}

/// `Queries.calculateMinShouldMatch`: an integer, a percentage, negative forms and
/// combinations (`3<90%`).
pub fn calculate_msm(optional: i32, spec: &str) -> EsResult<i32> {
    let s = spec.trim();
    if s.is_empty() {
        return Ok(0);
    }
    let bad = || EsError::illegal_argument(format!("failed to parse minimum_should_match [{spec}]"));
    if s.contains('<') {
        // Conditional specifications: "a<b c<d" applies the first whose number exceeds.
        let mut result = optional;
        for part in s.split_whitespace() {
            let (n, rest) = part.split_once('<').ok_or_else(bad)?;
            let n: i32 = n.trim().parse().map_err(|_| bad())?;
            if optional > n {
                result = calculate_msm(optional, rest)?;
            } else {
                break;
            }
        }
        return Ok(result);
    }
    let result = if let Some(p) = s.strip_suffix('%') {
        let pct: i32 = p.trim().parse().map_err(|_| bad())?;
        let calc = (pct as f32 * optional as f32 / 100.0) as i32;
        if pct < 0 { optional + calc } else { calc }
    } else {
        let n: i32 = s.parse().map_err(|_| bad())?;
        if n < 0 { optional + n } else { n }
    };
    Ok(if result < 0 { 0 } else { result.min(optional) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimum_should_match_like_elasticsearch() {
        assert_eq!(calculate_msm(4, "2").unwrap(), 2);
        assert_eq!(calculate_msm(4, "-1").unwrap(), 3);
        assert_eq!(calculate_msm(4, "75%").unwrap(), 3);
        assert_eq!(calculate_msm(4, "-25%").unwrap(), 3);
        assert_eq!(calculate_msm(3, "2<75%").unwrap(), 2);
        assert_eq!(calculate_msm(2, "2<75%").unwrap(), 2);
        assert_eq!(calculate_msm(10, "3<-25% 9<-3").unwrap(), 7);
        assert_eq!(calculate_msm(2, "5").unwrap(), 2);
        assert!(calculate_msm(2, "x").is_err());
    }

    #[test]
    fn wildcard_escaping() {
        assert_eq!(escape_wildcard("a*b?c\\"), "a\\*b\\?c\\\\");
    }
}
