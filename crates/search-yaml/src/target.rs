//! What the target cluster is, and whether a test's prerequisites hold on it.
//!
//! At start the runner asks the target what `ESRestTestCase` asks it: `GET /` (version,
//! distribution), `GET /_nodes/os,plugins` (OS name, node versions, modules → x-pack, ILM, SLM,
//! rollups, CCR, shutdown, searchable snapshots) and `GET /_cluster/state?filter_path=nodes_features`
//! (cluster features per node). A configurable list of *claimed* features is added on top, so a
//! target that doesn't publish features (roll, OpenSearch) can still opt into the tests it
//! supports; synthetic `gte_vX.Y.Z` features follow from the reported version.
//!
//! [`Prerequisites`](crate::suite::Prereq) are evaluated like `PrerequisiteSection.evaluate`:
//! `requires` first (all must hold), then `skip` (any triggers). Capability checks call
//! `GET /_capabilities` exactly as the Java runner does; definite answers (200, or 4xx for
//! "unknown") are cached for the whole run, while a 5xx or transport error fails the test.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::Context;

use crate::http::Client;
use crate::spec::encode_query_component;
use crate::suite::{CapabilityCheck, ClientsRequires, Prereq, XPackRequirement};
use crate::value::Value;

/// Prefix of the errors [`TargetInfo::capability`] returns (a test failure, not a parse error).
pub const CAPABILITY_ERROR: &str = "capabilities check failed";

/// The API dialect of the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Elasticsearch,
    OpenSearch,
}

impl Flavor {
    pub fn parse(s: &str) -> Option<Flavor> {
        match s {
            "elasticsearch" | "es" => Some(Flavor::Elasticsearch),
            "opensearch" | "os" => Some(Flavor::OpenSearch),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Flavor::Elasticsearch => "elasticsearch",
            Flavor::OpenSearch => "opensearch",
        }
    }
}

/// Test runner features this runner implements: Elasticsearch's `Features.SUPPORTED`, plus
/// `skip_os` and `fips_140` (which the Java runner never claims, so it always skips those tests;
/// we know the target's OS and it is never in FIPS mode) and OpenSearch's
/// `spi_on_classpath_jdk9` (locale data providers, present on every supported JDK).
pub const SUPPORTED_FEATURES: &[&str] = &[
    "catch_unauthorized",
    "default_shards",
    "embedded_stash_key",
    "headers",
    "node_selector",
    "stash_in_key",
    "stash_in_path",
    "stash_path_replace",
    "warnings",
    "warnings_regex",
    "yaml",
    "contains",
    "matches_in_any_order",
    "transform_and_set",
    "arbitrary_key",
    "allowed_warnings",
    "allowed_warnings_regex",
    "close_to",
    "is_after",
    "capabilities",
    "skip_os",
    "fips_140",
    "spi_on_classpath_jdk9",
];

/// Products detected from the node modules (`ESRestTestCase.ProductFeature`).
#[derive(Debug, Clone, Default)]
pub struct Products {
    pub xpack: bool,
    pub ilm: bool,
    pub slm: bool,
    pub rollups: bool,
    pub ccr: bool,
    pub shutdown: bool,
    pub searchable_snapshots: bool,
    pub legacy_templates: bool,
}

/// A semantic version `major.minor.patch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// Parses `9.5.5`, `9.5.5-SNAPSHOT`, `8.0.0-rc1` (qualifiers ignored).
    pub fn parse(s: &str) -> Option<Version> {
        let core = s.trim().split(['-', '+']).next()?;
        let mut it = core.split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next().unwrap_or("0").parse().ok()?;
        let patch = it.next().unwrap_or("0").parse().ok()?;
        Some(Version(major, minor, patch))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Everything known about the target cluster.
#[derive(Debug)]
pub struct TargetInfo {
    pub flavor: Flavor,
    /// The reported version string (`version.number`).
    pub version_string: String,
    /// Versions of all nodes (one entry per distinct version).
    pub node_versions: Vec<Version>,
    pub os: String,
    pub products: Products,
    /// Features per node, as reported by the cluster.
    pub node_features: Vec<HashSet<String>>,
    /// Features the runner was told the target supports (added to every node).
    pub claimed_features: HashSet<String>,
    /// Treat the target as serverless (for elasticsearch-clients-tests `requires`).
    pub serverless: bool,
    /// Feature flags enabled on the target (clients-tests `requires.feature_flag`).
    pub feature_flags: HashSet<String>,
    capabilities: Mutex<HashMap<CapabilityCheck, Option<bool>>>,
}

impl TargetInfo {
    /// Builds a description without asking the target (tests, offline evaluation).
    pub fn new(flavor: Flavor, version: &str) -> TargetInfo {
        TargetInfo {
            flavor,
            version_string: version.to_owned(),
            node_versions: Version::parse(version).into_iter().collect(),
            os: String::new(),
            products: Products { legacy_templates: true, ..Products::default() },
            node_features: vec![HashSet::new()],
            claimed_features: HashSet::new(),
            serverless: false,
            feature_flags: HashSet::new(),
            capabilities: Mutex::new(HashMap::new()),
        }
    }

    /// Asks the target what it is.
    pub async fn detect(client: &mut Client, flavor: Option<Flavor>) -> anyhow::Result<TargetInfo> {
        let root = get_json(client, "/").await.context("GET / (is the target up?)")?;
        let version_string = root
            .get("version")
            .and_then(|v| v.get("number"))
            .and_then(Value::as_str)
            .unwrap_or("0.0.0")
            .to_owned();
        let detected = if root.get("version").and_then(|v| v.get("distribution")).and_then(Value::as_str) == Some("opensearch") {
            Flavor::OpenSearch
        } else {
            Flavor::Elasticsearch
        };
        let mut info = TargetInfo::new(flavor.unwrap_or(detected), &version_string);
        if let Ok(nodes) = get_json(client, "/_nodes/os,plugins").await
            && let Some(nodes) = nodes.get("nodes").and_then(Value::as_map)
        {
            let mut versions = Vec::new();
            let mut os_names = Vec::new();
            for node in nodes.values() {
                if let Some(v) = node.get("version").and_then(Value::as_str).and_then(Version::parse) {
                    versions.push(v);
                }
                if let Some(name) = node.get("os").and_then(|o| o.get("pretty_name")).and_then(Value::as_str) {
                    os_names.push(name.to_owned());
                }
                for module in node.get("modules").and_then(Value::as_list).unwrap_or_default() {
                    let name = module.get("name").and_then(Value::as_str).unwrap_or_default();
                    let p = &mut info.products;
                    p.xpack |= name.starts_with("x-pack");
                    if name == "x-pack-ilm" {
                        p.ilm = true;
                        p.slm = true;
                    }
                    p.rollups |= name == "x-pack-rollup";
                    p.ccr |= name == "x-pack-ccr";
                    p.shutdown |= name == "x-pack-shutdown";
                    p.searchable_snapshots |= name == "searchable-snapshots";
                }
            }
            versions.sort();
            versions.dedup();
            if !versions.is_empty() {
                info.node_versions = versions;
            }
            os_names.sort();
            info.os = os_names.pop().unwrap_or_default();
        }
        if info.flavor == Flavor::Elasticsearch
            && let Ok(state) = get_json(client, "/_cluster/state?filter_path=nodes_features").await
            && let Some(list) = state.get("nodes_features").and_then(Value::as_list)
        {
            info.node_features = list
                .iter()
                .map(|n| {
                    n.get("features")
                        .and_then(Value::as_list)
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|f| f.as_str().map(str::to_owned))
                        .collect()
                })
                .collect();
            if info.node_features.is_empty() {
                info.node_features.push(HashSet::new());
            }
        }
        Ok(info)
    }

    pub fn min_version(&self) -> Version {
        self.node_versions.first().copied().unwrap_or_default()
    }

    /// `TestFeatureService.clusterHasFeature`: `any` = at least one node, else all nodes.
    pub fn has_feature(&self, feature: &str, any: bool) -> bool {
        if self.claimed_features.contains(feature) {
            return true;
        }
        let on = |n: &HashSet<String>| n.contains(feature);
        let reported = if any {
            self.node_features.iter().any(on)
        } else {
            !self.node_features.is_empty() && self.node_features.iter().all(on)
        };
        if reported {
            return true;
        }
        if let Some(v) = feature.strip_prefix("gte_v").and_then(Version::parse) {
            return if any {
                self.node_versions.iter().any(|n| *n >= v)
            } else {
                self.node_versions.iter().all(|n| *n >= v)
            };
        }
        false
    }

    /// Whether the minimum node version is in a legacy `skip.version` range list.
    pub fn version_in_ranges(&self, ranges: &str) -> Result<bool, String> {
        let current = Version::parse(&self.version_string).unwrap_or_default();
        let trimmed = ranges.trim();
        match trimmed {
            "all" => return Ok(true),
            "current" => return Ok(self.node_versions.len() == 1 && self.node_versions[0] == current),
            "non_current" => return Ok(!(self.node_versions.len() == 1 && self.node_versions[0] == current)),
            "mixed" => return Ok(self.node_versions.len() > 1),
            _ => {}
        }
        let min = self.min_version();
        for range in trimmed.split(',') {
            let mut ends = range.splitn(3, '-');
            let lower = ends.next().unwrap_or("").trim();
            let Some(upper) = ends.next().map(str::trim) else {
                return Err(format!("version range malformed: {ranges}"));
            };
            if ends.next().is_some() {
                return Err(format!("version range malformed: {ranges}"));
            }
            let lo = if lower.is_empty() { Version(0, 0, 0) } else { Version::parse(lower).ok_or(format!("bad version [{lower}]"))? };
            let hi = if upper.is_empty() { Version(u32::MAX, 0, 0) } else { Version::parse(upper).ok_or(format!("bad version [{upper}]"))? };
            if min >= lo && min <= hi {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Calls `GET /_capabilities` for a check, like `ClientYamlTestExecutionContext.checkCapability`:
    /// a 200 answers with `supported` (`None` when absent or null), a 4xx means unknown (the API
    /// is unsupported). Anything else (5xx, another status, a transport error, a non-boolean
    /// `supported`) is an error that fails the test, as the Java runner rethrows it. Only the
    /// definite answers are cached: a cluster that hiccups once must not turn every later
    /// capability-gated test of the run into a skip.
    pub async fn capability(&self, client: &mut Client, check: &CapabilityCheck) -> Result<Option<bool>, String> {
        if let Some(v) = self.capabilities.lock().expect("capabilities lock").get(check) {
            return Ok(*v);
        }
        let mut target = String::from("/_capabilities?method=");
        encode_query_component(&check.method, &mut target);
        target.push_str("&path=");
        encode_query_component(&check.path, &mut target);
        if let Some(p) = check.parameters.as_deref().filter(|p| !p.is_empty()) {
            target.push_str("&parameters=");
            encode_query_component(p, &mut target);
        }
        if let Some(c) = check.capabilities.as_deref().filter(|c| !c.is_empty()) {
            target.push_str("&capabilities=");
            encode_query_component(c, &mut target);
        }
        target.push_str("&error_trace=false&local_only=true");
        let r = client.send("GET", &target, &[], None).await.map_err(|e| format!("{CAPABILITY_ERROR}: {e:#}"))?;
        let result = match r.status {
            200 => {
                let v = Value::from_json(&r.body).map_err(|e| format!("{CAPABILITY_ERROR}: invalid response body: {e}"))?;
                match v.get("supported") {
                    None | Some(Value::Null) => None,
                    Some(Value::Bool(b)) => Some(*b),
                    Some(other) => return Err(format!("{CAPABILITY_ERROR}: [supported] is not a boolean: {}", other.to_json())),
                }
            }
            400..=499 => None,
            status => {
                return Err(format!(
                    "{CAPABILITY_ERROR}: {target} answered {status}: {}",
                    String::from_utf8_lossy(&r.body[..r.body.len().min(2000)])
                ));
            }
        };
        self.capabilities.lock().expect("capabilities lock").insert(check.clone(), result);
        Ok(result)
    }

    /// Evaluates a prerequisite section. `Some(reason)` means skip.
    pub async fn skip_reason(&self, client: &mut Client, p: &Prereq, what: &str) -> Result<Option<String>, String> {
        if p.is_empty() {
            return Ok(None);
        }
        let unsupported: Vec<&str> =
            p.runner_features.iter().map(String::as_str).filter(|f| !SUPPORTED_FEATURES.contains(f)).collect();
        let message = |reason: &Option<String>, extra: &str| {
            let mut m = format!("[{what}] skipped,");
            if let Some(r) = reason.as_deref().filter(|r| !r.is_empty()) {
                m.push_str(&format!(" reason: [{r}]"));
            }
            if !p.runner_features.is_empty() {
                m.push_str(&format!(" unsupported features [{}]", p.runner_features.join(", ")));
            }
            if !extra.is_empty() {
                m.push(' ');
                m.push_str(extra);
            }
            m
        };
        if !unsupported.is_empty() {
            return Ok(Some(format!("[{what}] skipped, runner does not support features [{}]", unsupported.join(", "))));
        }
        if let Some(issue) = &p.awaits_fix {
            return Ok(Some(message(&p.skip_reason, &format!("(awaits fix {issue})"))));
        }
        // requires: every criterion must hold.
        if p.xpack == XPackRequirement::Required && !self.products.xpack {
            return Ok(Some(message(&p.requires_reason, "(requires x-pack)")));
        }
        if let Some(missing) = p.required_cluster_features.iter().find(|f| !self.has_feature(f, false)) {
            return Ok(Some(message(&p.requires_reason, &format!("(cluster feature [{missing}] missing)"))));
        }
        for check in &p.required_capabilities {
            if self.capability(client, check).await? != Some(true) {
                return Ok(Some(message(&p.requires_reason, &format!("(capability {} {} not supported)", check.method, check.path))));
            }
        }
        // skip: any criterion triggers.
        if p.xpack == XPackRequirement::Forbidden && self.products.xpack {
            return Ok(Some(message(&p.skip_reason, "(x-pack present)")));
        }
        if let Some(ranges) = &p.skip_version
            && self.version_in_ranges(ranges)?
        {
            return Ok(Some(message(&p.skip_reason, &format!("(version {} in [{ranges}])", self.min_version()))));
        }
        if p.skip_os.contains(&self.os) {
            return Ok(Some(message(&p.skip_reason, &format!("(os [{}])", self.os))));
        }
        if let Some(f) = p.skip_cluster_features.iter().find(|f| self.has_feature(f, !p.skip_all_nodes)) {
            return Ok(Some(message(&p.skip_reason, &format!("(cluster feature [{f}] present)"))));
        }
        for check in &p.skip_capabilities {
            if self.capability(client, check).await?.unwrap_or(true) {
                return Ok(Some(message(&p.skip_reason, &format!("(capability {} {} present)", check.method, check.path))));
            }
        }
        for (feature, fixed_by) in &p.skip_known_issues {
            if self.has_feature(feature, true) && !self.has_feature(fixed_by, false) {
                return Ok(Some(message(&p.skip_reason, &format!("(known issue [{feature}] not fixed by [{fixed_by}])"))));
            }
        }
        Ok(None)
    }

    /// elasticsearch-clients-tests' file-level `requires`.
    pub fn clients_skip_reason(&self, r: &ClientsRequires) -> Option<String> {
        if self.serverless && r.serverless == Some(false) {
            return Some("not for serverless (requires.serverless: false)".into());
        }
        if !self.serverless && r.stack == Some(false) {
            return Some("serverless only (requires.stack: false)".into());
        }
        if let Some(flag) = &r.feature_flag
            && !self.feature_flags.contains(flag)
        {
            return Some(format!("behind feature flag [{flag}]"));
        }
        None
    }
}

async fn get_json(client: &mut Client, target: &str) -> anyhow::Result<Value> {
    let r = client.send("GET", target, &[], None).await?;
    if r.status >= 300 {
        anyhow::bail!("GET {target} returned {} {}", r.status, String::from_utf8_lossy(&r.body));
    }
    Ok(Value::from_json(&r.body)?)
}

/// Loads a claimed-features file: one feature per line, `#` comments.
pub fn load_features_file(path: &std::path::Path) -> anyhow::Result<HashSet<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(Version::parse("9.5.5"), Some(Version(9, 5, 5)));
        assert_eq!(Version::parse("8.0.0-SNAPSHOT"), Some(Version(8, 0, 0)));
        assert_eq!(Version::parse("7.10"), Some(Version(7, 10, 0)));
        assert!(Version::parse("x").is_none());
        let t = TargetInfo::new(Flavor::Elasticsearch, "9.5.5");
        assert!(t.version_in_ranges(" - 9.5.5").unwrap());
        assert!(!t.version_in_ranges(" - 8.99.99").unwrap());
        assert!(t.version_in_ranges("8.0.0 - ").unwrap());
        assert!(t.version_in_ranges("7.0.0 - 7.9.99, 9.0.0 - 9.9.99").unwrap());
        assert!(t.version_in_ranges("all").unwrap());
        assert!(t.version_in_ranges("current").unwrap());
        assert!(!t.version_in_ranges("mixed").unwrap());
        assert!(t.version_in_ranges("1.0.0").is_err());
    }

    #[test]
    fn features() {
        let mut t = TargetInfo::new(Flavor::Elasticsearch, "9.5.5");
        t.node_features = vec![["a".to_owned()].into_iter().collect()];
        t.claimed_features.insert("claimed".into());
        assert!(t.has_feature("a", true));
        assert!(t.has_feature("a", false));
        assert!(!t.has_feature("b", true));
        assert!(t.has_feature("claimed", false));
        assert!(t.has_feature("gte_v8.15.0", false));
        assert!(t.has_feature("gte_v9.5.5", false));
        assert!(!t.has_feature("gte_v9.6.0", false));
    }

    #[tokio::test]
    async fn prerequisites() {
        let mut t = TargetInfo::new(Flavor::Elasticsearch, "9.5.5");
        t.node_features = vec![["present".to_owned()].into_iter().collect()];
        t.os = "Ubuntu 24.04".into();
        let mut client = Client::new(crate::http::Endpoint::parse("http://127.0.0.1:9").unwrap(), std::time::Duration::from_millis(50));
        async fn check(t: &TargetInfo, c: &mut Client, p: Prereq) -> Option<String> {
            t.skip_reason(c, &p, "x").await.unwrap()
        }
        macro_rules! eval {
            ($p:expr) => {
                check(&t, &mut client, $p)
            };
        }
        assert!(eval!(Prereq::default()).await.is_none());
        let p = Prereq { required_cluster_features: vec!["present".into()], ..Default::default() };
        assert!(eval!(p).await.is_none());
        let p = Prereq { required_cluster_features: vec!["absent".into()], requires_reason: Some("r".into()), ..Default::default() };
        assert_eq!(eval!(p).await.unwrap(), "[x] skipped, reason: [r] (cluster feature [absent] missing)");
        let p = Prereq { skip_cluster_features: vec!["present".into()], ..Default::default() };
        assert!(eval!(p).await.is_some());
        let p = Prereq { runner_features: vec!["headers".into(), "warp_drive".into()], ..Default::default() };
        assert!(eval!(p).await.unwrap().contains("warp_drive"));
        let p = Prereq { runner_features: vec!["skip_os".into()], skip_os: vec!["Ubuntu 24.04".into()], ..Default::default() };
        assert!(eval!(p).await.is_some());
        let p = Prereq { skip_known_issues: vec![("present".into(), "fix".into())], ..Default::default() };
        assert!(eval!(p).await.is_some());
        let p = Prereq { skip_known_issues: vec![("present".into(), "present".into())], ..Default::default() };
        assert!(eval!(p).await.is_none());
        let p = Prereq { xpack: XPackRequirement::Required, ..Default::default() };
        assert!(eval!(p).await.is_some());
        let p = Prereq { skip_version: Some(" - 9.0.0".into()), ..Default::default() };
        assert!(eval!(p).await.is_none());
        let p = Prereq { awaits_fix: Some("#1".into()), ..Default::default() };
        assert!(eval!(p).await.is_some());
        // An unreachable capabilities API is a failure (the Java runner rethrows the
        // IOException), not "unknown".
        let cap = CapabilityCheck { method: "GET".into(), path: "/_x".into(), parameters: None, capabilities: Some("c".into()) };
        for p in [
            Prereq { required_capabilities: vec![cap.clone()], ..Default::default() },
            Prereq { skip_capabilities: vec![cap.clone()], ..Default::default() },
        ] {
            let err = t.skip_reason(&mut client, &p, "x").await.unwrap_err();
            assert!(err.starts_with(CAPABILITY_ERROR), "{err}");
        }
    }

    /// A server answering `/_capabilities` from the `capabilities` parameter (`yes`, `no`,
    /// `gone` → 400, `flaky` → 500 the first time then true, `odd` → a string, else null),
    /// recording every request target.
    async fn capabilities_server(seen: std::sync::Arc<Mutex<Vec<String>>>) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = l.accept().await else { return };
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 2048];
                    loop {
                        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            let n = sock.read(&mut chunk).await.unwrap_or(0);
                            if n == 0 {
                                return;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                        buf.drain(..end);
                        let target = head.split(' ').nth(1).unwrap().to_owned();
                        let flaky_before = seen.lock().unwrap().iter().filter(|t| t.contains("capabilities=flaky")).count();
                        let (status, body) = if target.contains("capabilities=yes") {
                            ("200 OK", r#"{"supported":true}"#)
                        } else if target.contains("capabilities=no") {
                            ("200 OK", r#"{"supported":false}"#)
                        } else if target.contains("capabilities=gone") {
                            ("400 Bad Request", r#"{"error":"no handler"}"#)
                        } else if target.contains("capabilities=flaky") && flaky_before == 0 {
                            ("500 Internal Server Error", r#"{"error":"boom"}"#)
                        } else if target.contains("capabilities=flaky") {
                            ("200 OK", r#"{"supported":true}"#)
                        } else if target.contains("capabilities=odd") {
                            ("200 OK", r#"{"supported":"true"}"#)
                        } else {
                            ("200 OK", r#"{"supported":null}"#)
                        };
                        seen.lock().unwrap().push(target);
                        let out = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len());
                        if sock.write_all(out.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn capabilities_requests_and_answers() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let port = capabilities_server(seen.clone()).await;
        let t = TargetInfo::new(Flavor::Elasticsearch, "9.5.5");
        let mut client = Client::new(crate::http::Endpoint::parse(&format!("http://127.0.0.1:{port}")).unwrap(), std::time::Duration::from_secs(5));
        let check = |caps: &str, params: Option<&str>| CapabilityCheck {
            method: "POST".into(),
            path: "/{index}/_search".into(),
            parameters: params.map(str::to_owned),
            capabilities: Some(caps.into()),
        };
        // The query string ESRestTestCase builds, parameters only when given.
        assert_eq!(t.capability(&mut client, &check("yes", Some("q,size"))).await, Ok(Some(true)));
        assert_eq!(
            seen.lock().unwrap().last().unwrap(),
            "/_capabilities?method=POST&path=%2F%7Bindex%7D%2F_search&parameters=q%2Csize&capabilities=yes&error_trace=false&local_only=true"
        );
        assert_eq!(t.capability(&mut client, &check("no", None)).await, Ok(Some(false)));
        assert!(!seen.lock().unwrap().last().unwrap().contains("parameters="));
        assert_eq!(t.capability(&mut client, &check("maybe", None)).await, Ok(None));
        // Answers are cached per check.
        assert_eq!(t.capability(&mut client, &check("yes", Some("q,size"))).await, Ok(Some(true)));
        assert_eq!(seen.lock().unwrap().len(), 3);
        // A 4xx is "unknown" (the API is unsupported), and cached.
        assert_eq!(t.capability(&mut client, &check("gone", None)).await, Ok(None));
        assert_eq!(t.capability(&mut client, &check("gone", None)).await, Ok(None));
        assert_eq!(seen.lock().unwrap().len(), 4);
        // A 5xx fails the test and is not cached: the next test asks again.
        let err = t.capability(&mut client, &check("flaky", None)).await.unwrap_err();
        assert!(err.starts_with(CAPABILITY_ERROR) && err.contains("500"), "{err}");
        assert_eq!(t.capability(&mut client, &check("flaky", None)).await, Ok(Some(true)));
        assert_eq!(seen.lock().unwrap().len(), 6);
        // A non-boolean `supported` is a ClassCastException in Java: a failure too.
        assert!(t.capability(&mut client, &check("odd", None)).await.is_err());

        // As Prerequisites.requireCapabilities/skipCapabilities: an unknown answer fails a
        // requirement and triggers a skip.
        let req = |c| Prereq { required_capabilities: vec![c], ..Default::default() };
        let skip = |c| Prereq { skip_capabilities: vec![c], ..Default::default() };
        assert!(t.skip_reason(&mut client, &req(check("yes", Some("q,size"))), "x").await.unwrap().is_none());
        assert!(t.skip_reason(&mut client, &req(check("no", None)), "x").await.unwrap().is_some());
        assert!(t.skip_reason(&mut client, &req(check("maybe", None)), "x").await.unwrap().is_some());
        assert!(t.skip_reason(&mut client, &skip(check("yes", Some("q,size"))), "x").await.unwrap().is_some());
        assert!(t.skip_reason(&mut client, &skip(check("no", None)), "x").await.unwrap().is_none());
    }

    #[test]
    fn clients_requires() {
        let t = TargetInfo::new(Flavor::Elasticsearch, "9.5.5");
        assert!(t.clients_skip_reason(&ClientsRequires { serverless: Some(true), stack: Some(true), feature_flag: None }).is_none());
        assert!(t.clients_skip_reason(&ClientsRequires { serverless: Some(true), stack: Some(false), feature_flag: None }).is_some());
        assert!(t.clients_skip_reason(&ClientsRequires { stack: Some(true), feature_flag: Some("f".into()), ..Default::default() }).is_some());
    }
}
