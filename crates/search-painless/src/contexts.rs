//! Script contexts: what a script of each kind receives and returns.
//!
//! Mirrors Elasticsearch's `ScriptContext`s (`GET _script_context` lists the same names,
//! methods and parameters; the capture is committed as `tests/fixtures/script_context.json`).
//! A context fixes the `execute` return type, the variables a script can read (`params`,
//! `doc`, `ctx`, `_score`, `state`, …) and the extra functions it can call (`emit` in runtime
//! fields, the decay and vector functions in `score`, `field`/`$` in document-based contexts,
//! `metadata()` in update and ingest).

use crate::types::{cls, ClassId, Ty};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Context {
    PainlessTest,
    Score,
    Filter,
    Field,
    Update,
    UpdateByQuery,
    Reindex,
    Ingest,
    ProcessorConditional,
    Aggs,
    AggsInit,
    AggsMap,
    AggsCombine,
    AggsReduce,
    AggregationSelector,
    BucketAggregation,
    NumberSort,
    StringSort,
    BytesRefSort,
    TermsSet,
    Similarity,
    SimilarityWeight,
    BooleanField,
    DateField,
    DoubleField,
    GeoPointField,
    GeometryField,
    IpField,
    KeywordField,
    LongField,
    CompositeField,
    Template,
    IngestTemplate,
    XpackTemplate,
    Analysis,
    Interval,
    MovingFunction,
    ScriptHeuristic,
    DoubleValues,
    WatcherCondition,
    WatcherTransform,
}

/// Which groups of context functions a context can call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// `field(String)` and `$(String, def)` over doc values.
    DocBased,
    /// Decay, saturation, sigmoid, random score and vector functions.
    Score,
    /// `emit(...)` with the given runtime field kind.
    Emit(EmitKind),
    /// `metadata()` and `field(String)` over the document being written.
    Write,
    /// `field(String)` (`SourceMapField`) over the document an ingest condition reads.
    Conditional,
}

/// Whether a class of an extension allowlist is visible in a context: Elasticsearch adds
/// `MovingFunctions` to `moving-function` (and `painless_test`) scripts, `Processors` to ingest
/// scripts, grok/dissect extractors to runtime fields and `SourceMapField` to ingest conditions;
/// elsewhere the names do not resolve.
pub fn class_visible(ctx: Context, c: ClassId) -> bool {
    use Context::*;
    match c {
        cls::MOVING_FUNCTIONS => matches!(ctx, MovingFunction | PainlessTest),
        cls::INGEST_PROCESSORS => ctx == Ingest,
        cls::NAMED_GROUP_EXTRACTOR => matches!(ctx, BooleanField | DateField | DoubleField | GeoPointField | GeometryField | IpField | KeywordField | LongField | CompositeField),
        cls::SOURCE_MAP_FIELD | cls::INGEST_CONDITIONAL_SCRIPT => ctx == ProcessorConditional,
        _ => true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmitKind {
    Boolean,
    Date,
    Double,
    GeoPoint,
    Geometry,
    Ip,
    Keyword,
    Long,
    Composite,
}

/// A variable a context provides: an `execute` argument or a getter (`getParams` → `params`).
#[derive(Clone, Copy, Debug)]
pub struct Var {
    pub name: &'static str,
    pub ty: Ty,
}

pub struct ContextDef {
    pub context: Context,
    pub name: &'static str,
    pub ret: Ty,
    /// `execute` arguments, then getters, in the order Elasticsearch defines them.
    pub vars: Vec<Var>,
    pub families: Vec<Family>,
}

const fn v(name: &'static str, ty: Ty) -> Var {
    Var { name, ty }
}

impl Context {
    pub const ALL: &'static [Context] = &[
        Context::PainlessTest,
        Context::Score,
        Context::Filter,
        Context::Field,
        Context::Update,
        Context::UpdateByQuery,
        Context::Reindex,
        Context::Ingest,
        Context::ProcessorConditional,
        Context::Aggs,
        Context::AggsInit,
        Context::AggsMap,
        Context::AggsCombine,
        Context::AggsReduce,
        Context::AggregationSelector,
        Context::BucketAggregation,
        Context::NumberSort,
        Context::StringSort,
        Context::BytesRefSort,
        Context::TermsSet,
        Context::Similarity,
        Context::SimilarityWeight,
        Context::BooleanField,
        Context::DateField,
        Context::DoubleField,
        Context::GeoPointField,
        Context::GeometryField,
        Context::IpField,
        Context::KeywordField,
        Context::LongField,
        Context::CompositeField,
        Context::Template,
        Context::IngestTemplate,
        Context::XpackTemplate,
        Context::Analysis,
        Context::Interval,
        Context::MovingFunction,
        Context::ScriptHeuristic,
        Context::DoubleValues,
        Context::WatcherCondition,
        Context::WatcherTransform,
    ];

    /// The context's name in Elasticsearch (`score`, `painless_test`, `long_field`, …).
    pub fn name(self) -> &'static str {
        use Context::*;
        match self {
            PainlessTest => "painless_test",
            Score => "score",
            Filter => "filter",
            Field => "field",
            Update => "update",
            UpdateByQuery => "update_by_query",
            Reindex => "reindex",
            Ingest => "ingest",
            ProcessorConditional => "processor_conditional",
            Aggs => "aggs",
            AggsInit => "aggs_init",
            AggsMap => "aggs_map",
            AggsCombine => "aggs_combine",
            AggsReduce => "aggs_reduce",
            AggregationSelector => "aggregation_selector",
            BucketAggregation => "bucket_aggregation",
            NumberSort => "number_sort",
            StringSort => "string_sort",
            BytesRefSort => "bytesref_sort",
            TermsSet => "terms_set",
            Similarity => "similarity",
            SimilarityWeight => "similarity_weight",
            BooleanField => "boolean_field",
            DateField => "date_field",
            DoubleField => "double_field",
            GeoPointField => "geo_point_field",
            GeometryField => "geometry_field",
            IpField => "ip_field",
            KeywordField => "keyword_field",
            LongField => "long_field",
            CompositeField => "composite_field",
            Template => "template",
            IngestTemplate => "ingest_template",
            XpackTemplate => "xpack_template",
            Analysis => "analysis",
            Interval => "interval",
            MovingFunction => "moving-function",
            ScriptHeuristic => "script_heuristic",
            DoubleValues => "double_values",
            WatcherCondition => "watcher_condition",
            WatcherTransform => "watcher_transform",
        }
    }

    /// Whether compilations in this context count against `script.max_compilations_rate`
    /// (ingest pipelines and their templates are exempt, as in Elasticsearch).
    pub fn compilation_rate_limited(self) -> bool {
        !matches!(self, Context::Ingest | Context::IngestTemplate | Context::ProcessorConditional)
    }

    /// Context variables that are parameters of the generated `execute` method (the rest are
    /// getters); they decide JVM slot numbers in helpful NPE messages.
    pub fn execute_params(self) -> &'static [&'static str] {
        match self {
            Context::Score => &["explanation"],
            _ => &[],
        }
    }

    /// JVM slots doc-based script classes reserve after the loop counter (observed: the first
    /// local of a filter, score or runtime-field script is two slots further than in
    /// `painless_test` or ingest scripts).
    pub fn reserved_slots(self) -> u16 {
        if self.def().has(Family::DocBased) { 2 } else { 0 }
    }

    pub fn from_name(name: &str) -> Option<Context> {
        Context::ALL.iter().copied().find(|c| c.name() == name)
    }

    pub fn def(self) -> ContextDef {
        use Context::*;
        let map = Ty::of(cls::MAP);
        let list = Ty::of(cls::LIST);
        // Elasticsearch analyzes `Object` return and argument types as `def`
        let obj = Ty::DEF;
        let num = Ty::of(cls::NUMBER);
        let (ret, vars, families): (Ty, Vec<Var>, Vec<Family>) = match self {
            PainlessTest => (obj, vec![v("params", map)], vec![]),
            Score => (
                Ty::DOUBLE,
                vec![
                    v("explanation", Ty::of(cls::EXPLANATION_HOLDER)),
                    v("doc", map),
                    v("params", map),
                    v("_score", Ty::DOUBLE),
                    v("_termStats", Ty::of(cls::SCRIPT_TERM_STATS)),
                ],
                vec![Family::DocBased, Family::Score],
            ),
            Filter => (Ty::BOOLEAN, vec![v("doc", map), v("params", map)], vec![Family::DocBased]),
            Field => (obj, vec![v("doc", map), v("params", map)], vec![Family::DocBased]),
            Update | UpdateByQuery | Reindex | Ingest => (Ty::VOID, vec![v("ctx", map), v("params", map)], vec![Family::Write]),
            ProcessorConditional => (Ty::BOOLEAN, vec![v("ctx", map), v("params", map)], vec![Family::Conditional]),
            WatcherCondition => (Ty::BOOLEAN, vec![v("ctx", map), v("params", map)], vec![]),
            WatcherTransform => (obj, vec![v("ctx", map), v("params", map)], vec![]),
            Aggs => (obj, vec![v("doc", map), v("params", map), v("_score", num), v("_value", obj)], vec![Family::DocBased]),
            AggsInit => (Ty::VOID, vec![v("params", map), v("state", obj)], vec![]),
            AggsMap => (Ty::VOID, vec![v("doc", map), v("params", map), v("state", map), v("_score", Ty::DOUBLE)], vec![Family::DocBased]),
            AggsCombine => (obj, vec![v("params", map), v("state", map)], vec![]),
            AggsReduce => (obj, vec![v("params", map), v("states", list)], vec![]),
            AggregationSelector => (Ty::BOOLEAN, vec![v("params", map)], vec![]),
            BucketAggregation => (num, vec![v("params", map)], vec![]),
            NumberSort => (Ty::DOUBLE, vec![v("doc", map), v("params", map), v("_score", Ty::DOUBLE)], vec![Family::DocBased]),
            StringSort => (Ty::STRING, vec![v("doc", map), v("params", map), v("_score", Ty::DOUBLE)], vec![Family::DocBased]),
            BytesRefSort => (obj, vec![v("doc", map), v("params", map), v("_score", Ty::DOUBLE)], vec![Family::DocBased]),
            TermsSet => (num, vec![v("doc", map), v("params", map)], vec![Family::DocBased]),
            Similarity => (
                Ty::DOUBLE,
                vec![
                    v("weight", Ty::DOUBLE),
                    v("query", Ty::of(cls::SIMILARITY_QUERY)),
                    v("field", Ty::of(cls::SIMILARITY_FIELD)),
                    v("term", Ty::of(cls::SIMILARITY_TERM)),
                    v("doc", Ty::of(cls::SIMILARITY_DOC)),
                ],
                vec![],
            ),
            SimilarityWeight => (
                Ty::DOUBLE,
                vec![v("query", Ty::of(cls::SIMILARITY_QUERY)), v("field", Ty::of(cls::SIMILARITY_FIELD)), v("term", Ty::of(cls::SIMILARITY_TERM))],
                vec![],
            ),
            BooleanField => (Ty::VOID, vec![v("doc", map), v("params", map)], vec![Family::DocBased, Family::Emit(EmitKind::Boolean)]),
            DateField => (Ty::VOID, vec![v("doc", map), v("params", map)], vec![Family::DocBased, Family::Emit(EmitKind::Date)]),
            DoubleField => (Ty::VOID, vec![v("doc", map), v("params", map)], vec![Family::DocBased, Family::Emit(EmitKind::Double)]),
            GeoPointField => (Ty::VOID, vec![v("doc", map), v("params", map)], vec![Family::DocBased, Family::Emit(EmitKind::GeoPoint)]),
            GeometryField => (Ty::VOID, vec![v("doc", map), v("params", map)], vec![Family::DocBased, Family::Emit(EmitKind::Geometry)]),
            IpField => (Ty::VOID, vec![v("doc", map), v("params", map)], vec![Family::DocBased, Family::Emit(EmitKind::Ip)]),
            KeywordField => (
                Ty::VOID,
                vec![v("doc", map), v("params", map), v("values", list)],
                vec![Family::DocBased, Family::Emit(EmitKind::Keyword)],
            ),
            LongField => (Ty::VOID, vec![v("doc", map), v("params", map)], vec![Family::DocBased, Family::Emit(EmitKind::Long)]),
            CompositeField => (
                Ty::VOID,
                vec![v("doc", map), v("fieldValues", map), v("params", map)],
                vec![Family::DocBased, Family::Emit(EmitKind::Composite)],
            ),
            Template | IngestTemplate | XpackTemplate => (Ty::STRING, vec![v("params", map)], vec![]),
            Analysis => (Ty::BOOLEAN, vec![v("token", Ty::of(cls::ANALYSIS_TOKEN))], vec![]),
            Interval => (Ty::BOOLEAN, vec![v("interval", Ty::of(cls::INTERVAL))], vec![]),
            MovingFunction => (Ty::DOUBLE, vec![v("params", map), v("values", Ty::array(cls::DOUBLE, 1))], vec![]),
            ScriptHeuristic => (Ty::DOUBLE, vec![v("params", map)], vec![]),
            DoubleValues => (Ty::DOUBLE, vec![], vec![]),
        };
        ContextDef { context: self, name: self.name(), ret, vars, families }
    }
}

impl ContextDef {
    pub fn has(&self, f: Family) -> bool {
        self.families.contains(&f)
    }
    pub fn emit_kind(&self) -> Option<EmitKind> {
        self.families.iter().find_map(|f| match f {
            Family::Emit(k) => Some(*k),
            _ => None,
        })
    }
    pub fn var_index(&self, name: &str) -> Option<usize> {
        self.vars.iter().position(|v| v.name == name)
    }
}

/// `GET _script_context`: every script context with the methods of its script class, as
/// Elasticsearch 9.5.5 answers (`execute` and the getters with their Java types).
pub fn script_context_json() -> serde_json::Value {
    serde_json::from_str(include_str!("script_context.json")).expect("bundled script contexts")
}

/// `GET _script_language`: the languages this engine runs and their contexts, as Elasticsearch
/// lists them (contexts sorted by name; Painless also claims the template contexts).
pub fn script_language_json() -> serde_json::Value {
    let mut painless: Vec<&str> = Context::ALL.iter().map(|c| c.name()).collect();
    painless.sort_unstable();
    serde_json::json!({
        "types_allowed": ["inline", "stored"],
        "language_contexts": [
            {"language": "mustache", "contexts": ["ingest_template", "template"]},
            {"language": "painless", "contexts": painless},
        ]
    })
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn script_contexts_cover_every_context() {
        let j = script_context_json();
        let names: Vec<&str> = j["contexts"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        let mut ours: Vec<&str> = Context::ALL.iter().map(|c| c.name()).collect();
        ours.sort_unstable();
        assert_eq!(names, ours);
        let langs = script_language_json();
        assert_eq!(langs["language_contexts"][1]["contexts"].as_array().unwrap().len(), Context::ALL.len());
    }
}
