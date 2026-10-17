//! `roll stream`: Kafka-compatible streams over the HTTP API (no Kafka client needed).

use crate::{Api, bytes, s};
use anyhow::{Context, bail};
use clap::Subcommand;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::BufRead;

#[derive(Subcommand)]
pub enum Stream {
    /// List streams.
    #[command(alias = "ls")]
    List,
    /// Create a stream; prints its bootstrap address and a key.
    Create { name: String },
    /// Show a stream: topics, keys, throughput.
    Show { stream: String },
    /// Delete a stream with all its topics and data.
    Delete { stream: String },
    /// List topics.
    Topics { stream: String },
    /// Manage a topic.
    #[command(subcommand)]
    Topic(Topic),
    /// Produce records: VALUE, or one record per stdin line.
    Produce {
        stream: String,
        topic: String,
        value: Option<String>,
        #[arg(long, short)]
        key: Option<String>,
        #[arg(long, short)]
        partition: Option<i32>,
        /// Header as name=value (repeatable).
        #[arg(long = "header", short = 'H')]
        headers: Vec<String>,
    },
    /// Read records. Without --group the read is stateless; with it, partitions are shared
    /// between everyone reading in the group and progress is committed.
    Consume {
        stream: String,
        topic: String,
        /// earliest or latest.
        #[arg(long, default_value = "earliest")]
        from: String,
        #[arg(long, short)]
        group: Option<String>,
        /// Keep waiting for new records.
        #[arg(long, short)]
        follow: bool,
        /// Stop after this many records.
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// read_committed: hide aborted and open transactions.
        #[arg(long)]
        committed: bool,
    },
    /// List consumer groups with lag.
    Groups { stream: String },
    /// Show a group's committed offsets and lag.
    Group { stream: String, group: String },
    /// Move a group's offsets (the group must be stopped).
    Reset {
        stream: String,
        group: String,
        #[arg(long)]
        topic: String,
        /// earliest, latest, an offset, or timestamp:<ms>.
        #[arg(long)]
        to: String,
    },
    /// Delete a consumer group.
    GroupDelete { stream: String, group: String },
    /// List API keys.
    Keys { stream: String },
    /// Create an API key; prints client settings.
    KeyCreate {
        stream: String,
        #[arg(long)]
        label: Option<String>,
    },
    /// Revoke an API key.
    KeyDelete { stream: String, username: String },
    /// Usage and the AWS cost estimate.
    Usage {
        stream: String,
        #[arg(long, default_value_t = 168)]
        hours: i64,
    },
}

#[derive(Subcommand)]
pub enum Topic {
    /// Create a topic.
    Create {
        stream: String,
        name: String,
        #[arg(long, short)]
        partitions: Option<i32>,
        /// Config as name=value (repeatable), e.g. -c cleanup.policy=compact.
        #[arg(long = "config", short)]
        configs: Vec<String>,
    },
    /// Show partitions, offsets, storage and configs.
    Show { stream: String, name: String },
    /// Add partitions or change configs online.
    Alter {
        stream: String,
        name: String,
        #[arg(long, short)]
        partitions: Option<i32>,
        #[arg(long = "config", short)]
        configs: Vec<String>,
        /// Remove an override (repeatable).
        #[arg(long)]
        unset: Vec<String>,
    },
    /// Delete a topic and its data.
    Delete { stream: String, name: String },
}

fn pairs(items: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    items
        .iter()
        .map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).with_context(|| format!("expected name=value, got {kv:?}")))
        .collect()
}

fn enc(part: &str) -> String {
    let mut out = String::new();
    for b in part.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn print_stream(v: &Value) {
    println!("{}  {}  {} topics, {} partitions", s(&v["id"]), s(&v["name"]), s(&v["topics"]), s(&v["partitions"]));
    println!("  kafka  {}", s(&v["bootstrap"]));
    println!("  http   {}", s(&v["http"]));
}

fn print_connect(c: &Value) {
    println!("\nusername  {}\nsecret    {}  (shown once)\n", s(&c["username"]), s(&c["secret"]));
    print!("{}", s(&c["properties"]));
}

fn print_topic(v: &Value) {
    let f = |v: &Value| v.as_f64().unwrap_or(0.0);
    println!(
        "{}  {} partitions  {} records  local {}  object storage {}",
        s(&v["name"]),
        s(&v["partitions"]),
        s(&v["records"]),
        bytes(f(&v["local_bytes"])),
        bytes(f(&v["remote_bytes"]))
    );
    for p in v["partition_stats"].as_array().into_iter().flatten() {
        println!(
            "  {:>3}  offsets {}..{}  stable {}  {} segments  {} chunks",
            s(&p["partition"]),
            s(&p["log_start_offset"]),
            s(&p["high_watermark"]),
            s(&p["last_stable_offset"]),
            s(&p["segments"]),
            s(&p["chunks"])
        );
    }
    if let Some(c) = v["configs"].as_object().filter(|c| !c.is_empty()) {
        for (k, val) in c {
            println!("  {k}={}", s(val));
        }
    }
}

fn print_record(r: &Value, json: bool) {
    if json {
        println!("{r}");
        return;
    }
    let show = |v: &Value| match v {
        Value::String(t) => t.clone(),
        Value::Null => "∅".into(),
        v => v.to_string(),
    };
    println!("{}:{}  {}  {}", s(&r["partition"]), s(&r["offset"]), show(&r["key"]), show(&r["value"]));
}

pub async fn run(api: &Api, json: bool, cmd: Stream) -> anyhow::Result<Option<Value>> {
    let st = |name: &str| format!("/v1/streams/{}", enc(name));
    Ok(Some(match cmd {
        Stream::List => {
            let v = api.get("/v1/streams").await?;
            if json {
                return Ok(Some(v));
            }
            for x in v.as_array().into_iter().flatten() {
                print_stream(x);
            }
            return Ok(None);
        }
        Stream::Create { name } => {
            let v = api.post("/v1/streams", json!({ "name": name })).await?;
            if json {
                return Ok(Some(v));
            }
            print_stream(&v);
            print_connect(&v["connect"]);
            return Ok(None);
        }
        Stream::Show { stream } => {
            let v = api.get(&st(&stream)).await?;
            if json {
                return Ok(Some(v));
            }
            print_stream(&v);
            for t in v["topic_list"].as_array().into_iter().flatten() {
                println!("  topic {:<30} {} partitions", s(&t["name"]), s(&t["partitions"]));
            }
            for k in v["keys"].as_array().into_iter().flatten() {
                println!("  key   {:<30} {}", s(&k["username"]), s(&k["label"]));
            }
            return Ok(None);
        }
        Stream::Delete { stream } => {
            api.call(reqwest::Method::DELETE, &st(&stream), None).await?;
            println!("deleted {stream}");
            return Ok(None);
        }
        Stream::Topics { stream } => {
            let v = api.get(&format!("{}/topics", st(&stream))).await?;
            if json {
                return Ok(Some(v));
            }
            for t in v.as_array().into_iter().flatten() {
                println!("{:<30} {} partitions", s(&t["name"]), s(&t["partitions"]));
            }
            return Ok(None);
        }
        Stream::Topic(Topic::Create { stream, name, partitions, configs }) => {
            let v = api.post(&format!("{}/topics", st(&stream)), json!({ "name": name, "partitions": partitions, "configs": pairs(&configs)? })).await?;
            if json {
                return Ok(Some(v));
            }
            print_topic(&v);
            return Ok(None);
        }
        Stream::Topic(Topic::Show { stream, name }) => {
            let v = api.get(&format!("{}/topics/{}", st(&stream), enc(&name))).await?;
            if json {
                return Ok(Some(v));
            }
            print_topic(&v);
            return Ok(None);
        }
        Stream::Topic(Topic::Alter { stream, name, partitions, configs, unset }) => {
            let mut changes: BTreeMap<String, Value> = pairs(&configs)?.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
            for k in unset {
                changes.insert(k, Value::Null);
            }
            let body = json!({ "partitions": partitions, "configs": if changes.is_empty() { Value::Null } else { json!(changes) } });
            let v = api.call(reqwest::Method::PATCH, &format!("{}/topics/{}", st(&stream), enc(&name)), Some(body)).await?;
            if json {
                return Ok(Some(v));
            }
            print_topic(&v);
            return Ok(None);
        }
        Stream::Topic(Topic::Delete { stream, name }) => {
            api.call(reqwest::Method::DELETE, &format!("{}/topics/{}", st(&stream), enc(&name)), None).await?;
            println!("deleted topic {name}");
            return Ok(None);
        }
        Stream::Produce { stream, topic, value, key, partition, headers } => {
            let headers = pairs(&headers)?;
            let record = |v: String| {
                let mut r = json!({ "value": v, "key": key, "partition": partition });
                if !headers.is_empty() {
                    r["headers"] = json!(headers);
                }
                r
            };
            let records: Vec<Value> = match value {
                Some(v) => vec![record(v)],
                None => std::io::stdin().lock().lines().map(|l| l.map(record)).collect::<Result<_, _>>()?,
            };
            if records.is_empty() {
                bail!("nothing to produce (pass a value or pipe lines on stdin)");
            }
            let mut total = 0;
            for batch in records.chunks(5000) {
                let v = api.post(&format!("{}/topics/{}/records", st(&stream), enc(&topic)), Value::Array(batch.to_vec())).await?;
                if json {
                    println!("{v}");
                }
                total += batch.len();
                if batch.len() == 1 && !json {
                    let o = &v["offsets"][0];
                    println!("{}:{}", s(&o["partition"]), s(&o["offset"]));
                }
            }
            if total > 1 && !json {
                println!("produced {total} records");
            }
            return Ok(None);
        }
        Stream::Consume { stream, topic, from, group, follow, limit, committed } => {
            let url = format!("{}/topics/{}/records", st(&stream), enc(&topic));
            let mut cursor: Option<String> = None;
            let mut consumer: Option<String> = None;
            let mut seen = 0usize;
            loop {
                let mut q = vec![("limit".to_string(), limit.map(|l| (l - seen).min(1000)).unwrap_or(1000).to_string())];
                if committed {
                    q.push(("isolation".into(), "read_committed".into()));
                }
                if follow {
                    q.push(("wait_ms".into(), "10000".into()));
                }
                match &group {
                    Some(g) => {
                        q.push(("group".into(), g.clone()));
                        q.push(("reset".into(), from.clone()));
                        if let Some(c) = &consumer {
                            q.push(("consumer".into(), c.clone()));
                        }
                    }
                    None => match &cursor {
                        Some(c) => q.push(("cursor".into(), c.clone())),
                        None => q.push(("from".into(), from.clone())),
                    },
                }
                let qs: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect();
                let v = api.get(&format!("{url}?{}", qs.join("&"))).await?;
                let recs = v["records"].as_array().cloned().unwrap_or_default();
                for r in &recs {
                    print_record(r, json);
                }
                seen += recs.len();
                cursor = v["cursor"].as_str().map(str::to_string).or(cursor);
                consumer = v["consumer"].as_str().map(str::to_string).or(consumer);
                if limit.is_some_and(|l| seen >= l) || (!follow && recs.is_empty()) {
                    break;
                }
            }
            if let (Some(g), Some(c)) = (&group, &consumer) {
                // Leave the group so the partitions (and the last delivery's offsets) settle now.
                let _ = api.call(reqwest::Method::DELETE, &format!("{}/groups/{}/consumers/{}", st(&stream), enc(g), enc(c)), None).await;
            }
            return Ok(None);
        }
        Stream::Groups { stream } => {
            let v = api.get(&format!("{}/groups", st(&stream))).await?;
            if json {
                return Ok(Some(v));
            }
            for g in v.as_array().into_iter().flatten() {
                println!("{:<30} {:<10} {:<12} {} members  lag {}", s(&g["group_id"]), s(&g["protocol_type"]), s(&g["state"]), s(&g["members"]), s(&g["lag"]));
            }
            return Ok(None);
        }
        Stream::Group { stream, group } => api.get(&format!("{}/groups/{}", st(&stream), enc(&group))).await?,
        Stream::Reset { stream, group, topic, to } => {
            api.post(&format!("{}/groups/{}/reset", st(&stream), enc(&group)), json!({ "topic": topic, "to": to })).await?
        }
        Stream::GroupDelete { stream, group } => {
            api.call(reqwest::Method::DELETE, &format!("{}/groups/{}", st(&stream), enc(&group)), None).await?;
            println!("deleted group {group}");
            return Ok(None);
        }
        Stream::Keys { stream } => {
            let v = api.get(&format!("{}/keys", st(&stream))).await?;
            if json {
                return Ok(Some(v));
            }
            for k in v.as_array().into_iter().flatten() {
                println!("{:<30} {}", s(&k["username"]), s(&k["label"]));
            }
            return Ok(None);
        }
        Stream::KeyCreate { stream, label } => {
            let v = api.post(&format!("{}/keys", st(&stream)), json!({ "label": label })).await?;
            if json {
                return Ok(Some(v));
            }
            print_connect(&v["connect"]);
            return Ok(None);
        }
        Stream::KeyDelete { stream, username } => {
            api.call(reqwest::Method::DELETE, &format!("{}/keys/{}", st(&stream), enc(&username)), None).await?;
            println!("revoked {username}");
            return Ok(None);
        }
        Stream::Usage { stream, hours } => {
            let u = api.get(&format!("{}/usage?hours={hours}", st(&stream))).await?;
            if json {
                return Ok(Some(u));
            }
            let f = |v: &Value| v.as_f64().unwrap_or(0.0);
            let e = &u["estimate"];
            let m = &e["monthly_usd"];
            println!("{stream} — projected from the last {hours}h");
            println!("  in {}  out {}  per month", bytes(f(&e["projected"]["bytes_in"])), bytes(f(&e["projected"]["bytes_out"])));
            println!("  s3 storage     ${:.2}", f(&m["s3_storage"]));
            println!("  local storage  ${:.2}", f(&m["local_storage"]));
            println!("  requests       ${:.2}", f(&m["requests"]));
            println!("  transfer       ${:.2}", f(&m["transfer"]));
            println!("  total          ${:.2} / month", f(&m["total"]));
            println!("  same on MSK    ${:.2} / month", f(&e["msk_monthly_usd"]["total"]));
            return Ok(None);
        }
    }))
}
