//! The query tree RediSearch's parsers build (`QueryNode`: `query.c`, `query_param.c`,
//! `ext/default.c`): the nodes and their options, parameters and their resolution, attributes
//! (`=>{$weight: …}`), validation, query expansion (synonyms, phonetics, stemming) and
//! FT.EXPLAIN's dump.

use std::collections::HashMap;

use bytes::Bytes;

use super::error::{Code, QErr};
use super::index::Index;
use super::qlex::{QType, Token};
use super::spec::{self, Field, Spec};
use super::strconv::{self, fmt_f};
use crate::util::{cstr, fmt_g};

/// `t_fieldMask`: a bit per TEXT field id.
pub type Mask = u128;
pub const MASK_ALL: Mask = u128::MAX;

/// `QueryNodeFlags`.
pub const N_VERBATIM: u32 = 0x01;
pub const N_OVERRIDDEN_INORDER: u32 = 0x02;
pub const N_YIELDS_DISTANCE: u32 = 0x04;
pub const N_INDEXES_EMPTY: u32 = 0x08;
pub const N_IS_TAG: u32 = 0x10;
pub const N_HYBRID_VECTOR_SUBQUERY: u32 = 0x20;

/// `RSSearchFlags` (validation ORs node flags into these, so `N_INDEXES_EMPTY` and
/// `S_CAN_SKIP_RICH_RESULTS` share a bit, as in the C).
pub const S_VERBATIM: u32 = 0x01;
pub const S_NO_STOPWORDS: u32 = 0x02;
pub const S_IN_ORDER: u32 = 0x04;
pub const S_CAN_SKIP_RICH_RESULTS: u32 = 0x08;

/// `QASTValidationFlags`.
pub const V_NO_WEIGHT: u32 = 0x01;
pub const V_NO_VECTOR: u32 = 0x02;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phonetic {
    #[default]
    Default,
    Enabled,
    Disabled,
}

/// `QueryNodeOptions`.
#[derive(Clone, Debug, PartialEq)]
pub struct Opts {
    pub field_mask: Mask,
    pub flags: u32,
    pub max_slop: i64,
    pub in_order: bool,
    pub weight: f64,
    pub explicit_weight: bool,
    pub phonetic: Phonetic,
    pub dist_field: Option<Vec<u8>>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { field_mask: MASK_ALL, flags: 0, max_slop: -1, in_order: false, weight: 1.0, explicit_weight: false, phonetic: Phonetic::Default, dist_field: None }
    }
}

/// `RSToken`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tok {
    pub s: Vec<u8>,
    pub expanded: bool,
    pub flags: u32,
}

impl Tok {
    pub fn new(s: Vec<u8>) -> Self {
        Tok { s, expanded: false, flags: 0 }
    }
}

/// `NumericFilter` (the parts a query sets).
#[derive(Clone, Debug, PartialEq)]
pub struct NumFilter {
    /// The field, by schema position.
    pub field: Option<usize>,
    pub min: f64,
    pub max: f64,
    pub min_inclusive: bool,
    pub max_inclusive: bool,
}

impl NumFilter {
    /// The tree search's filter: ascending, unlimited.
    pub fn filter(&self) -> super::numtree::NumericFilter {
        super::numtree::NumericFilter { min: self.min, max: self.max, min_inclusive: self.min_inclusive, max_inclusive: self.max_inclusive, ..Default::default() }
    }
}

/// `GeoDistance`: -1 invalid, then km, m, ft, mi.
pub const GEO_INVALID: i32 = -1;
pub const GEO_KM: i32 = 0;
const GEO_UNITS: [&str; 4] = ["km", "m", "ft", "mi"];

/// `GeoDistance_Parse` (any case).
pub fn geo_unit(s: &[u8]) -> i32 {
    let s = cstr(s);
    GEO_UNITS.iter().position(|u| s.eq_ignore_ascii_case(u.as_bytes())).map_or(GEO_INVALID, |i| i as i32)
}

/// `GeoDistance_Parse_Buffer`: units of 16 bytes or more are invalid.
pub fn geo_unit_buffer(s: &[u8]) -> i32 {
    if s.len() < 16 { geo_unit(s) } else { GEO_INVALID }
}

/// `GeoDistance_ToString`.
pub fn geo_unit_name(u: i32) -> &'static str {
    usize::try_from(u).ok().and_then(|i| GEO_UNITS.get(i)).copied().unwrap_or("<badunit>")
}

/// `GeoFilter`.
#[derive(Clone, Debug, PartialEq)]
pub struct GeoFilter {
    pub field: Option<usize>,
    pub lon: f64,
    pub lat: f64,
    pub radius: f64,
    pub unit: i32,
}

impl GeoFilter {
    /// `GeoFilter_Validate`.
    pub fn validate(&self) -> Result<(), QErr> {
        let syntax = |m: &str| Err(QErr::new(Code::Syntax, m));
        if self.unit == GEO_INVALID {
            return syntax("Invalid GeoFilter unit");
        }
        if self.lat > 90.0 || self.lat < -90.0 || self.lon > 180.0 || self.lon < -180.0 {
            return syntax("Invalid GeoFilter lat/lon");
        }
        if self.radius <= 0.0 {
            return syntax("Invalid GeoFilter radius");
        }
        Ok(())
    }
}

/// `VectorQuery`.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorQuery {
    pub knn: bool,
    pub field: Option<usize>,
    /// KNN's `k`.
    pub k: u64,
    /// RANGE's radius.
    pub radius: f64,
    /// The query vector (always a parameter's value).
    pub blob: Bytes,
    /// KNN results ordered by score (a pure vector query) rather than by id.
    pub by_score: bool,
    pub shard_window_ratio: f64,
    pub score_field: Option<Vec<u8>>,
    /// The raw `NAME value` parameters (`VecSimRawParam`), in order, and whether each value
    /// still names a query parameter.
    pub params: Vec<(Vec<u8>, Vec<u8>)>,
    pub need_resolve: Vec<bool>,
}

/// The node types (`QueryNodeType`) and their data.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Phrase { exact: bool },
    Union,
    Token(Tok),
    Numeric(NumFilter),
    Not,
    Optional,
    Geo(GeoFilter),
    /// Geometry predicate (`QueryType`: 1 CONTAINS, 2 WITHIN, 3 DISJOINT, 4 INTERSECTS) and WKT.
    Geometry { query_type: i32, wkt: Vec<u8>, field: Option<usize> },
    Prefix { tok: Tok, prefix: bool, suffix: bool },
    Ids(Vec<Bytes>),
    Wildcard,
    Tag { field: Option<usize> },
    Fuzzy { tok: Tok, max_dist: u8 },
    LexRange { begin: Option<Vec<u8>>, include_begin: bool, end: Option<Vec<u8>>, include_end: bool },
    Vector(Box<VectorQuery>),
    WildcardQuery(Tok),
    Null,
    Missing { field: Option<usize> },
}

/// `ParamType` (without `PARAM_NONE`: only real parameters are kept).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PType {
    Any,
    Term,
    TermCase,
    Size,
    Numeric,
    NumMinRange,
    NumMaxRange,
    GeoCoord,
    GeoUnit,
    Vec,
    Wildcard,
}

/// Which value of its node a parameter fills in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The token, prefix, fuzzy or wildcard text.
    Str,
    Min,
    Max,
    Lon,
    Lat,
    Radius,
    Unit,
    VecBlob,
    K,
    VecRadius,
    Wkt,
    ScoreField,
}

/// `Param`.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub name: Vec<u8>,
    pub ty: PType,
    pub target: Target,
    pub sign: i32,
}

/// `QueryNode`.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub kind: Kind,
    pub opts: Opts,
    pub params: Vec<Param>,
    pub children: Vec<Node>,
}

/// A value a token gives a node's field right away, when it isn't a parameter.
pub enum Val {
    Str(Vec<u8>),
    Num(f64),
    Size(u64),
}

/// `rm_normalize`: unescape (a backslash before punctuation or a space goes, repeatedly), stop
/// at a NUL, lowercase.
pub fn normalize(s: &[u8]) -> Vec<u8> {
    let s = cstr(s);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' && s.get(i + 1).is_some_and(|&c| c.is_ascii_punctuation() || is_space(c)) {
            i += 1;
            continue;
        }
        out.push(s[i]);
        i += 1;
    }
    let l = super::tokenize::lowercase(&out);
    cstr(&l).to_vec()
}

/// `rm_strndup_unescape`: like [`normalize`] without lowercasing.
pub fn unescape(s: &[u8]) -> Vec<u8> {
    let s = cstr(s);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' && s.get(i + 1).is_some_and(|&c| c.is_ascii_punctuation() || is_space(c)) {
            i += 1;
            continue;
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// A scanned token with its text read out of the query buffer, as the node constructors see
/// `QueryToken`.
#[derive(Clone, Debug)]
pub struct TextTok {
    pub tok: Token,
    pub s: Vec<u8>,
}

impl std::ops::Deref for TextTok {
    type Target = Token;

    fn deref(&self) -> &Token {
        &self.tok
    }
}

/// `QueryParam_SetParam`: a concrete token gives its value now; a parameter token records a
/// [`Param`] on `params` (counted in `num_params`) and gives nothing yet.
pub fn set_param(params: &mut Vec<Param>, num_params: &mut usize, target: Target, t: &TextTok) -> Option<Val> {
    let ty = match t.qtype {
        QType::Term => return Some(Val::Str(normalize(&t.s))),
        QType::TermCase => return Some(Val::Str(cstr(&t.s).to_vec())),
        QType::Numeric => return Some(Val::Num(t.numval)),
        QType::Size => return Some(Val::Size(size_cast(t.numval))),
        QType::Wildcard => return Some(Val::Str(cstr(&t.s).to_vec())),
        QType::ParamAny => PType::Any,
        QType::ParamTerm => PType::Term,
        QType::ParamTermCase => PType::TermCase,
        QType::ParamNumeric => PType::Numeric,
        QType::ParamNumericMinRange => PType::NumMinRange,
        QType::ParamNumericMaxRange => PType::NumMaxRange,
        QType::ParamGeoUnit => PType::GeoUnit,
        QType::ParamGeoCoord => PType::GeoCoord,
        QType::ParamVec => PType::Vec,
        QType::ParamSize => PType::Size,
        QType::ParamWildcard => PType::Wildcard,
    };
    params.push(Param { name: cstr(&t.s).to_vec(), ty, target, sign: t.sign });
    *num_params += 1;
    None
}

/// `(size_t)double`, as the C converts on x86-64/arm64 for the values a SIZE token holds.
fn size_cast(v: f64) -> u64 {
    if v.is_nan() || v <= -1.0 {
        if v.is_nan() { 0 } else { (v as i64) as u64 }
    } else {
        v as u64
    }
}

impl Node {
    /// `NewQueryNode`.
    pub fn new(kind: Kind) -> Self {
        Node { kind, opts: Opts::default(), params: Vec::new(), children: Vec::new() }
    }

    /// `NewQueryNodeChildren` (`NewNotNode`, `NewOptionalNode`).
    pub fn with_child(kind: Kind, child: Node) -> Self {
        let mut n = Node::new(kind);
        n.children.push(child);
        n
    }

    pub fn phrase(exact: bool) -> Self {
        Node::new(Kind::Phrase { exact })
    }

    /// `NewTokenNode`: already normalized (or verbatim) text.
    pub fn token(s: Vec<u8>) -> Self {
        Node::new(Kind::Token(Tok::new(cstr(&s).to_vec())))
    }

    /// `NewTokenNodeExpanded`.
    pub fn expanded(s: Vec<u8>, flags: u32) -> Self {
        Node::new(Kind::Token(Tok { s, expanded: true, flags }))
    }

    pub fn is_phrase(&self, exact: bool) -> bool {
        matches!(self.kind, Kind::Phrase { exact: e } if e == exact)
    }

    pub fn is_union(&self) -> bool {
        matches!(self.kind, Kind::Union)
    }

    pub fn is_not(&self) -> bool {
        matches!(self.kind, Kind::Not)
    }

    /// `QueryNode_AddChildren`: a TAG node keeps only token, phrase, prefix, lexrange and
    /// wildcard children, and makes their parameters case-sensitive.
    pub fn add_child(&mut self, mut ch: Node) {
        if matches!(self.kind, Kind::Tag { .. }) {
            if matches!(ch.kind, Kind::Token(_) | Kind::Phrase { .. } | Kind::Prefix { .. } | Kind::LexRange { .. } | Kind::WildcardQuery(_)) {
                for p in &mut ch.params {
                    p.ty = PType::TermCase;
                }
                self.children.push(ch);
            }
        } else {
            self.children.push(ch);
        }
    }

    /// `QueryNode_SetFieldMask`: AND the mask into the node and all its descendants.
    pub fn set_field_mask(&mut self, mask: Mask) {
        self.opts.field_mask &= mask;
        for c in &mut self.children {
            c.set_field_mask(mask);
        }
    }
}

/// `NewTokenNode_WithParams`.
pub fn token_node(t: &TextTok, num_params: &mut usize) -> Node {
    let mut n = Node::new(Kind::Token(Tok::default()));
    match t.qtype {
        QType::Term | QType::TermCase | QType::Numeric | QType::Size => {
            let s = if t.qtype == QType::Term { normalize(&t.s) } else { cstr(&t.s).to_vec() };
            n.kind = Kind::Token(Tok::new(s));
            if matches!(t.qtype, QType::Numeric | QType::Size) {
                n.opts.flags |= N_VERBATIM;
            }
        }
        _ => {
            set_param(&mut n.params, num_params, Target::Str, t);
        }
    }
    n
}

/// `NewPrefixNode_WithParams`.
pub fn prefix_node(t: &TextTok, prefix: bool, suffix: bool, num_params: &mut usize) -> Node {
    let mut n = Node::new(Kind::Prefix { tok: Tok::default(), prefix, suffix });
    if t.qtype == QType::Term {
        n.kind = Kind::Prefix { tok: Tok::new(unescape(&t.s)), prefix, suffix };
    } else {
        set_param(&mut n.params, num_params, Target::Str, t);
    }
    n
}

/// `NewWildcardNode_WithParams`.
pub fn wildcard_node(t: &TextTok, num_params: &mut usize) -> Node {
    let mut n = Node::new(Kind::WildcardQuery(Tok::default()));
    if t.qtype == QType::Wildcard {
        n.kind = Kind::WildcardQuery(Tok::new(t.s.clone()));
    } else {
        set_param(&mut n.params, num_params, Target::Str, t);
        if let Some(p) = n.params.last_mut() {
            p.ty = PType::Wildcard;
        }
    }
    n
}

/// `NewFuzzyNode_WithParams`.
pub fn fuzzy_node(t: &TextTok, max_dist: u8, num_params: &mut usize) -> Node {
    let mut n = Node::new(Kind::Fuzzy { tok: Tok::default(), max_dist });
    if matches!(t.qtype, QType::Term | QType::Numeric | QType::Size) {
        n.kind = Kind::Fuzzy { tok: Tok::new(normalize(&t.s)), max_dist };
    } else {
        set_param(&mut n.params, num_params, Target::Str, t);
    }
    n
}

/// A numeric filter under construction with its parameters (`QueryParam` of
/// `QP_NUMERIC_FILTER`).
#[derive(Clone, Debug)]
pub struct NumParam {
    pub nf: NumFilter,
    pub params: Vec<Param>,
}

/// `NewNumericFilterQueryParam_WithParams`: a missing bound is infinite.
pub fn numeric_param(min: Option<&TextTok>, max: Option<&TextTok>, min_inclusive: bool, max_inclusive: bool, num_params: &mut usize) -> NumParam {
    let mut p = NumParam { nf: NumFilter { field: None, min: 0.0, max: 0.0, min_inclusive, max_inclusive }, params: Vec::new() };
    match min {
        Some(t) => {
            if let Some(v) = set_param(&mut p.params, num_params, Target::Min, t) {
                p.nf.min = val_num(v);
            }
        }
        None => p.nf.min = f64::NEG_INFINITY,
    }
    match max {
        Some(t) => {
            if let Some(v) = set_param(&mut p.params, num_params, Target::Max, t) {
                p.nf.max = val_num(v);
            }
        }
        None => p.nf.max = f64::INFINITY,
    }
    p
}

fn val_num(v: Val) -> f64 {
    match v {
        Val::Num(n) => n,
        Val::Size(s) => s as f64,
        // A term token's text read as the double the C's union would hold: not reachable from
        // the grammar (numeric operands are numbers or parameters).
        Val::Str(_) => 0.0,
    }
}

/// `NewNumericNode`.
pub fn numeric_node(p: NumParam, field: Option<usize>) -> Node {
    let mut nf = p.nf;
    nf.field = field;
    let mut n = Node::new(Kind::Numeric(nf));
    n.params = p.params;
    n
}

/// A geo filter under construction (`QueryParam` of `QP_GEO_FILTER`).
#[derive(Clone, Debug)]
pub struct GeoParam {
    pub gf: GeoFilter,
    pub params: Vec<Param>,
}

/// `NewGeoFilterQueryParam_WithParams`: the filter, and the error a bad unit token sets (the
/// filter is kept either way).
pub fn geo_param(lon: &TextTok, lat: &TextTok, radius: &TextTok, unit: &TextTok, num_params: &mut usize) -> (GeoParam, Option<QErr>) {
    let mut p = GeoParam { gf: GeoFilter { field: None, lon: 0.0, lat: 0.0, radius: 0.0, unit: GEO_KM }, params: Vec::new() };
    if let Some(v) = set_param(&mut p.params, num_params, Target::Lon, lon) {
        p.gf.lon = val_num(v);
    }
    if let Some(v) = set_param(&mut p.params, num_params, Target::Lat, lat) {
        p.gf.lat = val_num(v);
    }
    if let Some(v) = set_param(&mut p.params, num_params, Target::Radius, radius) {
        p.gf.radius = val_num(v);
    }
    match unit.qtype {
        QType::Term => p.gf.unit = geo_unit_buffer(&unit.s),
        QType::ParamGeoUnit => {
            set_param(&mut p.params, num_params, Target::Unit, unit);
        }
        _ => return (p, Some(QErr::new(Code::Syntax, "Invalid GeoFilter unit"))),
    }
    (p, None)
}

/// `NewGeofilterNode`.
pub fn geo_node(p: GeoParam) -> Node {
    let mut n = Node::new(Kind::Geo(p.gf));
    n.params = p.params;
    n
}

/// `parseGeometryPredicate`.
pub fn geometry_predicate(s: &[u8]) -> i32 {
    for (i, name) in ["CONTAINS", "WITHIN", "DISJOINT", "INTERSECTS"].iter().enumerate() {
        if s.eq_ignore_ascii_case(name.as_bytes()) {
            return i as i32 + 1;
        }
    }
    0
}

/// `NewGeometryNode_FromWkt_WithParams`: `None` for an unknown predicate.
pub fn geometry_node(predicate: &[u8], wkt: &TextTok, num_params: &mut usize) -> Option<Node> {
    let query_type = geometry_predicate(predicate);
    if query_type == 0 {
        return None;
    }
    let mut n = Node::new(Kind::Geometry { query_type, wkt: Vec::new(), field: None });
    if let Some(Val::Str(s)) = set_param(&mut n.params, num_params, Target::Wkt, wkt) {
        n.kind = Kind::Geometry { query_type, wkt: s, field: None };
    }
    Some(n)
}

/// `NewVectorNode_WithParams`: `value` is KNN's k or RANGE's radius, `vec` the vector
/// parameter.
pub fn vector_node(knn: bool, value: &TextTok, vec: &TextTok, num_params: &mut usize) -> Node {
    let vq = VectorQuery {
        knn,
        field: None,
        k: 0,
        radius: 0.0,
        blob: Bytes::new(),
        by_score: false,
        shard_window_ratio: 1.0,
        score_field: None,
        params: Vec::new(),
        need_resolve: Vec::new(),
    };
    let mut n = Node::new(Kind::Vector(Box::new(vq)));
    n.opts.flags |= N_YIELDS_DISTANCE;
    set_param(&mut n.params, num_params, Target::VecBlob, vec);
    let target = if knn { Target::K } else { Target::VecRadius };
    if let Some(v) = set_param(&mut n.params, num_params, target, value)
        && let Kind::Vector(vq) = &mut n.kind
    {
        match (v, knn) {
            (Val::Size(k), true) => vq.k = k,
            (Val::Num(r), false) => vq.radius = r,
            (Val::Num(k), true) => vq.k = size_cast(k),
            (Val::Size(r), false) => vq.radius = r as f64,
            (Val::Str(_), _) => {}
        }
    }
    n
}

/// `QueryAttribute`.
#[derive(Clone, Debug)]
pub struct Attr {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
    /// `vallen`: the value's length before it was read as a C string.
    pub vallen: usize,
}

/// `QueryNode_ApplyAttributes`: stops at the first one that fails.
pub fn apply_attributes(n: &mut Node, attrs: &[Attr]) -> Result<(), QErr> {
    for a in attrs {
        apply_attribute(n, a)?;
    }
    Ok(())
}

fn apply_attribute(n: &mut Node, a: &Attr) -> Result<(), QErr> {
    let invalid = || QErr::user(Code::Syntax, "Invalid value", format!(" ({}) for `{}`", lossy(&a.value), lossy(&a.name)));
    if a.vallen == 0 {
        return Err(invalid());
    }
    let is = |s: &str| a.name.eq_ignore_ascii_case(s.as_bytes());
    let ok = if is("slop") {
        match strconv::parse_integer(&a.value) {
            Some(v) if v >= -1 => n.opts.max_slop = v,
            _ => return Err(invalid()),
        }
        true
    } else if is("inorder") {
        let b = strconv::parse_boolean(&a.value).ok_or_else(invalid)?;
        n.opts.in_order = b;
        n.opts.flags |= N_OVERRIDDEN_INORDER;
        true
    } else if is("weight") {
        match strconv::parse_double(&a.value, 1) {
            Some(d) if d >= 0.0 => {
                n.opts.weight = d;
                n.opts.explicit_weight = true;
            }
            _ => return Err(invalid()),
        }
        true
    } else if is("phonetic") {
        let b = strconv::parse_boolean(&a.value).ok_or_else(invalid)?;
        n.opts.phonetic = if b { Phonetic::Enabled } else { Phonetic::Disabled };
        true
    } else if is("yield_distance_as") && n.opts.flags & N_YIELDS_DISTANCE != 0 {
        n.opts.dist_field = Some(a.value.clone());
        true
    } else if let Kind::Vector(vq) = &mut n.kind {
        vector_attribute(vq, a)?
    } else {
        false
    };
    if !ok {
        return Err(QErr::user(Code::NoOption, "Invalid attribute", format!(" {}", lossy(&a.name))));
    }
    Ok(())
}

/// `QueryVectorNode_ApplyAttribute`: the shard ratio, or a raw parameter the vector index
/// reads later; `false` for anything else.
fn vector_attribute(vq: &mut VectorQuery, a: &Attr) -> Result<bool, QErr> {
    let is = |s: &str| a.name.eq_ignore_ascii_case(s.as_bytes());
    if is("shard_k_ratio") {
        vq.shard_window_ratio = shard_k_ratio(&a.value)?;
        return Ok(true);
    }
    if ["EF_RUNTIME", "EPSILON", "HYBRID_POLICY", "BATCH_SIZE", "RERANK"].iter().any(|s| is(s)) {
        vq.params.push((a.name.clone(), a.value.clone()));
        vq.need_resolve.push(false);
        return Ok(true);
    }
    Ok(false)
}

/// `validateShardKRatio`: a double in (0, 1].
pub fn shard_k_ratio(v: &[u8]) -> Result<f64, QErr> {
    match strconv::parse_double(v, 1) {
        None => Err(QErr::user(Code::Inval, "Invalid shard k ratio value", format!(" '{}'", lossy(v)))),
        Some(r) if r <= 0.0 || r > 1.0 => {
            Err(QErr::new(Code::Inval, format!("Invalid shard k ratio value: Shard k ratio must be greater than 0 and at most 1 (got {})", fmt_g(r))))
        }
        Some(r) => Ok(r),
    }
}

pub fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(cstr(b)).into_owned()
}

/// Query parameters (`PARAMS`): name → value. Names are C strings.
pub type Params = HashMap<Vec<u8>, Bytes>;

/// `Param_DictGet`.
pub fn param_get<'a>(params: Option<&'a Params>, name: &[u8]) -> Result<&'a Bytes, QErr> {
    params
        .and_then(|p| p.get(cstr(name)))
        .ok_or_else(|| QErr::user(Code::NoParam, "Parameter not found", format!(" `{}`", lossy(name))))
}

/// `QueryNode_EvalParams`: resolve every parameter, depth first, stopping at the first error.
pub fn eval_params(n: &mut Node, params: Option<&Params>, dialect: u32) -> Result<(), QErr> {
    match &n.kind {
        Kind::Union => {}
        Kind::Null | Kind::Missing { .. } => return Ok(()),
        Kind::Vector(_) => {
            resolve_all(n, params, dialect)?;
            if let Kind::Vector(vq) = &mut n.kind {
                for i in 0..vq.params.len() {
                    if vq.need_resolve[i] {
                        let v = param_get(params, &vq.params[i].1)?;
                        vq.params[i].1 = v.to_vec();
                    }
                }
            }
        }
        _ => resolve_all(n, params, dialect)?,
    }
    for c in &mut n.children {
        eval_params(c, params, dialect)?;
    }
    Ok(())
}

/// `QueryNode_EvalParamsCommon`: a value that reads as a number keeps its node unexpanded.
fn resolve_all(n: &mut Node, params: Option<&Params>, dialect: u32) -> Result<(), QErr> {
    let ps = std::mem::take(&mut n.params);
    let mut res = Ok(());
    for p in &ps {
        match resolve(n, p, params, dialect) {
            Ok(2) => n.opts.flags |= N_VERBATIM,
            Ok(_) => {}
            Err(e) => {
                res = Err(e);
                break;
            }
        }
    }
    n.params = ps;
    res
}

/// `QueryParam_Resolve`: 1 resolved, 2 resolved to a number (for text parameters).
fn resolve(n: &mut Node, p: &Param, params: Option<&Params>, dialect: u32) -> Result<u8, QErr> {
    let raw = param_get(params, &p.name)?.clone();
    let val = cstr(&raw);
    let bad_num = || QErr::user(Code::NumericValueInvalid, "Invalid numeric value", format!(" ({}) for parameter `{}`", lossy(val), lossy(&p.name)));
    // `checkNumericAndGeoValueValid`: dialect 2 and up wants a value.
    let present = dialect < 2 || !val.is_empty();
    match p.ty {
        PType::Any | PType::Term => {
            let numeric = strconv::parse_double(val, p.sign).is_some();
            set_str(n, p.target, normalize(&raw));
            Ok(1 + u8::from(numeric))
        }
        PType::Wildcard => {
            set_str(n, p.target, raw.to_vec());
            Ok(1)
        }
        PType::TermCase => {
            set_str(n, p.target, val.to_vec());
            Ok(1)
        }
        PType::Numeric | PType::GeoCoord => {
            let d = if present { strconv::parse_double(val, p.sign) } else { None };
            set_num(n, p.target, d.ok_or_else(bad_num)?);
            Ok(1)
        }
        PType::Size => {
            let v = if present { strconv::parse_integer(val) } else { None };
            match v {
                Some(v) if v >= 0 => {
                    set_size(n, p.target, v as u64);
                    Ok(1)
                }
                _ => Err(bad_num()),
            }
        }
        PType::NumMinRange | PType::NumMaxRange => {
            if !present {
                return Ok(1);
            }
            let d = double_range(val, p.ty == PType::NumMinRange, p.sign)?;
            set_num(n, p.target, d);
            Ok(1)
        }
        PType::GeoUnit => {
            if let Kind::Geo(g) = &mut n.kind {
                g.unit = geo_unit(val);
            }
            Ok(1)
        }
        PType::Vec => {
            if let Kind::Vector(vq) = &mut n.kind {
                vq.blob = raw;
            }
            Ok(1)
        }
    }
}

/// `parseDoubleRange` (the `(` it accepts doesn't make the bound exclusive: the C drops it).
pub fn double_range(s: &[u8], is_min: bool, sign: i32) -> Result<f64, QErr> {
    let s = s.strip_prefix(b"(").unwrap_or(s);
    let unsigned = s.strip_prefix(b"+").unwrap_or(s);
    let neg_inf = s.eq_ignore_ascii_case(b"-inf");
    let pos_inf = unsigned.eq_ignore_ascii_case(b"inf");
    if is_min && ((sign == 1 && neg_inf) || (sign == -1 && pos_inf)) {
        return Ok(f64::NEG_INFINITY);
    }
    if !is_min && ((sign == 1 && pos_inf) || (sign == -1 && neg_inf)) {
        return Ok(f64::INFINITY);
    }
    let (v, used, _) = strconv::from_chars(s);
    if used != s.len() || v.is_infinite() {
        let m = if is_min { "Bad lower range" } else { "Bad upper range" };
        return Err(QErr::user(Code::ParseArgs, m, format!(": {}", lossy(s))));
    }
    Ok(if sign == -1 { -v } else { v })
}

fn set_str(n: &mut Node, target: Target, s: Vec<u8>) {
    match (&mut n.kind, target) {
        (Kind::Token(t) | Kind::Prefix { tok: t, .. } | Kind::Fuzzy { tok: t, .. } | Kind::WildcardQuery(t), Target::Str) => t.s = s,
        (Kind::Geometry { wkt, .. }, Target::Wkt) => *wkt = s,
        (Kind::Vector(vq), Target::ScoreField) => vq.score_field = Some(s),
        _ => {}
    }
}

fn set_num(n: &mut Node, target: Target, v: f64) {
    match (&mut n.kind, target) {
        (Kind::Numeric(f), Target::Min) => f.min = v,
        (Kind::Numeric(f), Target::Max) => f.max = v,
        (Kind::Geo(g), Target::Lon) => g.lon = v,
        (Kind::Geo(g), Target::Lat) => g.lat = v,
        (Kind::Geo(g), Target::Radius) => g.radius = v,
        (Kind::Vector(vq), Target::VecRadius) => vq.radius = v,
        _ => {}
    }
}

fn set_size(n: &mut Node, target: Target, v: u64) {
    if let (Kind::Vector(vq), Target::K) = (&mut n.kind, target) {
        vq.k = v;
    }
}

/// The parts of `RSSearchOptions` validation and expansion read and write.
#[derive(Clone, Debug)]
pub struct SearchOpts {
    pub flags: u32,
    pub slop: i64,
    pub language: usize,
}

fn text_fields(spec: &Spec) -> impl Iterator<Item = (&Field, Mask)> {
    spec.fields.iter().filter_map(|f| {
        let id = f.ft_id?;
        (f.is(spec::T_TEXT) && !f.has(spec::F_NOINDEX) && id < 128).then_some((f, 1u128 << id))
    })
}

/// `IndexSpec_GetFieldByBit`.
fn field_by_bit(spec: &Spec, bit: Mask) -> Option<&Field> {
    text_fields(spec).find(|(_, b)| *b == bit).map(|(f, _)| f)
}

/// `IndexSpec_GetFieldBit`: an indexable TEXT field's bit, else 0.
pub fn field_bit(spec: &Spec, name: &[u8]) -> Mask {
    match spec.field(name) {
        Some(f) if f.is(spec::T_TEXT) && !f.has(spec::F_NOINDEX) => f.ft_id.filter(|&id| id < 128).map_or(0, |id| 1u128 << id),
        _ => 0,
    }
}

/// `FIELD_BIT` of a field known to be TEXT.
pub fn bit_of(f: &Field) -> Mask {
    f.ft_id.filter(|&id| id < 128).map_or(0, |id| 1u128 << id)
}

/// `QAST_CheckIsValid`.
pub fn check_is_valid(root: &Node, spec: &Spec, opts: &mut SearchOpts, flags: u32) -> Result<(), QErr> {
    let json_unordered = spec.is_json() && spec.flags & spec::I_UNDEFINED_ORDER != 0;
    if spec.flags & spec::I_NON_EMPTY == 0 && !json_unordered {
        return Ok(());
    }
    check_node(root, spec, opts, flags)
}

fn check_node(n: &Node, spec: &Spec, opts: &mut SearchOpts, flags: u32) -> Result<(), QErr> {
    let mut eff = flags;
    if n.opts.flags & N_HYBRID_VECTOR_SUBQUERY != 0 && matches!(n.kind, Kind::Vector(_)) {
        eff &= !(V_NO_WEIGHT | V_NO_VECTOR);
    }
    if eff & V_NO_WEIGHT != 0 && n.opts.explicit_weight {
        return Err(QErr::code(Code::WeightNotAllowed));
    }
    match &n.kind {
        Kind::Phrase { .. } => {
            if spec.is_json() && spec.flags & spec::I_UNDEFINED_ORDER != 0 {
                let top = opts.slop >= 0 || opts.flags & S_IN_ORDER != 0;
                if top || n.opts.max_slop >= 0 || n.opts.flags & N_OVERRIDDEN_INORDER != 0 {
                    for (f, bit) in text_fields(spec) {
                        if n.opts.field_mask & bit != 0 && f.has(spec::F_UNDEFINED_ORDER) {
                            return Err(QErr::user(
                                Code::BadOrderOption,
                                "slop/inorder are not supported for field with undefined ordering",
                                format!(" `{}`", f.name_str()),
                            ));
                        }
                    }
                }
            }
        }
        Kind::Null | Kind::Missing { .. } => return Ok(()),
        Kind::Tag { field } => {
            opts.flags |= N_IS_TAG;
            if field.and_then(|i| spec.fields.get(i)).is_some_and(|f| f.has(spec::F_INDEX_EMPTY)) {
                opts.flags |= N_INDEXES_EMPTY;
            }
        }
        Kind::Union | Kind::Token(_) => {
            // `QueryNode_ValidateToken` (it reads a union's bytes as a token's: the C union
            // overlays the token on the node data, which for a union is all zeros).
            if spec.flags & spec::I_NON_EMPTY != 0
                && let Kind::Token(t) = &n.kind
                && t.s.is_empty()
                && !does_index_empty(n, spec, opts)
            {
                return Err(QErr::new(Code::Syntax, "Use `INDEXEMPTY` in field creation in order to index and query for empty strings"));
            }
        }
        Kind::Numeric(f) => {
            if f.min > f.max {
                let name = f.field.and_then(|i| spec.fields.get(i)).map(|f| f.name_str()).unwrap_or_default();
                return Err(QErr::user(Code::Syntax, "Invalid numeric range (min > max)", format!(": @{name}:[{} {}]", fmt_f(f.min), fmt_f(f.max))));
            }
        }
        Kind::Vector(_) if eff & V_NO_VECTOR != 0 => {
            return Err(QErr::code(Code::VectorNotAllowed));
        }
        _ => {}
    }
    for c in &n.children {
        check_node(c, spec, opts, flags)?;
    }
    Ok(())
}

/// `QueryNode_DoesIndexEmpty`.
fn does_index_empty(n: &Node, spec: &Spec, opts: &SearchOpts) -> bool {
    if opts.flags & N_IS_TAG != 0 {
        return opts.flags & N_INDEXES_EMPTY != 0;
    }
    if n.opts.field_mask == MASK_ALL {
        return true;
    }
    let fields: Vec<&Field> = text_fields(spec).filter(|(_, b)| n.opts.field_mask & b != 0).map(|(f, _)| f).collect();
    fields.is_empty() || fields.iter().any(|f| f.has(spec::F_INDEX_EMPTY))
}

/// The query expanders RediSearch registers (`DefaultExtensionInit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expander {
    /// `DEFAULT`: synonyms, phonetics, then the stemmer.
    Default,
    /// `SBSTEM`.
    Stemmer,
    /// `SYNONYM`.
    Synonym,
    /// `PHONETIC`.
    Phonetic,
}

impl Expander {
    /// `Extensions_GetQueryExpander` (case-sensitive; unknown names expand nothing).
    pub fn find(name: &[u8]) -> Option<Expander> {
        match name {
            b"DEFAULT" => Some(Expander::Default),
            b"SBSTEM" => Some(Expander::Stemmer),
            b"SYNONYM" => Some(Expander::Synonym),
            b"PHONETIC" => Some(Expander::Phonetic),
            _ => None,
        }
    }
}

/// The expander's state (`RSQueryExpanderCtx` and the stemmer's private data, created by the
/// first token it sees).
struct ExpandCtx<'a> {
    ix: &'a mut Index,
    expander: Expander,
    language: usize,
    stemmer: Option<Option<&'static str>>,
    num_tokens: usize,
    /// The Chinese expander's tokenizer (`expandCn`), made by its first token.
    cn: Option<Box<super::tokenize::CnTokenizer>>,
}

/// `QAST_Expand`.
pub fn expand(root: &mut Node, ix: &mut Index, expander: Expander, language: usize, num_tokens: &mut usize) -> Result<(), QErr> {
    let mut cx = ExpandCtx { ix, expander, language, stemmer: None, num_tokens: *num_tokens, cn: None };
    let r = expand_node(&mut cx, root);
    *num_tokens = cx.num_tokens;
    r
}

fn expand_node(cx: &mut ExpandCtx, n: &mut Node) -> Result<(), QErr> {
    if n.opts.flags & N_VERBATIM != 0 || n.is_phrase(true) || matches!(n.kind, Kind::Tag { .. }) {
        return Ok(());
    }
    let fm = n.opts.field_mask;
    if fm != MASK_ALL {
        // At least one stemmable field.
        let spec = &cx.ix.spec;
        let mut expand = false;
        let mut bit = 1u128;
        let mut m = fm;
        while m != 0 {
            if m & bit != 0 && field_by_bit(spec, bit).is_some_and(|f| !f.has(spec::F_NOSTEM)) {
                expand = true;
                break;
            }
            m &= !bit;
            bit = bit.wrapping_shl(1);
        }
        if !expand {
            return Ok(());
        }
    }
    if let Kind::Token(t) = &n.kind
        && !t.s.is_empty()
    {
        let tok = t.clone();
        return match cx.expander {
            Expander::Default => default_expand(cx, n, &tok),
            Expander::Stemmer => {
                stem_expand(cx, n, &tok);
                Ok(())
            }
            Expander::Synonym => {
                synonym_expand(cx, n, &tok);
                Ok(())
            }
            Expander::Phonetic => {
                phonetic_expand(cx, n, &tok);
                Ok(())
            }
        };
    }
    for c in &mut n.children {
        expand_node(cx, c)?;
    }
    Ok(())
}

/// `Ext_ExpandToken`: the node becomes a union of itself and the new token (or the union
/// grows).
fn expand_token(cx: &mut ExpandCtx, n: &mut Node, s: Vec<u8>, flags: u32) {
    if !n.is_union() {
        let mut un = Node::new(Kind::Union);
        un.opts.field_mask = n.opts.field_mask;
        let old = std::mem::replace(n, un);
        n.add_child(old);
    }
    let mut exp = Node::expanded(s, flags);
    cx.num_tokens += 1;
    exp.opts.field_mask = n.opts.field_mask;
    n.add_child(exp);
}

/// `DefaultExpander`.
fn default_expand(cx: &mut ExpandCtx, n: &mut Node, tok: &Tok) -> Result<(), QErr> {
    let mut phonetic = n.opts.phonetic;
    synonym_expand(cx, n, tok);
    let spec = &cx.ix.spec;
    match phonetic {
        Phonetic::Default => {
            if check_phonetic_enabled(spec, n.opts.field_mask) {
                phonetic = Phonetic::Enabled;
            }
        }
        Phonetic::Enabled | Phonetic::Disabled => {
            let valid = if n.opts.field_mask == MASK_ALL {
                spec.flags & spec::I_PHONETIC != 0
            } else {
                text_fields(spec).any(|(f, b)| n.opts.field_mask & b != 0 && f.has(spec::F_PHONETICS))
            };
            if !valid {
                return Err(QErr::new(Code::Inval, "field does not support phonetics"));
            }
        }
    }
    if phonetic == Phonetic::Enabled {
        phonetic_expand(cx, n, tok);
    }
    stem_expand(cx, n, tok);
    Ok(())
}

/// `IndexSpec_CheckPhoneticEnabled`.
fn check_phonetic_enabled(spec: &Spec, fm: Mask) -> bool {
    if spec.flags & spec::I_PHONETIC == 0 {
        return false;
    }
    if fm == 0 || fm == MASK_ALL {
        return true;
    }
    text_fields(spec).any(|(f, b)| fm & b != 0 && f.has(spec::F_PHONETICS))
}

/// `SynonymExpand`: the token's synonym groups.
fn synonym_expand(cx: &mut ExpandCtx, n: &mut Node, tok: &Tok) {
    let groups = match cx.ix.synonyms.as_mut().and_then(|m| m.find(cstr(&tok.s))) {
        Some(g) => g.clone(),
        None => return,
    };
    for g in groups {
        expand_token(cx, n, cstr(&g).to_vec(), 0);
    }
}

/// `PhoneticExpand`: the double metaphone's primary code.
fn phonetic_expand(cx: &mut ExpandCtx, n: &mut Node, tok: &Tok) {
    if let Some(p) = super::phonetic::primary(&tok.s) {
        expand_token(cx, n, p, 0);
    }
}

/// `StemmerExpander`: `+stem` (fields that don't stem left out of its mask), and the stem
/// itself when it differs from the token.
fn stem_expand(cx: &mut ExpandCtx, n: &mut Node, tok: &Tok) {
    if spec::LANGUAGES.get(cx.language) == Some(&"chinese") {
        return cn_expand(cx, n, tok);
    }
    let stemmer = *cx.stemmer.get_or_insert_with(|| super::tokenize::stemmer_for(cx.language));
    let Some(lang) = stemmer else { return };
    let Some(stemmed) = snowball_stem(lang, &tok.s) else { return };
    let orig = n.opts.field_mask;
    let mut expandable = orig;
    if orig != MASK_ALL {
        let mut m = orig;
        let mut bit = 1u128;
        while m != 0 {
            if m & bit != 0 && field_by_bit(&cx.ix.spec, bit).is_some_and(|f| f.has(spec::F_NOSTEM)) {
                expandable &= !bit;
            }
            m &= !bit;
            bit = bit.wrapping_shl(1);
        }
    }
    if !n.is_union() {
        let mut un = Node::new(Kind::Union);
        un.opts.field_mask = n.opts.field_mask;
        let old = std::mem::replace(n, un);
        n.add_child(old);
    }
    n.opts.field_mask = expandable;
    let mut plus = vec![super::tokenize::STEM_PREFIX];
    plus.extend_from_slice(&stemmed);
    expand_token(cx, n, plus, 0);
    if stemmed != tok.s {
        expand_token(cx, n, stemmed, 0);
    }
    n.opts.field_mask = orig;
}

/// `expandCn` / `Ext_ExpandTokenWithPhrase`: the token's Chinese segmentation, as a (loose)
/// phrase beside it in a union; the new nodes keep the default field mask.
fn cn_expand(cx: &mut ExpandCtx, n: &mut Node, tok: &Tok) {
    let t = cx.cn.get_or_insert_with(|| Box::new(super::tokenize::CnTokenizer::with(super::friso::global())));
    t.start(&tok.s);
    let mut ph = Node::phrase(false);
    while let Some(x) = t.next_token() {
        cx.num_tokens += 1;
        ph.add_child(Node::expanded(cstr(&x.tok).to_vec(), tok.flags));
    }
    if !n.is_union() {
        let old = std::mem::replace(n, Node::new(Kind::Union));
        n.add_child(old);
    }
    n.add_child(ph);
}

/// `sb_stemmer_stem` over the token's bytes (C string): `None` when Snowball rejects them.
fn snowball_stem(lang: &str, s: &[u8]) -> Option<Vec<u8>> {
    let w = std::str::from_utf8(cstr(s)).ok()?;
    super::tokenize::snowball_stem(lang, w).map(String::into_bytes)
}

/// `QAST_DumpExplain`.
pub fn explain(root: &Node, ix: &Index) -> Vec<u8> {
    let mut out = Vec::new();
    dump(&mut out, ix, root, 0);
    out
}

fn pad(out: &mut Vec<u8>, depth: usize) {
    out.extend(std::iter::repeat_n(b' ', depth * 2));
}

fn field_name(spec: &Spec, i: Option<usize>) -> Vec<u8> {
    i.and_then(|i| spec.fields.get(i)).map(|f| cstr(&f.name).to_vec()).unwrap_or_default()
}

fn dump(out: &mut Vec<u8>, ix: &Index, n: &Node, depth: usize) {
    let spec = &ix.spec;
    pad(out, depth);
    let fm = n.opts.field_mask;
    if fm == 0 {
        out.extend_from_slice(b"@NULL:");
    }
    if fm != 0 && fm != MASK_ALL && !matches!(n.kind, Kind::Numeric(_) | Kind::Geo(_) | Kind::Ids(_)) {
        out.push(b'@');
        let mut m = fm;
        let mut i = 0u32;
        let mut k = 0;
        while m != 0 {
            let bit = (m & 1) << i;
            if bit != 0 {
                if k > 0 {
                    out.push(b'|');
                }
                match field_by_bit(spec, bit) {
                    Some(f) => out.extend_from_slice(cstr(&f.name)),
                    None => out.extend_from_slice(b"n/a"),
                }
                k += 1;
            }
            m >>= 1;
            i += 1;
        }
        out.push(b':');
    }
    let children = |out: &mut Vec<u8>| {
        for c in &n.children {
            dump(out, ix, c, depth + 1);
        }
    };
    match &n.kind {
        Kind::Phrase { exact } => {
            out.extend_from_slice(if *exact { b"EXACT {\n" } else { b"INTERSECT {\n" });
            children(out);
            pad(out, depth);
            out.push(b'}');
        }
        Kind::Token(t) => {
            if t.s.is_empty() {
                out.extend_from_slice(b"\"\"");
            } else {
                out.extend_from_slice(cstr(&t.s));
            }
            if t.expanded {
                out.extend_from_slice(b"(expanded)");
            }
            if n.opts.weight != 1.0 {
                out.extend_from_slice(format!(" => {{$weight: {};}}", fmt_g(n.opts.weight)).as_bytes());
            }
            out.push(b'\n');
            return;
        }
        Kind::Prefix { tok, prefix, suffix } => {
            out.extend_from_slice(match (prefix, suffix) {
                (true, true) => b"INFIX",
                (false, true) => b"SUFFIX",
                _ => b"PREFIX",
            });
            out.push(b'{');
            if *suffix {
                out.push(b'*');
            }
            out.extend_from_slice(cstr(&tok.s));
            if *prefix {
                out.push(b'*');
            }
            out.push(b'}');
        }
        Kind::LexRange { begin, end, .. } => {
            out.extend_from_slice(b"LEXRANGE{");
            out.extend_from_slice(begin.as_deref().map(cstr).unwrap_or(b""));
            out.extend_from_slice(b"...");
            out.extend_from_slice(end.as_deref().map(cstr).unwrap_or(b""));
            out.push(b'}');
        }
        Kind::Not | Kind::Optional | Kind::Union => {
            out.extend_from_slice(match n.kind {
                Kind::Not => b"NOT{\n".as_slice(),
                Kind::Optional => b"OPTIONAL{\n",
                _ => b"UNION {\n",
            });
            children(out);
            pad(out, depth);
            out.push(b'}');
        }
        Kind::Numeric(f) => {
            out.extend_from_slice(format!("NUMERIC {{{} {} @", fmt_f(f.min), if f.min_inclusive { "<=" } else { "<" }).as_bytes());
            out.extend_from_slice(&field_name(spec, f.field));
            out.extend_from_slice(format!(" {} {}}}", if f.max_inclusive { "<=" } else { "<" }, fmt_f(f.max)).as_bytes());
        }
        Kind::Tag { field } => {
            out.extend_from_slice(b"TAG:@");
            out.extend_from_slice(&field_name(spec, *field));
            out.extend_from_slice(b" {\n");
            children(out);
            pad(out, depth);
            out.push(b'}');
        }
        Kind::Geo(g) => {
            out.extend_from_slice(b"GEO ");
            out.extend_from_slice(&field_name(spec, g.field));
            out.extend_from_slice(format!(":{{{},{} --> {} {}}}", fmt_f(g.lon), fmt_f(g.lat), fmt_f(g.radius), geo_unit_name(g.unit)).as_bytes());
        }
        Kind::Ids(keys) => {
            out.extend_from_slice(b"IDS {");
            for k in keys {
                let id = ix.docs.get_id(k);
                if id != 0 {
                    out.extend_from_slice(format!("{id},").as_bytes());
                }
            }
            out.push(b'}');
        }
        Kind::Vector(vq) => {
            out.extend_from_slice(b"VECTOR {");
            if !n.children.is_empty() {
                out.push(b'\n');
                children(out);
                pad(out, depth);
                out.extend_from_slice(b"} => {");
            }
            let name = n.params.iter().find(|p| p.target == Target::VecBlob).map(|p| cstr(&p.name).to_vec());
            if vq.knn {
                out.extend_from_slice(format!("K={} nearest vectors to ", vq.k).as_bytes());
                if let Some(name) = name {
                    out.extend_from_slice(b"`$");
                    out.extend_from_slice(&name);
                    out.extend_from_slice(b"` ");
                }
            } else {
                out.extend_from_slice(format!("Vectors that are within {} distance radius from", fmt_g(vq.radius)).as_bytes());
                if let Some(name) = name {
                    out.extend_from_slice(b" `$");
                    out.extend_from_slice(&name);
                    out.extend_from_slice(b"` ");
                }
            }
            out.extend_from_slice(b"in vector index associated with field @");
            out.extend_from_slice(&field_name(spec, vq.field));
            for (k, v) in &vq.params {
                out.extend_from_slice(b", ");
                out.extend_from_slice(cstr(k));
                out.extend_from_slice(b" = ");
                out.extend_from_slice(v);
            }
            if let Some(sf) = &vq.score_field {
                out.extend_from_slice(b", yields distance as `");
                out.extend_from_slice(cstr(sf));
                out.push(b'`');
            }
            out.push(b'}');
        }
        Kind::Wildcard => out.extend_from_slice(b"<WILDCARD>"),
        Kind::Fuzzy { tok, .. } => {
            out.extend_from_slice(b"FUZZY{");
            out.extend_from_slice(cstr(&tok.s));
            out.push(b'}');
        }
        Kind::WildcardQuery(tok) => {
            out.extend_from_slice(b"WILDCARD{");
            out.extend_from_slice(cstr(&tok.s));
            out.push(b'}');
        }
        Kind::Null => out.extend_from_slice(b"<empty>"),
        Kind::Geometry { query_type, wkt, .. } => {
            out.extend_from_slice(format!("GEOSHAPE{{{query_type} ").as_bytes());
            out.extend_from_slice(cstr(wkt));
            out.push(b'}');
        }
        Kind::Missing { field } => {
            out.extend_from_slice(b"ISMISSING{");
            out.extend_from_slice(&field_name(spec, *field));
            out.push(b'}');
        }
    }
    let o = &n.opts;
    if o.weight != 1.0 || o.max_slop != -1 || o.in_order {
        out.extend_from_slice(b" => {");
        if o.weight != 1.0 {
            out.extend_from_slice(format!(" $weight: {};", fmt_g(o.weight)).as_bytes());
        }
        if o.max_slop != -1 {
            out.extend_from_slice(format!(" $slop: {};", o.max_slop as i32).as_bytes());
        }
        if o.in_order || o.max_slop != -1 {
            out.extend_from_slice(format!(" $inorder: {};", o.in_order).as_bytes());
        }
        out.extend_from_slice(b" }");
    }
    out.push(b'\n');
}
