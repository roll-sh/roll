//! `roll queues` — the RabbitMQ-compatible broker, driven through the rolld roll API (`/v1/queues`).

pub(crate) mod fmt;
mod parse;
mod top;

use crate::{Api, ago, s};
use anyhow::{Context, bail};
use clap::{Args, Subcommand, ValueEnum};
use fmt::{Align::L, Align::R, Table, count, f, rate, usd};
use reqwest::Method;
use serde_json::{Map, Value, json};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Args)]
pub struct Queues {
    /// Virtual host (default "/"; `ls` covers every vhost when omitted).
    #[arg(long, global = true, env = "ROLL_QUEUES_VHOST")]
    vhost: Option<String>,
    #[command(subcommand)]
    cmd: Option<Q>,
}

#[derive(Subcommand)]
enum Q {
    /// Overview: endpoints, totals and live rates (the default).
    Status,
    /// Print the AMQP URL for a vhost: `export AMQP_URL=$(roll queues url)`.
    Url,
    /// List vhosts, or create / delete / suspend / wake one.
    Vhosts {
        #[command(subcommand)]
        action: Option<VhostCmd>,
    },
    /// List queues.
    #[command(alias = "list")]
    Ls {
        /// Substring, or a glob with * and ? (e.g. 'orders.*').
        #[arg(long, short)]
        filter: Option<String>,
    },
    /// Show a queue with its consumers and bindings.
    Show { name: String },
    /// Declare a queue.
    Declare {
        name: String,
        #[arg(long = "type", short = 't', value_enum)]
        kind: Option<QueueType>,
        #[arg(long)]
        durable: bool,
        #[arg(long)]
        auto_delete: bool,
        /// Queue argument, repeatable: --arg x-max-length=1000 --arg x-dead-letter-exchange=dlx
        #[arg(long = "arg", value_name = "K=V", value_parser = parse::kv)]
        args: Vec<(String, String)>,
    },
    /// Delete a queue.
    #[command(alias = "delete")]
    Rm { name: String },
    /// Drop every ready message in a queue.
    Purge { name: String },
    /// Browse messages without consuming them.
    Peek {
        name: String,
        #[arg(long, short = 'n', default_value_t = 10)]
        count: u32,
        #[arg(long, default_value_t = 0)]
        offset: u64,
    },
    /// Publish a message: a literal body, @file, or - for stdin.
    Publish {
        /// Exchange (default: the default exchange, which routes by queue name).
        #[arg(long, short = 'e', default_value = "")]
        exchange: String,
        /// Routing key.
        #[arg(long, short = 'k', default_value = "")]
        key: String,
        /// Message header, repeatable: -H trace=abc -H retries=3
        #[arg(long = "header", short = 'H', value_name = "K=V", value_parser = parse::kv)]
        headers: Vec<(String, String)>,
        /// Basic property, repeatable: -p content_type=application/json -p delivery_mode=persistent
        #[arg(long = "property", short = 'p', value_name = "K=V", value_parser = parse::kv)]
        properties: Vec<(String, String)>,
        /// Body text, @path to read a file, or - to read stdin.
        body: String,
    },
    /// Re-publish dead-lettered messages from a DLQ to where they came from (or a target).
    Replay {
        dlq: String,
        /// At most this many messages (default: all).
        #[arg(long, short = 'n')]
        count: Option<u64>,
        #[arg(long)]
        to_exchange: Option<String>,
        /// Routing key for the target (with no --to-exchange, the default exchange).
        #[arg(long)]
        to_key: Option<String>,
    },
    /// List exchanges, or declare / rm one.
    Exchanges {
        #[command(subcommand)]
        action: Option<ExchangeCmd>,
    },
    /// Bind a queue (or exchange) to an exchange.
    Bind(BindArgs),
    /// Remove a binding.
    Unbind(BindArgs),
    /// List bindings.
    Bindings {
        /// Match on source, destination or routing key (substring or glob).
        #[arg(long, short)]
        filter: Option<String>,
    },
    /// List client connections, or close one.
    #[command(alias = "conns")]
    Connections {
        #[command(subcommand)]
        action: Option<ConnCmd>,
    },
    /// List channels.
    Channels,
    /// List policies, or set / rm one.
    Policies {
        #[command(subcommand)]
        action: Option<PolicyCmd>,
    },
    /// List users, or add / rm / change a password.
    Users {
        #[command(subcommand)]
        action: Option<UserCmd>,
    },
    /// List permissions, or set them for a user on the vhost.
    Permissions {
        #[command(subcommand)]
        action: Option<PermCmd>,
    },
    /// Metered usage, cost and the same workload on Amazon MQ and CloudAMQP.
    Usage {
        #[arg(long, default_value_t = 30)]
        days: u32,
    },
    /// Export or import RabbitMQ definitions JSON.
    #[command(alias = "defs")]
    Definitions {
        #[command(subcommand)]
        action: DefCmd,
    },
    /// Import topology from a RabbitMQ and shovel its messages over; with no --from, show progress.
    Migrate {
        /// Source management API, e.g. http://rabbit.internal:15672
        #[arg(long, value_name = "MANAGEMENT_URL")]
        from: Option<String>,
        #[arg(long, env = "ROLL_MIGRATE_USER")]
        user: Option<String>,
        #[arg(long, env = "ROLL_MIGRATE_PASSWORD", hide_env_values = true)]
        password: Option<String>,
        /// Also move the messages (live shovels), not just the topology.
        #[arg(long)]
        move_messages: bool,
        /// Where the shovels reach the source over AMQP, when the port the source reports isn't
        /// the one you can reach (NAT, load balancer, Docker), e.g. amqps://rabbit.example.com:5671.
        /// Credentials default to --user / --password.
        #[arg(
            long,
            value_name = "URI",
            env = "ROLL_MIGRATE_AMQP_URI",
            hide_env_values = true,
            requires = "from"
        )]
        amqp_uri: Option<String>,
        /// Also import the source's own shovels and federation upstreams (they start running here).
        #[arg(long, requires = "from")]
        include_shovels: bool,
        /// Follow the shovels until they drain.
        #[arg(long)]
        watch: bool,
        /// Stop the migration: remove its shovels (and the source credentials they hold).
        #[arg(long, conflicts_with_all = ["from", "watch", "move_messages"])]
        stop: bool,
    },
    /// Live view of rates and the deepest queues (Ctrl-C to quit).
    Top {
        /// Seconds between refreshes.
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
        /// Queues to show.
        #[arg(long, short = 'n', default_value_t = 15)]
        limit: usize,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum QueueType {
    Classic,
    Quorum,
    Stream,
}

impl QueueType {
    fn as_str(self) -> &'static str {
        match self {
            QueueType::Classic => "classic",
            QueueType::Quorum => "quorum",
            QueueType::Stream => "stream",
        }
    }
}

/// The user, password and one URL per protocol, as printed after provisioning.
fn credentials_text(c: &Value) -> String {
    let mut out = format!(
        "\nuser      {}\npassword  {}  (shown once)",
        c["user"].as_str().unwrap_or_default(),
        c["password"].as_str().unwrap_or_default()
    );
    if let Some(urls) = c["urls"].as_object() {
        for (proto, url) in urls {
            if let Some(u) = url.as_str() {
                out.push_str(&format!("\n{proto:<10}{u}"));
            }
        }
    }
    out
}

#[derive(Subcommand)]
enum VhostCmd {
    /// Create a vhost.
    Create {
        name: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, value_enum)]
        default_queue_type: Option<QueueType>,
        /// Also create a user for just this vhost and print its connection URLs.
        #[arg(long)]
        credentials: bool,
        /// Name for that user (default: the vhost name). Implies --credentials.
        #[arg(long)]
        user: Option<String>,
    },
    /// Issue (or rotate) the vhost's own user and print its connection URLs.
    Credentials {
        name: String,
        #[arg(long)]
        user: Option<String>,
    },
    /// Delete a vhost and everything in it.
    #[command(alias = "rm")]
    Delete { name: String },
    /// Unload a vhost now (scale to zero); it wakes on the next connection.
    Suspend { name: String },
    /// Load a sleeping vhost now.
    Wake { name: String },
}

#[derive(Subcommand)]
enum ExchangeCmd {
    /// Declare an exchange.
    Declare {
        name: String,
        /// direct, fanout, topic, headers, or a plugin type such as x-delayed-message.
        #[arg(long = "type", short = 't', default_value = "direct")]
        kind: String,
        #[arg(long)]
        durable: bool,
        #[arg(long)]
        auto_delete: bool,
        #[arg(long)]
        internal: bool,
        #[arg(long = "arg", value_name = "K=V", value_parser = parse::kv)]
        args: Vec<(String, String)>,
    },
    /// Delete an exchange.
    #[command(alias = "delete")]
    Rm { name: String },
}

#[derive(Args)]
struct BindArgs {
    /// Source exchange.
    source: String,
    /// Destination queue (or exchange with --dest-type exchange).
    destination: String,
    #[arg(long, short = 'k', default_value = "")]
    key: String,
    #[arg(long, value_enum, default_value = "queue")]
    dest_type: DestType,
    #[arg(long = "arg", value_name = "K=V", value_parser = parse::kv)]
    args: Vec<(String, String)>,
}

#[derive(Clone, Copy, ValueEnum)]
enum DestType {
    Queue,
    Exchange,
}

#[derive(Subcommand)]
enum ConnCmd {
    /// Force-close a connection.
    Close { id: u64 },
}

#[derive(Subcommand)]
enum PolicyCmd {
    /// Create or replace a policy on the vhost.
    Set {
        name: String,
        /// Regex matched against queue / exchange names.
        #[arg(long)]
        pattern: String,
        #[arg(long, default_value = "all",
              value_parser = ["all", "queues", "exchanges", "classic_queues", "quorum_queues", "streams"])]
        apply_to: String,
        #[arg(long, default_value_t = 0)]
        priority: i64,
        /// e.g. '{"max-length": 1000, "dead-letter-exchange": "dlx"}'
        #[arg(long, value_parser = parse::json_object)]
        definition: Map<String, Value>,
    },
    /// Delete a policy.
    #[command(alias = "delete")]
    Rm { name: String },
}

#[derive(Subcommand)]
enum UserCmd {
    /// Add a user (password from --password or stdin).
    Add {
        name: String,
        #[arg(long)]
        password: Option<String>,
        /// Comma-separated, e.g. administrator,monitoring
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
    },
    /// Delete a user.
    #[command(alias = "delete")]
    Rm { name: String },
    /// Change a user's password (from --password or stdin).
    Passwd {
        name: String,
        #[arg(long)]
        password: Option<String>,
    },
}

#[derive(Subcommand)]
enum PermCmd {
    /// Set a user's permissions on the vhost (regexes; default: everything).
    Set {
        user: String,
        #[arg(long, default_value = ".*")]
        configure: String,
        #[arg(long, default_value = ".*")]
        write: String,
        #[arg(long, default_value = ".*")]
        read: String,
    },
}

#[derive(Subcommand)]
enum DefCmd {
    /// Write definitions to a file (or stdout).
    Export { file: Option<PathBuf> },
    /// Import definitions from a file (or - for stdin).
    Import { file: PathBuf },
}

/// URL-encode one path segment (`/` → `%2F`).
fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

/// `/v1/queues/vhosts/{vhost}{rest}`
fn vpath(vhost: &str, rest: &str) -> String {
    format!("/v1/queues/vhosts/{}{rest}", enc(vhost))
}

/// Drop null fields (unset options) so the server applies its own defaults.
fn clean(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, clean(v)))
                .collect(),
        ),
        v => v,
    }
}

fn print_json(v: &Value) -> anyhow::Result<()> {
    let v = if v.is_null() { &json!({}) } else { v };
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn list(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

fn opt_true(b: bool) -> Option<bool> {
    b.then_some(true)
}

/// A password from the flag, or piped on stdin (`echo s3cret | roll queues users add app`).
fn password(flag: Option<String>) -> anyhow::Result<String> {
    if let Some(p) = flag {
        return Ok(p);
    }
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        bail!("pass --password, or pipe it on stdin");
    }
    let mut buf = String::new();
    stdin.lock().read_to_string(&mut buf)?;
    let p = buf.trim_end_matches(['\r', '\n']).to_string();
    if p.is_empty() {
        bail!("empty password");
    }
    Ok(p)
}

async fn vhost_names(api: &Api) -> anyhow::Result<Vec<String>> {
    let v = api.get("/v1/queues/vhosts").await?;
    Ok(list(&v).map(|v| s(&v["name"])).collect())
}

/// Queues of one vhost, or of every vhost when `vhost` is None.
async fn all_queues(api: &Api, vhost: Option<&str>) -> anyhow::Result<Vec<Value>> {
    let names = match vhost {
        Some(v) => vec![v.to_string()],
        None => vhost_names(api).await?,
    };
    let mut out = Vec::new();
    for v in &names {
        let qs = api.get(&vpath(v, "/queues")).await?;
        out.extend(list(&qs).cloned());
    }
    Ok(out)
}

pub async fn run(api: &Api, json: bool, q: Queues) -> anyhow::Result<()> {
    let only = q.vhost.as_deref();
    let vh = only.unwrap_or("/");
    match q.cmd.unwrap_or(Q::Status) {
        Q::Status => {
            let o = api.get("/v1/queues/overview").await?;
            if json {
                return print_json(&o);
            }
            let points = api
                .get("/v1/queues/rates?seconds=300")
                .await
                .unwrap_or(Value::Null);
            print_status(&o, &points["points"]);
        }
        Q::Url => {
            let v = api.get("/v1/queues/vhosts").await?;
            let url = list(&v)
                .find(|x| x["name"] == vh)
                .with_context(|| format!("no vhost {vh:?} (see `roll queues vhosts`)"))?["amqp_url"]
                .as_str()
                .with_context(|| format!("vhost {vh:?} has no AMQP URL"))?
                .to_string();
            println!("{url}");
        }
        Q::Vhosts { action: None } => {
            let v = api.get("/v1/queues/vhosts").await?;
            if json {
                return print_json(&v);
            }
            let mut t = Table::new(&[
                ("vhost", L),
                ("state", L),
                ("conns", R),
                ("queues", R),
                ("ready", R),
                ("unacked", R),
                ("stored", R),
                ("pub/s", R),
                ("del/s", R),
            ]);
            for x in list(&v) {
                t.row(vec![
                    s(&x["name"]),
                    s(&x["state"]),
                    count(f(&x["connections"])),
                    count(f(&x["queues"])),
                    count(f(&x["messages_ready"])),
                    count(f(&x["messages_unacked"])),
                    fmt::size(f(&x["stored_bytes"])),
                    rate(f(&x["rates"]["publish"])),
                    rate(f(&x["rates"]["deliver"])),
                ]);
            }
            t.print();
        }
        Q::Vhosts { action: Some(cmd) } => {
            let (v, msg) = match cmd {
                VhostCmd::Create {
                    name,
                    description,
                    default_queue_type,
                    credentials,
                    user,
                } => {
                    let body = clean(json!({
                        "name": name,
                        "description": description,
                        "default_queue_type": default_queue_type.map(QueueType::as_str),
                        "credentials": credentials || user.is_some(),
                        "user": user,
                    }));

                    let v = api.post("/v1/queues/vhosts", body).await?;
                    let urls = if v["credentials"].is_object() {
                        credentials_text(&v["credentials"])
                    } else {
                        v["amqp_url"]
                            .as_str()
                            .map(|u| format!("\n{u}"))
                            .unwrap_or_default()
                    };
                    (v, format!("created vhost {name}{urls}"))
                }
                VhostCmd::Credentials { name, user } => {
                    let v = api
                        .post(&vpath(&name, "/credentials"), clean(json!({ "user": user })))
                        .await?;
                    let text = credentials_text(&v);
                    (v, format!("new credentials for vhost {name}{text}"))
                }
                VhostCmd::Delete { name } => {
                    let v = api.call(Method::DELETE, &vpath(&name, ""), None).await?;
                    (v, format!("deleted vhost {name}"))
                }
                VhostCmd::Suspend { name } => {
                    let v = api.post(&vpath(&name, "/suspend"), json!({})).await?;
                    (
                        v,
                        format!("suspended vhost {name}; it wakes on the next connection"),
                    )
                }
                VhostCmd::Wake { name } => {
                    let v = api.post(&vpath(&name, "/wake"), json!({})).await?;
                    (v, format!("vhost {name} is running"))
                }
            };
            if json {
                return print_json(&v);
            }
            println!("{msg}");
        }
        Q::Ls { filter } => {
            let mut qs = all_queues(api, only).await?;
            qs.retain(|x| fmt::matches(filter.as_deref(), x["name"].as_str().unwrap_or_default()));
            if json {
                return print_json(&Value::Array(qs));
            }
            if qs.is_empty() {
                println!("no queues");
                return Ok(());
            }
            qs.sort_by(|a, b| {
                (s(&a["vhost"]), s(&a["name"])).cmp(&(s(&b["vhost"]), s(&b["name"])))
            });
            print_queues(&qs, only.is_none());
        }
        Q::Show { name } => {
            let x = api
                .get(&vpath(vh, &format!("/queues/{}", enc(&name))))
                .await?;
            if json {
                return print_json(&x);
            }
            print_queue(&x);
        }
        Q::Declare {
            name,
            kind,
            durable,
            auto_delete,
            args,
        } => {
            let body = clean(json!({
                "name": name,
                "type": kind.map(QueueType::as_str),
                "durable": opt_true(durable),
                "auto_delete": opt_true(auto_delete),
                "arguments": parse::object(&args),
            }));

            let v = api.post(&vpath(vh, "/queues"), body).await?;
            if json {
                return print_json(&v);
            }
            let kind = v["type"]
                .as_str()
                .or(kind.map(QueueType::as_str))
                .unwrap_or("classic");
            println!("declared {kind} queue {name} in {vh}");
        }
        Q::Rm { name } => {
            let v = api
                .call(
                    Method::DELETE,
                    &vpath(vh, &format!("/queues/{}", enc(&name))),
                    None,
                )
                .await?;
            if json {
                return print_json(&v);
            }
            println!("deleted queue {name}");
        }
        Q::Purge { name } => {
            let v = api
                .post(
                    &vpath(vh, &format!("/queues/{}/purge", enc(&name))),
                    json!({}),
                )
                .await?;
            if json {
                return print_json(&v);
            }
            println!("purged {} messages from {name}", count(f(&v["purged"])));
        }
        Q::Peek {
            name,
            count: n,
            offset,
        } => {
            let path = vpath(
                vh,
                &format!("/queues/{}/messages?offset={offset}&count={n}", enc(&name)),
            );
            let v = api.get(&path).await?;
            if json {
                return print_json(&v);
            }
            let msgs: Vec<&Value> = list(&v).collect();
            if msgs.is_empty() {
                println!("no messages in {name} at offset {offset}");
                return Ok(());
            }
            print_messages(&msgs);
        }
        Q::Publish {
            exchange,
            key,
            headers,
            properties,
            body,
        } => {
            let props = parse::properties(&properties, &headers).map_err(anyhow::Error::msg)?;
            let (body, encoding) = parse::encode_body(parse::read_body(&body, std::io::stdin())?);
            let req = json!({
                "exchange": exchange,
                "routing_key": key,
                "body": body,
                "body_encoding": encoding,
                "properties": props,
            });
            let v = api.post(&vpath(vh, "/publish"), req).await?;
            if json {
                return print_json(&v);
            }
            let via = format!("{} with key {key:?}", fmt::exchange_name(&exchange));
            match &v["routed"] {
                Value::Bool(false) => {
                    eprintln!("published to {via}, but no queue matched (unroutable)")
                }
                Value::Number(n) if n.as_u64() == Some(0) => {
                    eprintln!("published to {via}, but no queue matched (unroutable)")
                }
                Value::Number(n) => println!("published to {via}, routed to {n} queue(s)"),
                _ => println!("published to {via}"),
            }
        }
        Q::Replay {
            dlq,
            count: n,
            to_exchange,
            to_key,
        } => {
            let to_exchange = to_exchange.or_else(|| to_key.as_ref().map(|_| String::new()));
            let body = clean(
                json!({ "count": n, "target_exchange": to_exchange, "target_routing_key": to_key }),
            );

            let v = api
                .post(&vpath(vh, &format!("/queues/{}/replay", enc(&dlq))), body)
                .await?;
            if json {
                return print_json(&v);
            }
            let dest = match (&to_exchange, &to_key) {
                (Some(x), k) => format!(
                    " to {} with key {:?}",
                    fmt::exchange_name(x),
                    k.as_deref().unwrap_or("")
                ),
                _ => " to their original exchanges".into(),
            };
            println!(
                "replayed {} messages from {dlq}{dest}",
                count(f(&v["replayed"]))
            );
        }
        Q::Exchanges { action: None } => {
            let v = api.get(&vpath(vh, "/exchanges")).await?;
            if json {
                return print_json(&v);
            }
            let mut xs: Vec<&Value> = list(&v).collect();
            xs.sort_by_key(|x| s(&x["name"]));
            let mut t = Table::new(&[
                ("exchange", L),
                ("type", L),
                ("bindings", R),
                ("in/s", R),
                ("out/s", R),
                ("features", L),
            ]);
            for x in xs {
                let mut feats = flags(
                    x,
                    &[
                        ("durable", "durable"),
                        ("auto_delete", "auto-delete"),
                        ("internal", "internal"),
                    ],
                );
                if x["arguments"].as_object().is_some_and(|m| !m.is_empty()) {
                    feats.push(fmt::compact(&x["arguments"]));
                }
                t.row(vec![
                    fmt::exchange_name(x["name"].as_str().unwrap_or_default()).to_string(),
                    s(&x["type"]),
                    count(f(&x["bindings"])),
                    rate(f(&x["rates"]["publish_in"])),
                    rate(f(&x["rates"]["publish_out"])),
                    feats.join(" "),
                ]);
            }
            t.print();
        }
        Q::Exchanges { action: Some(cmd) } => {
            let (v, msg) = match cmd {
                ExchangeCmd::Declare {
                    name,
                    kind,
                    durable,
                    auto_delete,
                    internal,
                    args,
                } => {
                    let body = clean(json!({
                        "name": name,
                        "type": kind,
                        "durable": opt_true(durable),
                        "auto_delete": opt_true(auto_delete),
                        "internal": opt_true(internal),
                        "arguments": parse::object(&args),
                    }));

                    let v = api.post(&vpath(vh, "/exchanges"), body).await?;
                    (v, format!("declared {kind} exchange {name} in {vh}"))
                }
                ExchangeCmd::Rm { name } => {
                    let v = api
                        .call(
                            Method::DELETE,
                            &vpath(vh, &format!("/exchanges/{}", enc(&name))),
                            None,
                        )
                        .await?;
                    (v, format!("deleted exchange {name}"))
                }
            };
            if json {
                return print_json(&v);
            }
            println!("{msg}");
        }
        Q::Bind(b) => bind(api, json, vh, b, true).await?,
        Q::Unbind(b) => bind(api, json, vh, b, false).await?,
        Q::Bindings { filter } => {
            let v = api.get(&vpath(vh, "/bindings")).await?;
            let keep = |x: &Value| {
                ["source", "destination", "routing_key"]
                    .iter()
                    .any(|k| fmt::matches(filter.as_deref(), x[*k].as_str().unwrap_or_default()))
            };
            let bs: Vec<&Value> = list(&v).filter(|x| keep(x)).collect();
            if json {
                return print_json(&Value::Array(bs.into_iter().cloned().collect()));
            }
            if bs.is_empty() {
                println!("no bindings");
                return Ok(());
            }
            let mut t = Table::new(&[
                ("source", L),
                ("", L),
                ("destination", L),
                ("type", L),
                ("key", L),
                ("arguments", L),
            ]);
            for x in bs {
                t.row(vec![
                    fmt::exchange_name(x["source"].as_str().unwrap_or_default()).to_string(),
                    "→".into(),
                    s(&x["destination"]),
                    s(&x["destination_type"]),
                    s(&x["routing_key"]),
                    fmt::compact(&x["arguments"]),
                ]);
            }
            t.print();
        }
        Q::Connections { action: None } => {
            let v = api.get("/v1/queues/connections").await?;
            let cs: Vec<&Value> = list(&v)
                .filter(|x| only.is_none_or(|o| x["vhost"] == o))
                .collect();
            if json {
                return print_json(&Value::Array(cs.into_iter().cloned().collect()));
            }
            if cs.is_empty() {
                println!("no connections");
                return Ok(());
            }
            let mut t = Table::new(&[
                ("id", R),
                ("user", L),
                ("vhost", L),
                ("protocol", L),
                ("peer", L),
                ("ch", R),
                ("recv", R),
                ("sent", R),
                ("connected", L),
                ("client", L),
            ]);
            for x in cs {
                let peer = format!(
                    "{}{}",
                    s(&x["peer"]),
                    if x["tls"] == true { " (tls)" } else { "" }
                );
                let since = x["connected_at_ms"]
                    .as_i64()
                    .map(|ms| ago(ms / 1000))
                    .unwrap_or_else(|| "-".into());
                t.row(vec![
                    s(&x["id"]),
                    s(&x["user"]),
                    s(&x["vhost"]),
                    s(&x["protocol"]),
                    peer,
                    count(f(&x["channels"])),
                    fmt::size(f(&x["recv_oct"])),
                    fmt::size(f(&x["send_oct"])),
                    since,
                    fmt::truncate(&s(&x["client"]), 40),
                ]);
            }
            t.print();
        }
        Q::Connections {
            action: Some(ConnCmd::Close { id }),
        } => {
            let v = api
                .call(
                    Method::DELETE,
                    &format!("/v1/queues/connections/{id}"),
                    None,
                )
                .await?;
            if json {
                return print_json(&v);
            }
            println!("closed connection {id}");
        }
        Q::Channels => {
            let v = api.get("/v1/queues/channels").await?;
            let cs: Vec<&Value> = list(&v)
                .filter(|x| only.is_none_or(|o| x["vhost"] == o))
                .collect();
            if json {
                return print_json(&Value::Array(cs.into_iter().cloned().collect()));
            }
            if cs.is_empty() {
                println!("no channels");
                return Ok(());
            }
            let mut t = Table::new(&[
                ("id", R),
                ("conn", R),
                ("ch", R),
                ("user", L),
                ("vhost", L),
                ("prefetch", R),
                ("unacked", R),
                ("consumers", R),
                ("mode", L),
            ]);
            for x in cs {
                let mode = flags(x, &[("confirm", "confirm"), ("transactional", "tx")]);
                t.row(vec![
                    s(&x["id"]),
                    s(&x["connection"]),
                    s(&x["number"]),
                    s(&x["user"]),
                    s(&x["vhost"]),
                    count(f(&x["prefetch"])),
                    count(f(&x["unacked"])),
                    count(f(&x["consumers"])),
                    mode.join(" "),
                ]);
            }
            t.print();
        }
        Q::Policies { action: None } => {
            let v = api.get("/v1/queues/policies").await?;
            let ps: Vec<&Value> = list(&v)
                .filter(|x| only.is_none_or(|o| x["vhost"] == o))
                .collect();
            if json {
                return print_json(&Value::Array(ps.into_iter().cloned().collect()));
            }
            if ps.is_empty() {
                println!("no policies");
                return Ok(());
            }
            let mut t = Table::new(&[
                ("vhost", L),
                ("policy", L),
                ("pattern", L),
                ("apply to", L),
                ("priority", R),
                ("definition", L),
            ]);
            for x in ps {
                let name = format!(
                    "{}{}",
                    s(&x["name"]),
                    if x["operator"] == true {
                        " (operator)"
                    } else {
                        ""
                    }
                );
                t.row(vec![
                    s(&x["vhost"]),
                    name,
                    s(&x["pattern"]),
                    s(&x["apply_to"]),
                    s(&x["priority"]),
                    fmt::compact(&x["definition"]),
                ]);
            }
            t.print();
        }
        Q::Policies { action: Some(cmd) } => {
            let (v, msg) = match cmd {
                PolicyCmd::Set {
                    name,
                    pattern,
                    apply_to,
                    priority,
                    definition,
                } => {
                    let body = json!({ "pattern": pattern, "apply_to": apply_to, "definition": definition, "priority": priority });
                    let path = format!("/v1/queues/policies/{}/{}", enc(vh), enc(&name));
                    let v = api.call(Method::PUT, &path, Some(body)).await?;
                    (
                        v,
                        format!("set policy {name} on {apply_to} matching {pattern:?} in {vh}"),
                    )
                }
                PolicyCmd::Rm { name } => {
                    let path = format!("/v1/queues/policies/{}/{}", enc(vh), enc(&name));
                    let v = api.call(Method::DELETE, &path, None).await?;
                    (v, format!("deleted policy {name}"))
                }
            };
            if json {
                return print_json(&v);
            }
            println!("{msg}");
        }
        Q::Users { action: None } => {
            let v = api.get("/v1/queues/users").await?;
            if json {
                return print_json(&v);
            }
            let mut t = Table::new(&[("user", L), ("tags", L), ("vhosts", L)]);
            for x in list(&v) {
                let tags: Vec<String> = list(&x["tags"]).map(s).collect();
                let vhosts: Vec<String> = list(&x["permissions"]).map(|p| s(&p["vhost"])).collect();
                t.row(vec![
                    s(&x["name"]),
                    or_dash(tags.join(",")),
                    or_dash(vhosts.join(" ")),
                ]);
            }
            t.print();
        }
        Q::Users { action: Some(cmd) } => {
            let (v, msg) = match cmd {
                UserCmd::Add {
                    name,
                    password: p,
                    tags,
                } => {
                    let body = json!({ "password": password(p)?, "tags": tags });
                    let v = api
                        .call(
                            Method::PUT,
                            &format!("/v1/queues/users/{}", enc(&name)),
                            Some(body),
                        )
                        .await?;
                    (
                        v,
                        format!(
                            "added user {name}; grant access with `roll queues permissions set {name}`"
                        ),
                    )
                }
                UserCmd::Rm { name } => {
                    let v = api
                        .call(
                            Method::DELETE,
                            &format!("/v1/queues/users/{}", enc(&name)),
                            None,
                        )
                        .await?;
                    (v, format!("deleted user {name}"))
                }
                UserCmd::Passwd { name, password: p } => {
                    let body = json!({ "password": password(p)? });
                    let v = api
                        .call(
                            Method::PUT,
                            &format!("/v1/queues/users/{}", enc(&name)),
                            Some(body),
                        )
                        .await?;
                    (v, format!("changed the password of {name}"))
                }
            };
            if json {
                return print_json(&v);
            }
            println!("{msg}");
        }
        Q::Permissions { action: None } => {
            let v = api.get("/v1/queues/users").await?;
            let mut rows = Vec::new();
            for u in list(&v) {
                for p in list(&u["permissions"]).filter(|p| only.is_none_or(|o| p["vhost"] == o)) {
                    rows.push(json!({ "user": u["name"], "vhost": p["vhost"], "configure": p["configure"], "write": p["write"], "read": p["read"] }));
                }
            }
            if json {
                return print_json(&Value::Array(rows));
            }
            if rows.is_empty() {
                println!("no permissions");
                return Ok(());
            }
            let mut t = Table::new(&[
                ("user", L),
                ("vhost", L),
                ("configure", L),
                ("write", L),
                ("read", L),
            ]);
            for r in &rows {
                t.row(
                    ["user", "vhost", "configure", "write", "read"]
                        .iter()
                        .map(|k| s(&r[*k]))
                        .collect(),
                );
            }
            t.print();
        }
        Q::Permissions {
            action:
                Some(PermCmd::Set {
                    user,
                    configure,
                    write,
                    read,
                }),
        } => {
            let body = json!({ "configure": configure, "write": write, "read": read });
            let path = format!("/v1/queues/permissions/{}/{}", enc(vh), enc(&user));
            let v = api.call(Method::PUT, &path, Some(body)).await?;
            if json {
                return print_json(&v);
            }
            println!("{user} on {vh}: configure {configure:?}  write {write:?}  read {read:?}");
        }
        Q::Usage { days } => {
            let u = api.get(&format!("/v1/queues/usage?days={days}")).await?;
            if json {
                return print_json(&u);
            }
            print_usage(&u, days);
        }
        Q::Definitions {
            action: DefCmd::Export { file },
        } => {
            let v = api.get("/v1/queues/definitions").await?;
            let text = serde_json::to_string_pretty(&v)? + "\n";
            match file {
                Some(path) if path.as_os_str() != "-" => {
                    std::fs::write(&path, text)
                        .with_context(|| format!("writing {}", path.display()))?;
                    eprintln!("wrote {} ({})", path.display(), def_summary(&v));
                }
                _ => print!("{text}"),
            }
        }
        Q::Definitions {
            action: DefCmd::Import { file },
        } => {
            let text = if file.as_os_str() == "-" {
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                buf
            } else {
                std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?
            };
            let defs: Value = serde_json::from_str(&text)
                .with_context(|| format!("{} is not JSON", file.display()))?;
            let v = api.post("/v1/queues/definitions", defs.clone()).await?;
            if json {
                return print_json(&v);
            }
            println!("imported {}", def_summary(&defs));
            print!("{}", skipped_text(&v["skipped"]));
        }
        Q::Migrate {
            from,
            user,
            password: pass,
            move_messages,
            amqp_uri,
            include_shovels,
            watch,
            stop,
        } => {
            if stop {
                let v = api.call(Method::DELETE, "/v1/queues/migrate", None).await?;
                if json {
                    return print_json(&v);
                }
                let stopped: Vec<String> = list(&v["stopped"]).map(s).collect();
                if stopped.is_empty() {
                    println!("no migration shovels to stop");
                } else {
                    println!(
                        "stopped {} shovel(s): {}",
                        stopped.len(),
                        stopped.join(", ")
                    );
                }
                return Ok(());
            }
            if let Some(url) = from {
                let (Some(user), Some(pass)) = (user, pass) else {
                    bail!(
                        "--user and --password are required with --from (or ROLL_MIGRATE_USER / ROLL_MIGRATE_PASSWORD)"
                    );
                };
                let body = clean(json!({
                    "source": { "management_url": url, "username": user, "password": pass, "vhost": only, "amqp_uri": amqp_uri },
                    "move_messages": move_messages,
                    "include_shovels": include_shovels,
                }));

                let v = api.post("/v1/queues/migrate", body).await?;
                if json && !watch {
                    return print_json(&v);
                }
                let i = &v["imported"];
                println!(
                    "imported from {url}: {} vhosts, {} exchanges, {} queues, {} bindings, {} policies, {} users",
                    count(f(&i["vhosts"])),
                    count(f(&i["exchanges"])),
                    count(f(&i["queues"])),
                    count(f(&i["bindings"])),
                    count(f(&i["policies"])),
                    count(f(&i["users"])),
                );
                print!("{}", skipped_text(&v["skipped"]));
                let shovels: Vec<String> = list(&v["shovels"]).map(s).collect();
                if shovels.is_empty() {
                    if move_messages {
                        println!("no messages to move");
                    }
                    return Ok(());
                }
                println!("moving messages with {} shovel(s)", shovels.len());
                if !watch {
                    println!("follow with `roll queues migrate --watch`");
                    return Ok(());
                }
            }
            if watch {
                return watch_migration(api).await;
            }
            let v = api.get("/v1/queues/migrate").await?;
            if json {
                return print_json(&v);
            }
            print!("{}", migration_frame(&v).0);
        }
        Q::Top { interval, limit } => top::run(api, only, interval, limit).await?,
    }
    Ok(())
}

async fn bind(api: &Api, json: bool, vh: &str, b: BindArgs, bind: bool) -> anyhow::Result<()> {
    let dest_type = match b.dest_type {
        DestType::Queue => "queue",
        DestType::Exchange => "exchange",
    };
    let body = json!({
        "source": b.source,
        "destination": b.destination,
        "destination_type": dest_type,
        "routing_key": b.key,
        "arguments": parse::object(&b.args),
    });
    let method = if bind { Method::POST } else { Method::DELETE };
    let v = api
        .call(method, &vpath(vh, "/bindings"), Some(body))
        .await?;
    if json {
        return print_json(&v);
    }
    println!(
        "{} {} → {dest_type} {} (key {:?})",
        if bind { "bound" } else { "unbound" },
        fmt::exchange_name(&b.source),
        b.destination,
        b.key
    );
    Ok(())
}

fn or_dash(s: String) -> String {
    if s.is_empty() { "-".into() } else { s }
}

/// The names of the boolean fields that are true.
fn flags(x: &Value, names: &[(&str, &str)]) -> Vec<String> {
    names
        .iter()
        .filter(|(k, _)| x[*k] == true)
        .map(|(_, label)| label.to_string())
        .collect()
}

fn def_summary(v: &Value) -> String {
    let parts: Vec<String> = [
        "vhosts",
        "users",
        "exchanges",
        "queues",
        "bindings",
        "policies",
    ]
    .iter()
    .filter_map(|k| v[*k].as_array().map(|a| format!("{} {k}", a.len())))
    .collect();
    if parts.is_empty() {
        "definitions".into()
    } else {
        parts.join(", ")
    }
}

fn print_status(o: &Value, points: &Value) {
    println!(
        "roll queues {}  ·  {}  ·  up {}",
        s(&o["version"]),
        s(&o["node"]),
        fmt::span(o["uptime_s"].as_i64().unwrap_or(0))
    );
    let eps = &o["endpoints"];
    for k in [
        "amqp",
        "amqps",
        "http",
        "management",
        "mqtt",
        "stomp",
        "stream",
    ] {
        if let Some(url) = eps[k].as_str() {
            println!("  {k:<11} {url}");
        }
    }
    let t = &o["totals"];
    println!();
    let n = |k: &str| count(f(&t[k]));
    let mut grid = Table::new(&[("", L), ("", R), ("", L), ("", R), ("", L), ("", R)]);
    grid.row(vec![
        "vhosts".into(),
        n("vhosts"),
        "connections".into(),
        n("connections"),
        "channels".into(),
        n("channels"),
    ]);
    grid.row(vec![
        "queues".into(),
        n("queues"),
        "consumers".into(),
        n("consumers"),
        "exchanges".into(),
        n("exchanges"),
    ]);
    grid.row(vec![
        "ready".into(),
        n("messages_ready"),
        "unacked".into(),
        n("messages_unacked"),
        "stored".into(),
        fmt::size(f(&t["stored_bytes"])),
    ]);
    indent(&grid.render());
    println!();
    let pts: Vec<&Value> = list(points).collect();
    let tail = &pts[pts.len().saturating_sub(48)..];
    for k in ["publish", "deliver", "ack"] {
        let line: Vec<f64> = tail.iter().map(|p| f(&p[k])).collect();
        println!(
            "  {k:<8} {:>7}/s  {}",
            rate(f(&o["rates"][k])),
            fmt::spark(&line)
        );
    }
    if !o["storage"].is_null() {
        println!(
            "\n  storage  {} log · {} metadata",
            s(&o["storage"]["backend"]),
            s(&o["storage"]["metadata"])
        );
    }
}

fn print_queues(qs: &[Value], with_vhost: bool) {
    let mut cols = vec![];
    if with_vhost {
        cols.push(("vhost", L));
    }
    cols.extend([
        ("queue", L),
        ("type", L),
        ("ready", R),
        ("unacked", R),
        ("consumers", R),
        ("pub/s", R),
        ("del/s", R),
        ("size", R),
        ("features", L),
    ]);
    let mut t = Table::new(&cols);
    for x in qs {
        let mut row = vec![];
        if with_vhost {
            row.push(s(&x["vhost"]));
        }
        row.extend([
            s(&x["name"]),
            s(&x["type"]),
            count(f(&x["messages_ready"])),
            count(f(&x["messages_unacked"])),
            count(f(&x["consumers"])),
            rate(f(&x["rates"]["publish"])),
            rate(f(&x["rates"]["deliver"])),
            fmt::size(f(&x["bytes"])),
            queue_features(x),
        ]);
        t.row(row);
    }
    t.print();
}

fn queue_features(x: &Value) -> String {
    let mut out = flags(
        x,
        &[
            ("durable", "durable"),
            ("auto_delete", "auto-delete"),
            ("exclusive", "exclusive"),
        ],
    );
    if let Some(dlx) = x["dead_letter_exchange"].as_str() {
        out.push(format!("dlx={}", fmt::exchange_name(dlx)));
    }
    if x["is_dlq"] == true {
        out.push("dlq".into());
    }
    if let Some(p) = x["policy"].as_str() {
        out.push(format!("policy={p}"));
    }
    out.join(" ")
}

fn print_queue(x: &Value) {
    println!(
        "{}  {}  in {}  {}",
        s(&x["name"]),
        s(&x["type"]),
        s(&x["vhost"]),
        queue_features(x)
    );
    println!(
        "  messages   {} ({} ready, {} unacked)  {}",
        count(f(&x["messages"])),
        count(f(&x["messages_ready"])),
        count(f(&x["messages_unacked"])),
        fmt::size(f(&x["bytes"]))
    );
    let r = &x["rates"];
    println!(
        "  rates      publish {}/s  deliver {}/s  ack {}/s  redeliver {}/s",
        rate(f(&r["publish"])),
        rate(f(&r["deliver"])),
        rate(f(&r["ack"])),
        rate(f(&r["redeliver"]))
    );
    println!("  arguments  {}", fmt::compact(&x["arguments"]));
    if f(&x["dead_lettered"]) > 0.0 {
        println!("  dead-lettered  {}", count(f(&x["dead_lettered"])));
    }
    if x["first_offset"].is_number() {
        println!(
            "  offsets    {} → {}",
            s(&x["first_offset"]),
            s(&x["next_offset"])
        );
    }
    let consumers: Vec<&Value> = list(&x["consumers"]).collect();
    if !consumers.is_empty() {
        println!();
        let mut t = Table::new(&[
            ("consumer", L),
            ("conn", R),
            ("ch", R),
            ("ack", L),
            ("prefetch", R),
            ("priority", R),
            ("state", L),
        ]);
        for c in consumers {
            t.row(vec![
                s(&c["consumer_tag"]),
                s(&c["connection"]),
                s(&c["channel"]),
                if c["ack_required"] == true {
                    "manual".into()
                } else {
                    "auto".into()
                },
                count(f(&c["prefetch"])),
                s(&c["priority"]),
                format!(
                    "{}{}",
                    if c["active"] == false {
                        "waiting"
                    } else {
                        "active"
                    },
                    if c["exclusive"] == true {
                        " exclusive"
                    } else {
                        ""
                    }
                ),
            ]);
        }
        t.print();
    }
    let bindings: Vec<&Value> = list(&x["bindings"]).collect();
    if !bindings.is_empty() {
        println!();
        let mut t = Table::new(&[("bound from", L), ("key", L), ("arguments", L)]);
        for b in bindings {
            t.row(vec![
                fmt::exchange_name(b["source"].as_str().unwrap_or_default()).to_string(),
                s(&b["routing_key"]),
                fmt::compact(&b["arguments"]),
            ]);
        }
        t.print();
    }
}

const PEEK_BODY_CHARS: usize = 600;

fn print_messages(msgs: &[&Value]) {
    let w = msgs.iter().map(|m| s(&m["seq"]).len()).max().unwrap_or(1);
    for (i, m) in msgs.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let mut meta = vec![fmt::size(f(&m["payload_bytes"]))];
        if m["redelivered"] == true {
            meta.push("redelivered".into());
        }
        if f(&m["delivery_count"]) > 0.0 {
            meta.push(format!("delivery {}", s(&m["delivery_count"])));
        }
        println!(
            "#{:>w$}  {} → {}  {}",
            s(&m["seq"]),
            fmt::exchange_name(m["exchange"].as_str().unwrap_or_default()),
            s(&m["routing_key"]),
            meta.join(" · ")
        );
        let pad = " ".repeat(w + 3);
        if let Some(props) = m["properties"].as_object().filter(|p| !p.is_empty()) {
            let parts: Vec<String> = props
                .iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| match v {
                    Value::String(s) => format!("{k}={s}"),
                    v => format!("{k}={v}"),
                })
                .collect();
            if !parts.is_empty() {
                println!("{pad}{}", parts.join("  "));
            }
        }
        let payload = s(&m["payload"]);
        if m["payload_encoding"] == "base64" {
            println!("{pad}(binary, base64) {}", fmt::truncate(&payload, 80));
        } else {
            for line in fmt::truncate(&payload, PEEK_BODY_CHARS).lines() {
                println!("{pad}{line}");
            }
        }
        for d in list(&m["x_death"]) {
            let keys: Vec<String> = list(&d["routing_keys"]).map(s).collect();
            println!(
                "{pad}x-death: {} ×{} from {} via {} [{}]",
                s(&d["reason"]),
                s(&d["count"]),
                s(&d["queue"]),
                fmt::exchange_name(d["exchange"].as_str().unwrap_or_default()),
                keys.join(", ")
            );
        }
    }
}

fn print_usage(u: &Value, days: u32) {
    let m = &u["messages"];
    let st = &u["storage"];
    let c = &u["connections"];
    let days = u["period"]["days"].as_u64().unwrap_or(days as u64);
    println!("roll queues — last {days} days");
    let mut t = Table::new(&[("", L), ("usage", R), ("", L), ("cost", R)]);
    t.row(vec![
        "messages".into(),
        count(f(&m["published"]) + f(&m["delivered"])),
        format!(
            "({} in, {} out)",
            count(f(&m["published"])),
            count(f(&m["delivered"]))
        ),
        usd(f(&m["usd"])),
    ]);
    t.row(vec![
        "storage".into(),
        format!("{:.3}", f(&st["gb_months"])),
        format!("GB-months ({} now)", fmt::size(f(&st["current_bytes"]))),
        usd(f(&st["usd"])),
    ]);
    t.row(vec![
        "connections".into(),
        format!("{:.1}", f(&c["hours"])),
        "connection-hours".into(),
        usd(f(&c["usd"])),
    ]);
    t.row(vec![
        "total".into(),
        String::new(),
        String::new(),
        usd(f(&u["total_usd"])),
    ]);
    t.row(vec![
        "projected / month".into(),
        String::new(),
        String::new(),
        usd(f(&u["projected_month_usd"])),
    ]);
    indent(&t.render());

    let compare: Vec<&Value> = list(&u["compare"]).collect();
    if !compare.is_empty() {
        println!("\nsame workload elsewhere, per month");
        let mut t = Table::new(&[("", L), ("plan", L), ("cost", R), ("", L)]);
        t.row(vec![
            "roll queues".into(),
            "pay per use".into(),
            usd(f(&u["projected_month_usd"])),
            String::new(),
        ]);
        for x in compare {
            t.row(vec![
                s(&x["name"]),
                s(&x["plan"]),
                usd(f(&x["monthly_usd"])),
                x["note"].as_str().unwrap_or("").into(),
            ]);
        }
        indent(&t.render());
    }

    let p = &u["pricing"];
    if !p.is_null() {
        println!(
            "\npricing  {} / million messages · {} / GB-month · {} / connection-hour",
            fmt::price(f(&p["usd_per_million_messages"])),
            fmt::price(f(&p["usd_per_gb_month"])),
            fmt::price(f(&p["usd_per_connection_hour"]))
        );
        if let Some(basis) = p["basis"].as_str() {
            println!("         {basis}");
        }
    }

    let daily: Vec<&Value> = list(&u["daily"]).collect();
    if !daily.is_empty() {
        println!("\ndaily");
        let mut t = Table::new(&[
            ("day", L),
            ("published", R),
            ("delivered", R),
            ("stored", R),
            ("conn-h", R),
            ("cost", R),
        ]);
        for d in daily {
            t.row(vec![
                d["day"]
                    .as_i64()
                    .map(fmt::date)
                    .unwrap_or_else(|| s(&d["day"])),
                count(f(&d["published"])),
                count(f(&d["delivered"])),
                fmt::size(f(&d["stored_bytes"])),
                format!("{:.1}", f(&d["connection_hours"])),
                usd(f(&d["usd"])),
            ]);
        }
        indent(&t.render());
    }
}

fn indent(block: &str) {
    for line in block.lines().filter(|l| !l.trim().is_empty()) {
        println!("  {line}");
    }
}

/// Where one migration shovel is, from `GET /v1/queues/migrate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Starting, or running but hasn't read the source queue's length yet.
    Waiting,
    Moving,
    /// Running, and the source queue is empty.
    Drained,
    /// `terminated`: the shovel failed (it retries) or can't run; see its reason.
    Failed,
}

impl Phase {
    fn of(x: &Value) -> Phase {
        match (x["state"].as_str(), x["remaining"].as_u64()) {
            (Some("running"), Some(0)) => Phase::Drained,
            (Some("running"), Some(_)) => Phase::Moving,
            (Some("terminated"), _) => Phase::Failed,
            _ => Phase::Waiting,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Phase::Waiting => "waiting",
            Phase::Moving => "moving",
            Phase::Drained => "drained",
            Phase::Failed => "failed",
        }
    }
}

/// Text for the migration progress block, and whether every shovel has drained. Only
/// running shovels that have read the source and found it empty count as drained: a shovel
/// that hasn't read the source yet (`remaining` unknown) or has failed (`terminated`) has not.
fn migration_frame(v: &Value) -> (String, bool) {
    let shovels: Vec<&Value> = list(&v["shovels"]).collect();
    if shovels.is_empty() {
        return ("no migration shovels\n".into(), true);
    }
    let mut t = Table::new(&[
        ("shovel", L),
        ("state", L),
        ("", L),
        ("moved", R),
        ("left", R),
        ("route", L),
    ]);
    let (mut moved, mut left) = (0.0, 0.0);
    let (mut drained, mut failed, mut waiting) = (0, 0, 0);
    let mut reasons = String::new();
    for x in &shovels {
        let phase = Phase::of(x);
        let done = f(&x["transferred"]);
        let rem = x["remaining"].as_f64();
        moved += done;
        left += rem.unwrap_or(0.0);
        match phase {
            Phase::Drained => drained += 1,
            Phase::Failed => failed += 1,
            Phase::Waiting => waiting += 1,
            Phase::Moving => {}
        }
        let frac = match rem {
            Some(r) if done + r > 0.0 => done / (done + r),
            Some(_) => 1.0,
            None => 0.0,
        };
        if let Some(reason) = x["reason"].as_str().filter(|r| !r.is_empty()) {
            reasons.push_str(&format!("{} {}: {reason}\n", phase.label(), s(&x["name"])));
        }
        t.row(vec![
            s(&x["name"]),
            phase.label().into(),
            format!("{} {:>3.0}%", fmt::bar(frac, 20), frac * 100.0),
            count(done),
            rem.map(count).unwrap_or_else(|| "-".into()),
            format!("{} → {}", s(&x["src"]), s(&x["dest"])),
        ]);
    }
    let all_drained = drained == shovels.len();
    let mut out = t.render();
    out.push_str(&reasons);
    let mut notes = Vec::new();
    if failed > 0 {
        notes.push(format!(
            "{failed} failed (retrying; `roll queues migrate --stop` to give up)"
        ));
    }
    if waiting > 0 {
        notes.push(format!("{waiting} waiting to read the source"));
    }
    if all_drained {
        notes.push("drained; point clients at roll queues".into());
    }
    let notes = if notes.is_empty() {
        String::new()
    } else {
        format!(" — {}", notes.join(", "))
    };
    out.push_str(&format!(
        "{} moved, {} left{notes}\n",
        count(moved),
        count(left)
    ));
    (out, all_drained)
}

/// What an import skipped or changed (`skipped` from the roll API), one line each.
fn skipped_text(v: &Value) -> String {
    let items: Vec<&Value> = list(v).collect();
    if items.is_empty() {
        return String::new();
    }
    let partial = items
        .iter()
        .filter(|x| x["partial"].as_bool() == Some(true))
        .count();
    let mut out = format!(
        "skipped {}{}:\n",
        items.len() - partial,
        if partial > 0 {
            format!(", changed {partial}")
        } else {
            String::new()
        }
    );
    for x in items {
        let vhost = x["vhost"]
            .as_str()
            .map(|v| format!(" in {v}"))
            .unwrap_or_default();
        let what = if x["partial"].as_bool() == Some(true) {
            "changed"
        } else {
            "skipped"
        };
        out.push_str(&format!(
            "  {what} {} {}{vhost}: {}\n",
            s(&x["kind"]),
            s(&x["name"]),
            s(&x["reason"])
        ));
    }
    out
}

async fn watch_migration(api: &Api) -> anyhow::Result<()> {
    let tty = std::io::stdout().is_terminal();
    let mut drawn = 0;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let v = tokio::select! {
            _ = &mut ctrl_c => return Ok(()),
            v = api.get("/v1/queues/migrate") => v?,
        };
        let (frame, drained) = migration_frame(&v);
        if tty && drawn > 0 {
            // Move up over the previous frame and clear it.
            print!("\x1b[{drawn}A\x1b[J");
        }
        print!("{frame}");
        drawn = frame.lines().count();
        if drained {
            return Ok(());
        }
        tokio::select! {
            _ = &mut ctrl_c => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(flatten)]
        q: Queues,
    }

    fn parse(args: &[&str]) -> Queues {
        T::try_parse_from(std::iter::once("roll-queues").chain(args.iter().copied()))
            .unwrap()
            .q
    }

    #[test]
    fn vhost_paths_are_encoded() {
        assert_eq!(vpath("/", "/queues"), "/v1/queues/vhosts/%2F/queues");
        assert_eq!(vpath("prod eu", ""), "/v1/queues/vhosts/prod%20eu");
        assert_eq!(enc("a/b.c"), "a%2Fb.c");
    }

    #[test]
    fn declare_args() {
        let q = parse(&[
            "declare",
            "orders",
            "--type",
            "quorum",
            "--durable",
            "--arg",
            "x-max-length=10",
            "--vhost",
            "prod",
        ]);
        assert_eq!(q.vhost.as_deref(), Some("prod"));
        let Some(Q::Declare {
            name,
            kind,
            durable,
            auto_delete,
            args,
        }) = q.cmd
        else {
            panic!()
        };
        assert_eq!(name, "orders");
        assert_eq!(kind.map(QueueType::as_str), Some("quorum"));
        assert!(durable && !auto_delete);
        assert_eq!(args, vec![("x-max-length".to_string(), "10".to_string())]);
    }

    #[test]
    fn publish_args() {
        let q = parse(&[
            "publish",
            "-e",
            "amq.topic",
            "-k",
            "a.b",
            "-H",
            "x=1",
            "-p",
            "content_type=text/plain",
            "@body.json",
        ]);
        let Some(Q::Publish {
            exchange,
            key,
            headers,
            properties,
            body,
        }) = q.cmd
        else {
            panic!()
        };
        assert_eq!(
            (exchange.as_str(), key.as_str(), body.as_str()),
            ("amq.topic", "a.b", "@body.json")
        );
        assert_eq!(headers.len(), 1);
        assert_eq!(properties[0].1, "text/plain");
        assert!(T::try_parse_from(["x", "publish", "--header", "novalue", "hi"]).is_err());
    }

    #[test]
    fn default_is_status_and_subgroups_parse() {
        assert!(parse(&[]).cmd.is_none());
        assert!(matches!(
            parse(&["vhosts"]).cmd,
            Some(Q::Vhosts { action: None })
        ));
        assert!(matches!(
            parse(&["vhosts", "suspend", "/"]).cmd,
            Some(Q::Vhosts {
                action: Some(VhostCmd::Suspend { .. })
            })
        ));
        assert!(matches!(
            parse(&["connections", "close", "7"]).cmd,
            Some(Q::Connections {
                action: Some(ConnCmd::Close { id: 7 })
            })
        ));
        assert!(matches!(
            parse(&["bind", "x", "q", "-k", "k"]).cmd,
            Some(Q::Bind(_))
        ));
        let Some(Q::Policies {
            action:
                Some(PolicyCmd::Set {
                    definition,
                    apply_to,
                    ..
                }),
        }) = parse(&[
            "policies",
            "set",
            "lim",
            "--pattern",
            "^o",
            "--definition",
            r#"{"max-length":5}"#,
        ])
        .cmd
        else {
            panic!()
        };
        assert_eq!(apply_to, "all");
        assert_eq!(definition["max-length"], 5);
        assert!(
            T::try_parse_from([
                "x",
                "policies",
                "set",
                "p",
                "--pattern",
                ".",
                "--definition",
                "[]"
            ])
            .is_err()
        );
        assert!(
            T::try_parse_from([
                "x",
                "policies",
                "set",
                "p",
                "--pattern",
                ".",
                "--apply-to",
                "nope",
                "--definition",
                "{}"
            ])
            .is_err()
        );
    }

    #[test]
    fn migration_progress() {
        let (frame, drained) = migration_frame(&json!({ "shovels": [
            { "name": "orders", "state": "running", "src": "rabbit/orders", "dest": "/orders", "transferred": 750, "remaining": 250 },
        ]}));
        assert!(!drained);
        assert!(frame.contains(" 75%"), "{frame}");
        assert!(frame.contains("750 moved, 250 left"), "{frame}");

        // Terminated is a failure (with its reason), not "finished".
        let (frame, drained) = migration_frame(&json!({ "shovels": [
            { "name": "a", "state": "running", "transferred": 10, "remaining": 0 },
            { "name": "b", "state": "terminated", "reason": "source: connection refused", "transferred": 0, "remaining": null },
        ]}));
        assert!(!drained, "{frame}");
        assert!(
            frame.contains("failed b: source: connection refused"),
            "{frame}"
        );
        assert!(frame.contains("1 failed"), "{frame}");

        // A shovel that hasn't read the source yet has an unknown backlog, not zero.
        let (frame, drained) = migration_frame(&json!({ "shovels": [
            { "name": "a", "state": "running", "transferred": 10, "remaining": 0 },
            { "name": "c", "state": "running", "transferred": 0, "remaining": null },
            { "name": "d", "state": "starting", "transferred": 0 },
        ]}));
        assert!(!drained, "{frame}");
        assert!(frame.contains("2 waiting"), "{frame}");

        let (frame, drained) = migration_frame(&json!({ "shovels": [
            { "name": "a", "state": "running", "transferred": 10, "remaining": 0 },
            { "name": "e", "state": "running", "transferred": 0, "remaining": 0 },
        ]}));
        assert!(drained);
        assert!(frame.contains("drained; point clients"), "{frame}");
        assert!(migration_frame(&json!({ "shovels": [] })).1);
    }

    #[test]
    fn skipped_items() {
        assert_eq!(skipped_text(&json!([])), "");
        let t = skipped_text(&json!([
            { "kind": "policy", "vhost": "/", "name": "ha-all", "reason": "nothing left to import" },
            { "kind": "policy", "vhost": "/", "name": "ttl", "reason": "dropped ha-mode", "partial": true },
        ]));
        assert!(t.starts_with("skipped 1, changed 1:"), "{t}");
        assert!(
            t.contains("  skipped policy ha-all in /: nothing left to import"),
            "{t}"
        );
        assert!(
            t.contains("  changed policy ttl in /: dropped ha-mode"),
            "{t}"
        );
    }

    #[test]
    fn migrate_flags() {
        let t = T::try_parse_from(["t", "migrate", "--stop"]).unwrap();
        assert!(matches!(t.q.cmd, Some(Q::Migrate { stop: true, .. })));
        assert!(T::try_parse_from(["t", "migrate", "--stop", "--from", "http://x"]).is_err());
        assert!(
            T::try_parse_from(["t", "migrate", "--amqp-uri", "amqp://x"]).is_err(),
            "needs --from"
        );
        let t = T::try_parse_from([
            "t",
            "migrate",
            "--from",
            "http://x",
            "--amqp-uri",
            "amqps://x:5671",
            "--include-shovels",
        ])
        .unwrap();
        assert!(matches!(
            t.q.cmd,
            Some(Q::Migrate {
                amqp_uri: Some(_),
                include_shovels: true,
                ..
            })
        ));
    }

    #[test]
    fn request_bodies_drop_unset_fields() {
        let v = clean(
            json!({ "name": "q", "durable": null, "source": { "vhost": null, "user": "u" }, "arguments": {} }),
        );
        assert_eq!(
            v,
            json!({ "name": "q", "source": { "user": "u" }, "arguments": {} })
        );
    }

    #[test]
    fn features_and_summaries() {
        let q = json!({ "durable": true, "exclusive": false, "dead_letter_exchange": "dlx", "is_dlq": false, "policy": "lim" });
        assert_eq!(queue_features(&q), "durable dlx=dlx policy=lim");
        assert_eq!(
            def_summary(&json!({ "queues": [1, 2], "exchanges": [] })),
            "0 exchanges, 2 queues"
        );
        assert_eq!(def_summary(&json!({})), "definitions");
    }
}
