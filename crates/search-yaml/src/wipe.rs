//! Cleaning the cluster between tests.
//!
//! [`WipeMode::Full`] follows `ESRestTestCase.cleanUpCluster` step by step: wait for pending
//! cluster updates, stop rollup jobs, delete SLM policies, searchable-snapshot indices, snapshots
//! and repositories, reset feature states (when a system index exists), delete data streams, indices
//! (hidden and closed included, history indices excepted), ES|QL views, composable, component and
//! legacy templates (except the ones Elasticsearch manages), clear persistent and transient
//! cluster settings, and delete ILM policies (except the built-in ones). On top of that it deletes
//! ingest pipelines and stored scripts that aren't managed by the cluster, which the Java runner
//! leaves to each test's teardown.
//!
//! [`WipeMode::Light`] does the same deletions without the waits, the feature-state reset and the
//! x-pack extras; it is what a target with fresh clusters per file (roll) needs between the tests
//! of one file. Every deletion first lists what exists, so an already-clean cluster costs a
//! handful of cheap GETs.

use crate::http::{Client, Response};
use crate::target::TargetInfo;
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WipeMode {
    /// `ESRestTestCase.wipeCluster` plus pipelines and stored scripts.
    Full,
    /// Deletions only.
    Light,
    /// Nothing (a fresh cluster per test, or debugging).
    None,
}

impl WipeMode {
    pub fn parse(s: &str) -> Option<WipeMode> {
        match s {
            "full" => Some(WipeMode::Full),
            "light" => Some(WipeMode::Light),
            "none" => Some(WipeMode::None),
            _ => None,
        }
    }
}

/// ILM policies Elasticsearch creates itself (`ESRestTestCase.preserveILMPolicyIds`).
const PRESERVED_ILM_POLICIES: &[&str] = &[
    "ilm-history-ilm-policy",
    "slm-history-ilm-policy",
    "watch-history-ilm-policy",
    "watch-history-ilm-policy-16",
    "ml-size-based-ilm-policy",
    "logs",
    "logs@lifecycle",
    "metrics",
    "metrics@lifecycle",
    "profiling-60-days",
    "profiling-60-days@lifecycle",
    "synthetics",
    "agentless",
    "synthetics@lifecycle",
    "traces@lifecycle",
    "exemplars@lifecycle",
    "7-days-default",
    "7-days@lifecycle",
    "30-days-default",
    "30-days@lifecycle",
    "90-days-default",
    "90-days@lifecycle",
    "180-days-default",
    "180-days@lifecycle",
    "365-days-default",
    "365-days@lifecycle",
    ".fleet-files-ilm-policy",
    ".fleet-file-data-ilm-policy",
    ".fleet-actions-results-ilm-policy",
    ".fleet-file-fromhost-data-ilm-policy",
    ".fleet-file-fromhost-meta-ilm-policy",
    ".fleet-file-tohost-data-ilm-policy",
    ".fleet-file-tohost-meta-ilm-policy",
    ".deprecation-indexing-ilm-policy",
    ".monitoring-8-ilm-policy",
    "behavioral_analytics-events-default_policy",
    "logs-apm.app_logs-default_policy",
    "logs-apm.error_logs-default_policy",
    "metrics-apm.app_metrics-default_policy",
    "metrics-apm.internal_metrics-default_policy",
    "metrics-apm.service_destination_10m_metrics-default_policy",
    "metrics-apm.service_destination_1m_metrics-default_policy",
    "metrics-apm.service_destination_60m_metrics-default_policy",
    "metrics-apm.service_summary_10m_metrics-default_policy",
    "metrics-apm.service_summary_1m_metrics-default_policy",
    "metrics-apm.service_summary_60m_metrics-default_policy",
    "metrics-apm.service_transaction_10m_metrics-default_policy",
    "metrics-apm.service_transaction_1m_metrics-default_policy",
    "metrics-apm.service_transaction_60m_metrics-default_policy",
    "metrics-apm.transaction_10m_metrics-default_policy",
    "metrics-apm.transaction_1m_metrics-default_policy",
    "metrics-apm.transaction_60m_metrics-default_policy",
    "traces-apm.rum_traces-default_policy",
    "traces-apm.sampled_traces-default_policy",
    "traces-apm.traces-default_policy",
];

/// Templates (and, by the same naming rules, pipelines) Elasticsearch installs and re-creates
/// itself (`ESRestTestCase.isXPackTemplate`).
pub fn is_managed_template(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        ".monitoring-",
        ".watch",
        ".triggered_watches",
        ".data-frame-",
        ".ml-",
        ".transform-",
        ".deprecation-",
        ".fleet-",
        "behavioral_analytics-",
        "profiling-",
        "elastic-connectors",
        "apm@",
        "apm-",
        "traces-apm",
        "metrics-apm",
        "logs-apm",
        ".slm-history",
        "ilm-history",
        "logs-elasticsearch.querylog",
    ];
    if PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }
    if name.contains('@') && !name.ends_with("@custom") {
        return true;
    }
    matches!(
        name,
        ".watches"
            | "security_audit_log"
            | ".async-search"
            | ".profiling-ilm-lock"
            | "saml-service-provider"
            | "logs"
            | "logs-settings"
            | "logs-mappings"
            | "metrics"
            | "metrics-settings"
            | "metrics-tsdb-settings"
            | "metrics-mappings"
            | "synthetics"
            | "agentless"
            | "synthetics-settings"
            | "synthetics-mappings"
            | ".snapshot-blob-cache"
            | "logstash-index-template"
            | "security-index-template"
            | "data-streams-mappings"
            | "search-acl-filter"
            | ".kibana-reporting"
            | "ai-index-idx"
            | "ai-index-ds"
            | "ai-index-idx-managed"
            | "ai-index-ds-managed"
    )
}

/// A failure while cleaning up (the test that ran last is reported as failed).
#[derive(Debug)]
pub struct WipeError(pub String);

impl std::fmt::Display for WipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type Result<T> = std::result::Result<T, WipeError>;

async fn call(client: &mut Client, method: &str, target: &str, body: Option<&str>) -> Result<Response> {
    let body = body.map(|b| (bytes::Bytes::copy_from_slice(b.as_bytes()), "application/json".to_owned()));
    let started = std::time::Instant::now();
    let r = client.send(method, target, &[], body).await.map_err(|e| WipeError(format!("{method} {target}: {e:#}")));
    if std::env::var_os("SEARCH_YAML_TRACE_WIPE").is_some() {
        eprintln!("    wipe {method} {target} ({:.1} ms)", started.elapsed().as_secs_f64() * 1000.0);
    }
    r
}

/// Sends a request that must succeed (2xx, or one of `ok` statuses).
async fn expect(client: &mut Client, method: &str, target: &str, body: Option<&str>, ok: &[u16]) -> Result<Response> {
    let r = call(client, method, target, body).await?;
    if r.status < 300 || ok.contains(&r.status) {
        Ok(r)
    } else {
        Err(WipeError(format!(
            "cleanup request {method} {target} returned {} {}",
            r.status,
            String::from_utf8_lossy(&r.body[..r.body.len().min(2000)])
        )))
    }
}

fn json(r: &Response) -> Value {
    Value::from_json(&r.body).unwrap_or_default()
}

fn names_csv(names: &[String]) -> String {
    let mut out = String::new();
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        crate::spec::encode_path_part(n, &mut out);
    }
    out
}

fn encode(name: &str) -> String {
    let mut out = String::new();
    crate::spec::encode_path_part(name, &mut out);
    out
}

/// Cleans the cluster.
pub async fn wipe(client: &mut Client, target: &TargetInfo, mode: WipeMode) -> Result<()> {
    if mode == WipeMode::None {
        return Ok(());
    }
    let full = mode == WipeMode::Full;
    let xpack = target.products.xpack;
    // One cluster-state read answers three questions: searchable-snapshot indices, whether any
    // system index exists, and the stored scripts (each read costs ~10 ms on Elasticsearch).
    let meta = {
        let r = call(
            client,
            "GET",
            "/_cluster/state/metadata?filter_path=metadata.indices.*.settings.index.store.snapshot,metadata.indices.*.system,metadata.stored_scripts",
            None,
        )
        .await?;
        if r.status < 300 { json(&r) } else { Value::Null }
    };
    if full {
        expect(client, "GET", "/_cluster/health?wait_for_no_initializing_shards=true&timeout=70s&level=shards", None, &[408]).await?;
        expect(client, "GET", "/_cluster/health?wait_for_events=languid", None, &[]).await?;
        if target.products.rollups {
            wipe_rollups(client).await?;
        }
        if target.products.slm {
            let r = call(client, "GET", "/_slm/policy", None).await?;
            if r.status < 300
                && let Some(m) = json(&r).as_map()
            {
                for name in m.keys() {
                    expect(client, "DELETE", &format!("/_slm/policy/{}", encode(name)), None, &[404]).await?;
                }
            }
        }
        if target.products.searchable_snapshots {
            for name in meta_indices(&meta).iter().filter(|(_, i)| {
                i.get("settings").and_then(|s| s.get("index")).and_then(|s| s.get("store")).and_then(|s| s.get("snapshot")).is_some()
            }).map(|(n, _)| n) {
                expect(client, "DELETE", &format!("/{}", encode(name)), None, &[404]).await?;
            }
        }
    }
    // Snapshots and repositories.
    let r = call(client, "GET", "/_snapshot/_all", None).await?;
    if r.status < 300
        && let Some(repos) = json(&r).as_map()
    {
        for (name, spec) in repos {
            if spec.get("type").and_then(Value::as_str) == Some("fs") {
                expect(client, "DELETE", &format!("/_snapshot/{}/*", encode(name)), None, &[404]).await?;
            }
            expect(client, "DELETE", &format!("/_snapshot/{}", encode(name)), None, &[404]).await?;
        }
    }
    // Resetting feature states costs ~100 ms on Elasticsearch (a cluster state update per feature)
    // and only cleans what lives in system indices, so the reset the Java runner does after every
    // test is done only when a system index exists.
    let has_system = meta.is_null()
        || meta_indices(&meta).values().any(|i| i.get("system").and_then(Value::as_bool) == Some(true));
    if full && target.flavor == crate::target::Flavor::Elasticsearch && has_system {
        expect(client, "POST", "/_features/_reset?error_trace=true", None, &[]).await?;
    }
    // The Java runner only does this with x-pack, but data streams exist without it too (the
    // integ-test distribution, OpenSearch); a target without them answers 4xx, which is fine.
    {
        let r = call(client, "DELETE", "/_data_stream/*?expand_wildcards=all", None).await?;
        if r.status >= 300 && !matches!(r.status, 400 | 404 | 405) {
            let r2 = call(client, "DELETE", "/_data_stream/*", None).await?;
            if r2.status >= 300 && !matches!(r2.status, 400 | 404 | 405) {
                return Err(WipeError(format!("deleting data streams returned {}", r2.status)));
            }
        }
    }
    wipe_indices(client).await?;
    if full && xpack {
        let r = call(client, "GET", "/_query/view", None).await?;
        if r.status < 300
            && let Some(views) = json(&r).get("views").and_then(Value::as_list)
        {
            for view in views {
                if view.get("reserved").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                if let Some(name) = view.get("name").and_then(Value::as_str) {
                    expect(client, "DELETE", &format!("/_query/view/{}", encode(name)), None, &[404]).await?;
                }
            }
        }
    }
    wipe_templates(client, xpack || mode == WipeMode::Light, target.products.legacy_templates).await?;
    wipe_cluster_settings(client).await?;
    if target.products.ilm {
        let r = call(client, "GET", "/_ilm/policy?filter_path=*.modified_date", None).await?;
        if r.status < 300
            && let Some(m) = json(&r).as_map()
        {
            for name in m.keys().filter(|n| !PRESERVED_ILM_POLICIES.contains(&n.as_str())) {
                expect(client, "DELETE", &format!("/_ilm/policy/{}", encode(name)), None, &[404]).await?;
            }
        }
    }
    wipe_pipelines_and_scripts(client, &meta).await?;
    if full {
        // waitForClusterStateUpdatesToFinish: up to 30 s for pending tasks to drain.
        for _ in 0..300 {
            let r = call(client, "GET", "/_cluster/pending_tasks", None).await?;
            let pending = json(&r).get("tasks").and_then(Value::as_list).is_some_and(|t| !t.is_empty());
            if !pending {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    Ok(())
}

fn meta_indices(meta: &Value) -> crate::value::Map {
    meta.get("metadata").and_then(|m| m.get("indices")).and_then(Value::as_map).cloned().unwrap_or_default()
}

/// History indices that may reappear after their data streams are deleted; never deleted.
const KEPT_INDEX_PREFIXES: &[&str] = &[".ds-ilm-history-", ".ds-.slm-history-", ".ds-.watcher-history-", ".ds-.triggered_watches-"];

/// Deletes every index, including hidden and closed ones, except history indices.
async fn wipe_indices(client: &mut Client) -> Result<()> {
    let r = call(
        client,
        "DELETE",
        "/*,-.ds-ilm-history-*,-.ds-.slm-history-*,-.ds-.watcher-history-*,-.ds-.triggered_watches-*?expand_wildcards=open,closed,hidden",
        None,
    )
    .await?;
    if r.status < 300 || r.status == 404 {
        return Ok(());
    }
    // `action.destructive_requires_name: true` (the default outside test clusters) rejects
    // wildcards: list the indices and delete them by name.
    if r.status == 400 && String::from_utf8_lossy(&r.body).contains("Wildcard expressions or all indices are not allowed") {
        let list = expect(client, "GET", "/_cat/indices?h=index&expand_wildcards=all&format=json", None, &[]).await?;
        let names: Vec<String> = json(&list)
            .as_list()
            .unwrap_or_default()
            .iter()
            .filter_map(|i| i.get("index").and_then(Value::as_str))
            .filter(|n| !KEPT_INDEX_PREFIXES.iter().any(|p| n.starts_with(p)))
            .map(str::to_owned)
            .collect();
        for chunk in names.chunks(32) {
            expect(client, "DELETE", &format!("/{}?expand_wildcards=open,closed,hidden", names_csv(chunk)), None, &[404]).await?;
        }
        return Ok(());
    }
    Err(WipeError(format!(
        "deleting all indices returned {} {}",
        r.status,
        String::from_utf8_lossy(&r.body[..r.body.len().min(2000)])
    )))
}

async fn wipe_rollups(client: &mut Client) -> Result<()> {
    let r = call(client, "GET", "/_rollup/job/_all", None).await?;
    if r.status >= 300 {
        return Ok(());
    }
    let jobs = json(&r);
    for job in jobs.get("jobs").and_then(Value::as_list).unwrap_or_default() {
        let Some(id) = job.get("config").and_then(|c| c.get("id")).and_then(Value::as_str) else { continue };
        let id = encode(id);
        expect(client, "POST", &format!("/_rollup/job/{id}/_stop?wait_for_completion=true&timeout=10s"), None, &[404]).await?;
        expect(client, "DELETE", &format!("/_rollup/job/{id}"), None, &[404]).await?;
    }
    Ok(())
}

/// Composable, component and legacy templates that the cluster doesn't manage. Listings use
/// `filter_path` so the built-in templates' large bodies aren't transferred and parsed every time.
async fn wipe_templates(client: &mut Client, selective: bool, legacy: bool) -> Result<()> {
    if !selective {
        if legacy {
            expect(client, "DELETE", "/_template/*", None, &[404]).await?;
        }
        let _ = call(client, "DELETE", "/_index_template/*", None).await?;
        let _ = call(client, "DELETE", "/_component_template/*", None).await?;
        return Ok(());
    }
    for (list, key, path) in [
        ("/_index_template?filter_path=index_templates.name", "index_templates", "/_index_template/"),
        ("/_component_template?filter_path=component_templates.name", "component_templates", "/_component_template/"),
    ] {
        let r = call(client, "GET", list, None).await?;
        if r.status >= 300 {
            continue;
        }
        let names: Vec<String> = json(&r)
            .get(key)
            .and_then(Value::as_list)
            .unwrap_or_default()
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .filter(|n| !is_managed_template(n))
            .map(str::to_owned)
            .collect();
        if !names.is_empty() {
            // Composable templates may be referenced by data streams that just went away; a
            // failure here is retried once by name, then reported.
            let r = call(client, "DELETE", &format!("{path}{}", names_csv(&names)), None).await?;
            if r.status >= 300 && r.status != 404 {
                for n in &names {
                    expect(client, "DELETE", &format!("{path}{}", encode(n)), None, &[404]).await?;
                }
            }
        }
    }
    if legacy {
        let r = call(client, "GET", "/_template?filter_path=*.index_patterns", None).await?;
        if r.status < 300
            && let Some(m) = json(&r).as_map()
        {
            for name in m.keys().filter(|n| !is_managed_template(n)) {
                expect(client, "DELETE", &format!("/_template/{}", encode(name)), None, &[404]).await?;
            }
        }
    }
    Ok(())
}

/// Resets every persistent and transient setting (`{"persistent": {"cluster.*": null}, ...}`).
async fn wipe_cluster_settings(client: &mut Client) -> Result<()> {
    let r = expect(client, "GET", "/_cluster/settings", None, &[]).await?;
    let v = json(&r);
    let mut body = crate::value::Map::new();
    for scope in ["persistent", "transient"] {
        if let Some(settings) = v.get(scope).and_then(Value::as_map).filter(|m| !m.is_empty()) {
            let mut clear = crate::value::Map::new();
            for key in settings.keys() {
                clear.insert(format!("{key}.*"), Value::Null);
            }
            body.insert(scope.to_owned(), Value::Map(clear));
        }
    }
    if !body.is_empty() {
        expect(client, "PUT", "/_cluster/settings", Some(&Value::Map(body).to_json()), &[]).await?;
    }
    Ok(())
}

/// Ingest pipelines and stored scripts created by tests (managed pipelines are kept).
async fn wipe_pipelines_and_scripts(client: &mut Client, meta: &Value) -> Result<()> {
    let r = call(client, "GET", "/_ingest/pipeline", None).await?;
    if r.status < 300
        && let Some(m) = json(&r).as_map()
    {
        let names: Vec<String> = m
            .iter()
            .filter(|(name, p)| {
                let managed = p.get("_meta").and_then(|m| m.get("managed")).and_then(Value::as_bool) == Some(true);
                !managed && !is_managed_template(name)
            })
            .map(|(n, _)| n.clone())
            .collect();
        for name in names {
            expect(client, "DELETE", &format!("/_ingest/pipeline/{}", encode(&name)), None, &[404]).await?;
        }
    }
    if let Some(scripts) = meta.get("metadata").and_then(|m| m.get("stored_scripts")).and_then(Value::as_map) {
        for id in scripts.keys() {
            expect(client, "DELETE", &format!("/_scripts/{}", encode(id)), None, &[404]).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_templates() {
        for n in ["logs", "logs@mappings", ".ml-anomalies-", "apm-source-map", "ilm-history-7", ".watches", "synthetics-settings"] {
            assert!(is_managed_template(n), "{n}");
        }
        for n in ["test", "my-template", "logs@custom", "logs-foo", "my_pipeline"] {
            assert!(!is_managed_template(n), "{n}");
        }
    }

    #[test]
    fn modes() {
        assert_eq!(WipeMode::parse("full"), Some(WipeMode::Full));
        assert_eq!(WipeMode::parse("light"), Some(WipeMode::Light));
        assert_eq!(WipeMode::parse("none"), Some(WipeMode::None));
        assert_eq!(WipeMode::parse("x"), None);
    }
}
