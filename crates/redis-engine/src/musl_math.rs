//! musl's `log` and `pow` (Arm's optimized-routines, musl 1.2 `src/math/{log,pow}.c` and their
//! data tables; Copyright (c) 2018, Arm Limited, MIT), as the reference's modules get them on
//! Alpine aarch64 (`__FP_FAST_FMA`). Neither is correctly rounded, so the platform's or the
//! `libm` crate's (fdlibm) results differ in the last bit now and then: a Bloom filter's
//! bits per entry at the default error rate 0.01, a t-digest's merge thresholds, `JSON.NUMPOWBY`.

#[inline]
const fn f(b: u64) -> f64 {
    f64::from_bits(b)
}

#[inline]
fn top16(x: f64) -> u32 {
    (x.to_bits() >> 48) as u32
}

#[inline]
fn top12(x: f64) -> u32 {
    (x.to_bits() >> 52) as u32
}

/// `__math_invalid`: `(x - x) / (x - x)`, a NaN (`x`'s own when it is one).
fn invalid(x: f64) -> f64 {
    if x.is_nan() { x } else { f64::NAN }
}

/// `log(x)`.
pub fn log(x: f64) -> f64 {
    const OFF: u64 = 0x3fe6000000000000;
    let mut ix = x.to_bits();
    let top = top16(x);
    let lo_b = (1.0f64 - f(0x3fb0000000000000)).to_bits(); // 1.0 - 0x1p-4
    let hi_b = (1.0f64 + f(0x3fb0900000000000)).to_bits(); // 1.0 + 0x1.09p-4
    if ix.wrapping_sub(lo_b) < hi_b - lo_b {
        // Close to 1.0.
        if ix == 1.0f64.to_bits() {
            return 0.0;
        }
        let b = &LOG_B;
        let r = x - 1.0;
        let r2 = r * r;
        let r3 = r * r2;
        let mut y = r3 * (b[1] + r * b[2] + r2 * b[3] + r3 * (b[4] + r * b[5] + r2 * b[6] + r3 * (b[7] + r * b[8] + r2 * b[9] + r3 * b[10])));
        let w = r * f(0x41a0000000000000); // 0x1p27
        let rhi = r + w - w;
        let rlo = r - rhi;
        let w = rhi * rhi * b[0];
        let hi = r + w;
        let mut lo = r - hi + w;
        lo += b[0] * rlo * (rhi + r);
        y += lo;
        y += hi;
        return y;
    }
    if top.wrapping_sub(0x0010) >= 0x7ff0 - 0x0010 {
        // x < 0x1p-1022, inf or nan.
        if ix << 1 == 0 {
            return f64::NEG_INFINITY;
        }
        if ix == f64::INFINITY.to_bits() {
            return x;
        }
        if top & 0x8000 != 0 || top & 0x7ff0 == 0x7ff0 {
            return invalid(x);
        }
        // Subnormal: normalize.
        ix = (x * f(0x4330000000000000)).to_bits(); // 0x1p52
        ix = ix.wrapping_sub(52u64 << 52);
    }
    let tmp = ix.wrapping_sub(OFF);
    let i = ((tmp >> (52 - 7)) % 128) as usize;
    let k = (tmp as i64) >> 52;
    let iz = ix.wrapping_sub(tmp & (0xfffu64 << 52));
    let (invc, logc) = LOG_T[i];
    let z = f64::from_bits(iz);
    let r = z.mul_add(invc, -1.0);
    let kd = k as f64;
    let w = kd * LOG_LN2HI + logc;
    let hi = w + r;
    let lo = w - hi + r + kd * LOG_LN2LO;
    let r2 = r * r;
    let a = &LOG_A;
    lo + r2 * a[0] + r * r2 * (a[1] + r * a[2] + r2 * (a[3] + r * a[4])) + hi
}

/// `log_inline`: log(x) as hi + tail with ~15 bits more precision; `ix` normalized.
#[inline]
fn pow_log(ix: u64) -> (f64, f64) {
    const OFF: u64 = 0x3fe6955500000000;
    let tmp = ix.wrapping_sub(OFF);
    let i = ((tmp >> (52 - 7)) % 128) as usize;
    let k = (tmp as i64) >> 52;
    let iz = ix.wrapping_sub(tmp & (0xfffu64 << 52));
    let z = f64::from_bits(iz);
    let kd = k as f64;
    let (invc, logc, logctail) = POW_T[i];
    let r = z.mul_add(invc, -1.0);
    let t1 = kd * POW_LN2HI + logc;
    let t2 = t1 + r;
    let lo1 = kd * POW_LN2LO + logctail;
    let lo2 = t1 - t2 + r;
    let a = &POW_A;
    let ar = a[0] * r;
    let ar2 = r * ar;
    let ar3 = r * ar2;
    let hi = t2 + ar2;
    let lo3 = ar.mul_add(r, -ar2);
    let lo4 = t2 - hi + ar2;
    let p = ar3 * (a[1] + r * a[2] + ar2 * (a[3] + r * a[4] + ar2 * (a[5] + r * a[6])));
    let lo = lo1 + lo2 + lo3 + lo4 + p;
    let y = hi + lo;
    (y, hi - y + lo)
}

const SIGN_BIAS: u64 = 0x800 << 7;

/// `specialcase`: scale * (1 + tmp) near overflow or underflow.
fn exp_special(tmp: f64, sbits: u64, ki: u64) -> f64 {
    if ki & 0x80000000 == 0 {
        // k > 0: the exponent of scale might have overflowed by <= 460.
        let scale = f64::from_bits(sbits.wrapping_sub(1009u64 << 52));
        return f(0x7f00000000000000) * (scale + scale * tmp); // 0x1p1009
    }
    let sbits = sbits.wrapping_add(1022u64 << 52);
    let scale = f64::from_bits(sbits);
    let mut y = scale + scale * tmp;
    if y.abs() < 1.0 {
        let one = if y < 0.0 { -1.0 } else { 1.0 };
        let lo = scale - y + scale * tmp;
        let hi = one + y;
        let lo = one - hi + y + lo;
        y = (hi + lo) - one;
        if y == 0.0 {
            y = f64::from_bits(sbits & 0x8000000000000000);
        }
    }
    f(0x0010000000000000) * y // 0x1p-1022
}

/// `exp_inline`: sign * exp(x + xtail).
fn pow_exp(x: f64, xtail: f64, sign_bias: u64) -> f64 {
    let mut abstop = top12(x) & 0x7ff;
    let tiny = top12(f(0x3c90000000000000)); // 0x1p-54
    if abstop.wrapping_sub(tiny) >= top12(512.0) - tiny {
        if abstop.wrapping_sub(tiny) >= 0x80000000 {
            let one = 1.0 + x;
            return if sign_bias != 0 { -one } else { one };
        }
        if abstop >= top12(1024.0) {
            let neg = sign_bias != 0;
            return if x.to_bits() >> 63 != 0 {
                if neg { -0.0 } else { 0.0 }
            } else if neg {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
        }
        abstop = 0;
    }
    let z = EXP_INVLN2N * x;
    let kd = z + EXP_SHIFT;
    let ki = kd.to_bits();
    let kd = kd - EXP_SHIFT;
    let mut r = x + kd * EXP_NEGLN2HIN + kd * EXP_NEGLN2LON;
    r += xtail;
    let idx = (2 * (ki % 128)) as usize;
    let top = ki.wrapping_add(sign_bias) << (52 - 7);
    let tail = f64::from_bits(EXP_T[idx]);
    let sbits = EXP_T[idx + 1].wrapping_add(top);
    let r2 = r * r;
    let c = &EXP_C;
    let tmp = tail + r + r2 * (c[0] + r * c[1]) + r2 * r2 * (c[2] + r * c[3]);
    if abstop == 0 {
        return exp_special(tmp, sbits, ki);
    }
    let scale = f64::from_bits(sbits);
    scale + scale * tmp
}

/// 0: not an integer, 1: odd, 2: even (`iy` a non-zero finite double's bits).
fn checkint(iy: u64) -> u32 {
    let e = (iy >> 52 & 0x7ff) as i32;
    if e < 0x3ff {
        return 0;
    }
    if e > 0x3ff + 52 {
        return 2;
    }
    if iy & ((1u64 << (0x3ff + 52 - e)) - 1) != 0 {
        return 0;
    }
    if iy & (1u64 << (0x3ff + 52 - e)) != 0 {
        return 1;
    }
    2
}

fn zeroinfnan(i: u64) -> bool {
    i.wrapping_mul(2).wrapping_sub(1) >= 2 * f64::INFINITY.to_bits() - 1
}

/// `pow(x, y)`.
pub fn pow(x: f64, y: f64) -> f64 {
    let mut sign_bias = 0;
    let mut ix = x.to_bits();
    let iy = y.to_bits();
    let mut topx = top12(x);
    let topy = top12(y);
    if topx.wrapping_sub(0x001) >= 0x7ff - 0x001 || (topy & 0x7ff).wrapping_sub(0x3be) >= 0x43e - 0x3be {
        if zeroinfnan(iy) {
            if iy << 1 == 0 {
                return 1.0;
            }
            if ix == 1.0f64.to_bits() {
                return 1.0;
            }
            if ix << 1 > f64::INFINITY.to_bits() << 1 || iy << 1 > f64::INFINITY.to_bits() << 1 {
                return x + y;
            }
            if ix << 1 == 1.0f64.to_bits() << 1 {
                return 1.0;
            }
            if (ix << 1 < 1.0f64.to_bits() << 1) == (iy >> 63 == 0) {
                return 0.0;
            }
            return y * y;
        }
        if zeroinfnan(ix) {
            let mut x2 = x * x;
            if ix >> 63 != 0 && checkint(iy) == 1 {
                x2 = -x2;
            }
            return if iy >> 63 != 0 { 1.0 / x2 } else { x2 };
        }
        // x and y are non-zero finite.
        if ix >> 63 != 0 {
            match checkint(iy) {
                0 => return invalid(x),
                1 => sign_bias = SIGN_BIAS,
                _ => {}
            }
            ix &= 0x7fffffffffffffff;
            topx &= 0x7ff;
        }
        if (topy & 0x7ff).wrapping_sub(0x3be) >= 0x43e - 0x3be {
            if ix == 1.0f64.to_bits() {
                return 1.0;
            }
            if (topy & 0x7ff) < 0x3be {
                return if ix > 1.0f64.to_bits() { 1.0 + y } else { 1.0 - y };
            }
            return if (ix > 1.0f64.to_bits()) == (topy < 0x800) { f64::INFINITY } else { 0.0 };
        }
        if topx == 0 {
            // Subnormal x: normalize so the exponent becomes negative.
            ix = (x * f(0x4330000000000000)).to_bits();
            ix &= 0x7fffffffffffffff;
            ix = ix.wrapping_sub(52u64 << 52);
        }
    }
    let (hi, lo) = pow_log(ix);
    let ehi = y * hi;
    let elo = y * lo + y.mul_add(hi, -ehi);
    pow_exp(ehi, elo, sign_bias)
}

// ---- Data (`__log_data`, `__pow_log_data`, `__exp_data`) ----

const LOG_LN2HI: f64 = f(0x3fe62e42fefa3800);
const LOG_LN2LO: f64 = f(0x3d2ef35793c76730);
const LOG_B: [f64; 11] = [f(0xbfe0000000000000), f(0x3fd5555555555577), f(0xbfcffffffffffdcb), f(0x3fc999999995dd0c), f(0xbfc55555556745a7), f(0x3fc24924a344de30), f(0xbfbfffffa4423d65), f(0x3fbc7184282ad6ca), f(0xbfb999eb43b068ff), f(0x3fb78182f7afd085), f(0xbfb5521375d145cd)];
const LOG_A: [f64; 5] = [f(0xbfe0000000000001), f(0x3fd555555551305b), f(0xbfcfffffffeb4590), f(0x3fc999b324f10111), f(0xbfc55575e506c89f)];
/// (invc, logc)
const LOG_T: [(f64, f64); 128] = [
    (f(0x3ff734f0c3e0de9f), f(0xbfd7cc7f79e69000)),
    (f(0x3ff713786a2ce91f), f(0xbfd76feec20d0000)),
    (f(0x3ff6f26008fab5a0), f(0xbfd713e31351e000)),
    (f(0x3ff6d1a61f138c7d), f(0xbfd6b85b38287800)),
    (f(0x3ff6b1490bc5b4d1), f(0xbfd65d5590807800)),
    (f(0x3ff69147332f0cba), f(0xbfd602d076180000)),
    (f(0x3ff6719f18224223), f(0xbfd5a8ca86909000)),
    (f(0x3ff6524f99a51ed9), f(0xbfd54f4356035000)),
    (f(0x3ff63356aa8f24c4), f(0xbfd4f637c36b4000)),
    (f(0x3ff614b36b9ddc14), f(0xbfd49da7fda85000)),
    (f(0x3ff5f66452c65c4c), f(0xbfd445923989a800)),
    (f(0x3ff5d867b5912c4f), f(0xbfd3edf439b0b800)),
    (f(0x3ff5babccb5b90de), f(0xbfd396ce448f7000)),
    (f(0x3ff59d61f2d91a78), f(0xbfd3401e17bda000)),
    (f(0x3ff5805612465687), f(0xbfd2e9e2ef468000)),
    (f(0x3ff56397cee76bd3), f(0xbfd2941b3830e000)),
    (f(0x3ff54725e2a77f93), f(0xbfd23ec58cda8800)),
    (f(0x3ff52aff42064583), f(0xbfd1e9e129279000)),
    (f(0x3ff50f22dbb2bddf), f(0xbfd1956d2b48f800)),
    (f(0x3ff4f38f4734ded7), f(0xbfd141679ab9f800)),
    (f(0x3ff4d843cfde2840), f(0xbfd0edd094ef9800)),
    (f(0x3ff4bd3ec078a3c8), f(0xbfd09aa518db1000)),
    (f(0x3ff4a27fc3e0258a), f(0xbfd047e65263b800)),
    (f(0x3ff4880524d48434), f(0xbfcfeb224586f000)),
    (f(0x3ff46dce1b192d0b), f(0xbfcf474a7517b000)),
    (f(0x3ff453d9d3391854), f(0xbfcea4443d103000)),
    (f(0x3ff43a2744b4845a), f(0xbfce020d44e9b000)),
    (f(0x3ff420b54115f8fb), f(0xbfcd60a22977f000)),
    (f(0x3ff40782da3ef4b1), f(0xbfccc00104959000)),
    (f(0x3ff3ee8f5d57fe8f), f(0xbfcc202956891000)),
    (f(0x3ff3d5d9a00b4ce9), f(0xbfcb81178d811000)),
    (f(0x3ff3bd60c010c12b), f(0xbfcae2c9ccd3d000)),
    (f(0x3ff3a5242b75dab8), f(0xbfca45402e129000)),
    (f(0x3ff38d22cd9fd002), f(0xbfc9a877681df000)),
    (f(0x3ff3755bc5847a1c), f(0xbfc90c6d69483000)),
    (f(0x3ff35dce49ad36e2), f(0xbfc87120a645c000)),
    (f(0x3ff34679984dd440), f(0xbfc7d68fb4143000)),
    (f(0x3ff32f5cceffcb24), f(0xbfc73cb83c627000)),
    (f(0x3ff3187775a10d49), f(0xbfc6a39a9b376000)),
    (f(0x3ff301c8373e3990), f(0xbfc60b3154b7a000)),
    (f(0x3ff2eb4ebb95f841), f(0xbfc5737d76243000)),
    (f(0x3ff2d50a0219a9d1), f(0xbfc4dc7b8fc23000)),
    (f(0x3ff2bef9a8b7fd2a), f(0xbfc4462c51d20000)),
    (f(0x3ff2a91c7a0c1bab), f(0xbfc3b08abc830000)),
    (f(0x3ff293726014b530), f(0xbfc31b996b490000)),
    (f(0x3ff27dfa5757a1f5), f(0xbfc2875490a44000)),
    (f(0x3ff268b39b1d3bbf), f(0xbfc1f3b9f879a000)),
    (f(0x3ff2539d838ff5bd), f(0xbfc160c8252ca000)),
    (f(0x3ff23eb7aac9083b), f(0xbfc0ce7f57f72000)),
    (f(0x3ff22a012ba940b6), f(0xbfc03cdc49fea000)),
    (f(0x3ff2157996cc4132), f(0xbfbf57bdbc4b8000)),
    (f(0x3ff201201dd2fc9b), f(0xbfbe370896404000)),
    (f(0x3ff1ecf4494d480b), f(0xbfbd17983ef94000)),
    (f(0x3ff1d8f5528f6569), f(0xbfbbf9674ed8a000)),
    (f(0x3ff1c52311577e7c), f(0xbfbadc79202f6000)),
    (f(0x3ff1b17c74cb26e9), f(0xbfb9c0c3e7288000)),
    (f(0x3ff19e010c2c1ab6), f(0xbfb8a646b372c000)),
    (f(0x3ff18ab07bb670bd), f(0xbfb78d01b3ac0000)),
    (f(0x3ff1778a25efbcb6), f(0xbfb674f145380000)),
    (f(0x3ff1648d354c31da), f(0xbfb55e0e6d878000)),
    (f(0x3ff151b990275fdd), f(0xbfb4485cdea1e000)),
    (f(0x3ff13f0ea432d24c), f(0xbfb333d94d6aa000)),
    (f(0x3ff12c8b7210f9da), f(0xbfb22079f8c56000)),
    (f(0x3ff11a3028ecb531), f(0xbfb10e4698622000)),
    (f(0x3ff107fbda8434af), f(0xbfaffa6c6ad20000)),
    (f(0x3ff0f5ee0f4e6bb3), f(0xbfadda8d4a774000)),
    (f(0x3ff0e4065d2a9fce), f(0xbfabbcece4850000)),
    (f(0x3ff0d244632ca521), f(0xbfa9a1894012c000)),
    (f(0x3ff0c0a77ce2981a), f(0xbfa788583302c000)),
    (f(0x3ff0af2f83c636d1), f(0xbfa5715e67d68000)),
    (f(0x3ff09ddb98a01339), f(0xbfa35c8a49658000)),
    (f(0x3ff08cabaf52e7df), f(0xbfa149e364154000)),
    (f(0x3ff07b9f2f4e28fb), f(0xbf9e72c082eb8000)),
    (f(0x3ff06ab58c358f19), f(0xbf9a55f152528000)),
    (f(0x3ff059eea5ecf92c), f(0xbf963d62cf818000)),
    (f(0x3ff04949cdd12c90), f(0xbf9228fb8caa0000)),
    (f(0x3ff038c6c6f0ada9), f(0xbf8c317b20f90000)),
    (f(0x3ff02865137932a9), f(0xbf8419355daa0000)),
    (f(0x3ff0182427ea7348), f(0xbf781203c2ec0000)),
    (f(0x3ff008040614b195), f(0xbf60040979240000)),
    (f(0x3fefe01ff726fa1a), f(0x3f6feff384900000)),
    (f(0x3fefa11cc261ea74), f(0x3f87dc41353d0000)),
    (f(0x3fef6310b081992e), f(0x3f93cea3c4c28000)),
    (f(0x3fef25f63ceeadcd), f(0x3f9b9fc114890000)),
    (f(0x3feee9c8039113e7), f(0x3fa1b0d8ce110000)),
    (f(0x3feeae8078cbb1ab), f(0x3fa58a5bd001c000)),
    (f(0x3fee741aa29d0c9b), f(0x3fa95c8340d88000)),
    (f(0x3fee3a91830a99b5), f(0x3fad276aef578000)),
    (f(0x3fee01e009609a56), f(0x3fb07598e598c000)),
    (f(0x3fedca01e577bb98), f(0x3fb253f5e30d2000)),
    (f(0x3fed92f20b7c9103), f(0x3fb42edd8b380000)),
    (f(0x3fed5cac66fb5cce), f(0x3fb606598757c000)),
    (f(0x3fed272caa5ede9d), f(0x3fb7da76356a0000)),
    (f(0x3fecf26e3e6b2ccd), f(0x3fb9ab434e1c6000)),
    (f(0x3fecbe6da2a77902), f(0x3fbb78c7bb0d6000)),
    (f(0x3fec8b266d37086d), f(0x3fbd431332e72000)),
    (f(0x3fec5894bd5d5804), f(0x3fbf0a3171de6000)),
    (f(0x3fec26b533bb9f8c), f(0x3fc067152b914000)),
    (f(0x3febf583eeece73f), f(0x3fc147858292b000)),
    (f(0x3febc4fd75db96c1), f(0x3fc2266ecdca3000)),
    (f(0x3feb951e0c864a28), f(0x3fc303d7a6c55000)),
    (f(0x3feb65e2c5ef3e2c), f(0x3fc3dfc33c331000)),
    (f(0x3feb374867c9888b), f(0x3fc4ba366b7a8000)),
    (f(0x3feb094b211d304a), f(0x3fc5933928d1f000)),
    (f(0x3feadbe885f2ef7e), f(0x3fc66acd2418f000)),
    (f(0x3feaaf1d31603da2), f(0x3fc740f8ec669000)),
    (f(0x3fea82e63fd358a7), f(0x3fc815c0f51af000)),
    (f(0x3fea5740ef09738b), f(0x3fc8e92954f68000)),
    (f(0x3fea2c2a90ab4b27), f(0x3fc9bb3602f84000)),
    (f(0x3fea01a01393f2d1), f(0x3fca8bed1c2c0000)),
    (f(0x3fe9d79f24db3c1b), f(0x3fcb5b515c01d000)),
    (f(0x3fe9ae2505c7b190), f(0x3fcc2967ccbcc000)),
    (f(0x3fe9852ef297ce2f), f(0x3fccf635d5486000)),
    (f(0x3fe95cbaeea44b75), f(0x3fcdc1bd3446c000)),
    (f(0x3fe934c69de74838), f(0x3fce8c01b8cfe000)),
    (f(0x3fe90d4f2f6752e6), f(0x3fcf5509c0179000)),
    (f(0x3fe8e6528effd79d), f(0x3fd00e6c121fb800)),
    (f(0x3fe8bfce9fcc007c), f(0x3fd071b80e93d000)),
    (f(0x3fe899c0dabec30e), f(0x3fd0d46b9e867000)),
    (f(0x3fe87427aa2317fb), f(0x3fd13687334bd000)),
    (f(0x3fe84f00acb39a08), f(0x3fd1980d67234800)),
    (f(0x3fe82a49e8653e55), f(0x3fd1f8ffe0cc8000)),
    (f(0x3fe8060195f40260), f(0x3fd2595fd7636800)),
    (f(0x3fe7e22563e0a329), f(0x3fd2b9300914a800)),
    (f(0x3fe7beb377dcb5ad), f(0x3fd3187210436000)),
    (f(0x3fe79baa679725c2), f(0x3fd377266dec1800)),
    (f(0x3fe77907f2170657), f(0x3fd3d54ffbaf3000)),
    (f(0x3fe756cadbd6130c), f(0x3fd432eee32fe000)),
];
const POW_LN2HI: f64 = f(0x3fe62e42fefa3800);
const POW_LN2LO: f64 = f(0x3d2ef35793c76730);
const POW_A: [f64; 7] = [f(0xbfe0000000000000), f(0xbfe5555555555560), f(0x3fe0000000000006), f(0x3fe999999959554e), f(0xbfe555555529a47a), f(0xbff2495b9b4845e9), f(0x3ff0002b8b263fc3)];
/// (invc, logc, logctail)
const POW_T: [(f64, f64, f64); 128] = [
    (f(0x3ff6a00000000000), f(0xbfd62c82f2b9c800), f(0x3cfab42428375680)),
    (f(0x3ff6800000000000), f(0xbfd5d1bdbf580800), f(0xbd1ca508d8e0f720)),
    (f(0x3ff6600000000000), f(0xbfd5767717455800), f(0xbd2362a4d5b6506d)),
    (f(0x3ff6400000000000), f(0xbfd51aad872df800), f(0xbce684e49eb067d5)),
    (f(0x3ff6200000000000), f(0xbfd4be5f95777800), f(0xbd041b6993293ee0)),
    (f(0x3ff6000000000000), f(0xbfd4618bc21c6000), f(0x3d13d82f484c84cc)),
    (f(0x3ff5e00000000000), f(0xbfd404308686a800), f(0x3cdc42f3ed820b3a)),
    (f(0x3ff5c00000000000), f(0xbfd3a64c55694800), f(0x3d20b1c686519460)),
    (f(0x3ff5a00000000000), f(0xbfd347dd9a988000), f(0x3d25594dd4c58092)),
    (f(0x3ff5800000000000), f(0xbfd2e8e2bae12000), f(0x3d267b1e99b72bd8)),
    (f(0x3ff5600000000000), f(0xbfd2895a13de8800), f(0x3d15ca14b6cfb03f)),
    (f(0x3ff5600000000000), f(0xbfd2895a13de8800), f(0x3d15ca14b6cfb03f)),
    (f(0x3ff5400000000000), f(0xbfd22941fbcf7800), f(0xbd165a242853da76)),
    (f(0x3ff5200000000000), f(0xbfd1c898c1699800), f(0xbd1fafbc68e75404)),
    (f(0x3ff5000000000000), f(0xbfd1675cababa800), f(0x3d1f1fc63382a8f0)),
    (f(0x3ff4e00000000000), f(0xbfd1058bf9ae4800), f(0xbd26a8c4fd055a66)),
    (f(0x3ff4c00000000000), f(0xbfd0a324e2739000), f(0xbd0c6bee7ef4030e)),
    (f(0x3ff4a00000000000), f(0xbfd0402594b4d000), f(0xbcf036b89ef42d7f)),
    (f(0x3ff4a00000000000), f(0xbfd0402594b4d000), f(0xbcf036b89ef42d7f)),
    (f(0x3ff4800000000000), f(0xbfcfb9186d5e4000), f(0x3d0d572aab993c87)),
    (f(0x3ff4600000000000), f(0xbfcef0adcbdc6000), f(0x3d2b26b79c86af24)),
    (f(0x3ff4400000000000), f(0xbfce27076e2af000), f(0xbd172f4f543fff10)),
    (f(0x3ff4200000000000), f(0xbfcd5c216b4fc000), f(0x3d21ba91bbca681b)),
    (f(0x3ff4000000000000), f(0xbfcc8ff7c79aa000), f(0x3d27794f689f8434)),
    (f(0x3ff4000000000000), f(0xbfcc8ff7c79aa000), f(0x3d27794f689f8434)),
    (f(0x3ff3e00000000000), f(0xbfcbc286742d9000), f(0x3d194eb0318bb78f)),
    (f(0x3ff3c00000000000), f(0xbfcaf3c94e80c000), f(0x3cba4e633fcd9066)),
    (f(0x3ff3a00000000000), f(0xbfca23bc1fe2b000), f(0xbd258c64dc46c1ea)),
    (f(0x3ff3a00000000000), f(0xbfca23bc1fe2b000), f(0xbd258c64dc46c1ea)),
    (f(0x3ff3800000000000), f(0xbfc9525a9cf45000), f(0xbd2ad1d904c1d4e3)),
    (f(0x3ff3600000000000), f(0xbfc87fa06520d000), f(0x3d2bbdbf7fdbfa09)),
    (f(0x3ff3400000000000), f(0xbfc7ab890210e000), f(0x3d2bdb9072534a58)),
    (f(0x3ff3400000000000), f(0xbfc7ab890210e000), f(0x3d2bdb9072534a58)),
    (f(0x3ff3200000000000), f(0xbfc6d60fe719d000), f(0xbd10e46aa3b2e266)),
    (f(0x3ff3000000000000), f(0xbfc5ff3070a79000), f(0xbd1e9e439f105039)),
    (f(0x3ff3000000000000), f(0xbfc5ff3070a79000), f(0xbd1e9e439f105039)),
    (f(0x3ff2e00000000000), f(0xbfc526e5e3a1b000), f(0xbd20de8b90075b8f)),
    (f(0x3ff2c00000000000), f(0xbfc44d2b6ccb8000), f(0x3d170cc16135783c)),
    (f(0x3ff2c00000000000), f(0xbfc44d2b6ccb8000), f(0x3d170cc16135783c)),
    (f(0x3ff2a00000000000), f(0xbfc371fc201e9000), f(0x3cf178864d27543a)),
    (f(0x3ff2800000000000), f(0xbfc29552f81ff000), f(0xbd248d301771c408)),
    (f(0x3ff2600000000000), f(0xbfc1b72ad52f6000), f(0xbd2e80a41811a396)),
    (f(0x3ff2600000000000), f(0xbfc1b72ad52f6000), f(0xbd2e80a41811a396)),
    (f(0x3ff2400000000000), f(0xbfc0d77e7cd09000), f(0x3d0a699688e85bf4)),
    (f(0x3ff2400000000000), f(0xbfc0d77e7cd09000), f(0x3d0a699688e85bf4)),
    (f(0x3ff2200000000000), f(0xbfbfec9131dbe000), f(0xbd2575545ca333f2)),
    (f(0x3ff2000000000000), f(0xbfbe27076e2b0000), f(0x3d2a342c2af0003c)),
    (f(0x3ff2000000000000), f(0xbfbe27076e2b0000), f(0x3d2a342c2af0003c)),
    (f(0x3ff1e00000000000), f(0xbfbc5e548f5bc000), f(0xbd1d0c57585fbe06)),
    (f(0x3ff1c00000000000), f(0xbfba926d3a4ae000), f(0x3d253935e85baac8)),
    (f(0x3ff1c00000000000), f(0xbfba926d3a4ae000), f(0x3d253935e85baac8)),
    (f(0x3ff1a00000000000), f(0xbfb8c345d631a000), f(0x3d137c294d2f5668)),
    (f(0x3ff1a00000000000), f(0xbfb8c345d631a000), f(0x3d137c294d2f5668)),
    (f(0x3ff1800000000000), f(0xbfb6f0d28ae56000), f(0xbd269737c93373da)),
    (f(0x3ff1600000000000), f(0xbfb51b073f062000), f(0x3d1f025b61c65e57)),
    (f(0x3ff1600000000000), f(0xbfb51b073f062000), f(0x3d1f025b61c65e57)),
    (f(0x3ff1400000000000), f(0xbfb341d7961be000), f(0x3d2c5edaccf913df)),
    (f(0x3ff1400000000000), f(0xbfb341d7961be000), f(0x3d2c5edaccf913df)),
    (f(0x3ff1200000000000), f(0xbfb16536eea38000), f(0x3d147c5e768fa309)),
    (f(0x3ff1000000000000), f(0xbfaf0a30c0118000), f(0x3d2d599e83368e91)),
    (f(0x3ff1000000000000), f(0xbfaf0a30c0118000), f(0x3d2d599e83368e91)),
    (f(0x3ff0e00000000000), f(0xbfab42dd71198000), f(0x3d1c827ae5d6704c)),
    (f(0x3ff0e00000000000), f(0xbfab42dd71198000), f(0x3d1c827ae5d6704c)),
    (f(0x3ff0c00000000000), f(0xbfa77458f632c000), f(0xbd2cfc4634f2a1ee)),
    (f(0x3ff0c00000000000), f(0xbfa77458f632c000), f(0xbd2cfc4634f2a1ee)),
    (f(0x3ff0a00000000000), f(0xbfa39e87b9fec000), f(0x3cf502b7f526feaa)),
    (f(0x3ff0a00000000000), f(0xbfa39e87b9fec000), f(0x3cf502b7f526feaa)),
    (f(0x3ff0800000000000), f(0xbf9f829b0e780000), f(0xbd2980267c7e09e4)),
    (f(0x3ff0800000000000), f(0xbf9f829b0e780000), f(0xbd2980267c7e09e4)),
    (f(0x3ff0600000000000), f(0xbf97b91b07d58000), f(0xbd288d5493faa639)),
    (f(0x3ff0400000000000), f(0xbf8fc0a8b0fc0000), f(0xbcdf1e7cf6d3a69c)),
    (f(0x3ff0400000000000), f(0xbf8fc0a8b0fc0000), f(0xbcdf1e7cf6d3a69c)),
    (f(0x3ff0200000000000), f(0xbf7fe02a6b100000), f(0xbd19e23f0dda40e4)),
    (f(0x3ff0200000000000), f(0xbf7fe02a6b100000), f(0xbd19e23f0dda40e4)),
    (f(0x3ff0000000000000), f(0x0000000000000000), f(0x0000000000000000)),
    (f(0x3ff0000000000000), f(0x0000000000000000), f(0x0000000000000000)),
    (f(0x3fefc00000000000), f(0x3f80101575890000), f(0xbd10c76b999d2be8)),
    (f(0x3fef800000000000), f(0x3f90205658938000), f(0xbd23dc5b06e2f7d2)),
    (f(0x3fef400000000000), f(0x3f98492528c90000), f(0xbd2aa0ba325a0c34)),
    (f(0x3fef000000000000), f(0x3fa0415d89e74000), f(0x3d0111c05cf1d753)),
    (f(0x3feec00000000000), f(0x3fa466aed42e0000), f(0xbd2c167375bdfd28)),
    (f(0x3fee800000000000), f(0x3fa894aa149fc000), f(0xbd197995d05a267d)),
    (f(0x3fee400000000000), f(0x3faccb73cdddc000), f(0xbd1a68f247d82807)),
    (f(0x3fee200000000000), f(0x3faeea31c006c000), f(0xbd0e113e4fc93b7b)),
    (f(0x3fede00000000000), f(0x3fb1973bd1466000), f(0xbd25325d560d9e9b)),
    (f(0x3feda00000000000), f(0x3fb3bdf5a7d1e000), f(0x3d2cc85ea5db4ed7)),
    (f(0x3fed600000000000), f(0x3fb5e95a4d97a000), f(0xbd2c69063c5d1d1e)),
    (f(0x3fed400000000000), f(0x3fb700d30aeac000), f(0x3cec1e8da99ded32)),
    (f(0x3fed000000000000), f(0x3fb9335e5d594000), f(0x3d23115c3abd47da)),
    (f(0x3fecc00000000000), f(0x3fbb6ac88dad6000), f(0xbd1390802bf768e5)),
    (f(0x3feca00000000000), f(0x3fbc885801bc4000), f(0x3d2646d1c65aacd3)),
    (f(0x3fec600000000000), f(0x3fbec739830a2000), f(0xbd2dc068afe645e0)),
    (f(0x3fec400000000000), f(0x3fbfe89139dbe000), f(0xbd2534d64fa10afd)),
    (f(0x3fec000000000000), f(0x3fc1178e8227e000), f(0x3d21ef78ce2d07f2)),
    (f(0x3febe00000000000), f(0x3fc1aa2b7e23f000), f(0x3d2ca78e44389934)),
    (f(0x3feba00000000000), f(0x3fc2d1610c868000), f(0x3d039d6ccb81b4a1)),
    (f(0x3feb800000000000), f(0x3fc365fcb0159000), f(0x3cc62fa8234b7289)),
    (f(0x3feb400000000000), f(0x3fc4913d8333b000), f(0x3d25837954fdb678)),
    (f(0x3feb200000000000), f(0x3fc527e5e4a1b000), f(0x3d2633e8e5697dc7)),
    (f(0x3feae00000000000), f(0x3fc6574ebe8c1000), f(0x3d19cf8b2c3c2e78)),
    (f(0x3feac00000000000), f(0x3fc6f0128b757000), f(0xbd25118de59c21e1)),
    (f(0x3feaa00000000000), f(0x3fc7898d85445000), f(0xbd1c661070914305)),
    (f(0x3fea600000000000), f(0x3fc8beafeb390000), f(0xbd073d54aae92cd1)),
    (f(0x3fea400000000000), f(0x3fc95a5adcf70000), f(0x3d07f22858a0ff6f)),
    (f(0x3fea000000000000), f(0x3fca93ed3c8ae000), f(0xbd28724350562169)),
    (f(0x3fe9e00000000000), f(0x3fcb31d8575bd000), f(0xbd0c358d4eace1aa)),
    (f(0x3fe9c00000000000), f(0x3fcbd087383be000), f(0xbd2d4bc4595412b6)),
    (f(0x3fe9a00000000000), f(0x3fcc6ffbc6f01000), f(0xbcf1ec72c5962bd2)),
    (f(0x3fe9600000000000), f(0x3fcdb13db0d49000), f(0xbd2aff2af715b035)),
    (f(0x3fe9400000000000), f(0x3fce530effe71000), f(0x3cc212276041f430)),
    (f(0x3fe9200000000000), f(0x3fcef5ade4dd0000), f(0xbcca211565bb8e11)),
    (f(0x3fe9000000000000), f(0x3fcf991c6cb3b000), f(0x3d1bcbecca0cdf30)),
    (f(0x3fe8c00000000000), f(0x3fd07138604d5800), f(0x3cf89cdb16ed4e91)),
    (f(0x3fe8a00000000000), f(0x3fd0c42d67616000), f(0x3d27188b163ceae9)),
    (f(0x3fe8800000000000), f(0x3fd1178e8227e800), f(0xbd2c210e63a5f01c)),
    (f(0x3fe8600000000000), f(0x3fd16b5ccbacf800), f(0x3d2b9acdf7a51681)),
    (f(0x3fe8400000000000), f(0x3fd1bf99635a6800), f(0x3d2ca6ed5147bdb7)),
    (f(0x3fe8200000000000), f(0x3fd214456d0eb800), f(0x3d0a87deba46baea)),
    (f(0x3fe7e00000000000), f(0x3fd2bef07cdc9000), f(0x3d2a9cfa4a5004f4)),
    (f(0x3fe7c00000000000), f(0x3fd314f1e1d36000), f(0xbd28e27ad3213cb8)),
    (f(0x3fe7a00000000000), f(0x3fd36b6776be1000), f(0x3d116ecdb0f177c8)),
    (f(0x3fe7800000000000), f(0x3fd3c25277333000), f(0x3d183b54b606bd5c)),
    (f(0x3fe7600000000000), f(0x3fd419b423d5e800), f(0x3d08e436ec90e09d)),
    (f(0x3fe7400000000000), f(0x3fd4718dc271c800), f(0xbd2f27ce0967d675)),
    (f(0x3fe7200000000000), f(0x3fd4c9e09e173000), f(0xbd2e20891b0ad8a4)),
    (f(0x3fe7000000000000), f(0x3fd522ae0738a000), f(0x3d2ebe708164c759)),
    (f(0x3fe6e00000000000), f(0x3fd57bf753c8d000), f(0x3d1fadedee5d40ef)),
    (f(0x3fe6c00000000000), f(0x3fd5d5bddf596000), f(0xbd0a0b2a08a465dc)),
];
const EXP_INVLN2N: f64 = f(0x40671547652b82fe);
const EXP_NEGLN2HIN: f64 = f(0xbf762e42fefa0000);
const EXP_NEGLN2LON: f64 = f(0xbd0cf79abc9e3b3a);
const EXP_SHIFT: f64 = f(0x4338000000000000);
const EXP_C: [f64; 4] = [f(0x3fdffffffffffdbd), f(0x3fc555555555543c), f(0x3fa55555cf172b91), f(0x3f81111167a4d017)];
const EXP_T: [u64; 256] = [
    0x0000000000000000, 0x3ff0000000000000, 0x3c9b3b4f1a88bf6e, 0x3feff63da9fb3335,
    0xbc7160139cd8dc5d, 0x3fefec9a3e778061, 0xbc905e7a108766d1, 0x3fefe315e86e7f85,
    0x3c8cd2523567f613, 0x3fefd9b0d3158574, 0xbc8bce8023f98efa, 0x3fefd06b29ddf6de,
    0x3c60f74e61e6c861, 0x3fefc74518759bc8, 0x3c90a3e45b33d399, 0x3fefbe3ecac6f383,
    0x3c979aa65d837b6d, 0x3fefb5586cf9890f, 0x3c8eb51a92fdeffc, 0x3fefac922b7247f7,
    0x3c3ebe3d702f9cd1, 0x3fefa3ec32d3d1a2, 0xbc6a033489906e0b, 0x3fef9b66affed31b,
    0xbc9556522a2fbd0e, 0x3fef9301d0125b51, 0xbc5080ef8c4eea55, 0x3fef8abdc06c31cc,
    0xbc91c923b9d5f416, 0x3fef829aaea92de0, 0x3c80d3e3e95c55af, 0x3fef7a98c8a58e51,
    0xbc801b15eaa59348, 0x3fef72b83c7d517b, 0xbc8f1ff055de323d, 0x3fef6af9388c8dea,
    0x3c8b898c3f1353bf, 0x3fef635beb6fcb75, 0xbc96d99c7611eb26, 0x3fef5be084045cd4,
    0x3c9aecf73e3a2f60, 0x3fef54873168b9aa, 0xbc8fe782cb86389d, 0x3fef4d5022fcd91d,
    0x3c8a6f4144a6c38d, 0x3fef463b88628cd6, 0x3c807a05b0e4047d, 0x3fef3f49917ddc96,
    0x3c968efde3a8a894, 0x3fef387a6e756238, 0x3c875e18f274487d, 0x3fef31ce4fb2a63f,
    0x3c80472b981fe7f2, 0x3fef2b4565e27cdd, 0xbc96b87b3f71085e, 0x3fef24dfe1f56381,
    0x3c82f7e16d09ab31, 0x3fef1e9df51fdee1, 0xbc3d219b1a6fbffa, 0x3fef187fd0dad990,
    0x3c8b3782720c0ab4, 0x3fef1285a6e4030b, 0x3c6e149289cecb8f, 0x3fef0cafa93e2f56,
    0x3c834d754db0abb6, 0x3fef06fe0a31b715, 0x3c864201e2ac744c, 0x3fef0170fc4cd831,
    0x3c8fdd395dd3f84a, 0x3feefc08b26416ff, 0xbc86a3803b8e5b04, 0x3feef6c55f929ff1,
    0xbc924aedcc4b5068, 0x3feef1a7373aa9cb, 0xbc9907f81b512d8e, 0x3feeecae6d05d866,
    0xbc71d1e83e9436d2, 0x3feee7db34e59ff7, 0xbc991919b3ce1b15, 0x3feee32dc313a8e5,
    0x3c859f48a72a4c6d, 0x3feedea64c123422, 0xbc9312607a28698a, 0x3feeda4504ac801c,
    0xbc58a78f4817895b, 0x3feed60a21f72e2a, 0xbc7c2c9b67499a1b, 0x3feed1f5d950a897,
    0x3c4363ed60c2ac11, 0x3feece086061892d, 0x3c9666093b0664ef, 0x3feeca41ed1d0057,
    0x3c6ecce1daa10379, 0x3feec6a2b5c13cd0, 0x3c93ff8e3f0f1230, 0x3feec32af0d7d3de,
    0x3c7690cebb7aafb0, 0x3feebfdad5362a27, 0x3c931dbdeb54e077, 0x3feebcb299fddd0d,
    0xbc8f94340071a38e, 0x3feeb9b2769d2ca7, 0xbc87deccdc93a349, 0x3feeb6daa2cf6642,
    0xbc78dec6bd0f385f, 0x3feeb42b569d4f82, 0xbc861246ec7b5cf6, 0x3feeb1a4ca5d920f,
    0x3c93350518fdd78e, 0x3feeaf4736b527da, 0x3c7b98b72f8a9b05, 0x3feead12d497c7fd,
    0x3c9063e1e21c5409, 0x3feeab07dd485429, 0x3c34c7855019c6ea, 0x3feea9268a5946b7,
    0x3c9432e62b64c035, 0x3feea76f15ad2148, 0xbc8ce44a6199769f, 0x3feea5e1b976dc09,
    0xbc8c33c53bef4da8, 0x3feea47eb03a5585, 0xbc845378892be9ae, 0x3feea34634ccc320,
    0xbc93cedd78565858, 0x3feea23882552225, 0x3c5710aa807e1964, 0x3feea155d44ca973,
    0xbc93b3efbf5e2228, 0x3feea09e667f3bcd, 0xbc6a12ad8734b982, 0x3feea012750bdabf,
    0xbc6367efb86da9ee, 0x3fee9fb23c651a2f, 0xbc80dc3d54e08851, 0x3fee9f7df9519484,
    0xbc781f647e5a3ecf, 0x3fee9f75e8ec5f74, 0xbc86ee4ac08b7db0, 0x3fee9f9a48a58174,
    0xbc8619321e55e68a, 0x3fee9feb564267c9, 0x3c909ccb5e09d4d3, 0x3feea0694fde5d3f,
    0xbc7b32dcb94da51d, 0x3feea11473eb0187, 0x3c94ecfd5467c06b, 0x3feea1ed0130c132,
    0x3c65ebe1abd66c55, 0x3feea2f336cf4e62, 0xbc88a1c52fb3cf42, 0x3feea427543e1a12,
    0xbc9369b6f13b3734, 0x3feea589994cce13, 0xbc805e843a19ff1e, 0x3feea71a4623c7ad,
    0xbc94d450d872576e, 0x3feea8d99b4492ed, 0x3c90ad675b0e8a00, 0x3feeaac7d98a6699,
    0x3c8db72fc1f0eab4, 0x3feeace5422aa0db, 0xbc65b6609cc5e7ff, 0x3feeaf3216b5448c,
    0x3c7bf68359f35f44, 0x3feeb1ae99157736, 0xbc93091fa71e3d83, 0x3feeb45b0b91ffc6,
    0xbc5da9b88b6c1e29, 0x3feeb737b0cdc5e5, 0xbc6c23f97c90b959, 0x3feeba44cbc8520f,
    0xbc92434322f4f9aa, 0x3feebd829fde4e50, 0xbc85ca6cd7668e4b, 0x3feec0f170ca07ba,
    0x3c71affc2b91ce27, 0x3feec49182a3f090, 0x3c6dd235e10a73bb, 0x3feec86319e32323,
    0xbc87c50422622263, 0x3feecc667b5de565, 0x3c8b1c86e3e231d5, 0x3feed09bec4a2d33,
    0xbc91bbd1d3bcbb15, 0x3feed503b23e255d, 0x3c90cc319cee31d2, 0x3feed99e1330b358,
    0x3c8469846e735ab3, 0x3feede6b5579fdbf, 0xbc82dfcd978e9db4, 0x3feee36bbfd3f37a,
    0x3c8c1a7792cb3387, 0x3feee89f995ad3ad, 0xbc907b8f4ad1d9fa, 0x3feeee07298db666,
    0xbc55c3d956dcaeba, 0x3feef3a2b84f15fb, 0xbc90a40e3da6f640, 0x3feef9728de5593a,
    0xbc68d6f438ad9334, 0x3feeff76f2fb5e47, 0xbc91eee26b588a35, 0x3fef05b030a1064a,
    0x3c74ffd70a5fddcd, 0x3fef0c1e904bc1d2, 0xbc91bdfbfa9298ac, 0x3fef12c25bd71e09,
    0x3c736eae30af0cb3, 0x3fef199bdd85529c, 0x3c8ee3325c9ffd94, 0x3fef20ab5fffd07a,
    0x3c84e08fd10959ac, 0x3fef27f12e57d14b, 0x3c63cdaf384e1a67, 0x3fef2f6d9406e7b5,
    0x3c676b2c6c921968, 0x3fef3720dcef9069, 0xbc808a1883ccb5d2, 0x3fef3f0b555dc3fa,
    0xbc8fad5d3ffffa6f, 0x3fef472d4a07897c, 0xbc900dae3875a949, 0x3fef4f87080d89f2,
    0x3c74a385a63d07a7, 0x3fef5818dcfba487, 0xbc82919e2040220f, 0x3fef60e316c98398,
    0x3c8e5a50d5c192ac, 0x3fef69e603db3285, 0x3c843a59ac016b4b, 0x3fef7321f301b460,
    0xbc82d52107b43e1f, 0x3fef7c97337b9b5f, 0xbc892ab93b470dc9, 0x3fef864614f5a129,
    0x3c74b604603a88d3, 0x3fef902ee78b3ff6, 0x3c83c5ec519d7271, 0x3fef9a51fbc74c83,
    0xbc8ff7128fd391f0, 0x3fefa4afa2a490da, 0xbc8dae98e223747d, 0x3fefaf482d8e67f1,
    0x3c8ec3bc41aa2008, 0x3fefba1bee615a27, 0x3c842b94c3a9eb32, 0x3fefc52b376bba97,
    0x3c8a64a931d185ee, 0x3fefd0765b6e4540, 0xbc8e37bae43be3ed, 0x3fefdbfdad9cbe14,
    0x3c77893b4d91cd9d, 0x3fefe7c1819e90d8, 0x3c5305c14160cc89, 0x3feff3c22b8f71f1,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_matches_musl() {
        // fdlibm (the `libm` crate) rounds this one the other way.
        assert_eq!(log(0.01).to_bits(), 0xc0126bb1bbb55515);
        assert_eq!(log(1.0), 0.0);
        assert_eq!(log(0.0), f64::NEG_INFINITY);
        assert!(log(-1.0).is_nan());
        assert_eq!(log(f64::INFINITY), f64::INFINITY);
        for x in [0.5, 2.0, 10.0, 1e-300, 5e-324, 1e308, 0.999, 1.001, 123.456] {
            assert!((log(x) - x.ln()).abs() <= x.ln().abs() * 4e-16, "{x}");
        }
    }

    #[test]
    fn pow_matches_musl() {
        assert_eq!(pow(2.0, 10.0), 1024.0);
        assert_eq!(pow(-2.0, 3.0), -8.0);
        assert!(pow(-2.0, 0.5).is_nan());
        assert_eq!(pow(0.0, -1.0), f64::INFINITY);
        assert_eq!(pow(2.0, 2000.0), f64::INFINITY);
        assert_eq!(pow(2.0, -2000.0), 0.0);
        assert_eq!(pow(0.9, 0.0), 1.0);
        assert_eq!(pow(2.0, -1074.0), 5e-324);
        for (x, y) in [(0.9, 7.0), (1.5, 2.5), (10.0, -3.0), (3.0, 0.1), (1e-10, 3.0), (7.0, 300.0)] {
            assert!((pow(x, y) - x.powf(y)).abs() <= x.powf(y).abs() * 4e-16, "{x} {y}");
        }
    }
}
