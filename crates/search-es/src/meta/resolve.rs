//! Index expression resolution (`IndexNameExpressionResolver`): turns the `{index}` part of a
//! request (`logs-*,-logs-old,my-alias,<logs-{now/d}>`) into concrete index names.
//!
//! - `_all`, `*` or nothing select every index allowed by `expand_wildcards`.
//! - `*` patterns match index and alias names; hidden indices and aliases are only matched when
//!   `expand_wildcards` includes `hidden` or the pattern starts with `.`; open/closed filtering
//!   applies to what wildcards and aliases expand to.
//! - `-name` / `-pattern` exclude what earlier expressions selected.
//! - `<static-{date-math{format|zone}}>` names resolve against the request time.
//! - `cluster:index` (remote clusters) is rejected.
//! - Missing names fail with `index_not_found_exception` unless `ignore_unavailable`; an empty
//!   result fails unless `allow_no_indices`.

use super::{ClusterState, IndexMeta};
use crate::error::EsError;
use crate::json::Value;
use crate::time::{DateFormatter, Zone, parse_date_math};
use crate::xcontent::filter::glob_match;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IndicesOptions {
    pub ignore_unavailable: bool,
    pub allow_no_indices: bool,
    pub expand_open: bool,
    pub expand_closed: bool,
    pub expand_hidden: bool,
    /// Explicitly named closed indices fail (`index_closed_exception`).
    pub forbid_closed: bool,
    /// Aliases are resolved (otherwise only concrete index names count).
    pub allow_aliases: bool,
    /// Wildcards are expanded at all (`expand_wildcards=none` turns them off).
    pub allow_wildcards: bool,
    /// Data streams (and data stream aliases) resolve to their backing indices; requests that
    /// don't accept them treat their names as missing indices (`excluded_ds`).
    pub include_data_streams: bool,
}

impl IndicesOptions {
    /// `IndicesOptions.strictExpandOpen()`.
    pub const STRICT_EXPAND_OPEN: IndicesOptions = IndicesOptions {
        ignore_unavailable: false,
        allow_no_indices: true,
        expand_open: true,
        expand_closed: false,
        expand_hidden: false,
        forbid_closed: false,
        allow_aliases: true,
        allow_wildcards: true,
        include_data_streams: true,
    };
    /// `IndicesOptions.strictExpandOpenAndForbidClosed()`.
    pub const STRICT_EXPAND_OPEN_FORBID_CLOSED: IndicesOptions = IndicesOptions { forbid_closed: true, ..Self::STRICT_EXPAND_OPEN };
    /// `IndicesOptions.strictExpand()`: open and closed.
    pub const STRICT_EXPAND: IndicesOptions = IndicesOptions { expand_closed: true, ..Self::STRICT_EXPAND_OPEN };
    /// `IndicesOptions.lenientExpandOpen()`.
    pub const LENIENT_EXPAND_OPEN: IndicesOptions = IndicesOptions { ignore_unavailable: true, ..Self::STRICT_EXPAND_OPEN };
    /// `IndicesOptions.strictExpandHidden()`.
    pub const STRICT_EXPAND_HIDDEN: IndicesOptions = IndicesOptions { expand_closed: true, expand_hidden: true, ..Self::STRICT_EXPAND_OPEN };

    /// Applies the request's `ignore_unavailable`, `allow_no_indices` and `expand_wildcards`.
    pub fn with_params(mut self, get: impl Fn(&str) -> Option<String>) -> Result<IndicesOptions, EsError> {
        if let Some(v) = get("ignore_unavailable") {
            self.ignore_unavailable = parse_option_bool(&v, "ignore_unavailable")?;
        }
        if let Some(v) = get("allow_no_indices") {
            self.allow_no_indices = parse_option_bool(&v, "allow_no_indices")?;
        }
        if let Some(v) = get("expand_wildcards") {
            self.expand_open = false;
            self.expand_closed = false;
            self.expand_hidden = false;
            self.allow_wildcards = true;
            for w in v.split(',').map(str::trim) {
                match w {
                    "open" => self.expand_open = true,
                    "closed" => self.expand_closed = true,
                    "hidden" => self.expand_hidden = true,
                    "none" => {}
                    "all" => {
                        self.expand_open = true;
                        self.expand_closed = true;
                        self.expand_hidden = true;
                    }
                    other => return Err(EsError::illegal_argument(format!("No valid expand wildcard value [{other}]"))),
                }
            }
            if !self.expand_open && !self.expand_closed {
                self.allow_wildcards = false;
            }
        }
        Ok(self)
    }

    fn state_ok(&self, idx: &IndexMeta) -> bool {
        if idx.is_closed() { self.expand_closed } else { self.expand_open }
    }

    /// The same options without data streams.
    pub fn without_data_streams(self) -> IndicesOptions {
        IndicesOptions { include_data_streams: false, ..self }
    }
}

/// `index_not_found_exception` for a data stream name in a request that doesn't take data
/// streams (Elasticsearch marks it `excluded_ds`).
pub fn excluded_data_stream(name: &str) -> EsError {
    EsError::new(404, "index_not_found_exception", format!("no such index [{name}]"))
        .with_meta("resource.type", "index_or_alias")
        .with_meta("excluded_ds", "true")
        .with_meta("resource.id", name)
        .with_meta("index_uuid", "_na_")
        .with_meta("index", name)
}

/// Backing indices of a data stream that pass the state filter.
fn backing_of<'a>(state: &'a ClusterState, ds: &'a super::data_streams::DataStream, opts: &'a IndicesOptions) -> impl Iterator<Item = String> + 'a {
    ds.indices.iter().filter(move |n| state.metadata.indices.get(*n).is_some_and(|i| opts.state_ok(i))).cloned()
}

fn parse_option_bool(v: &str, name: &str) -> Result<bool, EsError> {
    match v {
        "true" | "" => Ok(true),
        "false" => Ok(false),
        other => Err(EsError::illegal_argument(format!("Could not convert [{name}] to boolean"))
            .caused_by(EsError::illegal_argument(format!("Failed to parse value [{other}] as only [true] or [false] are allowed.")))),
    }
}

pub fn is_wildcard(e: &str) -> bool {
    e.contains('*')
}

/// Splits a comma-separated path part.
pub fn split_expressions(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()
}

/// Whether the expressions select everything.
pub fn is_all(expressions: &[String]) -> bool {
    expressions.is_empty() || (expressions.len() == 1 && (expressions[0] == "_all" || expressions[0] == "*"))
}

/// Rejects `cluster:index` expressions.
pub fn reject_remote(expressions: &[String]) -> Result<(), EsError> {
    let remote: Vec<&String> = expressions.iter().filter(|e| !e.starts_with('<') && e.contains(':')).collect();
    if !remote.is_empty() {
        let list: Vec<&str> = remote.iter().map(|s| s.as_str()).collect();
        return Err(EsError::illegal_argument(format!(
            "Cross-cluster calls are not supported in this context but remote indices were requested: [{}]",
            list.join(", ")
        )));
    }
    Ok(())
}

/// Resolves expressions to concrete index names (sorted).
pub fn resolve(state: &ClusterState, expressions: &[String], opts: &IndicesOptions, now_ms: i64) -> Result<Vec<String>, EsError> {
    reject_remote(expressions)?;
    let indices = &state.metadata.indices;
    let mut out: BTreeSet<String> = BTreeSet::new();
    if is_all(expressions) {
        if opts.allow_wildcards {
            for (name, idx) in indices {
                if opts.state_ok(idx) && (!idx.is_hidden() || opts.expand_hidden) {
                    out.insert(name.clone());
                }
            }
            if opts.include_data_streams {
                for ds in state.metadata.data_streams.values() {
                    if !ds.hidden || opts.expand_hidden {
                        out.extend(backing_of(state, ds, opts));
                    }
                }
            }
        }
        if out.is_empty() && !opts.allow_no_indices {
            return Err(EsError::index_not_found(expressions.first().map(String::as_str).unwrap_or("_all")));
        }
        return Ok(out.into_iter().collect());
    }
    let aliases = state.aliases();
    for raw in expressions {
        let expr = resolve_date_math(raw, now_ms)?;
        let (exclude, base) = match expr.strip_prefix('-') {
            Some(rest) if opts.allow_wildcards => (true, rest.to_string()),
            _ => (false, expr.clone()),
        };
        if is_wildcard(&base) && opts.allow_wildcards {
            let mut matched: BTreeSet<String> = BTreeSet::new();
            let dot = base.starts_with('.');
            for (name, idx) in indices {
                if glob_match(&base, name) && opts.state_ok(idx) && (!idx.is_hidden() || opts.expand_hidden || dot) {
                    matched.insert(name.clone());
                }
            }
            if opts.allow_aliases {
                for (alias, members) in &aliases {
                    if !glob_match(&base, alias) {
                        continue;
                    }
                    let hidden = members.iter().any(|(_, a)| a.is_hidden == Some(true));
                    if hidden && !(opts.expand_hidden || dot) {
                        continue;
                    }
                    for (idx, _) in members {
                        if opts.state_ok(idx) {
                            matched.insert(idx.name.clone());
                        }
                    }
                }
            }
            if opts.include_data_streams {
                let md = &state.metadata;
                for (name, ds) in &md.data_streams {
                    if glob_match(&base, name) && (!ds.hidden || opts.expand_hidden || dot) {
                        matched.extend(backing_of(state, ds, opts));
                    }
                }
                if opts.allow_aliases {
                    for (alias, a) in &md.data_stream_aliases {
                        if glob_match(&base, alias) {
                            for ds in a.data_streams.iter().filter_map(|d| md.data_streams.get(d)) {
                                matched.extend(backing_of(state, ds, opts));
                            }
                        }
                    }
                }
            }
            if exclude {
                for m in &matched {
                    out.remove(m);
                }
            } else {
                if matched.is_empty() && !opts.allow_no_indices {
                    return Err(EsError::index_not_found(&base));
                }
                out.extend(matched);
            }
            continue;
        }
        // A concrete name (or alias).
        let resolved: Vec<String> = if let Some(idx) = indices.get(&base) {
            if !exclude && idx.is_closed() && opts.forbid_closed {
                if opts.ignore_unavailable {
                    Vec::new()
                } else {
                    return Err(EsError::closed_index(&idx.name, &idx.uuid));
                }
            } else {
                vec![base.clone()]
            }
        } else if let Some(members) = aliases.get(base.as_str()).filter(|_| opts.allow_aliases) {
            members.iter().filter(|(idx, _)| !idx.is_closed() || !opts.forbid_closed || opts.expand_closed).map(|(idx, _)| idx.name.clone()).collect()
        } else if let Some(ds) = state.metadata.data_streams.get(&base) {
            if opts.include_data_streams {
                ds.indices.iter().filter(|n| indices.get(*n).is_some_and(|i| !i.is_closed() || !opts.forbid_closed || opts.expand_closed)).cloned().collect()
            } else if exclude || opts.ignore_unavailable {
                Vec::new()
            } else {
                return Err(excluded_data_stream(&base));
            }
        } else if let Some(a) = state.metadata.data_stream_aliases.get(&base).filter(|_| opts.allow_aliases && opts.include_data_streams) {
            a.data_streams.iter().filter_map(|d| state.metadata.data_streams.get(d)).flat_map(|ds| ds.indices.iter().cloned()).collect()
        } else {
            if !exclude && base.starts_with('_') && base != "_all" {
                return Err(EsError::invalid_index_name_dot(&base, "must not start with '_'."));
            }
            if exclude || opts.ignore_unavailable {
                Vec::new()
            } else {
                return Err(EsError::index_not_found(&base));
            }
        };
        if exclude {
            for r in resolved {
                out.remove(&r);
            }
        } else {
            out.extend(resolved);
        }
    }
    if out.is_empty() && !opts.allow_no_indices {
        if expressions.len() == 1 {
            return Err(EsError::index_not_found(&expressions[0]));
        }
        return Err(EsError::new(404, "index_not_found_exception", "no such index [null]")
            .with_meta("resource.type", "index_expression")
            .with_meta("resource.id", Value::from(expressions.to_vec())));
    }
    Ok(out.into_iter().collect())
}

/// `action.destructive_requires_name`: wildcards and `_all` can't select indices to delete.
pub fn require_names(expressions: &[String]) -> Result<(), EsError> {
    if expressions.is_empty() || expressions.iter().any(|e| e == "_all" || (is_wildcard(e) && !e.starts_with('-'))) {
        return Err(EsError::illegal_argument("Wildcard expressions or all indices are not allowed"));
    }
    Ok(())
}

/// `DateMathExpressionResolver`: `<logs-{now/d}>`, `<logs-{now/M{yyyy.MM}}>`,
/// `<logs-{now/d{yyyy.MM.dd|+12:00}}>`, with `\` escaping `{` and `}`.
pub fn resolve_date_math(expr: &str, now_ms: i64) -> Result<String, EsError> {
    if !expr.starts_with('<') || !expr.ends_with('>') {
        return Ok(expr.to_string());
    }
    // `DateMathExpressionResolver.doResolveExpression`, state machine and messages included:
    // the expression is quoted without its `<` (but with its `>`) and positions count from the
    // `<`, in UTF-16 units.
    let parse_err = |msg: String| EsError::parse(msg);
    let text: Vec<u16> = expr.encode_utf16().collect();
    let shown = String::from_utf16_lossy(&text[1..]);
    let (from, length) = (1usize, text.len() - 1);
    let mut escape = false;
    let mut in_date_format = false;
    let mut in_placeholder = false;
    let mut before: Vec<u16> = Vec::new();
    let mut inside: Vec<u16> = Vec::new();
    const L: u16 = b'{' as u16;
    const R: u16 = b'}' as u16;
    const ESC: u16 = b'\\' as u16;
    let mut i = from;
    while i < length {
        let escaped_char = escape;
        escape = false;
        let c = text[i];
        if c == ESC {
            if escaped_char {
                before.push(c);
            } else {
                escape = true;
            }
            i += 1;
            continue;
        }
        if in_placeholder {
            match c {
                L => {
                    if in_date_format && escaped_char {
                        inside.push(c);
                    } else if !in_date_format {
                        in_date_format = true;
                        inside.push(c);
                    } else {
                        return Err(parse_err(format!("invalid dynamic name expression [{shown}]. invalid character in placeholder at position [{i}]")));
                    }
                }
                R => {
                    if in_date_format && escaped_char {
                        inside.push(c);
                    } else if in_date_format {
                        in_date_format = false;
                        inside.push(c);
                    } else {
                        before.extend(resolve_placeholder(&String::from_utf16_lossy(&inside), now_ms)?.encode_utf16());
                        inside.clear();
                        in_placeholder = false;
                    }
                }
                _ => inside.push(c),
            }
        } else {
            match c {
                L if escaped_char => before.push(c),
                L => in_placeholder = true,
                R if !escaped_char => {
                    return Err(parse_err(format!(
                        "invalid dynamic name expression [{shown}]. invalid character at position [{i}]. `{{` and `}}` are reserved characters and should be escaped when used as part of the index name using `\\` (e.g. `\\{{text\\}}`)"
                    )));
                }
                _ => before.push(c),
            }
        }
        i += 1;
    }
    if in_placeholder {
        return Err(parse_err(format!("invalid dynamic name expression [{shown}]. date math placeholder is open ended")));
    }
    if before.is_empty() {
        return Err(parse_err("nothing captured".into()));
    }
    Ok(String::from_utf16_lossy(&before))
}

/// One `{math{format|zone}}` placeholder: the math is anchored and printed with the format (the
/// formatter's own parser reads a non-`now` anchor), in the zone.
fn resolve_placeholder(inside: &str, now_ms: i64) -> Result<String, EsError> {
    let (math, pattern, zone) = match inside.find('{') {
        None => (inside, "uuuu.MM.dd", None),
        Some(left) => {
            if inside.rfind('}') != Some(inside.len() - 1) {
                return Err(EsError::parse(format!("invalid dynamic name expression [{inside}]. missing closing `}}` for date math format")));
            }
            if left + 2 == inside.len() {
                return Err(EsError::parse(format!("invalid dynamic name expression [{inside}]. missing date format")));
            }
            let pattern_and_zone = &inside[left + 1..inside.len() - 1];
            match pattern_and_zone.find('|') {
                Some(bar) => (&inside[..left], &pattern_and_zone[..bar], Some(&pattern_and_zone[bar + 1..])),
                None => (&inside[..left], pattern_and_zone, None),
            }
        }
    };
    // `DateUtils.of`: Java's zone errors are server errors.
    let zone = match zone {
        Some(z) => {
            let id = crate::query::term_level::java_zone_id(z).map_err(|(kind, msg)| EsError::new(500, kind, msg))?;
            Zone::parse(&id).map_err(|msg| EsError::new(500, "zone_rules_exception", msg))?
        }
        None => Zone::UTC,
    };
    let formatter = DateFormatter::new(pattern).map_err(EsError::illegal_argument)?;
    let instant = parse_date_math(math, now_ms, false, Some(&zone), &formatter).map_err(|e| e.to_es())?;
    Ok(formatter.format(instant, &zone))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::settings::Settings;
    use crate::meta::{AliasMeta, IndexState};
    use std::sync::Arc;

    fn state() -> ClusterState {
        let mut st = ClusterState::new("c");
        let mut add = |name: &str, closed: bool, hidden: bool, aliases: &[(&str, bool)]| {
            let mut settings = Settings::new();
            if hidden {
                settings.set("index.hidden", "true");
            }
            let mut idx = IndexMeta {
                name: name.into(),
                uuid: format!("u-{name}"),
                state: if closed { IndexState::Close } else { IndexState::Open },
                settings,
                mapping: Arc::new(crate::mapping::Mapping::empty()),
                aliases: Default::default(),
                version: 1,
                mapping_version: 1,
                settings_version: 1,
                aliases_version: 1,
                routing_num_shards: 1024,
                primary_terms: vec![1],
                system: false,
                recovery_source: None,
                dialect: Default::default(),
            };
            for (a, h) in aliases {
                idx.aliases.insert(a.to_string(), AliasMeta { is_hidden: h.then_some(true), ..Default::default() });
            }
            st.metadata.indices.insert(name.into(), Arc::new(idx));
        };
        add("a1", false, false, &[]);
        add("a2", true, false, &[]);
        add("b1", false, false, &[("al", false), ("hal", true)]);
        add(".h", false, true, &[]);
        add("c1", false, true, &[]);
        st
    }

    fn r(expr: &str, opts: IndicesOptions) -> Result<Vec<String>, String> {
        resolve(&state(), &split_expressions(expr), &opts, 1_792_454_400_000).map_err(|e| e.reason().to_string())
    }

    #[test]
    fn matches_elasticsearch() {
        let o = IndicesOptions::STRICT_EXPAND_OPEN;
        assert_eq!(r("*", o).unwrap(), vec!["a1", "b1"]);
        assert_eq!(r("_all", o).unwrap(), vec!["a1", "b1"]);
        assert_eq!(r("a*", o).unwrap(), vec!["a1", "b1"]);
        assert_eq!(r("a*,-a2", o).unwrap(), vec!["a1", "b1"]);
        assert_eq!(r("-a1", o).unwrap(), Vec::<String>::new());
        assert_eq!(r("a1,-a1", o).unwrap(), Vec::<String>::new());
        assert_eq!(r("nope", o).unwrap_err(), "no such index [nope]");
        assert_eq!(r("nope*", o).unwrap(), Vec::<String>::new());
        assert_eq!(r("al", o).unwrap(), vec!["b1"]);
        assert_eq!(r("hal", o).unwrap(), vec!["b1"]);
        assert_eq!(r("h*", o).unwrap(), Vec::<String>::new());
        assert_eq!(r(".h*", o).unwrap(), vec![".h"]);
        assert_eq!(r("a2", o).unwrap(), vec!["a2"]);
        assert_eq!(r("x:a1", o).unwrap_err(), "Cross-cluster calls are not supported in this context but remote indices were requested: [x:a1]");
        assert_eq!(r("<a1>", o).unwrap(), vec!["a1"]);
        assert_eq!(r("<x-{now/d}>", o).unwrap_err(), "no such index [x-2026.10.20]");
        assert_eq!(r("*,c1", o).unwrap(), vec!["a1", "b1", "c1"]);
    }

    #[test]
    fn options() {
        let p = |q: &str| {
            let pairs: Vec<(String, String)> = q.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
            IndicesOptions::STRICT_EXPAND_OPEN.with_params(|k| pairs.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.clone())).unwrap()
        };
        assert_eq!(r("a*", p("expand_wildcards=closed")).unwrap(), vec!["a2"]);
        assert_eq!(r("*", p("expand_wildcards=all")).unwrap(), vec![".h", "a1", "a2", "b1", "c1"]);
        assert_eq!(r("*", p("expand_wildcards=hidden&allow_no_indices=false")).unwrap_err(), "no such index [*]");
        assert_eq!(r("nope", p("ignore_unavailable=true")).unwrap(), Vec::<String>::new());
        assert_eq!(r("nope*", p("allow_no_indices=false")).unwrap_err(), "no such index [nope*]");
        assert_eq!(r("-a1", p("allow_no_indices=false")).unwrap_err(), "no such index [-a1]");
        assert_eq!(r("*,-*", p("allow_no_indices=false")).unwrap_err(), "no such index [null]");
        assert_eq!(r("a*,b1", p("expand_wildcards=none")).unwrap_err(), "no such index [a*]");
        let e = IndicesOptions::STRICT_EXPAND_OPEN.with_params(|k| (k == "expand_wildcards").then(|| "foo".to_string())).unwrap_err();
        assert_eq!(e.reason(), "No valid expand wildcard value [foo]");
    }

    #[test]
    fn date_math_names() {
        let now = 1_792_454_400_000; // 2026-10-20T00:00:00Z
        assert_eq!(resolve_date_math("<logs-{now/d}>", now).unwrap(), "logs-2026.10.20");
        assert_eq!(resolve_date_math("<logs-{now/M{yyyy.MM}}>", now).unwrap(), "logs-2026.10");
        assert_eq!(resolve_date_math("<logs-{now/d{yyyy.MM.dd|+12:00}}>", now).unwrap(), "logs-2026.10.20");
        assert_eq!(resolve_date_math("<logs-{now-1d/d}>", now).unwrap(), "logs-2026.10.19");
        assert_eq!(resolve_date_math("<\\{x\\}>", now).unwrap(), "{x}");
        assert_eq!(resolve_date_math("plain", now).unwrap(), "plain");
    }
}
