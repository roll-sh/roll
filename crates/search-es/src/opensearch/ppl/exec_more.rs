//! The PPL commands beyond the core pipeline ([`super::exec`]): `bin`, `timechart`, `chart`,
//! `rex`, `parse`, `spath`, `patterns`, `lookup`, `join`, `transpose`, `fillnull`, and the
//! `explain` plan printer.
//!
//! `bin` labels numeric buckets `lo-hi` (a string column) and rounds timestamps down;
//! `timechart` and `chart` are `stats` over a time (or row) split and an optional column
//! split, keeping the `limit` most significant column values and folding the rest into
//! `OTHER`. `patterns` implements the plugin's `simple_pattern` method (every run of letters
//! and digits that holds a digit becomes `<*>`, numbered with `show_numbered_token`) and
//! groups by the resulting pattern for `mode=aggregation`.

use super::exec::{self, Col, Scope, Settings, Table, Typed, compile, eval};
use super::parser::{BinOpts, Cmd, E, Expr, Op, Query};
use super::value::{self, Key, Ty, Val};
use crate::cluster::Cluster;
use crate::error::EsResult;
use crate::json::{Map, Value};
use std::collections::HashMap;

pub(super) fn command<'a>(cluster: &'a Cluster, t: Table, c: &'a Cmd, s: &'a Settings) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsResult<Table>> + Send + 'a>> {
    Box::pin(async move {
        Ok(match c {
            Cmd::Bin { field, opts, alias } => bin(t, field, opts, alias.as_deref(), s)?,
            Cmd::Timechart { agg, by, span, limit, useother, timefield } => timechart(t, agg, by.as_ref(), span.clone(), *limit, *useother, timefield, s)?,
            Cmd::Chart { agg, row, col, limit, top, useother, otherstr, usenull, nullstr } => chart(t, agg, row.as_ref(), col.as_ref(), *limit, *top, *useother, otherstr, *usenull, nullstr, s)?,
            Cmd::Rex { field, pattern, max_match, sed, offset_field } => rex(t, field, pattern, *max_match, *sed, offset_field.as_deref(), s)?,
            Cmd::Parse { field, pattern } => parse_cmd(t, field, pattern, s)?,
            Cmd::Spath { input, output, path } => spath(t, input, output.as_deref(), path.as_deref(), s)?,
            Cmd::Patterns { field, by, method, mode, max_sample_count, show_numbered_token, new_field, pattern, variable_count_threshold, frequency_threshold } => {
                let st = cluster.state();
                let get = |k: &str| st.metadata.cluster_setting(k).map(str::to_string);
                let p = PatternOpts {
                    brain: if method.is_empty() { get("plugins.ppl.pattern.method").unwrap_or_else(|| "simple_pattern".into()).eq_ignore_ascii_case("brain") } else { method == "brain" },
                    aggregation: if mode.is_empty() { get("plugins.ppl.pattern.mode").unwrap_or_else(|| "label".into()).eq_ignore_ascii_case("aggregation") } else { mode == "aggregation" },
                    numbered: show_numbered_token.unwrap_or_else(|| get("plugins.ppl.pattern.show.numbered.token").is_some_and(|v| v == "true")),
                    samples: max_sample_count.unwrap_or_else(|| get("plugins.ppl.pattern.max.sample.count").and_then(|v| v.parse().ok()).unwrap_or(10)),
                    variable_count_threshold: variable_count_threshold.unwrap_or(5),
                    frequency_threshold: frequency_threshold.unwrap_or(0.3),
                };
                patterns(t, field, by, &p, new_field, pattern.as_deref(), s)?
            }
            Cmd::Lookup { index, pos, keys, replace, outputs } => {
                let q = Query { explain: false, source: super::parser::Source { indices: vec![index.clone()], filter: None, pos: *pos, alias: None }, cmds: Vec::new(), text: s.src.clone() };
                let right = Box::pin(exec::run_all(cluster, &q, s)).await?;
                lookup(t, right, keys, *replace, outputs)?
            }
            Cmd::Join { kind, left, right, on, fields, sub, alias, overwrite } => {
                let rt = Box::pin(exec::run_all(cluster, sub, s)).await?;
                join(t, rt, kind, left.as_deref(), right.as_deref().or(alias.as_deref()), on.as_ref(), fields, *overwrite, s)?
            }
            Cmd::Transpose { n, column_name } => transpose(t, *n, column_name),
            Cmd::FillNull { with, fields } => {
                let mut t = t;
                let x = compile(with, &Scope { cols: &t.cols, fields: &t.fields, s })?;
                let v = eval(&x.c, &[])?;
                let targets: Vec<usize> = if fields.is_empty() { (0..t.cols.len()).filter(|i| !t.cols[*i].hidden).collect() } else { fields.iter().filter_map(|f| t.cols.iter().position(|c| &c.name == f)).collect() };
                for r in &mut t.rows {
                    for &i in &targets {
                        if r[i].is_null() {
                            r[i] = v.clone();
                        }
                    }
                }
                t
            }
            Cmd::Ad => return Err(super::analysis_error("CalciteUnsupportedException", "UNKNOWN", "AD command is unsupported in Calcite", Vec::new())),
            _ => unreachable!("core commands run in exec"),
        })
    })
}

// ------------------------------------------------------------------------------ bin

fn bin(t: Table, field: &Expr, o: &BinOpts, alias: Option<&str>, s: &Settings) -> EsResult<Table> {
    let mut t = t;
    let x = compile(field, &Scope { cols: &t.cols, fields: &t.fields, s })?;
    let name = alias.map(str::to_string).unwrap_or_else(|| match &field.kind {
        E::Field(f) => f.clone(),
        _ => field.text.clone(),
    });
    let vals: Vec<Val> = t.rows.iter().map(|r| eval(&x.c, r)).collect::<EsResult<_>>()?;
    let (out, ty): (Vec<Val>, Ty) = if matches!(x.ty, Ty::Timestamp | Ty::Date) {
        let (len, unit) = o.span.clone().unwrap_or((1.0, "h".into()));
        (vals.into_iter().map(|v| super::func::ts_of(&v).and_then(|t| exec::time_bucket(t, len, &unit)).map_or(Val::Null, Val::Ts)).collect(), Ty::Timestamp)
    } else {
        let nums: Vec<Option<f64>> = vals.iter().map(|v| v.as_f64()).collect();
        let (min, max) = nums.iter().flatten().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), x| (a.min(*x), b.max(*x)));
        let range = |lo: f64, hi: f64| if hi > lo { hi - lo } else { 1.0 };
        let (width, integral, origin) = if let Some((w, unit)) = &o.span {
            if !unit.is_empty() {
                // A time span on a non-date field: read the values as epoch millis.
                (*w, true, 0.0)
            } else {
                (*w, w.fract() == 0.0, 0.0)
            }
        } else if let Some(b) = o.bins {
            let r = range(min, max);
            let w = 10f64.powf((r / b.max(1) as f64).log10().ceil());
            (w, w >= 1.0, 0.0)
        } else if let Some((m, _)) = &o.minspan {
            let r = range(min, max);
            let w = 10f64.powf(r.log10().floor()).max(*m);
            (w, w.fract() == 0.0 && m.fract() == 0.0, 0.0)
        } else if o.start.is_some() || o.end.is_some() {
            let (lo, hi) = (o.start.unwrap_or(min).min(min), o.end.unwrap_or(max).max(max));
            let w = 10f64.powf(range(lo, hi).log10().floor());
            (w, w >= 1.0 && o.start.is_none_or(|v| v.fract() == 0.0), 0.0)
        } else {
            let w = 10f64.powf(range(min, max).log10().floor());
            (w, false, 0.0)
        };
        let fmt = |v: f64| if integral { format!("{}", v as i64) } else { crate::json::write::java_double(v) };
        (
            nums.into_iter()
                .map(|n| match n {
                    Some(v) if width > 0.0 => {
                        let lo = ((v - origin) / width).floor() * width + origin;
                        Val::Str(format!("{}-{}", fmt(lo), fmt(lo + width)))
                    }
                    _ => Val::Null,
                })
                .collect(),
            Ty::Str,
        )
    };
    if let Some(i) = t.cols.iter().position(|c| c.name == name) {
        t.cols[i].ty = ty;
        for (r, v) in t.rows.iter_mut().zip(out) {
            r[i] = v;
        }
    } else {
        t.cols.push(Col { name, ty, hidden: false });
        for (r, v) in t.rows.iter_mut().zip(out) {
            r.push(v);
        }
    }
    Ok(t)
}

// ------------------------------------------------------------------------------ timechart / chart

/// Seconds in a span, for `per_second(x)` and friends.
fn span_seconds(len: f64, unit: &str) -> f64 {
    let u = if unit == "M" { "mon".to_string() } else { unit.to_ascii_lowercase() };
    len * match u.as_str() {
        "ms" | "millisecond" | "milliseconds" => 0.001,
        "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86400.0,
        "w" | "week" | "weeks" => 604_800.0,
        "mon" | "month" | "months" => 2_592_000.0,
        _ => 1.0,
    }
}

#[allow(clippy::too_many_arguments)]
fn timechart(t: Table, agg: &super::parser::Agg, by: Option<&Expr>, span: Option<(f64, String)>, limit: usize, useother: bool, timefield: &str, s: &Settings) -> EsResult<Table> {
    let (len, unit) = span.unwrap_or((1.0, "m".into()));
    if len <= 0.0 {
        return Err(super::invalid_query("IllegalArgumentException", format!("Zero or negative time interval not supported: {}{unit}", len as i64)));
    }
    let sc = Scope { cols: &t.cols, fields: &t.fields, s };
    let tf = sc.find(timefield).map(|i| Typed { c: exec::C::Col(i), ty: t.cols[i].ty });
    let Some(tf) = tf else {
        return Err(sc.not_found_at(timefield, None));
    };
    let per = agg.func.starts_with("per_");
    let mut inner = agg.clone();
    if per {
        inner.func = "sum".into();
    }
    let ac = exec::compile_agg(&inner, &sc)?;
    let byc = match by {
        Some(e) => {
            let c = compile(e, &sc)?;
            non_scalar_split(e, &c)?;
            Some(c)
        }
        None => None,
    };
    let mut groups: indexmap::IndexMap<(Key, Key), (Val, Val, exec::Acc)> = indexmap::IndexMap::new();
    for r in &t.rows {
        let ts = super::func::ts_of(&eval(&tf.c, r)?).and_then(|x| exec::time_bucket(x, len, &unit));
        let Some(ts) = ts else { continue };
        let bv = match &byc {
            Some(c) => eval(&c.c, r)?,
            None => Val::Null,
        };
        let e = groups.entry((Key::Ts(ts), Key::of(&bv))).or_insert_with(|| (Val::Ts(ts), bv, ac.init()));
        ac.add(&mut e.2, r)?;
    }
    let secs = span_seconds(len, &unit);
    let factor = match agg.func.as_str() {
        "per_second" => 1.0,
        "per_minute" => 60.0,
        "per_hour" => 3600.0,
        "per_day" => 86400.0,
        _ => 0.0,
    };
    let finish = |acc: &exec::Acc| -> EsResult<Val> {
        let v = ac.finish(acc)?;
        Ok(if per { v.as_f64().map_or(Val::Null, |x| Val::Float(x / secs * factor)) } else { v })
    };
    let out_ty = if per { Ty::Double } else { ac.ty };
    let mut rows: Vec<Vec<Val>> = Vec::new();
    if let Some(bc) = &byc {
        // Rank the split values by their aggregate over all buckets; the rest become OTHER.
        let mut totals: indexmap::IndexMap<Key, (Val, exec::Acc)> = indexmap::IndexMap::new();
        for r in &t.rows {
            let bv = eval(&bc.c, r)?;
            if bv.is_null() {
                continue;
            }
            let e = totals.entry(Key::of(&bv)).or_insert_with(|| (bv, ac.init()));
            ac.add(&mut e.1, r)?;
        }
        let mut ranked: Vec<(Key, f64)> = totals.iter().map(|(k, (_, acc))| (k.clone(), ac.finish(acc).ok().and_then(|v| v.as_f64()).unwrap_or(0.0))).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let keep: std::collections::HashSet<Key> = if limit == 0 { ranked.iter().map(|r| r.0.clone()).collect() } else { ranked.iter().take(limit).map(|r| r.0.clone()).collect() };
        let mut merged: indexmap::IndexMap<(Key, Key), (Val, Val, exec::Acc)> = indexmap::IndexMap::new();
        for r in &t.rows {
            let ts = super::func::ts_of(&eval(&tf.c, r)?).and_then(|x| exec::time_bucket(x, len, &unit));
            let Some(ts) = ts else { continue };
            let bv = eval(&bc.c, r)?;
            let k = Key::of(&bv);
            let (k, bv) = if bv.is_null() {
                (Key::Str("NULL".into()), Val::Str("NULL".into()))
            } else if keep.contains(&k) {
                (k, bv)
            } else if useother {
                (Key::Str("OTHER".into()), Val::Str("OTHER".into()))
            } else {
                continue;
            };
            let e = merged.entry((Key::Ts(ts), k)).or_insert_with(|| (Val::Ts(ts), bv, ac.init()));
            ac.add(&mut e.2, r)?;
        }
        let mut list: Vec<_> = merged.into_values().collect();
        list.sort_by(|a, b| value::cmp(&a.0, &b.0).then_with(|| value::cmp(&a.1, &b.1)));
        for (ts, bv, acc) in list {
            rows.push(vec![ts, bv, finish(&acc)?]);
        }
        let by_name = by.map(|e| match &e.kind {
            E::Field(f) => f.clone(),
            _ => e.text.clone(),
        });
        // The split column prints its values as text whatever their type.
        let cols = vec![Col { name: timefield.into(), ty: Ty::Timestamp, hidden: false }, Col { name: by_name.unwrap_or_default(), ty: Ty::Str, hidden: false }, Col { name: ac.name.clone(), ty: out_ty, hidden: false }];
        let rows = rows.into_iter().map(|mut r| {
            if !matches!(r[1], Val::Str(_) | Val::Null) {
                r[1] = r[1].to_text(bc.ty).map_or(Val::Null, Val::Str);
            }
            r
        }).collect();
        return Ok(Table { cols, rows, fields: t.fields });
    }
    let mut list: Vec<_> = groups.into_values().collect();
    list.sort_by(|a, b| value::cmp(&a.0, &b.0));
    for (ts, _, acc) in list {
        rows.push(vec![ts, finish(&acc)?]);
    }
    let cols = vec![Col { name: timefield.into(), ty: Ty::Timestamp, hidden: false }, Col { name: ac.name.clone(), ty: out_ty, hidden: false }];
    Ok(Table { cols, rows, fields: t.fields })
}

/// `chart`/`timechart` split by an object or a multi-valued (nested) field.
fn non_scalar_split(e: &Expr, c: &Typed) -> EsResult<()> {
    let name = match &e.kind {
        E::Field(f) => f.clone(),
        _ => e.text.clone(),
    };
    match c.ty {
        Ty::Struct => Err(super::analysis_error("IllegalArgumentException", "UNKNOWN", format!("Cannot chart by [{name}] because it is an object."), Vec::new())),
        Ty::Array => Err(super::analysis_error("IllegalArgumentException", "UNKNOWN", format!("Cannot chart by [{name}] because it holds multiple values."), Vec::new())),
        _ => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn chart(t: Table, agg: &super::parser::Agg, row: Option<&(Expr, BinOpts)>, col: Option<&(Expr, BinOpts)>, limit: usize, top: bool, useother: bool, otherstr: &str, usenull: bool, nullstr: &str, s: &Settings) -> EsResult<Table> {
    let sc = Scope { cols: &t.cols, fields: &t.fields, s };
    let ac = exec::compile_agg(agg, &sc)?;
    let split = |e: &Expr| -> EsResult<Typed> {
        let c = compile(e, &sc)?;
        non_scalar_split(e, &c)?;
        Ok(c)
    };
    let rc = row.map(|(e, _)| split(e)).transpose()?;
    let cc = col.map(|(e, _)| split(e)).transpose()?;
    let name_of = |e: &Expr| match &e.kind {
        E::Field(f) => f.clone(),
        _ => e.text.clone(),
    };
    let mut cols = Vec::new();
    if let (Some((e, _)), Some(c)) = (row, &rc) {
        cols.push(Col { name: name_of(e), ty: c.ty, hidden: false });
    }
    // Column-split values beyond `limit` fold into OTHER (ranked by the aggregate).
    let mut keep: Option<std::collections::HashSet<Key>> = None;
    if let (Some((e, _)), Some(c)) = (col, &cc) {
        cols.push(Col { name: name_of(e), ty: Ty::Str, hidden: false });
        let mut totals: indexmap::IndexMap<Key, exec::Acc> = indexmap::IndexMap::new();
        for r in &t.rows {
            let v = eval(&c.c, r)?;
            if v.is_null() {
                continue;
            }
            ac.add(totals.entry(Key::of(&v)).or_insert_with(|| ac.init()), r)?;
        }
        let mut ranked: Vec<(Key, f64)> = totals.iter().map(|(k, a)| (k.clone(), ac.finish(a).ok().and_then(|v| v.as_f64()).unwrap_or(0.0))).collect();
        ranked.sort_by(|a, b| if top { b.1.total_cmp(&a.1) } else { a.1.total_cmp(&b.1) });
        if limit > 0 {
            keep = Some(ranked.into_iter().take(limit).map(|r| r.0).collect());
        }
    }
    cols.push(Col { name: ac.name.clone(), ty: ac.ty, hidden: false });
    let mut groups: indexmap::IndexMap<Vec<Key>, (Vec<Val>, exec::Acc)> = indexmap::IndexMap::new();
    for r in &t.rows {
        let mut kv = Vec::new();
        if let Some(c) = &rc {
            let v = eval(&c.c, r)?;
            if v.is_null() {
                continue;
            }
            kv.push(v);
        }
        if let Some(c) = &cc {
            let v = eval(&c.c, r)?;
            let v = if v.is_null() {
                if !usenull {
                    continue;
                }
                Val::Str(nullstr.into())
            } else if keep.as_ref().is_some_and(|k| !k.contains(&Key::of(&v))) {
                if !useother {
                    continue;
                }
                Val::Str(otherstr.into())
            } else {
                v.to_text(c.ty).map_or(Val::Null, Val::Str)
            };
            kv.push(v);
        }
        let key: Vec<Key> = kv.iter().map(Key::of).collect();
        let e = groups.entry(key).or_insert_with(|| (kv, ac.init()));
        ac.add(&mut e.1, r)?;
    }
    let mut list: Vec<_> = groups.into_values().collect();
    list.sort_by(|a, b| a.0.iter().zip(&b.0).map(|(x, y)| value::cmp(x, y)).find(|o| o.is_ne()).unwrap_or(std::cmp::Ordering::Equal));
    let mut rows = Vec::new();
    for (mut kv, acc) in list {
        kv.push(ac.finish(&acc)?);
        rows.push(kv);
    }
    Ok(Table { cols, rows, fields: t.fields })
}

// ------------------------------------------------------------------------------ rex / parse / spath

/// Java named groups (`(?<name>...)`) in Rust's syntax.
fn java_regex(p: &str) -> String {
    p.replace("(?<", "(?P<").replace("(?P<=", "(?<=").replace("(?P<!", "(?<!")
}

fn group_names(re: &regex::Regex) -> Vec<String> {
    re.capture_names().flatten().map(str::to_string).collect()
}

fn rex(t: Table, field: &str, pattern: &str, max_match: usize, sed: bool, offset_field: Option<&str>, s: &Settings) -> EsResult<Table> {
    let mut t = t;
    let src = Scope { cols: &t.cols, fields: &t.fields, s }.field(field, 0, "")?;
    if sed {
        let (from, to, global) = parse_sed(pattern).ok_or_else(|| super::semantic(format!("Invalid sed expression: {pattern}")))?;
        let re = regex::Regex::new(&java_regex(&from)).map_err(|e| super::semantic(format!("Invalid regex pattern: {e}")))?;
        let i = t.cols.iter().position(|c| c.name == field);
        for r in &mut t.rows {
            let v = eval(&src.c, r)?;
            let out = v.to_text(src.ty).map_or(Val::Null, |s| Val::Str(if global { re.replace_all(&s, to.as_str()).into() } else { re.replace(&s, to.as_str()).into() }));
            if let Some(i) = i {
                r[i] = out;
            }
        }
        return Ok(t);
    }
    let re = regex::Regex::new(&java_regex(pattern)).map_err(|e| super::semantic(format!("Invalid regex pattern: {e}")))?;
    let names = group_names(&re);
    if names.is_empty() {
        return Err(super::semantic("Rex pattern must contain at least one named capture group"));
    }
    let mut values: Vec<Vec<Val>> = Vec::with_capacity(t.rows.len());
    let mut offsets: Vec<Val> = Vec::new();
    for r in &t.rows {
        let v = eval(&src.c, r)?.to_text(src.ty);
        let mut row = vec![Val::Null; names.len()];
        let mut off = Vec::new();
        if let Some(s) = v {
            if max_match > 1 {
                let mut lists: Vec<Vec<Val>> = vec![Vec::new(); names.len()];
                for caps in re.captures_iter(&s).take(max_match) {
                    for (k, n) in names.iter().enumerate() {
                        if let Some(m) = caps.name(n) {
                            lists[k].push(Val::Str(m.as_str().into()));
                        }
                    }
                }
                row = lists.into_iter().map(|l| if l.is_empty() { Val::Null } else { Val::Arr(l) }).collect();
            } else if let Some(caps) = re.captures(&s) {
                for (k, n) in names.iter().enumerate() {
                    if let Some(m) = caps.name(n) {
                        row[k] = Val::Str(m.as_str().into());
                        off.push(format!("{n}={}-{}", m.start(), m.end().saturating_sub(1)));
                    }
                }
            }
        }
        values.push(row);
        offsets.push(if off.is_empty() { Val::Null } else { Val::Str(off.join("&")) });
    }
    let ty = if max_match > 1 { Ty::Array } else { Ty::Str };
    for (k, n) in names.iter().enumerate() {
        set_column(&mut t, n, ty, values.iter().map(|r| r[k].clone()).collect());
    }
    if let Some(of) = offset_field {
        set_column(&mut t, of, Ty::Str, offsets);
    }
    Ok(t)
}

fn parse_sed(p: &str) -> Option<(String, String, bool)> {
    let rest = p.strip_prefix("s/")?;
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut esc = false;
    for c in rest.chars() {
        if esc {
            if c != '/' {
                cur.push('\\');
            }
            cur.push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '/' {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    parts.push(cur);
    let from = parts.first()?.clone();
    let to = parts.get(1)?.replace("\\1", "${1}").replace("\\2", "${2}");
    Some((from, to, parts.get(2).is_some_and(|f| f.contains('g'))))
}

/// Replaces a column's values (keeping its place) or appends it.
fn set_column(t: &mut Table, name: &str, ty: Ty, vals: Vec<Val>) {
    match t.cols.iter().position(|c| c.name == name) {
        Some(i) => {
            t.cols[i].ty = ty;
            for (r, v) in t.rows.iter_mut().zip(vals) {
                r[i] = v;
            }
        }
        None => {
            t.cols.push(Col { name: name.into(), ty, hidden: false });
            for (r, v) in t.rows.iter_mut().zip(vals) {
                r.push(v);
            }
        }
    }
}

fn parse_cmd(t: Table, field: &Expr, pattern: &str, s: &Settings) -> EsResult<Table> {
    let mut t = t;
    let src = compile(field, &Scope { cols: &t.cols, fields: &t.fields, s })?;
    let re = regex::Regex::new(&format!("^(?:{})$", java_regex(pattern))).map_err(|e| super::semantic(format!("Invalid regex pattern: {e}")))?;
    let names = group_names(&re);
    let mut values: Vec<Vec<Val>> = Vec::new();
    for r in &t.rows {
        let v = eval(&src.c, r)?.to_text(src.ty);
        let row = match v.as_deref().and_then(|s| re.captures(s)) {
            Some(caps) => names.iter().map(|n| Val::Str(caps.name(n).map_or("", |m| m.as_str()).into())).collect(),
            None => vec![Val::Str(String::new()); names.len()],
        };
        values.push(row);
    }
    for (k, n) in names.iter().enumerate() {
        set_column(&mut t, n, Ty::Str, values.iter().map(|r| r[k].clone()).collect());
    }
    Ok(t)
}

fn spath(t: Table, input: &str, output: Option<&str>, path: Option<&str>, s: &Settings) -> EsResult<Table> {
    let mut t = t;
    let src = Scope { cols: &t.cols, fields: &t.fields, s }.field(input, 0, "")?;
    let docs: Vec<Option<Value>> = t.rows.iter().map(|r| eval(&src.c, r).map(|v| v.to_text(src.ty).and_then(|s| crate::json::parse(s.as_bytes()).ok().flatten()))).collect::<EsResult<_>>()?;
    match path {
        Some(p) => {
            let out = output.unwrap_or(p).to_string();
            let vals = docs.iter().map(|d| d.as_ref().and_then(|d| json_get(d, p)).map_or(Val::Null, json_cell)).collect();
            shadow(&mut t, &out);
            set_column(&mut t, &out, Ty::Str, vals);
        }
        None => {
            // Every leaf path becomes a column (`output` prefixes them).
            let prefix = output.unwrap_or(input).to_string();
            let mut paths: indexmap::IndexSet<String> = indexmap::IndexSet::new();
            for d in docs.iter().flatten() {
                leaf_paths(d, "", &mut paths);
            }
            shadow(&mut t, &prefix);
            if output.is_some() {
                // `output=x` keeps the whole object as a map column too.
                let vals = docs.iter().map(|d| d.as_ref().map_or(Val::Null, |d| Val::Json(d.clone()))).collect();
                set_column(&mut t, &prefix, Ty::Struct, vals);
            }
            for p in paths {
                let vals = docs.iter().map(|d| d.as_ref().and_then(|d| json_get(d, &p)).map_or(Val::Null, json_cell)).collect();
                set_column(&mut t, &format!("{prefix}.{p}"), Ty::Str, vals);
            }
        }
    }
    Ok(t)
}

/// An extracted value hides the mapped columns under the same name.
fn shadow(t: &mut Table, name: &str) {
    let under = format!("{name}.");
    for c in &mut t.cols {
        if c.name.starts_with(&under) {
            c.hidden = true;
            c.name = format!("\u{0}{}", c.name);
        }
    }
    if let Some(c) = t.cols.iter_mut().find(|c| c.name == name && matches!(c.ty, Ty::Struct)) {
        c.hidden = true;
        c.name = format!("\u{0}{name}");
    }
}

fn leaf_paths(v: &Value, prefix: &str, out: &mut indexmap::IndexSet<String>) {
    match v {
        Value::Obj(m) => {
            for (k, v) in m {
                let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                leaf_paths(v, &p, out);
            }
        }
        _ if !prefix.is_empty() => {
            out.insert(prefix.to_string());
        }
        _ => {}
    }
}

fn json_get<'v>(d: &'v Value, p: &str) -> Option<&'v Value> {
    let mut cur = d;
    for part in p.split('.') {
        let (name, idx) = match part.split_once('{') {
            Some((n, rest)) => (n, rest.trim_end_matches('}').parse::<usize>().ok()),
            None => (part, None),
        };
        if !name.is_empty() {
            cur = cur.get(name)?;
        }
        if let Some(i) = idx {
            cur = cur.as_array()?.get(i)?;
        }
    }
    Some(cur)
}

fn json_cell(v: &Value) -> Val {
    match v {
        Value::Null => Val::Null,
        Value::Str(s) => Val::Str(s.clone()),
        other => Val::Str(other.to_json()),
    }
}

// ------------------------------------------------------------------------------ patterns

/// The `simple_pattern` of a text: every match of the pattern (by default each run of ASCII
/// letters and digits, `[a-zA-Z0-9]+`) becomes `<*>`, or `<token1>`, `<token2>`, ... with
/// numbered tokens; the characters between stay.
fn simple_pattern(text: &str, numbered: bool, custom: Option<&regex::Regex>) -> (String, Vec<(String, String)>) {
    static DEFAULT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = custom.unwrap_or_else(|| DEFAULT.get_or_init(|| regex::Regex::new("[a-zA-Z0-9]+").expect("pattern")));
    let mut out = String::with_capacity(text.len());
    let mut tokens = Vec::new();
    let mut last = 0;
    for m in re.find_iter(text) {
        out.push_str(&text[last..m.start()]);
        if numbered {
            let name = format!("<token{}>", tokens.len() + 1);
            out.push_str(&name);
            tokens.push((name, m.as_str().to_string()));
        } else {
            out.push_str("<*>");
        }
        last = m.end();
    }
    out.push_str(&text[last..]);
    (out, tokens)
}

/// The `patterns` command's options, defaults resolved from the cluster settings.
struct PatternOpts {
    brain: bool,
    aggregation: bool,
    numbered: bool,
    samples: usize,
    variable_count_threshold: usize,
    frequency_threshold: f64,
}

fn patterns(t: Table, field: &Expr, by: &[super::parser::Group], o: &PatternOpts, new_field: &str, pattern: Option<&str>, s: &Settings) -> EsResult<Table> {
    let mut t = t;
    let sc = Scope { cols: &t.cols, fields: &t.fields, s };
    let src = compile(field, &sc)?;
    let byc = exec::group_columns(by, &sc)?;
    let custom = match pattern {
        Some(p) => Some(regex::Regex::new(&java_regex(p)).map_err(|e| super::semantic(format!("Invalid regex pattern: {e}")))?),
        None => None,
    };
    let texts: Vec<Option<String>> = t.rows.iter().map(|r| eval(&src.c, r).map(|v| v.to_text(src.ty))).collect::<EsResult<_>>()?;
    let keys: Vec<Vec<Val>> = t.rows.iter().map(|r| byc.iter().map(|(c, _)| eval(&c.c, r)).collect::<EsResult<Vec<_>>>()).collect::<EsResult<_>>()?;
    // Each message's raw pattern (`<*>` wildcards); brain sees each `BY` partition on its own.
    let raw: Vec<String> = if o.brain {
        let mut parts: indexmap::IndexMap<Vec<Key>, Vec<usize>> = indexmap::IndexMap::new();
        for (i, k) in keys.iter().enumerate() {
            parts.entry(k.iter().map(Key::of).collect()).or_default().push(i);
        }
        let mut out = vec![String::new(); texts.len()];
        for idx in parts.values() {
            let msgs: Vec<Option<String>> = idx.iter().map(|&i| texts[i].clone()).collect();
            for (&i, p) in idx.iter().zip(super::brain::parse_all(&msgs, o.variable_count_threshold, o.frequency_threshold)) {
                out[i] = p;
            }
        }
        out
    } else {
        texts.iter().map(|x| x.as_deref().map_or(String::new(), |s| simple_pattern(s, false, custom.as_ref()).0)).collect()
    };
    let label = |i: usize| -> (String, Vec<(String, String)>) {
        if !o.numbered {
            return (raw[i].clone(), Vec::new());
        }
        if o.brain {
            super::brain::number_tokens(&raw[i], texts[i].as_deref())
        } else {
            texts[i].as_deref().map_or((String::new(), Vec::new()), |s| simple_pattern(s, true, custom.as_ref()))
        }
    };
    let token_value = |toks: &[(String, String)]| -> Val {
        let mut m = Map::new();
        for (name, v) in toks {
            match m.get_mut(name) {
                Some(Value::Arr(a)) => a.push(Value::Str(v.clone())),
                _ => {
                    m.insert(name.clone(), Value::Arr(vec![Value::Str(v.clone())]));
                }
            }
        }
        Val::Json(Value::Obj(crate::json::java_hash_map(m, 16)))
    };
    if o.aggregation {
        // Grouped by partition and raw pattern; rows come out in the `HashMap` order of the
        // plugin's pattern map.
        // Per (partition, pattern): the partition's values, the count and the sample rows.
        type Group = (Vec<Val>, i64, Vec<usize>);
        let mut groups: indexmap::IndexMap<(Vec<Key>, String), Group> = indexmap::IndexMap::new();
        for (i, k) in keys.iter().enumerate() {
            let e = groups.entry((k.iter().map(Key::of).collect(), raw[i].clone())).or_insert_with(|| (k.clone(), 0, Vec::new()));
            e.1 += 1;
            if e.2.len() < o.samples {
                e.2.push(i);
            }
        }
        let names: Vec<String> = groups.keys().map(|(_, p)| p.clone()).collect();
        let order = crate::json::java_hash_order(&names, 16);
        let list: Vec<_> = groups.into_values().collect();
        let mut cols: Vec<Col> = byc.iter().map(|(c, n)| Col { name: n.clone(), ty: c.ty, hidden: false }).collect();
        cols.push(Col { name: new_field.into(), ty: Ty::Str, hidden: false });
        cols.push(Col { name: "pattern_count".into(), ty: Ty::Long, hidden: false });
        if o.numbered {
            cols.push(Col { name: "tokens".into(), ty: Ty::Struct, hidden: false });
        }
        cols.push(Col { name: "sample_logs".into(), ty: Ty::Array, hidden: false });
        let mut rows = Vec::new();
        for gi in order {
            let (bv, n, sample) = &list[gi];
            let mut row = bv.clone();
            let (pattern, _) = label(sample[0]);
            row.push(Val::Str(pattern));
            row.push(Val::Int(*n));
            if o.numbered {
                let toks: Vec<(String, String)> = sample.iter().flat_map(|&i| label(i).1).collect();
                row.push(token_value(&toks));
            }
            row.push(Val::Arr(sample.iter().map(|&i| texts[i].clone().map_or(Val::Null, Val::Str)).collect()));
            rows.push(row);
        }
        return Ok(Table { cols, rows, fields: t.fields });
    }
    let labelled: Vec<(String, Vec<(String, String)>)> = (0..texts.len()).map(label).collect();
    let pats: Vec<Val> = labelled.iter().map(|(p, _)| Val::Str(p.clone())).collect();
    set_column(&mut t, new_field, Ty::Str, pats);
    if o.numbered {
        let toks: Vec<Val> = labelled.iter().map(|(_, toks)| token_value(toks)).collect();
        set_column(&mut t, "tokens", Ty::Struct, toks);
    }
    Ok(t)
}

// ------------------------------------------------------------------------------ lookup / join

fn lookup(t: Table, right: Table, keys: &[(String, String)], replace: bool, outputs: &[(String, String)]) -> EsResult<Table> {
    let mut t = t;
    // keys: (lookup field, source field).
    let rk: Vec<usize> = keys.iter().map(|(l, _)| right.cols.iter().position(|c| &c.name == l).ok_or_else(|| super::semantic(format!("Field [{l}] not found.")))).collect::<EsResult<_>>()?;
    let lk: Vec<usize> = keys.iter().map(|(_, s)| t.cols.iter().position(|c| &c.name == s).ok_or_else(|| super::semantic(format!("Field [{s}] not found.")))).collect::<EsResult<_>>()?;
    let outs: Vec<(usize, String)> = if outputs.is_empty() {
        right.cols.iter().enumerate().filter(|(i, c)| !rk.contains(i) && !c.hidden).map(|(i, c)| (i, c.name.clone())).collect()
    } else {
        outputs.iter().map(|(a, b)| right.cols.iter().position(|c| &c.name == a).map(|i| (i, b.clone())).ok_or_else(|| super::semantic(format!("Field [{a}] not found.")))).collect::<EsResult<_>>()?
    };
    let mut index: HashMap<Vec<Key>, &Vec<Val>> = HashMap::new();
    for r in &right.rows {
        index.entry(rk.iter().map(|i| Key::of(&r[*i])).collect()).or_insert(r);
    }
    let mut targets = Vec::new();
    for (i, name) in &outs {
        let pos = match t.cols.iter().position(|c| &c.name == name) {
            Some(p) => p,
            None => {
                t.cols.push(Col { name: name.clone(), ty: right.cols[*i].ty, hidden: false });
                for r in &mut t.rows {
                    r.push(Val::Null);
                }
                t.cols.len() - 1
            }
        };
        targets.push((*i, pos));
    }
    for r in &mut t.rows {
        let key: Vec<Key> = lk.iter().map(|i| Key::of(&r[*i])).collect();
        match index.get(&key) {
            Some(m) => {
                for (ri, li) in &targets {
                    if replace || r[*li].is_null() {
                        r[*li] = m[*ri].clone();
                    }
                }
            }
            None if replace => {
                for (_, li) in &targets {
                    r[*li] = Val::Null;
                }
            }
            None => {}
        }
    }
    Ok(t)
}

#[allow(clippy::too_many_arguments)]
fn join(left: Table, right: Table, kind: &str, la: Option<&str>, ra: Option<&str>, on: Option<&Expr>, fields: &[String], overwrite: bool, s: &Settings) -> EsResult<Table> {
    let right = right.visible();
    let left_vis = left.clone().visible();
    // The joined row's scope: left columns (also as `L.x`), then right ones (`R.x`).
    let mut cols: Vec<Col> = left_vis.cols.clone();
    let ln = cols.len();
    for c in &right.cols {
        cols.push(c.clone());
    }
    let mut scope_cols = cols.clone();
    for (i, c) in scope_cols.iter_mut().enumerate() {
        if i < ln {
            if let Some(a) = la {
                c.name = format!("{a}.{}", c.name);
            }
        } else if let Some(a) = ra {
            c.name = format!("{a}.{}", c.name);
        }
    }
    let mut both = scope_cols.clone();
    both.extend(cols.iter().cloned());
    let cond = match on {
        Some(e) => Some(compile(e, &Scope { cols: &both, fields: &left.fields, s })?),
        None => None,
    };
    let key_pairs: Vec<(usize, usize)> = fields.iter().filter_map(|f| Some((left_vis.cols.iter().position(|c| &c.name == f)?, right.cols.iter().position(|c| &c.name == f)?))).collect();
    let matches = |l: &[Val], r: &[Val]| -> EsResult<bool> {
        if let Some(c) = &cond {
            let mut row: Vec<Val> = l.to_vec();
            row.extend(r.iter().cloned());
            let dup = row.clone();
            row.extend(dup);
            return Ok(eval(&c.c, &row)?.as_bool() == Some(true));
        }
        Ok(key_pairs.iter().all(|(a, b)| exec::compare(Op::Eq, &l[*a], &r[*b]).as_bool() == Some(true)))
    };
    let mut rows = Vec::new();
    let mut right_used = vec![false; right.rows.len()];
    for l in &left_vis.rows {
        let mut any = false;
        for (j, r) in right.rows.iter().enumerate() {
            if matches(l, r)? {
                any = true;
                right_used[j] = true;
                match kind {
                    "semi" => break,
                    "anti" => break,
                    _ => {
                        let mut row = l.clone();
                        row.extend(r.iter().cloned());
                        rows.push(row);
                    }
                }
            }
        }
        match kind {
            "semi" if any => rows.push(l.clone()),
            "anti" if !any => rows.push(l.clone()),
            "left" | "outer" | "full" if !any => {
                let mut row = l.clone();
                row.extend(std::iter::repeat_n(Val::Null, right.cols.len()));
                rows.push(row);
            }
            _ => {}
        }
    }
    if matches!(kind, "right" | "full") {
        for (j, r) in right.rows.iter().enumerate() {
            if !right_used[j] {
                let mut row = vec![Val::Null; ln];
                row.extend(r.iter().cloned());
                rows.push(row);
            }
        }
    }
    if matches!(kind, "semi" | "anti") {
        return Ok(Table { cols: left_vis.cols, rows, fields: left.fields });
    }
    // Duplicate names: without aliases (or with `overwrite`) the right side's value wins.
    let mut out_cols: Vec<Col> = Vec::new();
    let mut picks: Vec<usize> = Vec::new();
    for (i, c) in cols.iter().enumerate() {
        if i < ln {
            if let Some(j) = cols[ln..].iter().position(|r| r.name == c.name)
                && (!fields.is_empty() || la.is_none() || overwrite)
            {
                out_cols.push(c.clone());
                picks.push(ln + j);
                continue;
            }
            out_cols.push(c.clone());
            picks.push(i);
        } else if !cols[..ln].iter().any(|l| l.name == c.name) {
            out_cols.push(c.clone());
            picks.push(i);
        } else if la.is_some() && fields.is_empty() && !overwrite {
            let mut c = c.clone();
            c.name = format!("{}.{}", ra.unwrap_or("r"), c.name);
            out_cols.push(c);
            picks.push(i);
        }
    }
    let rows = rows
        .into_iter()
        .map(|r| {
            picks
                .iter()
                .enumerate()
                .map(|(k, &i)| {
                    // `overwrite`: a missing right value keeps the left one.
                    let v = r[i].clone();
                    if v.is_null() && i >= ln && k < ln { r[k].clone() } else { v }
                })
                .collect()
        })
        .collect();
    Ok(Table { cols: out_cols, rows, fields: left.fields })
}

// ------------------------------------------------------------------------------ transpose

fn transpose(t: Table, n: usize, column_name: &str) -> Table {
    let t = t.visible();
    let n = n.min(t.rows.len());
    let mut cols = vec![Col { name: column_name.into(), ty: Ty::Str, hidden: false }];
    for i in 0..n {
        cols.push(Col { name: format!("row {}", i + 1), ty: Ty::Str, hidden: false });
    }
    let rows = t
        .cols
        .iter()
        .enumerate()
        .map(|(ci, c)| {
            let mut r = vec![Val::Str(c.name.clone())];
            for row in t.rows.iter().take(n) {
                r.push(row[ci].to_text(c.ty).map_or(Val::Null, Val::Str));
            }
            r
        })
        .collect();
    Table { cols, rows, fields: t.fields }
}

// ------------------------------------------------------------------------------ explain

/// The logical and physical plans of a query, in the plugin's Calcite notation.
pub(super) fn plans(q: &Query, fields: &exec::Fields, s: &Settings) -> (String, String) {
    let index = q.source.indices.join(",");
    let mut cols: Vec<String> = fields.top.clone();
    let all: Vec<String> = fields.paths.keys().cloned().chain(exec::META.iter().map(|(m, _)| m.to_string())).collect();
    let pos = |name: &str| all.iter().position(|c| c == name).unwrap_or(0);
    let mut logical: Vec<String> = Vec::new();
    let mut pushed: Vec<String> = Vec::new();
    let mut dsl = Map::new();
    dsl.insert("from".into(), Value::from(0i64));
    dsl.insert("size".into(), Value::from(s.size_limit as i64));
    dsl.insert("timeout".into(), Value::Str("1m".into()));
    let mut total = s.size_limit as i64;
    for c in &q.cmds {
        match c {
            Cmd::Where(e) => {
                logical.push(format!("LogicalFilter(condition=[{}])", rex_expr(e, &pos)));
                pushed.push(format!("FILTER->{}", rex_expr(e, &|n: &str| cols.iter().position(|c| c == n).unwrap_or(0))));
                if let Some(q) = filter_dsl(e, fields) {
                    dsl.insert("query".into(), q);
                }
            }
            Cmd::Fields { exclude: false, fields: list } => {
                cols = list.iter().map(|(f, _)| f.clone()).collect();
                logical.push(format!("LogicalProject({})", cols.iter().map(|c| format!("{c}=[${}]", pos(c))).collect::<Vec<_>>().join(", ")));
            }
            Cmd::Head { n, .. } => {
                logical.push(format!("LogicalSort(fetch=[{n}])"));
                pushed.push(format!("LIMIT->{n}"));
                total = total.min(*n as i64);
                dsl.insert("size".into(), Value::from(total));
            }
            Cmd::Stats { aggs, by, .. } => {
                let groups: Vec<String> = by.iter().map(|g| g.name.clone()).collect();
                let agg_text: Vec<String> = aggs.iter().map(|a| format!("{}={}({})", a.name, a.func.to_ascii_uppercase(), a.args.iter().map(|_| "$0".to_string()).collect::<Vec<_>>().join(", "))).collect();
                logical.push(format!("LogicalAggregate(group=[{{{}}}], {})", (0..groups.len()).map(|i| i.to_string()).collect::<Vec<_>>().join(", "), agg_text.join(", ")));
                pushed.push(format!("AGGREGATION->rel#0:LogicalAggregate.NONE.[](input=RelSubset#0,group={{{}}},{})", (0..groups.len()).map(|i| i.to_string()).collect::<Vec<_>>().join(", "), agg_text.join(",")));
                dsl.insert("size".into(), Value::from(0i64));
                dsl.insert("aggregations".into(), aggs_dsl(aggs, &groups, fields, s));
                cols = aggs.iter().map(|a| a.name.clone()).chain(groups).collect();
            }
            Cmd::Top { n, fields: list, .. } => {
                let groups: Vec<String> = list.iter().map(|(f, _)| f.clone()).collect();
                logical.push(format!("LogicalAggregate(group=[{{{}}}], count=[COUNT()])", (0..groups.len()).map(|i| i.to_string()).collect::<Vec<_>>().join(", ")));
                pushed.push("AGGREGATION->rel#0:LogicalAggregate.NONE.[](input=RelSubset#0,group={0},count=COUNT())".to_string());
                let fake = vec![super::parser::Agg { func: "count".into(), args: Vec::new(), cond: None, name: "count".into(), pos: 0 }];
                dsl.insert("size".into(), Value::from(0i64));
                dsl.insert("aggregations".into(), aggs_dsl(&fake, &groups, fields, s));
                let _ = n;
                cols = groups.into_iter().chain(std::iter::once("count".to_string())).collect();
            }
            Cmd::Sort { keys, .. } => {
                logical.push(format!("LogicalSort({})", keys.iter().enumerate().map(|(i, k)| format!("sort{i}=[${}], dir{i}=[{}]", pos(&k.expr.text), if k.desc { "DESC-nulls-last" } else { "ASC-nulls-first" })).collect::<Vec<_>>().join(", ")));
            }
            Cmd::Eval(assigns) => {
                let mut items: Vec<String> = cols.iter().map(|c| format!("{c}=[${}]", pos(c))).collect();
                for (n, e) in assigns {
                    items.push(format!("{n}=[{}]", rex_expr(e, &pos)));
                    cols.push(n.clone());
                }
                logical.push(format!("LogicalProject({})", items.join(", ")));
            }
            _ => logical.push(format!("Logical{}", cmd_name(c))),
        }
    }
    if !dsl.contains_key("aggregations") {
        dsl.insert("_source".into(), crate::obj! { "includes" => Value::Arr(cols.iter().map(|c| Value::Str(c.clone())).collect()) });
        pushed.insert(0, format!("PROJECT->[{}]", cols.join(", ")));
    } else {
        pushed.push(format!("PROJECT->[{}]", cols.join(", ")));
    }
    pushed.push(format!("LIMIT->{}", s.size_limit));
    let mut text = format!("LogicalSystemLimit(fetch=[{}], type=[QUERY_SIZE_LIMIT])\n", s.size_limit);
    let mut depth = 1;
    let top_project = format!("LogicalProject({})", cols.iter().map(|c| format!("{c}=[${}]", pos(c))).collect::<Vec<_>>().join(", "));
    let mut stack: Vec<String> = vec![top_project];
    stack.extend(logical.into_iter().rev());
    for line in stack {
        text.push_str(&"  ".repeat(depth));
        text.push_str(&line);
        text.push('\n');
        depth += 1;
    }
    text.push_str(&"  ".repeat(depth));
    text.push_str(&format!("CalciteLogicalIndexScan(table=[[OpenSearch, {index}]])\n"));
    let physical = format!(
        "CalciteEnumerableIndexScan(table=[[OpenSearch, {index}]], PushDownContext=[[{}], OpenSearchRequestBuilder(sourceBuilder={}, requestedTotalSize={total}, pageSize=null, startFrom=0)])\n",
        pushed.join(", "),
        Value::Obj(dsl).to_json()
    );
    (text, physical)
}

fn cmd_name(c: &Cmd) -> &'static str {
    match c {
        Cmd::Dedup { .. } => "Dedup",
        Cmd::Rename(_) => "Project",
        _ => "Project",
    }
}

/// An expression in Calcite's `RexNode` notation (`>($1, 1)`).
fn rex_expr(e: &Expr, pos: &dyn Fn(&str) -> usize) -> String {
    match &e.kind {
        E::Field(f) => format!("${}", pos(f)),
        E::Lit(v, _) => match v {
            Val::Str(s) => format!("'{s}'"),
            v => v.to_text(Ty::Undefined).unwrap_or_else(|| "null".into()),
        },
        E::Bin(op, a, b) => {
            let sym = match op {
                Op::Add => "+",
                Op::Sub => "-",
                Op::Mul => "*",
                Op::Div => "/",
                Op::Mod => "MOD",
                Op::Eq => "=",
                Op::Ne => "<>",
                Op::Lt => "<",
                Op::Le => "<=",
                Op::Gt => ">",
                Op::Ge => ">=",
                Op::And => "AND",
                Op::Or => "OR",
                Op::Xor => "XOR",
            };
            format!("{sym}({}, {})", rex_expr(a, pos), rex_expr(b, pos))
        }
        E::Not(x) => format!("NOT({})", rex_expr(x, pos)),
        E::IsNull(x, false) => format!("IS NULL({})", rex_expr(x, pos)),
        E::IsNull(x, true) => format!("IS NOT NULL({})", rex_expr(x, pos)),
        E::Call(n, args) => format!("{}({})", n.to_ascii_uppercase(), args.iter().map(|a| rex_expr(a, pos)).collect::<Vec<_>>().join(", ")),
        _ => e.text.clone(),
    }
}

/// The query a simple `where` pushes down.
fn filter_dsl(e: &Expr, fields: &exec::Fields) -> Option<Value> {
    match &e.kind {
        E::Bin(op, a, b) if op.is_cmp() => {
            let (E::Field(f), E::Lit(v, _)) = (&a.kind, &b.kind) else { return None };
            let lit = v.to_json(Ty::Undefined);
            let target = match fields.get(f) {
                Some(fd) if fd.mtype == "text" && fd.keyword => format!("{f}.keyword"),
                _ => f.clone(),
            };
            Some(match op {
                Op::Eq => crate::obj! { "term" => crate::obj! { target => crate::obj! { "value" => lit, "boost" => 1.0 } } },
                Op::Gt => crate::obj! { "range" => crate::obj! { target => crate::obj! { "from" => lit, "to" => Value::Null, "include_lower" => false, "include_upper" => true, "boost" => 1.0 } } },
                Op::Ge => crate::obj! { "range" => crate::obj! { target => crate::obj! { "from" => lit, "to" => Value::Null, "include_lower" => true, "include_upper" => true, "boost" => 1.0 } } },
                Op::Lt => crate::obj! { "range" => crate::obj! { target => crate::obj! { "from" => Value::Null, "to" => lit, "include_lower" => true, "include_upper" => false, "boost" => 1.0 } } },
                Op::Le => crate::obj! { "range" => crate::obj! { target => crate::obj! { "from" => Value::Null, "to" => lit, "include_lower" => true, "include_upper" => true, "boost" => 1.0 } } },
                _ => return None,
            })
        }
        E::IsNull(x, true) => match &x.kind {
            E::Field(f) => Some(crate::obj! { "exists" => crate::obj! { "field" => f.clone(), "boost" => 1.0 } }),
            _ => None,
        },
        _ => None,
    }
}

/// The plugin's `_source`-reading script for fields without doc values (a Java-serialized
/// string holding the dynamic parameter's type).
fn source_script(field: &str) -> Value {
    let json = "{\n  \"dynamicParam\": 0,\n  \"type\": {\n    \"type\": \"VARCHAR\",\n    \"nullable\": true,\n    \"precision\": -1\n  }\n}";
    let mut ser = vec![0xAC, 0xED, 0x00, 0x05, 0x74];
    ser.extend_from_slice(&(json.len() as u16).to_be_bytes());
    ser.extend_from_slice(json.as_bytes());
    use base64::Engine;
    let blob = base64::engine::general_purpose::STANDARD.encode(ser);
    crate::obj! {
        "source" => format!("{{\"langType\":\"calcite\",\"script\":\"{blob}\"}}"),
        "lang" => "opensearch_compounded_script",
        "params" => crate::obj! { "utcTimestamp" => 0i64, "SOURCES" => crate::arr![1i64], "DIGESTS" => crate::arr![field] }
    }
}

/// A field's value source in an aggregation: its doc values, its `.keyword` sub-field, or a
/// `_source` script for text-only fields.
fn value_source(f: &str, fields: &exec::Fields) -> (&'static str, Value) {
    match fields.get(f) {
        Some(fd) if fd.mtype == "text" && fd.keyword => ("field", Value::Str(format!("{f}.keyword"))),
        Some(fd) if fd.mtype == "text" || fd.mtype == "match_only_text" => ("script", source_script(f)),
        _ => ("field", Value::Str(f.to_string())),
    }
}

fn aggs_dsl(aggs: &[super::parser::Agg], groups: &[String], fields: &exec::Fields, s: &Settings) -> Value {
    let metric = |a: &super::parser::Agg| -> Option<(String, Value)> {
        let field = match a.args.first().map(|e| &e.kind) {
            Some(E::Field(f)) => f.clone(),
            _ => return None,
        };
        let kind = match a.func.as_str() {
            "count" => "value_count",
            "sum" => "sum",
            "avg" => "avg",
            "min" => "min",
            "max" => "max",
            "dc" => "cardinality",
            _ => return None,
        };
        let (k, v) = value_source(&field, fields);
        Some((a.name.clone(), crate::obj! { kind => crate::obj! { k => v } }))
    };
    let mut metrics = Map::new();
    for a in aggs {
        if a.func == "count" && a.args.is_empty() {
            continue;
        }
        if let Some((n, v)) = metric(a) {
            metrics.insert(n, v);
        }
    }
    if groups.is_empty() {
        return Value::Obj(metrics);
    }
    let sources: Vec<Value> = groups
        .iter()
        .map(|g| {
            let (k, v) = value_source(g, fields);
            let mut terms = Map::new();
            terms.insert(k.into(), v);
            terms.insert("missing_bucket".into(), Value::Bool(true));
            terms.insert("missing_order".into(), Value::Str("first".into()));
            terms.insert("order".into(), Value::Str("asc".into()));
            let mut src = Map::new();
            src.insert(g.clone(), crate::obj! { "terms" => Value::Obj(terms) });
            Value::Obj(src)
        })
        .collect();
    let mut comp = crate::obj! { "composite" => crate::obj! { "size" => s.size_limit as i64, "sources" => Value::Arr(sources) } };
    if !metrics.is_empty()
        && let Value::Obj(m) = &mut comp
    {
        m.insert("aggregations".into(), Value::Obj(metrics));
    }
    crate::obj! { "composite_buckets" => comp }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_patterns_mask_alphanumeric_runs() {
        let (p, toks) = simple_pattern("Verification succeeded for blk_-1547954353065580372", true, None);
        assert_eq!(p, "<token1> <token2> <token3> <token4>_-<token5>");
        assert_eq!(toks[4].1, "1547954353065580372");
        assert_eq!(simple_pattern("a-1", false, None).0, "<*>-<*>");
    }

    #[test]
    fn sed_expressions() {
        assert_eq!(parse_sed("s/(\\d+)/N/g"), Some(("(\\d+)".into(), "N".into(), true)));
    }
}
