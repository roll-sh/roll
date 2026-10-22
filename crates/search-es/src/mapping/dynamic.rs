//! Dynamic mapping: the mapping update a document needs, as Elasticsearch's `DocumentParser` and
//! `DynamicFieldsBuilder` derive it.
//!
//! For every field of the document that the mapping doesn't know, the effective `dynamic` of its
//! parent object decides: `strict` fails, `false` ignores, `runtime` adds a runtime field, `true`
//! adds a concrete field. Matching `dynamic_templates` win over the built-in inference:
//!
//! | JSON value                              | field                                  |
//! | --------------------------------------- | -------------------------------------- |
//! | string matching a dynamic date format   | `date` (non-default formats recorded)  |
//! | numeric string with `numeric_detection` | `long` / `float`                       |
//! | other string                            | `text` + `keyword` sub-field (`ignore_above: 256`) |
//! | integer                                 | `long`                                 |
//! | other number                            | `float`                                |
//! | boolean                                 | `boolean`                              |
//! | object                                  | `object` (recursively)                 |
//! | array                                   | the type of its first non-null element |
//! | `null`, `[]`                            | nothing                                |
//!
//! Dotted names (`"a.b": 1`) expand to objects unless `subobjects` is `false`. The result is a
//! [`Mapping`] holding only the new fields, merged with [`Mapping::merge`] by the caller (which
//! then enforces the field limits).

use super::{DynamicTemplate, FieldMapper, Mapper, Mapping, MappingCtx, ObjectMapper, RuntimeField, types};
use crate::error::EsError;
use crate::json::{Map, Value};
use crate::obj;
use crate::time::DateFormatter;

/// Elasticsearch's default `dynamic_date_formats`.
pub const DEFAULT_DYNAMIC_DATE_FORMATS: [&str; 2] = ["strict_date_optional_time", "yyyy/MM/dd HH:mm:ss||yyyy/MM/dd||epoch_millis"];

/// Why a document can't be mapped.
#[derive(Debug, Clone, PartialEq)]
pub enum DynamicError {
    /// `mapping set to <mode>, dynamic introduction of [field] within [path] is not allowed`
    /// (`mode` is `strict`, or OpenSearch's `strict_allow_templates`).
    Strict { field: String, path: String, mode: &'static str },
    /// Any other failure (field name rules, template errors).
    Other(EsError),
}

impl DynamicError {
    /// The Elasticsearch exception, with an optional `[line:col] ` location prefix.
    pub fn into_es(self, location: Option<(usize, usize)>) -> EsError {
        let loc = location.map(|(l, c)| format!("[{l}:{c}] ")).unwrap_or_default();
        match self {
            DynamicError::Strict { field, path, mode } => EsError::new(
                400,
                "strict_dynamic_mapping_exception",
                format!("{loc}mapping set to {mode}, dynamic introduction of [{field}] within [{path}] is not allowed"),
            ),
            DynamicError::Other(e) => e,
        }
    }
}

struct Ctx<'a> {
    mapping: &'a Mapping,
    ctx: &'a MappingCtx,
    date_formats: Vec<(String, DateFormatter)>,
    update: Mapping,
}

/// Computes the mapping update for `doc`. `None` when the mapping already covers it.
pub fn dynamic_update(mapping: &Mapping, doc: &Map, ctx: &MappingCtx) -> Result<Option<Mapping>, DynamicError> {
    let formats: Vec<String> = match &mapping.dynamic_date_formats {
        Some(f) => f.clone(),
        None => DEFAULT_DYNAMIC_DATE_FORMATS.iter().map(|s| s.to_string()).collect(),
    };
    let date_formats = formats.into_iter().filter_map(|f| DateFormatter::new(&f).ok().map(|d| (f, d))).collect();
    let mut c = Ctx { mapping, ctx, date_formats, update: Mapping::empty() };
    let root_dynamic = mapping.root.dynamic.unwrap_or(super::Dynamic::True);
    walk_object(&mut c, Some(&mapping.root), "", doc, root_dynamic, &mut Vec::new())?;
    if c.update.root.children.is_empty() && c.update.runtime.is_empty() {
        return Ok(None);
    }
    check_limits(mapping, &c.update, ctx)?;
    Ok(Some(c.update))
}

/// The limits Elasticsearch enforces while adding dynamic fields (`DocumentParserContext`):
/// total fields and object depth, failing the document.
fn check_limits(base: &Mapping, update: &Mapping, ctx: &MappingCtx) -> Result<(), DynamicError> {
    fn count_new(update: &ObjectMapper, base: Option<&ObjectMapper>) -> usize {
        update
            .children
            .iter()
            .map(|(k, m)| match (m, base.and_then(|b| b.children.get(k))) {
                (Mapper::Object(o), Some(Mapper::Object(bo))) => crate::stack::maybe_grow(|| count_new(o, Some(bo))),
                (_, Some(_)) => 0,
                (Mapper::Object(o), None) => 1 + crate::stack::maybe_grow(|| count_new(o, None)),
                (Mapper::Field(f), None) => 1 + f.fields.len(),
                (Mapper::Alias(_), None) => 1,
            })
            .sum()
    }
    fn deepest(o: &ObjectMapper, path: &str, limit: i64) -> Option<String> {
        for (k, m) in &o.children {
            if let Mapper::Object(child) = m {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                // `MappingLookup.checkDepthLimit`: an object's depth is its dots plus two.
                if p.matches('.').count() as i64 + 2 > limit {
                    return Some(p);
                }
                if let Some(d) = crate::stack::maybe_grow(|| deepest(child, &p, limit)) {
                    return Some(d);
                }
            }
        }
        None
    }
    let failed = |msg: String| DynamicError::Other(EsError::document_parsing(format!("failed to parse: {msg}")).caused_by(EsError::illegal_argument(msg)));
    let new = count_new(&update.root, Some(&base.root)) + update.runtime.keys().filter(|k| !base.runtime.contains_key(*k)).count();
    if (base.total_fields() + new) as i64 > ctx.total_fields_limit {
        return Err(failed(format!("Limit of total fields [{}] has been exceeded while adding new fields [{new}]", ctx.total_fields_limit)));
    }
    if let Some(p) = deepest(&update.root, "", ctx.depth_limit) {
        return Err(failed(format!("Limit of mapping depth [{}] has been exceeded due to object field [{p}]", ctx.depth_limit)));
    }
    Ok(())
}

/// Walks an object of the document. `existing` is the mapped object at `path` (if any) and
/// `trail` the chain of new object names leading here (for building the update).
fn walk_object(c: &mut Ctx, existing: Option<&ObjectMapper>, path: &str, doc: &Map, dynamic: super::Dynamic, trail: &mut Vec<String>) -> Result<(), DynamicError> {
    let subobjects = existing.map(|o| !o.literal_dots()).unwrap_or(true);
    let disable_objects = existing.is_some_and(|o| o.disable_objects == Some(true));
    for (key, value) in doc {
        if disable_objects
            && let Value::Obj(inner) = value
            && !existing.is_some_and(|o| o.children.contains_key(key))
        {
            // OpenSearch's `disable_objects`: objects in the document become dotted leaf names.
            let mut flat = Map::new();
            flatten_into(&mut flat, key, inner);
            crate::stack::maybe_grow(|| walk_object(c, existing, path, &flat, dynamic, trail))?;
            continue;
        }
        if key.is_empty() {
            return Err(DynamicError::Other(EsError::document_parsing("failed to parse: field name cannot be an empty string").caused_by(EsError::illegal_argument("field name cannot be an empty string"))));
        }
        if key.trim().is_empty() {
            return Err(DynamicError::Other(EsError::document_parsing(format!("Field name cannot contain only whitespace: [{key}]"))));
        }
        if key.contains('.') {
            // Java's `split("\\.")`: trailing empty parts are dropped (`a.` is `a`).
            let mut parts: Vec<&str> = key.split('.').collect();
            while parts.last().is_some_and(|p| p.is_empty()) {
                parts.pop();
            }
            let failed = |msg: String| DynamicError::Other(EsError::document_parsing(format!("failed to parse: {msg}")).caused_by(EsError::illegal_argument(msg)));
            if parts.is_empty() {
                return Err(failed("field name cannot contain only dots".into()));
            }
            if parts.iter().any(|p| p.is_empty()) {
                return Err(failed(format!("field name cannot contain only whitespace: ['{key}']")));
            }
            if parts.iter().any(|p| p.trim().is_empty()) {
                return Err(failed(format!("field name starting or ending with a [.] makes object resolution ambiguous: [{key}]")));
            }
            if subobjects || parts.len() == 1 {
                // Expand `a.b.c: v` to `a: {b: {c: v}}`.
                let mut v = value.clone();
                for p in parts[1..].iter().rev() {
                    v = obj! { p.to_string() => v };
                }
                let mut single = Map::new();
                single.insert(parts[0].to_string(), v);
                crate::stack::maybe_grow(|| walk_object(c, existing, path, &single, dynamic, trail))?;
                continue;
            }
        }
        let full = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
        let child = existing.and_then(|o| o.children.get(key));
        match child {
            Some(Mapper::Object(o)) => {
                let child_dynamic = o.dynamic.unwrap_or(dynamic);
                if !o.is_enabled() {
                    continue;
                }
                for obj_value in objects_in(value) {
                    trail.push(key.clone());
                    crate::stack::maybe_grow(|| walk_object(c, Some(o), &full, obj_value, child_dynamic, trail))?;
                    trail.pop();
                }
            }
            Some(_) => {}
            None => {
                if c.mapping.runtime.contains_key(&full) || c.update.runtime.contains_key(&full) {
                    continue;
                }
                new_field(c, path, key, &full, value, dynamic, trail)?;
            }
        }
    }
    Ok(())
}

fn flatten_into(out: &mut Map, prefix: &str, obj: &Map) {
    for (k, v) in obj {
        let key = format!("{prefix}.{k}");
        match v {
            Value::Obj(inner) => crate::stack::maybe_grow(|| flatten_into(out, &key, inner)),
            other => {
                out.insert(key, other.clone());
            }
        }
    }
}

/// The objects inside a value (an object, or the objects of an array).
fn objects_in(v: &Value) -> Vec<&Map> {
    match v {
        Value::Obj(m) => vec![m],
        Value::Arr(a) => a.iter().flat_map(objects_in).collect(),
        _ => Vec::new(),
    }
}

/// The JSON type of a value for dynamic templates, or `None` for null/empty arrays.
fn first_value(v: &Value) -> Option<&Value> {
    match v {
        Value::Null => None,
        Value::Arr(a) => a.iter().find_map(first_value),
        other => Some(other),
    }
}

fn new_field(c: &mut Ctx, parent: &str, key: &str, full: &str, value: &Value, dynamic: super::Dynamic, trail: &[String]) -> Result<(), DynamicError> {
    let Some(v) = first_value(value) else { return Ok(()) };
    let strict = |mode: &'static str| DynamicError::Strict { field: key.to_string(), path: if parent.is_empty() { "_doc".into() } else { parent.to_string() }, mode };
    match dynamic {
        super::Dynamic::Strict => return Err(strict("strict")),
        super::Dynamic::False => return Ok(()),
        _ => {}
    }
    // Objects.
    if let Value::Obj(inner) = v {
        if dynamic == super::Dynamic::Runtime {
            // Objects aren't runtime fields; their leaves are.
            let mut t: Vec<String> = trail.to_vec();
            t.push(key.to_string());
            for obj in objects_in(value) {
                crate::stack::maybe_grow(|| walk_object(c, None, full, obj, dynamic, &mut t))?;
            }
            return Ok(());
        }
        let template = find_template(c, full, key, "object");
        let mapper = match template {
            Some(t) => template_mapper(c, &t, key, "object")?,
            // OpenSearch: without a template, `false_allow_templates` skips the object;
            // `strict_allow_templates` adds it (its unknown leaves then fail).
            None if dynamic == super::Dynamic::FalseAllowTemplates => return Ok(()),
            None => Mapper::Object(ObjectMapper::new(key)),
        };
        let child_dynamic = match &mapper {
            Mapper::Object(o) => o.dynamic.unwrap_or(dynamic),
            _ => dynamic,
        };
        insert(c, trail, mapper.clone());
        if let Mapper::Object(o) = &mapper {
            let mut t: Vec<String> = trail.to_vec();
            t.push(key.to_string());
            for obj in objects_in(value) {
                crate::stack::maybe_grow(|| walk_object(c, Some(o), full, obj, child_dynamic, &mut t))?;
            }
        }
        let _ = inner;
        return Ok(());
    }
    let (xtype, builtin): (&str, Builtin) = match v {
        Value::Str(s) => classify_string(c, s),
        Value::Num(n) if n.is_integer() => ("long", Builtin::Long),
        Value::Num(_) => ("double", Builtin::Float),
        Value::Bool(_) => ("boolean", Builtin::Boolean),
        _ => return Ok(()),
    };
    if let Some(t) = find_template(c, full, key, xtype) {
        if t.runtime {
            let dt = match xtype {
                "string" => "keyword",
                other => other,
            };
            let ty = t.mapping_type(dt);
            let mut conf = t.mapping_for(key, dt);
            conf.insert("type".into(), Value::from(ty));
            let rf = RuntimeField::parse(full, &Value::Obj(conf)).map_err(DynamicError::Other)?;
            c.update.runtime.insert(full.to_string(), rf);
        } else {
            let mapper = template_mapper(c, &t, key, xtype)?;
            insert(c, trail, mapper);
        }
        return Ok(());
    }
    match dynamic {
        super::Dynamic::StrictAllowTemplates => return Err(strict("strict_allow_templates")),
        super::Dynamic::FalseAllowTemplates => return Ok(()),
        _ => {}
    }
    if dynamic == super::Dynamic::Runtime {
        let ty = match builtin {
            Builtin::Text => "keyword",
            Builtin::Long => "long",
            Builtin::Float => "double",
            Builtin::Boolean => "boolean",
            Builtin::Date(_) => "date",
        };
        let mut params = Map::new();
        if let Builtin::Date(Some(f)) = &builtin {
            params.insert("format".into(), Value::from(f.clone()));
        }
        c.update.runtime.insert(full.to_string(), RuntimeField { ty: ty.into(), params });
        return Ok(());
    }
    let mapper = builtin_mapper(key, builtin, c.ctx);
    insert(c, trail, mapper);
    Ok(())
}

enum Builtin {
    Text,
    Long,
    Float,
    Boolean,
    /// A date with a non-default format.
    Date(Option<String>),
}

fn classify_string(c: &Ctx, s: &str) -> (&'static str, Builtin) {
    let as_long = crate::units::parse_java_long(s).is_some();
    let as_double = crate::units::parse_java_double(s).is_some();
    let numeric = c.mapping.numeric_detection.unwrap_or(false);
    if as_long && numeric {
        return ("long", Builtin::Long);
    }
    if as_double && numeric {
        return ("double", Builtin::Float);
    }
    if !as_long && !as_double && c.mapping.date_detection.unwrap_or(true) {
        for (pattern, f) in &c.date_formats {
            if f.parse(s, None, false).is_ok() {
                let fmt = if pattern == "strict_date_optional_time" || pattern == DateFormatter::DEFAULT { None } else { Some(pattern.clone()) };
                return ("date", Builtin::Date(fmt));
            }
        }
    }
    ("string", Builtin::Text)
}

fn builtin_mapper(name: &str, b: Builtin, ctx: &MappingCtx) -> Mapper {
    let def = |t: &str| types::lookup(t).expect("built-in type");
    match b {
        Builtin::Text => {
            let mut f = FieldMapper::new(name, def("text"));
            let mut kw = FieldMapper::new("keyword", def("keyword"));
            kw.set("ignore_above", Value::from(256));
            f.fields.insert("keyword".into(), kw);
            Mapper::Field(f)
        }
        Builtin::Long => Mapper::Field(FieldMapper::new(name, def("long"))),
        Builtin::Float => Mapper::Field(FieldMapper::new(name, def("float"))),
        Builtin::Boolean => Mapper::Field(FieldMapper::new(name, def("boolean"))),
        Builtin::Date(fmt) => {
            let mut f = FieldMapper::new(name, def("date"));
            if let Some(fmt) = fmt {
                // The built-in second default format carries its root locale explicitly.
                let builtin = fmt == DEFAULT_DYNAMIC_DATE_FORMATS[1];
                f.set("format", Value::from(fmt));
                if builtin {
                    f.set("locale", Value::from(""));
                }
            }
            let _ = ctx;
            Mapper::Field(f)
        }
    }
}

fn find_template(c: &Ctx, full: &str, key: &str, xtype: &str) -> Option<DynamicTemplate> {
    c.mapping.dynamic_templates.as_ref()?.iter().find(|t| t.matches(full, key, xtype)).cloned()
}

fn template_mapper(c: &Ctx, t: &DynamicTemplate, key: &str, xtype: &str) -> Result<Mapper, DynamicError> {
    let dt = match xtype {
        "string" => "text",
        "double" => "float",
        other => other,
    };
    let ty = t.mapping_type(dt);
    let conf = t.mapping_for(key, dt);
    super::parse_mapper(key, &ty, &conf, c.ctx, 1).map_err(|e| {
        if e.reason().starts_with("The mapper type") {
            DynamicError::Other(EsError::mapper_parsing(format!("failed to find type parsed [{ty}] for [{key}]")))
        } else {
            DynamicError::Other(e)
        }
    })
}

/// Adds a new mapper to the update under the object chain `trail`.
fn insert(c: &mut Ctx, trail: &[String], mapper: Mapper) {
    let mut cur = &mut c.update.root;
    let mut existing: Option<&ObjectMapper> = Some(&c.mapping.root);
    for name in trail {
        existing = existing.and_then(|o| match o.children.get(name) {
            Some(Mapper::Object(child)) => Some(child),
            _ => None,
        });
        let entry = cur.children.entry(name.clone()).or_insert_with(|| {
            // Re-create the existing object's shell so the merge keeps its parameters.
            let mut shell = ObjectMapper::new(name);
            if let Some(e) = existing {
                shell.kind = e.kind.clone();
            }
            Mapper::Object(shell)
        });
        let Mapper::Object(o) = entry else { return };
        cur = o;
    }
    let name = mapper.name().to_string();
    match (cur.children.get_mut(&name), mapper) {
        (Some(Mapper::Object(_)), Mapper::Object(_)) => {}
        (_, m) => {
            cur.children.insert(name, m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn update(mapping: &str, doc: &str) -> Result<Option<Value>, DynamicError> {
        let ctx = MappingCtx::from_settings(&crate::meta::settings::Settings::new());
        let m = Mapping::parse(parse(mapping.as_bytes()).unwrap().unwrap().as_object().unwrap(), &ctx).unwrap();
        let d = parse(doc.as_bytes()).unwrap().unwrap();
        Ok(dynamic_update(&m, d.as_object().unwrap(), &ctx)?.map(|u| u.to_value(&ctx, false)))
    }

    #[test]
    fn builtin_inference_matches_elasticsearch() {
        let u = update("{}", r#"{"d1":"2015-01-01","d2":"2015/01/01","s":"hello","n":5,"f":1.5,"b":true,"nul":null,"o":{"x":1},"arr":[1,2],"e":[],"dotted.name":"x","numstr":"12","eo":{},"mixed":[null,"x"]}"#)
            .unwrap()
            .unwrap();
        let text = r#"{"type":"text","fields":{"keyword":{"type":"keyword","ignore_above":256}}}"#;
        let expected = format!(
            r#"{{"properties":{{"arr":{{"type":"long"}},"b":{{"type":"boolean"}},"d1":{{"type":"date"}},"d2":{{"type":"date","format":"yyyy/MM/dd HH:mm:ss||yyyy/MM/dd||epoch_millis","locale":""}},"dotted":{{"properties":{{"name":{text}}}}},"eo":{{"type":"object"}},"f":{{"type":"float"}},"mixed":{text},"n":{{"type":"long"}},"numstr":{text},"o":{{"properties":{{"x":{{"type":"long"}}}}}},"s":{text}}}}}"#
        );
        assert_eq!(u.to_json(), expected);
    }

    #[test]
    fn strict_and_false() {
        let m = r#"{"dynamic":"strict","properties":{"o":{"type":"object","dynamic":"true","properties":{"s":{"type":"object","dynamic":"strict"}}}}}"#;
        assert_eq!(update(m, r#"{"x":1}"#).unwrap_err(), DynamicError::Strict { field: "x".into(), path: "_doc".into(), mode: "strict" });
        assert_eq!(update(m, r#"{"o":{"s":{"y":1}}}"#).unwrap_err(), DynamicError::Strict { field: "y".into(), path: "o.s".into(), mode: "strict" });
        assert_eq!(update(m, r#"{"o":{"z":{"q":1}}}"#).unwrap().unwrap().to_json(), r#"{"properties":{"o":{"properties":{"z":{"properties":{"q":{"type":"long"}}}}}}}"#);
        assert_eq!(update(r#"{"dynamic":false}"#, r#"{"x":1}"#).unwrap(), None);
    }

    #[test]
    fn runtime_and_templates() {
        assert_eq!(update(r#"{"dynamic":"runtime"}"#, r#"{"s":"x","n":1}"#).unwrap().unwrap().to_json(), r#"{"runtime":{"n":{"type":"long"},"s":{"type":"keyword"}}}"#);
        let m = r#"{"dynamic_templates":[{"strings":{"match_mapping_type":"string","mapping":{"type":"keyword"}}},{"longs_as_int":{"match":"i_*","mapping":{"type":"integer"}}}]}"#;
        assert_eq!(update(m, r#"{"s":"x","i_a":1,"l":2}"#).unwrap().unwrap().to_json(), r#"{"properties":{"i_a":{"type":"integer"},"l":{"type":"long"},"s":{"type":"keyword"}}}"#);
    }

    #[test]
    fn regex_templates_match_like_java() {
        // Elasticsearch 9.5.5 mapped `foo_x` as keyword, `ab` as long and left `abab` and
        // `baz_x` to the defaults; the look-behind template is accepted but matches nothing here.
        let m = r#"{"dynamic_templates":[{"t":{"match":"^(foo|bar)_.*$","match_pattern":"regex","mapping":{"type":"keyword"}}},{"u":{"match":"[ab]{2,3}","match_pattern":"regex","mapping":{"type":"long"}}},{"v":{"match":"(?<=a)b","match_pattern":"regex","mapping":{"type":"long"}}}]}"#;
        let text = r#"{"type":"text","fields":{"keyword":{"type":"keyword","ignore_above":256}}}"#;
        assert_eq!(
            update(m, r#"{"foo_x":"1","baz_x":"2","ab":"3","abab":"4"}"#).unwrap().unwrap().to_json(),
            format!(r#"{{"properties":{{"ab":{{"type":"long"}},"abab":{text},"baz_x":{text},"foo_x":{{"type":"keyword"}}}}}}"#)
        );
        // Matching is linear: a pattern that backtracks exponentially stays instant.
        let evil = format!(r#"{{"dynamic_templates":[{{"t":{{"match":"{}b","match_pattern":"regex","mapping":{{"type":"keyword"}}}}}}]}}"#, "a*".repeat(20));
        let start = std::time::Instant::now();
        let u = update(&evil, &format!(r#"{{"{}":"x"}}"#, "a".repeat(40))).unwrap().unwrap().to_json();
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert!(u.contains(r#""type":"text""#), "{u}");
        // Java's classes are ASCII: `\w+` doesn't match `é`.
        let w = r#"{"dynamic_templates":[{"t":{"match":"\\w+_\\d","match_pattern":"regex","mapping":{"type":"keyword"}}}]}"#;
        let u = update(w, r#"{"ab_1":"x","é_1":"y","ab_١":"z"}"#).unwrap().unwrap().to_json();
        assert!(u.contains(r#""ab_1":{"type":"keyword"}"#), "{u}");
        assert!(!u.contains(r#""é_1":{"type":"keyword"}"#) && !u.contains(r#""ab_١":{"type":"keyword"}"#), "{u}");
        // Invalid patterns fail when the mapping is created.
        for bad in ["(", "*", "a{2", "[a-"] {
            let ctx = MappingCtx::from_settings(&crate::meta::settings::Settings::new());
            let src = format!(r#"{{"dynamic_templates":[{{"t":{{"match":"{bad}","match_pattern":"regex","mapping":{{"type":"keyword"}}}}}}]}}"#);
            let e = Mapping::parse(parse(src.as_bytes()).unwrap().unwrap().as_object().unwrap(), &ctx).unwrap_err();
            assert_eq!(e.reason(), format!("Pattern [{bad}] of type [regex] is invalid. Cannot create dynamic template [t]."));
        }
    }

    #[test]
    fn known_fields_need_no_update() {
        assert_eq!(update(r#"{"properties":{"a":{"type":"keyword"}}}"#, r#"{"a":"x"}"#).unwrap(), None);
    }
}
