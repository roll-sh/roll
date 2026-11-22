//! The `gcs` and `azure` repository types (Elasticsearch's `repository-gcs` and
//! `repository-azure`): snapshots in a Google Cloud Storage bucket or an Azure container the
//! tenant names, kept exactly as an `s3` repository keeps them ([`super::s3::Bucket`]) through
//! [`crate::gcs`] and [`crate::azure`].
//!
//! Settings, validated with Elasticsearch's messages in its order:
//! - `gcs`: `bucket` (required, checked first), `base_path`, `chunk_size` (1b..5tb, default
//!   5tb), `compress`, `client`, `application_name`, the throttles, `readonly`; and the client's
//!   `endpoint`, `token_uri` and `project_id`.
//! - `azure`: `container` (default `elasticsearch-snapshots`), `base_path`, `chunk_size`
//!   (1b..2684354550000b), `compress`, `client`, `location_mode` (`primary_only` by default;
//!   `secondary_only` makes the repository read-only), the throttles, `readonly`; and the
//!   client's `endpoint_suffix`, `endpoint` and `secondary_endpoint`. A client's own settings
//!   are checked when the repository is first used, as Elasticsearch checks them when it
//!   creates the client: a failure fails `PUT _snapshot` with `verify` and `_verify` with
//!   Elasticsearch's `settings_exception`s.
//!
//! Elasticsearch reads a client's secure settings from its keystore; a roll tenant has none, so
//! they come with the repository's settings, as an `s3` repository's keys do: `gcs`'s
//! `credentials_file` (the service account key file's JSON, as a string) and `azure`'s
//! `account`, `key` and `sas_token`. They're moved into [`Repository::secrets`] at once, sealed
//! under the deployment's key, opened only to sign a request, and never returned by
//! `GET _snapshot` or the cluster state or written into an error or a log line. Any of them may
//! be a roll secrets reference instead (`roll://ENV/KEY`, [`super::refs`]).
//!
//! roll's own clients choose part sizes: a GCS resumable upload's chunks and an Azure blob's
//! blocks are [`PART`] (Elasticsearch has no `buffer_size` for either), each held once and
//! charged to the `request` breaker, as an `s3` part is.

use super::*;
use crate::blob::Store;
use crate::{azure, gcs};
use std::collections::BTreeMap;

/// Secure settings by type (`s3`'s are [`s3::SECRET_SETTINGS`]).
const GCS_SECRETS: [&str; 1] = ["credentials_file"];
const AZURE_SECRETS: [&str; 3] = ["account", "key", "sas_token"];
/// A GCS resumable chunk (a multiple of 256 KiB) and an Azure block.
pub(super) const PART: u64 = 8 << 20;
/// `chunk_size`'s bounds (`GoogleCloudStorageRepository.MAX_CHUNK_SIZE`,
/// `AzureStorageService.MAX_CHUNK_SIZE`).
const GCS_MAX_CHUNK: i64 = 5 << 40;
const AZURE_MAX_CHUNK: i64 = 2_684_354_550_000;
/// `container`'s default.
pub(crate) const AZURE_CONTAINER: &str = "elasticsearch-snapshots";

pub(super) fn secret_names(ty: &str) -> &'static [&'static str] {
    match ty {
        "s3" => &s3::SECRET_SETTINGS,
        "gcs" => &GCS_SECRETS,
        "azure" => &AZURE_SECRETS,
        _ => &[],
    }
}

/// What sealed credentials of a type are bound to.
pub(super) fn secret_kind(ty: &str) -> &'static str {
    match ty {
        "gcs" => "gcs repository credential",
        "azure" => "azure repository credential",
        _ => s3::SECRET_KIND,
    }
}

/// A credential given as an object (`"credentials_file": {…}`) arrives flattened into settings
/// (`credentials_file.private_key`), where it would be kept and shown: refused, naming only the
/// setting.
pub(super) fn secret_shapes(ty: &str, settings: &Settings) -> EsResult<()> {
    for name in secret_names(ty) {
        let nested = format!("{name}.");
        if settings.keys().any(|k| k.starts_with(&nested)) {
            return Err(EsError::illegal_argument(format!("[{name}] must be given as a string")));
        }
    }
    Ok(())
}

/// Moves a type's credentials out of `settings`, sealed as the cluster stores them. A reference
/// (`roll://…`, [`super::refs`]) isn't a value: it stays in the settings. A node without a
/// sealing key refuses a pasted value rather than keep it in the clear: a reference (through
/// roll's API) or a key on the node are the ways to give one there.
pub(super) fn take_secrets(cluster: &crate::Cluster, ty: &str, settings: &mut Settings) -> EsResult<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for k in secret_names(ty) {
        if settings.get(k).is_some_and(|v| crate::secret_refs::classify(cluster, v) != crate::secret_refs::Setting::Value) {
            continue;
        }
        if settings.get(k).is_some_and(|v| !v.is_empty()) && !crate::seal::may_store(cluster) {
            return Err(EsError::new(
                400,
                "settings_exception",
                format!("[{k}] would be stored in the clear: this node has no sealing key (ROLL_SECRET_KEY). Give it as a roll secrets reference (roll://ENV/KEY) through roll's API (PUT /v1/search/clusters/{{id}}/repositories/{{name}}), or have the operator set ROLL_SECRET_KEY"),
            ));
        }
        if let Some(v) = settings.remove(k)
            && let Some(v) = v.as_str().filter(|v| !v.is_empty())
        {
            out.insert(k.to_string(), crate::seal::seal_given(cluster, secret_kind(ty), v)?);
        }
    }
    Ok(out)
}

/// Seals repositories' credentials still in the clear (registered on a node without a key),
/// for [`crate::seal::seal_leftovers`].
pub(crate) fn seal_stored_secrets(md: &mut crate::meta::Metadata, sealer: Option<&crate::seal::Sealer>) {
    let Some(sealer) = sealer else { return };
    for r in md.repositories.values_mut() {
        if r.secrets.values().all(|v| crate::seal::is_sealed(v)) {
            continue;
        }
        let r = std::sync::Arc::make_mut(r);
        let kind = secret_kind(&r.ty);
        for v in r.secrets.values_mut().filter(|v| !crate::seal::is_sealed(v)) {
            *v = sealer.seal_text(kind, v);
        }
    }
}

/// `GoogleCloudStorageRepository`'s first check, before the blob store's own settings.
pub(super) fn gcs_bucket(name: &str, settings: &Settings) -> EsResult<()> {
    match settings.get("bucket").is_some_and(|b| !b.is_empty()) {
        true => Ok(()),
        false => Err(repository_exception(name, "Setting [bucket] is empty for repository")),
    }
}

/// Validates a `gcs` repository's settings past the bucket and the blob store's (`PUT _snapshot`).
pub(super) fn validate_gcs(cluster: &crate::Cluster, name: &str, settings: &Settings) -> EsResult<()> {
    let create = |e: EsError| repository_exception(name, "failed to create repository").caused_by(e);
    transfer::bounded_size(settings, "chunk_size", GCS_MAX_CHUNK, 1, GCS_MAX_CHUNK).map_err(create)?;
    // roll's own: what goes into URLs and keys, and the key file.
    let bucket = settings.get("bucket").unwrap_or_default();
    if !gcs::valid_bucket(bucket) {
        return Err(create(EsError::illegal_argument(format!("invalid bucket name [{bucket}]"))));
    }
    s3::base_path(settings).map_err(create)?;
    let allow = &s3::egress(cluster);
    gcs::endpoint(settings.get("endpoint"), allow).map_err(|e| create(EsError::illegal_argument(e)))?;
    if let Some(json) = settings.get("credentials_file").filter(|j| !j.is_empty() && crate::secret_refs::classify(cluster, j) == crate::secret_refs::Setting::Value) {
        gcs::ServiceAccount::parse(json, settings.get("token_uri"), allow).map_err(|e| create(EsError::illegal_argument(e)))?;
    }
    Ok(())
}

/// Validates an `azure` repository's settings past the blob store's (`PUT _snapshot`).
pub(super) fn validate_azure(name: &str, settings: &Settings) -> EsResult<()> {
    let create = |e: EsError| repository_exception(name, "failed to create repository").caused_by(e);
    transfer::bounded_size(settings, "chunk_size", AZURE_MAX_CHUNK, 1, AZURE_MAX_CHUNK).map_err(create)?;
    location_mode(settings).map_err(create)?;
    let container = settings.get("container").filter(|c| !c.is_empty()).unwrap_or(AZURE_CONTAINER);
    if !azure::valid_container(container) {
        return Err(create(EsError::illegal_argument(format!("invalid container name [{container}]"))));
    }
    s3::base_path(settings).map_err(create)
}

fn location_mode(settings: &Settings) -> EsResult<azure::LocationMode> {
    azure::LocationMode::parse(settings.get("location_mode").unwrap_or("primary_only")).map_err(EsError::illegal_argument)
}

/// The object sizes of a `gcs` or `azure` repository: `chunk_size`, at most what [`PART`]s can
/// make of one object.
fn sizes(r: &Repository) -> transfer::Sizes {
    let (max, parts) = match r.ty.as_str() {
        "azure" => (AZURE_MAX_CHUNK, azure::MAX_BLOCKS),
        _ => (GCS_MAX_CHUNK, u64::MAX),
    };
    let chunk = transfer::bounded_size(&r.settings, "chunk_size", max, 1, max).unwrap_or(max) as u64;
    transfer::Sizes { chunk: chunk.min(PART.saturating_mul(parts)), buffer: PART }
}

/// A `gcs` repository's bucket. A client that can't be made fails as Elasticsearch's does
/// ("cannot create blob store").
pub(super) fn gcs_store(cluster: &crate::Cluster, name: &str, r: &Repository, creds: &refs::Creds) -> EsResult<(Store, transfer::Sizes)> {
    let cannot = |why: String| repository_exception(name, "cannot create blob store").caused_by(EsError::illegal_argument(why));
    let s = &r.settings;
    let allow = &s3::egress(cluster);
    let endpoint = gcs::endpoint(s.get("endpoint"), allow).map_err(cannot)?;
    let account = match creds.get("credentials_file") {
        Some(json) => Some(gcs::ServiceAccount::parse(json, s.get("token_uri"), allow).map_err(cannot)?),
        None => None,
    };
    let client = gcs::Client::new(endpoint, s.get("bucket").unwrap_or_default(), account, s.get("application_name"), allow).map_err(|e| cannot(e.to_string()))?;
    Ok((Store::Gcs(client), sizes(r)))
}

/// An `azure` repository's container. A client that can't be made fails with Elasticsearch's
/// `settings_exception`: without an account there is no client.
pub(super) fn azure_store(cluster: &crate::Cluster, name: &str, r: &Repository, creds: &refs::Creds) -> EsResult<(Store, transfer::Sizes)> {
    let settings_error = |why: String| EsError::new(500, "settings_exception", why);
    let s = &r.settings;
    let client_name = s.get("client").filter(|c| !c.is_empty()).unwrap_or("default");
    let Some(account) = creds.get("account").filter(|a| !a.is_empty()) else {
        return Err(settings_error(format!("Unable to find client with name [{client_name}]")));
    };
    let mode = location_mode(s).map_err(|e| repository_exception(name, "cannot create blob store").caused_by(e))?;
    let auth = azure::Auth::new(account, creds.get("key"), creds.get("sas_token")).map_err(settings_error)?;
    let allow = &s3::egress(cluster);
    let (primary, secondary) = azure::endpoints(account, s.get("endpoint_suffix"), s.get("endpoint"), s.get("secondary_endpoint"), mode, allow).map_err(settings_error)?;
    let container = s.get("container").filter(|c| !c.is_empty()).unwrap_or(AZURE_CONTAINER);
    let client = azure::Client::new(account, auth, primary, secondary, mode, container, allow).map_err(|e| settings_error(e.to_string()))?;
    Ok((Store::Azure(client), sizes(r)))
}
