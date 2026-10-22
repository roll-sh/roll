//! Geo queries: `geo_bounding_box`, `geo_distance`, `geo_polygon`, `geo_shape`, `shape` and
//! `geo_grid`, plus the geo point, distance and geometry (GeoJSON and WKT) parsers they share.
//!
//! Points print as `[lon, lat]` and geometries as GeoJSON with Elasticsearch's type names, the
//! way the builders print them back. Validation that needs the field mapping (latitude ranges
//! under `STRICT`, the field type) happens when the query runs.

use super::term_level::Relation;
use super::{Common, Token, XParser, check_boost};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::obj;

/// A point: latitude and longitude (NaN when not given), plus an optional z.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeoPoint {
    pub lat: f64,
    pub lon: f64,
}

impl GeoPoint {
    pub const NAN: GeoPoint = GeoPoint { lat: f64::NAN, lon: f64::NAN };

    /// `[lon, lat]`.
    pub fn to_value(self) -> Value {
        Value::Arr(vec![Value::Num(Number::Float(self.lon)), Value::Num(Number::Float(self.lat))])
    }
}

fn parse_err(msg: impl Into<String>) -> EsError {
    EsError::parse(msg)
}

fn z_error(z: f64) -> EsError {
    parse_err(format!(
        "Exception parsing coordinates: found Z value [{}] but [ignore_z_value] parameter is [false]",
        crate::json::write::java_double(z)
    ))
}

/// `Geohash` decoding as Elasticsearch does it (through the morton code): the south-west corner
/// of the cell. Characters past the twelfth are ignored.
pub fn geohash_decode(hash: &str) -> EsResult<GeoPoint> {
    const B32: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";
    if hash.is_empty() {
        return Err(EsError::illegal_argument("empty geohash"));
    }
    let (mut lat, mut lon) = ((-90.0f64, 90.0f64), (-180.0f64, 180.0f64));
    let mut even = true;
    for (i, c) in hash.chars().enumerate() {
        if i >= 12 {
            break;
        }
        let Some(idx) = B32.iter().position(|b| *b as char == c) else {
            let msg = format!("unsupported symbol [{c}] in geohash [{hash}]");
            return Err(parse_err(msg.clone()).caused_by(EsError::illegal_argument(msg)));
        };
        for bit in (0..5).rev() {
            let r = if even { &mut lon } else { &mut lat };
            let mid = (r.0 + r.1) / 2.0;
            if idx & (1 << bit) != 0 {
                r.0 = mid;
            } else {
                r.1 = mid;
            }
            even = !even;
        }
    }
    // Elasticsearch decodes the morton code of the cell, which is its south-west corner.
    Ok(GeoPoint { lat: lat.0, lon: lon.0 })
}

/// `GeoPoint.resetFromString` (no z values): `lat,lon`, `POINT (lon lat)` or a geohash.
pub fn point_from_string(s: &str) -> EsResult<GeoPoint> {
    if s.contains(',') {
        let parts: Vec<&str> = s.split(',').collect();
        if parts.len() > 3 {
            return Err(parse_err(format!("failed to parse [{s}], expected 2 or 3 coordinates but found: [{}]", parts.len())));
        }
        let num = |t: &str, what: &str| {
            crate::units::parse_java_double(t.trim()).ok_or_else(|| parse_err(format!("{what} must be a number")))
        };
        let lat = num(parts[0], "latitude")?;
        let lon = num(parts[1], "longitude")?;
        if parts.len() == 3 {
            return Err(z_error(num(parts[2], "altitude")?));
        }
        return Ok(GeoPoint { lat, lon });
    }
    if s.trim().to_ascii_uppercase().starts_with("POINT") {
        return match parse_wkt(s)? {
            Geometry::Point(c) => {
                if c.len() > 2 {
                    return Err(z_error(c[2]));
                }
                Ok(GeoPoint { lat: c.get(1).copied().unwrap_or(f64::NAN), lon: c.first().copied().unwrap_or(f64::NAN) })
            }
            other => Err(parse_err(format!("[geo_point] supports only POINT among WKT primitives, but found {}", other.wkt_name()))),
        };
    }
    geohash_decode(s)
}

/// `GeoUtils.parseGeoPoint(parser)`: an object, an array `[lon, lat]` or a string.
pub fn parse_point(p: &mut XParser<'_>) -> EsResult<GeoPoint> {
    match p.token() {
        Token::StartObject => {
            let mut pt = GeoPoint::NAN;
            let mut geohash = None;
            let mut current = String::new();
            while let Some(t) = p.next_token() {
                match t {
                    Token::EndObject => break,
                    Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                    _ => match current.as_str() {
                        "lat" => pt.lat = p.double_value().map_err(|_| parse_err("latitude must be a number"))?,
                        "lon" => pt.lon = p.double_value().map_err(|_| parse_err("longitude must be a number"))?,
                        "geohash" => geohash = Some(p.text()?),
                        "z" => return Err(z_error(p.double_value()?)),
                        _ => return Err(parse_err("field must be either [lat], [lon] or [geohash]")),
                    },
                }
            }
            if let Some(h) = geohash {
                if !pt.lat.is_nan() || !pt.lon.is_nan() {
                    return Err(parse_err("field must be either lat/lon or geohash"));
                }
                return geohash_decode(&h);
            }
            match (pt.lat.is_nan(), pt.lon.is_nan()) {
                (true, true) => Err(parse_err("Required [lat, lon]")),
                (true, false) => Err(parse_err("Required [lat]")),
                (false, true) => Err(parse_err("Required [lon]")),
                _ => Ok(pt),
            }
        }
        Token::StartArray => {
            let mut coords = Vec::new();
            while let Some(t) = p.next_token() {
                if t == Token::EndArray {
                    break;
                }
                if t != Token::Number {
                    return Err(parse_err("numeric value expected"));
                }
                coords.push(p.double_value()?);
            }
            if coords.len() > 2 {
                return Err(z_error(coords[2]));
            }
            Ok(GeoPoint { lon: coords.first().copied().unwrap_or(f64::NAN), lat: coords.get(1).copied().unwrap_or(f64::NAN) })
        }
        Token::String => point_from_string(&p.text()?),
        _ => Err(parse_err("geo_point expected")),
    }
}

/// `DistanceUnit`: names in Elasticsearch's lookup order, and meters per unit.
const DISTANCE_UNITS: &[(&[&str], f64)] = &[
    (&["in", "inch"], 0.0254),
    (&["yd", "yards"], 0.9144),
    (&["ft", "feet"], 0.3048),
    (&["km", "kilometers"], 1000.0),
    (&["NM", "nmi", "nauticalmiles"], 1852.0),
    (&["mm", "millimeters"], 0.001),
    (&["cm", "centimeters"], 0.01),
    (&["mi", "miles"], 1609.344),
    (&["m", "meters"], 1.0),
];

/// `DistanceUnit.fromString`.
pub fn distance_unit(name: &str) -> EsResult<f64> {
    DISTANCE_UNITS
        .iter()
        .find(|(names, _)| names.contains(&name))
        .map(|(_, m)| *m)
        .ok_or_else(|| EsError::illegal_argument(format!("No distance unit match [{name}]")))
}

/// `DistanceUnit.parse(text, defaultUnit, DEFAULT)`: the distance in meters.
pub fn parse_distance(text: &str, default_unit_meters: f64) -> EsResult<f64> {
    for (names, meters) in DISTANCE_UNITS {
        for n in *names {
            if let Some(num) = text.strip_suffix(n) {
                return super::xparser::java_parse_double(num).map(|v| v * meters);
            }
        }
    }
    super::xparser::java_parse_double(text).map(|v| v * default_unit_meters)
}

/// `GeoValidationMethod.fromString`.
fn validation_method(s: &str) -> EsResult<&'static str> {
    for m in ["COERCE", "IGNORE_MALFORMED", "STRICT"] {
        if m.eq_ignore_ascii_case(s) {
            return Ok(m);
        }
    }
    Err(EsError::illegal_argument(format!("operator needs to be either [COERCE, IGNORE_MALFORMED, STRICT], but not [{s}]")))
}

/// `geo_distance`.
#[derive(Clone, Debug, PartialEq)]
pub struct GeoDistance {
    pub field: String,
    pub center: GeoPoint,
    /// Meters.
    pub distance: f64,
    pub distance_type: &'static str,
    pub validation_method: &'static str,
    pub ignore_unmapped: bool,
    pub common: Common,
}

impl GeoDistance {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<GeoDistance> {
        let (mut boost, mut name) = (1.0f32, None);
        let mut point = GeoPoint::NAN;
        let mut field: Option<String> = None;
        let mut distance: Option<Value> = None;
        let mut unit = 1.0;
        let mut distance_type = "arc";
        let mut vm = None;
        let mut ignore_unmapped = false;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartArray => {
                    field = Some(current.clone());
                    point = parse_point(p)?;
                }
                Token::StartObject => {
                    super::check_single_field(p, "geo_distance", &field, Some(&current))?;
                    field = Some(current.clone());
                    let mut name_in = current.clone();
                    while let Some(t) = p.next_token() {
                        match t {
                            Token::EndObject => break,
                            Token::FieldName => name_in = p.current_name().unwrap_or("").to_string(),
                            t if t.is_value() => match name_in.as_str() {
                                "lat" => point.lat = p.double_value()?,
                                "lon" => point.lon = p.double_value()?,
                                "geohash" => point = geohash_decode(&p.text()?)?,
                                _ => return Err(p.parsing_error(format!("[geo_distance] query does not support [{current}]"))),
                            },
                            _ => {}
                        }
                    }
                }
                t if t.is_value() => match current.as_str() {
                    "distance" => distance = p.value().clone().into(),
                    "unit" => unit = distance_unit(&p.text()?)?,
                    "distance_type" => {
                        distance_type = match p.text()?.as_str() {
                            "arc" => "arc",
                            "plane" => "plane",
                            other => return Err(EsError::illegal_argument(format!("No geo distance for [{other}]"))),
                        }
                    }
                    "_name" => name = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    "ignore_unmapped" => ignore_unmapped = p.boolean_value()?,
                    "validation_method" => vm = Some(validation_method(&p.text()?)?),
                    c if c.ends_with(".lat") => {
                        point.lat = p.double_value()?;
                        field = Some(c[..c.len() - 4].to_string());
                    }
                    c if c.ends_with(".lon") => {
                        point.lon = p.double_value()?;
                        field = Some(c[..c.len() - 4].to_string());
                    }
                    _ => {
                        if field.is_none() {
                            point = point_from_string(&p.text()?)?;
                            field = Some(current.clone());
                        } else {
                            return Err(p.parsing_error(format!("failed to parse [geo_distance] query. unexpected field [{current}]")));
                        }
                    }
                },
                _ => {}
            }
        }
        let Some(distance) = distance else { return Err(p.parsing_error("geo_distance requires 'distance' to be specified")) };
        let field = field.filter(|f| !f.is_empty()).ok_or_else(|| EsError::illegal_argument("fieldName must not be null or empty"))?;
        let meters = match &distance {
            Value::Num(n) => n.as_f64() * unit,
            Value::Str(s) => {
                if s.is_empty() {
                    return Err(EsError::illegal_argument("distance must not be null or empty"));
                }
                parse_distance(s, unit)?
            }
            other => parse_distance(&other.to_es_string(), unit)?,
        };
        if meters <= 0.0 {
            return Err(EsError::illegal_argument("distance must be greater than zero"));
        }
        let mut q = GeoDistance {
            field,
            center: point,
            distance: meters,
            distance_type,
            validation_method: vm.unwrap_or("STRICT"),
            ignore_unmapped: false,
            common: Common::default(),
        };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        q.ignore_unmapped = ignore_unmapped;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert(self.field.clone(), self.center.to_value());
        m.insert("distance".into(), Value::Num(Number::Float(self.distance)));
        m.insert("distance_type".into(), self.distance_type.into());
        m.insert("validation_method".into(), self.validation_method.into());
        m.insert("ignore_unmapped".into(), self.ignore_unmapped.into());
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// `geo_bounding_box`.
#[derive(Clone, Debug, PartialEq)]
pub struct GeoBoundingBox {
    pub field: String,
    pub top: f64,
    pub left: f64,
    pub bottom: f64,
    pub right: f64,
    pub validation_method: &'static str,
    pub ignore_unmapped: bool,
    pub common: Common,
}

/// `GeoBoundsParser.parseBoundingBox`: corners, single sides or a WKT `BBOX`.
fn parse_bbox(p: &mut XParser<'_>) -> EsResult<(f64, f64, f64, f64)> {
    let (mut top, mut bottom, mut left, mut right) = (f64::NAN, f64::NAN, f64::NAN, f64::NAN);
    let mut envelope: Option<(f64, f64, f64, f64)> = None;
    let mut current = String::new();
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
            Token::String if current == "wkt" => {
                let text = p.text()?;
                match parse_wkt(&text) {
                    Ok(Geometry::Envelope { min_x, max_x, max_y, min_y }) => envelope = Some((max_y, min_x, min_y, max_x)),
                    Ok(other) => {
                        return Err(parse_err(format!("failed to parse WKT bounding box. [{}] found. expected [ENVELOPE]", other.wkt_name())));
                    }
                    Err(e) => return Err(parse_err(format!("failed to parse WKT bounding box: {}", e.reason()))),
                }
            }
            _ => match current.as_str() {
                "top" => top = p.double_value()?,
                "bottom" => bottom = p.double_value()?,
                "left" => left = p.double_value()?,
                "right" => right = p.double_value()?,
                "top_left" | "topLeft" => {
                    let pt = parse_point(p)?;
                    top = pt.lat;
                    left = pt.lon;
                }
                "bottom_right" | "bottomRight" => {
                    let pt = parse_point(p)?;
                    bottom = pt.lat;
                    right = pt.lon;
                }
                "top_right" | "topRight" => {
                    let pt = parse_point(p)?;
                    top = pt.lat;
                    right = pt.lon;
                }
                "bottom_left" | "bottomLeft" => {
                    let pt = parse_point(p)?;
                    bottom = pt.lat;
                    left = pt.lon;
                }
                _ => return Err(parse_err(format!("failed to parse bounding box. unexpected field [{current}]"))),
            },
        }
    }
    if let Some(e) = envelope {
        if !(top.is_nan() && bottom.is_nan() && left.is_nan() && right.is_nan()) {
            return Err(parse_err("failed to parse bounding box. Conflicting definition found using well-known text and explicit corners."));
        }
        return Ok(e);
    }
    Ok((top, left, bottom, right))
}

impl GeoBoundingBox {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<GeoBoundingBox> {
        let (mut boost, mut name, mut vm, mut ignore_unmapped) = (1.0f32, None, None, false);
        let mut field = None;
        let mut bbox = None;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => match parse_bbox(p) {
                    Ok(b) => {
                        bbox = Some(b);
                        field = Some(current.clone());
                    }
                    Err(e) => return Err(parse_err(format!("failed to parse [geo_bounding_box] query. [{}]", e.reason()))),
                },
                t if t.is_value() => match current.as_str() {
                    "_name" => name = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    "validation_method" => vm = Some(validation_method(&p.text()?)?),
                    "ignore_unmapped" => ignore_unmapped = p.boolean_value()?,
                    _ => return Err(p.parsing_error(format!("failed to parse [geo_bounding_box] query. unexpected field [{current}]"))),
                },
                _ => {}
            }
        }
        let Some((top, left, bottom, right)) = bbox else {
            return Err(parse_err("failed to parse [geo_bounding_box] query. bounding box not provided"));
        };
        let field = field.filter(|f| !f.is_empty()).ok_or_else(|| EsError::illegal_argument("Field name must not be empty."))?;
        let d = |x: f64| crate::json::write::java_double(x);
        for (v, what) in [(top, "top latitude"), (left, "left longitude"), (bottom, "bottom latitude"), (right, "right longitude")] {
            if !v.is_finite() {
                return Err(EsError::illegal_argument(format!("{what} is invalid: {}", d(v))));
            }
        }
        if top < bottom {
            return Err(EsError::illegal_argument(format!("top is below bottom corner: {} vs. {}", d(top), d(bottom))));
        }
        let mut q = GeoBoundingBox { field, top, left, bottom, right, validation_method: "STRICT", ignore_unmapped: false, common: Common::default() };
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.ignore_unmapped = ignore_unmapped;
        if let Some(v) = vm {
            q.validation_method = v;
        }
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert(
            self.field.clone(),
            obj! {
                "top_left" => GeoPoint { lat: self.top, lon: self.left }.to_value(),
                "bottom_right" => GeoPoint { lat: self.bottom, lon: self.right }.to_value(),
            },
        );
        m.insert("validation_method".into(), self.validation_method.into());
        m.insert("ignore_unmapped".into(), self.ignore_unmapped.into());
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// `geo_polygon` (deprecated in favour of `geo_shape`).
#[derive(Clone, Debug, PartialEq)]
pub struct GeoPolygon {
    pub field: String,
    pub points: Vec<GeoPoint>,
    pub validation_method: &'static str,
    pub ignore_unmapped: bool,
    pub common: Common,
}

impl GeoPolygon {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<GeoPolygon> {
        p.warnings.push("Deprecated field [geo_polygon] used, replaced by [[geo_shape] query where polygons are defined in geojson or wkt]".into());
        let (mut boost, mut name, mut vm, mut ignore_unmapped) = (1.0f32, None, None, false);
        let mut field = None;
        let mut points: Option<Vec<GeoPoint>> = None;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => {
                    field = Some(current.clone());
                    while let Some(t) = p.next_token() {
                        match t {
                            Token::EndObject => break,
                            Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                            Token::StartArray if current == "points" => {
                                let mut pts = Vec::new();
                                while p.next_token().is_some_and(|t| t != Token::EndArray) {
                                    pts.push(parse_point(p)?);
                                }
                                points = Some(pts);
                            }
                            _ => {
                                return Err(p.parsing_error(format!(
                                    "[geo_polygon] query does not support token type [{t}] under [{current}]"
                                )));
                            }
                        }
                    }
                }
                t if t.is_value() => match current.as_str() {
                    "_name" => name = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    "ignore_unmapped" => ignore_unmapped = p.boolean_value()?,
                    "validation_method" => vm = Some(validation_method(&p.text()?)?),
                    _ => return Err(p.parsing_error(format!("[geo_polygon] query does not support [{current}]"))),
                },
                t => return Err(p.parsing_error(format!("[geo_polygon] unexpected token type [{t}]"))),
            }
        }
        let field = field.filter(|f| !f.is_empty()).ok_or_else(|| EsError::illegal_argument("fieldName must not be null"))?;
        let mut points = points.unwrap_or_default();
        if points.is_empty() {
            return Err(EsError::illegal_argument("No points defined for geo_polygon query"));
        }
        if points.first() != points.last() {
            points.push(points[0]);
        }
        if points.len() < 4 {
            return Err(EsError::illegal_argument("too few points defined for geo_polygon query"));
        }
        let mut q = GeoPolygon { field, points, validation_method: "STRICT", ignore_unmapped: false, common: Common::default() };
        if let Some(v) = vm {
            q.validation_method = v;
        }
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.ignore_unmapped = ignore_unmapped;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert(self.field.clone(), obj! { "points" => Value::Arr(self.points.iter().map(|p| p.to_value()).collect()) });
        m.insert("validation_method".into(), self.validation_method.into());
        m.insert("ignore_unmapped".into(), self.ignore_unmapped.into());
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// A geometry from GeoJSON or WKT (`org.elasticsearch.geometry`). Coordinates are `[x, y(, z)]`.
#[derive(Clone, Debug, PartialEq)]
pub enum Geometry {
    Point(Vec<f64>),
    MultiPoint(Vec<Vec<f64>>),
    LineString(Vec<Vec<f64>>),
    MultiLineString(Vec<Vec<Vec<f64>>>),
    Polygon(Vec<Vec<Vec<f64>>>),
    MultiPolygon(Vec<Vec<Vec<Vec<f64>>>>),
    GeometryCollection(Vec<Geometry>),
    Envelope { min_x: f64, max_x: f64, max_y: f64, min_y: f64 },
    Circle { center: Vec<f64>, radius_meters: f64 },
}

impl Geometry {
    fn wkt_name(&self) -> &'static str {
        match self {
            Geometry::Point(_) => "POINT",
            Geometry::MultiPoint(_) => "MULTIPOINT",
            Geometry::LineString(_) => "LINESTRING",
            Geometry::MultiLineString(_) => "MULTILINESTRING",
            Geometry::Polygon(_) => "POLYGON",
            Geometry::MultiPolygon(_) => "MULTIPOLYGON",
            Geometry::GeometryCollection(_) => "GEOMETRYCOLLECTION",
            Geometry::Envelope { .. } => "ENVELOPE",
            Geometry::Circle { .. } => "CIRCLE",
        }
    }

    /// GeoJSON as Elasticsearch prints a geometry.
    pub fn to_value(&self) -> Value {
        fn c1(c: &[f64]) -> Value {
            Value::Arr(c.iter().map(|x| Value::Num(Number::Float(*x))).collect())
        }
        fn c2(c: &[Vec<f64>]) -> Value {
            Value::Arr(c.iter().map(|x| c1(x)).collect())
        }
        fn c3(c: &[Vec<Vec<f64>>]) -> Value {
            Value::Arr(c.iter().map(|x| c2(x)).collect())
        }
        let (ty, coords) = match self {
            Geometry::Point(c) => ("Point", c1(c)),
            Geometry::MultiPoint(c) => ("MultiPoint", c2(c)),
            Geometry::LineString(c) => ("LineString", c2(c)),
            Geometry::MultiLineString(c) => ("MultiLineString", c3(c)),
            Geometry::Polygon(c) => ("Polygon", c3(c)),
            Geometry::MultiPolygon(c) => ("MultiPolygon", Value::Arr(c.iter().map(|x| c3(x)).collect())),
            Geometry::GeometryCollection(g) => {
                return obj! { "type" => "GeometryCollection", "geometries" => Value::Arr(g.iter().map(Geometry::to_value).collect()) };
            }
            Geometry::Envelope { min_x, max_x, max_y, min_y } => ("Envelope", c2(&[vec![*min_x, *max_y], vec![*max_x, *min_y]])),
            Geometry::Circle { center, radius_meters } => {
                return obj! {
                    "type" => "Circle",
                    "radius" => format!("{}m", crate::json::write::java_double(*radius_meters)),
                    "coordinates" => c1(center),
                };
            }
        };
        obj! { "type" => ty, "coordinates" => coords }
    }
}

fn geo_coord(v: &Value) -> EsResult<f64> {
    match v {
        Value::Num(n) => Ok(n.as_f64()),
        _ => Err(parse_err("geo coordinates must be numbers")),
    }
}

/// The path (array indices) of the first coordinate that isn't a number, if any.
fn bad_coordinate(v: &Value, path: &mut Vec<usize>) -> bool {
    match v {
        Value::Arr(a) => {
            for (i, x) in a.iter().enumerate() {
                path.push(i);
                if bad_coordinate(x, path) {
                    return true;
                }
                path.pop();
            }
            false
        }
        Value::Num(_) => false,
        _ => true,
    }
}

/// Coordinates nested to any depth (`CoordinateNode`).
enum Coords {
    Point(Vec<f64>),
    List(Vec<Coords>),
}

fn coords_of(v: &Value) -> EsResult<Coords> {
    let Value::Arr(a) = v else { return Err(parse_err("geo coordinates must be numbers")) };
    // `[]` is an empty list of coordinates, not a point.
    if a.is_empty() {
        return Ok(Coords::List(Vec::new()));
    }
    if a.first().is_some_and(|x| matches!(x, Value::Arr(_))) {
        return a.iter().map(|c| crate::stack::maybe_grow(|| coords_of(c))).collect::<EsResult<Vec<_>>>().map(Coords::List);
    }
    let pt: Vec<f64> = a.iter().map(geo_coord).collect::<EsResult<_>>()?;
    if pt.len() > 3 {
        return Err(parse_err("geo coordinates greater than 3 dimensions are not supported"));
    }
    if pt.len() < 2 {
        return Err(parse_err("geo coordinates must be at least 2 dimensions"));
    }
    Ok(Coords::Point(pt))
}

fn as_point(c: &Coords) -> EsResult<Vec<f64>> {
    match c {
        Coords::Point(p) => Ok(p.clone()),
        Coords::List(_) => Err(parse_err("expected single coordinate")),
    }
}

fn as_points(c: &Coords) -> EsResult<Vec<Vec<f64>>> {
    match c {
        Coords::List(l) => l.iter().map(as_point).collect(),
        Coords::Point(_) => Err(parse_err("expected a list of coordinates")),
    }
}

fn as_lists(c: &Coords) -> EsResult<Vec<Vec<Vec<f64>>>> {
    match c {
        Coords::List(l) => l.iter().map(as_points).collect(),
        Coords::Point(_) => Err(parse_err("expected a list of points")),
    }
}

fn close_ring(mut ring: Vec<Vec<f64>>) -> EsResult<Vec<Vec<f64>>> {
    if ring.first() != ring.last() {
        let first = ring[0].clone();
        ring.push(first);
    }
    if ring.len() < 4 {
        return Err(EsError::illegal_argument("at least 4 polygon points required"));
    }
    Ok(ring)
}

fn line(points: Vec<Vec<f64>>) -> EsResult<Vec<Vec<f64>>> {
    if points.len() < 2 {
        return Err(EsError::new(500, "exception", "not enough points to build a line"));
    }
    Ok(points)
}

/// GeoJSON (`GeoJson.fromXContent`): `type`, `coordinates`, `geometries`, `radius`. `at` gives
/// the location of a value below the object, for coordinate errors.
pub fn parse_geojson(m: &Map) -> EsResult<Geometry> {
    parse_geojson_at(m, &|_| (1, 1))
}

fn parse_geojson_at(m: &Map, at: &dyn Fn(&[crate::json::locate::Seg<'_>]) -> (usize, usize)) -> EsResult<Geometry> {
    let failed = |e: EsError| EsError::x_content_parse_bare("Failed to build [geojson] after last required field arrived").caused_by(e);
    let Some(ty) = m.get("type") else { return Err(EsError::illegal_argument("Required [type]")) };
    let ty = ty.to_es_string().to_lowercase();
    let coords = match m.get("coordinates") {
        Some(c) => Some(coords_of(c).map_err(|e| {
            use crate::json::locate::Seg;
            let mut bad = Vec::new();
            let (l, col) = if bad_coordinate(c, &mut bad) {
                let mut rel = vec![Seg::Key("coordinates")];
                rel.extend(bad.iter().map(|i| Seg::Index(*i)));
                at(&rel)
            } else {
                at(&[Seg::Key("coordinates")])
            };
            EsError::x_content_parse(l, col, "[geojson] failed to parse field [coordinates]").caused_by(e)
        })?),
        None => None,
    };
    let need = |c: &Option<Coords>| -> EsResult<()> {
        if c.is_none() { Err(failed(parse_err("coordinates not included"))) } else { Ok(()) }
    };
    let g = match ty.as_str() {
        "point" => {
            need(&coords)?;
            Geometry::Point(as_point(coords.as_ref().unwrap_or(&Coords::List(Vec::new()))).map_err(failed)?)
        }
        "multipoint" => {
            need(&coords)?;
            Geometry::MultiPoint(as_points(coords.as_ref().unwrap_or(&Coords::List(Vec::new()))).map_err(failed)?)
        }
        "linestring" => {
            need(&coords)?;
            Geometry::LineString(line(as_points(coords.as_ref().unwrap_or(&Coords::List(Vec::new()))).map_err(failed)?).map_err(failed)?)
        }
        "multilinestring" => {
            need(&coords)?;
            let lines = as_lists(coords.as_ref().unwrap_or(&Coords::List(Vec::new()))).map_err(failed)?;
            Geometry::MultiLineString(lines.into_iter().map(line).collect::<EsResult<_>>().map_err(failed)?)
        }
        "polygon" => {
            need(&coords)?;
            let rings = as_lists(coords.as_ref().unwrap_or(&Coords::List(Vec::new()))).map_err(failed)?;
            if rings.is_empty() {
                // `coordinates.children.get(0)` on no rings.
                return Err(failed(EsError::java(500, "index_out_of_bounds_exception", "Index 0 out of bounds for length 0")));
            }
            Geometry::Polygon(rings.into_iter().map(close_ring).collect::<EsResult<_>>().map_err(failed)?)
        }
        "multipolygon" => {
            need(&coords)?;
            let polys = match coords.as_ref() {
                Some(Coords::List(l)) => l.iter().map(as_lists).collect::<EsResult<Vec<_>>>().map_err(failed)?,
                _ => return Err(failed(parse_err("expected a list of polygons"))),
            };
            Geometry::MultiPolygon(
                polys.into_iter().map(|r| r.into_iter().map(close_ring).collect::<EsResult<Vec<_>>>()).collect::<EsResult<_>>().map_err(failed)?,
            )
        }
        "envelope" => {
            need(&coords)?;
            let pts = as_points(coords.as_ref().unwrap_or(&Coords::List(Vec::new()))).map_err(failed)?;
            if pts.len() != 2 {
                return Err(failed(parse_err("envelope requires exactly two coordinates")));
            }
            let (min_x, max_y, max_x, min_y) = (pts[0][0], pts[0][1], pts[1][0], pts[1][1]);
            if max_y < min_y {
                return Err(failed(EsError::illegal_argument("max y cannot be less than min y")));
            }
            Geometry::Envelope { min_x, max_x, max_y, min_y }
        }
        "circle" => {
            need(&coords)?;
            let center = as_point(coords.as_ref().unwrap_or(&Coords::List(Vec::new()))).map_err(failed)?;
            let Some(r) = m.get("radius") else { return Err(failed(parse_err("radius is not specified"))) };
            // The radius is parsed as its field arrives, so a bad one fails at its own location.
            let radius_meters = parse_distance(&r.to_es_string(), 1.0).map_err(|e| {
                let (l, c) = at(&[crate::json::locate::Seg::Key("radius")]);
                EsError::x_content_parse(l, c, "[geojson] failed to parse field [radius]").caused_by(e)
            })?;
            Geometry::Circle { center, radius_meters }
        }
        "geometrycollection" => {
            let Some(Value::Arr(gs)) = m.get("geometries") else { return Err(failed(parse_err("geometries not included"))) };
            let mut out = Vec::new();
            for g in gs {
                let Value::Obj(gm) = g else { return Err(failed(parse_err("geometry must be an object"))) };
                out.push(crate::stack::maybe_grow(|| parse_geojson_at(gm, &|_| (1, 1)))?);
            }
            Geometry::GeometryCollection(out)
        }
        other => {
            return Err(failed(EsError::illegal_argument(format!(
                "No enum constant org.elasticsearch.geometry.ShapeType.{}",
                other.to_uppercase()
            ))));
        }
    };
    Ok(g)
}

/// A WKT tokenizer and parser (`WellKnownText.fromWKT`), including `BBOX`.
pub fn parse_wkt(text: &str) -> EsResult<Geometry> {
    struct W<'a> {
        s: &'a [u8],
        i: usize,
        /// Geometry collections entered (`WellKnownText.MAX_NESTED_DEPTH`).
        depth: usize,
    }
    impl W<'_> {
        fn ws(&mut self) {
            while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
        }
        fn word(&mut self) -> String {
            self.ws();
            let st = self.i;
            while self.i < self.s.len() && self.s[self.i].is_ascii_alphabetic() {
                self.i += 1;
            }
            String::from_utf8_lossy(&self.s[st..self.i]).to_lowercase()
        }
        fn peek(&mut self) -> Option<u8> {
            self.ws();
            self.s.get(self.i).copied()
        }
        fn err(&self, msg: String) -> EsError {
            EsError::new(400, "i_o_exception", format!("java.text.ParseException: {msg}")).caused_by(EsError::parse(msg))
        }
        /// `tokenString`: the next token as `StreamTokenizer` reads it — a word (letters,
        /// digits, `+`, `-`, `.`) as is, a single character quoted, or `END-OF-STREAM`.
        fn found(&mut self) -> String {
            let Some(c) = self.peek() else { return "END-OF-STREAM".into() };
            let word = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.') || b >= 0x80;
            if word(c) {
                let end = self.s[self.i..].iter().position(|b| !word(*b)).map_or(self.s.len(), |n| self.i + n);
                return String::from_utf8_lossy(&self.s[self.i..end]).into_owned();
            }
            format!("'{}'", c as char)
        }
        /// Consumes `c`, or fails with `expected <what> but found: <token>`.
        fn expect_as(&mut self, c: u8, what: &str) -> EsResult<()> {
            if self.peek() == Some(c) {
                self.i += 1;
                return Ok(());
            }
            let found = self.found();
            Err(self.err(format!("expected {what} but found: {found}")))
        }
        fn expect(&mut self, c: u8) -> EsResult<()> {
            self.expect_as(c, &(c as char).to_string())
        }
        fn number(&mut self) -> EsResult<f64> {
            self.ws();
            let st = self.i;
            while self.i < self.s.len() && (self.s[self.i].is_ascii_digit() || b"+-.eE".contains(&self.s[self.i])) {
                self.i += 1;
            }
            if st == self.i {
                let found = self.found();
                return Err(self.err(format!("expected number but found: {found}")));
            }
            std::str::from_utf8(&self.s[st..self.i]).ok().and_then(|t| t.parse().ok()).ok_or_else(|| self.err("invalid number".into()))
        }
        fn empty(&mut self) -> bool {
            let save = self.i;
            if self.word() == "empty" {
                return true;
            }
            self.i = save;
            false
        }
        fn point_coords(&mut self) -> EsResult<Vec<f64>> {
            let mut c = vec![self.number()?, self.number()?];
            if matches!(self.peek(), Some(b'0'..=b'9' | b'-' | b'+' | b'.')) {
                c.push(self.number()?);
            }
            Ok(c)
        }
        fn point_list(&mut self) -> EsResult<Vec<Vec<f64>>> {
            self.list_of(|w| w.point_coords())
        }
        fn list_of<T>(&mut self, mut f: impl FnMut(&mut Self) -> EsResult<T>) -> EsResult<Vec<T>> {
            self.expect(b'(')?;
            let mut out = vec![f(self)?];
            while self.peek() == Some(b',') {
                self.i += 1;
                out.push(f(self)?);
            }
            self.expect_as(b')', ", or )")?;
            Ok(out)
        }
        fn geometry(&mut self) -> EsResult<Geometry> {
            let ty = self.word();
            Ok(match ty.as_str() {
                "point" => {
                    if self.empty() {
                        return Ok(Geometry::Point(vec![0.0, 0.0]));
                    }
                    self.expect(b'(')?;
                    let c = self.point_coords()?;
                    self.expect(b')')?;
                    Geometry::Point(c)
                }
                "multipoint" => Geometry::MultiPoint(if self.empty() { Vec::new() } else { self.point_list()? }),
                "linestring" => Geometry::LineString(if self.empty() { Vec::new() } else { line(self.point_list()?)? }),
                "multilinestring" => Geometry::MultiLineString(if self.empty() { Vec::new() } else { self.list_of(|w| w.point_list())? }),
                "polygon" => Geometry::Polygon(if self.empty() {
                    Vec::new()
                } else {
                    self.list_of(|w| w.point_list())?.into_iter().map(close_ring).collect::<EsResult<_>>()?
                }),
                "multipolygon" => Geometry::MultiPolygon(if self.empty() {
                    Vec::new()
                } else {
                    self.list_of(|w| w.list_of(|w| w.point_list()))?
                        .into_iter()
                        .map(|p| p.into_iter().map(close_ring).collect::<EsResult<Vec<_>>>())
                        .collect::<EsResult<_>>()?
                }),
                "geometrycollection" => Geometry::GeometryCollection(if self.empty() {
                    Vec::new()
                } else {
                    // Checked before descending: the text can nest far deeper than any JSON.
                    if self.depth >= 1000 {
                        return Err(self.err("maximum nested depth of 1000 exceeded".into()));
                    }
                    self.depth += 1;
                    let list = crate::stack::maybe_grow(|| self.list_of(|w| w.geometry()))?;
                    self.depth -= 1;
                    list
                }),
                "bbox" | "envelope" => {
                    self.expect(b'(')?;
                    let min_x = self.number()?;
                    self.expect(b',')?;
                    let max_x = self.number()?;
                    self.expect(b',')?;
                    let max_y = self.number()?;
                    self.expect(b',')?;
                    let min_y = self.number()?;
                    self.expect(b')')?;
                    if max_y < min_y {
                        return Err(EsError::illegal_argument("max y cannot be less than min y"));
                    }
                    Geometry::Envelope { min_x, max_x, max_y, min_y }
                }
                other => return Err(EsError::illegal_argument(format!("Unknown geometry type: {other}"))),
            })
        }
    }
    let mut w = W { s: text.as_bytes(), i: 0, depth: 0 };
    let g = w.geometry()?;
    if let Some(c) = w.peek() {
        return Err(w.err(format!("expected end of WKT string but found additional text: {}", c as char)));
    }
    Ok(g)
}

/// `GeometryParser.parse`: GeoJSON objects or WKT strings.
pub fn parse_geometry(p: &mut XParser<'_>) -> EsResult<Geometry> {
    match p.token() {
        Token::StartObject => {
            let m = match p.value() {
                Value::Obj(m) => m.clone(),
                _ => Map::new(),
            };
            let r = parse_geojson_at(&m, &|rel| p.location_of(rel));
            p.skip_children();
            r
        }
        Token::String => parse_wkt(&p.text()?),
        Token::Null => Err(EsError::illegal_argument("shape must not be null")),
        t => Err(parse_err(format!("shape must be an object consisting of type and coordinates, found {t}"))),
    }
}

/// A shape query's target: an inline geometry or a stored one.
#[derive(Clone, Debug, PartialEq)]
pub enum ShapeSource {
    Inline(Geometry),
    Indexed { id: String, index: Option<String>, path: Option<String>, routing: Option<String> },
}

/// `geo_shape` (`GEO`) and `shape` (`AbstractGeometryQueryBuilder`).
#[derive(Clone, Debug, PartialEq)]
pub struct ShapeQuery<const GEO: bool> {
    pub field: String,
    pub shape: ShapeSource,
    pub relation: Relation,
    pub strategy: Option<String>,
    pub ignore_unmapped: bool,
    pub common: Common,
}

/// `geo_shape`.
pub type GeoShape = ShapeQuery<true>;
/// `shape` (cartesian geometries).
pub type Shape = ShapeQuery<false>;

impl<const GEO: bool> ShapeQuery<GEO>
where
    ShapeQuery<GEO>: Into<super::Query>,
{
    pub fn parse(p: &mut XParser<'_>) -> EsResult<ShapeQuery<GEO>> {
        let mut field: Option<String> = None;
        let mut shape: Option<Geometry> = None;
        let (mut id, mut index, mut path, mut routing) = (None, None, None, None);
        let mut relation = None;
        let mut strategy = None;
        // Elasticsearch's parsed parameters start with a boost of 0 when none is given.
        let (mut boost, mut name, mut ignore_unmapped) = (0.0f32, None, false);
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => {
                    if field.is_some() {
                        return Err(p.parsing_error(format!("point specified twice. [{current}]")));
                    }
                    field = Some(current.clone());
                    while let Some(t) = p.next_token() {
                        if t == Token::EndObject {
                            break;
                        }
                        if t != Token::FieldName {
                            continue;
                        }
                        current = p.current_name().unwrap_or("").to_string();
                        p.next_token();
                        match current.as_str() {
                            "relation" => {
                                let text = p.text()?;
                                relation = Some(Relation::by_name(&text).ok_or_else(|| p.parsing_error(format!("Unknown shape operation [{text} ]")))?);
                            }
                            "shape" => shape = Some(parse_geometry(p)?),
                            "strategy" => {
                                let s = p.text()?;
                                if s != "recursive" && s != "term" {
                                    return Err(EsError::illegal_argument(format!("Unknown strategy [{s}]")));
                                }
                                strategy = Some(s);
                            }
                            "indexed_shape" => {
                                while let Some(t) = p.next_token() {
                                    match t {
                                        Token::EndObject => break,
                                        Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                                        t if t.is_value() => match current.as_str() {
                                            "id" => id = Some(p.text()?),
                                            "index" => index = Some(p.text()?),
                                            "path" => path = Some(p.text()?),
                                            "routing" => routing = Some(p.text()?),
                                            _ => {}
                                        },
                                        t => return Err(p.parsing_error(format!("unknown token [{t}] after [{current}]"))),
                                    }
                                }
                            }
                            _ => return Err(p.parsing_error(format!("query does not support [{current}]"))),
                        }
                    }
                }
                t if t.is_value() => match current.as_str() {
                    "boost" => boost = p.float_value()?,
                    "_name" => name = Some(p.text()?),
                    "ignore_unmapped" => ignore_unmapped = p.boolean_value()?,
                    _ => return Err(p.parsing_error(format!("query does not support [{current}]"))),
                },
                _ => {}
            }
        }
        let source = match (shape, id) {
            (Some(g), None) => ShapeSource::Inline(g),
            (None, Some(id)) => ShapeSource::Indexed { id, index, path, routing },
            (Some(_), Some(_)) => return Err(EsError::illegal_argument("either shape or indexedShapeId is required")),
            (None, None) => return Err(EsError::illegal_argument("either shape or indexedShapeId is required")),
        };
        let field = field.unwrap_or_default();
        let mut q = ShapeQuery::<GEO> { field, shape: source, relation: relation.unwrap_or(Relation::Intersects), strategy, ignore_unmapped, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut f = Map::new();
        match &self.shape {
            ShapeSource::Inline(g) => {
                f.insert("shape".into(), g.to_value());
            }
            ShapeSource::Indexed { id, index, path, routing } => {
                let mut s = Map::new();
                s.insert("id".into(), id.clone().into());
                super::opt_str(&mut s, "index", index);
                super::opt_str(&mut s, "path", path);
                super::opt_str(&mut s, "routing", routing);
                f.insert("indexed_shape".into(), Value::Obj(s));
            }
        }
        f.insert("relation".into(), self.relation.name().into());
        super::opt_str(&mut f, "strategy", &self.strategy);
        let mut m = Map::new();
        m.insert(self.field.clone(), Value::Obj(f));
        m.insert("ignore_unmapped".into(), self.ignore_unmapped.into());
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// `geo_grid`: documents in one geohash, geotile or geohex cell.
#[derive(Clone, Debug, PartialEq)]
pub struct GeoGrid {
    pub field: String,
    /// `geohash`, `geotile` or `geohex`.
    pub grid: &'static str,
    pub cell: String,
    pub ignore_unmapped: bool,
    pub common: Common,
}

impl GeoGrid {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<GeoGrid> {
        let (mut boost, mut name, mut ignore_unmapped) = (1.0f32, None, false);
        let mut field: Option<String> = None;
        let mut grid: Option<(&'static str, String)> = None;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => {
                    super::check_single_field(p, "geo_grid", &field, Some(&current))?;
                    field = Some(current.clone());
                    while let Some(t) = p.next_token() {
                        match t {
                            Token::EndObject => break,
                            Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                            t if t.is_value() => {
                                let g: &'static str = match current.as_str() {
                                    "geohash" => "geohash",
                                    "geotile" => "geotile",
                                    "geohex" => "geohex",
                                    _ => return Err(p.parsing_error(format!("[geo_grid] query does not support [{current}]"))),
                                };
                                if grid.is_some() {
                                    return Err(p.parsing_error("[geo_grid] only one grid can be specified"));
                                }
                                grid = Some((g, p.text()?));
                            }
                            t => return Err(p.parsing_error(format!("[geo_grid] unknown token [{t}] after [{current}]"))),
                        }
                    }
                }
                t if t.is_value() => match current.as_str() {
                    "_name" => name = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    "ignore_unmapped" => ignore_unmapped = p.boolean_value()?,
                    _ => return Err(p.parsing_error(format!("[geo_grid] query does not support [{current}]"))),
                },
                t => return Err(p.parsing_error(format!("[geo_grid] unknown token [{t}] after [{current}]"))),
            }
        }
        let field = field.ok_or_else(|| EsError::illegal_argument("[geo_grid] requires a field"))?;
        let (grid, cell) = grid.ok_or_else(|| EsError::illegal_argument("[geo_grid] requires a grid"))?;
        let mut q = GeoGrid { field, grid, cell, ignore_unmapped, common: Common::default() };
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert(self.field.clone(), obj! { self.grid => self.cell.clone() });
        m.insert("ignore_unmapped".into(), self.ignore_unmapped.into());
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}
