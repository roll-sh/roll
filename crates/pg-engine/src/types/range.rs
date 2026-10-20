//! Range and multirange types: `int4range`, `tstzrange`, ..., user ranges (CREATE TYPE ... AS
//! RANGE) and their multiranges. Values don't carry their type; bounds are plain subtype values.

use super::text::{FormatCtx, format_plain, parse_value};
use super::{DataType, Value};
use crate::error::{Error, Result, code};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

/// A range type and its multirange.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RangeType {
    pub oid: u32,
    pub array_oid: u32,
    pub multi_oid: u32,
    pub multi_array_oid: u32,
    pub name: Arc<str>,
    pub multi_name: Arc<str>,
    /// Namespace (pg_catalog for the built-in ones).
    pub schema: u32,
    pub subtype: DataType,
    /// Discrete built-ins are kept in `[)` form.
    pub canonical: Option<Arc<str>>,
    pub subdiff: Option<Arc<str>>,
    /// Collation oid of the subtype (0: none).
    #[serde(default)]
    pub collation: u32,
    /// That collation's definition, when it isn't a built-in one (to open it with).
    #[serde(default)]
    pub coll_def: Option<crate::types::collation::CollationDef>,
    /// The domain the subtype is declared as (its bounds are checked against it).
    #[serde(default)]
    pub subtype_domain: Option<u32>,
    /// The subtype's operator class when not its default btree one.
    #[serde(default)]
    pub subtype_opclass: Option<Arc<str>>,
}

impl PartialEq for RangeType {
    fn eq(&self, other: &Self) -> bool {
        self.oid == other.oid
    }
}

impl Eq for RangeType {}

impl Hash for RangeType {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.oid.hash(state)
    }
}

impl RangeType {
    /// The subtype's collation (`rng_collation`), when bounds don't compare as their bytes do.
    pub fn coll(&self) -> Option<Arc<crate::types::collation::Collation>> {
        if self.collation == 0 || !self.subtype.collatable() {
            return None;
        }
        let def = match &self.coll_def {
            Some(d) => d.clone(),
            None => crate::types::collation::static_by_oid(self.collation)?.clone(),
        };
        crate::types::collation::get(&def)
            .ok()
            .filter(|c| !c.bytewise() || !c.deterministic)
    }

    /// Whether ranges are canonicalised to `[)` by stepping integer or date bounds.
    pub fn discrete(&self) -> bool {
        self.canonical.is_some()
            && matches!(
                self.subtype,
                DataType::Int4 | DataType::Int8 | DataType::Date
            )
    }
}

thread_local! {
    /// The collation range bounds compare under (see `under`).
    static COLL: std::cell::RefCell<Option<Arc<crate::types::collation::Collation>>> =
        const { std::cell::RefCell::new(None) };
}

/// While the guard lives, range bounds compare under `c`: the subtype collation of the
/// range type a function or a sort works on (the binder attaches it to the call).
pub fn under(c: Option<Arc<crate::types::collation::Collation>>) -> CollGuard {
    CollGuard(Some(COLL.with(|x| x.replace(c))))
}

pub struct CollGuard(Option<Option<Arc<crate::types::collation::Collation>>>);

impl Drop for CollGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.0.take() {
            COLL.with(|x| *x.borrow_mut() = prev);
        }
    }
}

/// Bounds under the range type's own collation, when it has a special one.
fn scope(rt: &RangeType) -> Option<CollGuard> {
    rt.coll().map(|c| under(Some(c)))
}

/// `FunctionCall2Coll(rng_cmp_proc_finfo, rng_collation, ...)`: two bound values.
fn vcmp(a: &Value, b: &Value) -> Ordering {
    COLL.with(|c| match (&*c.borrow(), a, b) {
        (Some(c), Value::Text(x), Value::Text(y)) => c.cmp(x, y),
        _ => a.cmp_total(b),
    })
}

/// The built-in range types.
pub fn builtins() -> &'static [Arc<RangeType>] {
    static B: OnceLock<Vec<Arc<RangeType>>> = OnceLock::new();
    B.get_or_init(|| {
        let mk = |oid,
                  array_oid,
                  multi_oid,
                  multi_array_oid,
                  name: &str,
                  multi: &str,
                  subtype,
                  canonical: Option<&str>,
                  subdiff: &str| {
            Arc::new(RangeType {
                oid,
                array_oid,
                multi_oid,
                multi_array_oid,
                name: name.into(),
                multi_name: multi.into(),
                schema: crate::catalog::PG_CATALOG_OID,
                subtype,
                canonical: canonical.map(Into::into),
                subdiff: Some(subdiff.into()),
                collation: 0,
                coll_def: None,
                subtype_domain: None,
                subtype_opclass: None,
            })
        };
        vec![
            mk(
                3904,
                3905,
                4451,
                6150,
                "int4range",
                "int4multirange",
                DataType::Int4,
                Some("int4range_canonical"),
                "int4range_subdiff",
            ),
            mk(
                3906,
                3907,
                4532,
                6151,
                "numrange",
                "nummultirange",
                DataType::NUMERIC,
                None,
                "numrange_subdiff",
            ),
            mk(
                3908,
                3909,
                4533,
                6152,
                "tsrange",
                "tsmultirange",
                DataType::TIMESTAMP,
                None,
                "tsrange_subdiff",
            ),
            mk(
                3910,
                3911,
                4534,
                6153,
                "tstzrange",
                "tstzmultirange",
                DataType::TIMESTAMPTZ,
                None,
                "tstzrange_subdiff",
            ),
            mk(
                3912,
                3913,
                4535,
                6155,
                "daterange",
                "datemultirange",
                DataType::Date,
                Some("daterange_canonical"),
                "daterange_subdiff",
            ),
            mk(
                3926,
                3927,
                4536,
                6157,
                "int8range",
                "int8multirange",
                DataType::Int8,
                Some("int8range_canonical"),
                "int8range_subdiff",
            ),
        ]
    })
}

pub fn builtin(name: &str) -> Option<Arc<RangeType>> {
    builtins().iter().find(|r| &*r.name == name).cloned()
}

pub fn builtin_multi(name: &str) -> Option<Arc<RangeType>> {
    builtins().iter().find(|r| &*r.multi_name == name).cloned()
}

pub fn builtin_by_oid(oid: u32) -> Option<Arc<RangeType>> {
    builtins().iter().find(|r| r.oid == oid).cloned()
}

pub fn builtin_by_multi_oid(oid: u32) -> Option<Arc<RangeType>> {
    builtins().iter().find(|r| r.multi_oid == oid).cloned()
}

/// The built-in range over `subtype`, if there is one.
pub fn builtin_for_subtype(subtype: &DataType) -> Option<Arc<RangeType>> {
    builtins()
        .iter()
        .find(|r| r.subtype.same_base(subtype))
        .cloned()
}

/// One end of a range: its value (None: unbounded) and whether it is inclusive.
#[derive(Clone, Debug)]
pub struct Bound {
    pub val: Option<Value>,
    pub inclusive: bool,
    pub lower: bool,
}

impl Bound {
    pub fn infinite(&self) -> bool {
        self.val.is_none()
    }
}

/// A range value. Empty ranges have no bounds.
#[derive(Clone, Debug)]
pub struct Range {
    pub empty: bool,
    pub lower: Option<Value>,
    pub upper: Option<Value>,
    pub lower_inc: bool,
    pub upper_inc: bool,
}

impl Range {
    pub fn empty() -> Range {
        Range {
            empty: true,
            lower: None,
            upper: None,
            lower_inc: false,
            upper_inc: false,
        }
    }

    pub fn lower_bound(&self) -> Bound {
        Bound {
            val: self.lower.clone(),
            inclusive: self.lower_inc,
            lower: true,
        }
    }

    pub fn upper_bound(&self) -> Bound {
        Bound {
            val: self.upper.clone(),
            inclusive: self.upper_inc,
            lower: false,
        }
    }

    fn from_bounds(l: Bound, u: Bound) -> Range {
        Range {
            empty: false,
            lower_inc: l.inclusive && l.val.is_some(),
            upper_inc: u.inclusive && u.val.is_some(),
            lower: l.val,
            upper: u.val,
        }
    }
}

/// `range_cmp_bounds`.
pub fn cmp_bounds(b1: &Bound, b2: &Bound) -> Ordering {
    match (&b1.val, &b2.val) {
        (None, None) => {
            if b1.lower == b2.lower {
                Ordering::Equal
            } else if b1.lower {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (None, _) => {
            if b1.lower {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (_, None) => {
            if b2.lower {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(v1), Some(v2)) => {
            let c = vcmp(v1, v2);
            if c != Ordering::Equal {
                return c;
            }
            match (b1.inclusive, b2.inclusive) {
                (false, false) => {
                    if b1.lower == b2.lower {
                        Ordering::Equal
                    } else if b1.lower {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (false, true) => {
                    if b1.lower {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (true, false) => {
                    if b2.lower {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (true, true) => Ordering::Equal,
            }
        }
    }
}

/// `range_cmp_bound_values`: compare the values only (inclusivity ignored).
fn cmp_bound_values(b1: &Bound, b2: &Bound) -> Ordering {
    match (&b1.val, &b2.val) {
        (None, None) => {
            if b1.lower == b2.lower {
                Ordering::Equal
            } else if b1.lower {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (None, _) => {
            if b1.lower {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (_, None) => {
            if b2.lower {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(a), Some(b)) => vcmp(a, b),
    }
}

/// The btree ordering of ranges: empty first, then by lower bound, then by upper bound.
pub fn range_cmp(a: &Range, b: &Range) -> Ordering {
    match (a.empty, b.empty) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => cmp_bounds(&a.lower_bound(), &b.lower_bound())
            .then_with(|| cmp_bounds(&a.upper_bound(), &b.upper_bound())),
    }
}

fn step(v: &Value) -> Result<Value> {
    Ok(match v {
        Value::Int4(i) => {
            Value::Int4(i.checked_add(1).ok_or_else(|| {
                Error::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range")
            })?)
        }
        Value::Int8(i) => {
            Value::Int8(i.checked_add(1).ok_or_else(|| {
                Error::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range")
            })?)
        }
        Value::Date(d) if *d == i32::MAX || *d == i32::MIN => v.clone(),
        // daterange_canonical: the next day must still be a valid date.
        Value::Date(d) => Value::Date(
            d.checked_add(1)
                .filter(|n| crate::types::datetime::is_valid_date(*n))
                .ok_or_else(|| Error::new(code::DATETIME_FIELD_OVERFLOW, "date out of range"))?,
        ),
        v => v.clone(),
    })
}

/// `make_range`: check the bounds' order, normalise empties, and canonicalise discrete types.
pub fn make(rt: &RangeType, lower: Bound, upper: Bound) -> Result<Range> {
    let _g = scope(rt);
    let mut lower = lower;
    let mut upper = upper;
    if let (Some(l), Some(u)) = (&lower.val, &upper.val) {
        match vcmp(l, u) {
            Ordering::Greater => {
                return Err(Error::new(
                    code::DATA_EXCEPTION,
                    "range lower bound must be less than or equal to range upper bound",
                ));
            }
            Ordering::Equal if !(lower.inclusive && upper.inclusive) => return Ok(Range::empty()),
            _ => {}
        }
    }
    if lower.val.is_none() {
        lower.inclusive = false;
    }
    if upper.val.is_none() {
        upper.inclusive = false;
    }
    if rt.discrete() {
        // daterange_canonical leaves infinite dates as they are.
        let infinite = |v: &Value| matches!(v, Value::Date(d) if *d == i32::MAX || *d == i32::MIN);
        if let Some(l) = &lower.val
            && !lower.inclusive
            && !infinite(l)
        {
            lower.val = Some(step(l)?);
            lower.inclusive = true;
        }
        if let Some(u) = &upper.val
            && upper.inclusive
            && !infinite(u)
        {
            upper.val = Some(step(u)?);
            upper.inclusive = false;
        }
        if let (Some(l), Some(u)) = (&lower.val, &upper.val)
            && vcmp(l, u) != Ordering::Less
            && !(lower.inclusive && upper.inclusive && vcmp(l, u) == Ordering::Equal)
        {
            return Ok(Range::empty());
        }
    }
    Ok(Range::from_bounds(lower, upper))
}

pub fn contains_elem(r: &Range, v: &Value) -> bool {
    if r.empty {
        return false;
    }
    if let Some(l) = &r.lower {
        match vcmp(l, v) {
            Ordering::Greater => return false,
            Ordering::Equal if !r.lower_inc => return false,
            _ => {}
        }
    }
    if let Some(u) = &r.upper {
        match vcmp(u, v) {
            Ordering::Less => return false,
            Ordering::Equal if !r.upper_inc => return false,
            _ => {}
        }
    }
    true
}

pub fn contains(r1: &Range, r2: &Range) -> bool {
    if r2.empty {
        return true;
    }
    if r1.empty {
        return false;
    }
    cmp_bounds(&r1.lower_bound(), &r2.lower_bound()) != Ordering::Greater
        && cmp_bounds(&r1.upper_bound(), &r2.upper_bound()) != Ordering::Less
}

pub fn overlaps(r1: &Range, r2: &Range) -> bool {
    if r1.empty || r2.empty {
        return false;
    }
    let (l1, u1, l2, u2) = (
        r1.lower_bound(),
        r1.upper_bound(),
        r2.lower_bound(),
        r2.upper_bound(),
    );
    (cmp_bounds(&l1, &l2) != Ordering::Less && cmp_bounds(&l1, &u2) != Ordering::Greater)
        || (cmp_bounds(&l2, &l1) != Ordering::Less && cmp_bounds(&l2, &u1) != Ordering::Greater)
}

pub fn before(r1: &Range, r2: &Range) -> bool {
    !r1.empty && !r2.empty && cmp_bounds(&r1.upper_bound(), &r2.lower_bound()) == Ordering::Less
}

pub fn after(r1: &Range, r2: &Range) -> bool {
    !r1.empty && !r2.empty && cmp_bounds(&r1.lower_bound(), &r2.upper_bound()) == Ordering::Greater
}

/// `&<`: does not extend to the right of.
pub fn overleft(r1: &Range, r2: &Range) -> bool {
    !r1.empty && !r2.empty && cmp_bounds(&r1.upper_bound(), &r2.upper_bound()) != Ordering::Greater
}

/// `&>`: does not extend to the left of.
pub fn overright(r1: &Range, r2: &Range) -> bool {
    !r1.empty && !r2.empty && cmp_bounds(&r1.lower_bound(), &r2.lower_bound()) != Ordering::Less
}

/// `bounds_adjacent`: an upper bound `a` meets lower bound `b` with nothing in between.
fn bounds_adjacent(rt: &RangeType, a: &Bound, b: &Bound) -> bool {
    match cmp_bound_values(a, b) {
        Ordering::Less => {
            if !rt.discrete() {
                return false;
            }
            let l = Bound {
                val: a.val.clone(),
                inclusive: !a.inclusive,
                lower: true,
            };
            let u = Bound {
                val: b.val.clone(),
                inclusive: !b.inclusive,
                lower: false,
            };
            make(rt, l, u).is_ok_and(|r| r.empty)
        }
        Ordering::Equal => a.inclusive != b.inclusive,
        Ordering::Greater => false,
    }
}

/// `r1`'s upper bound meets `r2`'s lower bound (one direction of `adjacent`).
pub fn adjacent_after(rt: &RangeType, r1: &Range, r2: &Range) -> bool {
    let _g = scope(rt);
    !r1.empty && !r2.empty && bounds_adjacent(rt, &r1.upper_bound(), &r2.lower_bound())
}

pub fn adjacent(rt: &RangeType, r1: &Range, r2: &Range) -> bool {
    let _g = scope(rt);
    if r1.empty || r2.empty {
        return false;
    }
    bounds_adjacent(rt, &r1.upper_bound(), &r2.lower_bound())
        || bounds_adjacent(rt, &r2.upper_bound(), &r1.lower_bound())
}

/// `range_union_internal`: `strict` fails when the result would have a gap.
pub fn union(rt: &RangeType, r1: &Range, r2: &Range, strict: bool) -> Result<Range> {
    let _g = scope(rt);
    if r1.empty {
        return Ok(r2.clone());
    }
    if r2.empty {
        return Ok(r1.clone());
    }
    if strict && !overlaps(r1, r2) && !adjacent(rt, r1, r2) {
        return Err(Error::new(
            code::DATA_EXCEPTION,
            "result of range union would not be contiguous",
        ));
    }
    let (l1, l2) = (r1.lower_bound(), r2.lower_bound());
    let (u1, u2) = (r1.upper_bound(), r2.upper_bound());
    let l = if cmp_bounds(&l1, &l2) == Ordering::Less {
        l1
    } else {
        l2
    };
    let u = if cmp_bounds(&u1, &u2) == Ordering::Greater {
        u1
    } else {
        u2
    };
    make(rt, l, u)
}

pub fn intersect(rt: &RangeType, r1: &Range, r2: &Range) -> Result<Range> {
    let _g = scope(rt);
    if r1.empty || r2.empty || !overlaps(r1, r2) {
        return Ok(Range::empty());
    }
    let (l1, l2) = (r1.lower_bound(), r2.lower_bound());
    let (u1, u2) = (r1.upper_bound(), r2.upper_bound());
    let l = if cmp_bounds(&l1, &l2) != Ordering::Less {
        l1
    } else {
        l2
    };
    let u = if cmp_bounds(&u1, &u2) != Ordering::Greater {
        u1
    } else {
        u2
    };
    make(rt, l, u)
}

pub fn minus(rt: &RangeType, r1: &Range, r2: &Range) -> Result<Range> {
    let _g = scope(rt);
    if r1.empty || r2.empty {
        return Ok(r1.clone());
    }
    let (l1, u1, l2, u2) = (
        r1.lower_bound(),
        r1.upper_bound(),
        r2.lower_bound(),
        r2.upper_bound(),
    );
    let l1l2 = cmp_bounds(&l1, &l2);
    let l1u2 = cmp_bounds(&l1, &u2);
    let u1l2 = cmp_bounds(&u1, &l2);
    let u1u2 = cmp_bounds(&u1, &u2);
    if l1l2 == Ordering::Less && u1u2 == Ordering::Greater {
        return Err(Error::new(
            code::DATA_EXCEPTION,
            "result of range difference would not be contiguous",
        ));
    }
    if l1u2 == Ordering::Greater || u1l2 == Ordering::Less {
        return Ok(r1.clone());
    }
    if l1l2 != Ordering::Less && u1u2 != Ordering::Greater {
        return Ok(Range::empty());
    }
    if l1l2 != Ordering::Greater && u1l2 != Ordering::Less && u1u2 != Ordering::Greater {
        return make(
            rt,
            l1,
            Bound {
                val: l2.val,
                inclusive: !l2.inclusive,
                lower: false,
            },
        );
    }
    make(
        rt,
        Bound {
            val: u2.val,
            inclusive: !u2.inclusive,
            lower: true,
        },
        u1,
    )
}

/// Subtract `r2` from `r1`, possibly leaving two pieces (multirange difference).
pub fn minus_split(rt: &RangeType, r1: &Range, r2: &Range) -> Result<Vec<Range>> {
    let _g = scope(rt);
    if r1.empty || r2.empty || !overlaps(r1, r2) {
        return Ok(if r1.empty { vec![] } else { vec![r1.clone()] });
    }
    let (l1, u1, l2, u2) = (
        r1.lower_bound(),
        r1.upper_bound(),
        r2.lower_bound(),
        r2.upper_bound(),
    );
    let mut out = Vec::new();
    if cmp_bounds(&l1, &l2) == Ordering::Less {
        let r = make(
            rt,
            l1,
            Bound {
                val: l2.val,
                inclusive: !l2.inclusive,
                lower: false,
            },
        )?;
        if !r.empty {
            out.push(r);
        }
    }
    if cmp_bounds(&u1, &u2) == Ordering::Greater {
        let r = make(
            rt,
            Bound {
                val: u2.val,
                inclusive: !u2.inclusive,
                lower: true,
            },
            u1,
        )?;
        if !r.empty {
            out.push(r);
        }
    }
    Ok(out)
}

// ── multiranges ──

/// `multirange_canonicalize`: drop empties, sort, and merge overlapping or adjacent ranges.
pub fn normalize(rt: &RangeType, mut ranges: Vec<Range>) -> Result<Vec<Range>> {
    let _g = scope(rt);
    ranges.retain(|r| !r.empty);
    ranges.sort_by(range_cmp);
    let mut out: Vec<Range> = Vec::with_capacity(ranges.len());
    for r in ranges {
        if let Some(last) = out.last_mut()
            && (overlaps(last, &r) || adjacent(rt, last, &r))
        {
            *last = union(rt, last, &r, false)?;
            continue;
        }
        out.push(r);
    }
    Ok(out)
}

pub fn multi_cmp(a: &[Range], b: &[Range]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let c = range_cmp(x, y);
        if c != Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

/// The smallest range covering a multirange (`range_merge(anymultirange)`).
pub fn span(rt: &RangeType, m: &[Range]) -> Result<Range> {
    match (m.first(), m.last()) {
        (Some(f), Some(l)) => make(rt, f.lower_bound(), l.upper_bound()),
        _ => Ok(Range::empty()),
    }
}

pub fn multi_union(rt: &RangeType, a: &[Range], b: &[Range]) -> Result<Vec<Range>> {
    normalize(rt, a.iter().chain(b).cloned().collect())
}

pub fn multi_minus(rt: &RangeType, a: &[Range], b: &[Range]) -> Result<Vec<Range>> {
    let _g = scope(rt);
    let mut cur: Vec<Range> = a.to_vec();
    for r2 in b {
        let mut next = Vec::new();
        for r1 in &cur {
            next.extend(minus_split(rt, r1, r2)?);
        }
        cur = next;
    }
    normalize(rt, cur)
}

pub fn multi_intersect(rt: &RangeType, a: &[Range], b: &[Range]) -> Result<Vec<Range>> {
    let _g = scope(rt);
    let mut out = Vec::new();
    for r1 in a {
        for r2 in b {
            let r = intersect(rt, r1, r2)?;
            if !r.empty {
                out.push(r);
            }
        }
    }
    normalize(rt, out)
}

// ── text I/O ──

fn malformed(s: &str, detail: &str) -> Error {
    Error::new(
        code::INVALID_TEXT_REPRESENTATION,
        format!("malformed range literal: \"{s}\""),
    )
    .detail(detail)
}

/// `range_parse_bound`: (text, infinite) and the rest of the input.
fn parse_bound<'a>(input: &str, s: &'a str) -> Result<(Option<String>, &'a str)> {
    if s.starts_with([',', ')', ']']) {
        return Ok((None, s));
    }
    let mut out = String::new();
    let mut inquote = false;
    let mut chars = s.char_indices().peekable();
    loop {
        let Some((i, ch)) = chars.next() else {
            return Err(malformed(input, "Unexpected end of input."));
        };
        if !inquote && matches!(ch, ',' | ')' | ']') {
            return Ok((Some(out), &s[i..]));
        }
        match ch {
            '\\' => match chars.next() {
                Some((_, c)) => out.push(c),
                None => return Err(malformed(input, "Unexpected end of input.")),
            },
            '"' if !inquote => inquote = true,
            '"' if chars.peek().is_some_and(|(_, c)| *c == '"') => {
                chars.next();
                out.push('"');
            }
            '"' => inquote = false,
            c => out.push(c),
        }
    }
}

/// `range_in`.
pub fn parse(s: &str, rt: &RangeType, ctx: &FormatCtx) -> Result<Range> {
    let t = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if t.len() >= 5 && t[..5].eq_ignore_ascii_case("empty") {
        if !t[5..]
            .trim_matches(|c: char| c.is_ascii_whitespace())
            .is_empty()
        {
            return Err(malformed(s, "Junk after \"empty\" key word."));
        }
        return Ok(Range::empty());
    }
    let (lower_inc, rest) = match t.chars().next() {
        Some('[') => (true, &t[1..]),
        Some('(') => (false, &t[1..]),
        _ => return Err(malformed(s, "Missing left parenthesis or bracket.")),
    };
    let (lower, rest) = parse_bound(s, rest)?;
    let Some(rest) = rest.strip_prefix(',') else {
        return Err(malformed(s, "Missing comma after lower bound."));
    };
    let (upper, rest) = parse_bound(s, rest)?;
    let (upper_inc, rest) = match rest.chars().next() {
        Some(']') => (true, &rest[1..]),
        Some(')') => (false, &rest[1..]),
        _ => return Err(malformed(s, "Too many commas.")),
    };
    if !rest
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .is_empty()
    {
        return Err(malformed(s, "Junk after right parenthesis or bracket."));
    }
    let lower = lower
        .map(|v| parse_value(&v, &rt.subtype, ctx))
        .transpose()?;
    let upper = upper
        .map(|v| parse_value(&v, &rt.subtype, ctx))
        .transpose()?;
    make(
        rt,
        Bound {
            val: lower,
            inclusive: lower_inc,
            lower: true,
        },
        Bound {
            val: upper,
            inclusive: upper_inc,
            lower: false,
        },
    )
}

/// `range_bound_escape`.
fn escape_bound(out: &mut String, v: &str) {
    let quote = v.is_empty()
        || v.chars().any(|c| {
            matches!(c, '"' | '\\' | '(' | ')' | '[' | ']' | ',') || c.is_ascii_whitespace()
        });
    if quote {
        out.push('"');
    }
    for c in v.chars() {
        if c == '"' || c == '\\' {
            out.push(c);
        }
        out.push(c);
    }
    if quote {
        out.push('"');
    }
}

/// `range_out`.
pub fn format(r: &Range, ctx: &FormatCtx) -> String {
    if r.empty {
        return "empty".into();
    }
    let mut s = String::new();
    s.push(if r.lower_inc { '[' } else { '(' });
    if let Some(l) = &r.lower {
        escape_bound(&mut s, &format_plain(l, ctx));
    }
    s.push(',');
    if let Some(u) = &r.upper {
        escape_bound(&mut s, &format_plain(u, ctx));
    }
    s.push(if r.upper_inc { ']' } else { ')' });
    s
}

pub fn format_multi(m: &[Range], ctx: &FormatCtx) -> String {
    let parts: Vec<String> = m.iter().map(|r| format(r, ctx)).collect();
    format!("{{{}}}", parts.join(","))
}

/// `multirange_in`.
pub fn parse_multi(s: &str, rt: &RangeType, ctx: &FormatCtx) -> Result<Vec<Range>> {
    let bad = |detail: &str| {
        Error::new(
            code::INVALID_TEXT_REPRESENTATION,
            format!("malformed multirange literal: \"{s}\""),
        )
        .detail(detail.to_string())
    };
    let t = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let Some(body) = t.strip_prefix('{') else {
        return Err(bad("Missing left brace."));
    };
    #[derive(PartialEq)]
    enum St {
        Before,
        In,
        Escaped,
        Quoted,
        QuotedEscaped,
        After,
        Done,
    }
    let mut st = St::Before;
    let mut ranges = Vec::new();
    let mut seen = 0;
    let mut start = 0;
    let b = body.as_bytes();
    let mut i = 0;
    while st != St::Done {
        let Some(&ch) = b.get(i) else {
            return Err(bad("Unexpected end of input."));
        };
        if ch.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match st {
            St::Before => {
                if ch == b'[' || ch == b'(' {
                    start = i;
                    st = St::In;
                } else if ch == b'}' && seen == 0 {
                    st = St::Done;
                } else if body.len() >= i + 5 && body[i..i + 5].eq_ignore_ascii_case("empty") {
                    seen += 1;
                    i += 4;
                    st = St::After;
                } else {
                    return Err(bad("Expected range start."));
                }
            }
            St::In => {
                if ch == b']' || ch == b')' {
                    let r = parse(&body[start..=i], rt, ctx)?;
                    seen += 1;
                    if !r.empty {
                        ranges.push(r);
                    }
                    st = St::After;
                } else if ch == b'"' {
                    st = St::Quoted;
                } else if ch == b'\\' {
                    st = St::Escaped;
                }
            }
            St::Escaped => st = St::In,
            St::Quoted => {
                if ch == b'"' {
                    if b.get(i + 1) == Some(&b'"') {
                        i += 1;
                    } else {
                        st = St::In;
                    }
                } else if ch == b'\\' {
                    st = St::QuotedEscaped;
                }
            }
            St::QuotedEscaped => st = St::Quoted,
            St::After => {
                if ch == b',' {
                    st = St::Before;
                } else if ch == b'}' {
                    st = St::Done;
                } else {
                    return Err(bad("Expected comma or end of multirange."));
                }
            }
            St::Done => {}
        }
        i += 1;
    }
    if !body[i..]
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .is_empty()
    {
        return Err(bad("Junk after closing right brace."));
    }
    normalize(rt, ranges)
}

pub fn value(r: Range) -> Value {
    Value::Range(Arc::new(r))
}

pub fn multi_value(m: Vec<Range>) -> Value {
    Value::Multirange(Arc::new(m))
}
