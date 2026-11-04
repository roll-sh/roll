//! `roll agent automation …`: the owner's automations (docs/code/automations.md) through
//! `/v1/code/agents/{owner}/automations…`, with an owner token (`--token` or ROLL_CODE_TOKEN).

use anyhow::{Context, bail};
use clap::Subcommand;
use reqwest::Method;
use serde_json::{Value, json};
use std::io::Read;

#[derive(clap::Args)]
pub struct Automation {
    /// Whose automations (an owner's name).
    #[arg(long, env = "ROLL_OWNER")]
    owner: String,
    /// An owner token (rlc_…).
    #[arg(long, env = "ROLL_CODE_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the owner's automations.
    List,
    /// Show one automation.
    Get { automation: String },
    /// Create an automation from a JSON spec (a file, or `-` for stdin). A webhook trigger's
    /// secret is printed once.
    Create { spec: String },
    /// Replace an automation's definition with a JSON spec (a file, or `-`).
    Update {
        automation: String,
        spec: String,
        /// Only over this revision (409 if it changed).
        #[arg(long)]
        revision: Option<u64>,
    },
    /// Delete an automation (its runs going on are cancelled).
    Delete { automation: String },
    Enable { automation: String },
    Disable { automation: String },
    /// Run an automation now.
    Run {
        automation: String,
        /// JSON for `event.data.input`.
        #[arg(long)]
        input: Option<String>,
        /// The same key is the same run.
        #[arg(long)]
        key: Option<String>,
        /// Wait until the run ends and exit 1 unless it succeeded.
        #[arg(long)]
        wait: bool,
    },
    /// List runs, newest first.
    Runs {
        /// Only this automation's.
        #[arg(long)]
        automation: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// A run's steps, their outputs and logs.
    Show { run: String },
    /// Cancel a run.
    Cancel { run: String },
    /// Go on with a failed or cancelled run from the step that stopped it.
    Retry { run: String },
    /// Set one of an automation's secrets (the value from stdin; never shown again).
    SetSecret { automation: String, name: String },
    /// Drop one of an automation's secrets.
    RemoveSecret { automation: String, name: String },
    /// A new inbound webhook secret (printed once; the old one stops working).
    RotateWebhookSecret { automation: String },
    /// Every trigger and step this server has.
    Catalog,
    /// What an automation spec would do with an event, without doing it.
    Test {
        spec: String,
        /// The event: {source, kind, data} as JSON (a file, or `-`).
        event: String,
    },
}

struct Client {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Client {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> anyhow::Result<Value> {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.with_context(|| format!("can't reach {}", self.base))?;
        let status = res.status();
        let text = res.text().await?;
        let v: Value = if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text)) };
        if !status.is_success() {
            match v["error"].as_str() {
                Some(e) => bail!("{e}"),
                None => bail!("HTTP {status}"),
            }
        }
        Ok(v)
    }
}

fn read(src: &str) -> anyhow::Result<Value> {
    let text = if src == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    } else {
        std::fs::read_to_string(src).with_context(|| format!("reading {src}"))?
    };
    serde_json::from_str(&text).with_context(|| format!("{src} isn't JSON"))
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn t(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        v => v.to_string(),
    }
}

fn print_run(r: &Value) {
    println!("{}  {}  {}  {}", t(&r["id"]), t(&r["status"]), t(&r["name"]), t(&r["event"]["summary"]));
    if let Some(why) = r["reason"].as_str() {
        println!("  {why}");
    }
}

pub async fn run(base: &str, json_out: bool, a: Automation) -> anyhow::Result<()> {
    let c = Client { base: base.trim_end_matches('/').to_string(), token: a.token.filter(|t| !t.is_empty()), http: reqwest::Client::new() };
    let root = format!("/v1/code/agents/{}/automations", enc(&a.owner));
    // An automation by id, or by its name.
    let id_of = |given: String| {
        let (c, root) = (&c, &root);
        async move {
            if given.starts_with("aut_") {
                return Ok::<_, anyhow::Error>(given);
            }
            let all = c.call(Method::GET, root, None).await?;
            let found = all.as_array().into_iter().flatten().find(|x| x["name"].as_str() == Some(given.as_str())).and_then(|x| x["id"].as_str().map(str::to_string));
            found.with_context(|| format!("no automation named {given:?}"))
        }
    };
    let out = match a.cmd {
        Cmd::List => {
            let v = c.call(Method::GET, &root, None).await?;
            if !json_out {
                for x in v.as_array().into_iter().flatten() {
                    let on = if x["enabled"].as_bool() == Some(true) { "on " } else { "off" };
                    println!("{}  {on}  {:<10} {}  last: {}", t(&x["id"]), t(&x["trigger"]["type"]), t(&x["name"]), t(&x["last_run"]["status"]));
                }
                return Ok(());
            }
            v
        }
        Cmd::Get { automation } => c.call(Method::GET, &format!("{root}/{}", enc(&id_of(automation).await?)), None).await?,
        Cmd::Create { spec } => {
            let v = c.call(Method::POST, &root, Some(read(&spec)?)).await?;
            if !json_out {
                println!("created {} ({})", t(&v["automation"]["id"]), t(&v["automation"]["name"]));
                if let Some(s) = v["webhook_secret"].as_str() {
                    println!("webhook: {}\nsecret (shown once): {s}", t(&v["automation"]["webhook"]["url"]));
                }
                return Ok(());
            }
            v
        }
        Cmd::Update { automation, spec, revision } => {
            let mut body = read(&spec)?;
            if let (Some(r), Some(o)) = (revision, body.as_object_mut()) {
                o.insert("revision".into(), json!(r));
            }
            c.call(Method::PUT, &format!("{root}/{}", enc(&id_of(automation).await?)), Some(body)).await?
        }
        Cmd::Delete { automation } => {
            c.call(Method::DELETE, &format!("{root}/{}", enc(&id_of(automation).await?)), None).await?;
            json!({ "deleted": true })
        }
        Cmd::Enable { automation } => c.call(Method::POST, &format!("{root}/{}/enable", enc(&id_of(automation).await?)), None).await?,
        Cmd::Disable { automation } => c.call(Method::POST, &format!("{root}/{}/disable", enc(&id_of(automation).await?)), None).await?,
        Cmd::Run { automation, input, key, wait } => {
            let input: Value = match input {
                Some(i) => serde_json::from_str(&i).context("--input isn't JSON")?,
                None => Value::Null,
            };
            let id = id_of(automation).await?;
            let mut r = c.call(Method::POST, &format!("{root}/{}/run", enc(&id)), Some(json!({ "input": input, "key": key }))).await?;
            if wait {
                let path = format!("{root}/runs/{}", enc(&t(&r["id"])));
                while !matches!(r["status"].as_str(), Some("succeeded" | "failed" | "cancelled" | "skipped")) {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    r = c.call(Method::GET, &path, None).await?["run"].clone();
                }
            }
            if !json_out {
                print_run(&r);
            } else {
                println!("{}", serde_json::to_string_pretty(&r)?);
            }
            if wait && r["status"] != "succeeded" {
                std::process::exit(1);
            }
            return Ok(());
        }
        Cmd::Runs { automation, status, limit } => {
            let mut q = format!("?limit={limit}");
            if let Some(a) = automation {
                q.push_str(&format!("&automation={}", enc(&id_of(a).await?)));
            }
            if let Some(s) = status {
                q.push_str(&format!("&status={}", enc(&s)));
            }
            let v = c.call(Method::GET, &format!("{root}/runs{q}"), None).await?;
            if !json_out {
                v.as_array().into_iter().flatten().for_each(print_run);
                return Ok(());
            }
            v
        }
        Cmd::Show { run } => {
            let v = c.call(Method::GET, &format!("{root}/runs/{}", enc(&run)), None).await?;
            if !json_out {
                print_run(&v["run"]);
                for s in v["run"]["steps"].as_array().into_iter().flatten() {
                    println!("  {:<12} {:<10} attempt {}  {}", t(&s["id"]), t(&s["status"]), t(&s["attempt"]), s["error"].as_str().unwrap_or(""));
                    for l in s["log"].as_array().into_iter().flatten() {
                        println!("      {} {}", t(&l["level"]), t(&l["text"]));
                    }
                }
                return Ok(());
            }
            v
        }
        Cmd::Cancel { run } => c.call(Method::POST, &format!("{root}/runs/{}/cancel", enc(&run)), None).await?,
        Cmd::Retry { run } => c.call(Method::POST, &format!("{root}/runs/{}/retry", enc(&run)), None).await?,
        Cmd::SetSecret { automation, name } => {
            let mut value = String::new();
            std::io::stdin().read_to_string(&mut value)?;
            let value = value.trim_end_matches(['\n', '\r']).to_string();
            c.call(Method::PUT, &format!("{root}/{}/secrets/{}", enc(&id_of(automation).await?), enc(&name)), Some(json!({ "value": value }))).await?;
            json!({ "set": name })
        }
        Cmd::RemoveSecret { automation, name } => {
            c.call(Method::DELETE, &format!("{root}/{}/secrets/{}", enc(&id_of(automation).await?), enc(&name)), None).await?;
            json!({ "removed": name })
        }
        Cmd::RotateWebhookSecret { automation } => {
            let v = c.call(Method::POST, &format!("{root}/{}/webhook/secret", enc(&id_of(automation).await?)), None).await?;
            if !json_out {
                println!("secret (shown once): {}", t(&v["webhook_secret"]));
                return Ok(());
            }
            v
        }
        Cmd::Catalog => c.call(Method::GET, &format!("{root}/catalog"), None).await?,
        Cmd::Test { spec, event } => c.call(Method::POST, &format!("{root}/test"), Some(json!({ "automation": read(&spec)?, "event": read(&event)? }))).await?,
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
