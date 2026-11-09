//! `roll tasks …`: messages, queues, schedules and workflows.

use crate::{Api, s};
use anyhow::{Context, bail};
use clap::Subcommand;
use reqwest::Method;
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum Cmd {
    /// Publish a message to a URL, topic or pull queue.
    Publish {
        /// Destination URL (omit with --topic, or --queue for a pull queue).
        url: Option<String>,
        #[arg(long)]
        topic: Option<String>,
        #[arg(long)]
        queue: Option<String>,
        /// Body; `@file` reads a file, `-` reads stdin.
        #[arg(long, short)]
        body: Option<String>,
        #[arg(long)]
        content_type: Option<String>,
        #[arg(long, short = 'X')]
        method: Option<String>,
        /// `Name: value`, repeatable.
        #[arg(long = "header", short = 'H')]
        headers: Vec<String>,
        /// e.g. 30s, 10m, 2h.
        #[arg(long)]
        delay: Option<String>,
        /// Unix seconds or RFC 3339.
        #[arg(long)]
        not_before: Option<String>,
        #[arg(long)]
        retries: Option<u32>,
        #[arg(long)]
        timeout: Option<String>,
        #[arg(long)]
        callback: Option<String>,
        #[arg(long)]
        failure_callback: Option<String>,
        #[arg(long)]
        dedup_id: Option<String>,
        /// Deduplicate on the body and destination.
        #[arg(long)]
        content_dedup: bool,
        /// FIFO ordering group within the queue.
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        label: Option<String>,
    },
    /// List messages.
    #[command(alias = "ls")]
    Messages {
        /// pending, delivered, failed, canceled.
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        queue: Option<String>,
        #[arg(long)]
        topic: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        schedule: Option<String>,
        /// Include internal messages (schedule ticks, workflow calls).
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show a message with its body and delivery attempts.
    Message { id: String },
    /// Cancel messages by id.
    Cancel {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Deliver a failed or canceled message again.
    Retry { id: String },
    /// Dead-letter queue: list, retry or delete failed messages.
    Dlq {
        #[command(subcommand)]
        cmd: Option<Dlq>,
    },
    /// Queues: parallelism, rate limits, retries, pause.
    Queues {
        #[command(subcommand)]
        cmd: Option<Queues>,
    },
    /// Topics: fan out one publish to many endpoints.
    Topics {
        #[command(subcommand)]
        cmd: Option<Topics>,
    },
    /// Cron schedules.
    Schedules {
        #[command(subcommand)]
        cmd: Option<Schedules>,
    },
    /// Send an event (starts matching functions, resumes waiting runs).
    Send {
        name: String,
        /// JSON data.
        data: Option<String>,
        /// Deduplication id.
        #[arg(long)]
        id: Option<String>,
    },
    /// Recent events.
    Events {
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Sync an app: roll reads its functions from the URL.
    Sync { url: String },
    /// Apps and their functions.
    Apps {
        #[command(subcommand)]
        cmd: Option<Apps>,
    },
    /// Functions: list, invoke, pause.
    Functions {
        #[command(subcommand)]
        cmd: Option<Functions>,
    },
    /// Function runs.
    Runs {
        #[command(subcommand)]
        cmd: Option<Runs>,
    },
    /// Show signing keys, or rotate them.
    Keys {
        #[arg(long)]
        rotate: bool,
    },
    /// API keys.
    ApiKeys {
        #[command(subcommand)]
        cmd: Option<ApiKeys>,
    },
    /// Tenants hosted by a log store cell (with the node's admin token).
    Tenants {
        #[command(subcommand)]
        cmd: Option<Tenants>,
    },
    /// Usage and AWS cost estimate.
    Usage {
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// Workers and queue depths.
    Stats,
}

#[derive(Subcommand)]
pub enum Dlq {
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        queue: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Retry by id, by queue, or everything.
    Retry {
        ids: Vec<String>,
        #[arg(long)]
        queue: Option<String>,
        #[arg(long)]
        all: bool,
    },
    Delete { id: String },
}

#[derive(Subcommand)]
pub enum Queues {
    #[command(alias = "ls")]
    List,
    Show { name: String },
    /// Create or update a queue.
    Set {
        name: String,
        /// Deliveries in flight at once; 0 = unlimited.
        #[arg(long)]
        parallelism: Option<u32>,
        /// e.g. 100/s, 1000/m; `none` removes it.
        #[arg(long)]
        rate: Option<String>,
        #[arg(long)]
        retries: Option<u32>,
        #[arg(long)]
        timeout: Option<String>,
        /// Partitions: more spreads delivery across workers.
        #[arg(long)]
        partitions: Option<u32>,
    },
    Pause { name: String },
    Resume { name: String },
    Delete {
        name: String,
        /// Delete even with messages waiting (they're canceled).
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum Topics {
    #[command(alias = "ls")]
    List,
    /// Create a topic, or replace its endpoints.
    Set {
        name: String,
        #[arg(required = true)]
        urls: Vec<String>,
    },
    Add {
        name: String,
        #[arg(required = true)]
        urls: Vec<String>,
    },
    Remove {
        name: String,
        #[arg(required = true)]
        urls: Vec<String>,
    },
    Delete { name: String },
}

#[derive(Subcommand)]
pub enum Schedules {
    #[command(alias = "ls")]
    List,
    Show { id: String },
    /// Create a schedule that publishes to a URL, sends an event or runs a function.
    Create {
        /// Cron (5 or 6 fields), `@hourly`, `@every 5m` …
        cron: String,
        #[arg(long, group = "target")]
        url: Option<String>,
        #[arg(long, group = "target")]
        topic: Option<String>,
        #[arg(long, group = "target")]
        event: Option<String>,
        #[arg(long, group = "target")]
        function: Option<String>,
        /// Message body, or event data (JSON).
        #[arg(long, short)]
        body: Option<String>,
        /// IANA time zone, e.g. Europe/Budapest.
        #[arg(long)]
        tz: Option<String>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        label: Option<String>,
    },
    Pause { id: String },
    Resume { id: String },
    /// Fire now (doesn't change the schedule).
    Trigger { id: String },
    Delete { id: String },
}

#[derive(Subcommand)]
pub enum Apps {
    #[command(alias = "ls")]
    List,
    Delete { id: String },
}

#[derive(Subcommand)]
pub enum Functions {
    #[command(alias = "ls")]
    List,
    Show { id: String },
    /// Start a run with this data (JSON).
    Invoke {
        id: String,
        data: Option<String>,
        /// Wait for the result, e.g. 30s.
        #[arg(long)]
        wait: Option<String>,
    },
    Pause { id: String },
    Resume { id: String },
    /// Cancel every unfinished run.
    Cancel { id: String },
}

#[derive(Subcommand)]
pub enum Runs {
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        function: Option<String>,
        /// running, waiting, completed, failed, canceled.
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// A run with its steps.
    Show { id: String },
    Cancel { id: String },
    Rerun { id: String },
    /// Wait for a run to finish.
    Wait {
        id: String,
        #[arg(long, default_value = "60s")]
        timeout: String,
    },
}

#[derive(Subcommand)]
pub enum ApiKeys {
    #[command(alias = "ls")]
    List,
    Create { name: String },
    Delete { id: String },
}

#[derive(Subcommand)]
pub enum Tenants {
    #[command(alias = "ls")]
    List,
    /// Create a tenant; prints its API path and first API key.
    Create { id: String },
    /// Delete a tenant with all of its data.
    Delete { id: String },
    /// Mint another API key for a tenant.
    Key {
        id: String,
        #[arg(long, default_value = "cli")]
        name: String,
    },
}

fn enc(v: &str) -> String {
    let mut out = String::new();
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn query(pairs: &[(&str, Option<String>)]) -> String {
    let q: Vec<String> = pairs.iter().filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={}", enc(v)))).collect();
    if q.is_empty() { String::new() } else { format!("?{}", q.join("&")) }
}

/// `@file`, `-` (stdin) or the literal text.
fn read_body(b: &str) -> anyhow::Result<String> {
    Ok(if b == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else if let Some(path) = b.strip_prefix('@') {
        std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?
    } else {
        b.to_string()
    })
}

/// JSON if it parses, else a string.
fn json_or_text(v: &str) -> Value {
    serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_string()))
}

fn time(ms: &Value) -> String {
    let Some(ms) = ms.as_i64() else { return "-".into() };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let d = (ms - now) / 1000;
    let (n, unit) = match d.abs() {
        0..60 => (d.abs(), "s"),
        60..3600 => (d.abs() / 60, "m"),
        3600..86400 => (d.abs() / 3600, "h"),
        _ => (d.abs() / 86400, "d"),
    };
    if d >= 0 { format!("in {n}{unit}") } else { format!("{n}{unit} ago") }
}

fn destination(m: &Value) -> String {
    let t = &m["target"];
    for k in ["url", "queue", "function_id", "schedule_id"] {
        if let Some(v) = t[k].as_str() {
            return v.to_string();
        }
    }
    m["queue"].as_str().map(|q| format!("queue {q}")).unwrap_or_else(|| s(&t["kind"]))
}

fn print_messages(v: &Value) {
    for m in v["items"].as_array().into_iter().flatten() {
        println!(
            "{}  {:<9} {:>2}/{:<3} {:<10} {}{}",
            s(&m["id"]),
            s(&m["state"]),
            s(&m["attempts"]),
            s(&m["retries"]),
            // Pending: when it's due; finished: when it finished.
            time(if matches!(m["state"].as_str(), Some("pending" | "active")) { &m["due_at"] } else { &m["updated_at"] }),
            destination(m),
            m["label"].as_str().map(|l| format!("  [{l}]")).unwrap_or_default(),
        );
    }
    if let Some(c) = v["cursor"].as_str() {
        println!("… more (cursor {c})");
    }
}

pub async fn run(api: &Api, cmd: Cmd, raw: bool) -> anyhow::Result<()> {
    let out = match cmd {
        Cmd::Publish {
            url,
            topic,
            queue,
            body,
            content_type,
            method,
            headers,
            delay,
            not_before,
            retries,
            timeout,
            callback,
            failure_callback,
            dedup_id,
            content_dedup,
            group,
            label,
        } => {
            if url.is_none() && topic.is_none() && queue.is_none() {
                bail!("give a URL, --topic or --queue");
            }
            let mut h = serde_json::Map::new();
            for line in headers {
                let (k, v) = line.split_once(':').with_context(|| format!("header {line:?} isn't `Name: value`"))?;
                h.insert(k.trim().to_string(), json!(v.trim()));
            }
            let body = body.map(|b| read_body(&b)).transpose()?;
            let req = json!({
                "url": url, "topic": topic, "queue": queue, "body": body, "content_type": content_type,
                "method": method, "headers": h, "delay": delay,
                "not_before": not_before.map(|t| t.parse::<i64>().map(|s| json!(s * 1000)).unwrap_or(json!(t))),
                "retries": retries, "timeout": timeout, "callback": callback, "failure_callback": failure_callback,
                "dedup_id": dedup_id, "content_dedup": content_dedup, "group": group, "label": label,
            });
            let v = api.post("/publish", req).await?;
            if !raw {
                for m in v["messages"].as_array().into_iter().flatten() {
                    println!("{}  {}", s(&m["message_id"]), m["url"].as_str().or(m["queue"].as_str()).unwrap_or(""));
                }
                return Ok(());
            }
            v
        }
        Cmd::Messages { state, queue, topic, label, schedule, all, limit } => {
            let q = query(&[
                ("state", state),
                ("queue", queue),
                ("topic", topic),
                ("label", label),
                ("schedule_id", schedule),
                ("all", all.then(|| "true".into())),
                ("limit", Some(limit.to_string())),
            ]);
            let v = api.get(&format!("/messages{q}")).await?;
            if !raw {
                print_messages(&v);
                return Ok(());
            }
            v
        }
        Cmd::Message { id } => {
            let m = api.get(&format!("/messages/{id}")).await?;
            if !raw {
                println!("{}  {}  → {}", s(&m["id"]), s(&m["state"]), destination(&m));
                println!("  attempts {}/{}  due {}  created {}", s(&m["attempts"]), s(&m["retries"]), time(&m["due_at"]), time(&m["created_at"]));
                if let Some(r) = m["reason"].as_str() {
                    println!("  reason   {r}");
                }
                for a in m["attempts_log"].as_array().into_iter().flatten() {
                    println!(
                        "  #{} {}  {}  {} ms{}",
                        s(&a["n"]),
                        time(&a["at"]),
                        a["status"].as_i64().map(|c| c.to_string()).or(a["error"].as_str().map(String::from)).unwrap_or_default(),
                        s(&a["duration_ms"]),
                        a["next_at"].as_i64().map(|_| format!("  retry {}", time(&a["next_at"]))).unwrap_or_default(),
                    );
                }
                if let Some(b) = m["body"].as_str() {
                    println!("\n{b}");
                }
                return Ok(());
            }
            m
        }
        Cmd::Cancel { ids } => {
            let v = api.post("/messages/cancel", json!({ "ids": ids })).await?;
            println!("canceled {}", s(&v["canceled"]));
            return Ok(());
        }
        Cmd::Retry { id } => api.post(&format!("/messages/{id}/retry"), json!({})).await?,
        Cmd::Dlq { cmd } => match cmd.unwrap_or(Dlq::List { queue: None, limit: 50 }) {
            Dlq::List { queue, limit } => {
                let v = api.get(&format!("/dlq{}", query(&[("queue", queue), ("limit", Some(limit.to_string()))]))).await?;
                if !raw {
                    for m in v["items"].as_array().into_iter().flatten() {
                        println!("{}  {:>2} tries  {:<10} {}  {}", s(&m["id"]), s(&m["attempts"]), time(&m["updated_at"]), destination(m), s(&m["reason"]));
                    }
                    return Ok(());
                }
                v
            }
            Dlq::Retry { ids, queue, all } => {
                let v = api.post("/dlq/retry", json!({ "ids": ids, "queue": queue, "all": all })).await?;
                println!("retried {}", s(&v["retried"]));
                return Ok(());
            }
            Dlq::Delete { id } => {
                api.call(Method::DELETE, &format!("/dlq/{id}"), None).await?;
                println!("deleted {id}");
                return Ok(());
            }
        },
        Cmd::Queues { cmd } => match cmd.unwrap_or(Queues::List) {
            Queues::List => {
                let v = api.get("/queues").await?;
                if !raw {
                    for q in v.as_array().into_iter().flatten() {
                        let st = &q["stats"];
                        println!(
                            "{:<24} {:<7} ready {:<6} scheduled {:<6} active {:<5} parallelism {}  rate {}",
                            s(&q["name"]),
                            if q["paused"] == true { "paused" } else { "" },
                            s(&st["ready"]),
                            s(&st["scheduled"]),
                            s(&st["active"]),
                            q["parallelism"].as_u64().filter(|p| *p > 0).map(|p| p.to_string()).unwrap_or("∞".into()),
                            q["rate"].as_object().map(|r| format!("{}/{}ms", s(&r["limit"]), s(&r["period_ms"]))).unwrap_or("-".into()),
                        );
                    }
                    return Ok(());
                }
                v
            }
            Queues::Show { name } => api.get(&format!("/queues/{}", enc(&name))).await?,
            Queues::Set { name, parallelism, rate, retries, timeout, partitions } => {
                let mut b = serde_json::Map::new();
                if let Some(p) = parallelism {
                    b.insert("parallelism".into(), json!(p));
                }
                if let Some(r) = rate {
                    b.insert("rate".into(), if r == "none" { Value::Null } else { json!(r) });
                }
                if let Some(r) = retries {
                    b.insert("retries".into(), json!(r));
                }
                if let Some(t) = timeout {
                    b.insert("timeout".into(), json!(t));
                }
                if let Some(p) = partitions {
                    b.insert("partitions".into(), json!(p));
                }
                api.call(Method::PUT, &format!("/queues/{}", enc(&name)), Some(Value::Object(b))).await?
            }
            Queues::Pause { name } => api.post(&format!("/queues/{}/pause", enc(&name)), json!({})).await?,
            Queues::Resume { name } => api.post(&format!("/queues/{}/resume", enc(&name)), json!({})).await?,
            Queues::Delete { name, force } => {
                api.call(Method::DELETE, &format!("/queues/{}{}", enc(&name), if force { "?force=true" } else { "" }), None).await?;
                println!("deleted queue {name}");
                return Ok(());
            }
        },
        Cmd::Topics { cmd } => match cmd.unwrap_or(Topics::List) {
            Topics::List => {
                let v = api.get("/topics").await?;
                if !raw {
                    for t in v.as_array().into_iter().flatten() {
                        println!("{}", s(&t["name"]));
                        for e in t["endpoints"].as_array().into_iter().flatten() {
                            println!("  {}", s(&e["url"]));
                        }
                    }
                    return Ok(());
                }
                v
            }
            Topics::Set { name, urls } => api.call(Method::PUT, &format!("/topics/{}", enc(&name)), Some(json!({ "endpoints": urls }))).await?,
            Topics::Add { name, urls } => api.post(&format!("/topics/{}/endpoints", enc(&name)), json!({ "endpoints": urls })).await?,
            Topics::Remove { name, urls } => api.call(Method::DELETE, &format!("/topics/{}/endpoints", enc(&name)), Some(json!({ "urls": urls }))).await?,
            Topics::Delete { name } => {
                api.call(Method::DELETE, &format!("/topics/{}", enc(&name)), None).await?;
                println!("deleted topic {name}");
                return Ok(());
            }
        },
        Cmd::Schedules { cmd } => match cmd.unwrap_or(Schedules::List) {
            Schedules::List => {
                let v = api.get("/schedules").await?;
                if !raw {
                    for sc in v.as_array().into_iter().flatten() {
                        let next = sc["upcoming"].get(0).map(time).unwrap_or_else(|| "paused".into());
                        println!("{:<28} {:<18} {:<10} {}", s(&sc["id"]), s(&sc["cron"]), next, s(&sc["label"]));
                    }
                    return Ok(());
                }
                v
            }
            Schedules::Show { id } => api.get(&format!("/schedules/{}", enc(&id))).await?,
            Schedules::Create { cron, url, topic, event, function, body, tz, id, label } => {
                let body = body.map(|b| read_body(&b)).transpose()?;
                let mut req = json!({ "cron": cron, "tz": tz, "id": id, "label": label });
                if let Some(name) = event {
                    req["event"] = json!({ "name": name, "data": body.as_deref().map(json_or_text).unwrap_or(Value::Null) });
                } else if let Some(f) = function {
                    req["function"] = json!(f);
                } else if url.is_some() || topic.is_some() {
                    req["publish"] = json!({ "url": url, "topic": topic, "body": body });
                } else {
                    bail!("give --url, --topic, --event or --function");
                }
                let v = api.post("/schedules", req).await?;
                if !raw {
                    println!("schedule {}  next {}", s(&v["id"]), v["upcoming"].get(0).map(time).unwrap_or("-".into()));
                    return Ok(());
                }
                v
            }
            Schedules::Pause { id } => api.post(&format!("/schedules/{}/pause", enc(&id)), json!({})).await?,
            Schedules::Resume { id } => api.post(&format!("/schedules/{}/resume", enc(&id)), json!({})).await?,
            Schedules::Trigger { id } => api.post(&format!("/schedules/{}/trigger", enc(&id)), json!({})).await?,
            Schedules::Delete { id } => {
                api.call(Method::DELETE, &format!("/schedules/{}", enc(&id)), None).await?;
                println!("deleted schedule {id}");
                return Ok(());
            }
        },
        Cmd::Send { name, data, id } => {
            let v = api.post("/events", json!({ "name": name, "data": data.as_deref().map(json_or_text).unwrap_or(Value::Null), "id": id })).await?;
            if !raw {
                println!("{}", v["ids"].as_array().into_iter().flatten().map(s).collect::<Vec<_>>().join("\n"));
                return Ok(());
            }
            v
        }
        Cmd::Events { name, limit } => {
            let v = api.get(&format!("/events{}", query(&[("name", name), ("limit", Some(limit.to_string()))]))).await?;
            if !raw {
                for e in v["items"].as_array().into_iter().flatten() {
                    println!("{}  {:<10} {:<32} {}", s(&e["id"]), time(&e["ts"]), s(&e["name"]), e["data"]);
                }
                return Ok(());
            }
            v
        }
        Cmd::Sync { url } => {
            let v = api.post("/apps", json!({ "url": url })).await?;
            if !raw {
                println!("synced app {} ({} functions)", s(&v["id"]), v["functions"].as_array().map(Vec::len).unwrap_or(0));
                return Ok(());
            }
            v
        }
        Cmd::Apps { cmd } => match cmd.unwrap_or(Apps::List) {
            Apps::List => {
                let v = api.get("/apps").await?;
                if !raw {
                    for a in v.as_array().into_iter().flatten() {
                        println!("{:<24} {}  {} functions", s(&a["id"]), s(&a["url"]), a["functions"].as_array().map(Vec::len).unwrap_or(0));
                    }
                    return Ok(());
                }
                v
            }
            Apps::Delete { id } => {
                api.call(Method::DELETE, &format!("/apps/{}", enc(&id)), None).await?;
                println!("deleted app {id}");
                return Ok(());
            }
        },
        Cmd::Functions { cmd } => match cmd.unwrap_or(Functions::List) {
            Functions::List => {
                let v = api.get("/functions").await?;
                if !raw {
                    for f in v.as_array().into_iter().flatten() {
                        let triggers: Vec<String> = f["triggers"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|t| t["event"].as_str().or(t["cron"].as_str()).unwrap_or("").to_string())
                            .collect();
                        println!(
                            "{:<28} {:<16} {:<7} {}",
                            s(&f["id"]),
                            s(&f["app_id"]),
                            if f["paused"] == true { "paused" } else { "" },
                            triggers.join(", ")
                        );
                    }
                    return Ok(());
                }
                v
            }
            Functions::Show { id } => api.get(&format!("/functions/{}", enc(&id))).await?,
            Functions::Invoke { id, data, wait } => {
                let body = json!({ "data": data.as_deref().map(json_or_text).unwrap_or(Value::Null), "wait": wait });
                api.post(&format!("/functions/{}/invoke", enc(&id)), body).await?
            }
            Functions::Pause { id } => api.post(&format!("/functions/{}/pause", enc(&id)), json!({})).await?,
            Functions::Resume { id } => api.post(&format!("/functions/{}/resume", enc(&id)), json!({})).await?,
            Functions::Cancel { id } => api.post(&format!("/functions/{}/cancel", enc(&id)), json!({})).await?,
        },
        Cmd::Runs { cmd } => match cmd.unwrap_or(Runs::List { function: None, status: None, limit: 50 }) {
            Runs::List { function, status, limit } => {
                let v = api.get(&format!("/runs{}", query(&[("function", function), ("status", status), ("limit", Some(limit.to_string()))]))).await?;
                if !raw {
                    for r in v["items"].as_array().into_iter().flatten() {
                        println!("{}  {:<10} {:<10} {}", s(&r["id"]), s(&r["status"]), time(&r["created_at"]), s(&r["function_id"]));
                    }
                    return Ok(());
                }
                v
            }
            Runs::Show { id } => {
                let r = api.get(&format!("/runs/{id}")).await?;
                if !raw {
                    println!("{}  {}  {}  started {}", s(&r["id"]), s(&r["function_id"]), s(&r["status"]), time(&r["created_at"]));
                    for st in r["steps"].as_array().into_iter().flatten() {
                        println!("  {:<32} {:<8} {}", st["name"].as_str().or(st["id"].as_str()).unwrap_or(""), s(&st["op"]), time(&st["at"]));
                    }
                    if !r["output"].is_null() {
                        println!("output {}", r["output"]);
                    }
                    if !r["error"].is_null() {
                        println!("error  {}", r["error"]);
                    }
                    return Ok(());
                }
                r
            }
            Runs::Cancel { id } => api.post(&format!("/runs/{id}/cancel"), json!({})).await?,
            Runs::Rerun { id } => api.post(&format!("/runs/{id}/rerun"), json!({})).await?,
            Runs::Wait { id, timeout } => api.get(&format!("/runs/{id}/wait?timeout={}", enc(&timeout))).await?,
        },
        Cmd::Keys { rotate } => {
            let v = if rotate { api.post("/keys/rotate", json!({})).await? } else { api.get("/keys").await? };
            if !raw {
                println!("current  {}\nnext     {}", s(&v["current"]), s(&v["next"]));
                return Ok(());
            }
            v
        }
        Cmd::ApiKeys { cmd } => match cmd.unwrap_or(ApiKeys::List) {
            ApiKeys::List => {
                let v = api.get("/api-keys").await?;
                if !raw {
                    for k in v.as_array().into_iter().flatten() {
                        println!("{}  {:<24} {}…  created {}", s(&k["id"]), s(&k["name"]), s(&k["prefix"]), time(&k["created_at"]));
                    }
                    return Ok(());
                }
                v
            }
            ApiKeys::Create { name } => {
                let v = api.post("/api-keys", json!({ "name": name })).await?;
                if !raw {
                    println!("{}  {}\n\n{}\n\nstore it now: it isn't shown again", s(&v["id"]), s(&v["name"]), s(&v["token"]));
                    return Ok(());
                }
                v
            }
            ApiKeys::Delete { id } => {
                api.call(Method::DELETE, &format!("/api-keys/{id}"), None).await?;
                println!("deleted key {id}");
                return Ok(());
            }
        },
        Cmd::Tenants { cmd } => {
            let admin = api.cell();
            match cmd.unwrap_or(Tenants::List) {
                Tenants::List => {
                    let v = admin.get("/_roll/tenants").await?;
                    if !raw {
                        for t in v["tenants"].as_array().into_iter().flatten() {
                            let id = s(t);
                            let path = if id == "default" { "/v1/tasks".to_string() } else { format!("/t/{id}/v1/tasks") };
                            println!("{id:<24} {}{path}", admin.base);
                        }
                        return Ok(());
                    }
                    v
                }
                Tenants::Create { id } => {
                    let v = admin.post("/_roll/tenants", json!({ "id": id })).await?;
                    if !raw {
                        println!("{}  ROLL_TASKS_URL={}{}

{}

store the key now: it isn't shown again", s(&v["id"]), admin.base, s(&v["path"]), s(&v["token"]));
                        return Ok(());
                    }
                    v
                }
                Tenants::Delete { id } => {
                    admin.call(Method::DELETE, &format!("/_roll/tenants/{}", enc(&id)), None).await?;
                    println!("deleted tenant {id} and all of its data");
                    return Ok(());
                }
                Tenants::Key { id, name } => {
                    let v = admin.post(&format!("/_roll/tenants/{}/keys", enc(&id)), json!({ "name": name })).await?;
                    if !raw {
                        println!("{}  {}

{}

store it now: it isn't shown again", s(&v["id"]), s(&v["name"]), s(&v["token"]));
                        return Ok(());
                    }
                    v
                }
            }
        }
        Cmd::Usage { days } => {
            let u = api.get(&format!("/usage?days={days}")).await?;
            if !raw {
                let t = &u["total"];
                let c = &u["cost"];
                let f = |v: &Value| v.as_f64().unwrap_or(0.0);
                println!("last {days} days");
                println!("  published   {:>12}", s(&t["published"]));
                println!("  deliveries  {:>12}   ({} failed, {} dead)", s(&t["deliveries"]), s(&t["failed_attempts"]), s(&t["dead"]));
                println!("  events      {:>12}   runs {}  steps {}", s(&t["events"]), s(&t["runs"]), s(&t["steps"]));
                println!("  storage     {:>12}   writes", s(&t["storage_writes"]));
                println!("cost ({} layout, us-east-1)", c["layout"].as_str().unwrap_or("?"));
                for item in c["items"].as_array().into_iter().flatten() {
                    println!("  {:<11} ${:>11.4}", item["name"].as_str().unwrap_or("?").to_lowercase(), f(&item["usd"]));
                }
                println!(
                    "  total       ${:>11.4}   projected ${:.2} / month + ${:.2} for {}",
                    f(&c["total_usd"]),
                    f(&u["projected_month_usd"]),
                    f(&c["fixed_usd_per_month"]),
                    c["fixed"].as_str().unwrap_or("compute")
                );
                println!("  per million messages ${:.2}", f(&c["usd_per_million_messages"]));
                return Ok(());
            }
            u
        }
        Cmd::Stats => {
            let v = api.get("/stats").await?;
            if !raw {
                for n in v["nodes"].as_array().into_iter().flatten() {
                    println!("worker {}  {} lanes  {} in flight", s(&n["id"]), s(&n["lanes"]), s(&n["inflight"]));
                }
                for q in v["queues"].as_array().into_iter().flatten() {
                    let st = &q["stats"];
                    println!("queue  {:<24} ready {:<6} scheduled {:<6} active {}", s(&q["name"]), s(&st["ready"]), s(&st["scheduled"]), s(&st["active"]));
                }
                return Ok(());
            }
            v
        }
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
