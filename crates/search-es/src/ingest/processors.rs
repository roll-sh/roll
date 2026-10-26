//! Ingest processors (`org.elasticsearch.ingest.common` and friends) over an [`IngestDoc`]:
//! each is compiled once from its configuration ([`Processor::compile`]) and applied per
//! document with Elasticsearch's semantics and error texts (`ignore_missing`, `target_field`,
//! templated field names and values, type conversions).

use super::doc::{IngestDoc, java_type};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use super::jregex::{self, JavaPattern, JavaReplace};
use std::sync::Arc;

/// What a processor did.
pub enum Flow {
    Continue,
    Drop,
}

/// A mustache template over the document (or a plain string).
#[derive(Clone)]
pub enum Template {
    Plain(String),
    Mustache(Arc<search_painless::mustache::Template>),
}

impl Template {
    pub fn new(s: &str) -> EsResult<Template> {
        if !s.contains("{{") {
            return Ok(Template::Plain(s.to_string()));
        }
        let t = search_painless::mustache::Template::compile(s, &[]).map_err(crate::scripting::script_error)?;
        Ok(Template::Mustache(Arc::new(t)))
    }

    pub fn render(&self, d: &IngestDoc) -> EsResult<String> {
        match self {
            Template::Plain(s) => Ok(s.clone()),
            Template::Mustache(t) => t.render_json(&d.template_model()).map_err(crate::scripting::script_error),
        }
    }
}

/// A templated value (strings render; lists and maps render their strings).
#[derive(Clone)]
pub enum ValueTemplate {
    Str(Template),
    List(Vec<ValueTemplate>),
    Map(Vec<(String, ValueTemplate)>),
    Other(Value),
}

impl ValueTemplate {
    pub fn new(v: &Value) -> EsResult<ValueTemplate> {
        Ok(match v {
            Value::Str(s) => ValueTemplate::Str(Template::new(s)?),
            Value::Arr(a) => ValueTemplate::List(a.iter().map(ValueTemplate::new).collect::<EsResult<_>>()?),
            Value::Obj(m) => ValueTemplate::Map(m.iter().map(|(k, v)| Ok((k.clone(), ValueTemplate::new(v)?))).collect::<EsResult<_>>()?),
            other => ValueTemplate::Other(other.clone()),
        })
    }

    pub fn render(&self, d: &IngestDoc) -> EsResult<Value> {
        Ok(match self {
            ValueTemplate::Str(t) => Value::Str(t.render(d)?),
            ValueTemplate::List(l) => Value::Arr(l.iter().map(|x| x.render(d)).collect::<EsResult<_>>()?),
            ValueTemplate::Map(m) => {
                let mut out = Map::new();
                for (k, v) in m {
                    out.insert(k.clone(), v.render(d)?);
                }
                Value::Obj(out)
            }
            ValueTemplate::Other(v) => v.clone(),
        })
    }
}

fn s(cfg: &Map, k: &str) -> Option<String> {
    cfg.get(k).map(|v| match v {
        Value::Str(s) => s.clone(),
        other => other.to_es_string(),
    })
}

fn b(cfg: &Map, k: &str, d: bool) -> bool {
    match cfg.get(k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Str(s)) => s == "true",
        _ => d,
    }
}

fn req(cfg: &Map, ptype: &str, tag: Option<&str>, k: &str) -> EsResult<String> {
    s(cfg, k).ok_or_else(|| config_error(ptype, tag, k, "required property is missing"))
}

/// `ConfigurationUtils.newConfigurationException`.
pub fn config_error(ptype: &str, tag: Option<&str>, prop: &str, msg: &str) -> EsError {
    let mut e = EsError::new(400, "parse_exception", format!("[{prop}] {msg}")).with_meta("property_name", prop).with_meta("processor_type", ptype);
    if let Some(t) = tag {
        e = e.with_meta("processor_tag", t);
    }
    e
}

/// A compiled processor.
#[derive(Clone)]
pub enum Kind {
    Set { field: Template, value: Option<ValueTemplate>, copy_from: Option<String>, overwrite: bool, ignore_empty: bool },
    Remove { fields: Vec<Template>, keep: Option<Vec<Template>>, ignore_missing: bool },
    Rename { field: Template, target: Template, ignore_missing: bool, overwrite: bool },
    Append { field: Template, value: Option<ValueTemplate>, copy_from: Option<String>, allow_duplicates: bool },
    Convert { field: String, target: String, ty: String, ignore_missing: bool },
    Case { field: String, target: String, ignore_missing: bool, op: u8 },
    Split { field: String, target: String, sep: Arc<LazyPattern>, ignore_missing: bool, preserve_trailing: bool },
    Join { field: String, target: String, sep: String },
    Gsub { field: String, target: String, pattern: Arc<JavaReplace>, ignore_missing: bool },
    Sort { field: String, target: String, desc: bool },
    Json { field: String, target: Option<String>, add_to_root: bool, conflict_replace: bool },
    Kv { field: String, target: Option<String>, field_split: Arc<LazyPattern>, value_split: Arc<LazyPattern>, value_split_text: String, include: Option<Vec<String>>, exclude: Option<Vec<String>>, ignore_missing: bool, prefix: Option<String>, trim_key: Option<Arc<JavaReplace>>, trim_value: Option<Arc<JavaReplace>>, strip_brackets: bool },
    Date { field: String, target: String, formats: Vec<String>, timezone: Option<Template>, locale: Option<Template>, output_format: String },
    DateIndexName { field: String, prefix: Template, rounding: String, formats: Vec<String>, timezone: Option<String>, locale: crate::time::DateLocale, index_format: String },
    Dissect { field: String, dissect: Arc<search_painless::dissect::Dissect>, ignore_missing: bool },
    Grok { field: String, groks: Vec<Arc<search_painless::grok::Grok>>, ignore_missing: bool, trace: bool },
    Fail { message: Template },
    Drop,
    Script { bound: crate::scripting::Bound },
    Foreach { field: String, processor: Box<super::pipeline::Compiled>, ignore_missing: bool },
    Pipeline { name: Template, ignore_missing: bool },
    DotExpander { field: String, path: Option<String>, overwrite: bool },
    HtmlStrip { field: String, target: String, ignore_missing: bool },
    UrlDecode { field: String, target: String, ignore_missing: bool },
    UriParts { field: String, target: String, keep_original: bool, remove_if_successful: bool, ignore_missing: bool },
    Bytes { field: String, target: String, ignore_missing: bool },
    Csv { field: String, targets: Vec<String>, sep: char, quote: char, trim: bool, empty_value: Option<Value>, ignore_missing: bool },
    Fingerprint { fields: Vec<String>, target: String, salt: String, method: String, ignore_missing: bool },
    Reroute { destination: Option<Template>, dataset: Vec<String>, namespace: Vec<String> },
    Terminate,
    /// `enrich`: a lookup in a loaded enrich index (`crate::xpack::enrich`).
    Enrich(Box<crate::xpack::enrich::Processor>),
    /// Accepted but needs data or licenses roll does not have (geoip, user_agent, …).
    Unsupported(String),
    /// `geo_grid`: a grid cell id to its geometry (and hierarchy).
    GeoGrid(Box<GeoGrid>),
}

fn target(cfg: &Map, field: &str) -> String {
    s(cfg, "target_field").unwrap_or_else(|| field.to_string())
}

/// `URLDecoder.decode(s, UTF_8)` (Java 21): `+` is a space, runs of `%XX` escapes decode as UTF-8
/// (malformed sequences become U+FFFD), and its error texts.
fn java_url_decode(s: &str) -> Result<String, String> {
    if !s.contains(['%', '+']) {
        return Ok(s.to_string());
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut bytes = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '+' => {
                out.push(' ');
                i += 1;
            }
            '%' => {
                bytes.clear();
                while i + 2 < chars.len() && chars[i] == '%' {
                    let mut v = 0u8;
                    for &c in &chars[i + 1..i + 3] {
                        let Some(d) = c.to_digit(16).filter(|_| c.is_ascii()) else {
                            return Err(format!("URLDecoder: Illegal hex characters in escape (%) pattern - not a hexadecimal digit: \"{c}\" = {}", c as u32));
                        };
                        v = v * 16 + d as u8;
                    }
                    bytes.push(v);
                    i += 3;
                }
                if i < chars.len() && chars[i] == '%' {
                    return Err("URLDecoder: Incomplete trailing escape (%) pattern".into());
                }
                out.push_str(&String::from_utf8_lossy(&bytes));
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// A Java pattern compiled when the processor is created whose syntax error, like
/// `String.split(regex)`'s, only surfaces when a document is processed.
pub type LazyPattern = Result<JavaPattern, EsError>;

fn lazy_pattern(p: &str) -> Arc<LazyPattern> {
    Arc::new(JavaPattern::compile(p).map_err(crate::analysis::es_error))
}

fn pattern_of(p: &LazyPattern) -> EsResult<&JavaPattern> {
    p.as_ref().map_err(Clone::clone)
}

/// `ConfigurationUtils.newConfigurationException(type, tag, null, cause)` for a Java exception
/// thrown while creating a processor: `ExceptionsHelper.convertToElastic` wraps it in a 500
/// `exception` named after the Java class, with the processor metadata.
pub fn creation_exception(ptype: &str, tag: Option<&str>, java_class: &str, cause: EsError) -> EsError {
    let mut e = EsError::new(500, "exception", format!("{java_class}: {}", cause.reason())).with_meta("processor_type", ptype);
    if let Some(t) = tag {
        e = e.with_meta("processor_tag", t);
    }
    e.caused_by(cause)
}

/// `KeyValueProcessor.buildSplitter`: patterns longer than `String.split`'s one-character fast
/// path compile with the processor; the others compile (and fail) per document.
fn kv_splitter(p: &str, ptype: &str, tag: Option<&str>) -> EsResult<Arc<LazyPattern>> {
    let units: Vec<u16> = p.encode_utf16().take(3).collect();
    let eager = units.len() > 2 || (units.len() == 2 && units[0] != u16::from(b'\\'));
    let lazy = lazy_pattern(p);
    if eager && let Err(e) = &*lazy {
        return Err(creation_exception(ptype, tag, "java.util.regex.PatternSyntaxException", e.clone()));
    }
    Ok(lazy)
}

/// `KeyValueProcessor.buildTrimmer`: `(^([<trim>]+))|([<trim>]+$)` removed.
fn kv_trimmer(trim: Option<String>, ptype: &str, tag: Option<&str>) -> EsResult<Option<Arc<JavaReplace>>> {
    let Some(t) = trim else { return Ok(None) };
    JavaReplace::new(&format!("(^([{t}]+))|([{t}]+$)"), "")
        .map(|r| Some(Arc::new(r)))
        .map_err(|e| creation_exception(ptype, tag, "java.util.regex.PatternSyntaxException", crate::analysis::es_error(e)))
}

/// `KeyValueProcessor.STRIP_BRACKETS`.
fn bracket_stripper() -> &'static JavaReplace {
    static P: std::sync::OnceLock<JavaReplace> = std::sync::OnceLock::new();
    P.get_or_init(|| JavaReplace::new("(^[\\(\\[<\"'])|([\\]\\)>\"']$)", "").expect("static pattern"))
}

impl Kind {
    pub fn compile(cluster: &crate::Cluster, ptype: &str, cfg: &Map, tag: Option<&str>) -> EsResult<Kind> {
        let field = || req(cfg, ptype, tag, "field");
        let im = b(cfg, "ignore_missing", false);
        Ok(match ptype {
            "set" => Kind::Set {
                field: Template::new(&field()?)?,
                value: cfg.get("value").map(ValueTemplate::new).transpose()?,
                copy_from: s(cfg, "copy_from"),
                overwrite: b(cfg, "override", true),
                ignore_empty: b(cfg, "ignore_empty_value", false),
            },
            "remove" => {
                let list = |k: &str| -> EsResult<Option<Vec<Template>>> {
                    match cfg.get(k) {
                        None => Ok(None),
                        Some(Value::Arr(a)) => Ok(Some(a.iter().map(|v| Template::new(&v.to_es_string())).collect::<EsResult<_>>()?)),
                        Some(v) => Ok(Some(vec![Template::new(&v.to_es_string())?])),
                    }
                };
                Kind::Remove { fields: list("field")?.unwrap_or_default(), keep: list("keep")?, ignore_missing: im }
            }
            "rename" => Kind::Rename { field: Template::new(&field()?)?, target: Template::new(&req(cfg, ptype, tag, "target_field")?)?, ignore_missing: im, overwrite: b(cfg, "override", false) },
            "append" => Kind::Append {
                field: Template::new(&field()?)?,
                value: cfg.get("value").map(ValueTemplate::new).transpose()?,
                copy_from: s(cfg, "copy_from"),
                allow_duplicates: b(cfg, "allow_duplicates", true),
            },
            "convert" => {
                let f = field()?;
                let ty = req(cfg, ptype, tag, "type")?;
                if !["integer", "long", "float", "double", "string", "boolean", "ip", "auto"].contains(&ty.to_lowercase().as_str()) {
                    return Err(config_error(ptype, tag, "type", &format!("type [{ty}] not supported, cannot convert field.")));
                }
                Kind::Convert { target: target(cfg, &f), field: f, ty: ty.to_lowercase(), ignore_missing: im }
            }
            "lowercase" | "uppercase" | "trim" => {
                let f = field()?;
                Kind::Case { target: target(cfg, &f), field: f, ignore_missing: im, op: match ptype { "lowercase" => 0, "uppercase" => 1, _ => 2 } }
            }
            "split" => {
                let f = field()?;
                let sep = req(cfg, ptype, tag, "separator")?;
                Kind::Split { target: target(cfg, &f), field: f, sep: lazy_pattern(&sep), ignore_missing: im, preserve_trailing: b(cfg, "preserve_trailing", false) }
            }
            "join" => {
                let f = field()?;
                Kind::Join { target: target(cfg, &f), field: f, sep: req(cfg, ptype, tag, "separator")? }
            }
            "gsub" => {
                let f = field()?;
                let p = req(cfg, ptype, tag, "pattern")?;
                let r = req(cfg, ptype, tag, "replacement")?;
                let pattern = JavaReplace::new(&p, &r)
                    .map_err(|e| config_error(ptype, tag, "pattern", &format!("Invalid regex pattern. {}", jregex::syntax_message(&e))))?;
                Kind::Gsub { target: target(cfg, &f), field: f, pattern: Arc::new(pattern), ignore_missing: im }
            }
            "sort" => {
                let f = field()?;
                let order = s(cfg, "order").unwrap_or_else(|| "asc".into());
                if order != "asc" && order != "desc" {
                    return Err(config_error(ptype, tag, "order", &format!("Sort direction [{order}] not recognized. Valid values are: [asc, desc]")));
                }
                Kind::Sort { target: target(cfg, &f), field: f, desc: order == "desc" }
            }
            "json" => {
                let f = field()?;
                let add = b(cfg, "add_to_root", false);
                let t = s(cfg, "target_field");
                if add && t.is_some() {
                    return Err(config_error(ptype, tag, "target_field", "Cannot set a target field while also setting `add_to_root` to true"));
                }
                Kind::Json { target: if add { None } else { Some(t.unwrap_or_else(|| f.clone())) }, field: f, add_to_root: add, conflict_replace: s(cfg, "add_to_root_conflict_strategy").as_deref() != Some("merge") }
            }
            "kv" => {
                let f = field()?;
                let list = |k: &str| -> Option<Vec<String>> {
                    cfg.get(k).map(|v| match v {
                        Value::Arr(a) => a.iter().map(|x| x.to_es_string()).collect(),
                        o => vec![o.to_es_string()],
                    })
                };
                let field_split = req(cfg, ptype, tag, "field_split")?;
                let value_split = req(cfg, ptype, tag, "value_split")?;
                Kind::Kv {
                    target: s(cfg, "target_field"),
                    field_split: kv_splitter(&field_split, ptype, tag)?,
                    value_split: kv_splitter(&value_split, ptype, tag)?,
                    value_split_text: value_split,
                    include: list("include_keys"),
                    exclude: list("exclude_keys"),
                    ignore_missing: im,
                    prefix: s(cfg, "prefix"),
                    trim_key: kv_trimmer(s(cfg, "trim_key"), ptype, tag)?,
                    trim_value: kv_trimmer(s(cfg, "trim_value"), ptype, tag)?,
                    strip_brackets: b(cfg, "strip_brackets", false),
                    field: f,
                }
            }
            "date" => {
                let f = field()?;
                let formats: Vec<String> = match cfg.get("formats") {
                    Some(Value::Arr(a)) => a.iter().map(|v| v.to_es_string()).collect(),
                    _ => return Err(config_error(ptype, tag, "formats", "required property is missing")),
                };
                Kind::Date {
                    target: s(cfg, "target_field").unwrap_or_else(|| "@timestamp".into()),
                    field: f,
                    formats,
                    timezone: s(cfg, "timezone").map(|t| Template::new(&t)).transpose()?,
                    locale: s(cfg, "locale").map(|t| Template::new(&t)).transpose()?,
                    output_format: s(cfg, "output_format").unwrap_or_else(|| "yyyy-MM-dd'T'HH:mm:ss.SSSXXX".into()),
                }
            }
            "date_index_name" => {
                let f = field()?;
                let rounding = req(cfg, ptype, tag, "date_rounding")?;
                if !["y", "M", "w", "d", "h", "m", "s"].contains(&rounding.as_str()) {
                    return Err(config_error(ptype, tag, "date_rounding", "illegal date_rounding"));
                }
                Kind::DateIndexName {
                    field: f,
                    prefix: Template::new(&s(cfg, "index_name_prefix").unwrap_or_default())?,
                    rounding,
                    formats: match cfg.get("date_formats") {
                        Some(Value::Arr(a)) => a.iter().map(|v| v.to_es_string()).collect(),
                        _ => vec!["yyyy-MM-dd'T'HH:mm:ss.SSSXX".into()],
                    },
                    timezone: s(cfg, "timezone"),
                    locale: match s(cfg, "locale") {
                        None => crate::time::DateLocale::English,
                        Some(lt) if is_language_tag(&lt) => crate::time::DateLocale::parse(&lt),
                        Some(lt) => return Err(creation_exception(ptype, tag, "java.lang.IllegalArgumentException", EsError::illegal_argument(format!("Invalid language tag specified: {lt}")))),
                    },
                    index_format: s(cfg, "index_name_format").unwrap_or_else(|| "yyyy-MM-dd".into()),
                }
            }
            "dissect" => {
                let f = field()?;
                let p = req(cfg, ptype, tag, "pattern")?;
                let d = search_painless::dissect::Dissect::new(&p, s(cfg, "append_separator").as_deref()).map_err(|e| {
                    // `DissectException.PatternParse` is not an Elasticsearch exception.
                    let cause = EsError::java(400, e.kind, e.message.clone());
                    creation_exception(ptype, tag, "org.elasticsearch.dissect.DissectException$PatternParse", cause)
                })?;
                Kind::Dissect { field: f, dissect: Arc::new(d), ignore_missing: im }
            }
            "grok" => {
                let f = field()?;
                let patterns: Vec<String> = match cfg.get("patterns") {
                    Some(Value::Arr(a)) if !a.is_empty() => a.iter().map(|v| v.to_es_string()).collect(),
                    Some(Value::Arr(_)) => return Err(config_error(ptype, tag, "patterns", "List of patterns must not be empty")),
                    _ => return Err(config_error(ptype, tag, "patterns", "required property is missing")),
                };
                let ecs = s(cfg, "ecs_compatibility").unwrap_or_else(|| "disabled".into());
                let base = if ecs == "v1" { search_painless::grok::PatternBank::ecs_v1() } else { search_painless::grok::PatternBank::legacy() };
                let bank = match cfg.get("pattern_definitions") {
                    Some(Value::Obj(defs)) => {
                        let extra: Vec<(String, String)> = defs.iter().map(|(k, v)| (k.clone(), v.to_es_string())).collect();
                        base.extend_with(&extra).map_err(|e| EsError::new(400, e.kind, e.message.clone()))?
                    }
                    _ => base.clone(),
                };
                let watchdog = grok_watchdog(cluster, "ingest.grok.watchdog.max_execution_time");
                let mut groks = Vec::new();
                for p in &patterns {
                    let g = search_painless::grok::Grok::new(&bank, p).map_err(|e| {
                        EsError::new(400, "parse_exception", format!("[patterns] Invalid regex pattern found in: {patterns:?}. {}", e.message))
                            .with_meta("property_name", "patterns")
                            .with_meta("processor_type", "grok")
                    })?;
                    groks.push(Arc::new(g.with_max_execution_time(watchdog)));
                }
                Kind::Grok { field: f, groks, ignore_missing: im, trace: b(cfg, "trace_match", false) }
            }
            "fail" => Kind::Fail { message: Template::new(&req(cfg, ptype, tag, "message")?)? },
            "drop" => Kind::Drop,
            "terminate" => Kind::Terminate,
            "script" => {
                let script = match (cfg.get("source"), cfg.get("id")) {
                    (Some(_), Some(_)) => return Err(EsError::new(400, "parse_exception", "Only one of [id] or [source] may be configured")),
                    (None, None) => return Err(EsError::new(400, "parse_exception", "Need [id] or [source] for script processor")),
                    _ => {
                        let mut m = Map::new();
                        for k in ["source", "id", "lang", "params", "options"] {
                            if let Some(v) = cfg.get(k) {
                                m.insert(k.into(), v.clone());
                            }
                        }
                        let v = Value::Obj(m);
                        let mut p = crate::query::XParser::from_value(&v);
                        p.next_token();
                        crate::query::script::Script::parse(&mut p)?
                    }
                };
                let st = cluster.state();
                Kind::Script { bound: cluster.scripts.compile(&st, &script, search_painless::Context::Ingest)? }
            }
            "foreach" => {
                let f = field()?;
                let Some(Value::Obj(p)) = cfg.get("processor") else {
                    return Err(config_error(ptype, tag, "processor", "required property is missing"));
                };
                let compiled = super::pipeline::compile_list(cluster, &Value::Arr(vec![Value::Obj(p.clone())]))?;
                let one = compiled.into_iter().next().ok_or_else(|| config_error(ptype, tag, "processor", "required property is missing"))?;
                Kind::Foreach { field: f, processor: Box::new(one), ignore_missing: im }
            }
            "pipeline" => Kind::Pipeline { name: Template::new(&req(cfg, ptype, tag, "name")?)?, ignore_missing: b(cfg, "ignore_missing_pipeline", false) },
            "dot_expander" => {
                let f = field()?;
                if !f.contains('.') && f != "*" {
                    return Err(config_error(ptype, tag, "field", "field does not contain a dot and is not a wildcard"));
                }
                Kind::DotExpander { field: f, path: s(cfg, "path"), overwrite: b(cfg, "override", false) }
            }
            "html_strip" => {
                let f = field()?;
                Kind::HtmlStrip { target: target(cfg, &f), field: f, ignore_missing: im }
            }
            "urldecode" => {
                let f = field()?;
                Kind::UrlDecode { target: target(cfg, &f), field: f, ignore_missing: im }
            }
            "uri_parts" => {
                let f = field()?;
                Kind::UriParts { target: s(cfg, "target_field").unwrap_or_else(|| "url".into()), field: f, keep_original: b(cfg, "keep_original", true), remove_if_successful: b(cfg, "remove_if_successful", false), ignore_missing: im }
            }
            "bytes" => {
                let f = field()?;
                Kind::Bytes { target: target(cfg, &f), field: f, ignore_missing: im }
            }
            "csv" => {
                let f = field()?;
                let targets: Vec<String> = match cfg.get("target_fields") {
                    Some(Value::Arr(a)) => a.iter().map(|v| v.to_es_string()).collect(),
                    Some(v) => vec![v.to_es_string()],
                    None => return Err(config_error(ptype, tag, "target_fields", "required property is missing")),
                };
                let one = |k: &str, d: char| -> EsResult<char> {
                    match s(cfg, k) {
                        None => Ok(d),
                        Some(v) if v.chars().count() == 1 => Ok(v.chars().next().unwrap()),
                        Some(_) => Err(config_error(ptype, tag, k, &format!("{k} has to be single character like \" or '"))),
                    }
                };
                Kind::Csv { field: f, targets, sep: one("separator", ',')?, quote: one("quote", '"')?, trim: b(cfg, "trim", false), empty_value: cfg.get("empty_value").cloned(), ignore_missing: im }
            }
            "fingerprint" => {
                let fields: Vec<String> = match cfg.get("fields") {
                    Some(Value::Arr(a)) if !a.is_empty() => a.iter().map(|v| v.to_es_string()).collect(),
                    _ => return Err(config_error(ptype, tag, "fields", "must specify at least one field")),
                };
                let method = s(cfg, "method").unwrap_or_else(|| "SHA-1".into());
                if !["MD5", "SHA-1", "SHA-256", "SHA-512", "MurmurHash3"].contains(&method.as_str()) {
                    return Err(config_error(ptype, tag, "method", &format!("[{method}] must be one of the supported hash methods [MD5, SHA-1, SHA-256, SHA-512, MurmurHash3]")));
                }
                Kind::Fingerprint { fields, target: s(cfg, "target_field").unwrap_or_else(|| "fingerprint".into()), salt: s(cfg, "salt").unwrap_or_default(), method, ignore_missing: im }
            }
            "reroute" => {
                let list = |k: &str| -> Vec<String> {
                    match cfg.get(k) {
                        Some(Value::Arr(a)) => a.iter().map(|v| v.to_es_string()).collect(),
                        Some(v) => vec![v.to_es_string()],
                        None => Vec::new(),
                    }
                };
                let dest = s(cfg, "destination");
                if dest.is_some() && (cfg.contains_key("dataset") || cfg.contains_key("namespace")) {
                    return Err(config_error(ptype, tag, "destination", "can only be set if dataset and namespace are not set"));
                }
                Kind::Reroute { destination: dest.map(|d| Template::new(&d)).transpose()?, dataset: list("dataset"), namespace: list("namespace") }
            }
            "enrich" => Kind::Enrich(Box::new(crate::xpack::enrich::compile(cluster, cfg, tag)?)),
            "geo_grid" => {
                let f = field()?;
                let tile = req(cfg, ptype, tag, "tile_type")?;
                let grid = match tile.to_lowercase().as_str() {
                    "geohash" => GridType::Geohash,
                    "geotile" => GridType::Geotile,
                    "geohex" => GridType::Geohex,
                    _ => return Err(config_error(ptype, tag, "tile_type", &format!("illegal value [{tile}], valid values are [GEOHASH, GEOTILE, GEOHEX]"))),
                };
                let format = s(cfg, "target_format").unwrap_or_else(|| "GEOJSON".into());
                let wkt = match format.to_lowercase().trim() {
                    "geojson" => false,
                    "wkt" => true,
                    _ => return Err(config_error(ptype, tag, "target_format", &format!("illegal value [{format}], valid values are [WKT, GEOJSON]"))),
                };
                let opt = |k: &str| s(cfg, k).filter(|v| !v.is_empty());
                Kind::GeoGrid(Box::new(GeoGrid {
                    target: s(cfg, "target_field").unwrap_or_else(|| f.clone()),
                    field: f,
                    parent: opt("parent_field"),
                    children: opt("children_field"),
                    non_children: opt("non_children_field"),
                    precision: opt("precision_field"),
                    ignore_missing: b(cfg, "ignore_missing", false),
                    grid,
                    wkt,
                }))
            }
            other => Kind::Unsupported(other.to_string()),
        })
    }
}

fn str_field<'a>(d: &'a IngestDoc, field: &str, ptype: &str) -> EsResult<Option<&'a str>> {
    match d.get_opt(field) {
        None => Ok(None),
        Some(Value::Str(s)) => Ok(Some(s)),
        Some(Value::Null) => Ok(None),
        Some(other) => Err(EsError::illegal_argument(format!("field [{field}] of type [{}] cannot be cast to [java.lang.String]", java_type(other))).with_meta("_ptype", ptype)),
    }
}

/// Fetches a field for a string processor, honoring `ignore_missing`.
fn input<'a>(d: &'a IngestDoc, field: &str, ignore_missing: bool) -> EsResult<Option<&'a Value>> {
    match d.get_opt(field) {
        Some(Value::Null) | None if ignore_missing => Ok(None),
        Some(Value::Null) => Err(EsError::illegal_argument(format!("field [{field}] is null, cannot process it."))),
        None => d.get(field).map(Some),
        Some(v) => Ok(Some(v)),
    }
}

fn convert_value(v: &Value, ty: &str, field: &str) -> EsResult<Value> {
    let text = match v {
        Value::Str(s) => s.clone(),
        other => other.to_es_string(),
    };
    // Java's parse methods fail with a `NumberFormatException` cause.
    let fail = |t: &str| {
        let nfe = if text.trim().is_empty() && matches!(t, "float" | "double") { "empty String".to_string() } else { format!("For input string: \"{text}\"") };
        EsError::illegal_argument(format!("unable to convert [{text}] to {t}")).caused_by(EsError::java(400, "number_format_exception", nfe))
    };
    Ok(match ty {
        "integer" => {
            let t = text.trim();
            let parsed = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) { i64::from_str_radix(h, 16).ok() } else { t.parse::<i64>().ok() };
            match parsed {
                Some(i) if i >= i32::MIN as i64 && i <= i32::MAX as i64 => Value::from(i),
                _ => return Err(fail("integer")),
            }
        }
        "long" => {
            let t = text.trim();
            let parsed = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) { i64::from_str_radix(h, 16).ok() } else { t.parse::<i64>().ok() };
            Value::from(parsed.ok_or_else(|| fail("long"))?)
        }
        "float" => Value::Num(Number::Float(crate::fetch::java_float_as_double(text.trim().parse::<f32>().map_err(|_| fail("float"))?))),
        "double" => Value::Num(Number::Float(text.trim().parse::<f64>().map_err(|_| fail("double"))?)),
        "string" => Value::Str(text),
        "boolean" => match text.to_lowercase().as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(EsError::illegal_argument(format!("[{text}] is not a boolean value, cannot convert to boolean"))),
        },
        "ip" => match crate::document::values::parse_ip(&text) {
            Some(_) => Value::Str(text),
            None => return Err(EsError::illegal_argument(format!("'{text}' is not an IP string literal."))),
        },
        _ => {
            // auto
            if let Value::Str(s) = v {
                if let Ok(i) = s.parse::<i32>() {
                    return Ok(Value::from(i as i64));
                }
                if let Ok(i) = s.parse::<i64>() {
                    return Ok(Value::from(i));
                }
                if let Ok(f) = s.parse::<f32>()
                    && f.is_finite()
                    && (f as f64).to_string().parse::<f64>().is_ok()
                    && s.parse::<f64>().map(|d| (d as f32) as f64 == d).unwrap_or(false)
                {
                    return Ok(Value::Num(Number::Float(f as f64)));
                }
                if let Ok(d) = s.parse::<f64>() {
                    return Ok(Value::Num(Number::Float(d)));
                }
                match s.to_lowercase().as_str() {
                    "true" => return Ok(Value::Bool(true)),
                    "false" => return Ok(Value::Bool(false)),
                    _ => {}
                }
            }
            let _ = field;
            v.clone()
        }
    })
}

fn map_strings(v: &Value, field: &str, f: &dyn Fn(&str) -> EsResult<String>) -> EsResult<Value> {
    match v {
        Value::Str(s) => Ok(Value::Str(f(s)?)),
        Value::Arr(a) => {
            let mut out = Vec::with_capacity(a.len());
            for x in a {
                match x {
                    Value::Str(s) => out.push(Value::Str(f(s)?)),
                    other => return Err(EsError::illegal_argument(format!("value [{}] of type [{}] in list field [{field}] cannot be cast to [java.lang.String]", other.to_es_string(), java_type(other)))),
                }
            }
            Ok(Value::Arr(out))
        }
        other => Err(EsError::illegal_argument(format!("field [{field}] of type [{}] cannot be cast to [java.lang.String]", java_type(other)))),
    }
}

fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push('\n');
            }
            _ if in_tag => {}
            '&' => {
                let mut ent = String::new();
                while let Some(&n) = chars.peek() {
                    if n == ';' || ent.len() > 8 {
                        break;
                    }
                    ent.push(n);
                    chars.next();
                }
                if chars.peek() == Some(&';') {
                    chars.next();
                    let rep = match ent.as_str() {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        "nbsp" => "\u{a0}",
                        _ => "",
                    };
                    if rep.is_empty() {
                        out.push('&');
                        out.push_str(&ent);
                        out.push(';');
                    } else {
                        out.push_str(rep);
                    }
                } else {
                    out.push('&');
                    out.push_str(&ent);
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// `ByteSizeValue.parseBytesSizeValue` to bytes.
fn bytes_of(s: &str) -> EsResult<i64> {
    crate::units::ByteSize::parse(s, "Ingest Field").map(|b| b.bytes())
}

/// Applies one processor.
pub fn run(cluster: &crate::Cluster, k: &Kind, d: &mut IngestDoc, depth: usize) -> EsResult<Flow> {
    match k {
        Kind::Set { field, value, copy_from, overwrite, ignore_empty } => {
            let path = field.render(d)?;
            let v = match (value, copy_from) {
                (Some(v), _) => v.render(d)?,
                (None, Some(c)) => d.get(c)?.clone(),
                _ => Value::Null,
            };
            if *ignore_empty && (v.is_null() || v.as_str() == Some("")) {
                return Ok(Flow::Continue);
            }
            if *overwrite || !d.has(&path) || d.get_opt(&path).is_some_and(Value::is_null) {
                d.set(&path, v)?;
            }
        }
        Kind::Remove { fields, keep, ignore_missing } => {
            if let Some(keep) = keep {
                let keep: Vec<String> = keep.iter().map(|t| t.render(d)).collect::<EsResult<_>>()?;
                let names: Vec<String> = d.source.keys().cloned().collect();
                for n in names {
                    if !keep.iter().any(|k| k == &n || k.starts_with(&format!("{n}."))) {
                        d.source.shift_remove(&n);
                    }
                }
            } else {
                for t in fields {
                    let p = t.render(d)?;
                    if p == "_index" || p == "_version" || p == "_version_type" {
                        return Err(EsError::illegal_argument(format!("cannot remove metadata [{p}]")));
                    }
                    match d.remove(&p) {
                        Ok(_) => {}
                        Err(_) if *ignore_missing => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        Kind::Rename { field, target, ignore_missing, overwrite } => {
            let f = field.render(d)?;
            let t = target.render(d)?;
            if !d.has(&f) {
                if *ignore_missing {
                    return Ok(Flow::Continue);
                }
                return Err(EsError::illegal_argument(format!("field [{f}] doesn't exist")));
            }
            if d.has(&t) && !overwrite {
                return Err(EsError::illegal_argument(format!("field [{t}] already exists")));
            }
            let v = d.remove(&f)?;
            if let Err(e) = d.set(&t, v.clone()) {
                d.set(&f, v)?;
                return Err(e);
            }
        }
        Kind::Append { field, value, copy_from, allow_duplicates } => {
            let path = field.render(d)?;
            let v = match (value, copy_from) {
                (Some(v), _) => v.render(d)?,
                (None, Some(c)) => d.get(c)?.clone(),
                _ => Value::Null,
            };
            d.append(&path, v, *allow_duplicates)?;
        }
        Kind::Convert { field, target, ty, ignore_missing } => {
            let Some(v) = input(d, field, *ignore_missing)?.cloned() else { return Ok(Flow::Continue) };
            let out = match &v {
                Value::Arr(a) => Value::Arr(a.iter().map(|x| convert_value(x, ty, field)).collect::<EsResult<_>>()?),
                other => convert_value(other, ty, field)?,
            };
            d.set(target, out)?;
        }
        Kind::Case { field, target, ignore_missing, op } => {
            let Some(v) = input(d, field, *ignore_missing)?.cloned() else { return Ok(Flow::Continue) };
            let out = map_strings(&v, field, &|s| {
                Ok(match op {
                    0 => s.to_lowercase(),
                    1 => s.to_uppercase(),
                    _ => s.trim().to_string(),
                })
            })?;
            d.set(target, out)?;
        }
        Kind::Split { field, target, sep, ignore_missing, preserve_trailing } => {
            let Some(v) = input(d, field, *ignore_missing)?.cloned() else { return Ok(Flow::Continue) };
            let Value::Str(s) = &v else {
                return Err(EsError::illegal_argument(format!("field [{field}] of type [{}] cannot be cast to [java.lang.String]", java_type(&v))));
            };
            let parts = pattern_of(sep)?.split(s, if *preserve_trailing { -1 } else { 0 })?;
            d.set(target, Value::Arr(parts.into_iter().map(Value::Str).collect()))?;
        }
        Kind::Join { field, target, sep } => {
            let v = d.get(field)?.clone();
            let Value::Arr(a) = &v else {
                return Err(EsError::illegal_argument(format!("field [{field}] of type [{}] cannot be cast to [java.util.List]", java_type(&v))));
            };
            let parts: Vec<String> = a.iter().map(|x| x.to_es_string()).collect();
            d.set(target, Value::Str(parts.join(sep)))?;
        }
        Kind::Gsub { field, target, pattern, ignore_missing } => {
            let Some(v) = input(d, field, *ignore_missing)?.cloned() else { return Ok(Flow::Continue) };
            let out = map_strings(&v, field, &|s| pattern.replace_all(s))?;
            d.set(target, out)?;
        }
        Kind::Sort { field, target, desc } => {
            let v = d.get(field)?.clone();
            let Value::Arr(mut a) = v else {
                return Err(EsError::illegal_argument(format!("field [{field}] of type [{}] cannot be cast to [java.util.List]", java_type(&v))));
            };
            a.sort_by(|x, y| match (x, y) {
                (Value::Num(p), Value::Num(q)) => p.as_f64().total_cmp(&q.as_f64()),
                (Value::Str(p), Value::Str(q)) => p.cmp(q),
                _ => x.to_es_string().cmp(&y.to_es_string()),
            });
            if *desc {
                a.reverse();
            }
            d.set(target, Value::Arr(a))?;
        }
        Kind::Json { field, target, add_to_root, conflict_replace } => {
            let v = d.get(field)?.clone();
            let text = match &v {
                Value::Str(s) => s.clone(),
                other => other.to_json(),
            };
            let parsed = crate::json::parse(text.as_bytes()).map_err(|e| EsError::illegal_argument(e.to_string()))?.unwrap_or(Value::Null);
            if *add_to_root {
                let Value::Obj(m) = parsed else {
                    return Err(EsError::illegal_argument("cannot add non-map fields to root of document"));
                };
                for (k, x) in m {
                    match (d.source.get_mut(&k), x, *conflict_replace) {
                        (Some(Value::Obj(old)), Value::Obj(new), false) => {
                            for (kk, vv) in new {
                                old.insert(kk, vv);
                            }
                        }
                        (_, x, _) => {
                            d.source.insert(k, x);
                        }
                    }
                }
            } else if let Some(t) = target {
                d.set(t, parsed)?;
            }
        }
        Kind::Kv { field, target, field_split, value_split, value_split_text, include, exclude, ignore_missing, prefix, trim_key, trim_value, strip_brackets } => {
            let Some(v) = input(d, field, *ignore_missing)?.cloned() else { return Ok(Flow::Continue) };
            let s = v.to_es_string();
            let (field_split, value_split) = (pattern_of(field_split)?, pattern_of(value_split)?);
            let mut pairs: Vec<(String, String)> = Vec::new();
            for part in field_split.split(&s, 0)? {
                let kv = value_split.split(&part, 2)?;
                let [k, val] = <[String; 2]>::try_from(kv).map_err(|_| EsError::illegal_argument(format!("field [{field}] does not contain value_split [{value_split_text}]")))?;
                let k = match trim_key {
                    Some(t) => t.replace_all(&k)?,
                    None => k,
                };
                let val = if *strip_brackets { bracket_stripper().replace_all(&val)? } else { val };
                let val = match trim_value {
                    Some(t) => t.replace_all(&val)?,
                    None => val,
                };
                if include.as_ref().is_some_and(|i| !i.contains(&k)) || exclude.as_ref().is_some_and(|e| e.contains(&k)) {
                    continue;
                }
                pairs.push((format!("{}{k}", prefix.as_deref().unwrap_or("")), val));
            }
            for (k, val) in pairs {
                let path = match target {
                    Some(t) => format!("{t}.{k}"),
                    None => k,
                };
                d.append(&path, Value::Str(val), true)?;
                // A single value stays a scalar.
                if let Some(Value::Arr(a)) = d.get_opt(&path).cloned()
                    && a.len() == 1
                {
                    d.set(&path, a[0].clone())?;
                }
            }
        }
        Kind::Date { field, target, formats, timezone, locale, output_format } => {
            let v = d.get(field)?.clone();
            let text = v.to_es_string();
            let unable = |e: EsError| EsError::illegal_argument(format!("unable to parse date [{text}]")).caused_by(e);
            // The zone and locale templates render once per document; their failures are
            // the date's (`DateProcessor.execute`).
            let zone = match timezone {
                Some(t) => Some(crate::time::Zone::parse(&t.render(d)?).map_err(|e| unable(zone_error(e)))?),
                None => None,
            };
            let loc = match locale {
                Some(t) => crate::time::DateLocale::parse(&crate::mapping::params::parse_locale_checked(&t.render(d)?).map_err(unable)?),
                None => crate::time::DateLocale::English,
            };
            // The first failure is the cause, later ones are suppressed by it
            // (`ExceptionsHelper.useOrSuppress`).
            let mut errors = Vec::new();
            let mut parsed = None;
            for f in formats {
                match parse_ingest_date(f, &text, zone.as_ref(), loc) {
                    Ok(i) => {
                        parsed = Some(i);
                        break;
                    }
                    Err(e) => errors.push(e),
                }
            }
            let Some(i) = parsed else {
                return Err(unable(first_suppressing(errors)));
            };
            // The output format prints in `Locale.ROOT` (`DateFormatter.forPattern`).
            let out = crate::time::DateFormatter::new(output_format).map_err(EsError::illegal_argument)?;
            d.set(target, Value::Str(out.format(i, zone.as_ref().unwrap_or(&crate::time::Zone::UTC))))?;
        }
        Kind::DateIndexName { field, prefix, rounding, formats, timezone, locale, index_format } => {
            let v = d.get(field)?.clone();
            let text = v.to_es_string();
            let zone = match timezone {
                Some(t) => crate::time::Zone::parse(t).map_err(EsError::illegal_argument)?,
                None => crate::time::Zone::UTC,
            };
            // Every format is tried and the last that parses wins (no early exit in
            // `DateIndexNameProcessor.execute`).
            let mut parsed = None;
            let mut errors = Vec::new();
            for f in formats {
                match parse_ingest_date(f, &text, Some(&zone), *locale) {
                    Ok(i) => parsed = Some(i),
                    Err(e) => errors.push(e),
                }
            }
            let i = parsed.ok_or_else(|| EsError::illegal_argument(format!("unable to parse date [{text}]")).caused_by(first_suppressing(errors)))?;
            let p = prefix.render(d)?;
            // The date prints in the index name format (`Locale.ROOT`, the processor's zone);
            // date math rounds it when the index name resolves.
            let shown = crate::time::DateFormatter::new(index_format).map_err(EsError::illegal_argument)?.format(i, &zone);
            let name = format!("<{p}{{{shown}||/{rounding}{{{index_format}|{}}}}}>", zone_name(&zone));
            d.meta.insert("_index".into(), Value::Str(name));
        }
        Kind::Dissect { field, dissect, ignore_missing } => {
            let Some(v) = input(d, field, *ignore_missing)?.cloned() else { return Ok(Flow::Continue) };
            let Value::Str(s) = &v else {
                return Err(EsError::illegal_argument(format!("field [{field}] of type [{}] cannot be cast to [java.lang.String]", java_type(&v))));
            };
            let Some(m) = dissect.parse(Some(s)) else {
                return Err(EsError::new(400, "dissect_exception", format!("Unable to find match for dissect pattern: {} against source: {s}", dissect.pattern())));
            };
            let mut keys: Vec<(String, String)> = m.into_iter().collect();
            keys.sort();
            for (k, val) in keys {
                d.set(&k, Value::Str(val))?;
            }
        }
        Kind::Grok { field, groks, ignore_missing, trace } => {
            let Some(v) = input(d, field, *ignore_missing)?.cloned() else { return Ok(Flow::Continue) };
            let Value::Str(s) = &v else {
                return Err(EsError::illegal_argument(format!("field [{field}] of type [{}] cannot be cast to [java.lang.String]", java_type(&v))));
            };
            for (i, g) in groks.iter().enumerate() {
                let caps = g.captures(s).map_err(grok_match_error)?;
                if let Some(caps) = caps {
                    for (k, val) in caps {
                        d.set(&k, grok_value(&val))?;
                    }
                    if *trace {
                        d.ingest.insert("_grok_match_index".into(), Value::Str(i.to_string()));
                    }
                    return Ok(Flow::Continue);
                }
            }
            return Err(EsError::illegal_argument("Provided Grok expressions do not match field value: [".to_string() + s + "]"));
        }
        Kind::Fail { message } => {
            return Err(EsError::new(500, "fail_processor_exception", message.render(d)?));
        }
        Kind::Drop => return Ok(Flow::Drop),
        Kind::Terminate => {}
        Kind::Script { bound } => super::pipeline::run_script(bound, d)?,
        Kind::Foreach { field, processor, ignore_missing } => {
            let v = match d.get_opt(field) {
                None | Some(Value::Null) if *ignore_missing => return Ok(Flow::Continue),
                None => return Err(EsError::illegal_argument(format!("field [{field}] not present as part of path [{field}]"))),
                Some(v) => v.clone(),
            };
            match v {
                Value::Arr(a) => {
                    let mut out = Vec::with_capacity(a.len());
                    for item in a {
                        d.ingest.insert("_value".into(), item);
                        super::pipeline::run_one(cluster, processor, d, depth)?;
                        out.push(d.ingest.shift_remove("_value").unwrap_or(Value::Null));
                    }
                    d.set(field, Value::Arr(out))?;
                }
                Value::Obj(m) => {
                    let mut out = Map::new();
                    for (k, item) in m {
                        d.ingest.insert("_key".into(), Value::Str(k.clone()));
                        d.ingest.insert("_value".into(), item);
                        super::pipeline::run_one(cluster, processor, d, depth)?;
                        let nk = d.ingest.shift_remove("_key").map(|v| v.to_es_string()).unwrap_or(k);
                        out.insert(nk, d.ingest.shift_remove("_value").unwrap_or(Value::Null));
                    }
                    d.set(field, Value::Obj(out))?;
                }
                other => return Err(EsError::illegal_argument(format!("field [{field}] of type [{}] cannot be cast to a list or map", java_type(&other)))),
            }
        }
        Kind::Pipeline { name, ignore_missing } => {
            let n = name.render(d)?;
            return super::pipeline::run_named(cluster, &n, d, depth + 1, *ignore_missing);
        }
        Kind::DotExpander { field, path, overwrite } => {
            let map: &mut Map = match path {
                Some(p) => match d.get_opt(p) {
                    Some(Value::Obj(_)) => {
                        let mut v = d.get(p)?.clone();
                        if let Value::Obj(m) = &mut v {
                            expand_dots(m, field, *overwrite)?;
                        }
                        d.set(p, v)?;
                        return Ok(Flow::Continue);
                    }
                    _ => return Ok(Flow::Continue),
                },
                None => &mut d.source,
            };
            expand_dots(map, field, *overwrite)?;
        }
        Kind::HtmlStrip { field, target, ignore_missing } => {
            let Some(v) = input(d, field, *ignore_missing)?.cloned() else { return Ok(Flow::Continue) };
            let out = map_strings(&v, field, &|s| Ok(strip_html(s)))?;
            d.set(target, out)?;
        }
        Kind::UrlDecode { field, target, ignore_missing } => {
            let Some(v) = input(d, field, *ignore_missing)?.cloned() else { return Ok(Flow::Continue) };
            let out = map_strings(&v, field, &|s| java_url_decode(s).map_err(EsError::illegal_argument))?;
            d.set(target, out)?;
        }
        Kind::UriParts { field, target, keep_original, remove_if_successful, ignore_missing } => {
            let Some(v) = input(d, field, *ignore_missing)?.cloned() else { return Ok(Flow::Continue) };
            let s = v.to_es_string();
            let parts = search_painless::es::uri::parse_uri(&s).or_else(|| search_painless::es::uri::parse_url(&s)).ok_or_else(|| EsError::illegal_argument(format!("unable to parse URI [{s}]")))?;
            let mut m = Map::new();
            if let Some(x) = &parts.scheme {
                m.insert("scheme".into(), Value::Str(x.clone()));
            }
            if let Some(x) = &parts.host {
                m.insert("domain".into(), Value::Str(x.clone()));
            }
            if parts.port >= 0 {
                m.insert("port".into(), Value::from(parts.port as i64));
            }
            if let Some(x) = &parts.path {
                m.insert("path".into(), Value::Str(x.clone()));
                if let Some(ext) = x.rsplit('/').next().and_then(|f| f.rsplit_once('.')).map(|(_, e)| e) {
                    m.insert("extension".into(), Value::Str(ext.to_string()));
                }
            }
            if let Some(x) = &parts.query {
                m.insert("query".into(), Value::Str(x.clone()));
            }
            if let Some(x) = &parts.fragment {
                m.insert("fragment".into(), Value::Str(x.clone()));
            }
            if let Some(x) = &parts.user_info {
                m.insert("user_info".into(), Value::Str(x.clone()));
                if let Some((u, p)) = x.split_once(':') {
                    m.insert("username".into(), Value::Str(u.to_string()));
                    m.insert("password".into(), Value::Str(p.to_string()));
                } else {
                    m.insert("username".into(), Value::Str(x.clone()));
                }
            }
            if *keep_original {
                m.insert("original".into(), Value::Str(s.clone()));
            }
            if *remove_if_successful && field != target {
                d.remove(field)?;
            }
            d.set(target, Value::Obj(m))?;
        }
        Kind::Bytes { field, target, ignore_missing } => {
            let Some(v) = input(d, field, *ignore_missing)?.cloned() else { return Ok(Flow::Continue) };
            let out = match &v {
                Value::Str(s) => Value::from(bytes_of(s)?),
                Value::Arr(a) => Value::Arr(a.iter().map(|x| Ok(Value::from(bytes_of(&x.to_es_string())?))).collect::<EsResult<_>>()?),
                other => return Err(EsError::illegal_argument(format!("field [{field}] of type [{}] cannot be cast to [java.lang.String]", java_type(other)))),
            };
            d.set(target, out)?;
        }
        Kind::Csv { field, targets, sep, quote, trim, empty_value, ignore_missing } => {
            let Some(v) = input(d, field, *ignore_missing)?.cloned() else { return Ok(Flow::Continue) };
            let s = v.to_es_string();
            let cells = parse_csv(&s, *sep, *quote, *trim)?;
            for (i, t) in targets.iter().enumerate() {
                if let Some(c) = cells.get(i) {
                    if c.is_empty() {
                        if let Some(ev) = empty_value {
                            d.set(t, ev.clone())?;
                        }
                    } else {
                        d.set(t, Value::Str(c.clone()))?;
                    }
                }
            }
        }
        Kind::Fingerprint { fields, target, salt, method, ignore_missing } => {
            let mut buf: Vec<u8> = Vec::new();
            let mut names = fields.clone();
            names.sort();
            let mut any = false;
            for f in &names {
                match d.get_opt(f) {
                    Some(v) => {
                        any = true;
                        buf.extend_from_slice(f.as_bytes());
                        buf.push(b'|');
                        buf.extend_from_slice(v.to_json().as_bytes());
                        buf.push(b'|');
                    }
                    None if *ignore_missing => {}
                    None => return Err(EsError::illegal_argument(format!("field [{f}] not present as part of path [{f}]"))),
                }
            }
            if !any {
                return Ok(Flow::Continue);
            }
            buf.extend_from_slice(salt.as_bytes());
            let digest = super::pipeline::hash(method, &buf);
            use base64::Engine as _;
            d.set(target, Value::Str(base64::engine::general_purpose::STANDARD.encode(digest)))?;
        }
        Kind::Reroute { destination, dataset, namespace } => {
            if let Some(dst) = destination {
                d.reroute = Some(dst.render(d)?);
            } else {
                let cur = d.meta.get("_index").map(|v| v.to_es_string()).unwrap_or_default();
                let parts: Vec<&str> = cur.splitn(3, '-').collect();
                let (ty, ds, ns) = (parts.first().copied().unwrap_or("logs"), parts.get(1).copied().unwrap_or("generic"), parts.get(2).copied().unwrap_or("default"));
                let pick = |cands: &[String], d: &IngestDoc, dflt: &str| -> String {
                    for c in cands {
                        if let Some(f) = c.strip_prefix("{{").and_then(|x| x.strip_suffix("}}")) {
                            if let Some(Value::Str(s)) = d.get_opt(f.trim()) {
                                return s.clone();
                            }
                        } else {
                            return c.clone();
                        }
                    }
                    dflt.to_string()
                };
                let ds2 = pick(dataset, d, ds);
                let ns2 = pick(namespace, d, ns);
                d.reroute = Some(format!("{ty}-{ds2}-{ns2}"));
            }
            return Ok(Flow::Drop).map(|_| Flow::Continue);
        }
        Kind::Enrich(p) => crate::xpack::enrich::run(cluster, p, d)?,
        Kind::GeoGrid(g) => {
            let v = match d.get_opt(&g.field) {
                None | Some(Value::Null) if g.ignore_missing => return Ok(Flow::Continue),
                None => return Err(EsError::illegal_argument(format!("field [{}] not present as part of path [{}]", g.field, g.field))),
                Some(Value::Null) => return Err(EsError::illegal_argument(format!("field [{}] is null, cannot process it.", g.field))),
                Some(v) => v.clone(),
            };
            let Value::Str(id) = v else {
                return Err(EsError::illegal_argument(format!("field [{}] must be a String tile address", g.field)));
            };
            let cell = grid_cell(g.grid, &id).map_err(|e| EsError::illegal_argument("invalid tile definition").caused_by(e))?;
            d.set(&g.target, cell.geometry.render(g.wkt))?;
            if let Some(p) = &g.parent
                && !cell.parent.is_empty()
            {
                d.set(p, Value::Str(cell.parent))?;
            }
            let invalid = |reason: String| EsError::illegal_argument("invalid tile definition").caused_by(EsError::illegal_argument(reason));
            if let Some(c) = &g.children {
                let children = cell.children.map_err(invalid)?;
                if !children.is_empty() {
                    d.set(c, Value::Arr(children.into_iter().map(Value::Str).collect()))?;
                }
            }
            if let Some(c) = &g.non_children {
                let non_children = cell.non_children.map_err(invalid)?;
                if !non_children.is_empty() {
                    d.set(c, Value::Arr(non_children.into_iter().map(Value::Str).collect()))?;
                }
            }
            if let Some(p) = &g.precision {
                d.set(p, Value::from(i64::from(cell.precision)))?;
            }
        }
        Kind::Unsupported(t) => {
            return Err(EsError::illegal_argument(format!("processor [{t}] is not available in roll search")));
        }
    }
    Ok(Flow::Continue)
}

/// The `geo_grid` processor's configuration.
#[derive(Clone, Debug)]
pub struct GeoGrid {
    field: String,
    target: String,
    parent: Option<String>,
    children: Option<String>,
    non_children: Option<String>,
    precision: Option<String>,
    ignore_missing: bool,
    grid: GridType,
    wkt: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GridType {
    Geohash,
    Geotile,
    Geohex,
}

/// A cell's shape: a `BBOX` (hash and tile cells) or a polygon ring of `(lon, lat)` (hex cells).
enum CellGeometry {
    Rect { min_lon: f64, max_lon: f64, max_lat: f64, min_lat: f64 },
    Polygon(Vec<(f64, f64)>),
}

impl CellGeometry {
    /// `GeometryParserFormat.WKT` / `GEOJSON` (`Envelope` for rectangles).
    fn render(&self, wkt: bool) -> Value {
        let j = crate::json::write::java_double;
        match (self, wkt) {
            (CellGeometry::Rect { min_lon, max_lon, max_lat, min_lat }, true) => Value::Str(format!("BBOX ({}, {}, {}, {})", j(*min_lon), j(*max_lon), j(*max_lat), j(*min_lat))),
            (CellGeometry::Rect { min_lon, max_lon, max_lat, min_lat }, false) => crate::obj! {
                "type" => "Envelope",
                "coordinates" => Value::Arr(vec![Value::Arr(vec![Value::from(*min_lon), Value::from(*max_lat)]), Value::Arr(vec![Value::from(*max_lon), Value::from(*min_lat)])]),
            },
            (CellGeometry::Polygon(ring), true) => {
                let pts: Vec<String> = ring.iter().map(|(lon, lat)| format!("{} {}", j(*lon), j(*lat))).collect();
                Value::Str(format!("POLYGON (({}))", pts.join(", ")))
            }
            (CellGeometry::Polygon(ring), false) => crate::obj! {
                "type" => "Polygon",
                "coordinates" => Value::Arr(vec![Value::Arr(ring.iter().map(|(lon, lat)| Value::Arr(vec![Value::from(*lon), Value::from(*lat)])).collect())]),
            },
        }
    }
}

/// What `GeoGridProcessor.TileHandler` computes for a cell id.
struct GridCell {
    geometry: CellGeometry,
    parent: String,
    /// `Err`: the reason computing them fails (the processor fails only when it asks).
    children: Result<Vec<String>, String>,
    non_children: Result<Vec<String>, String>,
    precision: u32,
}

fn grid_cell(grid: GridType, id: &str) -> EsResult<GridCell> {
    match grid {
        GridType::Geohash => {
            const BASE32: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";
            if let Some(c) = id.chars().find(|c| !c.is_ascii() || !BASE32.contains(&(*c as u8))) {
                return Err(EsError::illegal_argument(format!("unsupported symbol [{c}] in geohash [{id}]")));
            }
            if id.len() > 12 {
                return Err(EsError::illegal_argument(format!("precision of geohash [{id}] must not be greater than 12")));
            }
            if id.is_empty() {
                return Err(EsError::illegal_argument("empty geohash"));
            }
            let (min_lon, max_lon, max_lat, min_lat) = search_aggs::geo::geohash_cell_bounds(search_aggs::geo::geohash_long_of(id.as_bytes()));
            Ok(GridCell {
                geometry: CellGeometry::Rect { min_lon, max_lon, max_lat, min_lat },
                parent: id[..id.len().saturating_sub(1)].to_string(),
                children: Ok(BASE32.iter().map(|&c| format!("{id}{}", c as char)).collect()),
                non_children: Ok(Vec::new()),
                precision: id.len() as u32,
            })
        }
        GridType::Geotile => {
            // `GeoTileUtils.toBoundingBox`: `parseHash`, then `validateZXY`.
            let (z, x, y) = crate::search::extra::geotile_zxy(id)?;
            let tiles = 1i64 << z;
            let (x, y) = (i64::from(x), i64::from(y));
            let geometry = CellGeometry::Rect {
                min_lon: search_aggs::geo::tile_to_lon(x, tiles),
                max_lon: search_aggs::geo::tile_to_lon(x + 1, tiles),
                max_lat: search_aggs::geo::tile_to_lat(y, tiles),
                min_lat: search_aggs::geo::tile_to_lat(y + 1, tiles),
            };
            let children = if z + 1 > 29 {
                Vec::new()
            } else {
                let (cz, cx, cy) = (z + 1, x * 2, y * 2);
                vec![format!("{cz}/{cx}/{cy}"), format!("{cz}/{}/{cy}", cx + 1), format!("{cz}/{cx}/{}", cy + 1), format!("{cz}/{}/{}", cx + 1, cy + 1)]
            };
            Ok(GridCell { geometry, parent: if z == 0 { String::new() } else { format!("{}/{}/{}", z - 1, x / 2, y / 2) }, children: Ok(children), non_children: Ok(Vec::new()), precision: z })
        }
        GridType::Geohex => {
            use search_aggs::h3;
            // `H3.stringToH3` (`Long.parseUnsignedLong(address, 16)`) doesn't validate: the
            // processor reads any index whose fields are in range (as `0`).
            let Some(cell) = h3::string_to_h3_lenient(id) else {
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) || id.trim_start_matches('0').len() > 16 {
                    let msg = if id.trim_start_matches('0').len() > 16 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
                        format!("String value {id} exceeds range of unsigned long.")
                    } else {
                        format!("For input string: \"{id}\" under radix 16")
                    };
                    return Err(EsError::java(400, "number_format_exception", msg));
                }
                // Elasticsearch fails reading such an index (an exception without a message).
                let mut e = EsError::illegal_argument("");
                e.reason = None;
                return Err(e);
            };
            let res = h3::resolution(cell);
            let ring = h3_cartesian_ring(cell);
            let list = |n: usize, f: &dyn Fn(usize) -> u64| (0..n).map(|i| h3::h3_to_string(f(i))).collect::<Vec<_>>();
            // The finest cells have no children: `H3.h3ToChildren` / `h3ToNoChildrenIntersecting`
            // fail, when the processor asks for them.
            let finest = res >= h3::MAX_RES;
            Ok(GridCell {
                geometry: CellGeometry::Polygon(ring),
                parent: if res == 0 { String::new() } else { h3::h3_to_string(h3::ring::parent(cell)) },
                children: if finest { Err(format!("Invalid child resolution [{res}]")) } else { Ok(list(h3::ring::children_size(cell), &|i| h3::ring::child(cell, i))) },
                non_children: if finest { Err("Resolution overflow".into()) } else { Ok(list(h3::ring::no_children_size(cell), &|i| h3::ring::no_child_intersecting(cell, i))) },
                precision: res as u32,
            })
        }
    }
}

/// `H3CartesianUtil.computePoints`: the cell boundary quantized to the index encoding and closed;
/// the polar cells are closed along the dateline and the pole instead (vertices sorted by
/// longitude, the south one reversed). Cells crossing the dateline stay one polygon (Elasticsearch
/// splits those into a multipolygon).
fn h3_cartesian_ring(cell: u64) -> Vec<(f64, f64)> {
    use search_codec::points::encode;
    let q = |lon: f64, lat: f64| (encode::decode_longitude(encode::encode_longitude(lon)), encode::decode_latitude(encode::encode_latitude(lat)));
    let mut ring: Vec<(f64, f64)> = search_aggs::h3::h3_to_geo_boundary(cell).points.iter().map(|p| q(p.lon.to_degrees(), p.lat.to_degrees())).collect();
    let res = search_aggs::h3::resolution(cell);
    let north = search_aggs::h3::geo_to_h3(90.0, 0.0, res) == cell;
    let south = search_aggs::h3::geo_to_h3(-90.0, 0.0, res) == cell;
    if !(north || south) || ring.len() < 2 {
        if let Some(&first) = ring.first() {
            ring.push(first);
        }
        return ring;
    }
    ring.sort_by(|a, b| a.0.total_cmp(&b.0));
    let pos = |x: f64| if x < 0.0 { x + 360.0 } else { x };
    let (first, last) = (ring[0], ring[ring.len() - 1]);
    let t = (180.0 - pos(first.0)) / (pos(last.0) - pos(first.0));
    let y = first.1 + t * (last.1 - first.1);
    let pole = encode::decode_latitude(encode::encode_latitude(if north { 90.0 } else { -90.0 }));
    let (max_lon, min_lon) = (encode::decode_longitude(i32::MAX), encode::decode_longitude(i32::MIN));
    ring.extend([(max_lon, y), (max_lon, pole), (min_lon, pole), (min_lon, y), first]);
    if south {
        ring.reverse();
    }
    ring
}

/// `Locale.Builder.setLanguageTag`'s well-formedness: `-`-separated subtags, a 2–8 letter
/// language first (empty resets to `Locale.ROOT`).
fn is_language_tag(tag: &str) -> bool {
    if tag.is_empty() {
        return true;
    }
    let mut parts = tag.split('-');
    let lang = parts.next().unwrap_or("");
    (2..=8).contains(&lang.len()) && lang.bytes().all(|b| b.is_ascii_alphabetic()) && parts.all(|p| (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// `ExceptionsHelper.useOrSuppress` over a list of failures: the first, suppressing the rest.
fn first_suppressing(errors: Vec<EsError>) -> EsError {
    let mut it = errors.into_iter();
    match it.next() {
        Some(first) => {
            let rest: Vec<EsError> = it.collect();
            if rest.is_empty() { first } else { first.with_suppressed(rest) }
        }
        None => EsError::illegal_argument("no date formats"),
    }
}

/// `ZoneId.of`'s exception: `ZoneRulesException` for an unknown region, `DateTimeException`
/// for a malformed id.
fn zone_error(msg: String) -> EsError {
    let kind = if msg.starts_with("Unknown time-zone ID") { "zone_rules_exception" } else { "date_time_exception" };
    EsError::java(400, kind, msg)
}

/// The current year in UTC, which ingest date formats without a year take.
fn current_year() -> i64 {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    use chrono::Datelike;
    chrono::DateTime::from_timestamp_millis(ms).map_or(1970, |d| i64::from(d.year()))
}

/// `DateFormat.getFunction(format, zone, locale).apply(text)`: the ingest date processors'
/// formats, with the Java exceptions Elasticsearch reports as the cause.
fn parse_ingest_date(format: &str, text: &str, zone: Option<&crate::time::Zone>, locale: crate::time::DateLocale) -> EsResult<crate::time::Instant> {
    let nfe = |s: &str, radix: u32| {
        let tail = if radix == 10 { String::new() } else { format!(" under radix {radix}") };
        EsError::java(400, "number_format_exception", format!("For input string: \"{s}\"{tail}"))
    };
    let substring = |s: &str, from: usize, to: usize| -> EsResult<String> {
        // `String.substring` over UTF-16 units.
        let units: Vec<u16> = s.encode_utf16().collect();
        if to > units.len() {
            return Err(EsError::java(400, "string_index_out_of_bounds_exception", format!("Range [{from}, {to}) out of bounds for length {}", units.len())));
        }
        Ok(String::from_utf16_lossy(&units[from..to]))
    };
    match format {
        "ISO8601" => {
            let f = crate::time::DateFormatter::new("iso8601").map_err(EsError::illegal_argument)?;
            f.parse_detailed(text, zone, false).map_err(|e| e.to_es())
        }
        "UNIX" => {
            let secs = crate::query::xparser::java_parse_double(text)?;
            // `(long) (seconds * 1000.0)`: saturating, NaN becomes 0.
            Ok(crate::time::Instant::from_millis((secs * 1000.0) as i64))
        }
        "UNIX_MS" => crate::units::parse_java_long(text).map(crate::time::Instant::from_millis).ok_or_else(|| nfe(text, 10)),
        "TAI64N" => {
            let t = text.strip_prefix('@').unwrap_or(text);
            let base_s = substring(t, 1, 16)?;
            let base = i64::from_str_radix(&base_s, 16).map_err(|_| nfe(&base_s, 16))?;
            let rest_s = substring(t, 16, 24)?;
            let rest = i64::from_str_radix(&rest_s, 16).map_err(|_| nfe(&rest_s, 16))?;
            Ok(crate::time::Instant::from_millis(base.wrapping_mul(1000).wrapping_sub(10000).wrapping_add(rest / 1_000_000)))
        }
        _ => {
            let pattern = format.strip_prefix('8').unwrap_or(format);
            let f = crate::time::DateFormatter::new(pattern).map_err(EsError::illegal_argument)?.with_locale(locale);
            f.parse_with_year(text, zone, false, Some(current_year())).map_err(|e| e.to_es())
        }
    }
}

fn zone_name(z: &crate::time::Zone) -> String {
    match z {
        crate::time::Zone::Offset(0) => "UTC".into(),
        crate::time::Zone::Offset(s) => {
            let sign = if *s < 0 { '-' } else { '+' };
            let a = s.abs();
            format!("{sign}{:02}:{:02}", a / 3600, (a % 3600) / 60)
        }
        crate::time::Zone::Named(t) => t.name().to_string(),
    }
}

/// The grok watchdog limit from a static node setting (`ingest.grok.watchdog.max_execution_time`,
/// default 1s); `0` or a negative value disables it, as Elasticsearch's `MatcherWatchdog.noop()`.
pub(crate) fn grok_watchdog(cluster: &crate::Cluster, key: &str) -> Option<std::time::Duration> {
    let millis = match cluster.config.node_setting(key) {
        Some(v) => crate::units::TimeValue::parse(v, key).map(|t| t.as_millis()).unwrap_or(1000),
        None => 1000,
    };
    (millis > 0).then(|| std::time::Duration::from_millis(millis as u64))
}

/// A grok match failure as Elasticsearch reports it: an interrupted match is a
/// `RuntimeException`, a capture that doesn't convert a `NumberFormatException`.
fn grok_match_error(e: search_painless::grok::MatchError) -> EsError {
    let status = if matches!(e, search_painless::grok::MatchError::Interrupted { .. }) { 500 } else { 400 };
    EsError::java(status, e.kind(), e.reason())
}

fn grok_value(v: &search_painless::grok::GrokValue) -> Value {
    use search_painless::grok::GrokValue as G;
    match v {
        G::Str(s) => Value::Str(s.clone()),
        G::Int(i) => Value::from(*i as i64),
        G::Long(l) => Value::from(*l),
        G::Float(f) => Value::Num(Number::Float(crate::fetch::java_float_as_double(*f))),
        G::Double(d) => Value::Num(Number::Float(*d)),
        G::Bool(b) => Value::Bool(*b),
        G::List(l) => Value::Arr(l.iter().map(grok_value).collect()),
    }
}

fn expand_dots(m: &mut Map, field: &str, overwrite: bool) -> EsResult<()> {
    let keys: Vec<String> = if field == "*" { m.keys().filter(|k| k.contains('.')).cloned().collect() } else { vec![field.to_string()] };
    for k in keys {
        let Some(v) = m.shift_remove(&k) else { continue };
        let parts: Vec<&str> = k.split('.').collect();
        let mut cur = &mut *m;
        for p in &parts[..parts.len() - 1] {
            if !matches!(cur.get(*p), Some(Value::Obj(_))) {
                let old = cur.shift_remove(*p);
                if let Some(o) = old
                    && !o.is_null()
                {
                    return Err(EsError::illegal_argument(format!("cannot expend [{k}], because [{p}] is not an object but [{}]", o.to_es_string())));
                }
                cur.insert((*p).to_string(), Value::Obj(Map::new()));
            }
            cur = match cur.get_mut(*p) {
                Some(Value::Obj(x)) => x,
                _ => unreachable!(),
            };
        }
        let last = parts[parts.len() - 1];
        match cur.get_mut(last) {
            Some(existing) if !overwrite => {
                let mut list = match std::mem::take(existing) {
                    Value::Arr(a) => a,
                    o => vec![o],
                };
                match v {
                    Value::Arr(a) => list.extend(a),
                    o => list.push(o),
                }
                *existing = Value::Arr(list);
            }
            _ => {
                cur.insert(last.to_string(), v);
            }
        }
    }
    Ok(())
}

fn parse_csv(s: &str, sep: char, quote: char, trim: bool) -> EsResult<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_q {
            if c == quote {
                if chars.peek() == Some(&quote) {
                    cur.push(quote);
                    chars.next();
                } else {
                    in_q = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == quote && cur.trim().is_empty() {
            in_q = true;
            cur.clear();
        } else if c == sep {
            out.push(if trim { cur.trim().to_string() } else { std::mem::take(&mut cur) });
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    if in_q {
        return Err(EsError::illegal_argument("Unmatched quote"));
    }
    out.push(if trim { cur.trim().to_string() } else { cur });
    Ok(out)
}

#[allow(dead_code)]
fn _str<'a>(d: &'a IngestDoc, f: &str) -> EsResult<Option<&'a str>> {
    str_field(d, f, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iso(i: crate::time::Instant) -> String {
        crate::time::DateFormatter::new("strict_date_time").unwrap().format(i, &crate::time::Zone::UTC)
    }

    #[test]
    fn ingest_date_formats_follow_date_format_java() {
        let en = crate::time::DateLocale::English;
        assert_eq!(iso(parse_ingest_date("TAI64N", "4000000050d506482dbdf024", None, en).unwrap()), "2012-12-22T01:00:46.767Z");
        assert_eq!(iso(parse_ingest_date("TAI64N", "@4000000050d506482dbdf024", None, en).unwrap()), "2012-12-22T01:00:46.767Z");
        let e = parse_ingest_date("TAI64N", "zz", None, en).unwrap_err();
        assert_eq!((e.kind.as_ref(), e.reason()), ("string_index_out_of_bounds_exception", "Range [1, 16) out of bounds for length 2"));
        let e = parse_ingest_date("TAI64N", "4000000050d50648", None, en).unwrap_err();
        assert_eq!(e.reason(), "Range [16, 24) out of bounds for length 16");
        let e = parse_ingest_date("TAI64N", "4000000050d5064zzdbdf024", None, en).unwrap_err();
        assert_eq!((e.kind.as_ref(), e.reason()), ("number_format_exception", "For input string: \"000000050d5064z\" under radix 16"));
        assert_eq!(iso(parse_ingest_date("UNIX", " 1.5e9 ", None, en).unwrap()), "2017-07-14T02:40:00.000Z");
        assert_eq!(parse_ingest_date("UNIX", "abc", None, en).unwrap_err().reason(), "For input string: \"abc\"");
        assert_eq!(parse_ingest_date("UNIX_MS", "1.5", None, en).unwrap_err().reason(), "For input string: \"1.5\"");
        let es = crate::time::DateLocale::parse("es");
        assert_eq!(iso(parse_ingest_date("dd MMMM yyyy", "03 marzo 2021", None, es).unwrap()), "2021-03-03T00:00:00.000Z");
        assert!(parse_ingest_date("dd MMMM yyyy", "03 marzo 2021", None, en).is_err());
        // No year: the current one.
        let i = parse_ingest_date("MMM dd", "Mar 03", None, en).unwrap();
        assert!(iso(i).starts_with(&format!("{}-03-03", current_year())));
    }

    #[test]
    fn geo_grid_cells_match_elasticsearch() {
        let tile = grid_cell(GridType::Geotile, "4/8/5").unwrap();
        assert_eq!(tile.geometry.render(true).as_str(), Some("BBOX (0.0, 22.5, 55.77657301866769, 40.979898069620134)"));
        assert_eq!((tile.parent.as_str(), tile.precision), ("3/4/2", 4));
        assert_eq!(tile.children.unwrap(), ["5/16/10", "5/17/10", "5/16/11", "5/17/11"]);
        let hash = grid_cell(GridType::Geohash, "u0").unwrap();
        assert_eq!(hash.geometry.render(true).as_str(), Some("BBOX (0.0, 11.25, 50.625, 45.0)"));
        let children = hash.children.unwrap();
        assert_eq!((hash.parent.as_str(), children.len(), children[31].as_str()), ("u", 32, "u0z"));
        let hex = grid_cell(GridType::Geohex, "811fbffffffffff").unwrap();
        let wkt = hex.geometry.render(true);
        assert!(wkt.as_str().unwrap().starts_with("POLYGON ((1.1885095294564962 49.470279179513454, "), "{wkt:?}");
        assert_eq!((hex.parent.as_str(), hex.children.unwrap().len(), hex.non_children.unwrap().len()), ("801ffffffffffff", 7, 6));
        // the finest cells fail only when the processor asks for their children
        let finest = grid_cell(GridType::Geohex, "8f2830828052d25").unwrap();
        assert_eq!(finest.children.unwrap_err(), "Invalid child resolution [15]");
        assert_eq!(finest.non_children.unwrap_err(), "Resolution overflow");
        assert!(grid_cell(GridType::Geohex, "ffffffffffffffff").err().unwrap().reason.is_none());
        assert_eq!(grid_cell(GridType::Geotile, "30/1/1").err().unwrap().reason(), "Invalid geotile_grid precision of 30. Must be between 0 and 29.");
        assert_eq!(grid_cell(GridType::Geohash, "").err().unwrap().reason(), "empty geohash");
        assert!(grid_cell(GridType::Geotile, "3/9/1").is_err());
        assert_eq!(grid_cell(GridType::Geohex, "zz").err().unwrap().kind.as_ref(), "number_format_exception");
    }

    #[test]
    fn language_tags_and_suppressed_failures() {
        assert!(is_language_tag("es") && is_language_tag("de-AT") && is_language_tag(""));
        assert!(!is_language_tag("de_AT") && !is_language_tag("x") && !is_language_tag("de--AT"));
        let e = first_suppressing(vec![EsError::illegal_argument("a"), EsError::illegal_argument("b")]);
        assert_eq!((e.reason(), e.suppressed().len()), ("a", 1));
        assert_eq!(zone_error("Unknown time-zone ID: X/Y".into()).kind.as_ref(), "zone_rules_exception");
    }
}
