//! Block ciphers (`openssl.c`'s `px_find_cipher` and EVP use) and the cipher + padding
//! "combos" of encrypt() / decrypt() (`px.c`).
//!
//! As in the reference (OpenSSL 3 without the legacy provider), Blowfish, DES and CAST5 are
//! found and take their keys, but every encryption with them fails ("Cipher cannot be
//! initialized"); AES and triple DES work.

use super::px::{
    PXE_BAD_FORMAT, PXE_BAD_OPTION, PXE_CIPHER_INIT, PXE_DECRYPT_FAILED, PXE_ENCRYPT_FAILED,
    PXE_KEY_TOO_BIG, PXE_NO_CIPHER, Px, PxErr,
};
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Algo {
    Aes,
    Des3,
    /// The legacy provider's ciphers, which can't be initialized.
    Bf,
    Des,
    Cast5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Ecb,
    Cbc,
    Cfb,
}

const ALIASES: &[(&str, &str)] = &[
    ("bf", "bf-cbc"),
    ("blowfish", "bf-cbc"),
    ("blowfish-cbc", "bf-cbc"),
    ("blowfish-ecb", "bf-ecb"),
    ("blowfish-cfb", "bf-cfb"),
    ("des", "des-cbc"),
    ("3des", "des3-cbc"),
    ("3des-ecb", "des3-ecb"),
    ("3des-cbc", "des3-cbc"),
    ("cast5", "cast5-cbc"),
    ("aes", "aes-cbc"),
    ("rijndael", "aes-cbc"),
    ("rijndael-cbc", "aes-cbc"),
    ("rijndael-ecb", "aes-ecb"),
    ("rijndael-cfb", "aes-cfb"),
];

const TYPES: &[(&str, Algo, Mode)] = &[
    ("bf-cbc", Algo::Bf, Mode::Cbc),
    ("bf-ecb", Algo::Bf, Mode::Ecb),
    ("bf-cfb", Algo::Bf, Mode::Cfb),
    ("des-ecb", Algo::Des, Mode::Ecb),
    ("des-cbc", Algo::Des, Mode::Cbc),
    ("des3-ecb", Algo::Des3, Mode::Ecb),
    ("des3-cbc", Algo::Des3, Mode::Cbc),
    ("cast5-ecb", Algo::Cast5, Mode::Ecb),
    ("cast5-cbc", Algo::Cast5, Mode::Cbc),
    ("aes-ecb", Algo::Aes, Mode::Ecb),
    ("aes-cbc", Algo::Aes, Mode::Cbc),
    ("aes-cfb", Algo::Aes, Mode::Cfb),
];

enum Imp {
    Aes128(aes::Aes128),
    Aes192(aes::Aes192),
    Aes256(aes::Aes256),
    Tdes(des::TdesEde3),
}

impl Imp {
    fn encrypt_block(&self, b: &mut [u8]) {
        match self {
            Imp::Aes128(c) => c.encrypt_block(GenericArray::from_mut_slice(b)),
            Imp::Aes192(c) => c.encrypt_block(GenericArray::from_mut_slice(b)),
            Imp::Aes256(c) => c.encrypt_block(GenericArray::from_mut_slice(b)),
            Imp::Tdes(c) => c.encrypt_block(GenericArray::from_mut_slice(b)),
        }
    }

    fn decrypt_block(&self, b: &mut [u8]) {
        match self {
            Imp::Aes128(c) => c.decrypt_block(GenericArray::from_mut_slice(b)),
            Imp::Aes192(c) => c.decrypt_block(GenericArray::from_mut_slice(b)),
            Imp::Aes256(c) => c.decrypt_block(GenericArray::from_mut_slice(b)),
            Imp::Tdes(c) => c.decrypt_block(GenericArray::from_mut_slice(b)),
        }
    }
}

/// A cipher (`PX_Cipher` over an `OSSLCipher`).
pub(super) struct Cipher {
    pub algo: Algo,
    pub mode: Mode,
    key: Vec<u8>,
    /// The chaining value (CBC) or feedback register (CFB).
    iv: Vec<u8>,
    /// CFB: how much of the current keystream block is used.
    cfb_pos: usize,
    imp: Option<Imp>,
}

impl Cipher {
    /// `px_find_cipher` by the (lowercased) name.
    pub fn find(name: &str) -> Px<Cipher> {
        let name = ALIASES
            .iter()
            .find(|(a, _)| a.eq_ignore_ascii_case(name))
            .map_or(name, |(_, n)| n);
        let (_, algo, mode) = TYPES
            .iter()
            .find(|(n, _, _)| *n == name)
            .ok_or(PxErr::Code(PXE_NO_CIPHER))?;
        Ok(Cipher {
            algo: *algo,
            mode: *mode,
            key: Vec::new(),
            iv: Vec::new(),
            cfb_pos: 0,
            imp: None,
        })
    }

    pub fn block_size(&self) -> usize {
        match self.algo {
            Algo::Aes => 16,
            _ => 8,
        }
    }

    /// The largest key it takes.
    pub fn key_size(&self) -> usize {
        match self.algo {
            Algo::Aes => 32,
            Algo::Des3 => 24,
            Algo::Bf => 56,
            Algo::Des => 8,
            Algo::Cast5 => 16,
        }
    }

    pub fn iv_size(&self) -> usize {
        self.block_size()
    }

    /// `c->init`: take the key (as the algorithm sizes it) and the IV (zeros if none).
    pub fn init(&mut self, key: &[u8], iv: Option<&[u8]>) -> Px<()> {
        let bs = self.block_size();
        let klen = key.len();
        self.key = match self.algo {
            Algo::Aes => {
                let size = match klen {
                    0..=16 => 16,
                    17..=24 => 24,
                    25..=32 => 32,
                    _ => return Err(PxErr::Code(PXE_KEY_TOO_BIG)),
                };
                let mut k = vec![0u8; size];
                k[..klen].copy_from_slice(key);
                k
            }
            Algo::Des3 | Algo::Des => {
                let size = if self.algo == Algo::Des3 { 24 } else { 8 };
                let mut k = vec![0u8; size];
                let n = klen.min(size);
                k[..n].copy_from_slice(&key[..n]);
                k
            }
            // The strong-key check fails without the legacy provider, so longer keys are
            // refused.
            Algo::Bf if klen > 16 => return Err(PxErr::Code(PXE_KEY_TOO_BIG)),
            Algo::Bf | Algo::Cast5 => key.to_vec(),
        };
        self.iv = match iv {
            Some(iv) => {
                let mut v = vec![0u8; bs];
                let n = iv.len().min(bs);
                v[..n].copy_from_slice(&iv[..n]);
                v
            }
            None => vec![0u8; bs],
        };
        self.cfb_pos = 0;
        self.imp = None;
        Ok(())
    }

    /// EVP_EncryptInit_ex's key schedule, made the first time it's used.
    fn imp(&mut self) -> Px<&Imp> {
        if self.imp.is_none() {
            let k = &self.key;
            let imp = match (self.algo, k.len()) {
                (Algo::Aes, 16) => Imp::Aes128(aes::Aes128::new_from_slice(k).map_err(|_| PxErr::Code(PXE_CIPHER_INIT))?),
                (Algo::Aes, 24) => Imp::Aes192(aes::Aes192::new_from_slice(k).map_err(|_| PxErr::Code(PXE_CIPHER_INIT))?),
                (Algo::Aes, 32) => Imp::Aes256(aes::Aes256::new_from_slice(k).map_err(|_| PxErr::Code(PXE_CIPHER_INIT))?),
                (Algo::Des3, 24) => Imp::Tdes(des::TdesEde3::new_from_slice(k).map_err(|_| PxErr::Code(PXE_CIPHER_INIT))?),
                _ => return Err(PxErr::Code(PXE_CIPHER_INIT)),
            };
            self.imp = Some(imp);
        }
        Ok(self.imp.as_ref().expect("cipher state"))
    }

    /// `px_cipher_encrypt` (EVP_EncryptUpdate + EVP_EncryptFinal_ex).
    pub fn encrypt(&mut self, padding: bool, data: &[u8]) -> Px<Vec<u8>> {
        let bs = self.block_size();
        let mode = self.mode;
        self.imp()?;
        if mode == Mode::Cfb {
            return Ok(self.cfb(data, true));
        }
        let mut buf = data.to_vec();
        if padding {
            let n = bs - buf.len() % bs;
            buf.extend(std::iter::repeat_n(n as u8, n));
        } else if !buf.len().is_multiple_of(bs) {
            return Err(PxErr::Code(PXE_ENCRYPT_FAILED));
        }
        let mut iv = std::mem::take(&mut self.iv);
        let imp = self.imp.as_ref().expect("cipher state");
        for b in buf.chunks_mut(bs) {
            if mode == Mode::Cbc {
                for (x, v) in b.iter_mut().zip(&iv) {
                    *x ^= v;
                }
            }
            imp.encrypt_block(b);
            if mode == Mode::Cbc {
                iv.copy_from_slice(b);
            }
        }
        self.iv = iv;
        Ok(buf)
    }

    /// `px_cipher_decrypt` (EVP_DecryptUpdate + EVP_DecryptFinal_ex).
    pub fn decrypt(&mut self, padding: bool, data: &[u8]) -> Px<Vec<u8>> {
        let bs = self.block_size();
        let mode = self.mode;
        self.imp()?;
        if mode == Mode::Cfb {
            return Ok(self.cfb(data, false));
        }
        if !data.len().is_multiple_of(bs) || (padding && data.is_empty()) {
            return Err(PxErr::Code(PXE_DECRYPT_FAILED));
        }
        let mut buf = data.to_vec();
        let mut iv = std::mem::take(&mut self.iv);
        let imp = self.imp.as_ref().expect("cipher state");
        for b in buf.chunks_mut(bs) {
            let ct = b.to_vec();
            imp.decrypt_block(b);
            if mode == Mode::Cbc {
                for (x, v) in b.iter_mut().zip(&iv) {
                    *x ^= v;
                }
                iv.copy_from_slice(&ct);
            }
        }
        self.iv = iv;
        if padding {
            let n = *buf.last().unwrap_or(&0) as usize;
            if n == 0 || n > bs || buf[buf.len() - n..].iter().any(|&p| p as usize != n) {
                return Err(PxErr::Code(PXE_DECRYPT_FAILED));
            }
            buf.truncate(buf.len() - n);
        }
        Ok(buf)
    }

    /// CFB-128 (`CRYPTO_cfb128_encrypt`, a stream: no padding): the register holds the
    /// keystream block, each byte replaced by the ciphertext byte as it is used.
    fn cfb(&mut self, data: &[u8], encrypt: bool) -> Vec<u8> {
        let bs = self.block_size();
        let mut out = Vec::with_capacity(data.len());
        let mut reg = std::mem::take(&mut self.iv);
        let imp = self.imp.as_ref().expect("cipher state");
        for &b in data {
            if self.cfb_pos == 0 {
                imp.encrypt_block(&mut reg);
            }
            let c = b ^ reg[self.cfb_pos];
            out.push(c);
            reg[self.cfb_pos] = if encrypt { c } else { b };
            self.cfb_pos = (self.cfb_pos + 1) % bs;
        }
        self.iv = reg;
        out
    }
}

/// A cipher with its padding (`PX_Combo`).
pub(super) struct Combo {
    pub cipher: Cipher,
    pub padding: bool,
}

impl Combo {
    /// `px_find_combo`: `cipher[/pad:pkcs|none]`.
    pub fn find(name: &str) -> Px<Combo> {
        let mut parts = name.split('/');
        let cipher_name = parts.next().unwrap_or("");
        let mut pad: Option<&str> = None;
        for p in parts {
            if p.is_empty() {
                continue;
            }
            match p.split_once(':') {
                Some(("pad", v)) => pad = Some(v),
                Some(_) => return Err(PxErr::Code(PXE_BAD_OPTION)),
                None => return Err(PxErr::Code(PXE_BAD_FORMAT)),
            }
        }
        let cipher = Cipher::find(cipher_name).map_err(|_| PxErr::Code(PXE_NO_CIPHER))?;
        let padding = match pad {
            None | Some("pkcs") => true,
            Some("none") => false,
            Some(_) => return Err(PxErr::Code(PXE_NO_CIPHER)),
        };
        Ok(Combo { cipher, padding })
    }

    /// `combo_init`: the key cut to the cipher's largest, zero-padded; the IV cut or padded
    /// to its size.
    pub fn init(&mut self, key: &[u8], iv: Option<&[u8]>) -> Px<()> {
        let ks = self.cipher.key_size();
        let key = &key[..key.len().min(ks)];
        let ivs = self.cipher.iv_size();
        let ivbuf = iv.map(|iv| {
            let mut v = vec![0u8; ivs];
            let n = iv.len().min(ivs);
            v[..n].copy_from_slice(&iv[..n]);
            v
        });
        self.cipher.init(key, Some(ivbuf.as_deref().unwrap_or(&vec![0u8; ivs])))
    }

    pub fn encrypt(&mut self, data: &[u8]) -> Px<Vec<u8>> {
        self.cipher.encrypt(self.padding, data)
    }

    pub fn decrypt(&mut self, data: &[u8]) -> Px<Vec<u8>> {
        self.cipher.decrypt(self.padding, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn aes_fips197() {
        // FIPS-197 appendix C.1 (AES-128, one block, no padding).
        let mut c = Combo::find("aes-ecb/pad:none").unwrap();
        c.init(&unhex("000102030405060708090a0b0c0d0e0f"), None).unwrap();
        assert_eq!(
            hex(&c.encrypt(&unhex("00112233445566778899aabbccddeeff")).unwrap()),
            "69c4e0d86a7b0430d8cdb78070b4c55a"
        );
        // C.3 (AES-256).
        let mut c = Combo::find("aes-ecb/pad:none").unwrap();
        c.init(&unhex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"), None).unwrap();
        assert_eq!(
            hex(&c.encrypt(&unhex("00112233445566778899aabbccddeeff")).unwrap()),
            "8ea2b7ca516745bfeafc49904b496089"
        );
    }

    #[test]
    fn reference_outputs() {
        // encrypt('foo', 'bar', '3des') and encrypt('foo', 'bar', 'aes-cfb') on the reference.
        let mut c = Combo::find("3des").unwrap();
        c.init(b"bar", None).unwrap();
        assert_eq!(hex(&c.encrypt(b"foo").unwrap()), "338620bd2410a4ee");
        let mut c = Combo::find("aes-cfb").unwrap();
        c.init(b"bar", None).unwrap();
        assert_eq!(hex(&c.encrypt(b"foo").unwrap()), "9e10a8");
        let mut c = Combo::find("bf").unwrap();
        c.init(b"bar", None).unwrap();
        assert!(matches!(c.encrypt(b"foo"), Err(PxErr::Code(PXE_CIPHER_INIT))));
        let mut c = Combo::find("3des").unwrap();
        c.init(b"bar", None).unwrap();
        assert_eq!(c.decrypt(&unhex("338620bd2410a4ee")).unwrap(), b"foo");
    }
}
