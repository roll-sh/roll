//! `roll storage roles …` and `roll storage sessions …`: roles for temporary credentials and
//! the sessions the control plane mints for them (`/v1/storage/roles`).

use super::{Permission, duration, enc, when};
use crate::Api;
use crate::queues::fmt::{Align::L, Table};
use crate::s;
use anyhow::{Context, bail};
use clap::Subcommand;
use reqwest::Method;
use serde_json::{Map, Value, json};

#[derive(Subcommand)]
pub enum RolesCmd {
    /// Create a role. Who may assume it: --trust-key (AssumeRole), --oidc-issuer with
    /// --audience (AssumeRoleWithWebIdentity), or a whole --trust document.
    Create {
        name: String,
        #[arg(long, value_enum, default_value_t = Permission::Read)]
        permission: Permission,
        /// Limit it to this bucket (repeatable; default: every bucket).
        #[arg(long = "bucket", value_name = "BUCKET")]
        buckets: Vec<String>,
        /// An access key that may assume it (repeatable).
        #[arg(long = "trust-key", value_name = "KEY_ID")]
        trust_keys: Vec<String>,
        /// An OpenID Connect issuer whose ID tokens may assume it (exactly the tokens' `iss`).
        #[arg(long, value_name = "URL")]
        oidc_issuer: Option<String>,
        /// An accepted token audience (client id) for --oidc-issuer (repeatable).
        #[arg(long = "audience", value_name = "CLIENT_ID", requires = "oidc_issuer")]
        audiences: Vec<String>,
        /// A pattern (`*`, `?`) the token's `sub` must match.
        #[arg(long, requires = "oidc_issuer")]
        subject: Option<String>,
        /// Where the issuer's keys are, if not in its discovery document.
        #[arg(long, value_name = "URL", requires = "oidc_issuer")]
        jwks_uri: Option<String>,
        /// AssumeRole must pass this ExternalId.
        #[arg(long)]
        external_id: Option<String>,
        /// The whole trust as JSON (`{"keys":[…],"oidc":[…],"external_id":…}`), or @file.
        #[arg(long, conflicts_with_all = ["trust_keys", "oidc_issuer", "external_id"])]
        trust: Option<String>,
        /// An inline IAM policy limiting its sessions: JSON, or @file.
        #[arg(long)]
        policy: Option<String>,
        /// Longest session (15m to 12h; default 1h).
        #[arg(long, value_name = "DURATION")]
        max_session: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Show a role.
    Show { name: String },
    /// Change a role. Its existing sessions stop working.
    Update {
        name: String,
        #[arg(long, value_enum)]
        permission: Option<Permission>,
        /// Limit it to these buckets (repeatable; replaces the list).
        #[arg(long = "bucket", value_name = "BUCKET")]
        buckets: Vec<String>,
        /// Let it reach every bucket.
        #[arg(long, conflicts_with = "buckets")]
        all_buckets: bool,
        /// Replace the trust (JSON or @file).
        #[arg(long)]
        trust: Option<String>,
        /// Replace the inline policy (JSON or @file).
        #[arg(long)]
        policy: Option<String>,
        /// Remove the inline policy.
        #[arg(long, conflicts_with = "policy")]
        no_policy: bool,
        #[arg(long, value_name = "DURATION")]
        max_session: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Revoke every session of a role issued so far (new ones still work).
    Revoke { name: String },
    /// Delete a role; its sessions stop working at once.
    Delete { name: String },
}

#[derive(Subcommand)]
pub enum SessionsCmd {
    /// Mint temporary credentials of a role and print them as AWS_* variables.
    Create {
        role: String,
        /// How long they last (e.g. 15m, 1h; at most the role's max session).
        #[arg(long, default_value = "1h")]
        duration: String,
        /// Only this bucket (repeatable; within the role's).
        #[arg(long = "bucket", value_name = "BUCKET")]
        buckets: Vec<String>,
        /// Less than the role's permission.
        #[arg(long, value_enum)]
        permission: Option<Permission>,
        /// The session name (in the assumed-role ARN).
        #[arg(long = "name")]
        session_name: Option<String>,
        /// A session policy narrowing it further: JSON, or @file.
        #[arg(long)]
        policy: Option<String>,
        /// A session tag `key=value` (`aws:PrincipalTag/<key>` in policies; repeatable).
        #[arg(long = "tag", value_name = "KEY=VALUE")]
        tags: Vec<String>,
    },
    /// Seal new session tokens with a new key. Tokens of the previous key keep working until
    /// the next rotation.
    RotateKey,
}

/// JSON given inline or as `@file`.
fn json_arg(what: &str, v: &str) -> anyhow::Result<Value> {
    let text = match v.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path).with_context(|| format!("reading {what} from {path}"))?,
        None => v.to_string(),
    };
    serde_json::from_str(&text).with_context(|| format!("{what} isn't valid JSON"))
}

fn role_path(name: &str, rest: &str) -> String {
    format!("/v1/storage/roles/{}{rest}", enc(name))
}

fn list(v: &Value) -> String {
    match v.as_array() {
        Some(a) if !a.is_empty() => a.iter().map(s).collect::<Vec<_>>().join(","),
        Some(_) => "-".into(),
        None => "all".into(),
    }
}

fn trust_summary(t: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(k) = t["keys"].as_array().filter(|k| !k.is_empty()) {
        parts.push(k.iter().map(s).collect::<Vec<_>>().join(","));
    }
    for p in t["oidc"].as_array().into_iter().flatten() {
        parts.push(format!("oidc:{}", s(&p["issuer"])));
    }
    if parts.is_empty() { "control plane only".into() } else { parts.join(" ") }
}

fn print_role(r: &Value) {
    println!("{}  {}", s(&r["name"]), s(&r["arn"]));
    if r["description"].as_str().is_some_and(|d| !d.is_empty()) {
        println!("  description   {}", s(&r["description"]));
    }
    println!("  permission    {}", s(&r["permission"]));
    println!("  buckets       {}", list(&r["buckets"]));
    println!("  trust         {}", trust_summary(&r["trust"]));
    if !r["trust"]["external_id"].is_null() {
        println!("  external id   required");
    }
    println!("  max session   {}s", r["max_session_s"]);
    println!("  policy        {}", if r["policy"].is_null() { "-".to_string() } else { r["policy"].to_string() });
    println!("  created       {}", when(&r["created_at"]));
    if r["revoked_before"].as_i64().is_some_and(|t| t > 0) {
        println!("  revoked until {}", when(&r["revoked_before"]));
    }
}

pub async fn roles(api: &Api, as_json: bool, cmd: Option<RolesCmd>) -> anyhow::Result<Option<Value>> {
    Ok(Some(match cmd {
        None => {
            let v = api.get("/v1/storage/roles").await?;
            if as_json {
                return Ok(Some(v));
            }
            let mut t = Table::new(&[("name", L), ("permission", L), ("buckets", L), ("trusted", L), ("max session", L), ("created", L)]);
            for r in v.as_array().into_iter().flatten() {
                t.row(vec![s(&r["name"]), s(&r["permission"]), list(&r["buckets"]), trust_summary(&r["trust"]), format!("{}s", r["max_session_s"]), when(&r["created_at"])]);
            }
            t.print();
            return Ok(None);
        }
        Some(RolesCmd::Create { name, permission, buckets, trust_keys, oidc_issuer, audiences, subject, jwks_uri, external_id, trust, policy, max_session, description }) => {
            let trust = match trust {
                Some(t) => json_arg("--trust", &t)?,
                None => {
                    let mut oidc = Vec::new();
                    if let Some(issuer) = oidc_issuer {
                        if audiences.is_empty() {
                            bail!("--oidc-issuer needs at least one --audience (the client id its tokens are for)");
                        }
                        oidc.push(json!({ "issuer": issuer, "audiences": audiences, "subject": subject, "jwks_uri": jwks_uri }));
                    }
                    json!({ "keys": trust_keys, "oidc": oidc, "external_id": external_id })
                }
            };
            let mut body = json!({ "name": name, "permission": permission.as_str(), "trust": trust });
            if !buckets.is_empty() {
                body["buckets"] = json!(buckets);
            }
            if let Some(p) = policy {
                body["policy"] = json_arg("--policy", &p)?;
            }
            if let Some(m) = max_session {
                body["max_session_s"] = json!(duration(&m)?);
            }
            if let Some(d) = description {
                body["description"] = json!(d);
            }
            let v = api.post("/v1/storage/roles", body).await?;
            if !as_json {
                println!("created role {}", s(&v["arn"]));
                return Ok(None);
            }
            v
        }
        Some(RolesCmd::Show { name }) => {
            let v = api.get(&role_path(&name, "")).await?;
            if !as_json {
                print_role(&v);
                return Ok(None);
            }
            v
        }
        Some(RolesCmd::Update { name, permission, buckets, all_buckets, trust, policy, no_policy, max_session, description }) => {
            let mut patch = Map::new();
            if let Some(p) = permission {
                patch.insert("permission".into(), json!(p.as_str()));
            }
            if !buckets.is_empty() {
                patch.insert("buckets".into(), json!(buckets));
            }
            if all_buckets {
                patch.insert("buckets".into(), Value::Null);
            }
            if let Some(t) = trust {
                patch.insert("trust".into(), json_arg("--trust", &t)?);
            }
            if let Some(p) = policy {
                patch.insert("policy".into(), json_arg("--policy", &p)?);
            }
            if no_policy {
                patch.insert("policy".into(), Value::Null);
            }
            if let Some(m) = max_session {
                patch.insert("max_session_s".into(), json!(duration(&m)?));
            }
            if let Some(d) = description {
                patch.insert("description".into(), json!(d));
            }
            if patch.is_empty() {
                bail!("nothing to change: give --permission, --bucket, --all-buckets, --trust, --policy, --no-policy, --max-session or --description");
            }
            let v = api.call(Method::PATCH, &role_path(&name, ""), Some(Value::Object(patch))).await?;
            if !as_json {
                println!("updated role {name}; its earlier sessions no longer work");
                return Ok(None);
            }
            v
        }
        Some(RolesCmd::Revoke { name }) => {
            let v = api.post(&role_path(&name, "/revoke"), json!({})).await?;
            if !as_json {
                println!("revoked the sessions of role {name} issued until now");
                return Ok(None);
            }
            v
        }
        Some(RolesCmd::Delete { name }) => {
            let v = api.call(Method::DELETE, &role_path(&name, ""), None).await?;
            if !as_json {
                println!("deleted role {name}");
                return Ok(None);
            }
            v
        }
    }))
}

pub async fn sessions(api: &Api, as_json: bool, cmd: SessionsCmd) -> anyhow::Result<Option<Value>> {
    Ok(Some(match cmd {
        SessionsCmd::Create { role, duration: d, buckets, permission, session_name, policy, tags } => {
            let mut body = json!({ "duration_s": duration(&d)? });
            if !buckets.is_empty() {
                body["buckets"] = json!(buckets);
            }
            if let Some(p) = permission {
                body["permission"] = json!(p.as_str());
            }
            if let Some(n) = session_name {
                body["session_name"] = json!(n);
            }
            if let Some(p) = policy {
                body["policy"] = json_arg("--policy", &p)?;
            }
            if !tags.is_empty() {
                let mut m = Map::new();
                for t in &tags {
                    let (k, v) = t.split_once('=').with_context(|| format!("--tag {t:?}: expected key=value"))?;
                    m.insert(k.to_string(), json!(v));
                }
                body["tags"] = Value::Object(m);
            }
            let v = api.post(&role_path(&role, "/sessions"), body).await?;
            if !as_json {
                // Only the variables on stdout, so `export $(roll storage sessions create …)` works.
                eprintln!("temporary credentials of {} until {} UTC", s(&v["assumed_role_arn"]), when(&v["expiration"]));
                for k in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_ENDPOINT_URL", "AWS_REGION"] {
                    println!("{k}={}", s(&v["env"][k]));
                }
                return Ok(None);
            }
            v
        }
        SessionsCmd::RotateKey => {
            let v = api.post("/v1/storage/session-keys/rotate", json!({})).await?;
            if !as_json {
                println!("new session tokens are sealed with key {}; tokens of the previous key keep working until the next rotation", v["key_id"]);
                return Ok(None);
            }
            v
        }
    }))
}
