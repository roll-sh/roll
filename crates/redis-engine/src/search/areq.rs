//! RediSearch's query requests (`AREQ`, `aggregate_request.c`): compiling FT.SEARCH /
//! FT.AGGREGATE / FT.EXPLAIN arguments (`AREQ_Compile`: the query options, the options every
//! request shares and the aggregation plan's steps) and applying the index to them
//! (`AREQ_ApplyContext`: parsing the query, its parameters, the global filters, validation and
//! expansion), plus the query optimizer's pass over the parsed tree.

use bytes::Bytes;

use super::args::{Ac, AcErr, COALESCE, GE0, GE1, c_eq};
use super::error::{Code, QErr};
use super::index::Index;
use super::optimizer::{OptType, Plan};
use super::qast::{self, GeoFilter, Kind, Node, NumFilter, Params};
use super::qparse::{self, ParseOpts};
use super::spec;
use crate::config::Config;
use crate::util::cstr;

// `QEFlags`.
pub const F_IS_SEARCH: u64 = 1 << 0;
pub const F_IS_AGGREGATE: u64 = 1 << 1;
pub const F_SEND_SCORES: u64 = 1 << 2;
pub const F_SEND_SCORES_AS_FIELD: u64 = 1 << 3;
pub const F_SEND_SORTKEYS: u64 = 1 << 4;
pub const F_SEND_PAYLOADS: u64 = 1 << 5;
pub const F_SEND_NOFIELDS: u64 = 1 << 6;
pub const F_SEND_SCOREEXPLAIN: u64 = 1 << 7;
pub const F_SEND_HIGHLIGHT: u64 = 1 << 8;
pub const F_NOROWS: u64 = 1 << 9;
pub const F_HAS_SORTBY: u64 = 1 << 10;
pub const F_HAS_GROUPBY: u64 = 1 << 11;
pub const F_IS_CURSOR: u64 = 1 << 12;
pub const F_TYPED: u64 = 1 << 13;
pub const F_SENDRAWIDS: u64 = 1 << 14;
pub const F_REQUIRED_FIELDS: u64 = 1 << 15;
pub const F_HAS_WITHCOUNT: u64 = 1 << 16;
pub const F_HAS_DEPLETER: u64 = 1 << 17;
pub const AGG_LOAD_ALL: u64 = 1 << 18;
pub const OPTIMIZE: u64 = 1 << 19;
pub const FORMAT_DEFAULT: u64 = 1 << 20;
pub const FORMAT_EXPAND: u64 = 1 << 21;

const MAX_DIALECT: u32 = 4;
/// `APIVERSION_RETURN_MULTI_CMP_FIRST`.
const API_MULTI_CMP_FIRST: u32 = 3;
const SORT_MAX_FIELDS: usize = 8;
const MAX_GROUPBY_PROPERTIES: u32 = u16::MAX as u32;
/// `DEFAULT_LIMIT`.
const DEFAULT_LIMIT: u64 = 10;

/// Which command the request is for (`CommandType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Search,
    Aggregate,
    Explain,
}

/// `RSSearchOptions`' language before the index is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Unset,
    Unsupported,
    Is(usize),
}

/// A legacy `FILTER field min max` (`LegacyNumericFilter`).
#[derive(Clone, Debug)]
pub struct LegacyNumeric {
    pub field: Bytes,
    pub nf: NumFilter,
}

/// A legacy `GEOFILTER field lon lat radius unit` (`LegacyGeoFilter`).
#[derive(Clone, Debug)]
pub struct LegacyGeo {
    pub field: Bytes,
    pub gf: GeoFilter,
}

/// `RSSearchOptions`.
#[derive(Debug)]
pub struct SearchOptions {
    pub flags: u32,
    pub slop: i32,
    pub language: Lang,
    pub expander: Option<Bytes>,
    pub scorer: Option<Bytes>,
    pub inkeys: Option<Vec<Bytes>>,
    pub infields: Option<Vec<Bytes>>,
    pub field_mask: qast::Mask,
    pub params: Option<Params>,
    pub legacy_filters: Vec<LegacyNumeric>,
    pub legacy_geo: Vec<LegacyGeo>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            flags: 0,
            slop: -1,
            language: Lang::Unset,
            expander: None,
            scorer: None,
            inkeys: None,
            infields: None,
            field_mask: qast::MASK_ALL,
            params: None,
            legacy_filters: Vec::new(),
            legacy_geo: Vec::new(),
        }
    }
}

/// `SummarizeMode`.
pub const MODE_HIGHLIGHT: u8 = 1;
pub const MODE_SYNOPSIS: u8 = 2;

/// `ReturnedField`.
#[derive(Clone, Debug, Default)]
pub struct ReturnedField {
    pub name: Bytes,
    pub path: Bytes,
    pub explicit_return: bool,
    pub mode: u8,
    pub open_tag: Option<Bytes>,
    pub close_tag: Option<Bytes>,
    pub context_len: u32,
    pub num_frags: u32,
    pub separator: Option<Bytes>,
}

/// `FieldList`.
#[derive(Clone, Debug, Default)]
pub struct FieldList {
    pub fields: Vec<ReturnedField>,
    pub default_field: ReturnedField,
    pub explicit_return: bool,
}

impl FieldList {
    /// `FieldList_GetCreateField`: the entry for `path AS name` (no path: `name`).
    fn get_create(&mut self, name: &Bytes, path: Option<&Bytes>) -> usize {
        if let Some(i) = self.fields.iter().position(|f| cstr(&f.name) == cstr(name)) {
            let f = &mut self.fields[i];
            if let Some(p) = path
                && !f.explicit_return
                && f.mode != 0
            {
                f.path = p.clone();
            }
            return i;
        }
        self.fields.push(ReturnedField { name: name.clone(), path: path.unwrap_or(name).clone(), ..Default::default() });
        self.fields.len() - 1
    }
}

/// An aggregation plan step (`PLN_BaseStep`), as far as compiling needs them.
#[derive(Clone, Debug)]
pub enum Step {
    Arrange(Arrange),
    Group { properties: Vec<Bytes>, reducers: Vec<Reducer> },
    Apply { expr: Bytes, alias: Bytes },
    Filter { expr: Bytes },
    Load { args: Vec<Bytes>, all: bool },
}

/// `PLN_ArrangeStep`.
#[derive(Clone, Debug, Default)]
pub struct Arrange {
    pub offset: u64,
    pub limit: u64,
    pub is_limited: bool,
    /// `None` until a SORTBY (an empty list for `SORTBY 0`).
    pub sort_keys: Option<Vec<Bytes>>,
    pub asc_map: u64,
}

/// `PLN_Reducer`.
#[derive(Clone, Debug)]
pub struct Reducer {
    pub name: Bytes,
    pub args: Vec<Bytes>,
    pub alias: Bytes,
}

/// `CursorConfig`.
#[derive(Clone, Copy, Debug, Default)]
pub struct CursorConfig {
    pub max_idle: u32,
    pub chunk_size: u32,
}

/// A compiled request.
#[derive(Debug)]
pub struct Request {
    pub query: Bytes,
    pub flags: u64,
    pub opts: SearchOptions,
    pub dialect: u32,
    pub timeout: i64,
    pub bm25std_tanh: u64,
    pub max_search_results: u64,
    pub max_aggregate_results: u64,
    pub steps: Vec<Step>,
    pub out_fields: FieldList,
    pub cursor: CursorConfig,
    pub required_fields: Option<Vec<Bytes>>,
    /// `prefixesOffset`: where `_INDEX_PREFIXES` was, in the arguments from the query on.
    pub prefixes_offset: usize,
    pub args: Vec<Bytes>,
    pub payload: Option<Bytes>,
    /// The parsed query, once the index is applied.
    pub root: Option<Node>,
    pub num_tokens: usize,
    /// The query optimizer's decisions (with OPTIMIZE, once the index is applied).
    pub optimizer: Option<Plan>,
}

/// `ARG_HANDLED` / `ARG_UNKNOWN` (errors are `Err`).
#[derive(PartialEq, Eq)]
enum Arg {
    Handled,
    Unknown,
}

fn parse_args(code: Code, msg: &str) -> QErr {
    QErr::new(code, msg)
}

impl Request {
    /// `AREQ_New` + the flags `buildRequest` sets.
    fn new(cmd: Command, cfg: &Config, args: &[Bytes]) -> Request {
        let mut flags = FORMAT_DEFAULT;
        match cmd {
            Command::Search => flags |= F_IS_SEARCH,
            Command::Aggregate => flags |= F_IS_AGGREGATE,
            Command::Explain => {}
        }
        Request {
            query: Bytes::new(),
            flags,
            opts: SearchOptions::default(),
            dialect: cfg.num("search-default-dialect") as u32,
            timeout: cfg.num("search-timeout") as i64,
            bm25std_tanh: cfg.num("search-bm25std-tanh-factor") as u64,
            max_search_results: cfg.num("search-max-search-results") as u64,
            max_aggregate_results: cfg.num("search-max-aggregate-results") as u64,
            steps: Vec::new(),
            out_fields: FieldList::default(),
            cursor: CursorConfig::default(),
            required_fields: None,
            prefixes_offset: 0,
            args: args.to_vec(),
            payload: None,
            root: None,
            num_tokens: 0,
            optimizer: None,
        }
    }

    /// `ensureSimpleMode`.
    fn simple_mode(&mut self) -> bool {
        if self.flags & F_IS_AGGREGATE != 0 {
            return false;
        }
        self.flags |= F_IS_SEARCH;
        true
    }

    /// `AGPLN_GetArrangeStep`: the last arrange step after the last reducing step.
    fn arrange_index(&self) -> Option<usize> {
        for (i, s) in self.steps.iter().enumerate().rev() {
            match s {
                Step::Group { .. } => return None,
                Step::Arrange(_) => return Some(i),
                _ => {}
            }
        }
        None
    }

    /// `AGPLN_GetOrCreateArrangeStep`.
    fn arrange_or_create(&mut self) -> &mut Arrange {
        let i = match self.arrange_index() {
            Some(i) => i,
            None => {
                self.steps.push(Step::Arrange(Arrange::default()));
                self.steps.len() - 1
            }
        };
        match &mut self.steps[i] {
            Step::Arrange(a) => a,
            _ => unreachable!(),
        }
    }

    /// The arrange step before the first group (`hasQuerySortby`'s), if it sorts.
    pub fn has_query_sortby(&self) -> bool {
        for s in &self.steps {
            match s {
                Step::Group { .. } => return false,
                Step::Arrange(a) => return a.sort_keys.as_ref().is_some_and(|k| !k.is_empty()),
                _ => {}
            }
        }
        false
    }

    /// `handleCommonArgs`.
    fn common_args(&mut self, ac: &mut Ac) -> Result<Arg, QErr> {
        let mut dialect_specified = false;
        if ac.advance_if("LIMIT") {
            let is_search = self.flags & F_IS_SEARCH != 0;
            let (max_search, max_agg) = (self.max_search_results, self.max_aggregate_results);
            let arng = self.arrange_or_create();
            arng.is_limited = true;
            if ac.remaining() < 2 {
                return Err(parse_args(Code::ParseArgs, "LIMIT requires two arguments"));
            }
            let offset = ac.get_size(0);
            let limit = offset.and_then(|_| ac.get_size(0));
            let (Ok(offset), Ok(limit)) = (offset, limit) else {
                if let Ok(o) = offset {
                    arng.offset = o;
                }
                return Err(parse_args(Code::ParseArgs, "LIMIT needs two numeric arguments"));
            };
            arng.offset = offset;
            arng.limit = limit;
            if limit == 0 && offset != 0 {
                return Err(parse_args(Code::Limit, "The `offset` of the LIMIT must be 0 when `num` is 0"));
            }
            if limit == 0 {
                self.flags |= F_NOROWS | F_SEND_NOFIELDS;
            } else if limit > max_search && is_search {
                return Err(QErr::new(Code::Limit, format!("LIMIT exceeds maximum of {max_search}")));
            } else if limit > max_agg && !is_search {
                return Err(QErr::new(Code::Limit, format!("LIMIT exceeds maximum of {max_agg}")));
            } else if offset > max_search {
                return Err(QErr::new(Code::Limit, format!("OFFSET exceeds maximum of {max_search}")));
            }
        } else if ac.advance_if("SORTBY") {
            self.flags |= F_HAS_SORTBY;
            let existing = self.arrange_index();
            let mut arng = match existing {
                Some(i) => match &self.steps[i] {
                    Step::Arrange(a) => a.clone(),
                    _ => unreachable!(),
                },
                None => Arrange::default(),
            };
            let r = self.sortby(&mut arng, ac);
            if let Some(i) = existing {
                self.steps[i] = Step::Arrange(arng.clone());
            }
            r?;
            if arng.sort_keys.as_ref().is_some_and(|k| k.is_empty()) {
                self.flags &= !F_HAS_SORTBY;
                if existing.is_none() && arng.limit > 0 {
                    self.steps.push(Step::Arrange(arng));
                }
            } else if existing.is_none() {
                self.steps.push(Step::Arrange(arng));
            }
        } else if ac.advance_if("TIMEOUT") {
            if ac.remaining() < 1 {
                return Err(parse_args(Code::ParseArgs, "Need argument for TIMEOUT"));
            }
            self.timeout = ac.get_ll(GE0).map_err(|_| parse_args(Code::ParseArgs, "TIMEOUT requires a non negative integer"))?;
        } else if ac.advance_if("WITHCURSOR") {
            self.cursor_settings(ac)?;
        } else if ac.advance_if("_NUM_SSTRING") {
            self.flags |= F_TYPED;
        } else if ac.advance_if("WITHRAWIDS") {
            self.flags |= F_SENDRAWIDS;
        } else if ac.advance_if("PARAMS") {
            parse_params(&mut self.opts.params, ac)?;
        } else if ac.advance_if("_REQUIRED_FIELDS") {
            let args = ac
                .get_varargs()
                .map_err(|e| QErr::user(Code::ParseArgs, "Bad arguments", format!(" for _REQUIRED_FIELDS: {}", e.strerror())))?;
            self.required_fields = Some(args.objs.to_vec());
            self.flags |= F_REQUIRED_FIELDS;
        } else if ac.advance_if("DIALECT") {
            dialect_specified = true;
            if ac.remaining() < 1 {
                return Err(parse_args(Code::ParseArgs, "Need an argument for DIALECT"));
            }
            match ac.get_u32(GE1) {
                Ok(d) if d <= MAX_DIALECT => self.dialect = d,
                r => {
                    if let Ok(d) = r {
                        self.dialect = d;
                    }
                    return Err(QErr::new(Code::ParseArgs, format!("DIALECT requires a non negative integer >=1 and <= {MAX_DIALECT}")));
                }
            }
        } else if ac.advance_if("FORMAT") {
            let Ok(f) = ac.get() else {
                return Err(parse_args(Code::BadVal, "Need an argument for FORMAT"));
            };
            if c_eq(f, "EXPAND") {
                self.flags |= FORMAT_EXPAND;
            } else if c_eq(f, "STRING") {
                self.flags &= !FORMAT_EXPAND;
            } else {
                return Err(QErr::user(Code::ParseArgs, "FORMAT", format!(" {} is not supported", qast::lossy(f))));
            }
            self.flags &= !FORMAT_DEFAULT;
        } else if ac.advance_if("_INDEX_PREFIXES") {
            self.prefixes_offset = ac.offset - 1;
            // The C asserts (and crashes) on a malformed list.
            let _ = ac.get_varargs();
        } else if ac.advance_if("BM25STD_TANH_FACTOR") {
            if ac.remaining() < 1 {
                return Err(parse_args(Code::ParseArgs, "Need an argument for BM25STD_TANH_FACTOR"));
            }
            self.bm25std_tanh = ac.get_size(GE1).map_err(|_| parse_args(Code::ParseArgs, "BM25STD_TANH_FACTOR must be between 1 and 10000 inclusive"))?;
        } else {
            return Ok(Arg::Unknown);
        }
        if dialect_specified && self.dialect < API_MULTI_CMP_FIRST && self.flags & FORMAT_EXPAND != 0 {
            return Err(QErr::new(Code::Limit, format!("EXPAND format requires dialect {API_MULTI_CMP_FIRST} or greater")));
        }
        Ok(Arg::Handled)
    }

    /// `parseCursorSettings`.
    fn cursor_settings(&mut self, ac: &mut Ac) -> Result<(), QErr> {
        while let Some(a) = ac.peek() {
            let name = if a.eq_ignore_ascii_case(b"MAXIDLE") {
                "MAXIDLE"
            } else if a.eq_ignore_ascii_case(b"COUNT") {
                "COUNT"
            } else {
                break;
            };
            ac.offset += 1;
            let v = ac.get_u32(GE1).map_err(|e| QErr::bad_args(name, e))?;
            if name == "MAXIDLE" {
                self.cursor.max_idle = v;
            } else {
                self.cursor.chunk_size = v;
            }
        }
        self.flags |= F_IS_CURSOR;
        Ok(())
    }

    /// `parseSortby`.
    fn sortby(&mut self, arng: &mut Arrange, ac: &mut Ac) -> Result<(), QErr> {
        let legacy = self.flags & F_IS_SEARCH != 0;
        let bad = || parse_args(Code::ParseArgs, "Bad SORTBY arguments");
        if arng.sort_keys.is_some() {
            return Err(parse_args(
                Code::ParseArgs,
                if legacy { "Multiple SORTBY steps are not allowed" } else { "Multiple SORTBY steps are not allowed. Sort multiple fields in a single step" },
            ));
        }
        let mut asc_map = u64::MAX;
        let mut keys: Vec<Bytes> = Vec::new();
        if legacy {
            let Some(field) = ac.take() else { return Err(bad()) };
            keys.push(field.clone());
            if ac.advance_if("DESC") {
                asc_map &= !1;
            } else {
                ac.advance_if("ASC");
            }
        } else {
            let mut sub = ac.get_varargs().map_err(|e| QErr::user(Code::ParseArgs, "Bad arguments", format!(" for SORTBY: {}", e.strerror())))?;
            while let Some(s) = sub.take() {
                if s.first() == Some(&b'@') {
                    if keys.len() >= SORT_MAX_FIELDS {
                        return Err(QErr::new(Code::Limit, format!("Cannot sort by more than {SORT_MAX_FIELDS} fields")));
                    }
                    keys.push(s.slice(1..));
                    continue;
                }
                // `len - 1` wraps for a direction before any key (the shift is taken mod 64).
                let bit = (keys.len().wrapping_sub(1) as u32) % 64;
                if c_eq(s, "ASC") {
                    asc_map |= 1 << bit;
                } else if c_eq(s, "DESC") {
                    asc_map &= !(1 << bit);
                } else {
                    return Err(QErr::user(Code::ParseArgs, "MISSING ASC or DESC after sort field", format!(" ({})", qast::lossy(s))));
                }
            }
        }
        if ac.advance_if("MAX") {
            if legacy {
                return Err(parse_args(Code::ParseArgs, "SORTBY MAX is not supported by FT.SEARCH"));
            }
            // `rv = AC_GetUnsigned(…) != AC_OK`: the message is always the conversion one.
            let mx = ac.get_u32(0).map_err(|_| QErr::user(Code::ParseArgs, "Bad arguments", format!(" for MAX: {}", AcErr::Parse.strerror())))?;
            arng.limit = mx as u64;
        }
        arng.asc_map = asc_map;
        arng.sort_keys = Some(keys);
        Ok(())
    }

    /// `parseQueryArgs`.
    fn query_args(&mut self, ac: &mut Ac) -> Result<(), QErr> {
        let mut language: Option<Bytes> = None;
        let mut return_fields: Option<Vec<Bytes>> = None;
        self.flags |= FORMAT_DEFAULT;
        let mut optimization_specified = false;
        let mut has_empty_filter = false;
        while !ac.at_end() {
            // `AC_ParseArgSpec` over the query options.
            match self.query_spec(ac, &mut language, &mut return_fields) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => return Err(e),
            }
            if ac.advance_if("SUMMARIZE") {
                if !self.simple_mode() {
                    return Err(parse_args(Code::ParseArgs, "SUMMARIZE is not supported on FT.AGGREGATE"));
                }
                if parse_summarize(ac, &mut self.out_fields, false).is_err() {
                    return Err(parse_args(Code::ParseArgs, "Bad arguments for SUMMARIZE"));
                }
                self.flags |= F_SEND_HIGHLIGHT;
            } else if ac.advance_if("HIGHLIGHT") {
                if !self.simple_mode() {
                    return Err(parse_args(Code::ParseArgs, "HIGHLIGHT is not supported on FT.AGGREGATE"));
                }
                if parse_summarize(ac, &mut self.out_fields, true).is_err() {
                    return Err(parse_args(Code::ParseArgs, "Bad arguments for HIGHLIGHT"));
                }
                self.flags |= F_SEND_HIGHLIGHT;
            } else if self.flags & F_IS_SEARCH != 0 && ac.advance_if("FILTER") {
                let f = legacy_numeric(ac, &mut has_empty_filter)?;
                self.opts.legacy_filters.push(f);
            } else if self.flags & F_IS_SEARCH != 0 && ac.advance_if("GEOFILTER") {
                let g = legacy_geo(ac, &mut has_empty_filter)?;
                self.opts.legacy_geo.push(g);
            } else if ac.advance_if("WITHCOUNT") {
                self.flags &= !OPTIMIZE;
                if self.flags & F_IS_AGGREGATE != 0 {
                    self.flags |= F_HAS_WITHCOUNT;
                }
                optimization_specified = true;
            } else if ac.advance_if("WITHOUTCOUNT") {
                self.flags |= OPTIMIZE;
                if self.flags & F_IS_AGGREGATE != 0 {
                    self.flags &= !F_HAS_WITHCOUNT;
                }
                optimization_specified = true;
            } else if self.common_args(ac)? == Arg::Unknown {
                break;
            }
        }
        if self.dialect >= 2 && has_empty_filter {
            return Err(parse_args(Code::ParseArgs, "Numeric/Geo filter value/s cannot be empty"));
        }
        if !optimization_specified && self.dialect >= 4 {
            self.flags |= OPTIMIZE;
        }
        if self.flags & F_SEND_SCOREEXPLAIN != 0 && self.flags & F_SEND_SCORES == 0 {
            return Err(parse_args(Code::ParseArgs, "EXPLAINSCORE must be accompanied with WITHSCORES"));
        }
        if self.flags & F_IS_SEARCH != 0 && self.flags & F_SEND_SCORES_AS_FIELD != 0 {
            return Err(parse_args(Code::ParseArgs, "ADDSCORES is not supported on FT.SEARCH"));
        }
        self.opts.language = match &language {
            None => Lang::Unset,
            Some(l) => spec::find_language(l).map_or(Lang::Unsupported, Lang::Is),
        };
        if let Some(rf) = return_fields {
            if !self.simple_mode() {
                return Err(parse_args(Code::ParseArgs, "RETURN is not supported on FT.AGGREGATE"));
            }
            self.out_fields.explicit_return = true;
            if rf.is_empty() {
                self.flags |= F_SEND_NOFIELDS;
            }
            let mut r = Ac::new_sds(&rf);
            while let Some(path) = r.take() {
                let mut name = path;
                if r.advance_if("AS") {
                    match r.get() {
                        Err(_) => return Err(parse_args(Code::ParseArgs, "RETURN path AS name - must be accompanied with NAME")),
                        Ok(n) if c_eq(n, "AS") => return Err(parse_args(Code::ParseArgs, "Alias for RETURN cannot be `AS`")),
                        Ok(n) => name = n,
                    }
                }
                let i = self.out_fields.get_create(name, Some(path));
                self.out_fields.fields[i].explicit_return = true;
            }
            self.out_fields.fields.retain(|f| f.explicit_return);
        }
        Ok(())
    }

    /// One run of `AC_ParseArgSpec` over the query options: `Ok(false)` at the first argument
    /// that isn't one (`AC_ERR_ENOENT`), `Ok(true)` at the end.
    fn query_spec(&mut self, ac: &mut Ac, language: &mut Option<Bytes>, return_fields: &mut Option<Vec<Bytes>>) -> Result<bool, QErr> {
        while let Some(a) = ac.peek() {
            let is = |n: &str| a.eq_ignore_ascii_case(n.as_bytes());
            let bit = |flag: &str| is(flag);
            let name: &'static str;
            if is("INFIELDS") {
                name = "INFIELDS";
                ac.offset += 1;
                let s = ac.get_varargs().map_err(|e| QErr::bad_args(name, e))?;
                self.opts.infields = Some(s.objs.to_vec());
            } else if is("SLOP") {
                name = "SLOP";
                ac.offset += 1;
                self.opts.slop = ac.get_int(COALESCE).map_err(|e| QErr::bad_args(name, e))?;
            } else if is("LANGUAGE") {
                ac.offset += 1;
                *language = Some(ac.get().map_err(|e| QErr::bad_args("LANGUAGE", e))?.clone());
            } else if is("EXPANDER") {
                ac.offset += 1;
                self.opts.expander = Some(ac.get().map_err(|e| QErr::bad_args("EXPANDER", e))?.clone());
            } else if is("INKEYS") {
                ac.offset += 1;
                let s = ac.get_varargs().map_err(|e| QErr::bad_args("INKEYS", e))?;
                self.opts.inkeys = Some(s.objs.to_vec());
            } else if is("SCORER") {
                ac.offset += 1;
                self.opts.scorer = Some(ac.get().map_err(|e| QErr::bad_args("SCORER", e))?.clone());
            } else if is("RETURN") {
                ac.offset += 1;
                let s = ac.get_varargs().map_err(|e| QErr::bad_args("RETURN", e))?;
                *return_fields = Some(s.objs.to_vec());
            } else if bit("INORDER") {
                ac.offset += 1;
                self.opts.flags |= qast::S_IN_ORDER;
            } else if bit("VERBATIM") {
                ac.offset += 1;
                self.opts.flags |= qast::S_VERBATIM;
            } else if bit("WITHSCORES") {
                ac.offset += 1;
                self.flags |= F_SEND_SCORES;
            } else if bit("ADDSCORES") {
                ac.offset += 1;
                self.flags |= F_SEND_SCORES_AS_FIELD;
            } else if bit("WITHSORTKEYS") {
                ac.offset += 1;
                self.flags |= F_SEND_SORTKEYS;
            } else if bit("WITHPAYLOADS") {
                ac.offset += 1;
                self.flags |= F_SEND_PAYLOADS;
            } else if bit("NOCONTENT") {
                ac.offset += 1;
                self.flags |= F_SEND_NOFIELDS;
            } else if bit("NOSTOPWORDS") {
                ac.offset += 1;
                self.opts.flags |= qast::S_NO_STOPWORDS;
            } else if bit("EXPLAINSCORE") {
                ac.offset += 1;
                self.flags |= F_SEND_SCOREEXPLAIN;
            } else if is("PAYLOAD") {
                ac.offset += 1;
                self.payload = Some(ac.get().map_err(|e| QErr::bad_args("PAYLOAD", e))?.clone());
            } else {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `parseAggPlan`.
    fn agg_plan(&mut self, ac: &mut Ac) -> Result<(), QErr> {
        while !ac.at_end() {
            if self.common_args(ac)? == Arg::Handled {
                continue;
            }
            if ac.advance_if("GROUPBY") {
                if self.flags & F_IS_SEARCH != 0 {
                    return Err(QErr::new(Code::Inval, "option `GROUPBY` is mutually exclusive with simple (i.e. search) options"));
                }
                self.flags |= F_IS_AGGREGATE;
                self.groupby(ac)?;
                self.flags |= F_HAS_GROUPBY;
            } else if ac.advance_if("APPLY") {
                self.apply_or_filter(ac, true)?;
            } else if ac.advance_if("LOAD") {
                self.load(ac)?;
            } else if ac.advance_if("FILTER") {
                self.apply_or_filter(ac, false)?;
            } else {
                return Err(QErr::unknown_arg(ac, "<main>"));
            }
        }
        if self.flags & F_SEND_HIGHLIGHT == 0
            && self.flags & (F_SEND_SCORES | F_SEND_SCORES_AS_FIELD) == 0
            && (self.flags & F_IS_SEARCH == 0 || self.has_query_sortby())
        {
            self.opts.flags |= qast::S_CAN_SKIP_RICH_RESULTS;
        }
        Ok(())
    }

    /// `parseGroupby`.
    fn groupby(&mut self, ac: &mut Ac) -> Result<(), QErr> {
        let bad = |e: AcErr| QErr::user(Code::ParseArgs, "Bad arguments", format!(" for GROUPBY: {}", e.strerror()));
        let n = ac.get_u32(0).map_err(bad)?;
        if n > MAX_GROUPBY_PROPERTIES {
            // The count is read and the slice taken before the limit is checked.
            ac.get_slice(n as usize).map_err(bad)?;
            return Err(bad(AcErr::Limit));
        }
        let props = ac.get_slice(n as usize).map_err(bad)?;
        for p in props.objs {
            if p.first() != Some(&b'@') {
                let p = qast::lossy(p);
                return Err(QErr::user(Code::ParseArgs, "Bad arguments for GROUPBY", format!(": Unknown property `{p}`. Did you mean `@{p}`?")));
            }
        }
        self.steps.push(Step::Group { properties: props.objs.to_vec(), reducers: Vec::new() });
        while ac.advance_if("REDUCE") {
            let Ok(name) = ac.get() else {
                // `rv` is still the properties' AC_OK here.
                return Err(QErr::user(Code::ParseArgs, "Bad arguments", " for REDUCE: SUCCESS"));
            };
            let name = name.clone();
            let args = ac.get_varargs().map_err(|e| QErr::bad_args(&qast::lossy(&name), e))?;
            let mut alias = None;
            if ac.advance_if("AS") {
                alias = Some(ac.get().map_err(|e| QErr::user(Code::ParseArgs, "Bad arguments", format!(" for AS: {}", e.strerror())))?.clone());
            }
            let alias = alias.unwrap_or_else(|| reducer_alias(&name, args.objs));
            if let Some(Step::Group { reducers, .. }) = self.steps.last_mut() {
                reducers.push(Reducer { name, args: args.objs.to_vec(), alias });
            }
        }
        Ok(())
    }

    /// `handleApplyOrFilter`.
    fn apply_or_filter(&mut self, ac: &mut Ac, apply: bool) -> Result<(), QErr> {
        let expr = ac.get().map_err(|e| QErr::user(Code::ParseArgs, "Bad arguments", format!(" for APPLY/FILTER: {}", e.strerror())))?.clone();
        if apply {
            let mut alias = expr.clone();
            if ac.advance_if("AS") {
                alias = ac.get().map_err(|_| parse_args(Code::ParseArgs, "AS needs argument"))?.clone();
            }
            self.steps.push(Step::Apply { expr, alias });
        } else {
            self.steps.push(Step::Filter { expr });
        }
        Ok(())
    }

    /// `handleLoad`.
    fn load(&mut self, ac: &mut Ac) -> Result<(), QErr> {
        let bad = |e: AcErr| QErr::user(Code::ParseArgs, "Bad arguments", format!(" for LOAD: {}", e.strerror()));
        let start = ac.offset;
        let fields = match ac.get_varargs() {
            Ok(f) => f.objs.to_vec(),
            Err(AcErr::Parse) => {
                ac.offset = start;
                let s = ac.get().map_err(bad)?;
                if cstr(s) != b"*" {
                    return Err(parse_args(Code::ParseArgs, "Bad arguments for LOAD: Expected number of fields or `*`"));
                }
                self.flags |= AGG_LOAD_ALL;
                Vec::new()
            }
            Err(e) => return Err(bad(e)),
        };
        self.steps.push(Step::Load { args: fields, all: self.flags & AGG_LOAD_ALL != 0 });
        Ok(())
    }
}

/// `getReducerAlias`: `__generated_alias<name><args without leading @s, comma separated>`,
/// lowercased.
fn reducer_alias(name: &[u8], args: &[Bytes]) -> Bytes {
    let mut out = b"__generated_alias".to_vec();
    out.extend_from_slice(cstr(name));
    for (i, a) in args.iter().enumerate() {
        let mut s: &[u8] = a;
        while s.first() == Some(&b'@') {
            s = &s[1..];
        }
        out.extend_from_slice(s);
        if i + 1 < args.len() {
            out.push(b',');
        }
    }
    out.make_ascii_lowercase();
    // `rm_strndup(out, sdslen(out))`: a C string from here on.
    Bytes::from(cstr(&out).to_vec())
}

/// `parseParams`.
pub(crate) fn parse_params(dst: &mut Option<Params>, ac: &mut Ac) -> Result<(), QErr> {
    let args = ac.get_varargs().map_err(|e| QErr::user(Code::ParseArgs, "Bad arguments", format!(" for PARAMS: {}", e.strerror())))?;
    if dst.is_some() {
        return Err(QErr::new(Code::AddArgs, "Multiple PARAMS are not allowed. Parameters can be defined only once"));
    }
    if args.objs.is_empty() || args.objs.len() % 2 != 0 {
        return Err(QErr::new(Code::AddArgs, "Parameters must be specified in PARAM VALUE pairs"));
    }
    let mut params = Params::new();
    for pair in args.objs.chunks(2) {
        let name = cstr(&pair[0]).to_vec();
        if params.contains_key(&name) {
            return Err(QErr::user(Code::AddArgs, "Duplicate parameter", format!(" `{}`", qast::lossy(&name))));
        }
        params.insert(name, pair[1].clone());
    }
    *dst = Some(params);
    Ok(())
}

/// `parseCommon` (SUMMARIZE / HIGHLIGHT).
fn parse_summarize(ac: &mut Ac, fields: &mut FieldList, highlight: bool) -> Result<(), ()> {
    let mut open: Option<Bytes> = None;
    let mut close: Option<Bytes> = None;
    let mut context_len = 20u32;
    let mut num_frags = 3u32;
    let mut separator: Option<Bytes> = None;
    let mut ptrs = Vec::new();
    if ac.advance_if("FIELDS") {
        let list = ac.get_varargs().map_err(|_| ())?;
        for name in list.objs {
            ptrs.push(fields.get_create(name, None));
        }
    }
    while !ac.at_end() {
        if highlight && ac.advance_if("TAGS") {
            if ac.remaining() < 2 {
                return Err(());
            }
            open = ac.take().cloned();
            close = ac.take().cloned();
        } else if !highlight && ac.advance_if("LEN") {
            context_len = ac.get_u32(0).map_err(|_| ())?;
        } else if !highlight && ac.advance_if("FRAGS") {
            num_frags = ac.get_u32(0).map_err(|_| ())?;
        } else if !highlight && ac.advance_if("SEPARATOR") {
            separator = Some(ac.get().map_err(|_| ())?.clone());
        } else {
            break;
        }
    }
    let set = |f: &mut ReturnedField| {
        if highlight {
            f.open_tag = open.clone();
            f.close_tag = close.clone();
            f.mode |= MODE_HIGHLIGHT;
        } else {
            f.context_len = context_len;
            f.num_frags = num_frags;
            f.separator = separator.clone();
            f.mode |= MODE_SYNOPSIS;
        }
    };
    if ptrs.is_empty() {
        set(&mut fields.default_field);
    } else {
        for i in ptrs {
            set(&mut fields.fields[i]);
        }
    }
    Ok(())
}

/// `NumericFilter_LegacyParse`.
fn legacy_numeric(ac: &mut Ac, has_empty: &mut bool) -> Result<LegacyNumeric, QErr> {
    if ac.remaining() < 3 {
        return Err(parse_args(Code::ParseArgs, "FILTER requires 3 arguments"));
    }
    let field = Bytes::from(cstr(ac.take().expect("3 left")).to_vec());
    let mut nf = NumFilter { field: None, min: 0.0, max: 0.0, min_inclusive: true, max_inclusive: true };
    for is_min in [true, false] {
        let s = cstr(ac.take().expect("3 left"));
        if s.is_empty() {
            *has_empty = true;
        }
        let inclusive = s.first() != Some(&b'(');
        let v = qast::double_range(s, is_min, 1)?;
        if is_min {
            nf.min = v;
            nf.min_inclusive = inclusive;
        } else {
            nf.max = v;
            nf.max_inclusive = inclusive;
        }
    }
    Ok(LegacyNumeric { field, nf })
}

/// `GeoFilter_LegacyParse`.
fn legacy_geo(ac: &mut Ac, has_empty: &mut bool) -> Result<LegacyGeo, QErr> {
    if ac.remaining() < 5 {
        return Err(parse_args(Code::ParseArgs, "GEOFILTER requires 5 arguments"));
    }
    let field = Bytes::from(cstr(ac.take().expect("5 left")).to_vec());
    let mut vals = [0.0; 3];
    for (i, what) in ["<lon>", "<lat>", "<radius>"].into_iter().enumerate() {
        let at = ac.offset;
        let v = ac.get_double(0).map_err(|e| QErr::user(Code::ParseArgs, "Bad arguments", format!(" for {what}: {}", e.strerror())))?;
        if v == 0.0 && ac.objs[at].first().is_none_or(|&c| c == 0) {
            *has_empty = true;
        }
        vals[i] = v;
    }
    let unit = cstr(ac.take().expect("5 left"));
    let u = qast::geo_unit(unit);
    if u == qast::GEO_INVALID {
        return Err(QErr::user(Code::ParseArgs, "Unknown distance unit", format!(" {}", qast::lossy(unit))));
    }
    Ok(LegacyGeo { field, gf: GeoFilter { field: None, lon: vals[0], lat: vals[1], radius: vals[2], unit: u } })
}

/// `AREQ_Compile`: `args` start at the query.
pub fn compile(cmd: Command, cfg: &Config, args: &[Bytes]) -> Result<Request, QErr> {
    let mut req = Request::new(cmd, cfg, args);
    let mut ac = Ac::new_sds(args);
    let Some(q) = ac.take() else {
        return Err(parse_args(Code::ParseArgs, "No query string provided"));
    };
    req.query = q.clone();
    req.query_args(&mut ac)?;
    req.agg_plan(&mut ac)?;
    Ok(req)
}

/// `SetFilterNode`: the filter leads the root's intersection (made if needed); a KNN root takes
/// it under itself.
fn set_filter_node(root: &mut Option<Node>, num_tokens: &mut usize, filter: Node) {
    let Some(r) = root.as_mut() else { return };
    match &r.kind {
        Kind::Phrase { .. } => {
            r.children.insert(0, filter);
            *num_tokens += 1;
        }
        Kind::Vector(vq) if vq.knn => {
            if r.children.is_empty() {
                r.add_child(filter);
            } else {
                let child = r.children.remove(0);
                let mut nr = Node::phrase(false);
                nr.add_child(filter);
                nr.add_child(child);
                r.children.insert(0, nr);
            }
        }
        _ => {
            let old = root.take().expect("a root");
            let mut nr = Node::phrase(false);
            nr.add_child(filter);
            nr.add_child(old);
            *num_tokens += 1;
            *root = Some(nr);
        }
    }
}

/// `strtol(s, NULL, 10)`: the leading number, if any (0 otherwise).
fn strtol(s: &[u8]) -> i64 {
    let s = cstr(s);
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let neg = s.get(i) == Some(&b'-');
    if matches!(s.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    let mut v: i128 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        v = (v * 10 + (s[i] - b'0') as i128).min(1 << 64);
        i += 1;
    }
    let v = if neg { -v } else { v };
    v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// `jsonPathIsMultiValue` / `fieldSpecIsMultiValueJsonPath`.
fn multi_value_path(p: &[u8]) -> bool {
    p.first() == Some(&b'$') && super::jsonpath::parse(p).is_ok_and(|i| !i.is_single())
}

impl Request {
    /// `AREQ_ApplyContext`: everything up to the parsed, expanded tree.
    pub fn apply_context(&mut self, ix: &mut Index, resp3: bool) -> Result<(), QErr> {
        if self.prefixes_offset != 0 {
            let n = strtol(self.args.get(self.prefixes_offset + 1).map_or(&b""[..], |b| b)).max(0) as usize;
            let given = self.args.get(self.prefixes_offset + 2..).unwrap_or(&[]);
            let ours = &ix.spec.rule.prefixes;
            let coherent = n == ours.len() && given.len() >= n && ours.iter().zip(given).all(|(a, b)| cstr(a) == cstr(b));
            if !coherent {
                return Err(QErr { code: Code::Mismatch, public: None, private: None });
            }
        }
        if ix.spec.is_json() && self.flags & F_SEND_HIGHLIGHT != 0 {
            if !self.out_fields.explicit_return || self.out_fields.fields.is_empty() {
                return Err(QErr::new(Code::Inval, "HIGHLIGHT/SUMMARIZE on JSON indexes requires RETURN with explicit field names"));
            }
            let multi = self.out_fields.fields.iter().any(|rf| {
                if rf.mode == 0 && self.out_fields.default_field.mode == 0 {
                    return false;
                }
                let mut fs = ix.spec.field(cstr(&rf.name));
                if fs.is_none() && cstr(&rf.path) != cstr(&rf.name) {
                    fs = ix.spec.field(cstr(&rf.path));
                }
                multi_value_path(cstr(&rf.path)) || fs.and_then(|f| f.path.as_ref()).is_some_and(|p| multi_value_path(p))
            });
            if multi {
                return Err(QErr::new(Code::Inval, "HIGHLIGHT/SUMMARIZE is not supported for JSON fields with multi-value JSONPath"));
            }
        }
        if ix.spec.flags & spec::I_BYTE_OFFSETS == 0 && self.flags & F_SEND_HIGHLIGHT != 0 {
            return Err(QErr::new(Code::Inval, "Cannot use HIGHLIGHT/SUMMARIZE because NOOFSETS was specified at index level"));
        }
        if let Some(inf) = &self.opts.infields
            && !inf.is_empty()
        {
            self.opts.field_mask = inf.iter().fold(0, |m, s| m | qast::field_bit(&ix.spec, cstr(s)));
        }
        let language = match self.opts.language {
            Lang::Unset => ix.spec.rule.lang_default,
            Lang::Unsupported => return Err(QErr::new(Code::Inval, "No such language")),
            Lang::Is(l) => l,
        };
        self.opts.language = Lang::Is(language);
        if let Some(s) = &self.opts.scorer {
            let name = String::from_utf8_lossy(cstr(s));
            if !super::scorer_exists(&name) {
                return Err(QErr::new(Code::Inval, format!("No such scorer {name}")));
            }
        }
        if self.flags & FORMAT_DEFAULT != 0 {
            self.flags &= !(FORMAT_EXPAND | FORMAT_DEFAULT);
        }
        if self.flags & FORMAT_EXPAND != 0 {
            if !resp3 {
                return Err(parse_args(Code::BadVal, "EXPAND format is only supported with RESP3"));
            }
            if !ix.spec.is_json() {
                return Err(parse_args(Code::BadVal, "EXPAND format is only supported with JSON"));
            }
        }
        let stopwords = (self.opts.flags & qast::S_NO_STOPWORDS == 0).then_some(&ix.stopwords);
        let popts = ParseOpts { stopwords, params: self.opts.params.as_ref() };
        let parsed = qparse::parse(&self.query, self.dialect, &ix.spec, &popts)?;
        let mut root = Some(parsed.root.unwrap_or_else(|| Node::new(Kind::Null)));
        let mut num_tokens = parsed.num_tokens;
        if parsed.num_params > 0
            && let Some(r) = root.as_mut()
        {
            qast::eval_params(r, self.opts.params.as_ref(), self.dialect)?;
        }
        self.global_filters(ix, &mut root, &mut num_tokens)?;
        let mut sopts = qast::SearchOpts { flags: self.opts.flags, slop: self.opts.slop as i64, language };
        if let Some(r) = &root {
            qast::check_is_valid(r, &ix.spec, &mut sopts, 0)?;
        }
        self.opts.flags = sopts.flags;
        if self.opts.flags & qast::S_VERBATIM == 0
            && let Some(r) = root.as_mut()
        {
            let name = self.opts.expander.as_deref().map_or(&b"DEFAULT"[..], cstr);
            if let Some(exp) = qast::Expander::find(name) {
                qast::expand(r, ix, exp, language, &mut num_tokens)?;
            }
        }
        if self.flags & OPTIMIZE != 0
            && let Some(r) = root.as_mut()
        {
            self.optimizer = Some(self.optimize_nodes(&ix.spec, r));
        }
        self.root = root;
        self.num_tokens = num_tokens;
        Ok(())
    }

    /// `applyGlobalFilters`.
    fn global_filters(&self, ix: &Index, root: &mut Option<Node>, num_tokens: &mut usize) -> Result<(), QErr> {
        let sp = &ix.spec;
        for f in &self.opts.legacy_filters {
            let fs = sp.fields.iter().position(|x| cstr(&x.name) == cstr(&f.field) && x.name.len() == cstr(&f.field).len());
            if !fs.is_some_and(|i| sp.fields[i].is(spec::T_NUMERIC)) {
                if self.dialect != 1 {
                    let name = qast::lossy(&f.field);
                    return Err(if fs.is_some() {
                        QErr::user(Code::Inval, "Field is not a numeric field", format!(", field: {name}"))
                    } else {
                        QErr::user(Code::Inval, "Unknown Field", format!(" '{name}'"))
                    });
                }
                set_filter_node(root, num_tokens, Node::new(Kind::Null));
                continue;
            }
            let mut nf = f.nf.clone();
            nf.field = fs;
            set_filter_node(root, num_tokens, Node::new(Kind::Numeric(nf)));
        }
        for g in &self.opts.legacy_geo {
            let fs = sp.fields.iter().position(|x| cstr(&x.name) == cstr(&g.field) && x.name.len() == cstr(&g.field).len());
            if !fs.is_some_and(|i| sp.fields[i].is(spec::T_GEO)) {
                if self.dialect != 1 {
                    let general = if fs.is_some() { "Field is not a geo field" } else { "Unknown Field" };
                    return Err(QErr::user(Code::Inval, general, format!(", field: {}", qast::lossy(&g.field))));
                }
                set_filter_node(root, num_tokens, Node::new(Kind::Null));
            } else {
                let mut gf = g.gf.clone();
                gf.field = fs;
                set_filter_node(root, num_tokens, Node::new(Kind::Geo(gf)));
            }
        }
        if let Some(keys) = &self.opts.inkeys {
            set_filter_node(root, num_tokens, Node::new(Kind::Ids(keys.clone())));
        }
        Ok(())
    }

    /// `QOptimizer_Parse` + `QOptimizer_QueryNodes`: the sort field and the scorer the request
    /// needs, the numeric filter on a numeric SORTBY field taken out of the root's intersection,
    /// and what the optimizer can do.
    fn optimize_nodes(&self, sp: &spec::Spec, root: &mut Node) -> Plan {
        let mut plan = Plan::new();
        if let Some(i) = self.arrange_index()
            && let Step::Arrange(arng) = &self.steps[i]
        {
            plan.limit = arng.limit.wrapping_add(arng.offset) as usize;
            if self.flags & F_IS_SEARCH != 0 && plan.limit == 0 {
                plan.limit = DEFAULT_LIMIT as usize;
            }
            if let Some(name) = arng.sort_keys.as_ref().and_then(|k| k.first()) {
                let name = cstr(name);
                match sp.fields.iter().position(|f| f.name.as_ref() == name) {
                    Some(f) if sp.fields[f].types == spec::T_NUMERIC => {
                        plan.field = Some(f);
                        plan.asc = arng.asc_map & 1 != 0;
                    }
                    // Sorting by another field: no optimization.
                    _ => plan.ty = OptType::None,
                }
            }
        }
        // The scorers that need the query's terms or the document (unknown ones none).
        plan.scorer = plan.field.is_none()
            && matches!(self.opts.scorer.as_deref().map(cstr), None | Some(b"BM25STD" | b"TFIDF" | b"TFIDF.DOCNORM" | b"DISMAX" | b"BM25" | b"DOCSCORE" | b"HAMMING"));

        if matches!(root.kind, Kind::Wildcard) {
            plan.scorer = false;
        }
        let mut parent: Option<Vec<usize>> = None;
        let mut path = Vec::new();
        let mut req_score = false;
        let found = check_query_types(root, plan.field, &mut parent, &mut path, true, &mut req_score);
        if let Found::Node(p) = found {
            match parent {
                Some(pp) => {
                    let mut n = &mut *root;
                    for &c in &pp {
                        n = &mut n.children[c];
                    }
                    // `array_del_fast`: the last child takes its place.
                    let removed = n.children.swap_remove(p[pp.len()]);
                    if let Kind::Numeric(nf) = removed.kind {
                        plan.nf = Some(nf);
                    }
                }
                None => {
                    // A lone range on the sort field: a partial range is enough.
                    if plan.ty == OptType::Undecided {
                        plan.ty = OptType::PartialRange;
                    }
                    return plan;
                }
            }
        }
        let knn = matches!(&root.kind, Kind::Vector(v) if v.knn);
        if (plan.field.is_none() && req_score) || knn {
            if plan.ty == OptType::Undecided {
                plan.ty = OptType::None;
            }
            return plan;
        }
        if !req_score {
            if plan.field.is_some() {
                if plan.ty == OptType::Undecided {
                    plan.ty = OptType::PartialRange;
                }
            } else {
                if plan.ty == OptType::Undecided {
                    plan.ty = OptType::NoSorter;
                }
                plan.scorer = false;
            }
        }
        plan
    }
}

/// `checkQueryTypes`' result.
enum Found {
    None,
    /// The numeric node, by its path from the root.
    Node(Vec<usize>),
    /// `INVALUD_PTR`: more than one candidate.
    Invalid,
}

/// `checkQueryTypes`: the numeric node on the sort field among an intersection's direct
/// children, with that intersection as its parent; and whether text needs scoring.
fn check_query_types(n: &Node, field: Option<usize>, parent: &mut Option<Vec<usize>>, path: &mut Vec<usize>, has_parent: bool, req_score: &mut bool) -> Found {
    match &n.kind {
        Kind::Numeric(nf) => {
            if field.is_some() && nf.field == field {
                return Found::Node(path.clone());
            }
            Found::None
        }
        Kind::Phrase { .. } => {
            if n.opts.weight != 1.0 {
                return Found::None;
            }
            let mut ret = Found::None;
            for (i, c) in n.children.iter().enumerate() {
                path.push(i);
                let cur = check_query_types(c, field, parent, path, has_parent, req_score);
                path.pop();
                match cur {
                    // The C reads the bad pointer's type here (and crashes).
                    Found::Invalid => return Found::Invalid,
                    Found::Node(p) if parent.is_none() => {
                        if !matches!(ret, Found::None) {
                            return Found::Invalid;
                        }
                        ret = Found::Node(p);
                    }
                    _ => {}
                }
            }
            if matches!(ret, Found::Node(_)) && has_parent {
                *parent = Some(path.clone());
            }
            ret
        }
        Kind::Token(_) | Kind::Fuzzy { .. } | Kind::Prefix { .. } | Kind::WildcardQuery(_) | Kind::LexRange { .. } => {
            *req_score = true;
            Found::None
        }
        Kind::Optional | Kind::Not | Kind::Union => {
            for c in &n.children {
                let mut p = None;
                check_query_types(c, None, &mut p, path, false, req_score);
            }
            Found::None
        }
        _ => Found::None,
    }
}

