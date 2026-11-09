//! Client-certificate authentication.
//!
//! - **Cassandra 5**: `MutualTlsAuthenticator` and `MutualTlsWithPasswordFallbackAuthenticator`
//!   (ports of `MutualTlsAuthenticator.CertificateNegotiator` with `SpiffeCertificateValidator`,
//!   and of the fallback's `newSaslNegotiator`). The identity is the leaf certificate's first
//!   `spiffe://` URI subject alternative name; `identity_to_role` (`ADD IDENTITY`) maps it to
//!   the role, which must be allowed to log in (`ClientState.login`). Both still answer STARTUP
//!   with AUTHENTICATE; the AUTH_RESPONSE token is ignored when a certificate is presented. The
//!   fallback authenticates connections without a certificate with `PasswordAuthenticator`.
//! - **ScyllaDB**: `com.scylladb.auth.CertificateAuthenticator` and
//!   `CertificateOrPasswordAuthenticator`, from ScyllaDB's documentation
//!   (`operating-scylla/security/certificate-authentication.rst`,
//!   `certificate-or-password-authentication.rst`, `alternator/alternator.md`): the role is the
//!   first capture of the first `auth_certificate_role_queries` regular expression that matches
//!   the certificate's subject DN (`SUBJECT`, as `CN=alice,O=MyOrg`) or its subject alternative
//!   names (`ALTNAME`, as `EMAIL=alice@example.com,DNS=host.example.com`); a connection with a
//!   certificate is ready without any SASL exchange. The reference images run without TLS, so
//!   ScyllaDB's error texts here are not verified (see each message).
//!
//! The server verifies the chain during the TLS handshake (its client CA) and puts what the
//! authenticators need of the leaf in [`ClientInfo::certificate`](crate::ClientInfo).
//! Cassandra's identity cache (`IdentitiesCache`, `credentials_validity`) isn't modelled: a
//! change to `identity_to_role` applies to the next connection at once.

use std::sync::Arc;

use nosql_types::Dialect;

use crate::Database;
use crate::error::{Error, ErrorCode, Result};
use crate::session::{AuthStep, SessionState};

/// What the authenticators read of a verified client certificate (the chain's leaf).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientCertificate {
    /// The subject distinguished name in RFC 4514 form, most specific first (`CN=alice,O=MyOrg`).
    pub subject: String,
    /// Subject alternative names as `(type, value)`: `EMAIL`, `DNS`, `URI`, `IP`.
    pub alt_names: Vec<(String, String)>,
}

impl ClientCertificate {
    /// ScyllaDB's `ALTNAME` text: `TYPE=value` entries joined by commas.
    pub fn alt_names_text(&self) -> String {
        self.alt_names
            .iter()
            .map(|(t, v)| format!("{t}={v}"))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// `SpiffeCertificateValidator.getSANSpiffe`: the first URI name starting with `spiffe://`.
    pub fn spiffe(&self) -> Option<&str> {
        self.alt_names
            .iter()
            .find(|(t, v)| t == "URI" && v.starts_with("spiffe://"))
            .map(|(_, v)| v.as_str())
    }
}

/// Where an `auth_certificate_role_queries` entry looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The subject distinguished name.
    Subject,
    /// The subject alternative names.
    AltName,
}

/// One `auth_certificate_role_queries` entry: a regular expression whose first capture group is
/// the role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleQuery {
    pub source: Source,
    pub query: String,
}

impl RoleQuery {
    /// ScyllaDB's default: the subject's common name.
    pub fn defaults() -> Vec<RoleQuery> {
        vec![RoleQuery {
            source: Source::Subject,
            query: "CN=([^,]+)".into(),
        }]
    }

    /// Parses a `source` name (`SUBJECT` / `ALTNAME`, any case) and checks the expression has a
    /// capture group; `Err` names the problem.
    pub fn parse(source: &str, query: &str) -> std::result::Result<RoleQuery, String> {
        let source = match source.trim().to_ascii_uppercase().as_str() {
            "SUBJECT" => Source::Subject,
            "ALTNAME" => Source::AltName,
            other => {
                return Err(format!(
                    "unknown certificate source {other:?} (SUBJECT or ALTNAME)"
                ));
            }
        };
        let re = regex::Regex::new(query).map_err(|e| format!("bad role query {query:?}: {e}"))?;
        if re.captures_len() < 2 {
            return Err(format!("role query {query:?} has no capture group"));
        }
        Ok(RoleQuery {
            source,
            query: query.to_string(),
        })
    }

    /// The role this query finds in `cert`.
    fn role(&self, cert: &ClientCertificate) -> Option<String> {
        let re = regex::Regex::new(&self.query).ok()?;
        let text = match self.source {
            Source::Subject => cert.subject.clone(),
            Source::AltName => cert.alt_names_text(),
        };
        let m = re.captures(&text)?.get(1)?;
        Some(m.as_str().to_string()).filter(|r| !r.is_empty())
    }
}

/// The authenticator classes roll implements, by `Settings::authenticator` (short or qualified).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authenticator {
    AllowAll,
    Password,
    /// Cassandra's `MutualTlsAuthenticator`.
    MutualTls,
    /// Cassandra's `MutualTlsWithPasswordFallbackAuthenticator`.
    MutualTlsWithPasswordFallback,
    /// ScyllaDB's `CertificateAuthenticator`.
    Certificate,
    /// ScyllaDB's `CertificateOrPasswordAuthenticator`.
    CertificateOrPassword,
}

impl Authenticator {
    pub fn of(class: &str) -> Authenticator {
        match class.rsplit('.').next().unwrap_or(class) {
            "MutualTlsAuthenticator" => Authenticator::MutualTls,
            "MutualTlsWithPasswordFallbackAuthenticator" => {
                Authenticator::MutualTlsWithPasswordFallback
            }
            "CertificateAuthenticator" => Authenticator::Certificate,
            "CertificateOrPasswordAuthenticator" => Authenticator::CertificateOrPassword,
            "" | "AllowAllAuthenticator" => Authenticator::AllowAll,
            _ => Authenticator::Password,
        }
    }

    /// The class AUTHENTICATE names (`IAuthenticator.getAuthenticateMessage`: the class's
    /// qualified name). ScyllaDB's `CertificateOrPasswordAuthenticator` sends it only to a
    /// connection without a certificate; its name there is not verified.
    pub fn class_name(self) -> &'static str {
        match self {
            Authenticator::AllowAll => "org.apache.cassandra.auth.AllowAllAuthenticator",
            Authenticator::Password => "org.apache.cassandra.auth.PasswordAuthenticator",
            Authenticator::MutualTls => "org.apache.cassandra.auth.MutualTlsAuthenticator",
            Authenticator::MutualTlsWithPasswordFallback => {
                "org.apache.cassandra.auth.MutualTlsWithPasswordFallbackAuthenticator"
            }
            Authenticator::Certificate => "com.scylladb.auth.CertificateAuthenticator",
            Authenticator::CertificateOrPassword => {
                "com.scylladb.auth.CertificateOrPasswordAuthenticator"
            }
        }
    }

    /// The class AUTHENTICATE names to a connection (`has_certificate`): ScyllaDB's
    /// `CertificateOrPasswordAuthenticator` falls back to `PasswordAuthenticator`'s SASL
    /// exchange (named as that, which every driver accepts; not verified).
    pub fn authenticate_class(self, has_certificate: bool) -> &'static str {
        match self {
            Authenticator::CertificateOrPassword if !has_certificate => {
                Authenticator::Password.class_name()
            }
            a => a.class_name(),
        }
    }

    /// Whether a connection authenticates by its certificate (`has_certificate`: the handshake
    /// verified one) rather than by SASL PLAIN.
    pub fn by_certificate(self, has_certificate: bool) -> bool {
        match self {
            Authenticator::MutualTls | Authenticator::Certificate => true,
            Authenticator::MutualTlsWithPasswordFallback | Authenticator::CertificateOrPassword => {
                has_certificate
            }
            Authenticator::AllowAll | Authenticator::Password => false,
        }
    }

    /// ScyllaDB's certificate authenticators answer STARTUP with READY when the certificate
    /// names a role (no SASL); Cassandra's always send AUTHENTICATE.
    pub fn ready_at_startup(self, has_certificate: bool) -> bool {
        matches!(
            self,
            Authenticator::Certificate | Authenticator::CertificateOrPassword
        ) && has_certificate
    }

    /// The authenticators of `dialect`'s reference.
    pub fn of_dialect(self, d: Dialect) -> bool {
        match self {
            Authenticator::AllowAll | Authenticator::Password => true,
            Authenticator::MutualTls | Authenticator::MutualTlsWithPasswordFallback => {
                d == Dialect::Cassandra
            }
            Authenticator::Certificate | Authenticator::CertificateOrPassword => {
                d == Dialect::Scylla
            }
        }
    }
}

fn bad(m: impl Into<String>) -> Error {
    Error::new(ErrorCode::BadCredentials, m)
}

/// The role `cert` authenticates as with `db`'s settings, or the reference's error.
async fn role_of(db: &Arc<Database>, cert: Option<&ClientCertificate>) -> Result<String> {
    let snap = super::store::ensure_fresh(db).await?;
    match db.dialect() {
        Dialect::Cassandra => {
            // Unreachable on Cassandra, which requires client auth for these authenticators
            // (`checkMtlsConfigurationIsValid`); roll's optional client auth gets here.
            let Some(cert) = cert else {
                return Err(bad(
                    "Unable to extract Spiffe from the certificate: no client certificate",
                ));
            };
            // `SpiffeCertificateValidator.identity`: its `CertificateException` message.
            let Some(identity) = cert.spiffe() else {
                return Err(bad("Unable to extract Spiffe from the certificate"));
            };
            match snap.identities.get(identity) {
                Some(role) => Ok(role.clone()),
                None => Err(bad(format!(
                    "Certificate identity '{identity}' not authorized"
                ))),
            }
        }
        Dialect::Scylla => {
            // Not verified (the references run without TLS).
            let Some(cert) = cert else {
                return Err(bad(
                    "No client certificate: certificate authentication requires one",
                ));
            };
            let queries = db.settings().scylla.auth_certificate_role_queries.clone();
            match queries.iter().find_map(|q| q.role(cert)) {
                Some(role) => Ok(role),
                None => Err(bad(format!(
                    "Could not extract a role name from the client certificate (subject {:?})",
                    cert.subject
                ))),
            }
        }
    }
}

/// Authenticates `state`'s connection by its client certificate (`state.client.certificate`)
/// with the database's certificate authenticator: the role must exist and be allowed to log in.
pub async fn authenticate(db: &Arc<Database>, state: &mut SessionState) -> Result<AuthStep> {
    let cert = state.client.certificate.clone();
    let role = role_of(db, cert.as_deref()).await?;
    let probe = SessionState {
        user: Some(role.clone()),
        ..Default::default()
    };
    let cx = super::authorizer::Ctx::new(db, &probe);
    let can_login = cx
        .user_roles()
        .iter()
        .find(|r| r.name == role)
        .is_some_and(|r| r.can_login);
    if !can_login {
        // `ClientState.login` (a missing role can't log in either).
        return Err(super::messages::not_permitted_to_login(&role));
    }
    state.user = Some(role);
    Ok(super::success(db.dialect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert(subject: &str, alt: &[(&str, &str)]) -> ClientCertificate {
        ClientCertificate {
            subject: subject.into(),
            alt_names: alt
                .iter()
                .map(|(t, v)| (t.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn classes() {
        use Authenticator as A;
        assert_eq!(
            A::of("org.apache.cassandra.auth.MutualTlsAuthenticator"),
            A::MutualTls
        );
        assert_eq!(
            A::of("MutualTlsWithPasswordFallbackAuthenticator"),
            A::MutualTlsWithPasswordFallback
        );
        assert_eq!(
            A::of("com.scylladb.auth.CertificateAuthenticator"),
            A::Certificate
        );
        assert_eq!(
            A::of("CertificateOrPasswordAuthenticator"),
            A::CertificateOrPassword
        );
        assert_eq!(A::of("PasswordAuthenticator"), A::Password);
        assert_eq!(A::of(""), A::AllowAll);
        assert_eq!(A::of("AllowAllAuthenticator"), A::AllowAll);
        for a in [
            A::AllowAll,
            A::Password,
            A::MutualTls,
            A::MutualTlsWithPasswordFallback,
            A::Certificate,
            A::CertificateOrPassword,
        ] {
            assert_eq!(A::of(a.class_name()), a);
        }
        assert_eq!(
            A::CertificateOrPassword.authenticate_class(false),
            "org.apache.cassandra.auth.PasswordAuthenticator"
        );
        assert_eq!(
            A::CertificateOrPassword.authenticate_class(true),
            "com.scylladb.auth.CertificateOrPasswordAuthenticator"
        );
        assert_eq!(
            A::MutualTlsWithPasswordFallback.authenticate_class(false),
            "org.apache.cassandra.auth.MutualTlsWithPasswordFallbackAuthenticator"
        );
        assert!(A::MutualTls.by_certificate(false) && A::Certificate.by_certificate(false));
        assert!(
            !A::MutualTlsWithPasswordFallback.by_certificate(false)
                && A::MutualTlsWithPasswordFallback.by_certificate(true)
        );
        assert!(
            !A::CertificateOrPassword.by_certificate(false)
                && A::CertificateOrPassword.by_certificate(true)
        );
        assert!(!A::Password.by_certificate(true));
        assert!(A::Certificate.ready_at_startup(true) && !A::Certificate.ready_at_startup(false));
        assert!(!A::MutualTls.ready_at_startup(true));
        assert!(
            A::MutualTls.of_dialect(Dialect::Cassandra)
                && !A::MutualTls.of_dialect(Dialect::Scylla)
        );
        assert!(
            A::Certificate.of_dialect(Dialect::Scylla)
                && !A::Certificate.of_dialect(Dialect::Cassandra)
        );
    }

    #[test]
    fn identities_and_role_queries() {
        let c = cert(
            "CN=alice,OU=dev,O=MyOrg",
            &[
                ("DNS", "host.example.com"),
                ("URI", "https://x"),
                ("URI", "spiffe://example.com/app"),
                ("EMAIL", "bob@example.com"),
            ],
        );
        assert_eq!(c.spiffe(), Some("spiffe://example.com/app"));
        assert_eq!(cert("CN=a", &[("URI", "https://x")]).spiffe(), None);
        assert_eq!(
            c.alt_names_text(),
            "DNS=host.example.com,URI=https://x,URI=spiffe://example.com/app,EMAIL=bob@example.com"
        );
        let q = &RoleQuery::defaults()[0];
        assert_eq!(q.role(&c).as_deref(), Some("alice"));
        assert_eq!(q.role(&cert("O=MyOrg", &[])), None);
        let alt = RoleQuery::parse("altname", "EMAIL=([^@,]+)@").unwrap();
        assert_eq!(alt.source, Source::AltName);
        assert_eq!(alt.role(&c).as_deref(), Some("bob"));
        assert_eq!(
            RoleQuery::parse("SUBJECT", "CN=([^,\\s]+)")
                .unwrap()
                .role(&c)
                .as_deref(),
            Some("alice")
        );
        assert_eq!(
            RoleQuery::parse("ISSUER", "x").unwrap_err(),
            "unknown certificate source \"ISSUER\" (SUBJECT or ALTNAME)"
        );
        assert_eq!(
            RoleQuery::parse("SUBJECT", "CN=.*").unwrap_err(),
            "role query \"CN=.*\" has no capture group"
        );
        assert!(
            RoleQuery::parse("SUBJECT", "CN=(")
                .unwrap_err()
                .starts_with("bad role query \"CN=(\"")
        );
    }
}
