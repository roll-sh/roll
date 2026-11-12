//! ES|QL execution: the source (`FROM` scans indices in batches — straight from doc values
//! ([`super::columnar`]) when every needed column has them, else through the search API — with
//! the leading `WHERE` pushed down as a query and a leading `LIMIT` as the scan's bound, `ROW`,
//! `SHOW`), then the processing commands over tables. `SET` settings apply here: unmapped
//! fields become null or `_source`-loaded columns, a time zone shifts the zone-sensitive date
//! functions to its wall clock.
//!
//! Commands that work row by row (`WHERE`, `EVAL`, `KEEP`, `DROP`, `RENAME`, `MV_EXPAND`) run on
//! each scanned batch as it arrives; the first command that needs every row (`STATS`, `SORT`)
//! consumes the batches (`STATS` aggregates them incrementally, so a `STATS` over an index
//! never holds the index in memory, `SORT | LIMIT n` keeps only the best `n` rows), and a
//! `LIMIT` stops the scan once it is satisfied.

use super::functions::{self, F, Fail};
use super::parser::{BinOp, Cmd, Expr, ExprKind, Query, Unmapped};
use super::value::{self, Acc, DataType, V, Warnings};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::obj;
use crate::xpack::internal;
use std::cmp::Ordering;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub struct Col {
    pub name: String,
    pub ty: DataType,
}

#[derive(Clone, Debug, Default)]
pub struct Table {
    pub cols: Vec<Col>,
    pub rows: Vec<Vec<V>>,
}

/// Figures Elasticsearch reports with each response.
#[derive(Default, Debug)]
pub struct Stats {
    pub documents_found: i64,
    pub values_loaded: i64,
    pub rows_emitted: i64,
    /// The concrete indices `FROM` read (`None`: no index source), for the profile.
    pub indices: Option<Vec<String>>,
    /// The fields loaded from them.
    pub fields: Vec<String>,
    /// The mapping types of the source's `unsupported` columns (several: a union type).
    pub original_types: Vec<(String, Vec<String>)>,
    /// Column `_meta` (`SET column_metadata=true`), by column name.
    pub col_meta: Vec<(String, Value)>,
}

/// The default and maximum number of rows a query returns.
pub const DEFAULT_LIMIT: usize = 1000;
pub const MAX_LIMIT: usize = 10_000;
const BATCH: usize = 1000;

/// A query's source text, and whether it came from SQL (whose functions are checked by the SQL
/// parser, not with ES|QL's rules).
pub struct Src<'a> {
    pub text: &'a str,
    pub sql: bool,
    /// The request's `locale`.
    pub locale: Option<&'a str>,
    /// Enrich tables and lookup indices the query reads, loaded before it runs.
    pub res: &'a Resources,
    /// The query's time zone (`SET time_zone`, the request's `time_zone`), when not UTC.
    pub tz: Option<crate::time::Zone>,
}

/// A union type: each mapping type of a field and the indices that map it.
type UnionTypes = Vec<(String, Vec<String>)>;

/// What `ENRICH` and `LOOKUP JOIN` read, loaded (asynchronously) before the pipeline is built.
#[derive(Default)]
pub struct Resources {
    /// Union types: source fields mapped with different types in different indices, each
    /// type with the indices that map it (sorted by type).
    unions: std::sync::OnceLock<HashMap<String, UnionTypes>>,
    /// Source fields of a type Elasticsearch doesn't support (`binary`, ranges, ...), with that
    /// type: any use other than `KEEP`/`DROP` fails.
    unsupported: std::sync::OnceLock<HashMap<String, String>>,
    /// By policy: its table and the types of its enrich fields.
    enrich: HashMap<String, (std::sync::Arc<crate::xpack::enrich::Table>, Vec<Col>)>,
    /// By index: its columns and every row.
    lookup: HashMap<String, Table>,
    /// The source fields Elasticsearch would resolve (`needed_columns`) and every source field:
    /// unknown-column suggestions only come from resolved fields, like Elasticsearch's, whose
    /// field-caps request asks for just those.
    suggest: std::sync::OnceLock<(Option<std::collections::HashSet<String>>, std::collections::HashSet<String>)>,
    /// `SET column_metadata=true`: the `_meta` of `BUCKET` groups, by output name.
    col_meta: parking_lot::Mutex<Vec<(String, Value)>>,
}

/// The `_meta.bucket` Elasticsearch reports for a `BUCKET` group: the numeric interval, or
/// the date rounding's unit and count.
fn bucket_meta(c: &CE) -> Option<Value> {
    match c {
        CE::ToLocal(x, _) | CE::FromLocal(x, _) => bucket_meta(x),
        CE::Bucket(_, BucketSpan::Num(s)) => Some(obj! { "bucket" => obj! { "interval" => *s } }),
        CE::Bucket(_, BucketSpan::Date(span)) => {
            let (n, unit) = span.split_once(' ')?;
            let n: i64 = n.parse().ok()?;
            let unit = match unit.trim_end_matches('s') {
                "millisecond" | "m" => "millisecond",
                "second" | "sec" => "second",
                "minute" | "min" => "minute",
                "hour" | "h" => "hour",
                "day" | "d" => "day",
                "week" | "w" => "week",
                "month" | "mo" => "month",
                "quarter" | "q" => "quarter",
                "year" | "yr" | "y" => "year",
                other => return Some(obj! { "bucket" => obj! { "unit" => other.to_string(), "interval" => n } }),
            };
            Some(obj! { "bucket" => obj! { "unit" => unit, "interval" => n } })
        }
        _ => None,
    }
}

impl std::ops::Deref for Src<'_> {
    type Target = str;
    fn deref(&self) -> &str {
        self.text
    }
}

/// Where an expression sits and its source text, for evaluation warnings.
#[derive(Debug)]
struct At {
    loc: String,
    text: String,
}

fn at_of(src: &str, e: &Expr) -> std::sync::Arc<At> {
    let (l, c) = super::parser::location(src, e.start);
    std::sync::Arc::new(At { loc: format!("{l}:{c}"), text: src[e.start..e.end].to_string() })
}

/// Records Elasticsearch's two warnings for a failed evaluation.
fn warn_failure(w: &mut Warnings, at: &At, msg: &str) {
    w.add(format!("Line {}: evaluation of [{}] failed, treating result as null. Only first 20 failures recorded.", at.loc, at.text));
    w.add(format!("Line {}: {msg}", at.loc));
}

/// Records Elasticsearch's warnings for an evaluation with several failures (one header, then
/// each failure).
fn warn_failures(w: &mut Warnings, at: &At, msgs: &[String]) {
    w.add(format!("Line {}: evaluation of [{}] failed, treating result as null. Only first 20 failures recorded.", at.loc, at.text));
    for m in msgs {
        w.add(format!("Line {}: {m}", at.loc));
    }
}

/// `Unknown column [x]`, with Elasticsearch's suggestions of similar names.
fn unknown_column(src: &Src<'_>, at: usize, name: &str, cols: &[Col]) -> EsError {
    let resolved = |c: &str| match src.res.suggest.get() {
        Some((Some(need), source)) if !src.sql => need.contains(c) || !source.contains(c),
        _ => true,
    };
    let mut scored: Vec<(f32, &str)> = cols.iter().filter(|c| resolved(&c.name)).map(|c| (similarity(name, &c.name), c.name.as_str())).filter(|(s, _)| *s >= 0.5).collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.dedup_by(|a, b| a.1 == b.1);
    let msg = match scored.as_slice() {
        [] => format!("Unknown column [{name}]"),
        [(_, one)] => format!("Unknown column [{name}], did you mean [{one}]?"),
        many => format!("Unknown column [{name}], did you mean any of [{}]?", many.iter().map(|(_, n)| *n).collect::<Vec<_>>().join(", ")),
    };
    verification(src, at, &msg)
}

/// Lucene's `LevenshteinDistance.getDistance`: `1 - edits / max(len)` over code points.
fn similarity(a: &str, b: &str) -> f32 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() || b.is_empty() {
        return if a == b { 1.0 } else { 0.0 };
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    1.0 - prev[b.len()] as f32 / a.len().max(b.len()) as f32
}

/// Mapping types Elasticsearch 9.5.5's ES|QL has no data type for (they read as `unsupported`
/// and can't be used in expressions). Types it supports that roll doesn't load yet (dense
/// vectors, histograms, `flattened`, ...) aren't here: those columns stay `null`.
fn es_unsupported_type(t: &str) -> bool {
    DataType::of_mapping(t) == DataType::Unsupported
        && !matches!(
            t,
            "object" | "nested" | "date_range" | "token_count" | "alias"
        )
}

/// One `verification_exception` listing every problem of `errs` (each `Found 1 problem\n...`).
fn merge_problems(mut errs: Vec<EsError>) -> EsError {
    if errs.len() == 1 {
        return errs.pop().expect("one");
    }
    let lines: Vec<String> = errs.iter().map(|e| e.reason().split_once('\n').map_or(e.reason(), |(_, l)| l).to_string()).collect();
    EsError::new(400, "verification_exception", format!("Found {} problems\n{}", lines.len(), lines.join("\n")))
}

/// `Cannot use field [f] due to ambiguities ...` at `at`, `written` as the query writes it.
fn ambiguous_at(src: &str, at: usize, written: &str, types: &[(String, Vec<String>)]) -> EsError {
    let list: Vec<String> = types.iter().map(|(t, idx)| format!("[{t}] in [{}]", idx.join(", "))).collect();
    verification(src, at, &format!("Cannot use field [{written}] due to ambiguities being mapped as [{}] incompatible types: {}", types.len(), list.join(", ")))
}

fn verification(src: &str, at: usize, msg: &str) -> EsError {
    let (l, c) = super::parser::location(src, at);
    EsError::new(400, "verification_exception", format!("Found 1 problem\nline {l}:{c}: {msg}"))
}

/// A compiled expression over a table's columns.
#[derive(Clone, Debug)]
enum CE {
    Lit(V),
    Col(usize),
    Neg(Box<CE>),
    Not(Box<CE>),
    Arith(BinOp, Box<CE>, Box<CE>, DataType, std::sync::Arc<At>),
    Cmp(BinOp, Box<CE>, Box<CE>, std::sync::Arc<At>),
    And(Box<CE>, Box<CE>),
    Or(Box<CE>, Box<CE>),
    IsNull(Box<CE>, bool),
    Like(Box<CE>, std::sync::Arc<Matcher>, bool, std::sync::Arc<At>),
    In(Box<CE>, Vec<CE>, bool, std::sync::Arc<At>),
    /// A function of SQL's that ES|QL doesn't have ([`value::scalar`]).
    Call(String, Vec<CE>, Vec<DataType>),
    Fn(F, Vec<CE>, Vec<DataType>, std::sync::Arc<At>),
    /// The average (`sum / value_count`) of an `aggregate_metric_double`, what functions
    /// that don't know the type read.
    AmdAverage(Box<CE>),
    /// `CASE`: conditions and values, with each condition's place for its multi-value warning.
    Case(Vec<CE>, Vec<std::sync::Arc<At>>),
    Cast(Box<CE>, DataType),
    /// A date plus or minus a span literal.
    DateShift(Box<CE>, Box<CE>, i64),
    /// The result of aggregation `i` (inside `STATS`).
    Agg(usize),
    /// `BUCKET(field, span)` in a `BY` clause.
    Bucket(Box<CE>, BucketSpan),
    /// A date as the zone's wall clock read as UTC (zone-sensitive functions take dates so).
    ToLocal(Box<CE>, crate::time::Zone),
    /// The inverse: a wall-clock date of the zone back to the instant.
    FromLocal(Box<CE>, crate::time::Zone),
}

/// Applies `f` to a date and each date of a multi-value.
fn map_dates(v: V, f: &impl Fn(i64) -> i64) -> V {
    match v {
        V::Date(ms) => V::Date(f(ms)),
        V::Multi(vs) => V::Multi(vs.into_iter().map(|x| map_dates(x, f)).collect()),
        other => other,
    }
}

fn naive(ms: i64) -> chrono::NaiveDateTime {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default().naive_utc()
}

/// `ms` (UTC) as the wall clock of `z`.
fn to_local(z: &crate::time::Zone, ms: i64) -> i64 {
    ms + z.offset_at_utc(naive(ms)) as i64 * 1000
}

/// A wall-clock time of `z` back to UTC.
fn from_local(z: &crate::time::Zone, ms: i64) -> i64 {
    ms - z.offset_for_local(naive(ms)) as i64 * 1000
}

/// A `LIKE` / `RLIKE` pattern compiled once per query. ES|QL matches with Lucene automata
/// (`WildcardPattern` / `RLikePattern.createAutomaton`); SQL evaluates `RLIKE` with Java regular
/// expressions.
#[derive(Debug)]
pub(super) enum Matcher {
    Dfa(Box<search_codec::automaton::Dfa>),
    Regex(regex::Regex),
    /// A pattern that doesn't compile: matched per value as before (never true for regexes).
    Raw(String, bool),
}

impl Matcher {
    /// Compiles `p`; ES|QL patterns that don't compile fail like Elasticsearch's
    /// (`Invalid regex pattern for RLIKE [p]: [why]`, also for `LIKE`), SQL ones never match.
    fn new(p: &str, rlike: bool, sql: bool) -> Result<Matcher, String> {
        use search_codec::automaton::{Dfa, dfa::DEFAULT_WORK_LIMIT, regexp};
        const TOO_COMPLEX: &str = "Pattern was too complex to determinize";
        let invalid = |why: &str| Err(format!("Invalid regex pattern for RLIKE [{p}]: [{why}]"));
        match (rlike, sql) {
            (true, true) => Ok(regex::Regex::new(&format!("^(?:{p})$")).map(Matcher::Regex).unwrap_or_else(|_| Matcher::Raw(p.to_string(), rlike))),
            (false, true) => Ok(Dfa::wildcard(p, false, DEFAULT_WORK_LIMIT).map(|d| Matcher::Dfa(Box::new(d))).unwrap_or_else(|_| Matcher::Raw(p.to_string(), rlike))),
            (false, false) => match Dfa::wildcard(p, false, DEFAULT_WORK_LIMIT) {
                Ok(d) => Ok(Matcher::Dfa(Box::new(d))),
                Err(_) => invalid(TOO_COMPLEX),
            },
            (true, false) => match regexp::compile(p, regexp::ALL, false, DEFAULT_WORK_LIMIT) {
                Ok(d) => Ok(Matcher::Dfa(Box::new(d))),
                Err(regexp::RegexpError::TooComplex(_)) => invalid(TOO_COMPLEX),
                Err(regexp::RegexpError::Syntax(why)) => invalid(&why),
            },
        }
    }

    fn matches(&self, s: &str) -> bool {
        use search_codec::automaton::Automaton;
        match self {
            Matcher::Dfa(d) => d.matches(s.as_bytes()),
            Matcher::Regex(r) => r.is_match(s),
            Matcher::Raw(p, rlike) => value::like(s, p, *rlike),
        }
    }
}

#[derive(Clone, Debug)]
enum BucketSpan {
    /// Numbers: `floor(x / span) * span`.
    Num(f64),
    /// Dates: truncation to this span (`1 day`, `12 hours`, ...).
    Date(String),
}

/// A date `BUCKET`, rounded on the query time zone's wall clock.
fn zoned_bucket(src: &Src<'_>, field: CE, span: BucketSpan) -> CE {
    match &src.tz {
        Some(z) => CE::FromLocal(Box::new(CE::Bucket(Box::new(CE::ToLocal(Box::new(field), z.clone())), span)), z.clone()),
        None => CE::Bucket(Box::new(field), span),
    }
}

/// `BUCKET(field, buckets_or_span [, from, to])` (`Bucket` in Elasticsearch).
fn compile_bucket(src: &Src<'_>, cols: &[Col], e: &Expr, args: &[Expr]) -> EsResult<(CE, DataType)> {
    if !(2..=4).contains(&args.len()) || args.len() == 3 {
        let msg = if args.len() < 2 || args.len() > 4 { "expects minimum two, maximum four arguments" } else { "expects exactly four arguments" };
        let (l, c) = super::parser::location(src, e.start);
        let cause = EsError::java(400, "ql_illegal_argument_exception", msg);
        return Err(EsError::new(400, "parsing_exception", format!("line {l}:{c}: error building [bucket]: {msg}")).caused_by(cause.clone()).with_root_causes(vec![cause]));
    }
    let (field, ft) = Compiler { src, cols, aggs: None }.compile(&args[0])?;
    let (_, bt) = Compiler { src, cols, aggs: None }.compile(&args[1])?;
    let call = &src[e.start..e.end];
    let lit = |x: &Expr| if let ExprKind::Lit(v) = &x.kind { Some(v.clone()) } else { None };
    let is_date = matches!(ft, DataType::Date | DataType::DateNanos);
    if !is_date && !ft.is_numeric() && ft != DataType::Null {
        return Err(verification(src, e.start, &format!("first argument of [{call}] must be [datetime, date_nanos or numeric], found value [{}] type [{}]", &src[args[0].start..args[0].end], functions::type_name_static(ft))));
    }
    if args.len() == 2 {
        if is_date {
            // A string literal (or parameter) naming a span works like the span literal.
            let span = match &args[1].kind {
                ExprKind::Span(span) => Some(span.clone()),
                ExprKind::Lit(V::Str(s)) => super::parser::span_text(s),
                _ => None,
            };
            let Some(span) = span else {
                return Err(verification(src, e.start, &format!("second argument of [{call}] must be [date_period or time_duration], found value [{}] type [{}]", &src[args[1].start..args[1].end], functions::type_name_static(bt))));
            };
            return Ok((zoned_bucket(src, field, BucketSpan::Date(span)), ft));
        }
        let Some(span) = lit(&args[1]).and_then(|v| v.as_f64()).filter(|_| bt.is_numeric()) else {
            return Err(verification(src, e.start, &format!("second argument of [{call}] must be [numeric], found value [{}] type [{}]", &src[args[1].start..args[1].end], functions::type_name_static(bt))));
        };
        // SQL's HISTOGRAM keeps the field's type (the values are still the bucket keys).
        return Ok((CE::Bucket(Box::new(field), BucketSpan::Num(span)), if src.sql { ft } else { DataType::Double }));
    }
    let buckets = lit(&args[1]).and_then(|v| v.as_i64()).unwrap_or(0);
    let bound = |x: &Expr| -> Option<f64> {
        match lit(x)? {
            V::Str(s) if is_date => value::parse_date(&s).map(|d| d as f64),
            v => v.as_f64(),
        }
    };
    let (Some(from), Some(to)) = (bound(&args[2]), bound(&args[3])) else {
        return Err(verification(src, e.start, &format!("third argument of [{call}] must be a constant, received [{}]", &src[args[2].start..args[2].end])));
    };
    if is_date {
        return Ok((zoned_bucket(src, field, BucketSpan::Date(pick_date_rounding(buckets, from as i64, to as i64))), ft));
    }
    let precise = (to - from) / buckets as f64;
    let next = 10f64.powf(precise.log10().ceil());
    let half = next / 2.0;
    Ok((CE::Bucket(Box::new(field), BucketSpan::Num(if precise < half { half } else { next })), DataType::Double))
}

/// `Bucket.DateRoundingPicker.pickRounding` in UTC: the finest unit giving at most `buckets`
/// buckets over `[from, to)`.
fn pick_date_rounding(buckets: i64, from: i64, to: i64) -> String {
    const PRIMARY: &[(&str, i64)] = &[
        ("1 day", 86_400_000),
        ("12 hours", 43_200_000),
        ("3 hours", 10_800_000),
        ("1 hour", 3_600_000),
        ("30 minutes", 1_800_000),
        ("10 minutes", 600_000),
        ("5 minutes", 300_000),
        ("1 minute", 60_000),
        ("30 seconds", 30_000),
        ("10 seconds", 10_000),
        ("5 seconds", 5_000),
        ("1 second", 1_000),
        ("100 milliseconds", 100),
        ("50 milliseconds", 50),
        ("10 milliseconds", 10),
        ("1 millisecond", 1),
    ];
    let ok_fixed = |w: i64| -> bool {
        if buckets <= 0 {
            return false;
        }
        let start = from.div_euclid(w) * w;
        if start >= to {
            return true;
        }
        let count = (to - start + w - 1) / w;
        count <= buckets
    };
    let ok_calendar = |unit: &str| -> bool {
        if buckets <= 0 {
            return false;
        }
        let Some(mut b) = value::date_trunc(unit, from) else { return false };
        for _ in 0..buckets {
            b = value::shift_date(b, unit, 1).unwrap_or(i64::MAX);
            if b >= to {
                return true;
            }
        }
        false
    };
    let (mut lo, mut hi) = (0i64, PRIMARY.len() as i64 - 1);
    let mut best = None;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        if ok_fixed(PRIMARY[mid as usize].1) {
            best = Some(mid as usize);
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    if let Some(b) = best {
        return PRIMARY[b].0.to_string();
    }
    for unit in ["1 week", "1 month", "1 year"] {
        if ok_calendar(unit) {
            return unit.to_string();
        }
    }
    "1 year".to_string()
}

struct Compiler<'a> {
    src: &'a Src<'a>,
    cols: &'a [Col],
    /// `Some` inside `STATS`: aggregation calls found, as (function, argument, star, extra args).
    aggs: Option<&'a mut Vec<AggSpec>>,
}

#[derive(Clone, Debug)]
struct AggSpec {
    func: String,
    arg: Option<CE>,
    arg_ty: DataType,
    extra: Vec<V>,
    /// `agg WHERE cond`: only rows matching `cond` are aggregated.
    filter: Option<CE>,
    /// `WEIGHTED_AVG`'s weight.
    weight: Option<CE>,
}

impl Compiler<'_> {
    /// The column and union types of `e` when it names a union-typed source field.
    fn union_of(&self, e: &Expr) -> Option<(usize, &UnionTypes)> {
        let ExprKind::Col(name) = &e.kind else { return None };
        let i = self.cols.iter().rposition(|c| c.name == *name)?;
        if self.cols[i].ty != DataType::Unsupported {
            return None;
        }
        self.src.res.unions.get()?.get(name).map(|u| (i, u))
    }

    /// Why the column `name` at `i` can't be used in an expression or `RENAME` (`at` is where it
    /// is written): a union type, or a type Elasticsearch doesn't support.
    fn unusable(src: &Src<'_>, cols: &[Col], i: usize, at: usize, written: &str) -> Option<EsError> {
        if src.sql || cols[i].ty != DataType::Unsupported {
            return None;
        }
        let name = cols[i].name.as_str();
        if let Some(u) = src.res.unions.get().and_then(|m| m.get(name)) {
            return Some(ambiguous_at(src, at, written, u));
        }
        let ty = src.res.unsupported.get().and_then(|m| m.get(name))?;
        Some(verification(src, at, &format!("Cannot use field [{written}] with unsupported type [{ty}]")))
    }

    /// `Cannot use field [f] due to ambiguities ...` for a union-typed field used where no
    /// conversion resolves it.
    fn ambiguous(&self, e: &Expr, types: &[(String, Vec<String>)]) -> EsError {
        ambiguous_at(self.src, e.start, &self.src[e.start..e.end], types)
    }

    fn col(&self, name: &str, at: usize) -> EsResult<(usize, DataType)> {
        match self.cols.iter().rposition(|c| c.name == name) {
            Some(i) => Ok((i, self.cols[i].ty)),
            None => Err(unknown_column(self.src, at, name, self.cols)),
        }
    }

    fn compile(&mut self, e: &Expr) -> EsResult<(CE, DataType)> {
        Ok(match &e.kind {
            ExprKind::Lit(v) => (CE::Lit(v.clone()), v.type_of()),
            ExprKind::Col(c) => {
                if let Some((_, u)) = self.union_of(e) {
                    return Err(self.ambiguous(e, u));
                }
                let (i, t) = self.col(c, e.start)?;
                if let Some(err) = Self::unusable(self.src, self.cols, i, e.start, &self.src[e.start..e.end]) {
                    return Err(err);
                }
                (CE::Col(i), t)
            }
            ExprKind::Star => (CE::Lit(V::Null), DataType::Null),
            ExprKind::Span(s) => {
                // Calendar units are periods; the rest fixed durations.
                let unit = s.split_once(' ').map_or("", |(_, u)| u);
                let fixed = matches!(unit.trim_end_matches('s'), "millisecond" | "ms" | "second" | "sec" | "s" | "minute" | "min" | "hour" | "h");
                (CE::Lit(V::Str(s.clone())), if fixed { DataType::TimeDuration } else { DataType::DatePeriod })
            }
            ExprKind::Neg(x) => {
                let (c, t) = self.compile(x)?;
                if !t.is_numeric() && t != DataType::Null {
                    return Err(verification(self.src, e.start, &format!("argument of [{}] must be [numeric, date_period or time_duration], found value [{}] type [{}]", &self.src[e.start..e.end], &self.src[x.start..x.end], functions::type_name_static(t))));
                }
                (CE::Neg(Box::new(c)), t)
            }
            ExprKind::Not(x) => {
                let (c, t) = self.compile(x)?;
                if t != DataType::Boolean && t != DataType::Null && !self.src.sql {
                    return Err(verification(self.src, e.start, &format!("argument of [{}] must be [boolean], found value [{}] type [{}]", &self.src[e.start..e.end], &self.src[x.start..x.end], functions::type_name_static(t))));
                }
                (CE::Not(Box::new(c)), DataType::Boolean)
            }
            ExprKind::Bin(op, l, r) => {
                let (mut lc, mut lt) = self.compile(l)?;
                let (mut rc, mut rt) = self.compile(r)?;
                if matches!(op, BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Lte | BinOp::Gt | BinOp::Gte) {
                    // Comparisons read an `aggregate_metric_double` as its average; the other
                    // metric and binary types don't compare.
                    for (c, t) in [(&mut lc, &mut lt), (&mut rc, &mut rt)] {
                        if *t == DataType::AggregateMetricDouble {
                            *c = CE::AmdAverage(Box::new(std::mem::replace(c, CE::Lit(V::Null))));
                            *t = DataType::Double;
                        }
                    }
                    for (i, (t, x)) in [(lt, l), (rt, r)].into_iter().enumerate() {
                        if t.is_counter() || matches!(t, DataType::Histogram | DataType::ExponentialHistogram | DataType::TDigest | DataType::Tsid) {
                            let accepted = if matches!(op, BinOp::Eq | BinOp::Neq) {
                                "boolean, cartesian_point, cartesian_shape, date_nanos, date_range, datetime, dense_vector, double, flattened, geo_point, geo_shape, geohash, geohex, geotile, integer, ip, keyword, long, text, unsigned_long or version"
                            } else {
                                "date_nanos, datetime, double, integer, ip, keyword, long, text, unsigned_long or version"
                            };
                            return Err(verification(
                                self.src,
                                e.start,
                                &format!("{} argument of [{}] must be [{accepted}], found value [{}] type [{}]", if i == 0 { "first" } else { "second" }, &self.src[e.start..e.end], &self.src[x.start..x.end], t.name()),
                            ));
                        }
                    }
                }
                match op {
                    BinOp::And => (CE::And(Box::new(lc), Box::new(rc)), DataType::Boolean),
                    BinOp::Or => (CE::Or(Box::new(lc), Box::new(rc)), DataType::Boolean),
                    BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Lte | BinOp::Gt | BinOp::Gte => {
                        let compatible = lt == rt
                            || lt == DataType::Null
                            || rt == DataType::Null
                            || (lt.is_numeric() && rt.is_numeric())
                            || (lt.is_string() && rt.is_string())
                            || matches!((lt, rt), (DataType::Date, DataType::Keyword) | (DataType::Keyword, DataType::Date) | (DataType::Ip, DataType::Keyword) | (DataType::Keyword, DataType::Ip) | (DataType::Version, DataType::Keyword) | (DataType::Keyword, DataType::Version));
                        if !compatible {
                            return Err(verification(
                                self.src,
                                e.start,
                                &{
                                    // Numeric types are reported as a family.
                                    let family = if lt.is_numeric() { "numeric" } else { lt.name() };
                                    format!("first argument of [{}] is [{family}] so second argument must also be [{family}] but was [{}]", &self.src[e.start..e.end], rt.name())
                                },
                            ));
                        }
                        // A comparison with the `null` literal folds to `null` (no evaluation, so no
                        // multi-value warning), as Elasticsearch's planner folds it.
                        if matches!(lc, CE::Lit(V::Null)) || matches!(rc, CE::Lit(V::Null)) {
                            return Ok((CE::Lit(V::Null), DataType::Boolean));
                        }
                        (CE::Cmp(*op, Box::new(lc), Box::new(rc), at_of(self.src, e)), DataType::Boolean)
                    }
                    BinOp::Add | BinOp::Sub if matches!(lt, DataType::Date | DataType::DateNanos) && matches!(rt, DataType::DatePeriod | DataType::TimeDuration) => {
                        (CE::DateShift(Box::new(lc), Box::new(rc), if *op == BinOp::Add { 1 } else { -1 }), DataType::Date)
                    }
                    BinOp::Add if matches!(rt, DataType::Date | DataType::DateNanos) && matches!(lt, DataType::DatePeriod | DataType::TimeDuration) => {
                        (CE::DateShift(Box::new(rc), Box::new(lc), 1), DataType::Date)
                    }
                    _ => {
                        for (t, x) in [(lt, l), (rt, r)] {
                            if !t.is_numeric() && t != DataType::Null {
                                return Err(verification(
                                    self.src,
                                    e.start,
                                    &format!(
                                        "{} argument of [{}] must be [date_nanos, datetime, numeric or dense_vector], found value [{}] type [{}]",
                                        if std::ptr::eq(x.as_ref(), l.as_ref()) { "first" } else { "second" },
                                        &self.src[e.start..e.end],
                                        &self.src[x.start..x.end],
                                        t.name()
                                    ),
                                ));
                            }
                        }
                        let ty = DataType::widen(lt, rt);
                        (CE::Arith(*op, Box::new(lc), Box::new(rc), ty, at_of(self.src, e)), ty)
                    }
                }
            }
            ExprKind::IsNull(x, neg) => (CE::IsNull(Box::new(self.compile(x)?.0), *neg), DataType::Boolean),
            ExprKind::Like(x, p, rlike, neg) => {
                let m = Matcher::new(p, *rlike, self.src.sql).map_err(|msg| super::parser::parse_error(self.src, e.start, &msg))?;
                (CE::Like(Box::new(self.compile(x)?.0), std::sync::Arc::new(m), *neg, at_of(self.src, e)), DataType::Boolean)
            }
            ExprKind::In(x, list, neg) => {
                let (c, _) = self.compile(x)?;
                let mut l = Vec::new();
                for i in list {
                    l.push(self.compile(i)?.0);
                }
                (CE::In(Box::new(c), l, *neg, at_of(self.src, e)), DataType::Boolean)
            }
            ExprKind::Cast(x, ty) => {
                // ES|QL has no conversion to text (`TO_STRING` gives keywords); SQL casts to
                // `VARCHAR`/`TEXT` do.
                let Some(t) = DataType::parse_name(ty).filter(|t| *t != DataType::Text || self.src.sql) else {
                    return Err(verification(self.src, e.start, &format!("Unsupported conversion to type [{}]", ty.to_ascii_uppercase())));
                };
                // A cast resolves a union type when every one of its types converts.
                if let Some((i, u)) = self.union_of(x) {
                    let f = functions::conversion_to(t);
                    if !f.is_some_and(|f| u.iter().all(|(ut, _)| functions::conversion_accepts(f, ut))) {
                        return Err(self.ambiguous(x, u));
                    }
                    return Ok((CE::Cast(Box::new(CE::Col(i)), t), t));
                }
                let (c, _) = self.compile(x)?;
                (CE::Cast(Box::new(c), t), t)
            }
            ExprKind::Call(name, args) if name == "$where" && args.len() == 2 => {
                let Some(aggs) = self.aggs.as_deref_mut() else { return Err(verification(self.src, e.start, "WHERE is only allowed after an aggregation")) };
                let before = aggs.len();
                let (c, t) = Compiler { src: self.src, cols: self.cols, aggs: Some(aggs) }.compile(&args[0])?;
                let (cond, ct) = Compiler { src: self.src, cols: self.cols, aggs: None }.compile(&args[1])?;
                if ct != DataType::Boolean && ct != DataType::Null {
                    return Err(verification(self.src, args[1].start, &format!("Condition expression needs to be boolean, found [{}]", ct.name().to_ascii_uppercase())));
                }
                let Some(aggs) = self.aggs.as_deref_mut() else { unreachable!() };
                if aggs.len() == before {
                    return Err(verification(self.src, args[0].start, &format!("WHERE clause allowed only for aggregate functions, none found in [{}]", &self.src[e.start..e.end])));
                }
                for a in &mut aggs[before..] {
                    a.filter = Some(cond.clone());
                }
                (c, t)
            }
            ExprKind::Call(name, _) if name == "bucket" || name == "bin" => {
                return Err(verification(self.src, e.start, &format!("can only use grouping function [{}] as part of the BY clause", &self.src[e.start..e.end])));
            }
            ExprKind::Call(name, args) => {
                if value::AGGS.contains(&name.as_str()) {
                    // A field that is `aggregate_metric_double` in some indices and numeric in
                    // the others aggregates as metrics (the numbers cast implicitly).
                    let amd_union = args
                        .first()
                        .and_then(|a| self.union_of(a))
                        .filter(|(_, u)| u.iter().any(|(t, _)| t == "aggregate_metric_double") && u.iter().all(|(t, _)| t == "aggregate_metric_double" || DataType::of_mapping(t).is_numeric()))
                        .map(|(i, _)| i);
                    let Some(aggs) = self.aggs.as_deref_mut() else {
                        return Err(verification(self.src, e.start, &format!("aggregate function [{}] not allowed outside STATS command", &self.src[e.start..e.end])));
                    };
                    let star = matches!(args.first().map(|a| &a.kind), Some(ExprKind::Star));
                    if args.is_empty() && name != "count" {
                        return Err(verification(self.src, e.start, &format!("error building [{name}]: expects exactly one argument")));
                    }
                    let mut inner = Compiler { src: self.src, cols: self.cols, aggs: None };
                    let (mut arg, mut arg_ty) = if let Some(i) = amd_union {
                        (Some(CE::Cast(Box::new(CE::Col(i)), DataType::AggregateMetricDouble)), DataType::AggregateMetricDouble)
                    } else if star || args.is_empty() {
                        (None, DataType::Null)
                    } else {
                        let (c, t) = inner.compile(&args[0])?;
                        (Some(c), t)
                    };
                    // `aggregate_metric_double`: SUM/MIN/MAX/COUNT/AVG combine the sub-metrics,
                    // every other aggregation reads each value's average.
                    let mut amd_func: Option<String> = None;
                    if arg_ty == DataType::AggregateMetricDouble {
                        if matches!(name.as_str(), "sum" | "min" | "max" | "count" | "avg") {
                            amd_func = Some(format!("$amd_{name}"));
                        } else if let Some(a) = arg.take() {
                            arg = Some(CE::AmdAverage(Box::new(a)));
                            arg_ty = DataType::Double;
                        }
                    }
                    if let Some(f) = amd_func {
                        let ty = if f == "$amd_count" { DataType::Long } else { DataType::Double };
                        aggs.push(AggSpec { func: f, arg, arg_ty, extra: Vec::new(), filter: None, weight: None });
                        return Ok((CE::Agg(aggs.len() - 1), ty));
                    }
                    if matches!(name.as_str(), "sum" | "avg" | "median" | "percentile" | "std_dev" | "median_absolute_deviation") && !arg_ty.is_numeric() && arg_ty != DataType::Null {
                        return Err(verification(
                            self.src,
                            e.start,
                            &{
                                let (which, accepted) = match name.as_str() {
                                    "sum" | "avg" => ("argument", "aggregate_metric_double, exponential_histogram, tdigest or numeric except unsigned_long or counter types"),
                                    "median" => ("argument", "exponential_histogram, tdigest or numeric except unsigned_long or counter types"),
                                    "percentile" => ("first argument", "exponential_histogram, tdigest or numeric except unsigned_long"),
                                    _ => ("argument", "numeric except unsigned_long or counter types"),
                                };
                                format!("{which} of [{}] must be [{accepted}], found value [{}] type [{}]", &self.src[e.start..e.end], &self.src[args[0].start..args[0].end], functions::type_name_static(arg_ty))
                            },
                        ));
                    }
                    if matches!(name.as_str(), "min" | "max") && super::spatial::is_spatial(arg_ty) {
                        return Err(verification(
                            self.src,
                            e.start,
                            &format!(
                                "argument of [{}] must be [boolean, date, ip, string, version, aggregate_metric_double, exponential_histogram, tdigest or numeric except counter types], found value [{}] type [{}]",
                                &self.src[e.start..e.end],
                                &self.src[args[0].start..args[0].end],
                                arg_ty.name()
                            ),
                        ));
                    }
                    // Parameters must be constants (folded while planning).
                    let mut extra = Vec::new();
                    let call = &self.src[e.start..e.end];
                    for (i, a) in args.iter().enumerate().skip(1) {
                        match fold(self.src, a) {
                            Some(v) => extra.push(v),
                            None if matches!(name.as_str(), "count_distinct" | "percentile" | "top" | "sample") => {
                                return Err(verification(self.src, e.start, &format!("{} argument of [{call}] must be a constant, received [{}]", ["first", "second", "third", "fourth"][i.min(3)], &self.src[a.start..a.end])));
                            }
                            None => {}
                        }
                    }
                    // Located at the offending argument.
                    if matches!(name.as_str(), "top" | "sample")
                        && let Some(n) = extra.first().and_then(V::as_i64)
                        && n <= 0
                    {
                        return Err(verification(self.src, args[1].start, &format!("Limit must be greater than 0 in [{call}], found [{n}]")));
                    }
                    if name == "top"
                        && let Some(V::Str(o)) = extra.get(1)
                        && !o.eq_ignore_ascii_case("asc")
                        && !o.eq_ignore_ascii_case("desc")
                    {
                        return Err(verification(self.src, args[2].start, &format!("Invalid order value in [{call}], expected [ASC, DESC] but got [{o}]")));
                    }
                    let ty = value::agg_type(name, arg_ty);
                    let weight = if name == "weighted_avg" || name.starts_with('$') {
                        match args.get(1) {
                            Some(w) => Some(Compiler { src: self.src, cols: self.cols, aggs: None }.compile(w)?.0),
                            None => return Err(verification(self.src, e.start, "error building [weighted_avg]: expects exactly two arguments")),
                        }
                    } else {
                        None
                    };
                    aggs.push(AggSpec { func: name.clone(), arg: if star { None } else { arg }, arg_ty, extra, filter: None, weight });
                    return Ok((CE::Agg(aggs.len() - 1), ty));
                }
                // A conversion function resolves a union type when it takes every one of its
                // types; each row converts the value its own index holds.
                if let (Some(f), [arg]) = (functions::resolve(name), args.as_slice())
                    && functions::is_conversion(f)
                    && let Some((i, u)) = self.union_of(arg)
                {
                    if !u.iter().all(|(ut, _)| functions::conversion_accepts(f, ut)) {
                        return Err(self.ambiguous(arg, u));
                    }
                    let ty = f.result_type(&[DataType::Keyword]);
                    return Ok((CE::Fn(f, vec![CE::Col(i)], vec![DataType::Keyword], at_of(self.src, e)), ty));
                }
                let mut cs = Vec::new();
                let mut ts = Vec::new();
                for a in args {
                    let (c, t) = self.compile(a)?;
                    cs.push(c);
                    ts.push(t);
                }
                if let Some(f) = functions::resolve(name) {
                    // String literals where numbers or dates are expected are converted while
                    // planning (`Cannot convert string [..]` when they don't parse).
                    if !self.src.sql {
                        for (i, a) in args.iter().enumerate() {
                            let ExprKind::Lit(V::Str(text)) = &a.kind else { continue };
                            if functions::accepts(f, args.len(), i, &ts, DataType::Keyword) {
                                continue;
                            }
                            if functions::accepts(f, args.len(), i, &ts, DataType::Double) {
                                match text.trim().parse::<f64>() {
                                    Ok(d) => {
                                        cs[i] = CE::Lit(V::Double(d));
                                        ts[i] = DataType::Double;
                                    }
                                    Err(_) => return Err(verification(self.src, a.start, &format!("Cannot convert string [{text}] to [DOUBLE], error [Cannot parse number [{text}]]"))),
                                }
                            } else if functions::accepts(f, args.len(), i, &ts, DataType::Long) {
                                match text.trim().parse::<i64>() {
                                    Ok(l) => {
                                        cs[i] = CE::Lit(V::Long(l));
                                        ts[i] = DataType::Long;
                                    }
                                    Err(_) => return Err(verification(self.src, a.start, &format!("Cannot convert string [{text}] to [LONG], error [Cannot parse number [{text}]]"))),
                                }
                            } else if functions::accepts(f, args.len(), i, &ts, DataType::Date) {
                                match value::parse_date(text) {
                                    Some(ms) => {
                                        cs[i] = CE::Lit(V::Date(ms));
                                        ts[i] = DataType::Date;
                                    }
                                    None => {
                                        return Err(verification(
                                            self.src,
                                            a.start,
                                            &format!("Cannot convert string [{text}] to [DATETIME], error [failed to parse date field [{text}] with format [strict_date_optional_time]]"),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    let ty = if self.src.sql {
                        f.result_type(&ts)
                    } else {
                        // Constant expressions count as constants (Elasticsearch folds them).
                        let folded: Vec<Option<V>> = args.iter().map(|a| fold(self.src, a)).collect();
                        let argv: Vec<functions::Arg<'_>> =
                            args.iter().zip(&ts).zip(&folded).map(|((a, t), lit)| functions::Arg { ty: *t, text: &self.src[a.start..a.end], lit: lit.as_ref() }).collect();
                        functions::check(f, name, &self.src[e.start..e.end], super::parser::location(self.src, e.start), &argv)?
                    };
                    // Zone-sensitive functions read the date as the zone's wall clock.
                    if let Some(z) = &self.src.tz {
                        let date_arg = match f {
                            F::DayName | F::MonthName => Some(0),
                            F::DateExtract | F::DateTrunc => Some(1),
                            F::DateFormat => Some(cs.len() - 1),
                            _ => None,
                        };
                        if let Some(i) = date_arg.filter(|i| ts.get(*i) == Some(&DataType::Date)) {
                            let a = std::mem::replace(&mut cs[i], CE::Lit(V::Null));
                            cs[i] = CE::ToLocal(Box::new(a), z.clone());
                        }
                    }
                    if f == F::DateFormat {
                        // The locale travels as a hidden last argument.
                        cs.push(CE::Lit(V::Str(self.src.locale.unwrap_or("en").to_string())));
                        ts.push(DataType::Keyword);
                    }
                    let call = CE::Fn(f, cs, ts, at_of(self.src, e));
                    return Ok((
                        match (&self.src.tz, f) {
                            (Some(z), F::DateTrunc) => CE::FromLocal(Box::new(call), z.clone()),
                            _ => call,
                        },
                        ty,
                    ));
                }
                if name == "case" {
                    let ty = ts.iter().skip(1).step_by(2).copied().chain(if ts.len() % 2 == 1 { ts.last().copied() } else { None }).find(|t| *t != DataType::Null).unwrap_or(DataType::Null);
                    // A union-type conversion reports the field it converts.
                    let ats = args
                        .iter()
                        .step_by(2)
                        .map(|a| match &a.kind {
                            ExprKind::Cast(x, _) if matches!(x.kind, ExprKind::Col(_)) => at_of(self.src, x),
                            _ => at_of(self.src, a),
                        })
                        .collect();
                    return Ok((CE::Case(cs, ats), ty));
                }
                let Some(ty) = value::scalar_type(name, &ts).filter(|_| self.src.sql) else {
                    // The name as written.
                    let written = self.src[e.start..e.end].split('(').next().unwrap_or(name).trim_end();
                    return Err(verification(self.src, e.start, &format!("Unknown function [{written}]")));
                };
                (CE::Call(name.clone(), cs, ts), ty)
            }
        })
    }
}

fn eval(c: &CE, row: &[V], aggs: &[V], w: &mut Warnings) -> EsResult<V> {
    Ok(match c {
        CE::Lit(v) => v.clone(),
        CE::Col(i) => row.get(*i).cloned().unwrap_or(V::Null),
        CE::Agg(i) => aggs.get(*i).cloned().unwrap_or(V::Null),
        CE::Neg(x) => match eval(x, row, aggs, w)? {
            V::Int(i) => i.checked_neg().map(V::Int).unwrap_or(V::Null),
            V::Long(l) => l.checked_neg().map(V::Long).unwrap_or(V::Null),
            V::Double(d) => V::Double(-d),
            _ => V::Null,
        },
        CE::Not(x) => match eval(x, row, aggs, w)?.truthy() {
            Some(b) => V::Bool(!b),
            None => V::Null,
        },
        CE::And(a, b) => {
            let l = eval(a, row, aggs, w)?.truthy();
            if l == Some(false) {
                return Ok(V::Bool(false));
            }
            let r = eval(b, row, aggs, w)?.truthy();
            match (l, r) {
                (_, Some(false)) => V::Bool(false),
                (Some(true), Some(true)) => V::Bool(true),
                _ => V::Null,
            }
        }
        CE::Or(a, b) => {
            let l = eval(a, row, aggs, w)?.truthy();
            if l == Some(true) {
                return Ok(V::Bool(true));
            }
            let r = eval(b, row, aggs, w)?.truthy();
            match (l, r) {
                (_, Some(true)) => V::Bool(true),
                (Some(false), Some(false)) => V::Bool(false),
                _ => V::Null,
            }
        }
        CE::Arith(op, a, b, ty, at) => {
            let (x, y) = (eval(a, row, aggs, w)?, eval(b, row, aggs, w)?);
            match value::arith(*op, &x, &y, *ty) {
                Ok(v) => v,
                Err(msg) => {
                    warn_failure(w, at, &msg);
                    V::Null
                }
            }
        }
        CE::Cmp(op, a, b, at) => {
            let (x, y) = (eval(a, row, aggs, w)?, eval(b, row, aggs, w)?);
            // Elasticsearch's evaluators check the left operand fully (null, then multi-valued)
            // before the right one; only a multi-value is a failure.
            for v in [&x, &y] {
                if v.is_null() {
                    return Ok(V::Null);
                }
                if matches!(v, V::Multi(_)) {
                    warn_failure(w, at, functions::MULTI);
                    return Ok(V::Null);
                }
            }
            let (x, y) = coerce_pair(x, y);
            V::Bool(match op {
                BinOp::Eq => value::eq(&x, &y) == Some(true),
                BinOp::Neq => value::eq(&x, &y) == Some(false),
                BinOp::Lt => value::ordered(&x, &y).is_some_and(Ordering::is_lt),
                BinOp::Lte => value::ordered(&x, &y).is_some_and(Ordering::is_le),
                BinOp::Gt => value::ordered(&x, &y).is_some_and(Ordering::is_gt),
                _ => value::ordered(&x, &y).is_some_and(Ordering::is_ge),
            })
        }
        CE::IsNull(x, neg) => V::Bool(eval(x, row, aggs, w)?.is_null() != *neg),
        CE::Like(x, m, neg, at) => match eval(x, row, aggs, w)? {
            V::Null => V::Null,
            V::Multi(_) => {
                warn_failure(w, at, functions::MULTI);
                V::Null
            }
            V::Str(s) => V::Bool(m.matches(&s) != *neg),
            v => V::Bool(m.matches(&v.to_text()) != *neg),
        },
        CE::In(x, list, neg, at) => {
            let v = eval(x, row, aggs, w)?;
            if v.is_null() {
                return Ok(V::Null);
            }
            let mut found = false;
            let mut saw_null = false;
            // A multi-valued operand on either side fails the row, also when another item matches.
            let mut multi = matches!(v, V::Multi(_));
            for i in list {
                let o = eval(i, row, aggs, w)?;
                if multi || matches!(o, V::Multi(_)) {
                    multi = true;
                    continue;
                }
                let (a, b) = coerce_pair(v.clone(), o);
                match value::eq(&a, &b) {
                    Some(true) => found = true,
                    None => saw_null = true,
                    _ => {}
                }
            }
            if multi {
                warn_failure(w, at, functions::MULTI);
                V::Null
            } else if found {
                V::Bool(!*neg)
            } else if saw_null {
                V::Null
            } else {
                V::Bool(*neg)
            }
        }
        CE::AmdAverage(x) => match eval(x, row, aggs, w)? {
            V::Str(t) => match value::agg_metric_parts(&t) {
                [_, _, Some(sum), Some(n)] if n > 0.0 => V::Double(sum / n),
                _ => V::Null,
            },
            _ => V::Null,
        },
        CE::Case(args, ats) => {
            let mut i = 0;
            while i + 1 < args.len() {
                let cond = eval(&args[i], row, aggs, w)?;
                if matches!(cond, V::Multi(_)) {
                    // A multi-valued condition is false, with Elasticsearch's warning.
                    if let Some(at) = ats.get(i / 2) {
                        w.add(format!("Line {}: evaluation of [{}] failed, treating result as false. Only first 20 failures recorded.", at.loc, at.text));
                        w.add(format!("Line {}: java.lang.IllegalArgumentException: CASE expects a single-valued boolean", at.loc));
                    }
                } else if cond.truthy() == Some(true) {
                    return eval(&args[i + 1], row, aggs, w);
                }
                i += 2;
            }
            if i < args.len() { eval(&args[i], row, aggs, w)? } else { V::Null }
        }
        CE::Cast(x, t) => value::convert(&eval(x, row, aggs, w)?, *t),
        CE::Bucket(x, span) => match (eval(x, row, aggs, w)?, span) {
            (V::Null, _) => V::Null,
            (v, BucketSpan::Num(s)) => match v.as_f64() {
                Some(x) => V::Double((x / s).floor() * s),
                None => V::Null,
            },
            (v, BucketSpan::Date(u)) => match v.as_i64() {
                Some(ms) => value::date_trunc(u, ms).map(V::Date).unwrap_or(V::Null),
                None => V::Null,
            },
        },
        CE::DateShift(d, span, sign) => match (eval(d, row, aggs, w)?, eval(span, row, aggs, w)?) {
            (V::Date(ms), V::Str(sp)) => value::shift_date(ms, &sp, *sign).map(V::Date).unwrap_or(V::Null),
            _ => V::Null,
        },
        CE::Call(name, args, types) => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval(a, row, aggs, w)?);
            }
            match value::scalar(name, &vals, types) {
                Some(r) => r?,
                None => V::Null,
            }
        }
        CE::ToLocal(x, z) => map_dates(eval(x, row, aggs, w)?, &|ms| to_local(z, ms)),
        CE::FromLocal(x, z) => map_dates(eval(x, row, aggs, w)?, &|ms| from_local(z, ms)),
        CE::Fn(f, args, types, at) => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval(a, row, aggs, w)?);
            }
            if *f == F::MvSort
                && let Some(V::Str(o)) = vals.get(1)
                && !o.eq_ignore_ascii_case("asc")
                && !o.eq_ignore_ascii_case("desc")
            {
                return Err(EsError::illegal_argument(format!("Invalid order value in [{}], expected one of [ASC, DESC] but got [{o}]", at.text)));
            }
            match functions::eval(*f, &vals, types) {
                Ok(v) => v,
                Err(Fail::Warn(msg)) => {
                    warn_failure(w, at, &msg);
                    V::Null
                }
                Err(Fail::Partial(v, msgs)) => {
                    warn_failures(w, at, &msgs);
                    v
                }
                Err(Fail::Error(e)) => return Err(e),
            }
        }
    })
}

/// The value of a constant expression (no column, no aggregation), as Elasticsearch folds
/// constants while planning; `None` when `e` reads columns or doesn't compile.
pub fn fold(text: &str, e: &Expr) -> Option<V> {
    if let ExprKind::Lit(v) = &e.kind {
        return Some(v.clone());
    }
    let mut r = std::collections::HashSet::new();
    refs(e, &mut r);
    if !r.is_empty() {
        return None;
    }
    let res = Resources::default();
    let src = Src { text, sql: false, locale: None, res: &res, tz: None };
    let (c, _) = Compiler { src: &src, cols: &[], aggs: None }.compile(e).ok()?;
    eval(&c, &[], &[], &mut Warnings::default()).ok()
}

/// Comparisons between dates or IPs and string literals convert the literal.
fn coerce_pair(x: V, y: V) -> (V, V) {
    match (&x, &y) {
        (V::Date(_), V::Str(s)) => {
            let d = value::parse_date(s).map(V::Date).unwrap_or(V::Null);
            (x, d)
        }
        (V::Str(s), V::Date(_)) => (value::parse_date(s).map(V::Date).unwrap_or(V::Null), y),
        (V::Ip(_), V::Str(s)) => (x, V::Ip(s.clone())),
        (V::Str(s), V::Ip(_)) => (V::Ip(s.clone()), y),
        _ => (x, y),
    }
}

/// A command compiled against its input columns.
enum Stage {
    Where(CE),
    Eval(Vec<(CE, usize)>),
    /// Output columns as indices of the input.
    Project(Vec<usize>),
    MvExpand(usize),
    /// Appends the pattern's keys (`keep`: the input columns that stay).
    Dissect { field: CE, dissect: std::sync::Arc<search_painless::dissect::Dissect>, keys: Vec<String>, keep: Vec<usize> },
    Grok { field: CE, grok: std::sync::Arc<search_painless::grok::Grok>, outputs: Vec<(String, DataType)>, keep: Vec<usize> },
    /// Appends the enrich fields (`fields`: name in the enrich documents) of the matches.
    Enrich { key: usize, table: std::sync::Arc<crate::xpack::enrich::Table>, fields: Vec<(String, DataType)>, keep: Vec<usize> },
    Sample(f64),
    /// Joins each row with the lookup rows whose `keys` equal its `on` columns.
    Lookup { on: Vec<usize>, rows: std::sync::Arc<HashMap<Vec<GroupKey>, Vec<Vec<V>>>>, add: usize, keep: Vec<usize> },
}

/// A hashable form of a value, for group keys and joins.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum GroupKey {
    Null,
    Bool(bool),
    Int(i64),
    /// Doubles by their bits, as Elasticsearch's double `BlockHash` keys them
    /// (`doubleToLongBits`): `-0.0` and `0.0` are two groups.
    Double(u64),
    Str(String),
    Date(i64),
    Multi(Vec<GroupKey>),
}

impl GroupKey {
    pub fn of(v: &V) -> GroupKey {
        match v {
            V::Null => GroupKey::Null,
            V::Bool(b) => GroupKey::Bool(*b),
            V::Int(i) => GroupKey::Int(*i as i64),
            V::Long(l) => GroupKey::Int(*l),
            V::Double(d) => GroupKey::Double(double_key(*d)),
            V::Str(s) | V::Ip(s) => GroupKey::Str(s.clone()),
            V::Date(d) => GroupKey::Date(*d),
            V::Multi(vs) => GroupKey::Multi(vs.iter().map(GroupKey::of).collect()),
        }
    }
}

/// The input columns that stay when `names` are appended (a column of the same name is
/// replaced, the new one moving to the end).
fn keep_except(cols: &[Col], names: &[String]) -> (Vec<usize>, Vec<Col>) {
    let keep: Vec<usize> = (0..cols.len()).filter(|i| !names.contains(&cols[*i].name)).collect();
    let out = keep.iter().map(|i| cols[*i].clone()).collect();
    (keep, out)
}

/// The output keys of a dissect pattern, in pattern order (skipped and `?` keys excluded,
/// appended keys once).
fn dissect_keys(pattern: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut rest = pattern;
    while let Some(i) = rest.find("%{") {
        let after = &rest[i + 2..];
        let Some(j) = after.find('}') else { break };
        let mut k = &after[..j];
        k = k.strip_suffix("->").unwrap_or(k);
        let named_skip = k.starts_with('?');
        let k = k.trim_start_matches(['+', '?', '*', '&']);
        let k = k.split('/').next().unwrap_or(k);
        if !k.is_empty() && !named_skip && !keys.iter().any(|x| x == k) {
            keys.push(k.to_string());
        }
        rest = &after[j + 1..];
    }
    keys
}

fn glob(p: &str, s: &str) -> bool {
    crate::xcontent::filter::glob_match(p, s)
}

/// Compiles a row-wise command; returns the stage and the output columns.
fn compile_stage(src: &Src<'_>, cmd: &Cmd, cols: &[Col]) -> EsResult<(Stage, Vec<Col>)> {
    Ok(match cmd {
        Cmd::Where(e) => {
            let (c, t) = Compiler { src, cols, aggs: None }.compile(e)?;
            if t != DataType::Boolean && t != DataType::Null {
                return Err(verification(src, e.start, &format!("Condition expression needs to be boolean, found [{}]", t.name().to_ascii_uppercase())));
            }
            (Stage::Where(c), cols.to_vec())
        }
        Cmd::Eval(list) => {
            // Values are appended to the row as evaluated (later ones see earlier ones); the
            // output keeps the last column of each name ([`eval_projection`]).
            let mut phys = cols.to_vec();
            let mut evals = Vec::new();
            for n in list {
                let (c, t) = Compiler { src, cols: &phys, aggs: None }.compile(&n.expr)?;
                if t == DataType::Source {
                    // Located at the assignment's name.
                    let at = src[..n.expr.start].rfind(n.name.as_str()).filter(|_| src[n.expr.start..n.expr.end] != *n.name).unwrap_or(n.expr.start);
                    return Err(verification(src, at, &format!("EVAL does not support type [_source] as the return data type of expression [{}]", &src[n.expr.start..n.expr.end])));
                }
                phys.push(Col { name: n.name.clone(), ty: t });
                evals.push((c, phys.len() - 1));
            }
            let mut out = cols.to_vec();
            for (i, n) in list.iter().enumerate() {
                out.retain(|x| x.name != n.name);
                out.push(phys[cols.len() + i].clone());
            }
            (Stage::Eval(evals), out)
        }
        Cmd::Keep(pats) => {
            let mut idx: Vec<usize> = Vec::new();
            for (p, at) in pats {
                let matched: Vec<usize> = (0..cols.len()).filter(|i| glob(p, &cols[*i].name)).collect();
                if matched.is_empty() && !p.contains('*') {
                    return Err(unknown_column(src, *at, p, cols));
                }
                if matched.is_empty() && p != "*" {
                    return Err(verification(src, *at, &format!("No matches found for pattern [{p}]")));
                }
                for m in matched {
                    // A later explicit name moves the column to its position.
                    if !p.contains('*') {
                        idx.retain(|x| *x != m);
                    }
                    if !idx.contains(&m) {
                        idx.push(m);
                    }
                }
            }
            let out = idx.iter().map(|i| cols[*i].clone()).collect();
            (Stage::Project(idx), out)
        }
        Cmd::Drop(pats) => {
            for (p, at) in pats {
                if !p.contains('*') && !cols.iter().any(|c| c.name == *p) {
                    return Err(unknown_column(src, *at, p, cols));
                }
                if p.contains('*') && p != "*" && !cols.iter().any(|c| glob(p, &c.name)) {
                    return Err(verification(src, *at, &format!("No matches found for pattern [{p}]")));
                }
            }
            let idx: Vec<usize> = (0..cols.len()).filter(|i| !pats.iter().any(|(p, _)| glob(p, &cols[*i].name))).collect();
            let out = idx.iter().map(|i| cols[*i].clone()).collect();
            (Stage::Project(idx), out)
        }
        Cmd::Rename(list) => {
            // Each output column remembers the input column it reads.
            let mut out: Vec<(Col, usize)> = cols.iter().cloned().enumerate().map(|(i, c)| (c, i)).collect();
            // Renaming a union-typed or unsupported field fails, every such field reported.
            let problems: Vec<EsError> = list.iter().filter_map(|(old, _, at)| cols.iter().rposition(|c| c.name == *old).and_then(|i| Compiler::unusable(src, cols, i, *at, old))).collect();
            if !problems.is_empty() {
                return Err(merge_problems(problems));
            }
            for (old, new, at) in list {
                let Some(i) = out.iter().position(|(c, _)| c.name == *old) else {
                    return Err(unknown_column(src, *at, old, cols));
                };
                out[i].0.name = new.clone();
                // Renaming onto an existing name drops that column.
                let mut j = 0;
                out.retain(|(c, _)| {
                    let keep = j == i || c.name != *new;
                    j += 1;
                    keep
                });
            }
            let idx = out.iter().map(|(_, i)| *i).collect();
            (Stage::Project(idx), out.into_iter().map(|(c, _)| c).collect())
        }
        Cmd::MvExpand(c, at) => {
            let Some(i) = cols.iter().position(|x| x.name == *c) else { return Err(unknown_column(src, *at, c, cols)) };
            if let Some(err) = Compiler::unusable(src, cols, i, *at, c) {
                return Err(err);
            }
            (Stage::MvExpand(i), cols.to_vec())
        }
        Cmd::Dissect { field, pattern, append_separator, at } => {
            let (f, t) = Compiler { src, cols, aggs: None }.compile(field)?;
            if !t.is_string() && t != DataType::Null {
                return Err(verification(src, field.start, &format!("Dissect only supports KEYWORD or TEXT values, found expression [{}] type [{}]", &src[field.start..field.end], t.name().to_ascii_uppercase())));
            }
            let d = search_painless::dissect::Dissect::new(pattern, append_separator.as_deref()).map_err(|_| super::parser::parse_error(src, *at, &format!("Invalid pattern for dissect: [{pattern}]")))?;
            let keys = dissect_keys(pattern);
            let (keep, mut out) = keep_except(cols, &keys);
            out.extend(keys.iter().map(|k| Col { name: k.clone(), ty: DataType::Keyword }));
            (Stage::Dissect { field: f, dissect: std::sync::Arc::new(d), keys, keep }, out)
        }
        Cmd::Grok { field, pattern, at } => {
            let (f, t) = Compiler { src, cols, aggs: None }.compile(field)?;
            if !t.is_string() && t != DataType::Null {
                return Err(verification(src, field.start, &format!("Grok only supports KEYWORD or TEXT values, found expression [{}] type [{}]", &src[field.start..field.end], t.name().to_ascii_uppercase())));
            }
            let g = search_painless::grok::Grok::new(search_painless::grok::PatternBank::legacy(), pattern)
                .map_err(|e| super::parser::parse_error(src, *at, &format!("Invalid pattern [{pattern}] for grok: {}", e.message)))?;
            use search_painless::grok::CaptureType as CT;
            let mut outputs: Vec<(String, DataType)> = Vec::new();
            for c in g.captures_config() {
                let ty = match c.ty {
                    CT::Int => DataType::Integer,
                    CT::Long => DataType::Long,
                    CT::Float | CT::Double => DataType::Double,
                    CT::Boolean => DataType::Boolean,
                    CT::String => DataType::Keyword,
                };
                if !outputs.iter().any(|(n, _)| *n == c.name) {
                    outputs.push((c.name.clone(), ty));
                }
            }
            // Elasticsearch adds the captures by name.
            outputs.sort_by(|a, b| a.0.cmp(&b.0));
            let names: Vec<String> = outputs.iter().map(|(n, _)| n.clone()).collect();
            let (keep, mut out) = keep_except(cols, &names);
            out.extend(outputs.iter().map(|(n, t)| Col { name: n.clone(), ty: *t }));
            (Stage::Grok { field: f, grok: std::sync::Arc::new(g), outputs, keep }, out)
        }
        Cmd::Enrich { policy, policy_at, on, with, at: cmd_at } => {
            let name = policy.rsplit(':').next().unwrap_or(policy);
            let Some((table, fields)) = src.res.enrich.get(name) else {
                return Err(verification(src, *policy_at, &format!("cannot find enrich policy [{policy}]")));
            };
            let (on_name, on_at) = on.clone().unwrap_or_else(|| (table.match_field.clone(), *policy_at));
            let Some(key) = cols.iter().rposition(|c| c.name == on_name) else { return Err(unknown_column(src, on_at, &on_name, cols)) };
            // The match column's type must suit the policy (`Analyzer.resolveEnrich`); union and
            // unsupported columns fail as they do anywhere else.
            if let Some(err) = Compiler::unusable(src, cols, key, on_at, &on_name) {
                return Err(err);
            }
            let key_ty = cols[key].ty;
            let (allowed, listed): (&[DataType], &str) = if table.kind == "geo_match" {
                (&[DataType::GeoPoint, DataType::GeoShape], "geo_point, geo_shape")
            } else {
                (&[DataType::Keyword, DataType::Text, DataType::Ip, DataType::Long, DataType::Integer, DataType::Double, DataType::Date], "keyword, text, ip, long, integer, float, double, datetime")
            };
            if key_ty != DataType::Null && !allowed.contains(&key_ty) {
                return Err(verification(
                    src,
                    on_at,
                    &format!("Unsupported type [{}] for enrich matching field [{on_name}]; only [{listed}] allowed for type [{}]", functions::type_name_static(key_ty), table.kind),
                ));
            }
            let mut adds: Vec<(String, String, DataType)> = Vec::new();
            match with {
                Some(list) => {
                    for (out_name, field, at) in list {
                        let Some(c) = fields.iter().find(|c| c.name == *field) else {
                            let _ = at;
                            return Err(verification(src, *cmd_at, &format!("Enrich field [{field}] not found in enrich policy [{policy}]")));
                        };
                        adds.push((out_name.clone(), field.clone(), c.ty));
                    }
                }
                None => adds.extend(fields.iter().map(|c| (c.name.clone(), c.name.clone(), c.ty))),
            }
            let names: Vec<String> = adds.iter().map(|a| a.0.clone()).collect();
            let (keep, mut out) = keep_except(cols, &names);
            out.extend(adds.iter().map(|(n, _, t)| Col { name: n.clone(), ty: *t }));
            (Stage::Enrich { key, table: table.clone(), fields: adds.into_iter().map(|a| (a.1, a.2)).collect(), keep }, out)
        }
        Cmd::Sample(p) => (Stage::Sample(*p), cols.to_vec()),
        Cmd::LookupJoin { index, index_at, on } => {
            let Some(lt) = src.res.lookup.get(index) else { return Err(verification(src, *index_at, &format!("Unknown index [{index}]"))) };
            let mut on_idx = Vec::new();
            let mut lk_idx = Vec::new();
            for (f, at) in on {
                let Some(i) = cols.iter().rposition(|c| c.name == *f) else { return Err(verification(src, *at, &format!("Unknown column [{f}] in left side of join"))) };
                let Some(j) = lt.cols.iter().position(|c| c.name == *f) else { return Err(verification(src, *at, &format!("Unknown column [{f}] in right side of join"))) };
                for (side, ty) in [("right", lt.cols[j].ty), ("left", cols[i].ty)] {
                    if ty == DataType::Flattened {
                        return Err(verification(src, *at, &format!("JOIN with {side} field [{f}] of type [FLATTENED] is not supported")));
                    }
                }
                on_idx.push(i);
                lk_idx.push(j);
            }
            // The lookup's columns replace same-named input columns, except the join keys.
            let add_cols: Vec<usize> = (0..lt.cols.len()).filter(|j| !lk_idx.contains(j)).collect();
            let names: Vec<String> = add_cols.iter().map(|j| lt.cols[*j].name.clone()).collect();
            let (keep, mut out) = keep_except(cols, &names);
            out.extend(add_cols.iter().map(|j| lt.cols[*j].clone()));
            let mut by_key: HashMap<Vec<GroupKey>, Vec<Vec<V>>> = HashMap::new();
            for r in &lt.rows {
                let k: Vec<GroupKey> = lk_idx.iter().map(|j| GroupKey::of(&r[*j])).collect();
                if k.iter().any(|x| matches!(x, GroupKey::Null | GroupKey::Multi(_))) {
                    continue;
                }
                by_key.entry(k).or_default().push(add_cols.iter().map(|j| r[*j].clone()).collect());
            }
            (Stage::Lookup { on: on_idx, rows: std::sync::Arc::new(by_key), add: add_cols.len(), keep }, out)
        }
        _ => unreachable!("not a row-wise command"),
    })
}

fn run_stage(stage: &Stage, in_cols: usize, rows: Vec<Vec<V>>, w: &mut Warnings) -> EsResult<Vec<Vec<V>>> {
    let mut out = Vec::with_capacity(rows.len());
    match stage {
        Stage::Where(c) => {
            for r in rows {
                if eval(c, &r, &[], w)?.truthy() == Some(true) {
                    out.push(r);
                }
            }
        }
        Stage::Eval(evals) => {
            for mut r in rows {
                let _ = in_cols;
                for (c, _) in evals {
                    let v = eval(c, &r, &[], w)?;
                    r.push(v);
                }
                out.push(r);
            }
        }
        Stage::Project(idx) => {
            for r in rows {
                out.push(idx.iter().map(|i| r.get(*i).cloned().unwrap_or(V::Null)).collect());
            }
        }
        Stage::Dissect { field, dissect, keys, keep } => {
            for r in rows {
                let v = eval(field, &r, &[], w)?;
                let parsed = match &v {
                    V::Str(s) => dissect.parse(Some(s)),
                    _ => None,
                };
                let mut n: Vec<V> = keep.iter().map(|i| r[*i].clone()).collect();
                for k in keys {
                    n.push(parsed.as_ref().and_then(|m| m.get(k)).map(|s| V::Str(s.clone())).unwrap_or(V::Null));
                }
                out.push(n);
            }
        }
        Stage::Grok { field, grok, outputs, keep } => {
            use search_painless::grok::GrokValue as G;
            fn conv(g: &G) -> V {
                match g {
                    G::Str(s) => V::Str(s.clone()),
                    G::Int(i) => V::Int(*i),
                    G::Long(l) => V::Long(*l),
                    G::Float(f) => V::Double(*f as f64),
                    G::Double(d) => V::Double(*d),
                    G::Bool(b) => V::Bool(*b),
                    G::List(l) => V::Multi(l.iter().map(conv).collect()),
                }
            }
            for r in rows {
                let v = eval(field, &r, &[], w)?;
                let caps = match &v {
                    V::Str(s) => grok.captures(s).ok().flatten(),
                    _ => None,
                };
                let mut n: Vec<V> = keep.iter().map(|i| r[*i].clone()).collect();
                for (name, _) in outputs {
                    n.push(caps.as_ref().and_then(|c| c.iter().find(|(k, _)| k == name)).map(|(_, g)| conv(g)).unwrap_or(V::Null));
                }
                out.push(n);
            }
        }
        Stage::Enrich { key, table, fields, keep } => {
            for r in rows {
                let k = r.get(*key).cloned().unwrap_or(V::Null);
                let hits = if k.is_null() { Vec::new() } else { table.lookup(&k.to_json(DataType::Keyword), usize::MAX) };
                let mut n: Vec<V> = keep.iter().map(|i| r[*i].clone()).collect();
                for (f, ty) in fields {
                    let mut vals: Vec<V> = Vec::new();
                    for h in &hits {
                        if let Some(x) = h.get(f) {
                            match value::from_field(x, *ty) {
                                V::Multi(m) => vals.extend(m),
                                V::Null => {}
                                one => vals.push(one),
                            }
                        }
                    }
                    n.push(match vals.len() {
                        0 => V::Null,
                        1 => vals.pop().expect("one"),
                        _ => V::Multi(vals),
                    });
                }
                out.push(n);
            }
        }
        Stage::Sample(p) => {
            use rand::Rng;
            let mut rng = rand::thread_rng();
            for r in rows {
                if rng.r#gen::<f64>() < *p {
                    out.push(r);
                }
            }
        }
        Stage::Lookup { on, rows: lk, add, keep } => {
            for r in rows {
                let k: Vec<GroupKey> = on.iter().map(|i| GroupKey::of(&r[*i])).collect();
                let base: Vec<V> = keep.iter().map(|i| r[*i].clone()).collect();
                match lk.get(&k) {
                    Some(matches) => {
                        for m in matches {
                            let mut n = base.clone();
                            n.extend(m.iter().cloned());
                            out.push(n);
                        }
                    }
                    None => {
                        let mut n = base;
                        n.extend(std::iter::repeat_n(V::Null, *add));
                        out.push(n);
                    }
                }
            }
        }
        Stage::MvExpand(i) => {
            for r in rows {
                match r.get(*i) {
                    Some(V::Multi(vs)) => {
                        for v in vs {
                            let mut n = r.clone();
                            n[*i] = v.clone();
                            out.push(n);
                        }
                    }
                    _ => out.push(r),
                }
            }
        }
    }
    Ok(out)
}

/// The physical layout an `EVAL` produces: the input row extended by every evaluated value,
/// then projected to the output columns (names that were shadowed are dropped).
fn eval_projection(cols_in: &[Col], list: &[super::parser::Named], out: &[Col]) -> Vec<usize> {
    let mut names: Vec<String> = cols_in.iter().map(|c| c.name.clone()).collect();
    for n in list {
        names.push(n.name.clone());
    }
    out.iter().map(|c| names.iter().rposition(|n| *n == c.name).unwrap_or(0)).collect()
}

/// A pipeline of row-wise stages.
struct Pipeline {
    stages: Vec<(Stage, usize, Option<Vec<usize>>)>,
}

impl Pipeline {
    fn run(&self, mut rows: Vec<Vec<V>>, w: &mut Warnings) -> EsResult<Vec<Vec<V>>> {
        for (s, n, proj) in &self.stages {
            rows = run_stage(s, *n, rows, w)?;
            if let Some(p) = proj {
                // Each output column reads a distinct physical column, so values move.
                for r in &mut rows {
                    let mut n: Vec<V> = Vec::with_capacity(p.len());
                    for i in p {
                        n.push(r.get_mut(*i).map(std::mem::take).unwrap_or(V::Null));
                    }
                    *r = n;
                }
            }
        }
        Ok(rows)
    }
}

fn build_pipeline(src: &Src<'_>, cmds: &[Cmd], mut cols: Vec<Col>) -> EsResult<(Pipeline, Vec<Col>)> {
    let mut stages = Vec::new();
    for cmd in cmds {
        let (stage, out) = compile_stage(src, cmd, &cols)?;
        let proj = match cmd {
            // Only when a new column replaces one of the same name does the row get rebuilt.
            Cmd::Eval(list) => Some(eval_projection(&cols, list, &out)).filter(|p| p.len() != cols.len() + list.len() || p.iter().enumerate().any(|(i, x)| i != *x)),
            _ => None,
        };
        stages.push((stage, cols.len(), proj));
        cols = out;
    }
    Ok((Pipeline { stages }, cols))
}

fn is_row_wise(c: &Cmd) -> bool {
    matches!(
        c,
        Cmd::Where(_) | Cmd::Eval(_) | Cmd::Keep(_) | Cmd::Drop(_) | Cmd::Rename(_) | Cmd::MvExpand(..) | Cmd::Dissect { .. } | Cmd::Grok { .. } | Cmd::Enrich { .. } | Cmd::Sample(_) | Cmd::LookupJoin { .. }
    )
}

/// `STATS`: aggregations per group, built incrementally.
///
/// Groups live in creation order (the output order); a hash table maps each key's hash to
/// its first group, colliding groups chained through `next`. Keys are hashed and compared
/// straight from the row's values (as [`GroupKey`] would compare them), so a row of an
/// existing group allocates nothing.
struct Grouper {
    keys: Vec<(CE, DataType, String)>,
    outputs: Vec<(CE, DataType, String)>,
    specs: Vec<AggSpec>,
    index: HashMap<u64, u32, std::hash::BuildHasherDefault<IdHasher>>,
    groups: Vec<Group>,
}

struct Group {
    key: Vec<V>,
    accs: Vec<Acc>,
    /// The next group with the same hash (`u32::MAX`: none).
    next: u32,
}

/// The hasher of keys that are already hashes.
#[derive(Default)]
struct IdHasher(u64);

impl std::hash::Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0.rotate_left(8)) ^ u64::from(*b);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

/// A double's group key bits: `doubleToLongBits`, which keeps `-0.0` apart from `0.0` and
/// collapses every NaN to the canonical one.
#[inline]
fn double_key(d: f64) -> u64 {
    if d.is_nan() { f64::NAN.to_bits() } else { d.to_bits() }
}

/// Feeds `v` to `h` as its [`GroupKey`] would hash (ints and longs alike, doubles by their
/// bits, keywords and ips alike).
fn hash_key(v: &V, h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    match v {
        V::Null => 0u8.hash(h),
        V::Bool(b) => (1u8, *b).hash(h),
        V::Int(i) => (2u8, *i as i64).hash(h),
        V::Long(l) => (2u8, *l).hash(h),
        V::Double(d) => (3u8, double_key(*d)).hash(h),
        V::Str(s) | V::Ip(s) => (4u8, s.as_str()).hash(h),
        V::Date(d) => (5u8, *d).hash(h),
        V::Multi(vs) => {
            (6u8, vs.len()).hash(h);
            vs.iter().for_each(|x| hash_key(x, h));
        }
    }
}

/// Whether `a` and `b` are the same group key ([`GroupKey`] equality).
fn same_key(a: &V, b: &V) -> bool {
    match (a, b) {
        (V::Null, V::Null) => true,
        (V::Bool(x), V::Bool(y)) => x == y,
        (V::Int(_) | V::Long(_), V::Int(_) | V::Long(_)) => a.as_i64() == b.as_i64(),
        (V::Double(x), V::Double(y)) => double_key(*x) == double_key(*y),
        (V::Str(x) | V::Ip(x), V::Str(y) | V::Ip(y)) => x == y,
        (V::Date(x), V::Date(y)) => x == y,
        (V::Multi(x), V::Multi(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same_key(p, q)),
        _ => false,
    }
}

/// Whether `x` repeats `p` within one multi-value (`MultivalueDedupe`): doubles compare with
/// the primitive `==`, so `[-0.0, 0.0]` is one key (the first), while separate rows holding
/// `-0.0` and `0.0` are two groups.
fn mv_duplicate(p: &V, x: &V) -> bool {
    match (p, x) {
        (V::Double(a), V::Double(b)) => a == b,
        _ => same_key(p, x),
    }
}

impl Grouper {
    fn new(src: &Src<'_>, aggs: &[super::parser::Named], by: &[super::parser::Named], cols: &[Col]) -> EsResult<Grouper> {
        let mut keys = Vec::new();
        for n in by {
            let (c, t) = match &n.expr.kind {
                ExprKind::Call(f, args) if f == "bucket" || f == "bin" => compile_bucket(src, cols, &n.expr, args)?,
                // `TBUCKET(span)`: `BUCKET(@timestamp, span)`.
                ExprKind::Call(f, args) if f == "tbucket" => {
                    let mut full = vec![Expr { kind: ExprKind::Col("@timestamp".into()), start: n.expr.start, end: n.expr.start }];
                    full.extend(args.iter().cloned());
                    compile_bucket(src, cols, &n.expr, &full)?
                }
                _ => Compiler { src, cols, aggs: None }.compile(&n.expr)?,
            };
            if t.unsortable() && !matches!(t, DataType::Tsid | DataType::Flattened) {
                return Err(verification(src, n.expr.start, &format!("cannot group by on [{}] type for grouping [{}]", t.name(), &src[n.expr.start..n.expr.end])));
            }
            if let Some(m) = bucket_meta(&c) {
                let mut cm = src.res.col_meta.lock();
                cm.retain(|(k, _)| *k != n.name);
                cm.push((n.name.clone(), m));
            }
            keys.push((c, t, n.name.clone()));
        }
        let mut specs = Vec::new();
        let mut outputs = Vec::new();
        for n in aggs {
            let before = specs.len();
            let (c, t) = Compiler { src, cols, aggs: Some(&mut specs) }.compile(&n.expr)?;
            if specs.len() == before {
                // The first column the expression reads that isn't a group.
                fn first_col(e: &Expr) -> Option<(&str, usize)> {
                    match &e.kind {
                        ExprKind::Col(c) => Some((c, e.start)),
                        ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::IsNull(x, _) | ExprKind::Like(x, ..) | ExprKind::Cast(x, _) => first_col(x),
                        ExprKind::Bin(_, a, b) => first_col(a).or_else(|| first_col(b)),
                        ExprKind::In(x, l, _) => first_col(x).or_else(|| l.iter().find_map(first_col)),
                        ExprKind::Call(_, args) => args.iter().find_map(first_col),
                        _ => None,
                    }
                }
                let (what, at) = first_col(&n.expr).filter(|(c, _)| !by.iter().any(|b| b.name == *c)).unwrap_or((&src[n.expr.start..n.expr.end], n.expr.start));
                return Err(verification(src, at, &format!("column [{what}] must appear in the STATS BY clause or be used in an aggregate function")));
            }
            outputs.push((c, t, n.name.clone()));
        }
        Ok(Grouper { keys, outputs, specs, index: HashMap::default(), groups: Vec::new() })
    }

    fn add(&mut self, rows: &[Vec<V>], w: &mut Warnings) -> EsResult<()> {
        // Column keys are read in place; computed ones are evaluated into `owned`.
        let nk = self.keys.len();
        let mut owned: Vec<V> = vec![V::Null; nk];
        let cols: Vec<Option<usize>> = self.keys.iter().map(|(c, _, _)| if let CE::Col(i) = c { Some(*i) } else { None }).collect();
        for r in rows {
            for (i, (c, _, _)) in self.keys.iter().enumerate() {
                if cols[i].is_none() {
                    owned[i] = eval(c, r, &[], w)?;
                }
            }
            let key_of = |i: usize| -> &V {
                match cols[i] {
                    Some(c) => r.get(c).unwrap_or(&V::Null),
                    None => &owned[i],
                }
            };
            if nk == 1
                && let V::Multi(vs) = key_of(0)
            {
                // A multi-valued key puts the row once in every value's group.
                for (j, v) in vs.iter().enumerate() {
                    if vs[..j].iter().any(|p| mv_duplicate(p, v)) {
                        continue;
                    }
                    let g = self.group_of(&[v]);
                    self.add_row(g, r, w)?;
                }
            } else if (0..nk).any(|i| matches!(key_of(i), V::Multi(_))) {
                let kv: Vec<V> = (0..nk).map(|i| key_of(i).clone()).collect();
                for key in expand_keys(&kv) {
                    let refs: Vec<&V> = key.iter().collect();
                    let g = self.group_of(&refs);
                    self.add_row(g, r, w)?;
                }
            } else {
                let g = match nk {
                    0 => self.group_of(&[]),
                    1 => self.group_of(&[key_of(0)]),
                    _ => {
                        let refs: Vec<&V> = (0..nk).map(key_of).collect();
                        self.group_of(&refs)
                    }
                };
                self.add_row(g, r, w)?;
            }
        }
        Ok(())
    }

    /// The group of `key`, created (in output order) when new.
    fn group_of(&mut self, key: &[&V]) -> usize {
        use std::hash::Hasher;
        let mut h = std::hash::DefaultHasher::new();
        for v in key {
            hash_key(v, &mut h);
        }
        let hash = h.finish();
        let fresh = self.groups.len() as u32;
        let head = *self.index.entry(hash).or_insert(fresh);
        if head != fresh {
            let mut at = head;
            loop {
                let g = &self.groups[at as usize];
                if g.key.len() == key.len() && g.key.iter().zip(key).all(|(a, b)| same_key(a, b)) {
                    return at as usize;
                }
                if g.next == u32::MAX {
                    break;
                }
                at = g.next;
            }
            self.groups[at as usize].next = fresh;
        }
        self.groups.push(Group { key: key.iter().map(|v| (*v).clone()).collect(), accs: self.specs.iter().map(|s| Acc::for_spec(&s.func, s.arg_ty, &s.extra)).collect(), next: u32::MAX });
        fresh as usize
    }

    /// Adds `r` to group `g`.
    fn add_row(&mut self, g: usize, r: &[V], w: &mut Warnings) -> EsResult<()> {
        let accs = &mut self.groups[g].accs;
        for (s, a) in self.specs.iter().zip(accs.iter_mut()) {
            if let Some(f) = &s.filter
                && eval(f, r, &[], w)?.truthy() != Some(true)
            {
                continue;
            }
            match &s.arg {
                None => a.add(&s.func, &V::Null, true),
                Some(c) if s.weight.is_some() => {
                    let v = eval(c, r, &[], w)?;
                    let weight = eval(s.weight.as_ref().expect("weight"), r, &[], w)?;
                    a.add_weighted(&v, &weight);
                }
                Some(CE::Col(i)) => a.add(&s.func, r.get(*i).unwrap_or(&V::Null), false),
                Some(c) => {
                    let v = eval(c, r, &[], w)?;
                    a.add(&s.func, &v, false);
                }
            }
        }
        Ok(())
    }

    fn finish(mut self, w: &mut Warnings) -> EsResult<Table> {
        let mut cols: Vec<Col> = self.outputs.iter().map(|(_, t, n)| Col { name: n.clone(), ty: *t }).collect();
        for (_, t, n) in &self.keys {
            cols.retain(|c| c.name != *n);
            cols.push(Col { name: n.clone(), ty: *t });
        }
        if self.keys.is_empty() && self.groups.is_empty() {
            self.group_of(&[]);
        }
        let mut rows = Vec::with_capacity(self.groups.len());
        for Group { key, accs, .. } in &self.groups {
            let results: Vec<V> = self.specs.iter().zip(accs).map(|(s, a)| a.finish(&s.func, value::agg_type(&s.func, s.arg_ty), &s.extra)).collect();
            let mut row = Vec::new();
            for (c, _, n) in &self.outputs {
                if self.keys.iter().any(|(_, _, kn)| kn == n) {
                    continue;
                }
                row.push(eval(c, &[], &results, w)?);
            }
            row.extend(key.iter().cloned());
            rows.push(row);
        }
        Ok(Table { cols, rows })
    }
}

fn expand_keys(kv: &[V]) -> Vec<Vec<V>> {
    let mut out: Vec<Vec<V>> = vec![Vec::new()];
    for v in kv {
        let vals: Vec<V> = match v {
            // Each distinct value once (`MultivalueDedupe`).
            V::Multi(vs) => {
                let mut d: Vec<V> = Vec::with_capacity(vs.len());
                for x in vs {
                    if !d.iter().any(|p| mv_duplicate(p, x)) {
                        d.push(x.clone());
                    }
                }
                d
            }
            other => vec![other.clone()],
        };
        let mut next = Vec::with_capacity(out.len() * vals.len());
        for prefix in &out {
            for x in &vals {
                let mut p = prefix.clone();
                p.push(x.clone());
                next.push(p);
            }
        }
        out = next;
    }
    out
}

/// `SORT` keys compiled against a table's columns: (expression, ascending, nulls first, type).
struct SortSpec(Vec<(CE, bool, bool, DataType)>);

impl SortSpec {
    fn new(src: &Src<'_>, keys: &[super::parser::SortKey], cols: &[Col]) -> EsResult<SortSpec> {
        let mut compiled = Vec::with_capacity(keys.len());
        for k in keys {
            let (c, ty) = Compiler { src, cols, aggs: None }.compile(&k.expr)?;
            if ty.unsortable() || super::spatial::is_spatial(ty) {
                return Err(verification(src, k.expr.start, &format!("cannot sort on {}", ty.name())));
            }
            compiled.push((c, k.asc, k.nulls_first, ty));
        }
        Ok(SortSpec(compiled))
    }

    /// A row's sort key.
    fn key(&self, r: &[V], w: &mut Warnings) -> EsResult<Vec<V>> {
        let mut kv = Vec::with_capacity(self.0.len());
        for (c, asc, _, ty) in &self.0 {
            let v = eval(c, r, &[], w)?;
            // Multi-values sort by their minimum ascending, maximum descending.
            let v = match v {
                V::Multi(mut vs) => {
                    vs.sort_by(|a, b| functions::cmp_typed(a, b, *ty));
                    if *asc { vs.into_iter().next().unwrap_or(V::Null) } else { vs.pop().unwrap_or(V::Null) }
                }
                other => other,
            };
            kv.push(v);
        }
        Ok(kv)
    }

    fn cmp(&self, a: &[V], b: &[V]) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        for (i, (_, asc, nulls_first, ty)) in self.0.iter().enumerate() {
            let (x, y) = (&a[i], &b[i]);
            let o = match (x.is_null(), y.is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => {
                    if *nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (false, true) => {
                    if *nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                _ => {
                    let o = functions::cmp_typed(x, y, *ty);
                    if *asc { o } else { o.reverse() }
                }
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }
}

fn sort_rows(src: &Src<'_>, keys: &[super::parser::SortKey], t: &mut Table, w: &mut Warnings) -> EsResult<()> {
    let spec = SortSpec::new(src, keys, &t.cols)?;
    let mut keyed: Vec<(Vec<V>, Vec<V>)> = Vec::with_capacity(t.rows.len());
    for r in std::mem::take(&mut t.rows) {
        keyed.push((spec.key(&r, w)?, r));
    }
    keyed.sort_by(|a, b| spec.cmp(&a.0, &b.0));
    t.rows = keyed.into_iter().map(|(_, r)| r).collect();
    Ok(())
}

/// `SORT ... | LIMIT n` over a stream of batches: keeps the best `n` rows seen so far (ties in
/// arrival order, as the stable full sort would), selecting in linear time whenever the buffer
/// doubles, so memory stays O(n) however many rows arrive. Once `n` rows are kept, a row whose
/// key isn't strictly better than the worst kept one is dropped on arrival.
struct TopN {
    spec: SortSpec,
    n: usize,
    /// (key, arrival, row).
    buf: Vec<(Vec<V>, u64, Vec<V>)>,
    seq: u64,
    /// The worst kept key after the last trim.
    bound: Option<Vec<V>>,
}

impl TopN {
    fn new(spec: SortSpec, n: usize) -> TopN {
        TopN { spec, n, buf: Vec::new(), seq: 0, bound: None }
    }

    fn add(&mut self, rows: Vec<Vec<V>>, w: &mut Warnings) -> EsResult<()> {
        if self.n == 0 {
            return Ok(());
        }
        for r in rows {
            let k = self.spec.key(&r, w)?;
            self.seq += 1;
            if self.bound.as_ref().is_some_and(|b| self.spec.cmp(&k, b) != std::cmp::Ordering::Less) {
                continue;
            }
            self.buf.push((k, self.seq, r));
        }
        if self.buf.len() >= self.n.saturating_mul(2).max(1024) {
            self.trim();
            let spec = &self.spec;
            self.bound = self.buf.iter().max_by(|a, b| spec.cmp(&a.0, &b.0).then(a.1.cmp(&b.1))).map(|e| e.0.clone());
        }
        Ok(())
    }

    fn trim(&mut self) {
        if self.buf.len() > self.n {
            let spec = &self.spec;
            self.buf.select_nth_unstable_by(self.n - 1, |a, b| spec.cmp(&a.0, &b.0).then(a.1.cmp(&b.1)));
            self.buf.truncate(self.n);
        }
    }

    fn finish(mut self) -> Vec<Vec<V>> {
        if self.n == 0 {
            return Vec::new();
        }
        self.trim();
        let spec = &self.spec;
        self.buf.sort_unstable_by(|a, b| spec.cmp(&a.0, &b.0).then(a.1.cmp(&b.1)));
        self.buf.into_iter().map(|(_, _, r)| r).collect()
    }
}

/// `SORT keys | (EVAL|KEEP|DROP|RENAME)* | LIMIT n` at the start of `cmds`: the keys, `n` and
/// the position of the `LIMIT`. Those commands keep one row per row, so the limit applies
/// first (Elasticsearch's optimizer pushes it down the same way).
fn sort_then_limit(cmds: &[Cmd]) -> Option<(&[super::parser::SortKey], usize, usize)> {
    let Some(Cmd::Sort(keys)) = cmds.first() else { return None };
    let j = 1 + cmds[1..].iter().take_while(|c| matches!(c, Cmd::Eval(_) | Cmd::Keep(_) | Cmd::Drop(_) | Cmd::Rename(_))).count();
    match cmds.get(j) {
        Some(Cmd::Limit(n)) => Some((keys, *n, j)),
        _ => None,
    }
}

/// For a prefix `WHERE* (EVAL|KEEP|DROP|RENAME)*` before `SORT keys`: the number of leading
/// `WHERE`s and the keys over the source's columns, when every key is a source column the
/// prefix passes through (renamed or not, never redefined).
fn top_source_keys(prefix: &[Cmd], keys: &[super::parser::SortKey], cols: &[Col]) -> Option<(usize, Vec<super::parser::SortKey>)> {
    let nw = prefix.iter().take_while(|c| matches!(c, Cmd::Where(_))).count();
    let after = &prefix[nw..];
    if !after.iter().all(|c| matches!(c, Cmd::Eval(_) | Cmd::Keep(_) | Cmd::Drop(_) | Cmd::Rename(_))) {
        return None;
    }
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        let ExprKind::Col(name) = &k.expr.kind else { return None };
        let mut name = name.clone();
        for c in after.iter().rev() {
            match c {
                Cmd::Eval(list) if list.iter().any(|n| n.name == name) => return None,
                Cmd::Rename(list) => {
                    for (old, new, _) in list.iter().rev() {
                        if *new == name {
                            name = old.clone();
                        }
                    }
                }
                _ => {}
            }
        }
        let col = cols.iter().find(|c| c.name == name)?;
        if matches!(col.ty, DataType::Source | DataType::Unsupported | DataType::Null) {
            return None;
        }
        out.push(super::parser::SortKey { expr: Expr { kind: ExprKind::Col(name), start: k.expr.start, end: k.expr.end }, asc: k.asc, nulls_first: k.nulls_first });
    }
    Some((nw, out))
}

/// `FROM | WHERE* | ... | SORT | LIMIT n` with the sort on source columns, selected while
/// reading doc values ([`super::columnar::scan_top`]): only the `n` best docs are read whole,
/// and the commands after the `WHERE`s run on just those rows. `None`: the columnar source
/// doesn't apply (the caller scans every row).
#[allow(clippy::too_many_arguments)]
fn top_from_source(cluster: &Cluster, src: &Src<'_>, s: &Source, prefix: &[Cmd], keys: &[super::parser::SortKey], n: usize, query: Option<&Value>, stats: &mut Stats, w: &mut Warnings) -> EsResult<Option<Vec<Vec<V>>>> {
    if n == 0 || !s.load.is_empty() || !s.ft.is_empty() || (s.fields.is_empty() && s.metadata.is_empty()) {
        return Ok(None);
    }
    let Some((nw, src_keys)) = top_source_keys(prefix, keys, &s.cols) else { return Ok(None) };
    let now = crate::time::now_millis();
    let Some(plan) = super::columnar::plan(cluster, &s.index, &s.cols, &s.fields, &s.metadata, s.needed.as_ref(), 0, now) else { return Ok(None) };
    let spec = SortSpec::new(src, &src_keys, &s.cols)?;
    let mut names = std::collections::HashSet::new();
    let mut wheres = Vec::with_capacity(nw);
    for c in &prefix[..nw] {
        if let Cmd::Where(e) = c {
            refs(e, &mut names);
        }
        if let (Stage::Where(ce), _) = compile_stage(src, c, &s.cols)? {
            wheres.push(ce);
        }
    }
    for k in &src_keys {
        refs(&k.expr, &mut names);
    }
    let read: Vec<usize> = (0..s.cols.len()).filter(|&i| names.contains(&s.cols[i].name)).collect();
    let rows = {
        let mut key = |r: &[V]| -> EsResult<Option<Vec<V>>> {
            for c in &wheres {
                if eval(c, r, &[], w)?.truthy() != Some(true) {
                    return Ok(None);
                }
            }
            spec.key(r, w).map(Some)
        };
        let cmp = |a: &[V], b: &[V]| spec.cmp(a, b);
        super::columnar::scan_top(cluster, plan, query, now, stats, super::columnar::TopSel { n, read, key: &mut key, cmp: &cmp })?
    };
    let Some(rows) = rows else { return Ok(None) };
    let (p, _) = build_pipeline(src, &prefix[nw..], s.cols.clone())?;
    Ok(Some(p.run(rows, w)?))
}

/// The scanned source of a `FROM`.
struct Source {
    index: String,
    cols: Vec<Col>,
    /// Columns read from `fields`, by position (metadata columns are read from the hit).
    fields: Vec<String>,
    metadata: Vec<String>,
    /// The fields the query reads (`None`: all).
    needed: Option<std::collections::HashSet<String>>,
    /// Full-text queries, each read into its hidden column (`fulltext`).
    ft: Vec<Value>,
    /// Unmapped fields read from `_source` as keywords (`SET unmapped_fields="load"`).
    load: Vec<String>,
    /// Fields without doc values (not aggregatable): read from `_source`, so their values keep
    /// the source's order.
    source_order: std::collections::HashSet<String>,
    /// `float` / `half_float` fields: their values widen from single precision, as their doc
    /// values do (`1.1` reads as `1.100000023841858`).
    floats: std::collections::HashSet<String>,
    /// `constant_keyword` fields (in some target index): roll's queries don't read them, so a
    /// `WHERE` on one is never pushed down.
    constants: std::collections::HashSet<String>,
    /// The concrete indices the source resolved to.
    indices: Vec<String>,
    /// The mapping types of each `unsupported` column, sorted.
    original_types: Vec<(String, Vec<String>)>,
    /// Union types: per field, its types and the indices mapping each.
    unions: HashMap<String, Vec<(String, Vec<String>)>>,
    /// Fields of a type Elasticsearch doesn't support, with that type.
    unsupported: HashMap<String, String>,
    /// Fields that are `flattened` in some target index.
    flattened: Vec<String>,
    /// The same, with the indices mapping each as `flattened` (`None`: every index). Below such
    /// a field, in those indices, there is no column: its keys read as null.
    flattened_in: Vec<(String, Option<Vec<String>>)>,
}

async fn resolve_source(cluster: &Cluster, src: &str, sources: &[String], metadata: &[String], at: usize) -> EsResult<Source> {
    let index = sources.join(",");
    let caps = internal::call(cluster, "GET", &format!("/{}/_field_caps?fields=*&allow_no_indices=false&expand_wildcards=open", encode(&index)), None)
        .await
        .unwrap_or_else(|_| Value::Null);
    // Missing concrete names fail the whole request; see which other names resolve.
    let caps = if caps.is_null() {
        internal::call(cluster, "GET", &format!("/{}/_field_caps?fields=*&ignore_unavailable=true&expand_wildcards=open", encode(&index)), None).await.unwrap_or(Value::Null)
    } else {
        caps
    };
    if caps.get("indices").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
        // A concrete name that doesn't exist next to names that do is a missing index.
        let st = cluster.state();
        for s in sources {
            if s.contains('*') || s.contains(':') || s.starts_with('-') {
                continue;
            }
            let exists = st.metadata.indices.contains_key(s.as_str()) || st.metadata.indices.values().any(|i| i.aliases.contains_key(s.as_str())) || st.metadata.data_streams.contains_key(s.as_str());
            if !exists {
                return Err(EsError::index_not_found(s));
            }
        }
    }
    if caps.get("indices").and_then(Value::as_array).is_none_or(|a| a.is_empty()) {
        // Patterns that match nothing are an empty result with no columns.
        if sources.iter().all(|s| s.contains('*')) {
            return Ok(Source { index, cols: vec![Col { name: "<no-fields>".into(), ty: DataType::Null }], fields: Vec::new(), metadata: Vec::new(), needed: None, ft: Vec::new(), load: Vec::new(), source_order: Default::default(), floats: Default::default(), constants: Default::default(), indices: Vec::new(), original_types: Vec::new(), unions: HashMap::new(), unsupported: HashMap::new(), flattened: Vec::new(), flattened_in: Vec::new() });
        }
        return Err(verification_plain(&format!("Unknown index [{index}]")));
    }
    let _ = (src, at);
    let indices: Vec<String> = caps.get("indices").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
    let mut cols = Vec::new();
    let mut source_order = std::collections::HashSet::new();
    let mut floats = std::collections::HashSet::new();
    let mut constants = std::collections::HashSet::new();
    let mut original_types: Vec<(String, Vec<String>)> = Vec::new();
    let mut unions: HashMap<String, Vec<(String, Vec<String>)>> = HashMap::new();
    let mut unsupported: HashMap<String, String> = HashMap::new();
    if let Some(Value::Obj(fields)) = caps.get("fields") {
        // Metadata fields (`_id`, `_source`, ...) only come through `METADATA`; mapped fields
        // may still start with an underscore.
        let is_meta = |caps: &Value| caps.as_object().is_some_and(|m| m.values().any(|t| t.get("metadata_field").and_then(Value::as_bool) == Some(true)));
        let mut names: Vec<&String> = fields.iter().filter(|(_, c)| !is_meta(c)).map(|(k, _)| k).collect();
        names.sort();
        // Fields inside nested objects aren't columns: in the indices where their parent is
        // nested they don't exist (they read as null there), elsewhere they resolve as usual.
        // `None`: nested in every index.
        let nested: Vec<(String, Option<Vec<String>>)> = names
            .iter()
            .filter_map(|n| {
                let m = fields[n.as_str()].as_object()?;
                let caps = m.get("nested")?;
                let only = (m.len() > 1).then(|| caps.get("indices").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default());
                Some((format!("{n}."), only))
            })
            .collect();
        for name in names {
            let mut types: Vec<&String> = fields[name].as_object().map(|m| m.keys().collect()).unwrap_or_default();
            let object = |t: &&String| matches!(t.as_str(), "object" | "nested");
            let under: Vec<&Option<Vec<String>>> = nested.iter().filter(|(p, _)| name.starts_with(p.as_str())).map(|(_, i)| i).collect();
            if !under.is_empty() {
                if under.iter().any(|i| i.is_none()) {
                    continue;
                }
                let nested_in: Vec<&String> = under.iter().filter_map(|i| i.as_ref()).flatten().collect();
                let in_nested = |t: &&String| {
                    let idx: Vec<String> = fields[name].get(t.as_str()).and_then(|c| c.get("indices")).and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
                    !idx.is_empty() && idx.iter().all(|i| nested_in.contains(&i))
                };
                types.retain(|t| !in_nested(t));
                if types.is_empty() {
                    continue;
                }
            }
            if types.iter().all(object) {
                continue;
            }
            // `time_series_metric: counter` (in time series indices) makes counter types; a
            // field that is a counter in some indices only is unsupported.
            let caps_of = |t: &str| fields[name].get(t);
            let counter = types.iter().any(|t| caps_of(t).and_then(|c| c.get("time_series_metric")).and_then(Value::as_str) == Some("counter"));
            let metric_conflict = types.iter().any(|t| caps_of(t).and_then(|c| c.get("metric_conflicts_indices")).is_some());
            let ty = match types.as_slice() {
                // An object in some indices and a leaf in others.
                _ if types.iter().any(object) => DataType::Unsupported,
                _ if metric_conflict => DataType::Unsupported,
                [one] if counter => DataType::of_mapping(one).counter_of(),
                // Dates and nanosecond dates read as nanosecond dates.
                [a, b] if matches!((a.as_str(), b.as_str()), ("date", "date_nanos") | ("date_nanos", "date")) => DataType::DateNanos,
                [one] => DataType::of_mapping(one),
                many => {
                    let ts: Vec<DataType> = many.iter().map(|t| DataType::of_mapping(t)).collect();
                    if ts.iter().all(|t| *t == ts[0]) { ts[0] } else { DataType::Unsupported }
                }
            };
            if fields[name].as_object().is_some_and(|m| m.values().all(|t| t.get("aggregatable").and_then(Value::as_bool) == Some(false))) {
                source_order.insert(name.clone());
            }
            if types.iter().any(|t| matches!(t.as_str(), "float" | "half_float")) {
                floats.insert(name.clone());
            }
            if types.iter().any(|t| t.as_str() == "constant_keyword") {
                constants.insert(name.clone());
            }
            if ty == DataType::Unsupported {
                let mut ts: Vec<String> = types.iter().map(|t| t.to_string()).collect();
                ts.sort();
                let indices_of = |t: &str| -> Vec<String> {
                    let mut idx: Vec<String> = fields[name].get(t).and_then(|c| c.get("indices")).and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
                    idx.sort();
                    idx
                };
                // A type Elasticsearch doesn't support makes the whole field unsupported, typed
                // as the first index (by name) that maps such a type has it: not a union.
                let es_unsupported: Vec<&String> = ts.iter().filter(|t| es_unsupported_type(t)).collect();
                if !es_unsupported.is_empty() {
                    let first = if es_unsupported.len() == 1 || ts.len() == 1 {
                        es_unsupported[0].clone()
                    } else {
                        es_unsupported.iter().min_by_key(|t| indices_of(t).into_iter().next()).map(|t| (*t).clone()).unwrap_or_default()
                    };
                    unsupported.insert(name.clone(), first.clone());
                    original_types.push((name.clone(), vec![first]));
                    cols.push(Col { name: name.clone(), ty });
                    continue;
                }
                // Leaf types that differ between indices: a union type conversions resolve.
                if ts.len() > 1 && !ts.iter().any(|t| matches!(t.as_str(), "object" | "nested")) {
                    let by_type: Vec<(String, Vec<String>)> = ts
                        .iter()
                        .map(|t| (t.clone(), indices_of(t)))
                        .collect();
                    unions.insert(name.clone(), by_type);
                }
                original_types.push((name.clone(), ts));
            }

            cols.push(Col { name: name.clone(), ty });
        }
    }
    let fields = cols.iter().map(|c| c.name.clone()).collect();
    for m in metadata {
        let ty = match m.as_str() {
            "_id" | "_index" | "_ignored" | "_index_mode" => DataType::Keyword,
            "_version" => DataType::Long,
            "_source" => DataType::Source,
            "_score" => DataType::Double,
            "_tsid" => DataType::Tsid,
            other => return Err(verification(src, at, &format!("unsupported metadata field [{other}]"))),
        };
        cols.push(Col { name: m.clone(), ty });
    }
    let flattened_in: Vec<(String, Option<Vec<String>>)> = match caps.get("fields") {
        Some(Value::Obj(f)) => f
            .iter()
            .filter_map(|(k, c)| {
                let m = c.as_object()?;
                let caps = m.get("flattened")?;
                let only = (m.len() > 1).then(|| caps.get("indices").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default());
                Some((k.clone(), only))
            })
            .collect(),
        _ => Vec::new(),
    };
    let flattened = flattened_in.iter().map(|(k, _)| k.clone()).collect();
    Ok(Source { index, cols, fields, metadata: metadata.to_vec(), needed: None, ft: Vec::new(), load: Vec::new(), source_order, floats, constants, indices, original_types, unions, unsupported, flattened, flattened_in })
}

fn verification_plain(msg: &str) -> EsError {
    EsError::new(400, "verification_exception", msg)
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.*,:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Translates a `WHERE` over source fields into a query (rows still get the full condition;
/// this only narrows the scan).
fn pushdown(e: &Expr, cols: &[Col], ft: &[Value]) -> Option<Value> {
    let field_of = |x: &Expr| -> Option<(String, DataType)> {
        if let ExprKind::Col(c) = &x.kind {
            let col = cols.iter().find(|k| k.name == *c)?;
            matches!(col.ty, DataType::Keyword | DataType::Integer | DataType::Long | DataType::Double | DataType::Date | DataType::Ip | DataType::Boolean).then(|| (c.clone(), col.ty))
        } else {
            None
        }
    };
    fn find_lit(x: &Expr) -> Option<&V> {
        match &x.kind {
            ExprKind::Lit(V::Null) => None,
            ExprKind::Lit(v) => Some(v),
            _ => None,
        }
    }
    let lit = |x: &Expr| -> Option<Value> {
        match &x.kind {
            ExprKind::Lit(V::Null) => None,
            ExprKind::Lit(v) => Some(v.to_json(v.type_of())),
            _ => None,
        }
    };
    match &e.kind {
        // A full-text call (rewritten to its hidden column) narrows the scan as itself.
        ExprKind::Col(c) if c.starts_with(super::fulltext::HIDDEN) => c[super::fulltext::HIDDEN.len()..].parse::<usize>().ok().and_then(|i| ft.get(i).cloned()),
        ExprKind::Bin(BinOp::And, a, b) => match (pushdown(a, cols, ft), pushdown(b, cols, ft)) {
            (Some(x), Some(y)) => Some(obj! { "bool" => obj! { "filter" => crate::arr![x, y] } }),
            (Some(x), None) | (None, Some(x)) => Some(x),
            _ => None,
        },
        ExprKind::Bin(BinOp::Or, a, b) => {
            let (x, y) = (pushdown(a, cols, ft)?, pushdown(b, cols, ft)?);
            Some(obj! { "bool" => obj! { "should" => crate::arr![x, y], "minimum_should_match" => 1 } })
        }
        ExprKind::Bin(op @ (BinOp::Eq | BinOp::Lt | BinOp::Lte | BinOp::Gt | BinOp::Gte), a, b) => {
            let (f, ty, v, flipped) = match (field_of(a), lit(b), field_of(b), lit(a)) {
                (Some((f, ty)), Some(v), _, _) => (f, ty, v, false),
                (_, _, Some((f, ty)), Some(v)) => (f, ty, v, true),
                _ => return None,
            };
            if ty == DataType::Keyword && !matches!(v, Value::Str(_)) {
                return None;
            }
            // Dates compare as instants: the literal as ES|QL reads it, never through the
            // field's own `format`.
            let date = if ty == DataType::Date {
                let ms = match find_lit(a).or_else(|| find_lit(b))? {
                    V::Date(ms) => *ms,
                    V::Str(s) => value::parse_date(s)?,
                    V::Long(l) => *l,
                    V::Int(i) => i64::from(*i),
                    _ => return None,
                };
                Some(ms)
            } else {
                None
            };
            let v = date.map_or(v, Value::from);
            let range = |rop: &str, v: Value| -> Value {
                let mut r = crate::json::Map::new();
                r.insert(rop.into(), v);
                if date.is_some() {
                    r.insert("format".into(), "epoch_millis".into());
                }
                obj! { "range" => obj! { f.clone() => Value::Obj(r) } }
            };
            let rop = match (op, flipped) {
                (BinOp::Eq, _) if date.is_some() => {
                    let mut r = crate::json::Map::new();
                    r.insert("gte".into(), v.clone());
                    r.insert("lte".into(), v);
                    r.insert("format".into(), "epoch_millis".into());
                    return Some(obj! { "range" => obj! { f => Value::Obj(r) } });
                }
                (BinOp::Eq, _) => return Some(obj! { "term" => obj! { f => v } }),
                (BinOp::Lt, false) | (BinOp::Gt, true) => "lt",
                (BinOp::Lte, false) | (BinOp::Gte, true) => "lte",
                (BinOp::Gt, false) | (BinOp::Lt, true) => "gt",
                _ => "gte",
            };
            Some(range(rop, v))
        }
        ExprKind::IsNull(x, neg) => {
            let (f, _) = field_of(x)?;
            let ex = obj! { "exists" => obj! { "field" => f } };
            Some(if *neg { ex } else { obj! { "bool" => obj! { "must_not" => ex } } })
        }
        ExprKind::In(x, list, false) => {
            let (f, ty) = field_of(x)?;
            if ty == DataType::Date {
                return None;
            }
            let vals: Option<Vec<Value>> = list.iter().map(lit).collect();
            Some(obj! { "terms" => obj! { f => Value::Arr(vals?) } })
        }
        ExprKind::Like(x, p, false, false) => {
            let (f, ty) = field_of(x)?;
            (ty == DataType::Keyword).then(|| obj! { "wildcard" => obj! { f => obj! { "value" => p.clone() } } })
        }
        _ => None,
    }
}

pub(crate) fn refs(e: &Expr, out: &mut std::collections::HashSet<String>) {
    match &e.kind {
        ExprKind::Col(c) => {
            out.insert(c.clone());
        }
        ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::IsNull(x, _) | ExprKind::Like(x, ..) | ExprKind::Cast(x, _) => refs(x, out),
        ExprKind::Bin(_, a, b) => {
            refs(a, out);
            refs(b, out);
        }
        ExprKind::In(x, l, _) => {
            refs(x, out);
            l.iter().for_each(|i| refs(i, out));
        }
        ExprKind::Call(_, args) => args.iter().for_each(|a| refs(a, out)),
        ExprKind::Lit(_) | ExprKind::Star | ExprKind::Span(_) => {}
    }
}

/// The names a query references that neither its source nor an earlier command defines, in
/// order of first reference: the fields `SET unmapped_fields` resolves (`nullify`, `load`).
fn unmapped_references(cmds: &[Cmd], cols: &[Col]) -> Vec<String> {
    let mut defined: std::collections::HashSet<String> = cols.iter().map(|c| c.name.clone()).collect();
    let mut out: Vec<String> = Vec::new();
    let note = |name: &str, defined: &std::collections::HashSet<String>, out: &mut Vec<String>| {
        if !name.contains('*') && !defined.contains(name) && !name.starts_with(super::fulltext::HIDDEN) && !out.iter().any(|o| o == name) {
            out.push(name.to_string());
        }
    };
    fn names_of(e: &Expr) -> Vec<String> {
        let mut s = std::collections::HashSet::new();
        refs(e, &mut s);
        let mut v: Vec<String> = s.into_iter().collect();
        v.sort();
        v
    }
    for c in cmds {
        match c {
            Cmd::Where(e) => names_of(e).iter().for_each(|n| note(n, &defined, &mut out)),
            Cmd::Eval(list) | Cmd::Row(list) => {
                for n in list {
                    names_of(&n.expr).iter().for_each(|x| note(x, &defined, &mut out));
                    defined.insert(n.name.clone());
                }
            }
            Cmd::Stats { aggs, by } | Cmd::InlineStats { aggs, by } => {
                for n in aggs.iter().chain(by) {
                    names_of(&n.expr).iter().for_each(|x| note(x, &defined, &mut out));
                }
                for n in aggs.iter().chain(by) {
                    defined.insert(n.name.clone());
                }
            }
            Cmd::Sort(keys) => keys.iter().for_each(|k| names_of(&k.expr).iter().for_each(|x| note(x, &defined, &mut out))),
            Cmd::Keep(pats) | Cmd::Drop(pats) => pats.iter().for_each(|(p, _)| note(p, &defined, &mut out)),
            Cmd::Rename(list) => {
                for (old, new, _) in list {
                    note(old, &defined, &mut out);
                    defined.insert(new.clone());
                }
            }
            Cmd::MvExpand(n, _) => note(n, &defined, &mut out),
            Cmd::Dissect { field, pattern, .. } => {
                names_of(field).iter().for_each(|x| note(x, &defined, &mut out));
                defined.extend(dissect_keys(pattern));
            }
            Cmd::Grok { field, pattern, .. } => {
                names_of(field).iter().for_each(|x| note(x, &defined, &mut out));
                let mut rest = pattern.as_str();
                while let Some(i) = rest.find("%{") {
                    let Some(j) = rest[i..].find('}') else { break };
                    if let Some(name) = rest[i + 2..i + j].split(':').nth(1) {
                        defined.insert(name.to_string());
                    }
                    rest = &rest[i + j..];
                }
            }
            Cmd::Enrich { on, with, .. } => {
                if let Some((f, _)) = on {
                    note(f, &defined, &mut out);
                }
                // Without `WITH`, every enrich field is added: nothing after can be told apart.
                match with {
                    Some(w) => w.iter().for_each(|(n, _, _)| {
                        defined.insert(n.clone());
                    }),
                    None => return out,
                }
            }
            Cmd::LookupJoin { on, .. } => {
                on.iter().for_each(|(f, _)| note(f, &defined, &mut out));
                // The lookup index's fields join the row.
                return out;
            }
            _ => {}
        }
    }
    out
}

/// The source columns a pipeline reads (`None`: all of them reach the output). Only these are
/// loaded from the index: `FROM x | STATS COUNT(*)` loads no field at all.
fn needed_columns(cmds: &[Cmd], cols: &[Col]) -> Option<std::collections::HashSet<String>> {
    let mut need = std::collections::HashSet::new();
    for c in cmds {
        match c {
            Cmd::Where(e) => refs(e, &mut need),
            Cmd::Eval(list) => list.iter().for_each(|n| refs(&n.expr, &mut need)),
            Cmd::Sort(keys) => keys.iter().for_each(|k| refs(&k.expr, &mut need)),
            Cmd::MvExpand(c, _) => {
                need.insert(c.clone());
            }
            Cmd::Rename(list) => list.iter().for_each(|(old, _, _)| {
                need.insert(old.clone());
            }),
            Cmd::Stats { aggs, by } => {
                aggs.iter().chain(by).for_each(|n| refs(&n.expr, &mut need));
                return Some(need);
            }
            Cmd::Keep(pats) => {
                for col in cols {
                    if pats.iter().any(|(p, _)| glob(p, &col.name)) {
                        need.insert(col.name.clone());
                    }
                }
                return Some(need);
            }
            Cmd::Drop(_) | Cmd::Limit(_) => {}
            _ => return None,
        }
    }
    None
}

/// Scans the source in batches, calling `sink` with each batch of rows; `sink` returns `false`
/// to stop.
async fn scan(cluster: &Cluster, s: &Source, query: Option<Value>, limit: Option<usize>, stats: &mut Stats, mut sink: impl FnMut(Vec<Vec<V>>) -> EsResult<bool>) -> EsResult<()> {
    // Doc values straight from the shards when every needed column has them.
    if s.load.is_empty() && (!s.fields.is_empty() || !s.metadata.is_empty()) {
        let now = crate::time::now_millis();
        if let Some(plan) = super::columnar::plan(cluster, &s.index, &s.cols, &s.fields, &s.metadata, s.needed.as_ref(), s.ft.len(), now)
            && super::columnar::scan(cluster, plan, query.as_ref(), limit, now, stats, &mut sink)?
        {
            return Ok(());
        }
    }
    let mut fields: Vec<Value> = Vec::new();
    for (i, f) in s.fields.iter().enumerate() {
        if s.needed.as_ref().is_some_and(|n| !n.contains(f)) {
            continue;
        }
        let ty = s.cols[i].ty;
        fields.push(match ty {
            DataType::Date | DataType::DateNanos => obj! { "field" => f.clone(), "format" => "epoch_millis" },
            _ if s.load.contains(f) => obj! { "field" => f.clone(), "include_unmapped" => true },
            _ => obj! { "field" => f.clone() },
        });
    }
    let size = limit.map_or(BATCH, |l| l.clamp(1, BATCH));
    let mut body = obj! {
        "size" => size,
        "_source" => false,
        "sort" => crate::arr!["_doc"],
        "fields" => Value::Arr(fields),
        "track_total_hits" => true,
    };
    // Full-text calls are named optional clauses: `matched_queries` fills their columns.
    let query = if s.ft.is_empty() {
        query
    } else {
        let should: Vec<Value> = s
            .ft
            .iter()
            .enumerate()
            .map(|(i, q)| obj! { "bool" => obj! { "must" => crate::arr![q.clone()], "_name" => format!("{}{i}", super::fulltext::HIDDEN) } })
            .collect();
        let mut b = crate::json::Map::new();
        // A filter keeps the `should` clauses optional.
        b.insert("filter".into(), crate::arr![query.unwrap_or_else(|| obj! { "match_all" => obj! {} })]);
        b.insert("should".into(), Value::Arr(should));
        Some(obj! { "bool" => Value::Obj(b) })
    };
    if let (Some(q), Value::Obj(m)) = (query, &mut body) {
        m.insert("query".into(), q);
    }
    if s.metadata.iter().any(|m| m == "_score")
        && let Value::Obj(m) = &mut body
    {
        m.insert("track_scores".into(), true.into());
    }
    if s.metadata.iter().any(|m| m == "_version")
        && let Value::Obj(m) = &mut body
    {
        m.insert("version".into(), true.into());
    }
    if s.metadata.iter().any(|m| m == "_tsid")
        && let Value::Obj(m) = &mut body
    {
        m.insert("docvalue_fields".into(), crate::arr!["_tsid"]);
    }
    if s.metadata.iter().any(|m| m == "_source")
        && let Value::Obj(m) = &mut body
    {
        m.insert("_source".into(), true.into());
    }
    let mut resp = internal::call(cluster, "POST", &format!("/{}/_search?scroll=1m&ignore_unavailable=true", encode(&s.index)), Some(&body)).await?;
    let mut seen = 0usize;
    loop {
        let hits = resp.pointer(&["hits", "hits"]).and_then(Value::as_array).cloned().unwrap_or_default();
        if hits.is_empty() {
            break;
        }
        let mut columnar: HashMap<String, bool> = HashMap::new();
        let mut constants: HashMap<(String, String), V> = HashMap::new();
        let mut rows = Vec::with_capacity(hits.len());
        for h in &hits {
            let index = h.get("_index").and_then(Value::as_str).unwrap_or("");
            let array_order = match columnar.get(index) {
                Some(b) => *b,
                None => {
                    let st = cluster.state();
                    let b = st.metadata.indices.get(index).and_then(|m| m.settings.get("index.mode")).and_then(crate::meta::index_mode::IndexMode::parse).is_some_and(|m| m.is_strict_columnar());
                    columnar.insert(index.to_string(), b);
                    b
                }
            };
            let mut row = Vec::with_capacity(s.cols.len());
            for (i, f) in s.fields.iter().enumerate() {
                // A union type's values read as their own index maps them.
                let ty = match s.unions.get(f) {
                    Some(u) => u.iter().find(|(_, idx)| idx.iter().any(|x| x == index)).map_or(DataType::Unsupported, |(t, _)| DataType::of_mapping(t)),
                    None => s.cols[i].ty,
                };
                let under_flattened = !s.load.contains(f)
                    && s.flattened_in.iter().any(|(r, only)| f.len() > r.len() && f.starts_with(r.as_str()) && f.as_bytes()[r.len()] == b'.' && only.as_ref().is_none_or(|ix| ix.iter().any(|x| x == index)));
                let mut v = match under_flattened {
                    true => V::Null,
                    false => h.pointer(&["fields", f.as_str()]).map(|x| value::from_field_in(x, ty, array_order || s.source_order.contains(f))).unwrap_or(V::Null),
                };
                if s.floats.contains(f) {
                    v = widen_float(v);
                } else if v.is_null() && s.cols[i].ty == DataType::Keyword {
                    // `constant_keyword` (field caps report it as `keyword`): the mapping's value.
                    v = constant_of(cluster, &mut constants, index, f);
                }
                if !v.is_null() {
                    stats.values_loaded += 1;
                }
                row.push(v);
            }
            for m in &s.metadata {
                row.push(match m.as_str() {
                    "_version" => h.get("_version").and_then(Value::as_i64).map(V::Long).unwrap_or(V::Null),
                    // Lucene scores are floats, widened to double.
                    "_score" => V::Double(h.get("_score").and_then(Value::as_f64).unwrap_or(0.0) as f32 as f64),
                    "_source" => h.get("_source").map(|x| V::Str(x.to_json())).unwrap_or(V::Null),
                    // The ignored fields: keyword doc values, sorted.
                    "_ignored" => h.get("_ignored").map(|x| value::from_field_in(x, DataType::Keyword, false)).unwrap_or(V::Null),
                    "_tsid" => h.pointer(&["fields", "_tsid"]).and_then(Value::as_array).and_then(|a| a.first()).map(|x| V::Str(x.to_es_string())).unwrap_or(V::Null),
                    other => h.get(other).map(|x| V::Str(x.to_es_string())).unwrap_or(V::Null),
                });
            }
            if !s.ft.is_empty() {
                let matched = h.get("matched_queries");
                for i in 0..s.ft.len() {
                    let name = format!("{}{i}", super::fulltext::HIDDEN);
                    let hit = match matched {
                        Some(Value::Arr(a)) => a.iter().any(|x| x.as_str() == Some(name.as_str())),
                        Some(Value::Obj(m)) => m.contains_key(&name),
                        _ => false,
                    };
                    row.push(V::Bool(hit));
                }
            }
            rows.push(row);
        }
        stats.documents_found += rows.len() as i64;
        seen += rows.len();
        let more = sink(rows)?;
        if !more || limit.is_some_and(|l| seen >= l) {
            break;
        }
        let Some(id) = resp.get("_scroll_id").map(Value::to_es_string) else { break };
        resp = internal::call(cluster, "POST", "/_search/scroll", Some(&obj! { "scroll" => "1m", "scroll_id" => id })).await?;
    }
    if let Some(id) = resp.get("_scroll_id").map(Value::to_es_string) {
        let _ = internal::call(cluster, "DELETE", "/_search/scroll", Some(&obj! { "scroll_id" => crate::arr![id] })).await;
    }
    Ok(())
}

/// A single-precision value read as a double (what its doc values hold).
fn widen_float(v: V) -> V {
    match v {
        V::Double(d) => V::Double(d as f32 as f64),
        V::Multi(vs) => V::Multi(vs.into_iter().map(widen_float).collect()),
        other => other,
    }
}

/// The `constant_keyword` value of `field` in `index` (cached per scan).
fn constant_of(cluster: &Cluster, cache: &mut HashMap<(String, String), V>, index: &str, field: &str) -> V {
    cache
        .entry((index.to_string(), field.to_string()))
        .or_insert_with(|| {
            let st = cluster.state();
            let Some(idx) = st.metadata.indices.get(index) else { return V::Null };
            let Ok(engine) = cluster.engine.index(cluster, &st, idx) else { return V::Null };
            let table = engine.table();
            table.get(field).and_then(|f| f.constant_value.clone()).map(V::Str).unwrap_or(V::Null)
        })
        .clone()
}

/// Splices views: `FROM <view>` runs the view's query.
/// Rewrites a `FROM` that names ES|QL views (exactly, or through a wildcard) into a
/// [`Cmd::Union`] of the views' queries plus one branch for the remaining index patterns, the
/// way Elasticsearch resolves views as subqueries; nested views and subqueries resolve the
/// same way, and a view reached again along its own chain is a circular reference.
fn expand_views(st: &crate::meta::ClusterState, mut q: Query) -> EsResult<Query> {
    if st.metadata.xpack.len(super::VIEWS) == 0 && !matches!(q.cmds.first(), Some(Cmd::Union { .. })) {
        return Ok(q);
    }
    let text: std::sync::Arc<str> = q.src.as_str().into();
    if let Some(first) = q.cmds.first_mut() {
        expand_source(st, first, &text, &mut Vec::new())?;
    }
    Ok(q)
}

/// Whether `name` is something an index pattern resolves to (index, alias or data stream).
fn is_index_abstraction(st: &crate::meta::ClusterState, name: &str) -> bool {
    st.metadata.indices.contains_key(name) || st.has_alias(name) || st.metadata.data_streams.contains_key(name)
}

fn expand_source(st: &crate::meta::ClusterState, cmd: &mut Cmd, text: &std::sync::Arc<str>, chain: &mut Vec<String>) -> EsResult<()> {
    match cmd {
        Cmd::Union { branches, .. } => {
            for b in branches.iter_mut().filter(|b| b.view.is_none()) {
                let src = b.src.clone();
                if let Some(first) = b.cmds.first_mut() {
                    expand_source(st, first, &src, chain)?;
                }
            }
            Ok(())
        }
        Cmd::From { sources, metadata, at } => {
            let views: Vec<&String> = st.metadata.xpack.iter(super::VIEWS).map(|(k, _)| k).collect();
            if views.is_empty() {
                return Ok(());
            }
            // Written order: each view where it is named, the index patterns together at the
            // first of them.
            enum Part {
                Indices,
                View(String),
            }
            let mut parts: Vec<Part> = Vec::new();
            let mut plain: Vec<String> = Vec::new();
            for s in sources.iter() {
                let mut hit: Vec<String> = Vec::new();
                if s.contains('*') {
                    hit.extend(views.iter().filter(|v| crate::xcontent::filter::glob_match(s, v)).map(|v| v.to_string()));
                } else if views.contains(&s) && !is_index_abstraction(st, s) {
                    hit.push(s.clone());
                }
                let keep = hit.is_empty() || (s.contains('*') && (st.metadata.indices.keys().any(|i| crate::xcontent::filter::glob_match(s, i)) || st.metadata.data_streams.keys().any(|i| crate::xcontent::filter::glob_match(s, i))));
                if keep {
                    if plain.is_empty() {
                        parts.push(Part::Indices);
                    }
                    plain.push(s.clone());
                }
                for v in hit {
                    if !parts.iter().any(|p| matches!(p, Part::View(x) if *x == v)) {
                        parts.push(Part::View(v));
                    }
                }
            }
            if !parts.iter().any(|p| matches!(p, Part::View(_))) {
                return Ok(());
            }
            let mut branches = Vec::new();
            for part in parts {
                match part {
                    Part::Indices => branches.push(super::parser::Branch { cmds: vec![Cmd::From { sources: plain.clone(), metadata: metadata.clone(), at: *at }], src: text.clone(), view: None, at: *at }),
                    Part::View(v) => {
                        if chain.contains(&v) {
                            return Err(verification_plain(&format!("circular view reference '{v}': {}", chain.join(" -> "))));
                        }
                        let body = st.metadata.xpack.get(super::VIEWS, &v).and_then(|x| x.get("query")).map(Value::to_es_string).unwrap_or_default();
                        let vq = super::parser::parse(&body, &[], &[])?;
                        let vtext: std::sync::Arc<str> = body.as_str().into();
                        let mut cmds = vq.cmds;
                        chain.push(v.clone());
                        if let Some(first) = cmds.first_mut() {
                            expand_source(st, first, &vtext, chain)?;
                        }
                        chain.pop();
                        // `METADATA` fields of the outer `FROM` are null over a view.
                        let nulls: Vec<super::parser::Named> = metadata
                            .iter()
                            .map(|m| {
                                let ty = match m.as_str() {
                                    "_score" => "double",
                                    "_version" => "long",
                                    _ => "keyword",
                                };
                                let lit = Expr { kind: ExprKind::Lit(V::Null), start: 0, end: 0 };
                                super::parser::Named { name: m.clone(), expr: Expr { kind: ExprKind::Cast(Box::new(lit), ty.into()), start: 0, end: 0 } }
                            })
                            .collect();
                        if !nulls.is_empty() {
                            cmds.push(Cmd::Eval(nulls));
                        }
                        branches.push(super::parser::Branch { cmds, src: vtext, view: Some(v), at: 0 });
                    }
                }
            }
            *cmd = Cmd::Union { branches, at: *at };
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Elasticsearch's checks on `FORK` placement: one `FORK` per query (each branch of a later
/// `FORK` repeats the earlier one, and is reported once per branch), none inside a subquery.
fn check_forks(text: &str, cmds: &[Cmd]) -> EsResult<()> {
    let mut problems: Vec<(usize, &str)> = Vec::new();
    const MULTIPLE: &str = "Only a single FORK command is supported, but found multiple";
    let mut seen: Vec<usize> = Vec::new();
    for c in cmds {
        if let Cmd::Fork { branches, at } = c {
            for b in branches {
                problems.extend(seen.iter().map(|s| (*s, MULTIPLE)));
                for inner in b {
                    if let Cmd::Fork { at, .. } = inner {
                        problems.push((*at, MULTIPLE));
                    }
                }
            }
            seen.push(*at);
        }
    }
    if let Some(Cmd::Union { branches, .. }) = cmds.first() {
        for b in branches.iter().filter(|b| b.view.is_none()) {
            for c in &b.cmds {
                if let Cmd::Fork { at, .. } = c {
                    problems.push((*at, "FORK inside subquery is not supported"));
                }
            }
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    let n = problems.len();
    let mut msg = format!("Found {n} problem{}", if n == 1 { "" } else { "s" });
    for (at, p) in problems {
        let (l, c) = super::parser::location(text, at);
        msg.push_str(&format!("\nline {l}:{c}: {p}"));
    }
    Err(EsError::new(400, "verification_exception", msg))
}

/// Whether any command of the query, inside `FORK` branches too, is a `LIMIT`.
fn has_limit(cmds: &[Cmd]) -> bool {
    cmds.iter().any(|c| match c {
        Cmd::Limit(_) => true,
        Cmd::Fork { branches, .. } => branches.iter().any(|b| has_limit(b)),
        _ => false,
    })
}

/// Elasticsearch's name for a type in verification messages.
fn type_text(t: DataType) -> &'static str {
    match t {
        DataType::Date => "datetime",
        t => t.name(),
    }
}

/// Where `name` is defined by the commands (the alias of `EVAL name = ...`, `STATS name = ...`).
fn definition_at(text: &str, cmds: &[Cmd], name: &str) -> Option<usize> {
    let alias = |n: &super::parser::Named| {
        let before = text.get(..n.expr.start)?.trim_end();
        let before = before.strip_suffix('=')?.trim_end();
        let start = before.rfind(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '@' | '`'))).map_or(0, |i| i + 1);
        Some(start)
    };
    for c in cmds.iter().rev() {
        let list: Vec<&super::parser::Named> = match c {
            Cmd::Eval(l) | Cmd::Row(l) => l.iter().collect(),
            Cmd::Stats { aggs, by } | Cmd::InlineStats { aggs, by } => aggs.iter().chain(by).collect(),
            _ => continue,
        };
        if let Some(n) = list.into_iter().find(|n| n.name == name) {
            return alias(n).or(Some(n.expr.start));
        }
    }
    None
}

/// The column names a command reads (expressions, `KEEP`/`DROP` names, renamed columns).
fn cmd_refs(c: &Cmd, out: &mut std::collections::HashSet<String>) {
    match c {
        Cmd::Where(e) => refs(e, out),
        Cmd::Eval(l) | Cmd::Row(l) => l.iter().for_each(|n| refs(&n.expr, out)),
        Cmd::Stats { aggs, by } | Cmd::InlineStats { aggs, by } => aggs.iter().chain(by).for_each(|n| refs(&n.expr, out)),
        Cmd::Sort(keys) => keys.iter().for_each(|k| refs(&k.expr, out)),
        Cmd::Keep(p) | Cmd::Drop(p) => p.iter().filter(|(n, _)| !n.contains('*')).for_each(|(n, _)| {
            out.insert(n.clone());
        }),
        Cmd::Rename(l) => l.iter().for_each(|(a, _, _)| {
            out.insert(a.clone());
        }),
        Cmd::MvExpand(n, _) => {
            out.insert(n.clone());
        }
        Cmd::Dissect { field, .. } | Cmd::Grok { field, .. } => refs(field, out),
        Cmd::LookupJoin { on, .. } => on.iter().for_each(|(n, _)| {
            out.insert(n.clone());
        }),
        Cmd::Enrich { on: Some((n, _)), .. } => {
            out.insert(n.clone());
        }
        Cmd::Fork { branches, .. } => branches.iter().flatten().for_each(|c| cmd_refs(c, out)),
        _ => {}
    }
}

/// The union of tables (`FORK` branches, subqueries): columns in order of first appearance,
/// rows of each table in turn, null where a table lacks a column. A column of two types is an
/// error (`conflict(name, existing, new, table)`), unless `union_ok` lets it become a union type.
fn union_tables(parts: Vec<Table>, conflict: &dyn Fn(&str, DataType, DataType, usize) -> EsError, union_ok: &dyn Fn(&str, usize) -> bool, original: &mut Vec<(String, Vec<String>)>) -> EsResult<Table> {
    let mut cols: Vec<Col> = Vec::new();
    let mut maps: Vec<Vec<usize>> = Vec::with_capacity(parts.len());
    for (pi, t) in parts.iter().enumerate() {
        let mut map = Vec::with_capacity(t.cols.len());
        for c in &t.cols {
            match cols.iter().position(|x| x.name == c.name) {
                Some(i) => {
                    let have = cols[i].ty;
                    if have != c.ty && c.ty != DataType::Null {
                        if have == DataType::Null {
                            cols[i].ty = c.ty;
                        } else if have.is_string() && c.ty.is_string() {
                            cols[i].ty = DataType::Keyword;
                        } else if union_ok(&c.name, pi) {
                            let mut types: Vec<String> = original.iter().find(|(n, _)| *n == c.name).map(|(_, t)| t.clone()).unwrap_or_else(|| vec![have.name().to_string()]);
                            if !types.iter().any(|t| t == c.ty.name()) {
                                types.push(c.ty.name().to_string());
                            }
                            types.sort();
                            original.retain(|(n, _)| *n != c.name);
                            original.push((c.name.clone(), types));
                            cols[i].ty = DataType::Unsupported;
                        } else {
                            return Err(conflict(&c.name, have, c.ty, pi));
                        }
                    }
                    map.push(i);
                }
                None => {
                    map.push(cols.len());
                    cols.push(c.clone());
                }
            }
        }
        maps.push(map);
    }
    let total = parts.iter().map(|t| t.rows.len()).sum();
    let mut rows = Vec::with_capacity(total);
    for (t, map) in parts.into_iter().zip(&maps) {
        for r in t.rows {
            let mut out = vec![V::Null; cols.len()];
            for (v, &i) in r.into_iter().zip(map) {
                out[i] = v;
            }
            rows.push(out);
        }
    }
    Ok(Table { cols, rows })
}

fn merge_stats(into: &mut Stats, from: Stats) {
    into.documents_found += from.documents_found;
    into.values_loaded += from.values_loaded;
    match (&mut into.indices, from.indices) {
        (Some(a), Some(b)) => a.extend(b.into_iter().filter(|x| !a.contains(x)).collect::<Vec<_>>()),
        (a @ None, b) => *a = b,
        _ => {}
    }
    for f in from.fields {
        if !into.fields.contains(&f) {
            into.fields.push(f);
        }
    }
    into.col_meta.extend(from.col_meta);
    for (n, t) in from.original_types {
        if !into.original_types.iter().any(|(x, _)| *x == n) {
            into.original_types.push((n, t));
        }
    }
}

/// Runs a parsed query.
pub async fn execute(cluster: &Cluster, q: Query, filter: Option<Value>, w: &mut Warnings) -> EsResult<(Table, Stats)> {
    let (mut t, stats) = execute_inner(cluster, q, filter, w).await?;
    // Unsupported columns (union types read for their conversions among them) print null.
    let unsupported: Vec<usize> = (0..t.cols.len()).filter(|&i| t.cols[i].ty == DataType::Unsupported).collect();
    if !unsupported.is_empty() {
        for r in &mut t.rows {
            for &i in &unsupported {
                if let Some(v) = r.get_mut(i) {
                    *v = V::Null;
                }
            }
        }
    }
    // Full-text columns never reach the output.
    if t.cols.iter().any(|c| c.name.starts_with(super::fulltext::HIDDEN)) {
        let keep: Vec<usize> = (0..t.cols.len()).filter(|&i| !t.cols[i].name.starts_with(super::fulltext::HIDDEN)).collect();
        t.cols = keep.iter().map(|&i| t.cols[i].clone()).collect();
        for r in &mut t.rows {
            *r = keep.iter().map(|&i| r[i].clone()).collect();
        }
    }
    Ok((t, stats))
}

async fn execute_inner(cluster: &Cluster, q: Query, filter: Option<Value>, w: &mut Warnings) -> EsResult<(Table, Stats)> {
    let q = expand_views(&cluster.state(), q)?;
    // An implicit `LIMIT 1000` ends every query without a limit.
    if !has_limit(&q.cmds) && !q.sql {
        w.add(format!("No limit defined, adding default limit of [{DEFAULT_LIMIT}]"));
    }
    check_forks(&q.src, &q.cmds)?;
    run_query(cluster, q, filter, w, true, &[]).await
}

/// Every column name the commands reference, with where (expressions, `KEEP`/`DROP`
/// patterns, renamed and expanded columns), in source order.
fn ref_positions(cmds: &[Cmd]) -> Vec<(String, usize)> {
    fn expr(e: &Expr, out: &mut Vec<(String, usize)>) {
        match &e.kind {
            ExprKind::Col(n) => out.push((n.clone(), e.start)),
            ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::IsNull(x, _) | ExprKind::Like(x, ..) | ExprKind::Cast(x, _) => expr(x, out),
            ExprKind::Bin(_, a, b) => {
                expr(a, out);
                expr(b, out);
            }
            ExprKind::In(x, l, _) => {
                expr(x, out);
                l.iter().for_each(|a| expr(a, out));
            }
            ExprKind::Call(_, args) => args.iter().for_each(|a| expr(a, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for c in cmds {
        match c {
            Cmd::Where(e) => expr(e, &mut out),
            Cmd::Eval(l) | Cmd::Row(l) => l.iter().for_each(|n| expr(&n.expr, &mut out)),
            Cmd::Stats { aggs, by } | Cmd::InlineStats { aggs, by } => aggs.iter().chain(by).for_each(|n| expr(&n.expr, &mut out)),
            Cmd::Sort(k) => k.iter().for_each(|k| expr(&k.expr, &mut out)),
            Cmd::Keep(p) | Cmd::Drop(p) => out.extend(p.iter().cloned()),
            Cmd::Rename(l) => out.extend(l.iter().map(|(a, _, at)| (a.clone(), *at))),
            Cmd::MvExpand(n, at) => out.push((n.clone(), *at)),
            Cmd::Dissect { field, .. } | Cmd::Grok { field, .. } => expr(field, &mut out),
            _ => {}
        }
    }
    out
}

/// `unmapped_fields="load"` can't read below a `flattened` field: a mapped field under one
/// (in another index) fails at the `FROM`, a referenced unmapped name under one where it is.
fn check_flattened_load(src: &Src<'_>, source: &Source, cmds: &[Cmd], from_at: usize) -> EsResult<()> {
    let parent = |name: &str| source.flattened.iter().filter(|f| name.len() > f.len() && name.starts_with(f.as_str()) && name.as_bytes()[f.len()] == b'.').max_by_key(|f| f.len()).cloned();
    let mut problems: Vec<(usize, String, String)> = Vec::new();
    let refs = ref_positions(cmds);
    // A mapped field under a flattened parent, when something reads it (or below it).
    for f in &source.fields {
        let read = refs.iter().any(|(r, _)| r == f || (r.starts_with(f.as_str()) && r.as_bytes().get(f.len()) == Some(&b'.')));
        if read && let Some(p) = parent(f) {
            problems.push((from_at, f.clone(), p));
        }
    }
    for (name, at) in refs {
        let known = source.cols.iter().any(|c| c.name == name || (name.contains('*') && glob(&name, &c.name)));
        if !known && let Some(p) = parent(&name) {
            problems.push((at, name, p));
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    let n = problems.len();
    let mut msg = format!("Found {n} problem{}", if n == 1 { "" } else { "s" });
    for (at, name, p) in problems {
        let (l, c) = super::parser::location(src, at);
        msg.push_str(&format!("\nline {l}:{c}: Loading subfield [{name}] when parent [{p}] is of flattened field type is not supported with unmapped_fields=\"load\""));
    }
    Err(EsError::new(400, "verification_exception", msg))
}

/// `TS`'s time series functions and what each aggregates per series (`$` names take the
/// timestamp too).
fn ts_inner(name: &str) -> Option<&'static str> {
    Some(match name {
        "avg_over_time" => "avg",
        "sum_over_time" => "sum",
        "min_over_time" => "min",
        "max_over_time" => "max",
        "count_over_time" => "count",
        "count_distinct_over_time" => "count_distinct",
        "present_over_time" => "present",
        "absent_over_time" => "absent",
        "stddev_over_time" => "std_dev",
        "variance_over_time" => "variance",
        "percentile_over_time" => "percentile",
        "first_over_time" => "$first",
        "last_over_time" => "$last",
        "rate" => "$rate",
        "irate" => "$irate",
        "increase" => "$increase",
        "delta" => "$delta",
        "idelta" => "$idelta",
        "deriv" => "$deriv",
        _ => return None,
    })
}

/// Rewrites a `TS` query: the source becomes a `FROM` that also reads `_tsid`, and the first
/// `STATS` becomes two: per time series (`_tsid` plus the groups) the inner functions (a plain
/// aggregate reads each series' last value), then the outer aggregates across series.
fn ts_rewrite(text: &str, cmds: &mut Vec<Cmd>) -> EsResult<()> {
    let Some(Cmd::Ts { sources, metadata, at }) = cmds.first().cloned() else { return Ok(()) };
    let Some(si) = cmds.iter().position(|c| matches!(c, Cmd::Stats { .. })) else {
        cmds[0] = Cmd::From { sources, metadata, at };
        return Ok(());
    };
    let mut md = metadata.clone();
    if !md.iter().any(|m| m == "_tsid") {
        md.push("_tsid".into());
    }
    cmds[0] = Cmd::From { sources, metadata: md, at };
    // `@timestamp` must survive the commands before `STATS`.
    let mut has_ts = true;
    for c in &cmds[1..si] {
        match c {
            Cmd::Rename(l) if l.iter().any(|(a, _, _)| a == "@timestamp") => has_ts = false,
            Cmd::Drop(p) if p.iter().any(|(n, _)| glob(n, "@timestamp")) => has_ts = false,
            Cmd::Keep(p) if !p.iter().any(|(n, _)| glob(n, "@timestamp")) => has_ts = false,
            _ => {}
        }
    }
    let Cmd::Stats { aggs, by } = cmds[si].clone() else { unreachable!() };
    let col = |name: &str, at: usize| Expr { kind: ExprKind::Col(name.into()), start: at, end: at };
    let call = |f: &str, args: Vec<Expr>, e: &Expr| Expr { kind: ExprKind::Call(f.into(), args), start: e.start, end: e.end };
    let mut inner_aggs = Vec::new();
    let mut outer_aggs = Vec::new();
    for (i, a) in aggs.iter().enumerate() {
        let slot = format!("$ts{i}");
        let needs_ts = |e: &Expr| -> EsResult<()> {
            if has_ts {
                return Ok(());
            }
            Err(verification(text, e.start, &format!("[{}] requires the [@timestamp] field, which was either not present in the source index, or has been dropped or renamed", &text[e.start..e.end])))
        };
        let inner_of = |e: &Expr| -> EsResult<Expr> {
            match &e.kind {
                ExprKind::Call(f, args) if ts_inner(f).is_some() => {
                    let to = ts_inner(f).expect("checked");
                    needs_ts(e)?;
                    let mut args = args.clone();
                    if to.starts_with('$') {
                        args.truncate(1);
                        args.push(col("@timestamp", e.start));
                    }
                    Ok(call(to, args, e))
                }
                // Anything else reads each series' last value in the bucket.
                _ => Ok(call("$last", vec![e.clone(), col("@timestamp", e.start)], e)),
            }
        };
        match &a.expr.kind {
            ExprKind::Call(f, args) if (f == "first" || f == "last") && args.len() == 2 && matches!(args[1].kind, ExprKind::Lit(V::Null)) => {
                needs_ts(&a.expr)?;
                let to = if f == "first" { "$first" } else { "$last" };
                inner_aggs.push(super::parser::Named { name: slot.clone(), expr: call(to, vec![args[0].clone(), col("@timestamp", a.expr.start)], &a.expr) });
                outer_aggs.push(super::parser::Named { name: a.name.clone(), expr: call(to.trim_start_matches('$'), vec![col(&slot, a.expr.start)], &a.expr) });
            }
            ExprKind::Call(f, args) if value::AGGS.contains(&f.as_str()) && !f.starts_with('$') && !args.is_empty() => {
                let inner = if matches!(args[0].kind, ExprKind::Star) { call("count", vec![args[0].clone()], &a.expr) } else { inner_of(&args[0])? };
                let outer_fn = if matches!(args[0].kind, ExprKind::Star) { "sum".to_string() } else { f.clone() };
                inner_aggs.push(super::parser::Named { name: slot.clone(), expr: inner });
                let mut oargs = vec![col(&slot, a.expr.start)];
                oargs.extend(args.iter().skip(1).cloned());
                outer_aggs.push(super::parser::Named { name: a.name.clone(), expr: call(&outer_fn, oargs, &a.expr) });
            }
            ExprKind::Call(f, _) if ts_inner(f).is_some() => {
                inner_aggs.push(super::parser::Named { name: slot.clone(), expr: inner_of(&a.expr)? });
                outer_aggs.push(super::parser::Named { name: a.name.clone(), expr: call("$last", vec![col(&slot, a.expr.start), col("@timestamp", a.expr.start)], &a.expr) });
            }
            _ => {
                inner_aggs.push(a.clone());
                outer_aggs.push(super::parser::Named { name: a.name.clone(), expr: col(&a.name, a.expr.start) });
            }
        }
    }
    let mut inner_by = vec![super::parser::Named { name: "_tsid".into(), expr: col("_tsid", at) }];
    inner_by.extend(by.iter().cloned());
    let outer_by: Vec<super::parser::Named> = by.iter().map(|n| super::parser::Named { name: n.name.clone(), expr: col(&n.name, n.expr.start) }).collect();
    cmds[si] = Cmd::Stats { aggs: inner_aggs, by: inner_by };
    cmds.insert(si + 1, Cmd::Stats { aggs: outer_aggs, by: outer_by });
    Ok(())
}

/// A boxed [`run_query`] (it recurses through subqueries).
type QueryRun<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = EsResult<(Table, Stats)>> + Send + 'a>>;

/// Runs a query (`top`: the request's own, which gets the implicit limit; branches of a union
/// don't). `outer`: the commands that will run over this query's result, for fields they
/// reference that `SET unmapped_fields` has to provide.
fn run_query<'a>(cluster: &'a Cluster, mut q: Query, filter: Option<Value>, w: &'a mut Warnings, top: bool, outer: &'a [Cmd]) -> QueryRun<'a> {
    Box::pin(async move {
        if matches!(q.cmds.first(), Some(Cmd::Ts { .. })) {
            ts_rewrite(&q.src, &mut q.cmds)?;
        }
        if top && !q.cmds.iter().any(|c| matches!(c, Cmd::Limit(_))) {
            q.cmds.push(Cmd::Limit(DEFAULT_LIMIT));
        }
        let fork = q.cmds.iter().position(|c| matches!(c, Cmd::Fork { .. }));
        let union = matches!(q.cmds.first(), Some(Cmd::Union { .. }));
        if fork.is_none() && !union {
            return execute_plain(cluster, q, filter, w, outer).await;
        }
        let mut stats = Stats::default();
        let (parts, rest): (Vec<Table>, Vec<Cmd>) = if let Some(f) = fork {
            let Cmd::Fork { branches, at } = q.cmds[f].clone() else { unreachable!() };
            let rest: Vec<Cmd> = q.cmds[f + 1..].to_vec();
            let refs: Vec<Cmd> = rest.iter().chain(outer).cloned().collect();
            let mut parts = Vec::with_capacity(branches.len());
            for (i, b) in branches.iter().enumerate() {
                let mut cmds: Vec<Cmd> = q.cmds[..f].to_vec();
                cmds.extend(b.iter().cloned());
                if !b.iter().any(|c| matches!(c, Cmd::Limit(_))) {
                    cmds.push(Cmd::Limit(DEFAULT_LIMIT));
                }
                let tag = Expr { kind: ExprKind::Lit(V::Str(format!("fork{}", i + 1))), start: at, end: at };
                cmds.push(Cmd::Eval(vec![super::parser::Named { name: "_fork".into(), expr: tag }]));
                let bq = Query { src: q.src.clone(), cmds, sql: q.sql, locale: q.locale.clone(), settings: q.settings.clone() };
                let (t, s) = run_query(cluster, bq, filter.clone(), w, false, &refs).await?;
                merge_stats(&mut stats, s);
                parts.push(t);
            }
            let text = q.src.clone();
            let conflict = |name: &str, have: DataType, new: DataType, pi: usize| {
                let at = definition_at(&text, &branches[pi], name).unwrap_or(at);
                verification(&text, at, &format!("Column [{name}] has conflicting data types in FORK branches: [{}] and [{}]", type_text(new).to_ascii_uppercase(), type_text(have).to_ascii_uppercase()))
            };
            let mut original = std::mem::take(&mut stats.original_types);
            let t = union_tables(parts, &conflict, &|_, _| false, &mut original)?;
            stats.original_types = original;
            (vec![t], rest)
        } else {
            let Cmd::Union { branches, at } = q.cmds[0].clone() else { unreachable!() };
            let rest: Vec<Cmd> = q.cmds[1..].to_vec();
            // Names the commands over the union read reach the branches' `unmapped_fields`
            // only when no branch reads them itself (that branch provides the column then).
            let refs: Vec<Cmd> = rest.iter().chain(outer).cloned().collect();
            let mut mentioned = std::collections::HashSet::new();
            for b in &branches {
                for c in b.cmds.iter().skip(1) {
                    cmd_refs(c, &mut mentioned);
                }
            }
            let mut wanted = std::collections::HashSet::new();
            for c in &refs {
                cmd_refs(c, &mut wanted);
            }
            let mut pass: Vec<String> = wanted.into_iter().filter(|n| !mentioned.contains(n)).collect();
            pass.sort();
            let carry: Vec<Cmd> = if pass.is_empty() { Vec::new() } else { vec![Cmd::Keep(pass.into_iter().map(|n| (n, 0)).collect())] };
            let mut parts = Vec::with_capacity(branches.len());
            for b in &branches {
                let bq = Query { src: b.src.to_string(), cmds: b.cmds.clone(), sql: q.sql, locale: q.locale.clone(), settings: q.settings.clone() };
                let (t, s) = run_query(cluster, bq, filter.clone(), w, false, &carry).await?;
                merge_stats(&mut stats, s);
                parts.push(t);
            }
            let parts_types: Vec<Vec<(String, DataType)>> = parts.iter().map(|t| t.cols.iter().map(|c| (c.name.clone(), c.ty)).collect()).collect();
            let text = q.src.clone();
            let first_view = branches.first().and_then(|b| b.view.clone());
            let conflict = |name: &str, have: DataType, new: DataType, _pi: usize| {
                let (l, c) = if first_view.is_some() { (1, 1) } else { super::parser::location(&text, at) };
                let inv = first_view.as_ref().map(|v| format!(" (in view [{v}])")).unwrap_or_default();
                EsError::new(400, "verification_exception", format!("Found 1 problem\nline {l}:{c}{inv}: Column [{name}] has conflicting data types in subqueries: [{}, {}]", type_text(have), type_text(new)))
            };
            // A column of several types is tolerated (an unsupported union type) until a
            // command after the union reads it.
            let deferred: std::cell::RefCell<Vec<(String, EsError)>> = std::cell::RefCell::new(Vec::new());
            let lazy = |name: &str, pi: usize| {
                let have = parts_types[..pi].iter().find_map(|t| t.iter().find(|(n, _)| n == name).map(|(_, t)| *t));
                let new = parts_types[pi].iter().find(|(n, _)| n == name).map(|(_, t)| *t);
                if let (Some(h), Some(n)) = (have, new) {
                    deferred.borrow_mut().push((name.to_string(), conflict(name, h, n, pi)));
                }
                true
            };
            let mut original = std::mem::take(&mut stats.original_types);
            let t = union_tables(parts, &conflict, &lazy, &mut original)?;
            stats.original_types = original;
            let mut used = std::collections::HashSet::new();
            for c in rest.iter().filter(|c| !matches!(c, Cmd::Keep(_) | Cmd::Drop(_))) {
                cmd_refs(c, &mut used);
            }
            if let Some((_, e)) = deferred.into_inner().into_iter().find(|(n, _)| used.contains(n)) {
                return Err(e);
            }
            (vec![t], rest)
        };
        let t = parts.into_iter().next().unwrap_or_default();
        let text = q.src.clone();
        let res = load_resources(cluster, &text, &rest).await?;
        let tz = q.settings.time_zone.as_deref().and_then(|z| crate::time::Zone::parse(z).ok()).filter(|z| *z != crate::time::Zone::UTC);
        let src = Src { text: &text, sql: q.sql, locale: q.locale.as_deref(), res: &res, tz };
        let t = run_rest(&src, t, &rest, w)?;
        stats.rows_emitted = t.rows.len() as i64;
        stats.col_meta.extend(std::mem::take(&mut *src.res.col_meta.lock()));
        Ok((t, stats))
    })
}

/// Runs a query with no `FORK` and no union source.
async fn execute_plain(cluster: &Cluster, q: Query, filter: Option<Value>, w: &mut Warnings, outer: &[Cmd]) -> EsResult<(Table, Stats)> {
    let text = q.src.clone();
    let res = load_resources(cluster, &text, &q.cmds).await?;
    let tz = q.settings.time_zone.as_deref().and_then(|z| crate::time::Zone::parse(z).ok()).filter(|z| *z != crate::time::Zone::UTC);
    let src = Src { text: &text, sql: q.sql, locale: q.locale.as_deref(), res: &res, tz };
    let mut stats = Stats::default();
    let mut cmds: Vec<Cmd> = q.cmds.clone();
    let first = cmds.remove(0);
    // Fields the commands over this query's result reference count for `unmapped_fields`.
    let ref_cmds: Vec<Cmd> = cmds.iter().chain(outer).cloned().collect();
    let ft = if matches!(first, Cmd::From { .. }) { super::fulltext::extract(&src, &mut cmds)? } else { Vec::new() };
    // The row-wise prefix runs on every batch.
    let split = cmds.iter().position(|c| !is_row_wise(c)).unwrap_or(cmds.len());
    let (prefix, rest) = cmds.split_at(split);
    let mut table = match &first {
        Cmd::Row(list) => {
            let mut cols: Vec<Col> = Vec::new();
            let mut row = Vec::new();
            for n in list {
                let (c, t) = Compiler { src: &src, cols: &cols, aggs: None }.compile(&n.expr)?;
                let v = eval(&c, &row, &[], w)?;
                if let Some(i) = cols.iter().position(|x| x.name == n.name) {
                    cols.remove(i);
                    row.remove(i);
                }
                cols.push(Col { name: n.name.clone(), ty: t });
                row.push(v);
            }
            if q.settings.unmapped != Unmapped::Default {
                for name in unmapped_references(&ref_cmds, &cols) {
                    cols.push(Col { name, ty: DataType::Null });
                    row.push(V::Null);
                }
            }
            let (p, out) = build_pipeline(&src, prefix, cols)?;
            Table { rows: p.run(vec![row], w)?, cols: out }
        }
        Cmd::Show(what) => {
            if what != "INFO" {
                return Err(super::parser::parse_error(&src, 5, &format!("mismatched input '{what}' expecting 'info'")));
            }
            let cols = vec![Col { name: "version".into(), ty: DataType::Keyword }, Col { name: "date".into(), ty: DataType::Keyword }, Col { name: "hash".into(), ty: DataType::Keyword }];
            let row = vec![V::Str(crate::version::ES_VERSION.into()), V::Str(crate::version::ES_BUILD_DATE.into()), V::Str(crate::version::ES_BUILD_HASH.into())];
            let (p, out) = build_pipeline(&src, prefix, cols)?;
            Table { rows: p.run(vec![row], w)?, cols: out }
        }
        Cmd::From { sources, metadata, at } => {
            let mut source = resolve_source(cluster, &src, sources, metadata, *at).await?;
            if q.settings.unmapped == Unmapped::Load && !source.flattened.is_empty() {
                check_flattened_load(&src, &source, &ref_cmds, *at)?;
            }
            if q.settings.unmapped != Unmapped::Default && source.cols.first().is_none_or(|c| c.name != "<no-fields>") {
                let load = q.settings.unmapped == Unmapped::Load;
                for name in unmapped_references(&ref_cmds, &source.cols) {
                    let at = source.fields.len();
                    source.cols.insert(at, Col { name: name.clone(), ty: if load { DataType::Keyword } else { DataType::Null } });
                    source.fields.push(name.clone());
                    if load {
                        source.source_order.insert(name.clone());
                        source.load.push(name);
                    }
                }
                // A keyword some target indices leave unmapped loads from their `_source`.
                if load && source.indices.len() > 1 {
                    let mut used = std::collections::HashSet::new();
                    ref_cmds.iter().for_each(|c| cmd_refs(c, &mut used));
                    let candidates: Vec<String> = source.fields.iter().zip(&source.cols).filter(|(f, c)| c.ty == DataType::Keyword && used.contains(*f) && !source.load.contains(f)).map(|(f, _)| f.clone()).collect();
                    if !candidates.is_empty() {
                        let path = format!("/{}/_field_caps?fields={}&include_unmapped=true&ignore_unavailable=true&expand_wildcards=open", encode(&source.index), encode(&candidates.join(",")));
                        let caps = internal::call(cluster, "GET", &path, None).await.unwrap_or(Value::Null);
                        for f in candidates {
                            if caps.get("fields").and_then(|m| m.get(&f)).and_then(|m| m.get("unmapped")).is_some() {
                                source.source_order.insert(f.clone());
                                source.load.push(f);
                            }
                        }
                    }
                }
            }
            for i in 0..ft.len() {
                source.cols.push(Col { name: format!("{}{i}", super::fulltext::HIDDEN), ty: DataType::Boolean });
            }
            source.ft = ft;
            source.needed = needed_columns(&cmds, &source.cols);
            stats.indices = Some(source.indices.clone());
            stats.original_types = source.original_types.clone();
            let _ = src.res.unions.set(source.unions.clone());
            let _ = src.res.unsupported.set(source.unsupported.clone());
            stats.fields = source.fields.iter().filter(|f| source.needed.as_ref().is_none_or(|n| n.contains(*f))).cloned().collect();
            let _ = src.res.suggest.set((source.needed.clone(), source.cols.iter().map(|c| c.name.clone()).collect()));
            let (pipe, out_cols) = build_pipeline(&src, prefix, source.cols.clone())?;
            // Fields loaded from `_source` aren't indexed: they never narrow the scan.
            let push_cols: Vec<Col> = source.cols.iter().map(|c| if source.load.contains(&c.name) || source.constants.contains(&c.name) || (source.metadata.contains(&c.name) && !matches!(c.name.as_str(), "_index" | "_id")) { Col { name: c.name.clone(), ty: DataType::Unsupported } } else { c.clone() }).collect();
            let pushed = prefix.first().and_then(|c| if let Cmd::Where(e) = c { pushdown(e, &push_cols, &source.ft) } else { None });
            // The request's `filter` narrows the source like a leading `WHERE`.
            let query = match (pushed, filter) {
                (Some(a), Some(b)) => Some(obj! { "bool" => obj! { "filter" => crate::arr![a, b] } }),
                (a, b) => a.or(b),
            };
            match rest.first() {
                Some(Cmd::Stats { aggs, by }) => {
                    let mut g = Grouper::new(&src, aggs, by, &out_cols)?;
                    scan(cluster, &source, query, None, &mut stats, |rows| {
                        let rows = pipe.run(rows, w)?;
                        g.add(&rows, w)?;
                        Ok(true)
                    })
                    .await?;
                    let t = g.finish(w)?;
                    return finish(cluster, &src, t, &rest[1..], w, stats).await;
                }
                Some(Cmd::Sort(_)) if sort_then_limit(rest).is_some() => {
                    let (keys, n, j) = sort_then_limit(rest).expect("matched");
                    let mut top = TopN::new(SortSpec::new(&src, keys, &out_cols)?, n.min(MAX_LIMIT));
                    match top_from_source(cluster, &src, &source, prefix, keys, top.n, query.as_ref(), &mut stats, w)? {
                        Some(rows) => top.add(rows, w)?,
                        None => {
                            scan(cluster, &source, query, None, &mut stats, |rows| {
                                let rows = pipe.run(rows, w)?;
                                top.add(rows, w)?;
                                Ok(true)
                            })
                            .await?
                        }
                    }
                    let t = Table { cols: out_cols, rows: top.finish() };
                    // The one-row-per-row commands between `SORT` and `LIMIT` run on the `n` rows.
                    let after: Vec<Cmd> = rest[1..j].iter().chain(&rest[j + 1..]).cloned().collect();
                    return finish(cluster, &src, t, &after, w, stats).await;
                }
                Some(Cmd::Limit(n)) => {
                    let n = (*n).min(MAX_LIMIT);
                    let only_scan_bound = keeps_one_row_per_doc(prefix);
                    let mut rows_out: Vec<Vec<V>> = Vec::new();
                    scan(cluster, &source, query, only_scan_bound.then_some(n), &mut stats, |rows| {
                        let mut rows = pipe.run(rows, w)?;
                        rows.truncate(n - rows_out.len());
                        rows_out.extend(rows);
                        Ok(rows_out.len() < n)
                    })
                    .await?;
                    let t = Table { cols: out_cols, rows: rows_out };
                    return finish(cluster, &src, t, &rest[1..], w, stats).await;
                }
                _ => {
                    let mut rows_out: Vec<Vec<V>> = Vec::new();
                    scan(cluster, &source, query, None, &mut stats, |rows| {
                        rows_out.extend(pipe.run(rows, w)?);
                        Ok(true)
                    })
                    .await?;
                    Table { cols: out_cols, rows: rows_out }
                }
            }
        }
        _ => return Err(super::parser::parse_error(&src, 0, "mismatched input expecting {'from', 'row', 'show'}")),
    };
    if matches!(first, Cmd::From { .. }) {
        return finish(cluster, &src, table, rest, w, stats).await;
    }
    table = run_rest(&src, table, rest, w)?;
    stats.rows_emitted = table.rows.len() as i64;
    stats.col_meta.extend(std::mem::take(&mut *src.res.col_meta.lock()));
    Ok((table, stats))
}

/// Whether `prefix` turns each doc into exactly one row, so a `LIMIT n` after it can stop the
/// scan after `n` docs: `WHERE` and `SAMPLE` drop rows, `MV_EXPAND` multiplies them.
fn keeps_one_row_per_doc(prefix: &[Cmd]) -> bool {
    prefix.iter().all(|c| !matches!(c, Cmd::Where(_) | Cmd::MvExpand(..) | Cmd::Sample(_)))
}

/// Loads the enrich tables and lookup indices `cmds` read.
async fn load_resources(cluster: &Cluster, text: &str, cmds: &[Cmd]) -> EsResult<Resources> {
    let mut res = Resources::default();
    let mut warmed = false;
    for c in cmds {
        match c {
            Cmd::Enrich { policy, .. } => {
                let name = policy.rsplit(':').next().unwrap_or(policy).to_string();
                if res.enrich.contains_key(&name) {
                    continue;
                }
                let fields_in_order: Vec<String> = {
                    let st = cluster.state();
                    let Some(p) = st.metadata.xpack.get(crate::xpack::enrich::POLICIES, &name) else { continue };
                    let cfg = p.get("config").and_then(Value::as_object).and_then(|m| m.values().next()).cloned().unwrap_or(Value::Null);
                    match cfg.get("enrich_fields") {
                        Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect(),
                        Some(Value::Str(s)) => vec![s.clone()],
                        _ => Vec::new(),
                    }
                };
                if !warmed {
                    crate::xpack::enrich::warm(cluster).await;
                    warmed = true;
                }
                let table = cluster.xpack.enrich.tables.read().get(&name).cloned();
                let Some(table) = table else { continue };
                let caps = internal::call(cluster, "GET", &format!("/{}/_field_caps?fields=*", encode(&table.index)), None).await.unwrap_or(Value::Null);
                let mut cols = Vec::new();
                for f in fields_in_order {
                    let ty = caps.get("fields").and_then(|m| m.get(&f)).and_then(Value::as_object).and_then(|m| m.keys().next()).map(|t| DataType::of_mapping(t)).unwrap_or(DataType::Keyword);
                    cols.push(Col { name: f, ty });
                }
                res.enrich.insert(name, (table, cols));
            }
            Cmd::LookupJoin { index, index_at, .. } => {
                if res.lookup.contains_key(index) {
                    continue;
                }
                // The name may be an index, an alias or a data stream: it must resolve to one
                // concrete index, in lookup mode.
                let (concrete, mode) = {
                    let st = cluster.state();
                    let concrete: Vec<String> = if st.metadata.indices.contains_key(index.as_str()) {
                        vec![index.clone()]
                    } else if let Some(ds) = st.metadata.data_streams.get(index.as_str()) {
                        ds.indices.clone()
                    } else {
                        st.metadata.indices.iter().filter(|(_, m)| m.aliases.contains_key(index.as_str())).map(|(k, _)| k.clone()).collect()
                    };
                    match concrete.as_slice() {
                        [] => continue,
                        [one] => {
                            let mode = st.metadata.indices.get(one.as_str()).and_then(|m| m.settings.get("index.mode")).map(|s| s.to_string()).unwrap_or_else(|| "standard".into());
                            (one.clone(), mode)
                        }
                        _ => return Err(verification(text, *index_at, &format!("Lookup Join requires a single lookup mode index; [{index}] resolves to multiple indices"))),
                    }
                };
                if mode != "lookup" {
                    return Err(verification_plain(&format!("Lookup Join requires a single lookup mode index; [{index}] resolves to [{concrete}] in [{mode}] mode")));
                }
                let source = resolve_source(cluster, text, std::slice::from_ref(&concrete), &[], *index_at).await?;
                let mut rows: Vec<Vec<V>> = Vec::new();
                let mut stats = Stats::default();
                scan(cluster, &source, None, None, &mut stats, |batch| {
                    rows.extend(batch);
                    Ok(true)
                })
                .await?;
                res.lookup.insert(index.clone(), Table { cols: source.cols, rows });
            }
            _ => {}
        }
    }
    Ok(res)
}

async fn finish(_cluster: &Cluster, src: &Src<'_>, t: Table, rest: &[Cmd], w: &mut Warnings, mut stats: Stats) -> EsResult<(Table, Stats)> {
    let t = run_rest(src, t, rest, w)?;
    stats.rows_emitted = t.rows.len() as i64;
    stats.col_meta.extend(std::mem::take(&mut *src.res.col_meta.lock()));
    Ok((t, stats))
}

/// The commands after the first blocking one, over a materialized table.
fn run_rest(src: &Src<'_>, mut t: Table, cmds: &[Cmd], w: &mut Warnings) -> EsResult<Table> {
    for cmd in cmds {
        match cmd {
            Cmd::Stats { aggs, by } => {
                let mut g = Grouper::new(src, aggs, by, &t.cols)?;
                g.add(&t.rows, w)?;
                t = g.finish(w)?;
            }
            Cmd::InlineStats { aggs, by } => {
                // Every row gets its group's aggregations, the group columns moving after them.
                let mut g = Grouper::new(src, aggs, by, &t.cols)?;
                g.add(&t.rows, w)?;
                let key_cols: Vec<CE> = g.keys.iter().map(|(c, _, _)| c.clone()).collect();
                let stats = g.finish(w)?;
                let nk = by.len();
                let na = stats.cols.len() - nk;
                let mut by_key: HashMap<Vec<GroupKey>, Vec<V>> = HashMap::new();
                for r in &stats.rows {
                    by_key.insert(r[na..].iter().map(GroupKey::of).collect(), r[..na].to_vec());
                }
                let names: Vec<String> = stats.cols.iter().map(|c| c.name.clone()).collect();
                let (keep, mut cols) = keep_except(&t.cols, &names);
                cols.extend(stats.cols.iter().cloned());
                let mut rows = Vec::with_capacity(t.rows.len());
                for r in std::mem::take(&mut t.rows) {
                    let mut kv = Vec::with_capacity(nk);
                    for c in &key_cols {
                        kv.push(eval(c, &r, &[], w)?);
                    }
                    let mut n: Vec<V> = keep.iter().map(|i| r[*i].clone()).collect();
                    if kv.iter().any(|v| matches!(v, V::Multi(_))) {
                        // A multi-valued key joins every group it fell into: each aggregation
                        // becomes the multi-value of those groups' results, in key order.
                        let mut per: Vec<Vec<V>> = vec![Vec::new(); na];
                        for key in expand_keys(&kv) {
                            let k: Vec<GroupKey> = key.iter().map(GroupKey::of).collect();
                            if let Some(a) = by_key.get(&k) {
                                for (acc, v) in per.iter_mut().zip(a) {
                                    acc.extend(v.values().into_iter().cloned());
                                }
                            }
                        }
                        n.extend(per.into_iter().map(|mut vs| match vs.len() {
                            0 => V::Null,
                            1 => vs.pop().expect("one"),
                            _ => V::Multi(vs),
                        }));
                    } else {
                        let k: Vec<GroupKey> = kv.iter().map(GroupKey::of).collect();
                        match by_key.get(&k) {
                            Some(a) => n.extend(a.iter().cloned()),
                            None => n.extend(std::iter::repeat_n(V::Null, na)),
                        }
                    }
                    n.extend(kv);
                    rows.push(n);
                }
                t = Table { cols, rows };
            }
            Cmd::Sort(keys) => sort_rows(src, keys, &mut t, w)?,
            Cmd::Limit(n) => t.rows.truncate((*n).min(MAX_LIMIT)),
            c if is_row_wise(c) => {
                let (p, out) = build_pipeline(src, std::slice::from_ref(c), t.cols.clone())?;
                t.rows = p.run(std::mem::take(&mut t.rows), w)?;
                t.cols = out;
            }
            _ => return Err(super::parser::parse_error(src, 0, "mismatched input 'FROM' expecting {'eval', 'keep', 'limit', 'sort', 'stats', 'where'}")),
        }
    }
    Ok(t)
}

/// The columns and rows as the JSON response prints them (`columnar`: one array per column).
pub fn values_json(t: &Table, columnar: bool) -> Value {
    values_json_in(t, columnar, None)
}

/// [`values_json`] with dates printed in `tz` (`2025-05-31T02:00:00.000+02:00`).
pub fn values_json_in(t: &Table, columnar: bool, tz: Option<&crate::time::Zone>) -> Value {
    let cell = |v: &V, ty: DataType| -> Value {
        match (tz, v) {
            (Some(z), V::Date(_) | V::Multi(_)) if ty == DataType::Date => zoned_json(v, z),
            _ => v.to_json(ty),
        }
    };
    if columnar {
        Value::Arr((0..t.cols.len()).map(|i| Value::Arr(t.rows.iter().map(|r| cell(&r[i], t.cols[i].ty)).collect())).collect())
    } else {
        Value::Arr(t.rows.iter().map(|r| Value::Arr(r.iter().enumerate().map(|(i, v)| cell(v, t.cols[i].ty)).collect())).collect())
    }
}

fn zoned_json(v: &V, z: &crate::time::Zone) -> Value {
    use std::sync::OnceLock;
    static F: OnceLock<crate::time::DateFormatter> = OnceLock::new();
    let f = F.get_or_init(|| crate::time::DateFormatter::new("strict_date_optional_time").expect("built-in format"));
    match v {
        V::Date(ms) => Value::from(f.format(crate::fetch::instant_of(false, *ms), z)),
        V::Multi(vs) => Value::Arr(vs.iter().map(|x| zoned_json(x, z)).collect()),
        other => other.to_json(DataType::Date),
    }
}

pub fn columns_json(t: &Table) -> Value {
    Value::Arr(t.cols.iter().map(|c| obj! { "name" => c.name.clone(), "type" => c.ty.name() }).collect())
}

/// Text formats (`txt`, `csv`, `tsv`), shared with SQL.
pub fn render_text(names: &[String], rows: &[Vec<String>], format: &str, delimiter: char, header: bool) -> String {
    let mut out = String::new();
    match format {
        "txt" => {
            let widths: Vec<usize> = names
                .iter()
                .enumerate()
                .map(|(i, n)| rows.iter().map(|r| r[i].chars().count()).chain(std::iter::once(n.chars().count())).max().unwrap_or(0).max(15))
                .collect();
            if header {
                let cells: Vec<String> = names
                    .iter()
                    .zip(&widths)
                    .map(|(n, w)| {
                        let len = n.chars().count();
                        let left = (w - len) / 2;
                        format!("{}{}{}", " ".repeat(left), n, " ".repeat(w - len - left))
                    })
                    .collect();
                out.push_str(&cells.join("|"));
                out.push('\n');
                out.push_str(&widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("+"));
                out.push('\n');
            }
            for r in rows {
                let cells: Vec<String> = r.iter().zip(&widths).map(|(c, w)| format!("{c}{}", " ".repeat(w - c.chars().count()))).collect();
                out.push_str(&cells.join("|"));
                out.push('\n');
            }
        }
        _ => {
            let esc = |s: &str| -> String {
                if format == "tsv" {
                    return s.replace('\\', "\\\\").replace('\n', "\\n").replace('\t', "\\t");
                }
                if s.contains(delimiter) || s.contains('"') || s.contains('\n') || s.contains('\r') {
                    format!("\"{}\"", s.replace('"', "\"\""))
                } else {
                    s.to_string()
                }
            };
            let sep = if format == "tsv" { "\t".to_string() } else { delimiter.to_string() };
            let eol = if format == "tsv" { "\n" } else { "\r\n" };
            if header {
                out.push_str(&names.iter().map(|n| esc(n)).collect::<Vec<_>>().join(&sep));
                out.push_str(eol);
            }
            for r in rows {
                out.push_str(&r.iter().map(|c| esc(c)).collect::<Vec<_>>().join(&sep));
                out.push_str(eol);
            }
        }
    }
    out
}

/// A cell as the text formats print it (`null` empty in csv/tsv).
pub fn cell_text(v: &V, ty: DataType, format: &str) -> String {
    match v {
        V::Null => {
            if format == "txt" {
                "null".into()
            } else {
                String::new()
            }
        }
        V::Long(l) if ty == DataType::Date => crate::time::java_instant_string(*l),
        V::Multi(vs) => format!("[{}]", vs.iter().map(|x| cell_text(x, ty, format)).collect::<Vec<_>>().join(", ")),
        other => other.to_text(),
    }
}

/// `drop_null_columns`: the indices of columns with at least one value.
pub fn non_null_columns(t: &Table) -> Vec<usize> {
    (0..t.cols.len()).filter(|i| t.rows.iter().any(|r| !r[*i].is_null())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Table {
        Table {
            cols: vec![Col { name: "a".into(), ty: DataType::Integer }, Col { name: "b".into(), ty: DataType::Keyword }],
            rows: vec![vec![V::Int(1), V::Str("x".into())], vec![V::Int(2), V::Str("y".into())], vec![V::Int(3), V::Str("x".into())], vec![V::Null, V::Null]],
        }
    }

    fn run(q: &str) -> Table {
        let parsed = super::super::parser::parse(&format!("ROW z = 1 | {q}"), &[], &[]).unwrap();
        let mut w = Warnings::default();
        run_rest(&Src { text: &parsed.src, sql: false, locale: None, res: &Resources::default(), tz: None }, table(), &parsed.cmds[1..], &mut w).unwrap()
    }

    fn src_of(text: &str) -> (String, Resources) {
        (text.to_string(), Resources::default())
    }

    fn cmds(q: &str) -> Vec<Cmd> {
        super::super::parser::parse(q, &[], &[]).unwrap().cmds
    }

    #[test]
    fn groups_hash_values_in_place_like_group_keys() {
        let text = "ROW z = 1 | STATS c = COUNT(*) BY a";
        let parsed = super::super::parser::parse(text, &[], &[]).unwrap();
        let Cmd::Stats { aggs, by } = &parsed.cmds[1] else { panic!() };
        let res = Resources::default();
        let src = Src { text, sql: false, locale: None, res: &res, tz: None };
        let mut w = Warnings::default();
        let mut g = Grouper::new(&src, aggs, by, &[Col { name: "a".into(), ty: DataType::Double }]).unwrap();
        let s = |x: &str| V::Str(x.into());
        let rows = vec![
            vec![V::Double(0.0)],
            vec![V::Double(-0.0)],
            vec![V::Int(3)],
            vec![V::Long(3)],
            // Each distinct value of a multi-value once.
            vec![V::Multi(vec![s("x"), s("x"), s("y")])],
            vec![V::Ip("x".into())],
            vec![V::Null],
            vec![V::Null],
        ];
        g.add(&rows, &mut w).unwrap();
        let t = g.finish(&mut w).unwrap();
        let got: Vec<(V, V)> = t.rows.iter().map(|r| (r[1].clone(), r[0].clone())).collect();
        // Doubles group by their bits, as Elasticsearch's double BlockHash: -0.0 is its own group.
        assert_eq!(got, vec![(V::Double(0.0), V::Long(1)), (V::Double(-0.0), V::Long(1)), (V::Int(3), V::Long(2)), (s("x"), V::Long(2)), (s("y"), V::Long(1)), (V::Null, V::Long(2))]);
        // Many groups, so hashes collide and chain.
        let mut g = Grouper::new(&src, aggs, by, &[Col { name: "a".into(), ty: DataType::Long }]).unwrap();
        let rows: Vec<Vec<V>> = (0..20_000).map(|i| vec![V::Long(i % 5000)]).collect();
        g.add(&rows, &mut w).unwrap();
        let t = g.finish(&mut w).unwrap();
        assert_eq!(t.rows.len(), 5000);
        assert!(t.rows.iter().enumerate().all(|(i, r)| r[0] == V::Long(4) && r[1] == V::Long(i as i64)));
    }

    #[test]
    fn union_types_resolve_only_through_conversions_taking_every_type() {
        let res = Resources::default();
        let u = vec![("ip".to_string(), vec!["a".to_string()]), ("keyword".to_string(), vec!["b".to_string(), "c".to_string()])];
        res.unions.set(HashMap::from([("client_ip".to_string(), u)])).unwrap();
        let cols = vec![Col { name: "client_ip".into(), ty: DataType::Unsupported }];
        let compile = |text: &str| -> EsResult<DataType> {
            let parsed = super::super::parser::parse(&format!("ROW z = 1 | EVAL x = {text}"), &[], &[]).unwrap();
            let Cmd::Eval(list) = &parsed.cmds[1] else { panic!() };
            let src = Src { text: &parsed.src, sql: false, locale: None, res: &res, tz: None };
            Compiler { src: &src, cols: &cols, aggs: None }.compile(&list[0].expr).map(|(_, t)| t)
        };
        assert_eq!(compile("TO_IP(client_ip)").unwrap(), DataType::Ip);
        assert_eq!(compile("client_ip::ip").unwrap(), DataType::Ip);
        assert_eq!(compile("TO_STRING(client_ip)").unwrap(), DataType::Keyword);
        let err = compile("TO_LONG(client_ip)").unwrap_err();
        assert!(err.reason().contains("Cannot use field [client_ip] due to ambiguities being mapped as [2] incompatible types: [ip] in [a], [keyword] in [b, c]"), "{}", err.reason());
        assert!(compile("client_ip == \"x\"").unwrap_err().reason().contains("due to ambiguities"));
        // A field of a type Elasticsearch doesn't support fails in any expression, conversions
        // included ("Cannot use field [g] with unsupported type [binary]").
        let res2 = Resources::default();
        res2.unsupported.set(HashMap::from([("client_ip".to_string(), "binary".to_string())])).unwrap();
        let parsed = super::super::parser::parse("ROW z = 1 | EVAL x = TO_IP(client_ip)", &[], &[]).unwrap();
        let Cmd::Eval(list) = &parsed.cmds[1] else { panic!() };
        let src = Src { text: &parsed.src, sql: false, locale: None, res: &res2, tz: None };
        let err = Compiler { src: &src, cols: &cols, aggs: None }.compile(&list[0].expr).unwrap_err();
        assert_eq!(err.reason(), "Found 1 problem\nline 1:28: Cannot use field [client_ip] with unsupported type [binary]");
        // Unsupported for roll alone (no known Elasticsearch-unsupported type): reads as null.
        let res3 = Resources::default();
        let parsed = super::super::parser::parse("ROW z = 1 | EVAL x = client_ip", &[], &[]).unwrap();
        let Cmd::Eval(list) = &parsed.cmds[1] else { panic!() };
        let src = Src { text: &parsed.src, sql: false, locale: None, res: &res3, tz: None };
        assert_eq!(Compiler { src: &src, cols: &cols, aggs: None }.compile(&list[0].expr).unwrap().1, DataType::Unsupported);
    }

    /// Runs `q` (after `ROW z = 1 | `) over one row with `t = ["b", "a"]`, `n = [3, 1]`; the
    /// result and the warnings.
    fn run_multi(q: &str) -> (Table, Vec<String>) {
        let parsed = super::super::parser::parse(&format!("ROW z = 1 | {q}"), &[], &[]).unwrap();
        let mut w = Warnings::default();
        let t = Table {
            cols: vec![Col { name: "t".into(), ty: DataType::Keyword }, Col { name: "n".into(), ty: DataType::Long }],
            rows: vec![vec![V::Multi(vec![V::Str("b".into()), V::Str("a".into())]), V::Multi(vec![V::Long(3), V::Long(1)])]],
        };
        let out = run_rest(&Src { text: &parsed.src, sql: false, locale: None, res: &Resources::default(), tz: None }, t, &parsed.cmds[1..], &mut w).unwrap();
        (out, w.list)
    }

    #[test]
    fn multi_valued_comparisons_like_and_in_warn_like_elasticsearch() {
        let mv = "java.lang.IllegalArgumentException: single-value function encountered multi-value";
        let (t, w) = run_multi(r#"EVAL x = t == "a", y = t > "a", z = t LIKE "a*", u = t IN ("a", "x"), v = 1 IN (n, 3) | KEEP x, y, z, u, v"#);
        assert_eq!(t.rows, vec![vec![V::Null; 5]]);
        let expect = [("1:22", r#"t == "a""#), ("1:36", r#"t > "a""#), ("1:49", r#"t LIKE "a*""#), ("1:66", r#"t IN ("a", "x")"#), ("1:87", "1 IN (n, 3)")];
        let mut want = Vec::new();
        for (at, text) in expect {
            want.push(format!("Line {at}: evaluation of [{text}] failed, treating result as null. Only first 20 failures recorded."));
            want.push(format!("Line {at}: {mv}"));
        }
        assert_eq!(w, want);
        // `WHERE` drops the row with the same warnings; a comparison with the null literal folds
        // to null without evaluating.
        let (t, w) = run_multi(r#"WHERE t == "a""#);
        assert!(t.rows.is_empty());
        assert_eq!(w.len(), 2);
        let (t, w) = run_multi("EVAL s = t > null, q = null IN (n) | KEEP s, q");
        assert_eq!((t.rows, w), (vec![vec![V::Null, V::Null]], vec![]));
    }

    #[test]
    fn multi_valued_conversions_warn_for_every_failed_value() {
        let (t, w) = run_multi(r#"EVAL k = TO_LONG(t), m = TO_LONG(["1", "x", "2", "y"]) | KEEP k, m"#);
        assert_eq!(t.rows, vec![vec![V::Null, V::Multi(vec![V::Long(1), V::Long(2)])]]);
        let p = "org.elasticsearch.xpack.esql.core.InvalidArgumentException: Cannot parse number";
        assert_eq!(
            w,
            vec![
                "Line 1:22: evaluation of [TO_LONG(t)] failed, treating result as null. Only first 20 failures recorded.".to_string(),
                format!("Line 1:22: {p} [b]"),
                format!("Line 1:22: {p} [a]"),
                r#"Line 1:38: evaluation of [TO_LONG(["1", "x", "2", "y"])] failed, treating result as null. Only first 20 failures recorded."#.to_string(),
                format!("Line 1:38: {p} [x]"),
                format!("Line 1:38: {p} [y]"),
            ]
        );
    }

    #[test]
    fn sample_keeps_the_scan_unbounded_ahead_of_limit() {
        // `SAMPLE` drops rows, so a `LIMIT` after it can't stop the scan after `n` docs.
        let bound = |q: &str| {
            let c = cmds(q);
            keeps_one_row_per_doc(&c[1..c.len() - 1])
        };
        assert!(!bound("FROM i | SAMPLE 0.5 | LIMIT 10"));
        assert!(bound("FROM i | EVAL x = 1 | LIMIT 10"));
    }

    #[test]
    fn rename_of_union_or_unsupported_fields_reports_every_problem() {
        let res = Resources::default();
        let u = vec![("keyword".to_string(), vec!["u2".to_string()]), ("long".to_string(), vec!["u1".to_string()])];
        res.unions.set(HashMap::from([("f".to_string(), u)])).unwrap();
        res.unsupported.set(HashMap::from([("g".to_string(), "binary".to_string())])).unwrap();
        let cols = vec![Col { name: "f".into(), ty: DataType::Unsupported }, Col { name: "g".into(), ty: DataType::Unsupported }, Col { name: "h".into(), ty: DataType::Long }];
        let parsed = super::super::parser::parse("FROM u* | RENAME f AS z, g AS w, h AS v", &[], &[]).unwrap();
        let src = Src { text: &parsed.src, sql: false, locale: None, res: &res, tz: None };
        let Err(err) = build_pipeline(&src, &parsed.cmds[1..], cols.clone()) else { panic!("rename must fail") };
        assert_eq!(
            err.reason(),
            "Found 2 problems\nline 1:18: Cannot use field [f] due to ambiguities being mapped as [2] incompatible types: [keyword] in [u2], [long] in [u1]\nline 1:26: Cannot use field [g] with unsupported type [binary]"
        );
        let parsed = super::super::parser::parse("FROM u* | RENAME h AS v", &[], &[]).unwrap();
        let src = Src { text: &parsed.src, sql: false, locale: None, res: &res, tz: None };
        assert!(build_pipeline(&src, &parsed.cmds[1..], cols).is_ok());
    }

    #[test]
    fn elasticsearch_unsupported_types() {
        for t in ["binary", "completion", "ip_range", "integer_range", "join", "percolator", "rank_feature", "rank_features", "search_as_you_type", "sparse_vector"] {
            assert!(es_unsupported_type(t), "{t}");
        }
        for t in ["keyword", "long", "object", "dense_vector", "histogram", "flattened", "aggregate_metric_double", "date_range", "geo_shape"] {
            assert!(!es_unsupported_type(t), "{t}");
        }
    }

    #[test]
    fn limits_move_ahead_of_one_row_per_row_commands() {
        let c = cmds("FROM i | SORT a | EVAL b = a + 1 | KEEP b | RENAME b AS c | DROP x | LIMIT 7 | WHERE c > 1");
        let (keys, n, j) = sort_then_limit(&c[1..]).unwrap();
        assert_eq!((keys.len(), n, j), (1, 7, 5));
        // A row-filtering or row-multiplying command in between keeps the full sort.
        assert!(sort_then_limit(&cmds("FROM i | SORT a | WHERE a > 1 | LIMIT 3")[1..]).is_none());
        assert!(sort_then_limit(&cmds("FROM i | SORT a | MV_EXPAND a | LIMIT 3")[1..]).is_none());
        assert!(sort_then_limit(&cmds("FROM i | SORT a")[1..]).is_none());
    }

    #[test]
    fn sort_keys_map_back_to_source_columns() {
        let cols = vec![Col { name: "a".into(), ty: DataType::Long }, Col { name: "b".into(), ty: DataType::Keyword }, Col { name: "s".into(), ty: DataType::Source }];
        let key_names = |q: &str| -> Option<(usize, Vec<String>)> {
            let c = cmds(q);
            let sort = c.iter().position(|c| matches!(c, Cmd::Sort(_))).unwrap();
            let Cmd::Sort(keys) = &c[sort] else { unreachable!() };
            top_source_keys(&c[1..sort], keys, &cols).map(|(nw, ks)| {
                (nw, ks.iter().map(|k| if let ExprKind::Col(n) = &k.expr.kind { n.clone() } else { String::new() }).collect())
            })
        };
        assert_eq!(key_names("FROM i | WHERE a > 1 | WHERE b == \"x\" | SORT a, b"), Some((2, vec!["a".into(), "b".into()])));
        // Renames are followed back, chained renames in order.
        assert_eq!(key_names("FROM i | RENAME a AS z, b AS a | SORT a DESC"), Some((0, vec!["b".into()])));
        assert_eq!(key_names("FROM i | RENAME a AS z | KEEP z | SORT z"), Some((0, vec!["a".into()])));
        // A key EVAL defines, an expression key, a WHERE after an EVAL, `_source`: no pushdown.
        assert_eq!(key_names("FROM i | EVAL a = a + 1 | SORT a"), None);
        assert_eq!(key_names("FROM i | SORT a + 1"), None);
        assert_eq!(key_names("FROM i | EVAL c = 1 | WHERE a > 1 | SORT a"), None);
        assert_eq!(key_names("FROM i | SORT s"), None);
        assert_eq!(key_names("FROM i | EVAL c = a | SORT b"), Some((0, vec!["b".into()])));
    }

    #[test]
    fn top_n_bound_drops_rows_that_cannot_win() {
        let (text, res) = src_of("ROW a = 1 | SORT a");
        let src = Src { text: &text, sql: false, locale: None, res: &res, tz: None };
        let parsed = super::super::parser::parse(&text, &[], &[]).unwrap();
        let Cmd::Sort(keys) = &parsed.cmds[1] else { panic!() };
        let cols = vec![Col { name: "a".into(), ty: DataType::Integer }, Col { name: "seq".into(), ty: DataType::Integer }];
        let mut w = Warnings::default();
        let mut top = TopN::new(SortSpec::new(&src, keys, &cols).unwrap(), 2);
        top.add((0..2000).map(|i| vec![V::Int(i % 10), V::Int(i)]).collect(), &mut w).unwrap();
        assert!(top.bound.is_some());
        let before = top.buf.len();
        // Equal keys arriving later never beat the kept ones.
        top.add((0..100).map(|i| vec![V::Int(0), V::Int(10_000 + i)]).collect(), &mut w).unwrap();
        assert_eq!(top.buf.len(), before);
        top.add(vec![vec![V::Null, V::Int(-1)], vec![V::Int(-5), V::Int(-2)]], &mut w).unwrap();
        let out = top.finish();
        assert_eq!(out, vec![vec![V::Int(-5), V::Int(-2)], vec![V::Int(0), V::Int(0)]]);
    }

    #[test]
    fn top_n_keeps_the_best_rows_in_arrival_order_for_ties() {
        let (text, res) = src_of("ROW a = 1 | SORT a DESC");
        let src = Src { text: &text, sql: false, locale: None, res: &res, tz: None };
        let parsed = super::super::parser::parse(&text, &[], &[]).unwrap();
        let Cmd::Sort(keys) = &parsed.cmds[1] else { panic!() };
        let cols = vec![Col { name: "a".into(), ty: DataType::Integer }, Col { name: "seq".into(), ty: DataType::Integer }];
        let mut w = Warnings::default();
        let mut top = TopN::new(SortSpec::new(&src, keys, &cols).unwrap(), 3);
        // Many batches, so the buffer is trimmed several times; ties on `a` keep arrival order.
        let mut seq = 0;
        for batch in 0..20 {
            let rows: Vec<Vec<V>> = (0..500)
                .map(|i| {
                    seq += 1;
                    vec![if (batch * 500 + i) % 997 == 0 { V::Int(100) } else { V::Int(i % 50) }, V::Int(seq)]
                })
                .collect();
            top.add(rows, &mut w).unwrap();
        }
        let out = top.finish();
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|r| r[0] == V::Int(100)));
        let seqs: Vec<&V> = out.iter().map(|r| &r[1]).collect();
        assert!(seqs.windows(2).all(|w| value::cmp(w[0], w[1]).is_lt()), "{seqs:?}");
        let empty = TopN::new(SortSpec::new(&src, keys, &cols).unwrap(), 0);
        assert!(empty.finish().is_empty());
    }

    #[test]
    fn like_patterns_compile_once_and_fail_like_es() {
        let m = Matcher::new("a?c*", false, false).unwrap();
        assert!(m.matches("abcdef") && !m.matches("ac") && m.matches("a\u{e9}c"));
        let r = Matcher::new("a.c|x[0-9]+", true, false).unwrap();
        assert!(r.matches("abc") && r.matches("x42") && !r.matches("abcd"));
        assert_eq!(Matcher::new("(", true, false).unwrap_err(), "Invalid regex pattern for RLIKE [(]: [unexpected end-of-string]");
        assert_eq!(Matcher::new("*a?????????????", false, false).unwrap_err(), "Invalid regex pattern for RLIKE [*a?????????????]: [Pattern was too complex to determinize]");
        // SQL never fails on a pattern.
        assert!(!Matcher::new("(", true, true).unwrap().matches("("));
    }

    #[test]
    fn unmapped_references_skip_defined_names() {
        let q = super::super::parser::parse("FROM i | EVAL x = a + b | WHERE c > x | RENAME d AS e | KEEP e, x, f*, g | STATS m = MAX(h) BY e", &[], &[]).unwrap();
        let cols = vec![Col { name: "a".into(), ty: DataType::Long }];
        assert_eq!(unmapped_references(&q.cmds[1..], &cols), ["b", "c", "d", "g", "h"]);
    }

    #[test]
    fn zones_shift_to_wall_clock_and_back() {
        let paris = crate::time::Zone::parse("Europe/Paris").unwrap();
        // 2025-05-31T22:30:00Z is 00:30 on June 1st in Paris (UTC+2 in summer).
        let ms = 1_748_730_600_000;
        assert_eq!(to_local(&paris, ms), ms + 2 * 3_600_000);
        assert_eq!(from_local(&paris, to_local(&paris, ms)), ms);
        let fixed = crate::time::Zone::parse("-05:00").unwrap();
        assert_eq!(to_local(&fixed, 0), -5 * 3_600_000);
        assert_eq!(map_dates(V::Multi(vec![V::Date(0), V::Null]), &|m| m + 1), V::Multi(vec![V::Date(1), V::Null]));
        let t = Table { cols: vec![Col { name: "d".into(), ty: DataType::Date }], rows: vec![vec![V::Date(ms)]] };
        assert_eq!(values_json_in(&t, false, Some(&paris)).to_json(), r#"[["2025-06-01T00:30:00.000+02:00"]]"#);
        assert_eq!(values_json(&t, false).to_json(), r#"[["2025-05-31T22:30:00.000Z"]]"#);
    }

    #[test]
    fn stats_by_group() {
        let t = run("STATS c = COUNT(*), s = SUM(a), m = MAX(a) BY b | SORT b");
        assert_eq!(t.cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["c", "s", "m", "b"]);
        assert_eq!(t.rows[0], vec![V::Long(2), V::Long(4), V::Int(3), V::Str("x".into())]);
        assert_eq!(t.rows[2], vec![V::Long(1), V::Null, V::Null, V::Null]);
        let t = run("STATS COUNT(a), avg = AVG(a)");
        assert_eq!(t.rows, vec![vec![V::Long(3), V::Double(2.0)]]);
    }

    #[test]
    fn where_eval_keep_sort_limit() {
        let t = run("WHERE a > 1 | EVAL d = a * 2 | KEEP d, b | SORT d DESC | LIMIT 1");
        assert_eq!(t.cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["d", "b"]);
        assert_eq!(t.rows, vec![vec![V::Int(6), V::Str("x".into())]]);
        let t = run("SORT a NULLS FIRST | LIMIT 1");
        assert_eq!(t.rows[0][0], V::Null);
        let t = run("RENAME b AS c | DROP a");
        assert_eq!(t.cols, vec![Col { name: "c".into(), ty: DataType::Keyword }]);
    }

    #[test]
    fn unknown_columns_fail_with_locations() {
        let parsed = super::super::parser::parse("ROW z = 1 | WHERE nofield > 1", &[], &[]).unwrap();
        let mut w = Warnings::default();
        let e = run_rest(&Src { text: &parsed.src, sql: false, locale: None, res: &Resources::default(), tz: None }, table(), &parsed.cmds[1..], &mut w).unwrap_err();
        assert_eq!(e.reason(), "Found 1 problem\nline 1:19: Unknown column [nofield]");
    }

    #[test]
    fn text_formats() {
        let txt = render_text(&["c".into(), "message".into()], &[vec!["1".into(), "Disconnected".into()]], "txt", ',', true);
        assert_eq!(txt, "       c       |    message    \n---------------+---------------\n1              |Disconnected   \n");
        let csv = render_text(&["a".into()], &[vec!["x,y".into()]], "csv", ',', true);
        assert_eq!(csv, "a\r\n\"x,y\"\r\n");
    }

    #[test]
    fn pushdown_translates_simple_conditions() {
        let cols = vec![Col { name: "n".into(), ty: DataType::Long }, Col { name: "k".into(), ty: DataType::Keyword }];
        let q = super::super::parser::parse("ROW x = 1 | WHERE n > 5 AND k == \"a\"", &[], &[]).unwrap();
        let Cmd::Where(e) = &q.cmds[1] else { panic!() };
        let d = pushdown(e, &cols, &[]).unwrap();
        assert_eq!(d.to_json(), r#"{"bool":{"filter":[{"range":{"n":{"gt":5}}},{"term":{"k":"a"}}]}}"#);
    }
}
