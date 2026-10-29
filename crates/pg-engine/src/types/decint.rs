//! Signed big integers kept in decimal: base 10^9 limbs, least significant first. numeric's
//! digits. PostgreSQL keeps base-10000 digits so that text output and input are linear and
//! scaling by a power of ten is a shift; a binary big integer makes both quadratic (100,000
//! digits printed ~100x slower than PostgreSQL). Nine digits a limb do PostgreSQL's
//! schoolbook multiplication and long division in about a fifth of its digit operations;
//! multiplication switches to Karatsuba for long operands.

use std::cmp::Ordering;
use std::fmt;
use std::ops::{Add, AddAssign, Div, DivAssign, Mul, MulAssign, Neg, Rem, RemAssign, Sub, SubAssign};

pub use num_bigint::Sign;
use smallvec::{SmallVec, smallvec};

/// Limbs, four inline (the size of a Vec): values below 10^36 need no allocation of their own.
type Mag = SmallVec<[u32; 4]>;

const BASE: u64 = 1_000_000_000;
const LIMB_DIGITS: usize = 9;
const KARATSUBA: usize = 40;
const POW10: [u32; 10] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
];

/// `neg` is never set for zero; `mag` has no most-significant zero limbs (zero is empty).
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct DecInt {
    neg: bool,
    mag: Mag,
}

fn trim(v: &mut Mag) {
    while v.last() == Some(&0) {
        v.pop();
    }
}

fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

fn add_mag(a: &[u32], b: &[u32]) -> Mag {
    let (a, b) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut out = Mag::with_capacity(a.len() + 1);
    let mut carry = 0u32;
    for (i, &x) in a.iter().enumerate() {
        let mut s = x + b.get(i).copied().unwrap_or(0) + carry;
        carry = (s as u64 >= BASE) as u32;
        if carry != 0 {
            s -= BASE as u32;
        }
        out.push(s);
    }
    if carry != 0 {
        out.push(carry);
    }
    out
}

/// a - b for |a| >= |b|.
fn sub_mag(a: &[u32], b: &[u32]) -> Mag {
    let mut out = Mag::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, &x) in a.iter().enumerate() {
        let mut d = x as i64 - b.get(i).copied().unwrap_or(0) as i64 - borrow;
        borrow = (d < 0) as i64;
        if d < 0 {
            d += BASE as i64;
        }
        out.push(d as u32);
    }
    debug_assert_eq!(borrow, 0);
    trim(&mut out);
    out
}

/// a * m + add, for m, add < 2^32.
fn mul_small(a: &[u32], m: u32, add: u32) -> Mag {
    let mut out = Mag::with_capacity(a.len() + 2);
    let mut carry = add as u64;
    for &x in a {
        let t = x as u64 * m as u64 + carry;
        out.push((t % BASE) as u32);
        carry = t / BASE;
    }
    while carry != 0 {
        out.push((carry % BASE) as u32);
        carry /= BASE;
    }
    trim(&mut out);
    out
}

/// (a / d, a % d) for 0 < d < 2^32.
fn divrem_small(a: &[u32], d: u32) -> (Mag, u32) {
    let mut q = smallvec![0u32; a.len()];
    let mut rem = 0u64;
    for i in (0..a.len()).rev() {
        let t = rem * BASE + a[i] as u64;
        q[i] = (t / d as u64) as u32;
        rem = t % d as u64;
    }
    trim(&mut q);
    (q, rem as u32)
}

fn schoolbook(a: &[u32], b: &[u32]) -> Mag {
    let mut out = smallvec![0u32; a.len() + b.len()];
    for (i, &x) in a.iter().enumerate() {
        if x == 0 {
            continue;
        }
        let mut carry = 0u64;
        let row = &mut out[i..];
        for (r, &y) in row.iter_mut().zip(b) {
            let t = x as u64 * y as u64 + *r as u64 + carry;
            carry = t / BASE;
            *r = (t - carry * BASE) as u32;
        }
        let mut k = b.len();
        while carry != 0 {
            let t = row[k] as u64 + carry;
            row[k] = (t % BASE) as u32;
            carry = t / BASE;
            k += 1;
        }
    }
    trim(&mut out);
    out
}

/// `acc[at..] += v`.
fn add_into(acc: &mut Mag, v: &[u32], at: usize) {
    if acc.len() < at + v.len() + 1 {
        acc.resize(at + v.len() + 1, 0);
    }
    let mut carry = 0u32;
    let mut i = 0;
    while i < v.len() || carry != 0 {
        let mut s = acc[at + i] + v.get(i).copied().unwrap_or(0) + carry;
        carry = (s as u64 >= BASE) as u32;
        if carry != 0 {
            s -= BASE as u32;
        }
        acc[at + i] = s;
        i += 1;
        if at + i == acc.len() && carry != 0 {
            acc.push(0);
        }
    }
}

fn split(v: &[u32], m: usize) -> (Mag, &[u32]) {
    if v.len() <= m {
        return (Mag::from_slice(v), &[]);
    }
    let mut lo = Mag::from_slice(&v[..m]);
    trim(&mut lo);
    (lo, &v[m..])
}

fn mul_mag(a: &[u32], b: &[u32]) -> Mag {
    if a.is_empty() || b.is_empty() {
        return Mag::new();
    }
    if a.len().min(b.len()) < KARATSUBA {
        return schoolbook(a, b);
    }
    let m = a.len().max(b.len()) / 2;
    let (a0, a1) = split(a, m);
    let (b0, b1) = split(b, m);
    if a1.is_empty() || b1.is_empty() {
        // One operand fits in the low half: (x1 B^m + x0) y.
        let (x0, x1, y) = if a1.is_empty() { (&b0, b1, a) } else { (&a0, a1, b) };
        let mut out = mul_mag(x0, y);
        add_into(&mut out, &mul_mag(x1, y), m);
        trim(&mut out);
        return out;
    }
    let z0 = mul_mag(&a0, &b0);
    let z2 = mul_mag(a1, b1);
    let z1 = mul_mag(&add_mag(&a0, a1), &add_mag(&b0, b1));
    let z1 = sub_mag(&sub_mag(&z1, &z0), &z2);
    let mut out = Mag::with_capacity(a.len() + b.len() + 1);
    out.extend_from_slice(&z0);
    add_into(&mut out, &z1, m);
    add_into(&mut out, &z2, 2 * m);
    trim(&mut out);
    out
}

/// Knuth's algorithm D: (u / v, u % v) for v != 0.
fn divrem_mag(u: &[u32], v: &[u32]) -> (Mag, Mag) {
    if cmp_mag(u, v) == Ordering::Less {
        return (Mag::new(), Mag::from_slice(u));
    }
    if v.len() == 1 {
        let (q, r) = divrem_small(u, v[0]);
        return (q, if r == 0 { Mag::new() } else { smallvec![r] });
    }
    let n = v.len();
    let d = (BASE / (v[n - 1] as u64 + 1)) as u32;
    let mut un = mul_small(u, d, 0);
    un.resize(u.len() + 1, 0);
    let mut vn = mul_small(v, d, 0);
    vn.resize(n, 0);
    let m = u.len() - n;
    let mut q = smallvec![0u32; m + 1];
    let (vt, vs) = (vn[n - 1] as u64, vn[n - 2] as u64);
    for j in (0..=m).rev() {
        let num = un[j + n] as u64 * BASE + un[j + n - 1] as u64;
        let mut qhat = num / vt;
        let mut rhat = num % vt;
        while qhat >= BASE || qhat * vs > rhat * BASE + un[j + n - 2] as u64 {
            qhat -= 1;
            rhat += vt;
            if rhat >= BASE {
                break;
            }
        }
        // un[j..=j+n] -= qhat * vn
        let mut carry = 0u64;
        let mut borrow = 0i64;
        for (u, &v) in un[j..j + n].iter_mut().zip(&vn) {
            let p = qhat * v as u64 + carry;
            carry = p / BASE;
            let t = *u as i64 - (p - carry * BASE) as i64 - borrow;
            borrow = (t < 0) as i64;
            *u = (t + borrow * BASE as i64) as u32;
        }
        let t = un[j + n] as i64 - carry as i64 - borrow;
        if t < 0 {
            // qhat was one too large: add vn back.
            qhat -= 1;
            let mut c = 0u32;
            for i in 0..n {
                let mut s = un[i + j] + vn[i] + c;
                c = (s as u64 >= BASE) as u32;
                if c != 0 {
                    s -= BASE as u32;
                }
                un[i + j] = s;
            }
            un[j + n] = (t + c as i64) as u32;
        } else {
            un[j + n] = t as u32;
        }
        q[j] = qhat as u32;
    }
    trim(&mut q);
    un.truncate(n);
    trim(&mut un);
    let (r, _) = divrem_small(&un, d);
    (q, r)
}

fn digits_u32(x: u32) -> usize {
    POW10.iter().position(|&p| p > x).unwrap_or(10)
}

impl DecInt {
    fn from_mag(neg: bool, mag: Mag) -> DecInt {
        DecInt { neg: neg && !mag.is_empty(), mag }
    }

    pub fn zero() -> DecInt {
        DecInt::default()
    }

    pub fn one() -> DecInt {
        DecInt::from(1u32)
    }

    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    pub fn is_one(&self) -> bool {
        !self.neg && self.mag[..] == [1]
    }

    pub fn is_odd(&self) -> bool {
        self.mag.first().is_some_and(|l| l % 2 == 1)
    }

    pub fn is_negative(&self) -> bool {
        self.neg
    }

    pub fn is_positive(&self) -> bool {
        !self.neg && !self.mag.is_empty()
    }

    pub fn sign(&self) -> Sign {
        if self.neg {
            Sign::Minus
        } else if self.mag.is_empty() {
            Sign::NoSign
        } else {
            Sign::Plus
        }
    }

    pub fn abs(&self) -> DecInt {
        DecInt { neg: false, mag: self.mag.clone() }
    }

    /// A value from its sign and base-10^9 limbs (least significant first); None if a limb
    /// is out of range.
    pub fn from_limbs(neg: bool, mut mag: Mag) -> Option<DecInt> {
        if mag.iter().any(|&l| l as u64 >= BASE) {
            return None;
        }
        trim(&mut mag);
        Some(DecInt::from_mag(neg, mag))
    }

    /// The magnitude's base-10^9 limbs, least significant first.
    pub fn limbs(&self) -> &[u32] {
        &self.mag
    }

    /// The number of decimal digits of the magnitude (0 for zero).
    pub fn num_digits(&self) -> usize {
        match self.mag.last() {
            None => 0,
            Some(&top) => (self.mag.len() - 1) * LIMB_DIGITS + digits_u32(top),
        }
    }

    /// The magnitude's decimal digit at position `i` (0: units).
    pub fn digit(&self, i: usize) -> u32 {
        match self.mag.get(i / LIMB_DIGITS) {
            None => 0,
            Some(&l) => l / POW10[i % LIMB_DIGITS] % 10,
        }
    }

    /// How many low decimal digits are zero (0 for zero).
    pub fn trailing_zero_digits(&self) -> usize {
        let Some(i) = self.mag.iter().position(|&l| l != 0) else {
            return 0;
        };
        let mut l = self.mag[i];
        let mut k = 0;
        while l % 10 == 0 {
            l /= 10;
            k += 1;
        }
        i * LIMB_DIGITS + k
    }

    /// self * 10^k.
    pub fn mul_pow10(&self, k: usize) -> DecInt {
        if self.is_zero() || k == 0 {
            return self.clone();
        }
        let shifted = mul_small(&self.mag, POW10[k % LIMB_DIGITS], 0);
        let mut mag = smallvec![0u32; k / LIMB_DIGITS];
        mag.extend_from_slice(&shifted);
        DecInt { neg: self.neg, mag }
    }

    /// self / 10^k, truncated toward zero.
    pub fn div_pow10(&self, k: usize) -> DecInt {
        if k == 0 {
            return self.clone();
        }
        let limbs = k / LIMB_DIGITS;
        if limbs >= self.mag.len() {
            return DecInt::zero();
        }
        let (q, _) = divrem_small(&self.mag[limbs..], POW10[k % LIMB_DIGITS]);
        DecInt::from_mag(self.neg, q)
    }

    /// self / 10^k rounded half away from zero (the first dropped digit decides).
    pub fn round_pow10(&self, k: usize) -> DecInt {
        if k == 0 {
            return self.clone();
        }
        let q = self.div_pow10(k);
        if self.digit(k - 1) >= 5 {
            let mag = add_mag(&q.mag, &[1]);
            DecInt::from_mag(self.neg, mag)
        } else {
            q
        }
    }

    /// Whether self is a multiple of 10^k.
    pub fn divisible_pow10(&self, k: usize) -> bool {
        self.is_zero() || self.trailing_zero_digits() >= k
    }

    pub fn pow(&self, mut e: u32) -> DecInt {
        let mut base = self.clone();
        let mut acc = DecInt::one();
        while e > 0 {
            if e & 1 == 1 {
                acc = &acc * &base;
            }
            e >>= 1;
            if e > 0 {
                base = &base * &base;
            }
        }
        acc
    }

    /// floor(sqrt(self)) for self >= 0 (Newton's iteration from a float estimate).
    pub fn sqrt(&self) -> DecInt {
        if self.is_zero() || self.neg {
            return DecInt::zero();
        }
        let digits = self.num_digits();
        // Start above the root: a float's leading digits, rounded up.
        let lead = digits.min(30);
        let top = self.div_pow10(digits - lead).to_f64();
        let mut shift = digits - lead;
        let mut est = top;
        if shift % 2 == 1 {
            est *= 10.0;
            shift -= 1;
        }
        let r0 = (est.sqrt() * (1.0 + 1e-12)).ceil() + 1.0;
        let mut x = DecInt::from(r0 as u128).mul_pow10(shift / 2);
        loop {
            let y = (&x + &(self / &x)).div_small(2);
            if y >= x {
                return x;
            }
            x = y;
        }
    }

    fn div_small(&self, d: u32) -> DecInt {
        DecInt::from_mag(self.neg, divrem_small(&self.mag, d).0)
    }

    /// Truncating quotient and remainder at once.
    pub fn div_rem(&self, o: &DecInt) -> (DecInt, DecInt) {
        divrem_ref(self, o)
    }

    pub fn gcd(&self, other: &DecInt) -> DecInt {
        let (mut a, mut b) = (self.abs(), other.abs());
        while !b.is_zero() {
            let r = &a % &b;
            a = b;
            b = r;
        }
        a
    }

    /// The value of ASCII decimal digits (no sign), or None if a byte isn't a digit.
    pub fn from_digits(s: &[u8]) -> Option<DecInt> {
        let mut mag = Mag::with_capacity(s.len() / LIMB_DIGITS + 1);
        let mut end = s.len();
        while end > 0 {
            let start = end.saturating_sub(LIMB_DIGITS);
            let mut l = 0u32;
            for &c in &s[start..end] {
                if !c.is_ascii_digit() {
                    return None;
                }
                l = l * 10 + (c - b'0') as u32;
            }
            mag.push(l);
            end = start;
        }
        trim(&mut mag);
        Some(DecInt { neg: false, mag })
    }

    /// The value of base-10000 digits, most significant first (each 0..=9999).
    pub fn from_base10000(digits: impl ExactSizeIterator<Item = u32>) -> DecInt {
        let mut ascii = Vec::with_capacity(digits.len() * 4);
        for d in digits {
            debug_assert!(d < 10000);
            ascii.extend_from_slice(&[
                b'0' + (d / 1000) as u8,
                b'0' + (d / 100 % 10) as u8,
                b'0' + (d / 10 % 10) as u8,
                b'0' + (d % 10) as u8,
            ]);
        }
        DecInt::from_digits(&ascii).unwrap_or_default()
    }

    /// The magnitude's base-10000 digits, most significant first, no leading zero digit.
    pub fn base10000(&self) -> Mag {
        let n = self.num_digits().div_ceil(4);
        (0..n)
            .rev()
            .map(|g| {
                let b = 4 * g;
                self.digit(b + 3) * 1000 + self.digit(b + 2) * 100 + self.digit(b + 1) * 10 + self.digit(b)
            })
            .collect()
    }

    /// Parse in a radix (2..=36) from ASCII digits (jsonpath's 0x / 0o / 0b literals).
    pub fn parse_radix(s: &[u8], radix: u32) -> Option<DecInt> {
        let mut mag: Mag = Mag::new();
        for &c in s {
            let d = (c as char).to_digit(radix)?;
            mag = mul_small(&mag, radix, d);
        }
        Some(DecInt { neg: false, mag })
    }

    /// The magnitude's ASCII decimal digits ("0" for zero).
    pub fn magnitude_string(&self) -> String {
        let Some((&top, rest)) = self.mag.split_last() else {
            return "0".to_string();
        };
        let mut out = String::with_capacity(self.mag.len() * LIMB_DIGITS + 1);
        out.push_str(&top.to_string());
        for &l in rest.iter().rev() {
            let mut buf = [b'0'; LIMB_DIGITS];
            let mut v = l;
            for b in buf.iter_mut().rev() {
                *b = b'0' + (v % 10) as u8;
                v /= 10;
            }
            out.push_str(std::str::from_utf8(&buf).unwrap());
        }
        out
    }

    pub fn to_i64(&self) -> Option<i64> {
        if self.mag.len() > 3 {
            return None;
        }
        let v = self.to_i128()?;
        i64::try_from(v).ok()
    }

    pub fn to_u64(&self) -> Option<u64> {
        let v = self.to_i128()?;
        u64::try_from(v).ok()
    }

    pub fn to_i128(&self) -> Option<i128> {
        let v: i128 = match self.mag[..] {
            [] => 0,
            [a] => a as i128,
            [a, b] => (b as u64 * BASE + a as u64) as i128,
            _ => {
                if self.mag.len() > 5 {
                    return None;
                }
                let mut v: i128 = 0;
                for &l in self.mag.iter().rev() {
                    v = v.checked_mul(BASE as i128)?.checked_add(l as i128)?;
                }
                v
            }
        };
        Some(if self.neg { -v } else { v })
    }

    /// The nearest f64 (correctly rounded, as strtod over the digits).
    pub fn to_f64(&self) -> f64 {
        if let Some(v) = self.to_i64()
            && v.unsigned_abs() < (1u64 << 53)
        {
            return v as f64;
        }
        self.to_string().parse().unwrap_or(f64::NAN)
    }

    /// Two's complement bytes, least significant first (num-bigint's to_signed_bytes_le).
    pub fn to_signed_bytes_le(&self) -> Vec<u8> {
        if let Some(v) = self.to_i128() {
            let b = v.to_le_bytes();
            let mut n = 16;
            let fill = if v < 0 { 0xFF } else { 0 };
            while n > 1 && b[n - 1] == fill && (b[n - 2] & 0x80 == fill & 0x80) {
                n -= 1;
            }
            if v == 0 {
                return vec![0];
            }
            return b[..n].to_vec();
        }
        let mut mag = self.mag.clone();
        let mut bytes = Vec::new();
        while !mag.is_empty() {
            let (q, r) = divrem_small(&mag, 1 << 16);
            bytes.extend_from_slice(&(r as u16).to_le_bytes());
            mag = q;
        }
        while bytes.len() > 1 && bytes[bytes.len() - 1] == 0 {
            bytes.pop();
        }
        if bytes.last().is_some_and(|b| b & 0x80 != 0) {
            bytes.push(0);
        }
        if self.neg {
            let mut carry = true;
            for b in &mut bytes {
                *b = !*b;
                if carry {
                    let (v, c) = b.overflowing_add(1);
                    *b = v;
                    carry = c;
                }
            }
            while bytes.len() > 1 && bytes[bytes.len() - 1] == 0xFF && bytes[bytes.len() - 2] & 0x80 != 0 {
                bytes.pop();
            }
        }
        bytes
    }

    /// The value of two's complement bytes, least significant first.
    pub fn from_signed_bytes_le(bytes: &[u8]) -> DecInt {
        if bytes.is_empty() {
            return DecInt::zero();
        }
        let neg = bytes[bytes.len() - 1] & 0x80 != 0;
        let mut b = bytes.to_vec();
        if neg {
            let mut carry = true;
            for x in &mut b {
                *x = !*x;
                if carry {
                    let (v, c) = x.overflowing_add(1);
                    *x = v;
                    carry = c;
                }
            }
        }
        let mut mag: Mag = Mag::new();
        for &x in b.iter().rev() {
            mag = mul_small(&mag, 256, x as u32);
        }
        DecInt::from_mag(neg, mag)
    }
}

macro_rules! from_unsigned {
    ($($t:ty),*) => {$(
        impl From<$t> for DecInt {
            fn from(v: $t) -> DecInt {
                DecInt { neg: false, mag: mag_of_u128(v as u128) }
            }
        }
    )*};
}
/// (u64 arithmetic below 2^64: u128 division is a library call.)
fn mag_of_u128(v: u128) -> Mag {
    if let Ok(mut v) = u64::try_from(v) {
        if v == 0 {
            return Mag::new();
        }
        if v < BASE {
            return smallvec![v as u32];
        }
        let mut mag = Mag::with_capacity(3);
        while v != 0 {
            mag.push((v % BASE) as u32);
            v /= BASE;
        }
        return mag;
    }
    let b2 = BASE as u128 * BASE as u128;
    let (hi, lo) = (v / b2, (v % b2) as u64);
    let mut mag = smallvec![(lo % BASE) as u32, (lo / BASE) as u32];
    mag.extend(mag_of_u128(hi));
    mag
}
from_unsigned!(u8, u16, u32, u64, u128, usize);

macro_rules! from_signed {
    ($($t:ty),*) => {$(
        impl From<$t> for DecInt {
            fn from(v: $t) -> DecInt {
                let d = DecInt::from((v as i128).unsigned_abs());
                DecInt::from_mag(v < 0, d.mag)
            }
        }
    )*};
}
from_signed!(i8, i16, i32, i64, i128, isize);

impl Ord for DecInt {
    fn cmp(&self, o: &DecInt) -> Ordering {
        match (self.neg, o.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => cmp_mag(&self.mag, &o.mag),
            (true, true) => cmp_mag(&o.mag, &self.mag),
        }
    }
}

impl PartialOrd for DecInt {
    fn partial_cmp(&self, o: &DecInt) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl fmt::Display for DecInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.neg {
            f.write_str("-")?;
        }
        f.write_str(&self.magnitude_string())
    }
}

impl fmt::Debug for DecInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::str::FromStr for DecInt {
    type Err = ();
    fn from_str(s: &str) -> Result<DecInt, ()> {
        let (neg, d) = match s.as_bytes() {
            [b'-', d @ ..] => (true, d),
            [b'+', d @ ..] => (false, d),
            d => (false, d),
        };
        if d.is_empty() {
            return Err(());
        }
        let v = DecInt::from_digits(d).ok_or(())?;
        Ok(DecInt::from_mag(neg, v.mag))
    }
}

fn add_signed(a: &DecInt, b: &DecInt, b_neg: bool) -> DecInt {
    if a.neg == b_neg {
        return DecInt::from_mag(a.neg, add_mag(&a.mag, &b.mag));
    }
    match cmp_mag(&a.mag, &b.mag) {
        Ordering::Equal => DecInt::zero(),
        Ordering::Greater => DecInt::from_mag(a.neg, sub_mag(&a.mag, &b.mag)),
        Ordering::Less => DecInt::from_mag(b_neg, sub_mag(&b.mag, &a.mag)),
    }
}

fn add_ref(a: &DecInt, b: &DecInt) -> DecInt {
    add_signed(a, b, b.neg)
}

fn sub_ref(a: &DecInt, b: &DecInt) -> DecInt {
    add_signed(a, b, !b.neg && !b.mag.is_empty())
}

fn mul_ref(a: &DecInt, b: &DecInt) -> DecInt {
    if b.mag.len() == 1 {
        return DecInt::from_mag(a.neg != b.neg, mul_small(&a.mag, b.mag[0], 0));
    }
    if a.mag.len() == 1 {
        return DecInt::from_mag(a.neg != b.neg, mul_small(&b.mag, a.mag[0], 0));
    }
    DecInt::from_mag(a.neg != b.neg, mul_mag(&a.mag, &b.mag))
}

/// Truncating division and its remainder (the dividend's sign), as num-bigint's.
fn divrem_ref(a: &DecInt, b: &DecInt) -> (DecInt, DecInt) {
    assert!(!b.is_zero(), "attempt to divide by zero");
    let (q, r) = divrem_mag(&a.mag, &b.mag);
    (DecInt::from_mag(a.neg != b.neg, q), DecInt::from_mag(a.neg, r))
}

fn div_ref(a: &DecInt, b: &DecInt) -> DecInt {
    divrem_ref(a, b).0
}

fn rem_ref(a: &DecInt, b: &DecInt) -> DecInt {
    divrem_ref(a, b).1
}

macro_rules! binop {
    ($tr:ident, $m:ident, $atr:ident, $am:ident, $f:ident) => {
        impl $tr<&DecInt> for &DecInt {
            type Output = DecInt;
            fn $m(self, o: &DecInt) -> DecInt {
                $f(self, o)
            }
        }
        impl $tr<DecInt> for &DecInt {
            type Output = DecInt;
            fn $m(self, o: DecInt) -> DecInt {
                $f(self, &o)
            }
        }
        impl $tr<&DecInt> for DecInt {
            type Output = DecInt;
            fn $m(self, o: &DecInt) -> DecInt {
                $f(&self, o)
            }
        }
        impl $tr<DecInt> for DecInt {
            type Output = DecInt;
            fn $m(self, o: DecInt) -> DecInt {
                $f(&self, &o)
            }
        }
        impl $atr<&DecInt> for DecInt {
            fn $am(&mut self, o: &DecInt) {
                *self = $f(self, o);
            }
        }
        impl $atr<DecInt> for DecInt {
            fn $am(&mut self, o: DecInt) {
                *self = $f(self, &o);
            }
        }
        binop!(@prim $tr, $m, $atr, $am, $f, i32, u32, i64, u64, i128, usize);
    };
    (@prim $tr:ident, $m:ident, $atr:ident, $am:ident, $f:ident, $($t:ty),*) => {$(
        impl $tr<$t> for &DecInt {
            type Output = DecInt;
            fn $m(self, o: $t) -> DecInt {
                $f(self, &DecInt::from(o))
            }
        }
        impl $tr<$t> for DecInt {
            type Output = DecInt;
            fn $m(self, o: $t) -> DecInt {
                $f(&self, &DecInt::from(o))
            }
        }
        impl $atr<$t> for DecInt {
            fn $am(&mut self, o: $t) {
                *self = $f(self, &DecInt::from(o));
            }
        }
    )*};
}
binop!(Add, add, AddAssign, add_assign, add_ref);
binop!(Sub, sub, SubAssign, sub_assign, sub_ref);
binop!(Mul, mul, MulAssign, mul_assign, mul_ref);
binop!(Div, div, DivAssign, div_assign, div_ref);
binop!(Rem, rem, RemAssign, rem_assign, rem_ref);

impl Neg for DecInt {
    type Output = DecInt;
    fn neg(self) -> DecInt {
        let neg = !self.neg;
        DecInt::from_mag(neg, self.mag)
    }
}

impl Neg for &DecInt {
    type Output = DecInt;
    fn neg(self) -> DecInt {
        DecInt::from_mag(!self.neg, self.mag.clone())
    }
}

impl num_traits::Zero for DecInt {
    fn zero() -> DecInt {
        DecInt::zero()
    }
    fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }
}

impl num_traits::One for DecInt {
    fn one() -> DecInt {
        DecInt::one()
    }
}

impl num_traits::ToPrimitive for DecInt {
    fn to_i64(&self) -> Option<i64> {
        DecInt::to_i64(self)
    }
    fn to_u64(&self) -> Option<u64> {
        DecInt::to_u64(self)
    }
    fn to_i128(&self) -> Option<i128> {
        DecInt::to_i128(self)
    }
    fn to_f64(&self) -> Option<f64> {
        Some(DecInt::to_f64(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_bigint::BigInt;

    /// A deterministic stream of digit strings of assorted lengths.
    fn samples() -> Vec<String> {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut out = vec!["0".to_string(), "1".to_string(), "-1".to_string(), "999999999".to_string(),
            "1000000000".to_string(), "-1000000000000000000".to_string()];
        for len in [1usize, 5, 9, 10, 18, 19, 27, 40, 81, 200, 380, 400, 905, 1500] {
            for k in 0..4 {
                let mut s = String::new();
                if k % 2 == 1 {
                    s.push('-');
                }
                for i in 0..len {
                    let d = if k == 3 { 9 } else { next() % 10 };
                    s.push(char::from(b'0' + if i == 0 && d == 0 { 1 } else { d as u8 }));
                }
                out.push(s);
            }
        }
        out
    }

    #[test]
    fn arithmetic_matches_a_binary_big_integer() {
        let s = samples();
        for a in &s {
            let (da, ba): (DecInt, BigInt) = (a.parse().unwrap(), a.parse().unwrap());
            assert_eq!(da.to_string(), ba.to_string());
            assert_eq!(DecInt::from_signed_bytes_le(&ba.to_signed_bytes_le()), da);
            assert_eq!(da.to_signed_bytes_le(), ba.to_signed_bytes_le(), "{a}");
            assert_eq!(da.num_digits(), if ba == BigInt::from(0) { 0 } else { ba.magnitude().to_string().len() });
            if !da.is_negative() {
                assert_eq!(da.sqrt().to_string(), ba.sqrt().to_string(), "sqrt {a}");
            }
            for k in [0usize, 1, 8, 9, 10, 30] {
                let p = BigInt::from(10).pow(k as u32);
                assert_eq!(da.mul_pow10(k).to_string(), (&ba * &p).to_string());
                assert_eq!(da.div_pow10(k).to_string(), (&ba / &p).to_string());
            }
            for b in &s {
                let (db, bb): (DecInt, BigInt) = (b.parse().unwrap(), b.parse().unwrap());
                assert_eq!((&da + &db).to_string(), (&ba + &bb).to_string());
                assert_eq!((&da - &db).to_string(), (&ba - &bb).to_string());
                assert_eq!((&da * &db).to_string(), (&ba * &bb).to_string(), "{a} * {b}");
                assert_eq!(da.cmp(&db), ba.cmp(&bb));
                if !db.is_zero() {
                    assert_eq!((&da / &db).to_string(), (&ba / &bb).to_string(), "{a} / {b}");
                    assert_eq!((&da % &db).to_string(), (&ba % &bb).to_string(), "{a} % {b}");
                }
            }
        }
    }

    #[test]
    fn primitive_conversions() {
        for v in [0i128, 1, -1, 999_999_999, 1_000_000_000, i64::MAX as i128, i64::MIN as i128,
            u64::MAX as i128, i128::MAX, i128::MIN + 1, 10i128.pow(36), -(10i128.pow(27)) - 7] {
            let d = DecInt::from(v);
            assert_eq!(d.to_string(), v.to_string());
            assert_eq!(d.to_i128(), Some(v));
            assert_eq!(d.to_i64(), i64::try_from(v).ok());
        }
        assert_eq!(DecInt::from(u128::MAX).to_string(), u128::MAX.to_string());
        assert_eq!(DecInt::from(u128::MAX).to_i128(), None);
    }

    #[test]
    fn rounding_by_powers_of_ten() {
        let v: DecInt = "-12345".parse().unwrap();
        assert_eq!(v.round_pow10(1).to_string(), "-1235");
        assert_eq!(v.round_pow10(2).to_string(), "-123");
        assert_eq!(v.round_pow10(5).to_string(), "0");
        assert_eq!(v.round_pow10(9).to_string(), "0");
        assert_eq!("999999999".parse::<DecInt>().unwrap().round_pow10(1).to_string(), "100000000");
        assert_eq!("1000000000".parse::<DecInt>().unwrap().trailing_zero_digits(), 9);
    }
}
