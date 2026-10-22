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
    Date { field: String, target: String, formats: Vec<String>, timezone: Option<Template>, output_format: String },
    DateIndexName { field: String, prefix: Template, rounding: String, formats: Vec<String>, timezone: Option<String>, index_format: String },
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
    /// Accepted but needs data or licenses roll does not have (geoip, user_agent, …).
    Unsupported(String),
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
                let mut groks = Vec::new();
                for p in &patterns {
                    groks.push(Arc::new(search_painless::grok::Grok::new(&bank, p).map_err(|e| {
                        EsError::new(400, "parse_exception", format!("[patterns] Invalid regex pattern found in: {patterns:?}. {}", e.message))
                            .with_meta("property_name", "patterns")
                            .with_meta("processor_type", "grok")
                    })?));
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
        Kind::Date { field, target, formats, timezone, output_format } => {
            let v = d.get(field)?.clone();
            let text = v.to_es_string();
            let zone = match timezone {
                Some(t) => Some(crate::time::Zone::parse(&t.render(d)?).map_err(EsError::illegal_argument)?),
                None => None,
            };
            let mut last_err = None;
            let mut parsed = None;
            for f in formats {
                let pattern = match f.as_str() {
                    "ISO8601" => "iso8601",
                    "UNIX" => "epoch_second",
                    "UNIX_MS" => "epoch_millis",
                    "TAI64N" => "epoch_millis",
                    other => other,
                };
                match crate::time::DateFormatter::new(pattern).and_then(|fm| fm.parse(&text, zone.as_ref(), false)) {
                    Ok(i) => {
                        parsed = Some(i);
                        break;
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            let Some(i) = parsed else {
                return Err(EsError::illegal_argument(format!("unable to parse date [{text}]")).caused_by(EsError::illegal_argument(last_err.unwrap_or_default())));
            };
            let out = crate::time::DateFormatter::new(output_format).map_err(EsError::illegal_argument)?;
            d.set(target, Value::Str(out.format(i, zone.as_ref().unwrap_or(&crate::time::Zone::UTC))))?;
        }
        Kind::DateIndexName { field, prefix, rounding, formats, timezone, index_format } => {
            let v = d.get(field)?.clone();
            let text = v.to_es_string();
            let zone = match timezone {
                Some(t) => crate::time::Zone::parse(t).map_err(EsError::illegal_argument)?,
                None => crate::time::Zone::UTC,
            };
            let mut parsed = None;
            for f in formats {
                if let Ok(i) = crate::time::DateFormatter::new(f).and_then(|fm| fm.parse(&text, Some(&zone), false)) {
                    parsed = Some(i);
                    break;
                }
            }
            let i = parsed.ok_or_else(|| EsError::illegal_argument(format!("unable to parse date [{text}]")))?;
            let p = prefix.render(d)?;
            let name = format!("<{p}{{{}||/{rounding}{{{index_format}|{}}}}}>", crate::time::java_instant_string(i.millis()), zone_name(&zone));
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
                let caps = g.captures(s).map_err(|e| EsError::illegal_argument(format!("{e:?}")))?;
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
        Kind::Unsupported(t) => {
            return Err(EsError::illegal_argument(format!("processor [{t}] is not available in roll search")));
        }
    }
    Ok(Flow::Continue)
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
