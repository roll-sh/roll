//! Digests and HMAC (`openssl.c`'s `px_find_digest`, `px-hmac.c`), with the digest names
//! OpenSSL 3's default provider and its legacy alias table resolve.

use super::px::{PXE_CIPHER_INIT, PXE_HASH_UNUSABLE_FOR_HMAC, PXE_NO_HASH, Px, PxErr, elog};
use sha2::digest::DynDigest;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Sha512_224,
    Sha512_256,
    Sha3_224,
    Sha3_256,
    Sha3_384,
    Sha3_512,
    Ripemd160,
    Sm3,
    Blake2b512,
    Blake2s256,
    Md5Sha1,
    /// SHAKE: found, but EVP_MD_CTX_size() fails (no fixed output size).
    Shake128,
    Shake256,
}

/// Names the default provider's digests answer to (matched ignoring trailing spaces, as
/// OpenSSL 3's fetch does).
const PROVIDER: &[(&str, Kind)] = &[
    ("md5", Kind::Md5),
    ("ssl3-md5", Kind::Md5),
    ("1.2.840.113549.2.5", Kind::Md5),
    ("sha1", Kind::Sha1),
    ("sha-1", Kind::Sha1),
    ("ssl3-sha1", Kind::Sha1),
    ("1.3.14.3.2.26", Kind::Sha1),
    ("sha2-224", Kind::Sha224),
    ("sha-224", Kind::Sha224),
    ("sha224", Kind::Sha224),
    ("2.16.840.1.101.3.4.2.4", Kind::Sha224),
    ("sha2-256", Kind::Sha256),
    ("sha-256", Kind::Sha256),
    ("sha256", Kind::Sha256),
    ("2.16.840.1.101.3.4.2.1", Kind::Sha256),
    ("sha2-384", Kind::Sha384),
    ("sha-384", Kind::Sha384),
    ("sha384", Kind::Sha384),
    ("2.16.840.1.101.3.4.2.2", Kind::Sha384),
    ("sha2-512", Kind::Sha512),
    ("sha-512", Kind::Sha512),
    ("sha512", Kind::Sha512),
    ("2.16.840.1.101.3.4.2.3", Kind::Sha512),
    ("sha2-512/224", Kind::Sha512_224),
    ("sha-512/224", Kind::Sha512_224),
    ("sha512-224", Kind::Sha512_224),
    ("2.16.840.1.101.3.4.2.5", Kind::Sha512_224),
    ("sha2-512/256", Kind::Sha512_256),
    ("sha-512/256", Kind::Sha512_256),
    ("sha512-256", Kind::Sha512_256),
    ("2.16.840.1.101.3.4.2.6", Kind::Sha512_256),
    ("sha3-224", Kind::Sha3_224),
    ("2.16.840.1.101.3.4.2.7", Kind::Sha3_224),
    ("sha3-256", Kind::Sha3_256),
    ("2.16.840.1.101.3.4.2.8", Kind::Sha3_256),
    ("sha3-384", Kind::Sha3_384),
    ("2.16.840.1.101.3.4.2.9", Kind::Sha3_384),
    ("sha3-512", Kind::Sha3_512),
    ("2.16.840.1.101.3.4.2.10", Kind::Sha3_512),
    ("shake-128", Kind::Shake128),
    ("shake128", Kind::Shake128),
    ("2.16.840.1.101.3.4.2.11", Kind::Shake128),
    ("shake-256", Kind::Shake256),
    ("shake256", Kind::Shake256),
    ("2.16.840.1.101.3.4.2.12", Kind::Shake256),
    ("ripemd-160", Kind::Ripemd160),
    ("ripemd160", Kind::Ripemd160),
    ("ripemd", Kind::Ripemd160),
    ("rmd160", Kind::Ripemd160),
    ("1.3.36.3.2.1", Kind::Ripemd160),
    ("sm3", Kind::Sm3),
    ("1.2.156.10197.1.401", Kind::Sm3),
    ("blake2b-512", Kind::Blake2b512),
    ("blake2b512", Kind::Blake2b512),
    ("1.3.6.1.4.1.1722.12.2.1.16", Kind::Blake2b512),
    ("blake2s-256", Kind::Blake2s256),
    ("blake2s256", Kind::Blake2s256),
    ("1.3.6.1.4.1.1722.12.2.2.8", Kind::Blake2s256),
    ("md5-sha1", Kind::Md5Sha1),
];

/// The legacy provider's digests: found, but not loadable (EVP_DigestInit_ex fails).
const LEGACY: &[&str] = &["md4", "1.2.840.113549.2.4", "whirlpool", "1.0.10118.3.0.55"];

/// The legacy object-name aliases (matched exactly).
const ALIASES: &[(&str, Option<Kind>)] = &[
    ("rsa-md5", Some(Kind::Md5)),
    ("md5withrsaencryption", Some(Kind::Md5)),
    ("rsa-sha1", Some(Kind::Sha1)),
    ("sha1withrsaencryption", Some(Kind::Sha1)),
    ("rsa-sha1-2", Some(Kind::Sha1)),
    ("rsa-sha224", Some(Kind::Sha224)),
    ("sha224withrsaencryption", Some(Kind::Sha224)),
    ("rsa-sha256", Some(Kind::Sha256)),
    ("sha256withrsaencryption", Some(Kind::Sha256)),
    ("rsa-sha384", Some(Kind::Sha384)),
    ("sha384withrsaencryption", Some(Kind::Sha384)),
    ("rsa-sha512", Some(Kind::Sha512)),
    ("sha512withrsaencryption", Some(Kind::Sha512)),
    ("rsa-sha512/224", Some(Kind::Sha512_224)),
    ("sha512-224withrsaencryption", Some(Kind::Sha512_224)),
    ("rsa-sha512/256", Some(Kind::Sha512_256)),
    ("sha512-256withrsaencryption", Some(Kind::Sha512_256)),
    ("rsa-sha3-224", Some(Kind::Sha3_224)),
    ("id-rsassa-pkcs1-v1_5-with-sha3-224", Some(Kind::Sha3_224)),
    ("rsa-sha3-256", Some(Kind::Sha3_256)),
    ("id-rsassa-pkcs1-v1_5-with-sha3-256", Some(Kind::Sha3_256)),
    ("rsa-sha3-384", Some(Kind::Sha3_384)),
    ("id-rsassa-pkcs1-v1_5-with-sha3-384", Some(Kind::Sha3_384)),
    ("rsa-sha3-512", Some(Kind::Sha3_512)),
    ("id-rsassa-pkcs1-v1_5-with-sha3-512", Some(Kind::Sha3_512)),
    ("rsa-ripemd160", Some(Kind::Ripemd160)),
    ("ripemd160withrsa", Some(Kind::Ripemd160)),
    ("rsa-sm3", Some(Kind::Sm3)),
    ("sm3withrsaencryption", Some(Kind::Sm3)),
    ("rsa-md4", None),
    ("md4withrsaencryption", None),
];

/// A digest context (`PX_MD`).
pub(super) struct Md {
    pub kind: Kind,
    h: Option<Box<dyn DynDigest>>,
    /// MD5-SHA1's SHA-1 half.
    h2: Option<Box<dyn DynDigest>>,
}

fn hasher(kind: Kind) -> Option<Box<dyn DynDigest>> {
    use sha2::Digest as _;
    Some(match kind {
        Kind::Md5 | Kind::Md5Sha1 => Box::new(md5::Md5::new()),
        Kind::Sha1 => Box::new(sha1::Sha1::new()),
        Kind::Sha224 => Box::new(sha2::Sha224::new()),
        Kind::Sha256 => Box::new(sha2::Sha256::new()),
        Kind::Sha384 => Box::new(sha2::Sha384::new()),
        Kind::Sha512 => Box::new(sha2::Sha512::new()),
        Kind::Sha512_224 => Box::new(sha2::Sha512_224::new()),
        Kind::Sha512_256 => Box::new(sha2::Sha512_256::new()),
        Kind::Sha3_224 => Box::new(sha3::Sha3_224::new()),
        Kind::Sha3_256 => Box::new(sha3::Sha3_256::new()),
        Kind::Sha3_384 => Box::new(sha3::Sha3_384::new()),
        Kind::Sha3_512 => Box::new(sha3::Sha3_512::new()),
        Kind::Ripemd160 => Box::new(ripemd::Ripemd160::new()),
        Kind::Sm3 => Box::new(sm3::Sm3::new()),
        Kind::Blake2b512 => Box::new(blake2::Blake2b512::new()),
        Kind::Blake2s256 => Box::new(blake2::Blake2s256::new()),
        Kind::Shake128 | Kind::Shake256 => return None,
    })
}

impl Md {
    pub fn new(kind: Kind) -> Md {
        Md {
            kind,
            h: hasher(kind),
            h2: (kind == Kind::Md5Sha1).then(|| hasher(Kind::Sha1)).flatten(),
        }
    }

    /// `px_find_digest` by the (lowercased) name.
    pub fn find(name: &str) -> Px<Md> {
        let trimmed = name.trim_end_matches(' ');
        if let Some((_, k)) = PROVIDER.iter().find(|(n, _)| *n == trimmed) {
            return Ok(Md::new(*k));
        }
        if LEGACY.contains(&trimmed) {
            return Err(PxErr::Code(PXE_CIPHER_INIT));
        }
        match ALIASES.iter().find(|(n, _)| *n == name) {
            Some((_, Some(k))) => Ok(Md::new(*k)),
            Some((_, None)) => Err(PxErr::Code(PXE_CIPHER_INIT)),
            None => Err(PxErr::Code(PXE_NO_HASH)),
        }
    }

    /// `px_md_result_size` (EVP_MD_CTX_size).
    pub fn result_size(&self) -> Px<usize> {
        Ok(match self.kind {
            Kind::Md5 => 16,
            Kind::Sha1 | Kind::Ripemd160 => 20,
            Kind::Sha224 | Kind::Sha512_224 | Kind::Sha3_224 => 28,
            Kind::Sha256 | Kind::Sha512_256 | Kind::Sha3_256 | Kind::Sm3 | Kind::Blake2s256 => 32,
            Kind::Sha384 | Kind::Sha3_384 => 48,
            Kind::Sha512 | Kind::Sha3_512 | Kind::Blake2b512 => 64,
            Kind::Md5Sha1 => 36,
            Kind::Shake128 | Kind::Shake256 => return Err(elog("EVP_MD_CTX_size() failed")),
        })
    }

    /// `px_md_block_size` (EVP_MD_CTX_block_size).
    pub fn block_size(&self) -> usize {
        match self.kind {
            Kind::Sha384 | Kind::Sha512 | Kind::Sha512_224 | Kind::Sha512_256 | Kind::Blake2b512 => 128,
            Kind::Sha3_224 => 144,
            Kind::Sha3_256 | Kind::Shake256 => 136,
            Kind::Sha3_384 => 104,
            Kind::Sha3_512 => 72,
            Kind::Shake128 => 168,
            _ => 64,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        if let Some(h) = &mut self.h {
            h.update(data);
        }
        if let Some(h) = &mut self.h2 {
            h.update(data);
        }
    }

    pub fn reset(&mut self) {
        if let Some(h) = &mut self.h {
            h.reset();
        }
        if let Some(h) = &mut self.h2 {
            h.reset();
        }
    }

    /// `px_md_finish`: the digest; the context is reset afterwards.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = self.h.as_mut().map(|h| h.finalize_reset().into_vec()).unwrap_or_default();
        if let Some(h) = &mut self.h2 {
            out.extend_from_slice(&h.finalize_reset());
        }
        out
    }
}

/// `digest(data, name)`'s computation, `name` already lowercased.
#[cfg(test)]
pub(super) fn digest(data: &[u8], name: &str) -> Px<Vec<u8>> {
    let mut md = Md::find(name)?;
    md.result_size()?;
    md.update(data);
    Ok(md.finish())
}

/// An HMAC context (`PX_HMAC`).
pub(super) struct Hmac {
    md: Md,
    ipad: Vec<u8>,
    opad: Vec<u8>,
}

impl Hmac {
    /// `px_find_hmac`.
    pub fn find(name: &str) -> Px<Hmac> {
        let md = Md::find(name)?;
        if md.block_size() < 2 {
            return Err(PxErr::Code(PXE_HASH_UNUSABLE_FOR_HMAC));
        }
        Ok(Hmac {
            md,
            ipad: Vec::new(),
            opad: Vec::new(),
        })
    }

    pub fn result_size(&self) -> Px<usize> {
        self.md.result_size()
    }

    /// `hmac_init`.
    pub fn init(&mut self, key: &[u8]) {
        let bs = self.md.block_size();
        let mut keybuf = vec![0u8; bs];
        if key.len() > bs {
            self.md.update(key);
            let h = self.md.finish();
            let n = h.len().min(bs);
            keybuf[..n].copy_from_slice(&h[..n]);
            self.md.reset();
        } else {
            keybuf[..key.len()].copy_from_slice(key);
        }
        self.ipad = keybuf.iter().map(|b| b ^ 0x36).collect();
        self.opad = keybuf.iter().map(|b| b ^ 0x5c).collect();
        let ipad = self.ipad.clone();
        self.md.update(&ipad);
    }

    pub fn update(&mut self, data: &[u8]) {
        self.md.update(data);
    }

    /// `hmac_finish`.
    pub fn finish(&mut self) -> Vec<u8> {
        let inner = self.md.finish();
        self.md.reset();
        let opad = self.opad.clone();
        self.md.update(&opad);
        self.md.update(&inner);
        self.md.finish()
    }
}

/// `hmac(data, key, name)`'s computation, `name` already lowercased.
#[cfg(test)]
pub(super) fn hmac(data: &[u8], key: &[u8], name: &str) -> Px<Vec<u8>> {
    let mut h = Hmac::find(name)?;
    h.result_size()?;
    h.init(key);
    h.update(data);
    Ok(h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn digests_match_reference() {
        // Recorded from the reference: digest('abc', n) and hmac('abc', 'key', n).
        let cases = [
            ("md5", "900150983cd24fb0d6963f7d28e17f72", "d2fe98063f876b03193afb49b4979591"),
            ("sha1", "a9993e364706816aba3e25717850c26c9cd0d89d", "4fd0b215276ef12f2b3e4c8ecac2811498b656fc"),
            ("sha3-256", "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532", "09b6dbab8d11795ca7c8d82f1cf91682013c7cb980abbb25473be4ae7f7b5683"),
            ("sha3-512 ", "b751850b1a57168a5693cd924b6b096e08f621827444f70d884f5d0240d2712e10e116e9192af3c91a7ec57647e3934057340b4cf408d5a56592f8274eec53f0", "085e4e83503f40b82fef38438bc4905a55dbaa8c8878097a899db0b57ce7da57a368251c34474f60b3ebacb39b2edaca4b290456411c76ec7ab61944cfe2288e"),
            ("rsa-sha384", "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7", "30ddb9c8f347cffbfb44e519d814f074cf4047a55d6f563324f1c6a33920e5edfb2a34bac60bdc96cd33a95623d7d638"),
            ("sha512-224", "4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa", "28bc0433ef684ea9877c2f2437aa0ccfc2e8eb3a9b4c731606877b4f"),
            ("ripemd160", "8eb208f7e05d987a9b044a8e98c6b087f15a0bfc", "67fdce738ebfc7372bcd38f03c023b5746724d18"),
            ("sm3", "66c7f0f462eeedd9d1f2d46bdc10e4e24167c4875cf2f7a2297da02b8f4ba8e0", "28e63256e7c5a087b1f073265dc53092163f7b82729735d06f28f10af9d52393"),
            ("blake2b512", "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d17d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923", "05cc4815438d5cfe68fff446b8df57828cc96189de4b4e928e3f06d815d64e5bc15124a02ffd39859b3e2476da03bc0235ca86df623af2a5631779809e9fd04a"),
            ("blake2s256", "508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982", "0018bfddc0c878b3e270c6e59b35af01e164a42d89499c9447a90b58a4d60eb3"),
            ("md5-sha1", "900150983cd24fb0d6963f7d28e17f72a9993e364706816aba3e25717850c26c9cd0d89d", "a76e5cc4bdaa99b87cc7de69e0606e7d74fd2771909f120bb3b9a4649bb99bea2b4cec32"),
        ];
        for (n, d, h) in cases {
            assert_eq!(hex(&digest(b"abc", n).unwrap()), d, "{n}");
            assert_eq!(hex(&hmac(b"abc", b"key", n).unwrap()), h, "hmac {n}");
        }
        assert!(matches!(Md::find("rsa-sha256 "), Err(PxErr::Code(PXE_NO_HASH))));
        assert!(matches!(Md::find("md4"), Err(PxErr::Code(PXE_CIPHER_INIT))));
        assert!(digest(b"a", "shake128").is_err());
    }

    #[test]
    fn rfc4231_hmac_sha256() {
        let key = [0x0bu8; 20];
        assert_eq!(
            hex(&hmac(b"Hi There", &key, "sha256").unwrap()),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // A key longer than the block size is hashed first (test case 6).
        let key = [0xaau8; 131];
        assert_eq!(
            hex(&hmac(b"Test Using Larger Than Block-Size Key - Hash Key First", &key, "sha512").unwrap()),
            "80b24263c7c1a3ebb71493c1dd7be8b49b46d1f41b4aeec1121b013783f8f3526b56d037e05f2598bd0fd2215d6a1e5295e64f73f63f0aec8b915a985d786598"
        );
    }
}
