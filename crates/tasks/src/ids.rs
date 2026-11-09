//! Time-ordered ids (ULID layout: 48-bit milliseconds + 80 random bits, Crockford base32,
//! lowercase). Ids from one process are strictly increasing, so they sort in creation order;
//! FIFO groups and list cursors rely on that.

use std::sync::Mutex;

const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

static LAST: Mutex<(u64, u128)> = Mutex::new((0, 0));

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", ulid())
}

pub fn ulid() -> String {
    let now = crate::model::now_ms() as u64;
    let (ms, rand) = {
        let mut last = LAST.lock().unwrap();
        if now <= last.0 {
            last.1 = (last.1 + 1) & ((1u128 << 80) - 1);
        } else {
            *last = (now, random_u128() & ((1u128 << 79) - 1));
        }
        *last
    };
    let v: u128 = ((ms as u128) << 80) | rand;
    let mut out = [0u8; 26];
    for (i, c) in out.iter_mut().enumerate() {
        let shift = 125 - 5 * i as i32;
        let idx = if shift >= 0 { (v >> shift) & 31 } else { (v << -shift) & 31 };
        *c = ALPHABET[idx as usize];
    }
    String::from_utf8(out.to_vec()).unwrap()
}

/// Milliseconds encoded in an id made by `new_id`.
pub fn id_time(id: &str) -> Option<i64> {
    let s = id.rsplit('_').next()?;
    if s.len() != 26 {
        return None;
    }
    let mut v: u128 = 0;
    for b in s.bytes().take(10) {
        v = (v << 5) | ALPHABET.iter().position(|&c| c == b)? as u128;
    }
    Some(v as i64)
}

pub fn random_u128() -> u128 {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("os randomness");
    u128::from_le_bytes(b)
}

/// Uniform in [0, 1).
pub fn random_f64() -> f64 {
    (random_u128() >> 75) as f64 / (1u64 << 53) as f64
}

pub fn token(bytes: usize) -> Vec<u8> {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("os randomness");
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_sort_by_creation() {
        let ids: Vec<String> = (0..2000).map(|_| new_id("msg")).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        assert!(ids.windows(2).all(|w| w[0] != w[1]));
    }

    #[test]
    fn time_round_trips() {
        let before = crate::model::now_ms();
        let id = new_id("run");
        let t = id_time(&id).unwrap();
        assert!(t >= before && t <= crate::model::now_ms(), "{t} {before}");
        assert!(id.starts_with("run_") && id.len() == 30);
    }

    #[test]
    fn random_in_range() {
        for _ in 0..1000 {
            let r = random_f64();
            assert!((0.0..1.0).contains(&r));
        }
    }
}
