//! `roll storage accounts …`: the deployment's accounts (`/v1/storage/accounts`), and which one
//! `roll storage` acts in (`x-roll-account`).
//!
//! `roll storage accounts use <id|alias>` remembers the account in `<config>/roll/storage-account`
//! (`$XDG_CONFIG_HOME`, else `~/.config`); `ROLL_STORAGE_ACCOUNT` overrides it for one run.
//! Without either, commands act in the default account, as before.

use super::enc;
use crate::Api;
use crate::queues::fmt::{Align::L, Table};
use crate::s;
use anyhow::{Context, bail};
use clap::Subcommand;
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum AccountsCmd {
    /// List the accounts (the one `roll storage` acts in is marked).
    List,
    /// Create an account and its root user's access key (the secret is shown once).
    Create {
        name: String,
        #[arg(long)]
        email: Option<String>,
        /// Don't create the root key.
        #[arg(long)]
        no_root_key: bool,
    },
    /// Show an account.
    Show { account: String },
    /// Rename an account or change its email address.
    Update {
        account: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, conflicts_with = "no_email")]
        email: Option<String>,
        /// Remove the email address.
        #[arg(long)]
        no_email: bool,
    },
    /// Delete an account (it must have no buckets): its keys, roles and IAM entities go too.
    Delete { account: String },
    /// Make `roll storage` act in this account (its id or alias); `--default` goes back to the
    /// default account.
    Use {
        #[arg(required_unless_present = "default")]
        account: Option<String>,
        #[arg(long, conflicts_with = "account")]
        default: bool,
    },
}

/// Where the account `roll storage` acts in is remembered.
fn config_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("roll").join("storage-account"))
}

/// The account `roll storage` acts in: `ROLL_STORAGE_ACCOUNT`, else the remembered one.
pub fn current() -> Option<String> {
    if let Ok(v) = std::env::var("ROLL_STORAGE_ACCOUNT") {
        let v = v.trim().to_string();
        return (!v.is_empty()).then_some(v);
    }
    let text = std::fs::read_to_string(config_file()?).ok()?;
    let v = text.trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// An account id from an id or an alias.
async fn resolve(api: &Api, account: &str) -> anyhow::Result<Value> {
    let all = api.get("/v1/storage/accounts").await?;
    all.as_array()
        .into_iter()
        .flatten()
        .find(|a| a["id"].as_str() == Some(account) || a["alias"].as_str() == Some(account) || a["name"].as_str() == Some(account))
        .cloned()
        .with_context(|| format!("no account {account:?} (see `roll storage accounts`)"))
}

fn print_account(a: &Value) {
    println!("{}  {}{}", s(&a["id"]), s(&a["name"]), if a["default"].as_bool() == Some(true) { "  (default)" } else { "" });
    if let Some(al) = a["alias"].as_str() {
        println!("  alias      {al}");
    }
    if let Some(e) = a["email"].as_str() {
        println!("  email      {e}");
    }
    println!("  canonical  {}", s(&a["canonical"]));
}

pub async fn accounts(api: &Api, as_json: bool, cmd: Option<AccountsCmd>) -> anyhow::Result<Option<Value>> {
    let v = match cmd.unwrap_or(AccountsCmd::List) {
        AccountsCmd::List => {
            let v = api.get("/v1/storage/accounts").await?;
            if !as_json {
                let cur = current();
                let mut t = Table::new(&[("", L), ("ID", L), ("NAME", L), ("ALIAS", L), ("EMAIL", L)]);
                for a in v.as_array().into_iter().flatten() {
                    let id = s(&a["id"]);
                    let here = match &cur {
                        Some(c) => *c == id || a["alias"].as_str() == Some(c.as_str()),
                        None => a["default"].as_bool() == Some(true),
                    };
                    let name = if a["default"].as_bool() == Some(true) { format!("{} (default)", s(&a["name"])) } else { s(&a["name"]) };
                    t.row(vec![if here { "*".into() } else { String::new() }, id, name, s(&a["alias"]), s(&a["email"])]);
                }
                t.print();
                return Ok(None);
            }
            v
        }
        AccountsCmd::Create { name, email, no_root_key } => {
            let v = api.post("/v1/storage/accounts", json!({ "name": name, "email": email, "root_key": !no_root_key })).await?;
            if !as_json {
                print_account(&v);
                if let Some(k) = v.get("root_key").filter(|k| k.is_object()) {
                    println!("root key");
                    println!("  access key id      {}", s(&k["id"]));
                    println!("  secret access key  {}", s(&k["secret_access_key"]));
                    eprintln!("the secret is shown only now");
                }
                return Ok(None);
            }
            v
        }
        AccountsCmd::Show { account } => {
            let a = resolve(api, &account).await?;
            let v = api.get(&format!("/v1/storage/accounts/{}", enc(s(&a["id"]).as_str()))).await?;
            if !as_json {
                print_account(&v);
                return Ok(None);
            }
            v
        }
        AccountsCmd::Update { account, name, email, no_email } => {
            let a = resolve(api, &account).await?;
            let mut body = serde_json::Map::new();
            if let Some(n) = name {
                body.insert("name".into(), json!(n));
            }
            if let Some(e) = email {
                body.insert("email".into(), json!(e));
            }
            if no_email {
                body.insert("email".into(), Value::Null);
            }
            if body.is_empty() {
                bail!("nothing to change: give --name, --email or --no-email");
            }
            let v = api.call(reqwest::Method::PATCH, &format!("/v1/storage/accounts/{}", enc(s(&a["id"]).as_str())), Some(Value::Object(body))).await?;
            if !as_json {
                print_account(&v);
                return Ok(None);
            }
            v
        }
        AccountsCmd::Delete { account } => {
            let a = resolve(api, &account).await?;
            let v = api.call(reqwest::Method::DELETE, &format!("/v1/storage/accounts/{}", enc(s(&a["id"]).as_str())), None).await?;
            if !as_json {
                println!("deleted account {}", s(&a["id"]));
                return Ok(None);
            }
            v
        }
        AccountsCmd::Use { account, default } => {
            let file = config_file().context("no config directory: set HOME or XDG_CONFIG_HOME (or use ROLL_STORAGE_ACCOUNT)")?;
            let v = if default {
                let _ = std::fs::remove_file(&file);
                json!({ "account": Value::Null })
            } else {
                let a = resolve(api, account.as_deref().unwrap_or_default()).await?;
                std::fs::create_dir_all(file.parent().expect("a parent"))?;
                std::fs::write(&file, format!("{}\n", s(&a["id"])))?;
                json!({ "account": a["id"] })
            };
            if !as_json {
                match v["account"].as_str() {
                    Some(id) => println!("roll storage now acts in account {id}"),
                    None => println!("roll storage now acts in the default account"),
                }
                return Ok(None);
            }
            v
        }
    };
    Ok(Some(v))
}
