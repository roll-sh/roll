//! The trigonometry of Elasticsearch's H3 library (`org.elasticsearch.h3.FastMath`, a fork of
//! jafama's `FastMath`, Apache-2.0): table lookups plus a fourth-order Taylor correction for
//! `sin`, `cos`, `tan`, `asin`, `atan`, and fdlibm-derived code near the ends of the ranges.
//! Which H3 cell a point falls in depends on these exact values at cell edges, so they are
//! reproduced bit for bit: the tables are built (once, on first use) from fdlibm through
//! [`crate::fdlibm`] exactly as jafama's static initializer builds them from `StrictMath`.

// jafama's literals, verbatim.
#![allow(clippy::excessive_precision, clippy::unreadable_literal)]

use std::f64::consts::PI;
use std::sync::OnceLock;

use crate::fdlibm;

const ONE_DIV_F2: f64 = 1.0 / 2.0;
const ONE_DIV_F3: f64 = 1.0 / 6.0;
const ONE_DIV_F4: f64 = 1.0 / 24.0;
const TWO_POW_24: f64 = 16777216.0;
const TWO_POW_N24: f64 = 1.0 / 16777216.0;
const TWO_POW_66: f64 = 73786976294838206464.0;
const MIN_DOUBLE_EXPONENT: i32 = -1074;
const M_HALF_PI: f64 = PI * 0.5;
const M_QUARTER_PI: f64 = PI * 0.25;
const M_2PI: f64 = 2.0 * PI;

const ONE_OVER_TWOPI_TAB: [f64; 47] = [
    0x28BE60 as f64, 0xDB9391 as f64, 0x054A7F as f64, 0x09D5F4 as f64, 0x7D4D37 as f64, 0x7036D8 as f64, 0xA5664F as f64, 0x10E410 as f64,
    0x7F9458 as f64, 0xEAF7AE as f64, 0xF1586D as f64, 0xC91B8E as f64, 0x909374 as f64, 0xB80192 as f64, 0x4BBA82 as f64, 0x746487 as f64,
    0x3F877A as f64, 0xC72C4A as f64, 0x69CFBA as f64, 0x208D7D as f64, 0x4BAED1 as f64, 0x213A67 as f64, 0x1C09AD as f64, 0x17DF90 as f64,
    0x4E6475 as f64, 0x8E60D4 as f64, 0xCE7D27 as f64, 0x2117E2 as f64, 0xEF7E4A as f64, 0x0EC7FE as f64, 0x25FFF7 as f64, 0x816603 as f64,
    0xFBCBC4 as f64, 0x62D682 as f64, 0x9B47DB as f64, 0x4D9FB3 as f64, 0xC9F2C2 as f64, 0x6DD3D1 as f64, 0x8FD9A7 as f64, 0x97FA8B as f64,
    0x5D49EE as f64, 0xB1FAF9 as f64, 0x7C5ECF as f64, 0x41CE7D as f64, 0xE294A4 as f64, 0xBA9AFE as f64, 0xD7EC47 as f64,
];

const TWOPI_TAB0: f64 = f64::from_bits(0x401921FB40000000);
const TWOPI_TAB1: f64 = f64::from_bits(0x3E94442D00000000);
const TWOPI_TAB2: f64 = f64::from_bits(0x3D18469880000000);
const TWOPI_TAB3: f64 = f64::from_bits(0x3B98CC5160000000);
const TWOPI_TAB4: f64 = f64::from_bits(0x3A101B8380000000);
const INVPIO2: f64 = f64::from_bits(0x3FE45F306DC9C883);
const PIO2_HI: f64 = f64::from_bits(0x3FF921FB54400000);
const PIO2_LO: f64 = f64::from_bits(0x3DD0B4611A626331);
const INVTWOPI: f64 = INVPIO2 / 4.0;
const TWOPI_HI: f64 = 4.0 * PIO2_HI;
const TWOPI_LO: f64 = 4.0 * PIO2_LO;

const SIN_COS_TABS_SIZE: usize = (1 << 11) + 1;
const SIN_COS_DELTA_HI: f64 = TWOPI_HI / (SIN_COS_TABS_SIZE - 1) as f64;
const SIN_COS_DELTA_LO: f64 = TWOPI_LO / (SIN_COS_TABS_SIZE - 1) as f64;
const SIN_COS_INDEXER: f64 = 1.0 / (SIN_COS_DELTA_HI + SIN_COS_DELTA_LO);
const SIN_COS_MAX_VALUE_FOR_INT_MODULO: f64 = ((i32::MAX >> 9) as f64 / SIN_COS_INDEXER) * 0.99;

const TAN_VIRTUAL_TABS_SIZE: usize = (1 << 12) + 1;
const TAN_DELTA_HI: f64 = PIO2_HI / (TAN_VIRTUAL_TABS_SIZE - 1) as f64;
const TAN_DELTA_LO: f64 = PIO2_LO / (TAN_VIRTUAL_TABS_SIZE - 1) as f64;
const TAN_INDEXER: f64 = 1.0 / (TAN_DELTA_HI + TAN_DELTA_LO);
const TAN_MAX_VALUE_FOR_INT_MODULO: f64 = ((i32::MAX >> 9) as f64 / TAN_INDEXER) * 0.99;

const ASIN_TABS_SIZE: usize = (1 << 13) + 1;
const ASIN_POWTABS_POWER: i32 = 84;
const ASIN_POWTABS_SIZE: usize = (1 << 12) + 1;
const ASIN_POWTABS_SIZE_MINUS_ONE: usize = ASIN_POWTABS_SIZE - 1;
const ASIN_PIO2_HI: f64 = f64::from_bits(0x3FF921FB54442D18);
const ASIN_PIO2_LO: f64 = f64::from_bits(0x3C91A62633145C07);
const ASIN_PS0: f64 = f64::from_bits(0x3fc5555555555555);
const ASIN_PS1: f64 = f64::from_bits(0xbfd4d61203eb6f7d);
const ASIN_PS2: f64 = f64::from_bits(0x3fc9c1550e884455);
const ASIN_PS3: f64 = f64::from_bits(0xbfa48228b5688f3b);
const ASIN_PS4: f64 = f64::from_bits(0x3f49efe07501b288);
const ASIN_PS5: f64 = f64::from_bits(0x3f023de10dfdf709);
const ASIN_QS1: f64 = f64::from_bits(0xc0033a271c8a2d4b);
const ASIN_QS2: f64 = f64::from_bits(0x40002ae59c598ac8);
const ASIN_QS3: f64 = f64::from_bits(0xbfe6066c1b8d0159);
const ASIN_QS4: f64 = f64::from_bits(0x3fb3b8c5b12e9282);

const ATAN_TABS_SIZE: usize = (1 << 12) + 1;
const ATAN_HI3: f64 = f64::from_bits(0x3ff921fb54442d18);
const ATAN_LO3: f64 = f64::from_bits(0x3c91a62633145c07);
const ATAN_AT: [f64; 11] = [
    f64::from_bits(0x3fd555555555550d),
    f64::from_bits(0xbfc999999998ebc4),
    f64::from_bits(0x3fc24924920083ff),
    f64::from_bits(0xbfbc71c6fe231671),
    f64::from_bits(0x3fb745cdc54c206e),
    f64::from_bits(0xbfb3b0f2af749a6d),
    f64::from_bits(0x3fb10d66a0d03d51),
    f64::from_bits(0xbfadde2d52defd9a),
    f64::from_bits(0x3fa97b4b24760deb),
    f64::from_bits(0xbfa2b4442c6a6c2f),
    f64::from_bits(0x3f90ad3ae322da11),
];

/// A value with its first four derivatives divided by `n!`, per table entry.
#[derive(Clone, Copy, Default)]
struct Poly([f64; 5]);

impl Poly {
    /// `t0 + d * (t1 + d * (t2 + d * (t3 + d * t4)))`.
    #[inline]
    fn eval(&self, d: f64) -> f64 {
        let t = &self.0;
        t[0] + d * (t[1] + d * (t[2] + d * (t[3] + d * t[4])))
    }
}

struct Tables {
    sin: Vec<f64>,
    cos: Vec<f64>,
    tan_size: usize,
    tan: Vec<Poly>,
    asin_max_value: f64,
    asin_delta: f64,
    asin_indexer: f64,
    asin: Vec<Poly>,
    asin_max_value_pow: f64,
    asin_one_div_max_value_pow: f64,
    asin_param_pow: Vec<f64>,
    asin_pow: Vec<Poly>,
    atan_max_value: f64,
    atan_delta: f64,
    atan_indexer: f64,
    atan: Vec<Poly>,
    normalize_max_medium: f64,
    two_pow: Vec<f64>,
}

/// `Math.toRadians` (`deg * (PI / 180)`).
#[inline]
fn to_radians(deg: f64) -> f64 {
    deg * (PI / 180.0)
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        // Sin/cos, with exact zeros at multiples of PI/2.
        let pi_index = (SIN_COS_TABS_SIZE - 1) / 2;
        let (mut sin, mut cos) = (vec![0.0; SIN_COS_TABS_SIZE], vec![0.0; SIN_COS_TABS_SIZE]);
        for i in 0..SIN_COS_TABS_SIZE {
            let angle = i as f64 * SIN_COS_DELTA_HI + i as f64 * SIN_COS_DELTA_LO;
            let (mut s, mut c) = (fdlibm::sin(angle), fdlibm::cos(angle));
            if i == pi_index || i == 2 * pi_index {
                s = 0.0;
            } else if i == pi_index / 2 || i == 3 * pi_index / 2 {
                c = 0.0;
            }
            sin[i] = s;
            cos[i] = c;
        }
        let tan_max_value = to_radians(77.0);
        let tan_size = ((tan_max_value / (PI / 2.0)) * (TAN_VIRTUAL_TABS_SIZE - 1) as f64) as usize + 1;
        let tan = (0..tan_size)
            .map(|i| {
                let angle = i as f64 * TAN_DELTA_HI + i as f64 * TAN_DELTA_LO;
                let c = fdlibm::cos(angle);
                let s = fdlibm::sin(angle);
                let inv = 1.0 / c;
                let inv2 = inv * inv;
                let inv3 = inv2 * inv;
                let inv4 = inv2 * inv2;
                let inv5 = inv3 * inv2;
                Poly([
                    fdlibm::tan(angle),
                    inv2,
                    ((2.0 * s) * inv3) * ONE_DIV_F2,
                    ((2.0 * (1.0 + 2.0 * s * s)) * inv4) * ONE_DIV_F3,
                    ((8.0 * s * (2.0 + s * s)) * inv5) * ONE_DIV_F4,
                ])
            })
            .collect();
        let asin_poly = |x: f64| {
            let inv = 1.0 / (1.0 - x * x);
            let inv0_5 = inv.sqrt();
            let inv1_5 = inv0_5 * inv;
            let inv2_5 = inv1_5 * inv;
            let inv3_5 = inv2_5 * inv;
            Poly([
                fdlibm::asin(x),
                inv0_5,
                (x * inv1_5) * ONE_DIV_F2,
                ((1.0 + 2.0 * x * x) * inv2_5) * ONE_DIV_F3,
                ((5.0 + 2.0 * x * (2.0 + x * (5.0 - 2.0 * x))) * inv3_5) * ONE_DIV_F4,
            ])
        };
        let asin_max_value = fdlibm::sin(to_radians(73.0));
        let asin_delta = asin_max_value / (ASIN_TABS_SIZE - 1) as f64;
        let asin = (0..ASIN_TABS_SIZE).map(|i| asin_poly(i as f64 * asin_delta)).collect();
        let asin_max_value_pow = fdlibm::sin(to_radians(88.6));
        let mut asin_param_pow = Vec::with_capacity(ASIN_POWTABS_SIZE);
        let mut asin_pow = Vec::with_capacity(ASIN_POWTABS_SIZE);
        for i in 0..ASIN_POWTABS_SIZE {
            let x = fdlibm::pow(i as f64 * (1.0 / ASIN_POWTABS_SIZE_MINUS_ONE as f64), 1.0 / ASIN_POWTABS_POWER as f64) * asin_max_value_pow;
            asin_param_pow.push(x);
            asin_pow.push(asin_poly(x));
        }
        let atan_max_value = fdlibm::tan(to_radians(74.0));
        let atan_delta = atan_max_value / (ATAN_TABS_SIZE - 1) as f64;
        let atan = (0..ATAN_TABS_SIZE)
            .map(|i| {
                let x = i as f64 * atan_delta;
                let inv = 1.0 / (1.0 + x * x);
                let inv2 = inv * inv;
                let inv3 = inv2 * inv;
                let inv4 = inv2 * inv2;
                Poly([
                    fdlibm::atan(x),
                    inv,
                    (-2.0 * x * inv2) * ONE_DIV_F2,
                    ((-2.0 + 6.0 * x * x) * inv3) * ONE_DIV_F3,
                    ((24.0 * x * (1.0 - x * x)) * inv4) * ONE_DIV_F4,
                ])
            })
            .collect();
        let two_pow = (MIN_DOUBLE_EXPONENT..=1023).map(|i| fdlibm::pow(2.0, i as f64)).collect();
        Tables {
            sin,
            cos,
            tan_size,
            tan,
            asin_max_value,
            asin_delta,
            asin_indexer: 1.0 / asin_delta,
            asin,
            asin_max_value_pow,
            asin_one_div_max_value_pow: 1.0 / asin_max_value_pow,
            asin_param_pow,
            asin_pow,
            atan_max_value,
            atan_delta,
            atan_indexer: 1.0 / atan_delta,
            atan,
            normalize_max_medium: fdlibm::pow(2.0, 20.0) * (2.0 * PI),
            two_pow,
        }
    })
}

/// `FastMath.cos`.
pub fn cos(angle: f64) -> f64 {
    let t = tables();
    let mut angle = angle.abs();
    if angle > SIN_COS_MAX_VALUE_FOR_INT_MODULO {
        angle = remainder_two_pi(angle);
        if angle < 0.0 {
            angle += M_2PI;
        }
    }
    let mut index = (angle * SIN_COS_INDEXER + 0.5) as i32;
    let delta = (angle - index as f64 * SIN_COS_DELTA_HI) - index as f64 * SIN_COS_DELTA_LO;
    index &= (SIN_COS_TABS_SIZE - 2) as i32;
    let (c, s) = (t.cos[index as usize], t.sin[index as usize]);
    c + delta * (-s + delta * (-c * ONE_DIV_F2 + delta * (s * ONE_DIV_F3 + delta * c * ONE_DIV_F4)))
}

/// `FastMath.sin`.
pub fn sin(angle: f64) -> f64 {
    let t = tables();
    let (mut angle, negate) = if angle < 0.0 { (-angle, true) } else { (angle, false) };
    if angle > SIN_COS_MAX_VALUE_FOR_INT_MODULO {
        angle = remainder_two_pi(angle);
        if angle < 0.0 {
            angle += M_2PI;
        }
    }
    let mut index = (angle * SIN_COS_INDEXER + 0.5) as i32;
    let delta = (angle - index as f64 * SIN_COS_DELTA_HI) - index as f64 * SIN_COS_DELTA_LO;
    index &= (SIN_COS_TABS_SIZE - 2) as i32;
    let (s, c) = (t.sin[index as usize], t.cos[index as usize]);
    let result = s + delta * (c + delta * (-s * ONE_DIV_F2 + delta * (-c * ONE_DIV_F3 + delta * s * ONE_DIV_F4)));
    if negate { -result } else { result }
}

/// `FastMath.tan`.
pub fn tan(angle: f64) -> f64 {
    let t = tables();
    let mut angle = angle;
    if angle.abs() > TAN_MAX_VALUE_FOR_INT_MODULO {
        angle = remainder_two_pi(angle);
        if angle < -M_HALF_PI {
            angle += PI;
        } else if angle > M_HALF_PI {
            angle -= PI;
        }
    }
    let mut negate = false;
    if angle < 0.0 {
        angle = -angle;
        negate = true;
    }
    let mut index = (angle * TAN_INDEXER + 0.5) as i32;
    let mut delta = (angle - index as f64 * TAN_DELTA_HI) - index as f64 * TAN_DELTA_LO;
    index &= (2 * (TAN_VIRTUAL_TABS_SIZE - 1) - 1) as i32;
    if index > (TAN_VIRTUAL_TABS_SIZE - 1) as i32 {
        index = (2 * (TAN_VIRTUAL_TABS_SIZE - 1)) as i32 - index;
        delta = -delta;
        negate = !negate;
    }
    let result = if (index as usize) < t.tan_size {
        t.tan[index as usize].eval(delta)
    } else {
        let p = &t.tan[((TAN_VIRTUAL_TABS_SIZE - 1) as i32 - index) as usize].0;
        1.0 / (p[0] - delta * (p[1] - delta * (p[2] - delta * (p[3] - delta * p[4]))))
    };
    if negate { -result } else { result }
}

/// `FastMath.acos`.
pub fn acos(value: f64) -> f64 {
    M_HALF_PI - asin(value)
}

/// `FastMath.asin`.
pub fn asin(value: f64) -> f64 {
    let t = tables();
    let (value, negate) = if value < 0.0 { (-value, true) } else { (value, false) };
    let result = if value <= t.asin_max_value {
        let index = (value * t.asin_indexer + 0.5) as usize;
        let delta = value - index as f64 * t.asin_delta;
        t.asin[index].eval(delta)
    } else if value <= t.asin_max_value_pow {
        let index = (pow_fast(value * t.asin_one_div_max_value_pow, ASIN_POWTABS_POWER) * ASIN_POWTABS_SIZE_MINUS_ONE as f64 + 0.5) as usize;
        let delta = value - t.asin_param_pow[index];
        t.asin_pow[index].eval(delta)
    } else if value < 1.0 {
        let t = (1.0 - value) * 0.5;
        let p = t * (ASIN_PS0 + t * (ASIN_PS1 + t * (ASIN_PS2 + t * (ASIN_PS3 + t * (ASIN_PS4 + t * ASIN_PS5)))));
        let q = 1.0 + t * (ASIN_QS1 + t * (ASIN_QS2 + t * (ASIN_QS3 + t * ASIN_QS4)));
        let s = t.sqrt();
        let z = s + s * (p / q);
        ASIN_PIO2_HI - ((z + z) - ASIN_PIO2_LO)
    } else if value == 1.0 {
        M_HALF_PI
    } else {
        return f64::NAN;
    };
    if negate { -result } else { result }
}

/// `FastMath.atan`.
pub fn atan(value: f64) -> f64 {
    let t = tables();
    let (value, negate) = if value < 0.0 { (-value, true) } else { (value, false) };
    let result = if value == 1.0 {
        M_QUARTER_PI
    } else if value <= t.atan_max_value {
        let index = (value * t.atan_indexer + 0.5) as usize;
        let delta = value - index as f64 * t.atan_delta;
        t.atan[index].eval(delta)
    } else if value < TWO_POW_66 {
        let x = -1.0 / value;
        let x2 = x * x;
        let x4 = x2 * x2;
        let a = &ATAN_AT;
        let s1 = x2 * (a[0] + x4 * (a[2] + x4 * (a[4] + x4 * (a[6] + x4 * (a[8] + x4 * a[10])))));
        let s2 = x4 * (a[1] + x4 * (a[3] + x4 * (a[5] + x4 * (a[7] + x4 * a[9]))));
        ATAN_HI3 - ((x * (s1 + s2) - ATAN_LO3) - x)
    } else if value.is_nan() {
        return f64::NAN;
    } else {
        M_HALF_PI
    };
    if negate { -result } else { result }
}

/// `FastMath.atan2`.
pub fn atan2(y: f64, x: f64) -> f64 {
    let neg_zero = |v: f64| v == 0.0 && v.is_sign_negative();
    if x > 0.0 {
        if y == 0.0 {
            return if neg_zero(y) { -0.0 } else { 0.0 };
        }
        if x == f64::INFINITY {
            return if y == f64::INFINITY {
                M_QUARTER_PI
            } else if y == f64::NEG_INFINITY {
                -M_QUARTER_PI
            } else if y > 0.0 {
                0.0
            } else if y < 0.0 {
                -0.0
            } else {
                f64::NAN
            };
        }
        atan(y / x)
    } else if x < 0.0 {
        if y == 0.0 {
            return if neg_zero(y) { -PI } else { PI };
        }
        if x == f64::NEG_INFINITY {
            return if y == f64::INFINITY {
                3.0 * M_QUARTER_PI
            } else if y == f64::NEG_INFINITY {
                -3.0 * M_QUARTER_PI
            } else if y > 0.0 {
                PI
            } else if y < 0.0 {
                -PI
            } else {
                f64::NAN
            };
        }
        if y > 0.0 {
            M_HALF_PI + atan(-x / y)
        } else if y < 0.0 {
            -M_HALF_PI - atan(x / y)
        } else {
            f64::NAN
        }
    } else if x == 0.0 {
        if y == 0.0 {
            return if neg_zero(x) {
                if neg_zero(y) { -PI } else { PI }
            } else if neg_zero(y) {
                -0.0
            } else {
                0.0
            };
        }
        if y > 0.0 {
            M_HALF_PI
        } else if y < 0.0 {
            -M_HALF_PI
        } else {
            f64::NAN
        }
    } else {
        f64::NAN
    }
}

/// `FastMath.powFast`.
fn pow_fast(mut value: f64, mut power: i32) -> f64 {
    if power > 5 {
        let mut odd = 1.0;
        loop {
            if power & 1 != 0 {
                odd *= value;
            }
            value *= value;
            power >>= 1;
            if power <= 5 {
                break;
            }
        }
        match power {
            3 => odd * value * value * value,
            4 => {
                let v2 = value * value;
                odd * v2 * v2
            }
            _ => {
                let v2 = value * value;
                odd * v2 * v2 * value
            }
        }
    } else if power >= 0 {
        match power {
            0 => 1.0,
            1 => value,
            2 => value * value,
            3 => value * value * value,
            4 => {
                let v2 = value * value;
                v2 * v2
            }
            _ => {
                let v2 = value * value;
                v2 * v2 * value
            }
        }
    } else if power == i32::MIN {
        1.0 / (pow_fast(value, i32::MAX) * value)
    } else {
        1.0 / pow_fast(value, -power)
    }
}

/// `FastMath.remainderTwoPi`.
fn remainder_two_pi(angle: f64) -> f64 {
    let t = tables();
    let (angle, negate) = if angle < 0.0 { (-angle, true) } else { (angle, false) };
    let result = if angle <= t.normalize_max_medium {
        let fn_ = (angle * INVTWOPI + 0.5) as i32 as f64;
        (angle - fn_ * TWOPI_HI) - fn_ * TWOPI_LO
    } else if angle < f64::INFINITY {
        let lx = angle.to_bits() as i64;
        let exp = ((lx >> 52) & 0x7FF) - 1046;
        let mut z = f64::from_bits((lx - (exp << 52)) as u64);
        let x0 = z as i32 as f64;
        z = (z - x0) * TWO_POW_24;
        let x1 = z as i32 as f64;
        let x2 = (z - x1) * TWO_POW_24;
        sub_remainder_two_pi(t, x0, x1, x2, exp as i32, if x2 == 0.0 { 2 } else { 3 })
    } else {
        return f64::NAN;
    };
    if negate { -result } else { result }
}

/// `FastMath.subRemainderTwoPi` (fdlibm's `__kernel_rem_pio2`, specialized to `2*PI`).
#[allow(clippy::too_many_arguments)]
fn sub_remainder_two_pi(t: &Tables, x0: f64, x1: f64, x2: f64, e0: i32, nx: i32) -> f64 {
    let tab = |i: i32| if i >= 0 { ONE_OVER_TWOPI_TAB[i as usize] } else { 0.0 };
    let jx = nx - 1;
    let jv = (e0 - 3) / 24;
    let q = e0 - ((jv << 4) + (jv << 3)) - 24;
    let j = jv + 4;
    let (f0, f1, f2, f3, f4, f5, mut f6);
    let (q0, q1, q2, q3, q4);
    if jx == 1 {
        f5 = tab(j);
        f4 = tab(j - 1);
        f3 = tab(j - 2);
        f2 = tab(j - 3);
        f1 = tab(j - 4);
        f0 = tab(j - 5);
        f6 = 0.0;
        q0 = x0 * f1 + x1 * f0;
        q1 = x0 * f2 + x1 * f1;
        q2 = x0 * f3 + x1 * f2;
        q3 = x0 * f4 + x1 * f3;
        q4 = x0 * f5 + x1 * f4;
    } else {
        f6 = tab(j);
        f5 = tab(j - 1);
        f4 = tab(j - 2);
        f3 = tab(j - 3);
        f2 = tab(j - 4);
        f1 = tab(j - 5);
        f0 = tab(j - 6);
        q0 = x0 * f2 + x1 * f1 + x2 * f0;
        q1 = x0 * f3 + x1 * f2 + x2 * f1;
        q2 = x0 * f4 + x1 * f3 + x2 * f2;
        q3 = x0 * f5 + x1 * f4 + x2 * f3;
        q4 = x0 * f6 + x1 * f5 + x2 * f4;
    }
    let step = |z: f64| -> (f64, i32) {
        let fw = (TWO_POW_N24 * z) as i32 as f64;
        (fw, (z - TWO_POW_24 * fw) as i32)
    };
    let mut z = q4;
    let (mut fw, mut iq0) = step(z);
    z = q3 + fw;
    let (fw1, mut iq1) = step(z);
    z = q2 + fw1;
    let (fw2, mut iq2) = step(z);
    z = q1 + fw2;
    let (fw3, mut iq3) = step(z);
    z = q0 + fw3;
    let two_pow_q = t.two_pow[(q - MIN_DOUBLE_EXPONENT) as usize];
    z = (z * two_pow_q) % 8.0;
    z -= z as i32 as f64;
    let mut ih;
    if q > 0 {
        iq3 &= 0xFFFFFF >> q;
        ih = iq3 >> (23 - q);
    } else if q == 0 {
        ih = iq3 >> 23;
    } else if z >= 0.5 {
        ih = 2;
    } else {
        ih = 0;
    }
    if ih > 0 {
        let carry;
        if iq0 != 0 {
            carry = 1;
            iq0 = 0x1000000 - iq0;
            iq1 = 0x0FFFFFF - iq1;
            iq2 = 0x0FFFFFF - iq2;
            iq3 = 0x0FFFFFF - iq3;
        } else if iq1 != 0 {
            carry = 1;
            iq1 = 0x1000000 - iq1;
            iq2 = 0x0FFFFFF - iq2;
            iq3 = 0x0FFFFFF - iq3;
        } else if iq2 != 0 {
            carry = 1;
            iq2 = 0x1000000 - iq2;
            iq3 = 0x0FFFFFF - iq3;
        } else if iq3 != 0 {
            carry = 1;
            iq3 = 0x1000000 - iq3;
        } else {
            carry = 0;
        }
        if q > 0 {
            match q {
                1 => iq3 &= 0x7FFFFF,
                2 => iq3 &= 0x3FFFFF,
                _ => {}
            }
        }
        if ih == 2 {
            z = 1.0 - z;
            if carry != 0 {
                z -= two_pow_q;
            }
        }
    }
    let iq4;
    if z == 0.0 {
        let q5 = if jx == 1 {
            f6 = ONE_OVER_TWOPI_TAB[(jv + 5) as usize];
            x0 * f6 + x1 * f5
        } else {
            let f7 = ONE_OVER_TWOPI_TAB[(jv + 5) as usize];
            x0 * f7 + x1 * f6 + x2 * f5
        };
        z = q5;
        let (a, b) = step(z);
        fw = a;
        iq0 = b;
        z = q4 + fw;
        let (a, b) = step(z);
        iq1 = b;
        z = q3 + a;
        let (a, b) = step(z);
        iq2 = b;
        z = q2 + a;
        let (a, b) = step(z);
        iq3 = b;
        z = q1 + a;
        let (a, mut b) = step(z);
        z = q0 + a;
        z = (z * two_pow_q) % 8.0;
        z -= z as i32 as f64;
        if q > 0 {
            b &= 0xFFFFFF >> q;
            ih = b >> (23 - q);
        } else if q == 0 {
            ih = b >> 23;
        } else if z >= 0.5 {
            ih = 2;
        } else {
            ih = 0;
        }
        if ih > 0 {
            if iq0 != 0 {
                iq0 = 0x1000000 - iq0;
                iq1 = 0x0FFFFFF - iq1;
                iq2 = 0x0FFFFFF - iq2;
                iq3 = 0x0FFFFFF - iq3;
                b = 0x0FFFFFF - b;
            } else if iq1 != 0 {
                iq1 = 0x1000000 - iq1;
                iq2 = 0x0FFFFFF - iq2;
                iq3 = 0x0FFFFFF - iq3;
                b = 0x0FFFFFF - b;
            } else if iq2 != 0 {
                iq2 = 0x1000000 - iq2;
                iq3 = 0x0FFFFFF - iq3;
                b = 0x0FFFFFF - b;
            } else if iq3 != 0 {
                iq3 = 0x1000000 - iq3;
                b = 0x0FFFFFF - b;
            } else if b != 0 {
                b = 0x1000000 - b;
            }
            if q > 0 {
                match q {
                    1 => b &= 0x7FFFFF,
                    2 => b &= 0x3FFFFF,
                    _ => {}
                }
            }
        }
        iq4 = b;
        fw = two_pow_q * TWO_POW_N24;
    } else {
        iq4 = (z / two_pow_q) as i32;
        fw = two_pow_q;
    }
    let q4 = fw * iq4 as f64;
    fw *= TWO_POW_N24;
    let q3 = fw * iq3 as f64;
    fw *= TWO_POW_N24;
    let q2 = fw * iq2 as f64;
    fw *= TWO_POW_N24;
    let q1 = fw * iq1 as f64;
    fw *= TWO_POW_N24;
    let q0 = fw * iq0 as f64;
    let mut fw = TWOPI_TAB0 * q4;
    fw += TWOPI_TAB0 * q3 + TWOPI_TAB1 * q4;
    fw += TWOPI_TAB0 * q2 + TWOPI_TAB1 * q3 + TWOPI_TAB2 * q4;
    fw += TWOPI_TAB0 * q1 + TWOPI_TAB1 * q2 + TWOPI_TAB2 * q3 + TWOPI_TAB3 * q4;
    fw += TWOPI_TAB0 * q0 + TWOPI_TAB1 * q1 + TWOPI_TAB2 * q2 + TWOPI_TAB3 * q3 + TWOPI_TAB4 * q4;
    if ih == 0 { fw } else { -fw }
}
