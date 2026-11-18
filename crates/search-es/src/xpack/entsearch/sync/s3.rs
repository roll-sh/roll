//! The `s3` connector: a bucket's objects, listed a page at a time, each a document with its
//! text ([`super::extract`]). Any S3-compatible endpoint works; it is the tenant's choice, so it
//! goes through [`crate::s3::Client`] (public addresses only, no redirects, bounded reads).

use super::config::Values;
use super::extract;
use super::rules::{S3Rule, s3_rules};
use super::{DOC_BYTES_MAX, Sink, Stop};
use crate::json::{Map, Value};
use crate::s3::{Client, Credentials, Location};
use md5::Digest;

/// Syncs the configured buckets (or the advanced rules' prefixes); the new cursor
/// (`{"buckets": {bucket: newest LastModified}}`).
pub async fn sync(v: &Values, sink: &mut Sink<'_>, full: bool, cursor: &Value, advanced: Option<&Value>, allow_private: bool) -> Result<Value, Stop> {
    let targets: Vec<S3Rule> = match advanced {
        Some(a) => s3_rules(a)?,
        None => v.list("buckets").into_iter().map(|bucket| S3Rule { bucket, prefix: String::new(), extensions: Vec::new() }).collect(),
    };
    if targets.is_empty() {
        return Err(Stop::Failed("the connector's configuration has no [buckets]".into()));
    }
    if targets.iter().any(|t| t.bucket == "*") {
        return Err(Stop::Failed("[buckets] names buckets: roll's s3 connector doesn't list every bucket (*)".into()));
    }
    let region = v.text("region").unwrap_or_else(|| "us-east-1".into());
    let given = v.text("endpoint");
    let endpoint = crate::s3::endpoint(given.as_deref(), None, &region, allow_private).map_err(|e| Stop::Failed(format!("the connector's endpoint: {e}")))?;
    let creds = match (v.text("aws_access_key_id"), v.text("aws_secret_access_key")) {
        (Some(access_key), Some(secret_key)) => Some(Credentials { access_key, secret_key, session_token: None }),
        _ => None,
    };
    let page_size = v.int("page_size", 100)?.clamp(1, super::PAGE_DOCS as i64) as usize;
    let mut next = cursor.get("buckets").and_then(Value::as_object).cloned().unwrap_or_default();
    for t in &targets {
        let at = Location { endpoint: endpoint.clone(), bucket: t.bucket.clone(), region: region.clone(), path_style: given.is_some() };
        let client = Client::new(at, creds.clone(), allow_private).map_err(|e| Stop::Failed(format!("bucket [{}]: {e}", t.bucket)))?;
        let since = if full { None } else { next.get(&t.bucket).and_then(Value::as_str).map(str::to_string) };
        let newest = read_bucket(&client, sink, t, since.as_deref(), page_size).await?;
        let keep = next.get(&t.bucket).and_then(Value::as_str).map(str::to_string);
        if let Some(n) = [newest, keep].into_iter().flatten().max() {
            next.insert(t.bucket.clone(), n.into());
        }
    }
    Ok(crate::obj! { "buckets" => Value::Obj(next) })
}

/// Elastic's id of an object: the MD5 of `bucket/key`, in hex.
fn doc_id(bucket: &str, key: &str) -> String {
    md5::Md5::digest(format!("{bucket}/{key}").as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// One bucket (under a prefix): the newest `LastModified` it listed.
async fn read_bucket(client: &Client, sink: &mut Sink<'_>, t: &S3Rule, since: Option<&str>, page_size: usize) -> Result<Option<String>, Stop> {
    let mut token: Option<String> = None;
    let mut newest: Option<String> = None;
    loop {
        let (objects, next) = client.list_page(&t.prefix, token.as_deref(), page_size).await.map_err(|e| Stop::Failed(format!("listing bucket [{}] failed: {e}", t.bucket)))?;
        let mut docs = Vec::new();
        for o in objects {
            // Folder markers.
            if o.key.ends_with('/') && o.size == 0 {
                continue;
            }
            if !t.extensions.is_empty() && !t.extensions.contains(&extract::extension(&o.key)) {
                continue;
            }
            if newest.as_deref().is_none_or(|n| o.last_modified.as_str() > n) {
                newest = Some(o.last_modified.clone());
            }
            // An incremental sync reads only what changed since the last one.
            if since.is_some_and(|s| o.last_modified.as_str() <= s) {
                continue;
            }
            let mut doc = Map::new();
            doc.insert("_id".into(), doc_id(&t.bucket, &o.key).into());
            doc.insert("_timestamp".into(), o.last_modified.clone().into());
            doc.insert("bucket".into(), t.bucket.clone().into());
            doc.insert("filename".into(), o.key.clone().into());
            doc.insert("size_in_bytes".into(), (o.size as i64).into());
            if let Some(kind) = extract::kind_of(&o.key).filter(|_| o.size as usize <= DOC_BYTES_MAX) {
                match client.get(&o.key, DOC_BYTES_MAX).await {
                    Ok(Some(bytes)) => {
                        sink.read(bytes.len() as u64);
                        doc.insert("body".into(), extract::text(kind, &bytes).into());
                    }
                    // Gone since the listing.
                    Ok(None) => continue,
                    Err(e) => return Err(Stop::Failed(format!("reading [{}] from bucket [{}] failed: {e}", o.key, t.bucket))),
                }
            }
            docs.push(doc);
        }
        sink.page(docs).await?;
        match next {
            Some(n) => token = Some(n),
            None => return Ok(newest),
        }
    }
}
