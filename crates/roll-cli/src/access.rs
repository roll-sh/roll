//! `roll token …` and `roll mcp …`: scoped tokens, approvals and the audit log
//! (docs/permissions.md), and connecting MCP clients (docs/mcp.md).

use crate::{Api, ago, s};
use anyhow::{Context, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum Token {
    /// Create a scoped token from a preset, rules of your own, or both. The token is shown once.
    ///
    /// Examples:
    ///   roll token create ci --preset read-only --expires 90d
    ///   roll token create agent --preset project --project prj_x4k9…
    ///   roll token create migrate --product postgres --resource prj_ab… --allow read,write --ask delete
    Create(Box<Create>),
    /// List tokens and OAuth connections, newest first.
    #[command(alias = "ls")]
    List,
    /// Show a token's rules, expiry and last use.
    Show { id: String },
    /// What an agent connected with a token is offered over MCP: its tools (those that change
    /// things marked), prompts, and what the list costs an agent's context.
    Tools { id: String },
    /// Revoke a token or connection: every request with it fails from now on.
    Revoke { id: String },
    /// A new secret for a token, with the same rules; the old one stops working at once.
    Rotate { id: String },
    /// Change a token: its name, expiry, addresses, or (with --preset/--allow/--ask) its rules.
    Edit(Box<Edit>),
    /// Its rules narrowed to the actions and resources its requests used (never wider); `--apply`
    /// makes them its rules.
    ///
    /// Examples:
    ///   roll token tighten tok_…            what it would keep
    ///   roll token tighten tok_… --apply    keep only that
    Tighten {
        id: String,
        /// Days of activity to go by (1-90).
        #[arg(long, default_value_t = 30)]
        days: u32,
        #[arg(long)]
        apply: bool,
    },
    /// Take back what an approval allowed a token for a while (`roll token show` lists them).
    TakeBack { id: String, approval: String },
    /// Which tokens need attention and why (admin over everything, never expiring, unused,
    /// narrower than their rules, roaming), each with its fix.
    Checkup,
    /// Who the token you're using is (`ROLL_TOKEN` or `ROLL_ADMIN_TOKEN`).
    Whoami,
    /// The products, actions and presets the builder offers.
    Presets,
    /// roll's rules for every token: `--max-days 90`, `--require-expiry`, `--no-limit`,
    /// `--max-idle-days 60` (`0` for no idle limit); nothing to show them.
    Policy {
        #[arg(long)]
        max_days: Option<u32>,
        #[arg(long)]
        require_expiry: Option<bool>,
        #[arg(long, conflicts_with = "max_days")]
        no_limit: bool,
        /// Tokens unused this many days stop working (0: no limit).
        #[arg(long)]
        max_idle_days: Option<u32>,
    },
}

#[derive(Args)]
pub struct Create {
    /// A name you'll recognise it by ("ci", "cursor on my laptop").
    name: String,
    #[command(flatten)]
    rules: RuleArgs,
    /// Lifetime: 30d, 12h, 90m, or never (default 30d); from its start with --starts.
    #[arg(long, default_value = "30d")]
    expires: String,
    /// It works only from this far on (2h, 1d): refused until then.
    #[arg(long)]
    starts: Option<String>,
    /// Addresses it may be used from (CIDRs), repeatable.
    #[arg(long = "ip", value_delimiter = ',')]
    ip_allow: Vec<String>,
    /// Requests a minute it may make (per node; over it, 429 with Retry-After).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=100_000))]
    rate: Option<u32>,
    /// What the rules don't allow asks you instead of being refused.
    #[arg(long)]
    ask_otherwise: bool,
}

/// What a token may do, as flags (`create` and `edit`).
#[derive(Args)]
pub struct RuleArgs {
    /// Start from a preset: read-only, agent, project, development, previews, full.
    #[arg(long)]
    preset: Option<String>,
    /// Products a rule of your own covers (default: every product).
    #[arg(long, value_delimiter = ',')]
    product: Vec<String>,
    /// Workspace projects it covers (with --preset project/development, the project it's for).
    #[arg(long, value_delimiter = ',')]
    project: Vec<String>,
    /// Resources it covers, by their product's id.
    #[arg(long, value_delimiter = ',')]
    resource: Vec<String>,
    /// Only resources in these environments.
    #[arg(long = "env", value_delimiter = ',')]
    environments: Vec<String>,
    /// Actions allowed: read, write, delete, reveal, deploy, admin.
    #[arg(long, value_delimiter = ',')]
    allow: Vec<String>,
    /// Actions that wait for your approval each time.
    #[arg(long, value_delimiter = ',')]
    ask: Vec<String>,
}

impl RuleArgs {
    fn given(&self) -> bool {
        self.preset.is_some() || !self.allow.is_empty() || !self.ask.is_empty()
    }
}

#[derive(Args)]
pub struct Edit {
    id: String,
    /// A new name.
    #[arg(long)]
    name: Option<String>,
    /// New rules, built as `create` builds them (replaces every rule).
    #[command(flatten)]
    rules: RuleArgs,
    /// A new lifetime from now: 30d, 12h, 90m, or never.
    #[arg(long)]
    expires: Option<String>,
    /// New addresses it may be used from (replaces the list; `--ip none` clears it).
    #[arg(long = "ip", value_delimiter = ',')]
    ip_allow: Option<Vec<String>>,
    /// Requests a minute it may make (`none` takes the limit off).
    #[arg(long)]
    rate: Option<String>,
    /// Whether what the rules don't allow asks you instead of being refused.
    #[arg(long)]
    ask_otherwise: Option<bool>,
}

#[derive(Subcommand)]
pub enum SinkCmd {
    /// Stream new audit rows to an https URL (signed batches; the secret is shown once).
    Add {
        url: String,
        /// Send every row the log still has, not just new ones.
        #[arg(long)]
        from_start: bool,
    },
    /// Send a failed sink's batch now rather than after its wait.
    Retry { id: String },
    /// Stop streaming to a sink.
    Rm { id: String },
}

#[derive(Subcommand)]
pub enum Mcp {
    /// The config that connects an MCP client to roll's server (OAuth by default).
    Config {
        #[arg(value_enum, default_value = "claude-code")]
        client: Client,
        /// Use this scoped token instead of OAuth.
        #[arg(long)]
        with_token: Option<String>,
    },
    /// Requests waiting for your approval (--all: decided ones too).
    Approvals {
        #[arg(long)]
        all: bool,
    },
    /// Approve a request: it runs.
    Approve {
        id: String,
        /// Also allow the same action on the same resource without asking: 1h, 8h, 1d or always.
        #[arg(long, value_parser = ["1h", "8h", "1d", "always"])]
        remember: Option<String>,
    },
    /// Deny a request.
    Deny {
        id: String,
        /// Told to the agent.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Where approvals are also posted: a Slack or Discord channel through roll code's chat ops
    /// (`--install chi_… --channel C… --approver U…`), `--off`, or nothing to show it.
    Notify {
        #[arg(long, requires_all = ["channel", "approver"])]
        install: Option<String>,
        #[arg(long, requires = "install")]
        channel: Option<String>,
        /// A chat user id (Slack `U…`, a Discord user id) who may decide there; repeat it for more.
        #[arg(long, requires = "install")]
        approver: Vec<String>,
        #[arg(long, conflicts_with_all = ["install", "channel"])]
        off: bool,
    },
    /// Where the audit log streams to (a SIEM): list them, or add, retry or remove one.
    Sinks {
        #[command(subcommand)]
        what: Option<SinkCmd>,
    },
    /// Every request made with a token and every MCP tool call, newest first.
    Audit {
        /// csv or jsonl: the matching events as that, for a spreadsheet or a log tool.
        #[arg(long)]
        format: Option<String>,
        /// Only this token (tok_…).
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        product: Option<String>,
        /// allow, deny or ask.
        #[arg(long)]
        decision: Option<String>,
        /// Who touched this resource (or anything under it: a cluster's indices).
        #[arg(long)]
        resource: Option<String>,
        /// read, write, delete, reveal, deploy or admin.
        #[arg(long)]
        action: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Client {
    ClaudeCode,
    Cursor,
    Vscode,
    ClaudeDesktop,
    Codex,
    Json,
}

/// "30d" → milliseconds from now; "never" → none.
fn expiry(s: &str) -> anyhow::Result<Option<i64>> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;
    Ok(span(s, "--expires")?.map(|ms| now + ms))
}

/// `--starts` and `--expires` as times: the expiry counts from the start.
fn window(starts: Option<&str>, expires: &str) -> anyhow::Result<(Option<i64>, Option<i64>)> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;
    let from = match starts {
        Some(s) => Some(now + span(s, "--starts")?.with_context(|| format!("--starts {s}: a number and d, h or m (2h)"))?),
        None => None,
    };
    Ok((from, span(expires, "--expires")?.map(|ms| from.unwrap_or(now) + ms)))
}

/// "30d" → its milliseconds; "never" → none.
fn span(s: &str, flag: &str) -> anyhow::Result<Option<i64>> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("never") {
        return Ok(None);
    }
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = n.parse().with_context(|| format!("{flag} {s}: a number and d, h or m (30d), or never"))?;
    Ok(Some(match unit {
        "d" => n * 86_400_000,
        "h" => n * 3_600_000,
        "m" => n * 60_000,
        _ => bail!("{flag} {s}: a number and d, h or m (30d), or never"),
    }))
}

/// The rules a `create` asks for: the preset's (its "project" filled in), then a rule of its own.
async fn rules(api: &Api, c: &RuleArgs) -> anyhow::Result<Value> {
    let mut rules = Vec::new();
    let selectors = || {
        let mut sel: Vec<Value> = c.project.iter().map(|p| json!({ "project": p })).collect();
        sel.extend(c.resource.iter().map(|r| json!({ "id": r })));
        if sel.is_empty() { json!("*") } else { Value::Array(sel) }
    };
    if let Some(name) = &c.preset {
        let catalog = api.get("/v1/access/catalog").await?;
        let preset = catalog["presets"].as_array().and_then(|p| p.iter().find(|p| p["id"] == name.as_str())).cloned();
        let Some(preset) = preset else {
            let names: Vec<_> = catalog["presets"].as_array().into_iter().flatten().filter_map(|p| p["id"].as_str()).collect();
            bail!("no preset {name}: one of {}", names.join(", "));
        };
        for mut r in preset["rules"].as_array().cloned().unwrap_or_default() {
            if r["resources"] == "project" {
                r["resources"] = selectors();
            }
            if !c.environments.is_empty() {
                r["environments"] = json!(c.environments);
            }
            rules.push(r);
        }
    }
    if !c.allow.is_empty() || !c.ask.is_empty() {
        let mut actions = serde_json::Map::new();
        for a in &c.allow {
            actions.insert(a.trim().to_string(), json!("allow"));
        }
        for a in &c.ask {
            actions.insert(a.trim().to_string(), json!("ask"));
        }
        let products = if c.product.is_empty() { vec!["*".to_string()] } else { c.product.clone() };
        for p in products {
            let mut r = json!({ "product": p, "resources": selectors(), "actions": actions });
            if !c.environments.is_empty() {
                r["environments"] = json!(c.environments);
            }
            rules.push(r);
        }
    }
    if rules.is_empty() {
        bail!("say what the token may do: --preset read-only (or agent, project, …), or --allow read,write");
    }
    Ok(Value::Array(rules))
}

fn describe_rule(r: &Value) -> String {
    let resources = match &r["resources"] {
        Value::String(s) if s == "*" => "every resource".to_string(),
        Value::Array(a) => a
            .iter()
            .map(|x| match (x.get("project"), x.get("id"), x.get("index")) {
                (Some(p), ..) => format!("project {}", s(p)),
                (None, Some(id), Some(ix)) => format!("{} indices {}", s(id), s(ix)),
                (None, id, _) => id.map(s).unwrap_or_default(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => s(other),
    };
    let actions: Vec<String> = r["actions"].as_object().into_iter().flatten().map(|(a, e)| if e == "ask" { format!("{a} (ask)") } else { a.clone() }).collect();
    let env = r["environments"].as_array().map(|e| format!(" in {}", e.iter().map(s).collect::<Vec<_>>().join(", "))).unwrap_or_default();
    let product = if r["product"] == "*" { "every product".to_string() } else { s(&r["product"]) };
    format!("{product}: {} on {resources}{env}", actions.join(", "))
}

fn when(ms: &Value) -> String {
    match ms.as_i64() {
        Some(t) => ago(t / 1000),
        None => "never".into(),
    }
}

fn print_grant(g: &Value) {
    let state = if !g["revoked_at"].is_null() { "revoked" } else { "active" };
    println!("{}  {}  ({}, {state})", s(&g["id"]), s(&g["name"]), s(&g["kind"]));
    if let Some(c) = g["client"].as_object() {
        println!("  client: {}", s(&c["name"]).as_str());
    }
    for r in g["rules"].as_array().into_iter().flatten() {
        println!("  {}", describe_rule(r));
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    for t in g["temporary"].as_array().into_iter().flatten().filter(|t| t["until"].as_i64().is_some_and(|u| u > now)) {
        let by = t["approval"].as_str().map(|a| format!(", approved in {a}")).unwrap_or_default();
        println!("  for a while: {} until {}{by}", describe_rule(&t["rule"]), crate::access::date(t["until"].as_i64().unwrap_or(0)));
    }
    let expires = match g["expires_at"].as_i64() {
        Some(t) => format!("expires {}", crate::access::date(t)),
        None => "never expires".into(),
    };
    let ip = g["ip_allow"].as_array().filter(|a| !a.is_empty()).map(|a| format!(", from {}", a.iter().map(s).collect::<Vec<_>>().join(", "))).unwrap_or_default();
    let rate = g["rate_per_minute"].as_u64().map(|r| format!(", {r} requests a minute")).unwrap_or_default();
    println!("  {expires}{ip}{rate}; last used {}", when(&g["last_used_at"]));
}

/// A unix-ms time as a UTC date.
pub fn date(ms: i64) -> String {
    let secs = ms / 1000;
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

pub async fn token(api: &Api, json_out: bool, cmd: Token) -> anyhow::Result<()> {
    match cmd {
        Token::Create(c) => {
            let (not_before, expires_at) = window(c.starts.as_deref(), &c.expires)?;
            let body = json!({ "name": c.name, "rules": rules(api, &c.rules).await?, "not_before": not_before, "expires_at": expires_at, "ip_allow": c.ip_allow, "rate_per_minute": c.rate, "ask_otherwise": c.ask_otherwise });
            let made = api.post("/v1/access/tokens", body).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&made)?);
                return Ok(());
            }
            print_grant(&made["grant"]);
            println!("\n{}\n", s(&made["token"]));
            eprintln!("Copy it now: it isn't shown again. Use it as ROLL_TOKEN, or as Authorization: Bearer for the API and MCP.");
        }
        Token::List => {
            let list = api.get("/v1/access/tokens").await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            let list = list.as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("no tokens yet: roll token create <name> --preset read-only");
            }
            for g in &list {
                print_grant(g);
            }
        }
        Token::Show { id } => {
            let g = api.get(&format!("/v1/access/tokens/{id}")).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&g)?);
            } else {
                print_grant(&g);
            }
        }
        Token::Revoke { id } => {
            let g = api.call(reqwest::Method::DELETE, &format!("/v1/access/tokens/{id}"), None).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&g)?);
            } else {
                println!("revoked {} ({})", s(&g["id"]), s(&g["name"]));
            }
        }
        Token::Tools { id } => {
            let seen = api.get(&format!("/v1/access/tokens/{}/tools", urlencoding::encode(&id))).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&seen)?);
                return Ok(());
            }
            let tools = seen["tools"].as_array().cloned().unwrap_or_default();
            let changes = tools.iter().filter(|t| t["read_only"] != true).count();
            println!("{} tools ({changes} that change things), ~{} tokens of an agent's context", tools.len(), seen["context_tokens"].as_u64().unwrap_or(0));
            for t in &tools {
                let mark = if t["destructive"] == true { "!" } else if t["read_only"] == true { " " } else { "*" };
                println!("  {mark} {}", s(&t["name"]));
            }
            let prompts: Vec<String> = seen["prompts"].as_array().into_iter().flatten().map(|p| s(&p["name"])).collect();
            if !prompts.is_empty() {
                println!("prompts: {}", prompts.join(", "));
            }
        }
        Token::Tighten { id, days, apply } => {
            let t = api.get(&format!("/v1/access/tokens/{id}/tighten?days={days}")).await?;
            let days = t["days"].as_i64().unwrap_or(i64::from(days));
            if apply {
                // Applied over the rules it was made from; the grant as it is after.
                let g = api.post(&format!("/v1/access/tokens/{id}/tighten"), json!({ "from": t["from"], "days": days })).await?;
                if json_out {
                    println!("{}", serde_json::to_string_pretty(&g)?);
                } else if t["narrower"] == true {
                    print_grant(&g);
                } else {
                    println!("nothing to narrow: its rules are as they were");
                }
                return Ok(());
            }
            if json_out {
                println!("{}", serde_json::to_string_pretty(&t)?);
                return Ok(());
            }
            let n: i64 = t["used"].as_array().into_iter().flatten().filter_map(|u| u["count"].as_i64()).sum();
            if t["narrower"] != true {
                println!("{}", if n == 0 { "it made no requests in that time: revoke it if nothing needs it" } else { "it used everything its rules allow: nothing to narrow" });
                return Ok(());
            }
            println!("from {n} requests over {days} days, it would only be able to:");
            for r in t["rules"].as_array().into_iter().flatten() {
                println!("  {}", describe_rule(r));
            }
            eprintln!("roll token tighten {id} --apply to keep only that");
        }
        Token::Checkup => {
            let list = api.get("/v1/access/checkup").await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            let list = list.as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("nothing needs attention");
            }
            for c in &list {
                println!("{}  {}", s(&c["grant"]["id"]), s(&c["grant"]["name"]));
                for f in c["findings"].as_array().into_iter().flatten() {
                    let fix = match f["fix"].as_str().unwrap_or("") {
                        "revoke" => format!("roll token revoke {}", s(&c["grant"]["id"])),
                        "tighten" => format!("roll token tighten {} --apply", s(&c["grant"]["id"])),
                        "expire" => format!("roll token edit {} --expires 30d", s(&c["grant"]["id"])),
                        "limit_addresses" => format!("roll token edit {} --ip <cidr>", s(&c["grant"]["id"])),
                        other => other.to_string(),
                    };
                    println!("  {}: {fix}", s(&f["why"]));
                }
            }
        }
        Token::TakeBack { id, approval } => {
            let g = api.call(reqwest::Method::DELETE, &format!("/v1/access/tokens/{id}/temporary/{approval}"), None).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&g)?);
            } else {
                print_grant(&g);
            }
        }
        Token::Rotate { id } => {
            let made = api.post(&format!("/v1/access/tokens/{id}/rotate"), json!({})).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&made)?);
                return Ok(());
            }
            println!("{}", s(&made["token"]));
            eprintln!("The old secret stopped working. Copy this one now: it isn't shown again.");
        }
        Token::Edit(e) => {
            let mut patch = serde_json::Map::new();
            if let Some(n) = &e.name {
                patch.insert("name".into(), json!(n));
            }
            if e.rules.given() {
                patch.insert("rules".into(), rules(api, &e.rules).await?);
            }
            if let Some(x) = &e.expires {
                patch.insert("expires_at".into(), json!(expiry(x)?));
            }
            if let Some(ips) = &e.ip_allow {
                let ips: Vec<&String> = ips.iter().filter(|i| i.as_str() != "none").collect();
                patch.insert("ip_allow".into(), json!(ips));
            }
            if let Some(r) = &e.rate {
                let rate = match r.as_str() {
                    "none" => Value::Null,
                    n => json!(n.parse::<u32>().map_err(|_| anyhow::anyhow!("--rate is a number of requests a minute, or none"))?),
                };
                patch.insert("rate_per_minute".into(), rate);
            }
            if let Some(a) = e.ask_otherwise {
                patch.insert("ask_otherwise".into(), json!(a));
            }
            if patch.is_empty() {
                bail!("nothing to change: --name, --expires, --ip, --rate, --ask-otherwise, or new rules (--preset, --allow, --ask)");
            }
            let g = api.call(reqwest::Method::PATCH, &format!("/v1/access/tokens/{}", e.id), Some(Value::Object(patch))).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&g)?);
            } else {
                print_grant(&g);
            }
        }
        Token::Whoami => {
            let me = api.get("/v1/access").await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&me)?);
            } else if me["kind"] == "operator" {
                println!("the operator: everything");
            } else {
                print_grant(&me["grant"]);
            }
        }
        Token::Policy { max_days, require_expiry, no_limit, max_idle_days } => {
            let mut p = api.get("/v1/access/policy").await?;
            if let Some(d) = max_idle_days {
                p["max_idle_days"] = if d == 0 { Value::Null } else { json!(d) };
            }
            if max_days.is_some() || require_expiry.is_some() || no_limit || max_idle_days.is_some() {
                if let Some(d) = max_days {
                    p["max_days"] = json!(d);
                }
                if no_limit {
                    p["max_days"] = Value::Null;
                }
                if let Some(r) = require_expiry {
                    p["require_expiry"] = json!(r);
                }
                p = api.call(reqwest::Method::PUT, "/v1/access/policy", Some(p)).await?;
            }
            if json_out {
                println!("{}", serde_json::to_string_pretty(&p)?);
            } else {
                let days = p["max_days"].as_u64().map_or_else(|| "no limit".to_string(), |d| format!("at most {d} days"));
                let must = if p["require_expiry"].as_bool() == Some(true) { ", and must expire" } else { "" };
                let idle = p["max_idle_days"].as_u64().map(|d| format!("; unused for {d} days, they stop")).unwrap_or_default();
                println!("tokens live {days}{must}{idle}");
            }
        }
        Token::Presets => {
            let c = api.get("/v1/access/catalog").await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&c["presets"])?);
                return Ok(());
            }
            for p in c["presets"].as_array().into_iter().flatten() {
                println!("{:<12} {}\n             {}", s(&p["id"]), s(&p["name"]), s(&p["description"]));
            }
            println!("\nproducts: {}", c["products"].as_array().into_iter().flatten().map(s).collect::<Vec<_>>().join(", "));
            println!("actions:  {}", c["actions"].as_array().into_iter().flatten().map(s).collect::<Vec<_>>().join(", "));
        }
    }
    Ok(())
}

/// The MCP server's URL for an API base.
fn server(api: &str) -> String {
    format!("{}/v1/mcp", api.trim_end_matches('/'))
}

/// The config for one client: OAuth by default, a bearer token header with one.
pub fn config(client: Client, url: &str, token: Option<&str>) -> String {
    let header = token.map(|t| format!("Bearer {t}"));
    match client {
        Client::ClaudeCode => match &header {
            Some(h) => format!("claude mcp add --transport http roll {url} --header \"Authorization: {h}\""),
            None => format!("claude mcp add --transport http roll {url}"),
        },
        Client::Codex => match &header {
            Some(_) => format!("# ~/.codex/config.toml\n[mcp_servers.roll]\nurl = \"{url}\"\nbearer_token_env_var = \"ROLL_TOKEN\""),
            None => format!("# ~/.codex/config.toml (then: codex mcp login roll)\n[mcp_servers.roll]\nurl = \"{url}\""),
        },
        Client::Vscode => {
            let mut s = json!({ "type": "http", "url": url });
            if let Some(h) = &header {
                s["headers"] = json!({ "Authorization": h });
            }
            format!("// .vscode/mcp.json\n{}", serde_json::to_string_pretty(&json!({ "servers": { "roll": s } })).unwrap_or_default())
        }
        Client::Cursor | Client::ClaudeDesktop | Client::Json => {
            let mut s = json!({ "url": url });
            if let Some(h) = &header {
                s["headers"] = json!({ "Authorization": h });
            }
            let file = match client {
                Client::Cursor => "// ~/.cursor/mcp.json\n",
                Client::ClaudeDesktop => "// Claude Desktop: Settings → Connectors → Add custom connector, or claude_desktop_config.json\n",
                _ => "",
            };
            format!("{file}{}", serde_json::to_string_pretty(&json!({ "mcpServers": { "roll": s } })).unwrap_or_default())
        }
    }
}

fn print_approval(a: &Value) {
    let tool = a["tool"].as_str().map(|t| format!(" via {t}")).unwrap_or_default();
    println!("{}  {}  {}{tool}  ({}, {})", s(&a["id"]), s(&a["state"]), s(&a["summary"]), s(&a["grant_name"]), when(&a["created_at"]));
    if !a["args"].is_null() {
        println!("  {}", a["args"]);
    }
}

pub async fn mcp(api: &Api, json_out: bool, cmd: Mcp) -> anyhow::Result<()> {
    match cmd {
        Mcp::Config { client, with_token } => println!("{}", config(client, &server(&api.base), with_token.as_deref())),
        Mcp::Approvals { all } => {
            let path = if all { "/v1/access/approvals" } else { "/v1/access/approvals?state=pending" };
            let list = api.get(path).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            let list = list.as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("nothing waits for you");
            }
            for a in &list {
                print_approval(a);
            }
        }
        Mcp::Approve { id, remember } => {
            let body = remember.map_or_else(|| json!({}), |r| json!({ "remember": r }));
            let a = api.post(&format!("/v1/access/approvals/{id}/approve"), body).await?;
            if json_out { println!("{}", serde_json::to_string_pretty(&a)?) } else { println!("approved: {}", s(&a["summary"])) }
        }
        Mcp::Deny { id, reason } => {
            let a = api.post(&format!("/v1/access/approvals/{id}/deny"), json!({ "reason": reason })).await?;
            if json_out { println!("{}", serde_json::to_string_pretty(&a)?) } else { println!("denied: {}", s(&a["summary"])) }
        }
        Mcp::Sinks { what } => {
            let out = match what {
                Some(SinkCmd::Add { url, from_start }) => api.post("/v1/access/audit/sinks", json!({ "url": url, "from_start": from_start })).await?,
                Some(SinkCmd::Retry { id }) => api.post(&format!("/v1/access/audit/sinks/{id}/retry"), json!({})).await?,
                Some(SinkCmd::Rm { id }) => api.call(reqwest::Method::DELETE, &format!("/v1/access/audit/sinks/{id}"), None).await?,
                None => api.get("/v1/access/audit/sinks").await?,
            };
            if json_out {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if let Some(secret) = out["secret"].as_str() {
                println!("streaming the audit log to {} as {}", s(&out["sink"]["url"]), s(&out["sink"]["id"]));
                println!("signing secret (shown once): {secret}");
            } else if let Some(list) = out.as_array() {
                if list.is_empty() {
                    println!("the audit log isn't streamed anywhere (roll mcp sinks add https://…)");
                }
                for k in list {
                    let err = k["error"].as_str().map(|e| format!(" ({e})")).unwrap_or_default();
                    println!("{}  {:<6} {}  sent {}{err}", s(&k["id"]), s(&k["status"]), s(&k["url"]), when(&k["delivered_at"]));
                }
            } else {
                println!("done");
            }
        }
        Mcp::Notify { install, channel, approver, off } => {
            let v = match (install, channel, off) {
                (Some(i), Some(c), _) => {
                    let chat = json!({ "install": i, "channel": c.trim_start_matches('#'), "approvers": approver });
                    api.call(reqwest::Method::PUT, "/v1/access/notify", Some(json!({ "chat": chat }))).await?
                }
                (_, _, true) => api.call(reqwest::Method::PUT, "/v1/access/notify", Some(json!({ "chat": null }))).await?,
                _ => api.get("/v1/access/notify").await?,
            };
            if json_out {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else if v["chat"].is_null() {
                println!("approvals go only to the dashboard and `roll mcp approvals`");
            } else {
                let who: Vec<&str> = v["chat"]["approvers"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
                println!("approvals are also posted to {} in {}; decided there by {}", s(&v["chat"]["channel"]), s(&v["chat"]["install"]), who.join(", "));
            }
        }
        Mcp::Audit { format, token, product, decision, resource, action, limit } => {
            let mut q = url::form_urlencoded::Serializer::new(String::new());
            q.append_pair("limit", &limit.to_string());
            for (k, v) in [("grant", token), ("product", product), ("decision", decision), ("resource", resource), ("action", action), ("format", format.clone())] {
                if let Some(v) = v {
                    q.append_pair(k, &v);
                }
            }
            if format.is_some() {
                print!("{}", api.get_text(&format!("/v1/access/audit?{}", q.finish())).await?);
                return Ok(());
            }
            let log = api.get(&format!("/v1/access/audit?{}", q.finish())).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&log)?);
                return Ok(());
            }
            for e in log.as_array().into_iter().flatten() {
                let who = e["grant_name"].as_str().unwrap_or("operator");
                let what = e["tool"].as_str().map(str::to_string).unwrap_or_else(|| format!("{} {}", s(&e["method"]), s(&e["route"])));
                let res = e["resource"].as_str().map(|r| format!(" {r}")).unwrap_or_default();
                println!("{:<8} {:<5} {}  {who}: {what}{res}  ({})", when(&e["at"]), s(&e["decision"]), s(&e["status"]), s(&e["action"]));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_and_dates() {
        assert!(expiry("never").unwrap().is_none());
        assert!(expiry("30d").unwrap().unwrap() > 0 && expiry("1y").is_err());
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_794_509_022_828), "2026-11-12");
        assert!(config(Client::ClaudeCode, "https://api.roll.sh/v1/mcp", Some("rlt_x")).contains("Authorization: Bearer rlt_x"));
    }
}
