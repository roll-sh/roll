//! SAML identity providers (`CreateSAMLProvider` and the rest): the IdP's metadata document,
//! checked when it's given (an `entityID` and at least one RSA signing certificate; see
//! `sts/saml.rs`, which verifies assertions with it). Encrypted assertions aren't supported, so
//! `AssertionEncryptionMode=Required` and private keys are refused.

use super::*;
use crate::sts::saml::parse_metadata;
use s3_store::iam::{self as si, SamlProviderEntry, kind};

pub(super) async fn run(c: &Call<'_>, action: &str) -> Result<Option<String>, StsError> {
    Ok(Some(match action {
        "CreateSAMLProvider" => create(c).await?,
        "GetSAMLProvider" => get(c)?,
        "ListSAMLProviders" => list(c)?,
        "UpdateSAMLProvider" => update(c).await?,
        "DeleteSAMLProvider" => return delete(c).await.map(|_| None),
        "TagSAMLProvider" => return tag(c, true).await.map(|_| None),
        "UntagSAMLProvider" => return tag(c, false).await.map(|_| None),
        "ListSAMLProviderTags" => list_tags(c)?,
        _ => unreachable!("routed here"),
    }))
}

/// 100 years: a provider whose metadata has no `validUntil`.
const DEFAULT_VALIDITY_MS: i64 = 100 * 365 * 86_400_000;

fn arn(c: &Call, name: &str) -> String {
    crate::identity::saml_provider_arn(&c.account, name)
}

fn valid_provider_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 128 && n.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// The `SAMLMetadataDocument` parameter, parsed.
fn metadata(c: &Call) -> Result<(String, crate::sts::saml::Metadata), StsError> {
    let doc = c.p.required("SAMLMetadataDocument", "sAMLMetadataDocument")?;
    if doc.len() < 1000 {
        return Err(validation("1 validation error detected: Value at 'sAMLMetadataDocument' failed to satisfy constraint: Member must have length greater than or equal to 1000"));
    }
    let m = parse_metadata(doc).map_err(invalid_input)?;
    Ok((doc.to_string(), m))
}

fn no_encryption(c: &Call) -> Result<(), StsError> {
    if c.p.get("AssertionEncryptionMode").is_some_and(|m| m == "Required") || c.p.get("AddPrivateKey").is_some() {
        return Err(validation("roll doesn't support encrypted SAML assertions: AssertionEncryptionMode=Required and private keys aren't accepted."));
    }
    Ok(())
}

/// The provider `SAMLProviderArn` names.
fn provider(c: &Call) -> Result<SamlProviderEntry, StsError> {
    let a = c.p.required("SAMLProviderArn", "sAMLProviderArn")?;
    let gone = || no_such(format!("Manifest not found for arn {a}"));
    let name = a.strip_prefix(&format!("arn:aws:iam::{}:saml-provider/", c.account)).ok_or_else(gone)?;
    c.store.iam_read(|t| t.get(&c.account, kind::SAML, name)).map_err(store_error)?.ok_or_else(gone)
}

async fn create(c: &Call<'_>) -> Result<String, StsError> {
    let name = c.p.required("Name", "name")?.to_string();
    if !valid_provider_name(&name) {
        return Err(validation(format!("1 validation error detected: Value '{name}' at 'name' failed to satisfy constraint: Member must satisfy regular expression pattern: [\\w._-]+")));
    }
    c.allow("CreateSAMLProvider", &arn(c, &name))?;
    no_encryption(c)?;
    let (doc, m) = metadata(c)?;
    let now = c.store.now();
    let mut uuid = [0u8; 16];
    getrandom::fill(&mut uuid).expect("os randomness");
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    let h = hex::encode(uuid);
    let e = SamlProviderEntry {
        name: name.clone(),
        metadata: doc,
        entity_id: m.entity_id,
        certificates: m.certificates,
        created_at: now,
        valid_until: m.valid_until.unwrap_or(now + DEFAULT_VALIDITY_MS),
        uuid: format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]),
        tags: c.tags()?,
    };
    let tags = e.tags.clone();
    let acct = c.account.clone();
    let n = name.clone();
    c.store
        .iam_write(move |t| {
            if t.exists(&acct, kind::SAML, &n)? {
                return Err(StoreError::Conflict(format!("SAMLProvider with name {n} already exists.")));
            }
            if t.count(&acct, kind::SAML)? >= si::MAX_SAML_PROVIDERS {
                return Err(StoreError::LimitExceeded(format!("Cannot exceed quota for SAMLProvidersPerAccount: {}", si::MAX_SAML_PROVIDERS)));
            }
            t.put(&acct, kind::SAML, &n, &e)
        })
        .await
        .map_err(store_error)?;
    Ok(format!("{}{}", el("SAMLProviderArn", &arn(c, &name)), tags_xml(&tags)))
}

fn get(c: &Call) -> Result<String, StsError> {
    let e = provider(c)?;
    c.allow("GetSAMLProvider", &arn(c, &e.name))?;
    Ok(format!(
        "{}{}{}{}{}",
        el("SAMLProviderUUID", &e.uuid),
        el("SAMLMetadataDocument", &e.metadata),
        el("CreateDate", &date(e.created_at)),
        el("ValidUntil", &date(e.valid_until)),
        tags_xml(&e.tags)
    ))
}

fn list(c: &Call) -> Result<String, StsError> {
    c.allow("ListSAMLProviders", "*")?;
    let all: Vec<SamlProviderEntry> = c.store.iam_read(|t| t.list(&c.account, kind::SAML)).map_err(store_error)?;
    let members: String = all.iter().map(|e| format!("<member>{}{}{}</member>", el("Arn", &arn(c, &e.name)), el("ValidUntil", &date(e.valid_until)), el("CreateDate", &date(e.created_at)))).collect();
    Ok(format!("<SAMLProviderList>{members}</SAMLProviderList>"))
}

async fn edit(c: &Call<'_>, name: &str, f: impl FnOnce(&mut SamlProviderEntry) -> Result<(), StsError> + Send) -> Result<(), StsError> {
    let (acct, name) = (c.account.clone(), name.to_string());
    let mut err = None;
    let r = c
        .store
        .iam_write(|t| {
            let Some(mut e) = t.get::<SamlProviderEntry>(&acct, kind::SAML, &name)? else { return Err(StoreError::NotFound(format!("SAMLProvider {name} doesn't exist."))) };
            if let Err(x) = f(&mut e) {
                err = Some(x);
                return Err(StoreError::InvalidRequest("refused".into()));
            }
            t.put(&acct, kind::SAML, &name, &e)
        })
        .await;
    match (r, err) {
        (_, Some(e)) => Err(e),
        (r, None) => r.map_err(store_error),
    }
}

async fn update(c: &Call<'_>) -> Result<String, StsError> {
    let e = provider(c)?;
    c.allow("UpdateSAMLProvider", &arn(c, &e.name))?;
    no_encryption(c)?;
    let (doc, m) = metadata(c)?;
    let now = c.store.now();
    edit(c, &e.name, move |x| {
        x.metadata = doc;
        x.entity_id = m.entity_id;
        x.certificates = m.certificates;
        x.valid_until = m.valid_until.unwrap_or(now + DEFAULT_VALIDITY_MS);
        Ok(())
    })
    .await?;
    Ok(el("SAMLProviderArn", &arn(c, &e.name)))
}

async fn delete(c: &Call<'_>) -> Result<(), StsError> {
    let e = provider(c)?;
    c.allow("DeleteSAMLProvider", &arn(c, &e.name))?;
    let (acct, name) = (c.account.clone(), e.name.clone());
    c.store.iam_write(move |t| t.delete(&acct, kind::SAML, &name).map(|_| ())).await.map_err(store_error)
}

async fn tag(c: &Call<'_>, add: bool) -> Result<(), StsError> {
    let e = provider(c)?;
    c.allow(if add { "TagSAMLProvider" } else { "UntagSAMLProvider" }, &arn(c, &e.name))?;
    if add {
        let tags = c.tags()?;
        edit(c, &e.name, |x| merge_tags(&mut x.tags, tags)).await
    } else {
        let keys = c.p.list("TagKeys")?;
        edit(c, &e.name, move |x| {
            x.tags.retain(|(k, _)| !keys.iter().any(|y| y.eq_ignore_ascii_case(k)));
            Ok(())
        })
        .await
    }
}

fn list_tags(c: &Call) -> Result<String, StsError> {
    let e = provider(c)?;
    c.allow("ListSAMLProviderTags", &arn(c, &e.name))?;
    page(c.p, e.tags, |(k, _)| k.clone(), |(k, v)| format!("{}{}", el("Key", k), el("Value", v)), "Tags")
}
