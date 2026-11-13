//! `java.math`: `BigInteger`, `BigDecimal`, `MathContext` and `RoundingMode`.
//!
//! [`BigInt`] is sign and magnitude over little-endian 32-bit limbs; bitwise operations work on
//! the two's-complement view like Java's. [`BigDec`] is an unscaled `BigInt` with a scale, with
//! Java's preferred-scale rules for arithmetic, its rounding modes, and its `toString`,
//! `toPlainString` and `toEngineeringString` layouts.
//!
//! Java bounds a `BigInteger` to 2^31 - 1 bits of magnitude and fails anything larger with
//! `ArithmeticException: BigInteger would overflow supported range`. The operations that can
//! grow a value cheaply from a small input (`shiftLeft`, `pow`, the powers of ten behind
//! `setScale`/`movePoint*`, `multiply`) check that bound before allocating, as Java's do, and the
//! plain-value arithmetic reports it through [`crate::guard::note_big_overflow`]. Multiplication
//! is Karatsuba above [`KARATSUBA_LIMBS`] and radix conversion divides and conquers over
//! squared powers of the radix, so large values stay sub-quadratic where Java's are.

use std::cmp::Ordering;
use std::rc::Rc;

use crate::lib_lang::{int, long};
use crate::lookup::Builder;
use crate::types::{cls, Ty};
use crate::value::{ArrayData, JArray, Obj, Value};
use crate::vm::{Throw, Thrown, Vm, VR};

// ------------------------------------------------------------------------------ magnitudes

fn trim(v: &mut Vec<u32>) {
    while v.last() == Some(&0) {
        v.pop();
    }
}

fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (a, b) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(a.len() + 1);
    let mut carry = 0u64;
    for (i, &x) in a.iter().enumerate() {
        let s = x as u64 + b.get(i).copied().unwrap_or(0) as u64 + carry;
        out.push(s as u32);
        carry = s >> 32;
    }
    if carry > 0 {
        out.push(carry as u32);
    }
    out
}

/// `a - b` for `a >= b`.
fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, &x) in a.iter().enumerate() {
        let mut d = x as i64 - b.get(i).copied().unwrap_or(0) as i64 - borrow;
        if d < 0 {
            d += 1 << 32;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(d as u32);
    }
    trim(&mut out);
    out
}

fn mul_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u32; a.len() + b.len()];
    mul_into(a, b, &mut out);
    trim(&mut out);
    out
}

/// Operand length (limbs) from which multiplication splits (Java's Karatsuba threshold is 80
/// ints; ours is lower because the schoolbook loop here is not intrinsified).
pub const KARATSUBA_LIMBS: usize = 40;

/// `out += a * b` (`out` holds at least `a.len() + b.len()` limbs; the sum must fit).
fn mul_into(a: &[u32], b: &[u32], out: &mut [u32]) {
    let (a, b) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    if b.is_empty() {
        return;
    }
    if b.len() < KARATSUBA_LIMBS {
        for (j, &y) in b.iter().enumerate() {
            let mut carry = 0u64;
            let row = &mut out[j..];
            for (o, &x) in row.iter_mut().zip(a) {
                let t = x as u64 * y as u64 + *o as u64 + carry;
                *o = t as u32;
                carry = t >> 32;
            }
            add_carry(&mut row[a.len()..], carry as u32);
        }
        return;
    }
    let h = a.len().div_ceil(2);
    if b.len() <= h {
        // unbalanced: multiply `b` by `a` in slices of its own length
        for (i, chunk) in a.chunks(b.len()).enumerate() {
            mul_into(chunk, b, &mut out[i * b.len()..]);
        }
        return;
    }
    // a = a1·B^h + a0, b = b1·B^h + b0;
    // a·b = z2·B^2h + ((a0 + a1)(b0 + b1) - z0 - z2)·B^h + z0
    let (a0, a1) = a.split_at(h);
    let (b0, b1) = b.split_at(h);
    let mut z0 = vec![0u32; 2 * h];
    mul_into(a0, b0, &mut z0);
    let mut z2 = vec![0u32; a1.len() + b1.len()];
    mul_into(a1, b1, &mut z2);
    let sa = add_mag(a0, a1);
    let sb = add_mag(b0, b1);
    let mut z1 = vec![0u32; sa.len() + sb.len()];
    mul_into(&sa, &sb, &mut z1);
    sub_in_place(&mut z1, &z0);
    sub_in_place(&mut z1, &z2);
    add_in_place(out, &z0);
    add_in_place(&mut out[h..], &z1);
    add_in_place(&mut out[2 * h..], &z2);
}

/// Adds `c` at `dst[0]` and propagates the carry.
#[inline]
fn add_carry(dst: &mut [u32], mut c: u32) {
    for d in dst {
        if c == 0 {
            return;
        }
        let (v, o) = d.overflowing_add(c);
        *d = v;
        c = o as u32;
    }
}

/// `dst += src` (the sum must fit in `dst`; `src` may have high zero limbs past `dst`).
fn add_in_place(dst: &mut [u32], src: &[u32]) {
    let n = src.len().min(dst.len());
    let mut carry = 0u64;
    for (d, &x) in dst[..n].iter_mut().zip(&src[..n]) {
        let t = *d as u64 + x as u64 + carry;
        *d = t as u32;
        carry = t >> 32;
    }
    add_carry(&mut dst[n..], carry as u32);
}

/// `dst -= src` for `dst >= src`.
fn sub_in_place(dst: &mut [u32], src: &[u32]) {
    let n = src.len().min(dst.len());
    let mut borrow = 0u64;
    for (d, &x) in dst[..n].iter_mut().zip(&src[..n]) {
        let t = (*d as u64).wrapping_sub(x as u64).wrapping_sub(borrow);
        *d = t as u32;
        borrow = (t >> 63) & 1;
    }
    for d in &mut dst[n..] {
        if borrow == 0 {
            break;
        }
        let (v, o) = d.overflowing_sub(1);
        *d = v;
        borrow = o as u64;
    }
}

/// Java's bound on a magnitude: 2^31 - 1 bits.
const MAX_MAG_BITS: u64 = i32::MAX as u64;

fn mag_bits(m: &[u32]) -> u64 {
    match m.last() {
        Some(top) => (m.len() as u64 - 1) * 32 + (32 - top.leading_zeros() as u64),
        None => 0,
    }
}

/// A result outside Java's range: recorded (see [`crate::guard::note_big_overflow`]) with zero
/// as the placeholder.
#[cold]
fn big_overflow(site: crate::guard::BigSite) -> BigInt {
    crate::guard::note_big_overflow(site);
    BigInt::zero()
}

/// Java's `ArithmeticException: BigInteger would overflow supported range` with the frames of
/// where it is detected, then `outer` (`java.math` frames as [`jm`] takes them).
pub(crate) fn overflow_error(site: crate::guard::BigSite, outer: &[&str]) -> Thrown {
    use crate::guard::BigSite::*;
    let mut f: Vec<&str> = vec!["BigInteger.reportOverflow(BigInteger.java:1220)"];
    let range = ["BigInteger.checkRange(BigInteger.java:1215)", "BigInteger.<init>(BigInteger.java:1190)"];
    match site {
        Range => f.extend(range),
        ShiftLeft => f.extend(range.iter().copied().chain(["BigInteger.shiftLeft(BigInteger.java:3578)"])),
        ShiftRight => f.extend(range.iter().copied().chain(["BigInteger.shiftRight(BigInteger.java:3655)"])),
        PowShift => f.push("BigInteger.pow(BigInteger.java:2616)"),
        Pow => f.push("BigInteger.pow(BigInteger.java:2644)"),
        PowShiftLeft => f.extend(range.iter().copied().chain(["BigInteger.shiftLeft(BigInteger.java:3578)", "BigInteger.pow(BigInteger.java:2623)"])),
    }
    f.extend_from_slice(outer);
    jm(arith("BigInteger would overflow supported range"), &f)
}

/// `v`, or the range overflow recorded while computing it, with `outer` frames.
fn in_range<T>(v: T, outer: &[&str]) -> Result<T, Thrown> {
    match crate::guard::take_big_overflow() {
        Some(site) => Err(overflow_error(site, outer)),
        None => Ok(v),
    }
}

/// Digits per limb-sized chunk of a radix and the chunk's value (`radix^digits`), the largest
/// power that fits in a `u32`.
fn radix_chunk(radix: u32) -> (usize, u32) {
    let (mut d, mut v) = (0usize, 1u64);
    while v * radix as u64 <= u32::MAX as u64 {
        v *= radix as u64;
        d += 1;
    }
    (d, v as u32)
}

/// Magnitudes up to this many limbs convert chunk by chunk; longer ones split.
const RADIX_SPLIT_LIMBS: usize = 48;

/// The digits of a non-zero magnitude in `radix` (lowercase, no sign).
fn mag_to_radix(mag: &[u32], radix: u32) -> String {
    let (chunk_digits, chunk) = radix_chunk(radix);
    // pows[k] = chunk^(2^k), squared until it covers half the value
    let mut pows: Vec<Vec<u32>> = vec![vec![chunk]];
    while pows.last().unwrap().len() * 2 <= mag.len() + 1 && mag.len() > RADIX_SPLIT_LIMBS {
        let p = pows.last().unwrap();
        let sq = mul_mag(p, p);
        pows.push(sq);
    }
    let mut out = Vec::with_capacity(mag.len() * 32 / (radix.ilog2() as usize).max(1) + 1);
    radix_rec(mag, 0, radix, chunk_digits, chunk, &pows, &mut out);
    // the digits are ASCII
    String::from_utf8(out).unwrap()
}

/// Appends `x` in `radix`, left-padded with zeros to `pad` digits.
fn radix_rec(x: &[u32], pad: usize, radix: u32, chunk_digits: usize, chunk: u32, pows: &[Vec<u32>], out: &mut Vec<u8>) {
    let mut x = x;
    while x.last() == Some(&0) {
        x = &x[..x.len() - 1];
    }
    if x.len() <= RADIX_SPLIT_LIMBS {
        let start = out.len();
        let mut mag = x.to_vec();
        // chunks least significant first, digits reversed at the end
        while !mag.is_empty() {
            let mut rem = 0u64;
            for limb in mag.iter_mut().rev() {
                let v = (rem << 32) | *limb as u64;
                *limb = (v / chunk as u64) as u32;
                rem = v % chunk as u64;
            }
            trim(&mut mag);
            let mut r = rem as u32;
            for i in 0..chunk_digits {
                if mag.is_empty() && r == 0 && i > 0 {
                    break;
                }
                out.push(std::char::from_digit(r % radix, radix).unwrap() as u8);
                r /= radix;
            }
        }
        // a zero chunk still printed one digit
        while out.len() > start + 1 && out.last() == Some(&b'0') {
            out.pop();
        }
        if x.is_empty() {
            out.truncate(start);
        }
        let n = out.len() - start;
        for _ in n..pad {
            out.push(b'0');
        }
        out[start..].reverse();
        return;
    }
    // the largest power whose square still covers the value: balanced halves
    let k = pows.iter().rposition(|p| p.len() * 2 <= x.len() + 1).unwrap_or(0);
    let (q, r) = divmod_mag(x, &pows[k]);
    let digits = chunk_digits << k;
    radix_rec(&q, pad.saturating_sub(digits), radix, chunk_digits, chunk, pows, out);
    radix_rec(&r, digits, radix, chunk_digits, chunk, pows, out);
}

fn divmod_small(a: &[u32], d: u32) -> (Vec<u32>, u32) {
    let mut q = vec![0u32; a.len()];
    let mut rem = 0u64;
    for i in (0..a.len()).rev() {
        let x = (rem << 32) | a[i] as u64;
        q[i] = (x / d as u64) as u32;
        rem = x % d as u64;
    }
    trim(&mut q);
    (q, rem as u32)
}

fn shl_mag(a: &[u32], n: usize) -> Vec<u32> {
    if a.is_empty() {
        return Vec::new();
    }
    let (limbs, bits) = (n / 32, n % 32);
    let mut out = vec![0u32; limbs];
    if bits == 0 {
        out.extend_from_slice(a);
    } else {
        let mut carry = 0u32;
        for &x in a {
            out.push((x << bits) | carry);
            carry = x >> (32 - bits);
        }
        if carry > 0 {
            out.push(carry);
        }
    }
    out
}

fn shr_mag(a: &[u32], n: usize) -> Vec<u32> {
    let (limbs, bits) = (n / 32, n % 32);
    if limbs >= a.len() {
        return Vec::new();
    }
    let src = &a[limbs..];
    let mut out = Vec::with_capacity(src.len());
    if bits == 0 {
        out.extend_from_slice(src);
    } else {
        for i in 0..src.len() {
            let hi = src.get(i + 1).copied().unwrap_or(0);
            out.push((src[i] >> bits) | (hi << (32 - bits)));
        }
    }
    trim(&mut out);
    out
}

/// Knuth's algorithm D: `(a / b, a % b)` for `b != 0`.
fn divmod_mag(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    if cmp_mag(a, b) == Ordering::Less {
        return (Vec::new(), a.to_vec());
    }
    if b.len() == 1 {
        let (q, r) = divmod_small(a, b[0]);
        return (q, if r == 0 { Vec::new() } else { vec![r] });
    }
    let s = b[b.len() - 1].leading_zeros() as usize;
    let v = shl_mag(b, s);
    let mut u = shl_mag(a, s);
    u.push(0);
    if u.len() < a.len() + 1 {
        u.resize(a.len() + 1, 0);
    }
    let n = v.len();
    let m = u.len() - n - 1;
    let mut q = vec![0u32; m + 1];
    let b32 = 1u64 << 32;
    for j in (0..=m).rev() {
        let num = ((u[j + n] as u64) << 32) | u[j + n - 1] as u64;
        let mut qhat = num / v[n - 1] as u64;
        let mut rhat = num % v[n - 1] as u64;
        while qhat >= b32 || qhat * v[n - 2] as u64 > ((rhat << 32) | u[j + n - 2] as u64) {
            qhat -= 1;
            rhat += v[n - 1] as u64;
            if rhat >= b32 {
                break;
            }
        }
        let mut borrow = 0i64;
        let mut carry = 0u64;
        for i in 0..n {
            let p = qhat * v[i] as u64 + carry;
            carry = p >> 32;
            let t = u[i + j] as i64 - (p as u32) as i64 - borrow;
            u[i + j] = t as u32;
            borrow = if t < 0 { 1 } else { 0 };
        }
        let t = u[j + n] as i64 - carry as i64 - borrow;
        u[j + n] = t as u32;
        if t < 0 {
            qhat -= 1;
            let mut c = 0u64;
            for i in 0..n {
                let s2 = u[i + j] as u64 + v[i] as u64 + c;
                u[i + j] = s2 as u32;
                c = s2 >> 32;
            }
            u[j + n] = u[j + n].wrapping_add(c as u32);
        }
        q[j] = qhat as u32;
    }
    trim(&mut q);
    u.truncate(n);
    let mut r = shr_mag(&u, s);
    trim(&mut r);
    (q, r)
}

// ------------------------------------------------------------------------------ BigInt

/// An arbitrary-precision integer: sign and little-endian 32-bit magnitude (no leading zeros).
#[derive(PartialEq, Eq, Debug, Clone, Default)]
pub struct BigInt {
    neg: bool,
    mag: Vec<u32>,
}

fn arith(msg: &str) -> Thrown {
    Throw::new(cls::ARITHMETIC_EXCEPTION, Some(msg.to_string()))
}

/// Appends JDK frames (`java.base/java.math.<frame>`, or `java.lang.…` as given).
fn jm(mut t: Thrown, frames: &[&str]) -> Thrown {
    for f in frames {
        if f.starts_with("java.") {
            t.frames.push(format!("java.base/{f}"));
        } else {
            t.frames.push(format!("java.base/java.math.{f}"));
        }
    }
    t
}

const NEED_INCREMENT: [&str; 3] = ["BigDecimal.commonNeedIncrement(BigDecimal.java:4963)", "BigDecimal.needIncrement(BigDecimal.java:5013)", "BigDecimal.divideAndRound(BigDecimal.java:4927)"];

/// Re-frames a "Rounding necessary" failure of `setScale`.
fn rounding_frames(t: Thrown, outer: &[&str]) -> Thrown {
    if t.message.as_deref() != Some("Rounding necessary") {
        return t;
    }
    let mut t = t;
    t.frames.clear();
    let mut f: Vec<&str> = NEED_INCREMENT.to_vec();
    f.push("BigDecimal.setScale(BigDecimal.java:2981)");
    f.extend_from_slice(outer);
    jm(t, &f)
}

impl BigInt {
    pub fn zero() -> BigInt {
        BigInt::default()
    }

    fn from_parts(neg: bool, mut mag: Vec<u32>) -> BigInt {
        trim(&mut mag);
        // Java's `checkRange` (2^26 limbs is where the bound can be crossed)
        if mag.len() >= 1 << 26 && mag_bits(&mag) > MAX_MAG_BITS {
            return big_overflow(crate::guard::BigSite::Range);
        }
        let neg = neg && !mag.is_empty();
        BigInt { neg, mag }
    }

    pub fn from_i64(v: i64) -> BigInt {
        let u = v.unsigned_abs();
        BigInt::from_parts(v < 0, vec![u as u32, (u >> 32) as u32])
    }

    pub fn from_u64(u: u64) -> BigInt {
        BigInt::from_parts(false, vec![u as u32, (u >> 32) as u32])
    }

    /// Parses digits in a radix with an optional sign (Java's `new BigInteger(s, radix)`).
    pub fn parse_radix(s: &str, radix: u32) -> Option<BigInt> {
        let (neg, digits) = match s.as_bytes().first()? {
            b'-' => (true, &s[1..]),
            b'+' => (false, &s[1..]),
            _ => (false, s),
        };
        if digits.is_empty() {
            return None;
        }
        let mut mag: Vec<u32> = Vec::new();
        for ch in digits.chars() {
            let d = ch.to_digit(radix)?;
            let mut carry = d as u64;
            for limb in mag.iter_mut() {
                let x = *limb as u64 * radix as u64 + carry;
                *limb = x as u32;
                carry = x >> 32;
            }
            if carry > 0 {
                mag.push(carry as u32);
            }
        }
        Some(BigInt::from_parts(neg, mag))
    }

    /// Parses a decimal integer with an optional sign.
    pub fn parse(s: &str) -> Option<BigInt> {
        BigInt::parse_radix(s, 10)
    }

    pub fn signum(&self) -> i32 {
        if self.mag.is_empty() {
            0
        } else if self.neg {
            -1
        } else {
            1
        }
    }

    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    pub fn abs(&self) -> BigInt {
        BigInt { neg: false, mag: self.mag.clone() }
    }

    pub fn negate(&self) -> BigInt {
        BigInt::from_parts(!self.neg, self.mag.clone())
    }

    pub fn add(&self, o: &BigInt) -> BigInt {
        if self.neg == o.neg {
            return BigInt::from_parts(self.neg, add_mag(&self.mag, &o.mag));
        }
        match cmp_mag(&self.mag, &o.mag) {
            Ordering::Equal => BigInt::zero(),
            Ordering::Greater => BigInt::from_parts(self.neg, sub_mag(&self.mag, &o.mag)),
            Ordering::Less => BigInt::from_parts(o.neg, sub_mag(&o.mag, &self.mag)),
        }
    }

    pub fn sub(&self, o: &BigInt) -> BigInt {
        self.add(&o.negate())
    }

    pub fn mul(&self, o: &BigInt) -> BigInt {
        // the product has at least `bits(a) + bits(b) - 1` bits
        if !self.mag.is_empty() && !o.mag.is_empty() && mag_bits(&self.mag) + mag_bits(&o.mag) - 1 > MAX_MAG_BITS {
            return big_overflow(crate::guard::BigSite::Range);
        }
        BigInt::from_parts(self.neg != o.neg, mul_mag(&self.mag, &o.mag))
    }

    pub fn mul_small(&self, m: u32) -> BigInt {
        BigInt::from_parts(self.neg, mul_mag(&self.mag, &[m]))
    }

    /// Truncating division and remainder (`divideAndRemainder`).
    pub fn divrem(&self, o: &BigInt) -> Result<(BigInt, BigInt), Thrown> {
        if o.is_zero() {
            return Err(arith("BigInteger divide by zero"));
        }
        let (q, r) = divmod_mag(&self.mag, &o.mag);
        Ok((BigInt::from_parts(self.neg != o.neg, q), BigInt::from_parts(self.neg, r)))
    }

    /// `10^n`, as Java's `bigTenToThe` (`BigInteger.TEN.pow(n)` past its cache): out of range
    /// for `n >= 715,827,883`.
    pub fn pow10(n: u32) -> BigInt {
        if n <= 64 {
            let mut r = BigInt::from_i64(1);
            let mut left = n;
            while left > 0 {
                let k = left.min(9);
                r = r.mul_small(10u32.pow(k));
                left -= k;
            }
            return r;
        }
        BigInt::from_i64(10).pow(n)
    }

    /// `pow` with JDK 27's range checks, made before any work: the factor of two must shift by
    /// an `int`, and `(bits(odd part) - 1) * e + shift` must stay below `Integer.MAX_VALUE`.
    pub fn pow(&self, e: u32) -> BigInt {
        if e == 0 || (!self.neg && self.mag == [1]) {
            return BigInt::from_i64(1);
        }
        if self.mag.is_empty() || e == 1 {
            return self.clone();
        }
        let neg = self.neg && e & 1 == 1;
        let twos = self.lowest_set_bit() as u64;
        let shift = twos * e as u64;
        if shift > MAX_MAG_BITS {
            return big_overflow(crate::guard::BigSite::PowShift);
        }
        let odd = shr_mag(&self.mag, twos as usize);
        let rem_bits = mag_bits(&odd);
        if rem_bits == 1 {
            if 1 + shift > MAX_MAG_BITS {
                return big_overflow(crate::guard::BigSite::PowShiftLeft);
            }
            return BigInt::from_parts(neg, vec![1]).shift_left(shift as i32);
        }
        if rem_bits * e as u64 > 64 && rem_bits * e as u64 + shift - e as u64 >= MAX_MAG_BITS {
            return big_overflow(crate::guard::BigSite::Pow);
        }
        let mut result = vec![1u32];
        let mut base = odd;
        let mut e = e;
        while e > 0 {
            if e & 1 == 1 {
                result = mul_mag(&result, &base);
            }
            e >>= 1;
            if e > 0 {
                base = mul_mag(&base, &base);
            }
        }
        BigInt::from_parts(neg, result).shift_left(shift as i32)
    }

    pub fn gcd(&self, o: &BigInt) -> BigInt {
        let (mut a, mut b) = (self.abs(), o.abs());
        while !b.is_zero() {
            let r = a.divrem(&b).unwrap().1;
            a = b;
            b = r;
        }
        a
    }

    /// `mod`: the non-negative remainder.
    pub fn modulo(&self, m: &BigInt) -> Result<BigInt, Thrown> {
        if m.signum() <= 0 {
            return Err(arith("BigInteger: modulus not positive"));
        }
        let r = self.divrem(m)?.1;
        Ok(if r.neg { r.add(m) } else { r })
    }

    pub fn mod_inverse(&self, m: &BigInt) -> Result<BigInt, Thrown> {
        if m.signum() <= 0 {
            return Err(arith("BigInteger: modulus not positive"));
        }
        if *m == BigInt::from_i64(1) {
            return Ok(BigInt::zero());
        }
        let a = self.modulo(m)?;
        let (mut old_r, mut r) = (a, m.clone());
        let (mut old_s, mut s) = (BigInt::from_i64(1), BigInt::zero());
        while !r.is_zero() {
            let (q, rem) = old_r.divrem(&r)?;
            old_r = std::mem::replace(&mut r, rem);
            let ns = old_s.sub(&q.mul(&s));
            old_s = std::mem::replace(&mut s, ns);
        }
        if old_r != BigInt::from_i64(1) {
            return Err(arith("BigInteger not invertible."));
        }
        old_s.modulo(m)
    }

    pub fn mod_pow(&self, e: &BigInt, m: &BigInt) -> Result<BigInt, Thrown> {
        if m.signum() <= 0 {
            return Err(arith("BigInteger: modulus not positive"));
        }
        let (base, e) = if e.neg { (self.mod_inverse(m)?, e.abs()) } else { (self.modulo(m)?, e.clone()) };
        let mut result = BigInt::from_i64(1).modulo(m)?;
        let mut b = base;
        for i in 0..e.bit_length_mag() {
            if e.mag[i / 32] >> (i % 32) & 1 == 1 {
                result = result.mul(&b).modulo(m)?;
            }
            b = b.mul(&b).modulo(m)?;
        }
        Ok(result)
    }

    fn bit_length_mag(&self) -> usize {
        match self.mag.last() {
            Some(top) => (self.mag.len() - 1) * 32 + (32 - top.leading_zeros() as usize),
            None => 0,
        }
    }

    /// `bitLength`: bits of the minimal two's complement, excluding the sign bit.
    pub fn bit_length(&self) -> i32 {
        if self.neg {
            self.abs().sub(&BigInt::from_i64(1)).bit_length_mag() as i32
        } else {
            self.bit_length_mag() as i32
        }
    }

    pub fn bit_count(&self) -> i32 {
        let m = if self.neg { self.abs().sub(&BigInt::from_i64(1)).mag } else { self.mag.clone() };
        m.iter().map(|x| x.count_ones() as i32).sum()
    }

    pub fn lowest_set_bit(&self) -> i32 {
        for (i, x) in self.mag.iter().enumerate() {
            if *x != 0 {
                return (i * 32) as i32 + x.trailing_zeros() as i32;
            }
        }
        -1
    }

    /// Two's complement limbs, sign-extended to `n` limbs.
    fn tc(&self, n: usize) -> Vec<u32> {
        let mut v = self.mag.clone();
        v.resize(n, 0);
        if self.neg {
            let mut carry = 1u64;
            for x in v.iter_mut() {
                let t = (!*x) as u64 + carry;
                *x = t as u32;
                carry = t >> 32;
            }
        }
        v
    }

    fn from_tc(v: Vec<u32>) -> BigInt {
        let neg = v.last().is_some_and(|x| x >> 31 == 1);
        if !neg {
            return BigInt::from_parts(false, v);
        }
        let mut m = v;
        let mut carry = 1u64;
        for x in m.iter_mut() {
            let t = (!*x) as u64 + carry;
            *x = t as u32;
            carry = t >> 32;
        }
        BigInt::from_parts(true, m)
    }

    fn bitwise(&self, o: &BigInt, f: impl Fn(u32, u32) -> u32) -> BigInt {
        let n = self.mag.len().max(o.mag.len()) + 1;
        let (a, b) = (self.tc(n), o.tc(n));
        BigInt::from_tc(a.iter().zip(b.iter()).map(|(x, y)| f(*x, *y)).collect())
    }

    pub fn not(&self) -> BigInt {
        // ~x == -x - 1
        self.negate().sub(&BigInt::from_i64(1))
    }

    pub fn shift_left(&self, n: i32) -> BigInt {
        if self.mag.is_empty() {
            return BigInt::zero();
        }
        if n == i32::MIN {
            // `shiftRightImpl(-n)` with `-n` still negative: every bit is shifted out
            return if self.neg { BigInt::from_i64(-1) } else { BigInt::zero() };
        }
        if n < 0 {
            return self.shift_right(-n);
        }
        if mag_bits(&self.mag) + n as u64 > MAX_MAG_BITS {
            return big_overflow(crate::guard::BigSite::ShiftLeft);
        }
        BigInt::from_parts(self.neg, shl_mag(&self.mag, n as usize))
    }

    /// Arithmetic shift (floor for negatives).
    pub fn shift_right(&self, n: i32) -> BigInt {
        if self.mag.is_empty() {
            return BigInt::zero();
        }
        if n == i32::MIN {
            // a left shift by 2^31
            return big_overflow(crate::guard::BigSite::ShiftRight);
        }
        if n < 0 {
            return self.shift_left(-n);
        }
        let n = n as usize;
        let q = shr_mag(&self.mag, n);
        if self.neg {
            // round toward negative infinity when bits were shifted out
            let lost = (0..n.min(self.mag.len() * 32)).any(|i| self.mag[i / 32] >> (i % 32) & 1 == 1);
            let r = BigInt::from_parts(true, q);
            if lost {
                return r.sub(&BigInt::from_i64(1));
            }
            return r;
        }
        BigInt::from_parts(false, q)
    }

    pub fn test_bit(&self, n: i32) -> bool {
        let limbs = n as usize / 32 + 2;
        let v = self.tc(limbs.max(self.mag.len() + 1));
        v[n as usize / 32] >> (n as usize % 32) & 1 == 1
    }

    fn with_bit(&self, n: i32, f: impl Fn(u32, u32) -> u32) -> BigInt {
        let limbs = (n as usize / 32 + 2).max(self.mag.len() + 1);
        let mut v = self.tc(limbs);
        let i = n as usize / 32;
        v[i] = f(v[i], 1 << (n as usize % 32));
        BigInt::from_tc(v)
    }

    /// The low 64 bits of the two's complement (`longValue`).
    pub fn low_i64(&self) -> i64 {
        let v = self.tc(self.mag.len().max(2) + 1);
        (v[0] as u64 | (v[1] as u64) << 32) as i64
    }

    pub fn to_i64_exact(&self) -> Option<i64> {
        if self.bit_length() <= 63 {
            Some(self.low_i64())
        } else {
            None
        }
    }

    pub fn to_f64(&self) -> f64 {
        self.to_string().parse().unwrap_or(0.0)
    }

    /// `BigInteger.hashCode`.
    pub fn java_hash(&self) -> i32 {
        let mut h: i32 = 0;
        for limb in self.mag.iter().rev() {
            h = h.wrapping_mul(31).wrapping_add(*limb as i32);
        }
        h.wrapping_mul(self.signum())
    }

    /// The magnitude in a radix (2..=36), lowercase, without sign.
    pub fn abs_radix(&self, radix: u32) -> String {
        if self.mag.is_empty() {
            return "0".into();
        }
        mag_to_radix(&self.mag, radix)
    }

    pub fn to_string_radix(&self, radix: u32) -> String {
        let radix = if (2..=36).contains(&radix) { radix } else { 10 };
        let s = self.abs_radix(radix);
        if self.neg {
            format!("-{s}")
        } else {
            s
        }
    }

    /// `toByteArray`: minimal big-endian two's complement.
    pub fn to_byte_array(&self) -> Vec<u8> {
        let len = self.bit_length() as usize / 8 + 1;
        let v = self.tc(len / 4 + 2);
        (0..len).rev().map(|i| (v[i / 4] >> ((i % 4) * 8)) as u8).collect()
    }

    /// Number of decimal digits of the magnitude (1 for zero).
    fn digit_count(&self) -> i32 {
        if self.mag.is_empty() {
            return 1;
        }
        self.abs().to_string().len() as i32
    }
}

impl PartialOrd for BigInt {
    fn partial_cmp(&self, o: &BigInt) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for BigInt {
    fn cmp(&self, o: &BigInt) -> Ordering {
        match (self.neg, o.neg) {
            (false, true) => return Ordering::Greater,
            (true, false) => return Ordering::Less,
            _ => {}
        }
        let mag = cmp_mag(&self.mag, &o.mag);
        if self.neg {
            mag.reverse()
        } else {
            mag
        }
    }
}

impl std::fmt::Display for BigInt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.mag.is_empty() {
            return f.write_str("0");
        }
        if self.neg {
            f.write_str("-")?;
        }
        f.write_str(&mag_to_radix(&self.mag, 10))
    }
}

// ------------------------------------------------------------------------------ rounding

/// `java.math.RoundingMode`, by ordinal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    Up,
    Down,
    Ceiling,
    Floor,
    HalfUp,
    HalfDown,
    HalfEven,
    Unnecessary,
}

const ROUNDING_NAMES: [&str; 8] = ["UP", "DOWN", "CEILING", "FLOOR", "HALF_UP", "HALF_DOWN", "HALF_EVEN", "UNNECESSARY"];

impl Rounding {
    fn from_ordinal(o: u8) -> Rounding {
        use Rounding::*;
        [Up, Down, Ceiling, Floor, HalfUp, HalfDown, HalfEven, Unnecessary][o as usize % 8]
    }
    pub fn name(self) -> &'static str {
        ROUNDING_NAMES[self as usize]
    }
}

/// Divides `n` by `d` (both signed) rounding per `mode`.
fn div_round(n: &BigInt, d: &BigInt, mode: Rounding) -> Result<BigInt, Thrown> {
    let (q, r) = n.divrem(d)?;
    if r.is_zero() {
        return Ok(q);
    }
    let positive = n.neg == d.neg;
    let twice = r.abs().mul_small(2);
    let half = cmp_mag(&twice.mag, &d.mag);
    let away = match mode {
        Rounding::Up => true,
        Rounding::Down => false,
        Rounding::Ceiling => positive,
        Rounding::Floor => !positive,
        Rounding::HalfUp => half != Ordering::Less,
        Rounding::HalfDown => half == Ordering::Greater,
        Rounding::HalfEven => half == Ordering::Greater || (half == Ordering::Equal && q.mag.first().is_some_and(|x| x & 1 == 1)),
        Rounding::Unnecessary => return Err(arith("Rounding necessary")),
    };
    Ok(if away { q.add(&BigInt::from_i64(if positive { 1 } else { -1 })) } else { q })
}

// ------------------------------------------------------------------------------ BigDec

/// `java.math.BigDecimal`: `unscaled × 10^-scale`.
#[derive(Debug, Clone)]
pub struct BigDec {
    unscaled: BigInt,
    scale: i32,
}

/// `MathContext` (precision 0 is unlimited).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MathCtx {
    pub precision: u32,
    pub mode: Rounding,
}

fn nfe(msg: &str) -> Thrown {
    Throw::new(cls::NUMBER_FORMAT_EXCEPTION, Some(msg.to_string()))
}

impl BigDec {
    pub fn new(unscaled: BigInt, scale: i32) -> BigDec {
        BigDec { unscaled, scale }
    }

    pub fn from_i64(v: i64) -> BigDec {
        BigDec::new(BigInt::from_i64(v), 0)
    }

    /// `new BigDecimal(String)`.
    pub fn parse(s: &str) -> Result<BigDec, Thrown> {
        let b = s.as_bytes();
        let mut i = 0;
        let mut neg = false;
        if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
            neg = b[i] == b'-';
            i += 1;
        }
        let mut digits = String::new();
        let mut scale: i64 = 0;
        let mut seen_dot = false;
        while i < b.len() {
            let c = b[i];
            if c.is_ascii_digit() {
                digits.push(c as char);
                if seen_dot {
                    scale += 1;
                }
            } else if c == b'.' {
                if seen_dot {
                    return Err(jm(nfe("Character array contains more than one decimal point."), &["BigDecimal.<init>(BigDecimal.java:603)"]));
                }
                seen_dot = true;
            } else if c == b'e' || c == b'E' {
                break;
            } else {
                let ch = s[i..].chars().next().unwrap_or('?');
                return Err(jm(
                    nfe(&format!("Character {ch} is neither a decimal digit number, decimal point, nor \"e\" notation exponential mark.")),
                    &["BigDecimal.<init>(BigDecimal.java:626)"],
                ));
            }
            i += 1;
        }
        if digits.is_empty() || (i < b.len() && i + 1 >= b.len()) {
            // Java walks past the end of the character array
            let idx = if digits.is_empty() { s.len() } else { b.len() };
            let cause = Throw::new(cls::ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index {idx} out of bounds for length {}", b.len())));
            let mut t = Throw::new(cls::NUMBER_FORMAT_EXCEPTION, None).with_cause(&cause);
            t.frames = vec!["java.base/java.math.BigDecimal.<init>(BigDecimal.java:721)".into()];
            if !digits.is_empty() || s.is_empty() {
                return Err(t);
            }
            return Err(nfe("No digits found."));
        }
        if i < b.len() {
            // exponent
            i += 1;
            let exp_str = &s[i..];
            let e: i64 = match exp_str.parse::<i64>() {
                Ok(e) if !exp_str.is_empty() && !exp_str.starts_with("+-") => e,
                _ => return Err(nfe("No exponent digits.")),
            };
            scale -= e;
        }
        if scale > i32::MAX as i64 || scale < i32::MIN as i64 {
            return Err(nfe("Scale out of range."));
        }
        let u = BigInt::parse(&digits).unwrap_or_default();
        Ok(BigDec::new(if neg { u.negate() } else { u }, scale as i32))
    }

    /// `BigDecimal.valueOf(double)`: the value of `Double.toString(d)`.
    pub fn value_of(d: f64) -> Result<BigDec, Thrown> {
        if !d.is_finite() {
            return Err(nfe("Infinite or NaN"));
        }
        BigDec::parse(&crate::java::fmt_double(d))
    }

    pub fn signum(&self) -> i32 {
        self.unscaled.signum()
    }

    pub fn scale(&self) -> i32 {
        self.scale
    }

    pub fn precision(&self) -> i32 {
        self.unscaled.digit_count()
    }

    pub fn unscaled(&self) -> &BigInt {
        &self.unscaled
    }

    fn with_scale_exact(&self, s: i32) -> BigDec {
        if s >= self.scale {
            BigDec::new(self.unscaled.mul(&BigInt::pow10((s - self.scale) as u32)), s)
        } else {
            BigDec::new(self.unscaled.divrem(&BigInt::pow10((self.scale - s) as u32)).unwrap().0, s)
        }
    }

    pub fn set_scale(&self, s: i32, mode: Rounding) -> Result<BigDec, Thrown> {
        if s >= self.scale {
            return Ok(self.with_scale_exact(s));
        }
        let d = BigInt::pow10((self.scale as i64 - s as i64) as u32);
        Ok(BigDec::new(div_round(&self.unscaled, &d, mode)?, s))
    }

    pub fn add(&self, o: &BigDec) -> BigDec {
        let s = self.scale.max(o.scale);
        BigDec::new(self.with_scale_exact(s).unscaled.add(&o.with_scale_exact(s).unscaled), s)
    }

    pub fn sub(&self, o: &BigDec) -> BigDec {
        self.add(&o.negate())
    }

    pub fn negate(&self) -> BigDec {
        BigDec::new(self.unscaled.negate(), self.scale)
    }

    pub fn abs(&self) -> BigDec {
        BigDec::new(self.unscaled.abs(), self.scale)
    }

    pub fn mul(&self, o: &BigDec) -> Result<BigDec, Thrown> {
        let s = self.scale as i64 + o.scale as i64;
        let s = i32::try_from(s).map_err(|_| arith(if s > 0 { "Underflow" } else { "Overflow" }))?;
        Ok(BigDec::new(self.unscaled.mul(&o.unscaled), s))
    }

    /// `round(MathContext)`.
    pub fn round(&self, mc: MathCtx) -> Result<BigDec, Thrown> {
        let mut r = self.clone();
        if mc.precision == 0 {
            return Ok(r);
        }
        loop {
            let drop = r.precision() - mc.precision as i32;
            if drop <= 0 {
                return Ok(r);
            }
            let d = BigInt::pow10(drop as u32);
            r = BigDec::new(div_round(&r.unscaled, &d, mc.mode)?, r.scale - drop);
        }
    }

    /// Removes trailing zeros while the scale stays above `preferred`.
    fn strip_to(&self, preferred: i64) -> BigDec {
        let mut u = self.unscaled.clone();
        let mut s = self.scale as i64;
        let ten = BigInt::from_i64(10);
        while s > preferred && !u.is_zero() {
            let (q, r) = u.divrem(&ten).unwrap();
            if !r.is_zero() {
                break;
            }
            u = q;
            s -= 1;
        }
        BigDec::new(u, s as i32)
    }

    pub fn strip_trailing_zeros(&self) -> BigDec {
        if self.unscaled.is_zero() {
            return BigDec::new(BigInt::zero(), 0);
        }
        self.strip_to(i64::MIN)
    }

    /// `divide(divisor, mc)` with Java's preferred scale.
    pub fn divide_mc(&self, o: &BigDec, mc: MathCtx) -> Result<BigDec, Thrown> {
        if o.signum() == 0 {
            return Err(arith(if self.signum() == 0 { "Division undefined" } else { "Division by zero" }));
        }
        let preferred = self.scale as i64 - o.scale as i64;
        if self.signum() == 0 {
            return Ok(BigDec::new(BigInt::zero(), preferred.clamp(i32::MIN as i64, i32::MAX as i64) as i32));
        }
        if mc.precision == 0 {
            return self.divide(o);
        }
        // a = u1 / 10^s1, b = u2 / 10^s2: choose a result scale with p + 1 significant digits
        let adj = (self.precision() as i64 - self.scale as i64) - (o.precision() as i64 - o.scale as i64);
        let target_scale = mc.precision as i64 - adj + 1;
        // quotient at target_scale: u1 * 10^(target_scale + s2 - s1) / u2
        let shift = target_scale + o.scale as i64 - self.scale as i64;
        let (num, den) = if shift >= 0 {
            (self.unscaled.mul(&BigInt::pow10(shift as u32)), o.unscaled.clone())
        } else {
            (self.unscaled.clone(), o.unscaled.mul(&BigInt::pow10((-shift) as u32)))
        };
        let (q, r) = num.divrem(&den)?;
        // fold the remainder into a sticky digit so the final rounding sees it
        let exact = r.is_zero();
        let mut val = BigDec::new(q.mul_small(10).add(&BigInt::from_i64(if exact { 0 } else if num.neg == den.neg { 1 } else { -1 })), (target_scale + 1) as i32);
        val = val.round(mc)?;
        if mc.mode == Rounding::Unnecessary && !exact {
            return Err(arith("Rounding necessary"));
        }
        Ok(val.strip_to(preferred))
    }

    /// `divide(divisor, scale, roundingMode)`.
    pub fn divide_rounded(&self, o: &BigDec, scale: i32, mode: Rounding) -> Result<BigDec, Thrown> {
        if o.unscaled.is_zero() {
            return Err(arith(if self.unscaled.is_zero() { "Division undefined" } else { "Division by zero" }));
        }
        let e = scale as i64 + o.scale as i64 - self.scale as i64;
        let (n, d) = if e >= 0 {
            (self.unscaled.mul(&BigInt::pow10(e as u32)), o.unscaled.clone())
        } else {
            (self.unscaled.clone(), o.unscaled.mul(&BigInt::pow10((-e) as u32)))
        };
        Ok(BigDec::new(div_round(&n, &d, mode)?, scale))
    }

    /// `divide(divisor)`: exact or "Non-terminating decimal expansion".
    pub fn divide(&self, o: &BigDec) -> Result<BigDec, Thrown> {
        if o.signum() == 0 {
            return Err(arith(if self.signum() == 0 { "Division undefined" } else { "Division by zero" }));
        }
        let preferred = self.scale as i64 - o.scale as i64;
        if self.signum() == 0 {
            return Ok(BigDec::new(BigInt::zero(), preferred.clamp(i32::MIN as i64, i32::MAX as i64) as i32));
        }
        let p = (self.precision() as i64 + ((10.0 * o.precision() as f64) / 3.0).ceil() as i64).min(i32::MAX as i64) as u32;
        let q = self
            .divide_mc(o, MathCtx { precision: p, mode: Rounding::Unnecessary })
            .map_err(|_| arith("Non-terminating decimal expansion; no exact representable decimal result."))?;
        if preferred > q.scale as i64 {
            return q.set_scale(preferred as i32, Rounding::Unnecessary);
        }
        Ok(q)
    }

    /// `divideToIntegralValue`.
    pub fn divide_to_integral(&self, o: &BigDec) -> Result<BigDec, Thrown> {
        if o.signum() == 0 {
            return Err(arith(if self.signum() == 0 { "Division undefined" } else { "Division by zero" }));
        }
        let preferred = self.scale as i64 - o.scale as i64;
        // integer part of (u1/10^s1) / (u2/10^s2) = u1 * 10^s2 / (u2 * 10^s1)
        let (num, den) = (self.unscaled.mul(&BigInt::pow10(o.scale.max(0) as u32)), o.unscaled.mul(&BigInt::pow10(self.scale.max(0) as u32)));
        let (num, den) = if self.scale < 0 { (num.mul(&BigInt::pow10((-self.scale) as u32)), den) } else { (num, den) };
        let (num, den) = if o.scale < 0 { (num, den.mul(&BigInt::pow10((-o.scale) as u32))) } else { (num, den) };
        let q = num.divrem(&den)?.0;
        let q = BigDec::new(q, 0);
        if preferred > 0 {
            return q.set_scale(preferred as i32, Rounding::Unnecessary);
        }
        Ok(q.strip_to(preferred))
    }

    pub fn remainder(&self, o: &BigDec) -> Result<BigDec, Thrown> {
        let q = self.divide_to_integral(o)?;
        Ok(self.sub(&q.mul(o)?))
    }

    pub fn pow(&self, n: i32) -> Result<BigDec, Thrown> {
        if !(0..=999_999_999).contains(&n) {
            return Err(arith("Invalid operation"));
        }
        let s = self.scale as i64 * n as i64;
        let s = i32::try_from(s).map_err(|_| arith(if s > 0 { "Underflow" } else { "Overflow" }))?;
        Ok(BigDec::new(self.unscaled.pow(n as u32), s))
    }

    pub fn move_point(&self, n: i64) -> Result<BigDec, Thrown> {
        let s = self.scale as i64 + n;
        let s = i32::try_from(s).map_err(|_| arith(if s > 0 { "Underflow" } else { "Overflow" }))?;
        let r = BigDec::new(self.unscaled.clone(), s);
        Ok(if r.scale < 0 { r.with_scale_exact(0) } else { r })
    }

    pub fn compare(&self, o: &BigDec) -> Ordering {
        let s = self.scale.max(o.scale);
        self.with_scale_exact(s).unscaled.cmp(&o.with_scale_exact(s).unscaled)
    }

    pub fn java_equals(&self, o: &BigDec) -> bool {
        self.unscaled == o.unscaled && self.scale == o.scale
    }

    pub fn java_hash(&self) -> i32 {
        match self.unscaled.to_i64_exact() {
            Some(v) => {
                let a = v.unsigned_abs();
                let temp = (((a >> 32) as i32).wrapping_mul(31) as i64 + (a & 0xffff_ffff) as i64) as i32;
                31i32.wrapping_mul(if v < 0 { temp.wrapping_neg() } else { temp }).wrapping_add(self.scale)
            }
            None => 31i32.wrapping_mul(self.unscaled.java_hash()).wrapping_add(self.scale),
        }
    }

    pub fn to_big_int(&self) -> BigInt {
        if self.scale <= 0 {
            self.with_scale_exact(0).unscaled
        } else {
            self.unscaled.divrem(&BigInt::pow10(self.scale as u32)).unwrap().0
        }
    }

    pub fn to_f64(&self) -> f64 {
        self.to_java_string().parse().unwrap_or(0.0)
    }

    /// `toString`: plain when the scale is non-negative and the exponent is not too small,
    /// otherwise scientific (`1.23E+5`).
    pub fn to_java_string(&self) -> String {
        self.layout(true)
    }

    pub fn to_engineering_string(&self) -> String {
        self.layout(false)
    }

    fn layout(&self, sci: bool) -> String {
        let coeff = self.unscaled.abs().to_string();
        let neg = self.unscaled.signum() < 0;
        let mut out = String::new();
        if neg {
            out.push('-');
        }
        if self.scale == 0 {
            out.push_str(&coeff);
            return out;
        }
        let len = coeff.len() as i64;
        let adjusted = -(self.scale as i64) + (len - 1);
        if self.scale >= 0 && adjusted >= -6 {
            // plain
            let pad = self.scale as i64 - len;
            if pad >= 0 {
                out.push_str("0.");
                out.push_str(&"0".repeat(pad as usize));
                out.push_str(&coeff);
            } else {
                let k = (-pad) as usize;
                out.push_str(&coeff[..k]);
                out.push('.');
                out.push_str(&coeff[k..]);
            }
            return out;
        }
        if sci {
            out.push_str(&coeff[..1]);
            if len > 1 {
                out.push('.');
                out.push_str(&coeff[1..]);
            }
            if adjusted != 0 {
                out.push('E');
                if adjusted > 0 {
                    out.push('+');
                }
                out.push_str(&adjusted.to_string());
            }
            return out;
        }
        // engineering notation
        let mut sig = (adjusted % 3) as i32;
        if sig < 0 {
            sig += 3;
        }
        let mut adjusted = adjusted - sig as i64;
        let mut sig = sig + 1;
        if self.unscaled.is_zero() {
            match sig {
                1 => out.push('0'),
                2 => {
                    out.push_str("0.00");
                    adjusted += 3;
                }
                3 => {
                    out.push_str("0.0");
                    adjusted += 3;
                }
                _ => {}
            }
        } else if sig as i64 >= len {
            out.push_str(&coeff);
            for _ in len..sig as i64 {
                out.push('0');
            }
        } else {
            out.push_str(&coeff[..sig as usize]);
            out.push('.');
            out.push_str(&coeff[sig as usize..]);
        }
        if adjusted != 0 {
            out.push('E');
            if adjusted > 0 {
                out.push('+');
            }
            out.push_str(&adjusted.to_string());
        }
        sig = 0;
        let _ = sig;
        out
    }

    pub fn to_plain_string(&self) -> String {
        let coeff = self.unscaled.abs().to_string();
        let mut out = String::new();
        if self.unscaled.signum() < 0 {
            out.push('-');
        }
        if self.scale <= 0 {
            out.push_str(&coeff);
            if !self.unscaled.is_zero() {
                out.push_str(&"0".repeat((-self.scale) as usize));
            }
            return out;
        }
        let len = coeff.len() as i64;
        let pad = self.scale as i64 - len;
        if pad >= 0 {
            out.push_str("0.");
            out.push_str(&"0".repeat(pad as usize));
            out.push_str(&coeff);
        } else {
            let k = (-pad) as usize;
            out.push_str(&coeff[..k]);
            out.push('.');
            out.push_str(&coeff[k..]);
        }
        out
    }
}

// ------------------------------------------------------------------------------ objects

/// `MathContext` and `RoundingMode` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MathObj {
    Context(MathCtx),
    Mode(Rounding),
}

impl MathObj {
    pub fn class(&self) -> crate::types::ClassId {
        match self {
            MathObj::Context(_) => cls::MATH_CONTEXT,
            MathObj::Mode(_) => cls::ROUNDING_MODE,
        }
    }
    pub fn to_java_string(self) -> String {
        match self {
            MathObj::Context(c) => format!("precision={} roundingMode={}", c.precision, c.mode.name()),
            MathObj::Mode(m) => m.name().to_string(),
        }
    }
    pub fn java_hash(&self) -> i32 {
        match self {
            MathObj::Context(c) => (c.mode as i32 + 1).wrapping_mul(59).wrapping_add(c.precision as i32),
            MathObj::Mode(m) => crate::java::string_hash(m.name()),
        }
    }
}

/// A `BigInteger` value from decimal digits (Jackson's numbers beyond `long`).
pub fn big_integer_value(s: &str) -> Value {
    match BigInt::parse(s) {
        Some(b) => bi(b),
        None => Value::Null,
    }
}

fn bi(b: BigInt) -> Value {
    Value::obj(Obj::BigInt(Rc::new(b)))
}

fn bd(b: BigDec) -> Value {
    Value::obj(Obj::BigDec(Rc::new(b)))
}

fn big_of(v: &Value) -> Result<Rc<BigInt>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::BigInt(b) => Ok(b.clone()),
            _ => Err(crate::vm::cce("not a BigInteger")),
        },
        _ => Err(crate::vm::npe("Cannot read field \"signum\" because \"val\" is null")),
    }
}

fn dec_of(v: &Value) -> Result<Rc<BigDec>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::BigDec(b) => Ok(b.clone()),
            _ => Err(crate::vm::cce("not a BigDecimal")),
        },
        _ => Err(crate::vm::npe("Cannot read field \"intCompact\" because \"augend\" is null")),
    }
}

fn mc_of(v: &Value) -> Result<MathCtx, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Math(MathObj::Context(c)) => Ok(*c),
            _ => Err(crate::vm::cce("not a MathContext")),
        },
        _ => Err(crate::vm::npe("Cannot read field \"precision\" because \"mc\" is null")),
    }
}

fn rm_of(v: &Value) -> Result<Rounding, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Math(MathObj::Mode(m)) => Ok(*m),
            _ => Err(crate::vm::cce("not a RoundingMode")),
        },
        _ => Err(crate::vm::npe("Cannot invoke \"java.math.RoundingMode.ordinal()\" because \"roundingMode\" is null")),
    }
}

fn math(o: MathObj) -> Value {
    Value::obj(Obj::Math(o))
}

fn out_of_range(kind: &str) -> Thrown {
    arith(&format!("BigInteger out of {kind} range"))
}

// BigInteger natives
fn i_new(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = a[0].to_java_string();
    i_parse(&s, 10).map_err(|t| jm(t, &["BigInteger.<init>(BigInteger.java:721)"]))
}
fn i_new_radix(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = a[0].to_java_string();
    let r = int(&a[1]);
    if !(2..=36).contains(&r) {
        return Err(jm(nfe("Radix out of range"), &["BigInteger.<init>(BigInteger.java:531)"]));
    }
    i_parse(&s, r as u32)
}
const DIGITS_PER_INT: [usize; 37] = [0, 0, 30, 19, 15, 13, 11, 11, 10, 9, 9, 8, 8, 8, 8, 7, 7, 7, 7, 7, 7, 7, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 5];
fn i_parse(s: &str, radix: u32) -> VR {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return Err(jm(nfe("Zero length BigInteger"), &["BigInteger.<init>(BigInteger.java:533)"]));
    }
    let mut cursor = 0;
    if chars[0] == '-' || chars[0] == '+' {
        cursor = 1;
        if chars.len() == 1 {
            return Err(jm(nfe("Zero length BigInteger"), &["BigInteger.<init>(BigInteger.java:546)"]));
        }
    }
    if chars[cursor..].iter().any(|c| *c == '-' || *c == '+') {
        return Err(jm(nfe("Illegal embedded sign character"), &["BigInteger.<init>(BigInteger.java:541)"]));
    }
    while cursor < chars.len() && chars[cursor].to_digit(radix) == Some(0) {
        cursor += 1;
    }
    // Integer.parseInt over digit groups: the first failing group names the error
    let per = DIGITS_PER_INT[radix as usize];
    let rest = &chars[cursor..];
    if !rest.is_empty() {
        let mut first = rest.len() % per;
        if first == 0 {
            first = per;
        }
        let mut start = 0;
        let mut len = first;
        let mut line = 583;
        while start < rest.len() {
            let group: String = rest[start..start + len].iter().collect();
            if group.chars().any(|c| c.to_digit(radix).is_none()) {
                let msg = if radix == 10 { format!("For input string: \"{group}\"") } else { format!("For input string: \"{group}\" under radix {radix}") };
                return Err(jm(
                    nfe(&msg),
                    &["java.lang.NumberFormatException.forInputString(NumberFormatException.java:67)", "java.lang.Integer.parseInt(Integer.java:529)", &format!("BigInteger.<init>(BigInteger.java:{line})")],
                ));
            }
            start += len;
            len = per;
            line = 592;
        }
    }
    match BigInt::parse_radix(s, radix) {
        Some(b) => Ok(bi(b)),
        None => Err(nfe(&format!("For input string: \"{s}\""))),
    }
}
fn i_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bi(BigInt::from_i64(long(&a[0]))))
}
macro_rules! bin_i {
    ($name:ident, |$x:ident, $y:ident| $e:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            let $x = big_of(&a[0])?;
            let $y = big_of(&a[1])?;
            Ok($e)
        }
    };
}
bin_i!(i_add, |x, y| bi(x.add(&y)));
bin_i!(i_sub, |x, y| bi(x.sub(&y)));
bin_i!(i_mul, |x, y| bi(x.mul(&y)));
bin_i!(i_and, |x, y| bi(x.bitwise(&y, |p, q| p & q)));
bin_i!(i_or, |x, y| bi(x.bitwise(&y, |p, q| p | q)));
bin_i!(i_xor, |x, y| bi(x.bitwise(&y, |p, q| p ^ q)));
bin_i!(i_and_not, |x, y| bi(x.bitwise(&y, |p, q| p & !q)));
bin_i!(i_gcd, |x, y| bi(x.gcd(&y)));
bin_i!(i_max, |x, y| if *x >= *y { bi((*x).clone()) } else { bi((*y).clone()) });
bin_i!(i_min, |x, y| if *x <= *y { bi((*x).clone()) } else { bi((*y).clone()) });
bin_i!(i_compare, |x, y| Value::Int(x.cmp(&y) as i32));
fn i_div(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bi(big_of(&a[0])?
        .divrem(&*big_of(&a[1])?)
        .map_err(|t| jm(t, &["MutableBigInteger.divideKnuth(MutableBigInteger.java:1271)", "BigInteger.divideKnuth(BigInteger.java:2492)", "BigInteger.divide(BigInteger.java:2473)"]))?
        .0))
}
fn i_rem(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bi(big_of(&a[0])?
        .divrem(&*big_of(&a[1])?)
        .map_err(|t| {
            jm(
                t,
                &[
                    "MutableBigInteger.divideKnuth(MutableBigInteger.java:1271)",
                    "MutableBigInteger.divideKnuth(MutableBigInteger.java:1255)",
                    "BigInteger.remainderKnuth(BigInteger.java:2551)",
                    "BigInteger.remainder(BigInteger.java:2539)",
                ],
            )
        })?
        .1))
}
fn i_divrem(_: &mut Vm, a: &mut [Value]) -> VR {
    let (q, r) = big_of(&a[0])?.divrem(&*big_of(&a[1])?).map_err(|t| {
        jm(
            t,
            &[
                "MutableBigInteger.divideKnuth(MutableBigInteger.java:1271)",
                "MutableBigInteger.divideKnuth(MutableBigInteger.java:1255)",
                "BigInteger.divideAndRemainderKnuth(BigInteger.java:2522)",
                "BigInteger.divideAndRemainder(BigInteger.java:2510)",
            ],
        )
    })?;
    Ok(Value::obj(Obj::Array(JArray::from_data(Ty::of(cls::BIG_INTEGER), ArrayData::Ref(vec![bi(q), bi(r)])))))
}
fn i_mod(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bi(big_of(&a[0])?.modulo(&*big_of(&a[1])?).map_err(|t| jm(t, &["BigInteger.mod(BigInteger.java:2960)"]))?))
}
fn i_mod_inverse(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bi(big_of(&a[0])?.mod_inverse(&*big_of(&a[1])?).map_err(|t| {
        if t.message.as_deref() == Some("BigInteger not invertible.") {
            jm(t, &["MutableBigInteger.mutableModInverse(MutableBigInteger.java:2376)", "BigInteger.modInverse(BigInteger.java:3558)"])
        } else {
            jm(t, &["BigInteger.modInverse(BigInteger.java:3542)"])
        }
    })?))
}
fn i_mod_pow(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = big_of(&a[2])?;
    if m.signum() <= 0 {
        return Err(jm(arith("BigInteger: modulus not positive"), &["BigInteger.modPow(BigInteger.java:2981)"]));
    }
    Ok(bi(big_of(&a[0])?.mod_pow(&*big_of(&a[1])?, &m)?))
}
fn i_pow(_: &mut Vm, a: &mut [Value]) -> VR {
    let e = int(&a[1]);
    if e < 0 {
        return Err(jm(arith("Negative exponent"), &["BigInteger.pow(BigInteger.java:2597)"]));
    }
    Ok(bi(in_range(big_of(&a[0])?.pow(e as u32), &[])?))
}
fn i_abs(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bi(big_of(&a[0])?.abs()))
}
fn i_negate(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bi(big_of(&a[0])?.negate()))
}
fn i_not(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bi(big_of(&a[0])?.not()))
}
fn i_signum(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(big_of(&a[0])?.signum()))
}
fn i_bit_count(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(big_of(&a[0])?.bit_count()))
}
fn i_bit_length(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(big_of(&a[0])?.bit_length()))
}
fn i_lowest(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(big_of(&a[0])?.lowest_set_bit()))
}
fn bit_arg(v: &Value, frame: &str) -> Result<i32, Thrown> {
    let n = int(v);
    if n < 0 {
        return Err(jm(arith("Negative bit address"), &[frame]));
    }
    Ok(n)
}
fn i_test_bit(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = bit_arg(&a[1], "BigInteger.testBit(BigInteger.java:3829)")?;
    Ok(Value::Bool(big_of(&a[0])?.test_bit(n)))
}
fn i_set_bit(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = bit_arg(&a[1], "BigInteger.setBit(BigInteger.java:3844)")?;
    Ok(bi(big_of(&a[0])?.with_bit(n, |x, m| x | m)))
}
fn i_clear_bit(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = bit_arg(&a[1], "BigInteger.clearBit(BigInteger.java:3868)")?;
    Ok(bi(big_of(&a[0])?.with_bit(n, |x, m| x & !m)))
}
fn i_flip_bit(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = bit_arg(&a[1], "BigInteger.flipBit(BigInteger.java:3892)")?;
    Ok(bi(big_of(&a[0])?.with_bit(n, |x, m| x ^ m)))
}
fn i_shl(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bi(in_range(big_of(&a[0])?.shift_left(int(&a[1])), &[])?))
}
fn i_shr(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bi(in_range(big_of(&a[0])?.shift_right(int(&a[1])), &[])?))
}
fn i_to_bytes(_: &mut Vm, a: &mut [Value]) -> VR {
    let b = big_of(&a[0])?.to_byte_array();
    Ok(Value::obj(Obj::Array(JArray::from_data(Ty::BYTE, ArrayData::Byte(b.into_iter().map(|x| x as i8).collect())))))
}
fn i_to_string_radix(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(big_of(&a[0])?.to_string_radix(int(&a[1]) as u32)))
}
fn i_exact(a: &[Value], bits: u32, kind: &str) -> Result<i64, Thrown> {
    let b = big_of(&a[0])?;
    if (b.bit_length() as u32) < bits {
        Ok(b.low_i64())
    } else {
        let (m, line) = match kind {
            "long" => ("longValueExact", 5131),
            "int" => ("intValueExact", 5154),
            "short" => ("shortValueExact", 5173),
            _ => ("byteValueExact", 5192),
        };
        Err(jm(out_of_range(kind), &[&format!("BigInteger.{m}(BigInteger.java:{line})")]))
    }
}
fn i_long_exact(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(i_exact(a, 64, "long")?))
}
fn i_int_exact(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(i_exact(a, 32, "int")? as i32))
}
fn i_short_exact(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Short(i_exact(a, 16, "short")? as i16))
}
fn i_byte_exact(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Byte(i_exact(a, 8, "byte")? as i8))
}
fn i_int_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(big_of(&a[0])?.low_i64() as i32))
}
fn i_long_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(big_of(&a[0])?.low_i64()))
}
fn i_short_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Short(big_of(&a[0])?.low_i64() as i16))
}
fn i_byte_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Byte(big_of(&a[0])?.low_i64() as i8))
}
fn i_double_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(big_of(&a[0])?.to_f64()))
}
fn i_float_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Float(big_of(&a[0])?.to_string().parse::<f32>().unwrap_or(0.0)))
}

// BigDecimal natives
fn d_new(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[0].is_null() {
        return Err(crate::vm::npe("Cannot invoke \"String.toCharArray()\" because \"val\" is null"));
    }
    Ok(bd(BigDec::parse(&a[0].to_java_string()).map_err(|t| jm(t, &["BigDecimal.<init>(BigDecimal.java:515)", "BigDecimal.<init>(BigDecimal.java:921)"]))?))
}
fn d_new_mc(_: &mut Vm, a: &mut [Value]) -> VR {
    let mc = mc_of(&a[1])?;
    Ok(bd(BigDec::parse(&a[0].to_java_string())?.round(mc)?))
}
fn d_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bd(BigDec::value_of(crate::lib_lang::double(&a[0])).map_err(|t| jm(t, &["BigDecimal.valueOf(BigDecimal.java:1390)"]))?))
}
macro_rules! bin_d {
    ($name:ident, $name_mc:ident, |$x:ident, $y:ident| $e:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            let $x = dec_of(&a[0])?;
            let $y = dec_of(&a[1])?;
            Ok(bd($e?))
        }
        fn $name_mc(_: &mut Vm, a: &mut [Value]) -> VR {
            let $x = dec_of(&a[0])?;
            let $y = dec_of(&a[1])?;
            let mc = mc_of(&a[2])?;
            let r: BigDec = $e?;
            Ok(bd(r.round(mc)?))
        }
    };
}
bin_d!(d_add, d_add_mc, |x, y| Ok::<BigDec, Thrown>(x.add(&y)));
bin_d!(d_sub, d_sub_mc, |x, y| Ok::<BigDec, Thrown>(x.sub(&y)));
bin_d!(d_mul, d_mul_mc, |x, y| x.mul(&y));
bin_d!(d_rem, d_rem_mc, |x, y| x.remainder(&y).map_err(|t| div_zero_frames(
    t,
    1862,
    1863,
    &["BigDecimal.divideToIntegralValue(BigDecimal.java:1913)", "BigDecimal.divideAndRemainder(BigDecimal.java:2068)", "BigDecimal.remainder(BigDecimal.java:2010)"]
)));
bin_d!(d_div_int, d_div_int_mc, |x, y| x
    .divide_to_integral(&y)
    .map_err(|t| div_zero_frames(t, 1862, 1863, &["BigDecimal.divideToIntegralValue(BigDecimal.java:1913)"])));
fn div_zero_frames(t: Thrown, line_undefined: u32, line_zero: u32, outer: &[&str]) -> Thrown {
    let line = match t.message.as_deref() {
        Some("Division undefined") => line_undefined,
        Some("Division by zero") => line_zero,
        _ => return t,
    };
    let f = format!("BigDecimal.divide(BigDecimal.java:{line})");
    let mut frames = vec![f.as_str()];
    frames.extend_from_slice(outer);
    jm(t, &frames)
}
fn d_div(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bd(dec_of(&a[0])?.divide(&*dec_of(&a[1])?).map_err(|t| {
        if t.message.as_deref().is_some_and(|m| m.starts_with("Non-terminating")) {
            jm(t, &["BigDecimal.divide(BigDecimal.java:1811)"])
        } else {
            div_zero_frames(t, 1785, 1786, &[])
        }
    })?))
}
fn d_div_mc(_: &mut Vm, a: &mut [Value]) -> VR {
    let mc = mc_of(&a[2])?;
    Ok(bd(dec_of(&a[0])?.divide_mc(&*dec_of(&a[1])?, mc).map_err(|t| {
        if t.message.as_deref() == Some("Rounding necessary") {
            let mut f: Vec<&str> = NEED_INCREMENT.to_vec();
            f.extend(["BigDecimal.divideSmallFastPath(BigDecimal.java:5493)", "BigDecimal.divide(BigDecimal.java:5508)", "BigDecimal.divide(BigDecimal.java:1871)"]);
            jm(t, &f)
        } else {
            div_zero_frames(t, 1862, 1863, &[])
        }
    })?))
}
fn d_divrem(_: &mut Vm, a: &mut [Value]) -> VR {
    let (x, y) = (dec_of(&a[0])?, dec_of(&a[1])?);
    let q = x
        .divide_to_integral(&y)
        .map_err(|t| div_zero_frames(t, 1862, 1863, &["BigDecimal.divideToIntegralValue(BigDecimal.java:1913)", "BigDecimal.divideAndRemainder(BigDecimal.java:2068)"]))?;
    let r = x.sub(&q.mul(&y)?);
    Ok(Value::obj(Obj::Array(JArray::from_data(Ty::of(cls::BIG_DECIMAL), ArrayData::Ref(vec![bd(q), bd(r)])))))
}
fn d_divrem_mc(vm: &mut Vm, a: &mut [Value]) -> VR {
    d_divrem(vm, a)
}
fn d_compare(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(dec_of(&a[0])?.compare(&*dec_of(&a[1])?) as i32))
}
fn d_max(_: &mut Vm, a: &mut [Value]) -> VR {
    let (x, y) = (dec_of(&a[0])?, dec_of(&a[1])?);
    Ok(if x.compare(&y) != Ordering::Less { a[0].clone() } else { a[1].clone() })
}
fn d_min(_: &mut Vm, a: &mut [Value]) -> VR {
    let (x, y) = (dec_of(&a[0])?, dec_of(&a[1])?);
    Ok(if x.compare(&y) != Ordering::Greater { a[0].clone() } else { a[1].clone() })
}
fn d_abs(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bd(dec_of(&a[0])?.abs()))
}
fn d_abs_mc(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bd(dec_of(&a[0])?.abs().round(mc_of(&a[1])?)?))
}
fn d_negate(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bd(dec_of(&a[0])?.negate()))
}
fn d_negate_mc(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bd(dec_of(&a[0])?.negate().round(mc_of(&a[1])?)?))
}
fn d_plus(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(a[0].clone())
}
fn d_plus_mc(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bd(dec_of(&a[0])?.round(mc_of(&a[1])?)?))
}
fn d_pow(_: &mut Vm, a: &mut [Value]) -> VR {
    let r = in_range(dec_of(&a[0])?.pow(int(&a[1])), &["BigDecimal.pow(BigDecimal.java:2527)"])?;
    Ok(bd(r.map_err(|t| jm(t, &["BigDecimal.pow(BigDecimal.java:2523)"]))?))
}
fn d_pow_mc(_: &mut Vm, a: &mut [Value]) -> VR {
    let mc = mc_of(&a[2])?;
    Ok(bd(dec_of(&a[0])?.pow(int(&a[1]))?.round(mc)?))
}
fn d_precision(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(dec_of(&a[0])?.precision()))
}
fn d_scale(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(dec_of(&a[0])?.scale()))
}
fn d_signum(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(dec_of(&a[0])?.signum()))
}
fn d_round(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bd(dec_of(&a[0])?.round(mc_of(&a[1])?).map_err(|t| {
        jm(
            t,
            &[
                "BigDecimal.commonNeedIncrement(BigDecimal.java:4963)",
                "BigDecimal.needIncrement(BigDecimal.java:5013)",
                "BigDecimal.divideAndRound(BigDecimal.java:4949)",
                "BigDecimal.doRound(BigDecimal.java:4825)",
                "BigDecimal.plus(BigDecimal.java:2702)",
                "BigDecimal.round(BigDecimal.java:2881)",
            ],
        )
    })?))
}
fn d_set_scale(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = dec_of(&a[0])?;
    let r = in_range(d.set_scale(int(&a[1]), Rounding::Unnecessary), &scale_frames(&d, int(&a[1]), "BigDecimal.setScale(BigDecimal.java:3041)"))?;
    Ok(bd(r.map_err(|t| rounding_frames(t, &["BigDecimal.setScale(BigDecimal.java:3041)"]))?))
}
fn d_set_scale_mode(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = rm_of(&a[2])?;
    let d = dec_of(&a[0])?;
    let r = in_range(d.set_scale(int(&a[1]), m), &scale_frames(&d, int(&a[1]), "BigDecimal.setScale(BigDecimal.java:2914)"))?;
    Ok(bd(r.map_err(|t| rounding_frames(t, &[]))?))
}
fn d_strip(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bd(dec_of(&a[0])?.strip_trailing_zeros()))
}
fn d_move_left(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bd(in_range(dec_of(&a[0])?.move_point(int(&a[1]) as i64), &[TEN_TO_THE, MULTIPLY_POWER_TEN, SET_SCALE_UP, "BigDecimal.movePointLeft(BigDecimal.java:3067)"])??))
}
fn d_move_right(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bd(in_range(dec_of(&a[0])?.move_point(-(int(&a[1]) as i64)), &[TEN_TO_THE, MULTIPLY_POWER_TEN, SET_SCALE_UP, "BigDecimal.movePointRight(BigDecimal.java:3091)"])??))
}

const TEN_TO_THE: &str = "BigDecimal.bigTenToThe(BigDecimal.java:4332)";
const MULTIPLY_POWER_TEN: &str = "BigDecimal.bigMultiplyPowerTen(BigDecimal.java:4469)";
const SET_SCALE_UP: &str = "BigDecimal.setScale(BigDecimal.java:2974)";

/// The frames between `BigInteger.pow` and `setScale(…)` (`entry`): raising the scale multiplies
/// by a power of ten, lowering it divides.
fn scale_frames(d: &BigDec, new_scale: i32, entry: &'static str) -> Vec<&'static str> {
    if new_scale >= d.scale() {
        vec![TEN_TO_THE, MULTIPLY_POWER_TEN, SET_SCALE_UP, entry]
    } else {
        vec![TEN_TO_THE, "BigDecimal.setScale(BigDecimal.java:2983)", entry]
    }
}
fn d_scale_by_pow10(_: &mut Vm, a: &mut [Value]) -> VR {
    let x = dec_of(&a[0])?;
    let s = x.scale as i64 - int(&a[1]) as i64;
    let s = i32::try_from(s).map_err(|_| arith(if s > 0 { "Underflow" } else { "Overflow" }))?;
    Ok(bd(BigDec::new(x.unscaled.clone(), s)))
}
fn d_ulp(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bd(BigDec::new(BigInt::from_i64(1), dec_of(&a[0])?.scale)))
}
fn d_to_big_int(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(bi(dec_of(&a[0])?.to_big_int()))
}
fn d_to_big_int_exact(_: &mut Vm, a: &mut [Value]) -> VR {
    let x = dec_of(&a[0])?;
    let i = x.set_scale(0, Rounding::Unnecessary).map_err(|t| rounding_frames(t, &["BigDecimal.toBigIntegerExact(BigDecimal.java:3613)"]))?;
    Ok(bi(i.unscaled))
}
fn d_exact(a: &[Value], bits: u32) -> Result<i64, Thrown> {
    let x = dec_of(&a[0])?;
    let (outer, overflow_line) = match bits {
        64 => (None, 0),
        32 => (Some("BigDecimal.intValueExact(BigDecimal.java:3759)"), 3761),
        16 => (Some("BigDecimal.shortValueExact(BigDecimal.java:3779)"), 3781),
        _ => (Some("BigDecimal.byteValueExact(BigDecimal.java:3799)"), 3801),
    };
    let mut lv: Vec<&str> = vec!["BigDecimal.longValueExact(BigDecimal.java:3700)"];
    lv.extend(outer);
    let i = x.set_scale(0, Rounding::Unnecessary).map_err(|t| rounding_frames(t, &lv))?.unscaled;
    if i.bit_length() > 63 {
        let mut f = vec!["BigDecimal.longValueExact(BigDecimal.java:3697)"];
        f.extend(outer);
        return Err(jm(arith("Overflow"), &f));
    }
    if (i.bit_length() as u32) < bits {
        Ok(i.low_i64())
    } else {
        let name = match bits {
            32 => "intValueExact",
            16 => "shortValueExact",
            _ => "byteValueExact",
        };
        Err(jm(arith("Overflow"), &[&format!("BigDecimal.{name}(BigDecimal.java:{overflow_line})")]))
    }
}
fn d_long_exact(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(d_exact(a, 64)?))
}
fn d_int_exact(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(d_exact(a, 32)? as i32))
}
fn d_short_exact(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Short(d_exact(a, 16)? as i16))
}
fn d_byte_exact(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Byte(d_exact(a, 8)? as i8))
}
fn d_to_plain(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(dec_of(&a[0])?.to_plain_string()))
}
fn d_to_eng(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(dec_of(&a[0])?.to_engineering_string()))
}
fn d_int_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(dec_of(&a[0])?.to_big_int().low_i64() as i32))
}
fn d_long_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(dec_of(&a[0])?.to_big_int().low_i64()))
}
fn d_short_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Short(dec_of(&a[0])?.to_big_int().low_i64() as i16))
}
fn d_byte_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Byte(dec_of(&a[0])?.to_big_int().low_i64() as i8))
}
fn d_double_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(dec_of(&a[0])?.to_f64()))
}
fn d_float_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Float(dec_of(&a[0])?.to_java_string().parse::<f32>().unwrap_or(0.0)))
}

// MathContext / RoundingMode
fn mc_new(_: &mut Vm, a: &mut [Value]) -> VR {
    let p = int(&a[0]);
    if p < 0 {
        return Err(jm(crate::vm::iae("Digits < 0"), &["MathContext.<init>(MathContext.java:158)", "MathContext.<init>(MathContext.java:142)"]));
    }
    Ok(math(MathObj::Context(MathCtx { precision: p as u32, mode: Rounding::HalfUp })))
}
fn mc_new2(_: &mut Vm, a: &mut [Value]) -> VR {
    let p = int(&a[0]);
    if p < 0 {
        return Err(jm(crate::vm::iae("Digits < 0"), &["MathContext.<init>(MathContext.java:158)"]));
    }
    let m = rm_of(&a[1]).map_err(|_| jm(crate::vm::npe("null RoundingMode"), &["MathContext.<init>(MathContext.java:160)"]))?;
    Ok(math(MathObj::Context(MathCtx { precision: p as u32, mode: m })))
}
fn mc_precision(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(mc_of(&a[0])?.precision as i32))
}
fn mc_mode(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(math(MathObj::Mode(mc_of(&a[0])?.mode)))
}
fn rm_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = a[0].to_java_string();
    match ROUNDING_NAMES.iter().position(|n| *n == s) {
        Some(i) => Ok(math(MathObj::Mode(Rounding::from_ordinal(i as u8)))),
        None => Err(jm(crate::vm::iae(format!("No enum constant java.math.RoundingMode.{s}")), &["java.lang.Enum.valueOf(Enum.java:293)", "RoundingMode.valueOf(RoundingMode.java:120)"])),
    }
}
fn rm_values(_: &mut Vm, _: &mut [Value]) -> VR {
    let items = (0..8).map(|i| math(MathObj::Mode(Rounding::from_ordinal(i)))).collect();
    Ok(Value::obj(Obj::Array(JArray::from_data(Ty::of(cls::ROUNDING_MODE), ArrayData::Ref(items)))))
}
fn rm_name(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::str(rm_of(&a[0])?.name()))
}
fn rm_ordinal(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(rm_of(&a[0])? as i32))
}

pub fn register(b: &mut Builder) {
    use cls::*;
    b.supers(BIG_INTEGER, &[NUMBER, COMPARABLE]);
    b.sf(BIG_INTEGER, "BigInteger ONE", |_, _| Ok(bi(BigInt::from_i64(1))));
    b.sf(BIG_INTEGER, "BigInteger TEN", |_, _| Ok(bi(BigInt::from_i64(10))));
    b.sf(BIG_INTEGER, "BigInteger ZERO", |_, _| Ok(bi(BigInt::zero())));
    b.c(BIG_INTEGER, "(String)", i_new);
    b.c(BIG_INTEGER, "(String,int)", i_new_radix);
    for (sig, f) in [
        ("BigInteger abs()", i_abs as crate::lookup::Native),
        ("BigInteger add(BigInteger)", i_add),
        ("BigInteger and(BigInteger)", i_and),
        ("BigInteger andNot(BigInteger)", i_and_not),
        ("int bitCount()", i_bit_count),
        ("int bitLength()", i_bit_length),
        ("byte byteValueExact()", i_byte_exact),
        ("BigInteger clearBit(int)", i_clear_bit),
        ("int compareTo(BigInteger)", i_compare),
        ("BigInteger divide(BigInteger)", i_div),
        ("BigInteger[] divideAndRemainder(BigInteger)", i_divrem),
        ("BigInteger flipBit(int)", i_flip_bit),
        ("BigInteger gcd(BigInteger)", i_gcd),
        ("int getLowestSetBit()", i_lowest),
        ("int intValueExact()", i_int_exact),
        ("long longValueExact()", i_long_exact),
        ("BigInteger max(BigInteger)", i_max),
        ("BigInteger min(BigInteger)", i_min),
        ("BigInteger mod(BigInteger)", i_mod),
        ("BigInteger modInverse(BigInteger)", i_mod_inverse),
        ("BigInteger modPow(BigInteger,BigInteger)", i_mod_pow),
        ("BigInteger multiply(BigInteger)", i_mul),
        ("BigInteger negate()", i_negate),
        ("BigInteger not()", i_not),
        ("BigInteger or(BigInteger)", i_or),
        ("BigInteger pow(int)", i_pow),
        ("BigInteger remainder(BigInteger)", i_rem),
        ("BigInteger setBit(int)", i_set_bit),
        ("BigInteger shiftLeft(int)", i_shl),
        ("BigInteger shiftRight(int)", i_shr),
        ("short shortValueExact()", i_short_exact),
        ("int signum()", i_signum),
        ("BigInteger subtract(BigInteger)", i_sub),
        ("boolean testBit(int)", i_test_bit),
        ("byte[] toByteArray()", i_to_bytes),
        ("String toString(int)", i_to_string_radix),
        ("BigInteger xor(BigInteger)", i_xor),
        ("byte byteValue()", i_byte_value),
        ("short shortValue()", i_short_value),
        ("int intValue()", i_int_value),
        ("long longValue()", i_long_value),
        ("float floatValue()", i_float_value),
        ("double doubleValue()", i_double_value),
    ] {
        b.m(BIG_INTEGER, sig, f);
    }
    b.s(BIG_INTEGER, "BigInteger valueOf(long)", i_value_of);

    b.supers(BIG_DECIMAL, &[NUMBER, COMPARABLE]);
    b.sf(BIG_DECIMAL, "BigDecimal ONE", |_, _| Ok(bd(BigDec::from_i64(1))));
    b.sf(BIG_DECIMAL, "BigDecimal TEN", |_, _| Ok(bd(BigDec::from_i64(10))));
    b.sf(BIG_DECIMAL, "BigDecimal ZERO", |_, _| Ok(bd(BigDec::from_i64(0))));
    b.c(BIG_DECIMAL, "(String)", d_new);
    b.c(BIG_DECIMAL, "(String,MathContext)", d_new_mc);
    for (sig, f) in [
        ("BigDecimal abs()", d_abs as crate::lookup::Native),
        ("BigDecimal abs(MathContext)", d_abs_mc),
        ("BigDecimal add(BigDecimal)", d_add),
        ("BigDecimal add(BigDecimal,MathContext)", d_add_mc),
        ("byte byteValueExact()", d_byte_exact),
        ("int compareTo(BigDecimal)", d_compare),
        ("BigDecimal divide(BigDecimal)", d_div),
        ("BigDecimal divide(BigDecimal,MathContext)", d_div_mc),
        ("BigDecimal[] divideAndRemainder(BigDecimal)", d_divrem),
        ("BigDecimal[] divideAndRemainder(BigDecimal,MathContext)", d_divrem_mc),
        ("BigDecimal divideToIntegralValue(BigDecimal)", d_div_int),
        ("BigDecimal divideToIntegralValue(BigDecimal,MathContext)", d_div_int_mc),
        ("int intValueExact()", d_int_exact),
        ("long longValueExact()", d_long_exact),
        ("BigDecimal max(BigDecimal)", d_max),
        ("BigDecimal min(BigDecimal)", d_min),
        ("BigDecimal movePointLeft(int)", d_move_left),
        ("BigDecimal movePointRight(int)", d_move_right),
        ("BigDecimal multiply(BigDecimal)", d_mul),
        ("BigDecimal multiply(BigDecimal,MathContext)", d_mul_mc),
        ("BigDecimal negate()", d_negate),
        ("BigDecimal negate(MathContext)", d_negate_mc),
        ("BigDecimal plus()", d_plus),
        ("BigDecimal plus(MathContext)", d_plus_mc),
        ("BigDecimal pow(int)", d_pow),
        ("BigDecimal pow(int,MathContext)", d_pow_mc),
        ("int precision()", d_precision),
        ("BigDecimal remainder(BigDecimal)", d_rem),
        ("BigDecimal remainder(BigDecimal,MathContext)", d_rem_mc),
        ("BigDecimal round(MathContext)", d_round),
        ("int scale()", d_scale),
        ("BigDecimal scaleByPowerOfTen(int)", d_scale_by_pow10),
        ("BigDecimal setScale(int)", d_set_scale),
        ("BigDecimal setScale(int,RoundingMode)", d_set_scale_mode),
        ("short shortValueExact()", d_short_exact),
        ("int signum()", d_signum),
        ("BigDecimal stripTrailingZeros()", d_strip),
        ("BigDecimal subtract(BigDecimal)", d_sub),
        ("BigDecimal subtract(BigDecimal,MathContext)", d_sub_mc),
        ("BigInteger toBigInteger()", d_to_big_int),
        ("BigInteger toBigIntegerExact()", d_to_big_int_exact),
        ("String toEngineeringString()", d_to_eng),
        ("String toPlainString()", d_to_plain),
        ("BigDecimal ulp()", d_ulp),
        ("byte byteValue()", d_byte_value),
        ("short shortValue()", d_short_value),
        ("int intValue()", d_int_value),
        ("long longValue()", d_long_value),
        ("float floatValue()", d_float_value),
        ("double doubleValue()", d_double_value),
    ] {
        b.m(BIG_DECIMAL, sig, f);
    }
    b.s(BIG_DECIMAL, "BigDecimal valueOf(double)", d_value_of);

    b.supers(MATH_CONTEXT, &[OBJECT]);
    let ctx = |p: u32, m: Rounding| math(MathObj::Context(MathCtx { precision: p, mode: m }));
    let _ = ctx;
    b.sf(MATH_CONTEXT, "MathContext DECIMAL128", |_, _| Ok(math(MathObj::Context(MathCtx { precision: 34, mode: Rounding::HalfEven }))));
    b.sf(MATH_CONTEXT, "MathContext DECIMAL32", |_, _| Ok(math(MathObj::Context(MathCtx { precision: 7, mode: Rounding::HalfEven }))));
    b.sf(MATH_CONTEXT, "MathContext DECIMAL64", |_, _| Ok(math(MathObj::Context(MathCtx { precision: 16, mode: Rounding::HalfEven }))));
    b.sf(MATH_CONTEXT, "MathContext UNLIMITED", |_, _| Ok(math(MathObj::Context(MathCtx { precision: 0, mode: Rounding::HalfUp }))));
    b.c(MATH_CONTEXT, "(int)", mc_new);
    b.c(MATH_CONTEXT, "(int,RoundingMode)", mc_new2);
    b.m(MATH_CONTEXT, "int getPrecision()", mc_precision);
    b.m(MATH_CONTEXT, "RoundingMode getRoundingMode()", mc_mode);

    b.supers(ROUNDING_MODE, &[ENUM]);
    macro_rules! modes {
        ($($name:literal = $m:expr,)*) => {$(
            b.sf(ROUNDING_MODE, concat!("RoundingMode ", $name), |_, _| Ok(math(MathObj::Mode($m))));
        )*};
    }
    modes! {
        "CEILING" = Rounding::Ceiling, "DOWN" = Rounding::Down, "FLOOR" = Rounding::Floor, "HALF_DOWN" = Rounding::HalfDown,
        "HALF_EVEN" = Rounding::HalfEven, "HALF_UP" = Rounding::HalfUp, "UNNECESSARY" = Rounding::Unnecessary, "UP" = Rounding::Up,
    }
    b.s(ROUNDING_MODE, "RoundingMode valueOf(String)", rm_value_of);
    b.s(ROUNDING_MODE, "RoundingMode[] values()", rm_values);
    b.m(ROUNDING_MODE, "String name()", rm_name);
    b.m(ROUNDING_MODE, "int ordinal()", rm_ordinal);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> BigDec {
        BigDec::parse(s).unwrap()
    }

    #[test]
    fn big_int_basics() {
        let a = BigInt::parse("123456789012345678901234567890").unwrap();
        let b = BigInt::parse("987654321").unwrap();
        let (q, r) = a.divrem(&b).unwrap();
        assert_eq!(q.to_string(), "124999998873437499901");
        assert_eq!(r.to_string(), "574845669");
        assert_eq!(q.mul(&b).add(&r), a);
        let a2 = BigInt::parse("123456789012345678901234567890123").unwrap();
        let b2 = BigInt::parse("98765432109876543210").unwrap();
        let (q2, r2) = a2.divrem(&b2).unwrap();
        assert_eq!(q2.to_string(), "1249999988609");
        assert_eq!(r2.to_string(), "37037052338271595233");
        assert_eq!(BigInt::from_i64(-5).shift_right(1).to_string(), "-3");
        assert_eq!(BigInt::from_i64(-1).bit_length(), 0);
        assert_eq!(BigInt::from_i64(-128).to_byte_array(), vec![0x80]);
        assert_eq!(BigInt::from_i64(255).to_string_radix(16), "ff");
        assert_eq!(BigInt::from_i64(3).mod_pow(&BigInt::from_i64(4), &BigInt::from_i64(5)).unwrap().to_string(), "1");
        assert_eq!(BigInt::from_i64(3).mod_inverse(&BigInt::from_i64(11)).unwrap().to_string(), "4");
        assert_eq!(BigInt::from_i64(-6).bitwise(&BigInt::from_i64(3), |a, b| a & b).to_string(), "2");
    }

    #[test]
    fn big_dec_layouts() {
        assert_eq!(d("1.50").to_java_string(), "1.50");
        assert_eq!(d("1E+3").to_java_string(), "1E+3");
        assert_eq!(d("0.0000001").to_java_string(), "1E-7");
        assert_eq!(d("123.456e-10").to_java_string(), "1.23456E-8");
        assert_eq!(d("1E+3").to_plain_string(), "1000");
        assert_eq!(d("12345E+2").to_engineering_string(), "1.2345E+6");
        assert_eq!(BigDec::value_of(0.1).unwrap().to_java_string(), "0.1");
    }

    #[test]
    fn big_dec_arithmetic() {
        assert_eq!(d("10").divide(&d("4")).unwrap().to_java_string(), "2.5");
        assert!(d("1").divide(&d("3")).is_err());
        let mc = MathCtx { precision: 5, mode: Rounding::HalfEven };
        assert_eq!(d("1").divide_mc(&d("3"), mc).unwrap().to_java_string(), "0.33333");
        assert_eq!(d("2").divide_mc(&d("3"), mc).unwrap().to_java_string(), "0.66667");
        assert_eq!(d("1.25").set_scale(1, Rounding::HalfEven).unwrap().to_java_string(), "1.2");
        assert_eq!(d("1.25").set_scale(1, Rounding::HalfUp).unwrap().to_java_string(), "1.3");
        assert_eq!(d("10.5").divide_to_integral(&d("3")).unwrap().to_java_string(), "3.0");
        assert_eq!(d("10.5").remainder(&d("3")).unwrap().to_java_string(), "1.5");
        assert_eq!(d("999").round(MathCtx { precision: 2, mode: Rounding::HalfUp }).unwrap().to_java_string(), "1.0E+3");
        assert_eq!(d("1.10").add(&d("2.205")).to_java_string(), "3.305");
        assert_eq!(d("100").strip_trailing_zeros().to_java_string(), "1E+2");
    }
}
