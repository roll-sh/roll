//! String-to-key (`pgp-s2k.c`): the session key from a password, simple, salted or iterated
//! and salted.

use super::super::digest::{Kind, Md};
use super::super::px::{PXE_PGP_BAD_S2K_MODE, PXE_PGP_CORRUPT_DATA, PXE_PGP_UNSUPPORTED_CIPHER, PXE_PGP_UNSUPPORTED_HASH, Px, PxErr};
use super::Pgp;
use super::mbuf::Reader;

/// The OpenPGP digests pgcrypto knows: (code, name).
pub(super) const DIGESTS: &[(u8, &str)] = &[
    (1, "md5"),
    (2, "sha1"),
    (2, "sha-1"),
    (3, "ripemd160"),
    (8, "sha256"),
    (9, "sha384"),
    (10, "sha512"),
];

/// `pgp_get_digest_code`.
pub(super) fn digest_code(name: &str) -> Option<u8> {
    DIGESTS.iter().find(|d| d.1.eq_ignore_ascii_case(name)).map(|d| d.0)
}

/// `pgp_load_digest`.
pub(super) fn load_digest(code: u8) -> Px<Md> {
    let Some((_, name)) = DIGESTS.iter().find(|d| d.0 == code) else {
        return Err(PxErr::Code(PXE_PGP_CORRUPT_DATA));
    };
    Md::find(name).map_err(|_| PxErr::Code(PXE_PGP_UNSUPPORTED_HASH))
}

pub(super) fn sha1() -> Md {
    Md::new(Kind::Sha1)
}

/// `s2k_decode_count`.
pub(super) fn decode_count(c: u8) -> u32 {
    (16 + (c as u32 & 15)) << ((c as u32 >> 4) + 6)
}

/// `PGP_S2K`.
#[derive(Clone, Default)]
pub(super) struct S2k {
    pub mode: u8,
    pub digest_algo: u8,
    pub salt: [u8; 8],
    pub iter: u8,
    pub key: Vec<u8>,
}

impl S2k {
    /// `pgp_s2k_fill`: the mode, digest and random salt (and iteration count) to encrypt
    /// with.
    pub fn fill(mode: i32, digest_algo: i32, count: i32, random: &mut dyn FnMut(&mut [u8])) -> Px<S2k> {
        let mut s = S2k {
            mode: mode as u8,
            digest_algo: digest_algo as u8,
            ..Default::default()
        };
        match mode {
            0 => {}
            1 => random(&mut s.salt),
            3 => {
                random(&mut s.salt);
                let mut b = [0u8; 1];
                random(&mut b);
                s.iter = decide_iter(b[0], count);
            }
            _ => return Err(PxErr::Code(PXE_PGP_BAD_S2K_MODE)),
        }
        Ok(s)
    }

    /// `pgp_s2k_read`.
    pub fn read(r: &mut Reader, level: usize, ctx: &mut Pgp) -> Px<S2k> {
        let mut s = S2k {
            mode: r.byte(level, ctx)?,
            digest_algo: r.byte(level, ctx)?,
            ..Default::default()
        };
        match s.mode {
            0 => {}
            1 => s.salt.copy_from_slice(&r.read_fixed(level, 8, ctx)?),
            3 => {
                s.salt.copy_from_slice(&r.read_fixed(level, 8, ctx)?);
                s.iter = r.byte(level, ctx)?;
            }
            _ => return Err(PxErr::Code(PXE_PGP_BAD_S2K_MODE)),
        }
        Ok(s)
    }

    /// `pgp_s2k_process`: the key for `cipher` from `key`.
    pub fn process(&mut self, cipher: i32, key: &[u8]) -> Px<()> {
        let key_len = super::cfb::key_size(cipher);
        if key_len == 0 {
            return Err(PxErr::Code(PXE_PGP_UNSUPPORTED_CIPHER));
        }
        let mut md = load_digest(self.digest_algo)?;
        let count = match self.mode {
            0 | 1 => 0,
            3 => decode_count(self.iter) as usize,
            _ => return Err(PxErr::Code(PXE_PGP_BAD_S2K_MODE)),
        };
        let mut out = Vec::with_capacity(key_len);
        let mut preload = 0;
        while out.len() < key_len {
            md.reset();
            md.update(&vec![0u8; preload]);
            preload += 1;
            if self.mode != 0 {
                md.update(&self.salt);
            }
            md.update(key);
            if self.mode == 3 {
                // calc_s2k_iter_salted: salt and key repeated up to `count` bytes in all.
                let mut cur = 8 + key.len();
                while cur < count {
                    let c = if cur + 8 < count { 8 } else { count - cur };
                    md.update(&self.salt[..c]);
                    cur += c;
                    let c = if cur + key.len() < count {
                        key.len()
                    } else if cur < count {
                        count - cur
                    } else {
                        break;
                    };
                    md.update(&key[..c]);
                    cur += c;
                }
            }
            let h = md.finish();
            let n = (key_len - out.len()).min(h.len());
            out.extend_from_slice(&h[..n]);
        }
        self.key = out;
        Ok(())
    }
}

/// `decide_s2k_iter`.
fn decide_iter(rand_byte: u8, count: i32) -> u8 {
    if count == -1 {
        return 96 + (rand_byte & 0x1F);
    }
    (0..=255u8).find(|&i| decode_count(i) as i64 >= count as i64).unwrap_or(255)
}
