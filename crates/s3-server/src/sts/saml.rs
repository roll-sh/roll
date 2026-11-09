//! SAML 2.0 federation: IdP metadata (`CreateSAMLProvider`), and `AssumeRoleWithSAML`, which
//! trades a SAML response signed by the IdP for a role session, as AWS's STS does.
//!
//! **Verifying a response** (XML Signature, enveloped, exclusive canonicalization):
//!
//! - The document is parsed by `s3_proto::xmldsig` (no DTDs or entities, no processing
//!   instructions, size and depth limits, unique IDs).
//! - Exactly one `Assertion`, a direct child of the `Response`; an `EncryptedAssertion` is
//!   refused (roll keeps no SAML decryption keys).
//! - Every `ds:Signature` must be a direct child of the `Response` or the `Assertion`, and every
//!   one must verify; at least one must cover the assertion (the assertion's own, or the
//!   response's, which contains it). A signature somewhere else is refused.
//! - A signature verifies when: `SignedInfo` has exclusive canonicalization, RSA-SHA256 or
//!   RSA-SHA1 and exactly one `Reference`, whose `URI` is `#<ID>` of the signature's own parent
//!   element (the very node the assertion is then read from: no signature wrapping), whose
//!   transforms are exactly enveloped-signature then exclusive canonicalization, and whose
//!   SHA-256 (or SHA-1) digest of that element (without the signature) matches; and the
//!   canonical `SignedInfo` verifies with an RSA key of a signing certificate in the provider's
//!   metadata. `KeyInfo` in the response is ignored: only the metadata's keys count.
//! - The assertion must come from the provider's `entityID` (`Issuer`), have a bearer
//!   `SubjectConfirmation` whose `Recipient` is AWS's sign-in endpoint
//!   (`https://signin.aws.amazon.com/saml`, or the region's) or the deployment's configured one,
//!   be within `NotBefore`/`NotOnOrAfter` (of `Conditions` and of the confirmation; a few
//!   minutes of clock skew), and name an accepted `Audience` in every `AudienceRestriction`
//!   (those endpoints, or `urn:amazon:webservices`).
//! - Its `Role` attribute must pair the requested role with the requested provider, and it must
//!   have a `RoleSessionName`. `PrincipalTag:*`, `TransitiveTagKeys` and `SourceIdentity` become
//!   the session's.
//!
//! The role's trust policy then decides, with `saml:*` condition keys (`saml:aud` is the
//! recipient, `saml:sub`, `saml:sub_type`, `saml:iss`, `saml:namequalifier`, and the eduPerson
//! and X.500 attributes AWS maps), for its `Federated` principal, the provider's ARN.

use super::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use s3_proto::xmldsig::{Doc, NodeId};
use s3_store::iam::SamlProviderEntry;
use s3_store::sts::SamlIdentity;

pub(crate) const MD: &str = "urn:oasis:names:tc:SAML:2.0:metadata";
pub(crate) const SAMLP: &str = "urn:oasis:names:tc:SAML:2.0:protocol";
pub(crate) const SAML: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
pub(crate) const DS: &str = "http://www.w3.org/2000/09/xmldsig#";
pub(crate) const EXC_C14N: &str = "http://www.w3.org/2001/10/xml-exc-c14n#";
pub(crate) const ENVELOPED: &str = "http://www.w3.org/2000/09/xmldsig#enveloped-signature";
pub(crate) const RSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256";
pub(crate) const RSA_SHA1: &str = "http://www.w3.org/2000/09/xmldsig#rsa-sha1";
pub(crate) const SHA256: &str = "http://www.w3.org/2001/04/xmlenc#sha256";
pub(crate) const SHA1: &str = "http://www.w3.org/2000/09/xmldsig#sha1";
const SUCCESS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";
const BEARER: &str = "urn:oasis:names:tc:SAML:2.0:cm:bearer";
const ATTR: &str = "https://aws.amazon.com/SAML/Attributes/";
/// AWS's sign-in endpoint: the recipient (and audience) of assertions for AWS.
pub(crate) const AWS_SIGNIN: &str = "https://signin.aws.amazon.com/saml";
/// The clock skew allowed between the IdP and roll.
const SKEW_MS: i64 = 300_000;
/// Longest `SAMLAssertion` (AWS's limit), base64.
pub(crate) const MAX_ASSERTION: usize = 100_000;

/// Why a SAML response or metadata document was refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SamlError {
    Invalid(String),
    Expired,
}

fn invalid(m: impl Into<String>) -> SamlError {
    SamlError::Invalid(m.into())
}

/// An xs:dateTime in UTC (`2026-01-02T03:04:05Z`, fractions too: SAML's times are UTC), as Unix
/// ms.
pub(crate) fn parse_time(s: &str) -> Option<i64> {
    s3_proto::time::parse_iso8601(s.trim())
}

// ---- Metadata ----

/// What a provider's metadata says.
#[derive(Debug)]
pub(crate) struct Metadata {
    pub entity_id: String,
    /// Signing certificates (DER, base64).
    pub certificates: Vec<String>,
    pub valid_until: Option<i64>,
}

/// Parse IdP metadata: an `EntityDescriptor` (alone, or the one inside an `EntitiesDescriptor`)
/// with an `IDPSSODescriptor` and at least one signing certificate.
pub(crate) fn parse_metadata(xml: &str) -> Result<Metadata, String> {
    let doc = Doc::parse(xml).map_err(|e| format!("Could not parse metadata: {e}"))?;
    let root = doc.root();
    let mut valid_until = doc.el(root).attr("validUntil").and_then(parse_time);
    let entity = if doc.el(root).is(MD, "EntityDescriptor") {
        root
    } else if doc.el(root).is(MD, "EntitiesDescriptor") {
        let all: Vec<NodeId> = doc.children(root, MD, "EntityDescriptor").collect();
        let [one] = all.as_slice() else { return Err("Could not parse metadata: expected one EntityDescriptor".into()) };
        *one
    } else {
        return Err("Could not parse metadata: the root isn't an EntityDescriptor".into());
    };
    if let Some(t) = doc.el(entity).attr("validUntil").and_then(parse_time) {
        valid_until = Some(valid_until.map_or(t, |v| v.min(t)));
    }
    let entity_id = doc.el(entity).attr("entityID").filter(|e| !e.is_empty() && e.len() <= 1024).ok_or("Could not parse metadata: no entityID")?.to_string();
    let idp = doc.only_child(entity, MD, "IDPSSODescriptor").map_err(|e| format!("Could not parse metadata: {e}"))?.ok_or("Could not parse metadata: no IDPSSODescriptor")?;
    let mut certificates = Vec::new();
    for kd in doc.children(idp, MD, "KeyDescriptor") {
        if doc.el(kd).attr("use").is_some_and(|u| u != "signing") {
            continue;
        }
        for ki in doc.children(kd, DS, "KeyInfo") {
            for x in doc.children(ki, DS, "X509Data") {
                for c in doc.children(x, DS, "X509Certificate") {
                    let b64: String = doc.text_of(c).chars().filter(|c| !c.is_whitespace()).collect();
                    let der = STANDARD.decode(&b64).map_err(|_| "Could not parse metadata: a certificate isn't base64")?;
                    let cert = crate::x509::parse_der(&der).map_err(|_| "Could not parse metadata: a certificate doesn't parse")?;
                    if cert.rsa_public_key().is_some() && !certificates.contains(&b64) {
                        certificates.push(b64);
                    }
                }
            }
        }
    }
    if certificates.is_empty() {
        return Err("Could not parse metadata: no RSA signing certificate".into());
    }
    Ok(Metadata { entity_id, certificates, valid_until })
}

// ---- Responses ----

/// A verified assertion.
#[derive(Debug, Default)]
pub(crate) struct Assertion {
    pub issuer: String,
    pub subject: String,
    /// `persistent`, `transient`, or the `NameID` format URI.
    pub subject_type: String,
    /// The recipient it was for (`saml:aud`).
    pub recipient: String,
    /// (role ARN, provider ARN) pairs of the `Role` attribute.
    pub roles: Vec<(String, String)>,
    pub session_name: String,
    pub session_not_on_or_after: Option<i64>,
    pub tags: Vec<(String, String)>,
    pub transitive: Vec<String>,
    pub source_identity: Option<String>,
    /// Every attribute: name → values.
    pub attributes: Vec<(String, Vec<String>)>,
}

fn attr<'a>(doc: &'a Doc, n: NodeId, name: &str) -> Option<&'a str> {
    doc.el(n).attr(name)
}

/// The one child `{ns}local`, required.
fn one(doc: &Doc, n: NodeId, ns: &str, local: &str) -> Result<NodeId, SamlError> {
    doc.only_child(n, ns, local).map_err(|e| invalid(e.0))?.ok_or_else(|| invalid(format!("missing {local}")))
}

fn ct_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `ec:InclusiveNamespaces PrefixList` of a canonicalization method or transform.
fn inclusive(doc: &Doc, n: NodeId) -> Vec<String> {
    doc.children(n, EXC_C14N, "InclusiveNamespaces").next().and_then(|i| attr(doc, i, "PrefixList")).map(|p| p.split_whitespace().map(str::to_string).collect()).unwrap_or_default()
}

/// Verify the signature `sig` (a child of `target`) with one of `keys` (PKCS#1 RSA public keys).
fn verify_signature(doc: &Doc, sig: NodeId, target: NodeId, keys: &[Vec<u8>]) -> Result<(), SamlError> {
    let bad = |why: &str| invalid(format!("Response signature invalid: {why}"));
    for c in doc.elements(sig) {
        let e = doc.el(c);
        if e.ns != DS || !matches!(e.local.as_str(), "SignedInfo" | "SignatureValue" | "KeyInfo") {
            return Err(bad("unexpected element in Signature"));
        }
    }
    let si = one(doc, sig, DS, "SignedInfo")?;
    let sv = one(doc, sig, DS, "SignatureValue")?;
    for c in doc.elements(si) {
        let e = doc.el(c);
        if e.ns != DS || !matches!(e.local.as_str(), "CanonicalizationMethod" | "SignatureMethod" | "Reference") {
            return Err(bad("unexpected element in SignedInfo"));
        }
    }
    let cm = one(doc, si, DS, "CanonicalizationMethod")?;
    if attr(doc, cm, "Algorithm") != Some(EXC_C14N) {
        return Err(bad("only exclusive canonicalization (without comments) is supported"));
    }
    let sm = one(doc, si, DS, "SignatureMethod")?;
    let alg: &'static ring::signature::RsaParameters = match attr(doc, sm, "Algorithm") {
        Some(RSA_SHA256) => &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        Some(RSA_SHA1) => &ring::signature::RSA_PKCS1_2048_8192_SHA1_FOR_LEGACY_USE_ONLY,
        _ => return Err(bad("only RSA-SHA256 and RSA-SHA1 signatures are supported")),
    };
    let refs: Vec<NodeId> = doc.children(si, DS, "Reference").collect();
    let [r] = refs.as_slice() else { return Err(bad("exactly one Reference is required")) };
    let r = *r;
    // The reference must name the signature's own parent, by its unique ID.
    let target_id = attr(doc, target, "ID").ok_or_else(|| bad("the signed element has no ID"))?;
    let uri = attr(doc, r, "URI").ok_or_else(|| bad("the Reference has no URI"))?;
    if uri.strip_prefix('#') != Some(target_id) || doc.by_id(target_id) != Some(target) {
        return Err(bad("the Reference doesn't name the signed element"));
    }
    let transforms = one(doc, r, DS, "Transforms")?;
    let ts: Vec<NodeId> = doc.elements(transforms).collect();
    let [t1, t2] = ts.as_slice() else { return Err(bad("the transforms must be enveloped-signature and exclusive canonicalization")) };
    if !doc.el(*t1).is(DS, "Transform") || attr(doc, *t1, "Algorithm") != Some(ENVELOPED) || !doc.el(*t2).is(DS, "Transform") || attr(doc, *t2, "Algorithm") != Some(EXC_C14N) {
        return Err(bad("the transforms must be enveloped-signature and exclusive canonicalization"));
    }
    let dm = one(doc, r, DS, "DigestMethod")?;
    let dv = one(doc, r, DS, "DigestValue")?;
    let want = STANDARD.decode(doc.text_of(dv).chars().filter(|c| !c.is_whitespace()).collect::<String>()).map_err(|_| bad("DigestValue isn't base64"))?;
    let canonical = doc.exc_c14n(target, Some(sig), &inclusive(doc, *t2));
    let got: Vec<u8> = match attr(doc, dm, "Algorithm") {
        Some(SHA256) => sha2::Sha256::digest(canonical.as_bytes()).to_vec(),
        Some(SHA1) => sha1_digest(canonical.as_bytes()),
        _ => return Err(bad("only SHA-256 and SHA-1 digests are supported")),
    };
    if !ct_eq_bytes(&got, &want) {
        return Err(bad("the digest doesn't match"));
    }
    let signed = doc.exc_c14n(si, None, &inclusive(doc, cm));
    let value = STANDARD.decode(doc.text_of(sv).chars().filter(|c| !c.is_whitespace()).collect::<String>()).map_err(|_| bad("SignatureValue isn't base64"))?;
    if keys.iter().any(|k| ring::signature::UnparsedPublicKey::new(alg, k).verify(signed.as_bytes(), &value).is_ok()) {
        Ok(())
    } else {
        Err(bad("it doesn't verify with the provider's signing certificates"))
    }
}

fn sha1_digest(b: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, b).as_ref().to_vec()
}

/// The provider's signing keys (PKCS#1 RSA public keys).
fn keys_of(p: &SamlProviderEntry) -> Vec<Vec<u8>> {
    p.certificates.iter().filter_map(|b| STANDARD.decode(b).ok()).filter_map(|d| crate::x509::parse_der(&d).ok()).filter_map(|c| c.rsa_public_key().map(<[u8]>::to_vec)).collect()
}

/// Verify a base64 SAML response for `provider` at `now` (Unix ms). `recipients` are the
/// endpoints an assertion may be for; `audiences` what `Audience` may name.
pub(crate) fn verify(response: &str, provider: &SamlProviderEntry, now: i64, recipients: &[String], audiences: &[String]) -> Result<Assertion, SamlError> {
    let raw = STANDARD.decode(response.chars().filter(|c| !c.is_whitespace()).collect::<String>()).map_err(|_| invalid("the SAML response isn't base64"))?;
    let xml = std::str::from_utf8(&raw).map_err(|_| invalid("the SAML response isn't UTF-8"))?;
    let doc = Doc::parse(xml).map_err(|e| invalid(format!("Invalid SAML response: {e}")))?;
    let root = doc.root();
    if !doc.el(root).is(SAMLP, "Response") || attr(&doc, root, "Version") != Some("2.0") {
        return Err(invalid("Invalid SAML response: not a SAML 2.0 Response"));
    }
    let status = one(&doc, root, SAMLP, "Status")?;
    let code = one(&doc, status, SAMLP, "StatusCode")?;
    if attr(&doc, code, "Value") != Some(SUCCESS) {
        return Err(invalid("The SAML response doesn't have a Success status"));
    }
    if let Some(d) = attr(&doc, root, "Destination")
        && !recipients.iter().any(|r| r == d)
    {
        return Err(invalid("The SAML response's Destination isn't an accepted endpoint"));
    }
    if !doc.find_all(SAML, "EncryptedAssertion").is_empty() {
        return Err(invalid("Encrypted SAML assertions aren't supported"));
    }
    let assertions = doc.find_all(SAML, "Assertion");
    let [a] = assertions.as_slice() else { return Err(invalid("Invalid SAML response: it must hold exactly one Assertion")) };
    let a = *a;
    if doc.el(a).parent != Some(root) {
        return Err(invalid("Invalid SAML response: the Assertion must be a child of the Response"));
    }
    // Signatures: where they may be, each valid, one covering the assertion.
    let keys = keys_of(provider);
    let mut covered = false;
    for sig in doc.find_all(DS, "Signature") {
        let parent = doc.el(sig).parent.ok_or_else(|| invalid("Response signature invalid"))?;
        if parent != root && parent != a {
            return Err(invalid("Response signature invalid: a signature may only sign the Response or the Assertion"));
        }
        verify_signature(&doc, sig, parent, &keys)?;
        covered = true;
    }
    if !covered {
        return Err(invalid("The SAML response isn't signed"));
    }
    read_assertion(&doc, root, a, provider, now, recipients, audiences)
}

fn read_assertion(doc: &Doc, root: NodeId, a: NodeId, provider: &SamlProviderEntry, now: i64, recipients: &[String], audiences: &[String]) -> Result<Assertion, SamlError> {
    if attr(doc, a, "Version") != Some("2.0") {
        return Err(invalid("Invalid SAML assertion: not SAML 2.0"));
    }
    let issuer = doc.text_of(one(doc, a, SAML, "Issuer")?).trim().to_string();
    if issuer != provider.entity_id {
        return Err(invalid("The assertion's Issuer isn't the provider's entityID"));
    }
    if let Some(ri) = doc.only_child(root, SAML, "Issuer").map_err(|e| invalid(e.0))?
        && doc.text_of(ri).trim() != issuer
    {
        return Err(invalid("The response's Issuer isn't the assertion's"));
    }
    // Subject and its bearer confirmation for an accepted recipient.
    let subject = one(doc, a, SAML, "Subject")?;
    let name_id = one(doc, subject, SAML, "NameID")?;
    let sub = doc.text_of(name_id).trim().to_string();
    if sub.is_empty() {
        return Err(invalid("The assertion's NameID is empty"));
    }
    let subject_type = match attr(doc, name_id, "Format") {
        Some("urn:oasis:names:tc:SAML:2.0:nameid-format:persistent") => "persistent".to_string(),
        Some("urn:oasis:names:tc:SAML:2.0:nameid-format:transient") => "transient".to_string(),
        Some(f) => f.to_string(),
        None => "urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified".to_string(),
    };
    let mut recipient = None;
    let mut expired = false;
    for sc in doc.children(subject, SAML, "SubjectConfirmation") {
        if attr(doc, sc, "Method") != Some(BEARER) {
            continue;
        }
        let Some(d) = doc.only_child(sc, SAML, "SubjectConfirmationData").map_err(|e| invalid(e.0))? else { continue };
        let Some(r) = attr(doc, d, "Recipient").filter(|r| recipients.iter().any(|x| x == r)) else { continue };
        let Some(until) = attr(doc, d, "NotOnOrAfter").and_then(parse_time) else { continue };
        if now >= until + SKEW_MS || attr(doc, d, "NotBefore").and_then(parse_time).is_some_and(|nb| now + SKEW_MS < nb) {
            expired = true;
            continue;
        }
        recipient = Some(r.to_string());
        break;
    }
    let Some(recipient) = recipient else {
        return Err(if expired { SamlError::Expired } else { invalid("The assertion has no bearer SubjectConfirmation for an accepted Recipient") });
    };
    // Conditions: validity and audiences.
    let cond = one(doc, a, SAML, "Conditions")?;
    if let Some(nb) = attr(doc, cond, "NotBefore") {
        let nb = parse_time(nb).ok_or_else(|| invalid("Invalid NotBefore"))?;
        if now + SKEW_MS < nb {
            return Err(SamlError::Expired);
        }
    }
    if let Some(na) = attr(doc, cond, "NotOnOrAfter") {
        let na = parse_time(na).ok_or_else(|| invalid("Invalid NotOnOrAfter"))?;
        if now >= na + SKEW_MS {
            return Err(SamlError::Expired);
        }
    }
    let restrictions: Vec<NodeId> = doc.children(cond, SAML, "AudienceRestriction").collect();
    if restrictions.is_empty() {
        return Err(invalid("The assertion has no AudienceRestriction"));
    }
    for ar in restrictions {
        if !doc.children(ar, SAML, "Audience").any(|au| audiences.iter().any(|x| x == doc.text_of(au).trim())) {
            return Err(invalid("The assertion's Audience isn't an accepted one"));
        }
    }
    let session_not_on_or_after = doc.children(a, SAML, "AuthnStatement").filter_map(|s| attr(doc, s, "SessionNotOnOrAfter")).filter_map(parse_time).min();
    // Attributes.
    let mut out = Assertion { issuer, subject: sub, subject_type, recipient, session_not_on_or_after, ..Default::default() };
    for st in doc.children(a, SAML, "AttributeStatement") {
        for at in doc.children(st, SAML, "Attribute") {
            let Some(name) = attr(doc, at, "Name") else { continue };
            let values: Vec<String> = doc.children(at, SAML, "AttributeValue").map(|v| doc.text_of(v).trim().to_string()).collect();
            out.attributes.push((name.to_string(), values));
        }
    }
    let single = |name: &str| -> Result<Option<String>, SamlError> {
        let vals: Vec<&String> = out.attributes.iter().filter(|(n, _)| n == name).flat_map(|(_, v)| v).collect();
        match vals.as_slice() {
            [] => Ok(None),
            [v] => Ok(Some((*v).clone())),
            _ => Err(invalid(format!("The attribute {name} must have one value"))),
        }
    };
    out.session_name = single(&format!("{ATTR}RoleSessionName"))?.ok_or_else(|| invalid("The assertion has no RoleSessionName attribute"))?;
    st::validate_session_name(&out.session_name).map_err(invalid)?;
    out.source_identity = single(&format!("{ATTR}SourceIdentity"))?;
    for (n, vals) in &out.attributes {
        if n == &format!("{ATTR}Role") {
            for v in vals {
                let parts: Vec<&str> = v.split(',').map(str::trim).collect();
                let [x, y] = parts.as_slice() else { continue };
                let (role, prov) = if x.contains(":saml-provider/") { (*y, *x) } else { (*x, *y) };
                if role.contains(":role/") && prov.contains(":saml-provider/") {
                    out.roles.push((role.to_string(), prov.to_string()));
                }
            }
        } else if let Some(k) = n.strip_prefix(&format!("{ATTR}PrincipalTag:")) {
            let [v] = vals.as_slice() else { return Err(invalid(format!("The session tag {k} must have one value"))) };
            out.tags.push((k.to_string(), v.clone()));
        } else if n == &format!("{ATTR}TransitiveTagKeys") {
            out.transitive.extend(vals.iter().cloned());
        }
    }
    Ok(out)
}

/// The `saml:` keys AWS maps from well-known attributes (by OID name), and their keys.
const ATTRIBUTE_KEYS: &[(&str, &str)] = &[
    ("urn:oid:1.3.6.1.4.1.5923.1.1.1.1", "saml:edupersonaffiliation"),
    ("urn:oid:1.3.6.1.4.1.5923.1.1.1.2", "saml:edupersonnickname"),
    ("urn:oid:1.3.6.1.4.1.5923.1.1.1.3", "saml:edupersonorgdn"),
    ("urn:oid:1.3.6.1.4.1.5923.1.1.1.4", "saml:edupersonorgunitdn"),
    ("urn:oid:1.3.6.1.4.1.5923.1.1.1.5", "saml:edupersonprimaryaffiliation"),
    ("urn:oid:1.3.6.1.4.1.5923.1.1.1.6", "saml:edupersonprincipalname"),
    ("urn:oid:1.3.6.1.4.1.5923.1.1.1.7", "saml:edupersonentitlement"),
    ("urn:oid:1.3.6.1.4.1.5923.1.1.1.8", "saml:edupersonprimaryorgunitdn"),
    ("urn:oid:1.3.6.1.4.1.5923.1.1.1.9", "saml:edupersonscopedaffiliation"),
    ("urn:oid:1.3.6.1.4.1.5923.1.1.1.10", "saml:edupersontargetedid"),
    ("urn:oid:1.3.6.1.4.1.5923.1.1.1.11", "saml:edupersonassurance"),
    ("urn:oid:0.9.2342.19200300.100.1.1", "saml:uid"),
    ("urn:oid:0.9.2342.19200300.100.1.3", "saml:mail"),
    ("urn:oid:0.9.2342.19200300.100.1.45", "saml:organizationstatus"),
    ("urn:oid:2.5.4.3", "saml:cn"),
    ("urn:oid:2.5.4.4", "saml:surname"),
    ("urn:oid:2.5.4.42", "saml:givenname"),
    ("urn:oid:2.5.4.41", "saml:name"),
    ("urn:oid:2.5.4.45", "saml:x500uniqueidentifier"),
    ("urn:oid:1.3.6.1.4.1.5923.1.2.1.2", "saml:commonname"),
    ("http://schemas.microsoft.com/ws/2008/06/identity/claims/primarygroupsid", "saml:primarygroupsid"),
];

/// `saml:namequalifier`: Base64(SHA-1(issuer + account id + "/" + provider name)), as AWS
/// computes it.
pub(crate) fn name_qualifier(issuer: &str, account: &str, provider: &str) -> String {
    STANDARD.encode(sha1_digest(format!("{issuer}{account}/{provider}").as_bytes()))
}

/// The endpoints assertions may be for, and the audiences they may name.
pub(crate) fn endpoints(ctx: &Ctx) -> (Vec<String>, Vec<String>) {
    let mut recipients = vec![AWS_SIGNIN.to_string(), format!("https://{}.signin.aws.amazon.com/saml", ctx.server.cfg.region)];
    recipients.extend(ctx.server.cfg.saml_audiences.iter().cloned());
    let mut audiences = recipients.clone();
    audiences.push("urn:amazon:webservices".into());
    (recipients, audiences)
}

fn saml_error(e: SamlError) -> StsError {
    match e {
        SamlError::Invalid(m) => StsError::new("InvalidIdentityToken", 400, m),
        SamlError::Expired => StsError::new("ExpiredTokenException", 400, "Token is expired"),
    }
}

pub(crate) async fn assume_role_with_saml(ctx: &Ctx, store: &Store, p: &Params) -> Result<String, StsError> {
    let arn = p.required("RoleArn", "roleArn")?;
    let provider_arn = p.required("PrincipalArn", "principalArn")?;
    let assertion = p.required("SAMLAssertion", "sAMLAssertion")?;
    if assertion.len() < 4 || assertion.len() > MAX_ASSERTION {
        return Err(validation(format!("1 validation error detected: Value at 'sAMLAssertion' failed to satisfy constraint: Member must have length less than or equal to {MAX_ASSERTION}")));
    }
    let denied = || access_denied("Not authorized to perform sts:AssumeRoleWithSAML");
    let role = role_of(store, arn)?.ok_or_else(denied)?;
    let name = provider_arn.strip_prefix(&format!("arn:aws:iam::{}:saml-provider/", role.account)).ok_or_else(denied)?;
    let provider: SamlProviderEntry = store
        .iam_read(|t| t.get(&role.account, s3_store::iam::kind::SAML, name))
        .map_err(store_failure)?
        .ok_or_else(|| StsError::new("InvalidIdentityToken", 400, "The SAML provider in PrincipalArn doesn't exist."))?;
    let now = store.now();
    if provider.valid_until > 0 && now >= provider.valid_until {
        return Err(StsError::new("InvalidIdentityToken", 400, "The SAML provider's metadata is no longer valid (validUntil)."));
    }
    let (recipients, audiences) = endpoints(ctx);
    let a = verify(assertion, &provider, now, &recipients, &audiences).map_err(saml_error)?;
    if !a.roles.iter().any(|(r, pr)| r == arn && pr == provider_arn) {
        return Err(denied());
    }
    // The role's trust policy must allow this provider's assertion.
    if role.iam.trust_policy.is_none() {
        return Err(denied());
    }
    let tags = a.tags.clone();
    let transitive = a.transitive.clone();
    check_tags(&tags, &transitive)?;
    if let Some(si) = &a.source_identity {
        st::validate_source_identity(si).map_err(validation)?;
    }
    let nq = name_qualifier(&a.issuer, &role.account, &provider.name);
    let who = Principal::Known(Box::new(Requester { account: role.account.clone(), federated: Some(provider_arn.to_string()), ..Default::default() }));
    let mut extra = vec![
        ("saml:aud".to_string(), a.recipient.clone()),
        ("saml:iss".to_string(), a.issuer.clone()),
        ("saml:sub".to_string(), a.subject.clone()),
        ("saml:sub_type".to_string(), a.subject_type.clone()),
        ("saml:namequalifier".to_string(), nq.clone()),
        ("aws:federatedprovider".to_string(), provider_arn.to_string()),
        ("sts:rolesessionname".to_string(), a.session_name.clone()),
        ("aws:principaltype".to_string(), "SAMLUser".to_string()),
    ];
    for (oid, key) in ATTRIBUTE_KEYS {
        for (n, vals) in &a.attributes {
            if n == oid {
                extra.extend(vals.iter().map(|v| (key.to_string(), v.clone())));
            }
        }
    }
    for (k, v) in &tags {
        extra.push((format!("aws:requesttag/{}", k.to_ascii_lowercase()), v.clone()));
        extra.push(("aws:tagkeys".to_string(), k.clone()));
    }
    if let Some(si) = &a.source_identity {
        extra.push(("sts:sourceidentity".to_string(), si.clone()));
    }
    let cond = StsCond { ctx, id: None, p, extra };
    if trusted(store, &role, &who, None, "sts:AssumeRoleWithSAML", &cond)?.is_none() {
        return Err(denied());
    }
    if !tags.is_empty() && trusted(store, &role, &who, None, "sts:TagSession", &cond)?.is_none() {
        return Err(access_denied("Not authorized to perform sts:TagSession"));
    }
    let mut d = p.duration(st::ROLE_DEFAULT_SESSION_S, role.max_session_s, "The requested DurationSeconds exceeds the MaxSessionDuration set for this role.")?;
    // At most until the IdP's session ends.
    if let Some(end) = a.session_not_on_or_after {
        let left = (end - now).div_euclid(1000);
        if left <= 0 {
            return Err(StsError::new("ExpiredTokenException", 400, "Token is expired"));
        }
        d = d.min(left);
    }
    let policy = p.session_policy()?;
    let arns = p.policy_arns(store, &role.account)?;
    let mut s = role_session(&role, &a.session_name, policy);
    s.saml = Some(SamlIdentity { provider: provider.name.clone(), issuer: a.issuer.clone(), subject: a.subject.clone(), subject_type: a.subject_type.clone(), audience: a.recipient.clone(), name_qualifier: nq.clone() });
    s.tags = tags;
    s.transitive_tag_keys = transitive;
    s.source_identity = a.source_identity.clone();
    s.policy_arns = arns;
    let (s, c) = store.mint_session(s, d).await.map_err(store_failure)?;
    Ok(format!(
        "{}{}{}{}{}{}",
        assumed_role_xml(&s, &c),
        el("Audience", &a.recipient),
        el("Issuer", &a.issuer),
        el("NameQualifier", &nq),
        el("Subject", &a.subject),
        el("SubjectType", &a.subject_type)
    ))
}

fn el(name: &str, v: &str) -> String {
    format!("<{name}>{}</{name}>", escape(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_qualifiers() {
        assert_eq!(parse_time("2023-11-14T22:13:20Z"), Some(1_700_000_000_000));
        assert_eq!(parse_time("2023-11-14T22:13:20.500Z"), Some(1_700_000_000_500));
        assert_eq!(parse_time("2023-11-14T23:13:20+01:00"), None, "SAML's times are UTC");
        assert_eq!(parse_time("yesterday"), None);
        // AWS's documented example shape: Base64(SHA1(issuer + account + "/" + provider)).
        let q = name_qualifier("https://idp.example.com", "123456789012", "MyIdP");
        assert_eq!(q.len(), 28);
        assert_eq!(q, STANDARD.encode(sha1_digest(b"https://idp.example.com123456789012/MyIdP")));
    }

    /// A response signed by another implementation (signxml, on lxml's exclusive
    /// canonicalization: `tests/fixtures/saml/gen.py`), pretty-printed, with the namespaces
    /// declared on the Response only: roll's canonicalization must produce the same bytes.
    #[test]
    fn verifies_a_response_signed_elsewhere() {
        let response = include_str!("../../tests/fixtures/saml/response.xml");
        let m = parse_metadata(include_str!("../../tests/fixtures/saml/metadata.xml")).unwrap();
        assert_eq!(m.entity_id, "https://idp.signxml.test/metadata");
        let p = SamlProviderEntry { name: "signxml".into(), entity_id: m.entity_id, certificates: m.certificates, ..Default::default() };
        let rec = vec![AWS_SIGNIN.to_string()];
        let aud = vec![AWS_SIGNIN.to_string(), "urn:amazon:webservices".into()];
        let now = parse_time("2026-06-01T00:00:00Z").unwrap();
        let a = verify(&STANDARD.encode(response), &p, now, &rec, &aud).unwrap();
        assert_eq!((a.subject.as_str(), a.subject_type.as_str(), a.session_name.as_str()), ("interop-user@example.com", "persistent", "interop-user"));
        assert_eq!(a.roles, vec![("arn:aws:iam::123456789012:role/interop".to_string(), "arn:aws:iam::123456789012:saml-provider/signxml".to_string())]);
        assert!(a.attributes.iter().any(|(n, v)| n == "urn:oid:0.9.2342.19200300.100.1.3" && v == &["interop-user@example.com"]));
        // Any change to what's signed breaks it.
        let tampered = response.replace("interop-user@example.com</saml:NameID>", "admin@example.com</saml:NameID>");
        assert!(matches!(verify(&STANDARD.encode(&tampered), &p, now, &rec, &aud), Err(SamlError::Invalid(m)) if m.contains("digest")));
        // Whitespace outside the signed assertion doesn't matter; inside it does.
        let outside = response.replacen("<samlp:Status>", "<samlp:Status>\n\n", 1);
        assert!(verify(&STANDARD.encode(&outside), &p, now, &rec, &aud).is_ok());
        let inside = response.replacen("<saml:Subject>", "<saml:Subject> ", 1);
        assert!(verify(&STANDARD.encode(&inside), &p, now, &rec, &aud).is_err());
        // Another provider's keys don't verify it.
        let other = SamlProviderEntry { certificates: Vec::new(), ..p.clone() };
        assert!(verify(&STANDARD.encode(response), &other, now, &rec, &aud).is_err());
        // Past its NotOnOrAfter (2099), it's expired.
        assert_eq!(verify(&STANDARD.encode(response), &p, parse_time("2099-01-02T00:00:00Z").unwrap(), &rec, &aud).unwrap_err(), SamlError::Expired);
    }

    #[test]
    fn metadata_needs_a_signing_certificate() {
        assert!(parse_metadata("<x/>").unwrap_err().contains("EntityDescriptor"));
        let no_cert = format!("<md:EntityDescriptor xmlns:md=\"{MD}\" entityID=\"https://idp\"><md:IDPSSODescriptor/></md:EntityDescriptor>");
        assert!(parse_metadata(&no_cert).unwrap_err().contains("signing certificate"));
        let no_id = format!("<md:EntityDescriptor xmlns:md=\"{MD}\"><md:IDPSSODescriptor/></md:EntityDescriptor>");
        assert!(parse_metadata(&no_id).unwrap_err().contains("entityID"));
        assert!(parse_metadata("<!DOCTYPE x [<!ENTITY a \"b\">]><x/>").unwrap_err().contains("DTD"));
    }
}
