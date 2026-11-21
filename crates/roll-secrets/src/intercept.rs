//! The interception CA (docs/secrets.md, "Using a key without seeing it: the proxy's contract"): a
//! short-lived certificate authority per consumer, so roll's egress proxy can terminate TLS for the
//! hosts that consumer's keys are bound to, and swap placeholders in the requests it sees there.
//!
//! - **Self-signed per consumer**, not signed by a project's CA: the consumer's container trusts
//!   exactly this one certificate, and its own name constraints are all it can vouch for. A
//!   project root would make the container trust everything the root signs (every consumer's hosts),
//!   or trust an intermediate as an anchor, which not every TLS stack accepts. Nothing is sealed
//!   or stored either: the key is made in memory, handed to the proxy and gone with it.
//! - **Name-constrained, critical**: DNS names only for the bound hosts (`api.stripe.com`, and
//!   `.api.stripe.com` excluded so its subdomains aren't), a wildcard binding as its subtree
//!   (`*.stripe.com` permits `.stripe.com`), every IP address excluded, and email only at the
//!   reserved `invalid` domain. A stolen key mints nothing a client trusting it would accept for
//!   another name.
//! - **Hosts come from the bindings**, never from the proxy: [`BoundHosts`] answers, per project, the
//!   hosts of the keys the consumer holds live placeholders for. Placeholders' branch implements it
//!   over its `placeholders` and `key_hosts` tables, and its `end_consumer` calls
//!   [`Secrets::end_intercept_cas`].
//! - **Short-lived**: [`DEFAULT_TTL`] and at most [`MAX_TTL`]. Each call mints a new key and
//!   certificate; one ends at its expiry or with its consumer (a revocation row), and the proxy
//!   asks [`Secrets::intercept_ca_live`] before each connection it terminates, never caching it.
//! - Audited as `secrets.intercept_ca` (consumer, that project's hosts, expiry) in each project the
//!   hosts come from, and `secrets.intercept_ca.end`; never the key.

use crate::access::Caller;
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS intercept_cas (
    id TEXT PRIMARY KEY,
    consumer TEXT NOT NULL,
    serial TEXT NOT NULL,
    hosts TEXT NOT NULL,
    projects TEXT NOT NULL,
    issued_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER,
    revoked_why TEXT
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS intercept_cas_consumer ON intercept_cas (consumer, revoked_at);
CREATE INDEX IF NOT EXISTS intercept_cas_expiry ON intercept_cas (expires_at);
";

/// How long a CA lives unless the proxy asks for less, and at most.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 3600);
pub const MAX_TTL: Duration = DEFAULT_TTL;
/// The shortest a CA lives: a TTL under it is taken as this.
pub const MIN_TTL: Duration = Duration::from_secs(5 * 60);
/// The most hosts one CA names (a consumer's bindings, across its projects).
const MAX_HOSTS: usize = 256;

/// Where an interception CA's hosts come from: by project, the hosts of the keys `consumer` holds
/// live placeholders for, as bound (`api.stripe.com`, or `*.stripe.com` for a wildcard binding).
/// The proxy never chooses them. rolld sets it once ([`Secrets::set_bound_hosts`]); placeholders'
/// branch answers it from its own tables.
pub trait BoundHosts: Send + Sync {
    fn bound_hosts(&self, consumer: &str) -> Result<BTreeMap<String, BTreeSet<String>>>;
}

/// A consumer's interception CA. `cert_pem` is what the consumer's container trusts; `key` stays
/// in the proxy's memory, redacts itself and is wiped on drop. Never sent over HTTP.
#[derive(Clone, Debug)]
pub struct InterceptCa {
    /// Its record (`ica_…`), for [`Secrets::intercept_ca_live`].
    pub id: String,
    pub consumer: String,
    pub cert_pem: String,
    pub key: Value,
    /// The hosts it permits, as bound.
    pub hosts: Vec<String>,
    /// Unix ms.
    pub expires_at: i64,
}

/// A certificate for one host, signed by an [`InterceptCa`], for the proxy's side of the TLS it
/// terminates. `key` redacts itself.
#[derive(Clone, Debug)]
pub struct Leaf {
    pub cert_pem: String,
    pub key: Value,
}

impl InterceptCa {
    /// Whether the proxy terminates TLS for `host` (as the client asked for it: SNI or `CONNECT`;
    /// port and final dot allowed): a bound host, or one label under a wildcard binding. Anything
    /// else is tunnelled untouched.
    pub fn covers(&self, host: &str) -> bool {
        normalize(host).is_some_and(|h| self.hosts.iter().any(|p| matches(p, &h)))
    }

    /// A certificate for `host`, which the CA [`covers`](InterceptCa::covers), living as long as
    /// the CA.
    pub fn leaf(&self, host: &str) -> Result<Leaf> {
        let host = normalize(host).filter(|h| self.hosts.iter().any(|p| matches(p, h))).ok_or_else(|| Error::BadRequest(format!("{} isn't one of this CA's hosts", shown(host))))?;
        let ca_key = rcgen::KeyPair::from_pem(self.key.expose()).map_err(|e| Error::Internal(format!("the interception CA's key: {e}")))?;
        let issuer = rcgen::Issuer::from_ca_cert_pem(&self.cert_pem, &ca_key).map_err(|e| Error::Internal(format!("the interception CA's certificate: {e}")))?;
        let mut params = rcgen::CertificateParams::new(vec![host.clone()]).map_err(|e| Error::Internal(format!("a leaf's names: {e}")))?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(rcgen::DnType::CommonName, host);
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(1);
        params.not_after = at(self.expires_at)?;
        params.serial_number = Some(rcgen::SerialNumber::from_slice(&crate::pki::serial()));
        params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let key = rcgen::KeyPair::generate().map_err(|e| Error::Internal(format!("a leaf's key: {e}")))?;
        let cert = params.signed_by(&key, &issuer).map_err(|e| Error::Internal(format!("signing a leaf: {e}")))?;
        Ok(Leaf { cert_pem: cert.pem(), key: Value::new(key.serialize_pem()) })
    }
}

fn at(ms: i64) -> Result<time::OffsetDateTime> {
    time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)).map_err(|_| Error::Internal("a time out of range".into()))
}

fn label(l: &str) -> bool {
    (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !l.starts_with('-') && !l.ends_with('-')
}

/// A bound host as a CA may name it: a hostname of two labels or more, or `*.` and one with
/// three or more that isn't a public suffix. Never `*`, `*.com`, an address or a port.
fn checked(h: &str) -> std::result::Result<(), String> {
    let (wild, name) = match h.strip_prefix("*.") {
        Some(n) => (true, n),
        None => (false, h),
    };
    let labels: Vec<&str> = name.split('.').collect();
    if h.len() > 253 || labels.len() < 2 || !labels.iter().all(|l| label(l)) {
        return Err(format!("{} isn't a hostname of two labels or more, or *. and one", shown(h)));
    }
    if labels.last().is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit())) {
        return Err(format!("{h} is an address"));
    }
    // TODO: placeholders' branch has the Public Suffix List check; use it once it's on main.
    if wild && crate::ssh::PUBLIC_SUFFIXES.contains(&name) {
        return Err(format!("{h} covers a public suffix"));
    }
    Ok(())
}

/// A host as the client named it: lower case, no port, no final dot; `None` for an address or
/// anything that isn't a name.
fn normalize(h: &str) -> Option<String> {
    let mut h = h.trim().to_ascii_lowercase();
    if let Some((name, port)) = h.rsplit_once(':')
        && !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && !name.contains(':')
    {
        h.truncate(name.len());
    }
    let h = h.strip_suffix('.').unwrap_or(&h);
    (h.split('.').count() >= 2 && h.split('.').all(label) && !h.rsplit('.').next().is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()))).then(|| h.to_string())
}

/// A bound host matches `host` exactly, or a wildcard one label over its base.
fn matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(base) => host.split_once('.').is_some_and(|(first, rest)| !first.is_empty() && rest == base),
        None => pattern == host,
    }
}

fn shown(h: &str) -> String {
    h.chars().filter(|c| !c.is_control()).take(255).collect()
}

/// The CA's name constraints for `hosts` (checked). X.509 can't say "one label" or "this name
/// only", so: an exact host is permitted with its subdomains excluded unless another binding
/// needs them; a wildcard's whole subtree is permitted. [`InterceptCa::leaf`] signs exact names.
pub(crate) fn constraints(hosts: &[String]) -> rcgen::NameConstraints {
    use rcgen::{CidrSubnet, GeneralSubtree};
    let base = |h: &str| h.strip_prefix("*.").unwrap_or(h).to_string();
    let mut permitted = vec![];
    let mut excluded = vec![GeneralSubtree::IpAddress(CidrSubnet::V4([0; 4], [0; 4])), GeneralSubtree::IpAddress(CidrSubnet::V6([0; 16], [0; 16]))];
    for h in hosts {
        if let Some(b) = h.strip_prefix("*.") {
            permitted.push(GeneralSubtree::DnsName(format!(".{b}")));
            continue;
        }
        permitted.push(GeneralSubtree::DnsName(h.clone()));
        let below = format!(".{h}");
        if !hosts.iter().any(|o| o != h && (base(o) == *h || base(o).ends_with(&below))) {
            excluded.push(GeneralSubtree::DnsName(below));
        }
    }
    // Email: only at the reserved `invalid` domain (RFC 6761), so none. Directory names aren't
    // constrained: webpki refuses every certificate under a CA that does.
    permitted.push(GeneralSubtree::Rfc822Name("invalid".into()));
    rcgen::NameConstraints { permitted_subtrees: permitted, excluded_subtrees: excluded }
}

/// A self-signed CA for `hosts`, ending at `not_after`: its certificate (PEM), key and serial.
pub(crate) fn mint(id: &str, hosts: &[String], not_after: time::OffsetDateTime) -> Result<(String, Value, String)> {
    let key = rcgen::KeyPair::generate().map_err(|e| Error::Internal(format!("an interception CA's key: {e}")))?;
    let mut params = rcgen::CertificateParams::default();
    params.distinguished_name.push(rcgen::DnType::CommonName, format!("roll interception CA {id}"));
    params.distinguished_name.push(rcgen::DnType::OrganizationName, "roll");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
    params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign, rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params.name_constraints = Some(constraints(hosts));
    params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(1);
    params.not_after = not_after;
    let serial = crate::pki::serial();
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial));
    let cert = params.self_signed(&key).map_err(|e| Error::Internal(format!("an interception CA's certificate: {e}")))?;
    Ok((cert.pem(), Value::new(key.serialize_pem()), crate::pki::hex(&serial)))
}

fn when(ms: i64) -> String {
    at(ms).map(|t| format!("{}-{:02}-{:02} {:02}:{:02} UTC", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute())).unwrap_or_default()
}

impl Secrets {
    /// Where interception CAs' hosts come from (rolld, once). `false` when one was already set.
    pub fn set_bound_hosts(&self, source: std::sync::Arc<dyn BoundHosts>) -> bool {
        self.bound_hosts.set(source).is_ok()
    }

    /// A new interception CA for `consumer` (as placeholders name it: `apps:service:svc_1
    /// (untrusted)`, `sandbox:sbx_1`), permitting the hosts its live placeholders' keys are bound
    /// to, for `ttl` ([`DEFAULT_TTL`], taken into [`MIN_TTL`]..=[`MAX_TTL`]). In process only.
    pub fn intercept_ca(&self, consumer: &str, ttl: Option<Duration>) -> Result<InterceptCa> {
        if consumer.is_empty() || consumer.len() > 256 || consumer.chars().any(char::is_control) {
            return Err(Error::BadRequest("a consumer is 1 to 256 printable characters".into()));
        }
        let source = self.bound_hosts.get().ok_or_else(|| Error::Unavailable("no host bindings on this server: interception CAs need placeholders".into()))?;
        let mut by_project = source.bound_hosts(consumer)?;
        let mut all = BTreeSet::new();
        for hosts in by_project.values_mut() {
            *hosts = std::mem::take(hosts).into_iter().map(|h| h.trim().to_ascii_lowercase()).collect();
            for h in hosts.iter() {
                checked(h).map_err(|why| Error::BadRequest(format!("a binding can't be intercepted: {why}")))?;
                all.insert(h.clone());
            }
        }
        by_project.retain(|_, hosts| !hosts.is_empty());
        if all.is_empty() {
            return Err(Error::NotFound(format!("{} holds no live placeholder of a key bound to hosts", shown(consumer))));
        }
        if all.len() > MAX_HOSTS {
            return Err(Error::BadRequest(format!("{} hosts: one CA names {MAX_HOSTS} at most", all.len())));
        }
        let hosts: Vec<String> = all.into_iter().collect();
        let ttl = ttl.unwrap_or(DEFAULT_TTL).clamp(MIN_TTL, MAX_TTL);
        let now = self.db.now_ms()?;
        let expires_at = now + ttl.as_millis() as i64;
        let id = names::new_id("ica", 16);
        let (cert_pem, key, serial) = mint(&id, &hosts, at(expires_at)?)?;
        let projects: Vec<&String> = by_project.keys().collect();
        let caller = Caller::consumer(consumer);
        self.db.write(|c| {
            c.execute(
                "INSERT INTO intercept_cas (id, consumer, serial, hosts, projects, issued_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![id, consumer, serial, serde_json::to_string(&hosts).expect("JSON"), serde_json::to_string(&projects).expect("JSON"), now, expires_at],
            )?;
            for (project, hs) in &by_project {
                let hs: Vec<&str> = hs.iter().map(String::as_str).collect();
                let summary = format!("interception CA {id} for {consumer}: {}, until {}", hs.join(", "), when(expires_at));
                audit::record(c, self.keys()?, project, &caller, "secrets.intercept_ca", None, None, &[], &summary, now)?;
            }
            Ok::<_, Error>(())
        })?;
        Ok(InterceptCa { id, consumer: consumer.to_string(), cert_pem, key, hosts, expires_at })
    }

    /// Whether the proxy may still terminate TLS with CA `id`: it exists, hasn't expired, and its
    /// consumer hasn't ended. Asked per connection; never cache it.
    pub fn intercept_ca_live(&self, id: &str) -> Result<bool> {
        let now = self.db.now_ms()?;
        let row: Option<(i64, Option<i64>)> = self.db.read(|c| c.query_row("SELECT expires_at, revoked_at FROM intercept_cas WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional())?;
        Ok(row.is_some_and(|(end, revoked)| revoked.is_none() && end > now))
    }

    /// Ends every live interception CA of `consumer` (its sandbox stopped, its preview torn down),
    /// with a revocation row and `why`; the proxy drops them at its next [`Secrets::intercept_ca_live`].
    /// Placeholders' `end_consumer` calls it. How many ended.
    pub fn end_intercept_cas(&self, consumer: &str, why: &str) -> Result<usize> {
        let now = self.db.now_ms()?;
        let caller = Caller::consumer(consumer);
        self.db.write(|c| {
            let live: Vec<(String, String)> =
                c.query("SELECT id, projects FROM intercept_cas WHERE consumer = ? AND revoked_at IS NULL AND expires_at > ?", params![consumer, now], |r| Ok((r.get(0)?, r.get(1)?)))?;
            for (id, projects) in &live {
                c.execute("UPDATE intercept_cas SET revoked_at = ?, revoked_why = ? WHERE id = ?", params![now, why, id])?;
                for project in serde_json::from_str::<Vec<String>>(projects).unwrap_or_default() {
                    audit::record(c, self.keys()?, &project, &caller, "secrets.intercept_ca.end", None, None, &[], &format!("ended interception CA {id} for {consumer}: {why}"), now)?;
                }
            }
            Ok::<_, Error>(live.len())
        })
    }

    /// Rows a day past their end.
    pub(crate) fn sweep_intercept_cas(&self) -> Result<()> {
        let before = self.db.now_ms()? - 86_400_000;
        self.db.write(|c| Ok::<_, Error>(c.execute("DELETE FROM intercept_cas WHERE expires_at < ?", [before])?))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls_pki_types::{CertificateDer, ServerName, UnixTime};

    fn der(pem: &str) -> Vec<u8> {
        x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap().1.contents
    }

    /// webpki's verdict on a leaf for `names`, signed with the CA's key, as a client trusting
    /// only the CA would give it.
    fn verify(ca: &InterceptCa, names: &[&str], ip: bool) -> std::result::Result<(), webpki::Error> {
        let ca_key = rcgen::KeyPair::from_pem(ca.key.expose()).unwrap();
        let issuer = rcgen::Issuer::from_ca_cert_pem(&ca.cert_pem, &ca_key).unwrap();
        let mut params = rcgen::CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).unwrap();
        if ip {
            params.subject_alt_names.push(rcgen::SanType::IpAddress("10.0.0.1".parse().unwrap()));
        }
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let leaf = params.signed_by(&rcgen::KeyPair::generate().unwrap(), &issuer).unwrap();
        let ca_der = CertificateDer::from(der(&ca.cert_pem));
        let anchors = [webpki::anchor_from_trusted_cert(&ca_der).unwrap()];
        let leaf_der = CertificateDer::from(leaf.der().to_vec());
        let ee = webpki::EndEntityCert::try_from(&leaf_der).unwrap();
        ee.verify_for_usage(webpki::ALL_VERIFICATION_ALGS, &anchors, &[], UnixTime::now(), webpki::KeyUsage::server_auth(), None, None)?;
        ee.verify_is_valid_for_subject_name(&ServerName::try_from(names[0]).unwrap())
    }

    fn ca(hosts: &[&str]) -> InterceptCa {
        let hosts: Vec<String> = hosts.iter().map(|h| h.to_string()).collect();
        let expires_at = (time::OffsetDateTime::now_utc().unix_timestamp() + 3600) * 1000;
        let (cert_pem, key, _) = mint("ica_test", &hosts, at(expires_at).unwrap()).unwrap();
        InterceptCa { id: "ica_test".into(), consumer: "sandbox:sbx_1".into(), cert_pem, key, hosts, expires_at }
    }

    #[test]
    fn a_ca_vouches_only_for_its_bound_hosts_even_with_its_key() {
        let ca = ca(&["api.stripe.com", "*.example.com"]);
        for ok in [&["api.stripe.com"][..], &["files.example.com"], &["api.stripe.com", "files.example.com"]] {
            verify(&ca, ok, false).unwrap_or_else(|e| panic!("{ok:?}: {e}"));
        }
        // Signed with the CA's own key, and still refused: another host, a subdomain of an exact
        // binding, the wildcard's bare base, an address, or one bad name among good ones.
        for bad in [&["evil.com"][..], &["stripe.com"], &["x.api.stripe.com"], &["example.com"], &["api.stripe.com", "evil.com"]] {
            assert!(matches!(verify(&ca, bad, false), Err(webpki::Error::NameConstraintViolation)), "{bad:?}");
        }
        assert!(matches!(verify(&ca, &["api.stripe.com"], true), Err(webpki::Error::NameConstraintViolation)));
        // The constraints are critical.
        let d = der(&ca.cert_pem);
        let (_, cert) = x509_parser::parse_x509_certificate(&d).unwrap();
        assert!(cert.name_constraints().unwrap().unwrap().critical);
        // The proxy's own leaves: only what the CA covers, one label under a wildcard.
        assert!(ca.covers("API.Stripe.com:443") && ca.covers("a.example.com.") && !ca.covers("a.b.example.com") && !ca.covers("example.com") && !ca.covers("1.2.3.4"));
        let leaf = ca.leaf("files.example.com").unwrap();
        let leaf_der = CertificateDer::from(der(&leaf.cert_pem));
        let ca_der = CertificateDer::from(der(&ca.cert_pem));
        let ee = webpki::EndEntityCert::try_from(&leaf_der).unwrap();
        ee.verify_for_usage(webpki::ALL_VERIFICATION_ALGS, &[webpki::anchor_from_trusted_cert(&ca_der).unwrap()], &[], UnixTime::now(), webpki::KeyUsage::server_auth(), None, None).unwrap();
        assert!(ca.leaf("a.b.example.com").is_err() && ca.leaf("evil.com").is_err());
        // The key is never in a Debug print.
        let shown = format!("{ca:?} {leaf:?}");
        assert!(!shown.contains("PRIVATE KEY") && !shown.contains(&ca.key.expose()[30..60]), "{shown}");
    }

    #[test]
    fn a_nested_binding_keeps_its_subdomains_and_wildcards_never_cover_a_suffix() {
        let ca = ca(&["a.example.com", "b.a.example.com", "*.c.example.com", "c.example.com"]);
        for ok in ["a.example.com", "b.a.example.com", "c.example.com", "x.c.example.com"] {
            verify(&ca, &[ok], false).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in ["*", "*.com", "*.co.uk", "10.0.0.1", "*.10.0.0.1", "com", "api.stripe.com:443", "*.*.example.com"] {
            assert!(checked(bad).is_err(), "{bad}");
        }
        assert!(checked("*.stripe.com").is_ok() && checked("api.stripe.com").is_ok());
    }
}
