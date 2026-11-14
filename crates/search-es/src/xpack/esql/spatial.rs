//! ES|QL spatial values: `geo_point`, `cartesian_point`, `geo_shape` and `cartesian_shape`.
//!
//! A spatial value is held as its well-known text, normalized the way Elasticsearch prints it
//! (`WellKnownText.toWKT`: `POINT (1.0 -1.0)`, parts of multi-geometries joined by `,`, rings
//! of a polygon by `, `), so equality, `VALUES`, `TO_STRING` and output need no other form.
//! [`parse`] is a strict reader of Elasticsearch's WKT dialect (case-insensitive keywords,
//! `EMPTY`, `BBOX`, `CIRCLE`, optional `z`, unparenthesized `MULTIPOINT` coordinates, trailing
//! input ignored); geo values must have latitudes in [-90, 90] and longitudes in [-180, 180]
//! (`GeographyValidator`), and anything that doesn't read is `null`, as `TO_GEOPOINT` and its
//! siblings answer. Source values (GeoJSON objects, `[lon, lat]` arrays, `{lat, lon}` /
//! `{x, y}` objects, `"lat,lon"` strings, geohashes) are read through [`from_value`].
//!
//! `ST_X` / `ST_Y` read coordinates as the index encodes them: geo points quantized to Lucene's
//! 32-bit lat/lon grid (`GeoEncodingUtils`), cartesian points to floats (`XYEncodingUtils`).
//! `ST_GEOMETRYTYPE`, `ST_DIMENSION` and `ST_ISEMPTY` treat a multi-valued shape as the
//! collection of its values, as Elasticsearch's doc values hold it.

use super::value::DataType;
use crate::json::Value;
use crate::json::write::java_double;
use crate::query::geo::Geometry;

pub fn is_spatial(t: DataType) -> bool {
    matches!(t, DataType::GeoPoint | DataType::CartesianPoint | DataType::GeoShape | DataType::CartesianShape)
}

pub fn is_geo(t: DataType) -> bool {
    matches!(t, DataType::GeoPoint | DataType::GeoShape)
}

/// Which coordinate checks a WKT conversion applies (`SpatialCoordinateTypes.validator()`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// `GeometryValidator.NOOP`: reading stored or source values.
    None,
    /// `GeographyValidator.instance(true)`: longitudes in [-180, 180], latitudes in [-90, 90],
    /// z values ignored.
    Geo,
    /// `CartesianValidator`: only envelope ordering.
    Cartesian,
}

impl Check {
    pub fn of(ty: DataType) -> Check {
        if is_geo(ty) { Check::Geo } else { Check::Cartesian }
    }

    /// `validateCoordinate(x, y, z)`.
    fn coordinate(self, x: f64, y: f64) -> Result<(), String> {
        if self == Check::Geo {
            // `checkLongitude` runs first.
            if x.is_nan() || !(-180.0..=180.0).contains(&x) {
                return Err(format!("invalid longitude {}; must be between -180.0 and 180.0", java_double(x)));
            }
            if y.is_nan() || !(-90.0..=90.0).contains(&y) {
                return Err(format!("invalid latitude {}; must be between -90.0 and 90.0", java_double(y)));
            }
        }
        Ok(())
    }

    /// `validateBBox(minX, maxX, maxY, minY)`.
    fn bbox(self, min_x: f64, max_x: f64, max_y: f64, min_y: f64) -> Result<(), String> {
        if self == Check::Cartesian && max_x < min_x {
            return Err("max x cannot be less than min x".into());
        }
        if self != Check::None && max_y < min_y {
            return Err("max y cannot be less than min y".into());
        }
        Ok(())
    }

    fn coordinates(self, c: &Coords) -> Result<(), String> {
        c.xs.iter().zip(&c.ys).try_for_each(|(x, y)| self.coordinate(*x, *y))
    }
}

/// `WellKnownText.MAX_NESTED_DEPTH`.
const MAX_NESTED_DEPTH: usize = 1000;

const Z_OR_M: &str = "When specifying 'Z' or 'M', coordinates must include three values. Only two coordinates were provided";

/// A token of the `StreamTokenizer` Elasticsearch configures (`WellKnownText.newTokenizer`):
/// words are runs of letters, digits, `-`, `+`, `.` and characters from U+00A0 up (numbers
/// are words too); every other character but space, tab, CR and LF is a token of its own.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Tok<'a> {
    Word(&'a str),
    Char(char),
    End,
}

/// `WellKnownBinary.fromWKT`'s reader: Elasticsearch's WKT grammar and its error texts.
struct Reader<'a> {
    s: &'a str,
    i: usize,
    /// The last token read (`ttype`/`sval`), which error messages print.
    cur: Tok<'a>,
    /// `pushBack()`: the next read returns `cur` again.
    pushed: bool,
    check: Check,
}

/// Parallel coordinate lists (`CoordsList`): z values only when every coordinate has one.
#[derive(Default)]
struct Coords {
    xs: Vec<f64>,
    ys: Vec<f64>,
    zs: Vec<f64>,
}

impl Coords {
    fn has_z(&self) -> bool {
        !self.zs.is_empty()
    }

    fn into_points(self) -> Vec<Vec<f64>> {
        let has_z = self.has_z();
        (0..self.xs.len()).map(|i| if has_z { vec![self.xs[i], self.ys[i], self.zs[i]] } else { vec![self.xs[i], self.ys[i]] }).collect()
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '.') || c as u32 >= 0xA0
}

impl<'a> Reader<'a> {
    fn token(&mut self) -> Tok<'a> {
        if self.pushed {
            self.pushed = false;
            return self.cur;
        }
        let b = self.s.as_bytes();
        loop {
            while self.i < b.len() && matches!(b[self.i], b' ' | b'\t' | b'\r' | b'\n') {
                self.i += 1;
            }
            // `#` starts a comment that runs to the end of the line.
            if self.i < b.len() && b[self.i] == b'#' {
                while self.i < b.len() && !matches!(b[self.i], b'\r' | b'\n') {
                    self.i += 1;
                }
                continue;
            }
            break;
        }
        let rest = &self.s[self.i..];
        self.cur = match rest.chars().next() {
            None => Tok::End,
            Some(c) if is_word_char(c) => {
                let end = rest.find(|c: char| !is_word_char(c)).unwrap_or(rest.len());
                self.i += end;
                Tok::Word(&rest[..end])
            }
            Some(c) => {
                self.i += c.len_utf8();
                Tok::Char(c)
            }
        };
        self.cur
    }

    fn push_back(&mut self) {
        self.pushed = true;
    }

    /// `tokenString`.
    fn token_string(&self) -> String {
        match self.cur {
            Tok::Word(w) => w.to_string(),
            Tok::End => "END-OF-STREAM".into(),
            Tok::Char(c) => format!("'{c}'"),
        }
    }

    /// `nextWord`: a word (`EMPTY` in any case as `EMPTY`) or one of `(`, `)`, `,`.
    fn word(&mut self) -> Result<&'a str, String> {
        match self.token() {
            Tok::Word(w) if w.eq_ignore_ascii_case("EMPTY") => Ok("EMPTY"),
            Tok::Word(w) => Ok(w),
            Tok::Char('(') => Ok("("),
            Tok::Char(')') => Ok(")"),
            Tok::Char(',') => Ok(","),
            _ => Err(format!("expected word but found: {}", self.token_string())),
        }
    }

    /// `nextNumber`: Java's `Double.parseDouble`, plus `NaN` in any case.
    fn number(&mut self) -> Result<f64, String> {
        match self.token() {
            Tok::Word(w) if w.eq_ignore_ascii_case("NaN") => Ok(f64::NAN),
            Tok::Word(w) => parse_java_double(w).ok_or_else(|| format!("invalid number found: {w}")),
            _ => Err(format!("expected number but found: {}", self.token_string())),
        }
    }

    /// `isNumberNext`: whether a word (any word) comes next.
    fn number_next(&mut self) -> bool {
        let t = self.token();
        self.push_back();
        matches!(t, Tok::Word(_))
    }

    /// `isZOrMNext`: an upper-case `Z` or `M` after the type name. A token that isn't a word
    /// is consumed (Elasticsearch doesn't push it back), so `POINT *(1 2)` reads.
    fn z_or_m_next(&mut self) -> bool {
        match self.word() {
            Ok("Z" | "M") => true,
            Ok(_) => {
                self.push_back();
                false
            }
            Err(_) => false,
        }
    }

    fn expect(&mut self, want: &str) -> Result<(), String> {
        if self.word()? == want { Ok(()) } else { Err(format!("expected {want} but found: {}", self.token_string())) }
    }

    /// `nextEmptyOrOpen`: `true` for `(`, `false` for `EMPTY`.
    fn empty_or_open(&mut self) -> Result<bool, String> {
        match self.word()? {
            "(" => Ok(true),
            "EMPTY" => Ok(false),
            _ => Err(format!("expected EMPTY or ( but found: {}", self.token_string())),
        }
    }

    /// `nextCloserOrComma`: `true` for `,`.
    fn comma_or_close(&mut self) -> Result<bool, String> {
        match self.word()? {
            "," => Ok(true),
            ")" => Ok(false),
            _ => Err(format!("expected , or ) but found: {}", self.token_string())),
        }
    }

    /// `parseCoordinates`: `x y [z], ... )` after the opening parenthesis.
    fn coordinates(&mut self) -> Result<Coords, String> {
        let mut c = Coords::default();
        loop {
            c.xs.push(self.number()?);
            c.ys.push(self.number()?);
            if self.number_next() {
                c.zs.push(self.number()?);
            }
            if c.has_z() && c.zs.len() != c.xs.len() {
                return Err(format!("coordinate dimensions do not match: {}", self.token_string()));
            }
            if !self.comma_or_close()? {
                return Ok(c);
            }
        }
    }

    /// `wktReadCoordinates`: the list, then each coordinate validated.
    fn checked_coordinates(&mut self) -> Result<Coords, String> {
        let c = self.coordinates()?;
        self.check.coordinates(&c)?;
        Ok(c)
    }

    /// `wktReadPolygonRings`'s body after the polygon's `(`: `( ring ), ( hole ) ... )`.
    fn rings(&mut self) -> Result<Vec<Coords>, String> {
        let mut rings = Vec::new();
        loop {
            self.expect("(")?;
            rings.push(self.checked_coordinates()?);
            if !self.comma_or_close()? {
                return Ok(rings);
            }
        }
    }

    fn geometry(&mut self, depth: usize) -> Result<Geometry, String> {
        let kind = self.word()?.to_lowercase();
        let explicit_z = self.z_or_m_next();
        let z_check = |has_z: bool| if explicit_z && !has_z { Err(Z_OR_M.to_string()) } else { Ok(()) };
        let points = |c: Coords| c.into_points();
        Ok(match kind.as_str() {
            "point" => {
                if !self.empty_or_open()? {
                    return Err("Empty POINT cannot be represented in WKB".into());
                }
                let x = self.number()?;
                let y = self.number()?;
                let z = if self.number_next() { self.number()? } else { f64::NAN };
                self.expect(")")?;
                // A NaN z is no z.
                z_check(!z.is_nan())?;
                self.check.coordinate(x, y)?;
                Geometry::Point(if z.is_nan() { vec![x, y] } else { vec![x, y, z] })
            }
            "multipoint" | "linestring" => {
                let c = if self.empty_or_open()? { self.checked_coordinates()? } else { Coords::default() };
                if !c.xs.is_empty() {
                    z_check(c.has_z())?;
                }
                if kind == "multipoint" { Geometry::MultiPoint(points(c)) } else { Geometry::LineString(points(c)) }
            }
            "polygon" => {
                if !self.empty_or_open()? {
                    return Ok(Geometry::Polygon(Vec::new()));
                }
                let rings = self.rings()?;
                let has_z = rings[0].has_z();
                if rings.iter().any(|r| r.has_z() != has_z) {
                    return Err("holes must have the same number of dimensions as the polygon".into());
                }
                z_check(has_z)?;
                Geometry::Polygon(rings.into_iter().map(points).collect())
            }
            "multilinestring" => {
                if !self.empty_or_open()? {
                    return Ok(Geometry::MultiLineString(Vec::new()));
                }
                let mut lines: Vec<Coords> = Vec::new();
                loop {
                    let line = if self.empty_or_open()? { self.checked_coordinates()? } else { Coords::default() };
                    if lines.first().is_some_and(|f| f.has_z() != line.has_z()) {
                        return Err("all elements of the collection should have the same number of dimension".into());
                    }
                    lines.push(line);
                    if !self.comma_or_close()? {
                        break;
                    }
                }
                z_check(lines[0].has_z())?;
                Geometry::MultiLineString(lines.into_iter().map(points).collect())
            }
            "multipolygon" => {
                if !self.empty_or_open()? {
                    return Ok(Geometry::MultiPolygon(Vec::new()));
                }
                let mut polys: Vec<Vec<Coords>> = Vec::new();
                let poly_z = |p: &[Coords]| p.first().is_some_and(Coords::has_z);
                loop {
                    let poly = if self.empty_or_open()? { self.rings()? } else { Vec::new() };
                    if polys.first().is_some_and(|f| poly_z(f) != poly_z(&poly)) {
                        return Err("all elements of the collection should have the same number of dimension".into());
                    }
                    polys.push(poly);
                    if !self.comma_or_close()? {
                        break;
                    }
                }
                z_check(poly_z(&polys[0]))?;
                Geometry::MultiPolygon(polys.into_iter().map(|p| p.into_iter().map(points).collect()).collect())
            }
            "geometrycollection" => {
                if !self.empty_or_open()? {
                    return Ok(Geometry::GeometryCollection(Vec::new()));
                }
                if depth >= MAX_NESTED_DEPTH {
                    return Err(format!("maximum nested depth of {MAX_NESTED_DEPTH} exceeded"));
                }
                let mut gs: Vec<Geometry> = Vec::new();
                loop {
                    let g = self.geometry(depth + 1)?;
                    if gs.first().is_some_and(|f| has_z(f) != has_z(&g)) {
                        return Err("all elements of the collection should have the same number of dimension".into());
                    }
                    gs.push(g);
                    if !self.comma_or_close()? {
                        break;
                    }
                }
                z_check(has_z(&gs[0]))?;
                Geometry::GeometryCollection(gs)
            }
            "circle" => {
                if !self.empty_or_open()? {
                    return Err("Empty CIRCLE cannot be represented in WKB".into());
                }
                let x = self.number()?;
                let y = self.number()?;
                let r = self.number()?;
                let z = if self.number_next() { self.number()? } else { f64::NAN };
                self.expect(")")?;
                self.check.coordinate(x, y)?;
                z_check(!z.is_nan())?;
                Geometry::Circle { center: if z.is_nan() { vec![x, y] } else { vec![x, y, z] }, radius_meters: r }
            }
            "bbox" => {
                if !self.empty_or_open()? {
                    return Err("Empty ENVELOPE cannot be represented in WKB".into());
                }
                let min_x = self.number()?;
                self.expect(",")?;
                let max_x = self.number()?;
                self.expect(",")?;
                let max_y = self.number()?;
                self.expect(",")?;
                let min_y = self.number()?;
                self.expect(")")?;
                self.check.bbox(min_x, max_x, max_y, min_y)?;
                self.check.coordinate(min_x, min_y)?;
                self.check.coordinate(max_x, max_y)?;
                z_check(false)?;
                Geometry::Envelope { min_x, max_x, max_y, min_y }
            }
            _ => return Err(format!("Unknown geometry type: {kind}")),
        })
    }
}

/// Whether a geometry's WKB type has z (`wkbTypeHasZ`): empty geometries don't.
fn has_z(g: &Geometry) -> bool {
    match g {
        Geometry::Point(c) | Geometry::Circle { center: c, .. } => c.len() > 2,
        Geometry::MultiPoint(cs) | Geometry::LineString(cs) => cs.first().is_some_and(|c| c.len() > 2),
        Geometry::MultiLineString(ls) => ls.first().and_then(|l| l.first()).is_some_and(|c| c.len() > 2),
        Geometry::Polygon(rs) => rs.first().and_then(|r| r.first()).is_some_and(|c| c.len() > 2),
        Geometry::MultiPolygon(ps) => ps.first().and_then(|p| p.first()).and_then(|r| r.first()).is_some_and(|c| c.len() > 2),
        Geometry::GeometryCollection(gs) => gs.first().is_some_and(has_z),
        Geometry::Envelope { .. } => false,
    }
}

/// Java's `Double.parseDouble` on a WKT word: decimal (with an optional `d`/`f` suffix),
/// `Infinity`, and hexadecimal floating point (`0x1p3`).
fn parse_java_double(w: &str) -> Option<f64> {
    if let Some(x) = crate::units::parse_java_double(w) {
        return Some(x);
    }
    let t = w.strip_suffix(['d', 'D', 'f', 'F']).unwrap_or(w);
    let (neg, t) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))?;
    let (mantissa, exp) = t.split_once(['p', 'P'])?;
    let exp: i32 = exp.strip_prefix('+').unwrap_or(exp).parse().ok()?;
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    let mut v = 0.0f64;
    for c in int.chars() {
        v = v * 16.0 + c.to_digit(16)? as f64;
    }
    let mut scale = 1.0 / 16.0;
    for c in frac.chars() {
        v += c.to_digit(16)? as f64 * scale;
        scale /= 16.0;
    }
    let v = v * 2f64.powi(exp);
    Some(if neg { -v } else { v })
}

/// Reads Elasticsearch's WKT as `TO_GEOSHAPE` and its siblings do (`wktToWkb`), with the
/// coordinate checks of `check`. Trailing input after the geometry is ignored. `Err` holds
/// the parser's message (Elasticsearch prefixes it with `Failed to parse WKT: `).
pub fn parse_checked(text: &str, check: Check) -> Result<Geometry, String> {
    Reader { s: text, i: 0, cur: Tok::End, pushed: false, check }.geometry(0)
}

/// Reads Elasticsearch's WKT without coordinate checks (`None`: it doesn't parse).
pub fn parse(text: &str) -> Option<Geometry> {
    parse_checked(text, Check::None).ok()
}

/// WKT text as a value of spatial type `ty`, normalized; `Err` is the Java exception text of
/// the warning Elasticsearch records.
pub fn convert_checked(text: &str, ty: DataType) -> Result<String, String> {
    match parse_checked(text, Check::of(ty)) {
        Ok(g) => Ok(to_wkt(&g)),
        Err(m) => Err(format!("java.lang.IllegalArgumentException: Failed to parse WKT: {m}")),
    }
}

/// WKT text as a value of spatial type `ty`, normalized (`None`: not a valid value).
pub fn convert(text: &str, ty: DataType) -> Option<String> {
    convert_checked(text, ty).ok()
}

fn coords_ok(g: &Geometry, ok: &dyn Fn(&[f64]) -> bool) -> bool {
    match g {
        Geometry::Point(c) => c.is_empty() || ok(c),
        Geometry::MultiPoint(cs) | Geometry::LineString(cs) => cs.iter().all(|c| ok(c)),
        Geometry::MultiLineString(ls) | Geometry::Polygon(ls) => ls.iter().flatten().all(|c| ok(c)),
        Geometry::MultiPolygon(ps) => ps.iter().flatten().flatten().all(|c| ok(c)),
        Geometry::GeometryCollection(gs) => gs.iter().all(|x| coords_ok(x, ok)),
        Geometry::Envelope { min_x, max_x, max_y, min_y } => ok(&[*min_x, *max_y]) && ok(&[*max_x, *min_y]),
        Geometry::Circle { center, .. } => ok(center),
    }
}

/// Whether a value read from a document can be a value of `ty`.
fn valid(g: &Geometry, ty: DataType) -> bool {
    if matches!(ty, DataType::GeoPoint | DataType::CartesianPoint) && matches!(g, Geometry::Point(c) if c.is_empty()) {
        return false;
    }
    !is_geo(ty) || coords_ok(g, &|c| (-180.0..=180.0).contains(&c[0]) && (-90.0..=90.0).contains(&c[1]))
}

fn num(x: f64) -> String {
    java_double(x)
}

fn coord(c: &[f64]) -> String {
    c.iter().map(|x| num(*x)).collect::<Vec<_>>().join(" ")
}

fn list(cs: &[Vec<f64>]) -> String {
    format!("({})", cs.iter().map(|c| coord(c)).collect::<Vec<_>>().join(", "))
}

fn rings(ls: &[Vec<Vec<f64>>]) -> String {
    format!("({})", ls.iter().map(|l| list(l)).collect::<Vec<_>>().join(", "))
}

/// `WellKnownText.toWKT`.
pub fn to_wkt(g: &Geometry) -> String {
    let empty = |name: &str| format!("{name} EMPTY");
    match g {
        Geometry::Point(c) if c.is_empty() => empty("POINT"),
        Geometry::Point(c) => format!("POINT ({})", coord(c)),
        Geometry::MultiPoint(cs) if cs.is_empty() => empty("MULTIPOINT"),
        Geometry::MultiPoint(cs) => format!("MULTIPOINT {}", list(cs)),
        Geometry::LineString(cs) if cs.is_empty() => empty("LINESTRING"),
        Geometry::LineString(cs) => format!("LINESTRING {}", list(cs)),
        Geometry::MultiLineString(ls) if ls.is_empty() => empty("MULTILINESTRING"),
        // Empty parts print as `EMPTY` (`WellKnownText.fromWKB`).
        Geometry::MultiLineString(ls) => format!("MULTILINESTRING ({})", ls.iter().map(|l| if l.is_empty() { "EMPTY".into() } else { list(l) }).collect::<Vec<_>>().join(",")),
        Geometry::Polygon(ls) if ls.is_empty() => empty("POLYGON"),
        Geometry::Polygon(ls) => format!("POLYGON {}", rings(ls)),
        Geometry::MultiPolygon(ps) if ps.is_empty() => empty("MULTIPOLYGON"),
        Geometry::MultiPolygon(ps) => format!("MULTIPOLYGON ({})", ps.iter().map(|p| if p.is_empty() { "EMPTY".into() } else { rings(p) }).collect::<Vec<_>>().join(",")),
        Geometry::GeometryCollection(gs) if gs.is_empty() => empty("GEOMETRYCOLLECTION"),
        Geometry::GeometryCollection(gs) => format!("GEOMETRYCOLLECTION ({})", gs.iter().map(to_wkt).collect::<Vec<_>>().join(",")),
        Geometry::Envelope { min_x, max_x, max_y, min_y } => format!("BBOX ({}, {}, {}, {})", num(*min_x), num(*max_x), num(*max_y), num(*min_y)),
        Geometry::Circle { center, radius_meters } => format!("CIRCLE ({} {})", coord(center), num(*radius_meters)),
    }
}

/// A point as a source document gives it: `[x, y]`, `{lat, lon}` / `{x, y}`, `"lat,lon"` (or
/// `"x,y"` for cartesian points) or a geohash.
fn point_of(v: &Value, ty: DataType) -> Option<Vec<f64>> {
    let geo = is_geo(ty);
    // `[x, y(, z)]`: the field mapper reads the coordinates through Jackson's numeric
    // accessors, so numeric strings coerce.
    if let Value::Arr(a) = v
        && a.first().is_some_and(|x| matches!(x, Value::Num(_)))
    {
        let num = |x: &Value| match x {
            Value::Num(n) => Some(n.as_f64()),
            Value::Str(s) => crate::units::parse_java_double(s.trim()),
            _ => None,
        };
        return Some(vec![num(a.first()?)?, num(a.get(1)?)?]);
    }
    // Values the field's own parser accepted at index time (z values dropped).
    if !crate::opensearch::active() {
        let mut p = crate::query::XParser::from_value(v);
        p.next_token();
        let kind = if geo { crate::query::geo::PointKind::Geo } else { crate::query::geo::PointKind::Cartesian };
        return crate::query::geo::parse_field_point(&mut p, kind, true).ok().map(|pt| vec![pt.lon, pt.lat]);
    }
    match v {
        Value::Arr(a) if (2..=3).contains(&a.len()) && a.iter().all(|x| x.as_f64().is_some()) => Some(a.iter().filter_map(Value::as_f64).collect()),
        Value::Obj(m) if geo && m.contains_key("lat") => Some(vec![m.get("lon")?.as_f64().or_else(|| m.get("lon")?.as_str()?.trim().parse().ok())?, m.get("lat")?.as_f64().or_else(|| m.get("lat")?.as_str()?.trim().parse().ok())?]),
        Value::Obj(m) if !geo && m.contains_key("x") => Some(vec![m.get("x")?.as_f64()?, m.get("y")?.as_f64()?]),
        Value::Str(s) => {
            let parts: Vec<&str> = s.split(',').map(str::trim).collect();
            if parts.len() >= 2 && parts.iter().all(|p| p.parse::<f64>().is_ok()) {
                let n: Vec<f64> = parts.iter().filter_map(|p| p.parse().ok()).collect();
                // `"lat,lon"` for geo points, `"x,y"` for cartesian ones.
                return Some(if geo { [vec![n[1], n[0]], n[2..].to_vec()].concat() } else { n });
            }
            if geo {
                let p = crate::query::geo::geohash_decode(s).ok()?;
                return Some(vec![p.lon, p.lat]);
            }
            None
        }
        _ => None,
    }
}

/// One value of a spatial field read from `_source` or the search API's `fields` (`None`:
/// unreadable, so absent).
pub fn from_value(v: &Value, ty: DataType) -> Option<String> {
    let g = match v {
        // Point strings go through the field's parser (WKT z values are dropped there).
        Value::Str(_) if matches!(ty, DataType::GeoPoint | DataType::CartesianPoint) => Geometry::Point(point_of(v, ty)?),
        Value::Str(s) => parse(s)?,
        Value::Obj(_) if matches!(ty, DataType::GeoPoint | DataType::CartesianPoint) => Geometry::Point(point_of(v, ty)?),
        Value::Obj(m) if m.contains_key("type") => crate::query::geo::parse_geojson(m).ok()?,
        other if matches!(ty, DataType::GeoPoint | DataType::CartesianPoint) => Geometry::Point(point_of(other, ty)?),
        _ => return None,
    };
    valid(&g, ty).then(|| to_wkt(&g))
}

/// `GeoEncodingUtils.decodeLongitude(encodeLongitude(x))` and its latitude twin.
fn quantize_geo(x: f64, max: f64) -> f64 {
    let step = 2.0 * max / (1u64 << 32) as f64;
    let x = if x == max { next_down(x) } else { x };
    (x / step).floor() as i32 as f64 * step
}

fn next_down(x: f64) -> f64 {
    if x > 0.0 { f64::from_bits(x.to_bits() - 1) } else { x }
}

/// `ST_X` (`y = false`) / `ST_Y` of a point value. A cartesian coordinate that isn't a finite
/// float fails like `XYEncodingUtils.checkVal` (`Err`: the warning's exception text).
pub fn st_coordinate(wkt: &str, ty: DataType, y: bool) -> Result<Option<f64>, String> {
    let Some(Geometry::Point(c)) = parse(wkt) else { return Ok(None) };
    let Some(&v) = c.get(usize::from(y)) else { return Ok(None) };
    Ok(Some(match ty {
        DataType::GeoPoint => quantize_geo(v, if y { 90.0 } else { 180.0 }),
        _ => {
            let f = v as f32;
            if !f.is_finite() {
                return Err(format!(
                    "java.lang.IllegalArgumentException: invalid value {}; must be between -3.4028234663852886E38 and 3.4028234663852886E38",
                    java_double(f as f64)
                ));
            }
            f as f64
        }
    }))
}

/// The geometry a (possibly multi-valued) shape value stands for.
fn geometry_of(values: &[&str]) -> Option<Geometry> {
    match values {
        [one] => parse(one),
        many => Some(Geometry::GeometryCollection(many.iter().map(|v| parse(v)).collect::<Option<Vec<_>>>()?)),
    }
}

/// `ST_GEOMETRYTYPE`.
pub fn geometry_type(values: &[&str]) -> Option<String> {
    let name = match geometry_of(values)? {
        Geometry::Point(_) => "Point",
        Geometry::MultiPoint(_) => "MultiPoint",
        Geometry::LineString(_) => "LineString",
        Geometry::MultiLineString(_) => "MultiLineString",
        // Envelopes and circles are polygons once encoded.
        Geometry::Polygon(_) | Geometry::Envelope { .. } | Geometry::Circle { .. } => "Polygon",
        Geometry::MultiPolygon(_) => "MultiPolygon",
        Geometry::GeometryCollection(_) => "GeometryCollection",
    };
    Some(format!("ST_{name}"))
}

fn dimension(g: &Geometry) -> i32 {
    match g {
        Geometry::Point(_) | Geometry::MultiPoint(_) => 0,
        Geometry::LineString(_) | Geometry::MultiLineString(_) => 1,
        Geometry::GeometryCollection(gs) => gs.iter().map(dimension).max().unwrap_or(0),
        _ => 2,
    }
}

/// `ST_DIMENSION`.
pub fn st_dimension(values: &[&str]) -> Option<i32> {
    geometry_of(values).map(|g| dimension(&g))
}

fn empty(g: &Geometry) -> bool {
    match g {
        Geometry::Point(c) => c.is_empty(),
        Geometry::MultiPoint(cs) | Geometry::LineString(cs) => cs.is_empty(),
        Geometry::MultiLineString(ls) | Geometry::Polygon(ls) => ls.is_empty(),
        Geometry::MultiPolygon(ps) => ps.is_empty(),
        Geometry::GeometryCollection(gs) => gs.iter().all(empty),
        _ => false,
    }
}

/// `ST_ISEMPTY`.
pub fn st_is_empty(values: &[&str]) -> Option<bool> {
    geometry_of(values).map(|g| empty(&g))
}

/// Geohash's base 32 alphabet.
const GEOHASH_BASE32: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";

/// A grid cell's address from its long encoding: a geohash (`Geohash.stringEncode`), a
/// `zoom/x/y` tile (`GeoTileUtils.stringEncode`) or an H3 index in hex.
pub fn grid_address(cell: i64, ty: DataType) -> String {
    match ty {
        DataType::Geohash => {
            let level = (cell & 15) as usize;
            let mut bits = (cell as u64) >> 4;
            let mut chars = vec![b'0'; level];
            for c in chars.iter_mut().rev() {
                *c = GEOHASH_BASE32[(bits & 31) as usize];
                bits >>= 5;
            }
            String::from_utf8(chars).expect("ascii")
        }
        DataType::Geotile => {
            let mask = (1u64 << 29) - 1;
            let c = cell as u64;
            format!("{}/{}/{}", c >> 58, (c >> 29) & mask, c & mask)
        }
        _ => format!("{:x}", cell as u64),
    }
}

/// A grid cell's long encoding from its address, as Elasticsearch's conversions read it
/// (`Geohash.longEncode`, `GeoTileUtils.longEncode`, `H3.stringToH3`); the error is the Java
/// exception text.
pub fn grid_parse(s: &str, ty: DataType) -> Result<i64, String> {
    match ty {
        DataType::Geohash => {
            // At most 12 levels; characters outside the alphabet read as -1, like Java's `indexOf`.
            let length = s.encode_utf16().count() as i64;
            let mut level = length - 1;
            let mut cell: i64 = 0;
            for c in s.encode_utf16() {
                let b = GEOHASH_BASE32.iter().position(|&x| u16::from(x) == c).map_or(-1, |i| i as i64);
                cell |= b.wrapping_shl((level * 5) as u32);
                level -= 1;
                if level < 0 {
                    break;
                }
            }
            Ok(cell.wrapping_shl(4) | length)
        }
        DataType::Geotile => {
            let invalid = || format!("java.lang.IllegalArgumentException: Invalid geotile_grid hash string of {s}. Must be three integers in a form \"zoom/x/y\".");
            let parts: Vec<&str> = s.splitn(4, '/').collect();
            let [z, x, y] = parts.as_slice() else { return Err(invalid()) };
            let n = |p: &str| p.parse::<i32>().map(i64::from).map_err(|_| invalid());
            Ok(n(z)?.wrapping_shl(58) | n(x)?.wrapping_shl(29) | n(y)?)
        }
        _ => {
            if s.starts_with('-') {
                return Err(format!("java.lang.NumberFormatException: Illegal leading minus sign on unsigned string {s}."));
            }
            u64::from_str_radix(s, 16).map(|v| v as i64).map_err(|e| match e.kind() {
                std::num::IntErrorKind::PosOverflow => format!("java.lang.NumberFormatException: String value {s} exceeds range of unsigned long."),
                _ => format!("java.lang.NumberFormatException: For input string: \"{s}\" under radix 16"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> Option<String> {
        convert(s, DataType::GeoShape)
    }

    #[test]
    fn wkt_prints_like_elasticsearch() {
        assert_eq!(norm("POINT(1 2)").unwrap(), "POINT (1.0 2.0)");
        assert_eq!(norm(" point (1e2   2.50) x").unwrap(), "POINT (100.0 2.5)");
        assert_eq!(norm("POINT(1 2 3)").unwrap(), "POINT (1.0 2.0 3.0)");
        assert_eq!(norm("MULTIPOINT(1 2, 3 4)").unwrap(), "MULTIPOINT (1.0 2.0, 3.0 4.0)");
        assert_eq!(norm("MULTILINESTRING((0 0, 1 1), (2 2, 3 3))").unwrap(), "MULTILINESTRING ((0.0 0.0, 1.0 1.0),(2.0 2.0, 3.0 3.0))");
        assert_eq!(norm("POLYGON((0 0, 10 0, 10 10, 0 0), (1 1, 2 1, 2 2, 1 1))").unwrap(), "POLYGON ((0.0 0.0, 10.0 0.0, 10.0 10.0, 0.0 0.0), (1.0 1.0, 2.0 1.0, 2.0 2.0, 1.0 1.0))");
        assert_eq!(
            norm("MULTIPOLYGON(((0 0, 1 0, 1 1, 0 0)), ((5 5, 6 5, 6 6, 5 5)))").unwrap(),
            "MULTIPOLYGON (((0.0 0.0, 1.0 0.0, 1.0 1.0, 0.0 0.0)),((5.0 5.0, 6.0 5.0, 6.0 6.0, 5.0 5.0)))"
        );
        assert_eq!(norm("GEOMETRYCOLLECTION(POINT(1 2), LINESTRING(0 0, 1 1))").unwrap(), "GEOMETRYCOLLECTION (POINT (1.0 2.0),LINESTRING (0.0 0.0, 1.0 1.0))");
        assert_eq!(norm("BBOX(0, 1, 1, 0)").unwrap(), "BBOX (0.0, 1.0, 1.0, 0.0)");
        assert_eq!(norm("CIRCLE(1 2 3)").unwrap(), "CIRCLE (1.0 2.0 3.0)");
        assert_eq!(norm("LINESTRING EMPTY").unwrap(), "LINESTRING EMPTY");
        // Unclosed rings aren't checked.
        assert_eq!(norm("POLYGON((0 0, 1 0, 1 1))").unwrap(), "POLYGON ((0.0 0.0, 1.0 0.0, 1.0 1.0))");
    }

    #[test]
    fn invalid_values_are_none() {
        assert_eq!(norm("bad"), None);
        assert_eq!(norm("MULTIPOINT((1 2), (3 4))"), None);
        assert_eq!(norm("LINEARRING(0 0, 1 1, 1 0, 0 0)"), None);
        assert_eq!(norm("POLYGON((0 0, 1 0, 1 1, 0 0), EMPTY)"), None);
        assert_eq!(convert("POINT(200 100)", DataType::GeoPoint), None);
        assert_eq!(convert("POINT EMPTY", DataType::GeoPoint), None);
        assert_eq!(convert("POINT(200 100)", DataType::CartesianPoint).unwrap(), "POINT (200.0 100.0)");
        // Point conversions keep other geometries.
        assert_eq!(convert("LINESTRING(0 0, 1 1)", DataType::GeoPoint).unwrap(), "LINESTRING (0.0 0.0, 1.0 1.0)");
    }

    #[test]
    fn source_values() {
        let ty = DataType::GeoPoint;
        assert_eq!(from_value(&Value::from("POINT(1 -1)"), ty).unwrap(), "POINT (1.0 -1.0)");
        assert_eq!(from_value(&Value::from("41.12,-71.34"), ty).unwrap(), "POINT (-71.34 41.12)");
        assert_eq!(from_value(&crate::arr![-71.34, 41.12], ty).unwrap(), "POINT (-71.34 41.12)");
        assert_eq!(from_value(&crate::obj! { "lat" => 41.12, "lon" => -71.34 }, ty).unwrap(), "POINT (-71.34 41.12)");
        assert_eq!(from_value(&crate::obj! { "x" => 3.0, "y" => 4.0 }, DataType::CartesianPoint).unwrap(), "POINT (3.0 4.0)");
        assert_eq!(from_value(&crate::obj! { "type" => "Point", "coordinates" => crate::arr![1.0, 2.0] }, DataType::GeoShape).unwrap(), "POINT (1.0 2.0)");
    }

    #[test]
    fn coordinates_are_quantized_like_the_index() {
        assert_eq!(st_coordinate("POINT (1.0 2.0)", DataType::GeoPoint, false), Ok(Some(0.9999999403953552)));
        assert_eq!(st_coordinate("POINT (2.0 2.0)", DataType::GeoPoint, false), Ok(Some(1.9999999646097422)));
        assert_eq!(st_coordinate("POINT (-180.0 -90.0)", DataType::GeoPoint, false), Ok(Some(-180.0)));
        assert_eq!(st_coordinate("POINT (180.0 90.0)", DataType::GeoPoint, true), Ok(Some(89.99999995809048)));
        assert_eq!(st_coordinate("POINT (0.1 0.2)", DataType::CartesianPoint, false), Ok(Some(0.10000000149011612)));
        assert_eq!(st_coordinate("POINT (1.0 2.0)", DataType::CartesianPoint, true), Ok(Some(2.0)));
    }

    /// Elasticsearch 9.5.5's `TO_GEOSHAPE` / `TO_CARTESIAN*` outcomes (`Failed to parse WKT: …`).
    #[test]
    fn wkt_errors_match_elasticsearch() {
        let err = |s: &str, ty: DataType| convert_checked(s, ty).unwrap_err().trim_start_matches("java.lang.IllegalArgumentException: Failed to parse WKT: ").to_string();
        let g = DataType::GeoShape;
        let c = DataType::CartesianShape;
        assert_eq!(err("POINT (200 2)", g), "invalid longitude 200.0; must be between -180.0 and 180.0");
        assert_eq!(err("POINT (1 100)", g), "invalid latitude 100.0; must be between -90.0 and 90.0");
        assert_eq!(err("POINT (1e400 2)", DataType::GeoPoint), "invalid longitude Infinity; must be between -180.0 and 180.0");
        assert_eq!(err("POINT (NaN NaN)", DataType::GeoPoint), "invalid longitude NaN; must be between -180.0 and 180.0");
        assert_eq!(err("POLYGON ((0 0, 1 0, 1 1, 0 0)", g), "expected word but found: END-OF-STREAM");
        assert_eq!(err("", g), "expected word but found: END-OF-STREAM");
        assert_eq!(err("BBOX (1, 2, 3)", g), "expected , but found: ')'");
        assert_eq!(err("BBOX (1, 2, 3, 4)", g), "max y cannot be less than min y");
        assert_eq!(err("BBOX (3, 2, 4, 1)", c), "max x cannot be less than min x");
        assert_eq!(err("POINT(日 1)", g), "invalid number found: 日");
        assert_eq!(err("POINT EMPTY", g), "Empty POINT cannot be represented in WKB");
        assert_eq!(err("GEOMETRYCOLLECTION (POINT EMPTY)", g), "Empty POINT cannot be represented in WKB");
        assert_eq!(err("CIRCLE EMPTY", g), "Empty CIRCLE cannot be represented in WKB");
        assert_eq!(err("BBOX EMPTY", g), "Empty ENVELOPE cannot be represented in WKB");
        assert_eq!(err("foo", g), "Unknown geometry type: foo");
        assert_eq!(err("POINT foo", g), "expected EMPTY or ( but found: foo");
        assert_eq!(err("POINT z (1 2 3)", g), "expected EMPTY or ( but found: z");
        assert_eq!(err("POINT Z (1 2)", g), Z_OR_M);
        assert_eq!(err("MULTIPOINT (EMPTY)", g), "invalid number found: EMPTY");
        assert_eq!(err("MULTIPOINT ((1 2), (3 4))", g), "expected number but found: '('");
        assert_eq!(err("LINESTRING (1 2, 3 4 5)", g), "coordinate dimensions do not match: 5");
        assert_eq!(err("LINESTRING (1 2 3, 3 4)", g), "coordinate dimensions do not match: ')'");
        assert_eq!(err("POINT (1 2 foo)", g), "invalid number found: foo");
        assert_eq!(err("POINT (1 2, 3)", g), "expected ) but found: ','");
        assert_eq!(err("GEOMETRYCOLLECTION (POINT (1 2), POINT (1 2 3))", g), "all elements of the collection should have the same number of dimension");
        assert_eq!(err("POINT (1 infinity)", c), "invalid number found: infinity");
        let ok = |s: &str, ty: DataType| convert_checked(s, ty).unwrap();
        assert_eq!(ok("MULTIPOLYGON (((0 0, 1 0, 1 1, 0 0)), EMPTY)", g), "MULTIPOLYGON (((0.0 0.0, 1.0 0.0, 1.0 1.0, 0.0 0.0)),EMPTY)");
        assert_eq!(ok("MULTILINESTRING ((1 2, 3 4), EMPTY)", g), "MULTILINESTRING ((1.0 2.0, 3.0 4.0),EMPTY)");
        assert_eq!(ok("GEOMETRYCOLLECTION (POINT (1 2), LINESTRING EMPTY)", g), "GEOMETRYCOLLECTION (POINT (1.0 2.0),LINESTRING EMPTY)");
        assert_eq!(ok("POINT (NaN 2)", c), "POINT (NaN 2.0)");
        assert_eq!(ok("POINT (1 nan)", c), "POINT (1.0 NaN)");
        assert_eq!(ok("POINT (Infinity 2)", DataType::CartesianPoint), "POINT (Infinity 2.0)");
        assert_eq!(ok("POINT (-Infinity 2)", c), "POINT (-Infinity 2.0)");
        assert_eq!(ok("POINT *(1 2)", g), "POINT (1.0 2.0)");
        assert_eq!(ok("POINT Z (1 2 3)", g), "POINT (1.0 2.0 3.0)");
        assert_eq!(ok("POINT (1 2) # comment", g), "POINT (1.0 2.0)");
        assert_eq!(ok("POINT (0x1p3 1d)", g), "POINT (8.0 1.0)");
        assert_eq!(ok("POINT (+1 .5)", g), "POINT (1.0 0.5)");
        assert_eq!(ok("POINT(\t1 2)", g), "POINT (1.0 2.0)");
        assert_eq!(ok("BBOX (3, 2, 4, 1)", g), "BBOX (3.0, 2.0, 4.0, 1.0)");
        assert_eq!(ok("LINESTRING (1 2)", g), "LINESTRING (1.0 2.0)");
        assert_eq!(ok("POLYGON EMPTY", g), "POLYGON EMPTY");
        let st = |s: &str, y: bool| st_coordinate(s, DataType::CartesianPoint, y);
        assert_eq!(st("POINT (1.0E308 2.0)", false).unwrap_err(), "java.lang.IllegalArgumentException: invalid value Infinity; must be between -3.4028234663852886E38 and 3.4028234663852886E38");
        assert_eq!(st("POINT (1.0 NaN)", true).unwrap_err(), "java.lang.IllegalArgumentException: invalid value NaN; must be between -3.4028234663852886E38 and 3.4028234663852886E38");
    }

    #[test]
    fn geometry_functions() {
        assert_eq!(geometry_type(&["POINT (0.0 0.0)"]).unwrap(), "ST_Point");
        assert_eq!(geometry_type(&["BBOX (0.0, 1.0, 1.0, 0.0)"]).unwrap(), "ST_Polygon");
        assert_eq!(geometry_type(&["POINT (0.0 0.0)", "LINESTRING (0.0 0.0, 1.0 1.0)"]).unwrap(), "ST_GeometryCollection");
        assert_eq!(st_dimension(&["POINT (0.0 0.0)", "LINESTRING (0.0 0.0, 1.0 1.0)"]), Some(1));
        assert_eq!(st_dimension(&["GEOMETRYCOLLECTION (POINT (1.0 2.0),POLYGON ((0.0 0.0, 1.0 0.0, 1.0 1.0, 0.0 0.0)))"]), Some(2));
        assert_eq!(st_is_empty(&["POINT (0.0 0.0)"]), Some(false));
        assert_eq!(st_is_empty(&["LINESTRING EMPTY"]), Some(true));
    }

    /// Encodings Elasticsearch 9.5.5 returns for `TO_LONG(TO_GEOHASH(...))` and friends.
    #[test]
    fn grid_cells_encode_like_elasticsearch() {
        assert_eq!(grid_parse("9q8yy9", DataType::Geohash), Ok(5205638294));
        assert_eq!(grid_parse("4/2/5", DataType::Geotile), Ok(1152921505680588805));
        assert_eq!(grid_parse("811fbffffffffff", DataType::Geohex), Ok(581522903837704191));
        assert_eq!(grid_address(5205638294, DataType::Geohash), "9q8yy9");
        assert_eq!(grid_address(1152921505680588805, DataType::Geotile), "4/2/5");
        // Characters outside the alphabet and a 13th level read as Java's arithmetic does.
        assert_eq!(grid_address(grid_parse("abc", DataType::Geohash).unwrap(), DataType::Geohash), "zbc");
        assert_eq!(grid_address(grid_parse("u4pruydqqvjxx", DataType::Geohash).unwrap(), DataType::Geohash), "04pruydqqvjxx");
        assert!(grid_parse("1/1", DataType::Geotile).is_err());
    }
}

