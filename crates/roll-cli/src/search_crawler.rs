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
const DOMAIN_KEYS: &[&str] = &["url", "seed_urls", "sitemap_urls", "exclude_tags", "crawl_rules"];

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
                                values.insert("auth_origin".into(), json!(o));
                                notes.push(format!("domains[{i}].auth moved to the connector's sealed auth_* fields, sent only to {o}"));
                                credentialed = Some(o);
                            }
                            k if DOMAIN_KEYS.contains(&k) => {
                                out.insert(k.into(), dv.clone());
                            }
                            "extraction_rulesets" => {
                                dropped.push(format!("domains[{i}].extraction_rulesets"));
                                notes.push("extraction rules aren't supported by roll's crawler yet".into());
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
            "full_html_extraction_enabled" => {
                snippet.insert(k.clone(), v.clone());
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
pub async fn import(api: &Api, as_json: bool, cluster: &str, file: &str, connector: Option<&str>) -> anyhow::Result<()> {
    let yaml = std::fs::read_to_string(file).with_context(|| format!("can't read {file}"))?;
    let plan = convert(&yaml, connector)?;
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
