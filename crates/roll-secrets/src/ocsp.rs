//! A project CA's OCSP responder (docs/secrets.md, "Certificates"; RFC 6960), beside its CRL:
//! for clients that ask about one certificate rather than fetching a list. Served without a token
//! (`/v1/secrets/ocsp/{ca}`), as a CRL is. The CA signs each answer itself (no delegated
//! responder); a certificate is `good` while it's on the CA's list and not revoked, `revoked` with
//! its time once revoked, and `unknown` when the CA didn't sign it, it has expired, or the request
//! names another issuer.
//!
//! The route is public, so its cost is bounded: nonces are not echoed (RFC 8954 lets a responder
//! leave them out, as large public CAs do), so every answer can be kept; answers are kept a minute
//! per node by what the request asks about (a revocation drops the CA's); and a CA signs at most
//! [`SIGN_PER_SECOND`] fresh answers a second, answering `tryLater` beyond that.

use crate::{Error, Result, Secrets};
use code_sql::{OptionalExtension, params};
use der::asn1::{BitString, Null, OctetString};
use der::oid::ObjectIdentifier;
use der::{Decode, Encode};
use std::sync::Arc;
use x509_cert::spki::AlgorithmIdentifierOwned;
use x509_ocsp::{BasicOcspResponse, CertStatus, OcspGeneralizedTime, OcspRequest, OcspResponse, ResponderId, ResponseData, RevokedInfo, SingleResponse};

/// The most of a request read, and of the certificates one asks about.
pub(crate) const REQUEST_MAX: usize = 16 * 1024;
const ASKED_MAX: usize = 10;
/// Fresh answers one CA signs a second, per node; more get `tryLater`.
const SIGN_PER_SECOND: u32 = 50;

const SHA1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.14.3.2.26");
const SHA256: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1");
const ECDSA_SHA256: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2");

/// The Authority Information Access extension's content naming the OCSP responder (RFC 5280
/// 4.2.2.1): `SEQUENCE { SEQUENCE { id-ad-ocsp, [6] url } }`.
pub(crate) fn aia(url: &str) -> Vec<u8> {
    let mut desc = vec![0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x30, 0x01];
    desc.push(0x86);
    push_len(&mut desc, url.len());
    desc.extend_from_slice(url.as_bytes());
    let mut seq = vec![0x30];
    push_len(&mut seq, desc.len());
    seq.extend(desc);
    let mut out = vec![0x30];
    push_len(&mut out, seq.len());
    out.extend(seq);
    out
}

fn push_len(out: &mut Vec<u8>, n: usize) {
    if n < 0x80 {
        out.push(n as u8);
    } else {
        let bytes: Vec<u8> = n.to_be_bytes().into_iter().skip_while(|b| *b == 0).collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
}

fn digest(alg: &ObjectIdentifier, data: &[u8]) -> Option<Vec<u8>> {
    let algorithm = if *alg == SHA1 {
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY
    } else if *alg == SHA256 {
        &ring::digest::SHA256
    } else {
        return None;
    };
    Some(ring::digest::digest(algorithm, data).as_ref().to_vec())
}

fn time(ms: i64) -> Result<OcspGeneralizedTime> {
    let d = der::DateTime::from_unix_duration(std::time::Duration::from_millis(ms.max(0) as u64)).map_err(|e| Error::Internal(format!("a time: {e}")))?;
    Ok(OcspGeneralizedTime::from(d))
}

impl Secrets {
    /// The CA's OCSP answer (DER) to a request (DER). A request that doesn't parse gets
    /// `malformedRequest`, as RFC 6960 says; a CA that doesn't exist is a 404.
    pub fn ocsp(&self, id: &str, request: &[u8]) -> Result<(Arc<Vec<u8>>, bool)> {
        let malformed = || -> Result<(Arc<Vec<u8>>, bool)> { Ok((Arc::new(OcspResponse::malformed_request().to_der().map_err(|e| Error::Internal(e.to_string()))?), false)) };
        if request.len() > REQUEST_MAX {
            return malformed();
        }
        let Ok(req) = OcspRequest::from_der(request) else { return malformed() };
        if req.tbs_request.request_list.is_empty() || req.tbs_request.request_list.len() > ASKED_MAX {
            return malformed();
        }
        // Kept by what's asked (the certificates), not the request's bytes: extensions or a nonce
        // don't make a new answer.
        let asked = req.tbs_request.request_list.to_der().map_err(|e| Error::Internal(e.to_string()))?;
        let key = (id.to_string(), ring::digest::digest(&ring::digest::SHA256, &asked).as_ref().to_vec());
        let now = self.db.now_ms()?;
        let generation = {
            let cache = self.crls.lock().expect("crls");
            if let Some((at, _, der)) = cache.ocsp.get(&key)
                && now - at < 60_000
            {
                return Ok((der.clone(), true));
            }
            cache.revoked.get(id).copied().unwrap_or_default()
        };
        // Counted only for a CA that exists (made-up ids grow nothing), and before its key is
        // opened (going over the cap costs no unsealing).
        let exists: Option<i64> = self.db.read(|c| c.query_row("SELECT 1 FROM cas WHERE id = ?", [id], |r| r.get(0)).optional())?;
        if exists.is_none() {
            return Err(Error::NotFound(format!("no CA {id}")));
        }
        {
            let mut cache = self.crls.lock().expect("crls");
            let second = now / 1000;
            if cache.signing.len() > 1_000 {
                cache.signing.retain(|_, (at, _)| *at == second);
            }
            let (at, n) = cache.signing.entry(id.to_string()).or_insert((second, 0));
            if *at != second {
                (*at, *n) = (second, 0);
            }
            *n += 1;
            if *n > SIGN_PER_SECOND {
                return Ok((Arc::new(OcspResponse::try_later().to_der().map_err(|e| Error::Internal(e.to_string()))?), false));
            }
        }
        let (ca_pem, ca_key, _) = self.pki()?.open(id).map_err(|_| Error::NotFound(format!("no CA {id}")))?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(ca_pem.as_bytes()).map_err(|_| Error::Internal("the CA's certificate doesn't parse".into()))?;
        let ca = x509_cert::Certificate::from_der(&pem.contents).map_err(|_| Error::Internal("the CA's certificate doesn't parse".into()))?;
        let name = ca.tbs_certificate.subject.to_der().map_err(|e| Error::Internal(e.to_string()))?;
        let key_bits = ca.tbs_certificate.subject_public_key_info.subject_public_key.raw_bytes().to_vec();
        let mut responses = vec![];
        for r in &req.tbs_request.request_list {
            let cid = &r.req_cert;
            let ours = digest(&cid.hash_algorithm.oid, &name).is_some_and(|h| h == cid.issuer_name_hash.as_bytes()) && digest(&cid.hash_algorithm.oid, &key_bits).is_some_and(|h| h == cid.issuer_key_hash.as_bytes());
            let serial: String = cid.serial_number.as_bytes().iter().skip_while(|b| **b == 0).map(|b| format!("{b:02x}")).collect();
            let row: Option<(Option<i64>, i64)> = if ours {
                self.db.read(|c| c.query_row("SELECT revoked_at, not_after FROM ca_certs WHERE ca_id = ? AND serial = ?", params![id, serial], |r| Ok((r.get(0)?, r.get(1)?))).optional())?
            } else {
                None
            };
            let status = match row {
                Some((Some(at), _)) => CertStatus::revoked(RevokedInfo { revocation_time: time(at)?, revocation_reason: None }),
                Some((None, end)) if end >= now => CertStatus::Good(Null),
                _ => CertStatus::unknown(),
            };
            // Good for ten minutes: how long a revocation can take to reach a client that keeps answers.
            responses.push(SingleResponse { cert_id: cid.clone(), cert_status: status, this_update: time(now - 60_000)?, next_update: Some(time(now + 600_000)?), single_extensions: None });
        }
        let key_hash = digest(&SHA1, &key_bits).expect("SHA-1");
        let tbs = ResponseData {
            version: Default::default(),
            responder_id: ResponderId::ByKey(OctetString::new(key_hash).map_err(|e| Error::Internal(e.to_string()))?),
            produced_at: time(now)?,
            responses,
            response_extensions: None,
        };
        let tbs_der = tbs.to_der().map_err(|e| Error::Internal(e.to_string()))?;
        let ca_key = rcgen::KeyPair::from_pem(ca_key.expose()).map_err(|e| Error::Internal(format!("the CA's key: {e}")))?;
        let signature = rcgen::SigningKey::sign(&ca_key, &tbs_der).map_err(|e| Error::Internal(format!("signing an OCSP answer: {e}")))?;
        let basic = BasicOcspResponse {
            tbs_response_data: tbs,
            signature_algorithm: AlgorithmIdentifierOwned { oid: ECDSA_SHA256, parameters: None },
            signature: BitString::from_bytes(&signature).map_err(|e| Error::Internal(e.to_string()))?,
            certs: None,
        };
        let der = Arc::new(OcspResponse::successful(basic).and_then(|r| r.to_der()).map_err(|e| Error::Internal(e.to_string()))?);
        let mut cache = self.crls.lock().expect("crls");
        if cache.ocsp.len() >= 10_000 {
            cache.ocsp.retain(|_, (at, _, _)| now - *at < 60_000);
        }
        // A revocation meanwhile, or a full cache: served once, kept never.
        if cache.revoked.get(id).copied().unwrap_or_default() == generation && cache.ocsp.len() < 10_000 {
            cache.ocsp.insert(key, (now, generation, der.clone()));
        }
        Ok((der, true))
    }
}
