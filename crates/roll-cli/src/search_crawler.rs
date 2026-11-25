//! `roll search crawler import`: an Elastic Open Crawler YAML config as a `roll_web_crawler`
//! connector (docs/search/crawler.md, "Configuration"). The YAML is read here, so the cluster
//! only ever sees JSON: the simple fields go in the connector's configuration, the rest in its
//! advanced snippet, a domain's `auth` in the sealed `auth_*` fields, and every key roll's
//! crawler doesn't take is listed as dropped (by name, never its value).

use crate::search::{es_request, find};
use crate::{Api, s};
use anyhow::{Context, bail};
use serde_json::{Map, Value, json};

/// Open Crawler's limit keys the advanced snippet takes (roll's `crawler::config::LIMITS`).
const LIMITS: &[&str] = &[
    "max_duration",
    "max_response_size",
    "max_url_length",
    "max_url_segments",
    "max_url_params",
    "max_redirects",
    "max_extracted_links_count",
    "url_queue_size_limit",
    "max_title_size",
    "max_body_size",
    "max_keywords_size",
    "max_description_size",
    "max_indexed_links_count",
    "max_headings_count",
    "max_elastic_tag_size",
];
const DOMAIN_KEYS: &[&str] = &["url", "seed_urls", "sitemap_urls", "exclude_tags", "crawl_rules", "extraction_rulesets"];

/// What an Open Crawler config becomes.
#[derive(Debug)]
pub struct Import {
    pub connector: String,
    pub index: String,
    /// The configuration's values, credentials included.
    pub values: Map<String, Value>,
    pub snippet: Value,
    pub dropped: Vec<String>,
    pub notes: Vec<String>,
}

/// A YAML boolean, YAML 1.1's words included (`yes`, `on`…), which serde_yaml reads as text.
fn yaml_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => match n.as_i64() {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "on" | "1" => Some(true),
            "false" | "no" | "n" | "off" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn origin(url: &str) -> anyhow::Result<String> {
    let u = reqwest::Url::parse(url.trim()).with_context(|| format!("domains[].url [{url}] isn't a URL"))?;
    Ok(match u.port() {
        Some(p) => format!("{}://{}:{p}", u.scheme(), u.host_str().unwrap_or_default()),
        None => format!("{}://{}", u.scheme(), u.host_str().unwrap_or_default()),
    })
}

/// Reads an Open Crawler config (`yaml`) for connector `connector` (else one named after its
/// `output_index`).
pub fn convert(yaml: &str, connector: Option<&str>) -> anyhow::Result<Import> {
    let doc: Value = serde_yaml_ng::from_str(yaml).context("the crawler config isn't YAML")?;
    let Value::Object(top) = doc else { bail!("the crawler config is a mapping of Open Crawler's keys") };
    let mut values = Map::new();
    let mut snippet = Map::new();
    let mut dropped = Vec::new();
    let mut notes = Vec::new();
    let mut output_index: Option<String> = None;
    let mut domains = Vec::new();
    let mut urls: Vec<String> = Vec::new();
    let mut credentialed: Option<String> = None;
    for (k, v) in &top {
        match k.as_str() {
            "domains" => {
                let Value::Array(list) = v else { bail!("[domains] is a list") };
                for (i, d) in list.iter().enumerate() {
                    let Value::Object(d) = d else { bail!("domains[{i}] is a mapping") };
                    let url = d.get("url").and_then(Value::as_str).with_context(|| format!("domains[{i}] has no url"))?;
                    let mut out = Map::new();
                    for (dk, dv) in d {
                        match dk.as_str() {
                            "auth" => {
                                let o = origin(url)?;
                                if let Some(first) = &credentialed {
                                    bail!("domains {first} and {o} both have credentials: a connector holds one, so make a connector per authenticated domain");
                                }
                                let kind = dv.get("type").and_then(Value::as_str).unwrap_or("basic");
                                match kind {
                                    "basic" => {
                                        values.insert("auth_username".into(), json!(s(&dv["username"])));
                                        values.insert("auth_password".into(), json!(s(&dv["password"])));
                                    }
                                    "raw" => {
                                        values.insert("auth_header".into(), json!(s(&dv["header"])));
                                    }
                                    other => bail!("domains[{i}].auth has type [{other}]: basic or raw"),
                                }
                                if !o.starts_with("https://") {
                                    bail!("domains[{i}] has credentials on {o}: roll sends a credential only over https");
                                }
                                values.insert("auth_origin".into(), json!(o));
                                notes.push(format!("domains[{i}].auth moved to the connector's sealed auth_* fields, sent only to {o}"));
                                credentialed = Some(o);
                            }
                            k if DOMAIN_KEYS.contains(&k) => {
                                out.insert(k.into(), dv.clone());
                            }
                            other => dropped.push(format!("domains[{i}].{other}")),
                        }
                    }
                    let seeds: Vec<String> = d.get("seed_urls").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
                    if seeds.is_empty() {
                        urls.push(format!("{}/", origin(url)?));
                    } else {
                        urls.extend(seeds);
                    }
                    domains.push(Value::Object(out));
                }
            }
            "output_index" => output_index = v.as_str().map(str::to_string),
            "max_crawl_depth" | "max_unique_url_count" => {
                values.insert(k.clone(), v.clone());
            }
            "sitemap_discovery_disabled" => {
                let Some(disabled) = yaml_bool(v) else { bail!("[sitemap_discovery_disabled] is true or false") };
                values.insert("sitemap_discovery".into(), json!(!disabled));
            }
            "full_html_extraction_enabled" => {
                let Some(on) = yaml_bool(v) else { bail!("[full_html_extraction_enabled] is true or false") };
                snippet.insert(k.clone(), json!(on));
            }
            k if LIMITS.contains(&k) => {
                snippet.insert(k.into(), v.clone());
            }
            "elasticsearch" => match v {
                Value::Object(m) => dropped.extend(m.keys().map(|k| format!("elasticsearch.{k}"))),
                _ => dropped.push("elasticsearch".into()),
            },
            other => dropped.push(other.to_string()),
        }
    }
    if domains.is_empty() {
        bail!("the crawler config has no domains");
    }
    snippet.insert("domains".into(), Value::Array(domains));
    values.insert("urls".into(), json!(urls.join(",")));
    let base = output_index.clone().unwrap_or_else(|| connector.unwrap_or("crawler").to_string());
    let index = if base.starts_with("content-") {
        base.clone()
    } else {
        let i = format!("content-{base}");
        if output_index.is_some() {
            notes.push(format!("the index is {i}: a connector roll runs writes to an index starting with content-"));
        }
        i
    };
    let connector = connector.map(str::to_string).unwrap_or_else(|| index.trim_start_matches("content-").to_string());
    Ok(Import { connector, index, values, snippet: Value::Object(snippet), dropped, notes })
}

/// Makes (or updates) the connector, writes its configuration and snippet, and activates the
/// snippet once roll has validated it.
pub async fn import(api: &Api, as_json: bool, cluster: &str, file: &str, connector: Option<&str>, preset: Option<&str>) -> anyhow::Result<()> {
    let yaml = std::fs::read_to_string(file).with_context(|| format!("can't read {file}"))?;
    let mut plan = convert(&yaml, connector)?;
    if let Some(p) = preset {
        plan.values.insert("preset".into(), json!(preset_of(p)?));
    }
    let c = find(api, cluster).await?;
    let id = urlencoding::encode(&plan.connector).into_owned();
    es_request(api, &c, "PUT", &format!("/_connector/{id}"), Some(json!({ "index_name": plan.index, "is_native": true, "service_type": "roll_web_crawler", "name": plan.connector }))).await?;
    es_request(api, &c, "PUT", &format!("/_connector/{id}/_configuration"), Some(json!({ "values": plan.values }))).await?;
    es_request(api, &c, "PUT", &format!("/_connector/{id}/_filtering"), Some(json!({ "advanced_snippet": { "value": plan.snippet } }))).await?;
    // roll validates a draft on the cluster's scheduler.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let validation = loop {
        let conn = es_request(api, &c, "GET", &format!("/_connector/{id}"), None).await?;
        let v = conn.pointer("/filtering/0/draft/validation").cloned().unwrap_or(Value::Null);
        if v["state"].as_str() != Some("edited") {
            break v;
        }
        if std::time::Instant::now() > deadline {
            bail!("the cluster didn't validate the crawler's snippet within a minute");
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    };
    if validation["state"].as_str() != Some("valid") {
        let errors: Vec<String> = validation["errors"].as_array().into_iter().flatten().flat_map(|e| e["messages"].as_array().cloned().unwrap_or_default()).map(|m| s(&m)).collect();
        bail!("the crawler's config isn't valid: {}", errors.join("; "));
    }
    es_request(api, &c, "PUT", &format!("/_connector/{id}/_filtering/_activate"), None).await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&json!({ "connector": plan.connector, "index": plan.index, "dropped": plan.dropped, "notes": plan.notes }))?);
        return Ok(());
    }
    println!("connector  {}\nindex      {}", plan.connector, plan.index);
    for n in &plan.notes {
        println!("note       {n}");
    }
    if !plan.dropped.is_empty() {
        println!("dropped    {}", plan.dropped.join(", "));
    }
    println!("crawl it   roll search request {cluster} POST /_connector/_sync_job '{{\"id\":\"{}\",\"job_type\":\"full\"}}'", plan.connector);
    Ok(())
}

/// A `--preset` as the connector takes it.
fn preset_of(p: &str) -> anyhow::Result<&str> {
    match p {
        "docs" => Ok(p),
        _ => bail!("--preset is docs (a documentation site, one record per section), or leave it out"),
    }
}

/// A connector id from a URL's host, as the dashboard's crawl box makes it (`crawlerName`:
/// `docs-example-com`, runs of other characters one `-`, at most 60, else `site`).
fn crawler_id(url: &str) -> String {
    let host = url.split("://").nth(1).unwrap_or(url).split(['/', ':', '?', '#']).next().unwrap_or_default();
    let mut id = String::new();
    for c in host.to_ascii_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c);
        } else if !id.ends_with('-') {
            id.push('-');
        }
    }
    let id: String = id.trim_matches('-').chars().take(60).collect();
    if id.is_empty() { "site".into() } else { id }
}

/// Makes a crawler of `url` (as the dashboard's "crawl a site" box does) and starts its first
/// full crawl.
pub async fn create(api: &Api, as_json: bool, cluster: &str, url: &str, connector: Option<&str>, preset: Option<&str>) -> anyhow::Result<()> {
    let id = connector.map(str::to_string).unwrap_or_else(|| crawler_id(url));
    let index = format!("content-{id}");
    let c = find(api, cluster).await?;
    let path = urlencoding::encode(&id).into_owned();
    // A PUT would replace a connector of that id.
    if es_request(api, &c, "GET", &format!("/_connector/{path}"), None).await.is_ok() {
        bail!("a connector [{id}] exists already: pass --connector with another id");
    }
    let preset = preset.map(preset_of).transpose()?;
    // A crawl never changes an index's mappings: the docs preset's records need an index of
    // their own.
    if preset == Some("docs") && es_request(api, &c, "GET", &format!("/{}", urlencoding::encode(&index)), None).await.is_ok() {
        bail!("the index [{index}] exists already and keeps its mappings (the docs preset's hierarchy.* fields would get dynamic ones): pass --connector with another id, for a new index");
    }
    es_request(api, &c, "PUT", &format!("/_connector/{path}"), Some(json!({ "index_name": index, "is_native": true, "service_type": "roll_web_crawler", "name": id }))).await?;
    let mut values = serde_json::Map::new();
    values.insert("urls".into(), json!(url));
    if let Some(p) = preset {
        values.insert("preset".into(), json!(p));
    }
    es_request(api, &c, "PUT", &format!("/_connector/{path}/_configuration"), Some(json!({ "values": values }))).await?;
    let job = es_request(api, &c, "POST", "/_connector/_sync_job", Some(json!({ "id": id, "job_type": "full", "trigger_method": "on_demand" }))).await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&json!({ "connector": id, "index": index, "job": job["id"] }))?);
        return Ok(());
    }
    println!("connector  {id}\nindex      {index}\ncrawling   job {}", s(&job["id"]));
    if preset == Some("docs") {
        println!("search it  roll search request {cluster} POST /{index}/_search/template '{{\"id\":\"{index}-docs-search\",\"params\":{{\"query\":\"install\"}}}}'");
    }
    Ok(())
}

fn cluster_path(c: &Value, rest: &str) -> String {
    format!("/v1/search/clusters/{}/{rest}", urlencoding::encode(&s(&c["id"])))
}

/// `roll search crawler test`: one URL through the crawler's path.
pub async fn test(api: &Api, as_json: bool, cluster: &str, url: &str, connector: Option<&str>) -> anyhow::Result<()> {
    let c = find(api, cluster).await?;
    let v = api.post(&cluster_path(&c, "crawl-test"), json!({ "url": url, "connector": connector })).await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("url        {}", s(&v["url"]));
    if v["robots"].is_object() {
        println!("robots     {} ({}): {}", s(&v["robots"]["origin"]), s(&v["robots"]["read"]), if v["robots"]["allows"].as_bool() == Some(true) { "allowed" } else { "disallowed" });
    }
    let rule = v["rule"].as_str().map(|r| format!(" (rule: {r})")).unwrap_or_default();
    println!("verdict    {}{rule}", s(&v["verdict"]));
    for h in v["redirects"].as_array().into_iter().flatten() {
        println!("redirect   {} {} -> {} ({})", h["status"], s(&h["from"]), s(&h["to"]), s(&h["verdict"]));
    }
    if let Some(st) = v["status"].as_i64() {
        println!("status     {st} {}", s(&v["content_type"]));
    }
    if let Some(e) = v["error"].as_str() {
        println!("error      {e}");
    }
    if let Some(indexed) = v["indexed"].as_bool() {
        let why = v["reason"].as_str().map(|r| format!(": {r}")).unwrap_or_default();
        println!("indexed    {}{why}", if indexed { "yes" } else { "no" });
    }
    if let Some(d) = v["document"].as_object() {
        for k in ["id", "url", "title", "meta_description"] {
            if let Some(x) = d.get(k).and_then(Value::as_str) {
                println!("  {k:<14} {x}");
            }
        }
        if let Some(b) = d.get("body").and_then(Value::as_str) {
            let short: String = b.chars().take(200).collect();
            println!("  {:<14} {short}{}", "body", if b.chars().count() > 200 { "…" } else { "" });
        }
    }
    let links = v["links"].as_array().cloned().unwrap_or_default();
    if !links.is_empty() {
        let queued = links.iter().filter(|l| l["verdict"].as_str() == Some("allowed")).count();
        println!("links      {} ({queued} would be queued)", links.len());
        for l in links.iter().filter(|l| l["verdict"].as_str() != Some("allowed")).take(20) {
            println!("  skipped  {} ({})", s(&l["url"]), s(&l["verdict"]));
        }
    }
    Ok(())
}

/// `roll search crawler verify`: starts verifying a site (what to publish), or checks it.
pub async fn verify(api: &Api, as_json: bool, cluster: &str, domain: &str, check: bool) -> anyhow::Result<()> {
    let c = find(api, cluster).await?;
    // A URL is one origin, proven by the file it serves; anything else a domain, by DNS.
    let body = match domain.contains("://") {
        true => json!({ "origin": domain }),
        false => json!({ "domain": domain }),
    };
    let mut v = api.post(&cluster_path(&c, "site-verifications"), body).await?;
    if check {
        let site = urlencoding::encode(&s(&v["site"])).into_owned();
        v = api.post(&cluster_path(&c, &format!("site-verifications/{site}/check")), json!({})).await?;
    }
    if as_json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("site       {} ({})\nstatus     {}", s(&v["site"]), s(&v["kind"]), s(&v["status"]));
    if let Some(e) = v["error"].as_str().filter(|_| v["status"] != "verified") {
        println!("error      {e}");
    }
    if v["status"] != "verified" {
        match v["dns"].is_object() {
            true => println!("publish    a DNS TXT record on {}: {} (verifies every origin on the domain)", s(&v["dns"]["name"]), s(&v["dns"]["value"])),
            false => println!("publish    {} holding {}, served by that origin itself (verifies that origin alone)", s(&v["file"]["url"]), s(&v["file"]["content"])),
        }
        println!("then       roll search crawler verify {cluster} {} --check", s(&v["site"]));
    }
    Ok(())
}

/// `roll search crawler sites`: the owner's verifications.
pub async fn sites(api: &Api, as_json: bool, cluster: &str) -> anyhow::Result<()> {
    let c = find(api, cluster).await?;
    let v = api.get(&cluster_path(&c, "site-verifications")).await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    for x in v["verifications"].as_array().into_iter().flatten() {
        println!("{:<40} {:<7} {}", s(&x["site"]), s(&x["kind"]), s(&x["status"]));
    }
    Ok(())
}
