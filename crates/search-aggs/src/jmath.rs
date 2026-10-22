//! Java math functions whose exact results Elasticsearch's outputs depend on.
//!
//! * [`strict_log`]: `StrictMath.log` (fdlibm's `__ieee754_log`), bit for bit.
//! * [`fast_log`]: Elasticsearch's `FastMath.log` (via `ESSloppyMath.log`, derived from jafama):
//!   a 4096-entry table of `StrictMath.log` values plus a short series — used by geotile
//!   cell computation, so tiles at cell edges come out exactly as Elasticsearch's.
//!
//! * [`fast_atan`], [`fast_sinh`]: Elasticsearch's core `FastMath.atan` (an 8193-entry table
//!   of `StrictMath.atan` with a fourth-order Taylor correction, not the H3 library's 4097) and
//!   `FastMath.sinh` (via `StrictMath.expm1`): `GeoTileUtils.tileToLat`'s functions.
//!
//! fdlibm's constants are given by their bit patterns.

use std::sync::OnceLock;

const LN2_HI: f64 = f64::from_bits(0x3fe62e42fee00000);
const LN2_LO: f64 = f64::from_bits(0x3dea39ef35793c76);
const TWO54: f64 = f64::from_bits(0x4350000000000000);
const LG1: f64 = f64::from_bits(0x3fe5555555555593);
const LG2: f64 = f64::from_bits(0x3fd999999997fa04);
const LG3: f64 = f64::from_bits(0x3fd2492494229359);
const LG4: f64 = f64::from_bits(0x3fcc71c51d8e78af);
const LG5: f64 = f64::from_bits(0x3fc7466496cb03de);
const LG6: f64 = f64::from_bits(0x3fc39a09d078c69f);
const LG7: f64 = f64::from_bits(0x3fc2f112df3e5244);

#[inline]
fn hi(x: f64) -> i32 {
    (x.to_bits() >> 32) as i32
}

#[inline]
fn lo(x: f64) -> u32 {
    x.to_bits() as u32
}

#[inline]
fn with_hi(x: f64, h: i32) -> f64 {
    f64::from_bits(((h as u32 as u64) << 32) | (x.to_bits() & 0xffff_ffff))
}

/// `StrictMath.log` (fdlibm `e_log.c`).
pub fn strict_log(mut x: f64) -> f64 {
    let mut hx = hi(x);
    let lx = lo(x);
    let mut k: i32 = 0;
    if hx < 0x0010_0000 {
        if ((hx & 0x7fff_ffff) as u32 | lx) == 0 {
            return f64::NEG_INFINITY;
        }
        if hx < 0 {
            return f64::NAN;
        }
        k -= 54;
        x *= TWO54;
        hx = hi(x);
    }
    if hx >= 0x7ff0_0000 {
        return x + x;
    }
    k += (hx >> 20) - 1023;
    hx &= 0x000f_ffff;
    let i = (hx + 0x95f64) & 0x10_0000;
    x = with_hi(x, hx | (i ^ 0x3ff0_0000));
    k += i >> 20;
    let f = x - 1.0;
    if (0x000f_ffff & (2 + hx)) < 3 {
        if f == 0.0 {
            if k == 0 {
                return 0.0;
            }
            let dk = k as f64;
            return dk * LN2_HI + dk * LN2_LO;
        }
        let r = f * f * (0.5 - (1.0 / 3.0) * f);
        if k == 0 {
            return f - r;
        }
        let dk = k as f64;
        return dk * LN2_HI - ((r - dk * LN2_LO) - f);
    }
    let s = f / (2.0 + f);
    let dk = k as f64;
    let z = s * s;
    let mut i = hx - 0x6147a;
    let w = z * z;
    let j = 0x6b851 - hx;
    let t1 = w * (LG2 + w * (LG4 + w * LG6));
    let t2 = z * (LG1 + w * (LG3 + w * (LG5 + w * LG7)));
    i |= j;
    let r = t2 + t1;
    if i > 0 {
        let hfsq = 0.5 * f * f;
        if k == 0 {
            f - (hfsq - s * (hfsq + r))
        } else {
            dk * LN2_HI - ((hfsq - (s * (hfsq + r) + dk * LN2_LO)) - f)
        }
    } else if k == 0 {
        f - s * (f - r)
    } else {
        dk * LN2_HI - ((s * (f - r) - dk * LN2_LO) - f)
    }
}

const LOG_BITS: u32 = 12;
const LOG_TAB_SIZE: usize = 1 << LOG_BITS;
const MIN_DOUBLE_EXPONENT: i32 = -1074;
const MAX_DOUBLE_EXPONENT: i32 = 1023;

struct LogTables {
    log_x_log: Vec<f64>,
    log_x_inv: Vec<f64>,
}

fn log_tables() -> &'static LogTables {
    static T: OnceLock<LogTables> = OnceLock::new();
    T.get_or_init(|| {
        let mut log_x_log = Vec::with_capacity(LOG_TAB_SIZE);
        let mut log_x_inv = Vec::with_capacity(LOG_TAB_SIZE);
        for i in 0..LOG_TAB_SIZE {
            let x = 1.0 + i as f64 * (1.0 / LOG_TAB_SIZE as f64);
            log_x_log.push(strict_log(x));
            log_x_inv.push(1.0 / x);
        }
        LogTables { log_x_log, log_x_inv }
    })
}

/// `2^e` for `MIN_DOUBLE_EXPONENT <= e <= MAX_DOUBLE_EXPONENT` (exact, as `StrictMath.pow`).
fn two_pow(e: i32) -> f64 {
    if e >= -1022 {
        f64::from_bits(((e + 1023) as u64) << 52)
    } else {
        f64::from_bits(1u64 << (e - MIN_DOUBLE_EXPONENT))
    }
}

/// Elasticsearch's `FastMath.log`.
pub fn fast_log(mut value: f64) -> f64 {
    if value > 0.0 {
        if value == f64::INFINITY {
            return f64::INFINITY;
        }
        let h;
        if value > 0.95 {
            if value < 1.14 {
                let z = (value - 1.0) / (value + 1.0);
                let z2 = z * z;
                return z * (2.0 + z2 * ((2.0 / 3.0) + z2 * ((2.0 / 5.0) + z2 * ((2.0 / 7.0) + z2 * ((2.0 / 9.0) + z2 * (2.0 / 11.0))))));
            }
            h = 0.0;
        } else if value < f64::MIN_POSITIVE {
            value *= f64::from_bits(0x4330_0000_0000_0000);
            h = -52.0 * strict_log(2.0);
        } else {
            h = 0.0;
        }
        let t = log_tables();
        let bits_hi = (value.to_bits() >> 32) as i32;
        let exp = (bits_hi >> 20) - MAX_DOUBLE_EXPONENT;
        let x_index = ((bits_hi << 12) as u32 >> (32 - LOG_BITS)) as usize;
        let mut z = (value * two_pow(-exp)) * t.log_x_inv[x_index] - 1.0;
        z *= 1.0 - z * ((1.0 / 2.0) - z * (1.0 / 3.0));
        h + exp as f64 * strict_log(2.0) + (t.log_x_log[x_index] + z)
    } else if value == 0.0 {
        f64::NEG_INFINITY
    } else {
        f64::NAN
    }
}

/// The core `FastMath` atan tables (`ATAN_TABS_SIZE = 1 << 12 + 1`, which Java parses as
/// `1 << 13`): `(value, d1/1!, d2/2!, d3/3!, d4/4!)` per entry.
struct AtanTables {
    max_value: f64,
    delta: f64,
    indexer: f64,
    tab: Vec<[f64; 5]>,
}

const ATAN_TABS_SIZE: usize = 1 << 13;

fn atan_tables() -> &'static AtanTables {
    static T: OnceLock<AtanTables> = OnceLock::new();
    T.get_or_init(|| {
        let max_value = crate::fdlibm::tan(74.0 * (std::f64::consts::PI / 180.0));
        let delta = max_value / (ATAN_TABS_SIZE - 1) as f64;
        let tab = (0..ATAN_TABS_SIZE)
            .map(|i| {
                let x = i as f64 * delta;
                let inv = 1.0 / (1.0 + x * x);
                let inv2 = inv * inv;
                let inv3 = inv2 * inv;
                let inv4 = inv2 * inv2;
                [
                    crate::fdlibm::atan(x),
                    inv,
                    (-2.0 * x * inv2) * (1.0 / 2.0),
                    ((-2.0 + 6.0 * x * x) * inv3) * (1.0 / 6.0),
                    ((24.0 * x * (1.0 - x * x)) * inv4) * (1.0 / 24.0),
                ]
            })
            .collect();
        AtanTables { max_value, delta, indexer: 1.0 / delta, tab }
    })
}

const ATAN_HI3: f64 = f64::from_bits(0x3ff921fb54442d18);
const ATAN_LO3: f64 = f64::from_bits(0x3c91a62633145c07);
const ATAN_AT: [u64; 11] = [
    0x3fd555555555550d,
    0xbfc999999998ebc4,
    0x3fc24924920083ff,
    0xbfbc71c6fe231671,
    0x3fb745cdc54c206e,
    0xbfb3b0f2af749a6d,
    0x3fb10d66a0d03d51,
    0xbfadde2d52defd9a,
    0x3fa97b4b24760deb,
    0xbfa2b4442c6a6c2f,
    0x3f90ad3ae322da11,
];

/// Elasticsearch's core `FastMath.atan` (`ESSloppyMath.atan`).
pub fn fast_atan(value: f64) -> f64 {
    let (value, negate) = if value < 0.0 { (-value, true) } else { (value, false) };
    let t = atan_tables();
    let result = if value == 1.0 {
        std::f64::consts::PI / 4.0
    } else if value <= t.max_value {
        let index = (value * t.indexer + 0.5) as usize;
        let delta = value - index as f64 * t.delta;
        let e = &t.tab[index];
        e[0] + delta * (e[1] + delta * (e[2] + delta * (e[3] + delta * e[4])))
    } else if value < f64::from_bits(0x4410000000000000) {
        let a = ATAN_AT.map(f64::from_bits);
        let x = -1.0 / value;
        let x2 = x * x;
        let x4 = x2 * x2;
        let s1 = x2 * (a[0] + x4 * (a[2] + x4 * (a[4] + x4 * (a[6] + x4 * (a[8] + x4 * a[10])))));
        let s2 = x4 * (a[1] + x4 * (a[3] + x4 * (a[5] + x4 * (a[7] + x4 * a[9]))));
        ATAN_HI3 - ((x * (s1 + s2) - ATAN_LO3) - x)
    } else if value.is_nan() {
        return f64::NAN;
    } else {
        std::f64::consts::PI / 2.0
    };
    if negate { -result } else { result }
}

/// Elasticsearch's core `FastMath.sinh` (`ESSloppyMath.sinh`).
pub fn fast_sinh(mut value: f64) -> f64 {
    let h = if value < 0.0 {
        value = -value;
        -0.5
    } else {
        0.5
    };
    if value < 22.0 {
        if value < f64::from_bits(0x3E30000000000000) {
            if h < 0.0 { -value } else { value }
        } else {
            let t = crate::fdlibm::expm1(value);
            h * (t + t / (t + 1.0))
        }
    } else if value < strict_log(f64::MAX) {
        // `Math.exp`: HotSpot's intrinsic, only reached for |x| >= 22 (never by tile math).
        h * value.exp()
    } else {
        let t = (value * 0.5).exp();
        (h * t) * t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(x, StrictMath.log bits, ESSloppyMath.log bits)` from JDK 21 / Elasticsearch 9.5.5.
    const CASES: [(f64, i64, i64); 14] = [
        (2.0, 4604418534313441775, 4604418534313441775),
        (10.0, 4612367379483415830, 4612367379483415829),
        (1.0, 0, 0),
        (0.5, -4618953502541334033, -4618953502541334033),
        (1e-310, -4573882157708241656, -4573882157708241656),
        (1.000244140625, 4553138123938867889, 4553138123938867889),
        (0.96, -4637328146401063245, -4637328146401063245),
        (1.13, 4593471129226285447, 4593471129226285444),
        (1.14, 4593888810960523301, 4593888810960523298),
        (0.95, -4635819073051289282, -4635819073051289280),
        (3.7e-5, -4601437743845651023, -4601437743845651023),
        (7.25, 4611600456649654862, 4611600456649654862),
        (1e300, 4649287341619838901, 4649287341619838901),
        (0.9500000001, -4635819073066459303, -4635819073066459302),
    ];

    #[test]
    fn logs_match_java() {
        for (x, strict, fast) in CASES {
            assert_eq!(strict_log(x).to_bits() as i64, strict, "StrictMath.log({x})");
            assert_eq!(fast_log(x).to_bits() as i64, fast, "FastMath.log({x})");
        }
        assert!(strict_log(-1.0).is_nan());
        assert_eq!(strict_log(0.0), f64::NEG_INFINITY);
    }

    /// 200k values `(1 + nextDouble()) * 2^(nextInt(120) - 60)` of `java.util.Random(7)`,
    /// hashed `h = h * 31 + bits` (Java longs).
    #[test]
    fn logs_match_java_in_bulk() {
        let mut seed: u64 = (7u64 ^ 0x5DEECE66D) & ((1 << 48) - 1);
        let mut next = |bits: u32| -> i32 {
            seed = (seed.wrapping_mul(0x5DEECE66D).wrapping_add(0xB)) & ((1 << 48) - 1);
            (seed >> (48 - bits)) as i64 as i32
        };
        let (mut h1, mut h2) = (0i64, 0i64);
        for _ in 0..200_000 {
            let d = ((((next(26) as i64) << 27) + next(27) as i64) as f64) * (1.0 / (1u64 << 53) as f64);
            // nextInt(120): 120 is not a power of two.
            let e = loop {
                let u = next(31);
                let r = u % 120;
                if u - r + 119 >= 0 {
                    break r;
                }
            };
            let x = (1.0 + d) * 2f64.powi(e - 60);
            h1 = h1.wrapping_mul(31).wrapping_add(strict_log(x).to_bits() as i64);
            h2 = h2.wrapping_mul(31).wrapping_add(fast_log(x).to_bits() as i64);
        }
        assert_eq!(h1, -3035726698654596420);
        assert_eq!(h2, 442250299968934830);
    }
}

