//! Queries on runtime fields (`AbstractScriptFieldType` and its subclasses).
//!
//! A runtime field answers a query by running its script on every candidate document and
//! testing the values ([`Predicate`]); this module turns a query into that predicate the way
//! each script field type builds its `AbstractScriptFieldQuery`:
//!
//! - values are parsed with the type's own rules: longs as `NumberType.objectToLong` (a
//!   term with a decimal part matches nothing, range bounds round inward), dates through the
//!   field's format and date math (`||/d`, `now`) with the range's `format` and `time_zone`,
//!   ips with CIDR blocks, booleans as `Booleans.parseBoolean` (and its quirky ranges);
//! - keyword fields take `term` (also `case_insensitive`), `terms`, `range`, `prefix`,
//!   `wildcard`, `regexp`, `fuzzy` and single-term `match`/`match_phrase` (the keyword
//!   analyzer makes one token); the automaton queries reuse the engine's term automata
//!   (`MultiTermQuery::matches_term`);
//! - every other combination fails as Elasticsearch does: `Can only use prefix queries on
//!   keyword, text and wildcard fields - not on [f] which is a runtime field of type [long]`,
//!   geo points refusing exact and range searches.
//!
//! The predicate becomes a `RuntimeQuery` (`FilterScript`) through
//! [`QueryCtx::runtime_query`].

use std::sync::Arc;

use super::query::{QueryCtx, match_none, value_text};
use super::runtime::{Predicate, RtField, RtType, RtValue};
use crate::error::{EsError, EsResult};
use crate::json::{Number, Value};
use crate::query::Query;
use search_index::QueryRef;
use search_index::query::MultiTermQuery;

/// `AbstractScriptFieldType.unsupported`.
fn unsupported(f: &RtField, query: &str, fields: &str) -> EsError {
    EsError::illegal_argument(format!("Can only use {query} queries on {fields} fields - not on [{}] which is a runtime field of type [{}]", f.name, f.ty.name()))
}

/// `Booleans.parseBoolean`.
fn parse_bool(v: &Value) -> EsResult<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Null => Ok(false),
        other => match value_text(other).as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            s => Err(EsError::illegal_argument(format!("Failed to parse value [{s}] as only [true] or [false] are allowed."))),
        },
    }
}

/// `InetAddresses.forString`.
fn parse_ip(v: &Value) -> EsResult<[u8; 16]> {
    let s = value_text(v);
    crate::document::values::parse_ip(&s).ok_or_else(|| EsError::illegal_argument(format!("'{s}' is not an IP string literal.")))
}

/// An ip term: one address, or a CIDR block (`IpScriptFieldType.termQuery`).
fn ip_term(v: &Value) -> EsResult<([u8; 16], [u8; 16])> {
    let s = value_text(v);
    if s.contains('/') {
        return crate::engine::ranges::cidr(&s);
    }
    let b = parse_ip(v)?;
    Ok((b, b))
}

/// The value of a `term` (or one of `terms`); `None`: it can't match (a long with a decimal
/// part).
fn term_value(ctx: &QueryCtx, f: &RtField, v: &Value) -> EsResult<Option<RtValue>> {
    Ok(Some(match f.ty {
        RtType::Keyword => RtValue::Str(value_text(v)),
        RtType::Long => {
            if super::range::has_decimal(v) {
                return Ok(None);
            }
            RtValue::Long(super::range::parse_long_value(v)?)
        }
        RtType::Double => RtValue::Double(super::range::parse_double_value(v)?),
        RtType::Date => RtValue::Date(date_millis(ctx, f, v, None, None, false)?),
        RtType::Boolean => RtValue::Bool(parse_bool(v)?),
        RtType::Ip => {
            let b = parse_ip(v)?;
            RtValue::Ip(b)
        }
        RtType::GeoPoint => return Err(geo_exact(f)),
    }))
}

fn geo_exact(f: &RtField) -> EsError {
    EsError::illegal_argument(format!("Geometry fields do not support exact searching, use dedicated geometry queries instead: [{}]", f.name))
}

/// `DateFieldType.parseToLong` in milliseconds: the field's format (or the query's), date
/// math against the request's `now`.
fn date_millis(ctx: &QueryCtx, f: &RtField, v: &Value, format: Option<&str>, zone: Option<&crate::time::Zone>, round_up: bool) -> EsResult<i64> {
    let fmt = match format {
        Some(p) => Arc::new(crate::time::DateFormatter::new(p).map_err(EsError::illegal_argument)?),
        None => f.format.clone(),
    };
    let text = match v {
        Value::Str(s) => s.clone(),
        Value::Num(Number::Float(d)) if d.fract() == 0.0 => format!("{}", *d as i64),
        other => other.to_es_string(),
    };
    Ok(crate::time::parse_date_math(&text, ctx.now_ms, round_up, zone, &fmt).map_err(|e| e.to_es())?.millis())
}

/// The predicate of `term` / `terms` / single-token `match` values (`None`: nothing matches).
fn terms_predicate(ctx: &QueryCtx, f: &RtField, values: &[Value], case_insensitive: bool) -> EsResult<Option<Predicate>> {
    match f.ty {
        RtType::GeoPoint => Err(geo_exact(f)),
        RtType::Ip => Ok(Some(Predicate::Ips(values.iter().map(ip_term).collect::<EsResult<_>>()?))),
        RtType::Keyword if case_insensitive => Ok(Some(Predicate::TermsIgnoreCase(values.iter().map(|v| value_text(v).to_lowercase()).collect()))),
        RtType::Boolean => {
            let (mut t, mut fa) = (false, false);
            for v in values {
                let b = if case_insensitive { parse_bool(&Value::Str(value_text(v).to_ascii_lowercase()))? } else { parse_bool(v)? };
                if b {
                    t = true
                } else {
                    fa = true
                }
            }
            Ok(bool_predicate(t, fa))
        }
        _ => {
            let mut out = Vec::with_capacity(values.len());
            for v in values {
                out.extend(term_value(ctx, f, v)?);
            }
            Ok((!out.is_empty()).then_some(Predicate::Terms(out)))
        }
    }
}

/// `BooleanScriptFieldType.termsQuery(trueAllowed, falseAllowed)`.
fn bool_predicate(true_allowed: bool, false_allowed: bool) -> Option<Predicate> {
    match (true_allowed, false_allowed) {
        (true, true) => Some(Predicate::Exists),
        (true, false) => Some(Predicate::Bool(true)),
        (false, true) => Some(Predicate::Bool(false)),
        (false, false) => None,
    }
}

/// A `range` (`rangeQuery` of each script field type); `None`: an empty range.
fn range_predicate(ctx: &QueryCtx, f: &RtField, r: &crate::query::term_level::Range) -> EsResult<Option<Predicate>> {
    let from = r.from.as_ref().filter(|v| !v.is_null());
    let to = r.to.as_ref().filter(|v| !v.is_null());
    if from.is_none() && to.is_none() {
        return Ok(Some(Predicate::Exists));
    }
    let (il, iu) = (r.include_lower, r.include_upper);
    Ok(Some(match f.ty {
        RtType::GeoPoint => return Err(EsError::illegal_argument(format!("Runtime field [{}] of type [geo_point] does not support range queries", f.name))),
        RtType::Keyword => Predicate::Range { lo: from.map(|v| RtValue::Str(value_text(v))), hi: to.map(|v| RtValue::Str(value_text(v))), include_lower: il, include_upper: iu },
        // `NumberType.longRangeQuery`: decimal bounds round inward.
        RtType::Long => {
            let mut lo = None;
            let mut hi = None;
            if let Some(v) = from {
                let mut l = super::range::parse_long_value(v)?;
                let dec = super::range::has_decimal(v);
                if (!dec && !il) || (dec && super::range::signum(v) > 0.0) {
                    if l == i64::MAX {
                        return Ok(None);
                    }
                    l += 1;
                }
                lo = Some(RtValue::Long(l));
            }
            if let Some(v) = to {
                let mut u = super::range::parse_long_value(v)?;
                let dec = super::range::has_decimal(v);
                if (!dec && !iu) || (dec && super::range::signum(v) < 0.0) {
                    if u == i64::MIN {
                        return Ok(None);
                    }
                    u -= 1;
                }
                hi = Some(RtValue::Long(u));
            }
            Predicate::Range { lo, hi, include_lower: true, include_upper: true }
        }
        RtType::Double => {
            let lo = from.map(super::range::parse_double_value).transpose()?.map(|d| if il { d } else { d.next_up() });
            let hi = to.map(super::range::parse_double_value).transpose()?.map(|d| if iu { d } else { d.next_down() });
            Predicate::Range { lo: lo.map(RtValue::Double), hi: hi.map(RtValue::Double), include_lower: true, include_upper: true }
        }
        // `DateFieldType.dateRangeQuery`: an exclusive lower bound rounds up, then steps.
        RtType::Date => {
            let zone = r.time_zone.as_deref().map(crate::time::Zone::parse).transpose().map_err(EsError::illegal_argument)?;
            let fmt = r.format.as_deref();
            let mut lo = None;
            let mut hi = None;
            if let Some(v) = from {
                let mut l = date_millis(ctx, f, v, fmt, zone.as_ref(), !il)?;
                if !il {
                    if l == i64::MAX {
                        return Ok(None);
                    }
                    l += 1;
                }
                lo = Some(RtValue::Date(l));
            }
            if let Some(v) = to {
                let mut u = date_millis(ctx, f, v, fmt, zone.as_ref(), iu)?;
                if !iu {
                    if u == i64::MIN {
                        return Ok(None);
                    }
                    u -= 1;
                }
                hi = Some(RtValue::Date(u));
            }
            Predicate::Range { lo, hi, include_lower: true, include_upper: true }
        }
        // `IpFieldType.rangeQuery`.
        RtType::Ip => {
            let mut lo = [0u8; 16];
            let mut hi = [0xffu8; 16];
            if let Some(v) = from {
                lo = parse_ip(v)?;
                if !il {
                    match super::range::ip_step(lo, true) {
                        Some(x) => lo = x,
                        None => return Ok(None),
                    }
                }
            }
            if let Some(v) = to {
                hi = parse_ip(v)?;
                if !iu {
                    match super::range::ip_step(hi, false) {
                        Some(x) => hi = x,
                        None => return Ok(None),
                    }
                }
            }
            if lo > hi {
                return Ok(None);
            }
            Predicate::Ips(vec![(lo, hi)])
        }
        // `BooleanScriptFieldType.rangeQuery`: a missing bound reads as `false`.
        RtType::Boolean => return Ok(bool_range(from.map(parse_bool).transpose()?.unwrap_or(false), il, to.map(parse_bool).transpose()?.unwrap_or(false), iu)),
    }))
}

/// `BooleanScriptFieldType.rangeQuery` over its bounds (a missing one is `false`).
fn bool_range(lower: bool, include_lower: bool, upper: bool, include_upper: bool) -> Option<Predicate> {
    let (mut t, mut fa) = match (lower, include_lower) {
        (true, true) => (true, false),
        (true, false) => (false, false),
        (false, true) => (true, true),
        (false, false) => (true, false),
    };
    match (upper, include_upper) {
        (true, true) => {}
        (true, false) | (false, true) => t = false,
        (false, false) => (t, fa) = (false, false),
    }
    bool_predicate(t, fa)
}

impl QueryCtx<'_> {
    /// The runtime field `field` refers to, when it is one (`lookup` fields refuse queries).
    pub(crate) fn runtime_target(&self, field: &str, query: &str) -> EsResult<Option<Arc<RtField>>> {
        if self.runtime_mappings.is_none() && self.idx.mapping.runtime.is_empty() {
            return Ok(None);
        }
        // `lookup` runtime fields only fetch (`LookupRuntimeFieldType` refuses queries).
        if self.table.get(field).is_none()
            && let Some(Value::Obj(def)) = self.runtime_mappings.as_ref().and_then(|r| r.get(field))
            && def.get("type").and_then(Value::as_str) == Some("lookup")
        {
            return Err(EsError::illegal_argument(format!("Field [{field}] of type [lookup] does not support {query} queries")));
        }
        Ok(self.runtime_field(field))
    }

    fn predicate_query(&self, f: Arc<RtField>, pred: Option<Predicate>) -> EsResult<QueryRef> {
        match pred {
            Some(p) => self.runtime_query(f, p),
            None => Ok(match_none("runtime field query matches no value")),
        }
    }

    /// A query on a runtime field (`None`: `q` is no query on a runtime field).
    pub(crate) fn runtime_dispatch(&self, q: &Query) -> EsResult<Option<QueryRef>> {
        use Query as Q;
        let (field, name) = match q {
            Q::Term(t) => (&t.field, "term"),
            Q::Terms(t) if t.values.is_some() => (&t.field, "terms"),
            Q::Range(r) => (&r.field, "range"),
            Q::Exists(e) => (&e.field, "exists"),
            Q::Prefix(p) => (&p.field, "prefix"),
            Q::Wildcard(w) => (&w.field, "wildcard"),
            Q::Regexp(r) => (&r.field, "regexp"),
            Q::Fuzzy(z) => (&z.field, "fuzzy"),
            Q::Match(m) => (&m.field, "match"),
            _ => return Ok(None),
        };
        let Some(f) = self.runtime_target(field, name)? else { return Ok(None) };
        let pred = match q {
            Q::Term(t) => terms_predicate(self, &f, std::slice::from_ref(&t.value), t.case_insensitive)?,
            Q::Terms(t) => terms_predicate(self, &f, t.values.as_deref().unwrap_or_default(), false)?,
            Q::Exists(_) => Some(Predicate::Exists),
            Q::Range(r) => range_predicate(self, &f, r)?,
            Q::Prefix(p) => return self.runtime_prefix(f, &p.value, p.case_insensitive).map(Some),
            Q::Wildcard(w) => return self.runtime_wildcard(f, &w.value, w.case_insensitive).map(Some),
            Q::Regexp(r) => {
                if f.ty != RtType::Keyword {
                    return Err(unsupported(&f, "regexp", "keyword and text"));
                }
                // `KeywordScriptFieldType.regexpQuery` takes no match flags.
                if r.case_insensitive {
                    return Err(EsError::illegal_argument("Match flags not yet implemented [256]"));
                }
                let m = MultiTermQuery::regexp(u32::MAX, &r.value, crate::query::term_level::regexp_flags::to_codec(r.flags), false, r.max_determinized_states as u32).map_err(super::query::index_err)?;
                Some(Predicate::Automaton(Arc::new(m), format!("/{}/", r.value)))
            }
            Q::Fuzzy(z) => {
                if f.ty != RtType::Keyword {
                    return Err(unsupported(&f, "fuzzy", "keyword and text"));
                }
                let term = value_text(&z.value);
                let edits = super::query::fuzzy_edits(&z.fuzziness, &term);
                let m = MultiTermQuery::fuzzy(u32::MAX, &term, edits, z.prefix_length.max(0) as usize, z.max_expansions.max(1) as usize, z.transpositions);
                Some(Predicate::Automaton(Arc::new(m), format!("{term}~{edits}")))
            }
            Q::Match(m) => {
                let o = super::text::MatchOpts { operator: m.operator, analyzer: m.analyzer.as_deref(), ..Default::default() };
                return self.runtime_match(f, &m.query, super::text::MatchType::Boolean, &o, "match").map(Some);
            }
            _ => return Ok(None),
        };
        self.predicate_query(f, pred).map(Some)
    }

    /// `prefixQuery` of a runtime field.
    pub(crate) fn runtime_prefix(&self, f: Arc<RtField>, value: &str, case_insensitive: bool) -> EsResult<QueryRef> {
        if f.ty != RtType::Keyword {
            return Err(unsupported(&f, "prefix", "keyword, text and wildcard"));
        }
        let p = if case_insensitive { Predicate::PrefixIgnoreCase(value.to_lowercase()) } else { Predicate::Prefix(value.to_string()) };
        self.runtime_query(f, p)
    }

    /// `wildcardQuery` of a runtime field.
    pub(crate) fn runtime_wildcard(&self, f: Arc<RtField>, pattern: &str, case_insensitive: bool) -> EsResult<QueryRef> {
        if f.ty != RtType::Keyword {
            return Err(unsupported(&f, "wildcard", "keyword, text and wildcard"));
        }
        let m = MultiTermQuery::wildcard(u32::MAX, pattern, case_insensitive).map_err(super::query::index_err)?;
        self.runtime_query(f, Predicate::Automaton(Arc::new(m), pattern.to_string()))
    }

    /// A `match`-family query on a runtime field: the field has no analysis, so the text is
    /// one term (a phrase of one token is that term); phrase prefixes need text fields. A
    /// query's own `analyzer` splits a keyword field's text into terms (`MatchQueryParser`
    /// analyzes with it, then asks the field for a term query per token).
    pub(crate) fn runtime_match(&self, f: Arc<RtField>, value: &Value, ty: super::text::MatchType, o: &super::text::MatchOpts, query_name: &str) -> EsResult<QueryRef> {
        use super::text::MatchType;
        let r = match (ty, o.analyzer) {
            (MatchType::PhrasePrefix, _) => Err(unsupported(&f, "phrase prefix", "text")),
            (MatchType::BooleanPrefix, _) => self.runtime_prefix(f.clone(), &value_text(value), false),
            (MatchType::Boolean, Some(name)) if f.ty == RtType::Keyword => self.runtime_analyzed(f.clone(), &value_text(value), name, o, query_name),
            (MatchType::Boolean | MatchType::Phrase, _) => terms_predicate(self, &f, std::slice::from_ref(value), false).and_then(|p| self.predicate_query(f.clone(), p)),
        };
        match r {
            Err(e) if o.lenient => Ok(super::text::lenient_none(&f.name, &e)),
            r => r,
        }
    }
}

impl QueryCtx<'_> {
    /// A boolean `match` on keyword runtime field `f` with the query's `analyzer`: a term per
    /// token, any (`or`) or all (`and`) of them; no tokens match nothing.
    fn runtime_analyzed(&self, f: Arc<RtField>, text: &str, analyzer: &str, o: &super::text::MatchOpts, query_name: &str) -> EsResult<QueryRef> {
        let a = super::text::named_analyzer(self, analyzer, query_name)?;
        let mut terms: Vec<RtValue> = super::text::analyze(&a, text).into_iter().map(|t| RtValue::Str(t.term)).collect();
        terms.dedup();
        match terms.len() {
            0 => Ok(match_none("no tokens")),
            1 => self.runtime_query(f, Predicate::Terms(terms)),
            _ if o.operator == crate::query::full_text::Operator::And => {
                let mut b = search_index::query::BooleanQuery::new();
                for t in terms {
                    b = b.add(search_index::query::Occur::Must, self.runtime_query(f.clone(), Predicate::Terms(vec![t]))?);
                }
                Ok(super::query::arc(b))
            }
            _ => self.runtime_query(f, Predicate::Terms(terms)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(ty: RtType) -> RtField {
        RtField { name: "f".into(), ty, script: None, composite_key: None, format: Arc::new(crate::time::DateFormatter::new(crate::time::DateFormatter::DEFAULT).unwrap()), lenient: false }
    }

    #[test]
    fn booleans_parse_strictly() {
        assert!(parse_bool(&Value::from("true")).unwrap());
        assert!(!parse_bool(&Value::Bool(false)).unwrap());
        assert_eq!(parse_bool(&Value::from("yes")).unwrap_err().reason(), "Failed to parse value [yes] as only [true] or [false] are allowed.");
        assert_eq!(parse_bool(&Value::from("")).unwrap_err().reason(), "Failed to parse value [] as only [true] or [false] are allowed.");
    }

    #[test]
    fn ip_terms_take_cidr_blocks() {
        let (lo, hi) = ip_term(&Value::from("10.0.0.0/8")).unwrap();
        assert_eq!(search_index::query::range::inet_to_string(&lo), "10.0.0.0");
        assert_eq!(search_index::query::range::inet_to_string(&hi), "10.255.255.255");
        assert_eq!(
            ip_term(&Value::from("10.0.0.0/33")).unwrap_err().reason(),
            "Illegal prefix length [33] in [10.0.0.0/33]. Must be 0-32 for IPv4 ranges, 0-128 for IPv6 ranges"
        );
        assert_eq!(ip_term(&Value::from("x")).unwrap_err().reason(), "'x' is not an IP string literal.");
    }

    #[test]
    fn unsupported_queries_name_the_runtime_type() {
        assert_eq!(
            unsupported(&field(RtType::Long), "prefix", "keyword, text and wildcard").reason(),
            "Can only use prefix queries on keyword, text and wildcard fields - not on [f] which is a runtime field of type [long]"
        );
    }

    /// `BooleanScriptFieldType.rangeQuery`'s table, missing bounds read as `false` (so a
    /// lone `gte: true` matches nothing, as on Elasticsearch).
    #[test]
    fn boolean_ranges() {
        assert!(bool_range(true, true, false, true).is_none());
        assert!(bool_range(false, false, false, true).is_none());
        assert!(matches!(bool_range(false, true, true, true), Some(Predicate::Exists)));
        assert!(matches!(bool_range(false, true, true, false), Some(Predicate::Bool(false))));
        assert!(matches!(bool_range(true, true, true, true), Some(Predicate::Bool(true))));
        let p = Predicate::Bool(true);
        assert!(p.matches(&[RtValue::Bool(false), RtValue::Bool(true)]));
        assert!(!p.matches(&[RtValue::Bool(false)]));
    }
}
