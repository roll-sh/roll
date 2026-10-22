//! S3 Express One Zone directory buckets: their names and `CreateSession` sessions.
//!
//! A bucket is a directory bucket exactly when its name has the directory form
//! `<base>--<zone id>--x-s3`. General purpose bucket names can never end in `--x-s3` (S3
//! reserves the suffix, and `validate::bucket_name` always refused it), so the name alone tells,
//! with no flag to keep in sync. Any zone id is accepted (one region, one "zone" per deployment):
//! the zone only names the bucket.
//!
//! Underneath, a directory bucket is an ordinary roll bucket: one object is one blob, overwrites
//! and deletes go to the trash, and it can be branched (a branch of a directory bucket is a
//! directory bucket, and only directory buckets have directory branches). What S3 doesn't allow
//! for directory buckets (versioning, Object Lock) the store refuses here too; the rest of their
//! API surface is `s3-server`'s.

use crate::error::{Result, StoreError};
use crate::model::{AccessKey, Permission};
use crate::store::Store;
use crate::sts::{Credentials, Session, min_permission};
use serde::{Deserialize, Serialize};

/// The suffix of every directory bucket name.
pub const SUFFIX: &str = "--x-s3";

/// How long a `CreateSession` session lasts (as on S3).
pub const SESSION_S: i64 = 300;

/// The zone id in a directory bucket's name, if it has the directory form.
pub fn zone_of(name: &str) -> Option<&str> {
    let rest = name.strip_suffix(SUFFIX)?;
    let (base, zone) = rest.rsplit_once("--")?;
    (!base.is_empty() && !zone.is_empty()).then_some(zone)
}

/// Whether `name` is a directory bucket's.
pub fn is_directory_bucket(name: &str) -> bool {
    zone_of(name).is_some()
}

/// S3's directory bucket naming rules: 3-63 characters including the suffix, lowercase letters,
/// digits and hyphens, beginning with a letter or digit, ending in `--<zone id>--x-s3`, and
/// none of the reserved prefixes.
pub fn validate_name(name: &str) -> Result<()> {
    let bad = |why: &str| Err(StoreError::InvalidBucketName(format!("{name:?}: {why}")));
    if !(3..=63).contains(&name.len()) {
        return bad("must be 3-63 characters");
    }
    if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return bad("directory bucket names have only lowercase letters, digits and '-'");
    }
    let Some(zone) = zone_of(name) else { return bad("directory bucket names end in --<zone id>--x-s3") };
    let base = &name[..name.len() - SUFFIX.len() - zone.len() - 2];
    if !base.as_bytes()[0].is_ascii_alphanumeric() || !base.as_bytes()[base.len() - 1].is_ascii_alphanumeric() {
        return bad("the base name must begin and end with a letter or digit");
    }
    if !zone.as_bytes()[0].is_ascii_alphanumeric() || !zone.as_bytes()[zone.len() - 1].is_ascii_alphanumeric() || zone.contains("--") {
        return bad("the zone id must begin and end with a letter or digit");
    }
    for p in ["xn--", "sthree-", "amzn-s3-demo-"] {
        if name.starts_with(p) {
            return bad(&format!("can't start with {p}"));
        }
    }
    Ok(())
}

/// What a `CreateSession` session may reach: one directory bucket, read-only or read-write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpressScope {
    #[serde(rename = "b")]
    pub bucket: String,
    #[serde(rename = "ro", default)]
    pub read_only: bool,
}

impl ExpressScope {
    /// `ReadOnly` or `ReadWrite` (`s3express:SessionMode`).
    pub fn mode(&self) -> &'static str {
        if self.read_only { "ReadOnly" } else { "ReadWrite" }
    }

    /// The session's permission and scope, from those of what it was made from: never more,
    /// and never past its bucket. `ReadOnly` reads; `ReadWrite` does what its creator may do on
    /// the bucket. (S3 limits sessions to object operations, but SDKs given a custom endpoint
    /// sign CreateBucket, DeleteBucket and bucket policy calls with the session too.)
    pub(crate) fn narrow(&self, permission: Permission, buckets: Option<Vec<String>>) -> (Permission, Option<Vec<String>>) {
        let covered = buckets.as_ref().is_none_or(|b| b.contains(&self.bucket));
        let permission = if self.read_only { min_permission(permission, Permission::Read) } else { permission };
        (permission, Some(if covered { vec![self.bucket.clone()] } else { Vec::new() }))
    }
}

impl Store {
    /// S3 Express `CreateSession`: credentials for `bucket` (a directory bucket) that last
    /// [`SESSION_S`] (never past the session `caller` uses, if it's one). The session has the
    /// caller's identity, permission and bounds (its role, policies and tags), narrowed to the
    /// bucket and the mode. Nothing is stored. The caller's authorization to create it is
    /// `s3-server`'s.
    pub async fn create_express_session(&self, caller: &AccessKey, bucket: &str, read_only: bool) -> Result<Credentials> {
        if !is_directory_bucket(bucket) {
            return Err(StoreError::InvalidRequest("CreateSession is only for directory buckets".into()));
        }
        let now = self.now();
        let (mut base, duration) = match &caller.session {
            Some(s) if s.express.is_some() => return Err(StoreError::InvalidRequest("a CreateSession session can't create sessions".into())),
            Some(s) => ((**s).clone(), ((s.expires_at - now) / 1000).min(SESSION_S)),
            None => {
                let mut s = Session::new(caller.permission);
                s.key = Some(caller.id.clone());
                (s, SESSION_S)
            }
        };
        if duration < 1 {
            return Err(StoreError::InvalidRequest("the session's credentials are about to expire".into()));
        }
        base.express = Some(ExpressScope { bucket: bucket.to_string(), read_only });
        let (_, creds) = self.mint_session(base, duration).await?;
        Ok(creds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(zone_of("logs--use1-az4--x-s3"), Some("use1-az4"));
        assert_eq!(zone_of("my-logs--usw2-lax1-az1--x-s3"), Some("usw2-lax1-az1"));
        assert_eq!(zone_of("logs--x-s3"), None);
        assert_eq!(zone_of("--use1-az4--x-s3"), None);
        assert_eq!(zone_of("plain-bucket"), None);
        for ok in ["logs--use1-az4--x-s3", "a1--z--x-s3", "my-app-data--usw2-az1--x-s3"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["Logs--use1-az4--x-s3", "lo.gs--use1-az4--x-s3", "logs-x-s3", "-logs--az--x-s3", "logs---az--x-s3", "xn--a--az1--x-s3", &format!("{}--use1-az4--x-s3", "a".repeat(60))] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        assert!(crate::validate::bucket_name("logs--use1-az4--x-s3").is_err(), "never a general purpose name");
    }

    #[test]
    fn scope_only_narrows() {
        let rw = ExpressScope { bucket: "d--z1--x-s3".into(), read_only: false };
        let ro = ExpressScope { read_only: true, ..rw.clone() };
        assert_eq!(rw.narrow(Permission::Admin, None), (Permission::Admin, Some(vec!["d--z1--x-s3".to_string()])));
        assert_eq!(rw.narrow(Permission::Write, None).0, Permission::Write);
        assert_eq!(rw.narrow(Permission::Read, None).0, Permission::Read);
        assert_eq!(ro.narrow(Permission::Admin, None).0, Permission::Read);
        assert_eq!(rw.narrow(Permission::Write, Some(vec!["other".into()])).1, Some(Vec::new()), "outside the key's scope: nothing");
        assert_eq!(ro.mode(), "ReadOnly");
    }
}
