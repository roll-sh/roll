//! OpenPGP's CFB mode (`pgp-cfb.c`): the normal one of SYMENCRYPTED_DATA_MDC packets and the
//! old resynchronizing one of SYMENCRYPTED_DATA.

use super::super::cipher::Cipher;
use super::super::px::{EXTERNAL_ROUTINE, PXE_PGP_CORRUPT_DATA, PXE_PGP_UNSUPPORTED_CIPHER, Px, PxErr, strerror};
use crate::Error;

/// The OpenPGP symmetric algorithms pgcrypto knows: (code, names, cipher, key and block
/// length).
pub(super) const CIPHERS: &[(u8, &str, &str, usize, usize)] = &[
    (2, "3des", "3des-ecb", 24, 8),
    (3, "cast5", "cast5-ecb", 16, 8),
    (4, "bf", "bf-ecb", 16, 8),
    (4, "blowfish", "bf-ecb", 16, 8),
    (7, "aes", "aes-ecb", 16, 16),
    (7, "aes128", "aes-ecb", 16, 16),
    (8, "aes192", "aes-ecb", 24, 16),
    (9, "aes256", "aes-ecb", 32, 16),
    (10, "twofish", "twofish-ecb", 32, 16),
];

/// `pgp_get_cipher_code`.
pub(super) fn cipher_code(name: &str) -> Option<u8> {
    CIPHERS.iter().find(|c| c.1.eq_ignore_ascii_case(name)).map(|c| c.0)
}

/// `pgp_get_cipher_key_size` (0 for an unknown code).
pub(super) fn key_size(code: i32) -> usize {
    CIPHERS.iter().find(|c| c.0 as i32 == code).map_or(0, |c| c.3)
}

/// `pgp_get_cipher_block_size`.
pub(super) fn block_size(code: i32) -> usize {
    CIPHERS.iter().find(|c| c.0 as i32 == code).map_or(0, |c| c.4)
}

/// `pgp_load_cipher`.
fn load_cipher(code: i32) -> Px<Cipher> {
    let Some(c) = CIPHERS.iter().find(|c| c.0 as i32 == code) else {
        return Err(PxErr::Code(PXE_PGP_CORRUPT_DATA));
    };
    Cipher::find(c.2).map_err(|_| PxErr::Code(PXE_PGP_UNSUPPORTED_CIPHER))
}

/// `PGP_CFB`.
pub(super) struct Cfb {
    ciph: Cipher,
    bs: usize,
    pos: usize,
    block_no: usize,
    resync: bool,
    ignore_failure: bool,
    fr: Vec<u8>,
    fre: Vec<u8>,
    encbuf: Vec<u8>,
}

impl Cfb {
    /// `pgp_cfb_create`.
    pub fn new(algo: i32, key: &[u8], resync: bool, iv: Option<&[u8]>, ignore_failure: bool) -> Px<Cfb> {
        let mut ciph = load_cipher(algo)?;
        ciph.init(key, None)?;
        let bs = ciph.block_size();
        let mut fr = vec![0u8; bs];
        if let Some(iv) = iv {
            fr.copy_from_slice(&iv[..bs]);
        }
        Ok(Cfb {
            ciph,
            bs,
            pos: 0,
            block_no: 0,
            resync,
            ignore_failure,
            fr,
            fre: vec![0u8; bs],
            encbuf: vec![0u8; bs],
        })
    }

    /// `mix_*`: `n` bytes at the current position.
    fn mix(&mut self, data: &[u8], dst: &mut Vec<u8>, encrypt: bool) -> usize {
        let take = |s: &mut Cfb, data: &[u8], dst: &mut Vec<u8>| {
            for (k, &b) in data.iter().enumerate() {
                let i = s.pos + k;
                if encrypt {
                    s.encbuf[i] = s.fre[i] ^ b;
                    dst.push(s.encbuf[i]);
                } else {
                    s.encbuf[i] = b;
                    dst.push(s.fre[i] ^ b);
                }
            }
            s.pos += data.len();
        };
        if self.resync && self.block_no == 2 {
            // Block #2 is 2 bytes long.
            let n = (2 - self.pos).min(data.len());
            take(self, &data[..n], dst);
            if self.pos == 2 {
                let bs = self.bs;
                let mut fr = self.encbuf[2..bs].to_vec();
                fr.extend_from_slice(&self.encbuf[..2]);
                self.fr = fr;
                self.pos = 0;
                return n;
            }
            take(self, &data[n..], dst);
            return data.len();
        }
        take(self, data, dst);
        data.len()
    }

    /// `cfb_process`.
    fn process(&mut self, mut data: &[u8], encrypt: bool) -> Result<Vec<u8>, Error> {
        let ignore = !encrypt && self.ignore_failure;
        let mut out = Vec::with_capacity(data.len());
        while !data.is_empty() && self.pos > 0 {
            let n = (self.bs - self.pos).min(data.len());
            let n = self.mix(&data[..n], &mut out, encrypt);
            data = &data[n..];
            if self.pos == self.bs {
                self.fr.copy_from_slice(&self.encbuf);
                self.pos = 0;
            }
        }
        while !data.is_empty() {
            match self.ciph.encrypt(false, &self.fr.clone()) {
                Ok(b) => self.fre = b,
                Err(PxErr::Code(e)) if !ignore => {
                    return Err(Error::new(EXTERNAL_ROUTINE, format!("encrypt error: {}", strerror(e))));
                }
                Err(PxErr::Raised(e)) if !ignore => return Err(e),
                Err(_) => {}
            }
            if self.block_no < 5 {
                self.block_no += 1;
            }
            let n = self.bs.min(data.len());
            let n = self.mix(&data[..n], &mut out, encrypt);
            data = &data[n..];
            if self.pos == self.bs {
                self.fr.copy_from_slice(&self.encbuf);
                self.pos = 0;
            }
        }
        Ok(out)
    }

    pub fn encrypt(&mut self, data: &[u8]) -> Result<Vec<u8>, Error> {
        self.process(data, true)
    }

    pub fn decrypt(&mut self, data: &[u8]) -> Result<Vec<u8>, Error> {
        self.process(data, false)
    }
}
