//! Get/Put/DeleteBucketReplication and the `x-amz-replication-status` header.
//!
//! Replication goes between roll buckets of the same deployment and is metadata-only: the
//! replica shares the source version's blob (`s3_store::Store::replicate`). A destination that
//! isn't a roll bucket would be a real AWS bucket, which isn't supported (`NotImplemented`).

use crate::config::{Doc, malformed, node_xml, read_xml, reading, writing};
use crate::ctx::{Ctx, no_content};
use crate::error::StoreResult;
use crate::response::{self, Reply, set};
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use s3_proto::xml::Node;
use s3_proto::{Code, S3Error};
use s3_store::{Bucket, ReplicationConfig, ReplicationRule, RuleFilter, StoreError};

fn status(n: &Node, name: &str) -> Result<Option<bool>, S3Error> {
    match n.child(name).map(|c| c.text_of("Status").map(str::trim)) {
        None => Ok(None),
        Some(Some("Enabled")) => Ok(Some(true)),
        Some(Some("Disabled")) => Ok(Some(false)),
        Some(_) => Err(malformed()),
    }
}

fn tag(t: &Node) -> Result<(String, String), S3Error> {
    Ok((t.text_of("Key").filter(|k| !k.is_empty()).ok_or_else(malformed)?.to_string(), t.text_of("Value").ok_or_else(malformed)?.to_string()))
}

/// The roll bucket an S3 bucket ARN names.
pub(crate) fn arn_bucket(arn: &str) -> Option<&str> {
    arn.trim().strip_prefix("arn:aws:s3:::").filter(|b| !b.is_empty() && !b.contains('/'))
}

fn parse_rule(r: &Node) -> Result<ReplicationRule, S3Error> {
    let enabled = match r.text_of("Status").map(str::trim) {
        Some("Enabled") => true,
        Some("Disabled") => false,
        _ => return Err(malformed()),
    };
    let priority = r.text_of("Priority").map(|p| p.trim().parse::<i64>().map_err(|_| malformed())).transpose()?;
    let filter = match r.child("Filter") {
        None => None,
        Some(f) => {
            let mut out = RuleFilter::default();
            if f.children.len() > 1 {
                return Err(malformed());
            }
            for c in &f.children {
                match c.name.as_str() {
                    "Prefix" => out.prefix = Some(c.text.clone()),
                    "Tag" => out.tags.push(tag(c)?),
                    "And" => {
                        out.and = true;
                        for a in &c.children {
                            match a.name.as_str() {
                                "Prefix" => out.prefix = Some(a.text.clone()),
                                "Tag" => out.tags.push(tag(a)?),
                                _ => return Err(malformed()),
                            }
                        }
                    }
                    _ => return Err(malformed()),
                }
            }
            Some(out)
        }
    };
    let legacy_prefix = r.child("Prefix").map(|p| p.text.clone());
    if legacy_prefix.is_some() && filter.is_some() {
        return Err(malformed());
    }
    if filter.is_some() && r.child("DeleteMarkerReplication").is_none() {
        return Err(S3Error::msg(Code::InvalidRequest, "DeleteMarkerReplication must be specified for this version of Cross Region Replication configuration schema."));
    }
    let d = r.child("Destination").ok_or_else(malformed)?;
    let arn = d.text_of("Bucket").ok_or_else(malformed)?;
    let destination = arn_bucket(arn).ok_or_else(|| S3Error::msg(Code::InvalidArgument, "Invalid ARN").with("ArgumentName", "Bucket").with("ArgumentValue", arn.to_string()))?.to_string();
    let owner_override = match d.child("AccessControlTranslation") {
        None => false,
        Some(a) if a.text_of("Owner").map(str::trim) == Some("Destination") => true,
        Some(_) => return Err(malformed()),
    };
    let extra: String = d.children.iter().filter(|c| matches!(c.name.as_str(), "EncryptionConfiguration" | "ReplicationTime" | "Metrics")).map(node_xml).collect();
    Ok(ReplicationRule {
        id: r.text_of("ID").map(str::trim).unwrap_or_default().to_string(),
        priority,
        enabled,
        legacy_prefix,
        filter,
        delete_markers: status(r, "DeleteMarkerReplication")?.unwrap_or(false),
        destination,
        storage_class: d.text_of("StorageClass").map(|s| s.trim().to_string()),
        owner_override,
        account: d.text_of("Account").map(|s| s.trim().to_string()),
        existing_objects: status(r, "ExistingObjectReplication")?.unwrap_or(false),
        source_selection: r.child("SourceSelectionCriteria").map(node_xml),
        destination_extra: Some(extra).filter(|e| !e.is_empty()),
    })
}

pub(crate) fn parse(n: &Node) -> Result<ReplicationConfig, S3Error> {
    let rules = n.all("Rule").map(parse_rule).collect::<Result<Vec<_>, _>>()?;
    Ok(ReplicationConfig { role: n.text_of("Role").unwrap_or_default().trim().to_string(), rules })
}

fn enabled(b: bool) -> &'static str {
    if b { "Enabled" } else { "Disabled" }
}

pub(crate) fn render(cfg: &ReplicationConfig) -> String {
    let mut d = Doc::new("ReplicationConfiguration");
    d.el("Role", &cfg.role);
    for r in &cfg.rules {
        d.open("Rule").el("ID", &r.id);
        if let Some(p) = r.priority {
            d.el("Priority", &p.to_string());
        }
        if let Some(p) = &r.legacy_prefix {
            d.el("Prefix", p);
        }
        if let Some(f) = &r.filter {
            d.open("Filter");
            if f.and {
                d.open("And");
            }
            if let Some(p) = &f.prefix {
                d.el("Prefix", p);
            }
            for (k, v) in &f.tags {
                d.open("Tag").el("Key", k).el("Value", v).close("Tag");
            }
            if f.and {
                d.close("And");
            }
            d.close("Filter");
        }
        d.el("Status", enabled(r.enabled));
        if let Some(s) = &r.source_selection {
            d.raw(s);
        }
        if r.existing_objects {
            d.open("ExistingObjectReplication").el("Status", "Enabled").close("ExistingObjectReplication");
        }
        d.open("Destination").el("Bucket", &format!("arn:aws:s3:::{}", r.destination));
        d.opt("Account", r.account.as_deref());
        d.opt("StorageClass", r.storage_class.as_deref());
        if r.owner_override {
            d.open("AccessControlTranslation").el("Owner", "Destination").close("AccessControlTranslation");
        }
        if let Some(x) = &r.destination_extra {
            d.raw(x);
        }
        d.close("Destination");
        if r.legacy_prefix.is_none() {
            d.open("DeleteMarkerReplication").el("Status", enabled(r.delete_markers)).close("DeleteMarkerReplication");
        }
        d.close("Rule");
    }
    d.finish("ReplicationConfiguration")
}

pub(crate) fn get(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    reading(ctx, bucket)?;
    let cfg = ctx.store().replication(&bucket.name).s3()?.ok_or_else(|| S3Error::new(Code::ReplicationConfigurationNotFoundError).with("BucketName", bucket.name.clone()))?;
    response::ok_xml(render(&cfg))
}

pub(crate) async fn put(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    writing(ctx, bucket)?;
    let n = read_xml(ctx, body, "ReplicationConfiguration").await?;
    let mut cfg = parse(&n)?;
    cfg.validate(&bucket.name).map_err(|m| S3Error::msg(Code::InvalidRequest, m))?;
    for r in &cfg.rules {
        match ctx.store().bucket(&r.destination) {
            Ok(_) => {}
            Err(StoreError::NoSuchBucket(_)) => {
                return Err(S3Error::msg(
                    Code::NotImplemented,
                    format!("Replication to {} isn't supported: the destination must be a bucket in this roll deployment (replication to buckets outside roll, such as AWS S3 buckets, is not implemented).", r.destination),
                ));
            }
            Err(e) => return Err(crate::error::store_error(e)),
        }
    }
    ctx.store().set_replication(&bucket.name, Some(&cfg)).await.s3()?;
    Ok(response::empty(StatusCode::OK))
}

pub(crate) async fn delete(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    writing(ctx, bucket)?;
    ctx.store().set_replication(&bucket.name, None).await.s3()?;
    no_content()
}

/// `x-amz-replication-status` on GET and HEAD.
pub(crate) fn set_status(ctx: &Ctx, bucket: &Bucket, key: &str, version_id: &str, r: &mut Response) {
    if let Ok(Some(s)) = ctx.store().replication_status(&bucket.id, key, version_id) {
        set(r, "x-amz-replication-status", &s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let x = br#"<ReplicationConfiguration><Role>arn:aws:iam::1:role/r</Role>
          <Rule><ID>a</ID><Priority>2</Priority><Status>Enabled</Status>
            <Filter><And><Prefix>img/</Prefix><Tag><Key>k</Key><Value>v</Value></Tag></And></Filter>
            <DeleteMarkerReplication><Status>Enabled</Status></DeleteMarkerReplication>
            <Destination><Bucket>arn:aws:s3:::dst</Bucket><StorageClass>STANDARD_IA</StorageClass><Account>1</Account>
              <AccessControlTranslation><Owner>Destination</Owner></AccessControlTranslation>
              <ReplicationTime><Status>Enabled</Status><Time><Minutes>15</Minutes></Time></ReplicationTime></Destination>
            <ExistingObjectReplication><Status>Enabled</Status></ExistingObjectReplication></Rule>
          <Rule><Status>Disabled</Status><Prefix>logs/</Prefix><Destination><Bucket>arn:aws:s3:::dst</Bucket></Destination></Rule>
        </ReplicationConfiguration>"#;
        let cfg = parse(&s3_proto::xml::parse(x).unwrap()).unwrap();
        let r = &cfg.rules[0];
        assert_eq!((r.priority, r.enabled, r.delete_markers, r.owner_override, r.existing_objects), (Some(2), true, true, true, true));
        assert_eq!(r.filter.as_ref().unwrap().tags, [("k".to_string(), "v".to_string())]);
        assert_eq!(r.destination, "dst");
        assert!(r.destination_extra.as_deref().unwrap().contains("<Minutes>15</Minutes>"));
        assert_eq!(cfg.rules[1].legacy_prefix.as_deref(), Some("logs/"));
        let back = parse(&s3_proto::xml::parse(render(&cfg).as_bytes()).unwrap()).unwrap();
        assert_eq!(back, cfg);
        let no_dmr = br#"<ReplicationConfiguration><Rule><Status>Enabled</Status><Filter/><Destination><Bucket>arn:aws:s3:::d</Bucket></Destination></Rule></ReplicationConfiguration>"#;
        assert_eq!(parse(&s3_proto::xml::parse(no_dmr).unwrap()).unwrap_err().code, Code::InvalidRequest);
        assert_eq!(arn_bucket("arn:aws:s3:::b"), Some("b"));
        assert_eq!(arn_bucket("arn:aws:s3:::b/x"), None);
    }
}
