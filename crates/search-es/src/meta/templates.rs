//! Index templates: composable templates (`_index_template`), component templates
//! (`_component_template`) and legacy templates (`_template`).
//!
//! A new index takes its settings, mappings and aliases from the highest-priority composable
//! template whose patterns match its name — composed from its component templates in
//! `composed_of` order, then the template's own `template` — or, when none matches, from every
//! matching legacy template in ascending `order`. Request values override template values.
//!
//! Puts are validated as Elasticsearch validates them: required fields, pattern rules,
//! missing component templates, same-priority overlaps (glob intersection), settings and the
//! composed mapping; texts and exception types follow Elasticsearch.

use super::aliases::parse_aliases;
use super::index_settings::validate_user_settings_in;
use super::settings::Settings;
use super::{AliasMeta, Metadata};
use crate::error::EsError;
use crate::json::{Map, Value};
use crate::mapping::{Mapping, MappingCtx, MergeReason, unwrap_type};
use crate::obj;
use crate::xcontent::filter::glob_match;
use std::collections::BTreeMap;

/// The `template` block shared by composable and component templates.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Template {
    pub settings: Option<Settings>,
    pub mappings: Option<Map>,
    pub aliases: Option<BTreeMap<String, AliasMeta>>,
    pub lifecycle: Option<Value>,
    pub data_stream_options: Option<Value>,
}

/// The fields each template parser declares (for "did you mean" suggestions).
const TEMPLATE_FIELDS: &[&str] = &["settings", "mappings", "aliases", "lifecycle", "data_stream_options"];
const COMPONENT_FIELDS: &[&str] = &["template", "version", "_meta", "deprecated", "created_date_millis", "modified_date_millis"];
const INDEX_TEMPLATE_FIELDS: &[&str] = &[
    "index_patterns", "template", "composed_of", "priority", "version", "_meta", "data_stream", "allow_auto_create",
    "ignore_missing_component_templates", "deprecated", "created_date_millis", "modified_date_millis",
];

/// An `[x_content_parse_exception]` for an unknown field (with Elasticsearch's suggestions),
/// located in the raw body when possible.
fn unknown_field(raw: Option<&[u8]>, path: &[crate::json::locate::Seg], parser: &str, field: &str) -> EsError {
    let candidates = match parser {
        "template" => TEMPLATE_FIELDS,
        "component_template" => COMPONENT_FIELDS,
        "index_template" => INDEX_TEMPLATE_FIELDS,
        _ => &[],
    };
    let msg = format!("[{parser}] unknown field [{field}]{}", crate::rest::params::suggest(field, candidates));
    let loc = raw.and_then(|r| crate::json::locate::key_location(r, path, field));
    match loc {
        Some((l, c)) => EsError::x_content_parse(l, c, msg),
        None => EsError::new(400, "x_content_parse_exception", msg),
    }
}

/// `_meta` must be an object (`ObjectParser` `declareObject`; `null` clears it).
fn parse_meta(raw: Option<&[u8]>, parser: &str, v: &Value) -> Result<Option<crate::json::Map>, EsError> {
    match v {
        Value::Obj(m) => Ok(Some(m.clone())),
        Value::Null => Ok(None),
        other => {
            let msg = format!("[{parser}] _meta doesn't support values of type: {}", super::aliases::token_name(other));
            let at = raw.and_then(|r| crate::json::locate::locate(r, &[crate::json::locate::Seg::Key("_meta")], crate::json::locate::Target::Value));
            Err(match at {
                Some((l, c)) => EsError::x_content_parse(l, c, msg),
                None => EsError::new(400, "x_content_parse_exception", msg),
            })
        }
    }
}

/// `MetadataIndexTemplateService.validate`: the template name's rules, first in the list.
fn validate_name(name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name.contains(' ') {
        errors.push("name must not contain a space".to_string());
    }
    if name.contains(',') {
        errors.push("name must not contain a ','".to_string());
    }
    if name.contains('#') {
        errors.push("name must not contain a '#'".to_string());
    }
    if name.contains('*') {
        errors.push("name must not contain a '*'".to_string());
    }
    if name.starts_with('_') {
        errors.push("name must not start with '_'".to_string());
    }
    if name.to_lowercase() != name {
        errors.push("name must be lower cased".to_string());
    }
    errors
}

impl Template {
    fn parse(v: &Value, raw: Option<&[u8]>, path: &[crate::json::locate::Seg]) -> Result<Template, EsError> {
        let Value::Obj(m) = v else {
            return Err(EsError::new(400, "x_content_parse_exception", "[template] template doesn't support values of type: VALUE_STRING"));
        };
        let mut t = Template::default();
        for (k, val) in m {
            match k.as_str() {
                "settings" => {
                    let mut s = Settings::from_json(val)?;
                    s.normalize_prefix("index.");
                    t.settings = Some(s);
                }
                "mappings" => {
                    if let Value::Obj(mm) = val {
                        t.mappings = Some(unwrap_type(mm));
                    }
                }
                "aliases" => t.aliases = Some(parse_aliases(val)?.into_iter().collect()),
                "lifecycle" => {
                    let mut l = obj! { "enabled" => true };
                    if let (Value::Obj(lm), Some(out)) = (val, l.as_object_mut()) {
                        for (lk, lv) in lm {
                            out.insert(lk.clone(), lv.clone());
                        }
                    }
                    t.lifecycle = Some(l);
                }
                "data_stream_options" => {
                    if val.get("failure_store").and_then(|f| f.get("lifecycle")).and_then(|l| l.get("frozen_after")).is_some_and(|v| !v.is_null()) {
                        return Err(EsError::action_request_validation(&[
                            "Failure store lifecycle does not support freezing, please remove the frozen_after configuration.".into(),
                        ]));
                    }
                    t.data_stream_options = Some(val.clone())
                }
                other => {
                    let mut p = path.to_vec();
                    p.push(crate::json::locate::Seg::Key("template"));
                    return Err(unknown_field(raw, &p, "template", other));
                }
            }
        }
        Ok(t)
    }

    pub fn to_value(&self, flat: bool) -> Value {
        let mut m = Map::new();
        if let Some(s) = &self.settings {
            m.insert("settings".into(), s.to_json(flat));
        }
        if let Some(mp) = &self.mappings {
            m.insert("mappings".into(), Value::Obj(mp.clone()));
        }
        if let Some(a) = &self.aliases {
            m.insert("aliases".into(), aliases_value(a));
        }
        if let Some(l) = &self.lifecycle {
            m.insert("lifecycle".into(), l.clone());
        }
        if let Some(d) = &self.data_stream_options {
            m.insert("data_stream_options".into(), d.clone());
        }
        Value::Obj(m)
    }
}

pub fn aliases_value(a: &BTreeMap<String, AliasMeta>) -> Value {
    let mut am = Map::new();
    for (k, v) in a {
        am.insert(k.clone(), v.to_value());
    }
    Value::Obj(am)
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComponentTemplate {
    pub template: Template,
    pub version: Option<i64>,
    pub meta: Option<Map>,
    pub deprecated: Option<bool>,
    pub created_date: i64,
    pub modified_date: i64,
}

impl ComponentTemplate {
    pub fn parse(v: &Value, raw: Option<&[u8]>, now: i64) -> Result<ComponentTemplate, EsError> {
        let m = v.as_object().cloned().unwrap_or_default();
        let mut template = None;
        let mut ct = ComponentTemplate { template: Template::default(), version: None, meta: None, deprecated: None, created_date: now, modified_date: now };
        for (k, val) in &m {
            match k.as_str() {
                "template" => template = Some(Template::parse(val, raw, &[])?),
                "version" => ct.version = val.as_i64(),
                "_meta" => ct.meta = parse_meta(raw, "component_template", val)?,
                "deprecated" => ct.deprecated = val.as_bool(),
                "created_date_millis" | "modified_date_millis" => {}
                other => return Err(unknown_field(raw, &[], "component_template", other)),
            }
        }
        ct.template = template.ok_or_else(|| EsError::illegal_argument("Required [template]"))?;
        Ok(ct)
    }

    pub fn to_value_flat(&self, flat: bool) -> Value {
        let mut m = Map::new();
        m.insert("template".into(), self.template.to_value(flat));
        if let Some(v) = self.version {
            m.insert("version".into(), v.into());
        }
        if let Some(meta) = &self.meta {
            m.insert("_meta".into(), Value::Obj(meta.clone()));
        }
        if let Some(d) = self.deprecated {
            m.insert("deprecated".into(), d.into());
        }
        m.insert("created_date_millis".into(), self.created_date.into());
        m.insert("modified_date_millis".into(), self.modified_date.into());
        Value::Obj(m)
    }

    pub fn to_value(&self) -> Value {
        self.to_value_flat(false)
    }

    pub fn from_stored(v: &Value) -> Result<ComponentTemplate, EsError> {
        let mut ct = ComponentTemplate::parse(&strip_dates(v), None, 0)?;
        ct.created_date = v.get("created_date_millis").and_then(Value::as_i64).unwrap_or(0);
        ct.modified_date = v.get("modified_date_millis").and_then(Value::as_i64).unwrap_or(0);
        Ok(ct)
    }
}

fn strip_dates(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(m) = v.as_object_mut() {
        m.shift_remove("created_date_millis");
        m.shift_remove("modified_date_millis");
    }
    v
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComposableTemplate {
    pub index_patterns: Vec<String>,
    pub template: Option<Template>,
    pub composed_of: Vec<String>,
    pub priority: Option<i64>,
    pub version: Option<i64>,
    pub meta: Option<Map>,
    pub data_stream: Option<Value>,
    pub allow_auto_create: Option<bool>,
    pub ignore_missing_component_templates: Option<Vec<String>>,
    pub deprecated: Option<bool>,
    pub created_date: i64,
    pub modified_date: i64,
}

fn string_list(v: &Value) -> Vec<String> {
    match v {
        Value::Arr(a) => a.iter().map(Value::to_es_string).collect(),
        Value::Null => Vec::new(),
        other => vec![other.to_es_string()],
    }
}

impl ComposableTemplate {
    pub fn parse(v: &Value, raw: Option<&[u8]>, now: i64) -> Result<ComposableTemplate, EsError> {
        let m = v.as_object().cloned().unwrap_or_default();
        let mut t = ComposableTemplate {
            index_patterns: Vec::new(),
            template: None,
            composed_of: Vec::new(),
            priority: None,
            version: None,
            meta: None,
            data_stream: None,
            allow_auto_create: None,
            ignore_missing_component_templates: None,
            deprecated: None,
            created_date: now,
            modified_date: now,
        };
        let mut patterns = None;
        for (k, val) in &m {
            match k.as_str() {
                "index_patterns" => patterns = Some(string_list(val)),
                "template" => t.template = Some(Template::parse(val, raw, &[])?),
                "composed_of" => t.composed_of = string_list(val),
                "priority" => t.priority = val.as_i64(),
                "version" => t.version = val.as_i64(),
                "_meta" => t.meta = parse_meta(raw, "index_template", val)?,
                "data_stream" => {
                    let hidden = val.get("hidden").and_then(Value::as_bool).unwrap_or(false);
                    let routing = val.get("allow_custom_routing").and_then(Value::as_bool).unwrap_or(false);
                    t.data_stream = Some(obj! { "hidden" => hidden, "allow_custom_routing" => routing });
                }
                "allow_auto_create" => t.allow_auto_create = val.as_bool(),
                "ignore_missing_component_templates" => t.ignore_missing_component_templates = Some(string_list(val)),
                "deprecated" => t.deprecated = val.as_bool(),
                "created_date_millis" | "modified_date_millis" => {}
                other => return Err(unknown_field(raw, &[], "index_template", other)),
            }
        }
        t.index_patterns = patterns.ok_or_else(|| EsError::illegal_argument("Required [index_patterns]"))?;
        Ok(t)
    }

    pub fn priority_or_zero(&self) -> i64 {
        self.priority.unwrap_or(0)
    }

    pub fn to_value_flat(&self, flat: bool) -> Value {
        let mut m = Map::new();
        m.insert("index_patterns".into(), Value::from(self.index_patterns.clone()));
        if let Some(t) = &self.template {
            m.insert("template".into(), t.to_value(flat));
        }
        m.insert("composed_of".into(), Value::from(self.composed_of.clone()));
        if let Some(p) = self.priority {
            m.insert("priority".into(), p.into());
        }
        if let Some(v) = self.version {
            m.insert("version".into(), v.into());
        }
        if let Some(meta) = &self.meta {
            m.insert("_meta".into(), Value::Obj(meta.clone()));
        }
        if let Some(d) = &self.data_stream {
            m.insert("data_stream".into(), d.clone());
        }
        if let Some(a) = self.allow_auto_create {
            m.insert("allow_auto_create".into(), a.into());
        }
        if let Some(i) = &self.ignore_missing_component_templates {
            m.insert("ignore_missing_component_templates".into(), Value::from(i.clone()));
        }
        if let Some(d) = self.deprecated {
            m.insert("deprecated".into(), d.into());
        }
        m.insert("created_date_millis".into(), self.created_date.into());
        m.insert("modified_date_millis".into(), self.modified_date.into());
        Value::Obj(m)
    }

    pub fn to_value(&self) -> Value {
        self.to_value_flat(false)
    }

    pub fn from_stored(v: &Value) -> Result<ComposableTemplate, EsError> {
        let mut t = ComposableTemplate::parse(&strip_dates(v), None, 0)?;
        t.created_date = v.get("created_date_millis").and_then(Value::as_i64).unwrap_or(0);
        t.modified_date = v.get("modified_date_millis").and_then(Value::as_i64).unwrap_or(0);
        Ok(t)
    }

    pub fn matches(&self, index: &str, hidden: bool) -> bool {
        self.index_patterns.iter().any(|p| (!hidden || p != "*") && glob_match(p, index))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LegacyTemplate {
    pub order: i64,
    pub version: Option<i64>,
    pub index_patterns: Vec<String>,
    pub settings: Settings,
    pub mappings: Map,
    pub aliases: BTreeMap<String, AliasMeta>,
}

impl LegacyTemplate {
    pub fn parse(v: &Value) -> Result<LegacyTemplate, EsError> {
        let m = v.as_object().cloned().unwrap_or_default();
        let mut t = LegacyTemplate { order: 0, version: None, index_patterns: Vec::new(), settings: Settings::new(), mappings: Map::new(), aliases: BTreeMap::new() };
        for (k, val) in &m {
            match k.as_str() {
                "index_patterns" | "template" => t.index_patterns = string_list(val),
                "order" => t.order = val.as_i64().unwrap_or(0),
                "version" => t.version = val.as_i64(),
                "settings" => {
                    let mut s = Settings::from_json(val)?;
                    s.normalize_prefix("index.");
                    t.settings = s;
                }
                "mappings" => {
                    if let Value::Obj(mm) = val {
                        // `{"_doc": {...}}` or `{"<type>": {...}}` or the mapping itself.
                        t.mappings = unwrap_type(mm);
                    }
                }
                "aliases" => t.aliases = parse_aliases(val)?.into_iter().collect(),
                other => return Err(EsError::parse(format!("unknown key [{other}] in the template "))),
            }
        }
        if t.index_patterns.is_empty() {
            return Err(EsError::action_request_validation(&["index patterns are missing".to_string()]));
        }
        Ok(t)
    }

    pub fn to_value(&self, flat: bool) -> Value {
        let mut m = Map::new();
        m.insert("order".into(), self.order.into());
        if let Some(v) = self.version {
            m.insert("version".into(), v.into());
        }
        m.insert("index_patterns".into(), Value::from(self.index_patterns.clone()));
        m.insert("settings".into(), self.settings.to_json(flat));
        m.insert("mappings".into(), Value::Obj(self.mappings.clone()));
        m.insert("aliases".into(), aliases_value(&self.aliases));
        Value::Obj(m)
    }

    pub fn from_stored(v: &Value) -> Result<LegacyTemplate, EsError> {
        LegacyTemplate::parse(v)
    }

    pub fn matches(&self, index: &str, hidden: bool) -> bool {
        self.index_patterns.iter().any(|p| (!hidden || p != "*") && glob_match(p, index))
    }
}

/// Whether two glob patterns (`*` wildcards) can match a common string.
pub fn globs_intersect(a: &str, b: &str) -> bool {
    // Bottom-up over suffixes, two rows at a time: `cur[j]` says whether `a[i..]` and `b[j..]`
    // can match a common string. Iterative and O(|b|) memory, since patterns are user input.
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut next = vec![false; b.len() + 1];
    let mut cur = vec![false; b.len() + 1];
    // Row i = |a|: only stars may remain on b's side.
    next[b.len()] = true;
    for j in (0..b.len()).rev() {
        next[j] = b[j] == b'*' && next[j + 1];
    }
    for i in (0..a.len()).rev() {
        // Column j = |b|: only stars may remain on a's side.
        cur[b.len()] = a[i] == b'*' && next[b.len()];
        for j in (0..b.len()).rev() {
            cur[j] = if a[i] == b'*' || b[j] == b'*' {
                // A star matches nothing (skip it) or one more character of the other side.
                next[j] || cur[j + 1]
            } else {
                a[i] == b[j] && next[j + 1]
            };
        }
        std::mem::swap(&mut cur, &mut next);
    }
    next[0]
}

/// Whether two templates' patterns overlap: Elasticsearch intersects the globs; OpenSearch
/// (since 3.6) only asks whether a pattern's "minimum string" (its stars removed) matches a
/// pattern of the other side, so `app-*-a-*` and `app-*-b-*` may share a priority.
fn patterns_overlap(d: crate::cluster::Dialect, a: &[String], b: &[String]) -> bool {
    if d == crate::cluster::Dialect::OpenSearch {
        let min_matches = |xs: &[String], ys: &[String]| xs.iter().any(|x| {
            let min = x.replace('*', "");
            ys.iter().any(|y| crate::xcontent::filter::glob_match(y, &min))
        });
        return min_matches(a, b) || min_matches(b, a);
    }
    a.iter().any(|x| b.iter().any(|y| globs_intersect(x, y)))
}

fn overlap_text(names: &[(&String, &Vec<String>)]) -> (String, String) {
    let list: Vec<&str> = names.iter().map(|(n, _)| n.as_str()).collect();
    let details: Vec<String> = names.iter().map(|(n, p)| format!("{n} => [{}]", p.join(", "))).collect();
    (list.join(","), details.join(","))
}

/// Pattern rules for templates (`MetadataIndexTemplateService.validate`).
fn validate_patterns(patterns: &[String]) -> Vec<String> {
    let mut errors = Vec::new();
    for p in patterns {
        if p.contains(' ') {
            errors.push(format!("index_patterns [{p}] must not contain a space"));
        }
        if p.contains(',') {
            errors.push(format!("index_pattern [{p}] must not contain a ','"));
        }
        if p.contains('#') {
            errors.push(format!("index_pattern [{p}] must not contain a '#'"));
        }
        if p.contains(':') {
            errors.push(format!("index_pattern [{p}] must not contain a ':'"));
        }
        if p.starts_with('_') {
            errors.push(format!("index_pattern [{p}] must not start with '_'"));
        }
        if p.chars().any(|c| matches!(c, ' ' | '"' | ',' | '/' | '<' | '>' | '?' | '\\' | '|')) {
            errors.push(format!("index_pattern [{p}] must not contain the following characters {}", super::names::INVALID_CHARS_TEXT));
        }
    }
    errors
}

/// Validates a composable template before it is stored; returns deprecation warnings.
pub fn validate_composable(md: &Metadata, name: &str, t: &ComposableTemplate, create: bool) -> Result<Vec<String>, EsError> {
    if create && md.index_templates.contains_key(name) {
        return Err(EsError::illegal_argument(format!("index template [{name}] already exists")));
    }
    if t.priority.is_some_and(|p| p < 0) {
        return Err(EsError::action_request_validation(&["index template priority must be >= 0".into()]));
    }
    let mut errors = validate_name(name);
    errors.extend(validate_patterns(&t.index_patterns));
    if !errors.is_empty() {
        return Err(EsError::invalid_index_template(name, &validation_text(&errors)));
    }
    if t.index_patterns.iter().any(|p| p == "*") {
        let composed = compose(md, t)?;
        if composed.settings.contains("index.hidden") {
            return Err(EsError::action_request_validation(&["global composable templates may not specify the setting index.hidden".into()]));
        }
    }
    let ignored = t.ignore_missing_component_templates.clone().unwrap_or_default();
    let missing: Vec<&String> = t.composed_of.iter().filter(|c| !md.component_templates.contains_key(*c) && !ignored.contains(c)).collect();
    if !missing.is_empty() {
        let list: Vec<&str> = missing.iter().map(|s| s.as_str()).collect();
        if ignored.is_empty() {
            return Err(EsError::invalid_index_template(name, &format!("index template [{name}] specifies component templates [{}] that do not exist", list.join(", "))));
        }
        return Err(EsError::invalid_index_template(
            name,
            &format!(
                "index template [{name}] specifies a missing component templates [{}] that does not exist and is not part of 'ignore_missing_component_templates'",
                list.join(", ")
            ),
        ));
    }
    // Same-priority overlaps.
    let mut same: Vec<(&String, &Vec<String>)> = md
        .index_templates
        .iter()
        .filter(|(n, o)| *n != name && o.priority_or_zero() == t.priority_or_zero() && patterns_overlap(md.dialect, &o.index_patterns, &t.index_patterns))
        .map(|(n, o)| (n, &o.index_patterns))
        .collect();
    same.sort_by(|a, b| a.0.cmp(b.0));
    if !same.is_empty() {
        let (list, details) = overlap_text(&same);
        return Err(EsError::illegal_argument(format!(
            "index template [{name}] has index patterns [{}] matching patterns from existing templates [{list}] with patterns ({details}) that have the same priority [{}], multiple index templates may not match during index creation, please use a different priority",
            t.index_patterns.join(", "),
            t.priority_or_zero()
        )));
    }
    // Lifecycle and data stream options need a data stream.
    if t.data_stream.is_none() {
        let own = t.template.as_ref();
        let parts = t.composed_of.iter().filter_map(|c| md.component_templates.get(c)).map(|c| &c.template).chain(own);
        let (mut lifecycle, mut options) = (false, false);
        for p in parts {
            lifecycle |= p.lifecycle.is_some();
        }
        // Options reset by a later template (`data_stream_options: null`) don't count.
        options |= composed_options(md, t).and_then(|o| without_resets(&o)).is_some_and(|o| o.as_object().is_none_or(|m| !m.is_empty()));
        if lifecycle {
            return Err(EsError::illegal_argument(format!(
                "index template [{name}] specifies lifecycle configuration that can only be used in combination with a data stream"
            )));
        }
        if options {
            return Err(EsError::illegal_argument(format!(
                "index template [{name}] specifies data stream options that can only be used in combination with a data stream"
            )));
        }
    }
    // Settings.
    let composed = compose(md, t)?;
    if let Err(e) = validate_user_settings_in(md.dialect, &composed.settings, false) {
        errors.push(e.reason().to_string());
        return Err(EsError::invalid_index_template(name, &validation_text(&errors)));
    }
    // Analysis and mappings after composition.
    let with = if t.composed_of.is_empty() { String::new() } else { format!("with component templates [{}] ", t.composed_of.join(", ")) };
    let invalid = || EsError::illegal_argument(format!("composable template [{name}] template after composition {with}is invalid"));
    // The index mode's setting rules, on the settings the template's indices would get.
    let mut effective = composed.settings.clone();
    if t.data_stream.is_some() {
        crate::api::indices::data_stream_index_settings(md, "validate-index-name", &mut effective, &composed.mappings, crate::time::now_millis())?;
    }
    let mode = super::index_mode::IndexMode::of(&effective);
    if let Err(e) = mode.validate_settings(&effective) {
        if mode == super::index_mode::IndexMode::TimeSeries {
            // Data stream templates are validated twice (as a template and as a data stream).
            let n = if t.data_stream.is_some() { 2 } else { 1 };
            errors.extend(std::iter::repeat_n(e.reason().to_string(), n));
            return Err(EsError::invalid_index_template(name, &validation_text(&errors)));
        }
        return Err(invalid().caused_by(e));
    }
    if let Err(e) = crate::analysis::validate_settings(&composed.settings, None) {
        return Err(invalid().caused_by(e));
    }
    let ctx = MappingCtx::from_settings_in(md.dialect, &effective);
    let mut sources = composed.mappings.clone();
    // Data stream templates are checked as their backing indices will be built.
    if let Some(ds) = &t.data_stream {
        let custom_routing = ds.get("allow_custom_routing").and_then(Value::as_bool).unwrap_or(false);
        sources.insert(0, crate::api::indices::default_timestamp_mapping(custom_routing));
        sources.push(crate::api::indices::data_stream_mapping_snippet());
    }
    if let Some(default) = mode.default_mapping(&effective) {
        sources.insert(0, default);
    }
    let checked = compose_mappings(&sources, &ctx).and_then(|m| {
        mode.validate_source(&m, &effective)?;
        mode.validate_mapping(&m, &effective, t.data_stream.is_some())
    });
    if let Err(e) = checked {
        return Err(invalid().caused_by(EsError::illegal_argument(format!("invalid composite mappings for [{name}]")).caused_by(e)));
    }
    let mut warnings = Vec::new();
    let mut legacy: Vec<(&String, &Vec<String>)> =
        md.templates.iter().filter(|(_, l)| patterns_overlap(md.dialect, &l.index_patterns, &t.index_patterns)).map(|(n, l)| (n, &l.index_patterns)).collect();
    legacy.sort_by(|a, b| a.0.cmp(b.0));
    if !legacy.is_empty() {
        let (list, details) = overlap_text(&legacy);
        warnings.push(format!(
            "index template [{name}] has index patterns [{}] matching patterns from existing older templates [{list}] with patterns ({details}); this template [{name}] will take precedence during new index creation",
            t.index_patterns.join(", ")
        ));
    }
    if t.deprecated != Some(true) {
        for c in &t.composed_of {
            if md.component_templates.get(c).is_some_and(|ct| ct.deprecated == Some(true)) {
                warnings.push(format!("index template [{name}] uses deprecated component template [{c}]"));
            }
        }
    }
    Ok(warnings)
}

fn validation_text(errors: &[String]) -> String {
    let mut s = String::from("Validation Failed: ");
    for (i, e) in errors.iter().enumerate() {
        s.push_str(&format!("{}: {e};", i + 1));
    }
    s
}

/// Validates a component template.
pub fn validate_component(md: &Metadata, name: &str, t: &ComponentTemplate, create: bool) -> Result<(), EsError> {
    if create && md.component_templates.contains_key(name) {
        return Err(EsError::illegal_argument(format!("component template [{name}] already exists")));
    }
    let errors = validate_name(name);
    if !errors.is_empty() {
        return Err(EsError::invalid_index_template(name, &validation_text(&errors)));
    }
    if let Some(s) = &t.template.settings {
        validate_user_settings_in(md.dialect, s, false)?;
        super::index_mode::IndexMode::of(s).validate_settings(s)?;
        crate::analysis::validate_settings(s, None)?;
    }
    if let Some(m) = &t.template.mappings {
        let ctx = MappingCtx::from_settings_in(md.dialect, t.template.settings.as_ref().unwrap_or(&Settings::new()));
        let parsed = Mapping::parse(m, &ctx).map_err(|e| ctx.wrap_parse_error(e))?;
        parsed.validate(&ctx)?;
    }
    // Index templates using this component must still compose.
    let mut md2 = md.clone();
    md2.component_templates.insert(name.to_string(), std::sync::Arc::new(t.clone()));
    for (tn, it) in &md.index_templates {
        if it.composed_of.iter().any(|c| c == name) {
            let composed = compose(&md2, it)?;
            let ctx = MappingCtx::from_settings_in(md.dialect, &composed.settings);
            if let Err(e) = compose_mappings(&composed.mappings, &ctx) {
                return Err(EsError::illegal_argument(format!(
                    "updating component template [{name}] results in invalid composable template [{tn}] after templates are merged"
                ))
                .caused_by(e));
            }
        }
    }
    Ok(())
}

/// Validates a legacy template.
pub fn validate_legacy(md: &Metadata, name: &str, t: &LegacyTemplate, create: bool) -> Result<(), EsError> {
    if create && md.templates.contains_key(name) {
        return Err(EsError::illegal_argument(format!("index_template [{name}] already exists")));
    }
    let mut errors = validate_name(name);
    errors.extend(validate_patterns(&t.index_patterns));
    if !errors.is_empty() {
        return Err(EsError::invalid_index_template(name, &validation_text(&errors)));
    }
    let mut overlapping: Vec<(&String, &Vec<String>)> = md
        .index_templates
        .iter()
        .filter(|(_, it)| patterns_overlap(md.dialect, &it.index_patterns, &t.index_patterns))
        .map(|(n, it)| (n, &it.index_patterns))
        .collect();
    overlapping.sort_by(|a, b| a.0.cmp(b.0));
    if !overlapping.is_empty() {
        let (list, details) = overlap_text(&overlapping);
        return Err(EsError::illegal_argument(format!(
            "legacy template [{name}] has index patterns [{}] matching patterns from existing composable templates [{list}] with patterns ({details}), use composable templates (/_index_template) instead",
            t.index_patterns.join(", ")
        )));
    }
    validate_user_settings_in(md.dialect, &t.settings, false)?;
    crate::analysis::validate_settings(&t.settings, None)?;
    if !t.mappings.is_empty() {
        let ctx = MappingCtx::from_settings_in(md.dialect, &t.settings);
        let parsed = Mapping::parse(&t.mappings, &ctx).map_err(|e| ctx.wrap_parse_error(e))?;
        parsed.validate(&ctx)?;
    }
    Ok(())
}

/// The pieces of a template composition, in application order.
#[derive(Debug, Default)]
pub struct Composed {
    pub settings: Settings,
    pub mappings: Vec<Map>,
    pub aliases: BTreeMap<String, AliasMeta>,
}

/// Composes a composable template with its component templates.
pub fn compose(md: &Metadata, t: &ComposableTemplate) -> Result<Composed, EsError> {
    let mut out = Composed::default();
    let mut add = |tpl: &Template| {
        if let Some(s) = &tpl.settings {
            out.settings.merge(s);
        }
        if let Some(m) = &tpl.mappings {
            out.mappings.push(m.clone());
        }
        if let Some(a) = &tpl.aliases {
            for (k, v) in a {
                out.aliases.insert(k.clone(), v.clone());
            }
        }
    };
    for c in &t.composed_of {
        if let Some(ct) = md.component_templates.get(c) {
            add(&ct.template);
        }
    }
    if let Some(own) = &t.template {
        add(own);
    }
    Ok(out)
}

/// The lifecycle a template composes to (component templates first, then its own).
pub fn composed_lifecycle(md: &Metadata, t: &ComposableTemplate) -> Option<Value> {
    let mut parts: Vec<&Value> = Vec::new();
    for c in &t.composed_of {
        if let Some(l) = md.component_templates.get(c).and_then(|ct| ct.template.lifecycle.as_ref()) {
            parts.push(l);
        }
    }
    if let Some(l) = t.template.as_ref().and_then(|tt| tt.lifecycle.as_ref()) {
        parts.push(l);
    }
    super::data_streams::compose_lifecycle(&parts)
}

/// The data stream options a template composes to.
pub fn composed_options(md: &Metadata, t: &ComposableTemplate) -> Option<Value> {
    let mut parts: Vec<&Value> = Vec::new();
    for c in &t.composed_of {
        if let Some(o) = md.component_templates.get(c).and_then(|ct| ct.template.data_stream_options.as_ref()) {
            parts.push(o);
        }
    }
    if let Some(o) = t.template.as_ref().and_then(|tt| tt.data_stream_options.as_ref()) {
        parts.push(o);
    }
    // `ResettableValue` composition: `null` resets what came before, objects merge field by
    // field, anything else replaces.
    fn merge(acc: &mut Map, part: &Map) {
        for (k, v) in part {
            match (acc.get_mut(k), v) {
                (Some(Value::Obj(a)), Value::Obj(b)) => merge(a, b),
                _ => {
                    acc.insert(k.clone(), v.clone());
                }
            }
        }
    }
    /// An object whose every field was reset is itself reset.
    fn collapse(v: &mut Value) {
        if let Value::Obj(m) = v {
            m.values_mut().for_each(collapse);
            if !m.is_empty() && m.values().all(Value::is_null) {
                *v = Value::Null;
            }
        }
    }
    let mut out: Option<Map> = None;
    for p in parts {
        match p {
            Value::Null => out = None,
            Value::Obj(pm) => merge(out.get_or_insert_with(Map::new), pm),
            _ => {}
        }
    }
    out.map(|m| {
        let mut v = Value::Obj(m);
        if let Value::Obj(m) = &mut v {
            m.values_mut().for_each(collapse);
        }
        v
    })
}

/// Composed options as a data stream gets them: resets dropped, the failures lifecycle with its
/// `enabled` flag.
pub fn effective_options(v: &Value) -> Option<Value> {
    let mut o = without_resets(v)?;
    if let Some(l) = o.as_object_mut().and_then(|m| m.get_mut("failure_store")).and_then(|f| f.as_object_mut()).and_then(|f| f.get_mut("lifecycle")) {
        *l = super::data_streams::lifecycle_value(l);
    }
    Some(o)
}

/// Drops reset (`null`) fields of composed options, for what a data stream actually gets.
pub fn without_resets(v: &Value) -> Option<Value> {
    match v {
        Value::Null => None,
        Value::Obj(m) => {
            let mut out = Map::new();
            for (k, x) in m {
                if let Some(c) = without_resets(x) {
                    out.insert(k.clone(), c);
                }
            }
            Some(Value::Obj(out))
        }
        other => Some(other.clone()),
    }
}

/// Merges template mappings in order (later definitions replace earlier ones).
pub fn compose_mappings(sources: &[Map], ctx: &MappingCtx) -> Result<Mapping, EsError> {
    let mut acc = Mapping::empty();
    for src in sources {
        let m = Mapping::parse(src, ctx).map_err(|e| ctx.wrap_parse_error(e))?;
        acc = acc.merge(&m, ctx, MergeReason::IndexTemplate)?;
    }
    acc.validate(ctx)?;
    Ok(acc)
}

/// The composable template that applies to a new index, if any.
pub fn find_composable<'a>(md: &'a Metadata, index: &str, hidden: bool) -> Option<(&'a String, &'a ComposableTemplate)> {
    md.index_templates
        .iter()
        .filter(|(_, t)| t.matches(index, hidden))
        .max_by(|a, b| a.1.priority_or_zero().cmp(&b.1.priority_or_zero()).then_with(|| b.0.cmp(a.0)))
        .map(|(n, t)| (n, t.as_ref()))
}

/// The legacy templates that apply to a new index, in application order (ascending `order`).
pub fn find_legacy<'a>(md: &'a Metadata, index: &str, hidden: bool) -> Vec<(&'a String, &'a LegacyTemplate)> {
    let mut v: Vec<(&String, &LegacyTemplate)> = md.templates.iter().filter(|(_, t)| t.matches(index, hidden)).map(|(n, t)| (n, t.as_ref())).collect();
    v.sort_by(|a, b| a.1.order.cmp(&b.1.order).then_with(|| b.0.cmp(a.0)));
    v
}

/// What templates contribute to a new index named `index`.
pub fn for_new_index(md: &Metadata, index: &str, hidden: bool) -> Result<Composed, EsError> {
    if let Some((_, t)) = find_composable(md, index, hidden) {
        return compose(md, t);
    }
    let mut out = Composed::default();
    for (_, t) in find_legacy(md, index, hidden) {
        out.settings.merge(&t.settings);
        if !t.mappings.is_empty() {
            out.mappings.push(t.mappings.clone());
        }
        for (k, v) in &t.aliases {
            out.aliases.insert(k.clone(), v.clone());
        }
    }
    Ok(out)
}

/// `_index_template/_simulate_index` / `_simulate`: the resolved template and the other
/// templates that would have matched.
pub fn simulate(md: &Metadata, index: Option<&str>, template: Option<(&str, &ComposableTemplate)>) -> Result<Value, EsError> {
    let mut md = md.clone();
    if let Some((n, t)) = template {
        md.index_templates.insert(n.to_string(), std::sync::Arc::new(t.clone()));
    }
    let chosen: Option<(String, ComposableTemplate)> = match (index, template) {
        (Some(i), _) => find_composable(&md, i, false).map(|(n, t)| (n.clone(), t.clone())),
        (None, Some((n, t))) => Some((n.to_string(), t.clone())),
        (None, None) => None,
    };
    let Some((chosen_name, chosen)) = chosen else {
        return Ok(obj! {});
    };
    let composed = compose(&md, &chosen)?;
    // A data stream template is simulated as what its backing indices get.
    let data_stream = chosen.data_stream.is_some();
    // Simulating an existing data stream applies its own settings and mappings too.
    let existing = index.and_then(|i| md.data_streams.get(i)).cloned();
    let mut settings = composed.settings.clone();
    if let Some(ds) = &existing {
        settings.merge(&ds.settings);
    }
    if !settings.contains("index.routing.allocation.include._tier_preference") {
        settings.set("index.routing.allocation.include._tier_preference", if data_stream { "data_hot" } else { "data_content" });
    }
    if data_stream {
        let name = index.unwrap_or(chosen_name.as_str());
        crate::api::indices::data_stream_index_settings(&md, name, &mut settings, &composed.mappings, crate::time::now_millis())?;
    }
    let ctx = MappingCtx::from_settings_in(md.dialect, &settings);
    let mut sources = composed.mappings.clone();
    if let Some(default) = super::index_mode::IndexMode::of(&settings).default_mapping(&settings) {
        sources.insert(0, default);
    }
    if data_stream {
        let custom_routing = chosen.data_stream.as_ref().and_then(|d| d.get("allow_custom_routing")).and_then(Value::as_bool).unwrap_or(false);
        sources.insert(0, crate::api::indices::default_timestamp_mapping(custom_routing));
        if let Some(m) = existing.as_ref().and_then(|ds| ds.mappings.as_ref()) {
            sources.push(m.clone());
        }
        sources.push(crate::api::indices::data_stream_mapping_snippet());
    }
    let mapping = compose_mappings(&sources, &ctx)?;
    let mut tm = Map::new();
    tm.insert("settings".into(), settings.to_nested_json());
    if !mapping.is_empty() {
        tm.insert("mappings".into(), mapping.to_value(&ctx, false));
    }
    // The resolved aliases come from a `HashMap`, with `{index}` replaced by the index name (a
    // temporary one when simulating a template on its own), as index creation does.
    let aliases = match aliases_value(&composed.aliases) {
        Value::Obj(m) => {
            let target = match index {
                Some(i) => i.to_string(),
                None => format!("simulate_template_index_{}", crate::meta::names::random_uuid().to_lowercase()),
            };
            let m: Map = m.into_iter().map(|(k, v)| (k.replace("{index}", &target), v)).collect();
            Value::Obj(crate::json::java_hash_map(m, 16))
        }
        other => other,
    };
    tm.insert("aliases".into(), aliases);
    if data_stream {
        if let Some(l) = composed_lifecycle(&md, &chosen) {
            tm.insert("lifecycle".into(), super::data_streams::lifecycle_value(&l));
        }
        if let Some(o) = composed_options(&md, &chosen).and_then(|o| effective_options(&o)) {
            tm.insert("data_stream_options".into(), o);
        }
    }
    let patterns = chosen.index_patterns.clone();
    // Templates whose patterns overlap the chosen one's: legacy ones first, then composable.
    let mut overlapping: Vec<Value> = md
        .templates
        .iter()
        .filter(|(_, t)| patterns_overlap(md.dialect, &t.index_patterns, &patterns))
        .map(|(n, t)| obj! { "name" => n.clone(), "index_patterns" => t.index_patterns.clone() })
        .collect();
    let _ = index;
    let mut composable: Vec<Value> = md
        .index_templates
        .iter()
        .filter(|(n, t)| **n != chosen_name && patterns_overlap(md.dialect, &t.index_patterns, &patterns))
        .map(|(n, t)| obj! { "name" => n.clone(), "index_patterns" => t.index_patterns.clone() })
        .collect();
    composable.sort_by(|a, b| a.get("name").and_then(Value::as_str).cmp(&b.get("name").and_then(Value::as_str)));
    overlapping.extend(composable);
    Ok(obj! { "template" => Value::Obj(tm), "overlapping" => Value::Arr(overlapping) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_intersections() {
        assert!(globs_intersect("l*", "log-*"));
        assert!(globs_intersect("*", "x"));
        assert!(globs_intersect("a*b", "*b"));
        assert!(!globs_intersect("a*", "b*"));
        assert!(!globs_intersect("log-*", "leg-*"));
        assert!(globs_intersect("*-x", "a-*"));
        assert!(!globs_intersect("abc", "abd"));
    }
}
