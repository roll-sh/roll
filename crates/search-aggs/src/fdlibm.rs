//! fdlibm 5.3 (`StrictMath`) functions the H3 port needs to build its lookup tables: `sin`,
//! `cos`, `tan`, `asin`, `atan` and `pow`, bit for bit with Java's `StrictMath` (which is
//! fdlibm by specification), and `expm1` (`Math.expm1` is `StrictMath.expm1`; geotile bounds
//! go through it). [`crate::jmath::strict_log`] is the same family.
//!
//! The trigonometric functions reduce arguments with `__ieee754_rem_pio2`'s exact paths for
//! `|x| <= 2^19 * pi/2` only (the Payne-Hanek path for larger arguments is not ported); larger
//! finite arguments return NaN. Table building never gets near that bound.
//!
//! Constants are given as in fdlibm's sources; `__HI` / `__LO` word surgery becomes bit
//! operations on `f64`.

// fdlibm's literals, verbatim.
#![allow(clippy::excessive_precision, clippy::unreadable_literal, clippy::approx_constant)]

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

#[inline]
fn with_lo(x: f64, l: u32) -> f64 {
    f64::from_bits((x.to_bits() & 0xffff_ffff_0000_0000) | l as u64)
}

#[inline]
fn from_words(h: i32, l: u32) -> f64 {
    f64::from_bits(((h as u32 as u64) << 32) | l as u64)
}

// ---- expm1 ----

const EXPM1_O_THRESHOLD: f64 = 7.09782712893383973096e+02;
const EXPM1_LN2_HI: f64 = 6.93147180369123816490e-01;
const EXPM1_LN2_LO: f64 = 1.90821492927058770002e-10;
const EXPM1_INVLN2: f64 = 1.44269504088896338700e+00;
const EXPM1_Q: [f64; 5] = [-3.33333333333331316428e-02, 1.58730158725481460165e-03, -7.93650757867487942473e-05, 4.00821782732936239552e-06, -2.01099218183624371326e-07];
/// `StrictMath.expm1` (fdlibm `s_expm1.c`): `e^x - 1`.
pub fn expm1(mut x: f64) -> f64 {
    let mut hx = hi(x) as u32;
    let negative = hx & 0x8000_0000 != 0;
    hx &= 0x7fff_ffff;
    // Huge and non-finite arguments.
    if hx >= 0x4043_687A {
        if hx >= 0x4086_2E42 {
            if hx >= 0x7ff0_0000 {
                if ((hx & 0xfffff) | lo(x)) != 0 {
                    return x + x;
                }
                return if negative { -1.0 } else { x };
            }
            if x > EXPM1_O_THRESHOLD {
                return HUGE * HUGE;
            }
        }
        if negative && x + TINY < 0.0 {
            return TINY - 1.0;
        }
    }
    // Argument reduction.
    let k: i32;
    let mut c = 0.0;
    if hx > 0x3fd6_2e42 {
        let (h, l);
        if hx < 0x3FF0_A2B2 {
            if !negative {
                h = x - EXPM1_LN2_HI;
                l = EXPM1_LN2_LO;
                k = 1;
            } else {
                h = x + EXPM1_LN2_HI;
                l = -EXPM1_LN2_LO;
                k = -1;
            }
        } else {
            k = (EXPM1_INVLN2 * x + if negative { -0.5 } else { 0.5 }) as i32;
            let t = k as f64;
            h = x - t * EXPM1_LN2_HI;
            l = t * EXPM1_LN2_LO;
        }
        x = h - l;
        c = (h - x) - l;
    } else if hx < 0x3c90_0000 {
        let t = HUGE + x;
        return x - (t - (HUGE + x));
    } else {
        k = 0;
    }
    // x is in the primary range.
    let q = &EXPM1_Q;
    let hfx = 0.5 * x;
    let hxs = x * hfx;
    let r1 = 1.0 + hxs * (q[0] + hxs * (q[1] + hxs * (q[2] + hxs * (q[3] + hxs * q[4]))));
    let t = 3.0 - r1 * hfx;
    let mut e = hxs * ((r1 - t) / (6.0 - x * t));
    if k == 0 {
        return x - (x * e - hxs);
    }
    e = x * (e - c) - c;
    e -= hxs;
    if k == -1 {
        return 0.5 * (x - e) - 0.5;
    }
    if k == 1 {
        return if x < -0.25 { -2.0 * (e - (x + 0.5)) } else { 1.0 + 2.0 * (x - e) };
    }
    let add_exp = |y: f64| with_hi(y, hi(y).wrapping_add(k << 20));
    if k <= -2 || k > 56 {
        let y = 1.0 - (e - x);
        return add_exp(y) - 1.0;
    }
    if k < 20 {
        let t = from_words(0x3ff0_0000 - (0x20_0000 >> k), 0);
        add_exp(t - (e - x))
    } else {
        let t = from_words((0x3ff - k) << 20, 0);
        let y = x - (e + t) + 1.0;
        add_exp(y)
    }
}

// ---- kernels ----

const S1: f64 = -1.66666666666666324348e-01;
const S2: f64 = 8.33333333332248946124e-03;
const S3: f64 = -1.98412698298579493134e-04;
const S4: f64 = 2.75573137070700676789e-06;
const S5: f64 = -2.50507602534068634195e-08;
const S6: f64 = 1.58969099521155010221e-10;

/// `__kernel_sin(x, y, iy)`.
fn kernel_sin(x: f64, y: f64, iy: i32) -> f64 {
    let ix = hi(x) & 0x7fffffff;
    if ix < 0x3e400000 && x as i32 == 0 {
        return x;
    }
    let z = x * x;
    let v = z * x;
    let r = S2 + z * (S3 + z * (S4 + z * (S5 + z * S6)));
    if iy == 0 { x + v * (S1 + z * r) } else { x - ((z * (0.5 * y - v * r) - y) - v * S1) }
}

const C1: f64 = 4.16666666666666019037e-02;
const C2: f64 = -1.38888888888741095749e-03;
const C3: f64 = 2.48015872894767294178e-05;
const C4: f64 = -2.75573143513906633035e-07;
const C5: f64 = 2.08757232129817482790e-09;
const C6: f64 = -1.13596475577881948265e-11;

/// `__kernel_cos(x, y)`.
fn kernel_cos(x: f64, y: f64) -> f64 {
    let ix = hi(x) & 0x7fffffff;
    if ix < 0x3e400000 && x as i32 == 0 {
        return 1.0;
    }
    let z = x * x;
    let r = z * (C1 + z * (C2 + z * (C3 + z * (C4 + z * (C5 + z * C6)))));
    if ix < 0x3FD33333 {
        1.0 - (0.5 * z - (z * r - x * y))
    } else {
        let qx = if ix > 0x3fe90000 { 0.28125 } else { from_words(ix - 0x00200000, 0) };
        let hz = 0.5 * z - qx;
        let a = 1.0 - qx;
        a - (hz - (z * r - x * y))
    }
}

const T: [f64; 13] = [
    3.33333333333334091986e-01,
    1.33333333333201242699e-01,
    5.39682539762260521377e-02,
    2.18694882948595424599e-02,
    8.86323982359930005737e-03,
    3.59207910759131235356e-03,
    1.45620945432529025516e-03,
    5.88041240820264096874e-04,
    2.46463134818469906812e-04,
    7.81794442939557092300e-05,
    7.14072491382608190305e-05,
    -1.85586374855275456654e-05,
    2.59073051863633712884e-05,
];
const PIO4: f64 = 7.85398163397448278999e-01;
const PIO4LO: f64 = 3.06161699786838301793e-17;

/// `__kernel_tan(x, y, iy)` (fdlibm 5.3, "k_tan.c 1.5 04/04/22").
fn kernel_tan(mut x: f64, mut y: f64, iy: i32) -> f64 {
    let hx = hi(x);
    let ix = hx & 0x7fffffff;
    if ix < 0x3e300000 && x as i32 == 0 {
        if ((ix as u32 | lo(x)) | (iy + 1) as u32) == 0 {
            return 1.0 / x.abs();
        } else if iy == 1 {
            return x;
        } else {
            let w = x + y;
            let z = with_lo(w, 0);
            let v = y - (z - x);
            let a = -1.0 / w;
            let t = with_lo(a, 0);
            let s = 1.0 + t * z;
            return t + a * (s + t * v);
        }
    }
    if ix >= 0x3FE59428 {
        if hx < 0 {
            x = -x;
            y = -y;
        }
        let z = PIO4 - x;
        let w = PIO4LO - y;
        x = z + w;
        y = 0.0;
    }
    let z = x * x;
    let w = z * z;
    let mut r = T[1] + w * (T[3] + w * (T[5] + w * (T[7] + w * (T[9] + w * T[11]))));
    let mut v = z * (T[2] + w * (T[4] + w * (T[6] + w * (T[8] + w * (T[10] + w * T[12])))));
    let s = z * x;
    r = y + z * (s * (r + v) + y);
    r += T[0] * s;
    let w = x + r;
    if ix >= 0x3FE59428 {
        v = iy as f64;
        return (1 - ((hx >> 30) & 2)) as f64 * (v - 2.0 * (x - (w * w / (w + v) - r)));
    }
    if iy == 1 {
        w
    } else {
        let z = with_lo(w, 0);
        let v = r - (z - x);
        let a = -1.0 / w;
        let t = with_lo(a, 0);
        let s = 1.0 + t * z;
        t + a * (s + t * v)
    }
}

// ---- argument reduction ----

const NPIO2_HW: [i32; 32] = [
    0x3FF921FB, 0x400921FB, 0x4012D97C, 0x401921FB, 0x401F6A7A, 0x4022D97C, 0x4025FDBB, 0x402921FB, 0x402C463A, 0x402F6A7A, 0x4031475C,
    0x4032D97C, 0x40346B9C, 0x4035FDBB, 0x40378FDB, 0x403921FB, 0x403AB41B, 0x403C463A, 0x403DD85A, 0x403F6A7A, 0x40407E4C, 0x4041475C,
    0x4042106C, 0x4042D97C, 0x4043A28C, 0x40446B9C, 0x404534AC, 0x4045FDBB, 0x4046C6CB, 0x40478FDB, 0x404858EB, 0x404921FB,
];
const INVPIO2: f64 = 6.36619772367581382433e-01;
const PIO2_1: f64 = 1.57079632673412561417e+00;
const PIO2_1T: f64 = 6.07710050650619224932e-11;
const PIO2_2: f64 = 6.07710050630396597660e-11;
const PIO2_2T: f64 = 2.02226624879595063154e-21;
const PIO2_3: f64 = 2.02226624871116645580e-21;
const PIO2_3T: f64 = 8.47842766036889956997e-32;

/// `__ieee754_rem_pio2(x, y)` for `|x| <= 2^19 * pi/2` (`None` beyond).
fn rem_pio2(x: f64) -> Option<(i32, f64, f64)> {
    let hx = hi(x);
    let ix = hx & 0x7fffffff;
    if ix <= 0x3fe921fb {
        return Some((0, x, 0.0));
    }
    if ix < 0x4002d97c {
        return Some(if hx > 0 {
            let mut z = x - PIO2_1;
            if ix != 0x3ff921fb {
                let y0 = z - PIO2_1T;
                (1, y0, (z - y0) - PIO2_1T)
            } else {
                z -= PIO2_2;
                let y0 = z - PIO2_2T;
                (1, y0, (z - y0) - PIO2_2T)
            }
        } else {
            let mut z = x + PIO2_1;
            if ix != 0x3ff921fb {
                let y0 = z + PIO2_1T;
                (-1, y0, (z - y0) + PIO2_1T)
            } else {
                z += PIO2_2;
                let y0 = z + PIO2_2T;
                (-1, y0, (z - y0) + PIO2_2T)
            }
        });
    }
    if ix <= 0x413921fb {
        let t = x.abs();
        let n = (t * INVPIO2 + 0.5) as i32;
        let fn_ = n as f64;
        let mut r = t - fn_ * PIO2_1;
        let mut w = fn_ * PIO2_1T;
        let mut y0;
        if n < 32 && ix != NPIO2_HW[(n - 1) as usize] {
            y0 = r - w;
        } else {
            let j = ix >> 20;
            y0 = r - w;
            let mut i = j - ((hi(y0) >> 20) & 0x7ff);
            if i > 16 {
                let t = r;
                w = fn_ * PIO2_2;
                r = t - w;
                w = fn_ * PIO2_2T - ((t - r) - w);
                y0 = r - w;
                i = j - ((hi(y0) >> 20) & 0x7ff);
                if i > 49 {
                    let t = r;
                    w = fn_ * PIO2_3;
                    r = t - w;
                    w = fn_ * PIO2_3T - ((t - r) - w);
                    y0 = r - w;
                }
            }
        }
        let y1 = (r - y0) - w;
        return Some(if hx < 0 { (-n, -y0, -y1) } else { (n, y0, y1) });
    }
    None
}

/// `StrictMath.sin`.
pub fn sin(x: f64) -> f64 {
    let ix = hi(x) & 0x7fffffff;
    if ix <= 0x3fe921fb {
        return kernel_sin(x, 0.0, 0);
    }
    if ix >= 0x7ff00000 {
        return f64::NAN;
    }
    let Some((n, y0, y1)) = rem_pio2(x) else { return f64::NAN };
    match n & 3 {
        0 => kernel_sin(y0, y1, 1),
        1 => kernel_cos(y0, y1),
        2 => -kernel_sin(y0, y1, 1),
        _ => -kernel_cos(y0, y1),
    }
}

/// `StrictMath.cos`.
pub fn cos(x: f64) -> f64 {
    let ix = hi(x) & 0x7fffffff;
    if ix <= 0x3fe921fb {
        return kernel_cos(x, 0.0);
    }
    if ix >= 0x7ff00000 {
        return f64::NAN;
    }
    let Some((n, y0, y1)) = rem_pio2(x) else { return f64::NAN };
    match n & 3 {
        0 => kernel_cos(y0, y1),
        1 => -kernel_sin(y0, y1, 1),
        2 => -kernel_cos(y0, y1),
        _ => kernel_sin(y0, y1, 1),
    }
}

/// `StrictMath.tan`.
pub fn tan(x: f64) -> f64 {
    let ix = hi(x) & 0x7fffffff;
    if ix <= 0x3fe921fb {
        return kernel_tan(x, 0.0, 1);
    }
    if ix >= 0x7ff00000 {
        return f64::NAN;
    }
    let Some((n, y0, y1)) = rem_pio2(x) else { return f64::NAN };
    kernel_tan(y0, y1, 1 - ((n & 1) << 1))
}

// ---- asin ----

const PIO2_HI: f64 = 1.57079632679489655800e+00;
const PIO2_LO: f64 = 6.12323399573676603587e-17;
const PIO4_HI: f64 = 7.85398163397448278999e-01;
const PS0: f64 = 1.66666666666666657415e-01;
const PS1: f64 = -3.25565818622400915405e-01;
const PS2: f64 = 2.01212532134862925881e-01;
const PS3: f64 = -4.00555345006794114027e-02;
const PS4: f64 = 7.91534994289814532176e-04;
const PS5: f64 = 3.47933107596021167570e-05;
const QS1: f64 = -2.40339491173441421878e+00;
const QS2: f64 = 2.02094576023350569471e+00;
const QS3: f64 = -6.88283971605453293030e-01;
const QS4: f64 = 7.70381505559019352791e-02;

/// `StrictMath.asin`.
pub fn asin(x: f64) -> f64 {
    let hx = hi(x);
    let ix = hx & 0x7fffffff;
    if ix >= 0x3ff00000 {
        if ((ix - 0x3ff00000) as u32 | lo(x)) == 0 {
            return x * PIO2_HI + x * PIO2_LO;
        }
        return f64::NAN;
    } else if ix < 0x3fe00000 {
        if ix < 0x3e400000 {
            return x;
        }
        let t = x * x;
        let p = t * (PS0 + t * (PS1 + t * (PS2 + t * (PS3 + t * (PS4 + t * PS5)))));
        let q = 1.0 + t * (QS1 + t * (QS2 + t * (QS3 + t * QS4)));
        let w = p / q;
        return x + x * w;
    }
    let w = 1.0 - x.abs();
    let t = w * 0.5;
    let p = t * (PS0 + t * (PS1 + t * (PS2 + t * (PS3 + t * (PS4 + t * PS5)))));
    let q = 1.0 + t * (QS1 + t * (QS2 + t * (QS3 + t * QS4)));
    let s = t.sqrt();
    let t = if ix >= 0x3FEF3333 {
        let w = p / q;
        PIO2_HI - (2.0 * (s + s * w) - PIO2_LO)
    } else {
        let w = with_lo(s, 0);
        let c = (t - w * w) / (s + w);
        let r = p / q;
        let p = 2.0 * s * r - (PIO2_LO - 2.0 * c);
        let q = PIO4_HI - 2.0 * w;
        PIO4_HI - (p - q)
    };
    if hx > 0 { t } else { -t }
}

// ---- atan ----

const ATANHI: [f64; 4] = [4.63647609000806093515e-01, 7.85398163397448278999e-01, 9.82793723247329054082e-01, 1.57079632679489655800e+00];
const ATANLO: [f64; 4] = [2.26987774529616870924e-17, 3.06161699786838301793e-17, 1.39033110312309984516e-17, 6.12323399573676603587e-17];
const AT: [f64; 11] = [
    3.33333333333329318027e-01,
    -1.99999999998764832476e-01,
    1.42857142725034663711e-01,
    -1.11111104054623557880e-01,
    9.09088713343650656196e-02,
    -7.69187620504482999495e-02,
    6.66107313738753120669e-02,
    -5.83357013379057348645e-02,
    4.97687799461593236017e-02,
    -3.65315727442169155270e-02,
    1.62858201153657823623e-02,
];

/// `StrictMath.atan`.
pub fn atan(mut x: f64) -> f64 {
    let hx = hi(x);
    let ix = hx & 0x7fffffff;
    let id: i32;
    if ix >= 0x44100000 {
        if ix > 0x7ff00000 || (ix == 0x7ff00000 && lo(x) != 0) {
            return x + x;
        }
        return if hx > 0 { ATANHI[3] + ATANLO[3] } else { -ATANHI[3] - ATANLO[3] };
    }
    if ix < 0x3fdc0000 {
        if ix < 0x3e200000 {
            return x;
        }
        id = -1;
    } else {
        x = x.abs();
        if ix < 0x3ff30000 {
            if ix < 0x3fe60000 {
                id = 0;
                x = (2.0 * x - 1.0) / (2.0 + x);
            } else {
                id = 1;
                x = (x - 1.0) / (x + 1.0);
            }
        } else if ix < 0x40038000 {
            id = 2;
            x = (x - 1.5) / (1.0 + 1.5 * x);
        } else {
            id = 3;
            x = -1.0 / x;
        }
    }
    let z = x * x;
    let w = z * z;
    let s1 = z * (AT[0] + w * (AT[2] + w * (AT[4] + w * (AT[6] + w * (AT[8] + w * AT[10])))));
    let s2 = w * (AT[1] + w * (AT[3] + w * (AT[5] + w * (AT[7] + w * AT[9]))));
    if id < 0 {
        x - x * (s1 + s2)
    } else {
        let id = id as usize;
        let z = ATANHI[id] - ((x * (s1 + s2) - ATANLO[id]) - x);
        if hx < 0 { -z } else { z }
    }
}

// ---- pow ----

const BP: [f64; 2] = [1.0, 1.5];
const DP_H: [f64; 2] = [0.0, 5.84962487220764160156e-01];
const DP_L: [f64; 2] = [0.0, 1.35003920212974897128e-08];
const TWO53: f64 = 9007199254740992.0;
const HUGE: f64 = 1.0e300;
const TINY: f64 = 1.0e-300;
const L1: f64 = 5.99999999999994648725e-01;
const L2: f64 = 4.28571428578550184252e-01;
const L3: f64 = 3.33333329818377432918e-01;
const L4: f64 = 2.72728123808534006489e-01;
const L5: f64 = 2.30660745775561754067e-01;
const L6: f64 = 2.06975017800338417784e-01;
const P1: f64 = 1.66666666666666019037e-01;
const P2: f64 = -2.77777777770155933842e-03;
const P3: f64 = 6.61375632143793436117e-05;
const P4: f64 = -1.65339022054652515390e-06;
const P5: f64 = 4.13813679705723846039e-08;
const LG2: f64 = 6.93147180559945286227e-01;
const LG2_H: f64 = 6.93147182464599609375e-01;
const LG2_L: f64 = -1.90465429995776804525e-09;
const OVT: f64 = 8.0085662595372944372e-17;
const CP: f64 = 9.61796693925975554329e-01;
const CP_H: f64 = 9.61796700954437255859e-01;
const CP_L: f64 = -7.02846165095275826516e-09;
const IVLN2: f64 = 1.44269504088896338700e+00;
const IVLN2_H: f64 = 1.44269502162933349609e+00;
const IVLN2_L: f64 = 1.92596299112661746887e-08;

/// fdlibm's `scalbn`.
fn scalbn(mut x: f64, n: i32) -> f64 {
    const TWO54: f64 = 1.80143985094819840000e+16;
    const TWOM54: f64 = 5.55111512312578270212e-17;
    let mut hx = hi(x);
    let lx = lo(x);
    let mut k = (hx & 0x7ff00000) >> 20;
    if k == 0 {
        if (lx | (hx & 0x7fffffff) as u32) == 0 {
            return x;
        }
        x *= TWO54;
        hx = hi(x);
        k = ((hx & 0x7ff00000) >> 20) - 54;
        if n < -50000 {
            return TINY * x;
        }
    }
    if k == 0x7ff {
        return x + x;
    }
    k += n;
    if k > 0x7fe {
        return HUGE * HUGE.copysign(x);
    }
    if k > 0 {
        return with_hi(x, (hx & 0x800fffffu32 as i32) | (k << 20));
    }
    if k <= -54 {
        return if n > 50000 { HUGE * HUGE.copysign(x) } else { TINY * TINY.copysign(x) };
    }
    k += 54;
    with_hi(x, (hx & 0x800fffffu32 as i32) | (k << 20)) * TWOM54
}

/// `StrictMath.pow`.
pub fn pow(x: f64, y: f64) -> f64 {
    let hx = hi(x);
    let lx = lo(x);
    let hy = hi(y);
    let ly = lo(y);
    let mut ix = hx & 0x7fffffff;
    let iy = hy & 0x7fffffff;
    if (iy as u32 | ly) == 0 {
        return 1.0;
    }
    if ix > 0x7ff00000 || (ix == 0x7ff00000 && lx != 0) || iy > 0x7ff00000 || (iy == 0x7ff00000 && ly != 0) {
        return x + y;
    }
    let mut yisint = 0;
    if hx < 0 {
        if iy >= 0x43400000 {
            yisint = 2;
        } else if iy >= 0x3ff00000 {
            let k = (iy >> 20) - 0x3ff;
            if k > 20 {
                let j = ly >> (52 - k);
                if (j << (52 - k)) == ly {
                    yisint = 2 - (j & 1) as i32;
                }
            } else if ly == 0 {
                let j = iy >> (20 - k);
                if (j << (20 - k)) == iy {
                    yisint = 2 - (j & 1);
                }
            }
        }
    }
    if ly == 0 {
        if iy == 0x7ff00000 {
            if ((ix - 0x3ff00000) as u32 | lx) == 0 {
                return f64::NAN;
            } else if ix >= 0x3ff00000 {
                return if hy >= 0 { y } else { 0.0 };
            } else {
                return if hy < 0 { -y } else { 0.0 };
            }
        }
        if iy == 0x3ff00000 {
            return if hy < 0 { 1.0 / x } else { x };
        }
        if hy == 0x40000000 {
            return x * x;
        }
        if hy == 0x3fe00000 && hx >= 0 {
            return x.sqrt();
        }
    }
    let mut ax = x.abs();
    if lx == 0 && (ix == 0x7ff00000 || ix == 0 || ix == 0x3ff00000) {
        let mut z = ax;
        if hy < 0 {
            z = 1.0 / z;
        }
        if hx < 0 {
            if ((ix - 0x3ff00000) | yisint) == 0 {
                z = f64::NAN;
            } else if yisint == 1 {
                z = -z;
            }
        }
        return z;
    }
    let mut n = (hx >> 31) + 1;
    if (n | yisint) == 0 {
        return f64::NAN;
    }
    let mut s = 1.0;
    if (n | (yisint - 1)) == 0 {
        s = -1.0;
    }
    let (t1, t2);
    if iy > 0x41e00000 {
        if iy > 0x43f00000 {
            if ix <= 0x3fefffff {
                return if hy < 0 { HUGE * HUGE } else { TINY * TINY };
            }
            if ix >= 0x3ff00000 {
                return if hy > 0 { HUGE * HUGE } else { TINY * TINY };
            }
        }
        if ix < 0x3fefffff {
            return if hy < 0 { s * HUGE * HUGE } else { s * TINY * TINY };
        }
        if ix > 0x3ff00000 {
            return if hy > 0 { s * HUGE * HUGE } else { s * TINY * TINY };
        }
        let t = ax - 1.0;
        let w = (t * t) * (0.5 - t * (0.3333333333333333333333 - t * 0.25));
        let u = IVLN2_H * t;
        let v = t * IVLN2_L - w * IVLN2;
        let tt1 = with_lo(u + v, 0);
        t1 = tt1;
        t2 = v - (tt1 - u);
    } else {
        n = 0;
        if ix < 0x00100000 {
            ax *= TWO53;
            n -= 53;
            ix = hi(ax);
        }
        n += (ix >> 20) - 0x3ff;
        let j = ix & 0x000fffff;
        ix = j | 0x3ff00000;
        let k: usize;
        if j <= 0x3988E {
            k = 0;
        } else if j < 0xBB67A {
            k = 1;
        } else {
            k = 0;
            n += 1;
            ix -= 0x00100000;
        }
        ax = with_hi(ax, ix);
        let u = ax - BP[k];
        let v = 1.0 / (ax + BP[k]);
        let ss = u * v;
        let s_h = with_lo(ss, 0);
        let t_h = from_words(((ix >> 1) | 0x20000000) + 0x00080000 + ((k as i32) << 18), 0);
        let t_l = ax - (t_h - BP[k]);
        let s_l = v * ((u - s_h * t_h) - s_h * t_l);
        let mut s2 = ss * ss;
        let mut r = s2 * s2 * (L1 + s2 * (L2 + s2 * (L3 + s2 * (L4 + s2 * (L5 + s2 * L6)))));
        r += s_l * (s_h + ss);
        s2 = s_h * s_h;
        let t_h = with_lo(3.0 + s2 + r, 0);
        let t_l = r - ((t_h - 3.0) - s2);
        let u = s_h * t_h;
        let v = s_l * t_h + t_l * ss;
        let p_h = with_lo(u + v, 0);
        let p_l = v - (p_h - u);
        let z_h = CP_H * p_h;
        let z_l = CP_L * p_h + p_l * CP + DP_L[k];
        let t = n as f64;
        let tt1 = with_lo(((z_h + z_l) + DP_H[k]) + t, 0);
        t1 = tt1;
        t2 = z_l - (((tt1 - t) - DP_H[k]) - z_h);
    }
    let y1 = with_lo(y, 0);
    let p_l = (y - y1) * t1 + y * t2;
    let mut p_h = y1 * t1;
    let mut z = p_l + p_h;
    let j = hi(z);
    let i = lo(z) as i32;
    if j >= 0x40900000 {
        if ((j - 0x40900000) | i) != 0 || p_l + OVT > z - p_h {
            return s * HUGE * HUGE;
        }
    } else if (j & 0x7fffffff) >= 0x4090cc00 && ((j.wrapping_sub(0xc090cc00u32 as i32) | i) != 0 || p_l <= z - p_h) {
        return s * TINY * TINY;
    }
    let i = j & 0x7fffffff;
    let mut k = (i >> 20) - 0x3ff;
    let mut n = 0;
    if i > 0x3fe00000 {
        n = j + (0x00100000 >> (k + 1));
        k = ((n & 0x7fffffff) >> 20) - 0x3ff;
        let t = from_words(n & !(0x000fffff >> k), 0);
        n = ((n & 0x000fffff) | 0x00100000) >> (20 - k);
        if j < 0 {
            n = -n;
        }
        p_h -= t;
    }
    let t = with_lo(p_l + p_h, 0);
    let u = t * LG2_H;
    let v = (p_l - (t - p_h)) * LG2 + t * LG2_L;
    z = u + v;
    let w = v - (z - u);
    let t = z * z;
    let t1 = z - t * (P1 + t * (P2 + t * (P3 + t * (P4 + t * P5))));
    let r = (z * t1) / (t1 - 2.0) - (w + z * w);
    z = 1.0 - (r - z);
    let j = hi(z) + (n << 20);
    if (j >> 20) <= 0 { s * scalbn(z, n) } else { s * with_hi(z, hi(z) + (n << 20)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_cases() {
        assert_eq!(sin(0.0), 0.0);
        assert_eq!(cos(0.0), 1.0);
        assert_eq!(tan(0.0), 0.0);
        assert_eq!(asin(1.0), std::f64::consts::FRAC_PI_2);
        assert_eq!(atan(f64::INFINITY), std::f64::consts::FRAC_PI_2);
        for i in -1074..=1023 {
            let want = if i >= -1022 { f64::from_bits(((i + 1023) as u64) << 52) } else { f64::from_bits(1u64 << (i + 1074)) };
            assert_eq!(pow(2.0, i as f64), want, "2^{i}");
        }
        assert_eq!(pow(9.0, 0.5), 3.0);
        assert!(pow(-8.0, 1.0 / 3.0).is_nan());
        assert_eq!(pow(-2.0, 3.0), -8.0);
    }
}
