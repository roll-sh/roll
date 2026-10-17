//! Index specs (`spec.c`): FT.CREATE's options and schema, FT.ALTER's added fields, the
//! document rule (`rules.c`) and the replies FT.INFO builds from them.

use super::args::{Ac, AcErr, GE0, GE1};
use super::error::{Code, QErr, QResult};
use bytes::Bytes;

// Field types (`FieldType`).
pub const T_TEXT: u8 = 0x01;
pub const T_NUMERIC: u8 = 0x02;
pub const T_GEO: u8 = 0x04;
pub const T_TAG: u8 = 0x08;
pub const T_VECTOR: u8 = 0x10;
pub const T_GEOMETRY: u8 = 0x20;

/// `FieldSpec_GetTypeNames`, by type bit.
pub fn type_name(t: u8) -> &'static str {
    match t {
        T_TEXT => "TEXT",
        T_NUMERIC => "NUMERIC",
        T_GEO => "GEO",
        T_TAG => "TAG",
        T_VECTOR => "VECTOR",
        _ => "GEOSHAPE",
    }
}

// Field options (`FieldSpecOptions`).
pub const F_SORTABLE: u16 = 0x01;
pub const F_NOSTEM: u16 = 0x02;
pub const F_NOINDEX: u16 = 0x04;
pub const F_PHONETICS: u16 = 0x08;
pub const F_DYNAMIC: u16 = 0x10;
pub const F_UNF: u16 = 0x20;
pub const F_SUFFIX_TRIE: u16 = 0x40;
pub const F_UNDEFINED_ORDER: u16 = 0x80;
pub const F_INDEX_EMPTY: u16 = 0x100;
pub const F_INDEX_MISSING: u16 = 0x200;

// Index flags (`IndexFlags`).
pub const I_TERM_OFFSETS: u32 = 0x01;
pub const I_FIELD_FLAGS: u32 = 0x02;
pub const I_MULTI_VALUE: u32 = 0x04;
pub const I_CUSTOM_STOPWORDS: u32 = 0x08;
pub const I_FREQS: u32 = 0x10;
pub const I_NUMERIC: u32 = 0x20;
pub const I_BYTE_OFFSETS: u32 = 0x40;
pub const I_WIDE_SCHEMA: u32 = 0x80;
pub const I_HAS_SMAP: u32 = 0x100;
pub const I_TEMPORARY: u32 = 0x200;
pub const I_PHONETIC: u32 = 0x400;
pub const I_ASYNC: u32 = 0x800;
pub const I_SKIP_INITIAL_SCAN: u32 = 0x1000;
pub const I_FIELD_ALIAS: u32 = 0x4000;
pub const I_VECSIM: u32 = 0x8000;
pub const I_SUFFIX_TRIE: u32 = 0x10000;
pub const I_UNDEFINED_ORDER: u32 = 0x20000;
pub const I_GEOMETRY: u32 = 0x40000;
pub const I_NON_EMPTY: u32 = 0x80000;
pub const I_DEFAULT: u32 = I_FREQS | I_TERM_OFFSETS | I_FIELD_FLAGS | I_BYTE_OFFSETS;

pub const MAX_FIELDS: usize = 1024;
/// `SPEC_MAX_FIELD_ID`: TEXT fields get a bit of a 128-bit mask.
pub const MAX_TEXT_FIELDS: usize = 128;
pub const WIDE_THRESHOLD: usize = 32;
pub const MAX_PREFIXES: usize = 1_000_000;

/// The languages RediSearch knows (`__langPairs`), in RDB order (`RSLanguage`, 1-based after
/// "unsupported").
pub const LANGUAGES: &[&str] = &[
    "arabic",
    "armenian",
    "basque",
    "catalan",
    "chinese",
    "danish",
    "dutch",
    "english",
    "finnish",
    "french",
    "german",
    "greek",
    "hindi",
    "hungarian",
    "indonesian",
    "irish",
    "italian",
    "lithuanian",
    "malay",
    "nepali",
    "norwegian",
    "portuguese",
    "romanian",
    "russian",
    "serbian",
    "spanish",
    "swedish",
    "tagalog",
    "tamil",
    "turkish",
    "yiddish",
];

/// `RSLanguage_Find`: an index into [`LANGUAGES`].
pub fn find_language(name: &[u8]) -> Option<usize> {
    let name = crate::util::cstr(name);
    LANGUAGES.iter().position(|l| name.eq_ignore_ascii_case(l.as_bytes()))
}

pub const ENGLISH: usize = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocType {
    Hash,
    Json,
}

impl DocType {
    pub fn name(self) -> &'static str {
        match self {
            DocType::Hash => "HASH",
            DocType::Json => "JSON",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VecType {
    Float32,
    Float64,
    Float16,
    BFloat16,
    Uint8,
    Int8,
}

impl VecType {
    pub fn name(self) -> &'static str {
        match self {
            VecType::Float32 => "FLOAT32",
            VecType::Float64 => "FLOAT64",
            VecType::Float16 => "FLOAT16",
            VecType::BFloat16 => "BFLOAT16",
            VecType::Uint8 => "UINT8",
            VecType::Int8 => "INT8",
        }
    }

    pub fn size(self) -> usize {
        match self {
            VecType::Float32 => 4,
            VecType::Float64 => 8,
            VecType::Float16 | VecType::BFloat16 => 2,
            VecType::Uint8 | VecType::Int8 => 1,
        }
    }

    fn parse(ac: &mut Ac) -> Result<VecType, AcErr> {
        let s = crate::util::cstr(ac.get()?);
        let eq = |n: &str| s.eq_ignore_ascii_case(n.as_bytes());
        Ok(if eq("FLOAT32") {
            VecType::Float32
        } else if eq("FLOAT64") {
            VecType::Float64
        } else if eq("FLOAT16") {
            VecType::Float16
        } else if eq("BFLOAT16") {
            VecType::BFloat16
        } else if eq("UINT8") {
            VecType::Uint8
        } else if eq("INT8") {
            VecType::Int8
        } else {
            return Err(AcErr::NoEnt);
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    L2,
    Ip,
    Cosine,
}

impl Metric {
    pub fn name(self) -> &'static str {
        match self {
            Metric::L2 => "L2",
            Metric::Ip => "IP",
            Metric::Cosine => "COSINE",
        }
    }

    fn parse(ac: &mut Ac) -> Result<Metric, AcErr> {
        let s = crate::util::cstr(ac.get()?);
        let eq = |n: &str| s.eq_ignore_ascii_case(n.as_bytes());
        Ok(if eq("IP") {
            Metric::Ip
        } else if eq("L2") {
            Metric::L2
        } else if eq("COSINE") {
            Metric::Cosine
        } else {
            return Err(AcErr::NoEnt);
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algo {
    Flat,
    Hnsw,
    Svs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quant {
    None,
    Lvq8,
    Lvq4,
    Lvq4x4,
    Lvq4x8,
    LeanVec4x8,
    LeanVec8x8,
}

impl Quant {
    pub fn name(self) -> &'static str {
        match self {
            Quant::None => "NO_COMPRESSION",
            Quant::Lvq8 => "LVQ8",
            Quant::Lvq4 => "LVQ4",
            Quant::Lvq4x4 => "LVQ4x4",
            Quant::Lvq4x8 => "LVQ4x8",
            Quant::LeanVec4x8 => "LeanVec4x8",
            Quant::LeanVec8x8 => "LeanVec8x8",
        }
    }

    pub fn leanvec(self) -> bool {
        matches!(self, Quant::LeanVec4x8 | Quant::LeanVec8x8)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VectorParams {
    pub algo: Algo,
    pub ty: VecType,
    pub dim: usize,
    pub metric: Metric,
    pub multi: bool,
    pub initial_cap: Option<usize>,
    pub block_size: usize,
    pub m: usize,
    pub ef_construction: usize,
    pub ef_runtime: usize,
    pub epsilon: f64,
    pub graph_max_degree: usize,
    pub construction_window_size: usize,
    pub search_window_size: usize,
    pub quant: Quant,
    pub training_threshold: usize,
    pub reduce: usize,
}

impl VectorParams {
    fn new(algo: Algo, multi: bool) -> Self {
        VectorParams {
            algo,
            ty: VecType::Float32,
            dim: 0,
            metric: Metric::L2,
            multi,
            initial_cap: None,
            block_size: 0,
            m: 16,
            ef_construction: 200,
            ef_runtime: 10,
            epsilon: 0.01,
            graph_max_degree: 32,
            construction_window_size: 200,
            search_window_size: 10,
            quant: Quant::None,
            training_threshold: 0,
            reduce: 0,
        }
    }

    /// `expBlobSize`.
    pub fn blob_size(&self) -> usize {
        self.dim * self.ty.size()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coords {
    /// `GEOMETRY_COORDS_Cartesian` (FLAT).
    Flat,
    /// `GEOMETRY_COORDS_Geographic` (SPHERICAL).
    Spherical,
}

impl Coords {
    pub fn name(self) -> &'static str {
        match self {
            Coords::Flat => "FLAT",
            Coords::Spherical => "SPHERICAL",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: Bytes,
    /// The path (`AS` given), else the name.
    pub path: Option<Bytes>,
    pub types: u8,
    pub options: u16,
    pub sort_idx: i32,
    /// Position in the schema.
    pub index: usize,
    /// The TEXT field id (its bit in field masks).
    pub ft_id: Option<usize>,
    pub weight: f64,
    pub tag_sep: u8,
    pub tag_case_sensitive: bool,
    pub vector: Option<VectorParams>,
    pub coords: Coords,
}

impl Field {
    fn new(name: Bytes, path: Option<Bytes>, index: usize) -> Self {
        Field {
            name,
            path,
            types: 0,
            options: 0,
            sort_idx: -1,
            index,
            ft_id: None,
            weight: 1.0,
            tag_sep: b',',
            tag_case_sensitive: false,
            vector: None,
            coords: Coords::Spherical,
        }
    }

    #[cfg(test)]
    pub fn test(name: &str, types: u8) -> Self {
        let mut f = Field::new(Bytes::copy_from_slice(name.as_bytes()), None, 0);
        f.types = types;
        f
    }

    pub fn is(&self, t: u8) -> bool {
        self.types & t != 0
    }

    pub fn has(&self, o: u16) -> bool {
        self.options & o != 0
    }

    pub fn path(&self) -> &Bytes {
        self.path.as_ref().unwrap_or(&self.name)
    }

    pub fn sortable(&self) -> bool {
        self.has(F_SORTABLE)
    }

    pub fn indexable(&self) -> bool {
        !self.has(F_NOINDEX)
    }

    pub fn name_str(&self) -> String {
        text(&self.name)
    }
}

/// The C string an argument stands for (RediSearch reads arguments with `%s`).
pub fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(crate::util::cstr(b)).into_owned()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub doc_type: DocType,
    pub prefixes: Vec<Bytes>,
    pub filter: Option<String>,
    pub filter_expr: Option<super::expr::Expr>,
    pub lang_default: usize,
    pub lang_field: Option<String>,
    pub score_default: f64,
    pub score_field: Option<String>,
    pub payload_field: Option<String>,
    pub index_all: bool,
}

#[derive(Default)]
struct RuleArgs {
    doc_type: Option<Bytes>,
    filter: Option<Bytes>,
    score_default: Option<Bytes>,
    score_field: Option<Bytes>,
    lang_default: Option<Bytes>,
    lang_field: Option<Bytes>,
    payload_field: Option<Bytes>,
    index_all: Option<Bytes>,
}

/// `fast_float_strtod`: the longest prefix that parses (`None` when nothing does).
pub fn strtod_prefix(s: &str) -> Option<f64> {
    let s = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let b = s.as_bytes();
    let mut best = None;
    let mut end = 0;
    while end < b.len() && end < 400 {
        end += 1;
        if let Ok(v) = s[..end].parse::<f64>()
            && !s[..end].eq_ignore_ascii_case("inf")
            && !s[..end].to_ascii_lowercase().starts_with("nan")
        {
            best = Some(v);
        }
        if !s.is_char_boundary(end) {
            continue;
        }
    }
    if best.is_none() {
        let l = s.to_ascii_lowercase();
        let (neg, l) = match l.strip_prefix('-') {
            Some(r) => (true, r.to_string()),
            None => (false, l.strip_prefix('+').unwrap_or(&l).to_string()),
        };
        if l.starts_with("inf") {
            return Some(if neg { f64::NEG_INFINITY } else { f64::INFINITY });
        }
        if l.starts_with("nan") {
            return Some(f64::NAN);
        }
    }
    best
}

impl Rule {
    fn create(a: &RuleArgs, prefixes: Option<Ac>) -> QResult<Rule> {
        let doc_type = match &a.doc_type {
            None => DocType::Hash,
            Some(t) if crate::util::cstr(t).eq_ignore_ascii_case(b"HASH") => DocType::Hash,
            Some(t) if crate::util::cstr(t).eq_ignore_ascii_case(b"JSON") => DocType::Json,
            Some(t) => return Err(QErr::user(Code::AddArgs, "Invalid rule type", format!(": {}", text(t)))),
        };
        let score_default = match &a.score_default {
            Some(s) => {
                let s = text(s);
                match strtod_prefix(&s) {
                    Some(v) if (0.0..=1.0).contains(&v) => v,
                    _ => return Err(QErr::new(Code::AddArgs, "Invalid score")),
                }
            }
            None => 1.0,
        };
        let lang_default = match &a.lang_default {
            Some(l) => find_language(l).ok_or_else(|| QErr::new(Code::AddArgs, "Invalid language"))?,
            None => ENGLISH,
        };
        let prefixes: Vec<Bytes> = match prefixes {
            Some(ac) => {
                if ac.remaining() > MAX_PREFIXES {
                    return Err(QErr::new(
                        Code::Limit,
                        format!("Number of prefixes ({}) exceeds maximum allowed ({MAX_PREFIXES})", ac.remaining()),
                    ));
                }
                ac.rest().iter().map(|p| Bytes::copy_from_slice(crate::util::cstr(p))).collect()
            }
            None => vec![Bytes::new()],
        };
        let filter = a.filter.as_ref().map(|f| text(f));
        let filter_expr = match &a.filter {
            // `ExprAST_Parse` sets the parser's error first; "Invalid expression" never shows.
            Some(f) => Some(super::expr::parse(crate::util::cstr(f)).map_err(|e| match e {
                Some(m) => QErr::new(Code::Expr, m),
                None => QErr::code(Code::Expr),
            })?),
            None => None,
        };
        let index_all = match &a.index_all {
            Some(v) => {
                let v = crate::util::cstr(v);
                if v.eq_ignore_ascii_case(b"enable") {
                    true
                } else if v.eq_ignore_ascii_case(b"disable") {
                    false
                } else {
                    return Err(QErr::new(Code::AddArgs, "Invalid argument for `INDEXALL`, use ENABLE/DISABLE"));
                }
            }
            None => false,
        };
        Ok(Rule {
            doc_type,
            prefixes,
            filter,
            filter_expr,
            lang_default,
            lang_field: a.lang_field.as_ref().map(|f| text(f)),
            score_default,
            score_field: a.score_field.as_ref().map(|f| text(f)),
            payload_field: a.payload_field.as_ref().map(|f| text(f)),
            index_all,
        })
    }
}

/// The schema and options of an index (no data).
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub name: Bytes,
    pub fields: Vec<Field>,
    pub flags: u32,
    pub rule: Rule,
    /// Custom stopwords (lowercased), when STOPWORDS was given.
    pub stopwords: Option<Vec<Bytes>>,
    /// TEMPORARY, in seconds.
    pub timeout: Option<i64>,
    pub num_sortables: usize,
    /// TEXT field ids in use (`fieldIdToIndex`).
    pub text_ids: Vec<usize>,
    pub suffix_mask: u128,
}

impl Spec {
    pub fn field(&self, name: &[u8]) -> Option<&Field> {
        self.fields.iter().find(|f| f.name.as_ref() == name)
    }

    pub fn is_json(&self) -> bool {
        self.rule.doc_type == DocType::Json
    }

    /// `IndexSpec_ParseFromArgCursor` over FT.CREATE's arguments after the index name.
    pub fn parse(name: Bytes, args: &[Bytes], cfg: &crate::config::Config) -> QResult<Spec> {
        let mut flags = I_DEFAULT;
        let mut ac = Ac::new(args);
        let mut rule_args = RuleArgs::default();
        let mut prefixes: Option<Ac> = None;
        let mut stopwords: Option<Ac> = None;
        let mut timeout: Option<i64> = None;
        // `AC_ParseArgSpec` with the non-Flex options.
        while let Some(a) = ac.peek() {
            let a = crate::util::cstr(a);
            let is = |n: &str| a.eq_ignore_ascii_case(n.as_bytes());
            let name: &'static str;
            let rc: Result<(), AcErr> = if is("NOOFFSETS") {
                ac.advance().ok();
                flags &= !(I_TERM_OFFSETS | I_BYTE_OFFSETS);
                continue;
            } else if is("NOHL") {
                ac.advance().ok();
                flags &= !I_BYTE_OFFSETS;
                continue;
            } else if is("NOFIELDS") {
                ac.advance().ok();
                flags &= !I_FIELD_FLAGS;
                continue;
            } else if is("NOFREQS") {
                ac.advance().ok();
                flags &= !I_FREQS;
                continue;
            } else if is("MAXTEXTFIELDS") {
                ac.advance().ok();
                flags |= I_WIDE_SCHEMA;
                continue;
            } else if is("ASYNC") {
                ac.advance().ok();
                flags |= I_ASYNC;
                continue;
            } else if is("SKIPINITIALSCAN") {
                ac.advance().ok();
                flags |= I_SKIP_INITIAL_SCAN;
                continue;
            } else if is("NOSCOREIDX") {
                ac.advance().ok();
                continue;
            } else if is("ON") {
                name = "ON";
                ac.advance().ok();
                ac.get().map(|v| rule_args.doc_type = Some(v.clone()))
            } else if is("PREFIX") {
                name = "PREFIX";
                ac.advance().ok();
                ac.get_varargs().map(|v| prefixes = Some(v))
            } else if is("FILTER") {
                name = "FILTER";
                ac.advance().ok();
                ac.get().map(|v| rule_args.filter = Some(v.clone()))
            } else if is("SCORE") {
                name = "SCORE";
                ac.advance().ok();
                ac.get().map(|v| rule_args.score_default = Some(v.clone()))
            } else if is("SCORE_FIELD") {
                name = "SCORE_FIELD";
                ac.advance().ok();
                ac.get().map(|v| rule_args.score_field = Some(v.clone()))
            } else if is("LANGUAGE") {
                name = "LANGUAGE";
                ac.advance().ok();
                ac.get().map(|v| rule_args.lang_default = Some(v.clone()))
            } else if is("LANGUAGE_FIELD") {
                name = "LANGUAGE_FIELD";
                ac.advance().ok();
                ac.get().map(|v| rule_args.lang_field = Some(v.clone()))
            } else if is("PAYLOAD_FIELD") {
                name = "PAYLOAD_FIELD";
                ac.advance().ok();
                ac.get().map(|v| rule_args.payload_field = Some(v.clone()))
            } else if is("INDEXALL") {
                name = "INDEXALL";
                ac.advance().ok();
                ac.get().map(|v| rule_args.index_all = Some(v.clone()))
            } else if is("TEMPORARY") {
                name = "TEMPORARY";
                ac.advance().ok();
                ac.get_ll(0).map(|v| timeout = Some(v))
            } else if is("STOPWORDS") {
                name = "STOPWORDS";
                ac.advance().ok();
                ac.get_varargs().map(|v| stopwords = Some(v))
            } else {
                break;
            };
            if let Err(e) = rc {
                return Err(QErr::bad_args(name, e));
            }
        }
        if flags & I_WIDE_SCHEMA != 0 && flags & I_FIELD_FLAGS == 0 {
            return Err(QErr::new(Code::Inval, "MAXTEXTFIELDS cannot be used with NOFIELDS"));
        }
        // `timeout != -1`.
        let timeout = timeout.filter(|&t| t != -1);
        if timeout.is_some() {
            flags |= I_TEMPORARY;
        }
        let rule = Rule::create(&rule_args, prefixes)?;
        let stopwords = stopwords.map(|sw| {
            flags |= I_CUSTOM_STOPWORDS;
            sw.rest().iter().map(|w| Bytes::from(super::tokenize::lowercase(crate::util::cstr(w)))).collect()
        });
        let mut spec = Spec {
            name,
            fields: Vec::new(),
            flags,
            rule,
            stopwords,
            timeout,
            num_sortables: 0,
            text_ids: Vec::new(),
            suffix_mask: 0,
        };
        if !ac.advance_if("SCHEMA") {
            return Err(match ac.peek() {
                Some(bad) => QErr::user(Code::ArgUnrecognized, "Unknown argument", format!(" `{}`", text(bad))),
                None => QErr::new(Code::ParseArgs, "No schema found"),
            });
        }
        spec.add_fields(&mut ac, true, cfg)?;
        Ok(spec)
    }

    /// `IndexSpec_AddFieldsInternal`: on failure the schema is left as it was.
    pub fn add_fields(&mut self, ac: &mut Ac, is_new: bool, cfg: &crate::config::Config) -> QResult<()> {
        if ac.at_end() {
            return Err(QErr::new(Code::ParseArgs, "Fields arguments are missing"));
        }
        let saved = self.clone();
        let r = self.add_fields_inner(ac, is_new, cfg);
        if r.is_err() {
            *self = saved;
        }
        r
    }

    fn add_fields_inner(&mut self, ac: &mut Ac, is_new: bool, cfg: &crate::config::Config) -> QResult<()> {
        while !ac.at_end() {
            if self.fields.len() == MAX_FIELDS {
                return Err(QErr::new(Code::Limit, format!("Schema is limited to {MAX_FIELDS} fields")));
            }
            let path_raw = ac.take().cloned().unwrap_or_default();
            let (name, path) = if ac.advance_if("AS") {
                let Some(name) = ac.take() else {
                    return Err(QErr::new(Code::ParseArgs, "AS requires an argument"));
                };
                self.flags |= I_FIELD_ALIAS;
                (name.clone(), Some(path_raw))
            } else {
                (path_raw, None)
            };
            if name.is_empty() {
                return Err(QErr::new(Code::Inval, "Field name cannot be empty"));
            }
            if name.contains(&0) {
                return Err(QErr::new(Code::Inval, "Field name cannot contain null bytes"));
            }
            if path.as_ref().is_some_and(|p| p.contains(&0)) {
                return Err(QErr::new(Code::Inval, "Field path cannot contain null bytes"));
            }
            if self.field(&name).is_some() {
                return Err(QErr::user(Code::Inval, "Duplicate field in schema", format!(" - {}", text(&name))));
            }
            let mut fs = Field::new(name, path, self.fields.len());
            self.parse_field(ac, &mut fs, cfg)?;
            if fs.is(T_TEXT) && fs.indexable() {
                let id = self.text_ids.len();
                if id >= MAX_TEXT_FIELDS {
                    return Err(QErr::new(Code::Limit, format!("Schema is limited to {MAX_TEXT_FIELDS} TEXT fields")));
                }
                self.text_ids.push(fs.index);
                if id >= WIDE_THRESHOLD && self.flags & I_FIELD_FLAGS != 0 {
                    if is_new {
                        self.flags |= I_WIDE_SCHEMA;
                    } else if self.flags & I_WIDE_SCHEMA == 0 {
                        return Err(QErr::new(
                            Code::Limit,
                            "Cannot add more fields. Declare index with wide fields to allow adding unlimited fields",
                        ));
                    }
                }
                fs.ft_id = Some(id);
                if self.is_json() && self.flags & I_FIELD_ALIAS != 0 && self.flags & I_TERM_OFFSETS != 0 {
                    match super::jsonpath::parse(fs.path()) {
                        Ok(p) if p.defined_order() => fs.options &= !F_UNDEFINED_ORDER,
                        Ok(_) | Err(None) => {
                            fs.options |= F_UNDEFINED_ORDER;
                            self.flags |= I_UNDEFINED_ORDER;
                        }
                        Err(Some(_)) => return Err(json_path_error(&fs, &self.name)),
                    }
                }
            }
            if fs.sortable() {
                if self.is_json() {
                    fs.options |= F_UNF;
                }
                fs.sort_idx = self.num_sortables as i32;
                self.num_sortables += 1;
            }
            if fs.has(F_PHONETICS) {
                self.flags |= I_PHONETIC;
            }
            if fs.is(T_TEXT) && fs.indexable() && fs.has(F_SUFFIX_TRIE) {
                self.suffix_mask |= 1u128 << fs.ft_id.unwrap_or(0);
                self.flags |= I_SUFFIX_TRIE;
            }
            self.fields.push(fs);
        }
        Ok(())
    }

    /// `parseFieldSpec`.
    fn parse_field(&mut self, ac: &mut Ac, fs: &mut Field, cfg: &crate::config::Config) -> QResult<()> {
        if ac.at_end() {
            return Err(QErr::user(Code::ParseArgs, "Field", format!(" `{}` does not have a type", fs.name_str())));
        }
        if ac.advance_if("TEXT") {
            parse_text(ac, fs)?;
            if !fs.has(F_INDEX_EMPTY) {
                self.flags |= I_NON_EMPTY;
            }
        } else if ac.advance_if("TAG") {
            parse_tag(ac, fs)?;
            if !fs.has(F_INDEX_EMPTY) {
                self.flags |= I_NON_EMPTY;
            }
        } else if ac.advance_if("GEOSHAPE") {
            fs.types |= T_GEOMETRY;
            self.flags |= I_GEOMETRY;
            if ac.advance_if("FLAT") {
                fs.coords = Coords::Flat;
            } else {
                ac.advance_if("SPHERICAL");
                fs.coords = Coords::Spherical;
            }
            if ac.advance_if("INDEXMISSING") {
                fs.options |= F_INDEX_MISSING;
            }
        } else if ac.advance_if("VECTOR") {
            self.parse_vector(ac, fs, cfg)?;
            // No SORTABLE / NOINDEX for vectors.
            return Ok(());
        } else if ac.advance_if("NUMERIC") {
            fs.types |= T_NUMERIC;
            if ac.advance_if("INDEXMISSING") {
                fs.options |= F_INDEX_MISSING;
            }
        } else if ac.advance_if("GEO") {
            fs.types |= T_GEO;
            if ac.advance_if("INDEXMISSING") {
                fs.options |= F_INDEX_MISSING;
            }
        } else {
            return Err(QErr::user(Code::ParseArgs, "Invalid field type for field", format!(" `{}`", fs.name_str())));
        }
        while !ac.at_end() {
            if ac.advance_if("SORTABLE") {
                fs.options |= F_SORTABLE;
                if ac.advance_if("UNF") || fs.is(T_NUMERIC) || (fs.is(T_TAG) && fs.tag_case_sensitive) {
                    fs.options |= F_UNF;
                }
            } else if ac.advance_if("NOINDEX") {
                fs.options |= F_NOINDEX;
            } else {
                break;
            }
        }
        if !fs.indexable() && fs.has(F_INDEX_MISSING) {
            return Err(QErr::user(
                Code::ParseArgs,
                "'Field cannot be defined with both `NOINDEX` and `INDEXMISSING`",
                format!(" `{}` '", fs.name_str()),
            ));
        }
        Ok(())
    }

    /// `parseVectorField`.
    fn parse_vector(&mut self, ac: &mut Ac, fs: &mut Field, cfg: &crate::config::Config) -> QResult<()> {
        fs.types |= T_VECTOR;
        self.flags |= I_VECSIM;
        let mut multi = false;
        if self.is_json() {
            match super::jsonpath::parse(fs.path()) {
                Ok(p) => multi = !p.is_single(),
                Err(Some(_)) => return Err(json_path_error(fs, &self.name)),
                Err(None) => {
                    return Err(QErr::user(Code::ParseArgs, "Could not parse schema for field", format!(" `{}`", fs.name_str())));
                }
            }
        }
        let algo = match ac.get() {
            Err(e) => {
                return Err(QErr::user(Code::ParseArgs, "Bad arguments", format!(" for vector similarity algorithm: {}", e.strerror())));
            }
            Ok(a) => crate::util::cstr(a),
        };
        let mut p = if algo.eq_ignore_ascii_case(b"FLAT") {
            let mut p = VectorParams::new(Algo::Flat, multi);
            parse_flat(ac, &mut p)?;
            p
        } else if algo.eq_ignore_ascii_case(b"HNSW") {
            let mut p = VectorParams::new(Algo::Hnsw, multi);
            parse_hnsw(ac, &mut p)?;
            p
        } else if algo.eq_ignore_ascii_case(b"SVS-VAMANA") {
            let mut p = VectorParams::new(Algo::Svs, multi);
            parse_svs(ac, &mut p)?;
            if p.quant != Quant::None && p.training_threshold == 0 {
                p.training_threshold = 10 * 1024;
            }
            if p.quant.leanvec() && p.reduce == 0 {
                p.reduce = p.dim / 2;
            }
            p
        } else {
            return Err(QErr::user(Code::ParseArgs, "Bad arguments", format!(" for vector similarity algorithm: {}", AcErr::NoEnt.strerror())));
        };
        validate_vector(&mut p, cfg)?;
        fs.vector = Some(p);
        if ac.advance_if("INDEXMISSING") {
            fs.options |= F_INDEX_MISSING;
        }
        Ok(())
    }
}

/// `JSONParse_error`.
fn json_path_error(fs: &Field, index: &[u8]) -> QErr {
    QErr::user(Code::InvalPath, "Invalid JSONPath", format!(" '{}' in attribute '{}' in index '{}'", text(fs.path()), fs.name_str(), text(index)))
}

/// `parseTextField`.
fn parse_text(ac: &mut Ac, fs: &mut Field) -> QResult<()> {
    fs.types |= T_TEXT;
    while !ac.at_end() {
        if ac.advance_if("NOSTEM") {
            fs.options |= F_NOSTEM;
        } else if ac.advance_if("WEIGHT") {
            match ac.get_double(0) {
                Ok(d) => fs.weight = d,
                Err(e) => return Err(QErr::user(Code::ParseArgs, "Bad arguments", format!(" for weight: {}", e.strerror()))),
            }
        } else if ac.advance_if("PHONETIC") {
            let Some(m) = ac.take() else {
                return Err(QErr::new(Code::ParseArgs, "PHONETIC requires an argument"));
            };
            let m = crate::util::cstr(m);
            let ok = m.len() == 5 && m.starts_with(b"dm:") && [b"en", b"pt", b"fr", b"es"].iter().any(|l| &m[3..] == *l);
            if !ok {
                return Err(QErr::new(
                    Code::Inval,
                    "Matcher Format: <2 chars algorithm>:<2 chars language>. Support algorithms: double metaphone (dm). Supported languages: English (en), French (fr), Portuguese (pt) and Spanish (es)",
                ));
            }
            fs.options |= F_PHONETICS;
        } else if ac.advance_if("WITHSUFFIXTRIE") {
            fs.options |= F_SUFFIX_TRIE;
        } else if ac.advance_if("INDEXEMPTY") {
            fs.options |= F_INDEX_EMPTY;
        } else if ac.advance_if("INDEXMISSING") {
            fs.options |= F_INDEX_MISSING;
        } else {
            break;
        }
    }
    Ok(())
}

/// `parseTagField`.
fn parse_tag(ac: &mut Ac, fs: &mut Field) -> QResult<()> {
    fs.types |= T_TAG;
    while !ac.at_end() {
        if ac.advance_if("SEPARATOR") {
            let Some(sep) = ac.take() else {
                return Err(QErr::new(Code::ParseArgs, "SEPARATOR requires an argument"));
            };
            let sep = crate::util::cstr(sep);
            if sep.len() != 1 {
                return Err(QErr::user(
                    Code::ParseArgs,
                    &format!("Tag separator must be a single character. Got `{}`", text(sep)),
                    "",
                ));
            }
            fs.tag_sep = sep[0];
        } else if ac.advance_if("CASESENSITIVE") {
            fs.tag_case_sensitive = true;
        } else if ac.advance_if("WITHSUFFIXTRIE") {
            fs.options |= F_SUFFIX_TRIE;
        } else if ac.advance_if("INDEXEMPTY") {
            fs.options |= F_INDEX_EMPTY;
        } else if ac.advance_if("INDEXMISSING") {
            fs.options |= F_INDEX_MISSING;
        } else {
            break;
        }
    }
    Ok(())
}

fn algo_param(algo: &str, param: &str) -> String {
    format!("vector similarity {algo} index `{param}`")
}

fn mandatory(algo: &str, arg: &str) -> QErr {
    QErr::user(Code::ParseArgs, "Missing mandatory parameter: cannot create", format!(" {algo} index without specifying {arg} argument"))
}

/// `parseVectorField_hnsw`: a count, then exactly that many arguments.
fn parse_hnsw(ac: &mut Ac, p: &mut VectorParams) -> QResult<()> {
    const A: &str = "HNSW";
    let n = ac.get_size(0).map_err(|e| {
        QErr::user(Code::ParseArgs, "Bad arguments", format!(" for vector similarity number of parameters: {}", e.strerror()))
    })?;
    let mut sub = ac
        .get_slice(n as usize)
        .map_err(|_| QErr::user(Code::ParseArgs, "Bad arguments", " for vector similarity: not enough arguments"))?;
    let (mut ty, mut dim, mut metric) = (false, false, false);
    let bad = |param: &str, e: AcErr| QErr::bad_args(&algo_param(A, param), e);
    while !sub.at_end() {
        if sub.advance_if("TYPE") {
            p.ty = VecType::parse(&mut sub).map_err(|e| bad("TYPE", e))?;
            ty = true;
        } else if sub.advance_if("DIM") {
            p.dim = sub.get_size(GE1).map_err(|e| bad("DIM", e))? as usize;
            dim = true;
        } else if sub.advance_if("DISTANCE_METRIC") {
            p.metric = Metric::parse(&mut sub).map_err(|e| bad("DISTANCE_METRIC", e))?;
            metric = true;
        } else if sub.advance_if("INITIAL_CAP") {
            p.initial_cap = Some(sub.get_size(0).map_err(|e| bad("INITIAL_CAP", e))? as usize);
        } else if sub.advance_if("M") {
            p.m = sub.get_size(GE1).map_err(|e| bad("M", e))? as usize;
        } else if sub.advance_if("EF_CONSTRUCTION") {
            p.ef_construction = sub.get_size(GE1).map_err(|e| bad("EF_CONSTRUCTION", e))? as usize;
        } else if sub.advance_if("EF_RUNTIME") {
            p.ef_runtime = sub.get_size(GE1).map_err(|e| bad("EF_RUNTIME", e))? as usize;
        } else if sub.advance_if("EPSILON") {
            p.epsilon = sub.get_double(GE0).map_err(|e| bad("EPSILON", e))?;
        } else if sub.advance_if("RERANK") {
            return Err(QErr::new(Code::Inval, "RERANK is only supported for disk-based vector indexes"));
        } else {
            let a = sub.take().map(|a| text(a)).unwrap_or_default();
            return Err(QErr::user(Code::ParseArgs, "Bad arguments for algorithm", format!(" {A}: {a}")));
        }
    }
    if !ty {
        return Err(mandatory(A, "TYPE"));
    }
    if !dim {
        return Err(mandatory(A, "DIM"));
    }
    if !metric {
        return Err(mandatory(A, "DISTANCE_METRIC"));
    }
    Ok(())
}

/// The pair count of FLAT and SVS: even, halved.
fn pair_count(ac: &mut Ac, svs: bool) -> QResult<u64> {
    let n = match ac.get_size(0) {
        Ok(n) => n,
        Err(e) if svs => return Err(QErr::bad_args("vector similarity number of parameters", e)),
        Err(e) => {
            return Err(QErr::user(Code::ParseArgs, "Bad arguments", format!(" for vector similarity number of parameters: {}", e.strerror())));
        }
    };
    if n % 2 == 1 {
        let (msg, user) = if svs {
            ("Bad number of arguments for vector similarity index:", format!(" got {n} but expected even number as algorithm parameters (should be submitted as named arguments)"))
        } else {
            ("Bad number of arguments for vector similarity index", format!(": got {n} but expected even number as algorithm parameters (should be submitted as named arguments)"))
        };
        return Err(QErr::user(Code::ParseArgs, msg, user));
    }
    Ok(n / 2)
}

/// `parseVectorField_flat`.
fn parse_flat(ac: &mut Ac, p: &mut VectorParams) -> QResult<()> {
    const A: &str = "FLAT";
    let exp = pair_count(ac, false)?;
    let mut got = 0u64;
    let (mut ty, mut dim, mut metric) = (false, false, false);
    let bad = |param: &str, e: AcErr| QErr::bad_args(&algo_param(A, param), e);
    while exp > got && !ac.at_end() {
        if ac.advance_if("TYPE") {
            p.ty = VecType::parse(ac).map_err(|e| bad("TYPE", e))?;
            ty = true;
        } else if ac.advance_if("DIM") {
            p.dim = ac.get_size(GE1).map_err(|e| bad("DIM", e))? as usize;
            dim = true;
        } else if ac.advance_if("DISTANCE_METRIC") {
            p.metric = Metric::parse(ac).map_err(|e| bad("DISTANCE_METRIC", e))?;
            metric = true;
        } else if ac.advance_if("INITIAL_CAP") {
            p.initial_cap = Some(ac.get_size(0).map_err(|e| bad("INITIAL_CAP", e))? as usize);
        } else if ac.advance_if("BLOCK_SIZE") {
            p.block_size = ac.get_size(GE1).map_err(|e| bad("BLOCK_SIZE", e))? as usize;
        } else {
            let a = ac.take().map(|a| text(a)).unwrap_or_default();
            return Err(QErr::user(Code::ParseArgs, "Bad arguments for algorithm", format!(" {A}: {a}")));
        }
        got += 1;
    }
    if exp > got {
        return Err(QErr::new(Code::ParseArgs, format!("Expected {} parameters but got {}", exp * 2, got * 2)));
    }
    if !ty {
        return Err(mandatory(A, "TYPE"));
    }
    if !dim {
        return Err(mandatory(A, "DIM"));
    }
    if !metric {
        return Err(mandatory(A, "DISTANCE_METRIC"));
    }
    Ok(())
}

/// `parseVectorField_svs`.
fn parse_svs(ac: &mut Ac, p: &mut VectorParams) -> QResult<()> {
    const A: &str = "SVS-VAMANA";
    let exp = pair_count(ac, true)?;
    let mut got = 0u64;
    let (mut ty, mut dim, mut metric) = (false, false, false);
    let bad = |param: &str, e: AcErr| QErr::bad_args(&algo_param(A, param), e);
    while exp > got && !ac.at_end() {
        if ac.advance_if("TYPE") {
            p.ty = VecType::parse(ac).map_err(|e| bad("TYPE", e))?;
            if !matches!(p.ty, VecType::Float16 | VecType::Float32) {
                return Err(QErr::user(Code::ParseArgs, "Not supported data type is given. ", "Expected: FLOAT16, FLOAT32"));
            }
            ty = true;
        } else if ac.advance_if("DIM") {
            p.dim = ac.get_size(GE1).map_err(|e| bad("DIM", e))? as usize;
            dim = true;
        } else if ac.advance_if("DISTANCE_METRIC") {
            p.metric = Metric::parse(ac).map_err(|e| bad("DISTANCE_METRIC", e))?;
            metric = true;
        } else if ac.advance_if("GRAPH_MAX_DEGREE") {
            p.graph_max_degree = ac.get_size(GE1).map_err(|e| bad("GRAPH_MAX_DEGREE", e))? as usize;
        } else if ac.advance_if("CONSTRUCTION_WINDOW_SIZE") {
            p.construction_window_size = ac.get_size(GE1).map_err(|e| bad("CONSTRUCTION_WINDOW_SIZE", e))? as usize;
        } else if ac.advance_if("COMPRESSION") {
            let q = ac.get().map_err(|e| bad("COMPRESSION", e))?;
            let q = crate::util::cstr(q);
            let eq = |n: &str| q.eq_ignore_ascii_case(n.as_bytes());
            p.quant = if eq("LVQ8") {
                Quant::Lvq8
            } else if eq("LVQ4") {
                Quant::Lvq4
            } else if eq("LVQ4x4") {
                Quant::Lvq4x4
            } else if eq("LVQ4x8") {
                Quant::Lvq4x8
            } else if eq("LeanVec4x8") {
                Quant::LeanVec4x8
            } else if eq("LeanVec8x8") {
                Quant::LeanVec8x8
            } else {
                return Err(bad("COMPRESSION", AcErr::NoEnt));
            };
        } else if ac.advance_if("SEARCH_WINDOW_SIZE") {
            p.search_window_size = ac.get_size(GE1).map_err(|e| bad("SEARCH_WINDOW_SIZE", e))? as usize;
        } else if ac.advance_if("EPSILON") {
            p.epsilon = ac.get_double(GE0).map_err(|e| bad("EPSILON", e))?;
        } else if ac.advance_if("REDUCE") {
            p.reduce = ac.get_size(GE1).map_err(|e| bad("REDUCE", e))? as usize;
        } else if ac.advance_if("TRAINING_THRESHOLD") {
            let t = ac.get_size(GE1).map_err(|e| bad("TRAINING_THRESHOLD", e))? as usize;
            if t < 1024 {
                return Err(QErr::user(Code::ParseArgs, "Invalid TRAINING_THRESHOLD: cannot be lower than DEFAULT_BLOCK_SIZE ", "(1024)"));
            }
            p.training_threshold = t;
        } else {
            let a = ac.take().map(|a| text(a)).unwrap_or_default();
            return Err(QErr::user(Code::ParseArgs, "Bad arguments for algorithm", format!(" {A}: {a}")));
        }
        got += 1;
    }
    if exp > got {
        return Err(QErr::new(Code::ParseArgs, format!("Expected {} parameters but got {}", exp * 2, got * 2)));
    }
    if !ty {
        return Err(mandatory(A, "TYPE"));
    }
    if !dim {
        return Err(mandatory(A, "DIM"));
    }
    if !metric {
        return Err(mandatory(A, "DISTANCE_METRIC"));
    }
    if p.quant == Quant::None && p.training_threshold > 0 {
        return Err(QErr::new(Code::ParseArgs, "TRAINING_THRESHOLD is irrelevant when compression was not requested"));
    }
    if !p.quant.leanvec() && p.reduce > 0 {
        return Err(QErr::new(Code::ParseArgs, "REDUCE is irrelevant when compression is not of type LeanVec"));
    }
    Ok(())
}

/// `VecSimIndex_validate_params`: the block size the memory limit allows.
fn validate_vector(p: &mut VectorParams, cfg: &crate::config::Config) -> QResult<()> {
    let element = super::vector::element_size(p);
    let limit = {
        let resize = cfg.num("search-vss-max-resize") as usize;
        if resize > 0 {
            resize
        } else {
            let mem = super::vector::memory_limit(cfg);
            if mem == 0 { usize::MAX / 10 } else { mem / 10 }
        }
    };
    let max_block = limit / element.max(1);
    p.block_size = max_block.min(1024);
    if p.block_size == 0 {
        return Err(QErr::user(
            Code::Limit,
            "Vector index element size",
            format!(" {element} exceeded maximum size allowed by server limit which is {max_block}"),
        ));
    }
    Ok(())
}
