//! The `s3` connector: a bucket's objects, listed a page at a time, each a document with its
//! text ([`super::extract`]). Any S3-compatible endpoint works; it is the tenant's choice, so it
//! goes through [`crate::s3::Client`] (public addresses only, no redirects, bounded reads).

use super::config::Values;
use super::extract;
use super::rules::{S3Rule, s3_rules_within};
use super::{DOC_BYTES_MAX, PAGE_BYTES, Sink, Stop};
use crate::json::{Map, Value};
use crate::s3::{Client, Credentials, Location};
use md5::Digest;

/// Syncs the configured buckets (or the advanced rules' prefixes); the new cursor
/// (`{"buckets": {bucket: newest LastModified}}`).
pub async fn sync(v: &Values, sink: &mut Sink<'_>, full: bool, cursor: &Value, advanced: Option<&Value>, allow_private: bool) -> Result<Value, Stop> {
    let targets: Vec<S3Rule> = match advanced {
        Some(a) => s3_rules_within(a, &v.list("buckets"))?,
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

/// Whether an object modified at `last_modified` is read by an incremental sync after `cursor`
/// (the newest `LastModified` the last sync listed). Listings give whole seconds on AWS, so an
/// object written in the cursor's own second after that listing has the cursor's time: it's
/// read again rather than missed (indexing it twice is harmless).
fn changed_since(last_modified: &str, cursor: &str) -> bool {
    last_modified >= cursor
}

/// Whether an object's content can be fetched by its key: a `.` or `..` path segment is
/// normalised away by URL parsing while the request's signature covers the key as it is, so
/// such an object can't be read (it's indexed without its text instead of failing the sync).
fn fetchable(key: &str) -> bool {
    !key.split('/').any(|seg| seg == "." || seg == "..")
}

/// Elastic's id of an object: the MD5 of `bucket/key`, in hex.
fn doc_id(bucket: &str, key: &str) -> String {
    md5::Md5::digest(format!("{bucket}/{key}").as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Listing pages in a row with no key at all (an endpoint paging through nothing) before the
/// sync gives up. Pages of keys the sync skips (folder markers, other extensions, unchanged
/// objects) are progress and never count.
const EMPTY_PAGES_MAX: usize = 100;

/// One bucket (under a prefix): the newest `LastModified` it listed.
///
/// Every listing's bytes count against the sync's rate and its time limit like objects' do, so
/// an endpoint can't keep a sync listing pages that add nothing.
async fn read_bucket(client: &Client, sink: &mut Sink<'_>, t: &S3Rule, since: Option<&str>, page_size: usize) -> Result<Option<String>, Stop> {
    let mut token: Option<String> = None;
    let mut newest: Option<String> = None;
    let mut empty = 0usize;
    loop {
        let listed = tokio::select! {
            r = client.list_page(&t.prefix, token.as_deref(), page_size) => r,
            s = sink.stopped() => return Err(s),
        };
        let (objects, next, listing) = listed.map_err(|e| Stop::Failed(format!("listing bucket [{}] failed: {e}", t.bucket)))?;
        sink.read(listing as u64);
        empty = if objects.is_empty() { empty + 1 } else { 0 };
        let mut page = sink.new_page();
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
            if since.is_some_and(|s| !changed_since(&o.last_modified, s)) {
                continue;
            }
            let mut doc = Map::new();
            doc.insert("_timestamp".into(), o.last_modified.clone().into());
            doc.insert("bucket".into(), t.bucket.clone().into());
            doc.insert("filename".into(), o.key.clone().into());
            doc.insert("size_in_bytes".into(), (o.size as i64).into());
            // The object's text, charged while the document is built and until the page holds it.
            let mut held = sink.reserve();
            if !fetchable(&o.key) {
                // Its text can't be read (see [`fetchable`]): indexed by its metadata, and counted.
                let n = sink.counters.metadata.get("unreadable_object_count").and_then(Value::as_i64).unwrap_or(0);
                sink.counters.metadata.insert("unreadable_object_count".into(), (n + 1).into());
            } else if let Some(kind) = extract::kind_of(&o.key).filter(|_| o.size as usize <= DOC_BYTES_MAX) {
                let got = tokio::select! {
                    r = client.get(&o.key, DOC_BYTES_MAX) => r,
                    s = sink.stopped() => return Err(s),
                };
                match got {
                    Ok(Some(body)) => {
                        sink.read(body.len() as u64);
                        // The text is scanned, never a tree (`extract`).
                        let charge = |held: &mut crate::breaker::Reservation, n: usize| held.add(n as u64, "connector object").map_err(|e| Stop::Failed(format!("reading [{}] from bucket [{}] failed: {}", o.key, t.bucket, e.reason())));
                        charge(&mut held, body.len())?;
                        let text = extract::text(kind, &body);
                        drop(body);
                        charge(&mut held, text.len())?;
                        doc.insert("body".into(), text.into());
                    }
                    // Gone since the listing.
                    Ok(None) => continue,
                    Err(e) => return Err(Stop::Failed(format!("reading [{}] from bucket [{}] failed: {e}", o.key, t.bucket))),
                }
            }
            sink.push_doc(&mut page, &doc_id(&t.bucket, &o.key), doc)?;
            drop(held);
            // A page holds at most PAGE_BYTES of objects' text.
            if page.bytes() >= PAGE_BYTES {
                let full = std::mem::replace(&mut page, sink.new_page());
                sink.send(full).await?;
            }
        }
        // Sent empty too: it throttles, and notices a cancel or the time limit.
        sink.send(page).await?;
        if empty >= EMPTY_PAGES_MAX && next.is_some() {
            return Err(Stop::Failed(format!("listing bucket [{}] gave {EMPTY_PAGES_MAX} pages in a row with no key", t.bucket)));
        }
        match next {
            Some(n) => token = Some(n),
            None => return Ok(newest),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dot_segments_are_indexed_without_their_text() {
        assert!(super::fetchable("docs/a.txt") && super::fetchable("..hidden/x") && super::fetchable("a/.b"));
        assert!(!super::fetchable("a/../b.txt") && !super::fetchable("./b.txt") && !super::fetchable("a/.."));
    }

    #[test]
    fn incremental_syncs_reread_the_cursors_second() {
        assert!(super::changed_since("2026-11-18T10:00:00.000Z", "2026-11-18T10:00:00.000Z"));
        assert!(super::changed_since("2026-11-18T10:00:01.000Z", "2026-11-18T10:00:00.000Z"));
        assert!(!super::changed_since("2026-11-18T09:59:59.000Z", "2026-11-18T10:00:00.000Z"));
    }
}
