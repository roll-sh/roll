//! Floating point the way Redis' C code computes it on this CPU.
//!
//! Redis and its modules are C, and two things in C arithmetic depend on the architecture they are
//! compiled for, which shows in replies down to the last digit:
//!
//! - **Contraction.** GCC fuses `a * b + c` into one rounding (`fmadd`) on arm64, where the
//!   instruction is always there; for baseline x86-64 it can't, so the product is rounded first.
//!   [`Mac::mac`] is `a * b + c` as the compiler emits it: fused on arm64, not on x86-64.
//! - **Conversions out of range.** A C cast of a double to an integer is undefined when the value
//!   doesn't fit; arm64's instructions saturate (NaN to 0), x86-64's `cvttsd2si` gives the
//!   "integer indefinite" (the minimum), and unsigned casts are built from it. [`c_i64`] and its
//!   siblings reproduce what the compiled cast returns.
//!
//! Elsewhere roll's results are bit-identical on both architectures; these are the places Redis'
//! are not.

/// `a * b + c` as C compiled for this CPU computes it (see the module docs).
pub trait Mac: Sized {
    fn mac(self, b: Self, c: Self) -> Self;
}

impl Mac for f64 {
    #[inline(always)]
    fn mac(self, b: f64, c: f64) -> f64 {
        #[cfg(target_arch = "aarch64")]
        {
            self.mul_add(b, c)
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            self * b + c
        }
    }
}

impl Mac for f32 {
    #[inline(always)]
    fn mac(self, b: f32, c: f32) -> f32 {
        #[cfg(target_arch = "aarch64")]
        {
            self.mul_add(b, c)
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            self * b + c
        }
    }
}

/// 2^63, the first double past `i64::MAX`.
#[cfg(target_arch = "x86_64")]
const TWO_63: f64 = 9_223_372_036_854_775_808.0;

/// `(int64_t)x`.
#[inline]
pub fn c_i64(x: f64) -> i64 {
    #[cfg(target_arch = "x86_64")]
    {
        // cvttsd2si: in range it truncates; out of range, and NaN, it is i64::MIN.
        // (No double lies between -2^63 - 2048 and -2^63, so this is exactly "fits".)
        if (-TWO_63..TWO_63).contains(&x) { x as i64 } else { i64::MIN }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        x as i64
    }
}

/// `(int32_t)x`.
#[inline]
pub fn c_i32(x: f64) -> i32 {
    #[cfg(target_arch = "x86_64")]
    {
        if x > -2_147_483_649.0 && x < 2_147_483_648.0 { x as i32 } else { i32::MIN }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        x as i32
    }
}

/// `(uint64_t)x`.
#[inline]
pub fn c_u64(x: f64) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // Below 2^63 (and NaN, which compares false) one signed conversion; above, the
        // conversion of x - 2^63 with the top bit flipped.
        if x >= TWO_63 { (c_i64(x - TWO_63) as u64) ^ (1 << 63) } else { c_i64(x) as u64 }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        x as u64
    }
}

/// `(uint32_t)x`: the low half of the 64-bit signed conversion on x86-64.
#[inline]
pub fn c_u32(x: f64) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        c_i64(x) as u32
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        x as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_in_range_are_truncations() {
        for x in [0.0, 1.9, -1.9, 123456.75, -9.0e18, 9.0e18] {
            assert_eq!(c_i64(x), x as i64);
            if x >= 0.0 {
                assert_eq!(c_u64(x), x as u64);
            }
        }
        assert_eq!(c_u32(4_000_000_000.5), 4_000_000_000);
        assert_eq!(c_i32(-7.9), -7);
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn x86_out_of_range() {
        assert_eq!(c_i64(1e20), i64::MIN);
        assert_eq!(c_i64(f64::NAN), i64::MIN);
        assert_eq!(c_i64(f64::INFINITY), i64::MIN);
        assert_eq!(c_i32(3e9), i32::MIN);
        assert_eq!(c_u32(-1.5), u32::MAX);
        assert_eq!(c_u32(4_294_967_296.0), 0);
        assert_eq!(c_u64(-1.0), u64::MAX);
        assert_eq!(c_u64(1.8e19), 1.8e19 as u64);
        assert_eq!(c_u64(1e20), 0);
    }

    #[test]
    #[cfg(target_arch = "aarch64")]
    fn arm_saturates() {
        assert_eq!(c_i64(1e20), i64::MAX);
        assert_eq!(c_i64(f64::NAN), 0);
        assert_eq!(c_u32(-1.5), 0);
        assert_eq!(c_u64(1e20), u64::MAX);
    }
}
