//! `roll search chat`: answers from a cluster's chat profiles (docs/search/rag.md), with their
//! sources. `roll search chat <cluster> <profile> "question"` streams the answer as the model
//! writes it, then lists the sources (cited ones marked); `--json` prints the answer object.
//! `roll search chat profiles create|list|delete` manages the profiles through the console route.
//!
//! An answer is text: nothing in it is followed or loaded. A source's link is its own `url`
//! field, printed as data, never one the model wrote.

use crate::search::{es_request, find};
use crate::{Api, s};
use anyhow::{Context, bail};
use clap::Subcommand;
use serde_json::{Value, json};

#[derive(clap::Args)]
pub struct Chat {
    #[command(subcommand)]
    pub cmd: Option<ChatCmd>,
    /// The cluster.
    pub cluster: Option<String>,
    /// The chat profile.
    pub profile: Option<String>,
    /// The question.
    pub question: Option<String>,
    /// Search application params, as `key=value` (a profile over a search application).
    #[arg(long = "param")]
    pub params: Vec<String>,
}

#[derive(Subcommand)]
pub enum ChatCmd {
    /// The cluster's chat profiles.
    #[command(subcommand)]
    Profiles(Profiles),
}

#[derive(Subcommand)]
pub enum Profiles {
    /// Every profile of the cluster.
    #[command(alias = "ls")]
    List { cluster: String },
    /// A profile that answers from an index (`--index`: roll searches its text and
    /// `semantic_text` fields) or a search application (`--application`: its template), with a
    /// `completion` or `chat_completion` endpoint.
    Create {
        cluster: String,
        name: String,
        #[arg(long, required_unless_present = "application", conflicts_with = "application")]
        index: Option<String>,
        #[arg(long)]
        application: Option<String>,
        /// The completion endpoint (`roll search inference create … --task chat_completion`).
        #[arg(long)]
        inference_id: String,
        /// What the model is told besides roll's rules (tone, product, language).
        #[arg(long)]
        instructions: Option<String>,
        /// `_source` fields shown with each source (title, url); repeat for several.
        #[arg(long = "field")]
        fields: Vec<String>,
        /// Fields whose passages the model reads (default: what the query searches).
        #[arg(long = "passage")]
        passages: Vec<String>,
        #[arg(long)]
        max_sources: Option<u64>,
        #[arg(long)]
        max_output_tokens: Option<u64>,
        #[arg(long)]
        max_tokens_per_day: Option<u64>,
    },
    /// Delete a profile.
    #[command(alias = "rm")]
    Delete { cluster: String, name: String },
}

fn params(pairs: &[String]) -> anyhow::Result<Value> {
    let mut m = serde_json::Map::new();
    for p in pairs {
        let (k, v) = p.split_once('=').with_context(|| format!("--param {p}: give key=value"))?;
        m.insert(k.to_string(), serde_json::from_str(v).unwrap_or_else(|_| json!(v)));
    }
    Ok(Value::Object(m))
}

pub async fn run(api: &Api, as_json: bool, chat: Chat) -> anyhow::Result<()> {
    if let Some(ChatCmd::Profiles(p)) = chat.cmd {
        return profiles(api, as_json, p).await;
    }
    let (Some(cluster), Some(profile), Some(question)) = (chat.cluster, chat.profile, chat.question) else {
        bail!("give a cluster, a profile and a question: roll search chat <cluster> <profile> \"question\"");
    };
    let c = find(api, &cluster).await?;
    let path = format!("/v1/search/clusters/{}/chat/{}", urlencoding::encode(&s(&c["id"])), urlencoding::encode(&profile));
    let mut body = json!({ "question": question });
    if !chat.params.is_empty() {
        body["params"] = params(&chat.params)?;
    }
    if as_json {
        let v = api.post(&path, body).await?;
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    body["stream"] = json!(true);
    let mut res = api.open_post_stream(&path, &body).await?;
    let (mut buf, mut sources, mut done) = (String::new(), Value::Null, Value::Null);
    use std::io::Write;
    let mut out = std::io::stdout();
    while let Some(piece) = res.chunk().await? {
        buf.push_str(&String::from_utf8_lossy(&piece));
        while let Some(end) = buf.find("\n\n") {
            let block: String = buf.drain(..end + 2).collect();
            let event = block.lines().find_map(|l| l.strip_prefix("event: ")).unwrap_or_default().to_string();
            let data: Value = block.lines().find_map(|l| l.strip_prefix("data: ")).and_then(|d| serde_json::from_str(d).ok()).unwrap_or(Value::Null);
            match event.as_str() {
                "sources" => sources = data["sources"].clone(),
                "delta" => {
                    // The answer as text: markdown's characters arrive as entities (rag.md,
                    // "Answers leave as text"), shown here as themselves; control characters
                    // other than newlines are dropped, so a model can't move the terminal's
                    // cursor or retitle it.
                    let text: String = plain(data["text"].as_str().unwrap_or_default()).chars().filter(|c| !c.is_control() || *c == '\n').collect();
                    print!("{text}");
                    out.flush()?;
                }
                "done" => done = data,
                "error" => bail!("{}", data.pointer("/error/reason").and_then(Value::as_str).unwrap_or("the answer failed")),
                _ => {}
            }
        }
    }
    println!();
    if done["finish_reason"] == "no_sources" {
        println!("(no sources matched: the model wasn't asked)");
        return Ok(());
    }
    let cited: Vec<u64> = done["cited"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
    println!();
    for src in sources.as_array().into_iter().flatten() {
        let n = src["n"].as_u64().unwrap_or(0);
        let mark = if cited.contains(&n) { "*" } else { " " };
        let title = src["fields"]["title"].as_str().map(clean).unwrap_or_default();
        let url = src["fields"]["url"].as_str().map(clean).map(|u| format!("  {u}")).unwrap_or_default();
        println!("{mark}[{n}] {}/{}  {title}{url}", clean(&s(&src["_index"])), clean(&s(&src["_id"])));
    }
    if let Some(o) = done["omitted"].as_array().filter(|o| !o.is_empty()) {
        println!("({} more matched but didn't fit the context)", o.len());
    }
    if done["finish_reason"] != "stop" {
        println!("(finished: {})", s(&done["finish_reason"]));
    }
    Ok(())
}

/// Text from a document, for a terminal: control characters dropped.
fn clean(t: &str) -> String {
    t.chars().filter(|c| !c.is_control()).collect()
}

async fn profiles(api: &Api, as_json: bool, cmd: Profiles) -> anyhow::Result<()> {
    match cmd {
        Profiles::List { cluster } => {
            let c = find(api, &cluster).await?;
            let v = es_request(api, &c, "GET", "/_roll/chat", None).await?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            for p in v["profiles"].as_array().into_iter().flatten() {
                let from = p["index"].as_str().map(|i| format!("index {i}")).or_else(|| p["search_application"].as_str().map(|a| format!("application {a}"))).unwrap_or_default();
                println!("{:<20} {from:<28} {}", s(&p["name"]), s(&p["inference_id"]));
            }
        }
        Profiles::Create { cluster, name, index, application, inference_id, instructions, fields, passages, max_sources, max_output_tokens, max_tokens_per_day } => {
            let c = find(api, &cluster).await?;
            let mut body = json!({ "inference_id": inference_id, "context": {} });
            match (index, application) {
                (Some(i), _) => body["index"] = json!(i),
                (_, Some(a)) => body["search_application"] = json!(a),
                _ => bail!("give --index or --application"),
            }
            if let Some(i) = instructions {
                body["instructions"] = json!(i);
            }
            if !fields.is_empty() {
                body["context"]["fields"] = json!(fields);
            }
            if !passages.is_empty() {
                body["context"]["passages"] = json!(passages);
            }
            if let Some(n) = max_sources {
                body["context"]["max_sources"] = json!(n);
            }
            if let Some(n) = max_output_tokens {
                body["max_output_tokens"] = json!(n);
            }
            if let Some(n) = max_tokens_per_day {
                body["max_tokens_per_day"] = json!(n);
            }
            let v = es_request(api, &c, "PUT", &format!("/_roll/chat/{}", urlencoding::encode(&name)), Some(body)).await?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("chat profile {name} on {} ({})", s(&c["name"]), s(&v["result"]));
            }
        }
        Profiles::Delete { cluster, name } => {
            let c = find(api, &cluster).await?;
            es_request(api, &c, "DELETE", &format!("/_roll/chat/{}", urlencoding::encode(&name)), None).await?;
            if !as_json {
                println!("deleted chat profile {name}");
            }
        }
    }
    Ok(())
}

/// An answer's text for a terminal: the numeric entities roll writes markdown's characters as
/// (`&#91;` for `[`, exactly those thirteen) decoded; anything else stays as it came.
fn plain(text: &str) -> String {
    const LITERAL: [char; 13] = ['&', '\\', '`', '~', '|', '<', '>', '[', ']', '!', '*', '_', '#'];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("&#") {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let c = rest[2..].find(';').filter(|&e| (1..=3).contains(&e)).and_then(|e| rest[2..2 + e].parse::<u32>().ok().and_then(char::from_u32).map(|c| (c, e)));
        match c {
            Some((c, e)) if LITERAL.contains(&c) => {
                out.push(c);
                rest = &rest[2 + e + 1..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}
