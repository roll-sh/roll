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

/// A query rescorer.
#[derive(Clone, Debug, PartialEq)]
pub struct Rescore {
    pub window_size: Option<i32>,
    pub query: Query,
    pub query_weight: f32,
    pub rescore_query_weight: f32,
    pub score_mode: String,
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
    pub highlight: Option<Map>,
    pub suggest: Option<Map>,
    pub knn: Vec<Map>,
    pub retriever: Option<Map>,
    pub rank: Option<Map>,
    pub ext: Option<Map>,
    /// Deprecation warnings raised while parsing.
    pub warnings: Vec<String>,
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
                    return Err(EsError::illegal_argument(format!("[from] parameter cannot be negative but was [{v}]")));
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
                    match v {
                        -1 | i32::MAX => TrackTotalHits::Accurate,
                        0 => TrackTotalHits::Disabled,
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
            _ => return Err(unknown_key(p, t, key)),
        }
        Ok(())
    }

    fn object_key(&mut self, p: &mut XParser<'_>, key: &str) -> EsResult<()> {
        match key {
            "query" => self.query = Some(parse_inner(p)?),
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
            "aggregations" | "aggs" => self.aggs = Some(p.map()?),
            "highlight" => self.highlight = Some(p.map()?),
            "suggest" => self.suggest = Some(p.map()?),
            "sort" => self.sort = sorts(p)?,
            "rescore" => self.rescore.push(rescore(p)?),
            "ext" => self.ext = Some(p.map()?),
            "slice" => self.slice = Some(slice(p)?),
            "collapse" => self.collapse = Some(collapse(p)?),
            "pit" => self.pit = Some(pit(p)?),
            "runtime_mappings" => self.runtime_mappings = Some(p.map()?),
            "knn" => self.knn = vec![p.map()?],
            "retriever" => self.retriever = Some(p.map()?),
            "rank" => self.rank = Some(p.map()?),
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
    p.object("field_sort", F, |p, i| {
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
    p.object("_script", F, |p, i| {
        match i {
            0 => script = Some(Script::parse(p)?),
            1 => {
                let t = p.text()?;
                if !matches!(t.to_lowercase().as_str(), "number" | "string" | "version") {
                    return Err(EsError::illegal_argument(format!("Unknown ScriptSortType [{t}]")));
                }
                ty = Some(t.to_lowercase());
            }
            2 => order = Some(SortOrder::parse(&p.text()?)?),
            3 => mode = Some(p.text()?.to_lowercase()),
            _ => nested = Some(nested_sort(p)?),
        }
        Ok(())
    })?;
    let missing: Vec<&str> = [("script", script.is_none()), ("type", ty.is_none())].iter().filter(|x| x.1).map(|x| x.0).collect();
    if !missing.is_empty() {
        return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
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
                out = Some(Rescore { window_size: None, query: q, query_weight: qw, rescore_query_weight: rqw, score_mode: mode });
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
            1 => inner_hits = p.array_items(|p| Ok(p.take_value()))?,
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
            id = Some(p.text()?);
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
