//! X.509 certificates and PEM private keys, for IAM's server and signing certificates and SAML
//! providers' metadata: parsing (RustCrypto's `x509-cert`, `pkcs1`, `pkcs8`, `sec1`), validity,
//! fingerprints, and whether a private key belongs to a certificate. Only structure: no
//! private-key operation is ever performed (an EC key without its public half is derived with
//! `p256` for P-256 only), and nothing here logs or echoes a key.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};
use x509_cert::Certificate;
use x509_cert::der::{Decode, Encode};

const RSA: &str = "1.2.840.113549.1.1.1";
const EC: &str = "1.2.840.10045.2.1";
const P256: &str = "1.2.840.10045.3.1.7";

/// A parsed certificate.
pub(crate) struct Cert {
    pub der: Vec<u8>,
    /// Unix ms.
    pub not_before: i64,
    pub not_after: i64,
    /// The public key algorithm's OID, its parameters (an EC key's curve) and the key's bits.
    pub key_alg: String,
    pub key_params: Option<String>,
    pub key: Vec<u8>,
}

impl Cert {
    /// SHA-256 of the DER certificate, hex.
    pub fn fingerprint(&self) -> String {
        hex::encode(Sha256::digest(&self.der))
    }

    /// The certificate's RSA public key as PKCS#1 `RSAPublicKey` DER (what `ring` verifies with).
    pub fn rsa_public_key(&self) -> Option<&[u8]> {
        (self.key_alg == RSA).then_some(self.key.as_slice())
    }

    /// Whether it's valid at `now` (Unix ms).
    pub fn valid_at(&self, now: i64) -> bool {
        self.not_before <= now && now <= self.not_after
    }
}

/// The base64 bodies of a PEM text's `label` blocks. `Err` when a block is malformed.
pub(crate) fn pem_blocks(text: &str, label: &str) -> Result<Vec<Vec<u8>>, ()> {
    let (begin, end) = (format!("-----BEGIN {label}-----"), format!("-----END {label}-----"));
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(&begin) {
        let after = &rest[i + begin.len()..];
        let j = after.find(&end).ok_or(())?;
        let body: String = after[..j].chars().filter(|c| !c.is_whitespace()).collect();
        out.push(STANDARD.decode(body).map_err(|_| ())?);
        rest = &after[j + end.len()..];
    }
    Ok(out)
}

fn unix_ms(t: x509_cert::time::Time) -> i64 {
    i64::try_from(t.to_unix_duration().as_millis()).unwrap_or(i64::MAX)
}

/// Parse a DER certificate.
pub(crate) fn parse_der(der: &[u8]) -> Result<Cert, ()> {
    let c = Certificate::from_der(der).map_err(|_| ())?;
    let tbs = &c.tbs_certificate;
    let spki = &tbs.subject_public_key_info;
    let key_params = spki.algorithm.parameters.as_ref().and_then(|p| p.decode_as::<x509_cert::der::asn1::ObjectIdentifier>().ok()).map(|o| o.to_string());
    Ok(Cert {
        der: c.to_der().map_err(|_| ())?,
        not_before: unix_ms(tbs.validity.not_before),
        not_after: unix_ms(tbs.validity.not_after),
        key_alg: spki.algorithm.oid.to_string(),
        key_params,
        key: spki.subject_public_key.raw_bytes().to_vec(),
    })
}

/// Exactly one PEM certificate (whitespace around it is fine).
pub(crate) fn parse_pem(text: &str) -> Result<Cert, ()> {
    let blocks = pem_blocks(text, "CERTIFICATE")?;
    let [der] = blocks.as_slice() else { return Err(()) };
    parse_der(der)
}

/// A chain: one or more PEM certificates.
pub(crate) fn parse_chain(text: &str) -> Result<Vec<Cert>, ()> {
    let blocks = pem_blocks(text, "CERTIFICATE")?;
    if blocks.is_empty() {
        return Err(());
    }
    blocks.iter().map(|b| parse_der(b)).collect()
}

/// A private key's public half, in the form a certificate carries it.
enum PublicHalf {
    /// Modulus and exponent (big-endian, no leading zeros).
    Rsa(Vec<u8>, Vec<u8>),
    /// Curve OID and the uncompressed point.
    Ec(String, Vec<u8>),
}

fn rsa_half(der: &[u8]) -> Result<PublicHalf, ()> {
    let k = pkcs1::RsaPrivateKey::from_der(der).map_err(|_| ())?;
    Ok(PublicHalf::Rsa(k.modulus.as_bytes().to_vec(), k.public_exponent.as_bytes().to_vec()))
}

fn ec_half(der: &[u8], curve: Option<String>, outer_public: Option<&[u8]>) -> Result<PublicHalf, ()> {
    let k = sec1::EcPrivateKey::from_der(der).map_err(|_| ())?;
    let curve = curve.or_else(|| k.parameters.and_then(|p| p.named_curve()).map(|o| o.to_string())).ok_or(())?;
    let point = match k.public_key.or(outer_public) {
        Some(p) => p.to_vec(),
        None if curve == P256 => {
            let sk = p256::SecretKey::from_slice(k.private_key).map_err(|_| ())?;
            p256::elliptic_curve::sec1::ToEncodedPoint::to_encoded_point(&sk.public_key(), false).as_bytes().to_vec()
        }
        None => return Err(()),
    };
    Ok(PublicHalf::Ec(curve, point))
}

fn public_half(pem: &str) -> Result<PublicHalf, ()> {
    if let [der] = pem_blocks(pem, "RSA PRIVATE KEY")?.as_slice() {
        return rsa_half(der);
    }
    if let [der] = pem_blocks(pem, "EC PRIVATE KEY")?.as_slice() {
        return ec_half(der, None, None);
    }
    if let [der] = pem_blocks(pem, "PRIVATE KEY")?.as_slice() {
        let info = pkcs8::PrivateKeyInfo::from_der(der).map_err(|_| ())?;
        return match info.algorithm.oid.to_string().as_str() {
            RSA => rsa_half(info.private_key),
            EC => {
                let curve = info.algorithm.parameters.and_then(|p| p.decode_as::<pkcs8::ObjectIdentifier>().ok()).map(|o| o.to_string());
                ec_half(info.private_key, curve, info.public_key)
            }
            _ => Err(()),
        };
    }
    Err(())
}

/// Whether the PEM private key `key` belongs to `cert`. `Err`: the key isn't one (an encrypted
/// key, an unsupported algorithm, garbage).
pub(crate) fn key_matches(cert: &Cert, key: &str) -> Result<bool, ()> {
    Ok(match public_half(key)? {
        PublicHalf::Rsa(n, e) => cert.key_alg == RSA && pkcs1::RsaPublicKey::from_der(&cert.key).is_ok_and(|p| p.modulus.as_bytes() == n.as_slice() && p.public_exponent.as_bytes() == e.as_slice()),
        PublicHalf::Ec(curve, point) => cert.key_alg == EC && cert.key_params.as_deref() == Some(curve.as_str()) && cert.key == point,
    })
}

/// The certificate as PEM (64-character lines).
#[cfg(test)]
pub(crate) fn to_pem(der: &[u8]) -> String {
    let b = STANDARD.encode(der);
    let lines: Vec<&str> = b.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect();
    format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_signed(key: &rcgen::KeyPair) -> String {
        let params = rcgen::CertificateParams::new(vec!["example.com".into()]).unwrap();
        params.self_signed(key).unwrap().pem()
    }

    #[test]
    fn certificates_and_keys() {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let other = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let pem = self_signed(&key);
        let c = parse_pem(&pem).unwrap();
        assert_eq!(c.key_alg, EC);
        assert_eq!(c.key_params.as_deref(), Some(P256));
        assert!(c.valid_at(c.not_before) && !c.valid_at(c.not_after + 1));
        assert_eq!(c.fingerprint().len(), 64);
        assert!(key_matches(&c, &key.serialize_pem()).unwrap());
        assert!(!key_matches(&c, &other.serialize_pem()).unwrap());
        assert!(key_matches(&c, "not a key").is_err());
        assert!(parse_pem("not a certificate").is_err());
        assert!(parse_pem(&format!("{pem}{pem}")).is_err(), "exactly one");
        assert_eq!(parse_chain(&format!("{pem}\n{pem}")).unwrap().len(), 2);
        assert!(parse_pem(&to_pem(&c.der)).is_ok());
        assert!(c.rsa_public_key().is_none());
    }
}
