//! Span queries: `span_term`, `span_multi`, `span_first`, `span_near` (with `span_gap`),
//! `span_or`, `span_not`, `span_containing`, `span_within` and `span_field_masking`.
//!
//! Clauses must themselves be span queries, and a nested clause may not carry a boost; both are
//! checked while parsing, with Elasticsearch's texts.

use super::{Common, Query, Token, XParser, check_boost, check_single_field, parse_inner};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;

/// Whether a query may be used as a span clause (`SpanQueryBuilder`).
pub fn is_span(q: &Query) -> bool {
    matches!(
        q,
        Query::SpanTerm(_)
            | Query::SpanMulti(_)
            | Query::SpanFirst(_)
            | Query::SpanNear(_)
            | Query::SpanOr(_)
            | Query::SpanNot(_)
            | Query::SpanContaining(_)
            | Query::SpanWithin(_)
            | Query::SpanFieldMasking(_)
            | Query::SpanGap(_)
    )
}

/// `SpanQueryBuilderUtil.checkNoBoost`.
fn check_no_boost(p: &XParser<'_>, query: &str, field: &str, clause: &Query) -> EsResult<()> {
    if matches!(clause, Query::SpanGap(_)) {
        return Ok(());
    }
    if clause.boost() != 1.0 {
        return Err(p.parsing_error(format!(
            "{query} [{field}] as a nested span clause can't have non-default boost value [{}]",
            crate::json::write::java_float(clause.boost())
        )));
    }
    Ok(())
}

/// Parses a clause that must be a span query.
fn span_clause(p: &mut XParser<'_>, query: &str, field: &str) -> EsResult<Query> {
    let q = parse_inner(p)?;
    if !is_span(&q) {
        return Err(p.parsing_error(format!("{query} [{field}] must be of type span query")));
    }
    check_no_boost(p, query, field, &q)?;
    Ok(q)
}

/// `span_term`.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanTerm {
    pub field: String,
    pub value: Value,
    pub common: Common,
}

impl SpanTerm {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<SpanTerm> {
        let mut field: Option<String> = None;
        let mut value = None;
        let (mut boost, mut name) = (1.0f32, None);
        let mut current: Option<String> = None;
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().map(str::to_string),
                Token::StartObject => {
                    check_single_field(p, "span_term", &field, current.as_deref())?;
                    field = current.clone();
                    while let Some(t) = p.next_token() {
                        match t {
                            Token::EndObject => break,
                            Token::FieldName => current = p.current_name().map(str::to_string),
                            _ => match current.as_deref().unwrap_or("") {
                                "term" | "value" => value = p.object_bytes(),
                                "boost" => boost = p.float_value()?,
                                "_name" => name = Some(p.text()?),
                                other => return Err(p.parsing_error(format!("[span_term] query does not support [{other}]"))),
                            },
                        }
                    }
                }
                _ => {
                    check_single_field(p, "span_term", &field, p.current_name())?;
                    field = p.current_name().map(str::to_string);
                    value = p.object_bytes();
                }
            }
        }
        let field = field.filter(|f| !f.is_empty()).ok_or_else(|| EsError::illegal_argument("field name is null or empty"))?;
        let value = value.ok_or_else(|| EsError::illegal_argument("value cannot be null"))?;
        let mut q = SpanTerm { field, value, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("value".into(), self.value.clone());
        self.common.write_if_set(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// `span_gap` (only meaningful inside `span_near`).
#[derive(Clone, Debug, PartialEq)]
pub struct SpanGap {
    pub field: String,
    pub width: i32,
    pub common: Common,
}

impl SpanGap {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<SpanGap> {
        let mut field: Option<String> = None;
        let mut width = None;
        let mut current: Option<String> = None;
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().map(str::to_string),
                _ => {
                    check_single_field(p, "span_gap", &field, current.as_deref())?;
                    field = current.clone();
                    width = Some(p.int_value()?);
                }
            }
        }
        let field = field.filter(|f| !f.is_empty()).ok_or_else(|| EsError::illegal_argument("[span_gap] field name is null or empty"))?;
        let width = width.ok_or_else(|| EsError::illegal_argument("[span_gap] width is required"))?;
        if width <= 0 {
            return Err(EsError::illegal_argument("[span_gap] width must be > 0"));
        }
        Ok(SpanGap { field, width, common: Common::default() })
    }

    pub fn inner(&self) -> Value {
        obj! { self.field.clone() => self.width }
    }
}

/// `span_multi`: a multi-term query as a span.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanMulti {
    pub query: Query,
    pub common: Common,
}

impl SpanMulti {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<SpanMulti> {
        let mut sub = None;
        let (mut boost, mut name) = (1.0f32, None);
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => {
                    if current == "match" {
                        let q = parse_inner(p)?;
                        if !matches!(q, Query::Prefix(_) | Query::Wildcard(_) | Query::Regexp(_) | Query::Fuzzy(_) | Query::Range(_)) {
                            return Err(p.parsing_error("[span_multi] [match] must be of type multi term query"));
                        }
                        sub = Some(q);
                    } else {
                        return Err(p.parsing_error(format!("[span_multi] query does not support [{current}]")));
                    }
                }
                t if t.is_value() => match current.as_str() {
                    "_name" => name = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    _ => return Err(p.parsing_error(format!("[span_multi] query does not support [{current}]"))),
                },
                _ => {}
            }
        }
        let Some(query) = sub else { return Err(p.parsing_error("[span_multi] must have [match] multi term query clause")) };
        let mut q = SpanMulti { query, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("match".into(), self.query.to_value());
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// `span_first`.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanFirst {
    pub query: Query,
    pub end: i32,
    pub common: Common,
}

impl SpanFirst {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<SpanFirst> {
        let mut m = None;
        let mut end = None;
        let (mut boost, mut name) = (1.0f32, None);
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => {
                    if current == "match" {
                        m = Some(span_clause(p, "span_first", "match")?);
                    } else {
                        return Err(p.parsing_error(format!("[span_first] query does not support [{current}]")));
                    }
                }
                _ => match current.as_str() {
                    "boost" => boost = p.float_value()?,
                    "end" => end = Some(p.int_value()?),
                    "_name" => name = Some(p.text()?),
                    _ => return Err(p.parsing_error(format!("[span_first] query does not support [{current}]"))),
                },
            }
        }
        let Some(query) = m else { return Err(p.parsing_error("span_first must have [match] span query clause")) };
        let Some(end) = end else { return Err(p.parsing_error("span_first must have [end] set for it")) };
        if end < 0 {
            return Err(EsError::illegal_argument("parameter [end] needs to be positive."));
        }
        let mut q = SpanFirst { query, end, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("match".into(), self.query.to_value());
        m.insert("end".into(), self.end.into());
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// Parses a `clauses` array of span queries.
fn span_clauses(p: &mut XParser<'_>, query: &str) -> EsResult<Vec<Query>> {
    let mut out = Vec::new();
    while let Some(t) = p.next_token() {
        if t == Token::EndArray {
            break;
        }
        out.push(span_clause(p, query, "clauses")?);
    }
    Ok(out)
}

/// `span_near`.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanNear {
    pub clauses: Vec<Query>,
    pub slop: i32,
    pub in_order: bool,
    pub common: Common,
}

impl SpanNear {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<SpanNear> {
        let mut clauses = Vec::new();
        let (mut slop, mut in_order) = (0, true);
        let (mut boost, mut name) = (1.0f32, None);
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartArray => {
                    if current == "clauses" {
                        clauses = span_clauses(p, "span_near")?;
                    } else {
                        return Err(p.parsing_error(format!("[span_near] query does not support [{current}]")));
                    }
                }
                t if t.is_value() => match current.as_str() {
                    "in_order" => in_order = p.boolean_value()?,
                    "slop" => slop = p.int_value()?,
                    "boost" => boost = p.float_value()?,
                    "_name" => name = Some(p.text()?),
                    _ => return Err(p.parsing_error(format!("[span_near] query does not support [{current}]"))),
                },
                _ => return Err(p.parsing_error(format!("[span_near] query does not support [{current}]"))),
            }
        }
        if clauses.is_empty() {
            return Err(p.parsing_error("span_near must include [clauses]"));
        }
        let mut q = SpanNear { clauses, slop, in_order, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("clauses".into(), Value::Arr(self.clauses.iter().map(Query::to_value).collect()));
        m.insert("slop".into(), self.slop.into());
        m.insert("in_order".into(), self.in_order.into());
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// `span_or`.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanOr {
    pub clauses: Vec<Query>,
    pub common: Common,
}

impl SpanOr {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<SpanOr> {
        let mut clauses = Vec::new();
        let (mut boost, mut name) = (1.0f32, None);
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartArray => {
                    if current == "clauses" {
                        clauses = span_clauses(p, "span_or")?;
                    } else {
                        return Err(p.parsing_error(format!("[span_or] query does not support [{current}]")));
                    }
                }
                _ => match current.as_str() {
                    "_name" => name = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    _ => return Err(p.parsing_error(format!("[span_or] query does not support [{current}]"))),
                },
            }
        }
        if clauses.is_empty() {
            return Err(p.parsing_error("span_or must include [clauses]"));
        }
        let mut q = SpanOr { clauses, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("clauses".into(), Value::Arr(self.clauses.iter().map(Query::to_value).collect()));
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// `span_not`.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanNot {
    pub include: Query,
    pub exclude: Query,
    pub pre: i32,
    pub post: i32,
    pub common: Common,
}

impl SpanNot {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<SpanNot> {
        let (mut include, mut exclude) = (None, None);
        let (mut dist, mut pre, mut post) = (None, None, None);
        let (mut boost, mut name) = (1.0f32, None);
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => match current.as_str() {
                    "include" => include = Some(span_clause(p, "span_not", "include")?),
                    "exclude" => exclude = Some(span_clause(p, "span_not", "exclude")?),
                    _ => return Err(p.parsing_error(format!("[span_not] query does not support [{current}]"))),
                },
                _ => match current.as_str() {
                    "dist" => dist = Some(p.int_value()?),
                    "pre" => pre = Some(p.int_value()?),
                    "post" => post = Some(p.int_value()?),
                    "boost" => boost = p.float_value()?,
                    "_name" => name = Some(p.text()?),
                    _ => return Err(p.parsing_error(format!("[span_not] query does not support [{current}]"))),
                },
            }
        }
        let Some(include) = include else { return Err(p.parsing_error("span_not must have [include] span query clause")) };
        let Some(exclude) = exclude else { return Err(p.parsing_error("span_not must have [exclude] span query clause")) };
        if dist.is_some() && (pre.is_some() || post.is_some()) {
            return Err(p.parsing_error("span_not can either use [dist] or [pre] & [post] (or none)"));
        }
        let mut q = SpanNot { include, exclude, pre: 0, post: 0, common: Common::default() };
        if let Some(d) = dist {
            q.pre = d.max(0);
            q.post = d.max(0);
        }
        if let Some(v) = pre {
            q.pre = v.max(0);
        }
        if let Some(v) = post {
            q.post = v.max(0);
        }
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("include".into(), self.include.to_value());
        m.insert("exclude".into(), self.exclude.to_value());
        if self.pre != 0 {
            m.insert("pre".into(), self.pre.into());
        }
        if self.post != 0 {
            m.insert("post".into(), self.post.into());
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// `span_containing` and `span_within`: a big and a little span.
fn parse_big_little(p: &mut XParser<'_>, query: &str) -> EsResult<(Query, Query, f32, Option<String>)> {
    let (mut big, mut little) = (None, None);
    let (mut boost, mut name) = (1.0f32, None);
    let mut current = String::new();
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
            Token::StartObject => match current.as_str() {
                "big" => big = Some(span_clause(p, query, "big")?),
                "little" => little = Some(span_clause(p, query, "little")?),
                _ => return Err(p.parsing_error(format!("[{query}] query does not support [{current}]"))),
            },
            _ => match current.as_str() {
                "boost" => boost = p.float_value()?,
                "_name" => name = Some(p.text()?),
                _ => return Err(p.parsing_error(format!("[{query}] query does not support [{current}]"))),
            },
        }
    }
    let Some(big) = big else { return Err(p.parsing_error(format!("{query} must include [big]"))) };
    let Some(little) = little else { return Err(p.parsing_error(format!("{query} must include [little]"))) };
    Ok((big, little, boost, name))
}

/// `span_containing`.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanContaining {
    pub big: Query,
    pub little: Query,
    pub common: Common,
}

impl SpanContaining {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<SpanContaining> {
        let (big, little, boost, name) = parse_big_little(p, "span_containing")?;
        let mut q = SpanContaining { big, little, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("big".into(), self.big.to_value());
        m.insert("little".into(), self.little.to_value());
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// `span_within`.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanWithin {
    pub big: Query,
    pub little: Query,
    pub common: Common,
}

impl SpanWithin {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<SpanWithin> {
        let (big, little, boost, name) = parse_big_little(p, "span_within")?;
        let mut q = SpanWithin { big, little, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("big".into(), self.big.to_value());
        m.insert("little".into(), self.little.to_value());
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// `span_field_masking` (formerly `field_masking_span`).
#[derive(Clone, Debug, PartialEq)]
pub struct SpanFieldMasking {
    pub query: Query,
    pub field: String,
    pub common: Common,
}

impl SpanFieldMasking {
    /// `deprecated_name`: parsed under the old name `field_masking_span`.
    pub fn parse(p: &mut XParser<'_>, deprecated_name: bool) -> EsResult<SpanFieldMasking> {
        if deprecated_name {
            p.deprecated_name("field_masking_span", "span_field_masking", None);
        }
        const NAME: &str = "span_field_masking";
        let (mut inner, mut field) = (None, None);
        let (mut boost, mut name) = (1.0f32, None);
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => {
                    if current == "query" {
                        let q = parse_inner(p)?;
                        if !is_span(&q) {
                            return Err(p.parsing_error(format!("[{NAME}] query must be of type span query")));
                        }
                        check_no_boost(p, NAME, &current, &q)?;
                        inner = Some(q);
                    } else {
                        return Err(p.parsing_error(format!("[{NAME}] query does not support [{current}]")));
                    }
                }
                _ => match current.as_str() {
                    "boost" => boost = p.float_value()?,
                    "field" => field = Some(p.text()?),
                    "_name" => name = Some(p.text()?),
                    _ => return Err(p.parsing_error(format!("[{NAME}] query does not support [{current}]"))),
                },
            }
        }
        let Some(query) = inner else { return Err(p.parsing_error(format!("{NAME} must have [query] span query clause"))) };
        let Some(field) = field else { return Err(p.parsing_error(format!("{NAME} must have [field] set for it"))) };
        if field.is_empty() {
            return Err(EsError::illegal_argument("field name is null or empty"));
        }
        let mut q = SpanFieldMasking { query, field, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.to_value());
        m.insert("field".into(), self.field.clone().into());
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}
