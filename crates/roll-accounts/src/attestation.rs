//! Hardware keys only (docs/accounts.md, "Passkeys"): a company that requires its people's
//! passkeys to be on authenticators it chose (YubiKeys, say) lists those models (their AAGUIDs)
//! with their makers' root certificates. Registration then asks for attestation, and a passkey
//! counts only when its `packed` statement is verified (WebAuthn §8.2): the statement's signature
//! by the attestation certificate, that certificate's FIDO requirements, its AAGUID extension
//! matching the authenticator data, and its chain to one of the model's roots. An unverified
//! AAGUID is the authenticator's own say-so, so the list is never checked against one.
//! Off by default: then roll asks for no attestation and trusts none.

use crate::passkeys::{Cbor, Refused};
use serde::{Deserialize, Serialize};
use x509_parser::prelude::{FromDer, X509Certificate};

type R<T> = std::result::Result<T, Refused>;

/// The most models a policy lists, roots a model has, and certificates a statement carries.
pub(crate) const MAX_MODELS: usize = 50;
const MAX_ROOTS: usize = 5;
const MAX_CHAIN: usize = 5;
const MAX_CERT: usize = 8 * 1024;

/// FIDO's AAGUID extension in an attestation certificate.
const AAGUID_EXT: &str = "1.3.6.1.4.1.45724.1.1.4";

/// An authenticator model a company allows.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Model {
    /// Its AAGUID, as a UUID (`ee882879-721c-4913-9775-3dfcce97072a`).
    pub aaguid: String,
    /// What people see on their passkey ("YubiKey 5 NFC").
    pub name: String,
    /// Its maker's attestation root certificates, PEM.
    pub roots: Vec<String>,
}

/// The policy: off, or only the listed models.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Only passkeys on a listed model, with a verified attestation.
    #[serde(default)]
    pub require: bool,
    #[serde(default)]
    pub models: Vec<Model>,
}

/// An AAGUID's 16 bytes as a UUID.
pub fn uuid(b: &[u8; 16]) -> String {
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// The DER of each certificate in a PEM text.
fn pem_certs(pem: &str) -> R<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    for block in x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes()) {
        let block = block.map_err(|_| Refused("a root isn't a PEM certificate"))?;
        if block.label != "CERTIFICATE" {
            return Err(Refused("a root isn't a PEM certificate"));
        }
        X509Certificate::from_der(&block.contents).map_err(|_| Refused("a root isn't a certificate"))?;
        out.push(block.contents);
    }
    if out.is_empty() {
        return Err(Refused("a model needs its maker's root certificate (PEM)"));
    }
    Ok(out)
}

impl Policy {
    /// A policy as an admin sent it, checked: UUIDs, names, roots that parse; at least one model
    /// when it's required.
    pub(crate) fn checked(mut self) -> R<Policy> {
        if self.models.len() > MAX_MODELS {
            return Err(Refused("at most 50 models"));
        }
        let mut seen = std::collections::HashSet::new();
        for m in &mut self.models {
            m.aaguid = m.aaguid.trim().to_ascii_lowercase();
            let hexed: String = m.aaguid.chars().filter(|c| *c != '-').collect();
            if hexed.len() != 32 || !hexed.chars().all(|c| c.is_ascii_hexdigit()) || hex::decode(&hexed).map(|b| uuid(&b.try_into().unwrap_or([0; 16]))).ok().as_deref() != Some(m.aaguid.as_str()) {
                return Err(Refused("an AAGUID is a UUID (8-4-4-4-12 hex digits)"));
            }
            if !seen.insert(m.aaguid.clone()) {
                return Err(Refused("each AAGUID once"));
            }
            m.name = m.name.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string();
            if m.name.is_empty() || m.name.chars().count() > 64 {
                return Err(Refused("a model's name is 1 to 64 characters"));
            }
            if m.roots.is_empty() || m.roots.len() > MAX_ROOTS {
                return Err(Refused("a model has 1 to 5 root certificates"));
            }
            for r in &m.roots {
                pem_certs(r)?;
            }
        }
        if self.require && self.models.is_empty() {
            return Err(Refused("requiring hardware keys needs at least one allowed model"));
        }
        Ok(self)
    }

    /// The model `aaguid` is, if the policy allows it.
    pub(crate) fn model(&self, aaguid: &[u8; 16]) -> Option<&Model> {
        let id = uuid(aaguid);
        self.models.iter().find(|m| m.aaguid == id)
    }
}

/// Accept a certificate whatever extended key usages it lists: attestation certificates have no
/// standard one, and the FIDO requirements below are what matter.
struct AnyUsage;

impl webpki::ExtendedKeyUsageValidator for AnyUsage {
    fn validate(&self, _iter: webpki::KeyPurposeIdIter<'_, '_>) -> Result<(), webpki::Error> {
        Ok(())
    }
}

/// Verify a `packed` attestation statement with a certificate (WebAuthn §8.2) for `model`, at
/// `now_s`: its signature over the authenticator data and the client data's hash, the
/// certificate's requirements and AAGUID, and its chain to one of the model's roots.
pub(crate) fn verify_packed(stmt: &Cbor, auth_data: &[u8], client_hash: &[u8; 32], aaguid: &[u8; 16], model: &Model, now_s: u64) -> R<()> {
    let alg = match stmt.get(&Cbor::Text("alg".into())) {
        Some(Cbor::Int(a)) => *a,
        _ => return Err(Refused("the attestation names no algorithm")),
    };
    let sig = match stmt.get(&Cbor::Text("sig".into())) {
        Some(Cbor::Bytes(s)) => s.clone(),
        _ => return Err(Refused("the attestation has no signature")),
    };
    let chain: Vec<Vec<u8>> = match stmt.get(&Cbor::Text("x5c".into())) {
        Some(Cbor::Array(certs)) if !certs.is_empty() && certs.len() <= MAX_CHAIN => certs
            .iter()
            .map(|c| match c {
                Cbor::Bytes(b) if b.len() <= MAX_CERT => Ok(b.clone()),
                _ => Err(Refused("the attestation's certificates aren't certificates")),
            })
            .collect::<R<_>>()?,
        // Self attestation: signed by the new key itself, which proves no model.
        _ => return Err(Refused("this authenticator doesn't prove its model (no attestation certificate): your company requires a hardware key it lists")),
    };
    let (_, leaf) = X509Certificate::from_der(&chain[0]).map_err(|_| Refused("the attestation certificate isn't one"))?;
    // §8.2.1: version 3, subject OU "Authenticator Attestation", not a CA, and the AAGUID
    // extension (when present) is the authenticator data's.
    if leaf.version().0 != 2 {
        return Err(Refused("the attestation certificate isn't X.509 version 3"));
    }
    if !leaf.subject().iter_organizational_unit().any(|ou| ou.as_str().ok() == Some("Authenticator Attestation")) {
        return Err(Refused("the attestation certificate isn't an authenticator's (its OU)"));
    }
    if leaf.basic_constraints().ok().flatten().is_some_and(|b| b.value.ca) {
        return Err(Refused("the attestation certificate is a CA's"));
    }
    for ext in leaf.extensions() {
        if ext.oid.to_id_string() == AAGUID_EXT {
            // An OCTET STRING holding the 16 bytes.
            let v = ext.value;
            if v.len() != 18 || v[0] != 0x04 || v[1] != 16 || &v[2..] != aaguid {
                return Err(Refused("the attestation certificate is for another model"));
            }
        }
    }
    // The statement's signature, by the certificate's key.
    let mut msg = auth_data.to_vec();
    msg.extend_from_slice(client_hash);
    let spki = leaf.public_key().subject_public_key.data.to_vec();
    let algo: &dyn ring::signature::VerificationAlgorithm = match alg as i64 {
        crate::passkeys::ES256 => &ring::signature::ECDSA_P256_SHA256_ASN1,
        crate::passkeys::RS256 => &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        crate::passkeys::EDDSA => &ring::signature::ED25519,
        _ => return Err(Refused("an attestation algorithm roll doesn't take")),
    };
    ring::signature::UnparsedPublicKey::new(algo, &spki).verify(&msg, &sig).map_err(|_| Refused("the attestation's signature is wrong"))?;
    // The chain, to one of the model's roots.
    let roots: Vec<Vec<u8>> = model.roots.iter().map(|p| pem_certs(p)).collect::<R<Vec<_>>>()?.into_iter().flatten().collect();
    let roots_der: Vec<rustls_pki_types::CertificateDer<'_>> = roots.iter().map(|d| rustls_pki_types::CertificateDer::from(d.as_slice())).collect();
    let anchors: Vec<_> = roots_der.iter().map(|c| webpki::anchor_from_trusted_cert(c).map_err(|_| Refused("a root certificate can't be a trust anchor"))).collect::<R<_>>()?;
    let leaf_der = rustls_pki_types::CertificateDer::from(chain[0].as_slice());
    let intermediates: Vec<rustls_pki_types::CertificateDer<'_>> = chain[1..].iter().map(|d| rustls_pki_types::CertificateDer::from(d.as_slice())).collect();
    let ee = webpki::EndEntityCert::try_from(&leaf_der).map_err(|_| Refused("the attestation certificate isn't one webpki reads"))?;
    let now = rustls_pki_types::UnixTime::since_unix_epoch(std::time::Duration::from_secs(now_s));
    ee.verify_for_usage(webpki::ALL_VERIFICATION_ALGS, &anchors, &intermediates, now, AnyUsage, None, None)
        .map_err(|_| Refused("the attestation certificate isn't from this model's maker (its chain doesn't reach the roots your company listed)"))?;
    Ok(())
}
