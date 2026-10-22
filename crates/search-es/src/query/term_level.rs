//! Term-level queries: `term`, `terms`, `terms_set`, `range`, `exists`, `prefix`, `wildcard`,
//! `regexp`, `fuzzy` and `ids`, parsed and printed as Elasticsearch's builders do.
//!
//! Values keep the JSON type they were given (`maybeConvertToBytesRef` keeps numbers and
//! booleans), since `term` on a numeric field and `terms` print them back unchanged.

use super::script::Script;
use super::xparser::{Field, Tokens};
use super::{Common, Query, Token, XParser, check_boost, check_single_field};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;

/// `term`.
#[derive(Clone, Debug, PartialEq)]
pub struct Term {
    pub field: String,
    pub value: Value,
    pub case_insensitive: bool,
    pub common: Common,
}

/// `BaseTermQueryBuilder`'s constructor checks.
fn field_and_value(field: Option<String>, value: Option<Value>, field_msg: &str, value_msg: &str) -> EsResult<(String, Value)> {
    let field = field.filter(|f| !f.is_empty()).ok_or_else(|| EsError::illegal_argument(field_msg))?;
    let value = value.ok_or_else(|| EsError::illegal_argument(value_msg))?;
    Ok((field, value))
}

impl Term {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Term> {
        let mut field: Option<String> = None;
        let mut value: Option<Value> = None;
        let mut boost = 1.0f32;
        let mut name = None;
        let mut case_insensitive = false;
        let mut current: Option<String> = None;
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().map(str::to_string),
                Token::StartObject => {
                    check_single_field(p, "term", &field, current.as_deref())?;
                    field = current.clone();
                    while let Some(t) = p.next_token() {
                        match t {
                            Token::EndObject => break,
                            Token::FieldName => current = p.current_name().map(str::to_string),
                            _ => match current.as_deref().unwrap_or("") {
                                "term" => value = p.object_bytes(),
                                "value" => {
                                    if t == Token::StartArray {
                                        return Err(p.parsing_error(
                                            "[term] query does not support arrays for value - use a bool query with multiple term clauses \
                                             in the should section or use a Terms query if scoring is not required",
                                        ));
                                    }
                                    value = p.object_bytes();
                                }
                                "_name" => name = Some(p.text()?),
                                "boost" => boost = p.float_value()?,
                                "case_insensitive" => case_insensitive = p.boolean_value()?,
                                other => return Err(p.parsing_error(format!("[term] query does not support [{other}]"))),
                            },
                        }
                    }
                }
                Token::StartArray => return Err(p.parsing_error("[term] query does not support array of values")),
                t if t.is_value() => {
                    check_single_field(p, "term", &field, p.current_name())?;
                    field = current.clone();
                    value = p.object_bytes();
                }
                _ => {}
            }
        }
        let (field, value) = field_and_value(field, value, "field name is null or empty", "value cannot be null")?;
        let mut q = Term { field, value, case_insensitive: false, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        q.case_insensitive = case_insensitive;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("value".into(), self.value.clone());
        if self.case_insensitive {
            m.insert("case_insensitive".into(), true.into());
        }
        self.common.write_if_set(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// A `terms` lookup: the terms come from a field of another document.
#[derive(Clone, Debug, PartialEq)]
pub struct TermsLookup {
    pub index: String,
    pub id: String,
    pub path: String,
    pub routing: Option<String>,
}

impl TermsLookup {
    /// `TermsLookup.parseTermsLookup` (a `ConstructingObjectParser`).
    pub fn parse(p: &mut XParser<'_>) -> EsResult<TermsLookup> {
        const FIELDS: &[Field] = &[
            Field::new("index", Tokens::STRING),
            Field::new("id", Tokens::STRING),
            Field::new("path", Tokens::STRING),
            Field::new("routing", Tokens::STRING),
        ];
        let mut vals: [Option<String>; 4] = Default::default();
        p.object("terms_lookup", FIELDS, |p, i| {
            vals[i] = Some(p.text()?);
            Ok(())
        })?;
        let missing: Vec<&str> = ["index", "id", "path"].iter().enumerate().filter(|(i, _)| vals[*i].is_none()).map(|(_, n)| *n).collect();
        if !missing.is_empty() {
            return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
        }
        let [index, id, path, routing] = vals;
        Ok(TermsLookup { index: index.unwrap_or_default(), id: id.unwrap_or_default(), path: path.unwrap_or_default(), routing })
    }

    pub fn to_map(&self, m: &mut Map) {
        m.insert("index".into(), self.index.clone().into());
        m.insert("id".into(), self.id.clone().into());
        m.insert("path".into(), self.path.clone().into());
        if let Some(r) = &self.routing {
            m.insert("routing".into(), r.clone().into());
        }
    }
}

/// `terms`: a list of values, or a lookup.
#[derive(Clone, Debug, PartialEq)]
pub struct Terms {
    pub field: String,
    pub values: Option<Vec<Value>>,
    pub lookup: Option<TermsLookup>,
    pub common: Common,
}

/// `TermsQueryBuilder.parseValues`: the array's items, each through `objectBytes`.
pub(crate) fn parse_terms_values(p: &mut XParser<'_>) -> EsResult<Vec<Value>> {
    let mut out = Vec::new();
    while let Some(t) = p.next_token() {
        if t == Token::EndArray {
            break;
        }
        match p.object_bytes() {
            Some(v) => out.push(v),
            None => return Err(p.parsing_error("No value specified for terms query")),
        }
    }
    Ok(out)
}

impl Terms {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Terms> {
        let mut field: Option<String> = None;
        let mut values = None;
        let mut lookup = None;
        let mut boost = 1.0f32;
        let mut name = None;
        let mut current: Option<String> = None;
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().map(str::to_string),
                Token::StartArray => {
                    if field.is_some() {
                        return Err(p.parsing_error("[terms] query does not support multiple fields"));
                    }
                    field = current.clone();
                    values = Some(parse_terms_values(p)?);
                }
                Token::StartObject => {
                    if let Some(f) = &field {
                        return Err(p.parsing_error(format!(
                            "[terms] query does not support more than one field. Already got: [{f}] but also found [{}]",
                            current.as_deref().unwrap_or("null")
                        )));
                    }
                    field = current.clone();
                    lookup = Some(TermsLookup::parse(p)?);
                }
                t if t.is_value() => match current.as_deref().unwrap_or("") {
                    "boost" => boost = p.float_value()?,
                    "_name" => name = Some(p.text()?),
                    other => return Err(p.parsing_error(format!("[terms] query does not support [{other}]"))),
                },
                t => {
                    return Err(p.parsing_error(format!(
                        "[terms] unknown token [{t}] after [{}]",
                        current.as_deref().unwrap_or("null")
                    )));
                }
            }
        }
        let Some(field) = field else {
            return Err(p.parsing_error(
                "[terms] query requires a field name, followed by array of terms or a document lookup specification",
            ));
        };
        if field.is_empty() {
            return Err(EsError::illegal_argument("field name cannot be null."));
        }
        if values.is_none() && lookup.is_none() {
            return Err(EsError::illegal_argument("No value or termsLookup specified for terms query"));
        }
        let mut q = Terms { field, values, lookup, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        match &self.lookup {
            Some(l) => {
                let mut lm = Map::new();
                l.to_map(&mut lm);
                m.insert(self.field.clone(), Value::Obj(lm));
            }
            None => {
                m.insert(self.field.clone(), Value::Arr(self.values.clone().unwrap_or_default()));
            }
        }
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// `terms_set`.
#[derive(Clone, Debug, PartialEq)]
pub struct TermsSet {
    pub field: String,
    pub values: Vec<Value>,
    pub minimum_should_match_field: Option<String>,
    pub minimum_should_match_script: Option<Script>,
    pub minimum_should_match: Option<String>,
    pub common: Common,
}

impl TermsSet {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<TermsSet> {
        let t = p.next_token();
        if t != Some(Token::FieldName) {
            return Err(p.parsing_error(format!("[terms_set] unknown token [{}]", t.map(Token::name).unwrap_or("null"))));
        }
        let field = p.current_name().unwrap_or("").to_string();
        let t = p.next_token();
        if t != Some(Token::StartObject) {
            return Err(p.parsing_error(format!("[terms_set] unknown token [{}]", t.map(Token::name).unwrap_or("null"))));
        }
        let mut values = Vec::new();
        let (mut msm_field, mut msm, mut msm_script) = (None, None, None);
        let mut boost = 1.0f32;
        let mut name = None;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartArray => {
                    if current == "terms" {
                        values = parse_terms_values(p)?;
                    } else {
                        return Err(p.parsing_error(format!("[terms_set] query does not support [{current}]")));
                    }
                }
                Token::StartObject => {
                    if current == "minimum_should_match_script" {
                        msm_script = Some(Script::parse(p)?);
                    } else {
                        return Err(p.parsing_error(format!("[terms_set] query does not support [{current}]")));
                    }
                }
                t if t.is_value() => match current.as_str() {
                    "minimum_should_match_field" => msm_field = Some(p.text()?),
                    "minimum_should_match" => msm = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    "_name" => name = Some(p.text()?),
                    _ => return Err(p.parsing_error(format!("[terms_set] query does not support [{current}]"))),
                },
                t => return Err(p.parsing_error(format!("[terms_set] unknown token [{t}] after [{current}]"))),
            }
        }
        let t = p.next_token();
        if t != Some(Token::EndObject) {
            return Err(p.parsing_error(format!("[terms_set] unknown token [{}]", t.map(Token::name).unwrap_or("null"))));
        }
        let mut q = TermsSet {
            field,
            values,
            minimum_should_match_field: None,
            minimum_should_match_script: None,
            minimum_should_match: None,
            common: Common::default(),
        };
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        let exclusive = |q: &TermsSet, what: &str| -> EsResult<()> {
            let set = |f: bool| f as u8;
            let others = match what {
                "field" => set(q.minimum_should_match_script.is_some()) + set(q.minimum_should_match.is_some()),
                "script" => set(q.minimum_should_match_field.is_some()) + set(q.minimum_should_match.is_some()),
                _ => set(q.minimum_should_match_field.is_some()) + set(q.minimum_should_match_script.is_some()),
            };
            if others > 0 {
                return Err(EsError::illegal_argument(match what {
                    "field" => "A script or value has already been specified. Cannot specify both a field and a script or value",
                    "script" => "A field or value has already been specified. Cannot specify both a script and a field or value",
                    _ => "A field or script has already been specified. Cannot specify both a value and a script or field",
                }));
            }
            Ok(())
        };
        if let Some(f) = msm_field {
            exclusive(&q, "field")?;
            q.minimum_should_match_field = Some(f);
        }
        if let Some(v) = msm {
            exclusive(&q, "value")?;
            q.minimum_should_match = Some(v);
        }
        if let Some(s) = msm_script {
            exclusive(&q, "script")?;
            q.minimum_should_match_script = Some(s);
        }
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("terms".into(), Value::Arr(self.values.clone()));
        super::opt_str(&mut m, "minimum_should_match_field", &self.minimum_should_match_field);
        if let Some(s) = &self.minimum_should_match_script {
            m.insert("minimum_should_match_script".into(), s.to_value());
        }
        super::opt_str(&mut m, "minimum_should_match", &self.minimum_should_match);
        self.common.write_always(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// Range relations (`ShapeRelation`), as printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    Intersects,
    Disjoint,
    Within,
    Contains,
}

impl Relation {
    /// `ShapeRelation.getRelationByName`: case-insensitive.
    pub fn by_name(s: &str) -> Option<Relation> {
        match s.to_lowercase().as_str() {
            "intersects" => Some(Relation::Intersects),
            "disjoint" => Some(Relation::Disjoint),
            "within" => Some(Relation::Within),
            "contains" => Some(Relation::Contains),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Relation::Intersects => "intersects",
            Relation::Disjoint => "disjoint",
            Relation::Within => "within",
            Relation::Contains => "contains",
        }
    }
}

/// `range`.
#[derive(Clone, Debug, PartialEq)]
pub struct Range {
    pub field: String,
    pub from: Option<Value>,
    pub to: Option<Value>,
    pub include_lower: bool,
    pub include_upper: bool,
    /// The zone's id as `ZoneId.getId()` prints it.
    pub time_zone: Option<String>,
    pub format: Option<String>,
    pub relation: Option<Relation>,
    pub common: Common,
}

/// `ZoneId.of(id).getId()`, with Java's exceptions as `(kind, message)`.
pub fn java_zone_id(id: &str) -> Result<String, (&'static str, String)> {
    fn offset_id(s: &str) -> Result<i32, (&'static str, String)> {
        let invalid = || ("date_time_exception", format!("Invalid ID for ZoneOffset, invalid format: {s}"));
        let b = s.as_bytes();
        if b.is_empty() || !(b[0] == b'+' || b[0] == b'-') {
            return Err(invalid());
        }
        let sign = if b[0] == b'-' { -1 } else { 1 };
        let rest = &s[1..];
        let digits = |t: &str| -> Option<i32> { (t.len() == 2 && t.bytes().all(|c| c.is_ascii_digit())).then(|| t.parse().ok()).flatten() };
        let (h, m, sec) = match rest.len() {
            1 if rest.as_bytes()[0].is_ascii_digit() => ((rest.as_bytes()[0] - b'0') as i32, 0, 0),
            2 => (digits(rest).ok_or_else(invalid)?, 0, 0),
            4 => (digits(&rest[..2]).ok_or_else(invalid)?, digits(&rest[2..]).ok_or_else(invalid)?, 0),
            5 if &rest[2..3] == ":" => (digits(&rest[..2]).ok_or_else(invalid)?, digits(&rest[3..]).ok_or_else(invalid)?, 0),
            6 => (
                digits(&rest[..2]).ok_or_else(invalid)?,
                digits(&rest[2..4]).ok_or_else(invalid)?,
                digits(&rest[4..]).ok_or_else(invalid)?,
            ),
            8 if &rest[2..3] == ":" && &rest[5..6] == ":" => (
                digits(&rest[..2]).ok_or_else(invalid)?,
                digits(&rest[3..5]).ok_or_else(invalid)?,
                digits(&rest[6..]).ok_or_else(invalid)?,
            ),
            _ => return Err(invalid()),
        };
        if h > 18 {
            return Err(("date_time_exception", format!("Zone offset hours not in valid range: value {h} is not in the range -18 to 18")));
        }
        if m > 59 {
            return Err(("date_time_exception", format!("Zone offset minutes not in valid range: value {m} is not in the range -59 to 59")));
        }
        if sec > 59 {
            return Err(("date_time_exception", format!("Zone offset seconds not in valid range: value {sec} is not in the range -59 to 59")));
        }
        let total = sign * (h * 3600 + m * 60 + sec);
        if total.abs() > 18 * 3600 {
            return Err(("date_time_exception", "Zone offset not in valid range: -18:00 to +18:00".to_string()));
        }
        Ok(total)
    }
    fn offset_text(total: i32) -> String {
        if total == 0 {
            return "Z".into();
        }
        let a = total.abs();
        let (h, m, s) = (a / 3600, (a / 60) % 60, a % 60);
        let sign = if total < 0 { '-' } else { '+' };
        if s != 0 { format!("{sign}{h:02}:{m:02}:{s:02}") } else { format!("{sign}{h:02}:{m:02}") }
    }
    if id == "Z" {
        return Ok("Z".into());
    }
    if id.len() <= 1 || id.starts_with(['+', '-']) {
        return offset_id(id).map(offset_text);
    }
    if matches!(id, "UTC" | "GMT" | "UT") {
        return Ok(id.into());
    }
    for prefix in ["UTC", "GMT", "UT"] {
        if let Some(rest) = id.strip_prefix(prefix)
            && rest.starts_with(['+', '-'])
        {
            let total = offset_id(rest)?;
            return Ok(if total == 0 { prefix.to_string() } else { format!("{prefix}{}", offset_text(total)) });
        }
    }
    let valid_region = {
        let b = id.as_bytes();
        b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1..].iter().all(|c| c.is_ascii_alphanumeric() || b"~/._+-".contains(c))
    };
    if !valid_region {
        return Err(("date_time_exception", format!("Invalid ID for region-based ZoneId, invalid format: {id}")));
    }
    match crate::time::Zone::parse(id) {
        Ok(_) => Ok(id.to_string()),
        Err(_) => Err(("zone_rules_exception", format!("Unknown time-zone ID: {id}"))),
    }
}

/// A Java exception wrapped in `IllegalArgumentException(cause)`: the message is the cause's
/// `toString()`.
pub fn wrap_java(kind: &str, msg: &str) -> EsError {
    let class = match kind {
        "zone_rules_exception" => "java.time.zone.ZoneRulesException",
        _ => "java.time.DateTimeException",
    };
    EsError::illegal_argument(format!("{class}: {msg}")).caused_by(EsError::new(400, kind.to_string(), msg.to_string()))
}

/// `DateFormatter.forPattern`'s validation, with Elasticsearch's texts.
pub fn validate_date_format(format: &str) -> EsResult<()> {
    if format.is_empty() {
        return Err(EsError::illegal_argument("No date pattern provided"));
    }
    crate::time::DateFormatter::new(format).map(|_| ()).map_err(|e| {
        let cause = e.split_once("]: ").map(|(_, c)| c.to_string()).unwrap_or_else(|| e.clone());
        EsError::illegal_argument(e).caused_by(EsError::illegal_argument(cause))
    })
}

impl Range {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Range> {
        let mut field: Option<String> = None;
        let (mut from, mut to) = (None, None);
        let (mut include_lower, mut include_upper) = (true, true);
        let (mut time_zone, mut format, mut relation) = (None, None, None);
        let mut boost = 1.0f32;
        let mut name = None;
        let mut current: Option<String> = None;
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().map(str::to_string),
                Token::StartObject => {
                    check_single_field(p, "range", &field, current.as_deref())?;
                    field = current.clone();
                    while let Some(t) = p.next_token() {
                        match t {
                            Token::EndObject => break,
                            Token::FieldName => current = p.current_name().map(str::to_string),
                            _ => match current.as_deref().unwrap_or("") {
                                "boost" => boost = p.float_value()?,
                                "gt" => {
                                    from = p.object_bytes();
                                    include_lower = false;
                                }
                                "gte" => {
                                    from = p.object_bytes();
                                    include_lower = true;
                                }
                                "lt" => {
                                    to = p.object_bytes();
                                    include_upper = false;
                                }
                                "lte" => {
                                    to = p.object_bytes();
                                    include_upper = true;
                                }
                                "time_zone" => time_zone = Some(p.text()?),
                                "format" => format = Some(p.text()?),
                                "relation" => relation = Some(p.text()?),
                                "_name" => name = Some(p.text()?),
                                other => return Err(p.parsing_error(format!("[range] query does not support [{other}]"))),
                            },
                        }
                    }
                }
                t if t.is_value() => {
                    return Err(p.parsing_error(format!("[range] query does not support [{}]", current.as_deref().unwrap_or("null"))));
                }
                _ => {}
            }
        }
        let field = field.filter(|f| !f.is_empty()).ok_or_else(|| EsError::illegal_argument("field name is null or empty"))?;
        let mut q = Range {
            field,
            from,
            to,
            include_lower,
            include_upper,
            time_zone: None,
            format: None,
            relation: None,
            common: Common::default(),
        };
        if let Some(tz) = time_zone {
            q.time_zone = Some(java_zone_id(&tz).map_err(|(k, m)| wrap_java(k, &m))?);
        }
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        if let Some(f) = format {
            validate_date_format(&f)?;
            q.format = Some(f);
        }
        if let Some(r) = relation {
            let rel = Relation::by_name(&r).ok_or_else(|| EsError::illegal_argument(format!("{r} is not a valid relation")))?;
            if rel == Relation::Disjoint {
                return Err(EsError::illegal_argument(format!("[range] query does not support relation [{r}]")));
            }
            q.relation = Some(rel);
        }
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        if let Some(f) = &self.from {
            m.insert(if self.include_lower { "gte" } else { "gt" }.into(), f.clone());
        }
        if let Some(t) = &self.to {
            m.insert(if self.include_upper { "lte" } else { "lt" }.into(), t.clone());
        }
        super::opt_str(&mut m, "time_zone", &self.time_zone);
        if let Some(f) = self.format.as_ref().filter(|f| !f.trim().is_empty()) {
            m.insert("format".into(), f.clone().into());
        }
        if let Some(r) = self.relation {
            m.insert("relation".into(), r.name().into());
        }
        self.common.write_always(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// `exists`.
#[derive(Clone, Debug, PartialEq)]
pub struct Exists {
    pub field: String,
    pub common: Common,
}

impl Exists {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Exists> {
        let mut field = None;
        let mut boost = 1.0f32;
        let mut name = None;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                t if t.is_value() => match current.as_str() {
                    "field" => field = Some(p.text()?),
                    "_name" => name = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    _ => return Err(p.parsing_error(format!("[exists] query does not support [{current}]"))),
                },
                t => return Err(p.parsing_error(format!("[exists] unknown token [{t}] after [{current}]"))),
            }
        }
        let Some(field) = field else {
            return Err(p.parsing_error("[exists] must be provided with a [field]"));
        };
        if field.is_empty() {
            return Err(EsError::illegal_argument("field name is null or empty"));
        }
        let mut q = Exists { field, common: Common::default() };
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("field".into(), self.field.clone().into());
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// The shape shared by `prefix`, `wildcard` and `regexp`: a field with a text value and options.
fn parse_text_query<'a>(
    p: &mut XParser<'a>,
    query: &str,
    mut option: impl FnMut(&mut XParser<'a>, &str) -> EsResult<bool>,
) -> EsResult<(Option<String>, Option<String>)> {
    let mut field: Option<String> = None;
    let mut value: Option<String> = None;
    let mut current: Option<String> = None;
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => current = p.current_name().map(str::to_string),
            Token::StartObject => {
                check_single_field(p, query, &field, current.as_deref())?;
                field = current.clone();
                while let Some(t) = p.next_token() {
                    match t {
                        Token::EndObject => break,
                        Token::FieldName => current = p.current_name().map(str::to_string),
                        _ => {
                            let key = current.clone().unwrap_or_default();
                            let is_value = match query {
                                "wildcard" => key == "wildcard" || key == "value",
                                _ => key == "value",
                            };
                            if is_value {
                                value = if query == "wildcard" { Some(p.text()?) } else { p.text_or_null()? };
                            } else if !option(p, &key)? {
                                return Err(p.parsing_error(format!("[{query}] query does not support [{key}]")));
                            }
                        }
                    }
                }
            }
            _ => {
                check_single_field(p, query, &field, p.current_name())?;
                field = if query == "wildcard" { p.current_name().map(str::to_string) } else { current.clone() };
                value = if query == "wildcard" { Some(p.text()?) } else { p.text_or_null()? };
            }
        }
    }
    Ok((field, value))
}

/// `prefix`.
#[derive(Clone, Debug, PartialEq)]
pub struct Prefix {
    pub field: String,
    pub value: String,
    pub rewrite: Option<String>,
    pub case_insensitive: bool,
    pub common: Common,
}

/// Options common to the multi-term queries.
#[derive(Default)]
struct MultiTermOpts {
    boost: Option<f32>,
    name: Option<String>,
    rewrite: Option<String>,
    case_insensitive: bool,
}

impl MultiTermOpts {
    fn accept(&mut self, p: &mut XParser<'_>, key: &str) -> EsResult<bool> {
        match key {
            "_name" => self.name = Some(p.text()?),
            "boost" => self.boost = Some(p.float_value()?),
            "rewrite" => self.rewrite = p.text_or_null()?,
            "case_insensitive" => self.case_insensitive = p.boolean_value()?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

impl Prefix {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Prefix> {
        let mut o = MultiTermOpts::default();
        let (field, value) = parse_text_query(p, "prefix", |p, k| o.accept(p, k))?;
        let field = field.filter(|f| !f.is_empty()).ok_or_else(|| EsError::illegal_argument("field name is null or empty"))?;
        let value = value.ok_or_else(|| EsError::illegal_argument("value cannot be null"))?;
        let mut q = Prefix { field, value, rewrite: o.rewrite, case_insensitive: false, common: Common::default() };
        q.common.boost = check_boost(o.boost.unwrap_or(1.0), || q.clone().into())?;
        q.common.name = o.name;
        q.case_insensitive = o.case_insensitive;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("value".into(), self.value.clone().into());
        super::opt_str(&mut m, "rewrite", &self.rewrite);
        if self.case_insensitive {
            m.insert("case_insensitive".into(), true.into());
        }
        self.common.write_always(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// `wildcard`.
#[derive(Clone, Debug, PartialEq)]
pub struct Wildcard {
    pub field: String,
    pub value: String,
    pub rewrite: Option<String>,
    pub case_insensitive: bool,
    pub common: Common,
}

impl Wildcard {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Wildcard> {
        let mut o = MultiTermOpts::default();
        let (field, value) = parse_text_query(p, "wildcard", |p, k| o.accept(p, k))?;
        let field = field.filter(|f| !f.is_empty()).ok_or_else(|| EsError::illegal_argument("field name is null or empty"))?;
        let value = value.ok_or_else(|| EsError::illegal_argument("value cannot be null"))?;
        let mut q = Wildcard { field, value, rewrite: o.rewrite, case_insensitive: false, common: Common::default() };
        q.common.boost = check_boost(o.boost.unwrap_or(1.0), || q.clone().into())?;
        q.common.name = o.name;
        q.case_insensitive = o.case_insensitive;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("wildcard".into(), self.value.clone().into());
        super::opt_str(&mut m, "rewrite", &self.rewrite);
        if self.case_insensitive {
            m.insert("case_insensitive".into(), true.into());
        }
        self.common.write_always(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// Lucene `RegExp` syntax flags.
pub mod regexp_flags {
    pub const INTERSECTION: i32 = 0x0001;
    pub const COMPLEMENT: i32 = 0x10000;
    pub const EMPTY: i32 = 0x0004;
    pub const ANYSTRING: i32 = 0x0008;
    pub const INTERVAL: i32 = 0x0020;
    pub const NONE: i32 = 0;
    /// `RegExp.ALL`.
    pub const LUCENE_ALL: i32 = 0xff;
    /// `RegexpFlag.ALL`: Lucene's `ALL | DEPRECATED_COMPLEMENT`.
    pub const ALL: i32 = 0xff | 0x10000;

    /// `RegexpFlag.resolveValue`.
    pub fn resolve(flags: Option<&str>) -> Result<i32, String> {
        let Some(flags) = flags.filter(|f| !f.is_empty()) else { return Ok(LUCENE_ALL) };
        let mut magic = NONE;
        for s in flags.split('|') {
            if s.is_empty() {
                continue;
            }
            let v = match s.to_uppercase().as_str() {
                "INTERSECTION" => INTERSECTION,
                "COMPLEMENT" => COMPLEMENT,
                "EMPTY" => EMPTY,
                "ANYSTRING" => ANYSTRING,
                "INTERVAL" => INTERVAL,
                "NONE" => continue,
                "ALL" => return Ok(ALL),
                _ => return Err(format!("Unknown regexp flag [{s}]")),
            };
            magic |= v;
        }
        Ok(magic)
    }
}

/// `regexp`.
#[derive(Clone, Debug, PartialEq)]
pub struct Regexp {
    pub field: String,
    pub value: String,
    pub flags: i32,
    pub max_determinized_states: i32,
    pub rewrite: Option<String>,
    pub case_insensitive: bool,
    pub common: Common,
}

impl Regexp {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Regexp> {
        let mut o = MultiTermOpts::default();
        let mut flags = regexp_flags::ALL;
        let mut max_states = 10000;
        let (field, value) = parse_text_query(p, "regexp", |p, k| {
            match k {
                "flags" => {
                    let f = p.text_or_null()?;
                    flags = regexp_flags::resolve(f.as_deref()).map_err(EsError::illegal_argument)?;
                }
                "max_determinized_states" => max_states = p.int_value()?,
                "flags_value" => flags = p.int_value()?,
                _ => return o.accept(p, k),
            }
            Ok(true)
        })?;
        let field = field.filter(|f| !f.is_empty()).ok_or_else(|| EsError::illegal_argument("field name is null or empty"))?;
        let value = value.ok_or_else(|| EsError::illegal_argument("value cannot be null"))?;
        let mut q = Regexp {
            field,
            value,
            flags,
            max_determinized_states: max_states,
            rewrite: o.rewrite,
            case_insensitive: false,
            common: Common::default(),
        };
        q.common.boost = check_boost(o.boost.unwrap_or(1.0), || q.clone().into())?;
        q.common.name = o.name;
        q.case_insensitive = o.case_insensitive;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("value".into(), self.value.clone().into());
        m.insert("flags_value".into(), self.flags.into());
        if self.case_insensitive {
            m.insert("case_insensitive".into(), true.into());
        }
        m.insert("max_determinized_states".into(), self.max_determinized_states.into());
        super::opt_str(&mut m, "rewrite", &self.rewrite);
        self.common.write_always(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// `Fuzziness`: an edit distance or `AUTO[:low,high]`.
#[derive(Clone, Debug, PartialEq)]
pub enum Fuzziness {
    Edits(u8),
    Auto { low: i32, high: i32 },
}

impl Fuzziness {
    pub const AUTO: Fuzziness = Fuzziness::Auto { low: 3, high: 6 };

    /// `Fuzziness.fromEdits`.
    pub fn from_edits(n: i32) -> EsResult<Fuzziness> {
        match n {
            0..=2 => Ok(Fuzziness::Edits(n as u8)),
            _ => Err(EsError::illegal_argument(format!("Valid edit distances are [0, 1, 2] but was [{n}]"))),
        }
    }

    /// `Fuzziness.fromString`.
    pub fn from_text(s: &str) -> EsResult<Fuzziness> {
        if s.is_empty() {
            return Err(EsError::illegal_argument("fuzziness cannot be null or empty."));
        }
        let upper = s.to_uppercase();
        if upper == "AUTO" {
            return Ok(Fuzziness::AUTO);
        }
        if let Some(rest) = upper.strip_prefix("AUTO:") {
            let parts: Vec<&str> = rest.split(',').collect();
            // Java's String.split drops trailing empty strings.
            let mut parts = parts;
            while parts.len() > 1 && parts.last() == Some(&"") {
                parts.pop();
            }
            if parts.len() != 2 {
                return Err(EsError::parse("failed to find low and high distance values"));
            }
            let (Some(low), Some(high)) = (crate::units::parse_java_int(parts[0]), crate::units::parse_java_int(parts[1])) else {
                return Err(EsError::parse(format!("failed to parse [{upper}] as a \"auto:int,int\"")));
            };
            if low < 0 || high < 0 || low > high {
                return Err(EsError::parse(format!(
                    "fuzziness wrongly configured [{upper}]. Must be 0 < lower value <= higher value."
                )));
            }
            return Ok(Fuzziness::Auto { low, high });
        }
        match super::xparser::java_parse_float(&upper) {
            Ok(f) => {
                if f % 1.0 > 0.0 {
                    return Err(EsError::illegal_argument(format!(
                        "fuzziness needs to be one of 0.0, 1.0 or 2.0 but was {}",
                        crate::json::write::java_float(f)
                    )));
                }
                Fuzziness::from_edits(f as i32)
            }
            Err(e) => Err(EsError::illegal_argument(format!("fuzziness cannot be [{s}].")).caused_by(e)),
        }
    }

    /// `Fuzziness.parse`: a string or a number.
    pub fn parse(p: &XParser<'_>) -> EsResult<Fuzziness> {
        match p.token() {
            Token::String => Fuzziness::from_text(&p.text()?),
            Token::Number => Fuzziness::from_edits(p.int_value()?),
            t => Err(EsError::illegal_argument(format!("Can't parse fuzziness on token: [{t}]"))),
        }
    }

    /// `asString()`.
    pub fn as_string(&self) -> String {
        match self {
            Fuzziness::Edits(n) => n.to_string(),
            Fuzziness::Auto { low: 3, high: 6 } => "AUTO".into(),
            Fuzziness::Auto { low, high } => format!("AUTO:{low},{high}"),
        }
    }
}

/// `fuzzy`.
#[derive(Clone, Debug, PartialEq)]
pub struct Fuzzy {
    pub field: String,
    pub value: Value,
    pub fuzziness: Fuzziness,
    pub prefix_length: i32,
    pub max_expansions: i32,
    pub transpositions: bool,
    pub rewrite: Option<String>,
    pub common: Common,
}

impl Fuzzy {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Fuzzy> {
        let mut field: Option<String> = None;
        let mut value = None;
        let mut fuzziness = Fuzziness::AUTO;
        let (mut prefix_length, mut max_expansions, mut transpositions) = (0, 50, true);
        let mut rewrite = None;
        let mut boost = 1.0f32;
        let mut name = None;
        let mut current: Option<String> = None;
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().map(str::to_string),
                Token::StartObject => {
                    check_single_field(p, "fuzzy", &field, current.as_deref())?;
                    field = current.clone();
                    while let Some(t) = p.next_token() {
                        let key = current.clone().unwrap_or_default();
                        match t {
                            Token::EndObject => break,
                            Token::FieldName => current = p.current_name().map(str::to_string),
                            t if t.is_value() => match key.as_str() {
                                "term" | "value" => value = p.object_bytes(),
                                "boost" => boost = p.float_value()?,
                                "fuzziness" => fuzziness = Fuzziness::parse(p)?,
                                "prefix_length" => prefix_length = p.int_value()?,
                                "max_expansions" => max_expansions = p.int_value()?,
                                "transpositions" => transpositions = p.boolean_value()?,
                                "rewrite" => rewrite = p.text_or_null()?,
                                "_name" => name = Some(p.text()?),
                                _ => return Err(p.parsing_error(format!("[fuzzy] query does not support [{key}]"))),
                            },
                            t => return Err(p.parsing_error(format!("[fuzzy] unexpected token [{t}] after [{key}]"))),
                        }
                    }
                }
                _ => {
                    check_single_field(p, "fuzzy", &field, p.current_name())?;
                    field = p.current_name().map(str::to_string);
                    value = p.object_bytes();
                }
            }
        }
        let (field, value) = field_and_value(field, value, "field name cannot be null or empty", "query value cannot be null")?;
        let mut q = Fuzzy { field, value, fuzziness, prefix_length, max_expansions, transpositions, rewrite, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("value".into(), self.value.clone());
        m.insert("fuzziness".into(), self.fuzziness.as_string().into());
        m.insert("prefix_length".into(), self.prefix_length.into());
        m.insert("max_expansions".into(), self.max_expansions.into());
        m.insert("transpositions".into(), self.transpositions.into());
        super::opt_str(&mut m, "rewrite", &self.rewrite);
        self.common.write_always(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// `ids`.
#[derive(Clone, Debug, PartialEq)]
pub struct Ids {
    /// Ids in a sorted set, as Elasticsearch keeps them.
    pub ids: Vec<String>,
    pub common: Common,
}

impl Ids {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Ids> {
        const FIELDS: &[Field] = &[Field::new("values", Tokens::STRING_ARRAY), Field::new("boost", Tokens::FLOAT), Field::new("_name", Tokens::STRING)];
        let mut q = Ids { ids: Vec::new(), common: Common::default() };
        let r = p.object("ids", FIELDS, |p, i| {
            match i {
                0 => {
                    let vals = p.array_items(|p| p.text())?;
                    for v in vals {
                        if let Err(pos) = q.ids.binary_search(&v) {
                            q.ids.insert(pos, v);
                        }
                    }
                }
                1 => {
                    let b = p.float_value()?;
                    q.common.boost = check_boost(b, || q.clone().into())?;
                }
                _ => q.common.name = Some(p.text()?),
            }
            Ok(())
        });
        if let Err(e) = r {
            if super::xparser::is_illegal_argument(&e) {
                return Err(p.parsing_error(e.reason().to_string()).caused_by(e));
            }
            return Err(e);
        }
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("values".into(), Value::Arr(self.ids.iter().map(|s| Value::Str(s.clone())).collect()));
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// A query from a term-level builder, for callers that build queries directly.
pub fn term(field: &str, value: impl Into<Value>) -> Query {
    Term { field: field.into(), value: value.into(), case_insensitive: false, common: Common::default() }.into()
}
