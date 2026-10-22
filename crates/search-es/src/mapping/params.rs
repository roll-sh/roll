//! Field mapper parameters, modelled on Elasticsearch's `FieldMapper.Parameter`.
//!
//! Every field type declares its parameters in serialization order ([`super::types`]). A
//! parameter has a kind (how its JSON is parsed and validated), a default (constant, an index
//! setting, or derived from another parameter), a merge rule (whether a mapping update may change
//! it) and a serialization rule. As in Elasticsearch:
//!
//! - a parameter is *set* when the mapping names it, and *configured* when it is set to a value
//!   different from its default; by default only configured parameters are printed;
//! - "explicit" parameters (`coerce`, `ignore_malformed`, …) print whenever they are set;
//! - merges compare the effective values of non-updateable parameters and collect every
//!   conflict as `Cannot update parameter [x] from [a] to [b]`.

use super::MappingCtx;
use crate::error::EsError;
use crate::json::write::java_double;
use crate::json::{Map, Number, Value};
use crate::units::parse_java_double;

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Bool,
    /// An integer in `[min, max]`; `msg` formats the range error (`{v}` is the value).
    Int { min: i64, max: i64, msg: Option<&'static str> },
    /// A double; `positive` requires > 0.
    Double { positive: bool },
    Str,
    /// One of the options (`Unknown value [x] for field [p] - accepted values are [a, b]`).
    Enum(&'static [&'static str]),
    /// An analyzer name that must resolve.
    Analyzer,
    Similarity,
    Normalizer,
    /// `meta`: up to 5 string entries.
    Meta,
    /// A script (string or object), normalized to `{source, lang}`.
    Script,
    /// `null_value` of a numeric type, parsed as that type.
    NullNumber(NumKind),
    NullBool,
    NullIp,
    /// A date `format` (`||`-separated named formats or java-time patterns).
    Format,
    Locale,
    /// Any JSON value, kept as given (objects validated by the type hook).
    Json,
    /// `doc_values`: a boolean (the object form is only for columnar index modes).
    DocValues,
    /// A list of strings (a single string is accepted as one entry).
    StrList,
    /// `fielddata_frequency_filter`.
    FreqFilter,
    /// `index_prefixes`.
    IndexPrefixes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumKind {
    Long,
    Integer,
    Short,
    Byte,
    Double,
    Float,
    HalfFloat,
    ScaledFloat,
    UnsignedLong,
}

impl NumKind {
    pub fn from_type(t: &str) -> Option<NumKind> {
        Some(match t {
            "long" => NumKind::Long,
            "integer" => NumKind::Integer,
            "short" => NumKind::Short,
            "byte" => NumKind::Byte,
            "double" => NumKind::Double,
            "float" => NumKind::Float,
            "half_float" => NumKind::HalfFloat,
            "scaled_float" => NumKind::ScaledFloat,
            "unsigned_long" => NumKind::UnsignedLong,
            _ => return None,
        })
    }
}

/// A parameter's default.
#[derive(Clone, Copy, Debug)]
pub enum Dv {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'static str),
    EmptyObj,
    EmptyList,
    /// The effective value of another parameter of the same field.
    Param(&'static str),
    /// A boolean index setting (`index.mapping.coerce`, …).
    SettingBool(SettingDefault),
    /// `index.mapping.ignore_above`.
    IgnoreAbove,
    /// Computed by a type-specific function.
    Fn(fn(&super::FieldMapper, &MappingCtx) -> Value),
}

#[derive(Clone, Copy, Debug)]
pub enum SettingDefault {
    Coerce,
    IgnoreMalformed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(unpredictable_function_pointer_comparisons)]
pub enum Merge {
    /// Can't change once set.
    Fixed,
    Updatable,
    /// `norms`: may go from `true` to `false` only.
    TrueToFalse,
    /// A type-specific rule over the effective values (`dense_vector` `index_options`), with
    /// the way the conflict prints them.
    Custom { allowed: fn(&Value, &Value) -> bool, show: fn(&Value) -> String },
    /// A merge validator (`Parameter.setMergeValidator`) over the effective values: it may add
    /// its own conflict lines; a refusal also adds the generic `Cannot update parameter` line,
    /// with the values printed by `show`.
    Check { check: fn(&Value, &Value, &mut Vec<String>) -> bool, show: fn(&Value) -> String },
}

#[derive(Clone, Copy, Debug)]
pub enum Ser {
    /// When configured (set and different from the default).
    Configured,
    Always,
    /// Whenever set (Elasticsearch's `Explicit` values).
    WhenSet,
    /// Accepted but never printed, even with `include_defaults`.
    Never,
    /// Only when the value isn't `null`, even with `include_defaults`.
    NonNull,
    /// A type-specific check.
    Fn(fn(&super::FieldMapper, &MappingCtx) -> bool),
}

#[derive(Debug)]
pub struct ParamDef {
    pub name: &'static str,
    pub kind: Kind,
    pub default: Dv,
    pub merge: Merge,
    pub ser: Ser,
    pub accepts_null: bool,
}

/// Shorthand constructors for the type tables.
pub const fn p(name: &'static str, kind: Kind, default: Dv, merge: Merge) -> ParamDef {
    ParamDef { name, kind, default, merge, ser: Ser::Configured, accepts_null: false }
}

impl ParamDef {
    pub const fn ser(mut self, s: Ser) -> ParamDef {
        self.ser = s;
        self
    }
    pub const fn nullable(mut self) -> ParamDef {
        self.accepts_null = true;
        self
    }
}

pub const TRUE: Dv = Dv::Bool(true);
pub const FALSE: Dv = Dv::Bool(false);

/// `XContentMapValues.nodeBooleanValue(node)`: booleans, or the strings `true`/`false`.
pub fn node_bool(v: &Value) -> Result<bool, EsError> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => {
            let s = other.to_es_string();
            match s.as_str() {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(EsError::illegal_argument(format!("Failed to parse value [{s}] as only [true] or [false] are allowed."))),
            }
        }
    }
}

/// `XContentMapValues.nodeBooleanValue(node, name)`.
pub fn node_bool_named(v: &Value, name: &str) -> Result<bool, EsError> {
    node_bool(v).map_err(|e| EsError::illegal_argument(format!("Could not convert [{name}] to boolean")).caused_by(e))
}

fn nfe(s: &str) -> EsError {
    EsError::java(400, "number_format_exception", format!("For input string: \"{s}\""))
}

/// `XContentMapValues.nodeIntegerValue`: numbers (truncated) or numeric strings.
pub fn node_int(v: &Value) -> Result<i64, EsError> {
    match v {
        Value::Num(Number::Int(i)) => Ok(*i),
        Value::Num(Number::Float(f)) => Ok(*f as i64),
        Value::Num(n) => Ok(n.as_f64() as i64),
        other => {
            let s = other.to_es_string();
            crate::units::parse_java_long(&s).ok_or_else(|| nfe(&s))
        }
    }
}

pub fn node_double(v: &Value) -> Result<f64, EsError> {
    match v {
        Value::Num(n) => Ok(n.as_f64()),
        other => {
            let s = other.to_es_string();
            parse_java_double(&s).ok_or_else(|| nfe(&s))
        }
    }
}

/// Parses a parameter value. `field` is the field's leaf name, `ty` its type.
pub fn parse_value(def: &ParamDef, field: &str, type_name: &str, v: &Value, ctx: &MappingCtx) -> Result<Value, EsError> {
    let name = def.name;
    Ok(match def.kind {
        Kind::Bool => Value::Bool(node_bool(v)?),
        Kind::Int { min, max, msg } => {
            let n = node_int(v)?;
            if n < min || n > max {
                let m = match msg {
                    Some(m) => m.replace("{v}", &n.to_string()),
                    None => format!("[{name}] must be between [{min}] and [{max}], got [{n}]"),
                };
                if name == "dims" || name == "position_increment_gap" {
                    return Err(EsError::mapper_parsing(m));
                }
                return Err(EsError::illegal_argument(m));
            }
            Value::from(n)
        }
        Kind::Double { positive } => {
            let d = node_double(v)?;
            // NaN fails too, as in Java.
            if positive && d.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
                return Err(EsError::illegal_argument(format!("[{name}] must be a positive number, got [{}]", java_double(d))));
            }
            Value::from(d)
        }
        Kind::Str => match v {
            Value::Null => Value::Null,
            other => Value::Str(other.to_es_string()),
        },
        Kind::Enum(options) => {
            let s = v.to_es_string();
            if !options.contains(&s.as_str()) {
                return Err(EsError::mapper_parsing(format!(
                    "Unknown value [{s}] for field [{name}] - accepted values are [{}]",
                    options.join(", ")
                )));
            }
            Value::Str(s)
        }
        Kind::Analyzer => {
            let s = v.to_es_string();
            if !ctx.has_analyzer(&s) {
                return Err(EsError::illegal_argument(format!("analyzer [{s}] has not been configured in mappings")));
            }
            Value::Str(s)
        }
        Kind::Similarity => match v {
            Value::Null => Value::Null,
            other => {
                let s = other.to_es_string();
                if !ctx.has_similarity(&s) {
                    return Err(EsError::mapper_parsing(format!("Unknown Similarity type [{s}] for field [{field}]")));
                }
                Value::Str(s)
            }
        },
        Kind::Normalizer => match v {
            Value::Null => Value::Null,
            other => {
                let s = other.to_es_string();
                if !ctx.has_normalizer(&s) {
                    return Err(EsError::mapper_parsing(format!("normalizer [{s}] not found for field [{field}]")));
                }
                Value::Str(s)
            }
        },
        Kind::Meta => parse_meta(field, v)?,
        Kind::Script => match v {
            Value::Null => Value::Null,
            other => parse_script(field, other)?,
        },
        Kind::NullNumber(NumKind::UnsignedLong) => match v {
            Value::Null => Value::Null,
            // Kept as given (printed as a string), checked as `UnsignedLongFieldMapper` does.
            other => match crate::document::values::parse_unsigned(other) {
                Ok(_) => Value::Str(other.to_es_string()),
                // A double past the range fails outside the parameter's own error wrapping.
                Err(e) if matches!(other, Value::Num(Number::Float(d)) if d.fract() == 0.0) => return Err(e),
                Err(e) => {
                    let reason = e.reason().to_string();
                    return Err(EsError::mapper_parsing(format!("Error parsing [null_value] on field [{field}]: {reason}")).caused_by(e));
                }
            },
        },
        Kind::NullNumber(k) => match v {
            Value::Null => Value::Null,
            other => parse_number(k, other, false)?,
        },
        Kind::NullBool => match v {
            Value::Null => Value::Null,
            other => Value::Bool(node_bool(other)?),
        },
        Kind::NullIp => match v {
            Value::Null => Value::Null,
            other => {
                let s = other.to_es_string();
                if s.parse::<std::net::IpAddr>().is_err() {
                    return Err(EsError::mapper_parsing(format!("Error parsing [null_value] on field [{field}]: '{s}' is not an IP string literal.")));
                }
                Value::Str(s)
            }
        },
        Kind::Format => {
            let s = match v {
                Value::Str(s) => s.clone(),
                other => return Err(EsError::illegal_argument(format!("Invalid format: [{}]: expected string value", other.to_es_string()))),
            };
            if let Err(e) = crate::time::validate_format(&s) {
                return Err(EsError::illegal_argument(format!("Error parsing [{name}] on field [{field}]: {e}")).caused_by(EsError::illegal_argument(e)));
            }
            Value::Str(s)
        }
        Kind::Locale => Value::Str(parse_locale_checked(&v.to_es_string())?),
        Kind::Json => v.clone(),
        Kind::DocValues => {
            let extended = extended_doc_values(type_name);
            match v {
                Value::Obj(m) if extended && ctx.columnar.is_some() => {
                    let flag = |k: &str, d: bool| -> Result<bool, EsError> {
                        match m.get(k) {
                            None => Ok(d),
                            Some(x) => node_bool(x),
                        }
                    };
                    columnar_doc_values(flag("multi_value", ctx.dv_multi_value)?, flag("nullability", ctx.dv_nullability)?)
                }
                Value::Obj(_) if extended => {
                    return Err(EsError::mapper_parsing(format!(
                        "unsupported doc_values configuration for field [{field}] in non-columnar index; supported values: [true, false]"
                    )));
                }
                Value::Obj(m) => {
                    return Err(EsError::illegal_argument(format!(
                        "Failed to parse value [{}] as only [true] or [false] are allowed.",
                        crate::json::java_map_string(m)
                    )));
                }
                other => {
                    let b = node_bool(other)?;
                    // `true` in a columnar index is the default column configuration.
                    if b && extended && ctx.columnar.is_some() { columnar_doc_values(ctx.dv_multi_value, ctx.dv_nullability) } else { Value::Bool(b) }
                }
            }
        }
        Kind::StrList => match v {
            Value::Arr(a) => Value::Arr(a.iter().map(|x| Value::Str(x.to_es_string())).collect()),
            Value::Null => Value::Arr(Vec::new()),
            other => Value::Arr(vec![Value::Str(other.to_es_string())]),
        },
        Kind::FreqFilter => parse_freq_filter(v)?,
        Kind::IndexPrefixes => match v {
            Value::Null => Value::Null,
            other => parse_index_prefixes(field, other)?,
        },
    })
}

/// Types whose `doc_values` take the object form (`{multi_value, nullability}`) in strict
/// columnar indices.
pub fn extended_doc_values(type_name: &str) -> bool {
    matches!(
        type_name,
        "keyword" | "text" | "match_only_text" | "boolean" | "date" | "date_nanos" | "ip" | "long" | "integer" | "short" | "byte" | "double"
            | "float" | "half_float" | "scaled_float" | "unsigned_long"
    )
}

/// The object form of `doc_values` in strict columnar indices.
pub fn columnar_doc_values(multi_value: bool, nullability: bool) -> Value {
    crate::obj! { "multi_value" => multi_value, "nullability" => nullability }
}

/// `TypeParsers.parseMeta`.
pub fn parse_meta(field: &str, v: &Value) -> Result<Value, EsError> {
    let Value::Obj(m) = v else {
        return Err(EsError::mapper_parsing(format!(
            "[meta] must be an object, got {}[{}] for field [{field}]",
            java_simple_class(v),
            v.to_es_string()
        )));
    };
    if m.len() > 5 {
        return Err(EsError::mapper_parsing(format!("[meta] can't have more than 5 entries, but got {} on field [{field}]", m.len())));
    }
    for k in m.keys() {
        if k.chars().count() > 20 {
            return Err(EsError::mapper_parsing(format!("[meta] keys can't be longer than 20 chars, but got [{k}] for field [{field}]")));
        }
    }
    for val in m.values() {
        match val {
            Value::Str(s) if s.chars().count() > 50 => {
                return Err(EsError::mapper_parsing(format!("[meta] values can't be longer than 50 chars, but got [{s}] for field [{field}]")));
            }
            Value::Str(_) => {}
            Value::Null => return Err(EsError::mapper_parsing(format!("[meta] values can't be null (field [{field}])"))),
            other => {
                return Err(EsError::mapper_parsing(format!(
                    "[meta] values can only be strings, but got {}[{}] for field [{field}]",
                    java_simple_class(other),
                    other.to_es_string()
                )));
            }
        }
    }
    // Sorted, as Elasticsearch stores meta in a TreeMap.
    let mut sorted: Vec<(&String, &Value)> = m.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = Map::new();
    for (k, v) in sorted {
        out.insert(k.clone(), v.clone());
    }
    Ok(Value::Obj(out))
}

/// The Java class Jackson's map parser produces for a value (`Integer`, `String`, …).
pub fn java_simple_class(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "Boolean",
        Value::Num(Number::Int(i)) if i32::try_from(*i).is_ok() => "Integer",
        Value::Num(Number::Int(_)) => "Long",
        Value::Num(Number::UInt(_)) | Value::Num(Number::Big(_)) => "BigInteger",
        Value::Num(Number::Float(_)) => "Double",
        Value::Str(_) => "String",
        Value::Arr(_) => "ArrayList",
        Value::Obj(_) => "LinkedHashMap",
    }
}

/// `Script.parse`: a string is an inline painless script; objects take `source`, `lang`,
/// `params`, `options` (`id` is a stored script, which mapping scripts don't allow).
pub fn parse_script(field: &str, v: &Value) -> Result<Value, EsError> {
    match v {
        Value::Str(s) => Ok(crate::obj! { "source" => s.clone(), "lang" => "painless" }),
        Value::Obj(m) => {
            if m.contains_key("id") {
                return Err(EsError::illegal_argument(format!("stored scripts are not supported on field [{field}]")));
            }
            let mut out = Map::new();
            let source = m.get("source").or_else(|| m.get("inline"));
            match source {
                Some(Value::Str(s)) => {
                    out.insert("source".into(), Value::Str(s.clone()));
                }
                Some(Value::Obj(_)) => {
                    out.insert("source".into(), source.cloned().unwrap());
                }
                Some(other) => {
                    out.insert("source".into(), Value::Str(other.to_es_string()));
                }
                None => return Err(EsError::illegal_argument("must specify either [source] for an inline script or [id] for a stored script")),
            }
            out.insert("lang".into(), m.get("lang").cloned().unwrap_or_else(|| Value::from("painless")));
            for k in ["options", "params"] {
                if let Some(v) = m.get(k).filter(|v| v.as_object().is_some_and(|o| !o.is_empty())) {
                    out.insert(k.into(), v.clone());
                }
            }
            for k in m.keys() {
                if !matches!(k.as_str(), "source" | "inline" | "lang" | "params" | "options") {
                    return Err(EsError::new(400, "parse_exception", format!("[script] unknown field [{k}]")));
                }
            }
            Ok(Value::Obj(out))
        }
        other => Err(EsError::new(400, "parse_exception", format!("expected a string value or an object, but found [{}] instead", other.kind_name()))),
    }
}

/// `LocaleUtils.parse`: `en`, `en_US`, `en-US`, `und` (root). Printed as Java's `Locale.toString()`.
pub fn parse_locale(s: &str) -> String {
    let parts: Vec<&str> = s.split(['_', '-']).collect();
    match parts.as_slice() {
        [""] | ["ROOT"] | ["root"] | ["und"] => String::new(),
        [l] => l.to_lowercase(),
        [l, c] => format!("{}_{}", l.to_lowercase(), c.to_uppercase()),
        [l, c, v, ..] => format!("{}_{}_{}", l.to_lowercase(), c.to_uppercase(), v),
        [] => String::new(),
    }
}

/// ISO 639-1 codes `Locale.getISO3Language` knows (plus Java's legacy `in`, `iw`, `ji`, `mo`).
const ISO_LANGUAGES: &str = "aaabaeafakamanarasavayazbabebgbhbibmbnbobrbscacechcocrcscucvcydadedvdzeeeleneoeseteufafffifjfofrfygagdglgngugvhahehihohrhthuhyhziaidieigiiikinioisitiuiwjajijvkakgkikjkkklkmknkokrkskukvkwkylalblglilnloltlulvmgmhmimkmlmnmomrmsmtmynanbndnengnlnnnonrnvnyocojomorospapiplpsptqurmrnrorurwsascsdsesgsiskslsmsnsosqsrssstsusvswtatetgthtitktltntotrtstttwtyugukuruzvevivowawoxhyiyozazhzu";

/// ISO 3166-1 alpha-2 codes `Locale.getISO3Country` knows (plus Java's legacy `AN`, `CS`).
const ISO_COUNTRIES: &str = "ADAEAFAGAIALAMANAOAQARASATAUAWAXAZBABBBDBEBFBGBHBIBJBLBMBNBOBQBRBSBTBVBWBYBZCACCCDCFCGCHCICKCLCMCNCOCRCSCUCVCWCXCYCZDEDJDKDMDODZECEEEGEHERESETFIFJFKFMFOFRGAGBGDGEGFGGGHGIGLGMGNGPGQGRGSGTGUGWGYHKHMHNHRHTHUIDIEILIMINIOIQIRISITJEJMJOJPKEKGKHKIKMKNKPKRKWKYKZLALBLCLILKLRLSLTLULVLYMAMCMDMEMFMGMHMKMLMMMNMOMPMQMRMSMTMUMVMWMXMYMZNANCNENFNGNINLNONPNRNUNZOMPAPEPFPGPHPKPLPMPNPRPSPTPWPYQARERORSRURWSASBSCSDSESGSHSISJSKSLSMSNSOSRSSSTSVSXSYSZTCTDTFTGTHTJTKTLTMTNTOTRTTTVTWTZUAUGUMUSUYUZVAVCVEVGVIVNVUWFWSYEYTZAZMZW";

fn in_pairs(table: &str, code: &str) -> bool {
    code.len() == 2 && table.as_bytes().chunks(2).any(|c| c == code.as_bytes())
}

/// [`parse_locale`] with `LocaleUtils.parse`'s checks: at most three parts, separated
/// consistently by the first separator; the language and country must have ISO-3 codes.
pub fn parse_locale_checked(s: &str) -> Result<String, EsError> {
    let sep = s.chars().find(|c| *c == '-' || *c == '_').unwrap_or('-');
    let parts: Vec<&str> = s.split(sep).collect();
    if parts.len() > 3 {
        return Err(EsError::illegal_argument(format!("Locales can have at most 3 parts but got {}: [{}]", parts.len(), parts.join(", "))));
    }
    if parts.len() == 1 && parts[0].eq_ignore_ascii_case("ROOT") {
        return Ok(String::new());
    }
    let lang = parts[0].to_lowercase();
    if !(lang.is_empty() || lang.chars().count() == 3 || in_pairs(ISO_LANGUAGES, &lang)) {
        return Err(EsError::illegal_argument(format!("Unknown language: {}", parts[0]))
            .caused_by(EsError::java(400, "missing_resource_exception", format!("Couldn't find 3-letter language code for {lang}"))));
    }
    if let Some(country) = parts.get(1) {
        let c = country.to_uppercase();
        if !(c.is_empty() || c.chars().count() == 3 || in_pairs(ISO_COUNTRIES, &c)) {
            return Err(EsError::illegal_argument(format!("Unknown country: {country}"))
                .caused_by(EsError::java(400, "missing_resource_exception", format!("Couldn't find 3-letter country code for {c}"))));
        }
    }
    Ok(parse_locale(s))
}

fn parse_freq_filter(v: &Value) -> Result<Value, EsError> {
    let Value::Obj(m) = v else {
        return Err(EsError::mapper_parsing("[fielddata_frequency_filter] must be an object"));
    };
    let min = m.get("min").map(node_double).transpose()?.unwrap_or(0.0);
    let max = m.get("max").map(node_double).transpose()?.unwrap_or(i32::MAX as f64);
    let mss = m.get("min_segment_size").map(node_int).transpose()?.unwrap_or(0);
    for k in m.keys() {
        if !matches!(k.as_str(), "min" | "max" | "min_segment_size") {
            return Err(EsError::mapper_parsing(format!("unknown parameter [{k}] on fielddata_frequency_filter")));
        }
    }
    Ok(crate::obj! { "min" => min, "max" => max, "min_segment_size" => mss })
}

fn parse_index_prefixes(field: &str, v: &Value) -> Result<Value, EsError> {
    let Value::Obj(m) = v else {
        return Err(EsError::mapper_parsing("[index_prefixes] must be an object"));
    };
    let min = m.get("min_chars").map(node_int).transpose()?.unwrap_or(2);
    let max = m.get("max_chars").map(node_int).transpose()?.unwrap_or(5);
    for k in m.keys() {
        if !matches!(k.as_str(), "min_chars" | "max_chars") {
            return Err(EsError::mapper_parsing(format!("Unknown parameter [{k}] on index_prefixes for field [{field}]")));
        }
    }
    if min > max {
        return Err(EsError::illegal_argument(format!("min_chars [{min}] must be less than max_chars [{max}]")));
    }
    if min < 1 {
        return Err(EsError::illegal_argument(format!("min_chars [{min}] must be greater than zero")));
    }
    if max >= 20 {
        return Err(EsError::illegal_argument(format!("max_chars [{max}] must be less than 20")));
    }
    Ok(crate::obj! { "min_chars" => min, "max_chars" => max })
}

/// Parses a number for a numeric field type the way `NumberType.parse(value, coerce=false)`
/// does for `null_value`: strings are parsed, integers must not have a decimal part, values
/// must be in range.
pub fn parse_number(k: NumKind, v: &Value, coerce: bool) -> Result<Value, EsError> {
    let as_double = |v: &Value| -> Result<f64, EsError> {
        match v {
            Value::Num(n) => Ok(n.as_f64()),
            Value::Bool(_) | Value::Arr(_) | Value::Obj(_) | Value::Null => {
                let s = v.to_es_string();
                Err(nfe(&s))
            }
            Value::Str(s) => parse_java_double(s).ok_or_else(|| nfe(s)),
        }
    };
    match k {
        NumKind::Long | NumKind::Integer | NumKind::Short | NumKind::Byte => {
            let (min, max, name): (i64, i64, &str) = match k {
                NumKind::Long => (i64::MIN, i64::MAX, "long"),
                NumKind::Integer => (i32::MIN as i64, i32::MAX as i64, "integer"),
                NumKind::Short => (i16::MIN as i64, i16::MAX as i64, "short"),
                _ => (i8::MIN as i64, i8::MAX as i64, "byte"),
            };
            let i = match v {
                Value::Num(Number::Int(i)) => *i,
                Value::Str(s) => match crate::units::parse_java_long(s.trim()) {
                    Some(i) => i,
                    None => {
                        let d = parse_java_double(s).ok_or_else(|| nfe(s))?;
                        integer_from_double(d, s, coerce, name)?
                    }
                },
                Value::Num(Number::Float(d)) => integer_from_double(*d, &java_double(*d), coerce, name)?,
                Value::Num(Number::UInt(_)) | Value::Num(Number::Big(_)) => {
                    return Err(EsError::illegal_argument(format!("Value [{}] is out of range for a {name}", v.to_es_string())));
                }
                other => return Err(nfe(&other.to_es_string())),
            };
            if i < min || i > max {
                return Err(EsError::illegal_argument(format!("Value [{i}] is out of range for {}", article(name))));
            }
            Ok(Value::from(i))
        }
        NumKind::Double | NumKind::ScaledFloat => {
            let d = as_double(v)?;
            if !d.is_finite() {
                return Err(EsError::illegal_argument(format!("[double] supports only finite values, but got [{}]", java_double(d))));
            }
            Ok(Value::from(d))
        }
        NumKind::Float | NumKind::HalfFloat => {
            let d = as_double(v)?;
            let mut f = d as f32;
            // Half floats keep the nearest half-precision value; past 65504 that is infinity.
            if k == NumKind::HalfFloat && f.is_finite() {
                let h = crate::document::values::half_float(f);
                if !h.is_finite() {
                    return Err(EsError::illegal_argument(format!("[half_float] supports only finite values, but got [{}]", crate::json::write::java_float(f))));
                }
                f = h;
            }
            if !f.is_finite() {
                let tn = if k == NumKind::Float { "float" } else { "half_float" };
                return Err(EsError::illegal_argument(format!("[{tn}] supports only finite values, but got [{}]", crate::json::write::java_float(f))));
            }
            Ok(Value::from(f as f64))
        }
        NumKind::UnsignedLong => {
            let s = v.to_es_string();
            match s.parse::<u64>() {
                Ok(u) => Ok(Value::from(u)),
                Err(_) => Err(EsError::illegal_argument(format!("Value \"{s}\" has a decimal part"))),
            }
        }
    }
}

fn article(name: &str) -> String {
    if name.starts_with(['a', 'e', 'i', 'o', 'u']) { format!("an {name}") } else { format!("a {name}") }
}

fn integer_from_double(d: f64, shown: &str, coerce: bool, name: &str) -> Result<i64, EsError> {
    if !coerce && d.fract() != 0.0 {
        return Err(EsError::illegal_argument(format!("Value [{shown}] has a decimal part")));
    }
    if d.is_nan() || d.is_infinite() || !(-9.223372036854776E18..9.223372036854776E18).contains(&d) {
        return Err(EsError::illegal_argument(format!("Value [{shown}] is out of range for {}", article(name))));
    }
    Ok(d.trunc() as i64)
}

/// Java's `toString()` of a parameter value for conflict messages.
pub fn conflict_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Obj(m) => crate::json::java_map_string(m),
        other => other.to_es_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj;

    #[test]
    fn meta_rules() {
        assert!(parse_meta("f", &obj! { "a" => "b" }).is_ok());
        assert_eq!(parse_meta("f", &obj! { "a" => 1 }).unwrap_err().reason(), "[meta] values can only be strings, but got Integer[1] for field [f]");
        assert_eq!(parse_meta("f", &Value::from("x")).unwrap_err().reason(), "[meta] must be an object, got String[x] for field [f]");
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_number(NumKind::Integer, &Value::from("12"), false).unwrap(), Value::from(12));
        assert_eq!(parse_number(NumKind::Long, &Value::from(1.5), false).unwrap_err().reason(), "Value [1.5] has a decimal part");
        assert_eq!(parse_number(NumKind::Long, &Value::from("x"), false).unwrap_err().reason(), "For input string: \"x\"");
        assert_eq!(parse_number(NumKind::Byte, &Value::from(300), false).unwrap_err().reason(), "Value [300] is out of range for a byte");
        assert_eq!(parse_number(NumKind::Integer, &Value::from(1i64 << 40), false).unwrap_err().reason(), "Value [1099511627776] is out of range for an integer");
    }

    #[test]
    fn locales() {
        assert_eq!(parse_locale("en-us"), "en_US");
        assert_eq!(parse_locale("de"), "de");
        assert_eq!(parse_locale("und"), "");
        assert_eq!(parse_locale_checked("en-US").unwrap(), "en_US");
        assert_eq!(parse_locale_checked("en_US_POSIX").unwrap(), "en_US_POSIX");
        assert_eq!(parse_locale_checked("ROOT").unwrap(), "");
        assert_eq!(parse_locale_checked("fil").unwrap(), "fil");
        assert_eq!(parse_locale_checked("xx").unwrap_err().reason(), "Unknown language: xx");
        assert_eq!(parse_locale_checked("en_YY").unwrap_err().reason(), "Unknown country: YY");
        assert_eq!(parse_locale_checked("en_US_POSIX_X").unwrap_err().reason(), "Locales can have at most 3 parts but got 4: [en, US, POSIX, X]");
    }

    #[test]
    fn scripts() {
        assert_eq!(parse_script("f", &Value::from("emit(1)")).unwrap().to_json(), r#"{"source":"emit(1)","lang":"painless"}"#);
        assert!(parse_script("f", &obj! { "id" => "x" }).is_err());
    }
}
