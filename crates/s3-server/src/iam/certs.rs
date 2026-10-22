//! Server certificates, signing certificates and SSH public keys: stored and returned as IAM
//! does. They're IAM-only objects (nothing in roll serves TLS or verifies SSH with them).
//!
//! - Server certificates: the certificate must parse and be valid now, its chain must parse, and
//!   the private key must match it (`KeyPairMismatch`). The private key isn't kept: IAM never
//!   returns it, and nothing in roll would use it.
//! - Signing certificates (a user's, or the root user's without `UserName`): two per user, no
//!   duplicates within the account.
//! - SSH public keys: RSA (2048-16384 bits), `ssh-rsa` or PEM, five per user; `GetSSHPublicKey`
//!   returns them in either encoding.

use super::*;
use crate::x509;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use md5::{Digest, Md5};
use s3_store::iam::{self as si, ServerCert, SigningCert, SshKey, User, kind};

pub(super) async fn run(c: &Call<'_>, action: &str) -> Result<Option<String>, StsError> {
    Ok(Some(match action {
        "UploadServerCertificate" => upload_server(c).await?,
        "GetServerCertificate" => get_server(c)?,
        "ListServerCertificates" => list_server(c)?,
        "UpdateServerCertificate" => return update_server(c).await.map(|_| None),
        "DeleteServerCertificate" => return delete_server(c).await.map(|_| None),
        "TagServerCertificate" => return tag_server(c, true).await.map(|_| None),
        "UntagServerCertificate" => return tag_server(c, false).await.map(|_| None),
        "ListServerCertificateTags" => list_server_tags(c)?,
        "UploadSigningCertificate" => upload_signing(c).await?,
        "ListSigningCertificates" => list_signing(c)?,
        "UpdateSigningCertificate" => return update_signing(c, false).await.map(|_| None),
        "DeleteSigningCertificate" => return update_signing(c, true).await.map(|_| None),
        "UploadSSHPublicKey" => upload_ssh(c).await?,
        "GetSSHPublicKey" => get_ssh(c)?,
        "ListSSHPublicKeys" => list_ssh(c)?,
        "UpdateSSHPublicKey" => return update_ssh(c, false).await.map(|_| None),
        "DeleteSSHPublicKey" => return update_ssh(c, true).await.map(|_| None),
        _ => unreachable!("routed here"),
    }))
}

fn malformed_cert() -> StsError {
    StsError::new("MalformedCertificate", 400, "The request was rejected because the certificate was malformed or expired.")
}

/// A random id of `n` upper-case letters and digits after `prefix`.
fn random_id(prefix: &str, n: usize) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("os randomness");
    format!("{prefix}{}", b.iter().map(|x| A[*x as usize % A.len()] as char).collect::<String>())
}

fn status_param(c: &Call) -> Result<bool, StsError> {
    match c.p.required("Status", "status")? {
        "Active" => Ok(true),
        "Inactive" => Ok(false),
        v => Err(validation(format!("1 validation error detected: Value '{v}' at 'status' failed to satisfy constraint: Member must satisfy enum value set: [Active, Inactive]"))),
    }
}

fn status(active: bool) -> &'static str {
    if active { "Active" } else { "Inactive" }
}

/// Edit a user's document in one transaction; `f`'s errors come back as they are.
async fn edit_user(c: &Call<'_>, name: &str, f: impl FnOnce(&mut User) -> Result<(), StsError> + Send) -> Result<(), StsError> {
    let (acct, name) = (c.account.clone(), name.to_string());
    let mut err = None;
    let r = c
        .store
        .iam_write(|t| {
            let Some(mut u) = t.get::<User>(&acct, kind::USER, &name)? else { return Err(StoreError::NotFound(format!("The user with name {name} cannot be found."))) };
            if let Err(e) = f(&mut u) {
                err = Some(e);
                return Err(StoreError::InvalidRequest("refused".into()));
            }
            t.put(&acct, kind::USER, &name, &u)
        })
        .await;
    match (r, err) {
        (_, Some(e)) => Err(e),
        (r, None) => r.map_err(store_error),
    }
}

// ---- Server certificates ----

fn server_arn(c: &Call, s: &ServerCert) -> String {
    c.arn("server-certificate", &s.path, &s.name)
}

fn server_meta(c: &Call, s: &ServerCert) -> String {
    format!(
        "{}{}{}{}{}{}",
        el("Path", if s.path.is_empty() { "/" } else { &s.path }),
        el("ServerCertificateName", &s.name),
        el("ServerCertificateId", &s.id),
        el("Arn", &server_arn(c, s)),
        el("UploadDate", &date(s.uploaded_at)),
        el("Expiration", &date(s.expires_at))
    )
}

fn no_server(name: &str) -> StsError {
    no_such(format!("The Server Certificate with name {name} cannot be found."))
}

fn server(c: &Call, name: &str) -> Result<ServerCert, StsError> {
    c.store.iam_read(|t| t.get(&c.account, kind::SERVER_CERT, name)).map_err(store_error)?.ok_or_else(|| no_server(name))
}

async fn upload_server(c: &Call<'_>) -> Result<String, StsError> {
    let name = c.name("ServerCertificateName", 128)?.to_string();
    let path = c.path("Path")?;
    let body = c.p.required("CertificateBody", "certificateBody")?.trim().to_string();
    let key = zeroize::Zeroizing::new(c.p.required("PrivateKey", "privateKey")?.to_string());
    let chain = c.p.get("CertificateChain").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let tags = c.tags()?;
    let s = ServerCert { name: name.clone(), id: random_id("ASCA", 17), path, body, chain, uploaded_at: c.store.now(), expires_at: 0, tags };
    c.allow("UploadServerCertificate", &server_arn(c, &s))?;
    let cert = x509::parse_pem(&s.body).map_err(|_| malformed_cert())?;
    if !cert.valid_at(c.store.now()) {
        return Err(malformed_cert());
    }
    if let Some(ch) = &s.chain {
        x509::parse_chain(ch).map_err(|_| malformed_cert())?;
    }
    match x509::key_matches(&cert, &key) {
        Ok(true) => {}
        Ok(false) => return Err(StsError::new("KeyPairMismatch", 400, "The request was rejected because the public key certificate and the private key do not match.")),
        Err(()) => return Err(StsError::new("MalformedCertificate", 400, "The request was rejected because the private key was malformed, encrypted or of an unsupported type.")),
    }
    let s = ServerCert { expires_at: cert.not_after, ..s };
    let (acct, saved) = (c.account.clone(), s.clone());
    c.store
        .iam_write(move |t| {
            if t.exists(&acct, kind::SERVER_CERT, &name)? {
                return Err(StoreError::Conflict(format!("The Server Certificate with name {name} already exists.")));
            }
            if t.count(&acct, kind::SERVER_CERT)? >= si::MAX_SERVER_CERTS {
                return Err(StoreError::LimitExceeded(format!("Cannot exceed quota for ServerCertificatesPerAccount: {}", si::MAX_SERVER_CERTS)));
            }
            t.put(&acct, kind::SERVER_CERT, &name, &saved)
        })
        .await
        .map_err(store_error)?;
    Ok(format!("<ServerCertificateMetadata>{}</ServerCertificateMetadata>{}", server_meta(c, &s), tags_xml(&s.tags)))
}

fn get_server(c: &Call) -> Result<String, StsError> {
    let s = server(c, c.name("ServerCertificateName", 128)?)?;
    c.allow("GetServerCertificate", &server_arn(c, &s))?;
    Ok(format!(
        "<ServerCertificate><ServerCertificateMetadata>{}</ServerCertificateMetadata>{}{}{}</ServerCertificate>",
        server_meta(c, &s),
        el("CertificateBody", &s.body),
        s.chain.as_deref().map(|ch| el("CertificateChain", ch)).unwrap_or_default(),
        tags_xml(&s.tags)
    ))
}

fn list_server(c: &Call) -> Result<String, StsError> {
    c.allow("ListServerCertificates", &format!("arn:aws:iam::{}:server-certificate/*", c.account))?;
    let prefix = c.p.get("PathPrefix").unwrap_or("/").to_string();
    let all: Vec<ServerCert> = c.store.iam_read(|t| t.list(&c.account, kind::SERVER_CERT)).map_err(store_error)?;
    let all: Vec<ServerCert> = all.into_iter().filter(|s| (if s.path.is_empty() { "/" } else { s.path.as_str() }).starts_with(&prefix)).collect();
    page(c.p, all, |s| s.name.to_lowercase(), |s| server_meta(c, s), "ServerCertificateMetadataList")
}

async fn edit_server(c: &Call<'_>, s: &ServerCert, f: impl FnOnce(&mut ServerCert) -> Result<(), StsError> + Send) -> Result<(), StsError> {
    let (acct, name) = (c.account.clone(), s.name.clone());
    let mut err = None;
    let r = c
        .store
        .iam_write(|t| {
            let Some(mut s) = t.get::<ServerCert>(&acct, kind::SERVER_CERT, &name)? else { return Err(StoreError::NotFound(format!("The Server Certificate with name {name} cannot be found."))) };
            if let Err(e) = f(&mut s) {
                err = Some(e);
                return Err(StoreError::InvalidRequest("refused".into()));
            }
            if !s.name.eq_ignore_ascii_case(&name) {
                if t.exists(&acct, kind::SERVER_CERT, &s.name)? {
                    return Err(StoreError::Conflict(format!("The Server Certificate with name {} already exists.", s.name)));
                }
                t.delete(&acct, kind::SERVER_CERT, &name)?;
            }
            let key = s.name.clone();
            t.put(&acct, kind::SERVER_CERT, &key, &s)
        })
        .await;
    match (r, err) {
        (_, Some(e)) => Err(e),
        (r, None) => r.map_err(store_error),
    }
}

async fn update_server(c: &Call<'_>) -> Result<(), StsError> {
    let s = server(c, c.name("ServerCertificateName", 128)?)?;
    c.allow("UpdateServerCertificate", &server_arn(c, &s))?;
    let new_name = match c.p.get("NewServerCertificateName") {
        Some(_) => Some(c.name("NewServerCertificateName", 128)?.to_string()),
        None => None,
    };
    let new_path = match c.p.get("NewPath") {
        Some(_) => Some(c.path("NewPath")?),
        None => None,
    };
    let after = ServerCert { name: new_name.clone().unwrap_or_else(|| s.name.clone()), path: new_path.clone().unwrap_or_else(|| s.path.clone()), ..s.clone() };
    c.allow("UpdateServerCertificate", &server_arn(c, &after))?;
    edit_server(c, &s, move |x| {
        if let Some(n) = new_name {
            x.name = n;
        }
        if let Some(p) = new_path {
            x.path = p;
        }
        Ok(())
    })
    .await
}

async fn delete_server(c: &Call<'_>) -> Result<(), StsError> {
    let s = server(c, c.name("ServerCertificateName", 128)?)?;
    c.allow("DeleteServerCertificate", &server_arn(c, &s))?;
    let (acct, name) = (c.account.clone(), s.name.clone());
    c.store.iam_write(move |t| t.delete(&acct, kind::SERVER_CERT, &name).map(|_| ())).await.map_err(store_error)
}

async fn tag_server(c: &Call<'_>, add: bool) -> Result<(), StsError> {
    let s = server(c, c.name("ServerCertificateName", 128)?)?;
    c.allow(if add { "TagServerCertificate" } else { "UntagServerCertificate" }, &server_arn(c, &s))?;
    if add {
        let tags = c.tags()?;
        edit_server(c, &s, |x| merge_tags(&mut x.tags, tags)).await
    } else {
        let keys = c.p.list("TagKeys")?;
        edit_server(c, &s, move |x| {
            x.tags.retain(|(k, _)| !keys.iter().any(|y| y.eq_ignore_ascii_case(k)));
            Ok(())
        })
        .await
    }
}

fn list_server_tags(c: &Call) -> Result<String, StsError> {
    let s = server(c, c.name("ServerCertificateName", 128)?)?;
    c.allow("ListServerCertificateTags", &server_arn(c, &s))?;
    page(c.p, s.tags, |(k, _)| k.clone(), |(k, v)| format!("{}{}", el("Key", k), el("Value", v)), "Tags")
}

// ---- Signing certificates ----

/// Whose certificates a call is about: `UserName`'s, or the caller's own (`None`: the root
/// user's), and the ARN its permission is checked on.
fn cert_owner(c: &Call) -> Result<(Option<User>, String), StsError> {
    match c.user_or_self()? {
        Some(n) => {
            let u = super::users::user(c, &n)?;
            let arn = c.arn("user", &u.path, &u.name);
            Ok((Some(u), arn))
        }
        None => Ok((None, crate::identity::root_arn(&c.account))),
    }
}

fn cert_xml(owner: &Option<User>, s: &SigningCert) -> String {
    format!(
        "{}{}{}{}{}",
        owner.as_ref().map(|u| el("UserName", &u.name)).unwrap_or_default(),
        el("CertificateId", &s.id),
        el("CertificateBody", &s.body),
        el("Status", status(s.active)),
        el("UploadDate", &date(s.uploaded_at))
    )
}

fn certs_of(c: &Call, owner: &Option<User>) -> Result<Vec<SigningCert>, StsError> {
    let all: Vec<SigningCert> = c.store.iam_read(|t| t.list(&c.account, kind::SIGNING_CERT)).map_err(store_error)?;
    Ok(all.into_iter().filter(|s| match owner {
        Some(u) => s.user.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(&u.name)),
        None => s.user.is_none(),
    }).collect())
}

async fn upload_signing(c: &Call<'_>) -> Result<String, StsError> {
    let (owner, arn) = cert_owner(c)?;
    c.allow("UploadSigningCertificate", &arn)?;
    let body = c.p.required("CertificateBody", "certificateBody")?.trim().to_string();
    let cert = x509::parse_pem(&body).map_err(|_| malformed_cert())?;
    if !cert.valid_at(c.store.now()) {
        return Err(malformed_cert());
    }
    let s = SigningCert { id: random_id("", 24), user: owner.as_ref().map(|u| u.name.clone()), body, fingerprint: cert.fingerprint(), active: true, uploaded_at: c.store.now() };
    let (acct, saved) = (c.account.clone(), s.clone());
    let mut err = None;
    let r = c
        .store
        .iam_write(|t| {
            let all: Vec<SigningCert> = t.list(&acct, kind::SIGNING_CERT)?;
            if all.iter().any(|x| x.fingerprint == saved.fingerprint) {
                err = Some(StsError::new("DuplicateCertificate", 409, "The request was rejected because the same certificate is associated with an IAM user in the account."));
                return Err(StoreError::InvalidRequest("refused".into()));
            }
            let mine = all.iter().filter(|x| match &saved.user {
                Some(u) => x.user.as_deref().is_some_and(|y| y.eq_ignore_ascii_case(u)),
                None => x.user.is_none(),
            });
            if mine.count() >= si::MAX_SIGNING_CERTS_PER_USER {
                return Err(StoreError::LimitExceeded(format!("Cannot exceed quota for SigningCertificatesPerUser: {}", si::MAX_SIGNING_CERTS_PER_USER)));
            }
            t.put(&acct, kind::SIGNING_CERT, &saved.id, &saved)
        })
        .await;
    if let Some(e) = err {
        return Err(e);
    }
    r.map_err(store_error)?;
    Ok(format!("<Certificate>{}</Certificate>", cert_xml(&owner, &s)))
}

fn list_signing(c: &Call) -> Result<String, StsError> {
    let (owner, arn) = cert_owner(c)?;
    c.allow("ListSigningCertificates", &arn)?;
    let certs = certs_of(c, &owner)?;
    page(c.p, certs, |s| s.id.clone(), |s| cert_xml(&owner, s), "Certificates")
}

async fn update_signing(c: &Call<'_>, delete: bool) -> Result<(), StsError> {
    let (owner, arn) = cert_owner(c)?;
    c.allow(if delete { "DeleteSigningCertificate" } else { "UpdateSigningCertificate" }, &arn)?;
    let id = c.p.required("CertificateId", "certificateId")?.to_string();
    let active = if delete { None } else { Some(status_param(c)?) };
    let found = certs_of(c, &owner)?.into_iter().find(|s| s.id == id).ok_or_else(|| no_such(format!("The Certificate with id {id} cannot be found.")))?;
    let acct = c.account.clone();
    c.store
        .iam_write(move |t| match active {
            None => t.delete(&acct, kind::SIGNING_CERT, &found.id).map(|_| ()),
            Some(a) => t.put(&acct, kind::SIGNING_CERT, &found.id, &SigningCert { active: a, ..found.clone() }),
        })
        .await
        .map_err(store_error)
}

// ---- SSH public keys ----

/// An RSA public key: (modulus, exponent), big-endian without leading zeros.
type RsaKey = (Vec<u8>, Vec<u8>);

fn invalid_key() -> StsError {
    StsError::new("InvalidPublicKey", 400, "The request was rejected because the public key is malformed or otherwise invalid.")
}

fn unrecognized() -> StsError {
    StsError::new("UnrecognizedPublicKeyEncoding", 400, "The request was rejected because the public key encoding format is unsupported or unrecognized.")
}

fn strip(b: &[u8]) -> Vec<u8> {
    let i = b.iter().position(|x| *x != 0).unwrap_or(b.len());
    b[i..].to_vec()
}

/// An SSH wire-format string (`u32` length, bytes), bounded by what's there.
fn ssh_string<'a>(b: &mut &'a [u8]) -> Option<&'a [u8]> {
    let n = u32::from_be_bytes(b.get(..4)?.try_into().ok()?) as usize;
    let s = b.get(4..4usize.checked_add(n)?)?;
    *b = &b[4 + n..];
    Some(s)
}

/// Parse an `ssh-rsa` key or a PEM RSA public key (`PUBLIC KEY` or `RSA PUBLIC KEY`).
fn parse_ssh(body: &str) -> Result<RsaKey, StsError> {
    let body = body.trim();
    if let Some(rest) = body.strip_prefix("ssh-rsa ") {
        let b64 = rest.split_whitespace().next().ok_or_else(invalid_key)?;
        let blob = STANDARD.decode(b64).map_err(|_| invalid_key())?;
        let mut b = blob.as_slice();
        if ssh_string(&mut b) != Some(b"ssh-rsa") {
            return Err(invalid_key());
        }
        let e = ssh_string(&mut b).ok_or_else(invalid_key)?;
        let n = ssh_string(&mut b).ok_or_else(invalid_key)?;
        if !b.is_empty() {
            return Err(invalid_key());
        }
        return Ok((strip(n), strip(e)));
    }
    let decode = |der: &[u8]| pkcs1::der::Decode::from_der(der).map(|k: pkcs1::RsaPublicKey| (k.modulus.as_bytes().to_vec(), k.public_exponent.as_bytes().to_vec())).map_err(|_| invalid_key());
    if let Ok([der]) = x509::pem_blocks(body, "RSA PUBLIC KEY").as_deref() {
        return decode(der);
    }
    if let Ok([der]) = x509::pem_blocks(body, "PUBLIC KEY").as_deref() {
        let spki = <x509_cert::spki::SubjectPublicKeyInfoRef as pkcs1::der::Decode>::from_der(der).map_err(|_| invalid_key())?;
        if spki.algorithm.oid.to_string() != "1.2.840.113549.1.1.1" {
            return Err(unrecognized());
        }
        return decode(spki.subject_public_key.raw_bytes());
    }
    if body.starts_with("ssh-") || body.starts_with("ecdsa-") {
        return Err(unrecognized());
    }
    Err(unrecognized())
}

fn ssh_blob((n, e): &RsaKey) -> Vec<u8> {
    let mut out = Vec::new();
    let mut put = |b: &[u8]| {
        out.extend_from_slice(&(b.len() as u32).to_be_bytes());
        out.extend_from_slice(b);
    };
    let mpint = |b: &[u8]| if b.first().is_some_and(|x| x & 0x80 != 0) { [&[0u8][..], b].concat() } else { b.to_vec() };
    put(b"ssh-rsa");
    put(&mpint(e));
    put(&mpint(n));
    out
}

fn der_len(n: usize) -> Vec<u8> {
    if n < 128 {
        return vec![n as u8];
    }
    let b: Vec<u8> = n.to_be_bytes().iter().copied().skip_while(|x| *x == 0).collect();
    [vec![0x80 | b.len() as u8], b].concat()
}

fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    [vec![tag], der_len(content.len()), content.to_vec()].concat()
}

/// The key as PEM (`SubjectPublicKeyInfo`).
fn ssh_pem((n, e): &RsaKey) -> String {
    let int = |b: &[u8]| der(0x02, &if b.first().is_some_and(|x| x & 0x80 != 0) { [&[0u8][..], b].concat() } else { b.to_vec() });
    let rsa = der(0x30, &[int(n), int(e)].concat());
    let alg = [0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00];
    let spki = der(0x30, &[alg.to_vec(), der(0x03, &[vec![0], rsa].concat())].concat());
    let b = STANDARD.encode(spki);
    let lines: Vec<&str> = b.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect();
    format!("-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n", lines.join("\n"))
}

fn fingerprint(k: &RsaKey) -> String {
    Md5::digest(ssh_blob(k)).iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

fn ssh_xml(u: &User, k: &SshKey, body: &str) -> String {
    format!(
        "<SSHPublicKey>{}{}{}{}{}{}</SSHPublicKey>",
        el("UserName", &u.name),
        el("SSHPublicKeyId", &k.id),
        el("Fingerprint", &k.fingerprint),
        el("SSHPublicKeyBody", body),
        el("Status", status(k.active)),
        el("UploadDate", &date(k.uploaded_at))
    )
}

fn ssh_user(c: &Call) -> Result<User, StsError> {
    let u = super::users::user(c, c.name("UserName", 64)?)?;
    Ok(u)
}

async fn upload_ssh(c: &Call<'_>) -> Result<String, StsError> {
    let u = ssh_user(c)?;
    c.allow("UploadSSHPublicKey", &c.arn("user", &u.path, &u.name))?;
    let body = c.p.required("SSHPublicKeyBody", "sSHPublicKeyBody")?.trim().to_string();
    let key = parse_ssh(&body)?;
    let bits = key.0.len() * 8 - key.0.first().map_or(8, |b| b.leading_zeros() as usize);
    if !(2048..=16384).contains(&bits) {
        return Err(invalid_key());
    }
    let k = SshKey { id: si::entity_id("APKA"), fingerprint: fingerprint(&key), body: body.clone(), active: true, uploaded_at: c.store.now() };
    let saved = k.clone();
    edit_user(c, &u.name, move |u| {
        if u.ssh_keys.iter().any(|x| x.fingerprint == saved.fingerprint) {
            return Err(StsError::new("DuplicateSSHPublicKey", 400, "The request was rejected because the SSH public key is already associated with the specified IAM user."));
        }
        if u.ssh_keys.len() >= si::MAX_SSH_KEYS_PER_USER {
            return Err(limit(format!("Cannot exceed quota for SSHPublicKeysPerUser: {}", si::MAX_SSH_KEYS_PER_USER)));
        }
        u.ssh_keys.push(saved);
        Ok(())
    })
    .await?;
    Ok(ssh_xml(&u, &k, &body))
}

fn ssh_key<'a>(c: &Call, u: &'a User) -> Result<&'a SshKey, StsError> {
    let id = c.p.required("SSHPublicKeyId", "sSHPublicKeyId")?;
    u.ssh_keys.iter().find(|k| k.id == id).ok_or_else(|| no_such(format!("The Public Key with id {id} cannot be found.")))
}

fn get_ssh(c: &Call) -> Result<String, StsError> {
    let u = ssh_user(c)?;
    c.allow("GetSSHPublicKey", &c.arn("user", &u.path, &u.name))?;
    let k = ssh_key(c, &u)?;
    let parsed = parse_ssh(&k.body)?;
    let body = match c.p.required("Encoding", "encoding")? {
        "SSH" => format!("ssh-rsa {}", STANDARD.encode(ssh_blob(&parsed))),
        "PEM" => ssh_pem(&parsed),
        v => return Err(validation(format!("1 validation error detected: Value '{v}' at 'encoding' failed to satisfy constraint: Member must satisfy enum value set: [SSH, PEM]"))),
    };
    Ok(ssh_xml(&u, k, &body))
}

fn list_ssh(c: &Call) -> Result<String, StsError> {
    let u = match c.user_or_self()? {
        Some(n) => super::users::user(c, &n)?,
        None => return Ok("<SSHPublicKeys/><IsTruncated>false</IsTruncated>".into()),
    };
    c.allow("ListSSHPublicKeys", &c.arn("user", &u.path, &u.name))?;
    let name = u.name.clone();
    page(
        c.p,
        u.ssh_keys,
        |k| k.id.clone(),
        |k| format!("{}{}{}{}", el("UserName", &name), el("SSHPublicKeyId", &k.id), el("Status", status(k.active)), el("UploadDate", &date(k.uploaded_at))),
        "SSHPublicKeys",
    )
}

async fn update_ssh(c: &Call<'_>, delete: bool) -> Result<(), StsError> {
    let u = ssh_user(c)?;
    c.allow(if delete { "DeleteSSHPublicKey" } else { "UpdateSSHPublicKey" }, &c.arn("user", &u.path, &u.name))?;
    let id = ssh_key(c, &u)?.id.clone();
    let active = if delete { None } else { Some(status_param(c)?) };
    edit_user(c, &u.name, move |u| {
        match active {
            None => u.ssh_keys.retain(|k| k.id != id),
            Some(a) => u.ssh_keys.iter_mut().filter(|k| k.id == id).for_each(|k| k.active = a),
        }
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_keys_round_trip() {
        let n: Vec<u8> = std::iter::once(0xC1u8).chain((0..255).map(|i| i as u8)).collect();
        let k = (n.clone(), vec![1, 0, 1]);
        let line = format!("ssh-rsa {} me@example", STANDARD.encode(ssh_blob(&k)));
        assert_eq!(parse_ssh(&line).unwrap(), k);
        assert_eq!(parse_ssh(&ssh_pem(&k)).unwrap(), k, "PEM round-trips");
        assert_eq!(fingerprint(&k).split(':').count(), 16);
        assert_eq!(parse_ssh("ssh-ed25519 AAAA").unwrap_err().code, "UnrecognizedPublicKeyEncoding");
        assert_eq!(parse_ssh("ssh-rsa !!!").unwrap_err().code, "InvalidPublicKey");
        // A length that runs past the blob.
        let mut bad = ssh_blob(&k);
        bad[3] = 0xff;
        assert_eq!(parse_ssh(&format!("ssh-rsa {}", STANDARD.encode(bad))).unwrap_err().code, "InvalidPublicKey");
        assert_eq!(der_len(5), vec![5]);
        assert_eq!(der_len(300), vec![0x82, 1, 44]);
    }
}
