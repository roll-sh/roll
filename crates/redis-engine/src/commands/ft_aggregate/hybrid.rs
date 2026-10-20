//! FT.HYBRID (`hybridCommandHandler`): a text SEARCH subquery and a vector VSIM subquery, each
//! run through its own pipeline and drained (`RPDepleter`), then merged by document key
//! (`RPHybridMerger`) with RRF or LINEAR scores and passed through the tail's
//! LOAD/GROUPBY/APPLY/SORTBY/FILTER/LIMIT steps. FT.PROFILE HYBRID reports both subqueries'
//! pipelines and the tail's.
//!
//! The arguments after the subqueries are read the way the module's generic `ArgParser` reads
//! them (named arguments in any order, COMBINE also as the first one, callbacks that may read
//! on from the main cursor, callback errors before parser errors).
//!
//! WITHCURSOR drains both subqueries and leaves a cursor on each (`HybridRequest_StartCursors`),
//! which FT.CURSOR READ reads like an FT.AGGREGATE's. Timeouts: each subquery's reader and the
//! merger check the clock (`_FT.DEBUG FT.HYBRID`'s TIMEOUT_AFTER_N_SEARCH, _VSIM and _TAIL time
//! them out after that many results); the warnings name where it happened.

use crate::fp::Mac;
use super::*;
use crate::search::areq::{AGG_LOAD_ALL, CursorConfig, F_IS_HYBRID_TAIL, F_IS_HYBRID_VECTOR, F_NO_SORT, HybridVec, Request};
use crate::search::args::{AcErr, GE0, GE1};
use crate::search::rsdict::RsDict;
use crate::search::spec::Metric;
use crate::config::Config;
use std::rc::Rc;

const DIALECT_ERROR: &str =
    "DIALECT is not supported in FT.HYBRID or any of its subqueries. Please check the documentation on search-default-dialect configuration.";

fn dialect_err() -> QErr {
    QErr::new(Code::ParseArgs, DIALECT_ERROR)
}

// ---------------------------------------------------------------------------------------------
// `ArgParser`

#[derive(Clone, Copy)]
enum Ty {
    Flag,
    /// A string, one of the allowed values when any are given (`strcasecmp`).
    Str(&'static [&'static str]),
    /// An `int`, optionally in a range.
    Int(Option<(i64, i64)>),
    Ll,
    Dbl,
    /// A sub-cursor: a fixed count when `min == max != 0`, else a count argument first.
    Sub(i64, i64),
}

struct Def {
    name: &'static str,
    ty: Ty,
    rep: bool,
    pos: bool,
    parsed: bool,
}

fn def(name: &'static str, ty: Ty) -> Def {
    Def { name, ty, rep: false, pos: false, parsed: false }
}

enum Val<'b> {
    Flag,
    Str(&'b Bytes),
    Int(i32),
    Ll(i64),
    Dbl(f64),
    Sub(Ac<'b>),
}

type Cb<'c, 'b> = dyn FnMut(&'static str, Val<'b>, &mut Ac<'b>) + 'c;

/// `parse_single_arg`: the value, then the definition's callback.
fn parse_single<'b>(ac: &mut Ac<'b>, d: &Def, cb: &mut Cb<'_, 'b>) -> Result<(), String> {
    let name = d.name;
    let fail = |m: &str| format!("{name}: {m}");
    let v = match d.ty {
        Ty::Flag => Val::Flag,
        Ty::Str(allowed) => {
            let s = ac.get().map_err(|e| fail(e.strerror()))?;
            if !allowed.is_empty() && !allowed.iter().any(|a| c_eq(s, a)) {
                return Err(fail("Invalid value for argument"));
            }
            Val::Str(s)
        }
        Ty::Int(range) => {
            let v = ac.get_int(0).map_err(|e| fail(e.strerror()))?;
            if let Some((lo, hi)) = range {
                if (v as i64) < lo {
                    return Err(fail("Value below minimum"));
                }
                if v as i64 > hi {
                    return Err(fail("Value above maximum"));
                }
            }
            Val::Int(v)
        }
        Ty::Ll => Val::Ll(ac.get_ll(0).map_err(|e| fail(e.strerror()))?),
        Ty::Dbl => Val::Dbl(ac.get_double(0).map_err(|e| fail(e.strerror()))?),
        Ty::Sub(min, max) => {
            let (count, short) = if max != 0 && min == max {
                (max as u64, "Not enough arguments were provided")
            } else {
                let c = ac.get_u32(0).map_err(|_| fail("Failed to parse the argument count"))?;
                (c as u64, "Not enough arguments were provided based on argument count")
            };
            if (count as i64) < min || (max != -1 && count as i64 > max) {
                return Err(fail("Invalid argument count"));
            }
            Val::Sub(ac.get_slice(count as usize).map_err(|_| fail(short))?)
        }
    };
    cb(name, v, ac);
    Ok(())
}

/// `ArgParser_New` + `ArgParser_Parse`: the error string (`ArgParser_GetErrorString`) on failure.
fn arg_parse<'b>(ac: &mut Ac<'b>, cmd: &str, defs: &mut [Def], cb: &mut Cb<'_, 'b>) -> Result<(), String> {
    if !ac.at_end() {
        ac.advance_if(cmd);
    }
    // The positional pass: only a first argument can be one (COMBINE).
    if let Some(a) = ac.peek()
        && let Some(i) = defs.iter().position(|d| d.pos && c_eq(a, d.name))
    {
        ac.advance().ok();
        parse_single(ac, &defs[i], cb)?;
        defs[i].parsed = true;
    }
    while let Some(a) = ac.peek() {
        let Some(i) = defs.iter().position(|d| c_eq(a, d.name)) else {
            return Err(format!("{}: Unknown argument", qast::lossy(a)));
        };
        if defs[i].pos && defs[i].parsed {
            ac.advance().ok();
            continue;
        }
        if defs[i].parsed && !defs[i].rep {
            return Err(format!("{}: Argument specified multiple times", defs[i].name));
        }
        ac.advance().ok();
        parse_single(ac, &defs[i], cb)?;
        defs[i].parsed = true;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The request

/// `HybridScoringContext`.
#[derive(Clone, Copy, Debug)]
enum Scoring {
    Rrf { constant: f64, window: i64 },
    Linear { alpha: f64, beta: f64, window: i64 },
}

impl Scoring {
    fn window(&self) -> i64 {
        match *self {
            Scoring::Rrf { window, .. } | Scoring::Linear { window, .. } => window,
        }
    }
}

/// `HybridExplainContext`.
struct ExplainCx {
    scorer: String,
    range: bool,
    envelope: String,
}

/// A parsed FT.HYBRID.
struct Hyb {
    search: Request,
    vector: Request,
    tail: Request,
    scoring: Scoring,
    /// The vector distance's key (YIELD_SCORE_AS or `__<field>_score`), hidden without an alias.
    vec_score: Bytes,
    hide: bool,
    dialect: u32,
    explain: Option<ExplainCx>,
    /// `QEXEC_S_MAX_TIMEOUT_CAPPED`.
    capped: bool,
    /// The VSIM range, for the explanation.
    radius: f64,
    epsilon: Option<String>,
    /// WITHCURSOR's settings.
    cursor: Option<CursorConfig>,
    /// `QEXEC_F_IS_CURSOR`: WITHCURSOR was given (the debug wrapper replies without a cursor,
    /// but the query counts as a cursor's).
    is_cursor: bool,
    /// `_FT.DEBUG`'s TIMEOUT_AFTER_N_SEARCH, _VSIM and _TAIL (0: none).
    debug: [u32; 3],
}

const S_LIMIT: u32 = 1;
const S_SORTBY: u32 = 1 << 1;
const S_WITHCURSOR: u32 = 1 << 2;
const S_PARAMS: u32 = 1 << 3;
const S_TIMEOUT: u32 = 1 << 4;
const S_FORMAT: u32 = 1 << 5;
const S_EXPLAINSCORE: u32 = 1 << 6;
const S_COMBINE: u32 = 1 << 7;
const S_GROUPBY: u32 = 1 << 8;
const S_APPLY: u32 = 1 << 9;
const S_LOAD: u32 = 1 << 10;
const S_FILTER: u32 = 1 << 11;

/// `HybridParseContext`: what the optional arguments' callbacks fill.
struct Opts {
    status: Option<QErr>,
    specified: u32,
    flags: u64,
    tail: Request,
    params: Option<qast::Params>,
    score_alias: Option<Bytes>,
    cursor: CursorConfig,
    max_results: u64,
    timeout: i64,
    scoring: Scoring,
    max_idle: u32,
}

impl Opts {
    fn fail(&mut self, e: QErr) {
        if self.status.is_none() {
            self.status = Some(e);
        }
    }

    fn on_arg<'b>(&mut self, name: &'static str, v: Val<'b>, main: &mut Ac<'b>) {
        match (name, v) {
            ("LIMIT", Val::Sub(mut s)) => {
                self.specified |= S_LIMIT;
                if s.remaining() < 2 {
                    return self.fail(QErr::new(Code::ParseArgs, "LIMIT requires 2 arguments"));
                }
                let Ok(offset) = s.get_size(GE0) else {
                    return self.fail(QErr::new(Code::ParseArgs, "LIMIT offset must be a non-negative integer"));
                };
                let Ok(num) = s.get_size(GE0) else {
                    return self.fail(QErr::new(Code::ParseArgs, "LIMIT count must be a non-negative integer"));
                };
                if num == 0 && offset != 0 {
                    return self.fail(QErr::new(Code::Limit, "The `offset` of the LIMIT must be 0 when `num` is 0"));
                }
                if num == 0 {
                    self.flags |= F_NOROWS | F_SEND_NOFIELDS;
                } else if num > self.max_results {
                    return self.fail(QErr::new(Code::Limit, format!("LIMIT exceeds maximum of {}", self.max_results)));
                } else if offset > i64::MAX as u64 - num {
                    return self.fail(QErr::new(Code::ParseArgs, "LIMIT offset + count overflow"));
                }
                let a = self.tail.arrange_or_create();
                a.is_limited = true;
                a.offset = offset;
                a.limit = num;
            }
            ("SORTBY", Val::Sub(mut s)) => {
                self.specified |= S_SORTBY;
                if self.flags & F_NO_SORT != 0 {
                    return self.fail(QErr::new(Code::ParseArgs, "NOSORT is not allowed with SORTBY"));
                }
                self.tail.arrange_or_create();
                while let Some(f) = s.take() {
                    let field = match strict_key_name(f, true, "SORTBY") {
                        Ok(n) => Bytes::copy_from_slice(n),
                        Err(e) => return self.fail(e),
                    };
                    let mut asc = true;
                    if let Some(d) = s.peek() {
                        if c_eq(d, "ASC") {
                            s.advance().ok();
                        } else if c_eq(d, "DESC") {
                            s.advance().ok();
                            asc = false;
                        }
                    }
                    let a = self.tail.arrange_or_create();
                    let keys = a.sort_keys.get_or_insert_with(|| {
                        a.asc_map = u64::MAX;
                        Vec::new()
                    });
                    keys.push(field);
                    let idx = keys.len() - 1;
                    if idx < 64 {
                        if asc {
                            a.asc_map |= 1 << idx;
                        } else {
                            a.asc_map &= !(1 << idx);
                        }
                    }
                }
            }
            ("NOSORT", _) => {
                self.flags |= F_NO_SORT;
                let first = self.tail.steps.iter().find_map(|s| match s {
                    Step::Arrange(a) => Some(a),
                    _ => None,
                });
                if first.is_some_and(|a| a.sort_keys.is_some()) {
                    self.fail(QErr::new(Code::ParseArgs, "NOSORT is not allowed with SORTBY"));
                }
            }
            ("WITHCURSOR", _) => {
                self.flags |= F_IS_CURSOR;
                self.specified |= S_WITHCURSOR;
                // `AC_ParseArgSpec` with MAXIDLE and COUNT.
                while let Some(a) = main.peek() {
                    let exact = |n: &str| a.len() == n.len() && a.eq_ignore_ascii_case(n.as_bytes());
                    let which = if exact("MAXIDLE") {
                        "MAXIDLE"
                    } else if exact("COUNT") {
                        "COUNT"
                    } else {
                        break;
                    };
                    main.advance().ok();
                    match main.get_u32(GE1) {
                        Err(e) => return self.fail(QErr::bad_args(which, e)),
                        Ok(v) if which == "MAXIDLE" => self.cursor.max_idle = v,
                        Ok(v) => self.cursor.chunk_size = v,
                    }
                }
                if self.cursor.max_idle == 0 || self.cursor.max_idle > self.max_idle {
                    self.cursor.max_idle = self.max_idle;
                }
            }
            ("PARAMS", Val::Sub(s)) => {
                self.specified |= S_PARAMS;
                if self.params.is_some() {
                    return self.fail(QErr::new(Code::AddArgs, "Multiple PARAMS are not allowed. Parameters can be defined only once"));
                }
                if s.objs.is_empty() || s.objs.len() % 2 != 0 {
                    return self.fail(QErr::new(Code::AddArgs, "Parameters must be specified in PARAM VALUE pairs"));
                }
                let mut params = qast::Params::new();
                for pair in s.objs.chunks(2) {
                    let name = cstr(&pair[0]).to_vec();
                    if params.contains_key(&name) {
                        return self.fail(QErr::user(Code::AddArgs, "Duplicate parameter", format!(" `{}`", qast::lossy(&name))));
                    }
                    params.insert(name, pair[1].clone());
                }
                self.params = Some(params);
            }
            ("TIMEOUT", Val::Ll(t)) => {
                self.specified |= S_TIMEOUT;
                self.timeout = t;
            }
            ("DIALECT", _) => self.fail(dialect_err()),
            ("FORMAT", Val::Str(f)) => {
                self.specified |= S_FORMAT;
                if c_eq(f, "STRING") {
                    self.flags &= !FORMAT_EXPAND;
                } else if c_eq(f, "EXPAND") {
                    self.flags |= FORMAT_EXPAND;
                }
            }
            ("EXPLAINSCORE", _) => {
                self.flags |= F_SEND_SCOREEXPLAIN;
                self.specified |= S_EXPLAINSCORE;
            }
            ("COMBINE", Val::Str(m)) => self.combine(m, main),
            ("GROUPBY", Val::Sub(mut s)) => {
                self.specified |= S_GROUPBY;
                self.flags |= F_IS_AGGREGATE;
                if s.remaining() == 0 {
                    return self.fail(QErr::new(Code::ParseArgs, "Bad arguments for GROUPBY: Expected at least one property"));
                }
                let mut props = Vec::new();
                while let Some(p) = s.take() {
                    if p.first() != Some(&b'@') {
                        let p = qast::lossy(p);
                        return self.fail(QErr::user(Code::ParseArgs, "Bad arguments for GROUPBY", format!(": Unknown property `{p}`. Did you mean `@{p}`?")));
                    }
                    props.push(p.clone());
                }
                self.tail.steps.push(Step::Group { properties: props, reducers: Vec::new() });
                while main.advance_if("REDUCE") {
                    let name = match main.get() {
                        Ok(n) => n.clone(),
                        Err(e) => return self.fail(QErr::user(Code::ParseArgs, "Bad arguments for REDUCE", format!(": {}", e.strerror()))),
                    };
                    let args = match main.get_varargs() {
                        Ok(a) => a,
                        Err(e) => return self.fail(QErr::bad_args(&qast::lossy(&name), e)),
                    };
                    let mut alias = None;
                    if main.advance_if("AS") {
                        match main.get() {
                            Ok(a) => alias = Some(a.clone()),
                            Err(e) => return self.fail(QErr::user(Code::ParseArgs, "Bad arguments", format!(" for AS: {}", e.strerror()))),
                        }
                    }
                    let alias = alias.unwrap_or_else(|| areq::reducer_alias(&name, args.objs));
                    if let Some(Step::Group { reducers, .. }) = self.tail.steps.last_mut() {
                        reducers.push(areq::Reducer { name, args: args.objs.to_vec(), alias });
                    }
                }
            }
            ("APPLY", Val::Str(e)) => {
                self.specified |= S_APPLY;
                let expr = Bytes::copy_from_slice(cstr(e));
                if main.advance_if("AS") {
                    match main.get() {
                        Ok(a) => self.tail.steps.push(Step::Apply { expr, alias: Bytes::copy_from_slice(cstr(a)) }),
                        Err(_) => self.fail(QErr::new(Code::ParseArgs, "AS needs argument")),
                    }
                } else {
                    self.tail.steps.push(Step::Apply { expr: expr.clone(), alias: expr });
                }
            }
            ("LOAD", Val::Str(first)) => {
                self.specified |= S_LOAD;
                let first = cstr(first);
                let mut args = Vec::new();
                if first == b"*" {
                    self.flags |= AGG_LOAD_ALL;
                } else {
                    let n = match crate::util::strtoll_full(first) {
                        Some(n) if n > 0 => n,
                        _ => return self.fail(QErr::new(Code::ParseArgs, "Bad arguments for LOAD: Expected number of fields or `*`")),
                    };
                    match main.get_slice(n as usize) {
                        Ok(s) => args = s.objs.to_vec(),
                        Err(_) => return self.fail(QErr::new(Code::ParseArgs, "Not enough arguments for LOAD")),
                    }
                }
                let all = self.flags & AGG_LOAD_ALL != 0;
                self.tail.steps.push(Step::Load { args, all });
            }
            ("FILTER", Val::Str(e)) => {
                self.specified |= S_FILTER;
                self.tail.steps.push(Step::Filter { expr: Bytes::copy_from_slice(cstr(e)) });
            }
            _ => {}
        }
    }

    /// `handleCombine`.
    fn combine<'b>(&mut self, method: &Bytes, main: &mut Ac<'b>) {
        self.specified |= S_COMBINE;
        let parsed = if c_eq(method, "LINEAR") { parse_linear(main) } else { parse_rrf(main) };
        let in_block = match parsed {
            Err(e) => return self.fail(e),
            Ok((scoring, alias)) => {
                self.scoring = scoring;
                // The method's parser resets the alias it may set.
                self.score_alias = alias;
                self.score_alias.is_some()
            }
        };
        if main.advance_if("YIELD_SCORE_AS") {
            if in_block {
                return self.fail(QErr::new(Code::ParseArgs, "YIELD_SCORE_AS specified more than once"));
            }
            match main.get() {
                Ok(a) => {
                    self.score_alias = Some(Bytes::copy_from_slice(cstr(a)));
                    self.flags |= F_SEND_SCORES_AS_FIELD;
                }
                Err(_) => self.fail(QErr::new(Code::ParseArgs, "Missing argument value for YIELD_SCORE_AS")),
            }
        } else if in_block {
            self.flags |= F_SEND_SCORES_AS_FIELD;
        }
    }
}

/// `getVarArgsForClause`.
fn clause_args<'b>(ac: &mut Ac<'b>, clause: &str) -> Result<Ac<'b>, QErr> {
    let count = match ac.get_u32(0) {
        Ok(c) => c,
        Err(AcErr::NoArg) => return Err(QErr::new(Code::ParseArgs, format!("Missing {clause} argument count"))),
        Err(e) => return Err(QErr::new(Code::ParseArgs, format!("Invalid {clause} argument count, error: {}", e.strerror()))),
    };
    if count % 2 != 0 {
        return Err(QErr::new(Code::ParseArgs, format!("{clause} expects pairs of key value arguments, argument count must be an even number")));
    }
    let left = ac.remaining();
    ac.get_slice(count as usize)
        .map_err(|_| QErr::user(Code::Syntax, "Not enough arguments", format!(" in {clause}, specified {count} but provided only {left}")))
}

const WINDOW: Ty = Ty::Int(Some((1, i64::MAX)));

/// `parseRRFClause`.
fn parse_rrf(ac: &mut Ac<'_>) -> Result<(Scoring, Option<Bytes>), QErr> {
    let mut s = clause_args(ac, "RRF")?;
    let mut defs = [def("CONSTANT", Ty::Dbl), def("WINDOW", WINDOW), def("YIELD_SCORE_AS", Ty::Str(&[]))];
    let (mut constant, mut window, mut alias) = (60.0, 20i64, None);
    arg_parse(&mut s, "RRF", &mut defs, &mut |n, v, _| match (n, v) {
        ("CONSTANT", Val::Dbl(c)) => constant = c,
        ("WINDOW", Val::Int(w)) => window = w as i64,
        ("YIELD_SCORE_AS", Val::Str(a)) => alias = Some(Bytes::copy_from_slice(cstr(a))),
        _ => {}
    })
    .map_err(|e| QErr::new(Code::ParseArgs, e))?;
    Ok((Scoring::Rrf { constant, window }, alias))
}

/// `parseLinearClause`.
fn parse_linear(ac: &mut Ac<'_>) -> Result<(Scoring, Option<Bytes>), QErr> {
    let mut s = clause_args(ac, "LINEAR")?;
    let mut defs = [def("ALPHA", Ty::Dbl), def("BETA", Ty::Dbl), def("YIELD_SCORE_AS", Ty::Str(&[])), def("WINDOW", WINDOW)];
    let (mut alpha, mut beta, mut window, mut alias) = (None, None, 20i64, None);
    arg_parse(&mut s, "LINEAR", &mut defs, &mut |n, v, _| match (n, v) {
        ("ALPHA", Val::Dbl(a)) => alpha = Some(a),
        ("BETA", Val::Dbl(b)) => beta = Some(b),
        ("WINDOW", Val::Int(w)) => window = w as i64,
        ("YIELD_SCORE_AS", Val::Str(a)) => alias = Some(Bytes::copy_from_slice(cstr(a))),
        _ => {}
    })
    .map_err(|e| QErr::new(Code::ParseArgs, e))?;
    match (alpha, beta) {
        (Some(_), None) => Err(QErr::new(Code::Syntax, "Missing value for BETA")),
        (None, Some(_)) => Err(QErr::new(Code::Syntax, "Missing value for ALPHA")),
        (a, b) => Ok((Scoring::Linear { alpha: a.unwrap_or(0.3), beta: b.unwrap_or(0.7), window }, alias)),
    }
}

/// `parseSearchSubquery`.
fn parse_search(ac: &mut Ac<'_>, s: &mut Request) -> Result<(), QErr> {
    let Some(q) = ac.take() else { return Err(QErr::new(Code::ParseArgs, "No query string provided for SEARCH")) };
    s.query = q.clone();
    while let Some(a) = ac.peek() {
        let exact = |n: &str| a.len() == n.len() && a.eq_ignore_ascii_case(n.as_bytes());
        let which = if exact("SCORER") {
            "SCORER"
        } else if exact("YIELD_SCORE_AS") {
            "YIELD_SCORE_AS"
        } else if c_eq(a, "VSIM") {
            break;
        } else if c_eq(a, "DIALECT") {
            return Err(dialect_err());
        } else {
            return Err(QErr::user(Code::ArgUnrecognized, "Unknown argument", format!(" `{}` in SEARCH", qast::lossy(a))));
        };
        ac.advance().ok();
        let v = Bytes::copy_from_slice(cstr(ac.get().map_err(|e| QErr::bad_args(which, e))?));
        if which == "SCORER" {
            s.opts.scorer = Some(v);
        } else {
            s.score_alias = Some(v);
        }
    }
    if s.score_alias.is_some() {
        s.flags |= F_SEND_SCORES_AS_FIELD;
    }
    Ok(())
}

/// What `parseVectorSubquery` gives besides the request's query.
struct Vsim {
    hv: HybridVec,
    alias: Option<Bytes>,
    epsilon: Option<String>,
}

fn expected_args(count: u32, i: u32) -> QErr {
    let verb = if i == 1 { "was" } else { "were" };
    QErr::user(Code::Syntax, "Expected arguments", format!(" {count}, but {i} {verb} provided"))
}

fn check_end(ac: &Ac<'_>, arg: &str) -> Result<(), QErr> {
    if ac.at_end() {
        return Err(QErr::user(Code::ParseArgs, "Missing argument value for ", arg));
    }
    Ok(())
}

/// The KNN/RANGE clause's argument count.
fn clause_count(ac: &mut Ac<'_>) -> Result<u32, QErr> {
    if ac.at_end() {
        return Err(QErr::new(Code::ParseArgs, "Missing argument count"));
    }
    let n = ac.get_u32(0).map_err(|_| QErr::new(Code::ParseArgs, "Invalid argument count: expected an unsigned integer"))?;
    if n == 0 || n % 2 != 0 {
        return Err(QErr::user(Code::Syntax, "Invalid argument count", format!(": {n} (must be a positive even number for key/value pairs)")));
    }
    Ok(n)
}

fn unknown_in(ac: &Ac<'_>, clause: &str) -> QErr {
    let cur = ac.peek().map(|a| qast::lossy(a)).unwrap_or_default();
    QErr::user(Code::ArgUnrecognized, "Unknown argument", format!(" `{cur}` in {clause}"))
}

/// `parseVectorSubquery`.
fn parse_vector(ac: &mut Ac<'_>, v: &mut Request) -> Result<Vsim, QErr> {
    if !ac.advance_if("VSIM") {
        return Err(QErr::new(Code::Syntax, "VSIM argument is required"));
    }
    let f = ac.get().map_err(|_| QErr::new(Code::Syntax, "Missing vector field name"))?;
    if f.first() != Some(&b'@') {
        return Err(QErr::new(Code::Syntax, "Missing @ prefix for vector field name"));
    }
    let field = Bytes::copy_from_slice(cstr(&f[1..]));
    let p = ac.get().map_err(|_| QErr::new(Code::Syntax, "Missing vector argument"))?;
    if p.first() != Some(&b'$') {
        return Err(QErr::new(Code::Syntax, "Invalid vector argument, expected a parameter name starting with $"));
    }
    let param = Bytes::copy_from_slice(cstr(&p[1..]));
    let mut hv = HybridVec {
        field,
        param,
        knn: true,
        k: 10,
        radius: 0.0,
        params: Vec::new(),
        attrs: Vec::new(),
        hide: false,
        skip_filter: false,
        by_score: true,
    };
    let mut alias: Option<Bytes> = None;
    let mut query: Option<Bytes> = None;
    if !ac.at_end() {
        if ac.advance_if("KNN") {
            let n = clause_count(ac)?;
            let (mut has_k, mut has_ef, mut has_ratio) = (false, false, false);
            let mut i = 0;
            while i < n {
                if ac.at_end() {
                    return Err(expected_args(n, i));
                }
                if ac.advance_if("K") {
                    if has_k {
                        return Err(QErr::new(Code::DupParam, "Duplicate K argument"));
                    }
                    check_end(ac, "K")?;
                    hv.k = ac.get_ll(GE1).map_err(|_| QErr::new(Code::Syntax, "Invalid K value"))? as u64;
                    has_k = true;
                } else if ac.advance_if("EF_RUNTIME") {
                    if has_ef {
                        return Err(QErr::new(Code::DupParam, "Duplicate EF_RUNTIME argument"));
                    }
                    check_end(ac, "EF_RUNTIME")?;
                    ac.clone().get_ll(GE1).map_err(|_| QErr::new(Code::Syntax, "Invalid EF_RUNTIME value"))?;
                    let raw = ac.take().expect("not at the end");
                    hv.params.push((b"EF_RUNTIME".to_vec(), raw.to_vec()));
                    has_ef = true;
                } else if ac.advance_if("SHARD_K_RATIO") {
                    if has_ratio {
                        return Err(QErr::new(Code::DupParam, "Duplicate SHARD_K_RATIO argument"));
                    }
                    let Some(raw) = ac.take() else {
                        return Err(QErr::new(Code::ParseArgs, "Missing argument value for SHARD_K_RATIO"));
                    };
                    qast::shard_k_ratio(cstr(raw))?;
                    hv.attrs.push(qast::Attr { name: b"shard_k_ratio".to_vec(), value: cstr(raw).to_vec(), vallen: raw.len() });
                    has_ratio = true;
                } else {
                    return Err(unknown_in(ac, "KNN"));
                }
                i += 2;
            }
            if !has_k {
                return Err(QErr::new(Code::ParseArgs, "Missing required argument K"));
            }
        } else if ac.advance_if("RANGE") {
            let n = clause_count(ac)?;
            hv.knn = false;
            let (mut has_radius, mut has_eps) = (false, false);
            let mut i = 0;
            while i < n {
                if ac.at_end() {
                    return Err(expected_args(n, i));
                }
                if ac.advance_if("RADIUS") {
                    if has_radius {
                        return Err(QErr::new(Code::DupParam, "Duplicate RADIUS argument"));
                    }
                    check_end(ac, "RADIUS")?;
                    hv.radius = ac.get_double(GE0).map_err(|_| QErr::new(Code::Syntax, "Invalid RADIUS value"))?;
                    has_radius = true;
                } else if ac.advance_if("EPSILON") {
                    if has_eps {
                        return Err(QErr::new(Code::DupParam, "Duplicate EPSILON argument"));
                    }
                    check_end(ac, "EPSILON")?;
                    match ac.clone().get_double(GE0) {
                        Ok(e) if e != 0.0 => {}
                        _ => return Err(QErr::new(Code::Syntax, "Invalid EPSILON value")),
                    }
                    let raw = ac.take().expect("not at the end");
                    hv.params.push((b"EPSILON".to_vec(), raw.to_vec()));
                    has_eps = true;
                } else {
                    return Err(unknown_in(ac, "RANGE"));
                }
                i += 2;
            }
            if !has_radius {
                return Err(QErr::new(Code::ParseArgs, "Missing required argument RADIUS"));
            }
        }
        if ac.advance_if("FILTER") {
            if ac.at_end() {
                return Err(QErr::new(Code::ParseArgs, "Missing argument count for FILTER"));
            }
            match ac.get_size(0) {
                // A filter expression without a count.
                Err(_) => query = ac.take().cloned(),
                Ok(count) => {
                    let left = ac.remaining();
                    let mut s = ac.get_slice(count as usize).map_err(|_| {
                        QErr::user(Code::Syntax, "Not enough arguments", format!(" in FILTER, specified {count} but provided only {left}"))
                    })?;
                    let Some(q) = s.take() else { return Err(QErr::new(Code::ParseArgs, "Missing filter-expression for FILTER")) };
                    query = Some(q.clone());
                    let mut defs = [def("POLICY", Ty::Str(&["ADHOC", "BATCHES"])), def("BATCH_SIZE", Ty::Str(&[]))];
                    let attrs = &mut hv.attrs;
                    arg_parse(&mut s, "FILTER", &mut defs, &mut |n, v, _| {
                        let Val::Str(x) = v else { return };
                        let x = cstr(x);
                        let (name, value): (&[u8], &[u8]) = match n {
                            "POLICY" if x.eq_ignore_ascii_case(b"ADHOC") => (b"HYBRID_POLICY", b"adhoc_bf"),
                            "POLICY" => (b"HYBRID_POLICY", x),
                            _ => (b"BATCH_SIZE", x),
                        };
                        attrs.push(qast::Attr { name: name.to_vec(), value: value.to_vec(), vallen: value.len() });
                    })
                    .map_err(|e| QErr::new(Code::ParseArgs, e))?;
                }
            }
            if !hv.knn {
                hv.by_score = false;
            }
        }
        while !ac.at_end() && ac.advance_if("YIELD_SCORE_AS") {
            if alias.is_some() {
                return Err(QErr::new(Code::DupParam, "Duplicate YIELD_SCORE_AS argument"));
            }
            let Some(a) = ac.take() else {
                return Err(QErr::new(Code::ParseArgs, "Missing argument value for YIELD_SCORE_AS"));
            };
            let a = cstr(a);
            hv.attrs.push(qast::Attr { name: b"yield_distance_as".to_vec(), value: a.to_vec(), vallen: a.len() });
            alias = Some(Bytes::copy_from_slice(a));
        }
        if ac.advance_if("DIALECT") {
            return Err(dialect_err());
        }
    }
    match query {
        Some(q) => v.query = q,
        None => {
            v.query = Bytes::from_static(b"*");
            hv.skip_filter = !hv.knn;
        }
    }
    let epsilon = hv.params.iter().find(|(n, _)| n.as_slice() == b"EPSILON").map(|(_, e)| String::from_utf8_lossy(e).into_owned());
    Ok(Vsim { hv, alias, epsilon })
}

/// `parseHybridCommand` up to the index: the subqueries, the merge and the tail.
fn parse(cfg: &Config, args: &[Bytes]) -> Result<Hyb, QErr> {
    let dialect = (cfg.num("search-default-dialect") as u32).max(2);
    let mut search = Request::hybrid(cfg, F_IS_HYBRID_SEARCH, dialect);
    search.validation = qast::V_NO_VECTOR;
    let mut vector = Request::hybrid(cfg, F_IS_HYBRID_VECTOR, dialect);
    vector.validation = qast::V_NO_WEIGHT | qast::V_NO_VECTOR;
    let mut ac = Ac::new_sds(args);

    // `parseSubqueriesCount` (the count may be left out).
    if ac.at_end() {
        return Err(QErr::new(Code::ParseArgs, "Missing subqueries count for FT.HYBRID"));
    }
    match ac.get_u32(GE1) {
        Ok(n) => {
            if n != 2 {
                return Err(QErr::new(Code::ParseArgs, "FT.HYBRID currently supports only two subqueries"));
            }
            if !ac.advance_if("SEARCH") {
                return Err(QErr::new(Code::Syntax, "SEARCH keyword is required"));
            }
        }
        Err(_) => {
            if !ac.advance_if("SEARCH") {
                return Err(QErr::new(Code::Syntax, "Invalid subqueries count: expected an unsigned integer"));
            }
        }
    }
    parse_search(&mut ac, &mut search)?;
    let Vsim { mut hv, alias, epsilon } = parse_vector(&mut ac, &mut vector)?;

    // `HybridParseOptionalArgs`.
    let max_results = cfg.num("search-max-search-results").max(0) as u64;
    let mut o = Opts {
        status: None,
        specified: 0,
        flags: 0,
        tail: Request::hybrid(cfg, 0, dialect),
        params: None,
        score_alias: None,
        cursor: CursorConfig::default(),
        max_results,
        timeout: cfg.num("search-timeout") as i64,
        scoring: Scoring::Rrf { constant: 60.0, window: 20 },
        max_idle: cfg.num("search-cursor-max-idle").max(0) as u32,
    };
    let mut defs = [
        def("LIMIT", Ty::Sub(2, 2)),
        def("SORTBY", Ty::Sub(1, -1)),
        def("NOSORT", Ty::Flag),
        def("WITHCURSOR", Ty::Flag),
        def("PARAMS", Ty::Sub(1, -1)),
        def("TIMEOUT", Ty::Ll),
        def("DIALECT", Ty::Int(Some((1, 4)))),
        def("FORMAT", Ty::Str(&["STRING", "EXPAND"])),
        def("EXPLAINSCORE", Ty::Flag),
        Def { pos: true, ..def("COMBINE", Ty::Str(&["RRF", "LINEAR"])) },
        def("GROUPBY", Ty::Sub(0, 65535)),
        Def { rep: true, ..def("APPLY", Ty::Str(&[])) },
        Def { rep: true, ..def("LOAD", Ty::Str(&[])) },
        def("FILTER", Ty::Str(&[])),
    ];
    let parsed = arg_parse(&mut ac, "HybridOptionalArgs", &mut defs, &mut |n, v, main| o.on_arg(n, v, main));
    if let Some(e) = o.status.take() {
        return Err(e);
    }
    parsed.map_err(|e| QErr::new(Code::ParseArgs, e))?;
    if o.flags & F_SEND_SCOREEXPLAIN != 0 {
        o.flags |= F_SEND_SCORES;
    }
    if o.specified & S_EXPLAINSCORE != 0 && o.specified & S_GROUPBY != 0 {
        return Err(QErr::new(Code::ParseArgs, "EXPLAINSCORE is not supported with GROUPBY"));
    }
    let cursor = (o.flags & F_IS_CURSOR != 0).then_some(o.cursor);

    // The vector distance's key, normalized into the vector score.
    let hide = alias.is_none();
    let vec_score = alias.unwrap_or_else(|| {
        let mut s = b"__".to_vec();
        s.extend_from_slice(&hv.field);
        s.extend_from_slice(b"_score");
        Bytes::from(s)
    });
    hv.hide = hide;
    vector.steps.push(Step::Normalize { field: hv.field.clone(), alias: vec_score.clone() });

    // `copyHybridConfigToSubquery`.
    for (i, r) in [&mut search, &mut vector].into_iter().enumerate() {
        r.opts.params = o.params.clone();
        r.flags |= o.flags & F_SEND_SCORES;
        if i == 0 {
            r.flags |= o.flags & F_SEND_SCOREEXPLAIN;
        }
        r.timeout = o.timeout;
        r.max_search_results = max_results;
        r.max_aggregate_results = max_results;
        r.strict_prefix = true;
    }
    let window = o.scoring.window();
    search.arrange_or_create().limit = window as u64;
    // `handleLoadStepForHybridPipelines`: the tail's LOADs run in both subqueries.
    let mut loads = Vec::new();
    o.tail.steps.retain(|s| {
        if matches!(s, Step::Load { .. }) {
            loads.push(s.clone());
            false
        } else {
            true
        }
    });
    if loads.is_empty() {
        loads.push(Step::Load { args: vec![Bytes::from_static(b"@__key"), Bytes::from_static(b"@__score")], all: false });
    }
    search.steps.extend(loads.iter().cloned());
    vector.steps.extend(loads);
    if o.flags & F_NO_SORT == 0 {
        o.tail.arrange_or_create();
    }
    if hv.knn && hv.k > window as u64 {
        hv.k = window as u64;
    }
    let radius = hv.radius;
    vector.hybrid_vec = Some(Box::new(hv));

    // `RSConfig_CapQueryTimeoutToForegroundLimit`.
    let limit = cfg.num("search-_max-foreground-timeout-limit") as i64;
    let capped = limit > 0 && cfg.num("search-workers") == 0 && (o.timeout <= 0 || o.timeout > limit);

    let mut tail = o.tail;
    tail.flags = o.flags | F_IS_HYBRID_TAIL;
    tail.score_alias = o.score_alias;
    tail.max_search_results = max_results;
    tail.max_aggregate_results = max_results;
    tail.strict_prefix = true;
    tail.timeout = o.timeout;
    Ok(Hyb { search, vector, tail, scoring: o.scoring, vec_score, hide, dialect, explain: None, capped, radius, epsilon, is_cursor: cursor.is_some(), cursor, debug: [0; 3] })
}

/// `AREQ_ApplyContext` for both subqueries (`AddValidationErrorContext`).
fn apply(h: &mut Hyb, ix: &mut Index, resp3: bool, cfg: &Config) -> Result<(), QErr> {
    h.search.apply_context(ix, resp3).map_err(|e| match e.code {
        Code::VectorNotAllowed => QErr::new(Code::VectorNotAllowed, "Vector expressions are not allowed in FT.HYBRID SEARCH"),
        _ => e,
    })?;
    h.vector.apply_context(ix, resp3).map_err(|e| match e.code {
        Code::VectorNotAllowed => QErr::new(Code::VectorNotAllowed, "Vector expressions are not allowed in FT.HYBRID VSIM FILTER"),
        Code::WeightNotAllowed => QErr::new(Code::WeightNotAllowed, "Weight attributes are not allowed in FT.HYBRID VSIM FILTER"),
        _ => e,
    })?;
    if h.tail.flags & F_SEND_SCOREEXPLAIN != 0 {
        let scorer = match &h.search.opts.scorer {
            Some(s) => String::from_utf8_lossy(cstr(s)).into_owned(),
            None => {
                let s = cfg.text("search-default-scorer:value");
                if s.is_empty() { "BM25STD".to_string() } else { s }
            }
        };
        let range = h.vector.hybrid_vec.as_ref().is_some_and(|v| !v.knn);
        let envelope = if !range {
            "vector branch (KNN)".to_string()
        } else {
            match &h.epsilon {
                Some(e) => format!("vector branch (RANGE: radius={:.4}, epsilon={e})", h.radius),
                None => format!("vector branch (RANGE: radius={:.4})", h.radius),
            }
        };
        h.explain = Some(ExplainCx { scorer, range, envelope });
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Processors

/// `RPVectorNormalizer`: the vector distance as a score in [0, 1].
pub(super) struct VecNorm {
    pub(super) metric: Metric,
    pub(super) key: Option<KRef>,
}

impl<'a> Proc<'a> for VecNorm {
    fn name(&self) -> String {
        "Vector Normalizer".into()
    }

    fn next(&mut self, up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        let st = pull(up, cx, r);
        if st != St::Ok {
            return st;
        }
        let d = self.key.and_then(|k| rget(&r.row, k)).and_then(|v| v.to_number());
        let s = match d {
            None => 0.0,
            Some(d) => match self.metric {
                Metric::L2 => 1.0 / (1.0 + d),
                Metric::Ip => (1.0 + d) / 2.0,
                Metric::Cosine => (1.0 + (1.0 - d)) / 2.0,
            },
        };
        r.score = s;
        if let Some(k) = self.key {
            rset(&mut r.row, k, RsValue::Number(s));
        }
        St::Ok
    }
}

/// `RPSafeDepleter`: the background depleter.
struct SafeDepleter<'a>(Depleter<'a>);

impl<'a> Proc<'a> for SafeDepleter<'a> {
    fn name(&self) -> String {
        "Threadsafe-Depleter".into()
    }

    fn drained(&self) -> bool {
        self.0.done
    }

    fn next(&mut self, up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        Proc::next(&mut self.0, up, cx, r)
    }
}

/// A subquery's pipeline.
struct Sub<'a> {
    chain: Vec<Box<dyn Proc<'a> + 'a>>,
    cx: Pcx<'a>,
    /// Its last status (`subqueriesReturnCodes`).
    rc: St,
    expansions: bool,
    clock: Instant,
    parse_ns: u64,
    build_ns: u64,
}

type Merged<'a> = [Option<Res<'a>>; 2];

/// `RPHybridMerger`: up to a window of results from each subquery, merged by document key.
struct Merger<'a> {
    subs: Rc<RefCell<Vec<Sub<'a>>>>,
    scoring: Scoring,
    load_all: bool,
    score_key: Option<KRef>,
    explain: Option<ExplainCx>,
    entries: Option<std::vec::IntoIter<(Bytes, Merged<'a>)>>,
}

impl<'a> Merger<'a> {
    /// `RPHybridMerger_Accum`: `Some` status when it fails.
    fn accumulate(&mut self, cx: &mut Pcx<'a>) -> Option<St> {
        let window = self.scoring.window().max(0) as usize;
        let rrf = matches!(self.scoring, Scoring::Rrf { .. });
        let mut dict: RsDict<Merged<'a>> = RsDict::new();
        dict.expand(window.saturating_mul(2).min(1 << 20));
        let mut failed = false;
        let mut subs = self.subs.borrow_mut();
        for (i, sub) in subs.iter_mut().enumerate() {
            let mut consumed = 0usize;
            let mut rc = St::Ok;
            while consumed < window {
                let mut x = Res::new();
                rc = pull(&mut sub.chain, &mut sub.cx, &mut x);
                if rc != St::Ok {
                    break;
                }
                consumed += 1;
                let score = if rrf { consumed as f64 } else { x.score };
                // `RLookupRow_WriteFieldsFrom`: the row by the tail's keys.
                let mut row = Row { dyn_vals: Vec::new(), sv: &[] };
                for k in sub.cx.lookups[0].keys.iter().filter(|k| !k.dead) {
                    let Some(v) = rget(&x.row, KRef::of(k)) else { continue };
                    let tail = &mut cx.lookups[0];
                    let ti = match tail.find(&k.name) {
                        Some(t) => t,
                        None if self.load_all => match tail.get_write(&k.name, k.flags & !fq::K_TRANSIENT) {
                            Some(t) => t,
                            None => continue,
                        },
                        None => continue,
                    };
                    rset(&mut row, KRef::of(&tail.keys[ti]), v);
                }
                x.row = row;
                let Some(dmd) = x.dmd else {
                    consumed -= 1;
                    continue;
                };
                let key = Bytes::copy_from_slice(cstr(&dmd.key));
                x.score = score;
                let mut slot = Some(x);
                let found = match dict.find_mut(&key) {
                    Some(e) => {
                        e[i] = slot.take();
                        true
                    }
                    None => false,
                };
                if !found {
                    let mut e: Merged<'a> = [None, None];
                    e[i] = slot.take();
                    dict.add(key, e);
                }
            }
            sub.rc = rc;
            failed |= rc == St::Error;
        }
        let timed_out = subs.iter().any(|s| s.rc == St::TimedOut);
        drop(subs);
        if failed {
            return Some(St::Error);
        }
        // ON_TIMEOUT FAIL: a timed-out subquery yields nothing.
        if timed_out && cx.policy == Policy::Fail {
            return Some(St::TimedOut);
        }
        cx.total = dict.len() as i64;
        cx.skipped = 0;
        self.entries = Some(dict.into_entries().into_iter());
        None
    }
}

impl<'a> Proc<'a> for Merger<'a> {
    fn name(&self) -> String {
        "Hybrid Merger".into()
    }

    fn drained(&self) -> bool {
        self.entries.is_some()
    }

    fn next(&mut self, _up: &mut Chain<'a>, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        // Behind the tail's `RPTimeoutAfterCount`.
        cx.tmo.begin();
        let st = self.merge(cx, r);
        cx.tmo.end(st == St::TimedOut);
        st
    }
}

impl<'a> Merger<'a> {
    /// `RPHybridMerger_Accum`, then `RPHybridMerger_Yield` (which checks the clock every 100
    /// results; a timed-out subquery's timeout comes last).
    fn merge(&mut self, cx: &mut Pcx<'a>, r: &mut Res<'a>) -> St {
        if self.entries.is_none()
            && let Some(st) = self.accumulate(cx)
        {
            return st;
        }
        let Some((_, mut e)) = self.entries.as_mut().and_then(|it| it.next()) else {
            return if self.subs.borrow().iter().any(|s| s.rc == St::TimedOut) { St::TimedOut } else { St::Eof };
        };
        if cx.tmo.tick() {
            return St::TimedOut;
        }
        let has = [e[0].is_some(), e[1].is_some()];
        let v = [e[0].as_ref().map_or(0.0, |x| x.score), e[1].as_ref().map_or(0.0, |x| x.score)];
        let score = match self.scoring {
            Scoring::Rrf { constant, .. } => {
                let mut s = 0.0;
                for i in 0..2 {
                    if has[i] {
                        s += 1.0 / (constant + v[i]);
                    }
                }
                s
            }
            Scoring::Linear { alpha, beta, .. } => {
                let w = [alpha, beta];
                let mut s = 0.0f64;
                for i in 0..2 {
                    if has[i] {
                        s = w[i].mac(v[i], s);
                    }
                }
                s
            }
        };
        let expl = self.explain.as_ref().map(|ec| {
            let text = e[0].as_mut().and_then(|x| x.expl.take());
            explain(ec, self.scoring, has, v, score, text)
        });
        let pi = if has[0] { 0 } else { 1 };
        let mut p = e[pi].take().expect("a source");
        p.score = score;
        if expl.is_some() {
            p.expl = expl;
        }
        // `mergeRLookupRowsFromSourcesIntoTarget`.
        for o in e.iter_mut().filter_map(Option::take) {
            for (idx, val) in o.row.dyn_vals.into_iter().enumerate() {
                if let Some(val) = val {
                    if p.row.dyn_vals.len() <= idx {
                        p.row.dyn_vals.resize(idx + 1, None);
                    }
                    p.row.dyn_vals[idx] = Some(val);
                }
            }
        }
        if let Some(k) = self.score_key {
            rset(&mut p.row, k, RsValue::Number(score));
        }
        *r = p;
        St::Ok
    }
}

fn leaf(s: String) -> Expl {
    Expl { s, children: Vec::new() }
}

/// `buildHybridScoreExplain`.
fn explain(ec: &ExplainCx, scoring: Scoring, has: [bool; 2], v: [f64; 2], score: f64, text: Option<Expl>) -> Expl {
    let label = ["text", "vector"];
    let mut line = String::from("final score: ");
    for i in 0..2 {
        if i > 0 {
            line.push_str(" + ");
        }
        if !has[i] {
            line.push_str(&format!("0 [{}: no match]", label[i]));
            continue;
        }
        match scoring {
            Scoring::Rrf { constant, .. } => line.push_str(&format!("1 / (constant {constant:.2} + rank {:.0})", v[i])),
            Scoring::Linear { alpha, beta, .. } => line.push_str(&format!("{:.4} * {:.4}", [alpha, beta][i], v[i])),
        }
    }
    line.push_str(&format!(" = {}", fq::fmt_17g(score)));
    let (rrf, node) = match scoring {
        Scoring::Rrf { constant, window } => (true, format!("Hybrid score (RRF: window={window}, constant={constant:.2})")),
        Scoring::Linear { alpha, beta, window } => (false, format!("Hybrid score (LINEAR: alpha={alpha:.4}, beta={beta:.4}, window={window})")),
    };
    let (alpha, beta) = match scoring {
        Scoring::Linear { alpha, beta, .. } => (alpha, beta),
        Scoring::Rrf { .. } => (0.0, 0.0),
    };
    let text_branch = if !has[0] {
        leaf(if rrf { "text rank = <no match>" } else { "text contribution = <no match>" }.into())
    } else {
        let s = if rrf { format!("text rank = {:.0}", v[0]) } else { format!("text contribution = {alpha:.4} * {:.4} = {:.4}", v[0], alpha * v[0]) };
        let mut scorer = leaf(format!("Text scorer: {}", ec.scorer));
        if !rrf {
            scorer.children.push(leaf(format!("normalized text score = {:.4}", v[0])));
        }
        scorer.children.extend(text);
        Expl { s, children: vec![scorer] }
    };
    let mut vector_branch = leaf(ec.envelope.clone());
    if ec.range {
        vector_branch.children.push(leaf(format!("matched within radius = {}", has[1])));
    }
    if rrf {
        vector_branch.children.push(leaf(if has[1] { format!("vector rank = {:.0}", v[1]) } else { "vector rank = <no match>".into() }));
    } else if has[1] {
        vector_branch.children.push(leaf(format!("vector contribution = {beta:.4} * {:.4} = {:.4}", v[1], beta * v[1])));
        vector_branch.children.push(leaf(format!("normalized vector score = {:.4}", v[1])));
    } else {
        vector_branch.children.push(leaf("vector contribution = <no match>".into()));
    }
    Expl { s: line, children: vec![Expl { s: node, children: vec![text_branch, vector_branch] }] }
}

// ---------------------------------------------------------------------------------------------
// Execution

/// FT.PROFILE HYBRID's options and clock.
struct HProf {
    limited: bool,
    clock: Instant,
    parse_ns: u64,
}

fn ms(ns: u64) -> Reply {
    Reply::Double(ns as f64 / 1_000_000.0)
}

fn kv(k: &str, v: Reply) -> (Reply, Reply) {
    (Reply::Status(k.into()), v)
}

/// The processors' profile (`printProfileRP`).
fn rp_profile(chain: &Chain<'_>, counts: &[(u64, u64)]) -> Reply {
    let mut out = Vec::new();
    let mut upstream = 0.0;
    for (i, rp) in chain.iter().enumerate() {
        let (n, ns) = counts.get(i).copied().unwrap_or((0, 0));
        let total = ns as f64 / 1_000_000.0;
        let mut m = vec![kv("Type", Reply::Status(rp.name()))];
        if rp.gil() {
            m.push(kv("GIL-Time", Reply::Double(0.0)));
        }
        m.push(kv("Time", Reply::Double(total - upstream)));
        m.push(kv("Results processed", Reply::Int(n as i64 - 1)));
        out.push(Reply::Map(m));
        upstream = total;
    }
    Reply::Array(out)
}

/// The command's reply (`Ok(Err)`: an error reply after the pipeline was built), or the error
/// building it.
#[allow(clippy::too_many_arguments)]
fn run_hybrid(eng: &Engine, ix: &Index, h: &Hyb, db: usize, resp3: bool, background: bool, clock: Instant, prof: Option<&mut HProf>) -> Result<Result<Reply, Error>, Error> {
    let spec = &ix.spec;
    let now = crate::util::now_ms();
    let mut ft_to_field = Vec::new();
    for (i, f) in spec.fields.iter().enumerate() {
        if let Some(id) = f.ft_id {
            if ft_to_field.len() <= id {
                ft_to_field.resize(id + 1, 0);
            }
            ft_to_field[id] = i as u16;
        }
    }
    let q = Qcx::new(&ix.docs, now, ft_to_field);
    let profile = prof.is_some();
    let subs = build_subs(eng, ix, &q, h, db, background, profile, now)?;
    run_tail(eng, ix, h, db, resp3, background, clock, prof, subs)
}

/// `HybridRequest_BuildDepletionPipeline`: each subquery's pipeline, ending in its depleter.
#[allow(clippy::too_many_arguments)]
fn build_subs<'a>(eng: &'a Engine, ix: &'a Index, q: &'a Qcx<'a>, h: &'a Hyb, db: usize, background: bool, profile: bool, now: i64) -> Result<Vec<Sub<'a>>, Error> {
    let cfg = &eng.cfg;
    let spec = &ix.spec;
    let avg = if ix.stats.num_documents != 0 { ix.stats.total_docs_len as f64 / ix.stats.num_documents as f64 } else { 0.0 };
    let mut subs = Vec::new();
    for (i, req) in [&h.search, &h.vector].into_iter().enumerate() {
        let sub_clock = Instant::now();
        let mut ecx = EvalCx {
            ix,
            q,
            req_mask: req.opts.field_mask,
            slop: req.opts.slop as i64,
            in_order: req.opts.flags & qast::S_IN_ORDER != 0,
            dialect: req.dialect,
            max_expansions: cfg.num("search-max-prefix-expansions").max(0) as usize,
            min_prefix: cfg.num("search-min-prefix").max(0) as usize,
            min_heap: cfg.num("search-union-iterator-heap").max(0) as usize,
            prioritize: cfg.flag("search-_prioritize-intersect-union-children"),
            in_not: false,
            expansions_warning: false,
            top: None,
            token_id: std::cell::Cell::new(0),
            can_skip_rich: false,
            metric_requests: Vec::new(),
            hybrid: true,
            memo: None,
        };
        let policy = Policy::of(req.on_timeout);
        let tmo = Tmo::new(req.timeout, policy, background, (h.debug[i] > 0).then_some(h.debug[i]));
        q.clock.deadline.set(tmo.iter_deadline());
        let root = req.root.as_ref().expect("a parsed query");
        let it = ecx.iterate(root)?;
        let it = if profile { iters::profiled(it) } else { it };
        let parse_ns = sub_clock.elapsed().as_nanos() as u64;
        let sargs = scorers::Args { avg_doc_len: avg, tanh_factor: req.bm25std_tanh as i64, payload: req.payload.as_deref() };
        let mut cx = Pcx {
            eng,
            db,
            spec,
            lookups: Vec::new(),
            total: 0,
            skipped: 0,
            result_limit: req.max_aggregate_results.min(u32::MAX as u64) as u32,
            min_score: 0.0,
            err: None,
            force: spec.is_json() && req.dialect < 3,
            dialect: req.dialect,
            prof: None,
            policy,
            tmo,
            iclock: Some(&q.clock),
        };
        let metrics = std::mem::take(&mut ecx.metric_requests);
        let Builder { mut chain, .. } = build(req, &mut cx, it, ix, now, background, sargs, &metrics)?;
        if i == 1
            && h.hide
            && let Some(k) = cx.lookups[0].find(&h.vec_score)
        {
            cx.lookups[0].keys[k].flags |= K_HIDDEN;
        }
        let depleter = Depleter { buf: VecDeque::new(), last: St::Eof, done: false };
        if background {
            chain.push(Box::new(SafeDepleter(depleter)));
        } else {
            chain.push(Box::new(depleter));
        }
        if profile {
            cx.prof = Some(vec![(0, 0); chain.len()]);
        }
        let build_ns = (sub_clock.elapsed().as_nanos() as u64).saturating_sub(parse_ns);
        subs.push(Sub { chain, cx, rc: St::Ok, expansions: ecx.expansions_warning, clock: sub_clock, parse_ns, build_ns });
    }
    Ok(subs)
}

/// `HybridRequest_BuildMergePipeline` and `sendChunk_hybrid`.
#[allow(clippy::too_many_arguments)]
fn run_tail<'a>(
    eng: &'a Engine,
    ix: &'a Index,
    h: &'a Hyb,
    db: usize,
    resp3: bool,
    background: bool,
    clock: Instant,
    prof: Option<&mut HProf>,
    subs: Vec<Sub<'a>>,
) -> Result<Result<Reply, Error>, Error> {
    let cfg = &eng.cfg;
    let spec = &ix.spec;
    let profile = prof.is_some();

    // `HybridRequest_BuildMergePipeline`.
    let tail_clock = Instant::now();
    let tail_req = &h.tail;
    let load_all = tail_req.flags & AGG_LOAD_ALL != 0;
    let policy = Policy::of(tail_req.on_timeout);
    let mut cx = Pcx {
        eng,
        db,
        spec,
        lookups: vec![Lookup::default()],
        total: 0,
        skipped: 0,
        result_limit: u32::MAX,
        min_score: 0.0,
        err: None,
        // The tail's search context has no API version (dialect).
        force: spec.is_json(),
        dialect: 1,
        prof: None,
        policy,
        tmo: Tmo::new(tail_req.timeout, policy, background, (h.debug[2] > 0).then_some(h.debug[2])),
        iclock: None,
    };
    if !load_all {
        // `HybridRequest_SynchronizeLookupKeys`.
        for s in &subs {
            for k in s.cx.lookups[0].keys.iter().filter(|k| !k.dead) {
                cx.lookups[0].get_write(&k.name, k.flags & !fq::K_TRANSIENT);
            }
        }
    }
    let score_key = match &tail_req.score_alias {
        Some(a) => match cx.lookups[0].get_write(cstr(a), 0) {
            Some(i) => Some(KRef::of(&cx.lookups[0].keys[i])),
            None => {
                return Err(QErr::user(Code::DupField, "Could not create score alias, name already exists in query", format!(", score alias: {}", qast::lossy(a))).into());
            }
        },
        None => cx.lookups[0].get_read(Some(spec), b"__score", 0).map(|i| KRef::of(&cx.lookups[0].keys[i])),
    };
    let subs = Rc::new(RefCell::new(subs));
    let merger = Merger {
        subs: subs.clone(),
        scoring: h.scoring,
        load_all,
        score_key,
        explain: h.explain.as_ref().map(|e| ExplainCx { scorer: e.scorer.clone(), range: e.range, envelope: e.envelope.clone() }),
        entries: None,
    };
    let max_groups = cfg.num("search-max-aggregate-groups").max(1) as usize;
    let mut b = Builder { req: tail_req, chain: vec![Box::new(merger)], opt: Opt { ty: OptType::Undecided, scorer: false }, safe: false, max_groups, sortkey: None };
    b.aggregation(&mut cx)?;
    let mut chain = b.chain;
    if profile {
        cx.prof = Some(vec![(0, 0); chain.len()]);
    }
    let tail_build_ns = tail_clock.elapsed().as_nanos() as u64;

    // `sendChunk_hybrid`.
    let collect = policy != Policy::Return || cfg.num("search-on-oom") == 1;
    let mut rows: Vec<Res<'_>> = Vec::new();
    let mut rc;
    let mut r = Res::new();
    if collect {
        loop {
            let mut x = Res::new();
            rc = pull(&mut chain, &mut cx, &mut x);
            if rc != St::Ok {
                break;
            }
            rows.push(x);
        }
        if cx.tmo.checks() && cx.tmo.passed() {
            rc = St::TimedOut;
        }
    } else {
        rc = pull(&mut chain, &mut cx, &mut r);
    }
    let total = reported(cx.total - cx.skipped);
    // `finishSendChunk_HREQ` counts the query unless an error other than a timeout was
    // replied; `handleSendChunkError_hybrid` the error, on the coordinator's side.
    let counted = Counted { cursor: h.is_cursor, ..Counted::default() };
    let chunk_err = |e: QErr| {
        if e.code == Code::TimedOut {
            eng.search.stats.count_query(counted, clock.elapsed());
        }
        Ok(Err(count_error(eng, e.into(), Some(true))))
    };
    // `HybridRequest_GetError`: the tail's error unless it is a missing value, else a subquery's.
    if let Some(e) = cx.err.as_ref().filter(|e| e.code != Code::NoPropVal) {
        return chunk_err(e.clone());
    }
    if let Some(e) = subs.borrow().iter().find_map(|s| s.cx.err.clone()) {
        return chunk_err(e);
    }
    // `handleSendChunkError_hybrid`: ON_TIMEOUT FAIL's timeout (FT.PROFILE warns instead).
    if rc == St::TimedOut && policy == Policy::Fail && prof.is_none() {
        return chunk_err(QErr::code(Code::TimedOut));
    }
    if !collect && rc == St::Ok {
        rows.push(std::mem::replace(&mut r, Res::new()));
        loop {
            let mut x = Res::new();
            rc = pull(&mut chain, &mut cx, &mut x);
            if rc != St::Ok {
                break;
            }
            rows.push(x);
        }
    }

    // `serializeResult_hybrid`.
    let flags = tail_req.flags;
    let expand = flags & FORMAT_EXPAND != 0;
    let li = cx.lookups.len() - 1;
    let rule = &spec.rule;
    let special = |n: &[u8]| [&rule.lang_field, &rule.score_field, &rule.payload_field].iter().any(|f| f.as_deref().is_some_and(|f| f.as_bytes() == n));
    let mut results = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut m = Vec::new();
        if flags & F_SEND_SCORES != 0 {
            let s = if flags & F_SEND_SCOREEXPLAIN != 0 {
                Reply::Array(vec![Reply::Double(row.score), row.expl.as_ref().map_or(Reply::Nil, fq::explain_reply)])
            } else {
                Reply::Double(row.score)
            };
            m.push(kv("score", s));
        }
        if flags & F_SEND_NOFIELDS == 0 {
            for k in &cx.lookups[li].keys {
                if k.dead || k.flags & K_HIDDEN != 0 || special(&k.name) {
                    continue;
                }
                let Some(mut v) = rget(&row.row, KRef::of(k)) else { continue };
                if let RsValue::Trio(t) = &v {
                    v = if expand { t[2].clone() } else { t[0].clone() };
                }
                m.push((Reply::Bulk(k.name.clone()), v.reply(resp3, false, expand)));
            }
        }
        results.push(Reply::Map(m));
    }
    drop(rows);

    // `finishSendChunkReply_hybrid`.
    let mut warnings = Vec::new();
    if h.capped {
        warnings.push(Reply::Status(
            "Query TIMEOUT exceeded the configured maximum (search-_max-foreground-timeout-limit) while search-workers is disabled; effective timeout was capped".into(),
        ));
    }
    // `replyWarningsWithSuffixes`: a subquery's timeout hides the tail's.
    let mut sub_timed_out = false;
    let mut expansions = false;
    for (i, s) in subs.borrow().iter().enumerate() {
        let suffix = if i == 0 { "(SEARCH)" } else { "(VSIM)" };
        if s.rc == St::TimedOut {
            warnings.push(Reply::Status(format!("{TIMEOUT_WARNING} {suffix}")));
            sub_timed_out = true;
        } else if s.rc == St::Error {
            if let Some(e) = &s.cx.err {
                warnings.push(Reply::Status(format!("{} {suffix}", e.text())));
            }
        } else if s.expansions {
            warnings.push(Reply::Status(format!("Max prefix expansions limit was reached {suffix}")));
            expansions = true;
        }
    }
    // `updateHybridWarningMetrics`: each kind once, on the coordinator's side; then the query
    // (`finishSendChunk_HREQ`).
    if sub_timed_out || rc == St::TimedOut {
        eng.search.stats.warning(QueryWarning::Timeout, true);
    }
    if expansions {
        eng.search.stats.warning(QueryWarning::MaxPrefixExpansions, true);
    }
    eng.search.stats.count_query(counted, clock.elapsed());
    if rc == St::TimedOut && !sub_timed_out {
        warnings.push(Reply::Status(format!("{TIMEOUT_WARNING} (POST PROCESSING)")));
    } else if rc == St::Error
        && let Some(e) = &cx.err
    {
        warnings.push(Reply::Status(format!("{} (POST PROCESSING)", e.text())));
    }
    let mut pairs = vec![
        kv("total_results", Reply::Int(total)),
        kv("results", Reply::Array(results)),
        kv("warnings", Reply::Array(warnings)),
        kv("execution_time", ms(clock.elapsed().as_nanos() as u64)),
    ];
    let Some(p) = prof else { return Ok(Ok(Reply::Map(pairs))) };

    // `printHybridProfile`.
    let pc = PCtx { limited: p.limited, clock: true };
    let mut shard = Vec::new();
    for (i, s) in subs.borrow_mut().iter_mut().enumerate() {
        let mut m = vec![
            kv("Total profile time", ms(s.clock.elapsed().as_nanos() as u64)),
            kv("Parsing time", ms(s.parse_ns)),
            kv("Workers queue time", ms(0)),
            kv("Pipeline creation time", ms(s.build_ns)),
        ];
        if background {
            m.push(kv("Total GIL time", ms(0)));
        }
        // A subquery's profile warnings are added only where a chunk is replied
        // (`ProfileWarnings_Add` in `sendChunk`), which its depleter never is.
        m.push(kv("Warning", Reply::Array(vec![Reply::Status("None".into())])));
        if let Some(it) = s.chain.first().and_then(|q| q.profile_iters(&pc)) {
            m.push(kv("Iterators profile", Reply::Map(it)));
        }
        let counts = s.cx.prof.take().unwrap_or_default();
        m.push(kv("Result processors profile", rp_profile(&s.chain, &counts)));
        shard.push(kv(if i == 0 { "SEARCH" } else { "VSIM" }, Reply::Map(m)));
    }
    let mut coord = vec![
        kv("Total profile time", ms(p.clock.elapsed().as_nanos() as u64)),
        kv("Parsing time", ms(p.parse_ns)),
        kv("Workers queue time", ms(0)),
        kv("Pipeline creation time", ms(tail_build_ns)),
    ];
    if background {
        coord.push(kv("Total GIL time", ms(0)));
    }
    coord.push(kv("Warning", Reply::Array(vec![Reply::Status("None".into())])));
    let counts = cx.prof.take().unwrap_or_default();
    coord.push(kv("Result processors profile", rp_profile(&chain, &counts)));
    let profile = Reply::Map(vec![kv("Shards", Reply::Array(vec![Reply::Map(shard)])), kv("Coordinator", Reply::Map(coord))]);
    if resp3 {
        pairs.push(kv("Profile", profile));
        return Ok(Ok(Reply::Map(pairs)));
    }
    let mut flat: Vec<Reply> = pairs.into_iter().flat_map(|(k, v)| [k, v]).collect();
    flat.push(profile);
    Ok(Ok(Reply::Array(flat)))
}

/// `_FT.DEBUG`'s FT.HYBRID: `User` (`DEBUG_hybridCommandHandler`) holds the arguments with the
/// debug parameters, parsed once the pipeline is built, and runs the query without a cursor;
/// `Internal` (`_FT.HYBRID`) has them parsed already.
#[derive(Clone, Copy)]
enum Dbg<'p> {
    None,
    User(&'p [Bytes]),
    Internal([u32; 3]),
}

/// `hybridCommandHandler`.
fn hybrid(cx: &mut Cx, index: &Bytes, args: &[Bytes], mut prof: Option<HProf>, dbg: Dbg<'_>) -> CmdResult {
    // `hybridCommandHandler`'s memory guard (`DEBUG_hybridCommandHandler` has none).
    if !matches!(dbg, Dbg::User(_))
        && let Some(o) = super::oom::guard(cx)
    {
        return super::oom::hybrid(cx, o, matches!(dbg, Dbg::Internal(_)), prof.is_some());
    }
    let clock = prof.as_ref().map_or_else(Instant::now, |p| p.clock);
    let Some(name) = cx.eng.search.load(cstr(index), false) else {
        return Err(QErr::user(Code::NoIndex, "Index not found", format!(": {}", qast::lossy(index))).into());
    };
    // `MakeDefaultHybridRequest`: each subquery opens its own search context on the index.
    if let Some(ix) = cx.eng.search.indexes.get_mut(&name) {
        ix.counter += 2;
    }
    let (args, params) = match dbg {
        Dbg::User(argv) => {
            let (c, p) = debug::split(argv)?;
            if c.len() < 2 {
                return Err(arity_error("_FT.DEBUG|FT.HYBRID"));
            }
            (&c[2..], Some(p))
        }
        _ => (args, None),
    };
    if prof.is_none() && matches!(dbg, Dbg::None) {
        verify_acl(cx, index)?;
    }
    let resp3 = cx.client.resp >= 3;
    // The debug wrapper runs in the foreground.
    let background = params.is_none() && cx.can_block() && cx.eng.cfg.num("search-workers") > 0;
    let db = cx.client.db;
    // `CleanupAndReplyStatus` (and the workers' callbacks) count an error building or
    // running the request by the request: `_FT.HYBRID`'s on the shard's side;
    // `DEBUG_hybridCommandHandler` replies its own uncounted.
    let side = (!matches!(dbg, Dbg::User(_))).then_some(!matches!(dbg, Dbg::Internal(_)));
    let mut h = parse(&cx.eng.cfg, args).map_err(|e| count_error(cx.eng, e.into(), side))?;
    {
        let eng = &mut *cx.eng;
        let ix = eng.search.indexes.get_mut(&name).expect("loaded");
        if let Err(e) = apply(&mut h, ix, resp3, &eng.cfg) {
            return Err(count_error(eng, e.into(), side));
        }
    }
    match (dbg, params) {
        (Dbg::Internal(d), _) => h.debug = d,
        (_, Some(p)) => {
            let strict = Policy::of(cx.eng.cfg.num("search-on-timeout") as i64) == Policy::Strict;
            h.debug = debug::parse_hybrid(p, strict)?;
            // `HybridRequest_Execute`: the debug wrapper replies without a cursor.
            h.cursor = None;
        }
        _ => {}
    }
    if let Some(p) = prof.as_mut() {
        p.parse_ns = p.clock.elapsed().as_nanos() as u64;
    }
    if h.cursor.is_some() {
        return start_cursors(cx, name, h, db, resp3, background, matches!(dbg, Dbg::Internal(_)));
    }
    let res = {
        let eng: &Engine = cx.eng;
        let ix: &Index = eng.search.indexes.get(&name).expect("loaded");
        run_hybrid(eng, ix, &h, db, resp3, background, clock, prof.as_mut())
    };
    // The dialect counts once the pipeline is built (queued, in the background); the debug
    // wrapper never counts it.
    if (res.is_ok() || background) && !matches!(dbg, Dbg::User(_)) {
        count_hybrid_dialect(cx, &name, &h);
    }
    res.map_err(|e| count_error(cx.eng, e, side))?
}

fn count_hybrid_dialect(cx: &mut Cx, name: &[u8], h: &Hyb) {
    if let Some(ix) = cx.eng.search.indexes.get_mut(name) {
        ix.used_dialects |= 1 << (h.dialect.clamp(1, 8) - 1);
    }
}

/// FT.HYBRID index [count] SEARCH query [SCORER s] [YIELD_SCORE_AS a] VSIM @field $vector
/// [KNN n … | RANGE n …] [FILTER …] [YIELD_SCORE_AS a] [COMBINE …] [options].
pub(crate) fn ft_hybrid(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    hybrid(cx, &args[1], &args[2..], None, Dbg::None)
}

/// FT.PROFILE index HYBRID [LIMITED] QUERY …: FT.HYBRID's reply with the profile appended.
pub(crate) fn profile_hybrid(cx: &mut Cx, index: &Bytes, args: &[Bytes], limited: bool, clock: Instant) -> CmdResult {
    hybrid(cx, index, args, Some(HProf { limited, clock, parse_ns: 0 }), Dbg::None)
}

/// `_FT.DEBUG FT.HYBRID index … DEBUG_PARAMS_COUNT <count>` (`argv` from FT.HYBRID on).
pub(crate) fn debug_hybrid(cx: &mut Cx, argv: &[Bytes]) -> CmdResult {
    hybrid(cx, &argv[1], &argv[2..], None, Dbg::User(argv))
}

/// `_FT.DEBUG _FT.HYBRID index … DEBUG_PARAMS_COUNT <count>`: the shard's command, its debug
/// parameters parsed first.
pub(crate) fn debug_internal_hybrid(cx: &mut Cx, argv: &[Bytes]) -> CmdResult {
    let (c, p) = debug::split(argv)?;
    let strict = Policy::of(cx.eng.cfg.num("search-on-timeout") as i64) == Policy::Strict;
    let d = debug::parse_hybrid(p, strict)?;
    if c.len() < 2 {
        return Err(arity_error("_FT.DEBUG|_FT.HYBRID"));
    }
    hybrid(cx, &c[1], &c[2..], None, Dbg::Internal(d))
}

// ---------------------------------------------------------------------------------------------
// WITHCURSOR

/// A subquery's cursor (FT.HYBRID WITHCURSOR): what its depleter held, read chunk by chunk
/// like an FT.AGGREGATE's (`RPDepleter_Next_Yield`).
pub(in crate::commands) struct HybCursor {
    req: Request,
    lookups: Vec<Lookup>,
    items: VecDeque<Pend>,
    dmds: Vec<Dmd>,
    /// The depletion's last status, returned once the results are out.
    tail: St,
    /// The depletion's total, which the first read reports (`finishSendChunk` resets it).
    total: i64,
    chunk: u32,
    db: usize,
}

/// What draining a subquery left.
struct Drained {
    lookups: Vec<Lookup>,
    items: VecDeque<Pend>,
    dmds: Vec<Dmd>,
    tail: St,
    total: i64,
    expansions: bool,
}

/// `HybridRequest_StartCursors`: both subqueries drained (`RPDepleter_DepleteAll`), a cursor
/// left on each; the reply maps SEARCH and VSIM to their ids, with the warnings.
fn start_cursors(cx: &mut Cx, name: Bytes, h: Hyb, db: usize, resp3: bool, background: bool, internal: bool) -> CmdResult {
    let side = Some(!internal);
    let conf = h.cursor.unwrap_or_default();
    let cap = cx.eng.cfg.num("search-index-cursor-limit") as i64;
    let spec_id = cx.eng.search.indexes.get(&name).expect("loaded").id;
    cx.eng.search.cursors.collect_expired();
    let open = cx.eng.search.cursors.index_total(spec_id) as i64;
    let drained: Result<Vec<Drained>, Error> = {
        let eng: &Engine = cx.eng;
        let ix: &Index = eng.search.indexes.get(&name).expect("loaded");
        let now = crate::util::now_ms();
        let mut ft_to_field = Vec::new();
        for (i, f) in ix.spec.fields.iter().enumerate() {
            if let Some(id) = f.ft_id {
                if ft_to_field.len() <= id {
                    ft_to_field.resize(id + 1, 0);
                }
                ft_to_field[id] = i as u16;
            }
        }
        let q = Qcx::new(&ix.docs, now, ft_to_field);
        let subs = build_subs(eng, ix, &q, &h, db, background, false, now);
        subs.and_then(|subs| {
            // `Cursors_Reserve`, once per subquery.
            if open + 2 > cap {
                return Err(QErr::new(Code::Limit, format!("INDEX_CURSOR_LIMIT of {cap} has been reached for an index")).into());
            }
            let mut out = Vec::new();
            for mut sub in subs {
                let mut dmds = Vec::new();
                let mut items = VecDeque::new();
                let tail = loop {
                    let mut x = Res::new();
                    let st = pull(&mut sub.chain, &mut sub.cx, &mut x);
                    if st != St::Ok {
                        break st;
                    }
                    items.push_back(Pend::of(x, &mut dmds));
                };
                let total = reported(sub.cx.total - sub.cx.skipped);
                if tail == St::Error {
                    let e = sub.cx.err.clone().unwrap_or_else(|| QErr::new(Code::Generic, "Failed to deplete set of results, rc=2"));
                    return Err(e.into());
                }
                out.push(Drained { lookups: std::mem::take(&mut sub.cx.lookups), items, dmds, tail, total, expansions: sub.expansions });
            }
            Ok(out)
        })
    };
    let drained = drained.map_err(|e| count_error(cx.eng, e, side))?;
    count_hybrid_dialect(cx, &name, &h);
    let timed_out = drained.iter().any(|d| d.tail == St::TimedOut);
    if timed_out && Policy::of(h.tail.on_timeout) != Policy::Return {
        return Err(count_error(cx.eng, QErr::new(Code::TimedOut, "Depleting timed out").into(), side));
    }
    let Hyb { search, vector, .. } = h;
    let mut ids = Vec::new();
    let mut expansions = Vec::new();
    for (mut req, d) in [search, vector].into_iter().zip(drained) {
        // The subquery's request is internal as the command is (its cursor's reads count so).
        req.internal = internal;
        let cursors = &mut cx.eng.search.cursors;
        let id = cursors.reserve(spec_id, name.clone(), conf.max_idle, cap).expect("under the index's cursor limit");
        let mut c = cursors.take_running(id).expect("reserved");
        expansions.push(d.expansions);
        let hc = HybCursor { req, lookups: d.lookups, items: d.items, dmds: d.dmds, tail: d.tail, total: d.total, chunk: conf.chunk_size, db };
        c.exec = Some(Exec(Box::new(hc)));
        cursors.pause(c);
        ids.push(id);
    }
    // `replyWithCursors`: its warnings counted on the shard's side.
    let mut warnings = Vec::new();
    if timed_out {
        cx.eng.search.stats.warning(QueryWarning::Timeout, false);
        warnings.push(Reply::Status(TIMEOUT_WARNING.into()));
    }
    for (i, e) in expansions.into_iter().enumerate() {
        if e {
            cx.eng.search.stats.warning(QueryWarning::MaxPrefixExpansions, false);
            warnings.push(Reply::Status(format!("Max prefix expansions limit was reached {}", if i == 0 { "(SEARCH)" } else { "(VSIM)" })));
        }
    }
    let pairs = vec![kv("SEARCH", Reply::Int(ids[0] as i64)), kv("VSIM", Reply::Int(ids[1] as i64)), kv("warnings", Reply::Array(warnings))];
    Ok(if resp3 { Reply::Map(pairs) } else { Reply::Array(pairs.into_iter().flat_map(|(k, v)| [k, v]).collect()) })
}

/// FT.CURSOR READ of a subquery's cursor (`runCursor` over its depleter): `num` results (the
/// chunk size sticks), the cursor done once they are out.
pub(in crate::commands) fn read_cursor(cx: &mut Cx, mut c: crate::search::cursor::Cursor, mut hc: Box<HybCursor>, count: u64) -> CmdResult {
    let mut num = count;
    if num == 0 {
        num = hc.chunk as u64;
        if num == 0 {
            num = CURSOR_READ_SIZE as u64;
        }
    }
    hc.chunk = num as u32;
    if hc.req.cap_timeout(&cx.eng.cfg) {
        hc.req.capped = true;
    }
    let resp3 = cx.client.resp >= 3;
    let (reply, done) = {
        let eng: &Engine = cx.eng;
        let ix: &Index = eng.search.indexes.get(&c.index).expect("alive");
        let HybCursor { req, lookups, items, dmds, tail, total, db, .. } = &mut *hc;
        // The documents gone since read as deleted.
        for d in dmds.iter_mut() {
            if ix.docs.get(d.id).is_none() {
                d.flags |= DOC_DELETED;
            }
        }
        let mut rows = Vec::new();
        let mut rc = St::Ok;
        while (rows.len() as u64) < num {
            let Some(p) = items.pop_front() else {
                rc = std::mem::replace(tail, St::Eof);
                break;
            };
            let dmd = p.dmd.map(|i| &dmds[i]);
            let sv: &[_] = match dmd {
                Some(d) if p.has_sv => &d.sort,
                _ => &[],
            };
            rows.push(Res { doc_id: p.doc_id, score: p.score, expl: p.expl, dmd, row: Row { dyn_vals: p.vals, sv }, ixr: p.ixr });
        }
        let policy = Policy::of(req.on_timeout);
        let mut pcx = Pcx {
            eng,
            db: *db,
            spec: &ix.spec,
            lookups: std::mem::take(lookups),
            total: 0,
            skipped: 0,
            result_limit: 0,
            min_score: 0.0,
            err: None,
            force: ix.spec.is_json() && req.dialect < 3,
            dialect: req.dialect,
            prof: None,
            policy,
            tmo: Tmo::default(),
            iclock: None,
        };
        let mut warnings = Vec::new();
        // `trackWarnings_Resp2` / `_replyWarnings` and `finishSendChunk` of the subquery's
        // request (`_FT.HYBRID`'s internal: no query counted, its warnings the shard's).
        if rc == St::TimedOut {
            eng.search.stats.warning(QueryWarning::Timeout, !req.internal);
        }
        eng.search.stats.count_query(Counted { internal: req.internal, cursor: true, aggregate: false }, std::time::Duration::ZERO);
        if resp3 {
            if rc == St::TimedOut {
                warnings.push(Reply::Status(TIMEOUT_WARNING.into()));
            }
            if req.capped {
                warnings.push(Reply::Status(CAPPED_WARNING.into()));
            }
        }
        let shown = std::mem::take(total);
        let li = pcx.lookups.len() - 1;
        let reply = chunk_reply(req, &mut pcx, li, None, &rows, resp3, (shown, shown), warnings);
        drop(rows);
        *lookups = std::mem::take(&mut pcx.lookups);
        let done = match rc {
            St::Ok => false,
            St::TimedOut => policy != Policy::Return,
            _ => true,
        };
        (reply, done)
    };
    let id = c.id;
    if !done {
        c.exec = Some(Exec(hc));
        cx.eng.search.cursors.pause(c);
    }
    Ok(Reply::Array(vec![reply, Reply::Int(if done { 0 } else { id as i64 })]))
}
