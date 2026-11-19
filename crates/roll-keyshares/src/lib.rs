//! Splitting a key into shares (docs/secrets.md, "Splitting the deployment key"): Shamir's secret
//! sharing over GF(2^8), as Vault's unseal and recovery keys. Any `threshold` of the shares give
//! the key back; fewer give nothing about it. Used by `roll secrets key split` and `combine` on
//! the operator's machine only (a crate of its own so the CLI doesn't carry the secrets server): a share never goes to rolld, the database or a log.
//!
//! A share is `rlks_` and base64url (no padding) of
//!
//! ```text
//! version (1) ‖ set id (8) ‖ threshold (1) ‖ index (1) ‖ y (key length + 8) ‖ checksum (4)
//! ```
//!
//! - The checksum is the first 4 bytes of SHA-256 of `rlks_` and everything before it, so a
//!   mistyped or truncated share is refused on its own.
//! - The set id is random per split, so shares of two splits (before and after a rotation, or of
//!   two keys) are never combined.
//! - What's shared is the key followed by an 8-byte check, the first bytes of
//!   SHA-256(`roll-key-shares:v1` ‖ set id ‖ key). Shares that pass their checksums but don't
//!   belong together (one re-encoded by hand) combine into a check that fails: an error, never a
//!   wrong key. The check is inside the shared secret, so fewer than `threshold` holders learn
//!   nothing from it either.
//!
//! The field is GF(2^8) with AES's polynomial (x^8 + x^4 + x^3 + x + 1), multiplied without
//! tables or branches on secret bytes. Written in-tree (about 100 lines, with the vectors below)
//! rather than taken from a crate: the workspace has no audited Shamir crate, and the maintained
//! ones bring their own share encodings without set ids or checks.

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// What every share starts with.
pub const PREFIX: &str = "rlks_";
const VERSION: u8 = 1;
const CHECK: &[u8] = b"roll-key-shares:v1";
const CHECK_LEN: usize = 8;
const SUM_LEN: usize = 4;
/// version, set id, threshold, index.
const HEAD: usize = 1 + 8 + 1 + 1;
/// The longest key split (a deployment key is a line of text).
pub const MAX_KEY: usize = 4096;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyShareError {
    #[error("a split needs 2 to 255 shares and a threshold from 2 to the number of shares (got {threshold} of {shares})")]
    Parameters { shares: usize, threshold: usize },
    #[error("the key is empty or longer than {MAX_KEY} bytes")]
    KeyLength,
    #[error("share {0} isn't a roll key share (rlks_…)")]
    NotAShare(usize),
    #[error("share {0} is damaged: its checksum doesn't match (mistyped or cut short?)")]
    Corrupt(usize),
    #[error("share {0} is from a different split than share 1 (an older split, or another key's)")]
    MixedSets(usize),
    #[error("share #{0} of the set was given twice")]
    Duplicate(u8),
    #[error("{have} distinct shares given; this split needs {need}")]
    TooFew { have: usize, need: usize },
    #[error("these shares don't combine into the key they were split from (one was altered)")]
    Mismatch,
}

/// One share, parsed and its checksum checked. Its `y` bytes are zeroized on drop.
pub struct Share {
    pub set: [u8; 8],
    pub threshold: u8,
    pub index: u8,
    y: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for Share {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Share(#{} of a {}-share threshold)", self.index, self.threshold)
    }
}

impl Share {
    /// Parse the `n`th share given (1-based, for the error).
    pub fn parse(n: usize, text: &str) -> Result<Share, KeyShareError> {
        use base64::Engine as _;
        let body = text.trim().strip_prefix(PREFIX).ok_or(KeyShareError::NotAShare(n))?;
        let raw = Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(body).map_err(|_| KeyShareError::NotAShare(n))?);
        if raw.len() < HEAD + 1 + CHECK_LEN + SUM_LEN {
            return Err(KeyShareError::NotAShare(n));
        }
        let (data, sum) = raw.split_at(raw.len() - SUM_LEN);
        if checksum(data) != sum {
            return Err(KeyShareError::Corrupt(n));
        }
        let (threshold, index) = (data[9], data[10]);
        if data[0] != VERSION || threshold < 2 || index == 0 {
            return Err(KeyShareError::NotAShare(n));
        }
        Ok(Share { set: data[1..9].try_into().expect("8 bytes"), threshold, index, y: Zeroizing::new(data[HEAD..].to_vec()) })
    }

    fn encode(&self) -> String {
        use base64::Engine as _;
        let mut raw = Zeroizing::new(Vec::with_capacity(HEAD + self.y.len() + SUM_LEN));
        raw.push(VERSION);
        raw.extend_from_slice(&self.set);
        raw.extend_from_slice(&[self.threshold, self.index]);
        raw.extend_from_slice(&self.y);
        let sum = checksum(&raw);
        raw.extend_from_slice(&sum);
        format!("{PREFIX}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&raw[..]))
    }
}

fn checksum(data: &[u8]) -> [u8; SUM_LEN] {
    let d = Sha256::new().chain_update(PREFIX).chain_update(data).finalize();
    d[..SUM_LEN].try_into().expect("4 bytes")
}

fn key_check(set: &[u8; 8], key: &[u8]) -> [u8; CHECK_LEN] {
    let d = Sha256::new().chain_update(CHECK).chain_update(set).chain_update(key).finalize();
    d[..CHECK_LEN].try_into().expect("8 bytes")
}

/// Split `key` into `shares` shares, any `threshold` of which give it back.
pub fn split(key: &[u8], shares: usize, threshold: usize) -> Result<Vec<String>, KeyShareError> {
    let mut set = [0u8; 8];
    getrandom::fill(&mut set).expect("system randomness");
    split_with(key, shares, threshold, set, |c| getrandom::fill(c).expect("system randomness"))
}

/// [`split`] with the set id and the coefficients' source given (the vectors below).
fn split_with(key: &[u8], shares: usize, threshold: usize, set: [u8; 8], mut random: impl FnMut(&mut [u8])) -> Result<Vec<String>, KeyShareError> {
    if !(2..=255).contains(&shares) || threshold < 2 || threshold > shares {
        return Err(KeyShareError::Parameters { shares, threshold });
    }
    if key.is_empty() || key.len() > MAX_KEY {
        return Err(KeyShareError::KeyLength);
    }
    let mut secret = Zeroizing::new(key.to_vec());
    secret.extend_from_slice(&key_check(&set, key));
    // One polynomial per byte: the byte, then threshold - 1 random coefficients.
    let mut coefficients = Zeroizing::new(vec![0u8; threshold - 1]);
    let mut ys = Zeroizing::new(vec![0u8; shares * secret.len()]);
    for (b, s) in secret.iter().enumerate() {
        random(&mut coefficients);
        for x in 1..=shares {
            let y = coefficients.iter().rev().fold(0u8, |y, c| mul(y, x as u8) ^ c);
            ys[(x - 1) * secret.len() + b] = mul(y, x as u8) ^ s;
        }
    }
    Ok((1..=shares)
        .map(|x| {
            let y = Zeroizing::new(ys[(x - 1) * secret.len()..x * secret.len()].to_vec());
            Share { set, threshold: threshold as u8, index: x as u8, y }.encode()
        })
        .collect())
}

/// The key the shares were split from. Every share is checked (its checksum, its set, no index
/// twice), then the first `threshold` of them are combined and the key's check verified.
pub fn combine<S: AsRef<str>>(shares: &[S]) -> Result<Zeroizing<Vec<u8>>, KeyShareError> {
    let parsed = shares.iter().enumerate().map(|(i, s)| Share::parse(i + 1, s.as_ref())).collect::<Result<Vec<_>, _>>()?;
    let Some(first) = parsed.first() else { return Err(KeyShareError::TooFew { have: 0, need: 2 }) };
    let mut seen = [false; 256];
    for (i, s) in parsed.iter().enumerate() {
        if s.set != first.set || s.threshold != first.threshold || s.y.len() != first.y.len() {
            return Err(KeyShareError::MixedSets(i + 1));
        }
        if std::mem::replace(&mut seen[s.index as usize], true) {
            return Err(KeyShareError::Duplicate(s.index));
        }
    }
    let need = first.threshold as usize;
    if parsed.len() < need {
        return Err(KeyShareError::TooFew { have: parsed.len(), need });
    }
    let points = &parsed[..need];
    // Lagrange at 0: in GF(2^8), 0 - x_j = x_j and x_i - x_j = x_i ^ x_j.
    let weights: Vec<u8> = points
        .iter()
        .map(|p| points.iter().filter(|q| q.index != p.index).fold(1u8, |w, q| mul(w, mul(q.index, inv(q.index ^ p.index)))))
        .collect();
    let mut secret = Zeroizing::new(vec![0u8; first.y.len()]);
    for (p, w) in points.iter().zip(&weights) {
        for (s, y) in secret.iter_mut().zip(p.y.iter()) {
            *s ^= mul(*y, *w);
        }
    }
    let key_len = secret.len() - CHECK_LEN;
    // Not secret-dependent timing worth hiding: the caller holds the shares already.
    if key_check(&first.set, &secret[..key_len])[..] != secret[key_len..] {
        return Err(KeyShareError::Mismatch);
    }
    secret.truncate(key_len);
    Ok(secret)
}

/// Multiplication in GF(2^8) modulo x^8 + x^4 + x^3 + x + 1, in constant time.
fn mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        p ^= a & (b & 1).wrapping_neg();
        let carry = (a >> 7).wrapping_neg();
        a = (a << 1) ^ (0x1b & carry);
        b >>= 1;
    }
    p
}

/// The inverse of a non-zero element: a^254. Only ever of share indexes (public).
fn inv(a: u8) -> u8 {
    let mut r = 1u8;
    for _ in 0..254 {
        r = mul(r, a);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"fake-deployment-key-for-tests-0123456789";

    fn subsets(n: usize) -> impl Iterator<Item = Vec<usize>> {
        (1u32..1 << n).map(move |m| (0..n).filter(|i| m & (1 << i) != 0).collect())
    }

    #[test]
    fn field_vectors() {
        // FIPS-197 §4.2: {57}·{83} = {c1}, {57}·{13} = {fe}; §5.1.1: {53}⁻¹ = {ca}.
        assert_eq!((mul(0x57, 0x83), mul(0x57, 0x13), inv(0x53)), (0xc1, 0xfe, 0xca));
        assert!((1..=255u8).all(|a| mul(a, inv(a)) == 1));
    }

    #[test]
    fn a_known_split_gives_known_shares() {
        // Threshold 2 with every coefficient 1: share x of byte s is s ^ x.
        let shares = split_with(b"\x42", 3, 2, [0xaa; 8], |c| c.fill(1)).unwrap();
        let ys: Vec<Vec<u8>> = shares.iter().enumerate().map(|(i, s)| Share::parse(i + 1, s).unwrap().y[..1].to_vec()).collect();
        assert_eq!(ys, [[0x43], [0x40], [0x41]]);
        // The encoding is stable: this exact text combines (with share 3) for as long as v1 lives.
        assert_eq!(shares[0], "rlks_AaqqqqqqqqqqAgFDQY45JEsRFImOMrOI");
        assert_eq!(&combine(&[shares[0].as_str(), shares[2].as_str()]).unwrap()[..], b"\x42");
    }

    #[test]
    fn every_threshold_subset_recovers_the_key_and_smaller_ones_dont() {
        for n in 2..=5 {
            for k in 2..=n {
                let shares = split(KEY, n, k).unwrap();
                for pick in subsets(n) {
                    let given: Vec<&str> = pick.iter().map(|&i| shares[i].as_str()).collect();
                    match combine(&given) {
                        Ok(key) => assert!(pick.len() >= k && &key[..] == KEY, "{k} of {n}: {pick:?}"),
                        Err(e) => assert_eq!((pick.len() < k, e), (true, KeyShareError::TooFew { have: pick.len(), need: k })),
                    }
                }
            }
        }
    }

    #[test]
    fn a_damaged_share_is_refused() {
        let shares = split(KEY, 3, 2).unwrap();
        let mut bad = shares[1].clone().into_bytes();
        let at = PREFIX.len() + 20;
        bad[at] = if bad[at] == b'A' { b'B' } else { b'A' };
        let bad = String::from_utf8(bad).unwrap();
        assert_eq!(combine(&[shares[0].as_str(), &bad]).unwrap_err(), KeyShareError::Corrupt(2));
        assert_eq!(combine(&[shares[0].as_str(), &shares[1][..shares[1].len() - 4]]).unwrap_err(), KeyShareError::Corrupt(2));
        assert_eq!(combine(&[shares[0].as_str(), "rlt_notashare"]).unwrap_err(), KeyShareError::NotAShare(2));
    }

    #[test]
    fn shares_of_two_splits_are_refused() {
        let (a, b) = (split(KEY, 3, 2).unwrap(), split(KEY, 3, 2).unwrap());
        assert_eq!(combine(&[a[0].as_str(), b[1].as_str()]).unwrap_err(), KeyShareError::MixedSets(2));
    }

    #[test]
    fn a_share_given_twice_is_refused() {
        let shares = split(KEY, 3, 2).unwrap();
        assert_eq!(combine(&[shares[0].as_str(), shares[0].as_str()]).unwrap_err(), KeyShareError::Duplicate(1));
    }

    #[test]
    fn an_altered_share_with_a_good_checksum_gives_an_error_not_a_wrong_key() {
        let shares = split(KEY, 3, 2).unwrap();
        let mut altered = Share::parse(1, &shares[0]).unwrap();
        altered.y[0] ^= 1;
        assert_eq!(combine(&[altered.encode().as_str(), shares[1].as_str()]).unwrap_err(), KeyShareError::Mismatch);
    }
}
