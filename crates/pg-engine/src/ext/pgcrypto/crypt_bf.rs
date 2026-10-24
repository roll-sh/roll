//! bcrypt (`crypt-blowfish.c`, Solar Designer's implementation): `$2a$` and the
//! sign-extension-bug-compatible `$2x$`.

use super::bf_tables;
use super::px::invalid_param;
use crate::Result;

const ITOA64: &[u8; 64] = b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

const ATOI64: [u8; 0x60] = [
    64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 0, 1, 54, 55, 56, 57, 58, 59, 60, 61,
    62, 63, 64, 64, 64, 64, 64, 64, 64, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18,
    19, 20, 21, 22, 23, 24, 25, 26, 27, 64, 64, 64, 64, 64, 64, 28, 29, 30, 31, 32, 33, 34, 35, 36,
    37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 64, 64, 64, 64, 64,
];

/// "OrpheanBeholderScryDoubt", as big-endian words.
const MAGIC: [u32; 6] = [0x4F727068, 0x65616E42, 0x65686F6C, 0x64657253, 0x63727944, 0x6F756274];

fn atoi64(c: u8) -> Option<u8> {
    let i = c.checked_sub(0x20)? as usize;
    let v = *ATOI64.get(i)?;
    (v <= 63).then_some(v)
}

/// `BF_decode`: `size` bytes from the salt's base-64 characters; None on a bad character.
fn decode(src: &[u8], size: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(size);
    let mut s = src.iter().copied();
    let mut next = || s.next().and_then(atoi64);
    while out.len() < size {
        let c1 = next()?;
        let c2 = next()?;
        out.push((c1 << 2) | ((c2 & 0x30) >> 4));
        if out.len() >= size {
            break;
        }
        let c3 = next()?;
        out.push(((c2 & 0x0F) << 4) | ((c3 & 0x3C) >> 2));
        if out.len() >= size {
            break;
        }
        let c4 = next()?;
        out.push(((c3 & 0x03) << 6) | c4);
    }
    Some(out)
}

/// `BF_encode`.
pub(super) fn encode(out: &mut Vec<u8>, src: &[u8]) {
    let mut i = 0;
    while i < src.len() {
        let c1 = src[i] as usize;
        i += 1;
        out.push(ITOA64[c1 >> 2]);
        let mut c = (c1 & 0x03) << 4;
        if i >= src.len() {
            out.push(ITOA64[c]);
            break;
        }
        let c2 = src[i] as usize;
        i += 1;
        c |= c2 >> 4;
        out.push(ITOA64[c]);
        c = (c2 & 0x0f) << 2;
        if i >= src.len() {
            out.push(ITOA64[c]);
            break;
        }
        let c2 = src[i] as usize;
        i += 1;
        c |= c2 >> 6;
        out.push(ITOA64[c]);
        out.push(ITOA64[c2 & 0x3f]);
    }
}

struct Ctx {
    s: [[u32; 256]; 4],
    p: [u32; 18],
}

impl Ctx {
    #[inline]
    fn f(&self, x: u32) -> u32 {
        ((self.s[0][(x >> 24) as usize].wrapping_add(self.s[1][((x >> 16) & 0xff) as usize]))
            ^ self.s[2][((x >> 8) & 0xff) as usize])
            .wrapping_add(self.s[3][(x & 0xff) as usize])
    }

    /// `BF_ENCRYPT`.
    #[inline]
    fn encrypt(&self, mut l: u32, mut r: u32) -> (u32, u32) {
        l ^= self.p[0];
        let mut i = 0;
        while i < 16 {
            r ^= self.f(l) ^ self.p[i + 1];
            l ^= self.f(r) ^ self.p[i + 2];
            i += 2;
        }
        (r ^ self.p[17], l)
    }

    /// `BF_body`: re-encrypt the P-array and S-boxes in place.
    fn body(&mut self) {
        let (mut l, mut r) = (0u32, 0u32);
        for i in (0..18).step_by(2) {
            (l, r) = self.encrypt(l, r);
            self.p[i] = l;
            self.p[i + 1] = r;
        }
        for b in 0..4 {
            for i in (0..256).step_by(2) {
                (l, r) = self.encrypt(l, r);
                self.s[b][i] = l;
                self.s[b][i + 1] = r;
            }
        }
    }
}

/// `_crypt_blowfish_rn`.
#[allow(clippy::needless_range_loop)] // (P-array and key words indexed together, as in C)
pub(super) fn crypt(key: &[u8], setting: &[u8]) -> Result<Vec<u8>> {
    let bad = || invalid_param("invalid salt");
    if setting.len() < 29 {
        return Err(bad());
    }
    let s = setting;
    if s[0] != b'$'
        || s[1] != b'2'
        || (s[2] != b'a' && s[2] != b'x')
        || s[3] != b'$'
        || !(b'0'..=b'3').contains(&s[4])
        || !s[5].is_ascii_digit()
        || (s[4] == b'3' && s[5] > b'1')
        || s[6] != b'$'
    {
        return Err(bad());
    }
    let cost = (s[4] - b'0') as u32 * 10 + (s[5] - b'0') as u32;
    let count: u32 = 1 << cost;
    if count < 16 {
        return Err(bad());
    }
    let salt_bytes = decode(&s[7..], 16).ok_or_else(bad)?;
    let salt: [u32; 4] = std::array::from_fn(|i| u32::from_be_bytes(salt_bytes[i * 4..i * 4 + 4].try_into().unwrap()));
    // BF_set_key: the key's bytes, its terminating NUL included, cycled over 18 words.
    let bug = s[2] == b'x';
    let mut expanded = [0u32; 18];
    let mut ctx = Ctx {
        s: bf_tables::S,
        p: [0; 18],
    };
    let mut pos = 0usize;
    for i in 0..18 {
        let mut tmp: u32 = 0;
        for _ in 0..4 {
            tmp <<= 8;
            let c = key.get(pos).copied().unwrap_or(0);
            tmp |= if bug { c as i8 as i32 as u32 } else { c as u32 };
            if c == 0 {
                pos = 0;
            } else {
                pos += 1;
            }
        }
        expanded[i] = tmp;
        ctx.p[i] = bf_tables::P[i] ^ tmp;
    }
    let (mut l, mut r) = (0u32, 0u32);
    for i in (0..18).step_by(2) {
        l ^= salt[i & 2];
        r ^= salt[(i & 2) + 1];
        (l, r) = ctx.encrypt(l, r);
        ctx.p[i] = l;
        ctx.p[i + 1] = r;
    }
    for b in 0..4 {
        for i in (0..256).step_by(4) {
            l ^= salt[2];
            r ^= salt[3];
            (l, r) = ctx.encrypt(l, r);
            ctx.s[b][i] = l;
            ctx.s[b][i + 1] = r;
            l ^= salt[0];
            r ^= salt[1];
            (l, r) = ctx.encrypt(l, r);
            ctx.s[b][i + 2] = l;
            ctx.s[b][i + 3] = r;
        }
    }
    for _ in 0..count {
        for i in 0..18 {
            ctx.p[i] ^= expanded[i];
        }
        ctx.body();
        for i in 0..18 {
            ctx.p[i] ^= salt[i % 4];
        }
        ctx.body();
    }
    let mut output = [0u32; 6];
    for i in (0..6).step_by(2) {
        let (mut l, mut r) = (MAGIC[i], MAGIC[i + 1]);
        for _ in 0..64 {
            (l, r) = ctx.encrypt(l, r);
        }
        output[i] = l;
        output[i + 1] = r;
    }
    let mut out = s[..28].to_vec();
    out.push(ITOA64[(atoi64(s[28]).unwrap_or(0) & 0x30) as usize]);
    let bytes: Vec<u8> = output.iter().flat_map(|w| w.to_be_bytes()).collect();
    encode(&mut out, &bytes[..23]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn known_hashes() {
        // From the reference (crypt('foo', '$2a$06$RQiOJ.3ELirrXwxIZY8q0O')).
        assert_eq!(
            super::crypt(b"foo", b"$2a$06$RQiOJ.3ELirrXwxIZY8q0O").unwrap(),
            b"$2a$06$RQiOJ.3ELirrXwxIZY8q0OusTIuehfvOWb1sloIGcfkRVgt2LFfbi".to_vec()
        );
        // OpenBSD's test vector: "U*U" under $2a$05$CCCCCCCCCCCCCCCCCCCCC.
        assert_eq!(
            super::crypt(b"U*U", b"$2a$05$CCCCCCCCCCCCCCCCCCCCC.").unwrap(),
            b"$2a$05$CCCCCCCCCCCCCCCCCCCCC.E5YPO9kmyuRGyh0XouQYb4YMJKvyOeW".to_vec()
        );
        // The $2x$ bug: 8-bit characters sign-extended (Solar Designer's test vector).
        assert_eq!(
            super::crypt(b"\xa3", b"$2x$05$/OK.fbVrR/bpIqNJ5ianF.").unwrap(),
            b"$2x$05$/OK.fbVrR/bpIqNJ5ianF.CE5elHaaO4EbggVDjb8P19RukzXSM3e".to_vec()
        );
    }
}
