//! Access points and Object Lambda access points (S3 Control's `CreateAccessPoint`,
//! `CreateAccessPointForObjectLambda` and friends): named entry points to a bucket with their
//! own policy, public access block and network origin, addressed by ARN, host name or alias.
//!
//! The store keeps their definitions (`access_points`, one durable row each); requests through
//! them are `s3-server`'s (`accesspoint/`). An access point names its bucket by name, as on S3:
//! deleting the bucket leaves the access point, whose requests then find no bucket. An Object
//! Lambda access point names its supporting access point instead, and keeps its configuration
//! (the transformations) as JSON that `s3-server` owns.

use crate::config::PublicAccessBlock;
use crate::error::{Result, StoreError};
use crate::rows;
use crate::store::Store;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

/// Which kind of access point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum AccessPointKind {
    /// `arn:aws:s3:<region>:<account>:accesspoint/<name>`.
    Standard,
    /// `arn:aws:s3-object-lambda:<region>:<account>:accesspoint/<name>`.
    ObjectLambda,
}

impl AccessPointKind {
    fn col(self) -> &'static str {
        match self {
            Self::Standard => "ap",
            Self::ObjectLambda => "olap",
        }
    }

    fn parse(s: &str) -> Self {
        if s == "olap" { Self::ObjectLambda } else { Self::Standard }
    }
}

/// An access point as stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AccessPoint {
    /// The 12-digit account it belongs to.
    pub account: String,
    pub kind: AccessPointKind,
    pub name: String,
    /// `<name>-<random>-s3alias` (Object Lambda: `<name>-<random>--ol-s3`): usable wherever a
    /// bucket name is.
    pub alias: String,
    /// The bucket's name; for an Object Lambda access point, its supporting access point's name.
    pub bucket: String,
    /// `Internet` or `VPC`.
    pub network_origin: String,
    pub vpc_id: Option<String>,
    /// The access point policy (JSON), validated by `s3-server` when it was put.
    pub policy: Option<String>,
    /// The access point's own public access block (on top of the bucket's).
    pub block: PublicAccessBlock,
    /// An Object Lambda access point's configuration (JSON owned by `s3-server`).
    pub config: Option<String>,
    /// Unix ms.
    pub created_at: i64,
}

/// What [`Store::create_access_point`] takes.
#[derive(Clone, Debug, Default)]
pub struct NewAccessPoint {
    pub account: String,
    pub name: String,
    pub bucket: String,
    pub vpc_id: Option<String>,
    pub block: PublicAccessBlock,
    pub config: Option<String>,
}

const COLS: &str = "account, kind, name, alias, bucket, network_origin, vpc_id, policy, block, config, created_at";

fn ap_row(r: &rusqlite::Row) -> rusqlite::Result<AccessPoint> {
    let kind: String = r.get(1)?;
    let block: String = r.get(8)?;
    Ok(AccessPoint {
        account: r.get(0)?,
        kind: AccessPointKind::parse(&kind),
        name: r.get(2)?,
        alias: r.get(3)?,
        bucket: r.get(4)?,
        network_origin: r.get(5)?,
        vpc_id: r.get(6)?,
        policy: r.get(7)?,
        // A block that no longer parses blocks everything.
        block: serde_json::from_str(&block).unwrap_or(PublicAccessBlock::ALL),
        config: r.get(9)?,
        created_at: r.get(10)?,
    })
}

fn find(c: &Connection, account: &str, kind: AccessPointKind, name: &str) -> Result<Option<AccessPoint>> {
    Ok(c.prepare_cached(&format!("SELECT {COLS} FROM access_points WHERE account = ?1 AND kind = ?2 AND name = ?3"))?.query_row(params![account, kind.col(), name], ap_row).optional()?)
}

/// S3's access point name rules: 3-50 characters of lowercase letters, digits and hyphens,
/// starting and ending with a letter or digit, not ending in `-s3alias`, not starting with
/// `xn--`.
pub fn validate_access_point_name(name: &str) -> std::result::Result<(), String> {
    let ok = (3..=50).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && name.bytes().next().is_some_and(|b| b != b'-')
        && name.bytes().last().is_some_and(|b| b != b'-')
        && !name.ends_with("-s3alias")
        && !name.ends_with("--ol-s3")
        && !name.starts_with("xn--");
    if ok { Ok(()) } else { Err("Your Access Point name is invalid: 3-50 lowercase letters, numbers and hyphens, starting and ending with a letter or number".into()) }
}

/// Whether a bucket name is an access point alias (S3 reserves the suffixes).
pub fn is_alias(name: &str) -> bool {
    name.ends_with("-s3alias") || name.ends_with("--ol-s3")
}

fn new_alias(name: &str, kind: AccessPointKind) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut b = [0u8; 26];
    getrandom::fill(&mut b).expect("os randomness");
    let random: String = b.iter().map(|x| ALPHABET[*x as usize % ALPHABET.len()] as char).collect();
    // At most 63 characters, like a bucket name.
    let stem: String = name.chars().take(26).collect::<String>().trim_end_matches('-').to_string();
    match kind {
        AccessPointKind::Standard => format!("{stem}-{random}-s3alias"),
        AccessPointKind::ObjectLambda => format!("{stem}-{random}--ol-s3"),
    }
}

impl Store {
    /// Create an access point. A standard one's bucket must exist, an Object Lambda one's
    /// supporting access point too; a name already taken (in the account and kind) is
    /// `Conflict`.
    pub async fn create_access_point(&self, kind: AccessPointKind, ap: NewAccessPoint) -> Result<AccessPoint> {
        validate_access_point_name(&ap.name).map_err(StoreError::InvalidArgument)?;
        let now = self.now();
        let alias = new_alias(&ap.name, kind);
        let block = rows::json(&ap.block)?;
        self.write(|c| {
            if find(c, &ap.account, kind, &ap.name)?.is_some() {
                return Err(StoreError::Conflict(format!("an access point named {} already exists", ap.name)));
            }
            match kind {
                AccessPointKind::Standard => {
                    crate::store::find_bucket(c, &ap.bucket)?;
                }
                AccessPointKind::ObjectLambda => {
                    if find(c, &ap.account, AccessPointKind::Standard, &ap.bucket)?.is_none() {
                        return Err(StoreError::InvalidArgument(format!("the supporting access point {} doesn't exist", ap.bucket)));
                    }
                }
            }
            let origin = if ap.vpc_id.is_some() { "VPC" } else { "Internet" };
            c.execute(
                &format!("INSERT INTO access_points ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9, ?10)"),
                params![ap.account, kind.col(), ap.name, alias, ap.bucket, origin, ap.vpc_id, block, ap.config, now],
            )?;
            Ok(())
        })
        .await?;
        self.access_point(&ap.account, kind, &ap.name)?.ok_or_else(|| StoreError::internal("access point vanished"))
    }

    pub fn access_point(&self, account: &str, kind: AccessPointKind, name: &str) -> Result<Option<AccessPoint>> {
        self.read(|c| find(c, account, kind, name))
    }

    /// The access point an alias names, of either kind.
    pub fn access_point_by_alias(&self, alias: &str) -> Result<Option<AccessPoint>> {
        self.read(|c| Ok(c.prepare_cached(&format!("SELECT {COLS} FROM access_points WHERE alias = ?1"))?.query_row([alias], ap_row).optional()?))
    }

    /// An account's access points of `kind` (of one bucket, or every one) with names after
    /// `after`, in name order, at most `limit`.
    pub fn access_points(&self, account: &str, kind: AccessPointKind, bucket: Option<&str>, after: Option<&str>, limit: usize) -> Result<Vec<AccessPoint>> {
        self.read(|c| {
            let rows = c
                .prepare_cached(&format!("SELECT {COLS} FROM access_points WHERE account = ?1 AND kind = ?2 AND (?3 IS NULL OR bucket = ?3) AND name > ?4 ORDER BY name LIMIT ?5"))?
                .query_map(params![account, kind.col(), bucket, after.unwrap_or(""), limit as i64], ap_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Delete an access point (`NotFound` if there's none). A standard access point that an
    /// Object Lambda access point is built on can't be deleted until that one is.
    pub async fn delete_access_point(&self, account: &str, kind: AccessPointKind, name: &str) -> Result<()> {
        self.write(|c| {
            if find(c, account, kind, name)?.is_none() {
                return Err(StoreError::NotFound(format!("no access point {name}")));
            }
            if kind == AccessPointKind::Standard {
                let used: Option<String> = c.query_row("SELECT name FROM access_points WHERE account = ?1 AND kind = 'olap' AND bucket = ?2 LIMIT 1", params![account, name], |r| r.get(0)).optional()?;
                if let Some(o) = used {
                    return Err(StoreError::Conflict(format!("the Object Lambda access point {o} uses {name}; delete it first")));
                }
            }
            c.execute("DELETE FROM access_points WHERE account = ?1 AND kind = ?2 AND name = ?3", params![account, kind.col(), name])?;
            Ok(())
        })
        .await
    }

    /// Set (`Some`) or remove (`None`) an access point's policy; the caller validated it.
    pub async fn set_access_point_policy(&self, account: &str, kind: AccessPointKind, name: &str, policy: Option<&str>) -> Result<()> {
        self.update_access_point(account, kind, name, "policy", policy).await
    }

    /// Replace an Object Lambda access point's configuration; the caller validated it.
    pub async fn set_access_point_config(&self, account: &str, kind: AccessPointKind, name: &str, config: &str) -> Result<()> {
        self.update_access_point(account, kind, name, "config", Some(config)).await
    }

    async fn update_access_point(&self, account: &str, kind: AccessPointKind, name: &str, col: &'static str, value: Option<&str>) -> Result<()> {
        let n = self
            .write(|c| Ok(c.execute(&format!("UPDATE access_points SET {col} = ?4 WHERE account = ?1 AND kind = ?2 AND name = ?3"), params![account, kind.col(), name, value])?))
            .await?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("no access point {name}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_aliases() {
        for ok in ["abc", "my-access-point", "a1b2", &"a".repeat(50)] {
            assert!(validate_access_point_name(ok).is_ok(), "{ok}");
        }
        for bad in ["ab", "-abc", "abc-", "ABC", "a_b", "xn--abc", "abc-s3alias", &"a".repeat(51)] {
            assert!(validate_access_point_name(bad).is_err(), "{bad}");
        }
        let a = new_alias(&"x".repeat(50), AccessPointKind::Standard);
        assert!(a.len() <= 63 && a.ends_with("-s3alias") && is_alias(&a), "{a}");
        let o = new_alias("olap", AccessPointKind::ObjectLambda);
        assert!(o.ends_with("--ol-s3") && is_alias(&o));
        assert_ne!(new_alias("x", AccessPointKind::Standard), new_alias("x", AccessPointKind::Standard));
        assert!(!is_alias("bucket"));
    }
}
