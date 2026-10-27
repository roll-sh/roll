//! `roll storage notify …` and `roll storage replicate …`: a bucket's event notifications (roll
//! queues, roll streams, webhooks, AWS) and its replication to other roll buckets, through the
//! bucket `PATCH` of the control plane.

use super::{bucket_path, enc};
use crate::Api;
use crate::queues::fmt::{Align::L, Align::R, Table, count};
use crate::s;
use anyhow::{Context, bail};
use clap::Subcommand;
use reqwest::Method;
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum NotifyCmd {
    /// Send a bucket's events somewhere: `queue:<vhost>/<queue>` (or `queue:<queue>` on the
    /// default vhost), `stream:<stream>/<topic>`, an `https://` webhook, or an ARN.
    Add {
        bucket: String,
        /// Where events go.
        #[arg(long, value_name = "DEST")]
        to: String,
        /// Event types (repeatable or comma-separated).
        #[arg(long = "events", value_name = "EVENT", value_delimiter = ',', default_value = "s3:ObjectCreated:*")]
        events: Vec<String>,
        /// Only keys starting with this.
        #[arg(long)]
        prefix: Option<String>,
        /// Only keys ending with this.
        #[arg(long)]
        suffix: Option<String>,
        /// The configuration's id (generated when not given).
        #[arg(long)]
        id: Option<String>,
        /// Don't send the destinations a test event first.
        #[arg(long)]
        skip_validation: bool,
    },
    /// Stop sending events to a destination.
    Rm { bucket: String, id: String },
    /// The secret webhook deliveries are signed with (`X-Roll-Signature`).
    Secret {
        bucket: String,
        /// Replace it; deliveries are signed with the new one from now on.
        #[arg(long)]
        rotate: bool,
    },
}

#[derive(Subcommand)]
pub enum ReplicateCmd {
    /// Stop replicating: one rule, or all of them.
    Rm { bucket: String, id: Option<String> },
}

/// A destination as given on the command line → its ARN.
pub fn destination_arn(dest: &str) -> anyhow::Result<String> {
    let d = dest.trim();
    if d.starts_with("arn:") {
        return Ok(d.to_string());
    }
    if d.starts_with("https://") || d.starts_with("http://") {
        return Ok(format!("arn:roll:webhook:{d}"));
    }
    if let Some(rest) = d.strip_prefix("queue:") {
        let (vhost, queue) = rest.rsplit_once('/').unwrap_or(("", rest));
        if queue.is_empty() {
            bail!("{dest:?}: expected queue:<vhost>/<queue> or queue:<queue>");
        }
        let vhost = if vhost.is_empty() { "/" } else { vhost };
        return Ok(format!("arn:roll:queues:{}:{queue}", enc(vhost)));
    }
    if let Some(rest) = d.strip_prefix("stream:") {
        match rest.split_once('/') {
            Some((stream, topic)) if !stream.is_empty() && !topic.is_empty() => return Ok(format!("arn:roll:streams:{stream}:{topic}")),
            _ => bail!("{dest:?}: expected stream:<stream>/<topic>"),
        }
    }
    bail!("{dest:?}: expected queue:<vhost>/<queue>, stream:<stream>/<topic>, an https:// webhook URL, or an ARN")
}

/// An ARN in the form `--to` takes it.
pub fn describe(arn: &str) -> String {
    if let Some(url) = arn.strip_prefix("arn:roll:webhook:") {
        return url.to_string();
    }
    if let Some((vhost, queue)) = arn.strip_prefix("arn:roll:queues:").and_then(|r| r.split_once(':')) {
        let vhost = urlencoding::decode(vhost).map(|v| v.into_owned()).unwrap_or_else(|_| vhost.to_string());
        return if vhost == "/" { format!("queue:{queue}") } else { format!("queue:{vhost}/{queue}") };
    }
    if let Some((stream, topic)) = arn.strip_prefix("arn:roll:streams:").and_then(|r| r.split_once(':')) {
        return format!("stream:{stream}/{topic}");
    }
    arn.to_string()
}

fn joined(v: &Value) -> String {
    v.as_array().into_iter().flatten().map(s).collect::<Vec<_>>().join(",")
}

/// The key filter of a notification configuration, in a few words.
fn filter(c: &Value) -> String {
    match (c["prefix"].as_str(), c["suffix"].as_str()) {
        (Some(p), Some(x)) => format!("{p}*{x}"),
        (Some(p), None) => format!("{p}*"),
        (None, Some(x)) => format!("*{x}"),
        (None, None) => "*".into(),
    }
}

/// One notification configuration, in a line (for `show`).
pub fn notify_line(c: &Value, backlog: &Value) -> String {
    let dest = s(&c["destination"]);
    let mut line = format!("{}: {} {} → {}", s(&c["id"]), joined(&c["events"]), filter(c), describe(&dest));
    if let Some(n) = backlog[&dest]["pending"].as_u64().filter(|n| *n > 0) {
        line.push_str(&format!(" ({n} pending)"));
    }
    line
}

/// One replication rule, in a line (for `show`).
pub fn replicate_line(r: &Value) -> String {
    let mut what = Vec::new();
    if r["delete_markers"].as_bool() == Some(true) {
        what.push("delete markers".to_string());
    }
    if r["existing_objects"].as_bool() == Some(true) {
        what.push(if r["backfilling"].as_bool() == Some(true) { "existing objects (catching up)" } else { "existing objects" }.to_string());
    }
    for (k, v) in r["tags"].as_object().into_iter().flatten() {
        what.push(format!("tag {k}={}", s(v)));
    }
    let mut line = format!("{}: {}* → {}", s(&r["id"]), r["prefix"].as_str().unwrap_or(""), s(&r["destination"]));
    if !what.is_empty() {
        line.push_str(&format!(" ({})", what.join(", ")));
    }
    if r["enabled"].as_bool() == Some(false) {
        line.push_str(" [disabled]");
    }
    if let Some(n) = r["pending"].as_u64().filter(|n| *n > 0) {
        line.push_str(&format!(" ({n} pending)"));
    }
    line
}

async fn patch(api: &Api, bucket: &str, body: Value) -> anyhow::Result<Value> {
    api.call(Method::PATCH, &bucket_path(bucket, ""), Some(body)).await
}

/// `roll storage notify [<bucket> | add | rm | secret]`.
pub async fn notify(api: &Api, as_json: bool, bucket: Option<String>, cmd: Option<NotifyCmd>) -> anyhow::Result<Option<Value>> {
    match cmd {
        None => {
            let bucket = bucket.context("give a bucket: roll storage notify <bucket>")?;
            let b = api.get(&bucket_path(&bucket, "")).await?;
            let n = &b["notifications"];
            if as_json {
                return Ok(Some(n.clone()));
            }
            let configs = n["configurations"].as_array().cloned().unwrap_or_default();
            if configs.is_empty() && n["event_bridge"].as_bool() != Some(true) {
                println!("no notifications on {bucket}; add one with `roll storage notify add {bucket} --to <dest>`");
                return Ok(None);
            }
            let mut t = Table::new(&[("id", L), ("destination", L), ("events", L), ("keys", L), ("pending", R), ("last error", L)]);
            for c in &configs {
                let dest = s(&c["destination"]);
                let w = &n["backlog"][&dest];
                t.row(vec![s(&c["id"]), describe(&dest), joined(&c["events"]), filter(c), count(w["pending"].as_f64().unwrap_or(0.0)), w["last_error"].as_str().unwrap_or("-").to_string()]);
            }
            t.print();
            if n["event_bridge"].as_bool() == Some(true) {
                println!("every event also goes to EventBridge");
            }
            Ok(None)
        }
        Some(NotifyCmd::Add { bucket, to, events, prefix, suffix, id, skip_validation }) => {
            let destination = destination_arn(&to)?;
            let b = api.get(&bucket_path(&bucket, "")).await?;
            let mut configs = b["notifications"]["configurations"].as_array().cloned().unwrap_or_default();
            let events: Vec<String> = events.into_iter().map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect();
            configs.push(json!({ "id": id.unwrap_or_default(), "destination": destination, "events": events, "prefix": prefix, "suffix": suffix }));
            let v = patch(api, &bucket, json!({ "notifications": { "configurations": configs, "skip_destination_validation": skip_validation } })).await?;
            if as_json {
                return Ok(Some(v["notifications"].clone()));
            }
            let added = v["notifications"]["configurations"].as_array().and_then(|c| c.last()).cloned().unwrap_or_default();
            println!("{bucket}: {} {} → {} (id {})", joined(&added["events"]), filter(&added), describe(&destination), s(&added["id"]));
            if destination.starts_with("arn:roll:webhook:") {
                eprintln!("deliveries are signed with the bucket's webhook secret: `roll storage notify secret {bucket}`");
            }
            Ok(None)
        }
        Some(NotifyCmd::Rm { bucket, id }) => {
            let b = api.get(&bucket_path(&bucket, "")).await?;
            let mut configs = b["notifications"]["configurations"].as_array().cloned().unwrap_or_default();
            let before = configs.len();
            configs.retain(|c| c["id"].as_str() != Some(&id));
            if configs.len() == before {
                bail!("no notification {id} on {bucket} (see `roll storage notify {bucket}`)");
            }
            // What's left was checked when it was added.
            let v = patch(api, &bucket, json!({ "notifications": { "configurations": configs, "skip_destination_validation": true } })).await?;
            if as_json {
                return Ok(Some(v["notifications"].clone()));
            }
            println!("removed notification {id} from {bucket}");
            Ok(None)
        }
        Some(NotifyCmd::Secret { bucket, rotate }) => {
            let b = if rotate {
                patch(api, &bucket, json!({ "notifications": { "rotate_webhook_secret": true } })).await?
            } else {
                api.get(&bucket_path(&bucket, "")).await?
            };
            let secret = b["notifications"]["webhook_secret"].clone();
            if as_json {
                return Ok(Some(json!({ "webhook_secret": secret })));
            }
            match secret.as_str() {
                Some(sec) => {
                    println!("{sec}");
                    eprintln!("webhooks carry X-Roll-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of \"<t>.<body>\" with this secret>");
                }
                None => println!("{bucket} has no webhook secret yet: it's made with the first webhook destination"),
            }
            Ok(None)
        }
    }
}

/// `roll storage replicate <bucket> [--to <dest> …] | rm`.
#[allow(clippy::too_many_arguments)]
pub async fn replicate(
    api: &Api,
    as_json: bool,
    bucket: Option<String>,
    to: Option<String>,
    prefix: Option<String>,
    delete_markers: bool,
    existing: bool,
    id: Option<String>,
    cmd: Option<ReplicateCmd>,
) -> anyhow::Result<Option<Value>> {
    let hint = |e: anyhow::Error, bucket: &str| -> anyhow::Error {
        let m = e.to_string();
        if m.contains("Versioning must be") {
            anyhow::anyhow!("{m} (turn it on with `roll storage update {bucket} --versioning on`)")
        } else if m.contains("must have versioning enabled") {
            anyhow::anyhow!("{m} (turn it on with `roll storage update <dest> --versioning on`)")
        } else {
            e
        }
    };
    match cmd {
        Some(ReplicateCmd::Rm { bucket, id }) => {
            let body = match &id {
                None => json!({ "replication": null }),
                Some(id) => {
                    let b = api.get(&bucket_path(&bucket, "")).await?;
                    let mut rules = b["replication"]["rules"].as_array().cloned().unwrap_or_default();
                    let before = rules.len();
                    rules.retain(|r| r["id"].as_str() != Some(id));
                    if rules.len() == before {
                        bail!("no replication rule {id} on {bucket} (see `roll storage replicate {bucket}`)");
                    }
                    if rules.is_empty() { json!({ "replication": null }) } else { json!({ "replication": { "rules": rules } }) }
                }
            };
            let v = patch(api, &bucket, body).await?;
            if as_json {
                return Ok(Some(v["replication"].clone()));
            }
            match id {
                Some(id) => println!("removed replication rule {id} from {bucket}"),
                None => println!("{bucket} no longer replicates (replicas already made stay)"),
            }
            Ok(None)
        }
        None => {
            let bucket = bucket.context("give a bucket: roll storage replicate <bucket> [--to <dest-bucket>]")?;
            let Some(to) = to else {
                let b = api.get(&bucket_path(&bucket, "")).await?;
                if as_json {
                    return Ok(Some(b["replication"].clone()));
                }
                let rules = b["replication"]["rules"].as_array().cloned().unwrap_or_default();
                if rules.is_empty() {
                    println!("{bucket} doesn't replicate; start with `roll storage replicate {bucket} --to <bucket>`");
                    return Ok(None);
                }
                let mut t = Table::new(&[("id", L), ("to", L), ("keys", L), ("status", L), ("delete markers", L), ("existing", L), ("pending", R), ("last error", L)]);
                for r in &rules {
                    let yes = |v: &Value| if v.as_bool() == Some(true) { "yes" } else { "no" }.to_string();
                    let existing = if r["backfilling"].as_bool() == Some(true) { "catching up".to_string() } else { yes(&r["existing_objects"]) };
                    t.row(vec![
                        s(&r["id"]),
                        s(&r["destination"]),
                        format!("{}*", r["prefix"].as_str().unwrap_or("")),
                        if r["enabled"].as_bool() == Some(false) { "disabled" } else { "enabled" }.to_string(),
                        yes(&r["delete_markers"]),
                        existing,
                        count(r["pending"].as_f64().unwrap_or(0.0)),
                        r["last_error"].as_str().unwrap_or("-").to_string(),
                    ]);
                }
                t.print();
                return Ok(None);
            };
            let b = api.get(&bucket_path(&bucket, "")).await?;
            let mut rules = b["replication"]["rules"].as_array().cloned().unwrap_or_default();
            rules.push(json!({ "id": id, "destination": to, "prefix": prefix, "delete_markers": delete_markers, "existing_objects": existing }));
            let v = patch(api, &bucket, json!({ "replication": { "rules": rules } })).await.map_err(|e| hint(e, &bucket))?;
            if as_json {
                return Ok(Some(v["replication"].clone()));
            }
            let added = v["replication"]["rules"].as_array().and_then(|r| r.last()).cloned().unwrap_or_default();
            println!("{bucket}/{}* → {to} (rule {})", prefix.as_deref().unwrap_or(""), s(&added["id"]));
            eprintln!("replicas share the source's data: instant, with no bytes copied and no transfer cost");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destinations() {
        assert_eq!(destination_arn("queue:orders").unwrap(), "arn:roll:queues:%2F:orders");
        assert_eq!(destination_arn("queue:/orders").unwrap(), "arn:roll:queues:%2F:orders");
        assert_eq!(destination_arn("queue:shop/orders").unwrap(), "arn:roll:queues:shop:orders");
        assert_eq!(destination_arn("stream:events/uploads").unwrap(), "arn:roll:streams:events:uploads");
        assert_eq!(destination_arn("https://example.com/hook?a=1").unwrap(), "arn:roll:webhook:https://example.com/hook?a=1");
        assert_eq!(destination_arn("arn:aws:sqs:us-east-1:1:q").unwrap(), "arn:aws:sqs:us-east-1:1:q");
        for bad in ["queue:", "queue:x/", "stream:x", "stream:/t", "ftp://x", "orders"] {
            assert!(destination_arn(bad).is_err(), "{bad}");
        }
        for friendly in ["queue:orders", "queue:shop/orders", "stream:events/uploads", "https://example.com/hook"] {
            assert_eq!(describe(&destination_arn(friendly).unwrap()), friendly);
        }
        assert_eq!(describe("arn:aws:sns:us-east-1:1:t"), "arn:aws:sns:us-east-1:1:t");
    }

    #[test]
    fn lines() {
        let c = json!({ "id": "a", "events": ["s3:ObjectCreated:*"], "prefix": "img/", "suffix": ".png", "destination": "arn:roll:streams:s:t" });
        assert_eq!(notify_line(&c, &json!({ "arn:roll:streams:s:t": { "pending": 2 } })), "a: s3:ObjectCreated:* img/*.png → stream:s/t (2 pending)");
        let r = json!({ "id": "r", "prefix": "", "destination": "dst", "delete_markers": true, "existing_objects": false, "enabled": false, "pending": 0, "tags": {} });
        assert_eq!(replicate_line(&r), "r: * → dst (delete markers) [disabled]");
    }
}
