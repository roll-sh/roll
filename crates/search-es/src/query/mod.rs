//! The Query DSL as Elasticsearch parses and prints it: every query type of Elasticsearch 9.5
//! (core, the bundled modules and the default distribution's x-pack queries) as a typed builder.
//!
//! Parsing follows Elasticsearch's `fromXContent` logic token by token through [`XParser`], so
//! valid queries end up with the same settings and invalid ones fail with Elasticsearch's
//! exception types, texts and `[line:col]` locations (`parsing_exception`,
//! `x_content_parse_exception`, `illegal_argument_exception`, …). Printing ([`Query::to_value`])
//! is `toXContent`: what `_search_shards`, `_validate/query`, profiles and error messages show,
//! including which defaults each builder prints.
//!
//! The builders know nothing about mappings; turning them into engine queries against an index
//! (`toQuery`) happens in the search phase. Expected outputs captured from Elasticsearch 9.5.5
//! are in `tests/fixtures/query/`.

pub mod compound;
pub mod full_text;
pub mod geo;
pub mod joining;
pub mod script;
pub mod span;
pub mod specialized;
pub mod term_level;
pub mod xparser;

use crate::error::{EsError, EsResult};
use crate::json::locate::Seg;
use crate::json::{Map, Value};
pub use xparser::{Token, XParser};

/// `boost` and `_name`, which every query but a few carries.
#[derive(Clone, Debug, PartialEq)]
pub struct Common {
    pub boost: f32,
    pub name: Option<String>,
}

impl Default for Common {
    fn default() -> Self {
        Common { boost: 1.0, name: None }
    }
}

impl Common {
    /// `printBoostAndQueryName`: the boost always, the name when set.
    pub fn write_always(&self, m: &mut Map) {
        m.insert("boost".into(), xparser::float_value(self.boost));
        if let Some(n) = &self.name {
            m.insert("_name".into(), Value::Str(n.clone()));
        }
    }

    /// `boostAndQueryNameToXContent`: each only when it differs from the default. OpenSearch's
    /// builders (from Elasticsearch 7.10) always print the boost.
    pub fn write_if_set(&self, m: &mut Map) {
        if self.boost != 1.0 || OPENSEARCH_PRINT.with(|f| f.get()) {
            m.insert("boost".into(), xparser::float_value(self.boost));
        }
        if let Some(n) = &self.name {
            m.insert("_name".into(), Value::Str(n.clone()));
        }
    }
}

thread_local! {
    /// Set while printing for the opensearch dialect ([`Query::to_value_in`]).
    static OPENSEARCH_PRINT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the query being printed is for the opensearch dialect.
pub(crate) fn opensearch_print() -> bool {
    OPENSEARCH_PRINT.with(|f| f.get())
}

macro_rules! queries {
    ($($variant:ident($ty:ty) = $name:literal),+ $(,)?) => {
        /// A parsed query.
        #[derive(Clone, Debug, PartialEq)]
        pub enum Query {
            $($variant(Box<$ty>),)+
        }

        impl Query {
            /// The query's name (`term`, `bool`, …).
            pub fn name(&self) -> &'static str {
                match self {
                    $(Query::$variant(_) => $name,)+
                }
            }

            /// The body under the query's name, as `doXContent` writes it.
            pub fn inner_value(&self) -> Value {
                match self {
                    $(Query::$variant(q) => q.inner(),)+
                }
            }

            pub fn common(&self) -> &Common {
                match self {
                    $(Query::$variant(q) => &q.common,)+
                }
            }

            pub fn common_mut(&mut self) -> &mut Common {
                match self {
                    $(Query::$variant(q) => &mut q.common,)+
                }
            }
        }

        $(impl From<$ty> for Query {
            fn from(q: $ty) -> Query {
                Query::$variant(Box::new(q))
            }
        })+
    };
}

queries! {
    MatchAll(compound::MatchAll) = "match_all",
    MatchNone(compound::MatchNone) = "match_none",
    Bool(compound::Bool) = "bool",
    Boosting(compound::Boosting) = "boosting",
    ConstantScore(compound::ConstantScore) = "constant_score",
    DisMax(compound::DisMax) = "dis_max",
    FunctionScore(compound::FunctionScore) = "function_score",
    ScriptScore(compound::ScriptScore) = "script_score",
    Term(term_level::Term) = "term",
    Terms(term_level::Terms) = "terms",
    TermsSet(term_level::TermsSet) = "terms_set",
    Range(term_level::Range) = "range",
    Exists(term_level::Exists) = "exists",
    Prefix(term_level::Prefix) = "prefix",
    Wildcard(term_level::Wildcard) = "wildcard",
    Regexp(term_level::Regexp) = "regexp",
    Fuzzy(term_level::Fuzzy) = "fuzzy",
    Ids(term_level::Ids) = "ids",
    Match(full_text::Match) = "match",
    MatchPhrase(full_text::MatchPhrase) = "match_phrase",
    MatchPhrasePrefix(full_text::MatchPhrasePrefix) = "match_phrase_prefix",
    MatchBoolPrefix(full_text::MatchBoolPrefix) = "match_bool_prefix",
    MultiMatch(full_text::MultiMatch) = "multi_match",
    CombinedFields(full_text::CombinedFields) = "combined_fields",
    QueryString(full_text::QueryString) = "query_string",
    SimpleQueryString(full_text::SimpleQueryString) = "simple_query_string",
    Intervals(full_text::Intervals) = "intervals",
    Nested(joining::Nested) = "nested",
    HasChild(joining::HasChild) = "has_child",
    HasParent(joining::HasParent) = "has_parent",
    ParentId(joining::ParentId) = "parent_id",
    GeoBoundingBox(geo::GeoBoundingBox) = "geo_bounding_box",
    GeoDistance(geo::GeoDistance) = "geo_distance",
    GeoPolygon(geo::GeoPolygon) = "geo_polygon",
    GeoShape(geo::GeoShape) = "geo_shape",
    Shape(geo::Shape) = "shape",
    GeoGrid(geo::GeoGrid) = "geo_grid",
    SpanTerm(span::SpanTerm) = "span_term",
    SpanMulti(span::SpanMulti) = "span_multi",
    SpanFirst(span::SpanFirst) = "span_first",
    SpanNear(span::SpanNear) = "span_near",
    SpanOr(span::SpanOr) = "span_or",
    SpanNot(span::SpanNot) = "span_not",
    SpanContaining(span::SpanContaining) = "span_containing",
    SpanWithin(span::SpanWithin) = "span_within",
    SpanFieldMasking(span::SpanFieldMasking) = "span_field_masking",
    SpanGap(span::SpanGap) = "span_gap",
    MoreLikeThis(specialized::MoreLikeThis) = "more_like_this",
    Script(specialized::ScriptQuery) = "script",
    Wrapper(specialized::Wrapper) = "wrapper",
    DistanceFeature(specialized::DistanceFeature) = "distance_feature",
    RankFeature(specialized::RankFeature) = "rank_feature",
    Percolate(specialized::Percolate) = "percolate",
    Pinned(specialized::Pinned) = "pinned",
    Knn(specialized::Knn) = "knn",
    SparseVector(specialized::SparseVector) = "sparse_vector",
    TextExpansion(specialized::TextExpansion) = "text_expansion",
    WeightedTokens(specialized::WeightedTokens) = "weighted_tokens",
    Semantic(specialized::Semantic) = "semantic",
    Rule(specialized::Rule) = "rule",
}

/// Every query name Elasticsearch 9.5.5 registers (deprecated names included), the candidates
/// of its "did you mean" suggestions.
pub const QUERY_NAMES: &[&str] = &[
    "bool", "boosting", "combined_fields", "constant_score", "dis_max", "distance_feature", "exact_knn", "exists",
    "field_masking_span", "function_score", "fuzzy", "geo_bounding_box", "geo_distance", "geo_grid", "geo_polygon",
    "geo_shape", "has_child", "has_parent", "ids", "intervals", "knn", "knn_score_doc", "match", "match_all",
    "match_bool_prefix", "match_none", "match_phrase", "match_phrase_prefix", "more_like_this", "multi_match", "nested",
    "parent_id", "percolate", "pinned", "prefix", "query_string", "range", "rank_feature", "regexp", "rule", "rule_query", "script",
    "script_score", "semantic", "shape", "simple_query_string", "span_containing", "span_field_masking", "span_first", "span_gap",
    "span_multi", "span_near", "span_not", "span_or", "span_term", "span_within", "sparse_vector", "term", "terms",
    "terms_set", "text_expansion", "weighted_tokens", "wildcard", "wrapper",
];

impl Query {
    /// `toXContent`: `{"<name>": {...}}`.
    pub fn to_value(&self) -> Value {
        let mut m = Map::with_capacity(1);
        m.insert(self.name().to_string(), self.inner_value());
        Value::Obj(m)
    }

    /// `toXContent` in a dialect's form: OpenSearch prints every query's boost.
    pub fn to_value_in(&self, dialect: crate::cluster::Dialect) -> Value {
        if dialect != crate::cluster::Dialect::OpenSearch {
            return self.to_value();
        }
        OPENSEARCH_PRINT.with(|f| f.set(true));
        let v = self.to_value();
        OPENSEARCH_PRINT.with(|f| f.set(false));
        v
    }

    /// `toString()`: the pretty-printed JSON Elasticsearch puts in messages.
    pub fn to_pretty_string(&self) -> String {
        self.to_value().to_pretty_json()
    }

    /// The query's `_name`.
    pub fn query_name(&self) -> Option<&str> {
        self.common().name.as_deref()
    }

    /// The query's `boost`.
    pub fn boost(&self) -> f32 {
        self.common().boost
    }
}

/// Parses a query from a request body value (`AbstractQueryBuilder.parseTopLevelQuery`).
/// `src` is the raw body and `base` the path of `value` inside it, for error locations.
pub fn parse_query(value: &Value, src: Option<&[u8]>, base: &[Seg<'_>]) -> Result<(Query, Vec<String>), EsError> {
    let base: Vec<Seg<'_>> = base.to_vec();
    let mut p = XParser::new(value, src, base);
    let q = parse_inner(&mut p)?;
    Ok((q, p.warnings))
}

/// Parses a stored query (an alias filter): locations refer to its compact JSON form.
pub fn parse_stored(value: &Value) -> EsResult<Query> {
    let mut p = XParser::from_value(value);
    p.string_source = true;
    parse_inner(&mut p)
}

/// `AbstractQueryBuilder.parseInnerQueryBuilder`: `{"<name>": {...}}`.
pub fn parse_inner(p: &mut XParser<'_>) -> EsResult<Query> {
    crate::stack::maybe_grow(|| parse_inner_level(p))
}

fn parse_inner_level(p: &mut XParser<'_>) -> EsResult<Query> {
    if p.current_token() != Some(Token::StartObject) && p.next_token() != Some(Token::StartObject) {
        return Err(p.parsing_error("[_na] query malformed, must start with start_object"));
    }
    if p.next_token() == Some(Token::EndObject) {
        let (l, c) = p.token_location();
        return Err(EsError::illegal_argument(format!("query malformed, empty clause found at [{l}:{c}]")));
    }
    if p.current_token() != Some(Token::FieldName) {
        return Err(p.parsing_error("[_na] query malformed, no field after start_object"));
    }
    let name = p.current_name().unwrap_or("");
    if p.next_token() != Some(Token::StartObject) {
        return Err(p.parsing_error(format!("[{name}] query malformed, no start_object after query name")));
    }
    p.depth += 1;
    if p.depth > p.max_depth {
        return Err(EsError::illegal_argument(
            "The nested depth of the query exceeds the maximum nested depth for queries set in [indices.query.bool.max_nested_depth]",
        ));
    }
    let q = match parse_named(name, p) {
        Ok(q) => q,
        // An unknown name in a nested registry (score functions) surfaces as an unknown query,
        // named after the enclosing query, with that registry's suggestions.
        Err(e) if e.kind == "named_object_not_found_exception" => {
            let (l, c) = bracket_location(e.reason()).unwrap_or((1, 1));
            let suggestion = crate::rest::params::suggest(name, compound::SCORE_FUNCTION_NAMES);
            return Err(EsError::parsing(l, c, format!("unknown query [{name}]{suggestion}")).caused_by(e));
        }
        Err(e) => return Err(e),
    };
    p.depth -= 1;
    if p.current_token() != Some(Token::EndObject) {
        return Err(p.parsing_error(format!(
            "[{name}] malformed query, expected [END_OBJECT] but found [{}]",
            p.current_token().map(Token::name).unwrap_or("null")
        )));
    }
    let next = p.next_token();
    if next != Some(Token::EndObject) {
        return Err(p.parsing_error(format!(
            "[{name}] malformed query, expected [END_OBJECT] but found [{}]",
            next.map(Token::name).unwrap_or("null")
        )));
    }
    Ok(q)
}

/// The `[line:col]` prefix of an x-content exception's message.
fn bracket_location(reason: &str) -> Option<(usize, usize)> {
    let inner = reason.strip_prefix('[')?.split_once(']')?.0;
    let (l, c) = inner.split_once(':')?;
    Some((l.parse().ok()?, c.parse().ok()?))
}

/// Looks up and runs the named query's parser, positioned on the query body's start.
fn parse_named(name: &str, p: &mut XParser<'_>) -> EsResult<Query> {
    Ok(match name {
        "match_all" => compound::MatchAll::parse(p)?.into(),
        "match_none" => compound::MatchNone::parse(p)?.into(),
        "bool" => compound::Bool::parse(p)?.into(),
        "boosting" => compound::Boosting::parse(p)?.into(),
        "constant_score" => compound::ConstantScore::parse(p)?.into(),
        "dis_max" => compound::DisMax::parse(p)?.into(),
        "function_score" => compound::FunctionScore::parse(p)?.into(),
        "script_score" => compound::ScriptScore::parse(p)?.into(),
        "term" => term_level::Term::parse(p)?.into(),
        "terms" => term_level::Terms::parse(p)?.into(),
        "terms_set" => term_level::TermsSet::parse(p)?.into(),
        "range" => term_level::Range::parse(p)?.into(),
        "exists" => term_level::Exists::parse(p)?.into(),
        "prefix" => term_level::Prefix::parse(p)?.into(),
        "wildcard" => term_level::Wildcard::parse(p)?.into(),
        "regexp" => term_level::Regexp::parse(p)?.into(),
        "fuzzy" => term_level::Fuzzy::parse(p)?.into(),
        "ids" => term_level::Ids::parse(p)?.into(),
        "match" => full_text::Match::parse(p)?.into(),
        "match_phrase" => full_text::MatchPhrase::parse(p)?.into(),
        "match_phrase_prefix" => full_text::MatchPhrasePrefix::parse(p)?.into(),
        "match_bool_prefix" => full_text::MatchBoolPrefix::parse(p)?.into(),
        "multi_match" => full_text::MultiMatch::parse(p)?.into(),
        "combined_fields" => full_text::CombinedFields::parse(p)?.into(),
        "query_string" => full_text::QueryString::parse(p)?.into(),
        "simple_query_string" => full_text::SimpleQueryString::parse(p)?.into(),
        "intervals" => full_text::Intervals::parse(p)?.into(),
        "nested" => joining::Nested::parse(p)?.into(),
        "has_child" => joining::HasChild::parse(p)?.into(),
        "has_parent" => joining::HasParent::parse(p)?.into(),
        "parent_id" => joining::ParentId::parse(p)?.into(),
        "geo_bounding_box" => geo::GeoBoundingBox::parse(p)?.into(),
        "geo_distance" => geo::GeoDistance::parse(p)?.into(),
        "geo_polygon" => geo::GeoPolygon::parse(p)?.into(),
        "geo_shape" => geo::GeoShape::parse(p)?.into(),
        "shape" => geo::Shape::parse(p)?.into(),
        "geo_grid" => geo::GeoGrid::parse(p)?.into(),
        "span_term" => span::SpanTerm::parse(p)?.into(),
        "span_multi" => span::SpanMulti::parse(p)?.into(),
        "span_first" => span::SpanFirst::parse(p)?.into(),
        "span_near" => span::SpanNear::parse(p)?.into(),
        "span_or" => span::SpanOr::parse(p)?.into(),
        "span_not" => span::SpanNot::parse(p)?.into(),
        "span_containing" => span::SpanContaining::parse(p)?.into(),
        "span_within" => span::SpanWithin::parse(p)?.into(),
        "span_field_masking" => span::SpanFieldMasking::parse(p, false)?.into(),
        "span_gap" => span::SpanGap::parse(p)?.into(),
        "field_masking_span" => span::SpanFieldMasking::parse(p, true)?.into(),
        "more_like_this" => specialized::MoreLikeThis::parse(p)?.into(),
        "script" => specialized::ScriptQuery::parse(p)?.into(),
        "wrapper" => specialized::Wrapper::parse(p)?.into(),
        "distance_feature" => specialized::DistanceFeature::parse(p)?.into(),
        "rank_feature" => specialized::RankFeature::parse(p)?.into(),
        "percolate" => specialized::Percolate::parse(p)?.into(),
        "pinned" => specialized::Pinned::parse(p)?.into(),
        "knn" => specialized::Knn::parse(p)?.into(),
        "sparse_vector" => specialized::SparseVector::parse(p)?.into(),
        "text_expansion" => specialized::TextExpansion::parse(p)?.into(),
        "weighted_tokens" => specialized::WeightedTokens::parse(p)?.into(),
        "semantic" => specialized::Semantic::parse(p)?.into(),
        "rule" => specialized::Rule::parse(p)?.into(),
        "rule_query" => {
            p.deprecated_name("rule_query", "rule", None);
            specialized::Rule::parse(p)?.into()
        }
        "exact_knn" => return Err(EsError::illegal_argument("[exact_knn] queries cannot be provided directly")),
        "knn_score_doc" => return Err(EsError::illegal_argument("[score_doc] queries cannot be provided directly")),
        _ => {
            let (l, c) = p.token_location();
            let suggestion = crate::rest::params::suggest(name, QUERY_NAMES);
            return Err(EsError::parsing(l, c, format!("unknown query [{name}]{suggestion}")).caused_by(EsError::new(
                400,
                "named_object_not_found_exception",
                format!("[{l}:{c}] unknown field [{name}]"),
            )));
        }
    })
}

/// `QueryBuilder.boost(float)`: negative boosts fail, naming the query as built so far.
pub fn check_boost(boost: f32, so_far: impl FnOnce() -> Query) -> EsResult<f32> {
    // `Float.compare(boost, 0f) < 0`: -0.0 is negative too, NaN is not.
    if boost < 0.0 || (boost == 0.0 && boost.is_sign_negative()) {
        return Err(EsError::illegal_argument(format!(
            "negative [boost] are not allowed in [{}], use a value between 0 and 1 to deboost",
            so_far().to_pretty_string()
        )));
    }
    Ok(boost)
}

/// `throwParsingExceptionOnMultipleFields`.
pub fn check_single_field(p: &XParser<'_>, query: &str, existing: &Option<String>, current: Option<&str>) -> EsResult<()> {
    if let Some(e) = existing {
        return Err(p.parsing_error(format!(
            "[{query}] query doesn't support multiple fields, found [{e}] and [{}]",
            current.unwrap_or("null")
        )));
    }
    Ok(())
}

/// Parses a query nested inside another (`parseInnerQueryBuilder` from a field's value).
pub fn parse_nested(p: &mut XParser<'_>) -> EsResult<Query> {
    parse_inner(p)
}

/// `maybeConvertToString` for printed values: strings and numbers as they were given.
pub(crate) fn opt_str(m: &mut Map, key: &str, v: &Option<String>) {
    if let Some(s) = v {
        m.insert(key.into(), Value::Str(s.clone()));
    }
}

#[cfg(test)]
mod tests;
