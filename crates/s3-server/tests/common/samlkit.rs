//! A SAML identity provider for tests (included with `#[path]` by `saml.rs` and `tools.rs`): a
//! generated RSA key and certificate, its metadata, and responses signed with it. The canonical
//! forms are written out literally (attributes in canonical order, namespaces where exclusive
//! canonicalization puts them), so the digests and signatures don't depend on roll's
//! canonicalization.

#![allow(dead_code)]

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::Digest;
use std::sync::OnceLock;

pub const SAMLP: &str = "urn:oasis:names:tc:SAML:2.0:protocol";
pub const SAML: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
pub const DS: &str = "http://www.w3.org/2000/09/xmldsig#";
pub const EXC: &str = "http://www.w3.org/2001/10/xml-exc-c14n#";
pub const ENVELOPED: &str = "http://www.w3.org/2000/09/xmldsig#enveloped-signature";
pub const RSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256";
pub const RSA_SHA1: &str = "http://www.w3.org/2000/09/xmldsig#rsa-sha1";
pub const SHA256: &str = "http://www.w3.org/2001/04/xmlenc#sha256";
pub const SIGNIN: &str = "https://signin.aws.amazon.com/saml";
pub const ISSUER: &str = "https://idp.example.test/saml";
pub const ATTR: &str = "https://aws.amazon.com/SAML/Attributes/";

pub struct Idp {
    pub key: rsa::RsaPrivateKey,
    pub cert_b64: String,
}

pub fn make_idp() -> Idp {
    use rsa::pkcs8::EncodePrivateKey;
    let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
    let pem = key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
    let kp = rcgen::KeyPair::from_pkcs8_pem_and_sign_algo(&pem, &rcgen::PKCS_RSA_SHA256).unwrap();
    let cert = rcgen::CertificateParams::new(vec!["idp.example.test".into()]).unwrap().self_signed(&kp).unwrap();
    Idp { key, cert_b64: STANDARD.encode(cert.der()) }
}

pub fn idp() -> &'static Idp {
    static I: OnceLock<Idp> = OnceLock::new();
    I.get_or_init(make_idp)
}

/// An attacker's key and certificate.
pub fn evil() -> &'static Idp {
    static I: OnceLock<Idp> = OnceLock::new();
    I.get_or_init(make_idp)
}

impl Idp {
    pub fn metadata(&self, entity: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata" xmlns:ds="{DS}" entityID="{entity}">
  <md:IDPSSODescriptor protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol">
    <md:KeyDescriptor use="signing">
      <ds:KeyInfo><ds:X509Data><ds:X509Certificate>
{}
      </ds:X509Certificate></ds:X509Data></ds:KeyInfo>
    </md:KeyDescriptor>
    <md:KeyDescriptor use="encryption"><ds:KeyInfo><ds:X509Data><ds:X509Certificate>{}</ds:X509Certificate></ds:X509Data></ds:KeyInfo></md:KeyDescriptor>
    <md:SingleSignOnService Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect" Location="https://idp.example.test/sso"/>
  </md:IDPSSODescriptor>
</md:EntityDescriptor>"#,
            self.cert_b64,
            evil().cert_b64
        )
    }

    pub fn sign(&self, data: &[u8], sha1: bool) -> String {
        use rsa::signature::{SignatureEncoding, Signer};
        let sig = if sha1 { rsa::pkcs1v15::SigningKey::<sha1::Sha1>::new(self.key.clone()).sign(data).to_vec() } else { rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(self.key.clone()).sign(data).to_vec() };
        STANDARD.encode(sig)
    }
}

/// What an assertion says.
#[derive(Clone)]
pub struct A {
    pub id: String,
    pub issuer: String,
    pub subject: String,
    pub format: String,
    pub recipient: String,
    pub audience: String,
    pub not_before: i64,
    pub not_on_or_after: i64,
    pub session_not_on_or_after: Option<i64>,
    /// (name, values)
    pub attrs: Vec<(String, Vec<String>)>,
}

pub fn iso(ms: i64) -> String {
    s3_proto::time::iso8601_ms(ms)
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

impl A {
    pub fn new(now: i64, role: &str, provider: &str) -> Self {
        A {
            id: "_a7f3".into(),
            issuer: ISSUER.into(),
            subject: "alice@example.com".into(),
            format: "urn:oasis:names:tc:SAML:2.0:nameid-format:persistent".into(),
            recipient: SIGNIN.into(),
            audience: SIGNIN.into(),
            not_before: now - 60_000,
            not_on_or_after: now + 300_000,
            session_not_on_or_after: None,
            attrs: vec![
                (format!("{ATTR}Role"), vec![format!("{role},{provider}")]),
                (format!("{ATTR}RoleSessionName"), vec!["alice".into()]),
                (format!("{ATTR}PrincipalTag:team"), vec!["web".into()]),
                ("urn:oid:1.3.6.1.4.1.5923.1.1.1.1".into(), vec!["staff".into(), "member".into()]),
            ],
        }
    }

    /// The assertion, canonical when `sig` is empty: `ns` declares `saml` on it (as the apex of
    /// its own canonicalization; inside a canonical Response the Response declares it).
    pub fn xml(&self, ns: bool, sig: &str) -> String {
        let ns = if ns { format!(" xmlns:saml=\"{SAML}\"") } else { String::new() };
        let sess = self.session_not_on_or_after.map(|t| format!(" SessionNotOnOrAfter=\"{}\"", iso(t))).unwrap_or_default();
        let attrs: String = self
            .attrs
            .iter()
            .map(|(n, vs)| format!("<saml:Attribute Name=\"{n}\">{}</saml:Attribute>", vs.iter().map(|v| format!("<saml:AttributeValue>{}</saml:AttributeValue>", esc(v))).collect::<String>()))
            .collect();
        let at = iso(self.not_before + 60_000);
        format!(
            "<saml:Assertion{ns} ID=\"{id}\" IssueInstant=\"{at}\" Version=\"2.0\"><saml:Issuer>{iss}</saml:Issuer>{sig}<saml:Subject><saml:NameID Format=\"{fmt}\">{sub}</saml:NameID><saml:SubjectConfirmation Method=\"urn:oasis:names:tc:SAML:2.0:cm:bearer\"><saml:SubjectConfirmationData NotOnOrAfter=\"{noa}\" Recipient=\"{rec}\"></saml:SubjectConfirmationData></saml:SubjectConfirmation></saml:Subject><saml:Conditions NotBefore=\"{nb}\" NotOnOrAfter=\"{noa}\"><saml:AudienceRestriction><saml:Audience>{aud}</saml:Audience></saml:AudienceRestriction></saml:Conditions><saml:AuthnStatement AuthnInstant=\"{at}\"{sess}><saml:AuthnContext><saml:AuthnContextClassRef>urn:oasis:names:tc:SAML:2.0:ac:classes:Password</saml:AuthnContextClassRef></saml:AuthnContext></saml:AuthnStatement><saml:AttributeStatement>{attrs}</saml:AttributeStatement></saml:Assertion>",
            id = self.id,
            iss = esc(&self.issuer),
            fmt = self.format,
            sub = esc(&self.subject),
            noa = iso(self.not_on_or_after),
            rec = self.recipient,
            nb = iso(self.not_before),
            aud = self.audience,
        )
    }
}

/// A signature over `canonical` (the referenced element's canonical form) for `#id`, as it
/// appears in the document.
pub fn signature(by: &Idp, id: &str, canonical: &str, sha1: bool) -> String {
    let digest = STANDARD.encode(sha2::Sha256::digest(canonical.as_bytes()));
    let method = if sha1 { RSA_SHA1 } else { RSA_SHA256 };
    let inner = format!(
        "<ds:CanonicalizationMethod Algorithm=\"{EXC}\"></ds:CanonicalizationMethod><ds:SignatureMethod Algorithm=\"{method}\"></ds:SignatureMethod><ds:Reference URI=\"#{id}\"><ds:Transforms><ds:Transform Algorithm=\"{ENVELOPED}\"></ds:Transform><ds:Transform Algorithm=\"{EXC}\"></ds:Transform></ds:Transforms><ds:DigestMethod Algorithm=\"{SHA256}\"></ds:DigestMethod><ds:DigestValue>{digest}</ds:DigestValue></ds:Reference>"
    );
    // SignedInfo's canonical form: the apex declares the ds namespace.
    let signed_info = format!("<ds:SignedInfo xmlns:ds=\"{DS}\">{inner}</ds:SignedInfo>");
    let value = by.sign(signed_info.as_bytes(), sha1);
    format!("<ds:Signature xmlns:ds=\"{DS}\"><ds:SignedInfo>{inner}</ds:SignedInfo><ds:SignatureValue>{value}</ds:SignatureValue><ds:KeyInfo><ds:X509Data><ds:X509Certificate>{}</ds:X509Certificate></ds:X509Data></ds:KeyInfo></ds:Signature>", by.cert_b64)
}

pub fn response(issuer: &str, sig: &str, body: &str) -> String {
    format!(
        "<samlp:Response xmlns:saml=\"{SAML}\" xmlns:samlp=\"{SAMLP}\" Destination=\"{SIGNIN}\" ID=\"_r91\" IssueInstant=\"2026-01-01T00:00:00.000Z\" Version=\"2.0\"><saml:Issuer>{issuer}</saml:Issuer>{sig}<samlp:Status><samlp:StatusCode Value=\"urn:oasis:names:tc:SAML:2.0:status:Success\"></samlp:StatusCode></samlp:Status>{body}</samlp:Response>"
    )
}

/// The canonical form of [`response`] without a signature: exclusive canonicalization renders
/// `saml` only where it's visibly used (the Issuer, the assertion), not on the Response.
/// `body` is the assertion's canonical form (declaring `saml`).
pub fn canonical_response(issuer: &str, body: &str) -> String {
    format!(
        "<samlp:Response xmlns:samlp=\"{SAMLP}\" Destination=\"{SIGNIN}\" ID=\"_r91\" IssueInstant=\"2026-01-01T00:00:00.000Z\" Version=\"2.0\"><saml:Issuer xmlns:saml=\"{SAML}\">{issuer}</saml:Issuer><samlp:Status><samlp:StatusCode Value=\"urn:oasis:names:tc:SAML:2.0:status:Success\"></samlp:StatusCode></samlp:Status>{body}</samlp:Response>"
    )
}

/// A response whose assertion is signed by `by`.
pub fn signed(a: &A, by: &Idp) -> String {
    let sig = signature(by, &a.id, &a.xml(true, ""), false);
    response(&a.issuer, "", &a.xml(true, &sig))
}

pub fn b64(xml: &str) -> String {
    STANDARD.encode(xml)
}
