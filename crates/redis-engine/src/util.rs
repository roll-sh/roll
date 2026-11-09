//! Small helpers with Redis-exact semantics: number parsing, glob matching, clocks.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    now_us().div_euclid(1000)
}

/// Microseconds since the Unix epoch: the system clock as last read on this thread (at most
/// [`clock::RESYNC_US`] ago), advanced by the monotonic counter. Commands read the time once or
/// twice each, and the system clock costs far more than the counter.
pub fn now_us() -> i64 {
    clock::wall_us()
}

/// Monotonic microseconds (an arbitrary origin), for durations.
pub fn mono_us() -> u64 {
    clock::mono_us()
}

fn system_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0)
}

pub mod clock {
    use std::cell::Cell;

    /// How long the wall clock is extrapolated from the counter before the system clock is read
    /// again (its steps and slewing show within this).
    pub const RESYNC_US: u64 = 1000;

    /// The CPU's virtual counter (`CNTVCT_EL0`), readable from user space, as Valkey's
    /// `monotonic_cntvct` reads it.
    #[cfg(target_arch = "aarch64")]
    mod counter {
        use std::sync::LazyLock;

        #[inline]
        fn ticks() -> u64 {
            let t: u64;
            // SAFETY: reads a register the kernel makes readable from EL0.
            unsafe { std::arch::asm!("mrs {}, cntvct_el0", out(reg) t, options(nomem, nostack, preserves_flags)) };
            t
        }

        /// Ticks per µs as a 32.32 fixed-point factor.
        static SCALE: LazyLock<u128> = LazyLock::new(|| {
            let f: u64;
            // SAFETY: as above.
            unsafe { std::arch::asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack, preserves_flags)) };
            ((1_000_000u128) << 32) / u128::from(f.max(1))
        });

        #[inline]
        pub fn us() -> u64 {
            ((u128::from(ticks()) * *SCALE) >> 32) as u64
        }
    }

    #[cfg(not(target_arch = "aarch64"))]
    mod counter {
        use std::sync::LazyLock;
        use std::time::Instant;

        static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);

        #[inline]
        pub fn us() -> u64 {
            ORIGIN.elapsed().as_micros() as u64
        }
    }

    #[inline]
    pub fn mono_us() -> u64 {
        counter::us()
    }

    thread_local! {
        /// (counter µs, system µs) at the last read of the system clock.
        static BASE: Cell<(u64, i64)> = const { Cell::new((0, i64::MIN)) };
    }

    pub fn wall_us() -> i64 {
        let mono = counter::us();
        BASE.with(|b| {
            let (at, wall) = b.get();
            let since = mono.wrapping_sub(at);
            if wall == i64::MIN || since > RESYNC_US {
                let now = super::system_us();
                b.set((mono, now));
                return now;
            }
            wall + since as i64
        })
    }
}

/// `string2ll`: an optional `-`, no `+`, no leading zeros, no spaces, at most 20 chars.
pub fn parse_i64(s: &[u8]) -> Option<i64> {
    if s.is_empty() || s.len() > 20 {
        return None;
    }
    if s == b"0" {
        return Some(0);
    }
    let (neg, digits) = match s[0] {
        b'-' => (true, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() || !(b'1'..=b'9').contains(&digits[0]) {
        return None;
    }
    let mut v: u64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u64)?;
    }
    if neg {
        if v > i64::MAX as u64 + 1 {
            None
        } else {
            Some((v as i64).wrapping_neg())
        }
    } else if v > i64::MAX as u64 {
        None
    } else {
        Some(v as i64)
    }
}

/// `string2ull`-style unsigned parse with the same strictness as [`parse_i64`].
pub fn parse_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() || s.len() > 20 {
        return None;
    }
    if s == b"0" {
        return Some(0);
    }
    if !(b'1'..=b'9').contains(&s[0]) {
        return None;
    }
    let mut v: u64 = 0;
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u64)?;
    }
    Some(v)
}

/// The bytes a C string function would see (up to the first NUL).
pub fn cstr(a: &[u8]) -> &[u8] {
    match a.iter().position(|&c| c == 0) {
        Some(i) => &a[..i],
        None => a,
    }
}

/// `strtoull(s, &end, 10)` accepting the whole string: leading whitespace, a sign (negative
/// values wrap), digits; no overflow.
pub fn strtoull(s: &[u8]) -> Option<u64> {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let start = i;
    let mut v: u64 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        v = v.checked_mul(10)?.checked_add((s[i] - b'0') as u64)?;
        i += 1;
    }
    if i == start || i != s.len() {
        return None;
    }
    Some(if neg { v.wrapping_neg() } else { v })
}

/// C `strtoll(s, &end, 10)` with `*end == '\0'` required: leading whitespace and a sign allowed,
/// overflow clamps, the empty string is 0.
pub fn strtoll_full(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return Some(0);
    }
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let start = i;
    let mut v: i128 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        v = (v * 10 + (s[i] - b'0') as i128).min(1 << 64);
        i += 1;
    }
    if i == start || i != s.len() {
        return None;
    }
    let v = if neg { -v } else { v };
    Some(v.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

/// Redis' `string2ull`.
pub fn string2ull(s: &[u8]) -> Option<u64> {
    if let Some(v) = parse_i64(s) {
        return if v < 0 { None } else { Some(v as u64) };
    }
    strtoull(s)
}

/// Is `s` the canonical text of an i64 (so it can be stored as an integer)?
pub fn is_int(s: &[u8]) -> bool {
    parse_i64(s).is_some()
}

pub fn fmt_i64(i: i64) -> Vec<u8> {
    itoa::Buffer::new().format(i).as_bytes().to_vec()
}

/// Glob matching exactly like Redis' `stringmatchlen`: `*`, `?`, `[abc]`, `[^a-z]`, `\x`.
pub fn glob_match(pattern: &[u8], s: &[u8], nocase: bool) -> bool {
    let mut skip_longer = false;
    glob(pattern, s, nocase, &mut skip_longer, 0)
}

fn lower(c: u8, nocase: bool) -> u8 {
    if nocase { c.to_ascii_lowercase() } else { c }
}

fn glob(mut p: &[u8], mut s: &[u8], nocase: bool, skip_longer: &mut bool, nesting: u32) -> bool {
    // Protect against abusive patterns like "*****…*a" (Redis caps nesting at 1000).
    if nesting > 1000 {
        return false;
    }
    while !p.is_empty() && !s.is_empty() {
        match p[0] {
            b'*' => {
                while p.len() > 1 && p[1] == b'*' {
                    p = &p[1..];
                }
                if p.len() == 1 {
                    return true;
                }
                while !s.is_empty() {
                    if glob(&p[1..], s, nocase, skip_longer, nesting + 1) {
                        return true;
                    }
                    if *skip_longer {
                        return false;
                    }
                    s = &s[1..];
                }
                *skip_longer = true;
                return false;
            }
            b'?' => s = &s[1..],
            b'[' => {
                p = &p[1..];
                let not = p.first() == Some(&b'^');
                if not {
                    p = &p[1..];
                }
                let mut matched = false;
                loop {
                    if p.is_empty() {
                        // Unterminated class: Redis steps back so the outer loop sees the end.
                        break;
                    }
                    if p[0] == b'\\' && p.len() >= 2 {
                        p = &p[1..];
                        if p[0] == s[0] {
                            matched = true;
                        }
                    } else if p[0] == b']' {
                        break;
                    } else if p.len() >= 3 && p[1] == b'-' {
                        let (mut start, mut end) = (p[0], p[2]);
                        if start > end {
                            std::mem::swap(&mut start, &mut end);
                        }
                        let c = lower(s[0], nocase);
                        let (start, end) = (lower(start, nocase), lower(end, nocase));
                        p = &p[2..];
                        if c >= start && c <= end {
                            matched = true;
                        }
                    } else if lower(p[0], nocase) == lower(s[0], nocase) {
                        matched = true;
                    }
                    p = &p[1..];
                }
                if not {
                    matched = !matched;
                }
                if !matched {
                    return false;
                }
                s = &s[1..];
                if p.is_empty() {
                    // `[` without a closing bracket consumed the rest of the pattern.
                    return s.is_empty();
                }
            }
            b'\\' if p.len() >= 2 => {
                p = &p[1..];
                if lower(p[0], nocase) != lower(s[0], nocase) {
                    return false;
                }
                s = &s[1..];
            }
            c => {
                if lower(c, nocase) != lower(s[0], nocase) {
                    return false;
                }
                s = &s[1..];
            }
        }
        p = &p[1..];
        if s.is_empty() {
            while !p.is_empty() && p[0] == b'*' {
                p = &p[1..];
            }
            break;
        }
    }
    p.is_empty() && s.is_empty()
}

/// Case-insensitive ASCII comparison for option names.
#[inline]
pub fn eq_ic(a: &[u8], b: &str) -> bool {
    a.eq_ignore_ascii_case(b.as_bytes())
}

/// Lowercase ASCII copy of an argument, for command and option lookup.
pub fn lower_str(a: &[u8]) -> String {
    String::from_utf8_lossy(a).to_ascii_lowercase()
}

/// A small fast RNG for sampling (eviction, expiry, SRANDMEMBER…). xorshift64*.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new() -> Self {
        let mut seed = [0u8; 8];
        let t = now_us() as u64;
        seed.copy_from_slice(&t.to_le_bytes());
        Self(u64::from_le_bytes(seed) ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }
    pub fn seeded(seed: u64) -> Self {
        Self(seed | 1)
    }
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in `0..n` (`n > 0`).
    #[inline]
    pub fn below(&mut self, n: usize) -> usize {
        ((self.next_u64() as u128 * n as u128) >> 64) as usize
    }
    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

impl Default for Rng {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bytes_eq_matches_slices() {
        let a: Vec<u8> = (0..40u8).collect();
        for n in 0..40 {
            let x = &a[..n];
            assert!(bytes_eq(x, x));
            assert!(!bytes_eq(x, &a[..n + 1]));
            for k in 0..n {
                let mut y = x.to_vec();
                y[k] ^= 1;
                assert!(!bytes_eq(x, &y), "len {n} differing at {k}");
            }
        }
    }

    use super::*;

    #[test]
    fn ints_like_string2ll() {
        assert_eq!(parse_i64(b"0"), Some(0));
        assert_eq!(parse_i64(b"-1"), Some(-1));
        assert_eq!(parse_i64(b"9223372036854775807"), Some(i64::MAX));
        assert_eq!(parse_i64(b"-9223372036854775808"), Some(i64::MIN));
        assert_eq!(parse_i64(b"9223372036854775808"), None);
        for bad in [&b"-0"[..], b"01", b"+1", b" 1", b"1 ", b"", b"-", b"1.0", b"0x10"] {
            assert_eq!(parse_i64(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn globs() {
        let t = |p: &str, s: &str| glob_match(p.as_bytes(), s.as_bytes(), false);
        assert!(t("*", "anything"));
        assert!(t("h?llo", "hello"));
        assert!(t("h*llo", "heeeello"));
        assert!(t("h[ae]llo", "hallo"));
        assert!(!t("h[^e]llo", "hello"));
        assert!(t("h[a-b]llo", "hbllo"));
        assert!(t("h[b-a]llo", "hbllo"));
        assert!(t("a\\*b", "a*b"));
        assert!(!t("a\\*b", "axb"));
        assert!(t("*a*", "bab"));
        assert!(!t("a", "ab"));
        assert!(t("ab*", "ab"));
        assert!(glob_match(b"HELLO", b"hello", true));
        assert!(!t("*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*b", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    }
}

/// C's `%g` (six significant digits, trailing zeros dropped, exponent form outside 1e-4..1e6).
pub fn fmt_g(v: f64) -> String {
    fmt_g_prec(v, 6)
}

/// C's `%.<p>g`.
pub fn fmt_g_prec(v: f64, p: usize) -> String {
    let p = p.max(1);
    if v.is_nan() {
        return if v.is_sign_negative() { "-nan" } else { "nan" }.into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-inf" } else { "inf" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.into();
    }
    fn trim(s: &str) -> &str {
        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { s }
    }
    let e = format!("{v:.*e}", p - 1);
    let (mant, exp) = e.split_once('e').expect("exponent form");
    let x: i32 = exp.parse().expect("exponent");
    if x >= -4 && x < p as i32 {
        trim(&format!("{:.*}", (p as i32 - 1 - x) as usize, v)).to_string()
    } else {
        format!("{}e{}{:02}", trim(mant), if x < 0 { '-' } else { '+' }, x.abs())
    }
}

#[cfg(test)]
mod fmt_g_tests {
    use super::fmt_g;

    #[test]
    fn like_printf() {
        assert_eq!(fmt_g(0.005), "0.005");
        assert_eq!(fmt_g(0.0003125), "0.0003125");
        assert_eq!(fmt_g(7.8125e-05), "7.8125e-05");
        assert_eq!(fmt_g(100000.0), "100000");
        assert_eq!(fmt_g(1000000.0), "1e+06");
        assert_eq!(fmt_g(123456789.0), "1.23457e+08");
        assert_eq!(fmt_g(0.1), "0.1");
        assert_eq!(fmt_g(1.5), "1.5");
        assert_eq!(fmt_g(1e-300), "1e-300");
        assert_eq!(super::fmt_g_prec(0.1 + 0.2, 12), "0.3");
        assert_eq!(super::fmt_g_prec(1.0 / 3.0, 12), "0.333333333333");
        assert_eq!(super::fmt_g_prec(1e300, 12), "1e+300");
    }
}

/// Byte-string equality for keys and members: short ones are compared inline in machine words
/// (the usual 5-32 byte keys cost a call into `memcmp` otherwise).
#[inline(always)]
pub fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    let n = a.len();
    if n != b.len() {
        return false;
    }
    let (pa, pb) = (a.as_ptr(), b.as_ptr());
    // SAFETY: every read is within the first `n` bytes of both slices (overlapping at the end).
    unsafe {
        let w64 = |p: *const u8, i: usize| (p.add(i) as *const u64).read_unaligned();
        let w32 = |p: *const u8, i: usize| (p.add(i) as *const u32).read_unaligned();
        match n {
            0 => true,
            1..=3 => a == b,
            4..=8 => w32(pa, 0) == w32(pb, 0) && w32(pa, n - 4) == w32(pb, n - 4),
            9..=16 => w64(pa, 0) == w64(pb, 0) && w64(pa, n - 8) == w64(pb, n - 8),
            17..=32 => {
                w64(pa, 0) == w64(pb, 0) && w64(pa, 8) == w64(pb, 8) && w64(pa, n - 16) == w64(pb, n - 16) && w64(pa, n - 8) == w64(pb, n - 8)
            }
            _ => a == b,
        }
    }
}

/// How the process hands free memory back to the system (the server's allocator registers it).
static PURGE: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Register [`purge_memory`]'s implementation (once).
pub fn set_memory_purge(f: fn()) {
    let _ = PURGE.set(f);
}

/// Give the calling thread's free memory back to the system (MEMORY PURGE, idle databases).
pub fn purge_memory() {
    if let Some(f) = PURGE.get() {
        f();
    }
}
