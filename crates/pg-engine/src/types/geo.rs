//! Geometric types (point, line, lseg, box, path, polygon, circle), following Postgres's
//! geo_ops.c: fuzzy comparisons within EPSILON, overflow-checked float arithmetic, and the
//! same text forms.

use crate::error::{Error, Result, code};
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

pub const EPSILON: f64 = 1.0e-06;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// `Ax + By + C = 0`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub a: f64,
    pub b: f64,
    pub c: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lseg {
    pub p: [Point; 2],
}

/// Corners kept as (high, low).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GBox {
    pub high: Point,
    pub low: Point,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Path {
    pub closed: bool,
    pub p: Vec<Point>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Polygon {
    pub p: Vec<Point>,
    pub boundbox: GBox,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Circle {
    pub center: Point,
    pub radius: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Geo {
    Point(Point),
    Lseg(Lseg),
    Path(Path),
    Box(GBox),
    Polygon(Polygon),
    Line(Line),
    Circle(Circle),
}

impl Geo {
    /// A stable tag per kind (storage and ordering).
    pub fn kind(&self) -> u8 {
        match self {
            Geo::Point(_) => 0,
            Geo::Lseg(_) => 1,
            Geo::Path(_) => 2,
            Geo::Box(_) => 3,
            Geo::Polygon(_) => 4,
            Geo::Line(_) => 5,
            Geo::Circle(_) => 6,
        }
    }

    /// Its coordinates in order (a path's closed flag aside).
    pub fn floats(&self) -> Vec<f64> {
        let pts = |ps: &[Point]| ps.iter().flat_map(|p| [p.x, p.y]).collect::<Vec<_>>();
        match self {
            Geo::Point(p) => vec![p.x, p.y],
            Geo::Lseg(l) => pts(&l.p),
            Geo::Path(p) => pts(&p.p),
            Geo::Box(b) => vec![b.high.x, b.high.y, b.low.x, b.low.y],
            Geo::Polygon(p) => pts(&p.p),
            Geo::Line(l) => vec![l.a, l.b, l.c],
            Geo::Circle(c) => vec![c.center.x, c.center.y, c.radius],
        }
    }

    /// Rebuild from `kind` and `floats` (and a path's closed flag).
    pub fn from_floats(kind: u8, closed: bool, f: &[f64]) -> Option<Geo> {
        let pts = |f: &[f64]| {
            f.as_chunks::<2>()
                .0
                .iter()
                .map(|c| Point { x: c[0], y: c[1] })
                .collect::<Vec<_>>()
        };
        Some(match kind {
            0 if f.len() == 2 => Geo::Point(Point { x: f[0], y: f[1] }),
            1 if f.len() == 4 => {
                let p = pts(f);
                Geo::Lseg(Lseg { p: [p[0], p[1]] })
            }
            2 if f.len().is_multiple_of(2) => Geo::Path(Path { closed, p: pts(f) }),
            3 if f.len() == 4 => Geo::Box(GBox {
                high: Point { x: f[0], y: f[1] },
                low: Point { x: f[2], y: f[3] },
            }),
            4 if f.len().is_multiple_of(2) && !f.is_empty() => Geo::Polygon(polygon(pts(f)).ok()?),
            5 if f.len() == 3 => Geo::Line(Line {
                a: f[0],
                b: f[1],
                c: f[2],
            }),
            6 if f.len() == 3 => Geo::Circle(Circle {
                center: Point { x: f[0], y: f[1] },
                radius: f[2],
            }),
            _ => return None,
        })
    }

    pub fn is_closed_path(&self) -> bool {
        matches!(self, Geo::Path(p) if p.closed)
    }
}

// ── fuzzy comparisons and checked arithmetic (float.h) ──

pub fn fp_zero(a: f64) -> bool {
    a.abs() <= EPSILON
}
pub fn fp_eq(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= EPSILON
}
pub fn fp_ne(a: f64, b: f64) -> bool {
    a != b && (a - b).abs() > EPSILON
}
pub fn fp_lt(a: f64, b: f64) -> bool {
    b - a > EPSILON
}
pub fn fp_le(a: f64, b: f64) -> bool {
    a - b <= EPSILON
}
pub fn fp_gt(a: f64, b: f64) -> bool {
    a - b > EPSILON
}
pub fn fp_ge(a: f64, b: f64) -> bool {
    b - a <= EPSILON
}

/// float8_eq & co.: NaN equals NaN and sorts above everything.
fn f8_eq(a: f64, b: f64) -> bool {
    if a.is_nan() {
        b.is_nan()
    } else {
        !b.is_nan() && a == b
    }
}
fn f8_lt(a: f64, b: f64) -> bool {
    if a.is_nan() {
        false
    } else {
        b.is_nan() || a < b
    }
}
fn f8_gt(a: f64, b: f64) -> bool {
    if a.is_nan() {
        !b.is_nan()
    } else {
        !b.is_nan() && a > b
    }
}
fn f8_min(a: f64, b: f64) -> f64 {
    if f8_lt(a, b) { a } else { b }
}
fn f8_max(a: f64, b: f64) -> f64 {
    if f8_gt(a, b) { a } else { b }
}

fn overflow() -> Error {
    Error::new(
        code::NUMERIC_VALUE_OUT_OF_RANGE,
        "value out of range: overflow",
    )
}
fn underflow() -> Error {
    Error::new(
        code::NUMERIC_VALUE_OUT_OF_RANGE,
        "value out of range: underflow",
    )
}

pub fn pl(a: f64, b: f64) -> Result<f64> {
    let r = a + b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}
pub fn mi(a: f64, b: f64) -> Result<f64> {
    let r = a - b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}
pub fn mul(a: f64, b: f64) -> Result<f64> {
    let r = a * b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && a != 0.0 && b != 0.0 {
        return Err(underflow());
    }
    Ok(r)
}
pub fn div(a: f64, b: f64) -> Result<f64> {
    if b == 0.0 && !a.is_nan() {
        return Err(Error::new(code::DIVISION_BY_ZERO, "division by zero"));
    }
    let r = a / b;
    if r.is_infinite() && !a.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && a != 0.0 && !b.is_infinite() {
        return Err(underflow());
    }
    Ok(r)
}

/// pg_hypot: sqrt(x² + y²) without needless overflow. `1 + (y/x)²` is one fused
/// multiply-add, as compilers contract it on the reference (aarch64) builds.
pub fn hypot(x: f64, y: f64) -> f64 {
    if x.is_infinite() || y.is_infinite() {
        return f64::INFINITY;
    }
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    let (mut x, mut y) = (x.abs(), y.abs());
    if x < y {
        std::mem::swap(&mut x, &mut y);
    }
    if y == 0.0 {
        return x;
    }
    let yx = y / x;
    x * yx.mul_add(yx, 1.0).sqrt()
}

// ── text input ──

fn bad(tname: &str, orig: &str) -> Error {
    Error::new(
        code::INVALID_TEXT_REPRESENTATION,
        format!("invalid input syntax for type {tname}: \"{orig}\""),
    )
}

fn skip_ws(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && s[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// float8in_internal on a prefix: the number at `i` (after spaces), and where it ends (after
/// trailing spaces).
fn single_decode(s: &[u8], i: usize, tname: &str, orig: &str) -> Result<(f64, usize)> {
    let start = skip_ws(s, i);
    let rest = &s[start..];
    let lower: Vec<u8> = rest.iter().take(9).map(u8::to_ascii_lowercase).collect();
    let special = [
        ("-infinity", f64::NEG_INFINITY),
        ("+infinity", f64::INFINITY),
        ("infinity", f64::INFINITY),
        ("-inf", f64::NEG_INFINITY),
        ("+inf", f64::INFINITY),
        ("inf", f64::INFINITY),
        ("nan", f64::NAN),
        ("+nan", f64::NAN),
        ("-nan", f64::NAN),
    ];
    for (w, v) in special {
        if lower.starts_with(w.as_bytes()) {
            return Ok((v, skip_ws(s, start + w.len())));
        }
    }
    let mut j = 0;
    if j < rest.len() && matches!(rest[j], b'+' | b'-') {
        j += 1;
    }
    let digits_start = j;
    while j < rest.len() && rest[j].is_ascii_digit() {
        j += 1;
    }
    let mut ndig = j - digits_start;
    if j < rest.len() && rest[j] == b'.' {
        j += 1;
        let f = j;
        while j < rest.len() && rest[j].is_ascii_digit() {
            j += 1;
        }
        ndig += j - f;
    }
    if ndig == 0 {
        return Err(bad(tname, orig));
    }
    if j < rest.len() && matches!(rest[j], b'e' | b'E') {
        let mut k = j + 1;
        if k < rest.len() && matches!(rest[k], b'+' | b'-') {
            k += 1;
        }
        let e = k;
        while k < rest.len() && rest[k].is_ascii_digit() {
            k += 1;
        }
        if k > e {
            j = k;
        }
    }
    let text = std::str::from_utf8(&rest[..j]).unwrap_or("");
    let v: f64 = text.parse().map_err(|_| bad(tname, orig))?;
    if v.is_infinite()
        || (v == 0.0
            && text
                .split(['e', 'E'])
                .next()
                .unwrap_or("")
                .bytes()
                .any(|b| (b'1'..=b'9').contains(&b)))
    {
        return Err(Error::new(
            code::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("\"{text}\" is out of range for type double precision"),
        ));
    }
    Ok((v, skip_ws(s, start + j)))
}

/// pair_decode: `x,y` or `(x,y)`; `end`: the caller takes over at the returned position.
fn pair_decode(s: &[u8], i: usize, end: bool, tname: &str, orig: &str) -> Result<(Point, usize)> {
    let mut i = skip_ws(s, i);
    let delim = s.get(i) == Some(&b'(');
    if delim {
        i += 1;
    }
    let (x, j) = single_decode(s, i, tname, orig)?;
    if s.get(j) != Some(&b',') {
        return Err(bad(tname, orig));
    }
    let (y, mut j) = single_decode(s, j + 1, tname, orig)?;
    if delim {
        if s.get(j) != Some(&b')') {
            return Err(bad(tname, orig));
        }
        j = skip_ws(s, j + 1);
    }
    if !end && j != s.len() {
        return Err(bad(tname, orig));
    }
    Ok((Point { x, y }, j))
}

/// path_decode: `npts` points, in `[...]` (open, if allowed), `(...)` or bare.
fn path_decode(
    s: &[u8],
    i: usize,
    opentype: bool,
    npts: usize,
    end: bool,
    tname: &str,
    orig: &str,
) -> Result<(Vec<Point>, bool, usize)> {
    let mut depth = 0;
    let mut i = skip_ws(s, i);
    let isopen = s.get(i) == Some(&b'[');
    if isopen {
        if !opentype {
            return Err(bad(tname, orig));
        }
        depth += 1;
        i += 1;
    } else if s.get(i) == Some(&b'(') {
        let cp = skip_ws(s, i + 1);
        if s.get(cp) == Some(&b'(') || s[i + 1..].iter().all(|&c| c != b'(') {
            depth += 1;
            i = cp;
        }
    }
    let mut pts = Vec::with_capacity(npts);
    for _ in 0..npts {
        let (p, j) = pair_decode(s, i, true, tname, orig)?;
        pts.push(p);
        i = j;
        if s.get(i) == Some(&b',') {
            i += 1;
        }
    }
    while depth > 0 {
        if s.get(i) == Some(&b')') || (s.get(i) == Some(&b']') && isopen && depth == 1) {
            depth -= 1;
            i = skip_ws(s, i + 1);
        } else {
            return Err(bad(tname, orig));
        }
    }
    if !end && i != s.len() {
        return Err(bad(tname, orig));
    }
    Ok((pts, isopen, i))
}

/// pair_count: the number of points `a,b,c,d` holds, if the commas pair up.
fn pair_count(s: &str) -> Option<usize> {
    let n = s.bytes().filter(|&c| c == b',').count();
    if n % 2 == 1 {
        Some(n.div_ceil(2))
    } else {
        None
    }
}

pub fn parse_point(s: &str) -> Result<Point> {
    Ok(pair_decode(s.as_bytes(), 0, false, "point", s)?.0)
}

pub fn parse_lseg(s: &str) -> Result<Lseg> {
    let (p, _, _) = path_decode(s.as_bytes(), 0, true, 2, false, "lseg", s)?;
    Ok(Lseg { p: [p[0], p[1]] })
}

pub fn parse_box(s: &str) -> Result<GBox> {
    let (p, _, _) = path_decode(s.as_bytes(), 0, false, 2, false, "box", s)?;
    let (mut high, mut low) = (p[0], p[1]);
    if f8_lt(high.x, low.x) {
        std::mem::swap(&mut high.x, &mut low.x);
    }
    if f8_lt(high.y, low.y) {
        std::mem::swap(&mut high.y, &mut low.y);
    }
    Ok(GBox { high, low })
}

pub fn parse_path(s: &str) -> Result<Path> {
    let npts = pair_count(s).ok_or_else(|| bad("path", s))?;
    let b = s.as_bytes();
    let mut i = skip_ws(b, 0);
    let mut depth = 0;
    // A single leading paren, when it's the only one.
    if b.get(i) == Some(&b'(') && b[i + 1..].iter().all(|&c| c != b'(') {
        i += 1;
        depth += 1;
    }
    let (p, isopen, mut i) = path_decode(b, i, true, npts, true, "path", s)?;
    if depth >= 1 {
        if b.get(i) != Some(&b')') {
            return Err(bad("path", s));
        }
        i = skip_ws(b, i + 1);
    }
    if i != b.len() {
        return Err(bad("path", s));
    }
    Ok(Path { closed: !isopen, p })
}

pub fn parse_polygon(s: &str) -> Result<Polygon> {
    let npts = pair_count(s).ok_or_else(|| bad("polygon", s))?;
    let (p, _, _) = path_decode(s.as_bytes(), 0, false, npts, false, "polygon", s)?;
    polygon(p)
}

pub fn parse_circle(s: &str) -> Result<Circle> {
    let b = s.as_bytes();
    let mut depth = 0;
    let mut i = skip_ws(b, 0);
    if b.get(i) == Some(&b'<') {
        depth += 1;
        i += 1;
    } else if b.get(i) == Some(&b'(') {
        let cp = skip_ws(b, i + 1);
        if b.get(cp) == Some(&b'(') {
            depth += 1;
            i = cp;
        }
    }
    let (center, mut i) = pair_decode(b, i, true, "circle", s)?;
    if b.get(i) == Some(&b',') {
        i += 1;
    }
    let (radius, mut i) = single_decode(b, i, "circle", s)?;
    if radius < 0.0 {
        return Err(bad("circle", s));
    }
    while depth > 0 {
        if b.get(i) == Some(&b')') || (b.get(i) == Some(&b'>') && depth == 1) {
            depth -= 1;
            i = skip_ws(b, i + 1);
        } else {
            return Err(bad("circle", s));
        }
    }
    if i != b.len() {
        return Err(bad("circle", s));
    }
    Ok(Circle { center, radius })
}

pub fn parse_line(s: &str) -> Result<Line> {
    let b = s.as_bytes();
    let i = skip_ws(b, 0);
    if b.get(i) == Some(&b'{') {
        let (a, j) = single_decode(b, i + 1, "line", s)?;
        if b.get(j) != Some(&b',') {
            return Err(bad("line", s));
        }
        let (bb, j) = single_decode(b, j + 1, "line", s)?;
        if b.get(j) != Some(&b',') {
            return Err(bad("line", s));
        }
        let (c, j) = single_decode(b, j + 1, "line", s)?;
        if b.get(j) != Some(&b'}') || skip_ws(b, j + 1) != b.len() {
            return Err(bad("line", s));
        }
        if fp_zero(a) && fp_zero(bb) {
            return Err(Error::new(
                code::INVALID_TEXT_REPRESENTATION,
                "invalid line specification: A and B cannot both be zero",
            ));
        }
        return Ok(Line { a, b: bb, c });
    }
    let (p, _, _) = path_decode(b, i, true, 2, false, "line", s)?;
    if point_eq(&p[0], &p[1]) {
        return Err(Error::new(
            code::INVALID_TEXT_REPRESENTATION,
            "invalid line specification: must be two distinct points",
        ));
    }
    line_construct(&p[0], point_sl(&p[0], &p[1])?)
}

// ── text output ──

fn pair(out: &mut String, p: &Point, extra: i32) {
    out.push('(');
    out.push_str(&super::text::format_float8(p.x, extra));
    out.push(',');
    out.push_str(&super::text::format_float8(p.y, extra));
    out.push(')');
}

fn points(out: &mut String, ps: &[Point], extra: i32) {
    for (i, p) in ps.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        pair(out, p, extra);
    }
}

pub fn format(g: &Geo, extra: i32) -> String {
    let f = |v: f64| super::text::format_float8(v, extra);
    let mut out = String::new();
    match g {
        Geo::Point(p) => pair(&mut out, p, extra),
        Geo::Lseg(l) => {
            out.push('[');
            points(&mut out, &l.p, extra);
            out.push(']');
        }
        Geo::Box(b) => points(&mut out, &[b.high, b.low], extra),
        Geo::Path(p) => {
            out.push(if p.closed { '(' } else { '[' });
            points(&mut out, &p.p, extra);
            out.push(if p.closed { ')' } else { ']' });
        }
        Geo::Polygon(p) => {
            out.push('(');
            points(&mut out, &p.p, extra);
            out.push(')');
        }
        Geo::Line(l) => out = format!("{{{},{},{}}}", f(l.a), f(l.b), f(l.c)),
        Geo::Circle(c) => {
            out.push('<');
            pair(&mut out, &c.center, extra);
            out.push(',');
            out.push_str(&f(c.radius));
            out.push('>');
        }
    }
    out
}

// ── points ──

pub fn pt(x: f64, y: f64) -> Point {
    Point { x, y }
}

pub fn point_add(a: &Point, b: &Point) -> Result<Point> {
    Ok(pt(pl(a.x, b.x)?, pl(a.y, b.y)?))
}
pub fn point_sub(a: &Point, b: &Point) -> Result<Point> {
    Ok(pt(mi(a.x, b.x)?, mi(a.y, b.y)?))
}
pub fn point_mul(a: &Point, b: &Point) -> Result<Point> {
    Ok(pt(
        mi(mul(a.x, b.x)?, mul(a.y, b.y)?)?,
        pl(mul(a.x, b.y)?, mul(a.y, b.x)?)?,
    ))
}
pub fn point_div(a: &Point, b: &Point) -> Result<Point> {
    let d = pl(mul(b.x, b.x)?, mul(b.y, b.y)?)?;
    Ok(pt(
        div(pl(mul(a.x, b.x)?, mul(a.y, b.y)?)?, d)?,
        div(mi(mul(a.y, b.x)?, mul(a.x, b.y)?)?, d)?,
    ))
}

pub fn point_eq(a: &Point, b: &Point) -> bool {
    if a.x.is_nan() || a.y.is_nan() || b.x.is_nan() || b.y.is_nan() {
        return f8_eq(a.x, b.x) && f8_eq(a.y, b.y);
    }
    fp_eq(a.x, b.x) && fp_eq(a.y, b.y)
}

pub fn point_dt(a: &Point, b: &Point) -> Result<f64> {
    Ok(hypot(mi(a.x, b.x)?, mi(a.y, b.y)?))
}

pub fn point_sl(a: &Point, b: &Point) -> Result<f64> {
    if fp_eq(a.x, b.x) {
        return Ok(f64::INFINITY);
    }
    if fp_eq(a.y, b.y) {
        return Ok(0.0);
    }
    div(mi(a.y, b.y)?, mi(a.x, b.x)?)
}

fn point_invsl(a: &Point, b: &Point) -> Result<f64> {
    if fp_eq(a.x, b.x) {
        return Ok(0.0);
    }
    if fp_eq(a.y, b.y) {
        return Ok(f64::INFINITY);
    }
    div(mi(a.x, b.x)?, mi(b.y, a.y)?)
}

// ── lines ──

pub fn line_construct(p: &Point, m: f64) -> Result<Line> {
    Ok(if m.is_infinite() {
        Line {
            a: -1.0,
            b: 0.0,
            c: p.x,
        }
    } else if m == 0.0 {
        Line {
            a: 0.0,
            b: -1.0,
            c: p.y,
        }
    } else {
        let c = mi(p.y, mul(m, p.x)?)?;
        Line {
            a: m,
            b: -1.0,
            c: if c == 0.0 { 0.0 } else { c },
        }
    })
}

pub fn line_pp(a: &Point, b: &Point) -> Result<Line> {
    if point_eq(a, b) {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "invalid line specification: must be two distinct points",
        ));
    }
    line_construct(a, point_sl(a, b)?)
}

fn line_invsl(l: &Line) -> Result<f64> {
    if fp_zero(l.a) {
        return Ok(f64::INFINITY);
    }
    if fp_zero(l.b) {
        return Ok(0.0);
    }
    div(l.b, l.a)
}

pub fn line_sl(l: &Line) -> Result<f64> {
    if fp_zero(l.a) {
        return Ok(0.0);
    }
    if fp_zero(l.b) {
        return Ok(f64::INFINITY);
    }
    div(l.a, -l.b)
}

/// line_interpt_line: where two lines cross (None: parallel).
pub fn line_interpt(l1: &Line, l2: &Line) -> Result<Option<Point>> {
    let (x, y);
    if !fp_zero(l1.b) {
        if fp_eq(l2.a, mul(l1.a, div(l2.b, l1.b)?)?) {
            return Ok(None);
        }
        x = div(
            mi(mul(l1.b, l2.c)?, mul(l2.b, l1.c)?)?,
            mi(mul(l1.a, l2.b)?, mul(l2.a, l1.b)?)?,
        )?;
        y = div(-pl(mul(l1.a, x)?, l1.c)?, l1.b)?;
    } else if !fp_zero(l2.b) {
        if fp_eq(l1.a, mul(l2.a, div(l1.b, l2.b)?)?) {
            return Ok(None);
        }
        x = div(
            mi(mul(l2.b, l1.c)?, mul(l1.b, l2.c)?)?,
            mi(mul(l2.a, l1.b)?, mul(l1.a, l2.b)?)?,
        )?;
        y = div(-pl(mul(l2.a, x)?, l2.c)?, l2.b)?;
    } else {
        return Ok(None);
    }
    Ok(Some(pt(
        if x == 0.0 { 0.0 } else { x },
        if y == 0.0 { 0.0 } else { y },
    )))
}

pub fn line_perp(l1: &Line, l2: &Line) -> Result<bool> {
    if fp_zero(l1.a) {
        return Ok(fp_zero(l2.b));
    }
    if fp_zero(l2.a) {
        return Ok(fp_zero(l1.b));
    }
    if fp_zero(l1.b) {
        return Ok(fp_zero(l2.a));
    }
    if fp_zero(l2.b) {
        return Ok(fp_zero(l1.a));
    }
    Ok(fp_eq(div(mul(l1.a, l2.a)?, mul(l1.b, l2.b)?)?, -1.0))
}

pub fn line_eq(l1: &Line, l2: &Line) -> Result<bool> {
    if [l1.a, l1.b, l1.c, l2.a, l2.b, l2.c]
        .iter()
        .any(|v| v.is_nan())
    {
        return Ok(f8_eq(l1.a, l2.a) && f8_eq(l1.b, l2.b) && f8_eq(l1.c, l2.c));
    }
    let ratio = if !fp_zero(l2.a) {
        div(l1.a, l2.a)?
    } else if !fp_zero(l2.b) {
        div(l1.b, l2.b)?
    } else if !fp_zero(l2.c) {
        div(l1.c, l2.c)?
    } else {
        1.0
    };
    Ok(fp_eq(l1.a, mul(ratio, l2.a)?)
        && fp_eq(l1.b, mul(ratio, l2.b)?)
        && fp_eq(l1.c, mul(ratio, l2.c)?))
}

pub fn line_distance(l1: &Line, l2: &Line) -> Result<f64> {
    if line_interpt(l1, l2)?.is_some() {
        return Ok(0.0);
    }
    let ratio = if !fp_zero(l1.a) && !l1.a.is_nan() && !fp_zero(l2.a) && !l2.a.is_nan() {
        div(l1.a, l2.a)?
    } else if !fp_zero(l1.b) && !l1.b.is_nan() && !fp_zero(l2.b) && !l2.b.is_nan() {
        div(l1.b, l2.b)?
    } else {
        1.0
    };
    div(mi(l1.c, mul(ratio, l2.c)?)?.abs(), hypot(l1.a, l1.b))
}

pub fn line_contain_point(l: &Line, p: &Point) -> Result<bool> {
    Ok(fp_zero(pl(pl(mul(l.a, p.x)?, mul(l.b, p.y)?)?, l.c)?))
}

/// line_closept_point: the point of `l` closest to `p`, and the distance.
pub fn line_closept_point(l: &Line, p: &Point) -> Result<(Point, f64)> {
    let tmp = line_construct(p, line_invsl(l)?)?;
    match line_interpt(&tmp, l)? {
        Some(c) => Ok((c, point_dt(&c, p)?)),
        None => Ok((*p, 0.0)),
    }
}

// ── line segments ──

pub fn lseg_sl(l: &Lseg) -> Result<f64> {
    point_sl(&l.p[0], &l.p[1])
}

fn lseg_invsl(l: &Lseg) -> Result<f64> {
    point_invsl(&l.p[0], &l.p[1])
}

pub fn lseg_length(l: &Lseg) -> Result<f64> {
    point_dt(&l.p[0], &l.p[1])
}

pub fn lseg_center(l: &Lseg) -> Result<Point> {
    Ok(pt(
        div(pl(l.p[0].x, l.p[1].x)?, 2.0)?,
        div(pl(l.p[0].y, l.p[1].y)?, 2.0)?,
    ))
}

pub fn lseg_eq(a: &Lseg, b: &Lseg) -> bool {
    point_eq(&a.p[0], &b.p[0]) && point_eq(&a.p[1], &b.p[1])
}

pub fn lseg_contain_point(l: &Lseg, p: &Point) -> Result<bool> {
    Ok(fp_eq(
        point_dt(p, &l.p[0])? + point_dt(p, &l.p[1])?,
        point_dt(&l.p[0], &l.p[1])?,
    ))
}

/// lseg_interpt_line: where a segment meets a line.
pub fn lseg_interpt_line(s: &Lseg, l: &Line) -> Result<Option<Point>> {
    let tmp = line_construct(&s.p[0], lseg_sl(s)?)?;
    let Some(ip) = line_interpt(&tmp, l)? else {
        return Ok(None);
    };
    if !lseg_contain_point(s, &ip)? {
        return Ok(None);
    }
    Ok(Some(if point_eq(&s.p[0], &ip) {
        s.p[0]
    } else if point_eq(&s.p[1], &ip) {
        s.p[1]
    } else {
        ip
    }))
}

/// lseg_interpt_lseg: where two segments meet.
pub fn lseg_interpt(l1: &Lseg, l2: &Lseg) -> Result<Option<Point>> {
    let tmp = line_construct(&l2.p[0], lseg_sl(l2)?)?;
    let Some(ip) = lseg_interpt_line(l1, &tmp)? else {
        return Ok(None);
    };
    if !lseg_contain_point(l2, &ip)? {
        return Ok(None);
    }
    Ok(Some(ip))
}

pub fn lseg_parallel(a: &Lseg, b: &Lseg) -> Result<bool> {
    Ok(fp_eq(lseg_sl(a)?, lseg_sl(b)?))
}

pub fn lseg_perp(a: &Lseg, b: &Lseg) -> Result<bool> {
    Ok(fp_eq(lseg_sl(a)?, lseg_invsl(b)?))
}

/// lseg_closept_point: the point of `l` closest to `p`, and the distance.
pub fn lseg_closept_point(l: &Lseg, p: &Point) -> Result<(Point, f64)> {
    let tmp = line_construct(p, point_invsl(&l.p[0], &l.p[1])?)?;
    let (c, _) = lseg_closept_line(l, &tmp)?;
    Ok((c, point_dt(&c, p)?))
}

/// lseg_closept_line: the point of `s` closest to `l`, and the distance.
pub fn lseg_closept_line(s: &Lseg, l: &Line) -> Result<(Point, f64)> {
    if let Some(p) = lseg_interpt_line(s, l)? {
        return Ok((p, 0.0));
    }
    let d1 = line_closept_point(l, &s.p[0])?.1;
    let d2 = line_closept_point(l, &s.p[1])?.1;
    Ok(if d1 < d2 { (s.p[0], d1) } else { (s.p[1], d2) })
}

/// lseg_closept_lseg: the point of `on` closest to `to`, and the distance.
pub fn lseg_closept_lseg(on: &Lseg, to: &Lseg) -> Result<(Point, f64)> {
    if let Some(p) = lseg_interpt(on, to)? {
        return Ok((p, 0.0));
    }
    let (mut best, mut dist) = lseg_closept_point(on, &to.p[0])?;
    let (p, d) = lseg_closept_point(on, &to.p[1])?;
    if f8_lt(d, dist) {
        dist = d;
        best = p;
    }
    for e in on.p {
        let d = lseg_closept_point(to, &e)?.1;
        if f8_lt(d, dist) {
            dist = d;
            best = e;
        }
    }
    Ok((best, dist))
}

// ── boxes ──

pub fn box_construct(a: &Point, b: &Point) -> GBox {
    let (hx, lx) = if f8_gt(a.x, b.x) {
        (a.x, b.x)
    } else {
        (b.x, a.x)
    };
    let (hy, ly) = if f8_gt(a.y, b.y) {
        (a.y, b.y)
    } else {
        (b.y, a.y)
    };
    GBox {
        high: pt(hx, hy),
        low: pt(lx, ly),
    }
}

pub fn box_same(a: &GBox, b: &GBox) -> bool {
    point_eq(&a.high, &b.high) && point_eq(&a.low, &b.low)
}

pub fn box_ov(a: &GBox, b: &GBox) -> bool {
    fp_le(a.low.x, b.high.x)
        && fp_le(b.low.x, a.high.x)
        && fp_le(a.low.y, b.high.y)
        && fp_le(b.low.y, a.high.y)
}

pub fn box_contain_box(a: &GBox, b: &GBox) -> bool {
    fp_ge(a.high.x, b.high.x)
        && fp_le(a.low.x, b.low.x)
        && fp_ge(a.high.y, b.high.y)
        && fp_le(a.low.y, b.low.y)
}

pub fn box_contain_point(b: &GBox, p: &Point) -> bool {
    b.high.x >= p.x && b.low.x <= p.x && b.high.y >= p.y && b.low.y <= p.y
}

pub fn box_wd(b: &GBox) -> Result<f64> {
    mi(b.high.x, b.low.x)
}
pub fn box_ht(b: &GBox) -> Result<f64> {
    mi(b.high.y, b.low.y)
}
pub fn box_ar(b: &GBox) -> Result<f64> {
    mul(box_wd(b)?, box_ht(b)?)
}
pub fn box_center(b: &GBox) -> Result<Point> {
    Ok(pt(
        div(pl(b.high.x, b.low.x)?, 2.0)?,
        div(pl(b.high.y, b.low.y)?, 2.0)?,
    ))
}

pub fn box_intersect(a: &GBox, b: &GBox) -> Option<GBox> {
    if !box_ov(a, b) {
        return None;
    }
    Some(GBox {
        high: pt(f8_min(a.high.x, b.high.x), f8_min(a.high.y, b.high.y)),
        low: pt(f8_max(a.low.x, b.low.x), f8_max(a.low.y, b.low.y)),
    })
}

pub fn bound_box(a: &GBox, b: &GBox) -> GBox {
    GBox {
        high: pt(f8_max(a.high.x, b.high.x), f8_max(a.high.y, b.high.y)),
        low: pt(f8_min(a.low.x, b.low.x), f8_min(a.low.y, b.low.y)),
    }
}

/// box_closept_point: the point of `b` closest to `p`, and the distance.
pub fn box_closept_point(b: &GBox, p: &Point) -> Result<(Point, f64)> {
    if box_contain_point(b, p) {
        return Ok((*p, 0.0));
    }
    let mut corner = pt(b.low.x, b.high.y);
    let (mut best, mut dist) = lseg_closept_point(&Lseg { p: [b.low, corner] }, p)?;
    let mut try_seg = |s: Lseg| -> Result<()> {
        let (c, d) = lseg_closept_point(&s, p)?;
        if f8_lt(d, dist) {
            dist = d;
            best = c;
        }
        Ok(())
    };
    try_seg(Lseg {
        p: [b.high, corner],
    })?;
    corner = pt(b.high.x, b.low.y);
    try_seg(Lseg { p: [b.low, corner] })?;
    try_seg(Lseg {
        p: [b.high, corner],
    })?;
    Ok((best, dist))
}

/// box_interpt_lseg: whether a segment crosses a box, and (if so) the segment's point
/// closest to the box's center.
pub fn box_interpt_lseg(b: &GBox, s: &Lseg) -> Result<Option<Point>> {
    let lbox = GBox {
        high: pt(f8_max(s.p[0].x, s.p[1].x), f8_max(s.p[0].y, s.p[1].y)),
        low: pt(f8_min(s.p[0].x, s.p[1].x), f8_min(s.p[0].y, s.p[1].y)),
    };
    if !box_ov(&lbox, b) {
        return Ok(None);
    }
    let close = lseg_closept_point(s, &box_center(b)?)?.0;
    if box_contain_point(b, &s.p[0]) || box_contain_point(b, &s.p[1]) {
        return Ok(Some(close));
    }
    let c1 = pt(b.low.x, b.high.y);
    let c2 = pt(b.high.x, b.low.y);
    for seg in [
        Lseg { p: [b.low, c1] },
        Lseg { p: [b.high, c1] },
        Lseg { p: [b.low, c2] },
        Lseg { p: [b.high, c2] },
    ] {
        if lseg_interpt(&seg, s)?.is_some() {
            return Ok(Some(close));
        }
    }
    Ok(None)
}

/// box_closept_lseg: the point of `b` closest to `s`, and the distance.
pub fn box_closept_lseg(b: &GBox, s: &Lseg) -> Result<(Option<Point>, f64)> {
    if let Some(p) = box_interpt_lseg(b, s)? {
        return Ok((Some(p), 0.0));
    }
    let c1 = pt(b.low.x, b.high.y);
    let (mut best, mut dist) = lseg_closept_lseg(&Lseg { p: [b.low, c1] }, s)?;
    let c2 = pt(b.high.x, b.low.y);
    for seg in [
        Lseg { p: [b.high, c1] },
        Lseg { p: [b.low, c2] },
        Lseg { p: [b.high, c2] },
    ] {
        let (c, d) = lseg_closept_lseg(&seg, s)?;
        if f8_lt(d, dist) {
            dist = d;
            best = c;
        }
    }
    Ok((Some(best), dist))
}

pub fn inter_lb(l: &Line, b: &GBox) -> Result<bool> {
    let mut p1 = b.low;
    let mut p2 = pt(b.low.x, b.high.y);
    if lseg_interpt_line(&Lseg { p: [p1, p2] }, l)?.is_some() {
        return Ok(true);
    }
    p1 = b.high;
    if lseg_interpt_line(&Lseg { p: [p1, p2] }, l)?.is_some() {
        return Ok(true);
    }
    p2 = pt(b.high.x, b.low.y);
    if lseg_interpt_line(&Lseg { p: [p1, p2] }, l)?.is_some() {
        return Ok(true);
    }
    p1 = b.low;
    Ok(lseg_interpt_line(&Lseg { p: [p1, p2] }, l)?.is_some())
}

pub fn box_poly(b: &GBox) -> Polygon {
    Polygon {
        p: vec![b.low, pt(b.low.x, b.high.y), b.high, pt(b.high.x, b.low.y)],
        boundbox: *b,
    }
}

pub fn box_circle(b: &GBox) -> Result<Circle> {
    let center = box_center(b)?;
    Ok(Circle {
        center,
        radius: point_dt(&center, &b.high)?,
    })
}

// ── paths and polygons ──

/// A polygon with its bounding box (make_bound_box).
pub fn polygon(p: Vec<Point>) -> Result<Polygon> {
    let Some(first) = p.first() else {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "cannot create bounding box for empty polygon",
        ));
    };
    let (mut x1, mut x2, mut y1, mut y2) = (first.x, first.x, first.y, first.y);
    for q in &p[1..] {
        if f8_lt(q.x, x1) {
            x1 = q.x;
        }
        if f8_gt(q.x, x2) {
            x2 = q.x;
        }
        if f8_lt(q.y, y1) {
            y1 = q.y;
        }
        if f8_gt(q.y, y2) {
            y2 = q.y;
        }
    }
    Ok(Polygon {
        p,
        boundbox: GBox {
            high: pt(x2, y2),
            low: pt(x1, y1),
        },
    })
}

/// The segments of a path: consecutive points, plus the closing one of a closed path.
fn segments(p: &[Point], closed: bool) -> Vec<Lseg> {
    let mut out = Vec::new();
    for i in 0..p.len() {
        let prev = if i > 0 {
            i - 1
        } else if closed {
            p.len() - 1
        } else {
            continue;
        };
        out.push(Lseg { p: [p[prev], p[i]] });
    }
    out
}

pub fn path_length(p: &Path) -> Result<f64> {
    let mut r = 0.0;
    for s in segments(&p.p, p.closed) {
        r = pl(r, point_dt(&s.p[0], &s.p[1])?)?;
    }
    Ok(r)
}

pub fn path_area(p: &Path) -> Result<Option<f64>> {
    if !p.closed {
        return Ok(None);
    }
    let n = p.p.len();
    let mut area = 0.0;
    for i in 0..n {
        let j = (i + 1) % n;
        area = pl(area, mul(p.p[i].x, p.p[j].y)?)?;
        area = mi(area, mul(p.p[i].y, p.p[j].x)?)?;
    }
    Ok(Some(div(area.abs(), 2.0)?))
}

fn bbox_of(p: &[Point]) -> GBox {
    let mut b = GBox {
        high: p[0],
        low: p[0],
    };
    for q in &p[1..] {
        b.high.x = f8_max(q.x, b.high.x);
        b.high.y = f8_max(q.y, b.high.y);
        b.low.x = f8_min(q.x, b.low.x);
        b.low.y = f8_min(q.y, b.low.y);
    }
    b
}

pub fn path_inter(a: &Path, b: &Path) -> Result<bool> {
    if a.p.is_empty() || b.p.is_empty() || !box_ov(&bbox_of(&a.p), &bbox_of(&b.p)) {
        return Ok(false);
    }
    for s1 in segments(&a.p, a.closed) {
        for s2 in segments(&b.p, b.closed) {
            if lseg_interpt(&s1, &s2)?.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The smallest distance between any segments of `a` and `b` (None: no segments).
fn segs_distance(a: &[Lseg], b: &[Lseg]) -> Result<Option<f64>> {
    let mut min: Option<f64> = None;
    for s1 in a {
        for s2 in b {
            let d = lseg_closept_lseg(s1, s2)?.1;
            if min.is_none_or(|m| f8_lt(d, m)) {
                min = Some(d);
            }
        }
    }
    Ok(min)
}

pub fn path_distance(a: &Path, b: &Path) -> Result<Option<f64>> {
    segs_distance(&segments(&a.p, a.closed), &segments(&b.p, b.closed))
}

pub fn dist_ppath(p: &Point, path: &Path) -> Result<f64> {
    let mut min: Option<f64> = None;
    for s in segments(&path.p, path.closed) {
        let d = lseg_closept_point(&s, p)?.1;
        if min.is_none_or(|m| f8_lt(d, m)) {
            min = Some(d);
        }
    }
    Ok(min.unwrap_or(0.0))
}

pub fn on_ppath(p: &Point, path: &Path) -> Result<bool> {
    if !path.closed {
        let Some(first) = path.p.first() else {
            return Ok(false);
        };
        let mut a = point_dt(p, first)?;
        for w in path.p.windows(2) {
            let b = point_dt(p, &w[1])?;
            if fp_eq(pl(a, b)?, point_dt(&w[0], &w[1])?) {
                return Ok(true);
            }
            a = b;
        }
        return Ok(false);
    }
    Ok(point_inside(p, &path.p)? != 0)
}

const ON_POLYGON: i32 = i32::MAX;

fn lseg_crossing(x: f64, y: f64, prev_x: f64, prev_y: f64) -> Result<i32> {
    if fp_zero(y) {
        if fp_zero(x) {
            return Ok(ON_POLYGON);
        }
        if fp_gt(x, 0.0) {
            if fp_zero(prev_y) {
                return Ok(if fp_gt(prev_x, 0.0) { 0 } else { ON_POLYGON });
            }
            return Ok(if fp_lt(prev_y, 0.0) { 1 } else { -1 });
        }
        if fp_zero(prev_y) {
            return Ok(if fp_lt(prev_x, 0.0) { 0 } else { ON_POLYGON });
        }
        return Ok(0);
    }
    let y_sign = if fp_gt(y, 0.0) { 1 } else { -1 };
    if fp_zero(prev_y) {
        return Ok(if fp_lt(prev_x, 0.0) { 0 } else { y_sign });
    }
    if (y_sign < 0 && fp_lt(prev_y, 0.0)) || (y_sign > 0 && fp_gt(prev_y, 0.0)) {
        return Ok(0);
    }
    if fp_ge(x, 0.0) && fp_gt(prev_x, 0.0) {
        return Ok(2 * y_sign);
    }
    if fp_lt(x, 0.0) && fp_le(prev_x, 0.0) {
        return Ok(0);
    }
    let z = mi(mul(mi(x, prev_x)?, y)?, mul(mi(y, prev_y)?, x)?)?;
    if fp_zero(z) {
        return Ok(ON_POLYGON);
    }
    if (y_sign < 0 && fp_lt(z, 0.0)) || (y_sign > 0 && fp_gt(z, 0.0)) {
        return Ok(0);
    }
    Ok(2 * y_sign)
}

/// point_inside: 0 outside, 1 inside, 2 on the boundary.
pub fn point_inside(p: &Point, plist: &[Point]) -> Result<i32> {
    let Some(first) = plist.first() else {
        return Ok(0);
    };
    let x0 = mi(first.x, p.x)?;
    let y0 = mi(first.y, p.y)?;
    let (mut prev_x, mut prev_y) = (x0, y0);
    let mut total = 0i32;
    for q in &plist[1..] {
        let x = mi(q.x, p.x)?;
        let y = mi(q.y, p.y)?;
        let c = lseg_crossing(x, y, prev_x, prev_y)?;
        if c == ON_POLYGON {
            return Ok(2);
        }
        total += c;
        prev_x = x;
        prev_y = y;
    }
    let c = lseg_crossing(x0, y0, prev_x, prev_y)?;
    if c == ON_POLYGON {
        return Ok(2);
    }
    total += c;
    Ok(if total != 0 { 1 } else { 0 })
}

fn plist_same(p1: &[Point], p2: &[Point]) -> bool {
    let n = p1.len();
    for i in 0..n {
        if !point_eq(&p2[i], &p1[0]) {
            continue;
        }
        let mut j = i + 1;
        let mut ii = 1;
        while ii < n {
            if j >= n {
                j = 0;
            }
            if !point_eq(&p2[j], &p1[ii]) {
                break;
            }
            ii += 1;
            j += 1;
        }
        if ii == n {
            return true;
        }
        let mut j = i as isize - 1;
        let mut ii = 1;
        while ii < n {
            if j < 0 {
                j = n as isize - 1;
            }
            if !point_eq(&p2[j as usize], &p1[ii]) {
                break;
            }
            ii += 1;
            j -= 1;
        }
        if ii == n {
            return true;
        }
    }
    false
}

pub fn poly_same(a: &Polygon, b: &Polygon) -> bool {
    a.p.len() == b.p.len() && plist_same(&a.p, &b.p)
}

pub fn poly_overlap(a: &Polygon, b: &Polygon) -> Result<bool> {
    if !box_ov(&a.boundbox, &b.boundbox) {
        return Ok(false);
    }
    for sa in segments(&a.p, true) {
        for sb in segments(&b.p, true) {
            if lseg_interpt(&sa, &sb)?.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(point_inside(&a.p[0], &b.p)? != 0 || point_inside(&b.p[0], &a.p)? != 0)
}

fn lseg_inside_poly(a: &Point, b: &Point, poly: &Polygon, start: usize) -> Result<bool> {
    let t = Lseg { p: [*a, *b] };
    let n = poly.p.len();
    let mut s0 = poly.p[if start == 0 { n - 1 } else { start - 1 }];
    let mut res = true;
    let mut intersection = false;
    let mut i = start;
    while i < n && res {
        let s = Lseg { p: [s0, poly.p[i]] };
        if lseg_contain_point(&s, &t.p[0])? {
            if lseg_contain_point(&s, &t.p[1])? {
                return Ok(true);
            }
            res = lseg_inside_poly(&t.p[0], &t.p[1], poly, i + 1)?;
        } else if lseg_contain_point(&s, &t.p[1])? {
            res = lseg_inside_poly(&t.p[1], &t.p[0], poly, i + 1)?;
        } else if let Some(ip) = lseg_interpt(&t, &s)? {
            intersection = true;
            res = lseg_inside_poly(&t.p[0], &ip, poly, i + 1)?;
            if res {
                res = lseg_inside_poly(&t.p[1], &ip, poly, i + 1)?;
            }
        }
        s0 = s.p[1];
        i += 1;
    }
    if res && !intersection {
        let mid = pt(
            div(pl(t.p[0].x, t.p[1].x)?, 2.0)?,
            div(pl(t.p[0].y, t.p[1].y)?, 2.0)?,
        );
        res = point_inside(&mid, &poly.p)? != 0;
    }
    Ok(res)
}

/// poly_contain_poly: whether `a` contains `b`.
pub fn poly_contain(a: &Polygon, b: &Polygon) -> Result<bool> {
    if !box_contain_box(&a.boundbox, &b.boundbox) {
        return Ok(false);
    }
    let mut prev = b.p[b.p.len() - 1];
    for q in &b.p {
        if !lseg_inside_poly(&prev, q, a, 0)? {
            return Ok(false);
        }
        prev = *q;
    }
    Ok(true)
}

pub fn poly_distance(a: &Polygon, b: &Polygon) -> Result<Option<f64>> {
    if poly_overlap(a, b)? {
        return Ok(Some(0.0));
    }
    segs_distance(&segments(&a.p, true), &segments(&b.p, true))
}

pub fn dist_ppoly(p: &Point, poly: &Polygon) -> Result<f64> {
    if point_inside(p, &poly.p)? != 0 {
        return Ok(0.0);
    }
    let n = poly.p.len();
    let mut r = lseg_closept_point(
        &Lseg {
            p: [poly.p[0], poly.p[n - 1]],
        },
        p,
    )?
    .1;
    for w in poly.p.windows(2) {
        let d = lseg_closept_point(&Lseg { p: [w[0], w[1]] }, p)?.1;
        if f8_lt(d, r) {
            r = d;
        }
    }
    Ok(r)
}

pub fn poly_circle(poly: &Polygon) -> Result<Circle> {
    let n = poly.p.len() as f64;
    let mut c = pt(0.0, 0.0);
    for q in &poly.p {
        c = point_add(&c, q)?;
    }
    let center = pt(div(c.x, n)?, div(c.y, n)?);
    let mut r = 0.0;
    for q in &poly.p {
        r = pl(r, point_dt(q, &center)?)?;
    }
    Ok(Circle {
        center,
        radius: div(r, n)?,
    })
}

// ── circles ──

pub fn circle_ar(c: &Circle) -> Result<f64> {
    mul(mul(c.radius, c.radius)?, PI)
}

pub fn circle_same(a: &Circle, b: &Circle) -> bool {
    ((a.radius.is_nan() && b.radius.is_nan()) || fp_eq(a.radius, b.radius))
        && point_eq(&a.center, &b.center)
}

pub fn circle_box(c: &Circle) -> Result<GBox> {
    let d = div(c.radius, 2f64.sqrt())?;
    Ok(GBox {
        high: pt(pl(c.center.x, d)?, pl(c.center.y, d)?),
        low: pt(mi(c.center.x, d)?, mi(c.center.y, d)?),
    })
}

pub fn circle_poly(npts: i32, c: &Circle) -> Result<Polygon> {
    if fp_zero(c.radius) {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            "cannot convert circle with radius zero to polygon",
        ));
    }
    if npts < 2 {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "must request at least 2 points",
        ));
    }
    if npts > 1 << 26 {
        return Err(Error::new(
            code::PROGRAM_LIMIT_EXCEEDED,
            "too many points requested",
        ));
    }
    let step = div(2.0 * PI, npts as f64)?;
    let mut p = Vec::with_capacity(npts as usize);
    for i in 0..npts {
        let angle = mul(step, i as f64)?;
        p.push(pt(
            mi(c.center.x, mul(c.radius, angle.cos())?)?,
            pl(c.center.y, mul(c.radius, angle.sin())?)?,
        ));
    }
    polygon(p)
}

pub fn dist_cpoly(c: &Circle, poly: &Polygon) -> Result<f64> {
    let d = mi(dist_ppoly(&c.center, poly)?, c.radius)?;
    Ok(if d < 0.0 { 0.0 } else { d })
}

/// The geometric casts (pg_cast): the converted value, None if there is no such cast.
/// `assignment`: whether the cast applies on assignment (else explicit only).
pub fn cast_kind(from: &super::DataType, to: &super::DataType) -> Option<bool> {
    use super::DataType as T;
    Some(match (from, to) {
        (T::Point, T::GBox)
        | (T::Path, T::Polygon)
        | (T::GBox, T::Polygon)
        | (T::Polygon, T::Path) => true,
        (T::Lseg, T::Point)
        | (T::GBox, T::Point | T::Lseg | T::Circle)
        | (T::Polygon, T::Point | T::GBox | T::Circle)
        | (T::Circle, T::Point | T::GBox | T::Polygon) => false,
        _ => return None,
    })
}

pub fn cast(g: &Geo, to: &super::DataType) -> Result<Option<Geo>> {
    use super::DataType as T;
    Ok(Some(match (g, to) {
        (Geo::Point(p), T::GBox) => Geo::Box(GBox { high: *p, low: *p }),
        (Geo::Lseg(l), T::Point) => Geo::Point(lseg_center(l)?),
        (Geo::Path(p), T::Polygon) => {
            if !p.closed {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    "open path cannot be converted to polygon",
                ));
            }
            Geo::Polygon(polygon(p.p.clone())?)
        }
        (Geo::Box(b), T::Point) => Geo::Point(box_center(b)?),
        (Geo::Box(b), T::Lseg) => Geo::Lseg(Lseg { p: [b.high, b.low] }),
        (Geo::Box(b), T::Polygon) => Geo::Polygon(box_poly(b)),
        (Geo::Box(b), T::Circle) => Geo::Circle(box_circle(b)?),
        (Geo::Polygon(p), T::Point) => Geo::Point(poly_circle(p)?.center),
        (Geo::Polygon(p), T::Path) => Geo::Path(Path {
            closed: true,
            p: p.p.clone(),
        }),
        (Geo::Polygon(p), T::GBox) => Geo::Box(p.boundbox),
        (Geo::Polygon(p), T::Circle) => Geo::Circle(poly_circle(p)?),
        (Geo::Circle(c), T::Point) => Geo::Point(c.center),
        (Geo::Circle(c), T::GBox) => Geo::Box(circle_box(c)?),
        (Geo::Circle(c), T::Polygon) => Geo::Polygon(circle_poly(12, c)?),
        _ => return Ok(None),
    }))
}

/// `g[i]` of a fixed-length geometric type (None: out of range).
pub fn subscript(g: &Geo, i: i64) -> Option<super::Value> {
    use super::Value;
    let i = usize::try_from(i).ok()?;
    let vp = |p: Point| Value::Geo(std::sync::Arc::new(Geo::Point(p)));
    match g {
        Geo::Point(p) => [p.x, p.y].get(i).map(|f| Value::Float8(*f)),
        Geo::Line(l) => [l.a, l.b, l.c].get(i).map(|f| Value::Float8(*f)),
        Geo::Lseg(l) => l.p.get(i).map(|p| vp(*p)),
        Geo::Box(b) => [b.high, b.low].get(i).map(|p| vp(*p)),
        _ => None,
    }
}

/// `g[i] := v` of a fixed-length geometric type.
pub fn assign(g: &Geo, i: i64, v: &super::Value) -> Result<super::Value> {
    use super::Value;
    let range = || Error::new(code::ARRAY_SUBSCRIPT_ERROR, "array subscript out of range");
    let f = || v.as_f64().unwrap_or(f64::NAN);
    let p = || match v {
        Value::Geo(x) => match &**x {
            Geo::Point(p) => *p,
            _ => Point::default(),
        },
        _ => Point::default(),
    };
    let i = usize::try_from(i).map_err(|_| range())?;
    let out = match g {
        Geo::Point(q) => {
            let mut a = [q.x, q.y];
            *a.get_mut(i).ok_or_else(range)? = f();
            Geo::Point(Point { x: a[0], y: a[1] })
        }
        Geo::Line(l) => {
            let mut a = [l.a, l.b, l.c];
            *a.get_mut(i).ok_or_else(range)? = f();
            Geo::Line(Line {
                a: a[0],
                b: a[1],
                c: a[2],
            })
        }
        Geo::Lseg(l) => {
            let mut l = *l;
            *l.p.get_mut(i).ok_or_else(range)? = p();
            Geo::Lseg(l)
        }
        Geo::Box(b) => {
            let mut a = [b.high, b.low];
            *a.get_mut(i).ok_or_else(range)? = p();
            Geo::Box(GBox {
                high: a[0],
                low: a[1],
            })
        }
        _ => return Err(range()),
    };
    Ok(Value::Geo(std::sync::Arc::new(out)))
}
