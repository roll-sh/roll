//! OpenSearch's geo point object grammar (`GeoUtils.parseGeoPointObject`).
//!
//! OpenSearch's parser decides the object's form from its first key and then reads exactly
//! that form: `{lat, lon}` (numbers or numeric strings), `{geohash}`, or a GeoJSON point
//! `{type: "Point", coordinates: [lon, lat]}`, and anything after the form's keys is refused.
//! Elasticsearch's parser instead accumulates any order of `lat`/`lon`/`geohash`/`z`, with
//! different messages, so the opensearch dialect routes objects here
//! ([`crate::query::geo::parse_point`]).

use crate::error::{EsError, EsResult};
use crate::query::geo::{GeoPoint, geohash_decode};
use crate::query::xparser::{Token, XParser};

const INVALID_FIELDS: &str = "field must be either [lon|lat], [type|coordinates], or [geohash]";

fn err(msg: impl Into<String>) -> EsError {
    EsError::parse(msg)
}

/// Parses the object the parser is on (its `START_OBJECT`), leaving it on the `END_OBJECT`.
pub fn parse_point_object(p: &mut XParser<'_>) -> EsResult<GeoPoint> {
    match p.next_token() {
        Some(Token::FieldName) => {}
        other => return Err(err(format!("token [{}] not allowed", other.map(Token::name).unwrap_or("null")))),
    }
    let point = match p.current_name().unwrap_or("") {
        "lat" | "lon" => lat_lon(p)?,
        "geohash" => {
            if p.next_token() != Some(Token::String) {
                return Err(err("geohash must be a string"));
            }
            geohash_decode(&p.text()?)?
        }
        "type" | "coordinates" => geojson(p)?,
        _ => return Err(err(INVALID_FIELDS)),
    };
    if p.next_token() != Some(Token::EndObject) {
        // Like OpenSearch's sub-parser, the rest of the object is consumed before failing.
        skip_to_end(p);
        return Err(err(INVALID_FIELDS));
    }
    Ok(point)
}

/// Leaves the parser on the `END_OBJECT` of the object it is inside.
fn skip_to_end(p: &mut XParser<'_>) {
    loop {
        match p.token() {
            Token::EndObject => return,
            Token::StartObject | Token::StartArray => p.skip_children(),
            _ => {}
        }
        if p.next_token().is_none() {
            return;
        }
    }
}

/// `parseGeoPointObjectBasicFields`: up to two of `lat` and `lon`.
fn lat_lon(p: &mut XParser<'_>) -> EsResult<GeoPoint> {
    let (mut lat, mut lon) = (None, None);
    for i in 0..2 {
        if i != 0 {
            p.next_token();
        }
        if p.token() != Token::FieldName {
            break;
        }
        let field = p.current_name().unwrap_or("");
        let slot = match field {
            "lat" => &mut lat,
            "lon" => &mut lon,
            _ => return Err(err(INVALID_FIELDS)),
        };
        match p.next_token() {
            Some(Token::Number | Token::String) => {
                let v = p.double_value().map_err(|_| {
                    let text = p.text().unwrap_or_default();
                    err("[lon] and [lat] must be valid double values")
                        .caused_by(EsError::java(400, "number_format_exception", format!("For input string: \"{text}\"")))
                })?;
                *slot = Some(v);
            }
            _ => return Err(err(format!("{field} must be a number"))),
        }
    }
    let Some(lon) = lon else { return Err(err("field [lon] missing")) };
    let Some(lat) = lat else { return Err(err("field [lat] missing")) };
    Ok(GeoPoint { lat, lon })
}

/// `parseGeoJsonFields`: `type` (`Point`, any case) and `coordinates`, in either order.
fn geojson(p: &mut XParser<'_>) -> EsResult<GeoPoint> {
    let (mut has_type, mut point) = (false, None);
    for i in 0..2 {
        if i != 0 {
            p.next_token();
        }
        if p.token() != Token::FieldName {
            if !has_type {
                return Err(err("field [type] missing"));
            }
            if point.is_none() {
                return Err(err("field [coordinates] missing"));
            }
        }
        match p.current_name().unwrap_or("") {
            "type" if p.token() == Token::FieldName => {
                if p.next_token() != Some(Token::String) {
                    return Err(err("type must be a string"));
                }
                if !p.text()?.eq_ignore_ascii_case("point") {
                    return Err(err("type must be Point"));
                }
                has_type = true;
            }
            "coordinates" if p.token() == Token::FieldName => {
                if p.next_token() != Some(Token::StartArray) {
                    return Err(err("coordinates must be an array"));
                }
                point = Some(coordinates(p)?);
            }
            _ => return Err(err(INVALID_FIELDS)),
        }
    }
    point.ok_or_else(|| err("field [coordinates] missing"))
}

/// `parseGeoPointArray`: `[lon, lat]` or `[lon, lat, z]` (the z ignored).
pub fn coordinates(p: &mut XParser<'_>) -> EsResult<GeoPoint> {
    let (mut x, mut y) = (f64::NAN, f64::NAN);
    let mut n = 0;
    while let Some(t) = p.next_token() {
        if t == Token::EndArray {
            break;
        }
        if t != Token::Number {
            return Err(err("numeric value expected"));
        }
        n += 1;
        match n {
            1 => x = p.double_value()?,
            2 => y = p.double_value()?,
            // `ignore_z_value` defaults to true on OpenSearch's `geo_point`: the z is dropped.
            3 => {
                p.double_value()?;
            }
            _ => return Err(err("[geo_point] field type does not accept more than 3 values")),
        }
    }
    if n < 2 {
        return Err(err("[geo_point] field type should have at least two dimensions"));
    }
    Ok(GeoPoint { lat: y, lon: x })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::Value;

    fn parse(s: &str) -> EsResult<GeoPoint> {
        let v: Value = crate::json::parse(s.as_bytes()).unwrap().unwrap();
        let mut p = XParser::started(&v, None, Vec::new());
        parse_point_object(&mut p)
    }

    fn msg(s: &str) -> String {
        parse(s).unwrap_err().reason().to_string()
    }

    #[test]
    fn forms() {
        let pt = parse(r#"{"lon": 52.5, "lat": 4.5}"#).unwrap();
        assert_eq!((pt.lat, pt.lon), (4.5, 52.5));
        let pt = parse(r#"{"lat": "4.5", "lon": "52.5"}"#).unwrap();
        assert_eq!((pt.lat, pt.lon), (4.5, 52.5));
        let pt = parse(r#"{"type": "Point", "coordinates": [52.5, 4.5]}"#).unwrap();
        assert_eq!((pt.lat, pt.lon), (4.5, 52.5));
        let pt = parse(r#"{"coordinates": [52.5, 4.5], "type": "point"}"#).unwrap();
        assert_eq!((pt.lat, pt.lon), (4.5, 52.5));
        assert!(parse(r#"{"geohash": "t0v5zsq1gpzf"}"#).is_ok());
    }

    #[test]
    fn errors_match_opensearch() {
        assert_eq!(msg(r#"{"lat": 1}"#), "field [lon] missing");
        assert_eq!(msg(r#"{"lon": 1}"#), "field [lat] missing");
        assert_eq!(msg(r#"{"lat": 1, "lon": 2, "z": 3}"#), INVALID_FIELDS);
        assert_eq!(msg(r#"{"foo": 1}"#), INVALID_FIELDS);
        assert_eq!(msg(r#"{"lat": true, "lon": 2}"#), "lat must be a number");
        assert_eq!(msg(r#"{"lat": "x", "lon": 2}"#), "[lon] and [lat] must be valid double values");
        assert_eq!(msg(r#"{"type": "Point"}"#), "field [coordinates] missing");
        assert_eq!(msg(r#"{"coordinates": [1, 2]}"#), "field [type] missing");
        assert_eq!(msg(r#"{"type": "LineString", "coordinates": [1, 2]}"#), "type must be Point");
        assert_eq!(msg(r#"{"type": 1, "coordinates": [1, 2]}"#), "type must be a string");
        assert_eq!(msg(r#"{"type": "Point", "coordinates": 1}"#), "coordinates must be an array");
        assert_eq!(msg(r#"{"type": "Point", "coordinates": [1]}"#), "[geo_point] field type should have at least two dimensions");
        assert_eq!(msg(r#"{"geohash": 1}"#), "geohash must be a string");
        assert_eq!(msg(r#"{"type": "Point", "coordinates": [1, 2, 3, 4]}"#), "[geo_point] field type does not accept more than 3 values");
        assert!(parse(r#"{"type": "Point", "coordinates": [1, 2, 3]}"#).is_ok());
        assert_eq!(msg(r#"{}"#), "token [END_OBJECT] not allowed");
    }
}
