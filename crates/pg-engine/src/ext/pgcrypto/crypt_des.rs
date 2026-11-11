//! Traditional and extended (BSDi, `_`) DES crypt (`crypt-des.c`, from FreeBSD): the
//! table-driven DES with salt-perturbed E-box.

use super::px::invalid_param;
use crate::Result;
use std::sync::OnceLock;

const A64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61,
    53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];

const KEY_PERM: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60,
    52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29,
    21, 13, 5, 28, 20, 12, 4,
];

const KEY_SHIFTS: [u8; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];

const COMP_PERM: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52,
    31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];

const SBOX: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12,
        11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9, 1,
        7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1,
        10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15,
        4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14,
        12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6, 9, 8,
        7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2,
        12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1,
        13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15,
        10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
        2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13,
        14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5,
        15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5,
        12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4,
        10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6,
        11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10,
        8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

const PBOX: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19,
    13, 30, 6, 22, 11, 4, 25,
];

fn bits32(i: usize) -> u32 {
    0x8000_0000 >> i
}

const BITS8: [u8; 8] = [0x80, 0x40, 0x20, 0x10, 0x08, 0x04, 0x02, 0x01];

/// `des_init`'s tables.
struct Tables {
    m_sbox: Box<[[u8; 4096]; 4]>,
    psbox: [[u32; 256]; 4],
    ip_maskl: [[u32; 256]; 8],
    ip_maskr: [[u32; 256]; 8],
    fp_maskl: [[u32; 256]; 8],
    fp_maskr: [[u32; 256]; 8],
    key_perm_maskl: [[u32; 128]; 8],
    key_perm_maskr: [[u32; 128]; 8],
    comp_maskl: [[u32; 128]; 8],
    comp_maskr: [[u32; 128]; 8],
}

#[allow(clippy::needless_range_loop)] // (the C tables' index arithmetic, kept as written)
fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut u_sbox = [[0u8; 64]; 8];
        for i in 0..8 {
            for j in 0..64 {
                let b = (j & 0x20) | ((j & 1) << 4) | ((j >> 1) & 0xf);
                u_sbox[i][j] = SBOX[i][b];
            }
        }
        let mut m_sbox = Box::new([[0u8; 4096]; 4]);
        for b in 0..4 {
            for i in 0..64 {
                for j in 0..64 {
                    m_sbox[b][(i << 6) | j] = (u_sbox[b << 1][i] << 4) | u_sbox[(b << 1) + 1][j];
                }
            }
        }
        let mut init_perm = [0u8; 64];
        let mut final_perm = [0u8; 64];
        let mut inv_key_perm = [255u8; 64];
        for i in 0..64 {
            final_perm[i] = IP[i] - 1;
            init_perm[final_perm[i] as usize] = i as u8;
        }
        let mut inv_comp_perm = [255u8; 56];
        for (i, k) in KEY_PERM.iter().enumerate() {
            inv_key_perm[(*k - 1) as usize] = i as u8;
        }
        for (i, c) in COMP_PERM.iter().enumerate() {
            inv_comp_perm[(*c - 1) as usize] = i as u8;
        }
        // bits28 / bits24: the low 28 / 24 bits of _crypt_bits32.
        let bits28 = |i: usize| bits32(i + 4);
        let bits24 = |i: usize| bits32(i + 8);
        let mut t = Tables {
            m_sbox,
            psbox: [[0; 256]; 4],
            ip_maskl: [[0; 256]; 8],
            ip_maskr: [[0; 256]; 8],
            fp_maskl: [[0; 256]; 8],
            fp_maskr: [[0; 256]; 8],
            key_perm_maskl: [[0; 128]; 8],
            key_perm_maskr: [[0; 128]; 8],
            comp_maskl: [[0; 128]; 8],
            comp_maskr: [[0; 128]; 8],
        };
        for k in 0..8 {
            for i in 0..256 {
                for j in 0..8 {
                    let inbit = 8 * k + j;
                    if i as u8 & BITS8[j] != 0 {
                        let obit = init_perm[inbit] as usize;
                        if obit < 32 {
                            t.ip_maskl[k][i] |= bits32(obit);
                        } else {
                            t.ip_maskr[k][i] |= bits32(obit - 32);
                        }
                        let obit = final_perm[inbit] as usize;
                        if obit < 32 {
                            t.fp_maskl[k][i] |= bits32(obit);
                        } else {
                            t.fp_maskr[k][i] |= bits32(obit - 32);
                        }
                    }
                }
            }
            for i in 0..128 {
                for j in 0..7 {
                    if i as u8 & BITS8[j + 1] == 0 {
                        continue;
                    }
                    let inbit = 8 * k + j;
                    let obit = inv_key_perm[inbit];
                    if obit != 255 {
                        let obit = obit as usize;
                        if obit < 28 {
                            t.key_perm_maskl[k][i] |= bits28(obit);
                        } else {
                            t.key_perm_maskr[k][i] |= bits28(obit - 28);
                        }
                    }
                    let inbit = 7 * k + j;
                    let obit = inv_comp_perm[inbit];
                    if obit != 255 {
                        let obit = obit as usize;
                        if obit < 24 {
                            t.comp_maskl[k][i] |= bits24(obit);
                        } else {
                            t.comp_maskr[k][i] |= bits24(obit - 24);
                        }
                    }
                }
            }
        }
        let mut un_pbox = [0u8; 32];
        for (i, p) in PBOX.iter().enumerate() {
            un_pbox[(*p - 1) as usize] = i as u8;
        }
        for b in 0..4 {
            for i in 0..256 {
                for j in 0..8 {
                    if i as u8 & BITS8[j] != 0 {
                        t.psbox[b][i] |= bits32(un_pbox[8 * b + j] as usize);
                    }
                }
            }
        }
        t
    })
}

/// One key's schedule and the salt bits.
struct Des {
    keysl: [u32; 16],
    keysr: [u32; 16],
    saltbits: u32,
}

impl Des {
    /// `des_setkey`.
    #[allow(clippy::needless_range_loop)]
    fn set_key(&mut self, key: &[u8; 8]) {
        let t = tables();
        let raw0 = u32::from_be_bytes([key[0], key[1], key[2], key[3]]) as usize;
        let raw1 = u32::from_be_bytes([key[4], key[5], key[6], key[7]]) as usize;
        let perm = |m: &[[u32; 128]; 8]| {
            m[0][raw0 >> 25]
                | m[1][(raw0 >> 17) & 0x7f]
                | m[2][(raw0 >> 9) & 0x7f]
                | m[3][(raw0 >> 1) & 0x7f]
                | m[4][raw1 >> 25]
                | m[5][(raw1 >> 17) & 0x7f]
                | m[6][(raw1 >> 9) & 0x7f]
                | m[7][(raw1 >> 1) & 0x7f]
        };
        let k0 = perm(&t.key_perm_maskl);
        let k1 = perm(&t.key_perm_maskr);
        let mut shifts = 0u32;
        for round in 0..16 {
            shifts += KEY_SHIFTS[round] as u32;
            let t0 = ((k0 << shifts) | (k0 >> (28 - shifts))) as usize;
            let t1 = ((k1 << shifts) | (k1 >> (28 - shifts))) as usize;
            let comp = |m: &[[u32; 128]; 8]| {
                m[0][(t0 >> 21) & 0x7f]
                    | m[1][(t0 >> 14) & 0x7f]
                    | m[2][(t0 >> 7) & 0x7f]
                    | m[3][t0 & 0x7f]
                    | m[4][(t1 >> 21) & 0x7f]
                    | m[5][(t1 >> 14) & 0x7f]
                    | m[6][(t1 >> 7) & 0x7f]
                    | m[7][t1 & 0x7f]
            };
            self.keysl[round] = comp(&t.comp_maskl);
            self.keysr[round] = comp(&t.comp_maskr);
        }
    }

    /// `setup_salt`.
    fn set_salt(&mut self, salt: u32) {
        self.saltbits = 0;
        let mut obit = 0x80_0000u32;
        for i in 0..24 {
            if salt & (1 << i) != 0 {
                self.saltbits |= obit;
            }
            obit >>= 1;
        }
    }

    /// `do_des`, encrypting `count` times.
    fn encrypt(&self, l_in: u32, r_in: u32, count: u32) -> (u32, u32) {
        let t = tables();
        let ip = |m: &[[u32; 256]; 8]| {
            m[0][(l_in >> 24) as usize]
                | m[1][((l_in >> 16) & 0xff) as usize]
                | m[2][((l_in >> 8) & 0xff) as usize]
                | m[3][(l_in & 0xff) as usize]
                | m[4][(r_in >> 24) as usize]
                | m[5][((r_in >> 16) & 0xff) as usize]
                | m[6][((r_in >> 8) & 0xff) as usize]
                | m[7][(r_in & 0xff) as usize]
        };
        let mut l = ip(&t.ip_maskl);
        let mut r = ip(&t.ip_maskr);
        let mut f = 0u32;
        for _ in 0..count {
            for round in 0..16 {
                let mut r48l = ((r & 0x0000_0001) << 23)
                    | ((r & 0xf800_0000) >> 9)
                    | ((r & 0x1f80_0000) >> 11)
                    | ((r & 0x01f8_0000) >> 13)
                    | ((r & 0x001f_8000) >> 15);
                let mut r48r = ((r & 0x0001_f800) << 7)
                    | ((r & 0x0000_1f80) << 5)
                    | ((r & 0x0000_01f8) << 3)
                    | ((r & 0x0000_001f) << 1)
                    | ((r & 0x8000_0000) >> 31);
                f = (r48l ^ r48r) & self.saltbits;
                r48l ^= f ^ self.keysl[round];
                r48r ^= f ^ self.keysr[round];
                f = t.psbox[0][t.m_sbox[0][(r48l >> 12) as usize] as usize]
                    | t.psbox[1][t.m_sbox[1][(r48l & 0xfff) as usize] as usize]
                    | t.psbox[2][t.m_sbox[2][(r48r >> 12) as usize] as usize]
                    | t.psbox[3][t.m_sbox[3][(r48r & 0xfff) as usize] as usize];
                f ^= l;
                l = r;
                r = f;
            }
            r = l;
            l = f;
        }
        let fp = |m: &[[u32; 256]; 8]| {
            m[0][(l >> 24) as usize]
                | m[1][((l >> 16) & 0xff) as usize]
                | m[2][((l >> 8) & 0xff) as usize]
                | m[3][(l & 0xff) as usize]
                | m[4][(r >> 24) as usize]
                | m[5][((r >> 16) & 0xff) as usize]
                | m[6][((r >> 8) & 0xff) as usize]
                | m[7][(r & 0xff) as usize]
        };
        (fp(&t.fp_maskl), fp(&t.fp_maskr))
    }
}

/// `ascii_to_bin` (bytes past 'z', and non-ASCII ones, count as 0).
fn ascii_to_bin(ch: u8) -> u32 {
    match ch {
        b'a'..=b'z' => (ch - b'a' + 38) as u32,
        b'A'..=b'Z' => (ch - b'A' + 12) as u32,
        b'.'..=b'9' => (ch - b'.') as u32,
        _ => 0,
    }
}

/// `px_crypt_des`.
pub(super) fn crypt(key: &[u8], setting: &[u8]) -> Result<Option<Vec<u8>>> {
    // The key's first 8 characters, each shifted up a bit, zero-padded.
    let mut keybuf = [0u8; 8];
    let mut k = 0;
    for b in keybuf.iter_mut() {
        let c = key.get(k).copied().unwrap_or(0);
        *b = c << 1;
        if c != 0 {
            k += 1;
        }
    }
    let mut des = Des {
        keysl: [0; 16],
        keysr: [0; 16],
        saltbits: 0,
    };
    des.set_key(&keybuf);
    let mut out: Vec<u8>;
    let count;
    let salt;
    if setting.first() == Some(&b'_') {
        // Extended crypt: `_`, 4 characters of count, 4 of salt; the whole key is used.
        if setting.len() < 9 {
            return Err(invalid_param("invalid salt"));
        }
        count = (1..5).fold(0u32, |a, i| a | ascii_to_bin(setting[i]) << ((i - 1) * 6));
        salt = (5..9).fold(0u32, |a, i| a | ascii_to_bin(setting[i]) << ((i - 5) * 6));
        while k < key.len() && key[k] != 0 {
            // Encrypt the key with itself, then XOR the next 8 characters into it.
            let (l, r) = des.encrypt(
                u32::from_be_bytes(keybuf[..4].try_into().unwrap()),
                u32::from_be_bytes(keybuf[4..].try_into().unwrap()),
                1,
            );
            keybuf[..4].copy_from_slice(&l.to_be_bytes());
            keybuf[4..].copy_from_slice(&r.to_be_bytes());
            for b in keybuf.iter_mut() {
                if k >= key.len() || key[k] == 0 {
                    break;
                }
                *b ^= key[k] << 1;
                k += 1;
            }
            des.set_key(&keybuf);
        }
        out = setting[..9].to_vec();
    } else {
        // Traditional: 2 characters of salt, the key's first 8 characters.
        count = 25;
        if setting.len() < 2 {
            return Err(invalid_param("invalid salt"));
        }
        salt = (ascii_to_bin(setting[1]) << 6) | ascii_to_bin(setting[0]);
        out = vec![setting[0], setting[1]];
    }
    des.set_salt(salt);
    // do_des refuses a count of 0.
    if count == 0 {
        return Ok(None);
    }
    let (r0, r1) = des.encrypt(0, 0, count);
    let mut push = |l: u32, shifts: &[u32]| {
        for s in shifts {
            out.push(A64[((l >> s) & 0x3f) as usize]);
        }
    };
    push(r0 >> 8, &[18, 12, 6, 0]);
    push((r0 << 16) | ((r1 >> 16) & 0xffff), &[18, 12, 6, 0]);
    push(r1 << 2, &[12, 6, 0]);
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    fn c(k: &[u8], s: &[u8]) -> Option<Vec<u8>> {
        super::crypt(k, s).unwrap()
    }

    #[test]
    fn known_hashes() {
        // From crypt-des.out / crypt-xdes.out.
        assert_eq!(c(b"", b"NB").unwrap(), b"NBPx/38Y48kHg");
        assert_eq!(c(b"foox", b"NB").unwrap(), b"NB53EGGqrrb5E");
        assert_eq!(c(b"", b"_J9..j2zz").unwrap(), b"_J9..j2zzR/nIRDK3pPc");
        assert_eq!(c(b"foox", b"_J9..j2zz").unwrap(), b"_J9..j2zzAYKMvO2BYRY");
        assert_eq!(c(b"longlongpassword", b"_J9..j2zz").unwrap(), b"_J9..j2zz4BeseiQNwUg");
        assert_eq!(c(b"password", b"_/!!!!!!!").unwrap(), b"_/!!!!!!!zqM49hRzxko");
        assert_eq!(c(b"password", b"_........"), None);
        assert!(super::crypt(b"foox", b"_J9..BWH").is_err());
    }
}
