//! Geo math with Lucene's arithmetic: `SloppyMath` (table-driven cos/asin and haversine
//! distance, which Elasticsearch's `arc` distance uses) and the geo-point doc values encoding.
//!
//! The tables are built with fdlibm functions (the `libm` crate, as Java's `StrictMath`), so
//! distances — and therefore geo distance filters at their boundary, decay scores and sort
//! values — match Elasticsearch to the bit.

use std::sync::OnceLock;

use search_codec::points::encode;

const TO_METERS: f64 = 6_371_008.771_4;
const ONE_DIV_F2: f64 = 1.0 / 2.0;
const ONE_DIV_F3: f64 = 1.0 / 6.0;
const ONE_DIV_F4: f64 = 1.0 / 24.0;
const SIN_COS_TABS_SIZE: usize = (1 << 11) + 1;
const ASIN_TABS_SIZE: usize = (1 << 13) + 1;

struct Tables {
    sin: Vec<f64>,
    cos: Vec<f64>,
    asin: Vec<f64>,
    d1: Vec<f64>,
    d2: Vec<f64>,
    d3: Vec<f64>,
    d4: Vec<f64>,
    delta_hi: f64,
    delta_lo: f64,
    indexer: f64,
    max_int_modulo: f64,
    asin_max: f64,
    asin_delta: f64,
    asin_indexer: f64,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let pio2_hi = f64::from_bits(0x3FF921FB54400000);
        let pio2_lo = f64::from_bits(0x3DD0B4611A626331);
        let twopi_hi = 4.0 * pio2_hi;
        let twopi_lo = 4.0 * pio2_lo;
        let delta_hi = twopi_hi / (SIN_COS_TABS_SIZE - 1) as f64;
        let delta_lo = twopi_lo / (SIN_COS_TABS_SIZE - 1) as f64;
        let indexer = 1.0 / (delta_hi + delta_lo);
        let max_int_modulo = ((i32::MAX >> 9) as f64 / indexer) * 0.99;
        let mut sin = vec![0.0; SIN_COS_TABS_SIZE];
        let mut cos = vec![0.0; SIN_COS_TABS_SIZE];
        let pi_i = (SIN_COS_TABS_SIZE - 1) / 2;
        for i in 0..SIN_COS_TABS_SIZE {
            let a = i as f64 * delta_hi + i as f64 * delta_lo;
            let mut s = libm::sin(a);
            let mut c = libm::cos(a);
            if i == pi_i || i == 2 * pi_i {
                s = 0.0;
            } else if i == pi_i / 2 || i == 3 * pi_i / 2 {
                c = 0.0;
            }
            sin[i] = s;
            cos[i] = c;
        }
        let asin_max = libm::sin(73.0f64.to_radians());
        let asin_delta = asin_max / (ASIN_TABS_SIZE - 1) as f64;
        let asin_indexer = 1.0 / asin_delta;
        let (mut asin, mut d1, mut d2, mut d3, mut d4) = (
            vec![0.0; ASIN_TABS_SIZE],
            vec![0.0; ASIN_TABS_SIZE],
            vec![0.0; ASIN_TABS_SIZE],
            vec![0.0; ASIN_TABS_SIZE],
            vec![0.0; ASIN_TABS_SIZE],
        );
        for i in 0..ASIN_TABS_SIZE {
            let x = i as f64 * asin_delta;
            asin[i] = libm::asin(x);
            let inv = 1.0 / (1.0 - x * x);
            let inv05 = libm::sqrt(inv);
            let inv15 = inv05 * inv;
            let inv25 = inv15 * inv;
            let inv35 = inv25 * inv;
            d1[i] = inv05;
            d2[i] = (x * inv15) * ONE_DIV_F2;
            d3[i] = ((1.0 + 2.0 * x * x) * inv25) * ONE_DIV_F3;
            d4[i] = ((5.0 + 2.0 * x * (2.0 + x * (5.0 - 2.0 * x))) * inv35) * ONE_DIV_F4;
        }
        Tables { sin, cos, asin, d1, d2, d3, d4, delta_hi, delta_lo, indexer, max_int_modulo, asin_max, asin_delta, asin_indexer }
    })
}

/// Lucene `SloppyMath.cos`.
pub fn sloppy_cos(mut a: f64) -> f64 {
    let t = tables();
    if a < 0.0 {
        a = -a;
    }
    if a > t.max_int_modulo {
        return a.cos();
    }
    let mut index = (a * t.indexer + 0.5) as i32;
    let delta = (a - index as f64 * t.delta_hi) - index as f64 * t.delta_lo;
    index &= (SIN_COS_TABS_SIZE - 2) as i32;
    let ic = t.cos[index as usize];
    let is = t.sin[index as usize];
    ic + delta * (-is + delta * (-ic * ONE_DIV_F2 + delta * (is * ONE_DIV_F3 + delta * ic * ONE_DIV_F4)))
}

/// Lucene `SloppyMath.asin`.
pub fn sloppy_asin(mut a: f64) -> f64 {
    let t = tables();
    let neg = a < 0.0;
    if neg {
        a = -a;
    }
    if a <= t.asin_max {
        let index = (a * t.asin_indexer + 0.5) as usize;
        let delta = a - index as f64 * t.asin_delta;
        let r = t.asin[index] + delta * (t.d1[index] + delta * (t.d2[index] + delta * (t.d3[index] + delta * t.d4[index])));
        return if neg { -r } else { r };
    }
    if a < 1.0 {
        let ps = [
            f64::from_bits(0x3fc5555555555555),
            f64::from_bits(0xbfd4d61203eb6f7d),
            f64::from_bits(0x3fc9c1550e884455),
            f64::from_bits(0xbfa48228b5688f3b),
            f64::from_bits(0x3f49efe07501b288),
            f64::from_bits(0x3f023de10dfdf709),
        ];
        let qs = [
            f64::from_bits(0xc0033a271c8a2d4b),
            f64::from_bits(0x40002ae59c598ac8),
            f64::from_bits(0xbfe6066c1b8d0159),
            f64::from_bits(0x3fb3b8c5b12e9282),
        ];
        let tt = (1.0 - a) * 0.5;
        let p = tt * (ps[0] + tt * (ps[1] + tt * (ps[2] + tt * (ps[3] + tt * (ps[4] + tt * ps[5])))));
        let q = 1.0 + tt * (qs[0] + tt * (qs[1] + tt * (qs[2] + tt * qs[3])));
        let s = tt.sqrt();
        let z = s + s * (p / q);
        let r = f64::from_bits(0x3FF921FB54442D18) - ((z + z) - f64::from_bits(0x3C91A62633145C07));
        return if neg { -r } else { r };
    }
    if a == 1.0 {
        return if neg { -std::f64::consts::FRAC_PI_2 } else { std::f64::consts::FRAC_PI_2 };
    }
    f64::NAN
}

/// Lucene `SloppyMath.haversinSortKey`.
pub fn haversin_sort_key(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let x1 = lat1.to_radians();
    let x2 = lat2.to_radians();
    let h1 = 1.0 - sloppy_cos(x1 - x2);
    let h2 = 1.0 - sloppy_cos((lon1 - lon2).to_radians());
    let h = h1 + sloppy_cos(x1) * sloppy_cos(x2) * h2;
    f64::from_bits(h.to_bits() & 0xFFFF_FFFF_FFFF_FFF8)
}

/// Distance of a sort key in meters.
pub fn haversin_meters_key(key: f64) -> f64 {
    TO_METERS * 2.0 * sloppy_asin(1f64.min((key * 0.5).sqrt()))
}

/// Lucene `SloppyMath.haversinMeters` = Elasticsearch's `arc` distance in meters.
pub fn haversin_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    haversin_meters_key(haversin_sort_key(lat1, lon1, lat2, lon2))
}

/// Elasticsearch's `plane` distance in meters.
pub fn plane_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let x = (lon2 - lon1).to_radians() * ((lat2 + lat1) / 2.0).to_radians().cos();
    let y = (lat2 - lat1).to_radians();
    (x * x + y * y).sqrt() * TO_METERS
}

/// Decodes a geo-point doc value (`LatLonDocValuesField`: latitude in the high 32 bits).
pub fn decode_geo_dv(v: i64) -> (f64, f64) {
    let lat = encode::decode_latitude((v >> 32) as i32);
    let lon = encode::decode_longitude(v as i32);
    (lat, lon)
}

/// Encodes a point the way `LatLonDocValuesField` does.
pub fn encode_geo_dv(lat: f64, lon: f64) -> i64 {
    let a = encode::encode_latitude(lat) as u32 as u64;
    let b = encode::encode_longitude(lon) as u32 as u64;
    (a << 32 | b) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_is_close_to_exact() {
        let d = haversin_meters(40.7143528, -74.0059731, 40.759011, -73.9844722);
        assert!((d - 5286.0).abs() < 10.0, "{d}");
        assert_eq!(haversin_meters(10.0, 20.0, 10.0, 20.0), 0.0);
        let (lat, lon) = decode_geo_dv(encode_geo_dv(52.5, 13.4));
        assert!((lat - 52.5).abs() < 1e-6 && (lon - 13.4).abs() < 1e-6);
        assert!((sloppy_cos(1.0) - 1f64.cos()).abs() < 1e-12);
        assert!((sloppy_asin(0.3) - 0.3f64.asin()).abs() < 1e-12);
        assert!((sloppy_asin(0.99) - 0.99f64.asin()).abs() < 1e-12);
    }
}
