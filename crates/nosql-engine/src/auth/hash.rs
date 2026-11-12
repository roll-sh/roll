//! Password hashes as the references store them in `salted_hash`.
//!
//! - Cassandra (`CassandraRoleManager.hashpw`, jBCrypt): bcrypt `$2a$<cost>$…`, the cost from
//!   `cassandra.auth_bcrypt_gensalt_log2_rounds` (10). `HASHED PASSWORD` values must parse as a
//!   jBCrypt salt (`$2$` or `$2a$`, cost 4–30, 22 salt characters).
//! - ScyllaDB (`crypt(3)`): new hashes are SHA-512-crypt `$6$<16 salt chars>$…` with the default
//!   5000 rounds (verified: the auth-enabled reference's `system.roles` hashes reproduce with
//!   `openssl passwd -6`); logins accept bcrypt, SHA-512-crypt, SHA-256-crypt and MD5-crypt, as
//!   ScyllaDB's documentation lists.
//!
//! SHA-crypt is Ulrich Drepper's specification (`sha-crypt.txt`); MD5-crypt is Poul-Henning
//! Kamp's FreeBSD algorithm.

use nosql_types::Dialect;
use rand::Rng;

const B64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// A new hash of `password` in the dialect's format.
pub fn hash(password: &str, dialect: Dialect, bcrypt_cost: u32) -> String {
    match dialect {
        Dialect::Cassandra => bcrypt::hash_with_result(password, bcrypt_cost.clamp(4, 31))
            .map(|h| h.format_for_version(bcrypt::Version::TwoA))
            // Unreachable with a clamped cost; fall back to the reference's default.
            .unwrap_or_else(|_| hash(password, dialect, bcrypt::DEFAULT_COST)),
        Dialect::Scylla => {
            let mut rng = rand::thread_rng();
            let salt: String = (0..16).map(|_| B64[rng.gen_range(0..64)] as char).collect();
            sha_crypt(password.as_bytes(), &salt, None, Sha::S512)
        }
    }
}

/// Whether `password` matches `hash` (any malformed hash fails, as the references log and
/// reject).
pub fn verify(password: &str, hash: &str, dialect: Dialect) -> bool {
    if is_bcrypt(hash) {
        if dialect == Dialect::Cassandra && !jbcrypt_salt_ok(hash) {
            return false;
        }
        return bcrypt::verify(password, hash).unwrap_or(false);
    }
    if dialect == Dialect::Cassandra {
        return false;
    }
    let computed = if let Some(rest) = hash.strip_prefix("$6$") {
        crypt_sha(password, rest, Sha::S512)
    } else if let Some(rest) = hash.strip_prefix("$5$") {
        crypt_sha(password, rest, Sha::S256)
    } else if let Some(rest) = hash.strip_prefix("$1$") {
        let salt = rest.split('$').next().unwrap_or("");
        Some(md5_crypt(password.as_bytes(), salt))
    } else {
        None
    };
    computed.is_some_and(|c| constant_eq(c.as_bytes(), hash.as_bytes()))
}

fn is_bcrypt(h: &str) -> bool {
    ["$2a$", "$2b$", "$2y$", "$2x$"]
        .iter()
        .any(|p| h.starts_with(p))
}

/// `BCrypt.checkpw("dummy", h)` succeeds (`RoleOptions.validate`): jBCrypt parses `$2$` / `$2a$`,
/// a two-digit cost in 4..=30 and a 22-character salt.
pub fn jbcrypt_salt_ok(h: &str) -> bool {
    let b = h.as_bytes();
    if b.len() < 3 || b[0] != b'$' || b[1] != b'2' {
        return false;
    }
    let off = if b[2] == b'$' {
        3
    } else if b[2] == b'a' && b.get(3) == Some(&b'$') {
        4
    } else {
        return false;
    };
    let Some(cost) = h.get(off..off + 2).and_then(|c| c.parse::<u32>().ok()) else {
        return false;
    };
    if !(4..=30).contains(&cost) || b.get(off + 2) != Some(&b'$') {
        return false;
    }
    h.get(off + 3..off + 25)
        .is_some_and(|s| s.bytes().all(|c| B64.contains(&c)))
}

/// `rounds=N$salt$hash` or `salt$hash` after the `$5$` / `$6$` prefix.
fn crypt_sha(password: &str, rest: &str, sha: Sha) -> Option<String> {
    let (rounds, rest) = match rest.strip_prefix("rounds=") {
        Some(r) => {
            let (n, rest) = r.split_once('$')?;
            (Some(n.parse::<u64>().ok()?), rest)
        }
        None => (None, rest),
    };
    let salt = rest.split('$').next().unwrap_or("");
    Some(sha_crypt(password.as_bytes(), salt, rounds, sha))
}

#[derive(Clone, Copy, PartialEq)]
enum Sha {
    S256,
    S512,
}

impl Sha {
    fn digest(self, data: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        match self {
            Sha::S256 => sha2::Sha256::digest(data).to_vec(),
            Sha::S512 => sha2::Sha512::digest(data).to_vec(),
        }
    }
}

/// SHA-crypt (`$5$` / `$6$`).
fn sha_crypt(pw: &[u8], salt: &str, rounds: Option<u64>, sha: Sha) -> String {
    let salt = &salt.as_bytes()[..salt.len().min(16)];
    let n = rounds.map(|r| r.clamp(1000, 999_999_999)).unwrap_or(5000);
    // B = H(pw salt pw)
    let b = sha.digest(&[pw, salt, pw].concat());
    let len = b.len();
    // A = H(pw salt B* bits)
    let mut a_in = [pw, salt].concat();
    let mut left = pw.len();
    while left > len {
        a_in.extend_from_slice(&b);
        left -= len;
    }
    a_in.extend_from_slice(&b[..left]);
    let mut bits = pw.len();
    while bits > 0 {
        if bits & 1 == 1 {
            a_in.extend_from_slice(&b);
        } else {
            a_in.extend_from_slice(pw);
        }
        bits >>= 1;
    }
    let a = sha.digest(&a_in);
    let dp = sha.digest(&pw.repeat(pw.len()));
    let p: Vec<u8> = dp.iter().cycle().take(pw.len()).copied().collect();
    let ds = sha.digest(&salt.repeat(16 + a[0] as usize));
    let s: Vec<u8> = ds.iter().cycle().take(salt.len()).copied().collect();
    let mut c = a;
    let mut buf = Vec::with_capacity(2 * len + p.len() * 2 + s.len());
    for i in 0..n {
        buf.clear();
        if i % 2 == 1 {
            buf.extend_from_slice(&p);
        } else {
            buf.extend_from_slice(&c);
        }
        if i % 3 != 0 {
            buf.extend_from_slice(&s);
        }
        if i % 7 != 0 {
            buf.extend_from_slice(&p);
        }
        if i % 2 == 1 {
            buf.extend_from_slice(&c);
        } else {
            buf.extend_from_slice(&p);
        }
        c = sha.digest(&buf);
    }
    let mut out = String::from(if sha == Sha::S512 { "$6$" } else { "$5$" });
    if let Some(r) = rounds {
        out.push_str(&format!("rounds={}$", r.clamp(1000, 999_999_999)));
    }
    out.push_str(std::str::from_utf8(salt).unwrap_or(""));
    out.push('$');
    let groups: &[(usize, usize, usize)] = if sha == Sha::S512 {
        &[
            (0, 21, 42),
            (22, 43, 1),
            (44, 2, 23),
            (3, 24, 45),
            (25, 46, 4),
            (47, 5, 26),
            (6, 27, 48),
            (28, 49, 7),
            (50, 8, 29),
            (9, 30, 51),
            (31, 52, 10),
            (53, 11, 32),
            (12, 33, 54),
            (34, 55, 13),
            (56, 14, 35),
            (15, 36, 57),
            (37, 58, 16),
            (59, 17, 38),
            (18, 39, 60),
            (40, 61, 19),
            (62, 20, 41),
        ]
    } else {
        &[
            (0, 10, 20),
            (21, 1, 11),
            (12, 22, 2),
            (3, 13, 23),
            (24, 4, 14),
            (15, 25, 5),
            (6, 16, 26),
            (27, 7, 17),
            (18, 28, 8),
            (9, 19, 29),
        ]
    };
    for &(x, y, z) in groups {
        b64_24(&mut out, c[x], c[y], c[z], 4);
    }
    if sha == Sha::S512 {
        b64_24(&mut out, 0, 0, c[63], 2);
    } else {
        b64_24(&mut out, 0, c[31], c[30], 3);
    }
    out
}

fn b64_24(out: &mut String, b2: u8, b1: u8, b0: u8, n: usize) {
    let mut w = ((b2 as u32) << 16) | ((b1 as u32) << 8) | b0 as u32;
    for _ in 0..n {
        out.push(B64[(w & 0x3f) as usize] as char);
        w >>= 6;
    }
}

/// MD5-crypt (`$1$`).
fn md5_crypt(pw: &[u8], salt: &str) -> String {
    use md5::{Digest, Md5};
    let salt = &salt.as_bytes()[..salt.len().min(8)];
    let md5 = |d: &[u8]| -> Vec<u8> { Md5::digest(d).to_vec() };
    let fin = md5(&[pw, salt, pw].concat());
    let mut ctx = [pw, b"$1$", salt].concat();
    let mut pl = pw.len() as isize;
    while pl > 0 {
        ctx.extend_from_slice(&fin[..(pl.min(16)) as usize]);
        pl -= 16;
    }
    let mut i = pw.len();
    while i > 0 {
        if i & 1 == 1 {
            ctx.push(0);
        } else {
            ctx.push(pw.first().copied().unwrap_or(0));
        }
        i >>= 1;
    }
    let mut fin = md5(&ctx);
    for i in 0..1000 {
        let mut c = Vec::new();
        if i & 1 == 1 {
            c.extend_from_slice(pw);
        } else {
            c.extend_from_slice(&fin);
        }
        if i % 3 != 0 {
            c.extend_from_slice(salt);
        }
        if i % 7 != 0 {
            c.extend_from_slice(pw);
        }
        if i & 1 == 1 {
            c.extend_from_slice(&fin);
        } else {
            c.extend_from_slice(pw);
        }
        fin = md5(&c);
    }
    let mut out = format!("$1${}$", std::str::from_utf8(salt).unwrap_or(""));
    for (x, y, z) in [(0, 6, 12), (1, 7, 13), (2, 8, 14), (3, 9, 15), (4, 10, 5)] {
        b64_24(&mut out, fin[x], fin[y], fin[z], 4);
    }
    b64_24(&mut out, 0, 0, fin[11], 2);
    out
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hashes the auth-enabled ScyllaDB reference generated (`system.roles` / raft log) and the
    /// passwords they were created with.
    const SCYLLA_HASHES: &[(&str, &str)] = &[
        (
            "alice",
            "$6$vyPzWOYrRHKwqRkm$bYWoOqzmzCmJBpSOwKJabQqFPxgwfkVqUzNekIxcFda4pkFgYiR/Sktbqkzd97LsbKnRduIwr2mhUMqqyNHFP1",
        ),
        (
            "x",
            "$6$MpzV2b1r6aYv/JX8$D3gyoGK9L5JzR9ar8oWTBn5KR3EJjR1NGnnhG.o5ZAVBm/n4QuFP14aCU5CZXSJCrvXXCFju6H6/vOgYjYZBP/",
        ),
        (
            "cassandra",
            "$6$x7IFjiX5VCpvNiFk$2IfjTvSyGL7zerpV.wbY7mJjaRCrJ/68dtT3UpT.sSmNYz1bPjtn3mH.kJKFvaZ2T4SbVeBijjmwGjcb83LlV/",
        ),
    ];

    #[test]
    fn scylla_reference_hashes_verify() {
        for (pw, h) in SCYLLA_HASHES {
            assert!(verify(pw, h, Dialect::Scylla), "{pw}");
            assert!(!verify("nope", h, Dialect::Scylla));
            assert!(!verify(pw, h, Dialect::Cassandra));
        }
    }

    #[test]
    fn sha_crypt_spec_vectors() {
        // From Drepper's sha-crypt.txt.
        assert_eq!(
            sha_crypt(b"Hello world!", "saltstring", None, Sha::S512),
            "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1"
        );
        assert_eq!(
            sha_crypt(
                b"Hello world!",
                "saltstringsaltstring",
                Some(10000),
                Sha::S512
            ),
            "$6$rounds=10000$saltstringsaltst$OW1/O6BYHV6BcXZu8QVeXbDWra3Oeqh0sbHbbMCVNSnCM/UrjmM0Dp8vOuZeHBy/YTBmSK6H9qs/y3RnOaw5v."
        );
        assert_eq!(
            sha_crypt(b"Hello world!", "saltstring", None, Sha::S256),
            "$5$saltstring$5B8vYYiY.CVt1RlTTf8KbXBH3hsxY/GNooZaBBGWEc5"
        );
        // The example of ScyllaDB's documentation (empty salt).
        assert!(verify(
            "myPassword",
            "$5$$QdpObVOY40UOyo.BuWdpRq0Cr/DPkw7ckBEs2NqWVn5",
            Dialect::Scylla
        ));
    }

    #[test]
    fn md5_crypt_vector() {
        // `openssl passwd -1 -salt saltsalt password`.
        assert_eq!(
            md5_crypt(b"password", "saltsalt"),
            "$1$saltsalt$qjXMvbEw8oaL.CzflDtaK/"
        );
    }

    #[test]
    fn bcrypt_per_dialect() {
        // The Cassandra reference's `cassandra` role.
        let h = "$2a$10$9tOsOB7hv7yuTpOaO0rnZ.8PkletfBblQxxL38KQdry4bTgbUyaiS";
        assert!(verify("cassandra", h, Dialect::Cassandra));
        assert!(verify("cassandra", h, Dialect::Scylla));
        let mine = hash("pw", Dialect::Cassandra, 4);
        assert!(mine.starts_with("$2a$04$"), "{mine}");
        assert!(verify("pw", &mine, Dialect::Cassandra));
        assert!(jbcrypt_salt_ok(&mine));
        assert!(!jbcrypt_salt_ok("notahash"));
        assert!(!jbcrypt_salt_ok(
            "$2b$10$9tOsOB7hv7yuTpOaO0rnZ.8PkletfBblQxxL38KQdry4bTgbUyaiS"
        ));
        assert!(!jbcrypt_salt_ok(
            "$2a$03$9tOsOB7hv7yuTpOaO0rnZ.8PkletfBblQxxL38KQdry4bTgbUyaiS"
        ));
        assert!(jbcrypt_salt_ok("$2a$10$9tOsOB7hv7yuTpOaO0rnZ."));
        let s = hash("pw", Dialect::Scylla, 4);
        assert!(s.starts_with("$6$") && s.len() == 3 + 16 + 1 + 86, "{s}");
        assert!(verify("pw", &s, Dialect::Scylla));
        assert!(!verify("pw", "", Dialect::Scylla));
        assert!(!verify("pw", "$6$", Dialect::Scylla));
    }
}
