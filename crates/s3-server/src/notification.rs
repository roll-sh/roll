//! Get/PutBucketNotificationConfiguration: S3's XML to and from the store's
//! [`NotificationConfig`]. Destination validation and the `s3:TestEvent` are the
//! [`crate::notify::Notifier`]'s.

use crate::config::{Doc, empty_doc, malformed, read_xml, reading, writing};
use crate::ctx::Ctx;
use crate::error::StoreResult;
use crate::notify::NotifyError;
use crate::response::{self, Reply};
use axum::body::Body;
use axum::http::StatusCode;
use s3_proto::xml::Node;
use s3_proto::{Code, S3Error};
use s3_store::{Bucket, NotificationConfig, NotificationTarget};

/// `(type, configuration element, destination element)`.
const KINDS: [(&str, &str, &str); 3] = [("topic", "TopicConfiguration", "Topic"), ("queue", "QueueConfiguration", "Queue"), ("function", "CloudFunctionConfiguration", "CloudFunction")];

pub(crate) fn render(cfg: &NotificationConfig) -> String {
    if cfg.is_empty() {
        return empty_doc("NotificationConfiguration");
    }
    let mut d = Doc::new("NotificationConfiguration");
    for (kind, el, dest) in KINDS {
        for t in cfg.configurations.iter().filter(|t| t.kind == kind) {
            d.open(el).el("Id", &t.id).el(dest, &t.destination);
            for e in &t.events {
                d.el("Event", e);
            }
            if t.prefix.is_some() || t.suffix.is_some() {
                d.open("Filter").open("S3Key");
                for (name, v) in [("Prefix", &t.prefix), ("Suffix", &t.suffix)] {
                    if let Some(v) = v {
                        d.open("FilterRule").el("Name", name).el("Value", v).close("FilterRule");
                    }
                }
                d.close("S3Key").close("Filter");
            }
            d.close(el);
        }
    }
    if cfg.event_bridge {
        d.raw("<EventBridgeConfiguration></EventBridgeConfiguration>");
    }
    d.finish("NotificationConfiguration")
}

fn invalid(msg: impl Into<String>) -> S3Error {
    S3Error::msg(Code::InvalidArgument, msg)
}

pub(crate) fn parse(n: &Node) -> Result<NotificationConfig, S3Error> {
    let mut cfg = NotificationConfig::default();
    for c in &n.children {
        if c.name == "EventBridgeConfiguration" {
            cfg.event_bridge = true;
            continue;
        }
        let Some((kind, _, dest)) = KINDS.iter().find(|(_, el, _)| *el == c.name) else { return Err(malformed()) };
        let destination = c.text_of(dest).map(str::trim).filter(|d| !d.is_empty()).ok_or_else(malformed)?.to_string();
        let events: Vec<String> = c.all("Event").map(|e| e.text.trim().to_string()).collect();
        let mut t = NotificationTarget { id: c.text_of("Id").map(str::trim).unwrap_or_default().to_string(), kind: kind.to_string(), destination, events, prefix: None, suffix: None };
        if let Some(f) = c.child("Filter") {
            let key = f.child("S3Key").ok_or_else(malformed)?;
            for rule in key.all("FilterRule") {
                let name = rule.text_of("Name").map(str::trim).unwrap_or_default().to_ascii_lowercase();
                let value = rule.text_of("Value").unwrap_or_default().to_string();
                let slot = match name.as_str() {
                    "prefix" => &mut t.prefix,
                    "suffix" => &mut t.suffix,
                    _ => return Err(invalid("filter rule name must be either prefix or suffix")),
                };
                if slot.is_some() {
                    return Err(invalid(format!("Cannot specify more than one {name} rule in a filter.")));
                }
                *slot = Some(value);
            }
        }
        cfg.configurations.push(t);
    }
    Ok(cfg)
}

const NAMES: [&str; 10] = ["ArgumentName1", "ArgumentName2", "ArgumentName3", "ArgumentName4", "ArgumentName5", "ArgumentName6", "ArgumentName7", "ArgumentName8", "ArgumentName9", "ArgumentName10"];
const VALUES: [&str; 10] = ["ArgumentValue1", "ArgumentValue2", "ArgumentValue3", "ArgumentValue4", "ArgumentValue5", "ArgumentValue6", "ArgumentValue7", "ArgumentValue8", "ArgumentValue9", "ArgumentValue10"];

/// S3's errors for a refused configuration.
pub(crate) fn notify_error(e: NotifyError) -> S3Error {
    match e {
        NotifyError::Invalid(m) => invalid(m),
        NotifyError::Destinations(failed) => {
            let mut err = invalid("Unable to validate the following destination configurations");
            for (i, (arn, why)) in failed.into_iter().take(10).enumerate() {
                err = err.with(NAMES[i], arn).with(VALUES[i], why);
            }
            err
        }
        NotifyError::Store(e) => crate::error::store_error(e),
    }
}

pub(crate) fn get(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    reading(ctx, bucket)?;
    let cfg = ctx.store().notifications(&bucket.name).s3()?.unwrap_or_default();
    response::ok_xml(render(&cfg))
}

pub(crate) async fn put(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    writing(ctx, bucket)?;
    let n = read_xml(ctx, body, "NotificationConfiguration").await?;
    let cfg = parse(&n)?;
    let skip = ctx.h("x-amz-skip-destination-validation").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
    if cfg.is_empty() {
        ctx.store().set_notifications(&bucket.name, None).await.s3()?;
    } else {
        ctx.server.notifier.put_config(bucket, cfg, skip).await.map_err(notify_error)?;
    }
    Ok(response::empty(StatusCode::OK))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let x = br#"<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
          <QueueConfiguration><Id>q</Id><Queue>arn:roll:queues:/:orders</Queue><Event>s3:ObjectCreated:*</Event>
            <Filter><S3Key><FilterRule><Name>prefix</Name><Value>img/</Value></FilterRule><FilterRule><Name>Suffix</Name><Value>.jpg</Value></FilterRule></S3Key></Filter></QueueConfiguration>
          <TopicConfiguration><Topic>arn:roll:webhook:https://x/</Topic><Event>s3:ObjectRemoved:*</Event><Event>s3:ObjectTagging:*</Event></TopicConfiguration>
          <EventBridgeConfiguration/>
        </NotificationConfiguration>"#;
        let cfg = parse(&s3_proto::xml::parse(x).unwrap()).unwrap();
        assert_eq!(cfg.configurations.len(), 2);
        assert_eq!(cfg.configurations[0].prefix.as_deref(), Some("img/"));
        assert_eq!(cfg.configurations[0].suffix.as_deref(), Some(".jpg"));
        assert_eq!(cfg.configurations[1].kind, "topic");
        assert!(cfg.event_bridge);
        let back = parse(&s3_proto::xml::parse(render(&cfg).as_bytes()).unwrap()).unwrap();
        // Rendered by type: topics first.
        assert_eq!(back.configurations[1], cfg.configurations[0]);
        assert_eq!(back.configurations[0], cfg.configurations[1]);
        assert!(back.event_bridge);
        let twice = br#"<NotificationConfiguration><QueueConfiguration><Queue>a</Queue><Event>s3:ObjectCreated:*</Event><Filter><S3Key><FilterRule><Name>prefix</Name><Value>a</Value></FilterRule><FilterRule><Name>prefix</Name><Value>b</Value></FilterRule></S3Key></Filter></QueueConfiguration></NotificationConfiguration>"#;
        assert_eq!(parse(&s3_proto::xml::parse(twice).unwrap()).unwrap_err().code, Code::InvalidArgument);
        assert!(render(&NotificationConfig::default()).ends_with("/>"));
    }
}
