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
    /// Inner hits of `nested` queries inside this one's query: rendered under each of this
    /// definition's hits (`InnerHitsContext` tree).
    pub children: Vec<InnerHitsDef>,
}

pub fn arc<Q: search_index::Query + 'static>(q: Q) -> QueryRef {
    Arc::new(q)
}

/// What the shard-independent part of `QueryBuilder.rewrite` turns a query into, when that is
/// `match_all` (`Some(true)`) or `match_none` (`Some(false)`).
pub(crate) fn rewrites_to(q: &crate::query::Query) -> Option<bool> {
    use crate::query::Query;
    match q {
        Query::MatchAll(_) => Some(true),
        Query::MatchNone(_) => Some(false),
        Query::Bool(b) => bool_rewrite(b),
        // `ConstantScoreQueryBuilder.doRewrite`: a filter that matches nothing.
        Query::ConstantScore(c) => (rewrites_to(&c.filter) == Some(false)).then_some(false),
        // `IdsQueryBuilder.doRewrite`.
        Query::Ids(i) if i.ids.is_empty() => Some(false),
        // `BoostingQueryBuilder.doRewrite` / `FunctionScoreQueryBuilder.doRewrite`: a positive
        // (inner) query that rewrites to any `MatchNoneQueryBuilder`, a user's one included.
        Query::Boosting(b) => (rewrites_to(&b.positive) == Some(false)).then_some(false),
        Query::FunctionScore(f) => (rewrites_to(&f.query) == Some(false)).then_some(false),
        _ => None,
    }
}

/// `BoolQueryBuilder.doRewrite`: no clause is `match_all`; a required clause that matches
/// nothing, a prohibited one that matches everything, or only optional clauses that all match
/// nothing make `match_none`.
fn bool_rewrite(b: &crate::query::compound::Bool) -> Option<bool> {
    if b.must.is_empty() && b.filter.is_empty() && b.must_not.is_empty() && b.should.is_empty() {
        return Some(true);
    }
    if b.must.is_empty() && b.filter.is_empty() && b.must_not.is_empty() && b.should.iter().all(|q| rewrites_to(q) == Some(false)) {
        return Some(false);
    }
    let none = |qs: &[crate::query::Query]| qs.iter().any(|q| rewrites_to(q) == Some(false));
    if none(&b.must) || none(&b.filter) || b.must_not.iter().any(|q| rewrites_to(q) == Some(true)) {
        return Some(false);
    }
    None
}

pub fn match_none(reason: &str) -> QueryRef {
    arc(MatchNoneQuery::new(reason))
}

/// `QueryShardException` ("failed to create query: …") wrapping a conversion error
/// (`SearchExecutionContext.toQuery` rethrows `QueryShardException`s and `ParsingException`s).
pub fn shard_exception(idx: &IndexMeta, e: EsError) -> EsError {
    if e.kind == "query_shard_exception" || e.kind == "parsing_exception" {
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

    /// The runtime field `name`: the mapping's `runtime` section shadows concrete fields of
    /// the same name, a request's `runtime_mappings` shadow both.
    pub fn runtime_field(&self, name: &str) -> Option<Arc<super::runtime::RtField>> {
        if self.idx.mapping.runtime.is_empty() && self.runtime_mappings.as_ref().is_none_or(|r| r.is_empty()) {
            return None;
        }
        // The shard's scope has them parsed already.
        if let Some(scope) = super::runtime::RuntimeScope::ambient()
            && scope.index_name() == self.idx.name
        {
            return scope.field(name).cloned();
        }
        super::runtime::RuntimeFields::of_index(&self.idx.mapping, self.runtime_mappings.as_ref()).ok()?.get(name).cloned()
    }

    /// A query over runtime field values (`AbstractScriptFieldQuery`, constant scoring).
    pub(crate) fn runtime_query(&self, f: Arc<super::runtime::RtField>, pred: super::runtime::Predicate) -> EsResult<QueryRef> {
        if !self.allow_expensive {
            return Err(EsError::new(400, "exception", "queries cannot be executed against runtime fields while [search.allow_expensive_queries] is set to [false]."));
        }
        let ev = super::runtime::Evaluator::new(&self.cluster.scripts, self.st, self.table, self.shard as i32, &self.idx.name, f)?;
        // `AbstractScriptFieldQuery`: constant scoring, its own description and explanation.
        let q = search_index::query::ScriptQuery { script: Arc::new(super::runtime::RuntimeQuery { ev, pred: Arc::new(pred) }) };
        Ok(arc(q))
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
    /// OpenSearch's `flat_object` sub-fields take them (`opensearch::flat_object`).
    fn check_keyed(f: &FieldInfo, query: &str) -> EsResult<()> {
        if f.flattened_key.is_some() && !crate::opensearch::flat_object::is_keyed(f) {
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
        // OpenSearch's shard rewrite of a date range its shard's values all fall in: a new
        // `RangeQueryBuilder` without bounds (keeping boost and name, as
        // `AbstractQueryBuilder.rewrite` carries them over), which builds an `exists` query
        // (`RangeQueryBuilder.doRewrite`, `WITHIN`).
        let within = match q {
            Query::Range(r) if crate::opensearch::active() => self.os_date_range_relation(r)?,
            _ => None,
        };
        let inner = match within {
            Some(exists) => exists,
            None => crate::stack::maybe_grow(|| self.do_to_query(q))?,
        };
        let c = q.common();
        let out = if c.boost != 1.0 && !inner.as_any().is::<MatchNoneQuery>() { arc(BoostQuery::new(inner, c.boost)) } else { inner };
        if let Some(n) = &c.name {
            self.named.borrow_mut().push((n.clone(), out.clone()));
        }
        Ok(out)
    }

    /// OpenSearch's `DateFieldType.isFieldWithinQuery` (the only field type whose ranges
    /// know the shard's bounds): when every indexed value of the shard lies in the range — what
    /// Lucene's point range rewrite also finds (it becomes `exists` or `match_all`) — the
    /// range's `exists` query; when none can (no points, or all outside) `match_none`.
    fn os_date_range_relation(&self, r: &crate::query::term_level::Range) -> EsResult<Option<QueryRef>> {
        if self.runtime_mappings.as_ref().is_some_and(|m| m.contains_key(&r.field)) || self.idx.mapping.runtime.contains_key(&r.field) {
            return Ok(None);
        }
        let Some(f) = self.field(&r.field) else { return Ok(None) };
        if !f.kind.is_date() || !f.index {
            return Ok(None);
        }
        let Ok(lq) = super::range::range_query(self, r) else { return Ok(None) };
        let Ok(rw) = self.searcher.rewrite(&lq) else { return Ok(None) };
        // The points side decides (a date range is a point range, alone or paired with its
        // doc-values twin).
        let points = match rw.as_any().downcast_ref::<search_index::query::range::IndexOrDocValuesQuery>() {
            Some(iodv) => iodv.index.clone(),
            None => rw,
        };
        let any = points.as_any();
        if any.is::<FieldExistsQuery>() || any.is::<MatchAllQuery>() {
            self.exists_query(&r.field).map(Some)
        } else if any.is::<MatchNoneQuery>() {
            // `DISJOINT`: a `MatchNoneQueryBuilder`.
            Ok(Some(match_none("User requested \"match_none\" query.")))
        } else {
            Ok(None)
        }
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
            // OpenSearch builds `match_all` and numeric / date ranges as approximations.
            Q::MatchAll(_) if crate::opensearch::active() => Ok(crate::opensearch::approximate::ApproximateScoreQuery::match_all()),
            Q::MatchAll(_) => Ok(match_all(self)),
            Q::MatchNone(_) => Ok(match_none("User requested \"match_none\" query.")),
            Q::Bool(b) => self.bool_query(b),
            Q::Boosting(b) => {
                let pos = self.to_query(&b.positive)?;
                // `BoostingQueryBuilder.doRewrite`: a positive query that rewrites to
                // `match_none` replaces the whole query; the negative one is never built.
                if rewrites_to(&b.positive) == Some(false) || rewritten_to_none(&pos) {
                    return Ok(pos);
                }
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
                let Some(f) = self.field(&t.field) else { return self.unmapped_or_meta_term(&t.field, std::slice::from_ref(&t.value), t.case_insensitive, "term") };
                let q = self.term_query(&f, &t.value, t.case_insensitive)?;
                Ok(constant_rewritten(&f, q))
            }
            Q::Terms(t) => self.terms_query(t),
            Q::Range(r) if crate::opensearch::active() && self.field(&r.field).is_some_and(|f| f.kind.is_numeric() || f.kind.is_date()) => {
                super::range::range_query(self, r).map(crate::opensearch::approximate::ApproximateScoreQuery::range)
            }
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
                // `IdsQueryBuilder.doRewrite` runs before any shard sees the query.
                if ids.ids.is_empty() {
                    return Ok(match_none("The \"ids\" query was rewritten to a \"match_none\" query."));
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
            Q::Kql(k) => self.to_query(&super::kql::rewrite(self, k)?),
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
        // The rewrite runs before any clause becomes a Lucene query, so a clause that would
        // fail to build (a bad analyzer, mixed analyzers…) never does.
        if bool_rewrite(b) == Some(false) || crate::opensearch::can_match::bool_matches_none(self, b) {
            return Ok(match_none("The \"bool\" query was rewritten to a \"match_none\" query."));
        }
        let mut q = BooleanQuery::new();
        // Elasticsearch adds clauses in this order: must, must_not, should, filter.
        for x in &b.must {
            q = q.add(Occur::Must, self.to_query(x)?);
        }
        // `BoolQueryBuilder.extractInnerHitBuilders` skips `must_not`: no hits come from it.
        let mark = self.inner_hits.borrow().len();
        for x in &b.must_not {
            q = q.add(Occur::MustNot, self.to_filter(x)?);
        }
        self.inner_hits.borrow_mut().truncate(mark);
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
        if f.kind == Kind::AggregateMetric {
            return super::aggregate_metric::terms(self, f, std::slice::from_ref(value), true);
        }
        let n = f.number;
        if f.kind == Kind::Version {
            // `indexedValueForSearch`: the encoded value; case-insensitively, the value's
            // letters in any case among the decoded terms.
            let s = value_text(value);
            if case_insensitive {
                let aut = search_codec::automaton::Dfa::wildcard(&escape_wildcard(&s), true, search_codec::automaton::dfa::DEFAULT_WORK_LIMIT)
                    .map_err(|e| EsError::new(400, "too_complex_to_determinize_exception", e.to_string()))?;
                let aut = Arc::new(super::version_field::Decoded::new(Arc::new(aut)));
                return Ok(arc(MultiTermQuery::automaton(n, aut, escape_wildcard(&s))));
            }
            return Ok(arc(TermQuery::new(n, super::version_field::encode(&s)?)));
        }
        match f.kind {
            Kind::Keyword | Kind::CountedKeyword | Kind::Flattened => {
                if !f.index && !f.doc_values {
                    return Err(not_searchable(f));
                }
                let s = value_text(value);
                // An empty value has no case: Elasticsearch builds a plain term query.
                if case_insensitive && !s.is_empty() {
                    return Ok(match &f.flattened_key {
                        Some(k) => self.multi_term(f, ci_term(n, &format!("{k}\0{s}"))?),
                        None => ci_described(f, &s, self.multi_term(f, ci_term(n, &s)?)),
                    });
                }
                let t = normalize(f, &s);
                if f.index {
                    Ok(crate::opensearch::keyword_term(f, arc(TermQuery::new(n, t.into_bytes()))))
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
                    if case_insensitive && !s.is_empty() {
                        return Ok(ci_described(f, &s, self.multi_term(f, ci_term(n, &s)?)));
                    }
                    return Ok(arc(ConstantScoreQuery::new(arc(DocValuesTermsQuery::new(n, [s.into_bytes()])))));
                }
                if case_insensitive && !s.is_empty() {
                    return Ok(ci_described(f, &s, arc(ci_term(n, &s)?)));
                }
                let q = arc(TermQuery::new(n, s.into_bytes()));
                Ok(if f.kind == Kind::MatchOnlyText { arc(ConstantScoreQuery::new(q)) } else { q })
            }
            Kind::Wildcard => {
                let s = wildcard_normalized(f, &value_text(value));
                if !case_insensitive && !crate::opensearch::active() {
                    return Ok(super::extra::wildcard_terms(f, vec![s]));
                }
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
                // `Queries.NO_DOCS_INSTANCE`; the `term` builder's own rewrite names itself
                // (see `constant_rewritten`).
                Ok(if matches { arc(MatchAllQuery) } else { match_none("") })
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
            // `FeatureField.newLinearQuery`: the score is the feature's stored value (the
            // inverse for `positive_score_impact: false`), times the boost.
            Kind::SparseVector | Kind::RankFeatures => {
                search_index::query::FeatureQuery::linear(n, value_text(value).into_bytes(), 1.0).map_err(index_err)
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

    /// A `term`-like query (`query_name`) on a metadata or unmapped field.
    pub(crate) fn unmapped_or_meta_term(&self, field: &str, values: &[Value], case_insensitive: bool, query_name: &str) -> EsResult<QueryRef> {
        match field {
            "_id" => {
                let ids: Vec<String> = values.iter().map(value_text).collect();
                Ok(ids_query(&ids))
            }
            "_index" => {
                // Index names are lower case: a case-insensitive match lower-cases the pattern.
                let any = values.iter().any(|v| {
                    let s = value_text(v);
                    self.index_matches(&if case_insensitive { s.to_lowercase() } else { s })
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
            // `TermQueryBuilder.doRewrite` and its kin.
            _ => Ok(match_none(&format!("The \"{query_name}\" query is against a field that does not exist"))),
        }
    }

    fn terms_query(&self, t: &crate::query::term_level::Terms) -> EsResult<QueryRef> {
        // Borrowed when the request lists the values (a 45,000-term list is not copied per
        // shard).
        let mut values: std::borrow::Cow<'_, [Value]> = match (&t.values, &t.lookup) {
            (Some(v), _) => std::borrow::Cow::Borrowed(v.as_slice()),
            (None, Some(l)) => std::borrow::Cow::Owned(super::extra::terms_lookup(self, l)?),
            (None, None) => std::borrow::Cow::Owned(Vec::new()),
        };
        if values.is_empty() {
            return Ok(match_none("No terms supplied for \"terms\" query."));
        }
        if t.bitmap {
            values = std::borrow::Cow::Owned(crate::opensearch::bitmap::terms(self, &t.field, &values)?);
            if values.is_empty() {
                return Ok(match_none("empty bitmap"));
            }
        }
        let max = self.idx.settings.get("index.max_terms_count").and_then(|v| v.parse::<usize>().ok()).unwrap_or(65536);
        if values.len() > max {
            return Err(EsError::illegal_argument(format!(
                "The number of terms [{}] used in the Terms Query request has exceeded the allowed maximum of [{max}]. This maximum can be set by changing the [index.max_terms_count] index level setting.",
                values.len()
            )));
        }
        let Some(f) = self.field(&t.field) else { return self.unmapped_or_meta_term(&t.field, &values, false, "terms") };
        if f.kind == Kind::AggregateMetric {
            return super::aggregate_metric::terms(self, &f, &values, false);
        }
        match f.kind {
            Kind::Keyword | Kind::Version | Kind::Text | Kind::MatchOnlyText | Kind::SearchAsYouType | Kind::Flattened | Kind::CountedKeyword => {
                if !f.index && !f.doc_values {
                    return Err(not_searchable(&f));
                }
                let terms: Vec<Vec<u8>> = values
                    .iter()
                    .map(|v| {
                        let s = value_text(v);
                        Ok(if f.kind.is_text() {
                            s.into_bytes()
                        } else if f.kind == Kind::Version {
                            super::version_field::encode(&s)?
                        } else {
                            normalize(&f, &s).into_bytes()
                        })
                    })
                    .collect::<EsResult<_>>()?;
                if f.index {
                    // Lucene's `TermInSetQuery` scores constantly by itself (its explanation is
                    // the query, not a `ConstantScore(…)` around it).
                    Ok(arc(MultiTermQuery::set(f.number, terms)))
                } else {
                    Ok(arc(ConstantScoreQuery::new(arc(DocValuesTermsQuery::new(f.number, terms)))))
                }
            }
            // `TermsQueryBuilder.doCoordinatorRewrite` on a constant field: `match_all` when
            // any value is the constant, a `match_none` naming the query otherwise.
            Kind::ConstantKeyword => {
                for v in values.iter() {
                    let q = self.term_query(&f, v, false)?;
                    if q.as_any().is::<MatchAllQuery>() {
                        return Ok(q);
                    }
                }
                Ok(match_none("The \"terms\" query was rewritten to a \"match_none\" query."))
            }
            Kind::Wildcard if !crate::opensearch::active() => Ok(super::extra::wildcard_terms(&f, values.iter().map(|v| wildcard_normalized(&f, &value_text(v))).collect())),
            Kind::Boolean | Kind::Ip | Kind::Wildcard => {
                let mut b = BooleanQuery::new();
                for v in values.iter() {
                    b = b.add(Occur::Should, self.term_query(&f, v, false)?);
                }
                Ok(arc(ConstantScoreQuery::new(arc(b))))
            }
            k if k.is_numeric() || k.is_date() => super::range::numeric_terms(self, &f, &values),
            _ => {
                let mut b = BooleanQuery::new();
                for v in values.iter() {
                    b = b.add(Occur::Should, self.term_query(&f, v, false)?);
                }
                Ok(arc(ConstantScoreQuery::new(arc(b))))
            }
        }
    }

    /// `SearchIndexNameMatcher`: whether an `_index` pattern resolves (leniently, wildcards
    /// expanded over indices, aliases and data streams) to the searched index — so the name of
    /// an alias or data stream that holds the index matches it too.
    pub fn index_matches(&self, pattern: &str) -> bool {
        use crate::xcontent::filter::glob_match;
        // `expand_wildcards=open`: a wildcard skips hidden names, except dot-prefixed ones
        // when the pattern starts with a dot too.
        let wild = pattern.contains('*');
        let hit = |name: &str, hidden: bool| -> bool {
            if !wild {
                return pattern == name;
            }
            (!hidden || (pattern.starts_with('.') && name.starts_with('.'))) && glob_match(pattern, name)
        };
        let name = &self.idx.name;
        if hit(name, self.idx.is_hidden()) {
            return true;
        }
        if self.idx.aliases.iter().any(|(a, m)| hit(a, m.is_hidden == Some(true))) {
            return true;
        }
        let md = &self.st.metadata;
        let Some(ds) = md.data_stream_of(name) else { return false };
        hit(&ds.name, ds.hidden) || md.data_stream_aliases.iter().any(|(a, da)| da.data_streams.iter().any(|d| d == &ds.name) && hit(a, false))
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
            // `getMatchingFieldNames`: multi-fields and search_as_you_type's subfields are field
            // types of their own; a text field's `_index_prefix` / `_index_phrase` aren't.
            for name in self.table.matching(&pattern) {
                if let Some(f) = self.field(&name) {
                    let internal_of_text = f.parent.as_deref().and_then(|p| self.table.get(p)).is_some_and(|p| {
                        matches!(p.kind, Kind::Text | Kind::MatchOnlyText) && (name.ends_with("._index_prefix") || name.ends_with("._index_phrase"))
                    });
                    if !internal_of_text {
                        fields.push(f);
                    }
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
            // `ExistsQueryBuilder.doRewrite`.
            return Ok(match_none("The \"exists\" query was rewritten to a \"match_none\" query."));
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
                // `_field_names` (see `engine::fields::FEATURE_FIELD`).
                Kind::Binary if !f.doc_values => super::ids::field_names_term(&f.name),
                Kind::Completion => super::ids::field_names_term(&f.name),
                // `MappedFieldType.existsQuery`: neither doc values nor norms (`engine::convert`
                // indexes the `_field_names` entry).
                Kind::Text | Kind::MatchOnlyText | Kind::SearchAsYouType | Kind::Keyword if f.index && !f.doc_values && !f.norms => {
                    super::ids::field_names_term(&f.name)
                }
                Kind::AggregateMetric => match super::aggregate_metric::exists_field(f) {
                    Some(n) => arc(FieldExistsQuery { field: n }),
                    None => match_none("aggregate_metric_double without metrics"),
                },
                // Weights, or the marker of an empty vector object (`engine::convert`).
                Kind::SparseVector => arc(
                    BooleanQuery::new()
                        .add(Occur::Should, arc(FieldExistsQuery { field: f.number }))
                        .add(Occur::Should, arc(TermQuery::new(crate::engine::fields::FEATURE_FIELD, f.name.as_bytes().to_vec()))),
                ),
                // A keyed flattened field exists where some term starts with its key.
                Kind::Flattened if f.flattened_key.is_some() => arc(MultiTermQuery::prefix(f.number, normalize(f, "").into_bytes())),
                // `RootFlattenedFieldType.existsQuery`: the doc values of the keyed subfield,
                // which has no skipper, so the query never rewrites to `match_all`.
                Kind::Flattened if f.ty == "flattened" => super::ids::described(arc(FieldExistsQuery { field: f.number }), "FieldExistsQuery", format!("FieldExistsQuery [field={}._keyed]", f.name)),
                _ => arc(FieldExistsQuery { field: f.number }),
            }
        };
        // An object's search_as_you_type fields bring their shingle and prefix subfields.
        let mut extra: Vec<QueryRef> = Vec::new();
        if self.field(field).is_none() {
            for f in fields.iter().filter(|f| f.kind == Kind::SearchAsYouType) {
                if f.prefix_field.is_some() {
                    extra.push(super::ids::field_names_term(&format!("{}._index_prefix", f.name)));
                }
                for &(_, n) in &f.shingle_fields {
                    extra.push(arc(FieldExistsQuery { field: n }));
                }
            }
        }
        if fields.len() == 1 && extra.is_empty() {
            return Ok(arc(ConstantScoreQuery::new(one(&fields[0]))));
        }
        let mut b = BooleanQuery::new();
        for f in &fields {
            b = b.add(Occur::Should, one(f));
        }
        for q in extra {
            b = b.add(Occur::Should, q);
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
        if let Some(rf) = self.runtime_target(field, "prefix")? {
            return self.runtime_prefix(rf, value, case_insensitive);
        }
        let Some(f) = self.field(field) else {
            if field == "_id" {
                self.expensive("prefix")?;
                return Ok(arc(MultiTermQuery::prefix(ID_FIELD, value.as_bytes().to_vec())));
            }
            if field == "_index" {
                let pattern = format!("{}*", if case_insensitive { value.to_lowercase() } else { value.to_string() });
                let ok = self.index_matches(&pattern);
                return Ok(if ok { arc(MatchAllQuery) } else { match_none("prefix doesn't match index") });
            }
            return Ok(match_none("The \"prefix\" query is against a field that does not exist"));
        };
        let rw = self.rewrite_of(rewrite)?;
        let wrap = |q: MultiTermQuery| -> QueryRef { self.multi_term(&f, match rw { Some(r) => q.with_rewrite(r), None => q }) };
        if f.kind == Kind::Version {
            // `VersionStringFieldType.prefixQuery` is `wildcardQuery(value + "*")`.
            self.expensive("wildcard")?;
            return Ok(wrap(super::version_field::prefix(&f, value, case_insensitive)?));
        }
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
            Kind::Wildcard => super::extra::wildcard_field_query(self, &f, &format!("{}*", escape_wildcard(&wildcard_normalized(&f, value))), case_insensitive),
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
        if let Some(rf) = self.runtime_target(field, "wildcard")? {
            return self.runtime_wildcard(rf, value, case_insensitive);
        }
        let Some(f) = self.field(field) else {
            if field == "_id" {
                return Ok(arc(MultiTermQuery::wildcard(ID_FIELD, value, case_insensitive).map_err(index_err)?));
            }
            if field == "_index" {
                let ok = self.index_matches(&if case_insensitive { value.to_lowercase() } else { value.to_string() });
                return Ok(if ok { arc(MatchAllQuery) } else { match_none("wildcard doesn't match index") });
            }
            return Ok(match_none("The \"wildcard\" query is against a field that does not exist"));
        };
        self.expensive("wildcard")?;
        Self::check_keyed(&f, "wildcard")?;
        let rw = self.rewrite_of(rewrite)?;
        if f.kind == Kind::Version {
            let q = super::version_field::wildcard(&f, value, case_insensitive)?;
            return Ok(self.multi_term(&f, match rw {
                Some(r) => q.with_rewrite(r),
                None => q,
            }));
        }
        match f.kind {
            Kind::Keyword | Kind::Version | Kind::Text | Kind::SearchAsYouType | Kind::MatchOnlyText | Kind::Flattened | Kind::CountedKeyword => {
                if !f.index && !f.doc_values {
                    return Err(not_searchable(&f));
                }
                let pattern = if f.kind.is_text() || f.normalizer.is_none() { value.to_string() } else { normalize_wildcard(&f, value) };
                let mut q = MultiTermQuery::wildcard(f.number, &pattern, case_insensitive).map_err(index_err)?;
                if crate::opensearch::flat_object::is_keyed(&f) {
                    q = crate::opensearch::flat_object::keyed_query(&f, q);
                }
                Ok(self.multi_term(&f, match rw {
                    Some(r) => q.with_rewrite(r),
                    None => q,
                }))
            }
            Kind::Wildcard => super::extra::wildcard_field_query(self, &f, &wildcard_normalized(&f, value), case_insensitive),
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
            return Ok(match_none("The \"regexp\" query is against a field that does not exist"));
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
                let mut q = MultiTermQuery::regexp(f.number, &r.value, crate::query::term_level::regexp_flags::to_codec(r.flags), r.case_insensitive, r.max_determinized_states as u32).map_err(index_err)?;
                if crate::opensearch::flat_object::is_keyed(&f) {
                    q = crate::opensearch::flat_object::keyed_query(&f, q);
                }
                if f.kind == Kind::Version {
                    q = super::version_field::regexp(q);
                }
                Ok(self.multi_term(&f, match rw {
                    Some(x) => q.with_rewrite(x),
                    None => q,
                }))
            }
            Kind::Wildcard => super::extra::wildcard_regexp(self, &f, r),
            // `ConstantFieldType.regexpQuery`: every document or none, as the value matches.
            Kind::ConstantKeyword => {
                let Some(c) = f.constant_value.as_deref() else { return Ok(match_none("constant_keyword without value")) };
                let q = MultiTermQuery::regexp(f.number, &r.value, crate::query::term_level::regexp_flags::to_codec(r.flags), r.case_insensitive, r.max_determinized_states as u32).map_err(index_err)?;
                Ok(if q.matches_term(c.as_bytes()) { arc(MatchAllQuery) } else { match_none("constant value doesn't match") })
            }
            _ => Err(qse(self, format!("Can only use regexp queries on keyword and text fields - not on [{}] which is of type [{}]", f.name, f.ty))),
        }
    }

    fn fuzzy_query(&self, fz: &crate::query::term_level::Fuzzy) -> EsResult<QueryRef> {
        let Some(f) = self.field(&fz.field) else { return Ok(match_none("The \"fuzzy\" query was rewritten to a \"match_none\" query.")) };
        self.expensive("fuzzy")?;
        Self::check_keyed(&f, "fuzzy")?;
        let value = value_text(&fz.value);
        if f.kind == Kind::Version {
            let edits = fuzzy_edits(&fz.fuzziness, &value);
            let mut q = super::version_field::fuzzy(&f, &value, edits, fz.prefix_length.max(0) as usize, fz.max_expansions.max(1) as usize, fz.transpositions);
            if let Some(r) = self.rewrite_of(fz.rewrite.as_deref())? {
                q = q.with_rewrite(r);
            }
            return Ok(self.multi_term(&f, q));
        }
        match f.kind {
            Kind::Keyword | Kind::Text | Kind::SearchAsYouType | Kind::MatchOnlyText | Kind::Flattened | Kind::CountedKeyword => {
                let v = if f.kind.is_text() { value.clone() } else { normalize(&f, &value) };
                // A `flat_object` sub-field fuzzes `<path>=<value>` (and so may edit the path,
                // as `key\0value` here); AUTO measures that whole string.
                let edits = if crate::opensearch::flat_object::is_keyed(&f) {
                    fuzzy_edits(&fz.fuzziness, &crate::opensearch::flat_object::fuzzy_subject(&f, &value))
                } else {
                    fuzzy_edits(&fz.fuzziness, &v)
                };
                let mut q = MultiTermQuery::fuzzy(f.number, &v, edits, fz.prefix_length.max(0) as usize, fz.max_expansions.max(1) as usize, fz.transpositions);
                if let Some(r) = self.rewrite_of(fz.rewrite.as_deref())? {
                    q = q.with_rewrite(r);
                }
                if !f.index && !f.doc_values {
                    return Err(not_searchable(&f));
                }
                let q = self.multi_term(&f, q);
                // `match_only_text` keeps no frequencies: its queries score constantly.
                Ok(if f.kind == Kind::MatchOnlyText { arc(ConstantScoreQuery::new(q)) } else { q })
            }
            Kind::Wildcard if !crate::opensearch::active() => {
                let v = wildcard_normalized(&f, &value);
                let edits = fuzzy_edits(&fz.fuzziness, &v);
                Ok(super::extra::wildcard_fuzzy(&f, &v, edits, fz.prefix_length.max(0) as usize, fz.max_expansions.max(1) as usize, fz.transpositions))
            }
            // `MappedFieldType.fuzzyQuery` (numbers and dates included).
            _ => Err(EsError::illegal_argument(format!("Can only use fuzzy queries on keyword and text fields - not on [{}] which is of type [{}]", f.name, f.ty))),
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
        let mark = self.inner_hits.borrow().len();
        let inner = self.to_query(&n.query);
        self.nested.replace(prev);
        let inner = inner?;
        let child_filter: QueryRef = arc(TermQuery::new(NESTED_PATH_FIELD, n.path.as_bytes().to_vec()));
        let filtered = arc(BooleanQuery::new().add(Occur::Must, inner.clone()).add(Occur::Filter, child_filter));
        // The block join filters its child query only when that may match other documents
        // (`NestedHelper.mightMatchNonNestedDocs`), which shows in explanations and profiles.
        // Inner hits always run the filtered form.
        let child = if super::might_match_non_nested_docs(self.table, &inner, &n.path) { filtered.clone() } else { inner.clone() };
        let mode = match n.score_mode {
            crate::query::joining::JoinScoreMode::None => search_index::query::JoinScoreMode::None,
            crate::query::joining::JoinScoreMode::Avg => search_index::query::JoinScoreMode::Avg,
            crate::query::joining::JoinScoreMode::Max => search_index::query::JoinScoreMode::Max,
            crate::query::joining::JoinScoreMode::Min => search_index::query::JoinScoreMode::Min,
            crate::query::joining::JoinScoreMode::Total => search_index::query::JoinScoreMode::Total,
        };
        if let Some(ih) = &n.inner_hits {
            super::inner::check_sort(self, ih.sort.as_ref(), Some(&n.path))?;
            let name = ih.name.clone().unwrap_or_else(|| n.path.clone());
            // Inner hits rewrite `knn` queries to exact ones: every matching child counts.
            let ih_child = if crate::query::contains_query(&n.query, "knn") {
                let prev = self.nested.replace(Some(n.path.clone()));
                let was = self.exact_knn.replace(true);
                let named = self.named.borrow().len();
                let defs = self.inner_hits.borrow().len();
                let inner = self.to_query(&n.query);
                self.named.borrow_mut().truncate(named);
                self.inner_hits.borrow_mut().truncate(defs);
                self.exact_knn.set(was);
                self.nested.replace(prev);
                let filter: QueryRef = arc(TermQuery::new(NESTED_PATH_FIELD, n.path.as_bytes().to_vec()));
                arc(BooleanQuery::new().add(Occur::Must, inner?).add(Occur::Filter, filter))
            } else {
                filtered
            };
            // Nested inner hits defined inside this query belong to this one's hits.
            let children: Vec<InnerHitsDef> = {
                let mut all = self.inner_hits.borrow_mut();
                let (keep, nested): (Vec<_>, Vec<_>) = all.drain(mark..).partition(|d| d.join.is_some());
                all.extend(keep);
                nested
            };
            self.inner_hits.borrow_mut().push(InnerHitsDef { name, spec: ih.clone(), nested_path: Some(n.path.clone()), query: ih_child, join: None, children });
        } else {
            // `NestedQueryBuilder.extractInnerHitBuilders` only looks inside a query that has
            // inner hits of its own: those of deeper queries are dropped.
            self.inner_hits.borrow_mut().truncate(mark);
        }
        Ok(arc(ToParentQuery { child, parent_filter, score_mode: mode }))
    }
}

/// Whether `q` is a `match_none` a query builder's rewrite produced (before the query is
/// built: `doRewrite` / `doIndexMetadataRewrite` returning a `MatchNoneQueryBuilder`), which
/// the enclosing builders' rewrites see.
pub(crate) fn rewritten_to_none(q: &QueryRef) -> bool {
    q.as_any()
        .downcast_ref::<search_index::query::MatchNoneQuery>()
        .is_some_and(|m| m.reason.ends_with("query was rewritten to a \"match_none\" query.") || m.reason.ends_with("query is against a field that does not exist"))
}

/// `TermQueryBuilder.maybeRewriteBasedOnConstantFields`: a `term` query on a constant field
/// rewrites (before it is built) to `match_all` or to a `match_none` naming the query.
pub(crate) fn constant_rewritten(f: &FieldInfo, q: QueryRef) -> QueryRef {
    if f.kind == Kind::ConstantKeyword && q.as_any().is::<search_index::query::MatchNoneQuery>() {
        match_none("The \"term\" query was rewritten to a \"match_none\" query.")
    } else {
        q
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
    super::ids::ids(ids)
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

/// A query value or pattern of a `wildcard` field through its normalizer: OpenSearch's
/// `wildcard` takes one (`WildcardFieldMapper` normalizes term, prefix, wildcard and regexp
/// input whole); Elasticsearch's has none, so this is the identity there.
fn wildcard_normalized(f: &FieldInfo, s: &str) -> String {
    match &f.normalizer {
        Some(n) => n.normalize(s),
        None => s.to_string(),
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

/// A case-insensitive `term` query (`AutomatonQueries.caseInsensitiveTermQuery`): unlike a
/// wildcard pattern, the empty term matches the empty value. Prints as the escaped term.
/// A case-insensitive term printed as Elasticsearch's `CaseInsensitiveTermQuery`.
fn ci_described(f: &FieldInfo, s: &str, q: QueryRef) -> QueryRef {
    super::ids::described(q, "CaseInsensitiveTermQuery", format!("CaseInsensitiveTermQuery{{{}:{s}}}", f.name))
}

fn ci_term(field: u32, s: &str) -> EsResult<MultiTermQuery> {
    let aut = search_codec::automaton::Dfa::term_case_insensitive(s, search_codec::automaton::dfa::DEFAULT_WORK_LIMIT)
        .map_err(|e| EsError::new(400, "too_complex_to_determinize_exception", e.to_string()))?;
    Ok(MultiTermQuery::automaton(field, Arc::new(aut), escape_wildcard(s)))
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
    // No upper bound: more required clauses than there are matches nothing, as in
    // Elasticsearch 9 (`Math.max(result, 0)`).
    Ok(result.max(0))
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
        assert_eq!(calculate_msm(2, "5").unwrap(), 5);
        assert_eq!(calculate_msm(2, "200%").unwrap(), 4);
        assert_eq!(calculate_msm(2, "-5").unwrap(), 0);
        assert!(calculate_msm(2, "x").is_err());
    }

    #[test]
    fn wildcard_escaping() {
        assert_eq!(escape_wildcard("a*b?c\\"), "a\\*b\\?c\\\\");
    }
}
