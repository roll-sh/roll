//! The option string of the pgp_* functions (`parse_args` / `set_arg` of `pgp-pgsql.c`),
//! with the undocumented debug and expect-* options.

use super::super::px::{PXE_ARGUMENT_ERROR, PXE_PGP_UNSUPPORTED_CIPHER, PXE_PGP_UNSUPPORTED_HASH, Px, PxErr};
use super::{Pgp, cfb, s2k};

/// `struct debug_expect`: -1 where nothing is expected.
pub(super) struct Expect {
    pub debug: bool,
    pub expect: bool,
    cipher_algo: i32,
    s2k_mode: i32,
    s2k_count: i32,
    s2k_cipher_algo: i32,
    s2k_digest_algo: i32,
    compress_algo: i32,
    use_sess_key: i32,
    disable_mdc: i32,
    unicode_mode: i32,
}

impl Default for Expect {
    fn default() -> Expect {
        Expect {
            debug: false,
            expect: false,
            cipher_algo: -1,
            s2k_mode: -1,
            s2k_count: -1,
            s2k_cipher_algo: -1,
            s2k_digest_algo: -1,
            compress_algo: -1,
            use_sess_key: -1,
            disable_mdc: -1,
            unicode_mode: -1,
        }
    }
}

/// C's `atoi`.
fn atoi(s: &str) -> i32 {
    let s = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut v: i64 = 0;
    for c in digits.bytes().take_while(u8::is_ascii_digit) {
        v = (v * 10 + (c - b'0') as i64).min(i64::from(u32::MAX) + 1);
    }
    (if neg { -v } else { v }) as i32
}

fn cipher(name: &str) -> i32 {
    cfb::cipher_code(name).map_or(PXE_PGP_UNSUPPORTED_CIPHER, i32::from)
}

fn digest(name: &str) -> i32 {
    s2k::digest_code(name).map_or(PXE_PGP_UNSUPPORTED_HASH, i32::from)
}

fn ok_if(cond: bool) -> Px<()> {
    if cond { Ok(()) } else { Err(PxErr::Code(PXE_ARGUMENT_ERROR)) }
}

/// `set_arg`.
fn set_arg(ctx: &mut Pgp, key: &str, val: &str, ex: &mut Expect) -> Px<()> {
    let neg = |c: i32| if c < 0 { Err(PxErr::Code(c)) } else { Ok(c) };
    match key {
        "cipher-algo" => ctx.cipher_algo = neg(cipher(val))?,
        "disable-mdc" => ctx.disable_mdc = atoi(val) != 0,
        "sess-key" => ctx.use_sess_key = atoi(val) != 0,
        "s2k-mode" => {
            let m = atoi(val);
            ok_if(matches!(m, 0 | 1 | 3))?;
            ctx.s2k_mode = m;
        }
        "s2k-count" => {
            let c = atoi(val);
            ok_if(ctx.s2k_mode == 3 && (1024..=65_011_712).contains(&c))?;
            ctx.s2k_count = c;
        }
        "s2k-digest-algo" => ctx.s2k_digest_algo = neg(digest(val))?,
        "s2k-cipher-algo" => ctx.s2k_cipher_algo = neg(cipher(val))?,
        "compress-algo" => {
            let a = atoi(val);
            ok_if((0..=3).contains(&a))?;
            ctx.compress_algo = a;
        }
        "compress-level" => {
            let l = atoi(val);
            ok_if((0..=9).contains(&l))?;
            ctx.compress_level = l;
        }
        "convert-crlf" => ctx.convert_crlf = atoi(val) != 0,
        "unicode-mode" => ctx.unicode_mode = atoi(val) != 0,
        "ignore-cipher-failure" => ctx.ignore_cipher_failure = atoi(val) != 0,
        "debug" => ex.debug = atoi(val) != 0,
        _ => {
            let Some(what) = key.strip_prefix("expect-") else {
                return Err(PxErr::Code(PXE_ARGUMENT_ERROR));
            };
            let slot = match what {
                "cipher-algo" => (&mut ex.cipher_algo, cipher(val)),
                "disable-mdc" => (&mut ex.disable_mdc, atoi(val)),
                "sess-key" => (&mut ex.use_sess_key, atoi(val)),
                "s2k-mode" => (&mut ex.s2k_mode, atoi(val)),
                "s2k-count" => (&mut ex.s2k_count, atoi(val)),
                "s2k-digest-algo" => (&mut ex.s2k_digest_algo, digest(val)),
                "s2k-cipher-algo" => (&mut ex.s2k_cipher_algo, cipher(val)),
                "compress-algo" => (&mut ex.compress_algo, atoi(val)),
                "unicode-mode" => (&mut ex.unicode_mode, atoi(val)),
                _ => return Err(PxErr::Code(PXE_ARGUMENT_ERROR)),
            };
            *slot.0 = slot.1;
            ex.expect = true;
        }
    }
    Ok(())
}

/// `getword`: the next word (`=` and `,` are words of their own), after whitespace.
fn getword(s: &[u8], mut p: usize) -> (usize, usize, usize) {
    let ws = |c: u8| c == b' ' || c == b'\t' || c == b'\n';
    while p < s.len() && ws(s[p]) {
        p += 1;
    }
    let start = p;
    if p < s.len() && (s[p] == b'=' || s[p] == b',') {
        p += 1;
    } else {
        while p < s.len() && !(ws(s[p]) || s[p] == b'=' || s[p] == b',') {
            p += 1;
        }
    }
    let end = p;
    while p < s.len() && ws(s[p]) {
        p += 1;
    }
    (start, end, p)
}

/// `parse_args`.
pub(super) fn parse_args(ctx: &mut Pgp, args: &[u8], ex: &mut Expect) -> Px<()> {
    let s: Vec<u8> = args.iter().map(u8::to_ascii_lowercase).collect();
    let bad = Err(PxErr::Code(PXE_ARGUMENT_ERROR));
    let mut p = 0;
    while p < s.len() {
        let (ks, ke, np) = getword(&s, p);
        if s.get(np) != Some(&b'=') {
            return bad;
        }
        let (vs, ve, np) = getword(&s, np + 1);
        p = np;
        if p < s.len() {
            if s[p] != b',' {
                return bad;
            }
            p += 1;
        }
        if ks >= s.len() || vs >= s.len() || ve == vs {
            return bad;
        }
        let key = String::from_utf8_lossy(&s[ks..ke]).into_owned();
        let val = String::from_utf8_lossy(&s[vs..ve]).into_owned();
        set_arg(ctx, &key, &val, ex)?;
    }
    Ok(())
}

/// `check_expect`: the NOTICEs of what decryption found otherwise.
pub(super) fn check_expect(ctx: &Pgp, ex: &Expect) -> Vec<String> {
    let mut out = Vec::new();
    let mut chk = |name: &str, want: i32, got: i32| {
        if want >= 0 && want != got {
            out.push(format!("pgp_decrypt: unexpected {name}: expected {want} got {got}"));
        }
    };
    chk("cipher_algo", ex.cipher_algo, ctx.cipher_algo);
    chk("s2k_mode", ex.s2k_mode, ctx.s2k_mode);
    chk("s2k_count", ex.s2k_count, ctx.s2k_count);
    chk("s2k_digest_algo", ex.s2k_digest_algo, ctx.s2k_digest_algo);
    chk("use_sess_key", ex.use_sess_key, ctx.use_sess_key as i32);
    if ctx.use_sess_key {
        chk("s2k_cipher_algo", ex.s2k_cipher_algo, ctx.s2k_cipher_algo);
    }
    chk("disable_mdc", ex.disable_mdc, ctx.disable_mdc as i32);
    chk("compress_algo", ex.compress_algo, ctx.compress_algo);
    chk("unicode_mode", ex.unicode_mode, ctx.unicode_mode as i32);
    out
}
