//! `roll code reviewer|usage|billing|provider|learnings|search`: roll's own reviewer over the
//! HTTP API (docs/code/http-api.md, "roll's reviewer").

use super::review::settings_patch;
use super::{Client, Spec, clip, enc, pretty, when};
use crate::s;
use anyhow::Context;
use clap::Subcommand;
use reqwest::Method;
use serde_json::{Map, Value, json};

#[derive(Subcommand)]
pub enum ProviderCmd {
    /// Use your own key for the owner's reviews (bring your own key).
    Set {
        owner: String,
        /// anthropic (default) or openai_compatible
        #[arg(long, default_value = "anthropic")]
        provider: String,
        /// The key; read from ROLL_REVIEW_API_KEY when not given.
        #[arg(long, env = "ROLL_REVIEW_API_KEY", hide_env_values = true)]
        api_key: String,
        /// The endpoint (required for openai_compatible).
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        secondary_model: Option<String>,
    },
    /// The owner's provider (the key's last 4 characters only).
    Show { owner: String },
    /// Go back to roll's hosted reviewer.
    Clear { owner: String },
}

#[derive(Subcommand)]
pub enum LearningsCmd {
    /// What the team told roll's reviewer.
    List { repo: String },
    /// Teach it something.
    Add {
        repo: String,
        text: String,
        /// Only for files matching this glob.
        #[arg(long)]
        path: Option<String>,
    },
    /// Forget a learning.
    Rm { repo: String, id: String },
}

/// `roll code reviewer <repo> [--enable|--disable] [--mode] [--speed] [--set k=v]`.
#[allow(clippy::too_many_arguments)]
pub async fn reviewer(c: &Client<'_>, json_out: bool, repo: &str, enable: bool, disable: bool, mode: Option<String>, speed: Option<String>, set: &[String]) -> anyhow::Result<()> {
    let path = format!("{}/reviewer", Spec::parse(repo)?.path());
    let mut patch = settings_patch(set)?;
    if enable {
        patch.insert("enabled".into(), true.into());
    }
    if disable {
        patch.insert("enabled".into(), false.into());
    }
    if let Some(m) = mode {
        patch.insert("mode".into(), m.into());
    }
    if let Some(sp) = speed {
        patch.insert("speed".into(), sp.into());
    }
    let v = if patch.is_empty() { c.get(&path).await? } else { c.patch(&path, Value::Object(patch)).await? };
    if json_out {
        return pretty(&v);
    }
    let e = &v["effective"];
    println!("enabled      {}", s(&e["enabled"]));
    println!("mode         {}", s(&e["mode"]));
    println!("speed        {}", s(&e["speed"]));
    println!("profile      {}", s(&e["profile"]));
    println!("required     {}", s(&e["required"]));
    println!("auto review  {}{}", s(&e["auto_review"]), if e["drafts"] == true { " (drafts too)" } else { "" });
    match v["provider"].as_object() {
        Some(p) => println!("provider     {} · {} ({})", s(&p["provider"].clone()), s(&p["model"].clone()), s(&p["source"].clone())),
        None => println!("provider     none: set one with `roll code provider set <owner>` or ROLL_REVIEW_PROVIDER on the server"),
    }
    let j = &v["jobs"];
    println!("jobs         {} queued, {} running", s(&j["queued"]), s(&j["running"]));
    if let Some(last) = j["last"].as_object() {
        let err = last.get("error").and_then(Value::as_str).map(|e| format!(": {}", clip(e, 100))).unwrap_or_default();
        println!("last         {} {}{err}", s(&last["state"].clone()), when(&last["updated_at"].clone()));
    }
    for e in v["errors"].as_array().into_iter().flatten() {
        println!("warning: {}", s(e));
    }
    Ok(())
}

/// `roll code review <change> --ai [--mode m] [--full]`.
pub async fn ai_review(c: &Client<'_>, json_out: bool, path: &str, mode: Option<String>, full: bool) -> anyhow::Result<()> {
    let mut b = Map::new();
    if let Some(m) = mode {
        b.insert("mode".into(), m.into());
    }
    if full {
        b.insert("full".into(), true.into());
    }
    let v = c.post(&format!("{path}/ai-review"), Value::Object(b)).await?;
    if json_out {
        return pretty(&v);
    }
    let j = &v["job"];
    println!("{} queued: roll will review{}{}", s(&j["id"]), j["mode"].as_str().map(|m| format!(" in {m} mode")).unwrap_or_default(), if j["full"] == true { " (full)" } else { "" });
    Ok(())
}

pub async fn usage(c: &Client<'_>, json_out: bool, owner: &str, month: Option<&str>) -> anyhow::Result<()> {
    let q = month.map(|m| format!("?month={}", enc(m))).unwrap_or_default();
    let v = c.get(&format!("/v1/code/owners/{}/usage{q}", enc(owner))).await?;
    if json_out {
        return pretty(&v);
    }
    let t = &v["totals"];
    println!("{} · {}", s(&v["owner"]), s(&v["month"]));
    println!("reviews      {}  (+{} replies, {} free, {} capped)", s(&t["reviews"]), s(&t["chats"]), s(&t["free_reviews"]), s(&t["capped_reviews"]));
    println!("charged      ${:.2}  (list ${:.2}, provider cost ${:.2})", t["price_usd"].as_f64().unwrap_or(0.0), t["list_usd"].as_f64().unwrap_or(0.0), t["provider_cost_usd"].as_f64().unwrap_or(0.0));
    let tk = &t["tokens"];
    println!("tokens       {} in · {} out · {} cache write · {} cache read", s(&tk["input"]), s(&tk["output"]), s(&tk["cache_write"]), s(&tk["cache_read"]));
    if let Some(cap) = v["cap"]["monthly_cap_usd"].as_f64() {
        println!("cap          ${cap:.2} ({} left, then {})", v["cap"]["remaining_usd"].as_f64().map(|r| format!("${r:.2}")).unwrap_or_default(), s(&v["cap"]["on_cap"]));
    }
    for (m, x) in v["by_mode"].as_object().into_iter().flatten() {
        println!("  {m:<9} {:>5} reviews  ${:.2}", s(&x["reviews"]), x["price_usd"].as_f64().unwrap_or(0.0));
    }
    for r in v["by_repo"].as_array().into_iter().flatten() {
        println!("  {:<30} {:>5}  ${:.2}", s(&r["repo"]), s(&r["count"]), r["price_usd"].as_f64().unwrap_or(0.0));
    }
    Ok(())
}

pub async fn billing(c: &Client<'_>, json_out: bool, owner: &str, cap: Option<String>, on_cap: Option<String>) -> anyhow::Result<()> {
    let path = format!("/v1/code/owners/{}/billing", enc(owner));
    let v = if cap.is_none() && on_cap.is_none() {
        c.get(&path).await?
    } else {
        let mut b = Map::new();
        if let Some(cap) = cap {
            let v = if cap == "none" { Value::Null } else { json!(cap.parse::<f64>().context("--cap is a number of dollars, or none")?) };
            b.insert("monthly_cap_usd".into(), v);
        }
        if let Some(o) = on_cap {
            b.insert("on_cap".into(), o.into());
        }
        c.call(Method::PUT, &path, Some(Value::Object(b))).await?
    };
    if json_out {
        return pretty(&v);
    }
    match v["monthly_cap_usd"].as_f64() {
        Some(cap) => println!("{}: cap ${cap:.2}/month, ${:.2} spent in {}, then {}", s(&v["owner"]), v["spent_usd"].as_f64().unwrap_or(0.0), s(&v["month"]), s(&v["on_cap"])),
        None => println!("{}: no cap, ${:.2} spent in {}", s(&v["owner"]), v["spent_usd"].as_f64().unwrap_or(0.0), s(&v["month"])),
    }
    Ok(())
}

pub async fn provider(c: &Client<'_>, json_out: bool, cmd: ProviderCmd) -> anyhow::Result<()> {
    let path = |o: &str| format!("/v1/code/owners/{}/review-provider", enc(o));
    let v = match cmd {
        ProviderCmd::Set { owner, provider, api_key, base_url, model, secondary_model } => {
            let mut b = Map::new();
            b.insert("provider".into(), provider.into());
            b.insert("api_key".into(), api_key.into());
            for (k, v) in [("base_url", base_url), ("model", model), ("secondary_model", secondary_model)] {
                if let Some(v) = v {
                    b.insert(k.into(), v.into());
                }
            }
            c.call(Method::PUT, &path(&owner), Some(Value::Object(b))).await?
        }
        ProviderCmd::Show { owner } => c.get(&path(&owner)).await?,
        ProviderCmd::Clear { owner } => {
            c.call(Method::DELETE, &path(&owner), None).await?;
            if !json_out {
                println!("{owner}'s reviews use roll's hosted reviewer again");
            }
            return Ok(());
        }
    };
    if json_out {
        return pretty(&v);
    }
    if v["provider"].is_null() {
        println!("{}: roll's hosted reviewer (no key of its own)", s(&v["owner"]));
        return Ok(());
    }
    let model = v["model"].as_str().map(|m| format!(" · {m}")).unwrap_or_default();
    println!("{}: {}{model} · key …{}", s(&v["owner"]), s(&v["provider"]), s(&v["key_last4"]));
    Ok(())
}

pub async fn learnings(c: &Client<'_>, json_out: bool, cmd: LearningsCmd) -> anyhow::Result<()> {
    let path = |r: &str| -> anyhow::Result<String> { Ok(format!("{}/learnings", Spec::parse(r)?.path())) };
    let v = match cmd {
        LearningsCmd::List { repo } => c.get(&path(&repo)?).await?,
        LearningsCmd::Add { repo, text, path: p } => {
            let mut b = json!({ "text": text });
            if let Some(p) = p {
                b["path"] = p.into();
            }
            c.post(&path(&repo)?, b).await?
        }
        LearningsCmd::Rm { repo, id } => {
            c.call(Method::DELETE, &format!("{}/{}", path(&repo)?, enc(&id)), None).await?;
            if !json_out {
                println!("forgot {id}");
            }
            return Ok(());
        }
    };
    if json_out {
        return pretty(&v);
    }
    let list = if v.is_array() { v.as_array().cloned().unwrap_or_default() } else { vec![v] };
    if list.is_empty() {
        println!("no learnings yet: resolve roll's threads with a reason, or say `@roll don't flag …`");
    }
    for l in list {
        let scope = l["path"].as_str().map(|p| format!(" ({p})")).unwrap_or_default();
        println!("{}  {:<9} {}{scope}", s(&l["id"]), s(&l["source"]), clip(&s(&l["text"]), 100));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn search(c: &Client<'_>, json_out: bool, repo: &str, query: &str, regex: bool, case: bool, path: Option<&str>, limit: usize) -> anyhow::Result<()> {
    let mut q = format!("?q={}&limit={limit}", enc(query));
    if regex {
        q.push_str("&regex=true");
    }
    if case {
        q.push_str("&case=true");
    }
    if let Some(p) = path {
        q.push_str(&format!("&path={}", enc(p)));
    }
    // `owner/repo@rev` searches that revision (a change's head, a branch).
    let spec = Spec::parse(repo)?;
    if let Some(rev) = &spec.rev {
        q.push_str(&format!("&rev={}", enc(rev)));
    }
    let v = c.get(&format!("{}/search{q}", spec.path())).await?;
    if json_out {
        return pretty(&v);
    }
    for m in v["matches"].as_array().into_iter().flatten() {
        println!("{}:{}: {}", s(&m["path"]), s(&m["line"]), clip(s(&m["text"]).trim(), 160));
    }
    if v["truncated"] == true {
        eprintln!("(more matches: raise --limit or narrow with --path)");
    }
    Ok(())
}
