//! Point-in-time restore of a log store kept in a versioned S3 bucket.
//!
//! - **Backups.** `deploy/aws/tasks.yaml` turns on versioning, so every object the cell
//!   overwrites or deletes (pruned commits, old snapshots, prune markers) stays as a
//!   noncurrent version for `BackupDays`.
//! - **Why a timestamp restores correctly.** A cell's objects are consistent at every instant;
//!   crash recovery relies on that. So putting every key back the way it was at time T gives a
//!   log that opens to exactly what was acknowledged at T.
//! - **Nothing is destroyed.** Restoring copies old versions back on top and deletes newer keys
//!   with delete markers. The undone versions stay in the bucket, so a restore can itself be
//!   undone by restoring to a later time.
//!
//! The cell must be stopped first (`desired-count 0`). Leaders release the lease when they stop
//! gracefully, and the restore refuses to run while the lease is held. A leader that kept
//! running anyway would leave a gap in the log, and the next node would refuse to open it rather
//! than serve mixed state.

/// One version (or delete marker) of a key, as S3 lists them.
#[derive(Clone, Debug)]
pub struct Version {
    pub key: String,
    pub version_id: String,
    /// Seconds and nanoseconds since the epoch.
    pub modified: (i64, u32),
    pub delete_marker: bool,
    /// Content hash: a version copied back keeps it, so a restore done twice does nothing.
    pub etag: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Copy this version back as the current one.
    Restore { key: String, version_id: String },
    /// The key didn't exist at the time: delete it (a delete marker).
    Delete { key: String },
}

/// What to do to put every key under the listing back to how it was at `at`.
///
/// `versions` is in S3's order: by key, newest first within each key (versions and delete
/// markers each in that order; they're merged here). Keys for which `skip` is true are left
/// alone (the lease).
pub fn plan(versions: &[Version], at: (i64, u32), skip: impl Fn(&str) -> bool) -> Vec<Action> {
    let mut by_key: std::collections::BTreeMap<&str, Vec<&Version>> = std::collections::BTreeMap::new();
    for v in versions {
        by_key.entry(&v.key).or_default().push(v);
    }
    let mut actions = Vec::new();
    for (key, mut vs) in by_key {
        if skip(key) {
            continue;
        }
        // Newest first. Same timestamp: keep the listing order, except that a delete marker
        // goes first, because log objects are deleted only after they're written.
        vs.sort_by(|a, b| b.modified.cmp(&a.modified).then(b.delete_marker.cmp(&a.delete_marker)));
        let current = vs[0];
        let then = vs.iter().find(|v| v.modified <= at);
        match then {
            Some(v) if !v.delete_marker => {
                let same = v.version_id == current.version_id || (v.etag.is_some() && v.etag == current.etag && !current.delete_marker);
                if !same {
                    actions.push(Action::Restore { key: key.to_string(), version_id: v.version_id.clone() });
                }
            }
            // Absent at the time.
            _ => {
                if !current.delete_marker {
                    actions.push(Action::Delete { key: key.to_string() });
                }
            }
        }
    }
    actions
}

/// `2026-10-17T09:30:00Z` (RFC 3339) or unix seconds.
pub fn parse_time(s: &str) -> anyhow::Result<(i64, u32)> {
    if let Ok(secs) = s.parse::<i64>() {
        return Ok((secs, 0));
    }
    let t = chrono::DateTime::parse_from_rfc3339(s).map_err(|e| anyhow::anyhow!("bad time {s:?} (RFC 3339 or unix seconds): {e}"))?;
    Ok((t.timestamp(), t.timestamp_subsec_nanos()))
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Summary {
    pub restored: usize,
    pub deleted: usize,
    pub applied: bool,
}

/// Restore the log at `s3://bucket/prefix` to how it was at `at`. Without `apply`, only reports
/// what it would do.
#[cfg(feature = "restore")]
pub async fn run(url: &str, at: (i64, u32), apply: bool, force: bool) -> anyhow::Result<Summary> {
    use anyhow::Context;
    use futures::StreamExt;
    let rest = url.strip_prefix("s3://").ok_or_else(|| anyhow::anyhow!("restore needs an s3:// log (a versioned bucket): {url}"))?;
    let (bucket, prefix) = rest.split_once('/').map(|(b, p)| (b, p.trim_matches('/'))).unwrap_or((rest, ""));
    let prefix = if prefix.is_empty() { "tasks" } else { prefix };

    let conf = aws_config::load_from_env().await;
    let mut b = aws_sdk_s3::config::Builder::from(&conf);
    if std::env::var("AWS_ENDPOINT_URL_S3").or_else(|_| std::env::var("AWS_ENDPOINT_URL")).is_ok() {
        // S3-compatible stores (MinIO) take path-style requests.
        b = b.force_path_style(true);
    }
    let s3 = aws_sdk_s3::Client::from_conf(b.build());

    let versioning = s3.get_bucket_versioning().bucket(bucket).send().await.context("reading the bucket's versioning")?;
    anyhow::ensure!(
        versioning.status() == Some(&aws_sdk_s3::types::BucketVersioningStatus::Enabled),
        "bucket {bucket} isn't versioned: there are no earlier versions to restore"
    );

    let lease_key = format!("{prefix}/lease");
    match s3.get_object().bucket(bucket).key(&lease_key).send().await {
        Ok(o) => {
            let body = o.body.collect().await?.into_bytes();
            let lease: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            if lease["released"] != serde_json::Value::Bool(true) && !force {
                anyhow::bail!(
                    "node {} holds the lease: stop the cell first (aws ecs update-service --desired-count 0) \
                     and wait for it to hand over, or pass --force if it crashed",
                    lease["node"].as_str().unwrap_or("?")
                );
            }
        }
        Err(e) if e.as_service_error().is_some_and(|e| e.is_no_such_key()) => {}
        Err(e) => return Err(anyhow::Error::new(e).context("reading the lease")),
    }

    let mut versions = Vec::new();
    let (mut key_marker, mut version_marker) = (None::<String>, None::<String>);
    loop {
        let page = s3
            .list_object_versions()
            .bucket(bucket)
            .prefix(format!("{prefix}/"))
            .set_key_marker(key_marker.take())
            .set_version_id_marker(version_marker.take())
            .send()
            .await
            .context("listing versions")?;
        let time = |t: Option<&aws_sdk_s3::primitives::DateTime>| t.map(|t| (t.secs(), t.subsec_nanos())).unwrap_or((0, 0));
        for v in page.versions() {
            versions.push(Version {
                key: v.key().unwrap_or_default().to_string(),
                version_id: v.version_id().unwrap_or("null").to_string(),
                modified: time(v.last_modified()),
                delete_marker: false,
                etag: v.e_tag().map(str::to_string),
            });
        }
        for m in page.delete_markers() {
            versions.push(Version {
                key: m.key().unwrap_or_default().to_string(),
                version_id: m.version_id().unwrap_or("null").to_string(),
                modified: time(m.last_modified()),
                delete_marker: true,
                etag: None,
            });
        }
        if page.is_truncated() != Some(true) {
            break;
        }
        key_marker = page.next_key_marker().map(str::to_string);
        version_marker = page.next_version_id_marker().map(str::to_string);
    }

    let actions = plan(&versions, at, |k| k == lease_key);
    let mut summary = Summary {
        restored: actions.iter().filter(|a| matches!(a, Action::Restore { .. })).count(),
        deleted: actions.iter().filter(|a| matches!(a, Action::Delete { .. })).count(),
        applied: false,
    };
    if !apply || actions.is_empty() {
        return Ok(summary);
    }
    let results: Vec<anyhow::Result<()>> = futures::stream::iter(actions)
        .map(|a| {
            let s3 = s3.clone();
            async move {
                match a {
                    Action::Restore { key, version_id } => {
                        let source = format!("{bucket}/{}?versionId={}", encode_key(&key), encode_key(&version_id));
                        s3.copy_object().bucket(bucket).key(&key).copy_source(source).send().await.with_context(|| format!("restoring {key}"))?;
                    }
                    Action::Delete { key } => {
                        s3.delete_object().bucket(bucket).key(&key).send().await.with_context(|| format!("deleting {key}"))?;
                    }
                }
                Ok(())
            }
        })
        .buffer_unordered(16)
        .collect()
        .await;
    for r in results {
        r?;
    }
    summary.applied = true;
    Ok(summary)
}

/// Percent-encode a key for `x-amz-copy-source`, keeping `/`.
#[cfg(feature = "restore")]
fn encode_key(k: &str) -> String {
    let mut out = String::new();
    for b in k.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(key: &str, id: &str, secs: i64, delete_marker: bool) -> Version {
        Version { key: key.into(), version_id: id.into(), modified: (secs, 0), delete_marker, etag: Some(format!("e-{id}")) }
    }

    #[test]
    fn every_key_goes_back_to_how_it_was() {
        let versions = vec![
            // Written before, untouched: nothing to do.
            v("t/wal/1", "a", 10, false),
            // Pruned after the time: back.
            v("t/wal/2", "b", 20, false),
            v("t/wal/2", "b-del", 60, true),
            // Written after the time: deleted.
            v("t/wal/3", "c", 70, false),
            // Pruned before the time: stays deleted.
            v("t/wal/0", "z", 5, false),
            v("t/wal/0", "z-del", 30, true),
            // Overwritten after the time: the earlier version.
            v("t/meta/pruned", "p3", 80, false),
            v("t/meta/pruned", "p2", 40, false),
            v("t/meta/pruned", "p1", 30, false),
            // Written and deleted after the time: already gone.
            v("t/snap/x/9.db.zst", "s", 70, false),
            v("t/snap/x/9.db.zst", "s-del", 75, true),
            // The lease is left alone.
            v("t/lease", "l2", 90, false),
            v("t/lease", "l1", 10, false),
        ];
        let mut got = plan(&versions, (50, 0), |k| k == "t/lease");
        got.sort_by_key(|a| format!("{a:?}"));
        assert_eq!(
            got,
            vec![
                Action::Delete { key: "t/wal/3".into() },
                Action::Restore { key: "t/meta/pruned".into(), version_id: "p2".into() },
                Action::Restore { key: "t/wal/2".into(), version_id: "b".into() },
            ]
        );
        // At the latest time, nothing changes.
        assert!(plan(&versions, (100, 0), |k| k == "t/lease").is_empty());
    }

    #[test]
    fn ties_order_deletes_after_writes_and_keep_the_listing_order() {
        // Written and pruned in the same second: absent at that second.
        let versions = vec![v("t/wal/5", "w", 50, false), v("t/wal/5", "d", 50, true)];
        assert_eq!(plan(&versions, (50, 0), |_| false), vec![]);
        assert_eq!(plan(&versions, (49, 0), |_| false), vec![]);
        // Two versions in one second: the one listed first is the newer.
        let versions = vec![v("t/meta/pruned", "new", 50, false), v("t/meta/pruned", "old", 50, false), v("t/meta/pruned", "older", 40, false)];
        assert_eq!(plan(&versions, (50, 0), |_| false), vec![]);
        assert_eq!(plan(&versions, (45, 0), |_| false), vec![Action::Restore { key: "t/meta/pruned".into(), version_id: "older".into() }]);
        // Already restored: the copy has a new id but the same content.
        let mut copy = v("t/meta/pruned", "copy", 60, false);
        copy.etag = Some("e-older".into());
        let restored = [vec![copy], versions.clone()].concat();
        assert_eq!(plan(&restored, (45, 0), |_| false), vec![]);
        // Created and restored back over a delete marker.
        let versions = vec![v("t/wal/6", "d", 60, true), v("t/wal/6", "w", 40, false)];
        assert_eq!(plan(&versions, (50, 0), |_| false), vec![Action::Restore { key: "t/wal/6".into(), version_id: "w".into() }]);
    }

    #[test]
    fn times_parse() {
        assert_eq!(parse_time("1700000000").unwrap(), (1_700_000_000, 0));
        assert_eq!(parse_time("2023-11-14T22:13:20.5Z").unwrap(), (1_700_000_000, 500_000_000));
        assert!(parse_time("yesterday").is_err());
    }
}
