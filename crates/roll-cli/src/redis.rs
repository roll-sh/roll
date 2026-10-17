//! `roll redis …`: serverless Redis.

use crate::{Api, ago, bytes, parse_window, s};
use anyhow::{Context, bail};
use clap::Subcommand;
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum Redis {
    /// List Redis databases.
    #[command(alias = "ls")]
    List,
    /// Create a database.
    Create {
        name: String,
        /// Memory limit in MiB (maxmemory).
        #[arg(long)]
        memory: Option<i64>,
        /// maxmemory-policy, e.g. allkeys-lru.
        #[arg(long)]
        eviction: Option<String>,
        /// `async` (default; durable within ~1s) or `sync` (replies wait for object storage).
        #[arg(long)]
        durability: Option<String>,
        /// Seconds without clients before scaling to zero; 0 = always on.
        #[arg(long)]
        idle: Option<i64>,
    },
    /// Show a database: state, connection details, replicas.
    Show { db: String },
    /// Change settings (applied live).
    Set {
        db: String,
        #[arg(long)]
        memory: Option<i64>,
        #[arg(long)]
        eviction: Option<String>,
        #[arg(long)]
        durability: Option<String>,
        #[arg(long)]
        idle: Option<i64>,
        /// Point-in-time restore window, e.g. 24h or 7d.
        #[arg(long)]
        retention: Option<String>,
        #[arg(long)]
        rename: Option<String>,
    },
    /// Delete a database.
    Delete { db: String },
    /// Print the `redis://` URL.
    Url { db: String },
    /// Print the REST URL and token (for @upstash/redis and plain HTTP).
    Rest { db: String },
    /// Branch a database: an instant copy, optionally of a past point in time.
    Branch {
        db: String,
        name: String,
        /// Unix seconds, or how long ago (30s, 15m, 2h, 1d).
        #[arg(long)]
        at: Option<String>,
    },
    /// Restore a database in place to a point in time (clients reconnect to the restored state).
    Restore {
        db: String,
        /// Unix seconds, or how long ago (30s, 15m, 2h, 1d).
        #[arg(long)]
        at: String,
    },
    /// Add a read replica.
    Replica { db: String },
    /// Delete a read replica.
    ReplicaDelete { id: String },
    /// New password (existing connections stay up).
    RotatePassword { db: String },
    /// Scale to zero now.
    Unload { db: String },
    /// Usage and AWS cost estimate (with the ElastiCache Serverless equivalent).
    Usage {
        db: String,
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// Run a command, e.g. `roll redis cli cache SET k v`.
    Cli {
        db: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// `--at`: unix seconds, or a duration ago (`90s`, `15m`, `2h`, `1d`).
fn at(v: &str) -> anyhow::Result<i64> {
    if let Ok(t) = v.parse::<i64>() {
        return Ok(t);
    }
    let (n, unit) = v.split_at(v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len()));
    let n: i64 = n.parse().with_context(|| format!("bad time {v:?}: use unix seconds or e.g. 15m"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => bail!("bad time {v:?}: use unix seconds or e.g. 30s, 15m, 2h, 1d"),
    };
    Ok(now() - secs)
}

fn print_db(d: &Value) {
    let st = &d["status"];
    let state = s(&st["state"]);
    let mem = st["used_memory"].as_f64().map(bytes).unwrap_or_else(|| "-".into());
    let branch = match d["parent_id"].as_str() {
        Some(p) => format!("  branch of {p}"),
        None => String::new(),
    };
    println!(
        "{:<20} {:<18} {:<7} {:>9} / {:<6} {:<12} {:<6} keys {:<6} conns {}{branch}",
        s(&d["name"]),
        s(&d["id"]),
        state,
        mem,
        format!("{}MB", s(&d["max_memory_mb"])),
        s(&d["eviction_policy"]),
        s(&d["durability"]),
        s(&st["keys"]),
        s(&st["connections"]),
    );
    for r in d["replicas"].as_array().into_iter().flatten() {
        println!("  replica {}  {}", s(&r["id"]), ago(r["created_at"].as_i64().unwrap_or(0)));
    }
}

async fn find(api: &Api, key: &str) -> anyhow::Result<Value> {
    api.get(&format!("/v1/redis/databases/{key}")).await.with_context(|| format!("no Redis database {key}"))
}

/// Print a reply like redis-cli does.
fn print_reply(v: &Value, indent: usize) {
    let pad = " ".repeat(indent);
    match v {
        Value::Null => println!("(nil)"),
        Value::Number(n) => println!("(integer) {n}"),
        Value::String(x) => println!("\"{x}\""),
        Value::Array(a) if a.is_empty() => println!("(empty array)"),
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    print!("{pad}");
                }
                print!("{}) ", i + 1);
                print_reply(x, indent + 3);
            }
        }
        Value::Object(o) if o.contains_key("error") => println!("(error) {}", s(&o["error"])),
        x => println!("{x}"),
    }
}

pub async fn run(api: &Api, as_json: bool, cmd: Redis) -> anyhow::Result<()> {
    let out = match cmd {
        Redis::List => {
            let v = api.get("/v1/redis/databases").await?;
            if !as_json {
                for d in v.as_array().into_iter().flatten() {
                    print_db(d);
                }
                return Ok(());
            }
            v
        }
        Redis::Create { name, memory, eviction, durability, idle } => {
            let body = json!({ "name": name, "max_memory_mb": memory, "eviction_policy": eviction, "durability": durability, "idle_timeout_s": idle });
            let v = api.post("/v1/redis/databases", body).await?;
            if !as_json {
                print_db(&v);
                println!("\n{}", s(&v["redis_url"]));
                println!("REST {}  token {}", s(&v["rest_url"]), s(&v["rest_token"]));
                return Ok(());
            }
            v
        }
        Redis::Show { db } => {
            let v = find(api, &db).await?;
            if !as_json {
                print_db(&v);
                println!("\n{}", s(&v["redis_url"]));
                println!("REST {}  token {}", s(&v["rest_url"]), s(&v["rest_token"]));
                println!("restore window {}", s(&v["history_retention_s"]));
                return Ok(());
            }
            v
        }
        Redis::Set { db, memory, eviction, durability, idle, retention, rename } => {
            let d = find(api, &db).await?;
            let retention = retention.map(|w| parse_window(&w)).transpose()?;
            let body = json!({
                "max_memory_mb": memory, "eviction_policy": eviction, "durability": durability,
                "idle_timeout_s": idle, "history_retention_s": retention, "name": rename,
            });
            let v = api.call(reqwest::Method::PATCH, &format!("/v1/redis/databases/{}", s(&d["id"])), Some(body)).await?;
            if !as_json {
                print_db(&v);
                return Ok(());
            }
            v
        }
        Redis::Delete { db } => {
            let d = find(api, &db).await?;
            api.call(reqwest::Method::DELETE, &format!("/v1/redis/databases/{}", s(&d["id"])), None).await?;
            println!("deleted {}", s(&d["name"]));
            return Ok(());
        }
        Redis::Url { db } => {
            println!("{}", s(&find(api, &db).await?["redis_url"]));
            return Ok(());
        }
        Redis::Rest { db } => {
            let d = find(api, &db).await?;
            println!("UPSTASH_REDIS_REST_URL={}\nUPSTASH_REDIS_REST_TOKEN={}", s(&d["rest_url"]), s(&d["rest_token"]));
            return Ok(());
        }
        Redis::Branch { db, name, at: when } => {
            let d = find(api, &db).await?;
            let t = when.as_deref().map(at).transpose()?;
            let v = api.post(&format!("/v1/redis/databases/{}/branches", s(&d["id"])), json!({ "name": name, "at": t })).await?;
            if !as_json {
                println!("branch {} ({}) of {}", s(&v["name"]), s(&v["id"]), s(&d["name"]));
                println!("{}", s(&v["redis_url"]));
                return Ok(());
            }
            v
        }
        Redis::Restore { db, at: when } => {
            let d = find(api, &db).await?;
            let t = at(&when)?;
            let v = api.post(&format!("/v1/redis/databases/{}/restore", s(&d["id"])), json!({ "at": t })).await?;
            if !as_json {
                println!("restored {} to {} ({}); previous state archived as {}", s(&d["name"]), t, ago(t), s(&v["previous_state"]));
                return Ok(());
            }
            v
        }
        Redis::Replica { db } => {
            let d = find(api, &db).await?;
            let v = api.post(&format!("/v1/redis/databases/{}/replicas", s(&d["id"])), json!({})).await?;
            if !as_json {
                println!("replica {} (read-only)\n{}", s(&v["id"]), s(&v["redis_url"]));
                return Ok(());
            }
            v
        }
        Redis::ReplicaDelete { id } => {
            api.call(reqwest::Method::DELETE, &format!("/v1/redis/replicas/{id}"), None).await?;
            println!("deleted replica {id}");
            return Ok(());
        }
        Redis::RotatePassword { db } => {
            let d = find(api, &db).await?;
            let v = api.post(&format!("/v1/redis/databases/{}/password", s(&d["id"])), json!({})).await?;
            if !as_json {
                println!("{}", s(&v["redis_url"]));
                return Ok(());
            }
            v
        }
        Redis::Unload { db } => {
            let d = find(api, &db).await?;
            api.post(&format!("/v1/redis/databases/{}/unload", s(&d["id"])), json!({})).await?
        }
        Redis::Usage { db, days } => {
            let d = find(api, &db).await?;
            let u = api.get(&format!("/v1/redis/databases/{}/usage?days={days}", s(&d["id"])) ).await?;
            if !as_json {
                let f = |v: &Value| v.as_f64().unwrap_or(0.0);
                let c = &u["cost"];
                println!("{} — last {days} days", s(&d["name"]));
                println!("  active        {:>10.1} h", f(&u["active_seconds"]) / 3600.0);
                println!("  memory        {:>10.4} GiB-h   ${:.4}", f(&u["gib_hours"]), f(&c["memory_usd"]));
                println!("  commands      {:>10}          ${:.4}", s(&u["commands"]), f(&c["compute_usd"]));
                println!("  storage       {:>10}          ${:.4}", bytes(f(&u["storage_bytes"])), f(&c["storage_usd"]));
                println!("  egress        {:>10}          ${:.4}", bytes(f(&u["net_out_bytes"])), f(&c["egress_usd"]));
                println!("  total                          ${:.4}", f(&c["total_usd"]));
                println!("  ElastiCache Serverless         ${:.4}", f(&c["elasticache_serverless_usd"]));
                return Ok(());
            }
            u
        }
        Redis::Cli { db, command } => {
            let d = find(api, &db).await?;
            let v = api.post(&format!("/v1/redis/databases/{}/command", s(&d["id"])), json!({ "command": command })).await?;
            if !as_json {
                match v.get("error") {
                    Some(e) => println!("(error) {}", s(e)),
                    None => print_reply(&v["result"], 0),
                }
                return Ok(());
            }
            v
        }
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
