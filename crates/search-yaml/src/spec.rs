//! The REST API specification (`rest-api-spec/api/*.json`) and turning a `do` into an HTTP request.
//!
//! Mirrors `ClientYamlSuiteRestApi` and `ClientYamlTestClient`:
//!
//! - the path is chosen among those whose parts are *all* supplied, preferring the ones that use
//!   the most supplied parameters (`/{index}/_doc/{id}` over `/{index}/_doc` when `id` is given);
//! - every other parameter must be an API parameter, a global one (`_common.json`) or the client
//!   parameter `ignore`, and required parameters must be present;
//! - path parts are percent-encoded like `java.net.URI` does, with `/` escaped as `%2F`; query
//!   parameters are form-encoded like Apache's `URIBuilder` (space as `+`).
//!
//! The Java runner picks a random path among the best ones and a random method (and sometimes
//! sends a GET body as `?source=`). By default we pick deterministically (`MethodStrategy`) so
//! failures reproduce; `MethodStrategy::Random` does what the Java runner does, from a seeded
//! per-test generator.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;

/// Whether an API takes a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodySupport {
    None,
    Optional,
    Required,
}

#[derive(Debug, Clone)]
pub struct ApiPath {
    pub path: String,
    pub methods: Vec<String>,
    pub parts: Vec<String>,
    pub deprecated: bool,
}

#[derive(Debug, Clone)]
pub struct Api {
    pub name: String,
    pub paths: Vec<ApiPath>,
    /// Parameter name → required.
    pub params: HashMap<String, bool>,
    pub body: BodySupport,
    /// `headers.accept` / `headers.content_type`: what the official clients send by default.
    pub accept: Vec<String>,
    pub content_type: Vec<String>,
}

/// How to pick among the methods a path allows when the test doesn't say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MethodStrategy {
    /// POST (then PUT) when there is a body, GET when there isn't, else the first listed.
    #[default]
    Natural,
    /// Always the first method the spec lists.
    First,
    /// The Java runner's choice: a random path among the best matching ones (deprecated paths
    /// included), a random method among the path's, and (decided by the runner) now and then a
    /// small JSON/YAML body as `GET ?source=`.
    Random,
}

/// All APIs plus the global parameters.
#[derive(Debug, Clone, Default)]
pub struct ApiSpec {
    pub apis: HashMap<String, Api>,
    pub global_params: HashSet<String>,
}

#[derive(Deserialize)]
struct RawApi {
    url: RawUrl,
    #[serde(default)]
    params: HashMap<String, RawParam>,
    #[serde(default)]
    body: Option<RawBody>,
    #[serde(default)]
    headers: RawHeaders,
}

#[derive(Deserialize, Default)]
struct RawHeaders {
    #[serde(default)]
    accept: Vec<String>,
    #[serde(default)]
    content_type: Vec<String>,
}

#[derive(Deserialize)]
struct RawUrl {
    paths: Vec<RawPath>,
}

#[derive(Deserialize)]
struct RawPath {
    path: String,
    methods: Vec<String>,
    #[serde(default)]
    parts: Option<HashMap<String, serde::de::IgnoredAny>>,
    #[serde(default)]
    deprecated: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct RawParam {
    #[serde(default)]
    required: bool,
}

#[derive(Deserialize)]
struct RawBody {
    #[serde(default)]
    required: bool,
}

#[derive(Deserialize)]
struct RawCommon {
    #[serde(default)]
    params: HashMap<String, serde::de::IgnoredAny>,
}

/// A planned HTTP request (before body encoding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub method: String,
    /// Encoded path and query string.
    pub target: String,
    /// The API's preferred `Accept` and `Content-Type` (first listed in the spec), if any.
    pub accept: Option<String>,
    pub content_type: Option<String>,
    /// Whether the chosen path also takes GET (for the Java runner's `?source=` variant).
    pub path_allows_get: bool,
}

impl ApiSpec {
    /// Loads every `*.json` file of a directory (recursively).
    pub fn load_dir(dir: &Path) -> anyhow::Result<ApiSpec> {
        let mut spec = ApiSpec::default();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
                let path = entry?.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "json") {
                    let bytes = std::fs::read(&path)?;
                    spec.add_file(&path, &bytes).with_context(|| format!("parsing {}", path.display()))?;
                }
            }
        }
        if spec.apis.is_empty() {
            bail!("no API specs found in {}", dir.display());
        }
        Ok(spec)
    }

    /// Adds one spec file (`_common.json` or `<api>.json`).
    pub fn add_file(&mut self, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        let file = path.file_name().and_then(|f| f.to_str()).unwrap_or_default();
        if file == "_common.json" {
            let common: RawCommon = serde_json::from_slice(bytes)?;
            self.global_params.extend(common.params.into_keys());
            return Ok(());
        }
        let map: HashMap<String, RawApi> = serde_json::from_slice(bytes)?;
        for (name, raw) in map {
            let paths = raw
                .url
                .paths
                .into_iter()
                .map(|p| {
                    let mut parts: Vec<String> = p.parts.map(|m| m.into_keys().collect()).unwrap_or_default();
                    parts.sort();
                    ApiPath { path: p.path, methods: p.methods, parts, deprecated: p.deprecated.is_some() }
                })
                .collect();
            let body = match raw.body {
                None => BodySupport::None,
                Some(b) if b.required => BodySupport::Required,
                Some(_) => BodySupport::Optional,
            };
            let params = raw.params.into_iter().map(|(k, v)| (k, v.required)).collect();
            let RawHeaders { accept, content_type } = raw.headers;
            self.apis.insert(name.clone(), Api { name, paths, params, body, accept, content_type });
        }
        Ok(())
    }

    pub fn api(&self, name: &str) -> Option<&Api> {
        self.apis.get(name)
    }

    /// Plans the request for `api` with already-unstashed parameters (`ignore` removed).
    pub fn plan(
        &self,
        api_name: &str,
        method: Option<&str>,
        params: &[(String, String)],
        has_body: bool,
        strategy: MethodStrategy,
    ) -> Result<Planned, String> {
        self.plan_with(api_name, method, params, has_body, strategy, false, &mut 0)
    }

    /// [`plan`](Self::plan); with `lenient_params` a parameter the spec doesn't list is sent in
    /// the query string instead of rejected, as the official clients (whose API definitions come
    /// from elasticsearch-specification, not this spec) do. `rng` drives
    /// [`MethodStrategy::Random`].
    #[allow(clippy::too_many_arguments)]
    pub fn plan_with(
        &self,
        api_name: &str,
        method: Option<&str>,
        params: &[(String, String)],
        has_body: bool,
        strategy: MethodStrategy,
        lenient_params: bool,
        rng: &mut u64,
    ) -> Result<Planned, String> {
        let Some(api) = self.api(api_name) else {
            return Err(format!(
                "Rest api [{api_name}] cannot be found in the rest spec. Either it doesn't exist or is missing from the test classpath."
            ));
        };
        let path = if strategy == MethodStrategy::Random {
            let best = api.best_paths(params)?;
            best[(crate::runner::splitmix64(rng) % best.len() as u64) as usize]
        } else {
            api.best_path(params)?
        };
        let mut required: Vec<&str> = api.params.iter().filter(|(_, r)| **r).map(|(k, _)| k.as_str()).collect();
        let mut target = String::with_capacity(path.path.len() + 32);
        let mut query = String::new();
        // Fill path parts in the path's order.
        let mut rest = path.path.as_str();
        while let Some(open) = rest.find('{') {
            target.push_str(&rest[..open]);
            let Some(close) = rest[open..].find('}') else {
                return Err(format!("malformed path [{}] for api [{api_name}]", path.path));
            };
            let part = &rest[open + 1..open + close];
            let Some((_, value)) = params.iter().find(|(k, _)| k == part) else {
                return Err(format!("provided path parts don't match the best matching path: {:?}", path.parts));
            };
            encode_path_part(value, &mut target);
            rest = &rest[open + close + 1..];
        }
        target.push_str(rest);
        for (k, v) in params {
            if path.parts.iter().any(|p| p == k) {
                continue;
            }
            if k == "ignore" {
                // A client parameter: the low-level client consumes it (accepted status codes).
                continue;
            }
            if api.params.contains_key(k) || self.global_params.contains(k) || lenient_params {
                required.retain(|r| r != k);
                query.push(if query.is_empty() { '?' } else { '&' });
                encode_query_component(k, &mut query);
                query.push('=');
                encode_query_component(v, &mut query);
            } else {
                return Err(format!("path/param [{k}] not supported by [{api_name}] api"));
            }
        }
        if !required.is_empty() {
            required.sort_unstable();
            return Err(format!("missing required parameter: [{}] by [{api_name}] api", required.join(", ")));
        }
        let methods = &path.methods;
        let chosen = if let Some(m) = method {
            if !methods.iter().any(|x| x == m) {
                return Err(format!(
                    "method [{m}] is not supported by path [{}]. Supported methods: [{}]",
                    path.path,
                    methods.join(", ")
                ));
            }
            m.to_owned()
        } else if has_body {
            if api.body == BodySupport::None {
                return Err(format!("body is not supported by [{api_name}] api"));
            }
            pick(methods, strategy, &["POST", "PUT"], rng)
        } else {
            if api.body == BodySupport::Required {
                return Err(format!("body is required by [{api_name}] api"));
            }
            pick(methods, strategy, &["GET"], rng)
        };
        target.push_str(&query);
        Ok(Planned {
            method: chosen,
            target,
            accept: api.accept.first().cloned(),
            content_type: api.content_type.first().cloned(),
            path_allows_get: methods.iter().any(|m| m == "GET"),
        })
    }
}

fn pick(methods: &[String], strategy: MethodStrategy, preferred: &[&str], rng: &mut u64) -> String {
    if strategy == MethodStrategy::Random && !methods.is_empty() {
        return methods[(crate::runner::splitmix64(rng) % methods.len() as u64) as usize].clone();
    }
    if strategy == MethodStrategy::Natural {
        for p in preferred {
            if methods.iter().any(|m| m == p) {
                return (*p).to_owned();
            }
        }
    }
    methods.first().cloned().unwrap_or_else(|| "GET".into())
}

impl Api {
    /// `getBestMatchingPaths`: every path whose parts are all supplied and that uses the most of
    /// them, in spec order.
    pub fn best_paths(&self, params: &[(String, String)]) -> Result<Vec<&ApiPath>, String> {
        let supplied: HashSet<&str> = params.iter().map(|(k, _)| k.as_str()).collect();
        let usable: Vec<&ApiPath> = self.paths.iter().filter(|p| p.parts.iter().all(|part| supplied.contains(part.as_str()))).collect();
        let Some(max) = usable.iter().map(|p| p.parts.len()).max() else {
            let mut keys: Vec<&str> = supplied.into_iter().collect();
            keys.sort_unstable();
            return Err(format!("Unable to find a matching path for api [{}][{}]", self.name, keys.join(", ")));
        };
        Ok(usable.into_iter().filter(|p| p.parts.len() == max).collect())
    }

    /// `getBestMatchingPaths` + a deterministic pick (first non-deprecated path, in spec order).
    pub fn best_path(&self, params: &[(String, String)]) -> Result<&ApiPath, String> {
        let supplied: HashSet<&str> = params.iter().map(|(k, _)| k.as_str()).collect();
        let mut best: Option<(usize, &ApiPath)> = None;
        for p in &self.paths {
            if !p.parts.iter().all(|part| supplied.contains(part.as_str())) {
                continue;
            }
            let n = p.parts.len();
            let better = match best {
                None => true,
                Some((bn, bp)) => n > bn || (n == bn && bp.deprecated && !p.deprecated),
            };
            if better {
                best = Some((n, p));
            }
        }
        best.map(|(_, p)| p).ok_or_else(|| {
            let mut keys: Vec<&str> = supplied.into_iter().collect();
            keys.sort_unstable();
            format!("Unable to find a matching path for api [{}][{}]", self.name, keys.join(", "))
        })
    }
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn push_pct(b: u8, out: &mut String) {
    out.push('%');
    out.push(HEX[(b >> 4) as usize] as char);
    out.push(HEX[(b & 15) as usize] as char);
}

/// `new URI(null, null, "/" + part, null).getRawPath().substring(1).replace("/", "%2F")`, with
/// non-ASCII characters sent as UTF-8 percent escapes.
pub fn encode_path_part(part: &str, out: &mut String) {
    for &b in part.as_bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'_' | b'-' | b'!' | b'.' | b'~' | b'\'' | b'(' | b')' | b'*' | b',' | b';' | b':' | b'$' | b'&' | b'+' | b'=' | b'@');
        if keep {
            out.push(b as char);
        } else {
            push_pct(b, out);
        }
    }
}

/// `application/x-www-form-urlencoded` as Apache HttpClient writes query parameters.
pub fn encode_query_component(s: &str, out: &mut String) {
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'!' | b'.' | b'~' | b'\'' | b'(' | b')' | b'*') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            push_pct(b, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ApiSpec {
        let mut s = ApiSpec::default();
        s.add_file(Path::new("_common.json"), br#"{"params":{"pretty":{},"error_trace":{},"filter_path":{}}}"#).unwrap();
        s.add_file(
            Path::new("index.json"),
            br#"{"index":{"url":{"paths":[
                {"path":"/{index}/_doc/{id}","methods":["PUT","POST"],"parts":{"id":{},"index":{}}},
                {"path":"/{index}/_doc","methods":["POST"],"parts":{"index":{}}}]},
              "params":{"refresh":{},"routing":{}},"body":{"required":true},
              "headers":{"accept":["application/json"],"content_type":["application/json"]}}}"#,
        )
        .unwrap();
        s.add_file(
            Path::new("indices.get_alias.json"),
            br#"{"indices.get_alias":{"url":{"paths":[
                {"path":"/_alias","methods":["GET"]},
                {"path":"/_alias/{name}","methods":["GET","HEAD"],"parts":{"name":{}}},
                {"path":"/{index}/_alias/{name}","methods":["GET"],"parts":{"index":{},"name":{}}},
                {"path":"/{index}/_alias","methods":["GET"],"parts":{"index":{}}}]},
              "params":{"local":{}}}}"#,
        )
        .unwrap();
        s.add_file(
            Path::new("search.json"),
            br#"{"search":{"url":{"paths":[{"path":"/_search","methods":["GET","POST"]},
                {"path":"/{index}/_search","methods":["GET","POST"],"parts":{"index":{}}}]},
              "params":{"q":{},"scroll":{"required":false}},"body":{}}}"#,
        )
        .unwrap();
        s.add_file(
            Path::new("needs.json"),
            br#"{"needs":{"url":{"paths":[{"path":"/_x","methods":["POST"]}]},"params":{"task":{"required":true}}}}"#,
        )
        .unwrap();
        s
    }

    fn p(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn path_selection() {
        let s = spec();
        let plan = s.plan("index", None, &p(&[("index", "test"), ("id", "1"), ("refresh", "true")]), true, MethodStrategy::Natural).unwrap();
        assert_eq!((plan.method.as_str(), plan.target.as_str()), ("POST", "/test/_doc/1?refresh=true"));
        assert_eq!((plan.accept.as_deref(), plan.content_type.as_deref()), (Some("application/json"), Some("application/json")));
        let plan = s.plan("index", None, &p(&[("index", "test")]), true, MethodStrategy::First).unwrap();
        assert_eq!(plan.target, "/test/_doc");
        let plan = s.plan("indices.get_alias", None, &p(&[("name", "a"), ("index", "i")]), false, MethodStrategy::Natural).unwrap();
        assert_eq!(plan.target, "/i/_alias/a");
        let plan = s.plan("indices.get_alias", None, &p(&[]), false, MethodStrategy::Natural).unwrap();
        assert_eq!(plan.target, "/_alias");
        let plan = s.plan("search", None, &p(&[("q", "a b"), ("ignore", "404")]), true, MethodStrategy::Natural).unwrap();
        assert_eq!((plan.method.as_str(), plan.target.as_str()), ("POST", "/_search?q=a+b"));
        assert_eq!(plan.accept, None);
        let plan = s.plan("search", Some("GET"), &p(&[("pretty", "true")]), true, MethodStrategy::Natural).unwrap();
        assert_eq!((plan.method.as_str(), plan.target.as_str()), ("GET", "/_search?pretty=true"));
        let plan = s.plan_with("search", None, &p(&[("unknown", "x")]), false, MethodStrategy::Natural, true, &mut 0).unwrap();
        assert_eq!(plan.target, "/_search?unknown=x");
        assert!(plan.path_allows_get);
    }

    #[test]
    fn random_paths_and_methods() {
        let mut s = spec();
        s.add_file(
            Path::new("dual.json"),
            br#"{"dual":{"url":{"paths":[
                {"path":"/{index}/_dual","methods":["GET","POST"],"parts":{"index":{}}},
                {"path":"/_dual/{index}","methods":["PUT"],"parts":{"index":{}},"deprecated":{"version":"9.0.0"}},
                {"path":"/_dual","methods":["GET"]}]},"body":{}}}"#,
        )
        .unwrap();
        let mut seen = HashSet::new();
        let mut rng = 42;
        for _ in 0..200 {
            let plan = s.plan_with("dual", None, &p(&[("index", "i")]), true, MethodStrategy::Random, false, &mut rng).unwrap();
            seen.insert((plan.method, plan.target));
        }
        // Both best paths (the deprecated one too), each with any of its methods; never `/_dual`.
        let want: HashSet<(String, String)> =
            [("GET", "/i/_dual"), ("POST", "/i/_dual"), ("PUT", "/_dual/i")].iter().map(|(m, t)| (m.to_string(), t.to_string())).collect();
        assert_eq!(seen, want);
        // The deterministic strategies avoid the deprecated path.
        let plan = s.plan("dual", None, &p(&[("index", "i")]), true, MethodStrategy::Natural).unwrap();
        assert_eq!((plan.method.as_str(), plan.target.as_str()), ("POST", "/i/_dual"));
        // Same generator state, same choices.
        let run = |mut rng: u64| {
            (0..20).map(|_| s.plan_with("dual", None, &p(&[("index", "i")]), false, MethodStrategy::Random, false, &mut rng).unwrap().target).collect::<Vec<_>>()
        };
        assert_eq!(run(7), run(7));
    }

    #[test]
    fn validation_errors_match_java() {
        let s = spec();
        let e = s.plan("search", None, &p(&[("nope", "1")]), false, MethodStrategy::Natural).unwrap_err();
        assert_eq!(e, "path/param [nope] not supported by [search] api");
        let e = s.plan("needs", None, &p(&[]), false, MethodStrategy::Natural).unwrap_err();
        assert_eq!(e, "missing required parameter: [task] by [needs] api");
        let e = s.plan("index", None, &p(&[("index", "x")]), false, MethodStrategy::Natural).unwrap_err();
        assert_eq!(e, "body is required by [index] api");
        let e = s.plan("indices.get_alias", None, &p(&[]), true, MethodStrategy::Natural).unwrap_err();
        assert_eq!(e, "body is not supported by [indices.get_alias] api");
        let e = s.plan("search", Some("PUT"), &p(&[]), false, MethodStrategy::Natural).unwrap_err();
        assert!(e.starts_with("method [PUT] is not supported by path [/_search]"), "{e}");
        assert!(s.plan("missing.api", None, &[], false, MethodStrategy::Natural).unwrap_err().contains("cannot be found"));
        let e = s.plan("index", None, &p(&[("id", "1")]), true, MethodStrategy::Natural).unwrap_err();
        assert_eq!(e, "Unable to find a matching path for api [index][id]");
    }

    #[test]
    fn encoding() {
        let mut out = String::new();
        encode_path_part("a/b c%d,e*_-.~中", &mut out);
        assert_eq!(out, "a%2Fb%20c%25d,e*_-.~%E4%B8%AD");
        let mut out = String::new();
        encode_query_component("a b,c=d&ü*", &mut out);
        assert_eq!(out, "a+b%2Cc%3Dd%26%C3%BC*");
    }
}
