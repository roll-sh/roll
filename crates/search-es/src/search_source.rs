//! The search request body (`SearchSourceBuilder.parseXContent`): `query`, paging, fetch options,
//! sorts and the other sections, parsed token by token with Elasticsearch's exceptions.
//!
//! Sections other crates own (aggregations: `search-aggs`) or that need the search phase's
//! builders (highlight, suggest, retrievers, rank, ext, `knn` sections) are kept as their JSON
//! values here; everything else is typed. Request validation that spans sections (`slice`
//! without a scroll or point in time, `search_after` without sort) belongs to the search action.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::query::script::Script;
use crate::query::xparser::{Field, Tokens};
use crate::query::{Query, Token, XParser, parse_inner};
use crate::units::TimeValue;

/// `_source` filtering (`FetchSourceContext`).
#[derive(Clone, Debug, PartialEq)]
pub struct FetchSource {
    pub fetch: bool,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
    pub exclude_vectors: Option<bool>,
}

/// `stored_fields` (`StoredFieldsContext`): `None` for `_none_`.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredFields {
    pub fields: Option<Vec<String>>,
}

/// An entry of `docvalue_fields` or `fields` (`FieldAndFormat`).
#[derive(Clone, Debug, PartialEq)]
pub struct FieldAndFormat {
    pub field: String,
    pub format: Option<String>,
    pub include_unmapped: Option<bool>,
}

/// A `script_fields` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptField {
    pub name: String,
    pub script: Script,
    pub ignore_failure: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortOrder {
    Asc,
    Desc,
}

impl SortOrder {
    /// `SortOrder.fromString`.
    pub fn parse(s: &str) -> EsResult<SortOrder> {
        match s.to_uppercase().as_str() {
            "ASC" => Ok(SortOrder::Asc),
            "DESC" => Ok(SortOrder::Desc),
            u => Err(EsError::illegal_argument(format!("No enum constant org.elasticsearch.search.sort.SortOrder.{u}"))),
        }
    }
}

/// A field sort's nested context.
#[derive(Clone, Debug, PartialEq)]
pub struct NestedSort {
    pub path: String,
    pub filter: Option<Query>,
    pub max_children: Option<i32>,
    pub nested: Option<Box<NestedSort>>,
}

/// One sort.
#[derive(Clone, Debug, PartialEq)]
pub enum Sort {
    Score { order: SortOrder },
    /// `_doc` and field sorts.
    Field {
        field: String,
        order: Option<SortOrder>,
        missing: Option<Value>,
        unmapped_type: Option<String>,
        mode: Option<String>,
        numeric_type: Option<String>,
        format: Option<String>,
        nested: Option<NestedSort>,
    },
    Script { script: Script, ty: String, order: Option<SortOrder>, mode: Option<String>, nested: Option<NestedSort> },
    /// `_geo_distance`, as given (parsed by the search phase).
    GeoDistance(Map),
}

/// `track_total_hits`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackTotalHits {
    Disabled,
    Accurate,
    UpTo(i32),
}

/// A rescorer: `query` (`QueryRescorer`) or, when `script` is set, `script`
/// (`ScriptRescorer`: the script's value replaces the score; `query` is then unused).
#[derive(Clone, Debug, PartialEq)]
pub struct Rescore {
    pub window_size: Option<i32>,
    pub query: Query,
    pub query_weight: f32,
    pub rescore_query_weight: f32,
    pub score_mode: String,
    pub script: Option<Script>,
}

/// `collapse`.
#[derive(Clone, Debug, PartialEq)]
pub struct Collapse {
    pub field: String,
    pub inner_hits: Vec<Value>,
    pub max_concurrent_group_searches: Option<i32>,
}

/// `slice`.
#[derive(Clone, Debug, PartialEq)]
pub struct Slice {
    pub field: Option<String>,
    pub id: i32,
    pub max: i32,
}

/// `pit`.
#[derive(Clone, Debug, PartialEq)]
pub struct PointInTime {
    pub id: String,
    pub keep_alive: Option<TimeValue>,
}

/// The raw body an `aggs` section came from, and its key: aggregations parse lazily, and
/// their syntax errors locate themselves in it.
#[derive(Debug, PartialEq)]
pub struct AggsRaw {
    pub body: Vec<u8>,
    pub key: String,
}

/// A parsed search request body.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SearchSource {
    pub query: Option<Query>,
    pub post_filter: Option<Query>,
    pub from: Option<i32>,
    pub size: Option<i32>,
    pub timeout: Option<TimeValue>,
    pub terminate_after: Option<i32>,
    pub min_score: Option<f32>,
    pub version: Option<bool>,
    pub seq_no_primary_term: Option<bool>,
    pub explain: Option<bool>,
    pub track_scores: bool,
    pub track_total_hits: Option<TrackTotalHits>,
    pub profile: bool,
    /// OpenSearch's `verbose_pipeline` (search pipelines report each processor's run).
    pub verbose_pipeline: bool,
    pub source: Option<FetchSource>,
    pub stored_fields: Option<StoredFields>,
    pub docvalue_fields: Vec<FieldAndFormat>,
    pub fields: Vec<FieldAndFormat>,
    pub script_fields: Vec<ScriptField>,
    pub sort: Vec<Sort>,
    pub search_after: Option<Vec<Value>>,
    pub indices_boost: Vec<(String, f32)>,
    pub rescore: Vec<Rescore>,
    pub stats: Vec<String>,
    pub collapse: Option<Collapse>,
    pub slice: Option<Slice>,
    pub pit: Option<PointInTime>,
    pub runtime_mappings: Option<Map>,
    pub aggs: Option<Map>,
    /// Where `aggs` came from, when the raw body is known.
    pub aggs_raw: Option<std::sync::Arc<AggsRaw>>,
    pub highlight: Option<Map>,
    pub suggest: Option<Map>,
    pub knn: Vec<Map>,
    pub retriever: Option<Map>,
    /// A `diversify` retriever over the rest of the request (set by [`crate::search::retriever`]).
    pub diversify: Option<crate::search::retriever::Diversify>,
    /// A compound retriever (`rrf`, `linear`): the children whose rankings fuse.
    pub fusion: Option<Box<crate::search::retriever::Fusion>>,
    /// Set by a `rescorer` retriever: its window is rescored before `collapse` applies (a
    /// plain search collapses first and rescores the collapsed hits).
    pub rescore_before_collapse: bool,
    pub rank: Option<Map>,
    /// `sub_searches` (deprecated with `rank`): their queries.
    pub sub_searches: Vec<Query>,
    pub ext: Option<Map>,
    /// Deprecation warnings raised while parsing.
    pub warnings: Vec<String>,
    /// OpenSearch: the search counts as one with a k-NN query for concurrent segment search
    /// although `query` doesn't show it (a hybrid search's final phase over rank docs).
    pub os_knn: bool,
}

/// `SearchSourceBuilder.from`'s message for a negative value (OpenSearch words it like the
/// `size` one).
pub fn negative_from(v: i32) -> String {
    if crate::opensearch::active() {
        format!("[from] parameter cannot be negative, found [{v}]")
    } else {
        format!("[from] parameter cannot be negative but was [{v}]")
    }
}

/// The checks `HighlightBuilder.fromXContent` makes while parsing (positioned on the
/// `highlight` object): empty tag lists, and `number_of_fragments` out of `[0, 10000]` (wrapped
/// in each enclosing `ObjectParser`'s "failed to parse field").
fn check_highlight(p: &XParser<'_>) -> EsResult<()> {
    use crate::json::locate::Seg;
    let Value::Obj(h) = p.value() else { return Ok(()) };
    let empty = |k: &str| matches!(h.get(k), Some(Value::Arr(a)) if a.is_empty());
    if empty("pre_tags") || empty("post_tags") {
        // Raised once the whole object was read: at its end.
        let (l, c) = crate::json::locate::value_end(p.source(), &p.path()).unwrap_or_else(|| p.token_location());
        return Err(EsError::parsing(l, c, "pre_tags or post_tags must not be empty"));
    }
    // The cause of a bad `key` value, if it is one.
    let cause = |key: &str, v: &Value| -> Option<EsError> {
        match key {
            "number_of_fragments" => {
                v.as_f64().is_some_and(|n| !(0.0..=10000.0).contains(&n)).then(|| EsError::illegal_argument("[number_of_fragments] must be between [0] and [10000]"))
            }
            // `intValue()` with string coercion; 0 and below -1 are refused.
            _ => {
                let n = match v {
                    Value::Num(n) => n.as_f64(),
                    Value::Str(s) => match crate::units::parse_java_double(s.trim()) {
                        Some(d) => d,
                        None => return Some(EsError::java(400, "number_format_exception", format!("For input string: \"{s}\""))),
                    },
                    _ => return None,
                };
                let n = n.trunc();
                (n == 0.0 || n < -1.0).then(|| EsError::illegal_argument("[max_analyzed_offset] must be a positive integer, or -1"))
            }
        }
    };
    const CHECKED: [&str; 2] = ["number_of_fragments", "max_analyzed_offset"];
    for key in CHECKED {
        if let Some(e) = h.get(key).and_then(|v| cause(key, v)) {
            let (l, c) = p.location_of(&[Seg::Key(key)]);
            return Err(EsError::x_content_parse(l, c, format!("[highlight] failed to parse field [{key}]")).caused_by(e));
        }
    }
    let fields: Vec<(&str, &Value)> = match h.get("fields") {
        Some(Value::Obj(m)) => m.iter().map(|(k, v)| (k.as_str(), v)).collect(),
        Some(Value::Arr(a)) => a.iter().filter_map(|x| match x {
            Value::Obj(m) => m.iter().next().map(|(k, v)| (k.as_str(), v)),
            _ => None,
        }).collect(),
        _ => Vec::new(),
    };
    for (name, f) in fields {
        for key in CHECKED {
            let Some(e) = f.get(key).and_then(|v| cause(key, v)) else { continue };
            let (l, c) = match h.get("fields") {
                Some(Value::Obj(_)) => p.location_of(&[Seg::Key("fields"), Seg::Key(name), Seg::Key(key)]),
                _ => p.token_location(),
            };
            let inner = EsError::x_content_parse(l, c, format!("[highlight_field] failed to parse field [{key}]")).caused_by(e);
            let mid = EsError::x_content_parse(l, c, format!("[fields] failed to parse field [{name}]")).caused_by(inner);
            return Err(EsError::x_content_parse(l, c, "[highlight] failed to parse field [fields]").caused_by(mid));
        }
    }
    Ok(())
}

fn unknown_key(p: &XParser<'_>, t: Token, key: &str) -> EsError {
    p.parsing_error(format!("Unknown key for a {t} in [{key}]."))
}

impl SearchSource {
    /// Parses a request body (`raw` gives error locations).
    pub fn parse(body: &Value, raw: Option<&[u8]>) -> EsResult<SearchSource> {
        let mut p = XParser::new(body, raw, Vec::new());
        let mut s = SearchSource::default();
        let t = p.next_token();
        if t != Some(Token::StartObject) {
            return Err(p.parsing_error(format!("Expected [START_OBJECT] but found [{}]", t.map(Token::name).unwrap_or("null"))));
        }
        let mut key = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => key = p.current_name().unwrap_or("").to_string(),
                t if t.is_value() => s.value_key(&mut p, t, &key)?,
                Token::StartObject => s.object_key(&mut p, &key)?,
                Token::StartArray => s.array_key(&mut p, &key)?,
                t => return Err(unknown_key(&p, t, &key)),
            }
        }
        s.warnings.append(&mut p.warnings);
        Ok(s)
    }

    fn value_key(&mut self, p: &mut XParser<'_>, t: Token, key: &str) -> EsResult<()> {
        match key {
            "from" => {
                let v = p.int_value()?;
                if v < 0 {
                    return Err(EsError::illegal_argument(negative_from(v)));
                }
                self.from = Some(v);
            }
            "size" => {
                let v = p.int_value()?;
                if v < 0 {
                    return Err(EsError::illegal_argument(format!("[size] parameter cannot be negative, found [{v}]")));
                }
                self.size = Some(v);
            }
            "timeout" => self.timeout = Some(TimeValue::parse(&p.text()?, "timeout")?),
            "terminate_after" => {
                let v = p.int_value()?;
                if v <= 0 {
                    return Err(EsError::illegal_argument("terminateAfter must be > 0"));
                }
                self.terminate_after = Some(v);
            }
            "min_score" => self.min_score = Some(p.float_value()?),
            "version" => self.version = Some(p.boolean_value()?),
            "seq_no_primary_term" => self.seq_no_primary_term = Some(p.boolean_value()?),
            "explain" => self.explain = Some(p.boolean_value()?),
            "track_scores" => self.track_scores = p.boolean_value()?,
            "track_total_hits" => {
                self.track_total_hits = Some(if t == Token::Boolean || (t == Token::String && p.is_boolean_value()) {
                    if p.boolean_value()? { TrackTotalHits::Accurate } else { TrackTotalHits::Disabled }
                } else {
                    let v = p.int_value()?;
                    if v < -1 {
                        return Err(EsError::illegal_argument(format!("[track_total_hits] parameter must be positive or equals to -1, got {v}")));
                    }
                    // `TRACK_TOTAL_HITS_DISABLED` is -1; `Integer.MAX_VALUE` is accurate.
                    match v {
                        -1 => TrackTotalHits::Disabled,
                        i32::MAX => TrackTotalHits::Accurate,
                        n => TrackTotalHits::UpTo(n),
                    }
                })
            }
            "_source" => self.source = Some(fetch_source(p)?),
            "stored_fields" => self.stored_fields = Some(stored_fields(p, key)?),
            "sort" => self.sort.push(Sort::Field {
                field: p.text()?,
                order: None,
                missing: None,
                unmapped_type: None,
                mode: None,
                numeric_type: None,
                format: None,
                nested: None,
            }),
            "profile" => self.profile = p.boolean_value()?,
            "verbose_pipeline" if crate::opensearch::active() => self.verbose_pipeline = p.boolean_value()?,
            _ => return Err(unknown_key(p, t, key)),
        }
        Ok(())
    }

    fn object_key(&mut self, p: &mut XParser<'_>, key: &str) -> EsResult<()> {
        match key {
            "query" => {
                if !self.sub_searches.is_empty() {
                    return Err(EsError::illegal_argument("cannot specify field [query] and field [sub_searches]"));
                }
                self.query = Some(parse_inner(p)?)
            }
            "post_filter" => self.post_filter = Some(parse_inner(p)?),
            "_source" => self.source = Some(fetch_source(p)?),
            "script_fields" => {
                while let Some(t) = p.next_token() {
                    if t == Token::EndObject {
                        break;
                    }
                    self.script_fields.push(script_field(p)?);
                }
            }
            "aggregations" | "aggs" => {
                self.aggs_raw = p.raw().map(|r| std::sync::Arc::new(AggsRaw { body: r.to_vec(), key: key.to_string() }));
                let aggs = p.map()?;
                // `SearchSourceBuilder.parseXContent` parses the aggregations (their embedded
                // queries included) in body order: a malformed one fails before later keys.
                if let Some(raw) = &self.aggs_raw {
                    crate::search::aggs::parse_error(&aggs, Some(raw))?;
                }
                self.aggs = Some(aggs);
            }
            "highlight" => {
                check_highlight(p)?;
                self.highlight = Some(p.map()?);
            }
            "suggest" => {
                crate::search::suggest::check(p)?;
                self.suggest = Some(p.map()?);
            }
            "sort" => self.sort = sorts(p)?,
            "rescore" => self.rescore.push(rescore(p)?),
            "ext" if crate::opensearch::active() => self.ext = Some(crate::opensearch::parse_ext(p)?),
            "ext" => self.ext = Some(p.map()?),
            "slice" => self.slice = Some(slice(p)?),
            "collapse" => self.collapse = Some(collapse(p)?),
            "pit" => self.pit = Some(pit(p)?),
            // OpenSearch has no runtime fields; its search-time `derived` fields run as them.
            "runtime_mappings" if crate::opensearch::active() => return Err(p.parsing_error("Unknown key for a START_OBJECT in [runtime_mappings].")),
            "derived" if crate::opensearch::active() => self.runtime_mappings = Some(crate::opensearch::derived_fields(p.map()?)?),
            "runtime_mappings" => self.runtime_mappings = Some(p.map()?),
            "knn" => self.knn = vec![p.map()?],
            "retriever" => self.retriever = Some(p.map()?),
            "rank" => {
                self.warnings.push("Deprecated field [rank] used, replaced by [retriever]".into());
                self.rank = Some(p.map()?)
            }
            // OpenSearch still reads the (deprecated) object form `{"index": boost, ..}`.
            "indices_boost" if crate::opensearch::active() => {
                let mut name = String::new();
                while let Some(t) = p.next_token() {
                    match t {
                        Token::EndObject => break,
                        Token::FieldName => name = p.current_name().unwrap_or("").to_string(),
                        t if t.is_value() => self.indices_boost.push((name.clone(), p.float_value()?)),
                        t => return Err(p.parsing_error(format!("Unknown key for a {t} in [{name}]."))),
                    }
                }
            }
            _ => return Err(unknown_key(p, Token::StartObject, key)),
        }
        Ok(())
    }

    fn array_key(&mut self, p: &mut XParser<'_>, key: &str) -> EsResult<()> {
        match key {
            "stored_fields" => self.stored_fields = Some(stored_fields(p, key)?),
            "docvalue_fields" | "fields" => {
                let mut out = Vec::new();
                while let Some(t) = p.next_token() {
                    if t == Token::EndArray {
                        break;
                    }
                    out.push(field_and_format(p)?);
                }
                if key == "fields" {
                    self.fields = out;
                } else {
                    // OpenSearch's `DocValueFieldsFetchSubPhase`: `use_field_mapping` (a 6.x
                    // transition value) means the field's own format.
                    if crate::opensearch::active() {
                        for f in out.iter_mut().filter(|f| f.format.as_deref() == Some("use_field_mapping")) {
                            f.format = None;
                        }
                    }
                    self.docvalue_fields = out;
                }
            }
            "indices_boost" => {
                while let Some(t) = p.next_token() {
                    if t == Token::EndArray {
                        break;
                    }
                    self.indices_boost.push(index_boost(p)?);
                }
            }
            "sort" => self.sort = sorts(p)?,
            "rescore" => {
                while let Some(t) = p.next_token() {
                    if t == Token::EndArray {
                        break;
                    }
                    self.rescore.push(rescore(p)?);
                }
            }
            "stats" => {
                while let Some(t) = p.next_token() {
                    match t {
                        Token::EndArray => break,
                        Token::String => self.stats.push(p.text()?),
                        t => return Err(p.parsing_error(format!("Expected [VALUE_STRING] in [{key}] but found [{t}]"))),
                    }
                }
            }
            "_source" => self.source = Some(fetch_source(p)?),
            "search_after" => self.search_after = Some(p.list()?),
            "sub_searches" => {
                self.warnings.push("Deprecated field [sub_searches] used, replaced by [retriever]".into());
                if self.query.is_some() {
                    return Err(EsError::illegal_argument("cannot specify field [sub_searches] and field [query]"));
                }
                while let Some(t) = p.next_token() {
                    match t {
                        Token::EndArray => break,
                        Token::StartObject => self.sub_searches.push(sub_search(p)?),
                        t => return Err(p.xcontent_error(format!("malformed query within the [sub_searches] field; found {t}"))),
                    }
                }
            }
            "knn" => {
                while let Some(t) = p.next_token() {
                    match t {
                        Token::EndArray => break,
                        Token::StartObject => self.knn.push(p.map()?),
                        t => {
                            return Err(p.xcontent_error(format!(
                                "malformed knn format, within the knn search array only objects are allowed; found {t}"
                            )));
                        }
                    }
                }
            }
            _ => return Err(unknown_key(p, Token::StartArray, key)),
        }
        Ok(())
    }
}

/// `SubSearchSourceBuilder.fromXContent`: `{"query": {..}}`.
fn sub_search(p: &mut XParser<'_>) -> EsResult<Query> {
    let mut query = None;
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => {
                let name = p.current_name().unwrap_or("").to_string();
                let t = p.next_token();
                if name != "query" || t != Some(Token::StartObject) {
                    return Err(p.xcontent_error(format!("[sub_search] unknown field [{name}]")));
                }
                query = Some(parse_inner(p)?);
            }
            t => return Err(p.parsing_error(format!("Unknown key for a {t} in [sub_search]."))),
        }
    }
    query.ok_or_else(|| p.xcontent_error("Required [query]"))
}

/// `FetchSourceContext.fromXContent`.
fn fetch_source(p: &mut XParser<'_>) -> EsResult<FetchSource> {
    let mut fs = FetchSource { fetch: true, includes: Vec::new(), excludes: Vec::new(), exclude_vectors: None };
    let string_array = |p: &mut XParser<'_>, key: &str| -> EsResult<Vec<String>> {
        let mut out = Vec::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndArray => break,
                Token::String => out.push(p.text()?),
                t => return Err(p.parsing_error(format!("Unknown key for a {t} in [{key}]."))),
            }
        }
        Ok(out)
    };
    match p.token() {
        Token::Boolean => fs.fetch = p.boolean_value()?,
        Token::String => fs.includes = vec![p.text()?],
        Token::StartArray => {
            while let Some(t) = p.next_token() {
                if t == Token::EndArray {
                    break;
                }
                fs.includes.push(p.text()?);
            }
        }
        Token::StartObject => {
            let mut key = String::new();
            while let Some(t) = p.next_token() {
                let includes = || key == "includes" || key == "include";
                let excludes = || key == "excludes" || key == "exclude";
                match t {
                    Token::EndObject => break,
                    Token::FieldName => {
                        key = p.current_name().unwrap_or("").to_string();
                        if key == "include" {
                            p.deprecated_name("include", "includes", None);
                        } else if key == "exclude" {
                            p.deprecated_name("exclude", "excludes", None);
                        }
                    }
                    Token::StartArray if includes() => fs.includes = string_array(p, &key)?,
                    Token::StartArray if excludes() => fs.excludes = string_array(p, &key)?,
                    Token::String if includes() => fs.includes = vec![p.text()?],
                    Token::String if excludes() => fs.excludes = vec![p.text()?],
                    Token::String | Token::Boolean if key == "exclude_vectors" => fs.exclude_vectors = Some(p.boolean_value()?),
                    t => return Err(unknown_key(p, t, &key)),
                }
            }
        }
        t => {
            return Err(p.parsing_error(format!(
                "Expected one of [VALUE_BOOLEAN, VALUE_STRING, START_ARRAY, START_OBJECT] but found [{t}]"
            )));
        }
    }
    crate::fetch::check_ambiguous_source(&fs.includes, &fs.excludes)?;
    Ok(fs)
}

/// `StoredFieldsContext.fromXContent`.
fn stored_fields(p: &mut XParser<'_>, key: &str) -> EsResult<StoredFields> {
    let names = match p.token() {
        Token::String => vec![p.text()?],
        Token::StartArray => {
            let mut out = Vec::new();
            while let Some(t) = p.next_token() {
                if t == Token::EndArray {
                    break;
                }
                out.push(p.text()?);
            }
            out
        }
        t => return Err(p.parsing_error(format!("Expected [VALUE_STRING] or [START_ARRAY] in [{key}] but found [{t}]"))),
    };
    if names.iter().any(|n| n == "_none_") {
        if names.len() > 1 {
            return Err(EsError::illegal_argument("cannot combine _none_ with other fields"));
        }
        return Ok(StoredFields { fields: None });
    }
    Ok(StoredFields { fields: Some(names) })
}

/// `FieldAndFormat.fromXContent`: a name, or `{field, format, include_unmapped}`.
fn field_and_format(p: &mut XParser<'_>) -> EsResult<FieldAndFormat> {
    if p.token() != Token::StartObject {
        return Ok(FieldAndFormat { field: p.text()?, format: None, include_unmapped: None });
    }
    const FIELDS: &[Field] =
        &[Field::new("field", Tokens::STRING), Field::new("format", Tokens::STRING_OR_NULL), Field::new("include_unmapped", Tokens::BOOLEAN)];
    let (mut field, mut format, mut include_unmapped) = (None, None, None);
    p.object("fetch_field_and_format", FIELDS, |p, i| {
        match i {
            0 => field = Some(p.text()?),
            1 => format = p.text_or_null()?,
            _ => include_unmapped = Some(p.boolean_value()?),
        }
        Ok(())
    })?;
    let field = field.ok_or_else(|| EsError::illegal_argument("Required [field]"))?;
    Ok(FieldAndFormat { field, format, include_unmapped })
}

/// A `script_fields` entry (`SearchSourceBuilder.ScriptField`), on its name.
fn script_field(p: &mut XParser<'_>) -> EsResult<ScriptField> {
    let name = p.current_name().unwrap_or("").to_string();
    let t = p.next_token();
    if t != Some(Token::StartObject) {
        return Err(p.parsing_error(format!(
            "Expected [START_OBJECT] in [script_fields] but found [{}]",
            t.map(Token::name).unwrap_or("null")
        )));
    }
    let (mut script, mut ignore_failure) = (None, false);
    let mut key = String::new();
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => key = p.current_name().unwrap_or("").to_string(),
            t if t.is_value() => match key.as_str() {
                "script" => script = Some(Script::parse(p)?),
                "ignore_failure" => ignore_failure = p.boolean_value()?,
                _ => return Err(unknown_key(p, t, &key)),
            },
            Token::StartObject if key == "script" => script = Some(Script::parse(p)?),
            t => return Err(unknown_key(p, t, &key)),
        }
    }
    let script = script.ok_or_else(|| p.parsing_error("Expected [script] in [script_fields]"))?;
    Ok(ScriptField { name, script, ignore_failure })
}

/// `script_fields` given as a value (`top_hits`, already shape-checked by its parser).
pub fn script_fields_from_value(spec: &Map) -> EsResult<Vec<ScriptField>> {
    let mut out = Vec::with_capacity(spec.len());
    for (name, def) in spec {
        let Some(script) = def.get("script") else { continue };
        let mut p = XParser::from_value(script);
        p.next_token();
        let script = Script::parse(&mut p)?;
        let ignore_failure = def.get("ignore_failure").and_then(Value::as_bool).unwrap_or(false);
        out.push(ScriptField { name: name.clone(), script, ignore_failure });
    }
    Ok(out)
}

/// An `indices_boost` entry: `{"index": boost}`.
fn index_boost(p: &mut XParser<'_>) -> EsResult<(String, f32)> {
    if p.token() != Token::StartObject {
        return Err(p.parsing_error(format!("Expected [START_OBJECT] in [indices_boost] but found [{}]", p.token())));
    }
    let t = p.next_token();
    if t != Some(Token::FieldName) {
        return Err(p.parsing_error(format!("Expected [FIELD_NAME] in [indices_boost] but found [{}]", t.map(Token::name).unwrap_or("null"))));
    }
    let index = p.current_name().unwrap_or("").to_string();
    p.next_token();
    let boost = p.float_value()?;
    let t = p.next_token();
    if t != Some(Token::EndObject) {
        return Err(p.parsing_error(format!("Expected [END_OBJECT] in [indices_boost] but found [{}]", t.map(Token::name).unwrap_or("null"))));
    }
    Ok((index, boost))
}

/// [`sorts`] over an already parsed value (aggregations' `sort`).
pub fn parse_sorts_value(v: &Value) -> EsResult<Vec<Sort>> {
    let mut p = XParser::from_value(v);
    p.next_token();
    sorts(&mut p)
}

/// `SortBuilder.fromXContent`: a name, an object of sorts, or an array of both.
fn sorts(p: &mut XParser<'_>) -> EsResult<Vec<Sort>> {
    let mut out = Vec::new();
    match p.token() {
        Token::StartObject => compound_sort(p, &mut out)?,
        Token::String => out.push(simple_sort(p.text()?, None)),
        Token::StartArray => {
            while let Some(t) = p.next_token() {
                match t {
                    Token::EndArray => break,
                    Token::StartObject => compound_sort(p, &mut out)?,
                    Token::String => out.push(simple_sort(p.text()?, None)),
                    _ => {
                        return Err(EsError::illegal_argument(
                            "malformed sort format, within the sort array, an object, or an actual string are allowed",
                        ));
                    }
                }
            }
        }
        _ => return Err(EsError::illegal_argument("malformed sort format, within the sort array, an object, or an actual string are allowed")),
    }
    Ok(out)
}

fn simple_sort(name: String, order: Option<SortOrder>) -> Sort {
    if name == "_score" {
        return Sort::Score { order: order.unwrap_or(SortOrder::Desc) };
    }
    Sort::Field { field: name, order, missing: None, unmapped_type: None, mode: None, numeric_type: None, format: None, nested: None }
}

/// `{"field": "asc"}`, `{"field": {...}}`, `{"_script": {...}}`, `{"_geo_distance": {...}}`.
fn compound_sort(p: &mut XParser<'_>, out: &mut Vec<Sort>) -> EsResult<()> {
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => {
                let name = p.current_name().unwrap_or("").to_string();
                let t = p.next_token().unwrap_or(Token::Null);
                match t {
                    Token::String => out.push(simple_sort(name, Some(SortOrder::parse(&p.text()?)?))),
                    Token::StartObject => match name.as_str() {
                        "_script" => out.push(script_sort(p)?),
                        "_geo_distance" | "_geoDistance" => out.push(Sort::GeoDistance(p.map()?)),
                        "_score" => {
                            const F: &[Field] = &[Field::new("order", Tokens::STRING)];
                            let mut order = SortOrder::Desc;
                            p.object("_score", F, |p, _| {
                                order = SortOrder::parse(&p.text()?)?;
                                Ok(())
                            })?;
                            out.push(Sort::Score { order });
                        }
                        _ => out.push(field_sort(p, name)?),
                    },
                    t => return Err(EsError::illegal_argument(format!("malformed sort format, within the sort array, an object, or an actual string are allowed, found [{t}]"))),
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The `nested` object of a sort.
fn nested_sort(p: &mut XParser<'_>) -> EsResult<NestedSort> {
    const F: &[Field] = &[
        Field::new("path", Tokens::STRING),
        Field::new("filter", Tokens::OBJECT),
        Field::new("max_children", Tokens::INT),
        Field::new("nested", Tokens::OBJECT),
    ];
    let (mut path, mut filter, mut max_children, mut nested) = (None, None, None, None);
    p.object("nested_sort", F, |p, i| {
        match i {
            0 => path = Some(p.text()?),
            1 => filter = Some(parse_inner(p)?),
            2 => max_children = Some(p.int_value()?),
            _ => nested = Some(Box::new(nested_sort(p)?)),
        }
        Ok(())
    })?;
    Ok(NestedSort { path: path.unwrap_or_default(), filter, max_children, nested })
}

/// `FieldSortBuilder` (`field_sort` object parser).
fn field_sort(p: &mut XParser<'_>, field: String) -> EsResult<Sort> {
    const F: &[Field] = &[
        Field::new("missing", Tokens::VALUE),
        Field::new("unmapped_type", Tokens::STRING),
        Field::new("order", Tokens::STRING),
        Field::new("mode", Tokens::STRING),
        Field::new("nested", Tokens::OBJECT),
        Field::new("numeric_type", Tokens::STRING),
        Field::new("format", Tokens::STRING),
    ];
    let (mut order, mut missing, mut unmapped_type, mut mode, mut nested, mut numeric_type, mut format) = (None, None, None, None, None, None, None);
    // A sort `format` is an Elasticsearch 8 addition: OpenSearch's parser doesn't know it.
    let fields = if crate::opensearch::active() { &F[..F.len() - 1] } else { F };
    p.object("field_sort", fields, |p, i| {
        match i {
            0 => missing = Some(p.value().clone()),
            1 => unmapped_type = Some(p.text()?),
            2 => order = Some(SortOrder::parse(&p.text()?)?),
            3 => {
                let m = p.text()?.to_lowercase();
                if !matches!(m.as_str(), "min" | "max" | "sum" | "avg" | "median") {
                    return Err(EsError::illegal_argument(format!("Unknown SortMode [{m}]")));
                }
                mode = Some(m);
            }
            4 => nested = Some(nested_sort(p)?),
            5 => {
                let t = p.text()?;
                if !matches!(t.as_str(), "long" | "double" | "date" | "date_nanos") {
                    return Err(EsError::illegal_argument(format!("invalid value for [numeric_type], must be [long, double, date, date_nanos], got {t}")));
                }
                numeric_type = Some(t);
            }
            _ => format = Some(p.text()?),
        }
        Ok(())
    })?;
    Ok(Sort::Field { field, order, missing, unmapped_type, mode, numeric_type, format, nested })
}

/// `ScriptSortBuilder` (`_script`).
fn script_sort(p: &mut XParser<'_>) -> EsResult<Sort> {
    const F: &[Field] = &[
        Field::new("script", Tokens::OBJECT_OR_STRING),
        Field::new("type", Tokens::STRING),
        Field::new("order", Tokens::STRING),
        Field::new("mode", Tokens::STRING),
        Field::new("nested", Tokens::OBJECT),
    ];
    let (mut script, mut ty, mut order, mut mode, mut nested) = (None, None, None, None, None);
    // Where `mode` and the last of the two constructor arguments (`script`, `type`) were, and
    // whether `mode` came after both (`ConstructingObjectParser` queues it until then).
    let (mut mode_at, mut last_required) = ((0, 0), ("", (0, 0)));
    let mut mode_late = false;
    p.object("_script", F, |p, i| {
        match i {
            0 => {
                last_required = ("script", p.token_location());
                script = Some(Script::parse(p)?);
            }
            1 => {
                last_required = ("type", p.token_location());
                let t = p.text()?;
                if !matches!(t.to_lowercase().as_str(), "number" | "string" | "version") {
                    return Err(EsError::illegal_argument(format!("Unknown ScriptSortType [{t}]")));
                }
                ty = Some(t.to_lowercase());
            }
            2 => order = Some(SortOrder::parse(&p.text()?)?),
            3 => {
                mode_at = p.token_location();
                mode_late = script.is_some() && ty.is_some();
                mode = Some(p.text()?.to_lowercase());
            }
            _ => nested = Some(nested_sort(p)?),
        }
        Ok(())
    })?;
    let missing: Vec<&str> = [("script", script.is_none()), ("type", ty.is_none())].iter().filter(|x| x.1).map(|x| x.0).collect();
    if !missing.is_empty() {
        return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
    }
    // `ScriptSortBuilder.sortMode`: a string sort takes `min` or `max` only.
    if let (Some("string"), Some(m @ ("sum" | "avg" | "median"))) = (ty.as_deref(), mode.as_deref()) {
        let e = EsError::illegal_argument(format!("script sort of type [string] doesn't support mode [{m}]"));
        let at_mode = EsError::x_content_parse(mode_at.0, mode_at.1, "[_script] failed to parse field [mode]").caused_by(e);
        return Err(if mode_late {
            at_mode
        } else {
            let built = EsError::x_content_parse(mode_at.0, mode_at.1, "failed to build [_script] after last required field arrived").caused_by(at_mode);
            let (name, (l, c)) = last_required;
            EsError::x_content_parse(l, c, format!("[_script] failed to parse field [{name}]")).caused_by(built)
        });
    }
    Ok(Sort::Script { script: script.unwrap_or_else(|| Script::inline("")), ty: ty.unwrap_or_default(), order, mode, nested })
}

/// `RescorerBuilder.parseFromXContent`: `window_size` and one named rescorer (`query`).
fn rescore(p: &mut XParser<'_>) -> EsResult<Rescore> {
    let mut window_size = None;
    let mut out = None;
    let mut key = String::new();
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => key = p.current_name().unwrap_or("").to_string(),
            t if t.is_value() => {
                if key == "window_size" {
                    window_size = Some(p.int_value()?);
                } else {
                    return Err(p.parsing_error(format!("rescore doesn't support [{key}]")));
                }
            }
            Token::StartObject if key == "script" => {
                const F: &[Field] = &[Field::new("script", Tokens::OBJECT_OR_STRING)];
                let mut script = None;
                p.object("script", F, |p, _| {
                    script = Some(Script::parse(p)?);
                    Ok(())
                })?;
                let script = script.ok_or_else(|| EsError::illegal_argument("[script] is required"))?;
                let q = crate::query::Query::from(crate::query::compound::MatchAll::default());
                out = Some(Rescore { window_size: None, query: q, query_weight: 1.0, rescore_query_weight: 1.0, score_mode: "total".into(), script: Some(script) });
            }
            Token::StartObject => {
                if key != "query" {
                    let (l, c) = p.token_location();
                    return Err(EsError::new(400, "named_object_not_found_exception", format!("[{l}:{c}] unknown field [{key}]")));
                }
                const F: &[Field] = &[
                    Field::new("rescore_query", Tokens::OBJECT),
                    Field::new("query_weight", Tokens::FLOAT),
                    Field::new("rescore_query_weight", Tokens::FLOAT),
                    Field::new("score_mode", Tokens::STRING),
                ];
                let (mut q, mut qw, mut rqw, mut mode) = (None, 1.0f32, 1.0f32, "total".to_string());
                p.object("query", F, |p, i| {
                    match i {
                        0 => q = Some(parse_inner(p)?),
                        1 => qw = p.float_value()?,
                        2 => rqw = p.float_value()?,
                        _ => {
                            let m = p.text()?.to_lowercase();
                            if !matches!(m.as_str(), "total" | "multiply" | "avg" | "max" | "min") {
                                return Err(EsError::illegal_argument(format!("illegal score_mode [{m}]")));
                            }
                            mode = m;
                        }
                    }
                    Ok(())
                })?;
                let q = q.ok_or_else(|| EsError::illegal_argument("Required [rescore_query]"))?;
                out = Some(Rescore { window_size: None, query: q, query_weight: qw, rescore_query_weight: rqw, score_mode: mode, script: None });
            }
            t => return Err(p.parsing_error(format!("unexpected token [{t}] after [{key}]"))),
        }
    }
    let mut r = out.ok_or_else(|| p.parsing_error("missing rescore type"))?;
    r.window_size = window_size;
    Ok(r)
}

/// `CollapseBuilder.fromXContent`.
fn collapse(p: &mut XParser<'_>) -> EsResult<Collapse> {
    const F: &[Field] = &[
        Field::new("field", Tokens::STRING),
        Field::new("inner_hits", Tokens::OBJECT_ARRAY),
        Field::new("max_concurrent_group_searches", Tokens::INT),
    ];
    let (mut field, mut inner_hits, mut max) = (None, Vec::new(), None);
    p.object("collapse", F, |p, i| {
        match i {
            0 => field = Some(p.text()?),
            1 => {
                inner_hits = p.array_items(|p| {
                    let v = p.take_value();
                    // `InnerHitBuilder`: a name is required, and its own `collapse` takes only a
                    // field (no `inner_hits` or `collapse` of its own).
                    // (`p.object` adds the `[collapse] failed to parse field [inner_hits]` layer.)
                    let (l, c) = p.token_location();
                    if let Some(Value::Obj(inner)) = v.get("collapse")
                        && (inner.contains_key("inner_hits") || inner.contains_key("collapse"))
                    {
                        let invalid = EsError::parsing(l, c, "Invalid token in the inner collapse");
                        return Err(EsError::x_content_parse(l, c, "[inner_hits] failed to parse field [collapse]").caused_by(invalid));
                    }
                    if v.get("name").is_none() {
                        return Err(EsError::illegal_argument("inner_hits must have a [name]; set the [name] field in the inner_hits definition"));
                    }
                    Ok(v)
                })?
            }
            _ => {
                let v = p.int_value()?;
                if v < 1 {
                    return Err(EsError::illegal_argument("maxConcurrentGroupRequests must be positive."));
                }
                max = Some(v);
            }
        }
        Ok(())
    })?;
    Ok(Collapse { field: field.unwrap_or_default(), inner_hits, max_concurrent_group_searches: max })
}

/// `SliceBuilder.fromXContent`.
fn slice(p: &mut XParser<'_>) -> EsResult<Slice> {
    const F: &[Field] = &[Field::new("field", Tokens::STRING), Field::new("id", Tokens::INT), Field::new("max", Tokens::INT)];
    let mut s = Slice { field: None, id: -1, max: -1 };
    p.object("slice", F, |p, i| {
        match i {
            0 => s.field = Some(p.text()?),
            1 => {
                let id = p.int_value()?;
                if id < 0 {
                    return Err(EsError::illegal_argument("id must be greater than or equal to 0"));
                }
                if s.max != -1 && id >= s.max {
                    return Err(EsError::illegal_argument("max must be greater than id"));
                }
                s.id = id;
            }
            _ => {
                let max = p.int_value()?;
                if max <= 1 {
                    return Err(EsError::illegal_argument("max must be greater than 1"));
                }
                if s.id != -1 && s.id >= max {
                    return Err(EsError::illegal_argument("max must be greater than id"));
                }
                s.max = max;
            }
        }
        Ok(())
    })?;
    Ok(s)
}

/// `PointInTimeBuilder.fromXContent`.
fn pit(p: &mut XParser<'_>) -> EsResult<PointInTime> {
    const F: &[Field] = &[Field::new("id", Tokens::STRING), Field::new("keep_alive", Tokens::STRING)];
    let (mut id, mut keep_alive) = (None, None);
    p.object("pit", F, |p, i| {
        if i == 0 {
            let text = p.text()?;
            // `SearchContextId.decode` reads the id as URL-safe base64 while parsing.
            if let Some(msg) = crate::units::java_base64_url_error(&text) {
                return Err(EsError::illegal_argument(msg));
            }
            id = Some(text);
        } else {
            keep_alive = Some(TimeValue::parse(&p.text()?, "keep_alive")?);
        }
        Ok(())
    })?;
    let id = id.ok_or_else(|| EsError::illegal_argument("Required [id]"))?;
    Ok(PointInTime { id, keep_alive })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> EsResult<SearchSource> {
        let v = crate::json::parse(src.as_bytes()).unwrap().unwrap();
        SearchSource::parse(&v, Some(src.as_bytes()))
    }

    #[test]
    fn sections() {
        let s = parse(r#"{"query":{"match_all":{}},"from":1,"size":"2","sort":["a",{"b":"desc"},"_score"],"_source":{"include":"x"},"track_total_hits":true}"#).unwrap();
        assert_eq!(s.from, Some(1));
        assert_eq!(s.size, Some(2));
        assert_eq!(s.sort.len(), 3);
        assert_eq!(s.sort[2], Sort::Score { order: SortOrder::Desc });
        assert_eq!(s.source.unwrap().includes, vec!["x".to_string()]);
        assert_eq!(s.track_total_hits, Some(TrackTotalHits::Accurate));
        assert_eq!(s.warnings, vec!["Deprecated field [include] used, expected [includes] instead".to_string()]);
    }

    #[test]
    fn errors() {
        let e = parse(r#"{"foo":1}"#).unwrap_err();
        assert_eq!(e.reason(), "Unknown key for a VALUE_NUMBER in [foo].");
        assert_eq!(parse(r#"{"from":-1}"#).unwrap_err().reason(), "[from] parameter cannot be negative but was [-1]");
        assert_eq!(parse(r#"{"stored_fields":["a","_none_"]}"#).unwrap_err().reason(), "cannot combine _none_ with other fields");
        assert_eq!(parse(r#"{"terminate_after":-1}"#).unwrap_err().reason(), "terminateAfter must be > 0");
        let e = parse(r#"{"sort":{"a":{"order":"foo"}}}"#).unwrap_err();
        assert_eq!(e.reason(), "[1:23] [field_sort] failed to parse field [order]");
    }
}
