//! AWS Signature Version 4 for Bedrock: ported from `crates/mq-proxy/src/ecs.rs` (`sign`,
//! `amz_date`), with the canonical URI encoded the way services other than S3 want it (every
//! path segment encoded twice: a model id's `:` is `%3A` in the request and `%253A` in what's
//! signed).

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// An access key pair (and a session token for temporary credentials).
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credentials({}, …)", self.access_key)
    }
}

impl Credentials {
    /// `ACCESS_KEY_ID:SECRET_ACCESS_KEY[:SESSION_TOKEN]`, as the owner pastes it. Anything
    /// else (a Bedrock API key) isn't a key pair.
    pub fn parse(key: &str) -> Option<Credentials> {
        let mut parts = key.trim().splitn(3, ':');
        let access_key = parts.next()?;
        let secret_key = parts.next()?;
        let token = parts.next().map(str::to_string);
        let id_like = access_key.len() >= 16 && access_key.len() <= 128 && access_key.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
        if !id_like || secret_key.is_empty() {
            return None;
        }
        Some(Credentials { access_key: access_key.into(), secret_key: secret_key.into(), token })
    }
}

/// `YYYYMMDDTHHMMSSZ`.
pub fn amz_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac takes any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// RFC 3986 encoding as SigV4 wants it: unreserved characters kept, everything else `%XX`.
pub fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The canonical query string: pairs sorted, each side encoded.
fn canonical_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> =
        url::form_urlencoded::parse(query.as_bytes()).map(|(k, v)| (uri_encode(&k), uri_encode(&v))).collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// What gets signed.
pub struct Request<'a> {
    pub method: &'a str,
    /// The path as sent (each segment already encoded once).
    pub path: &'a str,
    pub query: &'a str,
    pub body: &'a [u8],
}

/// Signs a request: returns the `Authorization` value. `headers` (lowercase names) must hold
/// `host` and `x-amz-date`; they're sorted in place, and `x-amz-security-token` is added for
/// temporary credentials before signing.
pub fn sign(creds: &Credentials, region: &str, service: &str, req: &Request<'_>, headers: &mut Vec<(String, String)>) -> String {
    if let Some(t) = &creds.token
        && !headers.iter().any(|(k, _)| k == "x-amz-security-token")
    {
        headers.push(("x-amz-security-token".into(), t.clone()));
    }
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let date_time = headers.iter().find(|(k, _)| k == "x-amz-date").map(|(_, v)| v.clone()).unwrap_or_default();
    let date = &date_time[..8.min(date_time.len())];
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let path = if req.path.is_empty() { "/".to_string() } else { req.path.split('/').map(uri_encode).collect::<Vec<_>>().join("/") };
    let canonical =
        format!("{}\n{path}\n{}\n{canonical_headers}\n{signed}\n{}", req.method, canonical_query(req.query), hex::encode(Sha256::digest(req.body)));
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{date_time}\n{scope}\n{}", hex::encode(Sha256::digest(canonical.as_bytes())));
    let k = hmac(format!("AWS4{}", creds.secret_key).as_bytes(), date);
    let k = hmac(&k, region);
    let k = hmac(&k, service);
    let k = hmac(&k, "aws4_request");
    let signature = hex::encode(hmac(&k, &to_sign));
    format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}", creds.access_key)
}
