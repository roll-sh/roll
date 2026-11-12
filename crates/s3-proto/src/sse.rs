//! Server-side encryption request headers, parsed and checked the way S3 does:
//!
//! - SSE-C: `x-amz-server-side-encryption-customer-{algorithm,key,key-MD5}` (and the
//!   `x-amz-copy-source-…` set for a copy's source). The key is AES-256: 32 bytes, base64, with
//!   the base64 MD5 of those bytes.
//! - SSE-S3 and SSE-KMS: `x-amz-server-side-encryption: AES256 | aws:kms | aws:kms:dsse`, with
//!   `-aws-kms-key-id`, `-context` (base64 JSON) and `-bucket-key-enabled`.
//!
//! The key itself is only ever held in memory for the request that sent it.

use crate::error::{Code, S3Error};
use std::fmt;

pub const AES256: &str = "AES256";
pub const KMS: &str = "aws:kms";
pub const KMS_DSSE: &str = "aws:kms:dsse";

const CUSTOMER: &str = "x-amz-server-side-encryption-customer-";
const COPY_SOURCE_CUSTOMER: &str = "x-amz-copy-source-server-side-encryption-customer-";

/// An SSE-C key from a request.
#[derive(Clone, PartialEq, Eq)]
pub struct CustomerKey {
    pub key: [u8; 32],
    /// Base64 MD5 of the key: what S3 echoes and what's stored to recognise it.
    pub key_md5: String,
}

impl fmt::Debug for CustomerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CustomerKey").field("key", &"<redacted>").field("key_md5", &self.key_md5).finish()
    }
}

impl Drop for CustomerKey {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.key);
    }
}

impl CustomerKey {
    pub fn new(key: [u8; 32]) -> Self {
        let key_md5 = crate::checksum::b64(&crate::checksum::md5(&key));
        Self { key, key_md5 }
    }

    /// The key, base64 (as a backend request sends it).
    pub fn key_b64(&self) -> String {
        crate::checksum::b64(&self.key)
    }
}

fn invalid(msg: &str, header: &str) -> S3Error {
    S3Error::msg(Code::InvalidArgument, msg).with("ArgumentName", header.to_string())
}

/// The SSE-C key a request gives, for the object (`copy_source: false`) or a copy's source.
/// `None` if it sends none of the headers.
pub fn customer_key<'a>(get: impl Fn(&str) -> Option<&'a str>, copy_source: bool) -> Result<Option<CustomerKey>, S3Error> {
    let prefix = if copy_source { COPY_SOURCE_CUSTOMER } else { CUSTOMER };
    let (alg_h, key_h, md5_h) = (format!("{prefix}algorithm"), format!("{prefix}key"), format!("{prefix}key-MD5"));
    let (alg, key, md5) = (get(&alg_h).map(str::trim), get(&key_h).map(str::trim), get(&md5_h).map(str::trim));
    if alg.is_none() && key.is_none() && md5.is_none() {
        return Ok(None);
    }
    match alg {
        None => return Err(invalid("Requests specifying Server Side Encryption with Customer provided keys must provide a valid encryption algorithm.", &alg_h)),
        Some(a) if a != AES256 => return Err(invalid("The requested encryption algorithm is not valid, must be AES256.", &alg_h)),
        Some(_) => {}
    }
    let key = key.filter(|k| !k.is_empty()).ok_or_else(|| invalid("Requests specifying Server Side Encryption with Customer provided keys must provide an appropriate secret key.", &key_h))?;
    let raw = crate::checksum::b64_decode(key).ok_or_else(|| invalid("The secret key was invalid for the specified algorithm.", &key_h))?;
    let key: [u8; 32] = raw.as_slice().try_into().map_err(|_| invalid("The secret key was invalid for the specified algorithm.", &key_h))?;
    let ck = CustomerKey::new(key);
    let md5 = md5.filter(|m| !m.is_empty()).ok_or_else(|| invalid("Requests specifying Server Side Encryption with Customer provided keys must provide the client calculated MD5 of the secret key.", &md5_h))?;
    let sent = crate::checksum::b64_decode(md5).filter(|m| m.len() == 16).ok_or_else(|| invalid("The calculated MD5 hash of the key did not match the hash that was provided.", &md5_h))?;
    if sent != crate::checksum::md5(&ck.key) {
        return Err(invalid("The calculated MD5 hash of the key did not match the hash that was provided.", &md5_h));
    }
    Ok(Some(ck))
}

/// SSE-S3 or SSE-KMS as requested by `x-amz-server-side-encryption`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerSide {
    /// `AES256`: keys managed by the storage (the backing bucket's SSE-S3).
    S3,
    /// `aws:kms` (`dsse: false`) or `aws:kms:dsse`.
    Kms {
        dsse: bool,
        key_id: Option<String>,
        /// Base64 JSON, as sent.
        context: Option<String>,
        bucket_key: Option<bool>,
    },
}

impl ServerSide {
    /// The `x-amz-server-side-encryption` value.
    pub fn algorithm(&self) -> &'static str {
        match self {
            Self::S3 => AES256,
            Self::Kms { dsse: false, .. } => KMS,
            Self::Kms { dsse: true, .. } => KMS_DSSE,
        }
    }
}

/// The largest encryption context accepted (S3's limit on the header).
pub const MAX_CONTEXT_BYTES: usize = 8 << 10;

/// `x-amz-server-side-encryption` and the KMS headers; `None` if none are sent.
pub fn server_side<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Option<ServerSide>, S3Error> {
    const ALG: &str = "x-amz-server-side-encryption";
    const KEY: &str = "x-amz-server-side-encryption-aws-kms-key-id";
    const CTX: &str = "x-amz-server-side-encryption-context";
    const BK: &str = "x-amz-server-side-encryption-bucket-key-enabled";
    let key_id = get(KEY).map(str::trim).map(str::to_string);
    let context = get(CTX).map(str::trim).map(str::to_string);
    let bucket_key = match get(BK).map(str::trim) {
        None => None,
        Some(v) if v.eq_ignore_ascii_case("true") => Some(true),
        Some(v) if v.eq_ignore_ascii_case("false") => Some(false),
        Some(_) => return Err(invalid("x-amz-server-side-encryption-bucket-key-enabled must be true or false.", BK)),
    };
    let Some(alg) = get(ALG).map(str::trim) else {
        if key_id.is_some() || context.is_some() {
            return Err(invalid("Server Side Encryption with AWS KMS managed key requires HTTP header x-amz-server-side-encryption : aws:kms", ALG));
        }
        return Ok(None);
    };
    let dsse = match alg {
        AES256 => {
            if key_id.is_some() || context.is_some() {
                return Err(invalid("Server Side Encryption with AWS KMS managed key requires HTTP header x-amz-server-side-encryption : aws:kms", ALG));
            }
            return Ok(Some(ServerSide::S3));
        }
        KMS => false,
        KMS_DSSE => true,
        _ => return Err(invalid("The encryption method specified is not supported", ALG)),
    };
    if key_id.as_deref() == Some("") {
        return Err(invalid("x-amz-server-side-encryption-aws-kms-key-id must not be empty.", KEY));
    }
    if let Some(c) = &context {
        let ok = c.len() <= MAX_CONTEXT_BYTES
            && crate::checksum::b64_decode(c).and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok()).is_some_and(|v| v.as_object().is_some_and(|m| m.values().all(|x| x.is_string())));
        if !ok {
            return Err(invalid("The encryption context must be base64-encoded JSON of string key-value pairs.", CTX));
        }
    }
    Ok(Some(ServerSide::Kms { dsse, key_id, context, bucket_key }))
}

/// What a write asks for: an SSE-C key, or SSE-S3/SSE-KMS (both is an error).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requested {
    pub customer: Option<CustomerKey>,
    pub server_side: Option<ServerSide>,
}

impl Requested {
    pub fn is_none(&self) -> bool {
        self.customer.is_none() && self.server_side.is_none()
    }
}

/// The encryption headers of a write (PUT, POST, CreateMultipartUpload, a copy's destination).
pub fn requested<'a>(get: impl Fn(&str) -> Option<&'a str> + Copy) -> Result<Requested, S3Error> {
    let customer = customer_key(get, false)?;
    let server_side = server_side(get)?;
    if customer.is_some() && server_side.is_some() {
        return Err(invalid("Server Side Encryption with Customer provided key is incompatible with the encryption method specified", "x-amz-server-side-encryption"));
    }
    Ok(Requested { customer, server_side })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checksum::{b64, md5};

    fn headers<'a>(h: &'a [(&'a str, String)]) -> impl Fn(&str) -> Option<&'a str> + Copy + 'a {
        move |n: &str| h.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.as_str())
    }

    fn sse_c(prefix: &str, key: &[u8]) -> Vec<(&'static str, String)> {
        let p: &'static str = if prefix.is_empty() { CUSTOMER } else { COPY_SOURCE_CUSTOMER };
        vec![
            (Box::leak(format!("{p}algorithm").into_boxed_str()), "AES256".into()),
            (Box::leak(format!("{p}key").into_boxed_str()), b64(key)),
            (Box::leak(format!("{p}key-MD5").into_boxed_str()), b64(&md5(key))),
        ]
    }

    #[test]
    fn customer_keys() {
        let key = [7u8; 32];
        let h = sse_c("", &key);
        let ck = customer_key(headers(&h), false).unwrap().unwrap();
        assert_eq!(ck.key, key);
        assert_eq!(ck.key_md5, b64(&md5(&key)));
        assert_eq!(ck.key_b64(), b64(&key));
        assert!(!format!("{ck:?}").contains(&b64(&key)), "the key never shows in logs");
        assert_eq!(customer_key(headers(&h), true).unwrap(), None, "the copy source's set is separate");
        let src = sse_c("copy", &key);
        assert_eq!(customer_key(headers(&src), true).unwrap().unwrap(), ck);
        assert_eq!(customer_key(headers(&[]), false).unwrap(), None);
        let err = |h: Vec<(&'static str, String)>| customer_key(headers(&h), false).unwrap_err();
        let mut no_alg = h.clone();
        no_alg.remove(0);
        assert!(err(no_alg).message.contains("valid encryption algorithm"));
        let mut bad_alg = h.clone();
        bad_alg[0].1 = "AES128".into();
        assert_eq!(err(bad_alg).code, Code::InvalidArgument);
        let mut no_key = h.clone();
        no_key.remove(1);
        assert!(err(no_key).message.contains("appropriate secret key"));
        let mut short = h.clone();
        short[1].1 = b64(&[1u8; 16]);
        assert!(err(short).message.contains("secret key was invalid"));
        let mut junk = h.clone();
        junk[1].1 = "!!!".into();
        assert!(err(junk).message.contains("secret key was invalid"));
        let mut no_md5 = h.clone();
        no_md5.remove(2);
        assert!(err(no_md5).message.contains("client calculated MD5"));
        let mut wrong_md5 = h.clone();
        wrong_md5[2].1 = b64(&md5(&[8u8; 32]));
        let e = err(wrong_md5);
        assert_eq!(e.code, Code::InvalidArgument);
        assert!(e.message.contains("did not match"));
    }

    #[test]
    fn server_side_headers() {
        let get = |h: &[(&'static str, &'static str)]| {
            let owned: Vec<(&'static str, String)> = h.iter().map(|(k, v)| (*k, v.to_string())).collect();
            server_side(headers(Box::leak(owned.into_boxed_slice()))).map_err(|e| e.code)
        };
        assert_eq!(get(&[]), Ok(None));
        assert_eq!(get(&[("x-amz-server-side-encryption", "AES256")]), Ok(Some(ServerSide::S3)));
        assert_eq!(get(&[("x-amz-server-side-encryption", "aws:kms")]), Ok(Some(ServerSide::Kms { dsse: false, key_id: None, context: None, bucket_key: None })));
        let ctx = b64(br#"{"project":"roll"}"#);
        let ctx: &'static str = Box::leak(ctx.into_boxed_str());
        assert_eq!(
            get(&[("x-amz-server-side-encryption", "aws:kms:dsse"), ("x-amz-server-side-encryption-aws-kms-key-id", "arn:k"), ("x-amz-server-side-encryption-context", ctx), ("x-amz-server-side-encryption-bucket-key-enabled", "TRUE")]),
            Ok(Some(ServerSide::Kms { dsse: true, key_id: Some("arn:k".into()), context: Some(ctx.into()), bucket_key: Some(true) }))
        );
        let bad = Err(Code::InvalidArgument);
        assert_eq!(get(&[("x-amz-server-side-encryption", "aws:kms:other")]), bad);
        assert_eq!(get(&[("x-amz-server-side-encryption-aws-kms-key-id", "k")]), bad, "a key id needs aws:kms");
        assert_eq!(get(&[("x-amz-server-side-encryption", "AES256"), ("x-amz-server-side-encryption-aws-kms-key-id", "k")]), bad);
        assert_eq!(get(&[("x-amz-server-side-encryption", "aws:kms"), ("x-amz-server-side-encryption-context", "bm90IGpzb24=")]), bad, "not JSON");
        assert_eq!(get(&[("x-amz-server-side-encryption", "aws:kms"), ("x-amz-server-side-encryption-context", "W10=")]), bad, "not an object");
        assert_eq!(get(&[("x-amz-server-side-encryption", "aws:kms"), ("x-amz-server-side-encryption-bucket-key-enabled", "yes")]), bad);
        assert_eq!(get(&[("x-amz-server-side-encryption", "aws:kms"), ("x-amz-server-side-encryption-aws-kms-key-id", "")]), bad);
        assert_eq!(ServerSide::Kms { dsse: true, key_id: None, context: None, bucket_key: None }.algorithm(), "aws:kms:dsse");
    }

    #[test]
    fn sse_c_and_server_side_together_is_an_error() {
        let mut h = sse_c("", &[1u8; 32]);
        assert!(requested(headers(&h)).unwrap().customer.is_some());
        h.push(("x-amz-server-side-encryption", "AES256".into()));
        assert_eq!(requested(headers(&h)).unwrap_err().code, Code::InvalidArgument);
        assert!(requested(headers(&[])).unwrap().is_none());
    }
}
