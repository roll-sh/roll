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
use base64::Engine;

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

/// A malformed `Authorization` header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Malformed;

pub fn parse_authorization(header: Option<&str>) -> Result<Presented, Malformed> {
    let Some(h) = header.map(str::trim).filter(|h| !h.is_empty()) else { return Ok(Presented::None) };
    let (scheme, rest) = h.split_once(' ').unwrap_or((h, ""));
    let rest = rest.trim();
    let decode = |s: &str| -> Result<String, Malformed> {
        let bytes = base64::engine::general_purpose::STANDARD.decode(s).map_err(|_| Malformed)?;
        String::from_utf8(bytes).map_err(|_| Malformed)
    };
    if scheme.eq_ignore_ascii_case("basic") {
        let text = decode(rest)?;
        let (user, password) = text.split_once(':').ok_or(Malformed)?;
        return Ok(Presented::Basic { user: user.to_string(), password: password.to_string() });
    }
    if scheme.eq_ignore_ascii_case("apikey") {
        let text = decode(rest)?;
        let (id, secret) = text.split_once(':').ok_or(Malformed)?;
        return Ok(Presented::ApiKey { id: id.to_string(), secret: secret.to_string() });
    }
    if scheme.eq_ignore_ascii_case("bearer") {
        return Ok(Presented::Bearer(rest.to_string()));
    }
    Err(Malformed)
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

/// Whether `claim` proves access to `spec`. A spec without credentials is open.
pub fn verify(spec: &ClusterSpec, claim: &Claim) -> bool {
    if spec.credentials.is_empty() {
        return true;
    }
    let Some(secret) = &claim.secret else { return false };
    let mut ok = false;
    for Credential { user, api_key_id, secret: expected } in &spec.credentials {
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
        ok |= user_ok & key_ok & ct_eq(expected.as_bytes(), secret.as_bytes());
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(parse_authorization(Some(&format!("ApiKey {}", b64("c1:s")))), Ok(Presented::ApiKey { id: "c1".into(), secret: "s".into() }));
        assert_eq!(parse_authorization(Some("Bearer c1_abc")), Ok(Presented::Bearer("c1_abc".into())));
        assert_eq!(parse_authorization(Some("Basic !!!")), Err(Malformed));
        assert_eq!(parse_authorization(Some("Digest x")), Err(Malformed));
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
        bound.credentials.push(Credential { user: Some("bob".into()), api_key_id: None, secret: "pw".into() });
        assert!(!verify(&bound, &ok));
        assert!(verify(&bound, &Claim { user: Some("bob".into()), ..ok }));
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"abc", b"ab"));
    }
}
