//! crypt() and gen_salt() (`px-crypt.c`): the dispatch on the salt's prefix, MD5 crypt
//! (`crypt-md5.c`), SHA-256 / SHA-512 crypt (`crypt-sha.c`) and the salt generators
//! (`crypt-gensalt.c`).

use super::digest::{Kind, Md};
use super::px::{PXE_BAD_SALT_ROUNDS, PXE_UNKNOWN_SALT_ALGO, invalid_param};
use super::{crypt_bf, crypt_des};
use crate::error::code;
use crate::{Error, Result};

const ITOA64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// `px_crypt`: None where crypt(3) would return NULL. `notice` gets the NOTICEs raised.
pub(super) fn px_crypt(psw: &[u8], salt: &[u8], notice: &mut dyn FnMut(Error)) -> Result<Option<Vec<u8>>> {
    if salt.starts_with(b"$2a$") || salt.starts_with(b"$2x$") {
        return crypt_bf::crypt(psw, salt).map(Some);
    }
    if salt.starts_with(b"$2$") {
        return Ok(None);
    }
    if salt.starts_with(b"$1$") {
        return Ok(Some(md5_crypt(psw, salt)));
    }
    if salt.starts_with(b"$5$") || salt.starts_with(b"$6$") {
        return sha_crypt(psw, salt, notice).map(Some);
    }
    // Extended (`_`) and traditional DES; PX_MAX_CRYPT bounds the result.
    Ok(crypt_des::crypt(psw, salt)?.filter(|r| r.len() < 128))
}

fn to64(out: &mut Vec<u8>, mut v: u32, n: usize) {
    for _ in 0..n {
        out.push(ITOA64[(v & 0x3f) as usize]);
        v >>= 6;
    }
}

/// `px_crypt_md5`.
fn md5_crypt(pw: &[u8], salt: &[u8]) -> Vec<u8> {
    const MAGIC: &[u8] = b"$1$";
    let sp = salt.strip_prefix(MAGIC).unwrap_or(salt);
    // It stops at the first '$', 8 characters at most.
    let sl = sp.iter().take(8).take_while(|&&c| c != b'$').count();
    let sp = &sp[..sl];
    let mut ctx = Md::new(Kind::Md5);
    let mut ctx1 = Md::new(Kind::Md5);
    ctx.update(pw);
    ctx.update(MAGIC);
    ctx.update(sp);
    ctx1.update(pw);
    ctx1.update(sp);
    ctx1.update(pw);
    let mut fin = ctx1.finish();
    let mut pl = pw.len() as isize;
    while pl > 0 {
        ctx.update(&fin[..(pl as usize).min(16)]);
        pl -= 16;
    }
    fin.iter_mut().for_each(|b| *b = 0);
    let mut i = pw.len();
    while i != 0 {
        if i & 1 != 0 {
            ctx.update(&fin[..1]);
        } else {
            ctx.update(&pw[..1]);
        }
        i >>= 1;
    }
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(sp);
    out.push(b'$');
    let mut fin = ctx.finish();
    for i in 0..1000 {
        ctx1.reset();
        if i & 1 != 0 {
            ctx1.update(pw);
        } else {
            ctx1.update(&fin);
        }
        if i % 3 != 0 {
            ctx1.update(sp);
        }
        if i % 7 != 0 {
            ctx1.update(pw);
        }
        if i & 1 != 0 {
            ctx1.update(&fin);
        } else {
            ctx1.update(pw);
        }
        fin = ctx1.finish();
    }
    let f = |i: usize| fin[i] as u32;
    to64(&mut out, (f(0) << 16) | (f(6) << 8) | f(12), 4);
    to64(&mut out, (f(1) << 16) | (f(7) << 8) | f(13), 4);
    to64(&mut out, (f(2) << 16) | (f(8) << 8) | f(14), 4);
    to64(&mut out, (f(3) << 16) | (f(9) << 8) | f(15), 4);
    to64(&mut out, (f(4) << 16) | (f(10) << 8) | f(5), 4);
    to64(&mut out, f(11), 2);
    out
}

const SHA_ROUNDS_DEFAULT: u32 = 5000;
const SHA_ROUNDS_MIN: i32 = 1000;
const SHA_ROUNDS_MAX: i32 = 999_999_999;
const SHA_SALT_MAX: usize = 16;

/// `strtoint` over the bytes at `s`: the value (strtol's, truncated to int) and how many
/// bytes it took (0 without digits).
fn strtoint(s: &[u8]) -> (i32, usize) {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let neg = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let start = i;
    let mut v: i64 = 0;
    let mut over = false;
    while i < s.len() && s[i].is_ascii_digit() {
        match v.checked_mul(10).and_then(|x| x.checked_add((s[i] - b'0') as i64)) {
            Some(x) => v = x,
            None => over = true,
        }
        i += 1;
    }
    if i == start {
        return (0, 0);
    }
    let v = if over {
        if neg { i64::MIN } else { i64::MAX }
    } else if neg {
        -v
    } else {
        v
    };
    (v as i32, i)
}

/// `px_crypt_shacrypt`.
fn sha_crypt(pw: &[u8], salt: &[u8], notice: &mut dyn FnMut(Error)) -> Result<Vec<u8>> {
    const ROUNDS: &[u8] = b"rounds=";
    let internal = |m: &str| Error::new(code::INTERNAL_ERROR, m.to_string());
    if salt.len() < 3 {
        return Err(invalid_param("invalid salt"));
    }
    if salt[0] != b'$' || salt[2] != b'$' {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "invalid format of salt")
            .hint("magic byte format for shacrypt is either \"$5$\" or \"$6$\""));
    }
    let sha512 = match &salt[..3] {
        b"$5$" => false,
        b"$6$" => true,
        _ => return Err(internal(&format!("unknown crypt identifier \"{}\"", salt[1] as char))),
    };
    let mut dec = &salt[3..];
    let mut rounds = SHA_ROUNDS_DEFAULT;
    let mut custom = false;
    if dec.starts_with(ROUNDS) {
        let num = &dec[ROUNDS.len()..];
        let (mut srounds, used) = strtoint(num);
        if num.get(used) != Some(&b'$') {
            return Err(Error::new(code::SYNTAX_ERROR, "could not parse salt options"));
        }
        dec = &num[used + 1..];
        if srounds > SHA_ROUNDS_MAX {
            notice(notice_range(format!(
                "rounds={srounds} exceeds maximum supported value ({SHA_ROUNDS_MAX}), using {SHA_ROUNDS_MAX} instead"
            )));
            srounds = SHA_ROUNDS_MAX;
        } else if srounds < SHA_ROUNDS_MIN {
            notice(notice_range(format!(
                "rounds={srounds} is below supported value ({SHA_ROUNDS_MIN}), using {SHA_ROUNDS_MIN} instead"
            )));
            srounds = SHA_ROUNDS_MIN;
        }
        rounds = srounds as u32;
        custom = true;
    }
    let (kind, bs) = if sha512 { (Kind::Sha512, 64) } else { (Kind::Sha256, 32) };
    let mut out = salt[..3].to_vec();
    if custom {
        out.extend_from_slice(format!("rounds={rounds}$").as_bytes());
    }
    // The salt: the characters before the next '$' (after any leading ones), 16 at most.
    let contains = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
    let mut decoded: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < dec.len() && i < SHA_SALT_MAX {
        if contains(dec, b"$5$") || contains(dec, b"$6$") {
            return Err(internal("bogus magic byte found in salt string"));
        }
        if contains(dec, ROUNDS) {
            return Err(internal("invalid rounds option specified in salt string"));
        }
        let c = dec[i];
        if c != b'$' {
            if ITOA64.contains(&c) {
                decoded.push(c);
            } else {
                let l = crate::types::mbconv::utf_mblen(c).max(1).min(dec.len() - i);
                return Err(invalid_param(format!(
                    "invalid character in salt string: \"{}\"",
                    String::from_utf8_lossy(&dec[i..i + l])
                )));
            }
        } else if !decoded.is_empty() {
            break;
        }
        i += 1;
    }
    let salt_len = decoded.len();
    out.extend_from_slice(&decoded);
    if out.len() > 3 + 17 * usize::from(custom) + salt_len {
        return Err(internal("unexpected length of salt string"));
    }
    // (Digests B and DS hash the salt string's first salt_len bytes as they are, as
    // PostgreSQL does.)
    let raw_salt = &dec[..salt_len.min(dec.len())];
    let len = pw.len();
    let mut a = Md::new(kind);
    let mut b = Md::new(kind);
    a.update(pw);
    a.update(&decoded);
    b.update(pw);
    b.update(raw_salt);
    b.update(pw);
    let mut buf = b.finish();
    let mut block = len;
    while block > bs {
        a.update(&buf[..bs]);
        block -= bs;
    }
    a.update(&buf[..block]);
    let mut block = len;
    while block != 0 {
        if block & 1 != 0 {
            a.update(&buf[..bs]);
        } else {
            a.update(pw);
        }
        block >>= 1;
    }
    buf = a.finish();
    b.reset();
    for _ in 0..len {
        b.update(pw);
    }
    let dp = b.finish();
    let p_bytes: Vec<u8> = (0..len).map(|i| dp[i % bs]).collect();
    b.reset();
    for _ in 0..16 + buf[0] as usize {
        b.update(raw_salt);
    }
    let ds = b.finish();
    let s_bytes: Vec<u8> = (0..salt_len).map(|i| ds[i % bs]).collect();
    for r in 0..rounds {
        b.reset();
        if r & 1 != 0 {
            b.update(&p_bytes);
        } else {
            b.update(&buf[..bs]);
        }
        if r % 3 != 0 {
            b.update(&s_bytes);
        }
        if r % 7 != 0 {
            b.update(&p_bytes);
        }
        if r & 1 != 0 {
            b.update(&buf[..bs]);
        } else {
            b.update(&p_bytes);
        }
        buf = b.finish();
    }
    out.push(b'$');
    let g = |i: usize| buf[i] as u32;
    let mut b64 = |b2: u32, b1: u32, b0: u32, n: usize| to64(&mut out, (b2 << 16) | (b1 << 8) | b0, n);
    if sha512 {
        let order: [(usize, usize, usize); 21] = [
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
        ];
        for (x, y, z) in order {
            b64(g(x), g(y), g(z), 4);
        }
        b64(0, 0, g(63), 2);
    } else {
        let order: [(usize, usize, usize); 10] = [
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
        ];
        for (x, y, z) in order {
            b64(g(x), g(y), g(z), 4);
        }
        b64(0, g(31), g(30), 3);
    }
    Ok(out)
}

fn notice_range(msg: String) -> Error {
    let mut e = Error::new(code::NUMERIC_VALUE_OUT_OF_RANGE, msg);
    e.severity = crate::error::Severity::Notice;
    e
}

/// `BF_encode` of the generator (crypt-gensalt.c's copy).
fn bf_encode(out: &mut Vec<u8>, src: &[u8]) {
    crypt_bf::encode(out, src)
}

/// `px_gen_salt`: the salt, or a PXE code. `random` fills the generator's input.
pub(super) fn gen_salt(salt_type: &[u8], rounds: i32, random: &mut dyn FnMut(&mut [u8])) -> std::result::Result<Vec<u8>, i32> {
    // name, input length, default rounds, minimum, maximum
    const GENS: &[(&str, usize, i32, i32, i32)] = &[
        ("des", 2, 0, 0, 0),
        ("md5", 6, 0, 0, 0),
        ("xdes", 3, 29 * 25, 1, 0xFFFFFF),
        ("bf", 16, 6, 4, 31),
        ("sha256crypt", SHA_SALT_MAX, SHA_ROUNDS_DEFAULT as i32, SHA_ROUNDS_MIN, SHA_ROUNDS_MAX),
        ("sha512crypt", SHA_SALT_MAX, SHA_ROUNDS_DEFAULT as i32, SHA_ROUNDS_MIN, SHA_ROUNDS_MAX),
    ];
    let Some(&(name, input_len, def, min, max)) =
        GENS.iter().find(|g| g.0.as_bytes().eq_ignore_ascii_case(salt_type))
    else {
        return Err(PXE_UNKNOWN_SALT_ALGO);
    };
    let mut rounds = rounds;
    if def != 0 {
        if rounds == 0 {
            rounds = def;
        }
        if rounds < min || rounds > max {
            return Err(PXE_BAD_SALT_ROUNDS);
        }
    }
    let mut input = vec![0u8; input_len];
    random(&mut input);
    // (The generators take the count as an unsigned long.)
    let count = rounds as i64 as u64;
    let a = |v: u32| ITOA64[(v & 0x3f) as usize];
    let three = |b: &[u8]| (b[0] as u32) | ((b[1] as u32) << 8) | ((b[2] as u32) << 16);
    let mut out = Vec::new();
    match name {
        "des" => {
            if count != 0 && count != 25 {
                return Err(PXE_BAD_SALT_ROUNDS);
            }
            out.push(a(input[0] as u32));
            out.push(a(input[1] as u32));
        }
        "xdes" => {
            if count > 0xffffff || count & 1 == 0 {
                return Err(PXE_BAD_SALT_ROUNDS);
            }
            let c = count as u32;
            out.extend([b'_', a(c), a(c >> 6), a(c >> 12), a(c >> 18)]);
            let v = three(&input);
            out.extend([a(v), a(v >> 6), a(v >> 12), a(v >> 18)]);
        }
        "md5" => {
            if count != 0 && count != 1000 {
                return Err(PXE_BAD_SALT_ROUNDS);
            }
            out.extend_from_slice(b"$1$");
            for chunk in input.chunks(3) {
                let v = three(chunk);
                out.extend([a(v), a(v >> 6), a(v >> 12), a(v >> 18)]);
            }
        }
        "bf" => {
            out.extend_from_slice(format!("$2a${count:02}$").as_bytes());
            bf_encode(&mut out, &input);
        }
        _ => {
            out.extend_from_slice(if name == "sha256crypt" { b"$5$" } else { b"$6$" });
            out.extend_from_slice(format!("rounds={count}$").as_bytes());
            out.extend(input.iter().map(|&b| ITOA64[(b & 0x3f) as usize]));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(p: &str, s: &str) -> String {
        String::from_utf8(px_crypt(p.as_bytes(), s.as_bytes(), &mut |_| {}).unwrap().unwrap()).unwrap()
    }

    #[test]
    fn md5_crypt_vectors() {
        // crypt-md5.out.
        assert_eq!(c("", "$1$Szzz0yzz"), "$1$Szzz0yzz$To38XrR3BsbXQW2ZpfKjF1");
        assert_eq!(c("foox", "$1$Szzz0yzz"), "$1$Szzz0yzz$IYL49cd3t9bllsA7Jmz1M1");
    }

    #[test]
    fn sha_crypt_vectors() {
        // Drepper's specification's examples.
        assert_eq!(c("Hello world!", "$5$saltstring"), "$5$saltstring$5B8vYYiY.CVt1RlTTf8KbXBH3hsxY/GNooZaBBGWEc5");
        assert_eq!(
            c("Hello world!", "$5$rounds=10000$saltstringsaltstring"),
            "$5$rounds=10000$saltstringsaltst$3xv.VbSHBb41AL9AvLeujZkZRBAwqFMz2.opqey6IcA"
        );
        assert_eq!(
            c("Hello world!", "$6$saltstring"),
            "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1"
        );
        // crypt-shacrypt.out.
        assert_eq!(c("", "$5$Szzz0yzz"), "$5$Szzz0yzz$cA.ZFZKqblRYjdsbrWtVTYa/qSwPQnt2uh0LBtyYAAD");
        assert_eq!(
            c("foox", "$6$Szzz0yzz"),
            "$6$Szzz0yzz$KqDw1Y8kze.VFapkvTc9Y5fbqzltjeRz1aPGC/pkHRhFQZ2aM6PmZpXQjcD7AOH88Bq0CSD.VlmymQzcBMEUl0"
        );
        let mut notices = Vec::new();
        let r = px_crypt(b"the minimum number is still observed", b"$5$rounds=10$roundstoolow", &mut |e| {
            notices.push(e.message.clone())
        });
        assert_eq!(
            r.unwrap().unwrap(),
            b"$5$rounds=1000$roundstoolow$yfvwcWrQ8l/K0DAWyuPMDNHpIVlTQebY9l/gL972bIC"
        );
        assert_eq!(notices, vec!["rounds=10 is below supported value (1000), using 1000 instead"]);
    }

    #[test]
    fn gen_salt_shapes() {
        let mut r = |b: &mut [u8]| b.iter_mut().for_each(|x| *x = 0);
        assert_eq!(gen_salt(b"des", 0, &mut r).unwrap(), b"..");
        assert_eq!(gen_salt(b"xdes", 0, &mut r).unwrap(), b"_J9......");
        assert_eq!(gen_salt(b"md5", 0, &mut r).unwrap(), b"$1$........");
        assert_eq!(gen_salt(b"bf", 0, &mut r).unwrap(), b"$2a$06$......................");
        assert_eq!(gen_salt(b"sha256crypt", 0, &mut r).unwrap(), b"$5$rounds=5000$................");
        assert_eq!(gen_salt(b"bf", 3, &mut r), Err(PXE_BAD_SALT_ROUNDS));
        assert_eq!(gen_salt(b"xdes", 2, &mut r), Err(PXE_BAD_SALT_ROUNDS));
        assert_eq!(gen_salt(b"foo", 0, &mut r), Err(PXE_UNKNOWN_SALT_ALGO));
    }
}
