//! Finding a request's cluster and checking its credentials.
//!
//! A request names its cluster, in order of precedence, by:
//!
//! 1. Basic auth: user `<cluster>` (with the cluster's password) or `<user>@<cluster>`;
//! 2. `Authorization: ApiKey base64(<id>:<secret>)` where the id is the cluster (or
//!    `<key id>@<cluster>`);
//! 3. `Authorization: Bearer <cluster>_<secret>` (split at the last `_`);
//! 4. the TLS SNI or `Host` label under the node's domain (`<cluster>.search.example.com`),
//!    with the secret from Basic (any user name) or Bearer.
//!
//! Secrets are compared in constant time, and an unknown cluster fails exactly like a wrong
//! password so cluster names can't be probed.

use crate::catalog::{ClusterSpec, Credential};

/// Credentials as presented in the `Authorization` header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presented {
    None,
    Basic { user: String, password: String },
    ApiKey { id: String, secret: String },
    Bearer(String),
}

impl Presented {
    /// The principal Elasticsearch names in authentication errors.
    pub fn principal(&self) -> Option<&str> {
        match self {
            Presented::Basic { user, .. } => Some(user),
            Presented::ApiKey { id, .. } => Some(id),
            _ => None,
        }
    }
}

/// A malformed `Authorization` header, with what Elasticsearch answers for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// `Basic` with bad base64: "invalid basic authentication header encoding", caused by
    /// Java's `Base64` error text.
    BasicEncoding(String),
    /// `Basic` whose decoded value has no `:`: "invalid basic authentication header value".
    BasicValue,
    /// `ApiKey` that doesn't decode to `<id>:<secret>`: the message of
    /// `additional_unsuccessful_credentials` ("API key: <message>").
    ApiKey(String),
}

fn strip_prefix_ci<'a>(h: &'a str, prefix: &str) -> Option<&'a str> {
    (h.len() >= prefix.len() && h.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())).then(|| &h[prefix.len()..])
}

/// Java's `String.trim()`: strips chars `<= ' '`.
fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

/// `java.util.Base64.getDecoder().decode(..)` with its error texts (Elasticsearch passes them on).
pub fn java_base64(src: &[u8]) -> Result<Vec<u8>, String> {
    let len = src.len();
    if len == 0 {
        return Ok(Vec::new());
    }
    if len < 2 {
        return Err("Input byte[] should at least have 2 bytes for base64 bytes".into());
    }
    fn val(b: u8) -> i32 {
        match b {
            b'A'..=b'Z' => (b - b'A') as i32,
            b'a'..=b'z' => (b - b'a') as i32 + 26,
            b'0'..=b'9' => (b - b'0') as i32 + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => -2,
            _ => -1,
        }
    }
    let mut out = Vec::with_capacity(len / 4 * 3 + 2);
    let (mut bits, mut shift, mut sp) = (0i32, 18i32, 0usize);
    while sp < len {
        let raw = src[sp];
        sp += 1;
        let b = val(raw);
        if b < 0 {
            if b == -2 {
                if (shift == 6 && (sp == len || {
                    let next = src[sp];
                    sp += 1;
                    next != b'='
                })) || shift == 18
                {
                    return Err("Input byte array has wrong 4-byte ending unit".into());
                }
                break;
            }
            // Integer.toString(byte, 16): signed, e.g. "-3d".
            let signed = raw as i8 as i32;
            let hex = if signed < 0 { format!("-{:x}", -signed) } else { format!("{signed:x}") };
            return Err(format!("Illegal base64 character {hex}"));
        }
        bits |= b << shift;
        shift -= 6;
        if shift < 0 {
            out.extend_from_slice(&[(bits >> 16) as u8, (bits >> 8) as u8, bits as u8]);
            shift = 18;
            bits = 0;
        }
    }
    match shift {
        6 => out.push((bits >> 16) as u8),
        0 => out.extend_from_slice(&[(bits >> 16) as u8, (bits >> 8) as u8]),
        12 => return Err("Last unit does not have enough valid bits".into()),
        _ => {}
    }
    if sp < len {
        return Err(format!("Input byte array has incorrect ending byte at {sp}"));
    }
    Ok(out)
}

/// Parses `Authorization` like Elasticsearch's realms: `Basic `, `ApiKey ` and `Bearer `
/// (case-insensitive, one space, value trimmed); an empty value or another scheme presents
/// nothing.
pub fn parse_authorization(header: Option<&str>) -> Result<Presented, Malformed> {
    let Some(h) = header else { return Ok(Presented::None) };
    if let Some(rest) = strip_prefix_ci(h, "Basic ") {
        let rest = java_trim(rest);
        if rest.is_empty() {
            return Ok(Presented::None);
        }
        let bytes = java_base64(rest.as_bytes()).map_err(Malformed::BasicEncoding)?;
        let text = String::from_utf8_lossy(&bytes);
        let (user, password) = text.split_once(':').ok_or(Malformed::BasicValue)?;
        return Ok(Presented::Basic { user: user.to_string(), password: password.to_string() });
    }
    if let Some(rest) = strip_prefix_ci(h, "ApiKey ") {
        let rest = java_trim(rest);
        if rest.is_empty() {
            return Ok(Presented::None);
        }
        let bytes = java_base64(rest.as_bytes()).map_err(Malformed::ApiKey)?;
        let text = String::from_utf8_lossy(&bytes);
        let (id, secret) = text.split_once(':').filter(|(id, _)| !id.is_empty()).ok_or_else(|| Malformed::ApiKey("invalid ApiKey value".into()))?;
        return Ok(Presented::ApiKey { id: id.to_string(), secret: secret.to_string() });
    }
    if let Some(rest) = strip_prefix_ci(h, "Bearer ") {
        let rest = java_trim(rest);
        return Ok(if rest.is_empty() { Presented::None } else { Presented::Bearer(rest.to_string()) });
    }
    Ok(Presented::None)
}

/// Constant-time byte comparison (the length is not secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The cluster label of a host name under `domain` (`c1.search.example.com` → `c1`).
pub fn host_label<'a>(host: &'a str, domain: Option<&str>) -> Option<&'a str> {
    let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map(|(h, _)| h).unwrap_or(host);
    let domain = domain?;
    let label = host.strip_suffix(domain)?.strip_suffix('.')?;
    (!label.is_empty() && !label.contains('.')).then_some(label)
}

/// Which cluster a request is for and the secret to check (`None` when no secret was given).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub cluster: String,
    /// The Basic user (for credentials bound to a user).
    pub user: Option<String>,
    pub api_key_id: Option<String>,
    pub secret: Option<String>,
}

pub fn claim(p: &Presented, host_cluster: Option<&str>) -> Option<Claim> {
    match p {
        Presented::Basic { user, password } => {
            if let Some((u, c)) = user.rsplit_once('@') {
                return Some(Claim { cluster: c.to_string(), user: Some(u.to_string()), api_key_id: None, secret: Some(password.clone()) });
            }
            match host_cluster {
                Some(c) => Some(Claim { cluster: c.to_string(), user: Some(user.clone()), api_key_id: None, secret: Some(password.clone()) }),
                None => Some(Claim { cluster: user.clone(), user: None, api_key_id: None, secret: Some(password.clone()) }),
            }
        }
        Presented::ApiKey { id, secret } => {
            let (key, cluster) = match id.rsplit_once('@') {
                Some((k, c)) => (Some(k.to_string()), c.to_string()),
                None => (None, host_cluster.map(str::to_string).unwrap_or_else(|| id.clone())),
            };
            let key = key.or_else(|| host_cluster.map(|_| id.clone()));
            Some(Claim { cluster, user: None, api_key_id: key, secret: Some(secret.clone()) })
        }
        Presented::Bearer(token) => {
            if let Some(c) = host_cluster
                && !token.contains('_')
            {
                return Some(Claim { cluster: c.to_string(), user: None, api_key_id: None, secret: Some(token.clone()) });
            }
            let (c, s) = token.rsplit_once('_')?;
            Some(Claim { cluster: c.to_string(), user: None, api_key_id: None, secret: Some(s.to_string()) })
        }
        Presented::None => host_cluster.map(|c| Claim { cluster: c.to_string(), user: None, api_key_id: None, secret: None }),
    }
}

/// The `Authorization` header to authenticate inside the cluster when `claim` doesn't match
/// the cluster's own credentials: the cluster's security may know the user (native or file
/// realm), API key or access token. The cluster qualifier (`<user>@<cluster>`,
/// `<key id>@<cluster>`) is stripped so the cluster sees the credentials Elasticsearch clients
/// send. `None` when the claim can't name such credentials (Basic or ApiKey whose user/id is
/// the cluster itself, a Bearer token that isn't routed by host).
pub fn native_authorization(p: &Presented, claim: &Claim, host_cluster: Option<&str>) -> Option<String> {
    use base64::Engine;
    let b64 = |s: String| base64::engine::general_purpose::STANDARD.encode(s);
    match p {
        Presented::Basic { password, .. } => claim.user.as_ref().map(|u| format!("Basic {}", b64(format!("{u}:{password}")))),
        Presented::ApiKey { secret, .. } => claim.api_key_id.as_ref().map(|k| format!("ApiKey {}", b64(format!("{k}:{secret}")))),
        Presented::Bearer(token) => (host_cluster == Some(claim.cluster.as_str()) && claim.secret.as_ref() == Some(token)).then(|| format!("Bearer {token}")),
        Presented::None => None,
    }
}

/// Whether `claim` proves access to `spec`. A spec without credentials is open.
pub fn verify(spec: &ClusterSpec, claim: &Claim) -> bool {
    if spec.credentials.is_empty() {
        return true;
    }
    let Some(secret) = &claim.secret else { return false };
    let mut ok = false;
    let presented_hash = spec.credentials.iter().any(|c| c.secret_sha256.is_some()).then(|| crate::catalog::secret_hash(secret));
    for Credential { user, api_key_id, secret: expected, secret_sha256 } in &spec.credentials {
        let user_ok = match (user, &claim.user) {
            (Some(u), Some(cu)) => ct_eq(u.as_bytes(), cu.as_bytes()),
            (Some(_), None) => false,
            (None, _) => true,
        };
        let key_ok = match (api_key_id, &claim.api_key_id) {
            (Some(k), Some(ck)) => ct_eq(k.as_bytes(), ck.as_bytes()),
            (Some(_), None) => false,
            (None, _) => true,
        };
        // Evaluate every credential so timing doesn't reveal which one matched.
        let secret_ok = match (secret_sha256, &presented_hash) {
            (Some(h), Some(p)) => ct_eq(h, p),
            _ => !expected.is_empty() && ct_eq(expected.as_bytes(), secret.as_bytes()),
        };
        ok |= user_ok & key_ok & secret_ok;
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    #[test]
    fn headers() {
        assert_eq!(parse_authorization(None), Ok(Presented::None));
        assert_eq!(
            parse_authorization(Some(&format!("Basic {}", b64("c1:pw")))),
            Ok(Presented::Basic { user: "c1".into(), password: "pw".into() })
        );
        assert_eq!(
            parse_authorization(Some(&format!("basic  {}  ", b64("c1:pw")))),
            Ok(Presented::Basic { user: "c1".into(), password: "pw".into() })
        );
        assert_eq!(parse_authorization(Some(&format!("ApiKey {}", b64("c1:s")))), Ok(Presented::ApiKey { id: "c1".into(), secret: "s".into() }));
        assert_eq!(parse_authorization(Some("Bearer c1_abc")), Ok(Presented::Bearer("c1_abc".into())));
        // Elasticsearch 9.5.5's answers (captured with security enabled).
        let enc = |m: &str| Err(Malformed::BasicEncoding(m.into()));
        assert_eq!(parse_authorization(Some("Basic %%%")), enc("Illegal base64 character 25"));
        assert_eq!(parse_authorization(Some("Basic a")), enc("Input byte[] should at least have 2 bytes for base64 bytes"));
        assert_eq!(parse_authorization(Some("Basic YWJj=")), enc("Input byte array has wrong 4-byte ending unit"));
        assert_eq!(parse_authorization(Some("Basic \u{e9}")), enc("Illegal base64 character -3d"));
        assert_eq!(parse_authorization(Some("Basic YQ")), Err(Malformed::BasicValue));
        assert_eq!(parse_authorization(Some(&format!("Basic {}", b64("nocolon")))), Err(Malformed::BasicValue));
        assert_eq!(parse_authorization(Some("ApiKey a")), Err(Malformed::ApiKey("Input byte[] should at least have 2 bytes for base64 bytes".into())));
        assert_eq!(parse_authorization(Some(&format!("ApiKey {}", b64(":x")))), Err(Malformed::ApiKey("invalid ApiKey value".into())));
        assert_eq!(parse_authorization(Some(&format!("ApiKey {}", b64("nocolon")))), Err(Malformed::ApiKey("invalid ApiKey value".into())));
        for none in ["Digest x", "Basic", "Basic ", "Bearer", "Bearer  ", "ApiKey "] {
            assert_eq!(parse_authorization(Some(none)), Ok(Presented::None), "{none}");
        }
    }

    #[test]
    fn java_base64_matches_the_jdk() {
        assert_eq!(java_base64(b"YWJj").unwrap(), b"abc");
        assert_eq!(java_base64(b"YWI=").unwrap(), b"ab");
        assert_eq!(java_base64(b"YWI").unwrap(), b"ab");
        assert_eq!(java_base64(b"YQ==").unwrap(), b"a");
        assert_eq!(java_base64(b"YQ=").unwrap_err(), "Input byte array has wrong 4-byte ending unit");
        assert_eq!(java_base64(b"Y=").unwrap_err(), "Last unit does not have enough valid bits");
        assert_eq!(java_base64(b"YWJjZ").unwrap_err(), "Last unit does not have enough valid bits");
        assert_eq!(java_base64(b"YQ==YQ").unwrap_err(), "Input byte array has incorrect ending byte at 4");
        assert_eq!(java_base64(b"").unwrap(), b"");
    }

    #[test]
    fn native_authorizations() {
        let basic = |u: &str| Presented::Basic { user: u.into(), password: "pw".into() };
        let native = |p: &Presented, host: Option<&str>| native_authorization(p, &claim(p, host).unwrap(), host);
        assert_eq!(native(&basic("bob@c1"), None), Some(format!("Basic {}", b64("bob:pw"))));
        assert_eq!(native(&basic("bob"), Some("c1")), Some(format!("Basic {}", b64("bob:pw"))));
        // The cluster's own name as the user: only the cluster's credentials apply.
        assert_eq!(native(&basic("c1"), None), None);
        let key = |id: &str| Presented::ApiKey { id: id.into(), secret: "s".into() };
        assert_eq!(native(&key("k1@c1"), None), Some(format!("ApiKey {}", b64("k1:s"))));
        assert_eq!(native(&key("k1"), Some("c1")), Some(format!("ApiKey {}", b64("k1:s"))));
        assert_eq!(native(&key("c1"), None), None);
        assert_eq!(native(&Presented::Bearer("dGtu".into()), Some("c1")), Some("Bearer dGtu".into()));
        assert_eq!(native(&Presented::Bearer("c1_s".into()), None), None);
        assert_eq!(native(&Presented::None, Some("c1")), None);
    }

    #[test]
    fn claims() {
        let basic = |u: &str| Presented::Basic { user: u.into(), password: "pw".into() };
        assert_eq!(claim(&basic("c1"), None).unwrap().cluster, "c1");
        let c = claim(&basic("bob@c2"), None).unwrap();
        assert_eq!((c.cluster.as_str(), c.user.as_deref()), ("c2", Some("bob")));
        let c = claim(&basic("elastic"), Some("c3")).unwrap();
        assert_eq!((c.cluster.as_str(), c.user.as_deref()), ("c3", Some("elastic")));
        assert_eq!(claim(&Presented::Bearer("my_cluster_s3cr3t".into()), None).unwrap().cluster, "my_cluster");
        assert_eq!(claim(&Presented::Bearer("nounderscore".into()), None), None);
        assert_eq!(claim(&Presented::None, Some("c4")).unwrap().secret, None);
        assert_eq!(claim(&Presented::None, None), None);
        let c = claim(&Presented::ApiKey { id: "k1@c5".into(), secret: "s".into() }, None).unwrap();
        assert_eq!((c.cluster.as_str(), c.api_key_id.as_deref()), ("c5", Some("k1")));
    }

    #[test]
    fn hosts() {
        assert_eq!(host_label("c1.search.example.com", Some("search.example.com")), Some("c1"));
        assert_eq!(host_label("c1.search.example.com:9243", Some("search.example.com")), Some("c1"));
        assert_eq!(host_label("a.b.search.example.com", Some("search.example.com")), None);
        assert_eq!(host_label("search.example.com", Some("search.example.com")), None);
        assert_eq!(host_label("c1.search.example.com", None), None);
    }

    #[test]
    fn verification() {
        let spec = ClusterSpec::new("c1").with_password("pw");
        let ok = Claim { cluster: "c1".into(), user: None, api_key_id: None, secret: Some("pw".into()) };
        assert!(verify(&spec, &ok));
        assert!(!verify(&spec, &Claim { secret: Some("px".into()), ..ok.clone() }));
        assert!(!verify(&spec, &Claim { secret: None, ..ok.clone() }));
        assert!(verify(&ClusterSpec::new("open"), &Claim { secret: None, ..ok.clone() }));
        let mut bound = ClusterSpec::new("c1");
        bound.credentials.push(Credential::password("pw").for_user("bob"));
        assert!(!verify(&bound, &ok));
        assert!(verify(&bound, &Claim { user: Some("bob".into()), ..ok }));
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"abc", b"ab"));
        // Hashed secrets.
        let mut hashed = ClusterSpec::new("c1");
        hashed.credentials.push(Credential::hashed(crate::catalog::secret_hash("s3cret")));
        let c = Claim { cluster: "c1".into(), user: None, api_key_id: None, secret: Some("s3cret".into()) };
        assert!(verify(&hashed, &c));
        assert!(!verify(&hashed, &Claim { secret: Some("s3cre".into()), ..c.clone() }));
        // An empty hashed credential's clear secret never matches an empty password.
        assert!(!verify(&hashed, &Claim { secret: Some(String::new()), ..c }));
    }
}
