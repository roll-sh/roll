//! Checksums: MD5 (ETags, `Content-MD5`) and the `x-amz-checksum-*` algorithms, streaming, plus
//! the multipart forms (composite `<base64>-N` and full-object CRC combination).

use crate::error::{Code, S3Error};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use crc_fast::{CrcAlgorithm, Digest as CrcDigest};
use md5::Digest as _;
use sha2::Digest as _;

/// An `x-amz-checksum-*` algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Algorithm {
    Crc32,
    Crc32c,
    Crc64Nvme,
    Sha1,
    Sha256,
}

pub const ALGORITHMS: [Algorithm; 5] = [Algorithm::Crc32, Algorithm::Crc32c, Algorithm::Crc64Nvme, Algorithm::Sha1, Algorithm::Sha256];

impl Algorithm {
    /// `CRC32`, `crc32c`, … (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        ALGORITHMS.into_iter().find(|a| a.as_str().eq_ignore_ascii_case(s.trim()))
    }

    /// `CRC32`, `CRC32C`, `CRC64NVME`, `SHA1`, `SHA256`.
    pub fn as_str(self) -> &'static str {
        match self {
            Algorithm::Crc32 => "CRC32",
            Algorithm::Crc32c => "CRC32C",
            Algorithm::Crc64Nvme => "CRC64NVME",
            Algorithm::Sha1 => "SHA1",
            Algorithm::Sha256 => "SHA256",
        }
    }

    /// `x-amz-checksum-crc32`, …
    pub fn header(self) -> &'static str {
        match self {
            Algorithm::Crc32 => "x-amz-checksum-crc32",
            Algorithm::Crc32c => "x-amz-checksum-crc32c",
            Algorithm::Crc64Nvme => "x-amz-checksum-crc64nvme",
            Algorithm::Sha1 => "x-amz-checksum-sha1",
            Algorithm::Sha256 => "x-amz-checksum-sha256",
        }
    }

    /// The XML element: `ChecksumCRC32`, …
    pub fn xml_element(self) -> &'static str {
        match self {
            Algorithm::Crc32 => "ChecksumCRC32",
            Algorithm::Crc32c => "ChecksumCRC32C",
            Algorithm::Crc64Nvme => "ChecksumCRC64NVME",
            Algorithm::Sha1 => "ChecksumSHA1",
            Algorithm::Sha256 => "ChecksumSHA256",
        }
    }

    pub fn from_header(name: &str) -> Option<Self> {
        ALGORITHMS.into_iter().find(|a| a.header().eq_ignore_ascii_case(name))
    }

    pub fn from_xml_element(name: &str) -> Option<Self> {
        ALGORITHMS.into_iter().find(|a| a.xml_element() == name)
    }

    /// Raw digest length in bytes.
    pub fn digest_len(self) -> usize {
        match self {
            Algorithm::Crc32 | Algorithm::Crc32c => 4,
            Algorithm::Crc64Nvme => 8,
            Algorithm::Sha1 => 20,
            Algorithm::Sha256 => 32,
        }
    }

    pub fn is_crc(self) -> bool {
        matches!(self, Algorithm::Crc32 | Algorithm::Crc32c | Algorithm::Crc64Nvme)
    }

    fn crc(self) -> Option<CrcAlgorithm> {
        match self {
            Algorithm::Crc32 => Some(CrcAlgorithm::Crc32IsoHdlc),
            Algorithm::Crc32c => Some(CrcAlgorithm::Crc32Iscsi),
            Algorithm::Crc64Nvme => Some(CrcAlgorithm::Crc64Nvme),
            _ => None,
        }
    }

    /// The checksum type S3 uses for a multipart upload when the client doesn't say:
    /// CRC64NVME is always full-object, the rest composite.
    pub fn default_multipart_type(self) -> ChecksumType {
        if self == Algorithm::Crc64Nvme { ChecksumType::FullObject } else { ChecksumType::Composite }
    }

    /// Whether S3 allows this algorithm with this multipart checksum type.
    pub fn supports(self, t: ChecksumType) -> bool {
        match t {
            ChecksumType::Composite => self != Algorithm::Crc64Nvme,
            ChecksumType::FullObject => self.is_crc(),
        }
    }

    /// Decode and length-check a base64 checksum value.
    pub fn decode(self, b64: &str) -> Result<Vec<u8>, S3Error> {
        match B64.decode(b64.trim()) {
            Ok(v) if v.len() == self.digest_len() => Ok(v),
            _ => Err(S3Error::msg(Code::InvalidRequest, format!("Value for {} header is invalid.", self.header()))),
        }
    }

    pub fn checksum(self, data: &[u8]) -> Vec<u8> {
        let mut h = Hasher::new(self);
        h.update(data);
        h.finalize()
    }
}

/// `x-amz-checksum-type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChecksumType {
    FullObject,
    Composite,
}

impl ChecksumType {
    pub fn as_str(self) -> &'static str {
        match self {
            ChecksumType::FullObject => "FULL_OBJECT",
            ChecksumType::Composite => "COMPOSITE",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "FULL_OBJECT" => Some(ChecksumType::FullObject),
            "COMPOSITE" => Some(ChecksumType::Composite),
            _ => None,
        }
    }
}

/// A streaming checksum.
#[derive(Clone)]
pub enum Hasher {
    Crc(Algorithm, CrcDigest),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
}

impl Hasher {
    pub fn new(a: Algorithm) -> Self {
        match a.crc() {
            Some(c) => Hasher::Crc(a, CrcDigest::new(c)),
            None if a == Algorithm::Sha1 => Hasher::Sha1(sha1::Sha1::new()),
            None => Hasher::Sha256(sha2::Sha256::new()),
        }
    }

    pub fn algorithm(&self) -> Algorithm {
        match self {
            Hasher::Crc(a, _) => *a,
            Hasher::Sha1(_) => Algorithm::Sha1,
            Hasher::Sha256(_) => Algorithm::Sha256,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Crc(_, d) => d.update(data),
            Hasher::Sha1(h) => h.update(data),
            Hasher::Sha256(h) => h.update(data),
        }
    }

    /// The CRC value so far (CRC algorithms only), for full-object combination.
    pub fn crc_value(&self) -> Option<u64> {
        match self {
            Hasher::Crc(_, d) => Some(d.finalize()),
            _ => None,
        }
    }

    /// Raw digest bytes (CRCs big-endian, as S3 encodes them).
    pub fn finalize(self) -> Vec<u8> {
        match self {
            Hasher::Crc(a, d) => crc_bytes(a, d.finalize()),
            Hasher::Sha1(h) => h.finalize().to_vec(),
            Hasher::Sha256(h) => h.finalize().to_vec(),
        }
    }

    pub fn finalize_b64(self) -> String {
        B64.encode(self.finalize())
    }
}

/// A CRC value as the big-endian bytes S3 base64-encodes.
pub fn crc_bytes(a: Algorithm, v: u64) -> Vec<u8> {
    if a == Algorithm::Crc64Nvme { v.to_be_bytes().to_vec() } else { (v as u32).to_be_bytes().to_vec() }
}

/// A CRC value from its raw bytes.
pub fn crc_from_bytes(a: Algorithm, b: &[u8]) -> Option<u64> {
    match (a, b.len()) {
        (Algorithm::Crc64Nvme, 8) => Some(u64::from_be_bytes(b.try_into().ok()?)),
        (Algorithm::Crc32 | Algorithm::Crc32c, 4) => Some(u32::from_be_bytes(b.try_into().ok()?) as u64),
        _ => None,
    }
}

pub fn b64(raw: &[u8]) -> String {
    B64.encode(raw)
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    B64.decode(s.trim()).ok()
}

/// Streaming MD5.
#[derive(Clone, Default)]
pub struct Md5(md5::Md5);

impl Md5 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finalize(self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out.copy_from_slice(self.0.finalize().as_slice());
        out
    }
}

pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(data);
    h.finalize()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(data))
}

/// What a request body hashes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyDigest {
    pub len: u64,
    pub md5: [u8; 16],
    pub sha256: Option<[u8; 32]>,
    /// The requested checksum, raw.
    pub checksum: Option<(Algorithm, Vec<u8>)>,
    /// The same as a CRC value, for combining parts (CRC algorithms only).
    pub crc: Option<u64>,
}

impl BodyDigest {
    /// The ETag of a single-part object (unquoted hex MD5).
    pub fn etag(&self) -> String {
        hex::encode(self.md5)
    }

    /// The checksum, base64.
    pub fn checksum_b64(&self) -> Option<(Algorithm, String)> {
        self.checksum.as_ref().map(|(a, v)| (*a, B64.encode(v)))
    }

    /// Check `Content-MD5` (base64), if sent.
    pub fn verify_content_md5(&self, header: Option<&str>) -> Result<(), S3Error> {
        let Some(h) = header else { return Ok(()) };
        match B64.decode(h.trim()) {
            Ok(v) if v.len() == 16 => {
                if v == self.md5 {
                    Ok(())
                } else {
                    Err(S3Error::new(Code::BadDigest))
                }
            }
            _ => Err(S3Error::msg(Code::InvalidDigest, "The Content-MD5 you specified is not valid.")),
        }
    }

    /// Check a client-sent checksum (header or trailer, base64) against the computed one.
    pub fn verify_checksum(&self, a: Algorithm, b64: &str) -> Result<(), S3Error> {
        let want = a.decode(b64)?;
        match &self.checksum {
            Some((got_a, got)) if *got_a == a => {
                if *got == want {
                    Ok(())
                } else {
                    Err(S3Error::msg(Code::BadDigest, format!("The {} you specified did not match the calculated checksum.", a.as_str())))
                }
            }
            _ => Err(S3Error::msg(Code::InternalError, format!("{} was not computed", a.as_str()))),
        }
    }

    /// Check a hex SHA-256 payload hash (`x-amz-content-sha256`).
    pub fn verify_sha256(&self, hex_hash: &str) -> Result<(), S3Error> {
        match &self.sha256 {
            Some(got) if hex::encode(got).eq_ignore_ascii_case(hex_hash) => Ok(()),
            Some(got) => Err(S3Error::new(Code::XAmzContentSHA256Mismatch).with("ClientComputedContentSHA256", hex_hash).with("S3ComputedContentSHA256", hex::encode(got))),
            None => Err(S3Error::msg(Code::InternalError, "payload SHA-256 was not computed")),
        }
    }
}

/// Hashes a body once for everything a PUT needs: MD5 always, SHA-256 when the payload hash is
/// signed, and the requested checksum.
#[derive(Clone)]
pub struct BodyHasher {
    len: u64,
    md5: Md5,
    sha256: Option<sha2::Sha256>,
    checksum: Option<Hasher>,
}

impl BodyHasher {
    pub fn new(checksum: Option<Algorithm>, sha256: bool) -> Self {
        Self { len: 0, md5: Md5::new(), sha256: sha256.then(sha2::Sha256::new), checksum: checksum.map(Hasher::new) }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.len += data.len() as u64;
        self.md5.update(data);
        if let Some(h) = &mut self.sha256 {
            h.update(data);
        }
        if let Some(h) = &mut self.checksum {
            h.update(data);
        }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn finish(self) -> BodyDigest {
        let crc = self.checksum.as_ref().and_then(Hasher::crc_value);
        BodyDigest {
            len: self.len,
            md5: self.md5.finalize(),
            sha256: self.sha256.map(|h| h.finalize().into()),
            checksum: self.checksum.map(|h| (h.algorithm(), h.finalize())),
            crc,
        }
    }
}

/// The ETag of a multipart object: hex MD5 of the parts' binary MD5s, then `-<parts>`.
pub fn multipart_etag(part_md5s: &[[u8; 16]]) -> String {
    let mut h = Md5::new();
    for m in part_md5s {
        h.update(m);
    }
    format!("{}-{}", hex::encode(h.finalize()), part_md5s.len())
}

/// [`multipart_etag`] from the parts' ETags (hex, quotes allowed).
pub fn multipart_etag_from_etags<S: AsRef<str>>(etags: &[S]) -> Result<String, S3Error> {
    let mut md5s = Vec::with_capacity(etags.len());
    for e in etags {
        let hexs = crate::http::unquote_etag(e.as_ref());
        let raw = hex::decode(hexs).ok().filter(|v| v.len() == 16).ok_or_else(|| S3Error::new(Code::InvalidPart))?;
        md5s.push(raw.try_into().unwrap());
    }
    Ok(multipart_etag(&md5s))
}

/// The composite checksum of a multipart object: the algorithm over the concatenated raw part
/// checksums, base64, then `-<parts>`.
pub fn composite(a: Algorithm, part_checksums: &[Vec<u8>]) -> String {
    let mut h = Hasher::new(a);
    for c in part_checksums {
        h.update(c);
    }
    format!("{}-{}", h.finalize_b64(), part_checksums.len())
}

/// The full-object CRC of parts given as `(crc, length)`, in order. `None` for non-CRC
/// algorithms.
pub fn combine_crc(a: Algorithm, parts: &[(u64, u64)]) -> Option<u64> {
    let c = a.crc()?;
    let mut iter = parts.iter();
    let Some(&(mut acc, _)) = iter.next() else { return Some(crc_fast::checksum(c, b"")) };
    for &(crc, len) in iter {
        acc = crc_fast::checksum_combine(c, acc, crc, len);
    }
    Some(acc)
}

/// The checksum a request asks for: `(algorithm, base64 value if sent as a header)`. The value
/// is `None` when it comes in a trailer (`x-amz-trailer`) or the client only named the
/// algorithm (`x-amz-sdk-checksum-algorithm`).
pub fn requested<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Option<(Algorithm, Option<String>)>, S3Error> {
    let mut found: Option<(Algorithm, Option<String>)> = None;
    for a in ALGORITHMS {
        if let Some(v) = get(a.header()) {
            if found.is_some() {
                return Err(S3Error::msg(Code::InvalidRequest, "Expecting a single x-amz-checksum- header. Multiple checksum Types are not allowed."));
            }
            a.decode(v)?;
            found = Some((a, Some(v.trim().to_string())));
        }
    }
    if let Some(t) = get("x-amz-trailer") {
        let mut trailer = None;
        for name in t.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match Algorithm::from_header(name) {
                Some(a) if trailer.is_none() => trailer = Some(a),
                Some(_) => return Err(S3Error::msg(Code::InvalidRequest, "Expecting a single x-amz-checksum- trailer.")),
                None => return Err(S3Error::msg(Code::InvalidRequest, format!("The value specified in the x-amz-trailer header is not supported: {name}"))),
            }
        }
        if let Some(a) = trailer {
            if found.is_some() {
                return Err(S3Error::msg(Code::InvalidRequest, "Expecting a single x-amz-checksum- header. Multiple checksum Types are not allowed."));
            }
            found = Some((a, None));
        }
    }
    if found.is_none()
        && let Some(v) = get("x-amz-sdk-checksum-algorithm")
    {
        let a = Algorithm::parse(v).ok_or_else(|| S3Error::msg(Code::InvalidRequest, format!("Value for x-amz-sdk-checksum-algorithm header is invalid: {v}")))?;
        found = Some((a, None));
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        let check = b"123456789";
        assert_eq!(hex::encode(Algorithm::Crc32.checksum(check)), "cbf43926");
        assert_eq!(hex::encode(Algorithm::Crc32c.checksum(check)), "e3069283");
        assert_eq!(hex::encode(Algorithm::Crc64Nvme.checksum(check)), "ae8b14860a799888");
        assert_eq!(hex::encode(Algorithm::Sha1.checksum(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex::encode(Algorithm::Sha256.checksum(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        // S3 documents CRC32 of "Hello world" as "i9aeUg==".
        assert_eq!(b64(&Algorithm::Crc32.checksum(b"Hello world")), "i9aeUg==");
        assert_eq!(hex::encode(md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn streaming_equals_one_shot() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7 % 251) as u8).collect();
        for a in ALGORITHMS {
            let mut h = Hasher::new(a);
            for c in data.chunks(333) {
                h.update(c);
            }
            assert_eq!(h.finalize(), a.checksum(&data), "{a:?}");
        }
        let mut b = BodyHasher::new(Some(Algorithm::Crc32c), true);
        for c in data.chunks(1000) {
            b.update(c);
        }
        let d = b.finish();
        assert_eq!(d.len, 10_000);
        assert_eq!(d.md5, md5(&data));
        assert_eq!(hex::encode(d.sha256.unwrap()), sha256_hex(&data));
        assert_eq!(d.checksum.as_ref().unwrap().1, Algorithm::Crc32c.checksum(&data));
        d.verify_checksum(Algorithm::Crc32c, &b64(&Algorithm::Crc32c.checksum(&data))).unwrap();
        assert_eq!(d.verify_checksum(Algorithm::Crc32c, &b64(&[0, 0, 0, 0])).unwrap_err().code, Code::BadDigest);
        assert_eq!(d.verify_checksum(Algorithm::Crc32c, "AAA").unwrap_err().code, Code::InvalidRequest);
        d.verify_content_md5(Some(&b64(&md5(&data)))).unwrap();
        assert_eq!(d.verify_content_md5(Some(&b64(&md5(b"x")))).unwrap_err().code, Code::BadDigest);
        assert_eq!(d.verify_content_md5(Some("nope")).unwrap_err().code, Code::InvalidDigest);
        d.verify_sha256(&sha256_hex(&data).to_uppercase()).unwrap();
        assert_eq!(d.verify_sha256(&sha256_hex(b"")).unwrap_err().code, Code::XAmzContentSHA256Mismatch);
    }

    #[test]
    fn multipart_forms() {
        let parts: Vec<Vec<u8>> = vec![vec![1u8; 5000], vec![2u8; 3000], vec![3u8; 17]];
        let whole: Vec<u8> = parts.concat();
        for a in [Algorithm::Crc32, Algorithm::Crc32c, Algorithm::Crc64Nvme] {
            let crcs: Vec<(u64, u64)> = parts
                .iter()
                .map(|p| {
                    let mut h = Hasher::new(a);
                    h.update(p);
                    (h.crc_value().unwrap(), p.len() as u64)
                })
                .collect();
            let combined = combine_crc(a, &crcs).unwrap();
            assert_eq!(crc_bytes(a, combined), a.checksum(&whole), "{a:?}");
            assert_eq!(crc_from_bytes(a, &crc_bytes(a, combined)), Some(combined));
        }
        assert_eq!(combine_crc(Algorithm::Sha256, &[]), None);
        let raw: Vec<Vec<u8>> = parts.iter().map(|p| Algorithm::Sha256.checksum(p)).collect();
        let expect = b64(&Algorithm::Sha256.checksum(&raw.concat()));
        assert_eq!(composite(Algorithm::Sha256, &raw), format!("{expect}-3"));
        let md5s: Vec<[u8; 16]> = parts.iter().map(|p| md5(p)).collect();
        let etag = multipart_etag(&md5s);
        assert!(etag.ends_with("-3"));
        assert_eq!(hex::encode(md5(&md5s.concat())) + "-3", etag);
        let quoted: Vec<String> = md5s.iter().map(|m| format!("\"{}\"", hex::encode(m))).collect();
        assert_eq!(multipart_etag_from_etags(&quoted).unwrap(), etag);
        assert_eq!(multipart_etag_from_etags(&["zz"]).unwrap_err().code, Code::InvalidPart);
        assert_eq!(Algorithm::Crc64Nvme.default_multipart_type(), ChecksumType::FullObject);
        assert!(!Algorithm::Sha1.supports(ChecksumType::FullObject));
        assert!(!Algorithm::Crc64Nvme.supports(ChecksumType::Composite));
    }

    #[test]
    fn requested_checksum() {
        let crc = b64(&Algorithm::Crc32.checksum(b"x"));
        let r = requested(|n| (n == "x-amz-checksum-crc32").then_some(crc.as_str())).unwrap();
        assert_eq!(r, Some((Algorithm::Crc32, Some(crc.clone()))));
        let r = requested(|n| match n {
            "x-amz-trailer" => Some("x-amz-checksum-crc64nvme"),
            _ => None,
        })
        .unwrap();
        assert_eq!(r, Some((Algorithm::Crc64Nvme, None)));
        let r = requested(|n| match n {
            "x-amz-sdk-checksum-algorithm" => Some("sha256"),
            _ => None,
        })
        .unwrap();
        assert_eq!(r, Some((Algorithm::Sha256, None)));
        assert!(requested(|n| match n {
            "x-amz-checksum-crc32" => Some("AAAAAA=="),
            "x-amz-checksum-crc32c" => Some("AAAAAA=="),
            _ => None,
        })
        .is_err());
        assert!(requested(|n| match n {
            "x-amz-checksum-crc32" => Some("bad"),
            _ => None,
        })
        .is_err());
        assert_eq!(requested(|_| None).unwrap(), None);
    }

    #[test]
    fn requested_checksum_conflicts() {
        let crc = b64(&Algorithm::Crc32.checksum(b"x"));
        let req = |pairs: &[(&'static str, &str)]| {
            let pairs: Vec<(&str, String)> = pairs.iter().map(|(k, v)| (*k, v.to_string())).collect();
            requested(|n| pairs.iter().find(|(k, _)| *k == n).map(|(_, v)| v.as_str())).map_err(|e| e.code)
        };
        let bad = Err(Code::InvalidRequest);
        assert_eq!(req(&[("x-amz-checksum-crc32", crc.as_str()), ("x-amz-trailer", "x-amz-checksum-crc32")]), bad, "header and trailer");
        assert_eq!(req(&[("x-amz-trailer", "x-amz-checksum-md5")]), bad);
        assert_eq!(req(&[("x-amz-trailer", "x-amz-checksum-crc32, x-amz-checksum-sha1")]), bad);
        assert_eq!(req(&[("x-amz-sdk-checksum-algorithm", "md5")]), bad);
        assert_eq!(req(&[("x-amz-checksum-sha256", crc.as_str())]), bad, "wrong length for the algorithm");
        assert_eq!(req(&[("x-amz-checksum-crc32", crc.as_str()), ("x-amz-sdk-checksum-algorithm", "SHA256")]), Ok(Some((Algorithm::Crc32, Some(crc.clone())))), "a value beats the hint");
        assert_eq!(req(&[("x-amz-trailer", " x-amz-checksum-crc32c ,")]), Ok(Some((Algorithm::Crc32c, None))));
        assert_eq!(req(&[("x-amz-trailer", "")]), Ok(None));
        assert_eq!(req(&[("x-amz-checksum-crc32", format!(" {crc} ").as_str())]), Ok(Some((Algorithm::Crc32, Some(crc.clone())))));
    }

    #[test]
    fn names_lengths_and_edges() {
        for a in ALGORITHMS {
            assert_eq!(Algorithm::parse(&format!(" {} ", a.as_str().to_lowercase())), Some(a));
            assert_eq!(Algorithm::from_header(&a.header().to_uppercase()), Some(a));
            assert_eq!(Algorithm::from_xml_element(a.xml_element()), Some(a));
            let raw = a.checksum(b"data");
            assert_eq!(raw.len(), a.digest_len());
            assert_eq!(a.decode(&b64(&raw)).unwrap(), raw);
            assert_eq!(a.decode(&b64(&[0u8; 3])).unwrap_err().code, Code::InvalidRequest);
            assert_eq!(a.decode("!!").unwrap_err().code, Code::InvalidRequest);
            assert_eq!(a.is_crc(), crc_from_bytes(a, &raw).is_some());
            for t in [ChecksumType::FullObject, ChecksumType::Composite] {
                assert_eq!(ChecksumType::parse(&t.as_str().to_lowercase()), Some(t));
                assert_eq!(a.supports(t), if t == ChecksumType::FullObject { a.is_crc() } else { a != Algorithm::Crc64Nvme }, "{a:?} {t:?}");
            }
            assert!(a.supports(a.default_multipart_type()));
        }
        assert_eq!(Algorithm::parse("md5"), None);
        assert_eq!(Algorithm::from_xml_element("checksumcrc32"), None);
        assert_eq!(ChecksumType::parse("PARTIAL"), None);
        assert_eq!(crc_from_bytes(Algorithm::Crc32, &[0; 8]), None);
        assert_eq!(crc_from_bytes(Algorithm::Crc64Nvme, &[0; 4]), None);
        assert_eq!(combine_crc(Algorithm::Crc32, &[]).map(|c| crc_bytes(Algorithm::Crc32, c)), Some(Algorithm::Crc32.checksum(b"")));
        let one = u32::from_be_bytes(Algorithm::Crc32c.checksum(b"abc").try_into().unwrap()) as u64;
        assert_eq!(combine_crc(Algorithm::Crc32c, &[(one, 3)]), Some(one));
        assert_eq!(multipart_etag_from_etags(&["abcd"]).unwrap_err().code, Code::InvalidPart);
        assert_eq!(multipart_etag_from_etags::<&str>(&[]).unwrap(), format!("{}-0", hex::encode(md5(b""))));
        // A body hashed without a checksum or SHA-256 can't verify one.
        let mut h = BodyHasher::new(None, false);
        assert!(h.is_empty());
        h.update(b"abc");
        assert_eq!(h.len(), 3);
        let d = h.finish();
        assert_eq!((d.checksum_b64(), d.crc, d.sha256), (None, None, None));
        assert_eq!(d.etag(), hex::encode(md5(b"abc")));
        assert_eq!(d.verify_checksum(Algorithm::Crc32, &b64(&Algorithm::Crc32.checksum(b"abc"))).unwrap_err().code, Code::InternalError);
        assert_eq!(d.verify_sha256(&sha256_hex(b"abc")).unwrap_err().code, Code::InternalError);
        d.verify_content_md5(None).unwrap();
        assert_eq!(d.verify_content_md5(Some(&hex::encode(md5(b"abc")))).unwrap_err().code, Code::InvalidDigest, "hex is not base64 of 16 bytes");
        assert_eq!(d.verify_content_md5(Some(&b64(&[0u8; 15]))).unwrap_err().code, Code::InvalidDigest);
        let d = BodyHasher::new(Some(Algorithm::Sha1), false).finish();
        assert_eq!(d.verify_checksum(Algorithm::Crc32, &b64(&[0; 4])).unwrap_err().code, Code::InternalError, "a different algorithm was computed");
        assert_eq!(d.crc, None);
        assert_eq!(b64_decode(" AAAA "), Some(vec![0, 0, 0]));
        assert_eq!(b64_decode("@"), None);
    }
}
