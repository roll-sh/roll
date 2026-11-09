//! UUIDs and version-1 (time-based) UUIDs.
//!
//! Follows `org.apache.cassandra.utils.TimeUUID` (Apache-2.0): the timestamp counts 100 ns
//! intervals since 1582-10-15; `now()` uses microsecond resolution and is strictly increasing
//! per process; `min_timeuuid` / `max_timeuuid` use the clock-sequence-and-node values that sort
//! first / last under `TimeUUIDType`'s signed-byte comparison (`0x8080808080808080` and
//! `0x7f7f7f7f7f7f7f7f`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};

/// The UUID epoch (1582-10-15) in Unix milliseconds.
pub const UUID_EPOCH_UNIX_MILLIS: i64 = -12_219_292_800_000;
pub const MIN_CLOCK_SEQ_AND_NODE: u64 = 0x8080_8080_8080_8080;
pub const MAX_CLOCK_SEQ_AND_NODE: u64 = 0x7f7f_7f7f_7f7f_7f7f;

/// Parses `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (hex digits of either case).
pub fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let b = s.as_bytes();
    if b.len() != 36 || b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' {
        return None;
    }
    let mut out = [0u8; 16];
    let mut k = 0;
    let mut i = 0;
    while i < 36 {
        if b[i] == b'-' {
            i += 1;
            continue;
        }
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (*b.get(i + 1)? as char).to_digit(16)?;
        if b[i + 1] == b'-' {
            return None;
        }
        out[k] = (hi * 16 + lo) as u8;
        k += 1;
        i += 2;
    }
    Some(out)
}

pub fn format_uuid(u: &[u8; 16]) -> String {
    let h = crate::wire::hex(u);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// The version nibble.
pub fn version(u: &[u8]) -> u8 {
    u.get(6).map(|b| b >> 4).unwrap_or(0)
}

fn msb(u: &[u8; 16]) -> u64 {
    u64::from_be_bytes(u[..8].try_into().unwrap_or([0; 8]))
}

fn lsb(u: &[u8; 16]) -> u64 {
    u64::from_be_bytes(u[8..].try_into().unwrap_or([0; 8]))
}

/// Builds a version-1 UUID from a raw 60-bit timestamp and the low 64 bits.
pub fn from_raw_timestamp(raw: i64, lsb: u64) -> [u8; 16] {
    let raw = raw as u64;
    let msb = 0x1000u64 | (raw >> 48) | ((raw & 0xFFFF_0000_0000) >> 16) | (raw << 32);
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&msb.to_be_bytes());
    out[8..].copy_from_slice(&lsb.to_be_bytes());
    out
}

/// The raw 60-bit timestamp (100 ns since 1582-10-15) of a version-1 UUID.
pub fn raw_timestamp(u: &[u8; 16]) -> i64 {
    let m = msb(u) & !0xF000u64;
    ((m & 0xFFFF) << 48 | (m & 0xFFFF_0000) << 16 | (m >> 32)) as i64
}

/// Unix microseconds of a timeuuid.
pub fn timeuuid_to_micros(u: &[u8; 16]) -> i64 {
    raw_timestamp(u) / 10 + UUID_EPOCH_UNIX_MILLIS * 1000
}

/// `unixTimestampOf` / `toUnixTimestamp(timeuuid)`: Unix milliseconds.
pub fn unix_timestamp_of(u: &[u8; 16]) -> i64 {
    timeuuid_to_micros(u).div_euclid(1000)
}

fn unix_millis_to_raw(ms: i64, tenths: i64) -> i64 {
    ms.wrapping_mul(10_000)
        .wrapping_sub(UUID_EPOCH_UNIX_MILLIS * 10_000)
        .wrapping_add(tenths)
}

/// `minTimeuuid(ms)`: the smallest timeuuid at that millisecond.
pub fn min_timeuuid(ms: i64) -> [u8; 16] {
    from_raw_timestamp(unix_millis_to_raw(ms, 0), MIN_CLOCK_SEQ_AND_NODE)
}

/// `maxTimeuuid(ms)`: the largest timeuuid at that millisecond.
pub fn max_timeuuid(ms: i64) -> [u8; 16] {
    from_raw_timestamp(
        unix_millis_to_raw(ms.wrapping_add(1), 0) - 1,
        MAX_CLOCK_SEQ_AND_NODE,
    )
}

/// A timeuuid at a given Unix microsecond with the given low 64 bits.
pub fn at_unix_micros(micros: i64, lsb: u64) -> [u8; 16] {
    from_raw_timestamp(
        micros
            .wrapping_mul(10)
            .wrapping_sub(UUID_EPOCH_UNIX_MILLIS * 10_000),
        lsb,
    )
}

/// Generates timeuuids: strictly increasing microsecond timestamps (falling behind the clock
/// when called faster than once a microsecond), a random 14-bit clock sequence and a random node
/// with the multicast bit set, chosen once per generator.
pub struct Generator {
    last_micros: AtomicI64,
    clock_seq_and_node: u64,
}

impl Generator {
    pub fn new() -> Generator {
        let mut r = [0u8; 8];
        let _ = getrandom::fill(&mut r);
        let rnd = u64::from_be_bytes(r);
        // Variant 10, 14-bit clock sequence, 48-bit node with the multicast bit (RFC 4122 §4.5).
        let node = (rnd & 0xFFFF_FFFF_FFFF) | 0x0100_0000_0000;
        let clock = (rnd >> 48) & 0x3FFF;
        Generator {
            last_micros: AtomicI64::new(0),
            clock_seq_and_node: 0x8000_0000_0000_0000 | (clock << 48) | node,
        }
    }

    /// A generator with a fixed clock sequence and node (tests, deterministic ids).
    pub fn with_clock_seq_and_node(lsb: u64) -> Generator {
        Generator {
            last_micros: AtomicI64::new(0),
            clock_seq_and_node: lsb,
        }
    }

    pub fn clock_seq_and_node(&self) -> u64 {
        self.clock_seq_and_node
    }

    fn next_micros(&self, now_micros: i64) -> i64 {
        let mut last = self.last_micros.load(Ordering::Relaxed);
        loop {
            let next = if now_micros > last {
                now_micros
            } else {
                last + 1
            };
            match self.last_micros.compare_exchange_weak(
                last,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return next,
                Err(actual) => last = actual,
            }
        }
    }

    /// `now()`.
    pub fn next(&self) -> [u8; 16] {
        let now = crate::temporal::now_millis() * 1000;
        self.next_at(now)
    }

    /// The next timeuuid as if the clock read `now_micros`.
    pub fn next_at(&self, now_micros: i64) -> [u8; 16] {
        at_unix_micros(self.next_micros(now_micros), self.clock_seq_and_node)
    }
}

impl Default for Generator {
    fn default() -> Self {
        Generator::new()
    }
}

/// The process-wide generator behind [`now`].
pub fn global() -> &'static Generator {
    static G: OnceLock<Generator> = OnceLock::new();
    G.get_or_init(Generator::new)
}

/// `now()`: a fresh timeuuid from the process-wide generator.
pub fn now() -> [u8; 16] {
    global().next()
}

/// A random (version 4) UUID, as `uuid()` returns.
pub fn random_uuid() -> [u8; 16] {
    let mut u = [0u8; 16];
    let _ = getrandom::fill(&mut u);
    u[6] = (u[6] & 0x0f) | 0x40;
    u[8] = (u[8] & 0x3f) | 0x80;
    u
}

/// The low 64 bits, for comparisons.
pub fn lsb_of(u: &[u8; 16]) -> u64 {
    lsb(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format() {
        let u = parse_uuid("550E8400-e29b-41d4-A716-446655440000").unwrap();
        assert_eq!(format_uuid(&u), "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(version(&u), 4);
        for bad in [
            "",
            "550e8400e29b41d4a716446655440000",
            "550e8400-e29b-41d4-a716-44665544000",
            "550e8400-e29b-41d4-a716-44665544000g",
            "550e8400-e29b-41d4-a716+446655440000",
            "550e840-0e29b-41d4-a716-446655440000",
        ] {
            assert!(parse_uuid(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn min_max_timeuuid_match_references() {
        // minTimeuuid / maxTimeuuid of -1 ms on both images.
        assert_eq!(
            format_uuid(&min_timeuuid(-1)),
            "138118f0-1dd2-11b2-8080-808080808080"
        );
        assert_eq!(
            format_uuid(&max_timeuuid(-1)),
            "13813fff-1dd2-11b2-7f7f-7f7f7f7f7f7f"
        );
        assert_eq!(unix_timestamp_of(&min_timeuuid(-1)), -1);
        assert_eq!(unix_timestamp_of(&max_timeuuid(-1)), -1);
        assert_eq!(
            format_uuid(&min_timeuuid(0)),
            "13814000-1dd2-11b2-8080-808080808080"
        );
    }

    #[test]
    fn timestamps_of_known_uuid() {
        // toUnixTimestamp(50554d6e-29bb-11e5-b345-feff819cdc9f) = 1436832018805 on both images.
        let u = parse_uuid("50554d6e-29bb-11e5-b345-feff819cdc9f").unwrap();
        assert_eq!(unix_timestamp_of(&u), 1_436_832_018_805);
        assert_eq!(timeuuid_to_micros(&u), 1_436_832_018_805_899);
        let raw = raw_timestamp(&u);
        assert_eq!(from_raw_timestamp(raw, lsb_of(&u)), u);
    }

    #[test]
    fn generator_is_monotonic() {
        let g = Generator::with_clock_seq_and_node(0x8000_0100_0000_0001);
        let a = g.next_at(1_000_000);
        let b = g.next_at(1_000_000);
        let c = g.next_at(999_999);
        assert!(timeuuid_to_micros(&a) < timeuuid_to_micros(&b));
        assert!(timeuuid_to_micros(&b) < timeuuid_to_micros(&c));
        assert_eq!(version(&a), 1);
        assert_eq!(timeuuid_to_micros(&a), 1_000_000);
        let n = now();
        assert_eq!(version(&n), 1);
        assert_eq!(n[8] & 0xc0, 0x80);
        assert!((unix_timestamp_of(&n) - crate::temporal::now_millis()).abs() < 10_000);
        assert!(global().clock_seq_and_node() & 0x0100_0000_0000 != 0);
        let r = random_uuid();
        assert_eq!(version(&r), 4);
        assert_eq!(r[8] & 0xc0, 0x80);
    }
}

#[cfg(test)]
mod more_tests {
    use super::*;

    #[test]
    fn micros_and_generators() {
        let u = at_unix_micros(1_436_832_018_805_899, 0x8000_0000_0000_0000);
        assert_eq!(timeuuid_to_micros(&u), 1_436_832_018_805_899);
        assert_eq!(version(&u), 1);
        assert_eq!(lsb_of(&u), 0x8000_0000_0000_0000);
        let g = Generator::default();
        assert_ne!(g.clock_seq_and_node(), 0);
        assert_eq!(g.clock_seq_and_node() >> 62, 0b10);
        let a = g.next();
        let b = g.next();
        assert_eq!(crate::compare_timeuuid(&a, &b), std::cmp::Ordering::Less);
        assert_eq!(version(&[]), 0);
        assert_eq!(
            unix_timestamp_of(&min_timeuuid(i64::from(i32::MAX))),
            i64::from(i32::MAX)
        );
    }
}
