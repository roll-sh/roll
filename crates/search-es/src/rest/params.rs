//! Query parameters: the unknown-parameter check (with Elasticsearch's "did you mean"
//! suggestions, ranked by Lucene's `LevenshteinDistance`) and typed accessors with
//! Elasticsearch's error texts.

use super::Ctx;
use crate::error::{EsError, EsResult};
use crate::meta::resolve::IndicesOptions;
use crate::units::{TimeValue, parse_java_long};
use std::collections::BTreeSet;

/// Parameters every handler accepts (`ALWAYS_SUPPORTED` plus response parameters).
const ALWAYS: &[&str] = &["error_trace", "filter_path", "format", "human", "pretty"];

/// Lucene's `LevenshteinDistance.getDistance`: `1 - edits / max(len)`.
pub fn levenshtein_similarity(target: &str, other: &str) -> f32 {
    let s: Vec<char> = target.chars().collect();
    let t: Vec<char> = other.chars().collect();
    let (n, m) = (s.len(), t.len());
    if n == 0 || m == 0 {
        return if n == m { 1.0 } else { 0.0 };
    }
    let mut p: Vec<usize> = (0..=n).collect();
    let mut d: Vec<usize> = vec![0; n + 1];
    for j in 1..=m {
        let t_j = t[j - 1];
        d[0] = j;
        for i in 1..=n {
            let cost = if s[i - 1] == t_j { 0 } else { 1 };
            d[i] = (d[i - 1] + 1).min(p[i] + 1).min(p[i - 1] + cost);
        }
        std::mem::swap(&mut p, &mut d);
    }
    1.0 - (p[n] as f32 / m.max(n) as f32)
}

/// `SuggestingErrorOnUnknown.suggest`: ` did you mean [x]?` / ` did you mean any of [x, y]?`
/// for candidates more than half similar (Lucene `LevenshteinDistance`), or `""`.
pub fn suggest(unknown: &str, candidates: &[&str]) -> String {
    let mut scored: Vec<(f32, &str)> = candidates.iter().map(|c| (levenshtein_similarity(unknown, c), *c)).filter(|(d, _)| *d > 0.5).collect();
    if scored.is_empty() {
        return String::new();
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.cmp(b.1)));
    if scored.len() == 1 {
        return format!(" did you mean [{}]?", scored[0].1);
    }
    let names: Vec<&str> = scored.iter().map(|(_, c)| *c).collect();
    format!(" did you mean any of [{}]?", names.join(", "))
}

/// `BaseRestHandler.unrecognized`.
pub fn unrecognized(path: &str, invalid: &BTreeSet<String>, candidates: &BTreeSet<String>, detail: &str) -> String {
    let mut msg = format!("request [{path}] contains unrecognized {detail}{}: ", if invalid.len() > 1 { "s" } else { "" });
    for (i, inv) in invalid.iter().enumerate() {
        let mut scored: Vec<(f32, &String)> =
            candidates.iter().map(|c| (levenshtein_similarity(inv, c), c)).filter(|(d, _)| *d > 0.5).collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.cmp(b.1)));
        msg.push_str(&format!("[{inv}]"));
        if !scored.is_empty() {
            msg.push_str(" -> did you mean ");
            if scored.len() > 1 {
                msg.push_str("any of ");
            }
            let names: Vec<&str> = scored.iter().map(|(_, c)| c.as_str()).collect();
            msg.push_str(&format!("[{}]?", names.join(", ")));
        }
        if i + 1 < invalid.len() {
            msg.push_str(", ");
        }
    }
    msg
}

/// APIs whose handlers read `RestRequest.contentOrSourceParam()`, so a body can come in the
/// `source` query parameter (GET-with-body APIs). Elsewhere `source` is an unknown parameter.
pub fn accepts_source_param(api: crate::routes::Api) -> bool {
    use crate::routes::Api::*;
    matches!(
        api,
        Search | Msearch | Count | Explain | IndicesValidateQuery | FieldCaps | SearchTemplate | MsearchTemplate | RenderSearchTemplate
            | ScriptsPainlessExecute | Termvectors | Mtermvectors | Mget | TermsEnum | ClusterAllocationExplain | IndicesAnalyze
            | SearchMvt | KnnSearch
    )
}

/// Whether `name` is a path part of any of the API's routes.
fn path_part(ctx: &Ctx, name: &str) -> bool {
    ctx.api.spec_in(ctx.cluster.dialect).paths.iter().any(|p| p.parts.iter().any(|pt| pt.name == name))
}

/// Rejects query parameters the API doesn't know.
pub fn check_params(ctx: &Ctx) -> EsResult<()> {
    let source = accepts_source_param(ctx.api);
    let extras = crate::api::extra_params(ctx.api);
    let opensearch = ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch;
    let known = |k: &str| {
        ALWAYS.contains(&k)
            // OpenSearch still accepts `local` on its read APIs.
            || (opensearch && k == "local")
            || ctx.api.param_in(ctx.cluster.dialect, k).is_some()
            || ctx.route.params.iter().any(|(n, _)| *n == k)
            // Path and query parameters share one map in Elasticsearch, and handlers read their
            // path parts by name whichever route matched (`GET /_cat/indices?index=x`).
            || path_part(ctx, k)
            || (source && (k == "source" || k == "source_content_type"))
            || extras.contains(&k)
    };
    // The common case allocates nothing; the candidate set is only built for the error.
    if ctx.params.iter().all(|(k, _)| known(k)) {
        return Ok(());
    }
    let invalid: BTreeSet<String> = ctx.params.iter().map(|(k, _)| k.clone()).filter(|k| !known(k)).collect();
    let mut allowed: BTreeSet<String> = ALWAYS.iter().map(|s| s.to_string()).collect();
    allowed.extend(ctx.api.spec_in(ctx.cluster.dialect).params.iter().map(|p| p.name.to_string()));
    allowed.extend(ctx.route.params.iter().map(|(n, _)| n.to_string()));
    allowed.extend(ctx.api.spec_in(ctx.cluster.dialect).paths.iter().flat_map(|p| p.parts.iter().map(|pt| pt.name.to_string())));
    if source {
        allowed.insert("source".into());
        allowed.insert("source_content_type".into());
    }
    allowed.extend(extras.iter().map(|s| s.to_string()));
    Err(EsError::illegal_argument(unrecognized(&ctx.path, &invalid, &allowed, "parameter")))
}

impl Ctx<'_> {
    /// `paramAsBoolean(key, default)`: a bare `?key` is `true`.
    pub fn bool_param(&self, name: &str, default: bool) -> EsResult<bool> {
        match self.query(name) {
            None => Ok(default),
            Some("") => Ok(true),
            Some(v) => crate::units::parse_bool(v),
        }
    }

    /// `paramAsBoolean(key, null)`.
    pub fn opt_bool_param(&self, name: &str) -> EsResult<Option<bool>> {
        match self.query(name) {
            None => Ok(None),
            Some("") => Ok(Some(true)),
            Some(v) => crate::units::parse_bool(v).map(Some),
        }
    }

    pub fn time_param(&self, name: &str, default: TimeValue) -> EsResult<TimeValue> {
        match self.query(name) {
            None => Ok(default),
            Some(v) => TimeValue::parse(v, name),
        }
    }

    pub fn int_param(&self, name: &str, default: i64) -> EsResult<i64> {
        match self.query(name) {
            None => Ok(default),
            Some(v) => crate::units::parse_java_int(v)
                .map(|i| i as i64)
                .ok_or_else(|| EsError::illegal_argument(format!("Failed to parse int parameter [{name}] with value [{v}]")).caused_by(nfe(v))),
        }
    }

    pub fn long_param(&self, name: &str, default: i64) -> EsResult<i64> {
        match self.query(name) {
            None => Ok(default),
            Some(v) => parse_java_long(v)
                .ok_or_else(|| EsError::illegal_argument(format!("Failed to parse long parameter [{name}] with value [{v}]")).caused_by(nfe(v))),
        }
    }

    /// A comma-separated list parameter (path or query).
    pub fn list_param(&self, name: &str) -> Option<Vec<String>> {
        self.param(name).map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
    }

    /// Reads and validates the `timeout`/`master_timeout` parameters (they only need to parse).
    pub fn validate_timeouts(&self) -> EsResult<()> {
        for t in ["timeout", "master_timeout", "cluster_manager_timeout"] {
            if self.has_query(t) {
                self.time_param(t, TimeValue::seconds(30))?;
            }
        }
        if let Some(w) = self.query("wait_for_active_shards")
            && !(w == "all" || parse_java_long(w).is_some_and(|n| n >= 0))
        {
            if w == "index-setting" {
                return Err(EsError::illegal_argument(
                    "The 'index-setting' value for parameter 'wait_for_active_shards' is the default behaviour and this configuration value is not supported anymore. Please remove 'wait_for_active_shards=index-setting'.",
                ));
            }
            if parse_java_long(w).is_some() {
                return Err(EsError::illegal_argument("shard count cannot be a negative value"));
            }
            return Err(EsError::illegal_argument(format!("cannot parse ActiveShardCount[{w}]"))
                .caused_by(EsError::java(400, "number_format_exception", format!("For input string: \"{w}\""))));
        }
        Ok(())
    }

    pub fn indices_options(&self, defaults: IndicesOptions) -> EsResult<IndicesOptions> {
        let mut o = defaults.with_params(|k| self.query(k).map(str::to_string))?;
        // OpenSearch still expands wildcards under `expand_wildcards=none`, to nothing;
        // Elasticsearch takes them literally.
        if self.cluster.dialect == crate::cluster::Dialect::OpenSearch && self.has_query("expand_wildcards") {
            o.allow_wildcards = true;
        }
        Ok(o)
    }

    pub fn flat_settings(&self) -> EsResult<bool> {
        self.bool_param("flat_settings", false)
    }
}

/// `HeaderWarning.escapeAndEncode`: escape `\` and `"`, percent-encode control characters,
/// `%` and characters beyond Latin-1.
pub fn escape_warning(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '\t' | ' ' | '!' | '#'..='$' | '&'..='[' | ']'..='~' | '\u{80}'..='\u{ff}' => out.push(c),
            other => {
                let mut buf = [0u8; 4];
                for b in other.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

/// The `NumberFormatException` Java's number parsing raises.
pub fn nfe(v: &str) -> EsError {
    EsError::java(400, "number_format_exception", format!("For input string: \"{v}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn did_you_mean() {
        let cands: BTreeSet<String> = ["level", "local", "pretty", "timeout", "wait_for_status"].iter().map(|s| s.to_string()).collect();
        let inv: BTreeSet<String> = ["foo", "leve"].iter().map(|s| s.to_string()).collect();
        assert_eq!(unrecognized("/_cluster/health", &inv, &cands, "parameter"), "request [/_cluster/health] contains unrecognized parameters: [foo], [leve] -> did you mean [level]?");
        let inv: BTreeSet<String> = ["prety"].iter().map(|s| s.to_string()).collect();
        assert_eq!(unrecognized("/", &inv, &cands, "parameter"), "request [/] contains unrecognized parameter: [prety] -> did you mean [pretty]?");
    }

    #[test]
    fn similarity_like_lucene() {
        assert_eq!(levenshtein_similarity("leve", "level"), 0.8);
        assert_eq!(levenshtein_similarity("", ""), 1.0);
        assert_eq!(levenshtein_similarity("a", ""), 0.0);
    }

    #[test]
    fn warning_escapes() {
        assert_eq!(escape_warning("a \"b\" \\c"), "a \\\"b\\\" \\\\c");
        assert_eq!(escape_warning("100%"), "100%25");
        assert_eq!(escape_warning("é"), "é");
        assert_eq!(escape_warning("\u{2603}"), "%E2%98%83");
    }
}
