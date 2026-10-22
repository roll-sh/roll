//! Score-script functions (`ScoreScriptUtils`, `VectorScoreScriptUtils`), `DenseVector`,
//! `_termStats` and `explanation`, plus the geo and time helpers they need.
//!
//! Distances use Lucene's `SloppyMath.haversinMeters` (table-driven `cos`/`asin`), which is
//! what Elasticsearch's `GeoUtils.arcDistance` returns, so decay scores match to the bit.
//! Decay functions, `randomScore` and the vector functions are class bindings: their leading
//! arguments are parsed once per call site, on the first call.

use std::rc::Rc;
use std::sync::OnceLock;

use super::{binding, es, es_error, es_iae, es_value, EsObj};
use crate::lib_lang::double;
use crate::lookup::Builder;
use crate::types::cls;
use crate::value::Value;
use crate::vm::{Throw, Thrown, Vm, VR};

/// Score functions that are class bindings.
pub const BINDINGS: &[&str] = &[
    "randomScore",
    "decayGeoLinear",
    "decayGeoExp",
    "decayGeoGauss",
    "decayNumericLinear",
    "decayNumericExp",
    "decayNumericGauss",
    "decayDateLinear",
    "decayDateExp",
    "decayDateGauss",
    "l1norm",
    "l2norm",
    "cosineSimilarity",
    "dotProduct",
    "hamming",
    "maxSimDotProduct",
    "maxSimInvHamming",
];

/// How many leading arguments of a binding function its constructor takes.
pub fn binding_ctor_args(name: &str, arity: usize) -> usize {
    if name.starts_with("decay") {
        arity - 1
    } else {
        arity
    }
}

// ------------------------------------------------------------------------------ sloppy math

struct Tables {
    sin: Vec<f64>,
    cos: Vec<f64>,
    asin: Vec<f64>,
    d1: Vec<f64>,
    d2: Vec<f64>,
    d3: Vec<f64>,
    d4: Vec<f64>,
}

const TO_METERS: f64 = 6_371_008.771_4;
const SIN_COS_TABS_SIZE: usize = (1 << 11) + 1;
const ASIN_TABS_SIZE: usize = (1 << 13) + 1;

fn pio2_hi() -> f64 {
    f64::from_bits(0x3FF921FB54400000)
}
fn pio2_lo() -> f64 {
    f64::from_bits(0x3DD0B4611A626331)
}
fn sin_cos_delta_hi() -> f64 {
    4.0 * pio2_hi() / (SIN_COS_TABS_SIZE - 1) as f64
}
fn sin_cos_delta_lo() -> f64 {
    4.0 * pio2_lo() / (SIN_COS_TABS_SIZE - 1) as f64
}
fn sin_cos_indexer() -> f64 {
    1.0 / (sin_cos_delta_hi() + sin_cos_delta_lo())
}
fn sin_cos_max() -> f64 {
    ((i32::MAX >> 9) as f64 / sin_cos_indexer()) * 0.99
}
fn to_radians(d: f64) -> f64 {
    d * 0.017453292519943295
}
fn asin_max() -> f64 {
    libm::sin(to_radians(73.0))
}
fn asin_delta() -> f64 {
    asin_max() / (ASIN_TABS_SIZE - 1) as f64
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut sin = vec![0.0; SIN_COS_TABS_SIZE];
        let mut cos = vec![0.0; SIN_COS_TABS_SIZE];
        let pi_index = (SIN_COS_TABS_SIZE - 1) / 2;
        for i in 0..SIN_COS_TABS_SIZE {
            let angle = i as f64 * sin_cos_delta_hi() + i as f64 * sin_cos_delta_lo();
            let mut s = libm::sin(angle);
            let mut c = libm::cos(angle);
            if i == pi_index || i == 2 * pi_index {
                s = 0.0;
            } else if i == pi_index / 2 || i == 3 * pi_index / 2 {
                c = 0.0;
            }
            sin[i] = s;
            cos[i] = c;
        }
        let (mut asin, mut d1, mut d2, mut d3, mut d4) =
            (vec![0.0; ASIN_TABS_SIZE], vec![0.0; ASIN_TABS_SIZE], vec![0.0; ASIN_TABS_SIZE], vec![0.0; ASIN_TABS_SIZE], vec![0.0; ASIN_TABS_SIZE]);
        for i in 0..ASIN_TABS_SIZE {
            let x = i as f64 * asin_delta();
            asin[i] = libm::asin(x);
            let one_minus = 1.0 / (1.0 - x * x);
            let p05 = libm::sqrt(one_minus);
            let p15 = p05 * one_minus;
            let p25 = p15 * one_minus;
            let p35 = p25 * one_minus;
            d1[i] = p05;
            d2[i] = (x * p15) * (1.0 / 2.0);
            d3[i] = ((1.0 + 2.0 * x * x) * p25) * (1.0 / 6.0);
            d4[i] = ((5.0 + 2.0 * x * (2.0 + x * (5.0 - 2.0 * x))) * p35) * (1.0 / 24.0);
        }
        Tables { sin, cos, asin, d1, d2, d3, d4 }
    })
}

/// `SloppyMath.cos`.
fn sloppy_cos(a: f64) -> f64 {
    let a = a.abs();
    if a > sin_cos_max() {
        return a.cos();
    }
    let t = tables();
    let mut index = (a * sin_cos_indexer() + 0.5) as i32;
    let delta = (a - index as f64 * sin_cos_delta_hi()) - index as f64 * sin_cos_delta_lo();
    index &= (SIN_COS_TABS_SIZE - 2) as i32;
    let ic = t.cos[index as usize];
    let is = t.sin[index as usize];
    ic + delta * (-is + delta * (-ic * (1.0 / 2.0) + delta * (is * (1.0 / 6.0) + delta * ic * (1.0 / 24.0))))
}

/// Elasticsearch's `FastMath.log` (behind `ESSloppyMath.log`): a table of `log(1 + i/4096)`
/// and a third-order correction, exact close to 1.
pub fn fast_log(value: f64) -> f64 {
    struct Log {
        log: Vec<f64>,
        inv: Vec<f64>,
    }
    static TABS: OnceLock<Log> = OnceLock::new();
    const LOG_BITS: u32 = 12;
    const SIZE: usize = 1 << LOG_BITS;
    if value.is_nan() || value < 0.0 {
        return f64::NAN;
    }
    if value == 0.0 {
        return f64::NEG_INFINITY;
    }
    if value == f64::INFINITY {
        return f64::INFINITY;
    }
    let ln2 = libm::log(2.0);
    let mut value = value;
    let mut h = 0.0;
    if value > 0.95 {
        if value < 1.14 {
            let z = (value - 1.0) / (value + 1.0);
            let z2 = z * z;
            return z * (2.0 + z2 * ((2.0 / 3.0) + z2 * ((2.0 / 5.0) + z2 * ((2.0 / 7.0) + z2 * ((2.0 / 9.0) + z2 * (2.0 / 11.0))))));
        }
    } else if value < f64::MIN_POSITIVE {
        value *= f64::from_bits(0x4330000000000000);
        h = -52.0 * ln2;
    }
    let t = TABS.get_or_init(|| {
        let (mut log, mut inv) = (vec![0.0; SIZE], vec![0.0; SIZE]);
        for i in 0..SIZE {
            let x = 1.0 + i as f64 * (1.0 / SIZE as f64);
            log[i] = libm::log(x);
            inv[i] = 1.0 / x;
        }
        Log { log, inv }
    });
    let hi = (value.to_bits() >> 32) as i32;
    let exp = (hi >> 20) - 1023;
    let index = ((hi << 12) as u32 >> (32 - LOG_BITS)) as usize;
    // value / 2^exp, exactly
    let scaled = value * libm::pow(2.0, -exp as f64);
    let mut z = scaled * t.inv[index] - 1.0;
    z *= 1.0 - z * ((1.0 / 2.0) - z * (1.0 / 3.0));
    h + exp as f64 * ln2 + (t.log[index] + z)
}

/// `SphericalMercatorUtils.MERCATOR_BOUNDS / 180`.
const MERCATOR_FACTOR: f64 = 20037508.34 / 180.0;

/// `SphericalMercatorUtils.lonToSphericalMercator`.
pub fn lon_to_spherical_mercator(lon: f64) -> f64 {
    lon * MERCATOR_FACTOR
}

/// `SphericalMercatorUtils.latToSphericalMercator` (sloppy `cos` and `log`, as there).
pub fn lat_to_spherical_mercator(lat: f64) -> f64 {
    let cos = sloppy_cos((90.0 + lat) * std::f64::consts::PI / 360.0);
    let tan_lower = libm::tan(f64::from_bits(1));
    let tan = tan_lower.max((1.0 - cos * cos).sqrt() / cos);
    let y = fast_log(tan) / (std::f64::consts::PI / 180.0);
    y * MERCATOR_FACTOR
}

/// `SloppyMath.asin`.
fn sloppy_asin(a: f64) -> f64 {
    let (a, neg) = if a < 0.0 { (-a, true) } else { (a, false) };
    let r = if a <= asin_max() {
        let t = tables();
        let index = (a * (1.0 / asin_delta()) + 0.5) as usize;
        let delta = a - index as f64 * asin_delta();
        t.asin[index] + delta * (t.d1[index] + delta * (t.d2[index] + delta * (t.d3[index] + delta * t.d4[index])))
    } else if a < 1.0 {
        let ps = [0x3fc5555555555555u64, 0xbfd4d61203eb6f7d, 0x3fc9c1550e884455, 0xbfa48228b5688f3b, 0x3f49efe07501b288, 0x3f023de10dfdf709].map(f64::from_bits);
        let qs = [0xc0033a271c8a2d4bu64, 0x40002ae59c598ac8, 0xbfe6066c1b8d0159, 0x3fb3b8c5b12e9282].map(f64::from_bits);
        let t = (1.0 - a) * 0.5;
        let p = t * (ps[0] + t * (ps[1] + t * (ps[2] + t * (ps[3] + t * (ps[4] + t * ps[5])))));
        let q = 1.0 + t * (qs[0] + t * (qs[1] + t * (qs[2] + t * qs[3])));
        let s = t.sqrt();
        let z = s + s * (p / q);
        f64::from_bits(0x3FF921FB54442D18) - ((z + z) - f64::from_bits(0x3C91A62633145C07))
    } else if a == 1.0 {
        std::f64::consts::PI / 2.0
    } else {
        f64::NAN
    };
    if neg {
        -r
    } else {
        r
    }
}

/// `SloppyMath.haversinMeters(lat1, lon1, lat2, lon2)` (`GeoUtils.arcDistance`).
pub fn haversin_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let x1 = to_radians(lat1);
    let x2 = to_radians(lat2);
    let h1 = 1.0 - sloppy_cos(x1 - x2);
    let h2 = 1.0 - sloppy_cos(to_radians(lon1 - lon2));
    let h = h1 + sloppy_cos(x1) * sloppy_cos(x2) * h2;
    let key = f64::from_bits(h.to_bits() & 0xFFFFFFFFFFFFFFF8);
    TO_METERS * 2.0 * sloppy_asin(1f64.min((key * 0.5).sqrt()))
}

/// `GeoUtils.planeDistance`.
pub fn plane_distance(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let x = to_radians(lon2 - lon1) * to_radians((lat2 + lat1) / 2.0).cos();
    let y = to_radians(lat2 - lat1);
    (x * x + y * y).sqrt() * TO_METERS
}

/// Lower-left corner of a geohash cell, as Elasticsearch's `Geohash.decodeLatitude/Longitude`.
pub fn geohash_decode(hash: &str) -> Result<(f64, f64), Thrown> {
    const BASE32: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";
    let (mut lat_bits, mut lon_bits, mut nlat, mut nlon) = (0u64, 0u64, 0u32, 0u32);
    let mut even = true;
    for c in hash.bytes() {
        let Some(v) = BASE32.iter().position(|b| *b == c) else {
            return Err(es_iae(format!("unsupported symbol [{}] in geohash [{hash}]", c as char), &[]));
        };
        for bit in (0..5).rev() {
            let b = (v >> bit) & 1;
            if even {
                lon_bits = (lon_bits << 1) | b as u64;
                nlon += 1;
            } else {
                lat_bits = (lat_bits << 1) | b as u64;
                nlat += 1;
            }
            even = !even;
        }
    }
    let lat = if nlat == 0 { 0 } else { lat_bits << (32 - nlat.min(32)) };
    let lon = if nlon == 0 { 0 } else { lon_bits << (32 - nlon.min(32)) };
    let lat = ((lat as u32 ^ 0x8000_0000) as i32) as f64 * (180.0 / 4294967296.0);
    let lon = ((lon as u32 ^ 0x8000_0000) as i32) as f64 * (360.0 / 4294967296.0);
    Ok((lat, lon))
}

/// `GeoUtils.parseGeoPoint(String, false)`: `"lat,lon"`, a geohash, or WKT `POINT(lon lat)`.
pub fn parse_geo_point(s: &str) -> Result<(f64, f64), Thrown> {
    let t = s.trim();
    if t.to_ascii_uppercase().starts_with("POINT") {
        let inner = t[5..].trim().trim_start_matches('(').trim_end_matches(')');
        let parts: Vec<&str> = inner.split_whitespace().collect();
        if parts.len() >= 2 {
            let lon = parse_double(parts[0])?;
            let lat = parse_double(parts[1])?;
            return Ok((lat, lon));
        }
        return Err(es_error(cls::ILLEGAL_ARGUMENT_EXCEPTION, format!("Unable to parse point [{t}]"), &[]));
    }
    let parts: Vec<&str> = t.split(',').collect();
    match parts.len() {
        1 => geohash_decode(t),
        2 | 3 => {
            let lat = parse_double(parts[0].trim())?;
            let lon = parse_double(parts[1].trim())?;
            if parts.len() == 3 {
                return Err(es_error(
                    cls::ILLEGAL_ARGUMENT_EXCEPTION,
                    format!("Exception parsing coordinates: found Z value [{}] but [ignore_z_value] parameter is [false]", parts[2].trim()),
                    &[],
                ));
            }
            Ok((lat, lon))
        }
        _ => Err(es_error(cls::ILLEGAL_ARGUMENT_EXCEPTION, format!("failed to parse [{t}], expected 2 or 3 coordinates but found: [{}]", parts.len()), &[])),
    }
}

fn parse_double(s: &str) -> Result<f64, Thrown> {
    crate::java::parse_double(s).ok_or_else(|| {
        crate::lib_lang::nfe(
            format!("For input string: \"{s}\""),
            &[
                "jdk.internal.math.FloatingDecimal.check(FloatingDecimal.java:1751)",
                "jdk.internal.math.FloatingDecimal.readJavaFormatString(FloatingDecimal.java:1374)",
                "jdk.internal.math.FloatingDecimal.parseDouble(FloatingDecimal.java:61)",
                "java.lang.Double.parseDouble(Double.java:993)",
            ],
        )
    })
}

/// `DistanceUnit.DEFAULT.parse(s, DEFAULT)`: meters.
pub fn parse_distance(s: &str) -> Result<f64, Thrown> {
    const UNITS: &[(&[&str], f64)] = &[
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
    for (names, meters) in UNITS {
        for n in *names {
            if let Some(num) = s.strip_suffix(n) {
                let v = parse_double(num).map_err(|mut t| {
                    t.frames.push(format!("{}org.elasticsearch.common.unit.DistanceUnit$Distance.parseDistance(DistanceUnit.java:245)", super::SERVER));
                    t
                })?;
                return Ok(v * meters);
            }
        }
    }
    parse_double(s).map_err(|mut t| {
        t.frames.push(format!("{}org.elasticsearch.common.unit.DistanceUnit$Distance.parseDistance(DistanceUnit.java:248)", super::SERVER));
        t
    })
}

/// `TimeValue.parseTimeValue(s, default, setting).getMillis()`.
pub fn parse_time_value_millis(s: &str, setting: &str) -> Result<i64, Thrown> {
    let base = |msg: String| -> Thrown {
        let mut t = Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some(msg));
        t.frames = vec!["org.elasticsearch.base@9.5.5/org.elasticsearch.core.TimeValue.parseTimeValue(TimeValue.java:404)".into()];
            t
    };
    let norm = s.to_lowercase();
    let norm = norm.trim();
    let parse = |suffix: &str| -> Result<i64, Thrown> {
        let num = norm[..norm.len() - suffix.len()].trim();
        match num.parse::<i64>() {
            Ok(v) if v < -1 => Err(base(format!("failed to parse setting [{setting}] with value [{s}] as a time value: negative durations are not supported"))),
            Ok(v) => Ok(v),
            Err(_) => {
                if crate::java::parse_double(num).is_some() {
                    Err(base(format!("failed to parse [{s}], fractional time values are not supported")))
                } else {
                    Err(base(format!("failed to parse [{s}]")))
                }
            }
        }
    };
    let ms = if norm.ends_with("nanos") {
        parse("nanos")? / 1_000_000
    } else if norm.ends_with("micros") {
        parse("micros")? / 1000
    } else if norm.ends_with("ms") {
        parse("ms")?
    } else if norm.ends_with('s') {
        parse("s")?.saturating_mul(1000)
    } else if s.trim().ends_with('m') {
        parse("m")?.saturating_mul(60_000)
    } else if norm.ends_with('h') {
        parse("h")?.saturating_mul(3_600_000)
    } else if norm.ends_with('d') {
        parse("d")?.saturating_mul(86_400_000)
    } else if norm.trim_start_matches('-').trim_start_matches('0') == "1" && norm.starts_with('-') {
        -1
    } else if !norm.is_empty() && norm.bytes().all(|b| b == b'0') {
        0
    } else {
        return Err(base(format!("failed to parse setting [{setting}] with value [{s}] as a time value: unit is missing or unrecognized")));
    };
    Ok(ms)
}

/// `strict_date_optional_time||epoch_millis` with date math (`now`, `||+1d`, `/d`), in UTC.
/// `failed to parse date field [s] with format [strict_date_optional_time||epoch_millis]`,
/// caused by the formatter's `DateTimeParseException`.
pub fn date_parse_error(s: &str) -> Thrown {
    let msg = format!("failed to parse date field [{s}] with format [strict_date_optional_time||epoch_millis]");
    let cause = Throw::new(cls::DATE_TIME_PARSE_EXCEPTION, Some("Failed to parse with all enclosed parsers".into()));
    es_iae(msg, &["common.time.JavaDateFormatter.parse(JavaDateFormatter.java:230)"]).with_cause(&cause)
}

pub fn parse_date_math(vm: &mut Vm, s: &str) -> Result<i64, Thrown> {
    let fail = || {
        let inner = date_parse_error(s);
        let reason = inner.message.clone().unwrap_or_default();
        let mut t = Throw::new(cls::RUNTIME_EXCEPTION, Some(format!("{reason}: [{reason}]")));
        t.es_type = Some("parse_exception");
        t.es_root = true;
        t.frames = super::server_frames(&[
            "common.time.JavaDateMathParser.parseDateTime(JavaDateMathParser.java:233)",
            "common.time.JavaDateMathParser.parse(JavaDateMathParser.java:64)",
        ]);
        t.with_cause(&inner)
    };
    let (base, math) = if let Some(rest) = s.strip_prefix("now") {
        (vm.host.now_millis(), rest)
    } else {
        match s.find("||") {
            Some(i) => (parse_es_date(&s[..i]).ok_or_else(fail)?, &s[i + 2..]),
            None => return parse_es_date(s).ok_or_else(fail),
        }
    };
    apply_date_math(base, math).ok_or_else(|| es_iae(format!("operator not supported for date math [{s}]"), &[]))
}

fn apply_date_math(mut t: i64, math: &str) -> Option<i64> {
    use crate::time::{local_of_epoch, Unit};
    let b = math.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let op = b[i];
        i += 1;
        let mut n: i64 = 1;
        if op != b'/' {
            let st = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i > st {
                n = math[st..i].parse().ok()?;
            }
        }
        if i >= b.len() {
            return None;
        }
        let unit = b[i];
        i += 1;
        let u = match unit {
            b'y' => Unit::Years,
            b'M' => Unit::Months,
            b'w' => Unit::Weeks,
            b'd' => Unit::Days,
            b'h' | b'H' => Unit::Hours,
            b'm' => Unit::Minutes,
            b's' => Unit::Seconds,
            _ => return None,
        };
        let zdt = crate::time::TimeVal::Zoned {
            d: local_of_epoch(t.div_euclid(1000), (t.rem_euclid(1000) * 1_000_000) as i32, 0).0,
            t: local_of_epoch(t.div_euclid(1000), (t.rem_euclid(1000) * 1_000_000) as i32, 0).1,
            offset: 0,
            zone: crate::time::Zone::Offset(0),
        };
        let r = match op {
            b'+' => zdt.plus_unit(n, u).ok()?,
            b'-' => zdt.plus_unit(-n, u).ok()?,
            b'/' => {
                if u == Unit::Weeks {
                    let dow = zdt.get_long(crate::time::Field::DayOfWeek).ok()?;
                    zdt.truncated(Unit::Days).ok()?.plus_unit(-(dow - 1), Unit::Days).ok()?
                } else if u == Unit::Months {
                    zdt.truncated(Unit::Days).ok()?.with_field(crate::time::Field::DayOfMonth, 1).ok()?
                } else if u == Unit::Years {
                    zdt.truncated(Unit::Days).ok()?.with_field(crate::time::Field::DayOfYear, 1).ok()?
                } else {
                    zdt.truncated(u).ok()?
                }
            }
            _ => return None,
        };
        let (s, ns) = r.instant()?;
        t = s * 1000 + (ns / 1_000_000) as i64;
    }
    Some(t)
}

/// `strict_date_optional_time` or `epoch_millis`, to epoch milliseconds (UTC).
pub fn parse_es_date(s: &str) -> Option<i64> {
    if let Some(ms) = parse_iso_millis(s) {
        return Some(ms);
    }
    // epoch_millis: optional sign, digits, optional fraction (truncated)
    let t = s.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t),
    };
    if body.is_empty() {
        return None;
    }
    let (int, frac) = match body.find('.') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => (body, ""),
    };
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: i64 = int.parse().ok()?;
    Some(if neg { -v } else { v })
}

/// `strict_date_optional_time`: `yyyy[-MM[-dd]][Thh[:mm[:ss[.f{1,9}]]]][Z|±hh[:mm]]`.
fn parse_iso_millis(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize, n: usize| -> Option<i64> {
        if *i + n > b.len() || !b[*i..*i + n].iter().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let v = s[*i..*i + n].parse().ok()?;
        *i += n;
        Some(v)
    };
    let year = digits(&mut i, 4)?;
    let (mut month, mut day) = (1, 1);
    let (mut h, mut mi, mut sec, mut nanos) = (0i64, 0i64, 0i64, 0i64);
    let mut offset = 0i64;
    if i < b.len() && b[i] == b'-' {
        i += 1;
        month = digits(&mut i, 2)?;
        if i < b.len() && b[i] == b'-' {
            i += 1;
            day = digits(&mut i, 2)?;
        }
    }
    if i < b.len() && b[i] == b'T' {
        i += 1;
        h = digits(&mut i, 2)?;
        if i < b.len() && b[i] == b':' {
            i += 1;
            mi = digits(&mut i, 2)?;
            if i < b.len() && b[i] == b':' {
                i += 1;
                sec = digits(&mut i, 2)?;
                if i < b.len() && (b[i] == b'.' || b[i] == b',') {
                    i += 1;
                    let st = i;
                    while i < b.len() && b[i].is_ascii_digit() && i - st < 9 {
                        i += 1;
                    }
                    if i == st {
                        return None;
                    }
                    let f = &s[st..i];
                    nanos = format!("{f:0<9}").parse().ok()?;
                }
            }
        }
        if i < b.len() {
            match b[i] {
                b'Z' => i += 1,
                b'+' | b'-' => {
                    let sign = if b[i] == b'-' { -1 } else { 1 };
                    i += 1;
                    let oh = digits(&mut i, 2)?;
                    let mut om = 0;
                    if i < b.len() && b[i] == b':' {
                        i += 1;
                        om = digits(&mut i, 2)?;
                    } else if i + 2 <= b.len() {
                        om = digits(&mut i, 2)?;
                    }
                    offset = sign * (oh * 3600 + om * 60);
                }
                _ => return None,
            }
        }
    }
    if i != b.len() || !(1..=12).contains(&month) || h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let date = crate::time::Date::of(year, month, day).ok()?;
    let secs = date.epoch_day() * 86400 + h * 3600 + mi * 60 + sec - offset;
    Some(secs * 1000 + nanos / 1_000_000)
}

// ------------------------------------------------------------------------------ dense vectors

pub use super::dense::Vector;
use super::dense::{s_cosine, s_dot, s_hamming, s_l1, s_l2, v_cos, v_dims, v_dot, v_empty, v_get_vector, v_is_empty, v_l1, v_l2, v_magnitude, v_size};

// ------------------------------------------------------------------------------ score functions

fn s_saturation(_: &mut Vm, a: &mut [Value]) -> VR {
    let (v, k) = (double(&a[0]), double(&a[1]));
    Ok(Value::Double(v / (k + v)))
}

fn s_sigmoid(_: &mut Vm, a: &mut [Value]) -> VR {
    let (v, k, e) = (double(&a[0]), double(&a[1]), double(&a[2]));
    Ok(Value::Double(v.powf(e) / (k.powf(e) + v.powf(e))))
}

/// `BitMixer.mix32`.
fn mix32(k: i32) -> i32 {
    let mut k = k as u32;
    k = (k ^ (k >> 16)).wrapping_mul(0x85ebca6b);
    k = (k ^ (k >> 13)).wrapping_mul(0xc2b2ae35);
    (k ^ (k >> 16)) as i32
}

/// `StringHelper.murmurhash3_x86_32`.
pub fn murmur3_x86_32(data: &[u8], seed: i32) -> i32 {
    let (c1, c2) = (0xcc9e2d51u32, 0x1b873593u32);
    let mut h1 = seed as u32;
    let rounded = data.len() & !3;
    let mut i = 0;
    while i < rounded {
        let mut k1 = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        k1 = k1.wrapping_mul(c1);
        k1 = k1.rotate_left(15);
        k1 = k1.wrapping_mul(c2);
        h1 ^= k1;
        h1 = h1.rotate_left(13);
        h1 = h1.wrapping_mul(5).wrapping_add(0xe6546b64);
        i += 4;
    }
    let mut k1 = 0u32;
    let tail = data.len() & 3;
    if tail == 3 {
        k1 = (data[rounded + 2] as u32) << 16;
    }
    if tail >= 2 {
        k1 |= (data[rounded + 1] as u32) << 8;
    }
    if tail >= 1 {
        k1 |= data[rounded] as u32;
        k1 = k1.wrapping_mul(c1);
        k1 = k1.rotate_left(15);
        k1 = k1.wrapping_mul(c2);
        h1 ^= k1;
    }
    h1 ^= data.len() as u32;
    h1 ^= h1 >> 16;
    h1 = h1.wrapping_mul(0x85ebca6b);
    h1 ^= h1 >> 13;
    h1 = h1.wrapping_mul(0xc2b2ae35);
    h1 ^= h1 >> 16;
    h1 as i32
}

struct RandomScore {
    salted: i32,
    field: Option<Rc<str>>,
}

fn random_binding(vm: &mut Vm, a: &[Value], field: bool) -> Result<Rc<RandomScore>, Thrown> {
    binding(vm, |vm| {
        let seed = crate::lib_lang::int(&a[0]);
        let field_name = if field { Some(a[1].to_java_string()) } else { None };
        if let Some(f) = &field_name {
            // the doc values are looked up first
            let mut dv = crate::host::DocValues::default();
            match vm.host.doc_values(f, &mut dv) {
                Ok(true) => {}
                Ok(false) => {
                    let mut t = super::doc::no_field(f);
                    t.frames.push(format!("{}org.elasticsearch.script.ScoreScriptUtils$RandomScoreField.<init>(ScoreScriptUtils.java:53)", super::SERVER));
                    return Err(t);
                }
                Err(e) => return Err(super::doc::host_error(e)),
            }
        }
        let (shard, index) = vm.host.shard();
        if index.is_empty() {
            let outer = if field { "script.ScoreScriptUtils$RandomScoreField.<init>(ScoreScriptUtils.java:54)" } else { "script.ScoreScriptUtils$RandomScoreDoc.<init>(ScoreScriptUtils.java:78)" };
            return Err(es_iae("index name can not be looked up!", &["script.ScoreScript._getIndex(ScoreScript.java:179)", outer]));
        }
        let salt = (crate::java::string_hash(&index) << 10) | shard;
        Ok(RandomScore { salted: mix32(salt ^ seed), field: field_name.map(|s| Rc::from(s.as_str())) })
    })
}

fn random_value(salted: i32, seed_value: &str) -> f64 {
    let hash = murmur3_x86_32(seed_value.as_bytes(), salted);
    ((hash & 0x00FF_FFFF) as f32 / (1 << 24) as f32) as f64
}

fn s_random_doc(vm: &mut Vm, a: &mut [Value]) -> VR {
    let b = random_binding(vm, a, false)?;
    let id = vm.host.doc_id();
    Ok(Value::Double(random_value(b.salted, &id.to_string())))
}

fn s_random_field(vm: &mut Vm, a: &mut [Value]) -> VR {
    let b = random_binding(vm, a, true)?;
    let field = b.field.clone().unwrap_or_else(|| Rc::from(""));
    let v = super::doc::doc_get(vm, &Value::str(&field))?;
    let first = match es(&v) {
        Some(EsObj::Values(s)) => s.get(0)?,
        _ => Value::Null,
    };
    Ok(Value::Double(random_value(b.salted, &first.to_java_string())))
}

#[derive(Clone, Copy)]
enum Decay {
    Linear,
    Exp,
    Gauss,
}

fn scaling(d: Decay, scale: f64, decay: f64) -> f64 {
    match d {
        Decay::Linear => scale / (1.0 - decay),
        Decay::Exp => decay.ln() / scale,
        Decay::Gauss => 0.5 * scale.powf(2.0) / decay.ln(),
    }
}

fn decay_value(d: Decay, scaling: f64, distance: f64) -> f64 {
    match d {
        Decay::Linear => 0f64.max((scaling - distance) / scaling),
        Decay::Exp => (scaling * distance).exp(),
        Decay::Gauss => (0.5 * distance.powf(2.0) / scaling).exp(),
    }
}

struct NumDecay {
    origin: f64,
    offset: f64,
    scaling: f64,
}

fn num_decay(vm: &mut Vm, a: &[Value], d: Decay) -> VR {
    let b = binding(vm, |_| Ok(NumDecay { origin: double(&a[0]), offset: double(&a[2]), scaling: scaling(d, double(&a[1]), double(&a[3])) }))?;
    let v = double(&a[4]);
    let distance = 0f64.max((v - b.origin).abs() - b.offset);
    Ok(Value::Double(decay_value(d, b.scaling, distance)))
}

fn s_num_linear(vm: &mut Vm, a: &mut [Value]) -> VR {
    num_decay(vm, a, Decay::Linear)
}
fn s_num_exp(vm: &mut Vm, a: &mut [Value]) -> VR {
    num_decay(vm, a, Decay::Exp)
}
fn s_num_gauss(vm: &mut Vm, a: &mut [Value]) -> VR {
    num_decay(vm, a, Decay::Gauss)
}

fn outer(mut t: Thrown, frames: &[String]) -> Thrown {
    for f in frames {
        t.frames.push(format!("{}org.elasticsearch.{f}", super::SERVER));
    }
    t
}

struct GeoDecay {
    lat: f64,
    lon: f64,
    offset: f64,
    scaling: f64,
}

fn geo_decay(vm: &mut Vm, a: &[Value], d: Decay) -> VR {
    let (class, base) = match d {
        Decay::Linear => ("DecayGeoLinear", 98),
        Decay::Exp => ("DecayGeoExp", 120),
        Decay::Gauss => ("DecayGeoGauss", 142),
    };
    let b = binding(vm, |_| {
        let ctor = |line: u32| format!("script.ScoreScriptUtils${class}.<init>(ScoreScriptUtils.java:{line})");
        let unit = ["common.unit.DistanceUnit.parse(DistanceUnit.java:134)".to_string(), "common.unit.DistanceUnit.parse(DistanceUnit.java:146)".to_string()];
        let (lat, lon) = parse_geo_point(&a[0].to_java_string()).map_err(|t| outer(t, &[ctor(base)]))?;
        let scale = parse_distance(&a[1].to_java_string()).map_err(|t| outer(t, &[unit[0].clone(), unit[1].clone(), ctor(base + 1)]))?;
        let offset = parse_distance(&a[2].to_java_string()).map_err(|t| outer(t, &[unit[0].clone(), unit[1].clone(), ctor(base + 4)]))?;
        Ok(GeoDecay { lat, lon, offset, scaling: scaling(d, scale, double(&a[3])) })
    })?;
    let (plat, plon) = match es(&a[4]) {
        Some(EsObj::GeoPoint(x, y)) => (*x, *y),
        _ => return Err(crate::vm::npe("Cannot invoke \"org.elasticsearch.common.geo.GeoPoint.lat()\" because \"docValue\" is null")),
    };
    let distance = haversin_meters(b.lat, b.lon, plat, plon);
    let distance = 0f64.max(distance - b.offset);
    Ok(Value::Double(decay_value(d, b.scaling, distance)))
}

fn s_geo_linear(vm: &mut Vm, a: &mut [Value]) -> VR {
    geo_decay(vm, a, Decay::Linear)
}
fn s_geo_exp(vm: &mut Vm, a: &mut [Value]) -> VR {
    geo_decay(vm, a, Decay::Exp)
}
fn s_geo_gauss(vm: &mut Vm, a: &mut [Value]) -> VR {
    geo_decay(vm, a, Decay::Gauss)
}

struct DateDecay {
    origin: i64,
    offset: i64,
    scaling: f64,
}

fn date_decay(vm: &mut Vm, a: &[Value], d: Decay, class: &str) -> VR {
    let base = match d {
        Decay::Linear => 227,
        Decay::Exp => 250,
        Decay::Gauss => 272,
    };
    let b = binding(vm, |vm| {
        let ctor = |line: u32| format!("script.ScoreScriptUtils${class}.<init>(ScoreScriptUtils.java:{line})");
        let origin = parse_date_math(vm, &a[0].to_java_string()).map_err(|t| outer(t, &[ctor(base)]))?;
        let scale = parse_time_value_millis(&a[1].to_java_string(), &format!("{class}.scale")).map_err(|t| outer(t, &[ctor(base + 1)]))?;
        let offset = parse_time_value_millis(&a[2].to_java_string(), &format!("{class}.offset")).map_err(|t| outer(t, &[ctor(base + 3)]))?;
        Ok(DateDecay { origin, offset, scaling: scaling(d, scale as f64, double(&a[3])) })
    })?;
    let v = match crate::time::epoch_millis(&a[4]) {
        Some(ms) => ms,
        None => return Err(crate::vm::npe("Cannot invoke \"java.time.ZonedDateTime.toInstant()\" because \"docValueDate\" is null")),
    };
    let diff = if v >= b.origin { v - b.origin } else { b.origin - v };
    let distance = 0i64.max(diff - b.offset);
    Ok(Value::Double(decay_value(d, b.scaling, distance as f64)))
}

fn s_date_linear(vm: &mut Vm, a: &mut [Value]) -> VR {
    date_decay(vm, a, Decay::Linear, "DecayDateLinear")
}
fn s_date_exp(vm: &mut Vm, a: &mut [Value]) -> VR {
    date_decay(vm, a, Decay::Exp, "DecayDateExp")
}
fn s_date_gauss(vm: &mut Vm, a: &mut [Value]) -> VR {
    date_decay(vm, a, Decay::Gauss, "DecayDateGauss")
}

// ------------------------------------------------------------------------------ term stats

/// `StatsSummary`.
#[derive(Clone, Copy, Default)]
pub struct Summary {
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub count: i64,
}

impl Summary {
    fn of(values: &[f64]) -> Summary {
        if values.is_empty() {
            return Summary::default();
        }
        let mut s = Summary { min: f64::INFINITY, max: f64::NEG_INFINITY, sum: 0.0, count: 0 };
        for v in values {
            s.min = s.min.min(*v);
            s.max = s.max.max(*v);
            s.sum += v;
            s.count += 1;
        }
        s
    }
    fn average(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum / self.count as f64
        }
    }
    pub fn to_java_string(&self) -> String {
        format!(
            "StatsSummary{{count={}, sum={}, min={}, average={}, max={}}}",
            self.count,
            crate::java::fmt_double(self.sum),
            crate::java::fmt_double(self.min),
            crate::java::fmt_double(self.average()),
            crate::java::fmt_double(self.max)
        )
    }
}

fn term_stats(v: &Value) -> Result<Rc<crate::host::TermStats>, Thrown> {
    match es(v) {
        Some(EsObj::TermStats(t)) => Ok(t.clone()),
        _ => Err(crate::vm::cce("not term stats")),
    }
}

fn ts_unique(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(term_stats(&a[0])?.unique_terms_count))
}
fn ts_matched(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(term_stats(&a[0])?.matched_terms_count))
}
fn ts_doc_freq(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(es_value(EsObj::StatsSummary(Summary::of(&term_stats(&a[0])?.doc_freq))))
}
fn ts_total_term_freq(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(es_value(EsObj::StatsSummary(Summary::of(&term_stats(&a[0])?.total_term_freq))))
}
fn ts_term_freq(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(es_value(EsObj::StatsSummary(Summary::of(&term_stats(&a[0])?.term_freq))))
}
fn ts_term_positions(_: &mut Vm, a: &mut [Value]) -> VR {
    let t = term_stats(&a[0])?;
    let all: Vec<f64> = t.term_positions.iter().flatten().copied().collect();
    Ok(es_value(EsObj::StatsSummary(Summary::of(&all))))
}

fn summary(v: &Value) -> Result<Summary, Thrown> {
    match es(v) {
        Some(EsObj::StatsSummary(s)) => Ok(*s),
        _ => Err(crate::vm::cce("not a summary")),
    }
}

fn ss_min(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = summary(&a[0])?;
    Ok(Value::Double(if s.count == 0 { 0.0 } else { s.min }))
}
fn ss_max(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = summary(&a[0])?;
    Ok(Value::Double(if s.count == 0 { 0.0 } else { s.max }))
}
fn ss_avg(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(summary(&a[0])?.average()))
}
fn ss_sum(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(summary(&a[0])?.sum))
}
fn ss_count(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(summary(&a[0])?.count))
}

fn ex_set(vm: &mut Vm, a: &mut [Value]) -> VR {
    vm.st.explanation = if a[1].is_null() { None } else { Some(a[1].to_java_string()) };
    Ok(Value::Null)
}

pub fn register(b: &mut Builder) {
    use cls::*;
    b.s(SCORE_SCRIPT, "double saturation(double,double)", s_saturation);
    b.s(SCORE_SCRIPT, "double sigmoid(double,double,double)", s_sigmoid);
    let m = b.s(SCORE_SCRIPT, "double randomScore(int)", s_random_doc);
    b.nondeterministic(m);
    b.s(SCORE_SCRIPT, "double randomScore(int,String)", s_random_field);
    b.s(SCORE_SCRIPT, "double decayGeoLinear(String,String,String,double,GeoPoint)", s_geo_linear);
    b.s(SCORE_SCRIPT, "double decayGeoExp(String,String,String,double,GeoPoint)", s_geo_exp);
    b.s(SCORE_SCRIPT, "double decayGeoGauss(String,String,String,double,GeoPoint)", s_geo_gauss);
    b.s(SCORE_SCRIPT, "double decayNumericLinear(double,double,double,double,double)", s_num_linear);
    b.s(SCORE_SCRIPT, "double decayNumericExp(double,double,double,double,double)", s_num_exp);
    b.s(SCORE_SCRIPT, "double decayNumericGauss(double,double,double,double,double)", s_num_gauss);
    b.s(SCORE_SCRIPT, "double decayDateLinear(String,String,String,double,ZonedDateTime)", s_date_linear);
    b.s(SCORE_SCRIPT, "double decayDateExp(String,String,String,double,ZonedDateTime)", s_date_exp);
    b.s(SCORE_SCRIPT, "double decayDateGauss(String,String,String,double,ZonedDateTime)", s_date_gauss);
    b.s(SCORE_SCRIPT, "double l1norm(Object,String)", s_l1);
    b.s(SCORE_SCRIPT, "double l2norm(Object,String)", s_l2);
    b.s(SCORE_SCRIPT, "double cosineSimilarity(Object,String)", s_cosine);
    b.s(SCORE_SCRIPT, "double dotProduct(Object,String)", s_dot);
    b.s(SCORE_SCRIPT, "double hamming(Object,String)", s_hamming);
    b.s(SCORE_SCRIPT, "double maxSimDotProduct(Object,String)", super::rank::s_max_sim_dot);
    b.s(SCORE_SCRIPT, "double maxSimInvHamming(Object,String)", super::rank::s_max_sim_inv_hamming);

    // DenseVector
    b.interface(DENSE_VECTOR, &[]);
    b.sf(DENSE_VECTOR, "DenseVector EMPTY", v_empty);
    b.m(DENSE_VECTOR, "float getMagnitude()", v_magnitude);
    b.m(DENSE_VECTOR, "double dotProduct(Object)", v_dot);
    b.m(DENSE_VECTOR, "double l1Norm(Object)", v_l1);
    b.m(DENSE_VECTOR, "double l2Norm(Object)", v_l2);
    b.m(DENSE_VECTOR, "double cosineSimilarity(Object)", v_cos);
    b.m(DENSE_VECTOR, "float[] getVector()", v_get_vector);
    b.m(DENSE_VECTOR, "boolean isEmpty()", v_is_empty);
    b.m(DENSE_VECTOR, "int getDims()", v_dims);
    b.m(DENSE_VECTOR, "int size()", v_size);
    for c in [KNN_DENSE_VECTOR, BINARY_DENSE_VECTOR, BYTE_KNN_DENSE_VECTOR, BYTE_BINARY_DENSE_VECTOR, BIT_KNN_DENSE_VECTOR, BIT_BINARY_DENSE_VECTOR] {
        b.supers(c, &[OBJECT, DENSE_VECTOR]);
    }
    b.supers(DENSE_VECTOR_EMPTY, &[OBJECT, DENSE_VECTOR]);

    // RankVectors
    b.interface(RANK_VECTORS, &[]);
    b.sf(RANK_VECTORS, "RankVectors EMPTY", super::rank::r_empty);
    b.m(RANK_VECTORS, "float[] getMagnitudes()", super::rank::r_magnitudes);
    b.m(RANK_VECTORS, "Iterator getVectors()", super::rank::r_vectors);
    b.m(RANK_VECTORS, "boolean isEmpty()", super::rank::r_is_empty);
    b.m(RANK_VECTORS, "int getDims()", super::rank::r_dims);
    b.m(RANK_VECTORS, "int size()", super::rank::r_size);
    for c in [FLOAT_RANK_VECTORS, BFLOAT16_RANK_VECTORS, BYTE_RANK_VECTORS, BIT_RANK_VECTORS, RANK_VECTORS_EMPTY] {
        b.supers(c, &[OBJECT, RANK_VECTORS]);
    }

    // term stats, explanation
    b.supers(SCRIPT_TERM_STATS, &[OBJECT]);
    b.m(SCRIPT_TERM_STATS, "int uniqueTermsCount()", ts_unique);
    b.m(SCRIPT_TERM_STATS, "int matchedTermsCount()", ts_matched);
    b.m(SCRIPT_TERM_STATS, "StatsSummary docFreq()", ts_doc_freq);
    b.m(SCRIPT_TERM_STATS, "StatsSummary totalTermFreq()", ts_total_term_freq);
    b.m(SCRIPT_TERM_STATS, "StatsSummary termFreq()", ts_term_freq);
    b.m(SCRIPT_TERM_STATS, "StatsSummary termPositions()", ts_term_positions);
    b.supers(STATS_SUMMARY, &[OBJECT]);
    b.m(STATS_SUMMARY, "double getMin()", ss_min);
    b.m(STATS_SUMMARY, "double getMax()", ss_max);
    b.m(STATS_SUMMARY, "double getAverage()", ss_avg);
    b.m(STATS_SUMMARY, "double getSum()", ss_sum);
    b.m(STATS_SUMMARY, "long getCount()", ss_count);
    b.supers(EXPLANATION_HOLDER, &[OBJECT]);
    b.m(EXPLANATION_HOLDER, "void set(String)", ex_set);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversin_matches_elasticsearch() {
        // doc['g'].arcDistance(40, -70) with g = 41.12, -71.34 (quantized)
        assert_eq!(haversin_meters(41.1199999647215, -71.34000004269183, 40.0, -70.0), 168293.0769629009);
        assert_eq!(plane_distance(41.1199999647215, -71.34000004269183, 40.0, -70.0), 168297.68723657163);
    }

    #[test]
    fn murmur_and_mix() {
        assert_eq!(murmur3_x86_32(b"", 0), 0);
        assert_eq!(murmur3_x86_32(b"hello", 0), 0x248bfa47u32 as i32);
        assert_eq!(mix32(0), 0);
    }

    #[test]
    fn es_dates() {
        assert_eq!(parse_es_date("2020-01-01"), Some(1577836800000));
        assert_eq!(parse_es_date("1234"), Some(-23225875200000));
        assert_eq!(parse_es_date("1.5"), Some(1));
        assert_eq!(parse_es_date("2020-01-01T10:00:00+02:00"), Some(1577865600000));
        assert_eq!(parse_es_date("nope"), None);
    }

    #[test]
    fn time_values() {
        assert_eq!(parse_time_value_millis("1d", "x").unwrap(), 86_400_000);
        assert_eq!(parse_time_value_millis("36h", "x").unwrap(), 129_600_000);
        assert_eq!(parse_time_value_millis("0", "x").unwrap(), 0);
        assert!(parse_time_value_millis("1x", "x").is_err());
    }
}
