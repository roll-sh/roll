//! `_analyze` (`indices.analyze`): runs text through an analyzer and reports the tokens, as
//! `TransportAnalyzeAction` does.
//!
//! The request body is parsed here with `ObjectParser`'s rules and error texts (field type
//! checks, `[line:col]` locations from the raw body); analyzer selection, inline component
//! definitions, explain output and the token/character limits are `search-analysis`'s job.
//! An index (path or `index` parameter) is resolved as a single-index operation: one concrete
//! index or an alias pointing at exactly one, never a wildcard; its analysis chain comes from
//! the cluster's [`crate::analysis::AnalysisCache`].

use crate::analysis::{analyze_output, es_error, to_serde};
use crate::error::{EsError, EsResult};
use crate::json::locate::{Seg, Target, locate};
use crate::json::{Map, Value};
use crate::mapping::{Mapping, MapperRef};
use crate::meta::{ClusterState, IndexMeta, resolve};
use crate::rest::{Ctx, Reply};
use search_analysis::{AnalyzeLimits, AnalyzeRequest, Analysis, Analyzer, NameOrDefinition};
use std::sync::Arc;

/// The JSON token name Jackson reports for a value.
fn token_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "VALUE_NULL",
        Value::Bool(_) => "VALUE_BOOLEAN",
        Value::Num(_) => "VALUE_NUMBER",
        Value::Str(_) => "VALUE_STRING",
        Value::Arr(_) => "START_ARRAY",
        Value::Obj(_) => "START_OBJECT",
    }
}

struct BodyParser<'a> {
    raw: Option<&'a [u8]>,
}

impl BodyParser<'_> {
    fn loc(&self, path: &[Seg]) -> (usize, usize) {
        self.raw.and_then(|r| locate(r, path, Target::Value)).unwrap_or((1, 1))
    }

    fn err_at(&self, path: &[Seg], reason: String) -> EsError {
        let (l, c) = self.loc(path);
        EsError::x_content_parse(l, c, reason)
    }

    fn unsupported(&self, field: &str, v: &Value) -> EsError {
        self.err_at(&[Seg::Key(field)], format!("[analyze_request] {field} doesn't support values of type: {}", token_name(v)))
    }

    fn failed_field(&self, field: &str, path: &[Seg], cause: EsError) -> EsError {
        self.err_at(path, format!("[analyze_request] failed to parse field [{field}]")).caused_by(cause)
    }

    /// `declareString`: only a JSON string.
    fn string(&self, field: &str, v: &Value) -> EsResult<String> {
        match v {
            Value::Str(s) => Ok(s.clone()),
            other => Err(self.unsupported(field, other)),
        }
    }

    /// `declareStringArray`: a string or an array of scalars.
    fn string_array(&self, field: &str, v: &Value) -> EsResult<Vec<String>> {
        match v {
            Value::Str(s) => Ok(vec![s.clone()]),
            Value::Arr(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    let path = [Seg::Key(field), Seg::Index(i)];
                    match item {
                        Value::Str(s) => out.push(s.clone()),
                        Value::Num(_) | Value::Bool(_) => out.push(item.to_es_string()),
                        Value::Arr(_) => {
                            return Err(self.failed_field(field, &path, EsError::illegal_state("expected value but got [START_ARRAY]")));
                        }
                        other => {
                            let (l, c) = self.loc(&path);
                            let cause = EsError::illegal_argument(format!("Expected text at {l}:{c} but found {}", token_name(other)));
                            return Err(self.failed_field(field, &path, cause));
                        }
                    }
                }
                Ok(out)
            }
            other => Err(self.unsupported(field, other)),
        }
    }

    fn name_or_definition(&self, field: &str, path: &[Seg], v: &Value) -> EsResult<NameOrDefinition> {
        match v {
            Value::Str(s) => Ok(NameOrDefinition::Name(s.clone())),
            Value::Obj(_) => Ok(NameOrDefinition::Definition(definition_settings(v))),
            Value::Arr(_) => Err(self.failed_field(field, path, EsError::illegal_state("expected value but got [START_ARRAY]"))),
            other => {
                let (l, c) = self.loc(path);
                let cause = EsError::x_content_parse(l, c, format!("Expected [VALUE_STRING] or [START_OBJECT], got {}", token_name(other)));
                Err(self.failed_field(field, path, cause))
            }
        }
    }

    /// `declareObjectArray` of `NameOrDefinition`s: one object or an array.
    fn components(&self, field: &str, v: &Value) -> EsResult<Vec<NameOrDefinition>> {
        match v {
            Value::Obj(_) => Ok(vec![self.name_or_definition(field, &[Seg::Key(field)], v)?]),
            Value::Arr(items) => items
                .iter()
                .enumerate()
                .map(|(i, item)| self.name_or_definition(field, &[Seg::Key(field), Seg::Index(i)], item))
                .collect(),
            other => Err(self.unsupported(field, other)),
        }
    }

    fn parse(&self, body: &Map) -> EsResult<AnalyzeRequest> {
        let mut r = AnalyzeRequest::default();
        for (k, v) in body {
            match k.as_str() {
                "text" => r.text = self.string_array("text", v)?,
                "analyzer" => r.analyzer = Some(self.string("analyzer", v)?),
                "tokenizer" => {
                    r.tokenizer = Some(match v {
                        Value::Str(_) | Value::Obj(_) => self.name_or_definition("tokenizer", &[Seg::Key("tokenizer")], v)?,
                        other => return Err(self.unsupported("tokenizer", other)),
                    })
                }
                "filter" => r.filter.extend(self.components("filter", v)?),
                "char_filter" => r.char_filter.extend(self.components("char_filter", v)?),
                "field" => r.field = Some(self.string("field", v)?),
                "explain" => {
                    r.explain = match v {
                        Value::Bool(b) => *b,
                        Value::Str(s) => match s.as_str() {
                            "true" => true,
                            "false" => false,
                            _ => {
                                let cause = EsError::illegal_argument(format!("Failed to parse value [{s}] as only [true] or [false] are allowed."));
                                return Err(self.failed_field("explain", &[Seg::Key("explain")], cause));
                            }
                        },
                        other => return Err(self.unsupported("explain", other)),
                    }
                }
                "attributes" => r.attributes = self.string_array("attributes", v)?,
                "normalizer" => r.normalizer = Some(self.string("normalizer", v)?),
                other => {
                    let (l, c) = self.raw.and_then(|raw| crate::json::locate::key_location(raw, &[], other)).unwrap_or((1, 1));
                    return Err(EsError::x_content_parse(l, c, format!("[analyze_request] unknown field [{other}]")));
                }
            }
        }
        Ok(r)
    }
}

/// An inline component definition as Elasticsearch reads it: `Settings` built from the map,
/// so scalars become strings (`{"a": 1}` is `{"a": "1"}`), lists stay lists.
fn definition_settings(v: &Value) -> serde_json::Value {
    fn conv(v: &Value) -> serde_json::Value {
        match v {
            Value::Obj(m) => serde_json::Value::Object(m.iter().map(|(k, v)| (k.clone(), conv(v))).collect()),
            Value::Arr(a) => serde_json::Value::Array(a.iter().map(conv).collect()),
            Value::Null => serde_json::Value::Null,
            Value::Str(_) | Value::Bool(_) | Value::Num(_) => to_serde(&Value::Str(v.to_es_string())),
        }
    }
    conv(v)
}

/// The concrete index of a single-index operation (`concreteSingleIndex`): an index or an
/// alias with exactly one index; wildcards are not expanded.
pub fn concrete_single_index<'a>(st: &'a ClusterState, expr: &str, now_ms: i64) -> EsResult<&'a Arc<IndexMeta>> {
    let name = resolve::resolve_date_math(expr, now_ms)?;
    let found: Vec<&Arc<IndexMeta>> = if name == "_all" {
        st.metadata.indices.values().filter(|i| !i.is_hidden() && !i.is_closed()).collect()
    } else if let Some(i) = st.metadata.indices.get(&name) {
        vec![i]
    } else {
        let mut v: Vec<&Arc<IndexMeta>> = st.metadata.indices.values().filter(|i| i.aliases.contains_key(&name)).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        if v.len() > 1 {
            let names: Vec<&str> = v.iter().map(|i| i.name.as_str()).collect();
            return Err(EsError::illegal_argument(format!(
                "alias [{name}] has more than one index associated with it [{}], can't execute a single index op",
                names.join(", ")
            )));
        }
        v
    };
    match found.as_slice() {
        [] => Err(EsError::index_not_found(&name)),
        [one] => {
            if one.is_closed() {
                return Err(EsError::closed_index(&one.name, &one.uuid));
            }
            Ok(one)
        }
        _ => Err(EsError::illegal_argument("unable to return a single index as the index and options provided got resolved to multiple indices")),
    }
}

/// The analysis chain of an index (cached per index).
pub fn index_analysis(ctx: &Ctx, st: &ClusterState, idx: &IndexMeta) -> EsResult<Analysis> {
    ctx.cluster.index_analysis(st, idx)
}

/// What `field` of an `_analyze` request analyzes with (`getAnalyzer` for fields).
fn field_analyzer(analysis: &Analysis, mapping: &Mapping, ctx: &crate::mapping::MappingCtx, field: &str) -> EsResult<Option<Analyzer>> {
    let cant = || EsError::illegal_argument(format!("Can't process field [{field}], Analysis requests are only supported on tokenized fields"));
    if mapping.runtime.contains_key(field) {
        return Err(cant());
    }
    let mut found = mapping.lookup(field);
    if let Some(MapperRef::Alias(a)) = found {
        found = mapping.lookup(&a.path);
    }
    let f = match found {
        Some(MapperRef::Field(f)) => f,
        Some(_) => return Ok(None),
        None => {
            // Keys of a flattened field resolve to its keyed sub-field, which has no analyzer.
            for (i, _) in field.match_indices('.') {
                if let Some(MapperRef::Field(f)) = mapping.lookup(&field[..i])
                    && f.ty == "flattened"
                {
                    return Err(EsError::illegal_argument(format!("No analyzer configured for field {}._keyed", &field[..i])));
                }
            }
            return Ok(None);
        }
    };
    let by_name = |name: &str| {
        analysis
            .analyzer(name)
            .ok_or_else(|| EsError::illegal_argument(format!("No analyzer configured for field {field}")))
            .map(Some)
    };
    match f.ty {
        "text" | "search_as_you_type" | "match_only_text" => match f.effective("analyzer", ctx).as_str() {
            Some(name) => by_name(name),
            None => Ok(Some(analysis.default_analyzer())),
        },
        "keyword" => match f.effective("normalizer", ctx).as_str() {
            Some(n) => analysis
                .normalizer(n)
                .map(|n| Some(n.analyzer().clone()))
                .ok_or_else(|| EsError::illegal_argument(format!("No analyzer configured for field {field}"))),
            None => Ok(Analysis::global().analyzer("keyword")),
        },
        "flattened" => Ok(Analysis::global().analyzer("keyword")),
        _ => Err(cant()),
    }
}

fn limit(st_settings: &crate::meta::settings::Settings, key: &str, default: usize) -> usize {
    st_settings.get(key).and_then(|v| v.parse::<usize>().ok()).unwrap_or(default)
}

pub fn analyze(ctx: &Ctx) -> EsResult<Reply> {
    let Some(body) = ctx.body()? else {
        return Err(EsError::parse("request body or source parameter is required"));
    };
    let parser = BodyParser { raw: ctx.raw_json() };
    let req = match &body {
        Value::Obj(m) => parser.parse(m)?,
        other => {
            let (l, c) = parser.loc(&[]);
            return Err(EsError::x_content_parse(l, c, format!("[analyze_request] Expected START_OBJECT but was: {}", token_name(other))));
        }
    };
    let index = ctx.path_param("index").or_else(|| ctx.query("index"));
    let st = ctx.state();
    let out = match index {
        None => {
            req.validate(false).map_err(es_error)?;
            Analysis::global().analyze(&req, None, AnalyzeLimits::default()).map_err(es_error)?
        }
        Some(expr) => {
            req.validate(true).map_err(es_error)?;
            let idx = concrete_single_index(&st, expr, ctx.now_ms)?;
            let analysis = index_analysis(ctx, &st, idx)?;
            let field = match (&req.field, &req.analyzer, &req.normalizer, &req.tokenizer) {
                (Some(f), None, None, None) if req.filter.is_empty() && req.char_filter.is_empty() => {
                    field_analyzer(&analysis, &idx.mapping, &idx.mapping_ctx(), f)?
                }
                _ => None,
            };
            let limits = AnalyzeLimits {
                max_token_count: limit(&idx.settings, "index.analyze.max_token_count", 10_000),
                max_char_count: limit(&idx.settings, "index.analyze.max_char_count", 1_000_000),
            };
            analysis.analyze(&req, field.as_ref(), limits).map_err(es_error)?
        }
    };
    Reply::ok(analyze_output(&out))
}
