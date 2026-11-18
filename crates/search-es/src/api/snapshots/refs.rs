//! Credentials by reference: an `s3`, `gcs` or `azure` repository's credential setting (s3's
//! `access_key`, `secret_key` and `session_token`, gcs's `credentials_file`, azure's `account`,
//! `key` and `sas_token`) may hold a roll secrets reference, `roll://ENV/KEY`, instead of the
//! value. The reference isn't a secret: it stays in the repository's settings as written (the
//! cluster state, `GET _snapshot` and a branch show it), and the value is never stored.
//!
//! The value is resolved through the cluster's [`crate::secret_refs::SecretRefs`] when the
//! repository's client is built. Each repository operation (a snapshot, a restore, a
//! verification, a cleanup) first asks for the references' digests and builds its client from
//! the values kept in memory while they stay the same; a rotation moves a digest, and the next
//! operation resolves again. Nothing about them is persisted.
//!
//! `PUT _snapshot` checks each reference before registering the repository: one the cluster
//! can't resolve (no resolver on the node, no project standing for the cluster, no such key)
//! is a 400 `settings_exception` naming the setting, the reference and why. A reference refused
//! later (the secret deleted, access lost) fails that operation with the reason; the
//! repository never falls back to anonymous access, and no error carries a value.

use super::*;
use crate::secret_refs::{Revealed, is_reference};
use std::collections::{BTreeMap, HashMap};

/// A repository's credentials, opened: sealed ones from [`Repository::secrets`] and resolved
/// references.
#[derive(Default)]
pub(super) struct Creds(BTreeMap<&'static str, Revealed>);

impl Creds {
    pub(super) fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(Revealed::expose)
    }
}

/// Resolved references kept for one repository while their digests stay the same.
pub(super) struct Kept {
    /// `(setting, reference)`, as the repository named them when they were resolved.
    refs: Vec<(&'static str, String)>,
    digests: Vec<String>,
    values: Vec<Revealed>,
}

/// Repositories' kept references, by repository name (in memory only).
pub(super) type KeptRefs = parking_lot::Mutex<HashMap<String, Kept>>;

/// The references among type `ty`'s credential settings.
pub(super) fn references(ty: &str, settings: &Settings) -> Vec<(&'static str, String)> {
    cloud::secret_names(ty).iter().filter_map(|k| settings.get(k).filter(|v| is_reference(v)).map(|v| (*k, v.trim().to_string()))).collect()
}

fn why(setting: &str, reference: &str, why: &str) -> String {
    format!("[{setting}] names [{reference}], which can't be used: {why}")
}

const NO_RESOLVER: &str = "this node resolves no roll secrets references";

/// `PUT _snapshot`'s check of a definition's references (`settings` as given).
pub(super) async fn check(cluster: &crate::Cluster, ty: &str, settings: &Settings) -> EsResult<()> {
    let refs = references(ty, settings);
    let bad = |setting: &str, reference: &str, reason: &str| EsError::new(400, "settings_exception", why(setting, reference, reason));
    if let (Some((k, reference)), None) = (refs.first(), &cluster.config.secret_refs) {
        return Err(bad(k, reference, NO_RESOLVER));
    }
    for (k, reference) in &refs {
        if let Some(r) = &cluster.config.secret_refs {
            r.0.digest(&cluster.name, reference).await.map_err(|e| bad(k, reference, &e))?;
        }
    }
    Ok(())
}

/// Repository `name`'s credentials for a client built now.
pub(super) async fn credentials(cluster: &crate::Cluster, name: &str, r: &Repository) -> EsResult<Creds> {
    let mut creds = Creds::default();
    for k in cloud::secret_names(&r.ty) {
        if let Some(v) = r.secrets.get(*k) {
            creds.0.insert(k, Revealed::new(crate::seal::open(cluster, cloud::secret_kind(&r.ty), v)?));
        }
    }
    let refs = references(&r.ty, &r.settings);
    let Some((first, first_ref)) = refs.first() else { return Ok(creds) };
    let refused = |setting: &str, reference: &str, reason: &str| repository_exception(name, why(setting, reference, reason));
    let Some(resolver) = &cluster.config.secret_refs else { return Err(refused(first, first_ref, NO_RESOLVER)) };
    let mut digests = Vec::with_capacity(refs.len());
    for (k, reference) in &refs {
        digests.push(resolver.0.digest(&cluster.name, reference).await.map_err(|e| refused(k, reference, &e))?);
    }
    let kept = &cluster.snapshot_data.refs;
    let same = kept.lock().get(name).filter(|c| c.refs == refs && c.digests == digests).map(|c| c.values.clone());
    let values = match same {
        Some(v) => v,
        None => {
            let mut values = Vec::with_capacity(refs.len());
            for (k, reference) in &refs {
                values.push(resolver.0.resolve(&cluster.name, reference).await.map_err(|e| refused(k, reference, &e))?);
            }
            kept.lock().insert(name.to_string(), Kept { refs: refs.clone(), digests, values: values.clone() });
            values
        }
    };
    for ((k, _), v) in refs.into_iter().zip(values) {
        creds.0.insert(k, v);
    }
    Ok(creds)
}
