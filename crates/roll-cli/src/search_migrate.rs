//! `roll search migrate`: Elasticsearch, OpenSearch, Algolia, Typesense or Meilisearch into a
//! roll search cluster, through `POST /v1/search/clusters/{id}/migrate/inspect` and `…/migrate`
//! (docs/search/guides/migrate-from-algolia.md). Without `--index` it lists the source's indices;
//! `--dry-run` shows what each picked index becomes and what didn't map; otherwise it starts the
//! copies, and `--wait` follows their tasks to the end (starting copies that waited for a slot).
//! The source's key is read from the environment or stdin, never from the command line, and is
//! sent to roll alone.

use crate::search::{es_request, find};
use crate::{Api, s};
use anyhow::{Context, bail};
use serde_json::{Value, json};

/// What `roll search migrate` takes.
pub struct Args {
    pub cluster: String,
    pub from: String,
    pub host: Option<String>,
    pub app_id: Option<String>,
    pub username: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key_stdin: bool,
    pub indices: Vec<String>,
    pub dry_run: bool,
    pub size: Option<u32>,
    pub rps: Option<f64>,
    pub wait: bool,
}

/// The environment variable a source's key is read from unless `--api-key-env` says.
fn key_var(from: &str) -> &'static str {
    match from {
        "algolia" => "ALGOLIA_API_KEY",
        "typesense" => "TYPESENSE_API_KEY",
        "meilisearch" => "MEILISEARCH_API_KEY",
        _ => "ELASTICSEARCH_API_KEY",
    }
}

/// The request's `source`: the product, where it is, and its key (or, for Elasticsearch, a
/// username with the password in `ELASTICSEARCH_PASSWORD`).
fn source(a: &Args) -> anyhow::Result<Value> {
    let from = a.from.as_str();
    if !["elasticsearch", "opensearch", "algolia", "typesense", "meilisearch"].contains(&from) {
        bail!("--from is elasticsearch, opensearch, algolia, typesense or meilisearch");
    }
    let mut src = json!({ "kind": from });
    match (from, &a.app_id, &a.host) {
        ("algolia", Some(app), None) => src["app_id"] = json!(app),
        ("algolia", _, _) => bail!("Algolia takes --app-id (its application id), not --host"),
        (_, Some(_), _) => bail!("--app-id is for --from algolia"),
        (_, None, Some(h)) => src["host"] = json!(h),
        (_, None, None) => bail!("--host is the source's URL"),
    }
    let key = if a.api_key_stdin {
        let mut k = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut k)?;
        Some(k.trim().to_string()).filter(|k| !k.is_empty())
    } else {
        let var = a.api_key_env.clone().unwrap_or_else(|| key_var(from).to_string());
        std::env::var(&var).ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
    };
    if let Some(u) = &a.username {
        if !matches!(from, "elasticsearch" | "opensearch") {
            bail!("--username is for Elasticsearch and OpenSearch");
        }
        src["username"] = json!(u);
        src["password"] = json!(std::env::var("ELASTICSEARCH_PASSWORD").context("set ELASTICSEARCH_PASSWORD to the user's password")?);
    } else if let Some(k) = key {
        src["api_key"] = json!(k);
    } else if matches!(from, "algolia" | "typesense") {
        bail!("set {} to the source's API key (or use --api-key-env / --api-key-stdin)", a.api_key_env.as_deref().unwrap_or(key_var(from)));
    }
    Ok(src)
}

/// `index` or `index=dest`.
fn picks(indices: &[String]) -> Vec<Value> {
    indices
        .iter()
        .map(|i| match i.split_once('=') {
            Some((index, dest)) => json!({ "index": index, "dest": dest }),
            None => json!({ "index": i }),
        })
        .collect()
}

/// What didn't map, one line each.
fn print_plan(plan: &Value) {
    println!(
        "  {} as _id · {} synonyms · {} rules · search template {}",
        s(&plan["id_field"]),
        plan["synonyms"].as_u64().unwrap_or(0),
        plan["rules"].as_u64().unwrap_or(0),
        s(&plan["search_template"])
    );
    for n in plan["not_mapped"].as_array().into_iter().flatten() {
        println!("  not mapped: {} {}: {}", s(&n["kind"]), s(&n["name"]), s(&n["reason"]));
    }
    if let Some(more) = plan["not_mapped_more"].as_u64().filter(|n| *n > 0) {
        println!("  not mapped: {more} more");
    }
    for note in plan["notes"].as_array().into_iter().flatten() {
        println!("  note: {}", note.as_str().unwrap_or_default());
    }
}

pub async fn run(api: &Api, as_json: bool, a: Args) -> anyhow::Result<()> {
    let c = find(api, &a.cluster).await?;
    let base = format!("/v1/search/clusters/{}", s(&c["id"]));
    let src = source(&a)?;
    if a.indices.is_empty() {
        let v = api.post(&format!("{base}/migrate/inspect"), json!({ "source": src })).await?;
        if as_json {
            println!("{}", serde_json::to_string_pretty(&v)?);
            return Ok(());
        }
        for i in v["indices"].as_array().into_iter().flatten() {
            let replica = i["replica_of"].as_str().map(|p| format!("  (replica of {p})")).unwrap_or_default();
            println!("{:<40}  {:>12} docs  {}{replica}", s(&i["index"]), i["docs"].as_i64().map_or("-".into(), |n| n.to_string()), if i["exists"] == true { "  (exists on roll)" } else { "" });
        }
        return Ok(());
    }
    let mut body = json!({ "source": src, "indices": picks(&a.indices) });
    if let Some(n) = a.size {
        body["size"] = json!(n);
    }
    if let Some(r) = a.rps {
        body["requests_per_second"] = json!(r);
    }
    if a.dry_run {
        body["dry_run"] = json!(true);
    }
    let v = api.post(&format!("{base}/migrate"), body.clone()).await?;
    let mut rows: Vec<Value> = v["indices"].as_array().cloned().unwrap_or_default();
    if !as_json {
        for r in &rows {
            let what = match (&r["task"], &r["error"], r["queued"] == true) {
                (Value::String(t), _, _) => format!("copying (task {t})"),
                (_, Value::String(e), _) => format!("failed: {e}"),
                (_, _, true) => format!("waiting: {}", s(&r["reason"])),
                _ if a.dry_run => "plan".to_string(),
                _ => "?".to_string(),
            };
            println!("{} -> {}: {what}", s(&r["index"]), s(&r["dest"]));
            if r["plan"].is_object() {
                print_plan(&r["plan"]);
            }
        }
    }
    if a.wait && !a.dry_run {
        rows = follow(api, &c, &base, &body, rows, as_json).await?;
    }
    if as_json {
        println!("{}", serde_json::to_string_pretty(&json!({ "indices": rows }))?);
    }
    if rows.iter().any(|r| r["error"].is_string() || r["result"]["error"].is_object() || r["result"]["response"]["failures"].as_array().is_some_and(|f| !f.is_empty())) {
        bail!("some copies failed");
    }
    Ok(())
}

/// Follows each copy's task until it ends, starting copies that waited for a slot as others
/// end. Each row gets the task's final answer as `result`.
async fn follow(api: &Api, c: &Value, base: &str, body: &Value, mut rows: Vec<Value>, as_json: bool) -> anyhow::Result<Vec<Value>> {
    loop {
        let mut running = 0;
        for r in rows.iter_mut().filter(|r| r["task"].is_string() && r["result"].is_null()) {
            let t = es_request(api, c, "GET", &format!("/_tasks/{}", urlencoding::encode(&s(&r["task"]))), None).await?;
            if t["completed"] == true {
                if !as_json {
                    let resp = &t["response"];
                    match t["error"]["reason"].as_str() {
                        Some(e) => println!("{} -> {}: failed: {e}", s(&r["index"]), s(&r["dest"])),
                        None => println!("{} -> {}: done, {} documents", s(&r["index"]), s(&r["dest"]), resp["created"].as_u64().unwrap_or(0) + resp["updated"].as_u64().unwrap_or(0)),
                    }
                }
                r["result"] = t;
            } else {
                running += 1;
            }
        }
        let queued: Vec<Value> = rows.iter().filter(|r| r["queued"] == true).map(|r| json!({ "index": r["index"], "dest": r["dest"] })).collect();
        if running == 0 && queued.is_empty() {
            return Ok(rows);
        }
        if !queued.is_empty() && running < 2 {
            let mut again = body.clone();
            again["indices"] = json!(queued);
            let v = api.post(&format!("{base}/migrate"), again).await?;
            for n in v["indices"].as_array().into_iter().flatten() {
                if let Some(r) = rows.iter_mut().find(|r| r["queued"] == true && r["index"] == n["index"] && r["dest"] == n["dest"]) {
                    *r = n.clone();
                    if !as_json && n["task"].is_string() {
                        println!("{} -> {}: copying (task {})", s(&n["index"]), s(&n["dest"]), s(&n["task"]));
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    }
}
